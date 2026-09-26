//! Guards the daemon against re-entrantly acquiring the store lock.
//!
//! `store_lock::StoreMutex` wraps a `parking_lot::Mutex`, which is *not*
//! reentrant: a thread that takes the store lock twice deadlocks itself
//! permanently, and because every subsystem shares that one lock, the whole
//! daemon stops -- HTTP handlers, scheduler, runner and guardian workers all
//! pile up behind it and the board freezes with nothing left to read.
//!
//! The easy way to write that bug is a `MutexGuard` temporary in a `match`
//! scrutinee: it lives until the end of the *entire* `match`, so any arm that
//! calls `store.lock()` again re-enters a lock the same thread still holds.
//! The same applies to `if let`/`while let`, which desugar to `match`.
//!
//! Bind the guard to its own `let` instead, so it is dropped at the end of
//! that statement before any arm runs:
//!
//! ```ignore
//! let looked_up = store.lock().get_guardian(id);   // guard dropped here
//! let guardian = match looked_up { /* arms may lock again */ };
//! ```
//!
//! # What these lints cannot see
//!
//! Both detectors read *syntax*. They flag a live guard sitting next to
//! `ureq::`, `Command::new`, `.output()`, `thread::sleep` and friends -- they
//! do not follow a call graph, so a guard held across a plain function call
//! that does I/O several frames down is invisible to them.
//!
//! That is not a hypothetical gap. `server.rs`'s `cancel` handler held its
//! guard as a `match` scrutinee across a `background().spawn(...)` whose
//! closure kills tmux sessions. Nothing on those lines looks like I/O: the
//! spawn reads as "hand this to another thread", and under the deferred
//! `BackgroundWork` it is. Under `BackgroundWork::Immediate` -- every
//! `Daemon::new()`-built test daemon -- the closure runs inline, so the tmux
//! subprocesses ran with the global store lock held.
//!
//! What caught it was the WS-B.3 runtime guard watchdog in
//! `store_lock::StoreGuard`'s `Drop`, which measures the hold itself and does
//! not care how the blocking work was reached. Treat these source lints as the
//! cheap first pass and that watchdog as the real backstop; when a hold is
//! reported at a site these lints call clean, the lints are not wrong, they
//! are just looking at the wrong layer.

use std::path::Path;

/// The line with any trailing `// comment` removed (string literals are
/// respected, so a `"http://x"` URL is not truncated). Brace/lock accounting
/// must run over code only: a `{` inside a comment otherwise unbalances the
/// count and makes every region after it wrong.
fn code_of(line: &str) -> String {
    let bytes = line.as_bytes();
    let mut in_str = false;
    let mut escaped = false;
    let mut i = 0;
    while i < bytes.len() {
        if escaped {
            escaped = false;
            i += 1;
            continue;
        }
        match bytes[i] {
            b'\\' if in_str => {
                escaped = true;
                i += 1;
            }
            b'"' => in_str = !in_str,
            b'/' if !in_str && i + 1 < bytes.len() && bytes[i + 1] == b'/' => {
                return line[..i].to_string();
            }
            _ => {}
        }
        i += 1;
    }
    line.to_string()
}

/// 1-based line ranges of every `#[cfg(test)] mod ...` block: the lint is
/// about the production daemon; test-module code runs single-threaded under
/// the test harness and is out of scope.
fn cfg_test_ranges(lines: &[&str]) -> Vec<(usize, usize)> {
    let mut ranges = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        if lines[i].trim_start().starts_with("#[cfg(test)]") {
            // Find the `mod ... {` line within the next few lines.
            let mut j = i + 1;
            while j < lines.len() && j <= i + 3 && !lines[j].trim_start().starts_with("mod ") {
                j += 1;
            }
            if j < lines.len() && j <= i + 3 {
                if let Some(end) = brace_end(lines, j) {
                    ranges.push((i + 1, end + 1));
                    i = end + 1;
                    continue;
                }
            }
        }
        i += 1;
    }
    ranges
}

