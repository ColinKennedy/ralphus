//! Guardian: the review + stacked-rebase system.
//!
//! A *guardian* (a review) collects a set of feature branches, rebases them into
//! a single linear stack on top of a base branch (resolving conflicts along the
//! way), and exposes the result for approval. This module holds the persistence
//! (guardian + branch rows) as methods on [`Store`]; the git mechanics live in
//! `guardian_git.rs` and the orchestration in the server/merge path.

use rusqlite::{OptionalExtension, params};
use serde::Serialize;

use crate::store::{Result, Store, StoreError};

/// Lifecycle state of a guardian/review.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum GuardianStatus {
    /// Accepting branches; not yet merged.
    Collecting,
    /// Building the stacked rebase.
    Merging,
    /// A merge/rebase step failed and needs attention.
    MergeFailed,
    /// Stack built; awaiting human review.
    InReview,
    /// Approved by a human.
    Approved,
    /// Deployed (terminal; deploy itself is a stub).
    Deployed,
}

impl GuardianStatus {
    /// The stored lowercase string.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Collecting => "collecting",
            Self::Merging => "merging",
            Self::MergeFailed => "merge_failed",
            Self::InReview => "in_review",
            Self::Approved => "approved",
            Self::Deployed => "deployed",
        }
    }

    fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "collecting" => Self::Collecting,
            "merging" => Self::Merging,
            "merge_failed" => Self::MergeFailed,
            "in_review" => Self::InReview,
            "approved" => Self::Approved,
            "deployed" => Self::Deployed,
            _ => return None,
        })
    }
}

/// Per-branch merge state within a guardian.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MergeStatus {
    /// Not yet merged.
    Pending,
    /// Being rebased onto the stack.
    InProgress,
    /// Rebased cleanly.
    Done,
    /// Rebased after resolving conflicts.
    ConflictResolved,
    /// Failed to rebase.
    Failed,
}

impl MergeStatus {
    /// The stored lowercase string.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::InProgress => "in_progress",
            Self::Done => "done",
            Self::ConflictResolved => "conflict_resolved",
            Self::Failed => "failed",
        }
    }
}

/// A branch row in a guardian, for display.
#[derive(Debug, Clone, Serialize)]
pub struct BranchView {
    /// Order position (0-based).
    pub position: i64,
    /// The feature branch name.
    pub branch: String,
    /// Merge status string.
    pub merge_status: String,
    /// Optional detail (e.g. conflict summary).
    pub detail: Option<String>,
}

/// A guardian/review, for display.
#[derive(Debug, Clone, Serialize)]
pub struct GuardianView {
    /// Guardian id, e.g. `guardian-000000000001`.
    pub id: String,
    /// Human name.
    pub name: String,
    /// Base branch the stack rebases onto.
    pub base_branch: String,
    /// Absolute path to the git repository.
    pub git_root: String,
    /// The resulting review branch, once built.
    pub review_branch: Option<String>,
    /// Status string.
    pub status: String,
    /// Optional detail (e.g. merge failure reason).
    pub detail: Option<String>,
    /// The run this review was derived from, if any (manual reviews have none).
    pub run_id: Option<String>,
    /// Creation time (epoch ms).
    pub created_at_ms: i64,
    /// Shell check commands run in the review worktree before it is ready.
    pub checks: Vec<String>,
    /// The ordered branches.
    pub branches: Vec<BranchView>,
}

/// The ordered `(position, branch)` list a merge run consumes.
#[derive(Debug, Clone)]
pub struct OrderedBranch {
    /// Order position.
    pub position: i64,
    /// Branch name.
    pub branch: String,
}

impl Store {
    /// Create a guardian in the `Collecting` state; returns its id.
    pub fn create_guardian(&self, name: &str, base_branch: &str, git_root: &str) -> Result<String> {
        self.create_guardian_for_run(name, base_branch, git_root, None)
    }

    /// Create a guardian, optionally tagged with the run it was derived from.
    pub fn create_guardian_for_run(
        &self,
        name: &str,
        base_branch: &str,
        git_root: &str,
        run_id: Option<&str>,
    ) -> Result<String> {
        let id = self.next_id("guardian_seq", "guardian")?;
        let now = crate::store::now_ms();
        self.conn.execute(
            "INSERT INTO guardians(id, name, base_branch, git_root, review_branch, status, detail, run_id, created_at_ms, updated_at_ms)
             VALUES(?,?,?,?,NULL,?,NULL,?,?,?)",
            params![id, name, base_branch, git_root, GuardianStatus::Collecting.as_str(), run_id, now, now],
        )?;
        Ok(id)
    }

