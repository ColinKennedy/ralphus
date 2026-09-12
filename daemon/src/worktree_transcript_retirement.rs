//! Retire pane-snapshot and terminal-log transcript files once the worktree
//! that produced them is retired (RAL-348).
//!
//! `daemon/src/terminal_log.rs`'s `prune()` already ages/caps `.log`/`.raw`
//! transcript files on its own tick-loop timer (`TerminalLogConfig`,
//! `scheduler.rs`), independent of worktree state -- that scheme is
//! untouched by this module. Pane-snapshot files (`tmux::pane_snapshot_dir`)
//! had no pruning at all before this ticket; that gap is what this module
//! closes.
//!
//! Rather than run its own independent timer, this module piggybacks on the
//! existing worktree-retirement sweep
//! (`guardian_merge::retire_stale_worktrees`): once a worktree is confirmed
//! removed, every cell/proof session that ever ran with that worktree as its
//! `cwd` -- across every attempt and restart of every cell, for the entire
//! lifetime of the worktree -- is also safe to forget. A worktree only
//! retires once every claim against it (cell, proof, review) is terminal
//! (see `guardian_merge::terminal_worktree_claim`), so this never removes a
//! transcript or pane snapshot still referenced by live/recent squad, task,
//! or cell state.
//!
//! This deletes outright rather than archiving. Local worktree pruning has a
//! remote backup (the branch/commits live on the remote), so deleting the
//! local checkout loses nothing durable; transcripts and pane snapshots have
//! no such backup today. Archiving them somewhere before deletion is a
//! deliberately deferred future improvement, not an oversight.
//!
//! No separate retention policy or config surface exists for
//! transcripts/pane-snapshots: retirement here inherits whatever
//! worktree-retirement policy already applies to the project (age threshold,
//! per-machine opt-out, etc. -- see `docs/site/pages/views/worktree-retirement.md`).
//! A project with no worktree-retirement override simply has no override for
//! this either.

use crate::store::Store;

/// Delete every pane-snapshot and terminal-log transcript belonging to a
/// session that ran with `worktree_path` as its `cwd`, across every
/// cell/proof and every attempt/restart. Called only once a worktree's
/// removal is confirmed (`guardian_merge::retire_stale_worktrees`'s
/// `RetirementOutcome::Removed` arm) -- never for a worktree that is merely
/// eligible, deferred, or still claimed.
///
/// Returns the number of distinct cell/proof sessions swept, so the caller
/// can fold it into its own Cartographer note. Best-effort throughout: a
/// missing file, a store read error, or an individual deletion failure is
/// swallowed (deletion here is a cleanup convenience, never something that
/// should block or fail worktree retirement itself, which has already
/// happened by the time this runs).
pub(crate) fn retire_session_artifacts_for_worktree(store: &Store, worktree_path: &str) -> usize {
    let owners = match store.worktree_session_owners() {
        Ok(owners) => owners,
        Err(error) => {
            crate::rlog!(
                WARNING,
                "ralphus [guardian] could not list session owners while retiring transcripts for worktree {worktree_path}: {error}"
            );
            return 0;
        }
    };
    let target =
        crate::guardian_merge::normalized_worktree_path(std::path::Path::new(worktree_path));
    let mut swept = 0;
    for owner in owners {
        if crate::guardian_merge::normalized_worktree_path(std::path::Path::new(&owner.cwd))
            != target
        {
            continue;
        }
        delete_session_artifacts(&crate::tmux::session_name(
            &owner.squad_id,
            &owner.task_name,
            &owner.session_id,
        ));
        // A restarted/resumed cell runs a second tmux session under the same
        // sid with a fixed `-resume` suffix (see `server.rs`'s resume
        // endpoints) rather than allocating a new sid, so sweeping this one
        // extra, deterministic name covers every restart without needing
        // per-attempt bookkeeping. Best-effort deletion makes this a
        // harmless no-op for a session that was never restarted.
        delete_session_artifacts(&crate::tmux::session_name(
            &owner.squad_id,
            &owner.task_name,
            &format!("{}-resume", owner.session_id),
        ));
        swept += 1;
    }
    swept
}