/// Whether a 1-based line number falls inside any of the ranges.
fn in_ranges(line: usize, ranges: &[(usize, usize)]) -> bool {
    ranges
        .iter()
        .any(|&(start, end)| line >= start && line <= end)
}

/// Source lines whose `match`/`if let`/`while let` scrutinee takes a lock
/// guard, each paired with the lines inside that same expression which lock
/// again. Line numbers are 1-based.
fn reentrant_sites(src: &str) -> Vec<(usize, Vec<usize>)> {
    let lines: Vec<&str> = src.lines().collect();
    let test_ranges = cfg_test_ranges(&lines);
    let mut found = Vec::new();

    for (i, line) in lines.iter().enumerate() {
        if in_ranges(i + 1, &test_ranges) {
            continue;
        }
        // Prose in a comment that merely mentions the pattern -- such as the
        // explanatory notes left at the sites already written correctly -- is
        // not code.
        let code = code_of(line);
        let trimmed = code.trim_start();
        if trimmed.is_empty() {
            continue;
        }
        let branches = trimmed.starts_with("match ")
            || trimmed.contains(" match ")
            || trimmed.contains("if let ")
            || trimmed.contains("while let ");
        if !branches || !line.contains(".lock()") {
            continue;
        }

        // Walk braces forward to the end of the branching expression.
        let (mut depth, mut started, mut end) = (0i32, false, None);
        for (j, body) in lines.iter().enumerate().skip(i) {
            for ch in body.chars() {
                match ch {
                    '{' => {
                        depth += 1;
                        started = true;
                    }
                    '}' => {
                        depth -= 1;
                        if started && depth == 0 {
                            end = Some(j);
                        }
                    }
                    _ => {}
                }
                if end.is_some() {
                    break;
                }
            }
            if end.is_some() {
                break;
            }
        }
        let Some(end) = end else { continue };

        let inner: Vec<usize> = ((i + 1)..=end)
            .filter(|&j| !in_ranges(j + 1, &test_ranges) && code_of(lines[j]).contains(".lock()"))
            .map(|j| j + 1)
            .collect();
        if !inner.is_empty() {
            found.push((i + 1, inner));
        }
    }
    found
}

#[test]
fn no_lock_guard_is_reacquired_inside_a_branch_that_already_holds_it() {
    let src_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");

    let mut sources: Vec<_> = std::fs::read_dir(&src_dir)
        .expect("daemon/src must be readable")
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "rs"))
        .collect();
    sources.sort();
    assert!(!sources.is_empty(), "found no daemon/src/*.rs to scan");

    let mut offenders = Vec::new();
    for path in sources {
        let src = std::fs::read_to_string(&path).expect("source file must be readable");
        let name = path.file_name().unwrap_or_default().to_string_lossy();
        for (line, inner) in reentrant_sites(&src) {
            offenders.push(format!(
                "  daemon/src/{name}:{line} holds a lock guard for the whole \
                 expression; it is taken again at line(s) {inner:?}"
            ));
        }
    }

    assert!(
        offenders.is_empty(),
        "a lock guard held in a `match`/`if let` scrutinee is re-acquired inside \
         the same expression, which self-deadlocks the daemon (see this file's \
         module docs for the fix):\n{}",
        offenders.join("\n")
    );
}

#[test]
fn detector_flags_the_shape_this_test_exists_to_catch() {
    let bad = r"
    let guardian = match store.lock().get_guardian(id) {
        Ok(g) => g,
        Err(e) => {
            store.lock().record(e);
            return;
        }
    };
";
    assert_eq!(
        reentrant_sites(bad).len(),
        1,
        "detector must flag a re-lock inside a match arm"
    );

    let good = r"
    let looked_up = store.lock().get_guardian(id);
    let guardian = match looked_up {
        Ok(g) => g,
        Err(e) => {
            store.lock().record(e);
            return;
        }
    };
";
    assert!(
        reentrant_sites(good).is_empty(),
        "detector must accept a guard bound to its own `let`"
    );

    let prose = r"
    // a `MutexGuard` in a `match` scrutinee lives for the whole `match`, and
    // the body below takes `store.lock()` again.
    let x = 1;
";
    assert!(
        reentrant_sites(prose).is_empty(),
        "detector must ignore explanatory comments"
    );
}

