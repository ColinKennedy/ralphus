//! WS-E.4 ratchet: how many `GET` handlers are served from the read pool
//! instead of the writer lock, and the promise that the number only goes up.
//!
//! # Why a source scan rather than a latency test
//!
//! WS-E's goal is structural: no `GET` handler should take the writer mutex, so
//! a board poll can never queue behind the scheduler or a guardian-merge worker.
//! That is a property of which code path a handler uses, and a latency test
//! cannot see it -- an un-migrated handler looks fine right up until it lands
//! behind a slow writer, which is exactly the intermittent symptom that made the
//! original problem hard to pin down. Counting call paths makes the migration's
//! progress a number, and makes a regression (someone routing a pooled handler
//! back through `daemon.lock()`) fail immediately.
//!
//! The aggregate latency *is* gated, separately and properly, by
//! `board_contention.rs`. This file only answers "did the read path shrink".

use std::path::Path;

/// Minimum number of distinct `GET` handlers served from the read pool.
///
/// Raise this as handlers migrate; never lower it. At the time of writing the
/// pooled set is the 5 hottest board endpoints plus `health`, and WS-E.2 added
/// six more. The plan's E.4 target is every `GET` handler, which is not reached
/// yet -- several remaining ones are multi-layer composites (`get_squad`,
/// `squad_worktrees`) whose private helpers each need their own `_conn` split
/// first.
const MIN_POOLED_GET_HANDLERS: usize = 12;

/// Handlers that are known to be pooled and must stay that way.
///
/// The count above stops the total sliding; this stops a *specific* hot
/// endpoint quietly moving back onto the writer lock while some other handler
/// migrates and keeps the total level.
const MUST_STAY_POOLED: &[&str] = &[
    // The board's own poll set -- the reason any of this matters.
    "board",
    "task_index",
    "queue",
    "guardian_index",
    "guardian_list",
    "pr_index_list",
    // Health must never wait on the writer: a wedged daemon has to stay able to
    // report that it is wedged.
    "health",
];

fn server_source() -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/server.rs");
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

/// The body of `fn name(...)`, by brace matching from its signature.
fn body_of(src: &str, name: &str) -> Option<String> {
    let needle = format!("\nfn {name}(");
    let at = src.find(&needle)?;
    let open = src[at..].find('{')? + at;
    let mut depth = 0usize;
    for (offset, ch) in src[open..].char_indices() {
        match ch {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(src[open..=open + offset].to_string());
                }
            }
            _ => {}
        }
    }
    None
}

/// Every handler named by a `("GET", [...]) => handler(` route arm.
fn get_handlers(src: &str) -> Vec<String> {
    let mut out = Vec::new();
    for line in src.lines() {
        let trimmed = line.trim();
        if !trimmed.starts_with("(\"GET\", [") {
            continue;
        }
        let Some(arrow) = trimmed.find("=> ") else {
            continue;
        };
        let rest = &trimmed[arrow + 3..];
        let name: String = rest
            .chars()
            .take_while(|c| c.is_alphanumeric() || *c == '_')
            .collect();
        if !name.is_empty() && !out.contains(&name) {
            out.push(name);
        }
    }
    out
}

/// Whether a handler body reaches the store through the read pool.
fn is_pooled(body: &str) -> bool {
    [
        "with_read_snapshot",
        "read_pool",
        "read_running_count",
        "read_board_snapshot",
    ]
    .iter()
    .any(|marker| body.contains(marker))
}

#[test]
fn the_pooled_get_handler_count_only_goes_up() {
    let src = server_source();
    let handlers = get_handlers(&src);
    assert!(
        handlers.len() > 40,
        "only found {} GET handlers; the route-table scan is broken, not the code",
        handlers.len()
    );

    let mut pooled = Vec::new();
    let mut locked = Vec::new();
    for name in &handlers {
        let Some(body) = body_of(&src, name) else {
            // A handler defined as a method or behind a macro is out of scope
            // for this scan rather than a failure.
            continue;
        };
        if is_pooled(&body) {
            pooled.push(name.clone());
        } else if body.contains(".lock()") {
            locked.push(name.clone());
        }
    }

    eprintln!(
        "read path: {} of {} GET handlers pooled, {} still on the writer lock",
        pooled.len(),
        handlers.len(),
        locked.len()
    );
    eprintln!("still on the writer lock: {locked:?}");

    assert!(
        pooled.len() >= MIN_POOLED_GET_HANDLERS,
        "only {} GET handlers are served from the read pool, down from the {} \
         ratchet. A pooled handler was moved back onto `daemon.lock()`; pooled \
         set is {pooled:?}",
        pooled.len(),
        MIN_POOLED_GET_HANDLERS
    );

    for required in MUST_STAY_POOLED {
        assert!(
            pooled.iter().any(|p| p == required),
            "`{required}` is no longer served from the read pool. It is one of \
             the endpoints the board polls continuously (or `health`, which must \
             stay answerable while the writer is stuck), so putting it back on \
             the writer lock reintroduces exactly the starvation WS-E exists to \
             remove."
        );
    }
}

#[test]
fn the_ratchet_scan_can_tell_the_two_paths_apart() {
    // A self-test: if `is_pooled`/`body_of` silently stopped matching, the test
    // above would pass by finding nothing and asserting nothing useful.
    let fake = "
fn pooled_one(daemon: &Daemon) -> Reply {
    match daemon.with_read_snapshot(Store::thing_conn) { Ok(v) => json(200, &v), Err(e) => store_error(&e) }
}
fn locked_one(daemon: &Daemon) -> Reply {
    match daemon.lock().thing() { Ok(v) => json(200, &v), Err(e) => store_error(&e) }
}
";
    let pooled = body_of(fake, "pooled_one").expect("body_of found no pooled body");
    let locked = body_of(fake, "locked_one").expect("body_of found no locked body");
    assert!(is_pooled(&pooled), "a pooled body was not recognized");
    assert!(!is_pooled(&locked), "a locked body was misread as pooled");
    assert!(locked.contains(".lock()"));

    // And the route-arm scan must pick handler names out of real route syntax.
    let routes = "
        (\"GET\", [\"api\", \"daemon\"]) => health(daemon),
        (\"GET\", [\"api\", \"queue\"]) => queue(daemon),
        (\"POST\", [\"api\", \"squads\"]) => submit(daemon, body),
";
    let found = get_handlers(routes);
    assert_eq!(
        found,
        vec!["health".to_string(), "queue".to_string()],
        "the route scan mis-parsed GET arms (or picked up a POST)"
    );
}
