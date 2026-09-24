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

use std::path::Path;

/// Source lines whose `match`/`if let`/`while let` scrutinee takes a lock
/// guard, each paired with the lines inside that same expression which lock
/// again. Line numbers are 1-based.
fn reentrant_sites(src: &str) -> Vec<(usize, Vec<usize>)> {
    let lines: Vec<&str> = src.lines().collect();
    let mut found = Vec::new();

    for (i, line) in lines.iter().enumerate() {
        let trimmed = line.trim_start();
        // Prose in a comment that merely mentions the pattern -- such as the
        // explanatory notes left at the sites already written correctly -- is
        // not code.
        if trimmed.starts_with("//") {
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
            .filter(|&j| {
                let l = lines[j];
                !l.trim_start().starts_with("//") && l.contains(".lock()")
            })
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