// ── WS-B.1: one-call-level reentrancy ────────────────────────────────────
//
// The scrutinee detector above only flags a *literal* `.lock()` textually
// inside the scrutinee body. BUG-1 (the live hang) hid one call away: the
// match arm called `retire_dual_root_branch_for_guardian(&handle, ..)`, whose
// own body takes `store.lock()`. So the detector here also flags, inside a
// scrutinee-held region, any call to a function whose signature takes the
// store handle (`&StoreHandle`/`&StoreMutex`) -- one call level is enough to
// have caught it, and staying at one level keeps the false-positive surface
// readable.

/// Names of every `fn` in the scanned sources whose parameter list mentions
/// the store handle (`StoreHandle` or `StoreMutex`, in any spelling).
fn handle_taking_fn_names(sources: &[(String, String)]) -> std::collections::HashSet<String> {
    let mut names = std::collections::HashSet::new();
    for (_, src) in sources {
        let lines: Vec<&str> = src.lines().collect();
        let test_ranges = cfg_test_ranges(&lines);
        for (i, line) in lines.iter().enumerate() {
            if in_ranges(i + 1, &test_ranges) {
                continue;
            }
            let code = code_of(line);
            let trimmed = code.trim_start();
            if trimmed.is_empty() {
                continue;
            }
            let Some(rest) = trimmed
                .strip_prefix("pub fn ")
                .or_else(|| trimmed.strip_prefix("fn "))
                .or_else(|| trimmed.strip_prefix("pub(crate) fn "))
            else {
                continue;
            };
            let Some(name_end) = rest.find('(') else {
                continue;
            };
            let name = rest[..name_end].trim().to_string();
            if name.is_empty() || !name.chars().all(|c| c.is_alphanumeric() || c == '_') {
                continue;
            }
            // Parameter list may span lines; collect from the name's own
            // opening paren (the `pub(crate)` visibilities' parens come
            // before the name and must not unbalance the count) until the
            // parens balance.
            let mut params: String = rest[name_end..].to_string();
            let mut balance: i32 =
                params.matches('(').count() as i32 - params.matches(')').count() as i32;
            let mut j = i + 1;
            while balance > 0 && j < lines.len() {
                let code = code_of(lines[j]);
                params.push_str(&code);
                balance += code.matches('(').count() as i32;
                balance -= code.matches(')').count() as i32;
                j += 1;
            }
            if params.contains("StoreHandle") || params.contains("StoreMutex") {
                names.insert(name);
            }
        }
    }
    names
}

/// Within each scrutinee-held region (same brace walk as [`reentrant_sites`]),
/// the called functions whose signatures take the store handle -- i.e. calls
/// that would re-lock a lock the scrutinee already holds.
fn reentrant_via_callee(
    src: &str,
    handle_fns: &std::collections::HashSet<String>,
) -> Vec<(usize, Vec<usize>)> {
    let lines: Vec<&str> = src.lines().collect();
    let test_ranges = cfg_test_ranges(&lines);
    let mut found = Vec::new();

    for (i, line) in lines.iter().enumerate() {
        if in_ranges(i + 1, &test_ranges) {
            continue;
        }
        let code = code_of(line);
        let trimmed = code.trim_start();
        if trimmed.is_empty() {
            continue;
        }
        let branches = trimmed.starts_with("match ")
            || trimmed.contains(" match ")
            || trimmed.contains("if let ")
            || trimmed.contains("while let ");
        if !branches || !code.contains(".lock()") {
            continue;
        }
        let Some(end) = brace_end(&lines, i) else {
            continue;
        };
        let inner: Vec<usize> = ((i + 1)..=end)
            .filter(|&j| {
                let l = code_of(lines[j]);
                if l.trim().is_empty() || in_ranges(j + 1, &test_ranges) {
                    return false;
                }
                // `name(` / `.name(` / `::name(` where `name` takes the
                // handle -- a call that would re-lock while the scrutinee
                // guard is still alive.
                // The call must also pass a store-ish argument: generic
                // names like `new` are in the index (every
                // `impl StoreMutex::new` is), and flagging every
                // `Path::new(...)` would drown the signal.
                call_idents(&l)
                    .iter()
                    .any(|id| handle_fns.contains(id.as_str()))
                    && mentions_store_arg(&l)
            })
            .map(|j| j + 1)
            .collect();
        if !inner.is_empty() {
            found.push((i + 1, inner));
        }
    }
    found
}

