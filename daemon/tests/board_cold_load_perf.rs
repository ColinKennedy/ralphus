//! RAL-414: deterministic integration coverage proving every board tab's
//! true cold navigation stays inside the documented budget
//! (`ralphus_daemon::perf_timing::board_cold_load_budget_ms`, 2000ms by
//! default) even against realistically large fixtures -- the same shape of
//! load that once froze the *entire* daemon (one global store mutex) for
//! well over a minute on the Retirements tab once a project accumulated a
//! few hundred worktrees of history. See
//! `ralphus_daemon::guardian_merge::worktree_retirement_view`'s doc comment
//! for that incident.
//!
//! Each of the four instrumented board endpoints gets:
//!   * one always-run smoke test with a small (~10-15 entity) fixture, that
//!     only checks for a 200 and a well-formed body, and
//!   * one `#[ignore]`-gated heavy test that seeds the full-scale fixture,
//!     drives a real HTTP request against the real `tiny_http` loop (via
//!     `ureq`, exactly like `api_over_http.rs`/`http_concurrency.rs` --
//!     `server::route()` bypasses the HTTP layer entirely and never emits a
//!     `Server-Timing` header, so it cannot stand in for this), and asserts
//!     wall-clock elapsed time against the budget.
//!
//! ## Running the heavy tests
//!
//! ```sh
//! RALPHUS_BOARD_TIMING=1 cargo test -p ralphus-daemon --test board_cold_load_perf -- --ignored --test-threads=1
//! ```
//!
//! `--test-threads=1` matters here for a reason beyond the usual "these are
//! slow, don't fight the fast tests for CPU": every heavy test seeds its
//! fixture on the same thread that then measures the request, so two heavy
//! tests running concurrently would compete for CPU during both the seed and
//! the timed request, adding scheduler noise to a measurement that is
//! supposed to reflect the server's own cost. None of them share process
//! state (each opens its own in-memory `Store` and binds its own ephemeral
//! port), so serializing them is about measurement fidelity, not
//! correctness.
//!
//! ### Why `RALPHUS_BOARD_TIMING` is read from the shell, never set in code
//!
//! The ticket that produced this file originally called for each heavy test
//! to `std::env::set_var("RALPHUS_BOARD_TIMING", "1")` right before spawning
//! its server, guarding against `perf_timing::timing_enabled()`'s
//! process-lifetime `OnceLock` cache getting set by whichever test's
//! environment mutation is observed first. That approach does not compile in
//! this workspace: `[workspace.lints.rust] unsafe_code = "forbid"` bans
//! `unsafe` outright, and `std::env::set_var`/`remove_var` are `unsafe` as of
//! the 2024 edition -- the same reason `daemon/src/store.rs`
//! (`register_project_with_stamp`), `daemon/src/worktrees.rs`,
//! `daemon/src/runner.rs`, and `daemon/src/agent_profiles.rs` all
//! independently avoid it, per their own doc comments. So these tests do the
//! next best thing: they only ever *read* `RALPHUS_BOARD_TIMING` (via
//! `perf_timing::board_cold_load_budget_ms`/the handler under test), and
//! rely on the invoking shell to have exported it before `cargo test` even
//! starts. That also forecloses the exact race the ticket worried about --
//! nothing in this file ever mutates the variable, so there is no write to
//! race against; the flag is simply whatever the shell exported, for the
//! whole process, for every test in it. The always-run smoke tests below
//! never read the `Server-Timing` header at all, so they are unaffected
//! either way and safe to run in the same `cargo test` invocation as
//! anything else.
//!
//! Without `RALPHUS_BOARD_TIMING=1` exported, the heavy tests still run and
//! still enforce the budget against wall-clock time -- a failure's phase
//! breakdown just reads "no Server-Timing header present" instead of a real
//! one.
//!
//! ## Retirement-fixture deviation (documented, not hidden)
//!
//! The ticket asked for the 600+-worktree Retirements fixture to spread
//! across a realistic mix of retirement states (`scheduled`, `eligible`,
//! `claimed`, `failed`, `deferred`, `opted_out`, `retired`). Every state
//! other than `scheduled` requires `worktree_retirement_view` to see a
//! worktree whose last activity is already
//! `guardian_merge::WORKTREE_RETIREMENT_AGE_MS` (30 real days) in the past --
//! and every `Store` method that could stamp an old timestamp
//! (`record_guardian_worktree_retirement`, `clear_guardian_worktree_path`,
//! `guardian_worktree_records`, `worktree_claims`,
//! `guardian_worktree_retirements`) is `pub(crate)`, not `pub`, so none of
//! them are reachable from this external `tests/` crate. Every `pub` write
//! path that touches a guardian's or branch's timestamp
//! (`set_guardian_status`, `stamp_branch_started_at`,
//! `set_guardian_conflicts`, ...) unconditionally stamps
//! `crate::store::now_ms()` -- there is no `pub` way to backdate one, by
//! design (the same design that keeps a client from spoofing its own
//! activity timestamps). `retire_stale_worktrees` (the only `pub` function
//! that *does* write the pub(crate) durable-state rows) also shells out to
//! real `git worktree` subprocesses against a real repository, so it cannot
//! stand in as a fixture-seeding shortcut either.
//!
//! Given that wall, [`seed_worktree_retirement_fixture`] seeds every entry in
//! the `scheduled` state (fresh activity, the correct and common state for
//! most real worktrees), varying shape instead of state: every 5th guardian
//! also gets a combined worktree row and every 7th gets a second
//! branch/worktree, so the fixture is not one row repeated N times -- it
//! exercises the same per-record dedup/hashmap-building work
//! (`normalized_worktree_path`'s `canonicalize` call, `records_by_path`,
//! `claims_by_path`, `retirements_by_key`) the historical incident was
//! actually about, just without state diversity. If a true multi-state
//! fixture becomes necessary later, it needs either a new narrowly-scoped
//! `pub(crate)`-visible test-seeding entry point in `guardian_merge`/`store`
//! (explicitly out of scope here -- the ticket asked not to add production
//! methods unless there is truly no other way) or a `#[cfg(test)]`-only
//! `pub` backdating hook guarded the same way `perf_timing` gates its own
//! env-var check.

