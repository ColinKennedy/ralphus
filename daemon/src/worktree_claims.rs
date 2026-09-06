//! RAL-337: which squad owns a task worktree branch.
//!
//! A `ralphus:new-worktree/<branch>?upstream=<upstream>` placeholder names a
//! *base* branch. Left to the branch name alone, two different squads
//! submitting the same placeholder resolve to the same branch and the same
//! worktree, so the second squad opens a tree that already contains the
//! first squad's finished commits. The placeholder says `new-worktree`, so
//! that is the wrong answer.
//!
//! This module records the (project, base branch) -> owning squad mapping the
//! resolver needs to tell those two cases apart:
//!
//! - the squad that already claimed a branch in this family resolves back to
//!   the exact same branch, so `squad restart`/`squad retry` keep reusing
//!   their own worktree (the restart-safety property
//!   [`crate::worktrees::ensure_worktree`] documents);
//! - any other squad gets the next free `<base>-2`, `-3`, ... branch, which
//!   in turn lands in its own `w/<short>-2` directory via the collision
//!   suffixing [`crate::worktrees`] already applies to distinct branches.
//!
//! Only the mapping lives here. Choosing the branch needs live git state as
//! well (a worktree materialized before this table existed has no row, but
//! still occupies its branch), so that logic sits in
//! [`crate::worktrees::resolve_squad_branch`] next to the git calls.

use crate::store::{Result as StoreResult, Store, now_ms};

/// One recorded claim: `branch` in this base-branch family belongs to
/// `squad_id`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WorktreeClaim {
    pub branch: String,
    pub squad_id: String,
}

impl Store {
    /// The branch `squad_id` already claimed in `base_branch`'s family, if it
    /// has one.
    ///
    /// This is what makes resolution idempotent: every cell in a squad that
    /// names the same placeholder, and every later restart of that squad,
    /// resolves through this lookup to the branch the squad first claimed.
    ///
    /// # Errors
    /// Propagates any SQLite failure.
    pub(crate) fn task_worktree_claim_for_squad(
        &self,
        project: &str,
        base_branch: &str,
        squad_id: &str,
    ) -> StoreResult<Option<String>> {
        let mut q = self.conn.prepare(
            "SELECT branch FROM task_worktree_claims
             WHERE project = ? AND base_branch = ? AND squad_id = ?
             ORDER BY created_at_ms LIMIT 1",
        )?;
        let mut rows = q.query(rusqlite::params![project, base_branch, squad_id])?;
        match rows.next()? {
            Some(r) => Ok(Some(r.get::<_, String>(0)?)),
            None => Ok(None),
        }
    }

    /// Every claim in `base_branch`'s family, whichever squad holds it.
    ///
    /// # Errors
    /// Propagates any SQLite failure.
    pub(crate) fn task_worktree_claims(
        &self,
        project: &str,
        base_branch: &str,
    ) -> StoreResult<Vec<WorktreeClaim>> {
        let mut q = self.conn.prepare(
            "SELECT branch, squad_id FROM task_worktree_claims
             WHERE project = ? AND base_branch = ?
             ORDER BY created_at_ms",
        )?;
        let rows = q
            .query_map(rusqlite::params![project, base_branch], |r| {
                Ok(WorktreeClaim {
                    branch: r.get(0)?,
                    squad_id: r.get(1)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Record that `squad_id` owns `branch` (allocated from `base_branch`).
    ///
    /// `ON CONFLICT DO NOTHING` on the `(project, branch)` primary key: a
    /// concurrent resolver that already claimed this exact branch keeps it,
    /// rather than having its ownership overwritten by a later caller.
    ///
    /// # Errors
    /// Propagates any SQLite failure.
    pub(crate) fn record_task_worktree_claim(
        &self,
        project: &str,
        base_branch: &str,
        branch: &str,
        squad_id: &str,
    ) -> StoreResult<()> {
        self.conn.execute(
            "INSERT INTO task_worktree_claims(project, base_branch, branch, squad_id, created_at_ms)
             VALUES(?,?,?,?,?)
             ON CONFLICT(project, branch) DO NOTHING",
            rusqlite::params![project, base_branch, branch, squad_id, now_ms()],
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use crate::store::Store;

    fn store() -> Store {
        Store::open_in_memory().expect("open in-memory store")
    }

    #[test]
    fn a_squad_with_no_claim_reads_back_none() {
        let s = store();
        assert_eq!(
            s.task_worktree_claim_for_squad("proj", "feat", "squad-1")
                .expect("query"),
            None
        );
    }

    #[test]
    fn a_recorded_claim_reads_back_for_its_own_squad_only() {
        let s = store();
        s.record_task_worktree_claim("proj", "feat", "feat", "squad-1")
            .expect("record");
        assert_eq!(
            s.task_worktree_claim_for_squad("proj", "feat", "squad-1")
                .expect("query"),
            Some("feat".to_string())
        );
        // The whole point: a different squad must NOT see squad-1's branch as
        // its own, or it would inherit squad-1's commits.
        assert_eq!(
            s.task_worktree_claim_for_squad("proj", "feat", "squad-2")
                .expect("query"),
            None
        );
    }

    #[test]
    fn claims_are_listed_for_the_whole_family() {
        let s = store();
        s.record_task_worktree_claim("proj", "feat", "feat", "squad-1")
            .expect("record");
        s.record_task_worktree_claim("proj", "feat", "feat-2", "squad-2")
            .expect("record");
        let claims = s.task_worktree_claims("proj", "feat").expect("list");
        let pairs: Vec<(&str, &str)> = claims
            .iter()
            .map(|c| (c.branch.as_str(), c.squad_id.as_str()))
            .collect();
        assert_eq!(pairs, vec![("feat", "squad-1"), ("feat-2", "squad-2")]);
    }

    #[test]
    fn claims_are_scoped_per_project() {
        let s = store();
        s.record_task_worktree_claim("proj-a", "feat", "feat", "squad-1")
            .expect("record");
        assert!(
            s.task_worktree_claims("proj-b", "feat")
                .expect("list")
                .is_empty()
        );
        assert_eq!(
            s.task_worktree_claim_for_squad("proj-b", "feat", "squad-1")
                .expect("query"),
            None
        );
    }

    #[test]
    fn re_recording_the_same_branch_keeps_the_original_owner() {
        let s = store();
        s.record_task_worktree_claim("proj", "feat", "feat", "squad-1")
            .expect("record");
        s.record_task_worktree_claim("proj", "feat", "feat", "squad-2")
            .expect("record again");
        let claims = s.task_worktree_claims("proj", "feat").expect("list");
        assert_eq!(claims.len(), 1);
        assert_eq!(claims[0].squad_id, "squad-1");
    }
}