/// Brace-matched end line (0-based) of the expression starting at `start`:
/// the first line where brace depth returns to 0 after having gone positive.
/// String literals are skipped so a `'{'` in a format string cannot end the
/// walk early.
fn brace_end(lines: &[&str], start: usize) -> Option<usize> {
    let (mut depth, mut started) = (0i32, false);
    for (j, body) in lines.iter().enumerate().skip(start) {
        let mut in_str = false;
        let mut escaped = false;
        for ch in code_of(body).chars() {
            if escaped {
                escaped = false;
                continue;
            }
            match ch {
                '\\' if in_str => escaped = true,
                '"' => in_str = !in_str,
                '{' if !in_str => {
                    depth += 1;
                    started = true;
                }
                '}' if !in_str => {
                    depth -= 1;
                    if started && depth == 0 {
                        return Some(j);
                    }
                }
                _ => {}
            }
        }
    }
    None
}

/// Whether the line passes a store-ish value into something: a bare
/// `store`/`guard`/`handle`/`store_handle`/`daemon` word. Word-ish matching
/// (split on non-identifier chars) so `semaphore_handle` does not read as
/// `handle`.
fn mentions_store_arg(line: &str) -> bool {
    line.split(|c: char| !c.is_alphanumeric() && c != '_')
        .any(|word| {
            matches!(
                word,
                "store" | "store_handle" | "guard" | "handle" | "daemon"
            )
        })
}

/// Identifiers invoked as calls on a line: `foo(`, `.foo(`, `::foo(`.
fn call_idents(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    let bytes = line.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i];
        if c == b'_' || c.is_ascii_alphanumeric() {
            let start = i;
            while i < bytes.len() && (bytes[i] == b'_' || bytes[i].is_ascii_alphanumeric()) {
                i += 1;
            }
            // A call iff the identifier is immediately followed by `(`.
            if i < bytes.len() && bytes[i] == b'(' {
                let id = &line[start..i];
                if !matches!(
                    id,
                    "if" | "match" | "while" | "for" | "loop" | "fn" | "let" | "else"
                ) {
                    out.push(id.to_string());
                }
            }
        } else {
            i += 1;
        }
    }
    out
}

// ── WS-B.2: no live guard across I/O ──────────────────────────────────
//
// A store guard alive while the code performs blocking I/O stalls every other
// request for the I/O's full duration. This catches the shape static call-
// graph reasoning misses when the I/O is reached through layers of
// delegation: bind the guard, and while that binding is live, ban the tokens
// below. Escapes are possible only through the `allow-lock-io:` marker with
// a written justification on the offending (or one of the two preceding)
// lines.

/// Tokens that (heuristically) mean "blocking I/O" when they appear on a line
/// inside a live-guard region.
const IO_TOKENS: &[&str] = &[
    "ureq::",
    "Command::new",
    ".output()",
    ".status()",
    ".wait()",
    ".recv()",
    ".recv_timeout()",
    "thread::sleep",
    "run_command",
];

/// Marker that exempts one line from the guard-across-I/O ban. The rest of
/// the marker's line is the justification, and it must be non-empty.
const ALLOW_MARKER: &str = "allow-lock-io:";