fn delete_session_artifacts(session_name: &str) {
    crate::tmux::delete_pane_snapshot(session_name);
    crate::terminal_log::delete_for_session(session_name);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Store;

    fn temp_dir(label: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "ralphus-worktree-transcript-retirement-test-{label}-{:?}",
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    /// Ensure `squad_id`'s squad/task-0 rows exist, then insert a cell row
    /// under them with `cwd` set -- mirrors
    /// `guardian_merge::tests::insert_done_cell`, split so a test can insert
    /// several cells into the same squad/task.
    fn insert_cell(store: &Store, squad_id: &str, idx: i64, sid: &str, cwd: &str) {
        store
            .conn
            .execute(
                "INSERT OR IGNORE INTO squads (id, label, state, depends_on, created_at_ms, updated_at_ms) \
                 VALUES (?, NULL, 'done', '[]', 0, 0)",
                rusqlite::params![squad_id],
            )
            .unwrap();
        store
            .conn
            .execute(
                "INSERT OR IGNORE INTO tasks (squad_id, idx, name, state, depends_on) \
                 VALUES (?, 0, 'task-a', 'done', '[]')",
                rusqlite::params![squad_id],
            )
            .unwrap();
        store
            .conn
            .execute(
                "INSERT INTO cells (squad_id, task_idx, idx, sid, cwd, agent, state, depends_on) \
                 VALUES (?, 0, ?, ?, ?, 'claude', 'done', '[]')",
                rusqlite::params![squad_id, idx, sid, cwd],
            )
            .unwrap();
    }

    #[test]
    fn sweeps_pane_snapshot_and_terminal_log_for_matching_cwd_only() {
        let pane_dir = temp_dir("pane");
        let log_dir = temp_dir("log");
        crate::tmux::set_pane_snapshot_test_root(pane_dir.clone());
        crate::terminal_log::set_test_root(log_dir.clone());

        let store = Store::open_in_memory().expect("open store");
        insert_cell(&store, "sq1", 0, "sid-in-worktree", "/repo/w/wt-1");
        insert_cell(&store, "sq1", 1, "sid-elsewhere", "/repo/w/wt-2");

        let in_worktree_session = crate::tmux::session_name("sq1", "task-a", "sid-in-worktree");
        let elsewhere_session = crate::tmux::session_name("sq1", "task-a", "sid-elsewhere");
        crate::tmux::write_pane_snapshot(&in_worktree_session, "in worktree output");
        crate::tmux::write_pane_snapshot(&elsewhere_session, "elsewhere output");
        crate::terminal_log::write_attempt(&in_worktree_session, 0, "log line", 100);
        crate::terminal_log::write_attempt(&elsewhere_session, 0, "log line", 100);

        let swept = retire_session_artifacts_for_worktree(&store, "/repo/w/wt-1");
        assert_eq!(swept, 1);
        assert_eq!(crate::tmux::read_pane_snapshot(&in_worktree_session), None);
        assert!(crate::tmux::read_pane_snapshot(&elsewhere_session).is_some());
        assert!(crate::terminal_log::read_attempt(&in_worktree_session, 0).is_none());
        assert!(crate::terminal_log::read_attempt(&elsewhere_session, 0).is_some());

        let _ = std::fs::remove_dir_all(&pane_dir);
        let _ = std::fs::remove_dir_all(&log_dir);
    }

    #[test]
    fn sweeps_the_resume_session_too() {
        let pane_dir = temp_dir("resume-pane");
        let log_dir = temp_dir("resume-log");
        crate::tmux::set_pane_snapshot_test_root(pane_dir.clone());
        crate::terminal_log::set_test_root(log_dir.clone());

        let store = Store::open_in_memory().expect("open store");
        insert_cell(&store, "sq1", 0, "sid-1", "/repo/w/wt-1");

        let resume_session = crate::tmux::session_name("sq1", "task-a", "sid-1-resume");
        crate::tmux::write_pane_snapshot(&resume_session, "resumed output");

        let swept = retire_session_artifacts_for_worktree(&store, "/repo/w/wt-1");
        assert_eq!(swept, 1);
        assert_eq!(crate::tmux::read_pane_snapshot(&resume_session), None);

        let _ = std::fs::remove_dir_all(&pane_dir);
        let _ = std::fs::remove_dir_all(&log_dir);
    }

    #[test]
    fn nonexistent_worktree_sweeps_nothing() {
        let store = Store::open_in_memory().expect("open store");
        assert_eq!(
            retire_session_artifacts_for_worktree(&store, "/repo/w/no-such-worktree"),
            0
        );
    }
}