use std::thread;
use std::time::Instant;

use ralphus_core::schema::TaskFile;
use ralphus_daemon::perf_timing;
use ralphus_daemon::server::serve_with;
use ralphus_daemon::store::Store;

// ── Wire helpers ────────────────────────────────────────────────────────────

/// Bind an ephemeral port, serve `store` on a background thread using the
/// real HTTP loop (`server::serve_with`, the same entry point
/// `api_over_http.rs`/`http_concurrency.rs` use), and return the base URL.
/// The spawned thread is never joined or shut down -- `serve_with` runs
/// until the process exits, and every other integration test in this crate
/// relies on the same "process exit reaps it" cleanup instead of a shutdown
/// signal (there is no shutdown endpoint in `server.rs`).
fn spawn_server(store: Store) -> String {
    let server = tiny_http::Server::http("127.0.0.1:0").expect("bind ephemeral port");
    let addr = server.server_addr().to_ip().expect("ip addr");
    thread::spawn(move || serve_with(server, store, 4));
    format!("http://{addr}")
}

/// A real GET over the wire, returning status, body, and the `Server-Timing`
/// header if present.
fn get(base: &str, path: &str) -> (u16, String, Option<String>) {
    let url = format!("{base}{path}");
    match ureq::get(&url).call() {
        Ok(resp) => {
            let timing = resp.header("Server-Timing").map(str::to_string);
            let status = resp.status();
            let body = resp.into_string().unwrap_or_default();
            (status, body, timing)
        }
        Err(ureq::Error::Status(code, resp)) => {
            let timing = resp.header("Server-Timing").map(str::to_string);
            let body = resp.into_string().unwrap_or_default();
            (code, body, timing)
        }
        Err(e) => panic!("request error: {e}"),
    }
}