/// Lines inside a live store-guard region that perform blocking I/O, as
/// `(guard_binding_line_1based, guard_name, offending_lines_1based)`.
fn guard_io_sites(src: &str) -> Vec<(usize, String, Vec<usize>)> {
    let lines: Vec<&str> = src.lines().collect();
    let test_ranges = cfg_test_ranges(&lines);
    let mut found = Vec::new();

    for (i, line) in lines.iter().enumerate() {
        if in_ranges(i + 1, &test_ranges) {
            continue;
        }
        // Bindings: `let guard = <...>.lock()` (any receiver), `let store =
        // daemon.lock();`, etc.
        let binding_code = code_of(line);
        let trimmed = binding_code.trim_start();
        if trimmed.is_empty() {
            continue;
        }
        let Some(rest) = trimmed.strip_prefix("let ") else {
            continue;
        };
        let rest = rest.strip_prefix("mut ").unwrap_or(rest);
        let Some(eq) = rest.find('=') else {
            continue;
        };
        let name = rest[..eq].trim().to_string();
        // The binding must BE the guard: the RHS ends with `.lock()` after the
        // trailing `;`. A chain like `let x = store.lock().foo()` holds the
        // guard only as a statement temporary -- not a live binding.
        let rhs = rest[eq + 1..].trim();
        let rhs = rhs.strip_suffix(';').unwrap_or(rhs).trim_end();
        if name.is_empty()
            || !name.chars().all(|c| c.is_alphanumeric() || c == '_')
            || !rhs.ends_with(".lock()")
        {
            continue;
        }

        // The guard is live from this line until `drop(<name>)`, a
        // reassignment, or the end of the enclosing block (brace depth
        // returning below the binding line's own depth). All accounting
        // runs over comment-stripped code (see [`code_of`]).
        let mut depth = 0i32;
        for ch in code_of(line).chars() {
            match ch {
                '{' => depth += 1,
                '}' => depth -= 1,
                _ => {}
            }
        }
        let binding_depth = depth;
        let mut region_end = lines.len() - 1;
        for (j, body) in lines.iter().enumerate().skip(i + 1) {
            let code = code_of(body);
            if code.contains(&format!("drop({name})"))
                || (code.trim_start().starts_with(&format!("{name} = "))
                    && code.contains(".lock()"))
            {
                region_end = j;
                break;
            }
            if in_ranges(j + 1, &test_ranges) {
                region_end = j;
                break;
            }
            for ch in code.chars() {
                match ch {
                    '{' => depth += 1,
                    '}' => depth -= 1,
                    _ => {}
                }
            }
            if depth < binding_depth {
                region_end = j;
                break;
            }
        }

        let offending: Vec<usize> = ((i + 1)..=region_end)
            .filter(|&j| {
                if in_ranges(j + 1, &test_ranges) {
                    return false;
                }
                let l = code_of(lines[j]);
                if l.trim().is_empty() {
                    return false;
                }
                if !IO_TOKENS.iter().any(|tok| l.contains(tok)) {
                    return false;
                }
                // The marker (on the offending line or either preceding
                // line) exempts it.
                let allowed = (j.saturating_sub(2)..=j).any(|k| lines[k].contains(ALLOW_MARKER));
                !allowed
            })
            .map(|j| j + 1)
            .collect();
        if !offending.is_empty() {
            found.push((i + 1, name, offending));
        }
    }
    found
}

