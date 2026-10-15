//! Deterministic cell-session names and the durable ended-cell snapshots keyed
//! by them.
//!
//! A cell, proof step, or review agent run is identified by the triple
//! `(run_id, task, session_id)`; [`session_name`] folds it into one sanitized
//! string that keys the cell's transcript files (`crate::terminal_log`), its
//! live-activity state, and its snapshot (see [`write_pane_snapshot`]).

use std::path::PathBuf;

/// Deterministic, collision-avoiding cell session name for a runner
/// invocation. `run_id`/`task`/`session_id` is the same triple every call
/// site already uses to key a [`crate::runner::RunnerSpec`] (scheduler
/// sessions, `prompt`-kind verify steps, and Guardian merge/resolver/
/// synthesizer sessions all set distinct values here), so no new bookkeeping
/// is needed to later recompute the same name for the HTTP capture-pane
/// endpoint (RAL-102 — the ticket leaves the naming scheme to implementation
/// judgment, "best-effort... no specific scheme mandated").
///
/// `task` is required, not optional: `session_id` alone (e.g. the
/// conventional `"work"`) is routinely reused across every task in a run, so
/// keying on `(run_id, session_id)` alone made sibling tasks' cell sessions
/// collide — concurrent `new-session` calls for the same name race, and only
/// one survives while the rest fail with a runner error (discovered
/// empirically: a 5-task run sharing session id `"work"` left 4 of 5 sessions
/// dead on arrival).
#[must_use]
pub fn session_name(run_id: &str, task: &str, session_id: &str) -> String {
    let raw = format!("ralphus_{run_id}_{task}_{session_id}");
    let sanitized: String = raw
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    // session (and its Windows alternatives) get unwieldy with very long session
    // names; truncate defensively. Collisions are astronomically unlikely
    // since the run/session ids embedded here are already unique.
    const MAX_LEN: usize = 200;
    if sanitized.len() > MAX_LEN {
        sanitized[..MAX_LEN].to_string()
    } else {
        sanitized
    }
}