/// Parse a `Server-Timing` header value (`"name;dur=1, name2;dur=2"`, RFC
/// shape used by [`ralphus_daemon::perf_timing::PhaseTimer::finish`]) into
/// `(phase name, duration in ms)` pairs, in wire order.
fn parse_server_timing(header: &str) -> Vec<(String, f64)> {
    header
        .split(',')
        .filter_map(|entry| {
            let (name, rest) = entry.trim().split_once(';')?;
            let ms = rest.trim().strip_prefix("dur=")?.parse::<f64>().ok()?;
            Some((name.trim().to_string(), ms))
        })
        .collect()
}

/// The ticket's hard requirement: a budget-miss panic message must name the
/// tab, the fixture size that was seeded, the budget, the observed elapsed
/// time, and the phase breakdown (or say plainly that there wasn't one).
fn assert_cold_load_within_budget(
    tab: &str,
    fixture: &str,
    elapsed_ms: u128,
    server_timing: Option<&str>,
) {
    let budget_ms = perf_timing::board_cold_load_budget_ms();
    let phases = match server_timing {
        None => "no Server-Timing header present".to_string(),
        Some(header) => {
            let parsed = parse_server_timing(header);
            if parsed.is_empty() {
                format!("Server-Timing header present but unparsed: {header:?}")
            } else {
                parsed
                    .iter()
                    .map(|(name, ms)| format!("{name}={ms}ms"))
                    .collect::<Vec<_>>()
                    .join(", ")
            }
        }
    };
    assert!(
        elapsed_ms <= u128::from(budget_ms),
        "{tab} tab cold load exceeded its budget -- fixture: {fixture}; budget_ms: {budget_ms}; observed_ms: {elapsed_ms}; phases: [{phases}]"
    );
}

// ── Fixture seeding ──────────────────────────────────────────────────────────

/// Parses one `TaskFile` with `n` trivial tasks, reused (by reference) across
/// every squad [`seed_squads`] inserts -- `Store::insert_squad` takes the
/// file by reference, so one parse serves every squad in the fixture.
fn task_file_with_tasks(n: usize) -> TaskFile {
    let mut src = String::new();
    for i in 0..n {
        src.push_str(&format!(
            "[[task]]\nname=\"task-{i}\"\n[[task.cell]]\ncwd=\"/repo\"\nprompt=\"go\"\n"
        ));
    }
    toml::from_str(&src).expect("generated task toml parses")
}

/// Seeds `squads` squads, each with `tasks_per_squad` tasks, via
/// `Store::insert_squad` -- the same direct `Store` API
/// `daemon/tests/worktree_projects.rs` uses to seed its own squads, scaled
/// up instead of one-off HTTP submissions (parsing 200+ TOML documents over
/// real HTTP just to seed a fixture would dwarf the cost this file is trying
/// to measure).
fn seed_squads(store: &mut Store, squads: usize, tasks_per_squad: usize) {
    let file = task_file_with_tasks(tasks_per_squad);
    for i in 0..squads {
        store
            .insert_squad(&file, Some(&format!("perf-fixture-squad-{i}")), false)
            .expect("insert squad");
    }
}

/// Seeds `count` registered projects via `Store::register_project` -- the
/// direct store setter behind `POST /api/projects`'s handler, which (unlike
/// the HTTP route) does no filesystem/git validation, so a fake path is
/// fine.
fn seed_projects(store: &mut Store, count: usize) {
    for i in 0..count {
        store
            .register_project(
                &format!("perf-fixture-project-{i}"),
                "seeded for RAL-414 board cold-load perf coverage",
                &format!("C:/fixtures/perf/project-{i}"),
                "git",
            )
            .expect("register project");
    }
}