/// Lines inside a scrutinee-held guard region (`match daemon.lock()... {`)
/// that perform blocking I/O, as `(scrutinee_line_1based, offending_lines)`.
/// The scrutinee temporary lives until the end of the entire `match`/`if
/// let`, so its arms run with the store lock held -- exactly the region the
/// reentrancy detectors above walk.
fn scrutinee_io_sites(src: &str) -> Vec<(usize, Vec<usize>)> {
    let lines: Vec<&str> = src.lines().collect();
    let test_ranges = cfg_test_ranges(&lines);
    let mut found = Vec::new();

    for (i, line) in lines.iter().enumerate() {
        if in_ranges(i + 1, &test_ranges) {
            continue;
        }
        let code = code_of(line);
        let trimmed = code.trim_start();
        if trimmed.is_empty() {
            continue;
        }
        let branches = trimmed.starts_with("match ")
            || trimmed.contains(" match ")
            || trimmed.contains("if let ")
            || trimmed.contains("while let ");
        if !branches || !code.contains(".lock()") {
            continue;
        }
        let Some(end) = brace_end(&lines, i) else {
            continue;
        };
        let offending: Vec<usize> = ((i + 1)..=end)
            .filter(|&j| {
                if in_ranges(j + 1, &test_ranges) {
                    return false;
                }
                let l = code_of(lines[j]);
                if l.trim().is_empty() {
                    return false;
                }
                if !IO_TOKENS.iter().any(|tok| l.contains(tok)) {
                    return false;
                }
                let allowed = (j.saturating_sub(2)..=j).any(|k| lines[k].contains(ALLOW_MARKER));
                !allowed
            })
            .map(|j| j + 1)
            .collect();
        if !offending.is_empty() {
            found.push((i + 1, offending));
        }
    }
    found
}

/// Statements holding **two or more** `.lock()` temporaries at once.
///
/// The shape is `f(store.lock().a(), store.lock().b())` -- or, as it really
/// appeared, an `assert_eq!` comparing two locked reads. Rust drops a temporary
/// at the end of the enclosing *statement*, not the enclosing argument, so the
/// first guard is still alive when the second acquires. The store mutex is not
/// reentrant, so that is a permanent self-park.
///
/// This is BUG-1's shape without a `match`, which means neither of the other
/// two detectors sees it: `reentrant_sites` looks for a scrutinee, and
/// `reentrant_via_callee` follows a call. It is added because exactly this got
/// written during the WS-E migration -- mechanically rewriting `store.x()` into
/// `store.lock().x()` turned a two-operand comparison into a deadlock, and the
/// compiler is perfectly happy with it.
///
/// Returns `(line, lock_count)` per offending statement. Statements are joined
/// across lines by paren balance, which is what makes a multi-line `assert_eq!`
/// visible as one statement.
fn multi_lock_statements(src: &str) -> Vec<(usize, usize)> {
    let lines: Vec<&str> = src.lines().collect();
    let skip = cfg_test_ranges(&lines);
    let mut out = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let start = i;
        let mut stmt = code_of(lines[i]);
        // Join continuation lines while parens are unbalanced, so a multi-line
        // call or macro counts as the single statement it is. Bounded so a
        // malformed region cannot run away.
        let mut joined = 0;
        while stmt.matches('(').count() > stmt.matches(')').count()
            && i + 1 < lines.len()
            && joined < 20
        {
            i += 1;
            joined += 1;
            stmt.push(' ');
            stmt.push_str(code_of(lines[i]).trim());
        }
        // Only count locks *before* any `{` that opens a block. A closure or
        // block body starts a new statement scope, so
        // `foo(|| { store.lock().a(); })` holds nothing across anything -- the
        // temporary dies at the inner semicolon. Without this the paren-balance
        // join above swallows whole `thread::spawn(move || { ... })` bodies and
        // reports every lock inside them as one statement.
        let countable = stmt
            .split_once('{')
            .map_or(stmt.as_str(), |(before, _)| before);
        let locks = countable.matches(".lock()").count();
        // `cfg(test)` regions are out of scope for the same reason the other
        // detectors skip them: a test may deliberately construct a shape to
        // assert on it (this file's own fixtures do).
        if locks >= 2 && !in_ranges(start, &skip) {
            out.push((start + 1, locks));
        }
        i += 1;
    }
    out
}

fn daemon_sources() -> Vec<(String, String)> {
    let src_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut sources: Vec<(String, String)> = std::fs::read_dir(&src_dir)
        .expect("daemon/src must be readable")
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "rs"))
        .map(|p| {
            let name = p
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned();
            let src = std::fs::read_to_string(&p).expect("source file must be readable");
            (name, src)
        })
        .collect();
    sources.sort();
    assert!(!sources.is_empty(), "found no daemon/src/*.rs to scan");
    sources
}

