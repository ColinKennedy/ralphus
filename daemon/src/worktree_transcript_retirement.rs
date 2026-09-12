//! RAL-348: retire pane-snapshot and terminal-log files alongside the
//! worktree that produced them.
//!
//! `crate::terminal_log` already prunes `.log`/`.raw` attempt files on an
//! independent age/count-based schedule (`crate::config::TerminalLogConfig`),
//! but pane snapshots (`crate::tmux::write_pane_snapshot`) have no pruning at
//! all, and a session's terminal logs can otherwise outlive the worktree they
//! document by weeks if that session's own attempts never age out on their
//! own. Rather than add a second independent timer, this module piggybacks
//! on the existing daily worktree-retirement sweep
//! (`crate::guardian_merge::retire_stale_worktrees`): once a worktree is
//! judged safe to remove, every cell and proof step that ever ran with that
//! worktree as its `cwd` is *also* safe to retire — across every attempt and
//! resume/restart of that cell, since a worktree only retires once nothing
//! still holds a non-terminal claim on it (see
//! `crate::store::Store::worktree_claims`).
//!
//! Deletion is unconditional, not archival: local worktree pruning has a
//! remote (git remote / forge) backup to fall back on, but a pane snapshot or
//! terminal log has no copy anywhere else once it is gone. Archiving these
//! before deletion is a deliberately deferred future improvement, not an
//! oversight — see the ticket.
//!
//! No separate configuration surface exists (or is planned) for this
//! retirement path: it always fires exactly when the owning worktree's own
//! retirement policy (`crate::guardian_merge::WORKTREE_RETIREMENT_AGE_MS`,
//! any per-machine `[machine.targets.*.retirement]` opt-out) says the
//! worktree itself may go. A project with no override for worktree
//! retirement simply has no override for this either.

use std::path::Path;

use crate::guardian_merge::normalized_worktree_path;
use crate::store::Store;

/// Delete every pane-snapshot and terminal-log artifact for a cell or proof
/// step that ever ran with `worktree_path` as its `cwd`. Called once
/// `crate::guardian_merge::retire_stale_worktrees` has confirmed
/// `worktree_path` itself was actually removed — never for a worktree that
/// is merely eligible, deferred, or claimed, so a live/recent squad, task, or
/// cell never loses its transcript out from under it.
///
/// Returns the number of sessions swept, purely so the caller can fold it
/// into its own Cartographer note; the sweep itself is best-effort and never
/// fails the retirement it's attached to (a leftover file is a bounded,
/// non-fatal disk-space cost, matching every other cleanup path here).
pub(crate) fn retire_session_artifacts_for_worktree(store: &Store, worktree_path: &str) -> usize {
    let owners = match store.worktree_session_owners() {
        Ok(owners) => owners,
        Err(error) => {
            crate::rlog!(
                WARNING,
                "ralphus [guardian] could not enumerate session owners while retiring worktree {worktree_path}: {error}"
            );
            return 0;
        }
    };
    let key = normalized_worktree_path(Path::new(worktree_path));
    let mut swept = 0usize;
    for owner in owners {
        if normalized_worktree_path(Path::new(&owner.cwd)) != key {
            continue;
        }
        delete_session_artifacts(&owner.squad_id, &owner.task_name, &owner.session_id);
        // A cell's session may additionally have been resumed under a
        // `{session_id}-resume` id (see `server.rs`'s resume endpoints) — a
        // proof never resumes, so this is a harmless no-op for a proof
        // owner.
        let resume_session_id = format!("{}-resume", owner.session_id);
        delete_session_artifacts(&owner.squad_id, &owner.task_name, &resume_session_id);
        swept += 1;
    }
    swept
}

fn delete_session_artifacts(squad_id: &str, task_name: &str, session_id: &str) {
    let session = crate::tmux::session_name(squad_id, task_name, session_id);
    crate::tmux::delete_pane_snapshot(&session);
    crate::terminal_log::delete_for_session(&session);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Store;

    fn store() -> Store {
        Store::open_in_memory().expect("open in-memory store")
    }

    fn insert_squad_task_cell(store: &Store, squad_id: &str, cwd: &str, sid: &str) {
        store
            .conn
            .execute(
                "INSERT INTO squads(id, project, name, status, created_at_ms, updated_at_ms) \
                 VALUES (?1, 'proj', 'squad', 'running', 0, 0)",
                rusqlite::params![squad_id],
            )
            .expect("insert squad");
        store
            .conn
            .execute(
                "INSERT INTO tasks(squad_id, idx, name, state) VALUES (?1, 0, 'build', 'running')",
                rusqlite::params![squad_id],
            )
            .expect("insert task");
        store
            .conn
            .execute(
                "INSERT INTO cells(squad_id, task_idx, idx, sid, cwd, agent, state, depends_on) \
                 VALUES (?1, 0, 0, ?2, ?3, 'claude', 'done', '[]')",
                rusqlite::params![squad_id, sid, cwd],
            )
            .expect("insert cell");
    }

    #[test]
    fn sweeps_pane_snapshot_and_terminal_log_for_a_matching_cell() {
        crate::tmux::test_support::with_isolated_pane_snapshot_dir(|| {
            crate::terminal_log::set_test_root(
                std::env::temp_dir().join("ralphus-test-transcript-retirement-match"),
            );
            let s = store();
            insert_squad_task_cell(&s, "squad-1", "/repo/worktrees/w1", "work");
            let session = crate::tmux::session_name("squad-1", "build", "work");
            crate::tmux::write_pane_snapshot(&session, "some output");
            crate::terminal_log::write_attempt(&session, 0, "attempt output");
            assert!(crate::tmux::read_pane_snapshot(&session).is_some());

            let swept = retire_session_artifacts_for_worktree(&s, "/repo/worktrees/w1");

            assert_eq!(swept, 1);
            assert_eq!(crate::tmux::read_pane_snapshot(&session), None);
            assert_eq!(crate::terminal_log::read_attempt(&session, 0), None);
        });
    }

    #[test]
    fn leaves_sessions_for_other_worktrees_untouched() {
        crate::tmux::test_support::with_isolated_pane_snapshot_dir(|| {
            crate::terminal_log::set_test_root(
                std::env::temp_dir().join("ralphus-test-transcript-retirement-other"),
            );
            let s = store();
            insert_squad_task_cell(&s, "squad-1", "/repo/worktrees/w1", "work");
            let live_session = crate::tmux::session_name("squad-1", "build", "work");
            crate::tmux::write_pane_snapshot(&live_session, "still live");

            let swept = retire_session_artifacts_for_worktree(&s, "/repo/worktrees/other");

            assert_eq!(swept, 0);
            assert!(crate::tmux::read_pane_snapshot(&live_session).is_some());
        });
    }
}