/// Seeds `count` retirement-eligible guardian worktree rows using only `pub`
/// `Store` methods (see the module doc comment's "Retirement-fixture
/// deviation" section for why every entry lands in the `scheduled` state).
/// `normalized_worktree_path` (`guardian_merge.rs`) falls back to the
/// original string when `std::fs::canonicalize` fails, so the fake,
/// never-created paths below are safe to use without touching the real
/// filesystem.
///
/// Every 5th guardian also gets a combined worktree row, and every 7th also
/// gets a second branch/worktree, so the fixture is not one row repeated
/// `count` times. Returns the total number of worktree-record rows actually
/// produced, which is `>= count` (used in test fixture-size reporting).
fn seed_worktree_retirement_fixture(store: &mut Store, count: usize) -> usize {
    let mut records = 0usize;
    for i in 0..count {
        let root = format!("C:/fixtures/retire/project-{i}");
        let id = store
            .create_guardian(&format!("retire-fixture-{i}"), "main", &root)
            .expect("create guardian");
        store
            .add_guardian_branch(&id, "review-branch")
            .expect("add branch");
        let branch_id = store.get_guardian(&id).expect("get guardian").branches[0]
            .id
            .clone();
        store
            .set_branch_review(&id, &branch_id, "review-branch", &format!("{root}/wt"))
            .expect("set branch review");
        records += 1;

        if i % 5 == 0 {
            store
                .set_guardian_combined_worktree(&id, &format!("{root}/combined"))
                .expect("set combined worktree");
            records += 1;
        }

        if i % 7 == 0 {
            store
                .add_guardian_branch(&id, "review-branch-2")
                .expect("add second branch");
            let branch_id_2 = store.get_guardian(&id).expect("get guardian").branches[1]
                .id
                .clone();
            store
                .set_branch_review(
                    &id,
                    &branch_id_2,
                    "review-branch-2",
                    &format!("{root}/wt2"),
                )
                .expect("set second branch review");
            records += 1;
        }
    }
    records
}

// ── Fast smoke tests (always run; never touch `RALPHUS_BOARD_TIMING`) ──────

#[test]
fn tasks_route_returns_ok_and_well_formed_body() {
    let mut store = Store::open_in_memory().expect("open store");
    seed_squads(&mut store, 10, 2);
    let base = spawn_server(store);
    let (status, body, _) = get(&base, "/api/tasks");
    assert_eq!(status, 200, "{body}");
    assert!(body.contains("\"squads\""), "{body}");
    assert!(body.contains("perf-fixture-squad-0"), "{body}");
}

#[test]
fn task_index_route_returns_ok_and_well_formed_body() {
    let mut store = Store::open_in_memory().expect("open store");
    seed_squads(&mut store, 10, 2);
    let base = spawn_server(store);
    let (status, body, _) = get(&base, "/api/task-index");
    assert_eq!(status, 200, "{body}");
    assert!(body.contains("\"squads\""), "{body}");
}

#[test]
fn worktree_retirements_route_returns_ok_and_well_formed_body() {
    let mut store = Store::open_in_memory().expect("open store");
    seed_worktree_retirement_fixture(&mut store, 12);
    let base = spawn_server(store);
    let (status, body, _) = get(&base, "/api/worktree-retirements");
    assert_eq!(status, 200, "{body}");
    assert!(body.contains("\"age_threshold_days\""), "{body}");
    assert!(body.contains("\"entries\""), "{body}");
    assert!(body.contains("\"scheduled\""), "{body}");
}

#[test]
fn projects_route_returns_ok_and_well_formed_body() {
    let mut store = Store::open_in_memory().expect("open store");
    seed_projects(&mut store, 15);
    let base = spawn_server(store);
    let (status, body, _) = get(&base, "/api/projects");
    assert_eq!(status, 200, "{body}");
    assert!(body.contains("\"projects\""), "{body}");
    assert!(body.contains("perf-fixture-project-0"), "{body}");
}

// ── Heavy, full-scale cold-load budget tests (`--ignored` only) ────────────

/// Squads/tasks fixture scale shared by the tasks/task-index heavy tests.
const SQUAD_FIXTURE_COUNT: usize = 200;
const TASKS_PER_SQUAD: usize = 4;

/// Projects fixture scale for the projects heavy test.
const PROJECT_FIXTURE_COUNT: usize = 200;