#[test]
fn no_scrutinee_holds_the_lock_across_a_call_that_relocks() {
    let sources = daemon_sources();
    let handle_fns = handle_taking_fn_names(&sources);
    assert!(
        handle_fns.contains("retire_dual_root_branch_for_guardian"),
        "sanity: the name index must find handle-taking functions"
    );

    let mut offenders = Vec::new();
    for (name, src) in &sources {
        for (line, callees) in reentrant_via_callee(src, &handle_fns) {
            offenders.push(format!(
                "  daemon/src/{name}:{line} holds a lock guard for the whole \
                 expression; its arms call store-handle-taking function(s) at \
                 line(s) {callees:?}"
            ));
        }
    }

    assert!(
        offenders.is_empty(),
        "a lock guard held in a `match`/`if let` scrutinee flows into a function \
         that takes the store handle itself, which re-locks and self-deadlocks \
         the daemon (bind the scrutinee to a `let` first; see this file's module \
         docs):\n{}",
        offenders.join("\n")
    );
}

#[test]
fn no_live_store_guard_spans_blocking_io() {
    let sources = daemon_sources();

    let mut offenders = Vec::new();
    for (name, src) in &sources {
        for (guard_line, guard, lines) in guard_io_sites(src) {
            offenders.push(format!(
                "  daemon/src/{name}: guard `{guard}` (bound at line {guard_line}) is \
                 still live at line(s) {lines:?}, which perform blocking I/O -- \
                 drop the guard first, or justify with an `allow-lock-io:` \
                 comment if the I/O is genuinely bounded and unavoidable"
            ));
        }
        for (scrutinee_line, lines) in scrutinee_io_sites(src) {
            offenders.push(format!(
                "  daemon/src/{name}: scrutinee guard (bound at line {scrutinee_line}) is \
                 still live at line(s) {lines:?}, which perform blocking I/O -- \
                 bind the scrutinee to a `let` and drop it before the I/O, or \
                 justify with an `allow-lock-io:` comment"
            ));
        }
    }

    assert!(
        offenders.is_empty(),
        "a live store guard spans blocking I/O; the daemon's one global store \
         lock must never be held across a network call, subprocess, or sleep:\n{}",
        offenders.join("\n")
    );
}

#[test]
fn io_detector_flags_the_shapes_this_test_exists_to_catch() {
    let bad = r#"
    let guard = store.lock();
    do_store_work(&guard);
    let resp = ureq::post("https://api.example.com").call()?;
    drop(guard);
"#;
    assert_eq!(guard_io_sites(bad).len(), 1, "ureq under a live guard");

    let bad_sleep = r#"
    let guard = store.lock();
    std::thread::sleep(std::time::Duration::from_millis(25));
"#;
    assert_eq!(
        guard_io_sites(bad_sleep).len(),
        1,
        "sleep under a live guard"
    );

    let allowed = r#"
    let guard = store.lock();
    // allow-lock-io: 2s bounded DNS probe, measured at <50ms, cannot hold the lock long
    let resp = ureq::get("https://api.example.com").timeout(std::time::Duration::from_secs(2)).call()?;
    drop(guard);
"#;
    assert!(
        guard_io_sites(allowed).is_empty(),
        "an `allow-lock-io:` marker must exempt the line"
    );

    let good = r#"
    let probe = { let guard = store.lock(); guard.config() };
    let resp = ureq::post("https://api.example.com").call()?;
"#;
    assert!(
        guard_io_sites(good).is_empty(),
        "a guard dropped before the I/O must pass"
    );

    let scrutinee = r#"
    match daemon.lock().get(id) {
        Ok(p) => {
            let resp = ureq::post("https://api.example.com").call()?;
        }
        Err(_) => {}
    }
"#;
    assert_eq!(
        scrutinee_io_sites(scrutinee).len(),
        1,
        "I/O inside a scrutinee-held match must be flagged"
    );
}