    /// Ids of the guardians derived from a run, oldest first.
    pub fn guardians_for_run(&self, run_id: &str) -> Result<Vec<String>> {
        let mut stmt = self
            .conn
            .prepare("SELECT id FROM guardians WHERE run_id=? ORDER BY created_at_ms, id")?;
        let ids = stmt
            .query_map(params![run_id], |r| r.get::<_, String>(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(ids)
    }

    /// Append a branch to a guardian at the next position. Returns its position.
    pub fn add_guardian_branch(&self, guardian_id: &str, branch: &str) -> Result<i64> {
        self.guardian_exists(guardian_id)?;
        let next: i64 = self.conn.query_row(
            "SELECT COALESCE(MAX(position) + 1, 0) FROM guardian_branches WHERE guardian_id=?",
            params![guardian_id],
            |r| r.get(0),
        )?;
        self.conn.execute(
            "INSERT INTO guardian_branches(guardian_id, position, branch, merge_status, detail)
             VALUES(?,?,?,?,NULL)",
            params![guardian_id, next, branch, MergeStatus::Pending.as_str()],
        )?;
        Ok(next)
    }

    fn guardian_exists(&self, id: &str) -> Result<()> {
        let found: Option<i64> = self
            .conn
            .query_row("SELECT 1 FROM guardians WHERE id=?", params![id], |r| {
                r.get(0)
            })
            .optional()?;
        found.map(|_| ()).ok_or(StoreError::NotFound)
    }

    /// Set a guardian's status (and optional detail).
    pub fn set_guardian_status(
        &self,
        id: &str,
        status: GuardianStatus,
        detail: Option<&str>,
    ) -> Result<()> {
        let n = self.conn.execute(
            "UPDATE guardians SET status=?, detail=?, updated_at_ms=? WHERE id=?",
            params![status.as_str(), detail, crate::store::now_ms(), id],
        )?;
        if n == 0 {
            Err(StoreError::NotFound)
        } else {
            Ok(())
        }
    }

    /// Set the shell check commands run against the review worktree.
    pub fn set_guardian_checks(&self, id: &str, checks: &[String]) -> Result<()> {
        self.conn.execute(
            "UPDATE guardians SET checks=?, updated_at_ms=? WHERE id=?",
            params![crate::store::to_json(checks), crate::store::now_ms(), id],
        )?;
        Ok(())
    }

    /// The shell check commands for a guardian.
    pub fn guardian_checks(&self, id: &str) -> Result<Vec<String>> {
        let s: Option<String> = self
            .conn
            .query_row(
                "SELECT checks FROM guardians WHERE id=?",
                params![id],
                |r| r.get(0),
            )
            .optional()?;
        Ok(crate::store::from_json(&s.ok_or(StoreError::NotFound)?))
    }

    /// Set the built review branch name.
    pub fn set_guardian_review_branch(&self, id: &str, branch: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE guardians SET review_branch=?, updated_at_ms=? WHERE id=?",
            params![branch, crate::store::now_ms(), id],
        )?;
        Ok(())
    }

    /// Set a branch's merge status (and optional detail).
    pub fn set_branch_status(
        &self,
        guardian_id: &str,
        position: i64,
        status: MergeStatus,
        detail: Option<&str>,
    ) -> Result<()> {
        self.conn.execute(
            "UPDATE guardian_branches SET merge_status=?, detail=? WHERE guardian_id=? AND position=?",
            params![status.as_str(), detail, guardian_id, position],
        )?;
        Ok(())
    }

    /// Reorder a guardian's branches to match `order` (a permutation of the
    /// existing branch names). Positions are first shifted out of range to avoid
    /// colliding with the `(guardian_id, position)` primary key, then rewritten.
    /// Branch names in `order` that no longer exist are ignored; any left over
    /// keep their relative order after the reordered ones.
    pub fn reorder_guardian_branches(&mut self, guardian_id: &str, order: &[String]) -> Result<()> {
        self.guardian_exists(guardian_id)?;
        let tx = self.conn.transaction()?;
        const SHIFT: i64 = 1_000_000;
        tx.execute(
            "UPDATE guardian_branches SET position = position + ?1 WHERE guardian_id=?2",
            params![SHIFT, guardian_id],
        )?;
        let mut next: i64 = 0;
        for branch in order {
            let updated = tx.execute(
                "UPDATE guardian_branches SET position=?1
                 WHERE guardian_id=?2 AND branch=?3 AND position >= ?4",
                params![next, guardian_id, branch, SHIFT],
            )?;
            if updated > 0 {
                next += 1;
            }
        }
        // Compact any branches not named in `order`, preserving their order.
        let mut stmt = tx.prepare(
            "SELECT position FROM guardian_branches
             WHERE guardian_id=?1 AND position >= ?2 ORDER BY position",
        )?;
        let leftover: Vec<i64> = stmt
            .query_map(params![guardian_id, SHIFT], |r| r.get(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        drop(stmt);
        for old_pos in leftover {
            tx.execute(
                "UPDATE guardian_branches SET position=?1 WHERE guardian_id=?2 AND position=?3",
                params![next, guardian_id, old_pos],
            )?;
            next += 1;
        }
        tx.commit()?;
        Ok(())
    }

    /// The ordered branches of a guardian.
    pub fn guardian_branches(&self, guardian_id: &str) -> Result<Vec<OrderedBranch>> {
        let mut stmt = self.conn.prepare(
            "SELECT position, branch FROM guardian_branches WHERE guardian_id=? ORDER BY position",
        )?;
        let rows = stmt
            .query_map(params![guardian_id], |r| {
                Ok(OrderedBranch {
                    position: r.get(0)?,
                    branch: r.get(1)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Fetch a single guardian view.
    pub fn get_guardian(&self, id: &str) -> Result<GuardianView> {
        let row = self
            .conn
            .query_row(
                "SELECT id, name, base_branch, git_root, review_branch, status, detail, checks, run_id, created_at_ms
                 FROM guardians WHERE id=?",
                params![id],
                Self::map_guardian_row,
            )
            .optional()?
            .ok_or(StoreError::NotFound)?;
        self.hydrate_guardian(row)
    }

    /// List all guardians, newest first.
    pub fn list_guardians(&self) -> Result<Vec<GuardianView>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, name, base_branch, git_root, review_branch, status, detail, checks, run_id, created_at_ms
             FROM guardians ORDER BY created_at_ms DESC",
        )?;
        let rows = stmt
            .query_map([], Self::map_guardian_row)?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        rows.into_iter().map(|r| self.hydrate_guardian(r)).collect()
    }

    fn map_guardian_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<GuardianRow> {
        Ok(GuardianRow {
            id: r.get(0)?,
            name: r.get(1)?,
            base_branch: r.get(2)?,
            git_root: r.get(3)?,
            review_branch: r.get(4)?,
            status: r.get(5)?,
            detail: r.get(6)?,
            checks: r.get(7)?,
            run_id: r.get(8)?,
            created_at_ms: r.get(9)?,
        })
    }

    fn hydrate_guardian(&self, row: GuardianRow) -> Result<GuardianView> {
        let mut stmt = self.conn.prepare(
            "SELECT position, branch, merge_status, detail FROM guardian_branches
             WHERE guardian_id=? ORDER BY position",
        )?;
        let branches = stmt
            .query_map(params![row.id], |r| {
                Ok(BranchView {
                    position: r.get(0)?,
                    branch: r.get(1)?,
                    merge_status: r.get(2)?,
                    detail: r.get(3)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(GuardianView {
            id: row.id,
            name: row.name,
            base_branch: row.base_branch,
            git_root: row.git_root,
            review_branch: row.review_branch,
            status: row.status,
            detail: row.detail,
            run_id: row.run_id,
            created_at_ms: row.created_at_ms,
            checks: crate::store::from_json(&row.checks),
            branches,
        })
    }

    /// Approve a guardian that is in review.
    pub fn approve_guardian(&self, id: &str) -> Result<GuardianStatus> {
        match GuardianStatus::parse(&self.guardian_status_str(id)?) {
            Some(GuardianStatus::InReview) => {
                self.set_guardian_status(id, GuardianStatus::Approved, None)?;
                Ok(GuardianStatus::Approved)
            }
            Some(other) => Err(StoreError::InvalidTransition(format!(
                "can only approve a guardian in review, it is {}",
                other.as_str()
            ))),
            None => Err(StoreError::NotFound),
        }
    }

    fn guardian_status_str(&self, id: &str) -> Result<String> {
        self.conn
            .query_row(
                "SELECT status FROM guardians WHERE id=?",
                params![id],
                |r| r.get(0),
            )
            .optional()?
            .ok_or(StoreError::NotFound)
    }
}

struct GuardianRow {
    id: String,
    name: String,
    base_branch: String,
    git_root: String,
    review_branch: Option<String>,
    status: String,
    detail: Option<String>,
    checks: String,
    run_id: Option<String>,
    created_at_ms: i64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_add_and_fetch() {
        let store = Store::open_in_memory().unwrap();
        let id = store.create_guardian("my review", "main", "/repo").unwrap();
        assert_eq!(id, "guardian-000000000001");
        assert_eq!(store.add_guardian_branch(&id, "feature/a").unwrap(), 0);
        assert_eq!(store.add_guardian_branch(&id, "feature/b").unwrap(), 1);

        let g = store.get_guardian(&id).unwrap();
        assert_eq!(g.name, "my review");
        assert_eq!(g.status, "collecting");
        assert_eq!(g.branches.len(), 2);
        assert_eq!(g.branches[0].branch, "feature/a");
        assert_eq!(g.branches[1].position, 1);
    }

    #[test]
    fn status_transitions_and_approve() {
        let store = Store::open_in_memory().unwrap();
        let id = store.create_guardian("r", "main", "/repo").unwrap();
        assert!(store.approve_guardian(&id).is_err()); // not in review yet
        store
            .set_guardian_status(&id, GuardianStatus::InReview, None)
            .unwrap();
        assert_eq!(
            store.approve_guardian(&id).unwrap(),
            GuardianStatus::Approved
        );
        assert_eq!(store.get_guardian(&id).unwrap().status, "approved");
    }

    #[test]
    fn branch_status_and_review_branch() {
        let store = Store::open_in_memory().unwrap();
        let id = store.create_guardian("r", "main", "/repo").unwrap();
        store.add_guardian_branch(&id, "feat").unwrap();
        store
            .set_branch_status(&id, 0, MergeStatus::ConflictResolved, Some("2 files"))
            .unwrap();
        store
            .set_guardian_review_branch(&id, "guardian/abc")
            .unwrap();
        let g = store.get_guardian(&id).unwrap();
        assert_eq!(g.branches[0].merge_status, "conflict_resolved");
        assert_eq!(g.branches[0].detail.as_deref(), Some("2 files"));
        assert_eq!(g.review_branch.as_deref(), Some("guardian/abc"));
    }

    #[test]
    fn missing_guardian_is_not_found() {
        let store = Store::open_in_memory().unwrap();
        assert!(matches!(
            store.get_guardian("nope"),
            Err(StoreError::NotFound)
        ));
        assert!(store.add_guardian_branch("nope", "x").is_err());
    }

    #[test]
    fn reorder_branches_permutes_positions() {
        let mut store = Store::open_in_memory().unwrap();
        let id = store.create_guardian("r", "main", "/repo").unwrap();
        store.add_guardian_branch(&id, "a").unwrap();
        store.add_guardian_branch(&id, "b").unwrap();
        store.add_guardian_branch(&id, "c").unwrap();

        store
            .reorder_guardian_branches(&id, &["c".into(), "a".into(), "b".into()])
            .unwrap();
        let g = store.get_guardian(&id).unwrap();
        assert_eq!(
            g.branches
                .iter()
                .map(|b| b.branch.as_str())
                .collect::<Vec<_>>(),
            vec!["c", "a", "b"]
        );
        assert_eq!(g.branches[0].position, 0);
        assert_eq!(g.branches[2].position, 2);
    }

    #[test]
    fn reorder_ignores_unknown_and_keeps_leftovers() {
        let mut store = Store::open_in_memory().unwrap();
        let id = store.create_guardian("r", "main", "/repo").unwrap();
        store.add_guardian_branch(&id, "a").unwrap();
        store.add_guardian_branch(&id, "b").unwrap();
        // Only name "b"; "a" is a leftover and "zzz" does not exist.
        store
            .reorder_guardian_branches(&id, &["b".into(), "zzz".into()])
            .unwrap();
        let g = store.get_guardian(&id).unwrap();
        assert_eq!(
            g.branches
                .iter()
                .map(|b| b.branch.as_str())
                .collect::<Vec<_>>(),
            vec!["b", "a"]
        );
    }

    #[test]
    fn ordered_branches() {
        let store = Store::open_in_memory().unwrap();
        let id = store.create_guardian("r", "main", "/repo").unwrap();
        store.add_guardian_branch(&id, "a").unwrap();
        store.add_guardian_branch(&id, "b").unwrap();
        let ordered = store.guardian_branches(&id).unwrap();
        assert_eq!(
            ordered
                .iter()
                .map(|b| b.branch.as_str())
                .collect::<Vec<_>>(),
            vec!["a", "b"]
        );
    }
}