/// The sanitized prefix shared by every [`session_name`] built for `run_id`,
/// regardless of `task`/`session_id` — lets a caller reset or find all
/// cell-session-keyed state for one run without enumerating every
/// task/session pair it might have used. RAL-536 uses this to clear every
/// thinking-stall strike counter under one review/guardian id (`guardian-{id}`)
/// at once on a manual restart/reopen, since a review's many agent sessions
/// (auto-build, resolver/fix-pass, proof-synthesis, per-branch feedback, ...)
/// share that run id but use call-site-specific task/session ids that aren't
/// practical to enumerate individually.
#[must_use]
pub fn session_name_run_prefix(run_id: &str) -> String {
    let raw = format!("ralphus_{run_id}_");
    raw.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// Directory pane snapshots (see [`write_pane_snapshot`]) live under —
/// `state_dir()` (next to the SQLite DB), not the OS temp dir the
/// spec/result side-channel files use, since a snapshot is meant to survive
/// well past the session ending (and a daemon restart), not just long enough
/// for one runner invocation to hand off its result.
///
/// Not cleaned up on run/guardian deletion itself -- each snapshot is a
/// small, bounded text file (`PANE_SNAPSHOT_MAX_LINES`), so a leftover one is
/// a modest, bounded-per-entry disk-space cost, not the unbounded resource
/// leak documented for orphaned runner processes in
/// `the crash notes`. Retired instead by
/// `crate::worktree_transcript_retirement` (RAL-348), which piggybacks on
/// worktree retirement (`guardian_merge::retire_stale_worktrees`): once a
/// worktree is gone, every cell/proof session that ever ran in it is safe to
/// forget too, via [`delete_pane_snapshot`].
fn pane_snapshot_dir() -> PathBuf {
    #[cfg(test)]
    {
        if let Some(root) = PANE_SNAPSHOT_TEST_ROOT.with(|r| r.borrow().clone()) {
            return root;
        }
    }
    crate::state_dir().join("pane_snapshots")
}

#[cfg(test)]
thread_local! {
    /// Test-scoped override for [`pane_snapshot_dir`], set via
    /// [`set_pane_snapshot_test_root`].
    static PANE_SNAPSHOT_TEST_ROOT: std::cell::RefCell<Option<PathBuf>> = const { std::cell::RefCell::new(None) };
}

/// Redirect this thread's pane-snapshot storage to `root` for the duration of
/// a test — the sibling of `crate::terminal_log::set_test_root`, and needed
/// for the same reason.
///
/// A test's in-memory store hands out ids from 1, so a freshly created
/// guardian is `guardian-000000000001` with branch `branch-000000000001` —
/// exactly the ids a developer's real `~/.ralphus/pane_snapshots` is full of.
/// Any lookup that resolves a session by *recency of its pane snapshot*
/// (`server::freshest_resolver_or_feedback`) would otherwise read those real
/// files and pick a different session than the test set up, failing only on
/// machines with history and passing on clean CI. Isolating the terminal-log
/// root alone does not cover this: pane snapshots live in their own
/// `state_dir()` subtree.
#[cfg(test)]
pub(crate) fn set_pane_snapshot_test_root(root: PathBuf) {
    PANE_SNAPSHOT_TEST_ROOT.with(|r| *r.borrow_mut() = Some(root));
}

/// Path a session's persisted last-pane-content snapshot lives (or would
/// live) at within `dir`, keyed by its already-sanitized deterministic session
/// session name (see [`session_name`]) — the same name every "peek"/"open
/// terminal" endpoint already recomputes to address a *live* session, reused
/// here so no new bookkeeping is needed to find the historical record once
/// the live one is gone. Takes `dir` explicitly (rather than always calling
/// [`pane_snapshot_dir`] internally) purely so [`write_pane_snapshot`]/
/// [`read_pane_snapshot`]'s tests can point it at a throwaway directory
/// instead of the real `state_dir()`.
#[must_use]
fn pane_snapshot_path_in(dir: &std::path::Path, session_name: &str) -> PathBuf {
    dir.join(format!("{session_name}.txt"))
}

/// Path a session's persisted last-pane-content snapshot lives (or would
/// live) at — see [`pane_snapshot_path_in`].
#[must_use]
pub fn pane_snapshot_path(session_name: &str) -> PathBuf {
    pane_snapshot_path_in(&pane_snapshot_dir(), session_name)
}

/// Bound on a persisted snapshot's size (RAL-102 follow-up) — generous
/// enough to be a genuinely useful "what was last there" record, small
/// enough that a long project history's worth of snapshots doesn't grow
/// disk usage unboundedly the way keeping every full pane transcript
/// forever would. Mirrors the existing `tail_lines` truncation already
/// applied to a "no result file" failure's pane-tail diagnostic
/// (`daemon/src/runner.rs::read_result`), just with a larger budget
/// since this is the record a human actually reads after the fact, not a
/// one-line error-message addendum.
const PANE_SNAPSHOT_MAX_LINES: usize = 4000;

/// Implementation behind [`write_pane_snapshot`], taking `dir` explicitly so
/// it's unit-testable against a throwaway directory instead of the real
/// `state_dir()`.
fn write_pane_snapshot_in(dir: &std::path::Path, session_name: &str, content: &str) {
    if let Err(e) = std::fs::create_dir_all(dir) {
        // ralphus[ignore-rlog-pair]: this low-level helper has no Store; its Store-owning caller records the structured workflow outcome
        crate::rlog!(
            WARNING,
            "ralphus [cell-session] could not create pane snapshot dir {}: {e}",
            dir.display()
        );
        return;
    }
    // RAL-264: scrub resolved `from_env` secret values out of the pane content
    // before persisting it — the pane can legitimately show the agent's own
    // `$env:... = 'sk-or-v1-...'` assignment, and a stale snapshot is exactly
    // the kind of durable artifact the leak must not survive in.
    //
    // RAL-247: additionally scrub credential env-var values by pattern, so a
    // secret value that was never registered is still caught before the pane
    // text ever hits disk.
    let content = crate::redact::redact_all(content);
    let redacted = ralphus_core::redact::redact_secrets(&content);
    let truncated = crate::runner::tail_lines(&redacted, PANE_SNAPSHOT_MAX_LINES);
    if let Err(e) = std::fs::write(pane_snapshot_path_in(dir, session_name), truncated) {
        // ralphus[ignore-rlog-pair]: this low-level helper has no Store; its Store-owning caller records the structured workflow outcome
        crate::rlog!(
            WARNING,
            "ralphus [cell-session] could not write pane snapshot for {session_name}: {e}"
        );
    }
}

/// Persist `content` (the last successfully captured pane content — see
/// `crate::runner::SubprocessRunner::run_direct_attempt`) as the durable,
/// read-only historical record for `session_name`, truncated to
/// [`PANE_SNAPSHOT_MAX_LINES`]. Called unconditionally at the end of every
/// session attempt (done, failed, cancelled, or timed out) so "what was last
/// there" is always available once the live session is gone — even for an
/// attempt that never printed anything, `content` may legitimately be empty,
/// which simply overwrites any stale prior snapshot with an empty one rather
/// than leaving it stuck showing an older attempt's output. Best-effort: a
/// write failure (e.g. disk full) is logged but never fails the session
/// itself, since the snapshot is a diagnostic convenience, not part of the
/// session's actual result.
pub fn write_pane_snapshot(session_name: &str, content: &str) {
    write_pane_snapshot_in(&pane_snapshot_dir(), session_name, content);
}

/// Implementation behind [`read_pane_snapshot`], taking `dir` explicitly so
/// it's unit-testable against a throwaway directory instead of the real
/// `state_dir()`.
fn read_pane_snapshot_in(dir: &std::path::Path, session_name: &str) -> Option<String> {
    // RAL-247: redact on read too — a snapshot written before this fix (or by
    // a version without it) may already carry a secret value on disk.
    std::fs::read_to_string(pane_snapshot_path_in(dir, session_name))
        .ok()
        .filter(|s| !s.is_empty())
        .map(|s| ralphus_core::redact::redact_secrets(&s).into_owned())
}

/// Read back a session's persisted last-pane-content snapshot, if one was
/// ever written (see [`write_pane_snapshot`]). `None` when no snapshot
/// exists (the session never ran as a direct run, or its attempt(s) produced no
/// pane output at all) — the caller degrades to its own "nothing to show"
/// behavior in that case, never treating a missing file as an error.
#[must_use]
pub fn read_pane_snapshot(session_name: &str) -> Option<String> {
    read_pane_snapshot_in(&pane_snapshot_dir(), session_name)
}

/// Implementation behind [`delete_pane_snapshot`], taking `dir` explicitly so
/// it's unit-testable against a throwaway directory instead of the real
/// `state_dir()`.
fn delete_pane_snapshot_in(dir: &std::path::Path, session_name: &str) -> bool {
    let path = pane_snapshot_path_in(dir, session_name);
    match std::fs::remove_file(&path) {
        Ok(()) => true,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
        Err(e) => {
            // ralphus[ignore-rlog-pair]: filesystem cleanup helper has no Store; the retirement caller records the structured outcome
            crate::rlog!(
                WARNING,
                "ralphus [cell-session] could not delete pane snapshot for session {session_name} at {}: {e}",
                path.display()
            );
            false
        }
    }
}

/// Delete a session's persisted pane snapshot outright, if one exists.
/// Returns whether a snapshot file was removed. Best-effort: "already gone"
/// is silent and any other I/O error is logged as a warning,
/// matching `terminal_log::delete_for_session`'s cleanup contract -- called
/// by `crate::worktree_transcript_retirement` once the worktree a session
/// ran in has itself been retired (RAL-348). This deletes rather than
/// archives; unlike local worktree pruning, which has a remote backup,
/// pane-snapshot archival before deletion has no destination yet and is
/// deliberately deferred future work.
pub fn delete_pane_snapshot(session_name: &str) -> bool {
    delete_pane_snapshot_in(&pane_snapshot_dir(), session_name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_name_is_deterministic_and_sanitized() {
        let a = session_name("run-1", "build", "session/with weird:chars");
        let b = session_name("run-1", "build", "session/with weird:chars");
        assert_eq!(a, b);
        assert!(
            a.chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        );
        assert!(a.starts_with("ralphus_run-1_build_session"));
    }

    #[test]
    fn session_name_differs_for_different_sessions() {
        assert_ne!(
            session_name("run-1", "t", "a"),
            session_name("run-1", "t", "b")
        );
        assert_ne!(
            session_name("run-1", "t", "a"),
            session_name("run-2", "t", "a")
        );
    }

    #[test]
    fn session_name_differs_for_different_tasks_sharing_a_session_id() {
        // RAL-102 collision bug: sibling tasks that both use the conventional
        // session id "work" must not produce the same cell session name.
        assert_ne!(
            session_name("run-1", "task-a", "work"),
            session_name("run-1", "task-b", "work"),
        );
    }

    #[test]
    fn session_name_truncates_long_ids() {
        let long_id = "x".repeat(500);
        let name = session_name("run-1", "build", &long_id);
        assert!(name.len() <= 200);
    }

    /// A throwaway directory for a pane-snapshot test, cleaned up on drop —
    /// keeps these tests off the real `state_dir()`.
    struct TempSnapshotDir(PathBuf);
    impl TempSnapshotDir {
        fn new(unique: &str) -> Self {
            let dir = std::env::temp_dir().join(format!("ralphus-test-pane-snapshots-{unique}"));
            let _ = std::fs::remove_dir_all(&dir);
            Self(dir)
        }
    }
    impl Drop for TempSnapshotDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn pane_snapshot_round_trips() {
        let dir = TempSnapshotDir::new("round-trip");
        write_pane_snapshot_in(&dir.0, "ralphus_run-1_build_work", "line one\nline two");
        assert_eq!(
            read_pane_snapshot_in(&dir.0, "ralphus_run-1_build_work"),
            Some("line one\nline two".to_string())
        );
    }

    #[test]
    fn pane_snapshot_redacts_registered_secret_values() {
        // RAL-264: the pane snapshot is the durable "what was last there"
        // record the board's Live View shows; a pane that echoed the agent's
        // own `$env:... = 'sk-or-v1-...'` assignment must not persist the
        // resolved value under it.
        crate::redact::with_registry_lock(|| {
            crate::redact::clear_for_tests();
            crate::redact::register("sk-or-v1-pane-snapshot-test-token");

            let dir = TempSnapshotDir::new("redact");
            write_pane_snapshot_in(
                &dir.0,
                "s",
                "$env:ANTHROPIC_AUTH_TOKEN = 'sk-or-v1-pane-snapshot-test-token'\ndone",
            );
            let content = read_pane_snapshot_in(&dir.0, "s").expect("snapshot written");
            assert!(
                !content.contains("sk-or-v1-pane-snapshot-test-token"),
                "secret leaked into pane snapshot: {content}"
            );
            assert!(content.contains(crate::redact::REDACTED));
            assert!(content.contains("done"));
        });
    }

    #[test]
    fn pane_snapshot_missing_file_is_none() {
        let dir = TempSnapshotDir::new("missing");
        assert_eq!(read_pane_snapshot_in(&dir.0, "never-written"), None);
    }

    #[test]
    fn delete_pane_snapshot_removes_the_file() {
        let dir = TempSnapshotDir::new("delete");
        write_pane_snapshot_in(&dir.0, "s", "some output");
        assert!(pane_snapshot_path_in(&dir.0, "s").exists());
        delete_pane_snapshot_in(&dir.0, "s");
        assert!(!pane_snapshot_path_in(&dir.0, "s").exists());
    }

    #[test]
    fn delete_pane_snapshot_of_a_never_written_session_is_a_silent_no_op() {
        let dir = TempSnapshotDir::new("delete-missing");
        delete_pane_snapshot_in(&dir.0, "never-written");
    }

    #[test]
    fn write_pane_snapshot_redacts_secret_env_values_from_disk() {
        // RAL-247: the persisted pane-snapshot *file* must never contain a
        // credential env-var value, even when the captured pane did.
        let dir = TempSnapshotDir::new("redact-secret");
        let secret = "sk-snapshot-leak-guard";
        let content = format!("$env:ANTHROPIC_AUTH_TOKEN = '{secret}';\nordinary line");
        write_pane_snapshot_in(&dir.0, "s", &content);
        let disk =
            std::fs::read_to_string(pane_snapshot_path_in(&dir.0, "s")).expect("snapshot written");
        assert!(
            !disk.contains(secret),
            "credential value leaked into the pane-snapshot file: {disk}"
        );
        assert!(disk.contains("[REDACTED]"), "{disk}");
        assert!(
            disk.contains("ordinary line"),
            "non-secret content must be preserved: {disk}"
        );
    }

    #[test]
    fn read_pane_snapshot_redacts_secret_values_from_a_legacy_file() {
        // RAL-247: a snapshot file written before this fix may already carry
        // a secret value — the read path must scrub it defensively too.
        let dir = TempSnapshotDir::new("read-redacts-legacy");
        let secret = "sk-snapshot-legacy";
        let path = pane_snapshot_path_in(&dir.0, "s");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, format!("$env:ANTHROPIC_API_KEY = '{secret}'")).unwrap();
        let content = read_pane_snapshot_in(&dir.0, "s").expect("legacy snapshot readable");
        assert!(
            !content.contains(secret),
            "credential value leaked from a legacy snapshot: {content}"
        );
        assert!(content.contains("[REDACTED]"), "{content}");
    }

    #[test]
    fn pane_snapshot_empty_content_reads_back_as_none() {
        // An attempt that produced no pane output at all writes an empty
        // file (see `write_pane_snapshot`'s doc comment) — the reader treats
        // that the same as "no snapshot", not a real-but-blank record, so
        // callers don't have to special-case an empty string.
        let dir = TempSnapshotDir::new("empty");
        write_pane_snapshot_in(&dir.0, "s", "");
        assert_eq!(read_pane_snapshot_in(&dir.0, "s"), None);
    }

    #[test]
    fn pane_snapshot_overwrites_a_prior_attempt() {
        let dir = TempSnapshotDir::new("overwrite");
        write_pane_snapshot_in(&dir.0, "s", "first attempt's output");
        write_pane_snapshot_in(&dir.0, "s", "second attempt's output");
        assert_eq!(
            read_pane_snapshot_in(&dir.0, "s"),
            Some("second attempt's output".to_string())
        );
    }

    #[test]
    fn pane_snapshot_is_truncated_to_the_line_limit() {
        let dir = TempSnapshotDir::new("truncate");
        let lines: Vec<String> = (0..PANE_SNAPSHOT_MAX_LINES + 500)
            .map(|i| format!("line {i}"))
            .collect();
        write_pane_snapshot_in(&dir.0, "s", &lines.join("\n"));
        let saved = read_pane_snapshot_in(&dir.0, "s").expect("snapshot written");
        assert_eq!(saved.lines().count(), PANE_SNAPSHOT_MAX_LINES);
        // Keeps the *last* lines (the most recent output), not the first.
        assert!(saved.ends_with(&format!("line {}", PANE_SNAPSHOT_MAX_LINES + 499)));
    }

    #[test]
    fn pane_snapshot_different_sessions_do_not_collide() {
        let dir = TempSnapshotDir::new("distinct");
        write_pane_snapshot_in(&dir.0, "session-a", "a's output");
        write_pane_snapshot_in(&dir.0, "session-b", "b's output");
        assert_eq!(
            read_pane_snapshot_in(&dir.0, "session-a"),
            Some("a's output".to_string())
        );
        assert_eq!(
            read_pane_snapshot_in(&dir.0, "session-b"),
            Some("b's output".to_string())
        );
    }
}