#[test]
fn no_statement_holds_two_lock_guards_at_once() {
    let sources = daemon_sources();
    let mut offenders = Vec::new();
    for (name, src) in &sources {
        for (line, locks) in multi_lock_statements(src) {
            offenders.push(format!(
                "  daemon/src/{name}:{line} holds {locks} `.lock()` temporaries in                  one statement"
            ));
        }
    }
    assert!(
        offenders.is_empty(),
        "a single statement acquires the store lock more than once. Rust drops a          temporary at the end of the statement, not the argument, so the first          guard is still held when the second acquires -- and the store mutex is          not reentrant, so this self-parks permanently. Bind each read to its own          `let` first:
{}",
        offenders.join("
")
    );
}

#[test]
fn multi_lock_detector_flags_the_shape_this_test_exists_to_catch() {
    // The shape that actually got written: two locked reads compared in one
    // macro invocation, spanning several lines.
    let bad = r#"
    fn compare(store: &crate::store_lock::StoreHandle) {
        assert_eq!(
            store.lock().get_guardian(&a).unwrap().branch,
            store.lock().get_guardian(&b).unwrap().branch,
            "same branch"
        );
    }
"#;
    let hits = multi_lock_statements(bad);
    assert_eq!(
        hits.len(),
        1,
        "the two-locks-in-one-statement shape was not flagged: {hits:?}"
    );
    assert_eq!(hits[0].1, 2, "wrong lock count reported");

    // Two locks in *separate* statements are fine -- each temporary is dropped
    // at its own semicolon.
    let good = r#"
    fn compare(store: &crate::store_lock::StoreHandle) {
        let a = store.lock().get_guardian(&a).unwrap().branch.clone();
        let b = store.lock().get_guardian(&b).unwrap().branch.clone();
        assert_eq!(a, b);
    }
"#;
    assert!(
        multi_lock_statements(good).is_empty(),
        "sequential locks in separate statements were wrongly flagged"
    );

    // Locks in separate closure bodies are fine: each temporary dies at its own
    // inner statement, even though the outer call's parens stay open across
    // both. This is the shape that made the first version of this detector
    // report three false positives in `pr.rs`.
    let closures = r#"
    fn routed(store: &crate::store_lock::StoreHandle) {
        let fork = name.as_deref().and_then(|p| {
            owner
                .and_then(|o| store.lock().resolve_fork(p, o).ok().flatten())
                .or_else(|| store.lock().resolve_fork(p, "").ok().flatten())
        });
    }
"#;
    assert!(
        multi_lock_statements(closures).is_empty(),
        "locks in separate closure bodies were wrongly flagged"
    );

    // Same for a spawned thread body.
    let spawned = r#"
    fn later(store: &crate::store_lock::StoreHandle) {
        std::thread::spawn(move || {
            let a = store.lock().one();
            let b = store.lock().two();
        });
    }
"#;
    assert!(
        multi_lock_statements(spawned).is_empty(),
        "locks inside a spawned closure were wrongly flagged"
    );

    // A single lock per statement, however chained, is fine.
    let single = r#"
    fn one(store: &crate::store_lock::StoreHandle) {
        let v = store.lock().thing(a, b, c).map(|x| x.y).unwrap_or_default();
    }
"#;
    assert!(multi_lock_statements(single).is_empty());
}

#[test]
fn callee_detector_flags_a_relock_one_call_away() {
    let sources = vec![(
        "fake.rs".to_string(),
        r#"
    fn helper(store: &crate::store_lock::StoreHandle) { store.lock().work(); }

    fn caller(store: &crate::store_lock::StoreHandle) {
        match store.lock().get(id) {
            Ok(v) => { helper(store); }
            Err(_) => {}
        }
    }
"#
        .to_string(),
    )];
    let handle_fns = handle_taking_fn_names(&sources);
    assert!(handle_fns.contains("helper"));
    assert_eq!(
        reentrant_via_callee(&sources[0].1, &handle_fns).len(),
        1,
        "a callee that takes the handle must be flagged one call level away"
    );
}