/// Retirement fixture scale (guardians seeded; actual worktree-record rows
/// produced are higher -- see [`seed_worktree_retirement_fixture`]).
const RETIREMENT_FIXTURE_GUARDIANS: usize = 600;

#[test]
#[ignore = "expensive: seeds 200 squads x 4 tasks; run with --ignored --test-threads=1 (RALPHUS_BOARD_TIMING=1 exported for a phase breakdown on failure)"]
fn tasks_tab_cold_load_stays_under_budget_at_scale() {
    let mut store = Store::open_in_memory().expect("open store");
    seed_squads(&mut store, SQUAD_FIXTURE_COUNT, TASKS_PER_SQUAD);
    let base = spawn_server(store);

    let start = Instant::now();
    let (status, body, timing) = get(&base, "/api/tasks");
    let elapsed_ms = start.elapsed().as_millis();
    assert_eq!(status, 200, "{body}");

    assert_cold_load_within_budget(
        "tasks (Squads tab)",
        &format!("{SQUAD_FIXTURE_COUNT} squads x {TASKS_PER_SQUAD} tasks"),
        elapsed_ms,
        timing.as_deref(),
    );
}

#[test]
#[ignore = "expensive: seeds 200 squads x 4 tasks; run with --ignored --test-threads=1 (RALPHUS_BOARD_TIMING=1 exported for a phase breakdown on failure)"]
fn task_index_tab_cold_load_stays_under_budget_at_scale() {
    let mut store = Store::open_in_memory().expect("open store");
    seed_squads(&mut store, SQUAD_FIXTURE_COUNT, TASKS_PER_SQUAD);
    let base = spawn_server(store);

    let start = Instant::now();
    let (status, body, timing) = get(&base, "/api/task-index");
    let elapsed_ms = start.elapsed().as_millis();
    assert_eq!(status, 200, "{body}");

    assert_cold_load_within_budget(
        "task-index (Tasks tab)",
        &format!("{SQUAD_FIXTURE_COUNT} squads x {TASKS_PER_SQUAD} tasks"),
        elapsed_ms,
        timing.as_deref(),
    );
}

#[test]
#[ignore = "expensive: seeds 200 registered projects; run with --ignored --test-threads=1 (RALPHUS_BOARD_TIMING=1 exported for a phase breakdown on failure)"]
fn projects_tab_cold_load_stays_under_budget_at_scale() {
    let mut store = Store::open_in_memory().expect("open store");
    seed_projects(&mut store, PROJECT_FIXTURE_COUNT);
    let base = spawn_server(store);

    let start = Instant::now();
    let (status, body, timing) = get(&base, "/api/projects");
    let elapsed_ms = start.elapsed().as_millis();
    assert_eq!(status, 200, "{body}");

    assert_cold_load_within_budget(
        "projects (Projects tab)",
        &format!("{PROJECT_FIXTURE_COUNT} projects"),
        elapsed_ms,
        timing.as_deref(),
    );
}

#[test]
#[ignore = "expensive: seeds 600+ retirement-eligible worktrees; run with --ignored --test-threads=1 (RALPHUS_BOARD_TIMING=1 exported for a phase breakdown on failure)"]
fn worktree_retirements_tab_cold_load_stays_under_budget_at_scale() {
    let mut store = Store::open_in_memory().expect("open store");
    let records = seed_worktree_retirement_fixture(&mut store, RETIREMENT_FIXTURE_GUARDIANS);
    let base = spawn_server(store);

    let start = Instant::now();
    let (status, body, timing) = get(&base, "/api/worktree-retirements");
    let elapsed_ms = start.elapsed().as_millis();
    assert_eq!(status, 200, "{body}");

    assert_cold_load_within_budget(
        "worktree-retirements (Retirements tab)",
        &format!(
            "{records} worktree-retirement records across {RETIREMENT_FIXTURE_GUARDIANS} guardians (all `scheduled`; see module doc comment's retirement-fixture deviation note)"
        ),
        elapsed_ms,
        timing.as_deref(),
    );
}
