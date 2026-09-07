//! Guardian pull-request submission + bidirectional sync (RAL-117, RAL-190).
//!
//! Three layers:
//! - **Data model** (`impl Store` below): CRUD over the `guardian_pull_requests`
//!   / `guardian_pr_feedback_actioned` tables (schema already created in
//!   `store.rs`). Mirrors how `guardian.rs`/`cartographer.rs` add `Store`
//!   methods from their own module rather than `store.rs` itself.
//! - **Submission + feedback** ([`submit_pull_requests`], [`action_pr_feedback`]):
//!   pushes a review branch to the forge and opens a PR/MR, and pulls PR
//!   comments back into the owning review worktree. A request either targets
//!   one specific stacked branch, or (`branch_id: None`) the whole stack —
//!   [`submit_stack_for_guardian`] submits a PR for every enabled branch that
//!   doesn't already have one (never a squashed one-PR-for-everything) and,
//!   on GitHub, registers/grows a native PR stack spanning them
//!   ([`decide_stack_action`], `ForgeClient::create_stack`/`add_to_stack`).
//!   The feedback path deliberately delegates to
//!   [`crate::guardian_merge::run_feedback`] instead of re-implementing
//!   worktree-edit/commit/downstream-restack logic: that function already is
//!   "apply text feedback to one branch, then restack everything downstream
//!   of it", which is exactly what actioning a PR comment needs.
//!   `run_feedback` also now commits (amend-aware) and pushes the target
//!   branch itself, so [`action_pr_feedback_inner`] no longer re-pushes that
//!   same ref -- it only records the sha `run_feedback` already pushed. A
//!   whole-stack PR (`branch_id: None`) is the one exception: it tracks the
//!   COMBINED branch, which `run_feedback` never touches (it only pushes the
//!   specific branch it edited), so that case still pushes separately, as
//!   before.
//! - **Bidirectional sync** (RAL-190 — [`compute_sync_status`], [`pull_pr_commits`],
//!   [`resync_pr_bases`]): drift detection between a PR's remote branch and its
//!   review worktree, pulling a reviewer's direct push to the PR branch back
//!   into the worktree (delegating to [`crate::guardian_merge::pull_pr_commits`]
//!   for the actual rebase/conflict-resolution/restack, the same pattern
//!   `action_pr_feedback` uses), and keeping a stacked PR's base in sync after
//!   its review is reordered. Every push site (`submit_pull_requests_inner`,
//!   the whole-stack branch of `action_pr_feedback_inner`, and
//!   [`crate::guardian_merge::push_feedback_branch`] for a branch-specific
//!   feedback push) is guarded against clobbering a reviewer's own commits
//!   before force-pushing.
//!
//! See `crate::forge` for the forge-auth model (RAL-117 Q8).

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex};

use opentelemetry::trace::{SpanKind, Status};
use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};

use crate::guardian::{BranchView, GuardianView};
use crate::guardian_merge::{self, git};
use crate::runner::{Runner, RunnerSpec};
use crate::server::Reply;
use crate::store::{Result, Store, StoreError, now_ms};

// ---------------------------------------------------------------------------
// Data model
// ---------------------------------------------------------------------------

/// One row of `guardian_pull_requests`: a durable, mutable mapping from a
/// guardian's worktree (one stacked branch, or the combined worktree) to the
/// PR/MR submitted for it.
#[derive(Debug, Clone, Serialize)]
pub struct PullRequestView {
    pub id: String,
    pub guardian_id: String,
    /// `None` for a PR submitted from the combined review worktree (all
    /// branches in one PR); otherwise the stacked branch's stable id
    /// (RAL-122: renamed from `branch_position`, which silently rotted
    /// across a reorder).
    pub branch_id: Option<String>,
    /// `"github"` or `"gitlab"`.
    pub forge: String,
    /// `owner/repo` (GitHub) or the encoded namespace path (GitLab).
    pub repo: String,
    /// The branch name actually pushed to the remote (the alias) — never the
    /// internal `guardian/guardian-<id>/...` name.
    pub branch_alias: String,
    /// The branch/ref this PR merges into.
    pub base_ref: String,
    pub title: String,
    pub description: String,
    /// `None` until the forge call succeeds; kept mutable so a closed-and-
    /// reopened PR's new number can be recorded via the CLI.
    pub pr_number: Option<i64>,
    pub pr_url: Option<String>,
    /// `"open"`, `"merged"`, `"closed"`, `"dropped"` — free-form, mirrors
    /// forge state except `"dropped"` (RAL-302: this row was soft-deleted,
    /// e.g. by [`Store::drop_pull_request`]).
    pub state: String,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
    /// The commit sha last pushed to `branch_alias` (RAL-190), used as the
    /// baseline for [`compute_sync_status`]'s drift detection. `None` for a
    /// row created before this column existed or whose push has not
    /// completed yet.
    pub last_pushed_sha: Option<String>,
    /// The base ref this daemon last confirmed the forge actually accepted
    /// for this PR (RAL-279), used as the second anti-thrash baseline in
    /// [`poll_pr_base_drift`] alongside `base_ref`. `None` until a
    /// [`resync_pr_bases`] forge PATCH has ever succeeded for this row.
    pub last_pushed_base_ref: Option<String>,
    /// Groups every PR row created by the same "submit a stack" call
    /// (RAL-302), so a past submission's sibling branches are queryable as
    /// one unit. `None` for a row created before this column existed.
    pub stack_id: Option<String>,
    /// Why this row was soft-deleted (RAL-302), e.g. the RAL-300 out-of-band
    /// merge message. `None` unless `state == "dropped"`.
    pub dropped_reason: Option<String>,
    /// The id of the PR row that superseded this one (RAL-338): set on a
    /// fork-internal PR once reconcile-first promotion closes it and files a
    /// fresh cross-repository PR against the parent in its place. `None` for
    /// every row that was never promoted, including the current replacement
    /// itself. Kept rather than deleted so the closed PR's discussion stays
    /// visible in [`PrStackView`] history (this ticket's Q3.3).
    pub superseded_by: Option<String>,
}

/// One past "submit a stack" call for a review (RAL-302): every PR row that
/// call created, grouped by [`PullRequestView::stack_id`], in any state
/// (open/merged/closed/dropped) -- the read-only history behind the board's
/// "view past PR stacks" screen.
#[derive(Debug, Clone, Serialize)]
pub struct PrStackView {
    /// The grouping key. For a row predating the `stack_id` column, this
    /// falls back to that row's own `id`, so it still surfaces as a
    /// (single-PR) stack rather than being silently dropped from history.
    pub stack_id: String,
    /// Earliest `created_at_ms` among this stack's PRs.
    pub submitted_at_ms: i64,
    /// Oldest first, mirrors [`Store::list_pull_requests_for_guardian`].
    pub prs: Vec<PullRequestView>,
}

/// Groups `prs` (already ordered oldest-first, as returned by
/// [`Store::list_pull_requests_for_guardian`]) into [`PrStackView`]s by
/// `stack_id`, most-recently-submitted stack first.
pub fn group_into_stacks(prs: Vec<PullRequestView>) -> Vec<PrStackView> {
    let mut order: Vec<String> = Vec::new();
    let mut by_stack: HashMap<String, Vec<PullRequestView>> = HashMap::new();
    for pr in prs {
        let key = pr.stack_id.clone().unwrap_or_else(|| pr.id.clone());
        if !by_stack.contains_key(&key) {
            order.push(key.clone());
        }
        by_stack.entry(key).or_default().push(pr);
    }
    let mut stacks: Vec<PrStackView> = order
        .into_iter()
        .map(|stack_id| {
            let prs = by_stack.remove(&stack_id).unwrap_or_default();
            let submitted_at_ms = prs.iter().map(|p| p.created_at_ms).min().unwrap_or(0);
            PrStackView {
                stack_id,
                submitted_at_ms,
                prs,
            }
        })
        .collect();
    stacks.sort_by_key(|s| std::cmp::Reverse(s.submitted_at_ms));
    stacks
}

/// One row of [`Store::list_pull_requests_index`] (RAL-362): a flat PR view
/// annotated with the squad/task/cell it was submitted from, for the board's
/// Tasks tab. Deliberately a narrower field set than [`PullRequestView`] —
/// no `title`/`description`/`base_ref`/etc, since the Tasks tab row only ever
/// needs enough to render a PR badge and link back to its source task.
#[derive(Debug, Clone, Serialize)]
pub struct PrIndexRow {
    pub id: String,
    pub guardian_id: String,
    pub branch_id: Option<String>,
    /// The branch name actually pushed to the remote, joined in from
    /// `guardian_branches` (this row's own table has no branch name).
    pub branch_alias: Option<String>,
    pub forge: String,
    pub repo: String,
    pub pr_number: Option<i64>,
    pub pr_url: Option<String>,
    pub state: String,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
    /// Squad/task/cell this PR's branch was most recently submitted for,
    /// resolved via `cells.review_branch` (see [`Store::list_pull_requests_index`]
    /// doc comment) — `None` for a PR whose source cell has since been
    /// deleted, or a manually-added branch with no source cell at all.
    pub source_squad_id: Option<String>,
    pub source_task_idx: Option<i64>,
    pub source_cell_idx: Option<i64>,
}

struct PrRow {
    id: String,
    guardian_id: String,
    branch_id: Option<String>,
    forge: String,
    repo: String,
    branch_alias: String,
    base_ref: String,
    title: String,
    description: String,
    pr_number: Option<i64>,
    pr_url: Option<String>,
    state: String,
    created_at_ms: i64,
    updated_at_ms: i64,
    last_pushed_sha: Option<String>,
    last_pushed_base_ref: Option<String>,
    stack_id: Option<String>,
    dropped_reason: Option<String>,
    superseded_by: Option<String>,
}

impl From<PrRow> for PullRequestView {
    fn from(r: PrRow) -> Self {
        Self {
            id: r.id,
            guardian_id: r.guardian_id,
            branch_id: r.branch_id,
            forge: r.forge,
            repo: r.repo,
            branch_alias: r.branch_alias,
            base_ref: r.base_ref,
            title: r.title,
            description: r.description,
            pr_number: r.pr_number,
            pr_url: r.pr_url,
            state: r.state,
            created_at_ms: r.created_at_ms,
            updated_at_ms: r.updated_at_ms,
            last_pushed_sha: r.last_pushed_sha,
            last_pushed_base_ref: r.last_pushed_base_ref,
            stack_id: r.stack_id,
            dropped_reason: r.dropped_reason,
            superseded_by: r.superseded_by,
        }
    }
}

const PR_COLUMNS: &str = "id, guardian_id, branch_id, forge, repo, branch_alias, base_ref, title, description, pr_number, pr_url, state, created_at_ms, updated_at_ms, last_pushed_sha, last_pushed_base_ref, stack_id, dropped_reason, superseded_by";

fn map_pr_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<PrRow> {
    Ok(PrRow {
        id: r.get(0)?,
        guardian_id: r.get(1)?,
        branch_id: r.get(2)?,
        forge: r.get(3)?,
        repo: r.get(4)?,
        branch_alias: r.get(5)?,
        base_ref: r.get(6)?,
        title: r.get(7)?,
        description: r.get(8)?,
        pr_number: r.get(9)?,
        pr_url: r.get(10)?,
        state: r.get(11)?,
        created_at_ms: r.get(12)?,
        updated_at_ms: r.get(13)?,
        last_pushed_sha: r.get(14)?,
        last_pushed_base_ref: r.get(15)?,
        stack_id: r.get(16)?,
        dropped_reason: r.get(17)?,
        superseded_by: r.get(18)?,
    })
}

impl Store {
    /// Record a newly-submitted PR/MR; returns its row id.
    #[allow(clippy::too_many_arguments)]
    pub fn create_pull_request(
        &self,
        guardian_id: &str,
        branch_id: Option<&str>,
        forge: &str,
        repo: &str,
        branch_alias: &str,
        base_ref: &str,
        title: &str,
        description: &str,
        pr_number: Option<i64>,
        pr_url: Option<&str>,
    ) -> Result<String> {
        self.create_pull_request_ex(
            guardian_id,
            branch_id,
            forge,
            repo,
            branch_alias,
            base_ref,
            title,
            description,
            pr_number,
            pr_url,
            None,
        )
    }

    /// Full form of [`Self::create_pull_request`] that also stamps `stack_id`
    /// (RAL-302): the same value passed for every PR row created by one
    /// "submit a stack" call, so those sibling rows are queryable as a single
    /// past submission later, even if some of them are since dropped
    /// (see [`Self::drop_pull_request`]).
    #[allow(clippy::too_many_arguments)]
    pub fn create_pull_request_ex(
        &self,
        guardian_id: &str,
        branch_id: Option<&str>,
        forge: &str,
        repo: &str,
        branch_alias: &str,
        base_ref: &str,
        title: &str,
        description: &str,
        pr_number: Option<i64>,
        pr_url: Option<&str>,
        stack_id: Option<&str>,
    ) -> Result<String> {
        let id = self.next_id("guardian_pr_seq", "pr")?;
        let now = now_ms();
        self.conn.execute(
            "INSERT INTO guardian_pull_requests(
                id, guardian_id, branch_id, forge, repo, branch_alias, base_ref,
                title, description, pr_number, pr_url, state, created_at_ms, updated_at_ms,
                stack_id
             ) VALUES(?,?,?,?,?,?,?,?,?,?,?,'open',?,?,?)",
            params![
                id,
                guardian_id,
                branch_id,
                forge,
                repo,
                branch_alias,
                base_ref,
                title,
                description,
                pr_number,
                pr_url,
                now,
                now,
                stack_id,
            ],
        )?;
        Ok(id)
    }

    /// Fetch one PR row by its ralphus-internal id (worktree → PR direction).
    pub fn get_pull_request(&self, id: &str) -> Result<PullRequestView> {
        self.conn
            .query_row(
                &format!("SELECT {PR_COLUMNS} FROM guardian_pull_requests WHERE id=?"),
                params![id],
                map_pr_row,
            )
            .optional()?
            .map(PullRequestView::from)
            .ok_or(StoreError::NotFound)
    }

    /// All PRs submitted for a guardian, oldest first.
    pub fn list_pull_requests_for_guardian(
        &self,
        guardian_id: &str,
    ) -> Result<Vec<PullRequestView>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {PR_COLUMNS} FROM guardian_pull_requests WHERE guardian_id=? ORDER BY created_at_ms, id"
        ))?;
        let rows = stmt
            .query_map(params![guardian_id], map_pr_row)?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows.into_iter().map(PullRequestView::from).collect())
    }

    /// Ids of every guardian with at least one open, forge-numbered PR
    /// (RAL-279) — the poll target list for [`poll_pr_base_drift`], so it
    /// never makes forge calls for a review that was never submitted as a
    /// PR stack.
    pub fn guardian_ids_with_open_pull_requests(&self) -> Result<Vec<String>> {
        let mut stmt = self.conn.prepare(
            "SELECT DISTINCT guardian_id FROM guardian_pull_requests
             WHERE state='open' AND pr_number IS NOT NULL",
        )?;
        let rows = stmt
            .query_map([], |r| r.get::<_, String>(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Find the ralphus PR row for a given forge PR/MR number (PR → worktree
    /// direction), if one has been recorded.
    pub fn find_pull_request_by_number(
        &self,
        forge: &str,
        repo: &str,
        pr_number: i64,
    ) -> Result<Option<PullRequestView>> {
        Ok(self
            .conn
            .query_row(
                &format!(
                    "SELECT {PR_COLUMNS} FROM guardian_pull_requests WHERE forge=? AND repo=? AND pr_number=?"
                ),
                params![forge, repo, pr_number],
                map_pr_row,
            )
            .optional()?
            .map(PullRequestView::from))
    }

    /// RAL-362: flat, single-query index of every PR row, for the board's
    /// Tasks tab. Unlike [`Self::get_pull_request`]/[`Self::find_pull_request_by_number`]
    /// (which look up one specific PR), this returns every row across every
    /// guardian in one shot, each annotated with the squad/task/cell it was
    /// submitted from -- resolved the same way `guardian.rs`'s `hydrate_guardian`
    /// resolves `BranchView::source_squad_id`/`source_task_idx`/`source_cell_idx`:
    /// a correlated subquery to `cells` on `review_branch`, since that
    /// coordinate isn't stored directly on `guardian_branches`. No forge
    /// calls, no per-guardian N+1 -- one SELECT.
    pub fn list_pull_requests_index(&self) -> Result<Vec<PrIndexRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT pr.id, pr.guardian_id, pr.branch_id, gb.branch AS branch_alias,
                    pr.forge, pr.repo, pr.pr_number, pr.pr_url, pr.state,
                    pr.created_at_ms, pr.updated_at_ms,
                    s.squad_id AS source_squad_id,
                    s.task_idx AS source_task_idx,
                    s.idx AS source_cell_idx
             FROM guardian_pull_requests pr
             LEFT JOIN guardian_branches gb ON gb.id = pr.branch_id
             LEFT JOIN cells s ON s.rowid = (
                 SELECT s2.rowid FROM cells s2
                 WHERE s2.review_branch = gb.branch
                 ORDER BY s2.rowid DESC LIMIT 1
             )
             ORDER BY pr.created_at_ms, pr.id",
        )?;
        let rows = stmt
            .query_map([], |r| {
                Ok(PrIndexRow {
                    id: r.get(0)?,
                    guardian_id: r.get(1)?,
                    branch_id: r.get(2)?,
                    branch_alias: r.get(3)?,
                    forge: r.get(4)?,
                    repo: r.get(5)?,
                    pr_number: r.get(6)?,
                    pr_url: r.get(7)?,
                    state: r.get(8)?,
                    created_at_ms: r.get(9)?,
                    updated_at_ms: r.get(10)?,
                    source_squad_id: r.get(11)?,
                    source_task_idx: r.get(12)?,
                    source_cell_idx: r.get(13)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Mutate the recorded PR mapping after the fact — e.g. the CLI updating
    /// `pr_number`/`pr_url` once a PR is closed and reopened under a new
    /// number (RAL-117: this mapping is explicitly NOT write-once). Only
    /// fields passed as `Some` are changed.
    #[allow(clippy::too_many_arguments)]
    pub fn update_pull_request(
        &self,
        id: &str,
        pr_number: Option<Option<i64>>,
        pr_url: Option<Option<&str>>,
        branch_alias: Option<&str>,
        state: Option<&str>,
    ) -> Result<()> {
        self.update_pull_request_ex(id, pr_number, pr_url, branch_alias, state, None, None, None)
    }

    /// Full form of [`Self::update_pull_request`] that also allows updating
    /// `base_ref` (RAL-190: recomputed after a branch reorder),
    /// `last_pushed_sha` (RAL-190: recorded after every push to the alias, the
    /// baseline [`compute_sync_status`] drifts against), and
    /// `last_pushed_base_ref` (RAL-279: recorded after every forge base PATCH
    /// this daemon confirms succeeded, the second anti-thrash baseline
    /// [`poll_pr_base_drift`] drifts against). Only fields passed as `Some`
    /// are changed; `last_pushed_sha`/`last_pushed_base_ref` follow the same
    /// `Option<Option<..>>` "clear vs leave alone" convention as `pr_number`.
    #[allow(clippy::too_many_arguments)]
    pub fn update_pull_request_ex(
        &self,
        id: &str,
        pr_number: Option<Option<i64>>,
        pr_url: Option<Option<&str>>,
        branch_alias: Option<&str>,
        state: Option<&str>,
        base_ref: Option<&str>,
        last_pushed_sha: Option<Option<&str>>,
        last_pushed_base_ref: Option<Option<&str>>,
    ) -> Result<()> {
        let existing = self.get_pull_request(id)?;
        let new_pr_number = pr_number.unwrap_or(existing.pr_number);
        let new_pr_url = pr_url
            .map(|o| o.map(str::to_string))
            .unwrap_or(existing.pr_url);
        let new_alias = branch_alias.unwrap_or(&existing.branch_alias);
        let new_state = state.unwrap_or(&existing.state);
        let new_base_ref = base_ref.unwrap_or(&existing.base_ref);
        let new_last_pushed_sha = last_pushed_sha
            .map(|o| o.map(str::to_string))
            .unwrap_or(existing.last_pushed_sha);
        let new_last_pushed_base_ref = last_pushed_base_ref
            .map(|o| o.map(str::to_string))
            .unwrap_or(existing.last_pushed_base_ref);
        let n = self.conn.execute(
            "UPDATE guardian_pull_requests
             SET pr_number=?, pr_url=?, branch_alias=?, state=?, base_ref=?, last_pushed_sha=?, last_pushed_base_ref=?, updated_at_ms=?
             WHERE id=?",
            params![
                new_pr_number,
                new_pr_url,
                new_alias,
                new_state,
                new_base_ref,
                new_last_pushed_sha,
                new_last_pushed_base_ref,
                now_ms(),
                id
            ],
        )?;
        if n == 0 {
            Err(StoreError::NotFound)
        } else {
            Ok(())
        }
    }

    /// Soft-delete a recorded PR mapping (RAL-300, soft-deleted since
    /// RAL-302): used when a linked PR merges out-of-band while its review is
    /// mid-flight (not `in_review`) -- the PR is no longer this review's to
    /// track, and leaving it counted as `"open"` would keep it in a future
    /// "are all linked PRs merged" check for whatever review picks this
    /// branch up next. Sets `state='dropped'` and records `reason` rather
    /// than deleting the row outright, so a "view past PR stacks" screen can
    /// still show it happened.
    pub fn drop_pull_request(&self, id: &str, reason: &str) -> Result<()> {
        let n = self.conn.execute(
            "UPDATE guardian_pull_requests SET state='dropped', dropped_reason=?, updated_at_ms=? WHERE id=?",
            params![reason, now_ms(), id],
        )?;
        if n == 0 {
            Err(StoreError::NotFound)
        } else {
            Ok(())
        }
    }

    /// RAL-317: bulk soft-drop every currently open PR row for `guardian_id`
    /// (used by `review pr unlink`) -- the guardian-wide undo for "start this
    /// review's PR stack over". Silently a no-op if there's nothing open,
    /// matching the bulk-friendly precedent set by
    /// [`crate::guardian::Store::set_branch_enabled_by_name`] rather than
    /// [`Self::drop_pull_request`]'s single-row `NotFound` on no match.
    /// Dropped rows remain visible as history via `PrStackView`/
    /// `group_into_stacks` -- this never hard-deletes (mirrors
    /// [`Self::drop_pull_request`]).
    pub fn bulk_drop_open_pull_requests(&self, guardian_id: &str, reason: &str) -> Result<usize> {
        let n = self.conn.execute(
            "UPDATE guardian_pull_requests SET state='dropped', dropped_reason=?, updated_at_ms=? WHERE guardian_id=? AND state='open'",
            params![reason, now_ms(), guardian_id],
        )?;
        Ok(n)
    }

    /// Pick a `branch_alias` guaranteed not to collide with any other PR
    /// row already recorded for this `(forge, repo)`, appending a numeric
    /// suffix (`-002`, `-003`, ...) as needed (RAL-190). `exclude` is the
    /// `(guardian_id, branch_id)` this alias is being chosen *for*, so a
    /// resubmission of the same branch is allowed to keep reusing its own
    /// prior alias rather than being suffixed away from itself.
    ///
    /// Runs entirely under the caller's existing `Store` lock (no network
    /// call), so two concurrent submissions racing to reserve the same
    /// desired alias cannot both win — the loser recomputes against the
    /// row the winner just inserted the next time this is called.
    pub fn resolve_unique_pr_alias(
        &self,
        forge: &str,
        repo: &str,
        exclude: Option<(&str, &str)>,
        desired: &str,
    ) -> Result<String> {
        let mut stmt = self.conn.prepare(
            "SELECT branch_alias, guardian_id, branch_id FROM guardian_pull_requests
             WHERE forge=? AND repo=?",
        )?;
        let taken: std::collections::HashSet<String> = stmt
            .query_map(params![forge, repo], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, Option<String>>(2)?,
                ))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?
            .into_iter()
            .filter(|(_, gid, bid)| exclude != Some((gid.as_str(), bid.as_deref().unwrap_or(""))))
            .map(|(alias, _, _)| alias)
            .collect();
        if !taken.contains(desired) {
            return Ok(desired.to_string());
        }
        for n in 2..1000 {
            let candidate = format!("{desired}-{n:03}");
            if !taken.contains(&candidate) {
                return Ok(candidate);
            }
        }
        Err(StoreError::InvalidTransition(format!(
            "could not find a free branch alias for '{desired}' after 999 attempts"
        )))
    }

    /// Idempotently record that `external_comment_id` on `pr_id` has been
    /// actioned into the owning worktree, so a later feedback-pull only picks
    /// up genuinely new comments.
    pub fn mark_pr_comment_actioned(&self, pr_id: &str, external_comment_id: &str) -> Result<()> {
        self.conn.execute(
            "INSERT OR IGNORE INTO guardian_pr_feedback_actioned(id, pr_id, external_comment_id, actioned_at_ms)
             VALUES(?,?,?,?)",
            params![
                format!("{pr_id}:{external_comment_id}"),
                pr_id,
                external_comment_id,
                now_ms()
            ],
        )?;
        Ok(())
    }

    /// External comment ids already actioned for a PR.
    pub fn actioned_pr_comment_ids(
        &self,
        pr_id: &str,
    ) -> Result<std::collections::HashSet<String>> {
        let mut stmt = self.conn.prepare(
            "SELECT external_comment_id FROM guardian_pr_feedback_actioned WHERE pr_id=?",
        )?;
        let ids = stmt
            .query_map(params![pr_id], |r| r.get::<_, String>(0))?
            .collect::<std::result::Result<std::collections::HashSet<_>, _>>()?;
        Ok(ids)
    }

    /// The GitHub-native PR stack number registered for this guardian's
    /// chain of stacked PRs (see [`submit_stack_for_guardian`]), if one has
    /// been created yet. `None` for a GitLab review (no equivalent concept)
    /// or a GitHub one that hasn't submitted 2+ PRs yet.
    pub fn get_guardian_forge_stack_number(&self, guardian_id: &str) -> Result<Option<i64>> {
        let value: Option<Option<i64>> = self
            .conn
            .query_row(
                "SELECT forge_stack_number FROM guardians WHERE id=?",
                params![guardian_id],
                |r| r.get(0),
            )
            .optional()?;
        Ok(value.flatten())
    }

    /// Record the GitHub-native PR stack number just created for this
    /// guardian, so a later "submit stack" call appends to it instead of
    /// creating a second stack.
    pub fn set_guardian_forge_stack_number(&self, guardian_id: &str, number: i64) -> Result<()> {
        let n = self.conn.execute(
            "UPDATE guardians SET forge_stack_number=? WHERE id=?",
            params![number, guardian_id],
        )?;
        if n == 0 {
            Err(StoreError::NotFound)
        } else {
            Ok(())
        }
    }

    /// RAL-317: reset this guardian's registered GitHub-native PR stack
    /// number back to unset (used by `review pr unlink`), so the next auto
    /// or manual submission creates a fresh stack instead of trying to
    /// append to one whose PRs were just unlinked. The counterpart to
    /// [`Self::set_guardian_forge_stack_number`], which has no way to clear
    /// -- it only ever overwrites with a new number.
    pub fn clear_guardian_forge_stack_number(&self, guardian_id: &str) -> Result<()> {
        let n = self.conn.execute(
            "UPDATE guardians SET forge_stack_number=NULL WHERE id=?",
            params![guardian_id],
        )?;
        if n == 0 {
            Err(StoreError::NotFound)
        } else {
            Ok(())
        }
    }

    /// Record that `id` was closed and replaced by `superseded_by` (RAL-338:
    /// reconcile-first promotion, once a fork-internal PR's branch becomes
    /// the stack's new cross-repository root). The old row's own `state` is
    /// updated separately (to `"closed"`, via [`Self::update_pull_request_ex`])
    /// -- this only stamps the pointer, so [`PrStackView`] history can follow
    /// it to the replacement.
    pub fn set_pr_superseded_by(&self, id: &str, superseded_by: &str) -> Result<()> {
        let n = self.conn.execute(
            "UPDATE guardian_pull_requests SET superseded_by=?, updated_at_ms=? WHERE id=?",
            params![superseded_by, now_ms(), id],
        )?;
        if n == 0 {
            Err(StoreError::NotFound)
        } else {
            Ok(())
        }
    }
}

// ---------------------------------------------------------------------------
// Orchestration
// ---------------------------------------------------------------------------

/// One PR/MR to submit: either one specific stacked branch
/// (`branch_id = Some(id)`) or the whole stack (`branch_id = None`, see
/// [`submit_stack_for_guardian`]) -- a PR for every enabled branch that
/// doesn't already have an open one, each based on the branch below it
/// (never a squashed all-branches-in-one PR). `branch_alias`/`title`/
/// `description` only apply to a single stacked-branch request -- they don't
/// make sense across N PRs at once, so they're ignored on a whole-stack
/// request. Omitted `branch_alias` defaults to the feature branch's own
/// name, never the internal `guardian/guardian-<id>/...` ref. Omitted
/// `title`/`description` are synthesised from the branch's commits (see
/// [`synthesize_pr_text`]).
#[derive(Debug, Clone, Deserialize)]
pub struct PrRequest {
    #[serde(default)]
    pub branch_id: Option<String>,
    #[serde(default)]
    pub branch_alias: Option<String>,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    /// RAL-307: use the worktree/feature branch name (`branch.branch`)
    /// verbatim as the PR branch alias, bypassing the
    /// `apply_pr_branch_convention` default -- ignored when `branch_alias`
    /// is also set (an explicit alias always wins). `None` defers to the
    /// review's own [`crate::guardian::GuardianView::effective_match_pr_branch_name`]
    /// setting; `Some(_)` overrides it for this submission only, without
    /// changing the review's persisted default.
    #[serde(default)]
    pub use_worktree_branch_name: Option<bool>,
}

/// The PR branch alias `submit_stacked_branch_pr` pushes to, before
/// [`resolve_unique_pr_alias`]'s collision-suffixing: an explicit
/// `branch_alias` always wins verbatim; otherwise the default is templated
/// from the convention rather than reusing `branch_name` bare -- an
/// identically-named remote branch would mask the fact that its content is
/// the review's (possibly rebased/conflict-resolved/squashed) output, not the
/// task branch's own commits. RAL-307: that masking risk is an explicit,
/// opt-in tradeoff when `use_worktree_branch_name` (this submission's own
/// override) or `effective_match_pr_branch_name` (the review's persisted
/// setting, used when the override is `None`) asks for `branch_name`
/// verbatim instead -- makes it easy to trace a PR back to the review/
/// worktree that produced it, at the cost of that masking.
fn resolve_pr_alias(
    branch_alias: Option<&str>,
    use_worktree_branch_name: Option<bool>,
    effective_match_pr_branch_name: bool,
    pr_branch_convention: &str,
    branch_name: &str,
) -> String {
    let use_worktree_branch_name =
        use_worktree_branch_name.unwrap_or(effective_match_pr_branch_name);
    sanitize_branch_name(&match branch_alias {
        Some(alias) => alias.to_string(),
        None if use_worktree_branch_name => branch_name.to_string(),
        None => apply_pr_branch_convention(pr_branch_convention, branch_name),
    })
}

/// Applies a `.ralphus.toml` `[forge] pull_request_branch_convention`
/// template (RAL-244) by substituting every `{name}` placeholder with the
/// source branch's own name -- literal substring replacement, no other
/// placeholders are supported.
fn apply_pr_branch_convention(convention: &str, name: &str) -> String {
    convention.replace("{name}", name)
}

fn sanitize_branch_name(name: &str) -> String {
    let s: String = name
        .trim()
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || matches!(c, '-' | '_' | '/' | '.') {
                c
            } else {
                '-'
            }
        })
        .collect();
    if s.is_empty() {
        "review".to_string()
    } else {
        s
    }
}

/// Strip a `<remote>/` prefix from a base-branch string like `origin/main`, so
/// the forge PR base is a plain branch name (`main`).
fn strip_remote_prefix(branch: &str, remote: &str) -> String {
    branch
        .strip_prefix(&format!("{remote}/"))
        .unwrap_or(branch)
        .to_string()
}

fn qualify_forge_base(current_base: &str, remote: &str, forge_base: &str) -> String {
    if current_base.starts_with(&format!("{remote}/")) {
        format!("{remote}/{forge_base}")
    } else {
        forge_base.to_string()
    }
}

fn push_ref(
    root: &Path,
    remote: &str,
    local_ref: &str,
    alias: &str,
) -> std::result::Result<(), String> {
    git(
        root,
        &[
            "push",
            remote,
            "--force",
            &format!("{local_ref}:refs/heads/{alias}"),
        ],
    )
    .map(|_| ())
}

/// Refuse to force-push over commits the remote `alias` branch has that
/// `local_ref` does not (RAL-190) — e.g. a reviewer pushed a fix directly to
/// the open PR branch. A remote branch that doesn't exist yet, or one whose
/// tip is already an ancestor of `local_ref` (so the force-push is a strict
/// superset), is safe and returns `Ok(())`.
///
/// `last_pushed` is the PR row's `last_pushed_sha` (`None` before the first
/// push). When the remote tip still equals it, every commit on the remote got
/// there through this daemon, so overwriting them replaces our own history and
/// is safe no matter how far the two have diverged — which is the case after
/// any restack, since replaying a branch rewrites every SHA on it and leaves
/// the remote tip un-ancestored. Ancestry is therefore checked only as a
/// fallback, backed in turn by a patch-id comparison for a remote whose commits
/// were all replayed into `local_ref` under new SHAs.
fn guard_against_clobber(
    root: &Path,
    remote: &str,
    alias: &str,
    local_ref: &str,
    last_pushed: Option<&str>,
) -> std::result::Result<(), String> {
    if git(root, &["fetch", remote, alias]).is_err() {
        // No remote branch yet (or it's unreachable) -- nothing to clobber.
        return Ok(());
    }
    let Ok(remote_sha) = git(root, &["rev-parse", "FETCH_HEAD"]) else {
        return Ok(());
    };
    let remote_sha = remote_sha.trim();
    if last_pushed == Some(remote_sha) {
        return Ok(());
    }
    if git(
        root,
        &["merge-base", "--is-ancestor", remote_sha, local_ref],
    )
    .is_ok()
    {
        return Ok(());
    }
    // `git cherry <upstream> <head>` marks each commit on `head` with `-` when
    // `upstream` already holds a patch-equivalent commit and `+` when it does
    // not, so a `+`-free result means the remote carries no work `local_ref`
    // lacks -- only the pre-restack spelling of work it already has.
    if let Ok(cherry) = git(root, &["cherry", local_ref, remote_sha]) {
        if !cherry.lines().any(|l| l.starts_with('+')) {
            return Ok(());
        }
    }
    Err(format!(
        "PR branch '{alias}' has commits not present in the review worktree \
         (a reviewer likely pushed directly to it) -- pull PR commits into the \
         worktree first instead of overwriting them"
    ))
}

/// Commit subject lines a PR for `position` should be described by: the
/// branch's own commits, from its immediate predecessor's tip (or `base_sha`
/// for the lowest enabled position).
fn commit_log_for(root: &Path, base_sha: &str, guardian: &GuardianView, position: i64) -> String {
    let prev = guardian
        .branches
        .iter()
        .filter(|b| b.position < position && b.enabled)
        .max_by_key(|b| b.position)
        .and_then(|b| b.review_branch.clone())
        .unwrap_or_else(|| base_sha.to_string());
    let Some(tip) = guardian
        .branches
        .iter()
        .find(|b| b.position == position)
        .and_then(|b| b.review_branch.clone())
    else {
        return String::new();
    };
    git(root, &["log", "--format=%s", &format!("{prev}..{tip}")]).unwrap_or_default()
}

#[derive(Deserialize)]
struct SuggestedPr {
    title: String,
    #[serde(default)]
    description: String,
}

/// Parse the resolver agent's `{"title": ..., "description": ...}` response,
/// tolerating a chatty model that wraps the object in prose (same `{...}`-
/// substring fallback used by `guardian_merge::parse_manual_commands_response`).
fn parse_suggested_pr(text: &str) -> Option<(String, String)> {
    serde_json::from_str::<SuggestedPr>(text)
        .ok()
        .or_else(|| {
            let start = text.find('{')?;
            let end = text.rfind('}')?;
            (end > start)
                .then(|| serde_json::from_str::<SuggestedPr>(&text[start..=end]).ok())
                .flatten()
        })
        .map(|s| (s.title, s.description))
}

/// Synthesize a suggested PR title + description from a branch's (or the
/// whole stack's) commit subject lines, using the same `Runner`/`RunnerSpec`
/// mechanism `guardian_merge::generate_final_summary` uses for the change
/// summary.
/// `template`, if given (see `ForgeClient::fetch_pr_template`), is folded into
/// the prompt so the repo's PR template is honoured. Falls back to a plain
/// branch-name title and the guardian's existing change summary when the log
/// is empty or the agent call fails — never returns an error. `trace_context`
/// (the owning `pr.submit_pull_requests`/`pr.action_feedback` span, if any) is
/// forwarded onto the `RunnerSpec` so `runner.subprocess`'s span (RAL-96)
/// becomes a child of it instead of starting a disconnected trace.
fn synthesize_pr_text(
    runner: &dyn Runner,
    guardian: &GuardianView,
    position: i64,
    template: Option<&str>,
    trace_context: Option<&str>,
) -> (String, String) {
    let root = PathBuf::from(&guardian.git_root);
    let fallback_title = guardian
        .branches
        .iter()
        .find(|b| b.position == position)
        .map(|b| b.branch.clone())
        .unwrap_or_else(|| guardian.name.clone());
    let fallback_description = guardian.change_summary.clone().unwrap_or_default();

    let base_sha = match git(&root, &["rev-parse", &guardian.base_branch]) {
        Ok(s) => s.trim().to_string(),
        Err(_) => return (fallback_title, fallback_description),
    };
    let log = commit_log_for(&root, &base_sha, guardian, position);
    if log.trim().is_empty() {
        // ralphus[ignore-rlog-pair]: this low-level helper has no Store; its Store-owning caller records the structured workflow outcome
        crate::rlog!(
            DEBUG,
            "ralphus [pr] synthesize pr text position={position:?} skipped: empty commit log"
        );
        return (fallback_title, fallback_description);
    }

    let agent = guardian_merge::resolver_agent(guardian.resolver_agent.as_deref(), &root);
    let model = guardian_merge::resolver_model(guardian.resolver_model.as_deref(), &agent, &root);
    let template_note = template.map_or_else(String::new, |t| {
        format!(
            "\n\nThe target repository has a pull-request template you MUST \
             follow -- fill it in, keeping its section headers intact:\n\n{t}"
        )
    });
    let prompt = format!(
        "Suggest a pull request title and description for a code change. Here \
         are the commit subject lines (one per commit, oldest first):\n\n{log}\n\n\
         Respond with ONLY a JSON object of the form \
         {{\"title\": \"...\", \"description\": \"...\"}}. The title must be a \
         single concise line under 72 characters. The description should be a \
         few sentences of markdown explaining what changed and why, focused on \
         developer intent rather than file-level detail.{template_note}"
    );
    let spec = RunnerSpec {
        squad_id: "guardian".to_string(),
        task: "pr-description".to_string(),
        cell_id: "pr-writer".to_string(),
        cwd: guardian.git_root.clone(),
        prompt: Some(prompt),
        command: None,
        agent,
        executable: None,
        model,
        system_prompt: None,
        system_prompt_position: None,
        timeout_sec: None,
        budget_tokens: None,
        maximum_budget_usd: None,
        maximum_context: None,
        auto_compact_threshold: None,
        maximum_tool_output_tokens: None,
        proof: false,
        trace_context: trace_context.map(str::to_string),
        resume_agent_session_id: None,
        assigned_agent_session_id: None,
        env_overrides: std::collections::BTreeMap::new(),
        machine: None,
        tool_arg_truncate_chars: None,
        thrash_max_compactions: None,
        thrash_min_turn_gap: None,
        allow_personal_settings: false,
        allow_personal_memory: false,
    };
    // ralphus[ignore-rlog-pair]: this low-level helper has no Store; its Store-owning caller records the structured workflow outcome
    crate::rlog!(
        DEBUG,
        "ralphus [pr] synthesize pr text position={position:?} agent={:?} model={:?}",
        spec.agent,
        spec.model
    );
    let result = runner.run(&spec);
    if result.is_done() {
        if let Some((title, description)) = parse_suggested_pr(&result.summary) {
            if !title.trim().is_empty() {
                // ralphus[ignore-rlog-pair]: this low-level helper has no Store; its Store-owning caller records the structured workflow outcome
                crate::rlog!(
                    DEBUG,
                    "ralphus [pr] synthesize pr text position={position:?} done: used llm suggestion"
                );
                return (title, description);
            }
        }
    }
    // ralphus[ignore-rlog-pair]: this low-level helper has no Store; its Store-owning caller records the structured workflow outcome
    crate::rlog!(
        WARNING,
        "ralphus [pr] synthesize pr text position={position:?} falling back to plain title/summary \
         (agent result status={:?})",
        result.status
    );
    (fallback_title, fallback_description)
}

fn resolve_title_description(
    runner: &dyn Runner,
    guardian: &GuardianView,
    req: &PrRequest,
    position: i64,
    client: &crate::forge::ForgeClient,
    trace_context: Option<&str>,
) -> (String, String) {
    if let (Some(t), Some(d)) = (&req.title, &req.description) {
        return (t.clone(), d.clone());
    }
    let template = client.fetch_pr_template();
    let (synth_title, synth_description) = synthesize_pr_text(
        runner,
        guardian,
        position,
        template.as_deref(),
        trace_context,
    );
    (
        req.title.clone().unwrap_or(synth_title),
        req.description.clone().unwrap_or(synth_description),
    )
}

/// Currently open PR alias for each stacked branch that has one, keyed by
/// branch id -- when a branch has more than one historical PR row (e.g. a
/// resubmission), the most recently created still-open one wins. A small
/// projection of [`open_prs_by_branch`] onto just the alias, for callers that
/// don't need the full row (`stack_base_for`'s callers).
fn open_alias_by_branch(prs: &[PullRequestView]) -> HashMap<String, String> {
    open_prs_by_branch(prs)
        .into_iter()
        .map(|(bid, pr)| (bid.to_string(), pr.branch_alias.clone()))
        .collect()
}

/// Groups `prs` by branch id, open PRs only, "most recently created wins" when
/// a branch has more than one historical row (e.g. a resubmission) -- shared
/// by [`resync_pr_bases`] (which needs the full row to PATCH the forge PR) and
/// [`open_alias_by_branch`] (which only needs the alias).
fn open_prs_by_branch(prs: &[PullRequestView]) -> HashMap<&str, &PullRequestView> {
    let mut by_branch: HashMap<&str, &PullRequestView> = HashMap::new();
    for pr in prs {
        if pr.state != "open" {
            continue;
        }
        if let Some(bid) = pr.branch_id.as_deref() {
            by_branch
                .entry(bid)
                .and_modify(|existing| {
                    if pr.created_at_ms > existing.created_at_ms {
                        *existing = pr;
                    }
                })
                .or_insert(pr);
        }
    }
    by_branch
}

/// Base ref a stacked branch at `position` should target (RAL-190's stacking
/// rule): the alias of the nearest-preceding enabled branch that has an open
/// PR recorded in `alias_by_branch`, skipping any enabled branch that has
/// none so a gap doesn't break the chain, or `base_branch_name` if no earlier
/// branch qualifies. `ordered_enabled_branches` must already be sorted by
/// position ascending. Shared by [`submit_pull_requests_inner`] (seeding the
/// chain from already-open PRs before submitting more) and
/// [`resync_pr_bases`] (recomputing bases after a reorder) -- the two
/// previously had independent copies of this walk, which is exactly how a
/// branch submitted in its own request (no sibling requests in the same call
/// to chain off of) ended up basing on the guardian's own base branch instead
/// of the preceding branch's alias.
fn stack_base_for(
    ordered_enabled_branches: &[&BranchView],
    alias_by_branch: &HashMap<String, String>,
    position: i64,
    base_branch_name: &str,
) -> String {
    ordered_enabled_branches
        .iter()
        .filter(|b| b.position < position)
        .rev()
        .find_map(|b| alias_by_branch.get(&b.id).cloned())
        .unwrap_or_else(|| base_branch_name.to_string())
}

/// Recompute every stacked PR's `base_ref` to match the guardian's *current*
/// branch order (RAL-190) — called after a reorder, since a stacked PR's base
/// must always be the alias of the nearest-preceding enabled branch that has
/// its own open PR, or the guardian's own base branch if none precedes it.
/// A branch with no PR of its own is skipped without breaking the chain: the
/// next branch after it still bases off the nearest earlier branch that DOES
/// have one (its review content already carries the skipped branch's diff,
/// same as the initial submission ordering in [`submit_pull_requests_inner`]).
/// Combined-worktree PRs (`branch_id = None`) always target the guardian's
/// base branch already and are left untouched.
///
/// Updates the local `base_ref` record for every PR whose base changed, and
/// best-effort PATCHes the forge PR's base too — a forge call failing for one
/// PR is logged and does not stop the others from being resynced. Returns the
/// number of PRs whose `base_ref` changed locally.
pub fn resync_pr_bases(store: &Arc<Mutex<Store>>, id: &str) -> std::result::Result<usize, String> {
    resync_pr_bases_inner(store, id, false)
}

/// Fork-aware candidate client/remote resolution for guardian-scoped PR
/// maintenance polling (RAL-338): merge checks, base-drift/reorder polling,
/// resync, and feedback all need to resolve *some* PR row's client/remote
/// without any per-request acting-user context of their own, so every one of
/// them shares this single resolver rather than re-deriving fork-aware
/// routing logic that submission already baked into a PR's stored `repo`
/// column. Uses the project-wide default fork row (`user=""`) -- the PR was
/// already filed under whichever fork its own submission resolved.
/// `fork_client`/`fork_remote_name` are always `None` when the project has no
/// registered fork, in which case every method below degrades to today's
/// single-client behavior.
struct PrRepoRouting {
    parent_client: Option<crate::forge::ForgeClient>,
    parent_remote_name: String,
    fork_client: Option<crate::forge::ForgeClient>,
    fork_remote_name: Option<String>,
}

impl PrRepoRouting {
    /// The client that owns `repo` (a PR row's stored filing repository):
    /// picks whichever of the parent/fork candidates has a matching
    /// `repo_label()`. With no registered fork, always the parent candidate
    /// (`repo` is ignored), matching pre-fork-mode behavior exactly.
    fn client_for(&self, repo: &str) -> Option<&crate::forge::ForgeClient> {
        if self.fork_client.is_none() {
            return self.parent_client.as_ref();
        }
        let candidates: Vec<&crate::forge::ForgeClient> = [&self.parent_client, &self.fork_client]
            .into_iter()
            .flatten()
            .collect();
        crate::forge::client_for_repo(repo, &candidates)
    }

    /// The git remote a PR row's own branch ref lives on -- NOT the same
    /// question as [`Self::client_for`]. Every alias in fork mode is pushed
    /// to and fetched from the fork *unconditionally*, including the
    /// cross-repository root's (its PR is filed at the parent, but its git
    /// ref still lives on the fork -- see this ticket's routing table). So
    /// once a fork is registered at all, this always returns the fork's
    /// remote regardless of `repo`; only with no registered fork does it
    /// fall back to the parent's, matching pre-fork-mode behavior exactly.
    /// `repo` is accepted (rather than an argument-less form) so call sites
    /// read the same at both `client_for`/`remote_for` call sites even
    /// though only one of them actually varies by it.
    fn remote_for(&self, _repo: &str) -> &str {
        match &self.fork_remote_name {
            Some(fork_remote) => fork_remote,
            None => &self.parent_remote_name,
        }
    }
}

/// Resolve [`PrRepoRouting`] for `guardian`'s project (by `guardian.git_root`
/// unless the caller has a more specific `root`/project path for a
/// multi-project branch, e.g. [`check_pr_merges`]'s per-branch resolution).
fn resolve_pr_repo_routing(
    store: &Arc<Mutex<Store>>,
    root: &Path,
    base_branch: &str,
    forge_cfg: &crate::config::ForgeConfig,
) -> PrRepoRouting {
    let project_name = store
        .lock()
        .expect("poisoned")
        .project_name_for_path(root.to_str().unwrap_or_default());
    let fork = project_name.as_deref().and_then(|p| {
        store
            .lock()
            .expect("poisoned")
            .resolve_fork(p, "")
            .ok()
            .flatten()
    });
    let Some(fork) = fork else {
        let parent_remote_name = crate::forge::resolve_remote_name(root, base_branch, forge_cfg);
        return PrRepoRouting {
            parent_client: crate::forge::resolve_remote(root, base_branch, forge_cfg).ok(),
            parent_remote_name,
            fork_client: None,
            fork_remote_name: None,
        };
    };
    let parent_remote_name = crate::forge::resolve_remote_name_excluding(
        root,
        base_branch,
        forge_cfg,
        Some(&fork.remote_name),
    );
    let parent_client = crate::forge::resolve_remote_for(root, &parent_remote_name, forge_cfg).ok();
    let fork_client = crate::forge::resolve_remote_for(root, &fork.remote_name, forge_cfg).ok();
    PrRepoRouting {
        parent_client,
        parent_remote_name,
        fork_client,
        fork_remote_name: Some(fork.remote_name),
    }
}

/// Resolve the fork remote to push a branch's review ref under, if the
/// project owning `root` has a registered fork (RAL-338) -- used by
/// [`crate::guardian_merge::push_feedback_branch`]'s only caller so a
/// feedback push always targets the fork explicitly rather than inferring it
/// through `@{upstream}`/`remote.pushDefault` (see that function's doc
/// comment and this ticket's Risks section: a stray `git push -u` can rewrite
/// `@{upstream}` and cause the fork remote to be mistaken for the parent's).
/// Ensures the local fork git remote exists/is up to date as a side effect,
/// since the only caller is about to push to it. `None` when the project has
/// no registered fork, in which case the caller's existing inference is
/// unchanged.
pub(crate) fn resolve_feedback_fork_remote(
    store: &Arc<Mutex<Store>>,
    root: &Path,
) -> Option<String> {
    let project_name = store
        .lock()
        .expect("poisoned")
        .project_name_for_path(root.to_str()?)?;
    let fork = store
        .lock()
        .expect("poisoned")
        .resolve_fork(&project_name, "")
        .ok()
        .flatten()?;
    crate::project_forks::ensure_fork_remote(root, &fork.remote_name, &fork.fork_url).ok()?;
    Some(fork.remote_name)
}

fn resync_pr_bases_inner(
    store: &Arc<Mutex<Store>>,
    id: &str,
    require_forge_success: bool,
) -> std::result::Result<usize, String> {
    let guardian = store
        .lock()
        .expect("poisoned")
        .get_guardian(id)
        .map_err(|e| e.to_string())?;
    let root = PathBuf::from(&guardian.git_root);
    let forge_cfg = crate::config::resolve_forge(&root);
    let remote_name = crate::forge::resolve_remote_name(&root, &guardian.base_branch, &forge_cfg);
    let base_branch_name = strip_remote_prefix(&guardian.base_branch, &remote_name);

    let prs = store
        .lock()
        .expect("poisoned")
        .list_pull_requests_for_guardian(id)
        .map_err(|e| e.to_string())?;
    let by_branch = open_prs_by_branch(&prs);
    if by_branch.is_empty() {
        return Ok(0);
    }
    let alias_by_branch = open_alias_by_branch(&prs);

    let routing = resolve_pr_repo_routing(store, &root, &guardian.base_branch, &forge_cfg);
    let (parent_client, fork_client) = (routing.parent_client, routing.fork_client);
    let resolved_client_err = if parent_client.is_none() && fork_client.is_none() {
        crate::forge::resolve_remote(&root, &guardian.base_branch, &forge_cfg).err()
    } else {
        None
    };
    let candidates: Vec<&crate::forge::ForgeClient> =
        [parent_client.as_ref(), fork_client.as_ref()]
            .into_iter()
            .flatten()
            .collect();
    let mut ordered_branches: Vec<_> = guardian.branches.iter().filter(|b| b.enabled).collect();
    ordered_branches.sort_by_key(|b| b.position);

    let mut changed = 0usize;
    // PRs GitHub refused to move because they belong to a registered stack,
    // collected so the stack is dissolved once and all of them retried
    // together rather than once per refusal.
    let mut blocked_by_stack: Vec<(String, i64, String)> = Vec::new();
    let mut forge_errors = Vec::new();
    for branch in &ordered_branches {
        let Some(pr) = by_branch.get(branch.id.as_str()) else {
            continue;
        };
        let new_base = stack_base_for(
            &ordered_branches,
            &alias_by_branch,
            branch.position,
            &base_branch_name,
        );
        // A synchronous user-authored base change verifies every open member
        // of the chain on the forge, including unchanged N-1 links. Besides
        // satisfying the all-or-error request contract, this repairs a prior
        // partial attempt whose local row was already updated.
        if new_base != pr.base_ref || require_forge_success {
            // ralphus[ignore-rlog-pair]: this low-level helper has no Store; its Store-owning caller records the structured workflow outcome
            crate::rlog!(
                INFO,
                "ralphus [pr] review {id} resync base pr={} old={} new={new_base}",
                pr.id,
                pr.base_ref
            );
            if !require_forge_success {
                let _ = store.lock().expect("poisoned").update_pull_request_ex(
                    &pr.id,
                    None,
                    None,
                    None,
                    None,
                    Some(&new_base),
                    None,
                    None,
                );
            }
            changed += 1;
            // RAL-338: only a registered fork makes `pr.repo` potentially
            // differ from the guardian's single resolved client (a
            // fork-mode root PR is filed against the parent, everything
            // else against the fork) -- resolve per-PR via the stored
            // `repo` only in that case. Without a fork, keep using the one
            // resolved client unconditionally, exactly as before this
            // ticket, so a non-fork project's routing stays byte-identical
            // even if `pr.repo` and a freshly re-resolved client's
            // `repo_label()` ever cosmetically disagree (e.g. an
            // unnormalized GitLab path).
            let branch_client = if fork_client.is_some() {
                crate::forge::client_for_repo(&pr.repo, &candidates)
            } else {
                parent_client.as_ref()
            };
            if let (Some(c), Some(num)) = (branch_client, pr.pr_number) {
                match c.update_pull_request_base(num, &new_base) {
                    // RAL-279: only record `last_pushed_base_ref` once the
                    // forge has actually confirmed the new base -- this is
                    // exactly the "last state both sides are known to have
                    // agreed on" baseline `poll_pr_base_drift` needs to tell
                    // a genuine forge-side retarget apart from a PATCH that
                    // silently failed here and never landed.
                    Ok(()) => {
                        let _ = store.lock().expect("poisoned").update_pull_request_ex(
                            &pr.id,
                            None,
                            None,
                            None,
                            None,
                            Some(&new_base),
                            None,
                            Some(Some(&new_base)),
                        );
                    }
                    Err(e) => {
                        if is_stack_base_restriction(&e) {
                            blocked_by_stack.push((pr.id.clone(), num, new_base.clone()));
                        } else {
                            // ralphus[ignore-rlog-pair]: per-PR retry-loop detail; the batch summary in start_resync_pr_bases records the structured workflow outcome
                            crate::rlog!(
                                WARNING,
                                "ralphus [pr] review {id} resync base forge update failed pr={}: {e}",
                                pr.id
                            );
                            forge_errors.push(format!("pr {}: {e}", pr.id));
                        }
                    }
                }
            } else if require_forge_success {
                forge_errors.push(format!(
                    "pr {} has no forge client or forge PR/MR number",
                    pr.id
                ));
            }
        }
    }
    if !blocked_by_stack.is_empty() {
        // GitHub's native stack registration is always the *fork's* own
        // repo-scoped object in fork mode (RAL-338: the root's parent PR is
        // never part of it -- see `submit_stack_for_guardian`), so repoint
        // against the fork client when one exists, the plain client
        // otherwise.
        if let Some(c) = fork_client.as_ref().or(parent_client.as_ref()) {
            let ordered_pr_numbers: Vec<i64> = ordered_branches
                .iter()
                .filter_map(|b| by_branch.get(b.id.as_str()))
                .filter(|pr| {
                    fork_client
                        .as_ref()
                        .is_none_or(|fc| pr.repo == fc.repo_label())
                })
                .filter_map(|pr| pr.pr_number)
                .collect();
            match repoint_stacked_prs(store, id, c, &ordered_pr_numbers, &blocked_by_stack) {
                Ok(()) if require_forge_success => {
                    for (pr_id, _, new_base) in &blocked_by_stack {
                        let _ = store.lock().expect("poisoned").update_pull_request_ex(
                            pr_id,
                            None,
                            None,
                            None,
                            None,
                            Some(new_base),
                            None,
                            None,
                        );
                    }
                }
                Ok(()) => {}
                Err(e) => forge_errors.push(e),
            }
        }
    }
    if require_forge_success && changed > 0 {
        if let Some(e) = resolved_client_err {
            forge_errors.push(e);
        }
        if !forge_errors.is_empty() {
            return Err(forge_errors.join("; "));
        }
    }
    Ok(changed)
}

/// Whether a forge error is GitHub refusing to move a PR's base because the PR
/// belongs to a registered stack, as opposed to any other rejection (auth, a
/// deleted branch, rate limiting) that dissolving the stack would not fix.
fn is_stack_base_restriction(err: &str) -> bool {
    err.to_ascii_lowercase().contains("part of a stack")
}

/// Repoint PRs GitHub refused to move because they belong to a registered
/// stack: dissolve the stack, retry each base change, then register the stack
/// again so GitHub's UI still shows the chain.
///
/// GitHub has no endpoint for changing a stack's base branch and rejects a
/// base change on any PR while it is stacked, so a review whose base branch
/// moves can only be followed by rebuilding the grouping. The PRs themselves
/// are never closed or recreated — they keep their numbers, their comments and
/// their CI history, and only stop being displayed as a stack for the moment
/// between the two calls.
///
/// Best-effort throughout: every failure is logged and the remaining PRs are
/// still attempted, since the local `base_ref` records have already been
/// updated and a forge that disagrees is reconciled on the next resync.
fn repoint_stacked_prs(
    store: &Arc<Mutex<Store>>,
    id: &str,
    client: &crate::forge::ForgeClient,
    ordered_pr_numbers: &[i64],
    blocked: &[(String, i64, String)],
) -> std::result::Result<(), String> {
    let recorded = store
        .lock()
        .expect("poisoned")
        .get_guardian_forge_stack_number(id)
        .ok()
        .flatten();
    let Some(stack_number) = recorded else {
        // ralphus[ignore-rlog-pair]: this low-level helper has no Store; its Store-owning caller records the structured workflow outcome
        crate::rlog!(
            WARNING,
            "ralphus [pr] review {id} base move blocked by a stack this review has no record of \
             -- unstack it on the forge, then change the base again"
        );
        return Err(
            "forge rejected the base change because the PR belongs to an unrecorded stack"
                .to_string(),
        );
    };
    if let Err(e) = client.unstack(stack_number) {
        // ralphus[ignore-rlog-pair]: this low-level helper has no Store; its Store-owning caller records the structured workflow outcome
        crate::rlog!(
            ERROR,
            "ralphus [pr] review {id} could not dissolve stack {stack_number} to move PR bases: {e}"
        );
        return Err(format!("could not dissolve stack {stack_number}: {e}"));
    }
    let mut errors = Vec::new();
    for (pr_id, number, new_base) in blocked {
        if let Err(e) = client.update_pull_request_base(*number, new_base) {
            // ralphus[ignore-rlog-pair]: this low-level helper has no Store; its Store-owning caller records the structured workflow outcome
            crate::rlog!(
                WARNING,
                "ralphus [pr] review {id} resync base forge update failed pr={pr_id} after \
                 dissolving stack {stack_number}: {e}"
            );
            errors.push(format!("pr {pr_id}: {e}"));
        }
    }
    if ordered_pr_numbers.len() < 2 {
        return if errors.is_empty() {
            Ok(())
        } else {
            Err(errors.join("; "))
        };
    }
    match client.create_stack(ordered_pr_numbers) {
        Ok(Some(created)) => {
            let _ = store
                .lock()
                .expect("poisoned")
                .set_guardian_forge_stack_number(id, created.number);
            // ralphus[ignore-rlog-pair]: this low-level helper has no Store; its Store-owning caller records the structured workflow outcome
            crate::rlog!(
                INFO,
                "ralphus [pr] review {id} re-registered stack {} after moving PR bases \
                 (was {stack_number})",
                created.number
            );
        }
        Ok(None) => {}
        Err(e) => {
            // ralphus[ignore-rlog-pair]: this low-level helper has no Store; its Store-owning caller records the structured workflow outcome
            crate::rlog!(
                ERROR,
                "ralphus [pr] review {id} moved PR bases but could not re-register the stack \
                 (was {stack_number}), leaving the PRs unstacked: {e}"
            );
            errors.push(format!("could not re-register stack {stack_number}: {e}"));
        }
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.join("; "))
    }
}

/// Guardian ids with a [`resync_pr_bases`] currently in flight (RAL-273).
///
/// A plain in-process claim set rather than the guardian-status CAS the merge
/// triggers use (`claim_guardian_merge`, `rebase_on_manual_push`,
/// `rebuild_on_base_shift`): a resync commonly runs *alongside* a real
/// merge/rebase for the same guardian (`guardian_arrange` fires both), so it
/// must not contend with those for `in_review`/`merging`. Without this guard,
/// two overlapping resyncs for the same guardian (e.g. two reorders in quick
/// succession, or a poll firing mid-resync) read stale PR state and race
/// their forge PATCH calls.
static RESYNCING: LazyLock<Mutex<HashSet<String>>> = LazyLock::new(|| Mutex::new(HashSet::new()));

/// Perform the forge base rebuild before returning to an API caller. Waits
/// for an older background resync of this same review to finish, then owns
/// the same claim so stale and fresh PATCH calls cannot overlap.
pub fn resync_pr_bases_synchronously(
    store: &Arc<Mutex<Store>>,
    id: &str,
) -> std::result::Result<usize, String> {
    for _ in 0..3_000 {
        if RESYNCING.lock().expect("poisoned").insert(id.to_string()) {
            let result = resync_pr_bases_inner(store, id, true);
            RESYNCING.lock().expect("poisoned").remove(id);
            return result;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    Err("timed out waiting for an earlier PR/MR base sync".to_string())
}

/// Kick off [`resync_pr_bases`] in the background — called after a reorder,
/// so the reorder's own HTTP response is not held up by the forge network
/// calls this makes. A no-op (logged, not queued) if a resync for this
/// guardian is already running -- see [`RESYNCING`].
pub fn start_resync_pr_bases(store: Arc<Mutex<Store>>, id: &str) {
    let sid = id.to_string();
    {
        let mut inflight = RESYNCING.lock().expect("poisoned");
        if !inflight.insert(sid.clone()) {
            crate::rlog!(
                DEBUG,
                "ralphus [pr] review {sid} resync bases skipped: already in flight"
            );
            return;
        }
    }
    std::thread::spawn(move || {
        let result = resync_pr_bases(&store, &sid);
        RESYNCING.lock().expect("poisoned").remove(&sid);
        match result {
            Ok(n) if n > 0 => {
                let guard = store.lock().expect("poisoned");
                let _ = guard.cartographer_log(crate::cartographer::CartographerEntry {
                    level: crate::logging::LogLevel::INFO,
                    source: "pr",
                    message: "pull request base(s) resynced after reorder",
                    scope: Some("guardian"),
                    squad_id: None,
                    guardian_id: Some(&sid),
                    cell_id: None,
                    task: None,
                    log_path: None,
                    payload: serde_json::json!({"count": n}),
                    admin_only: false,
                });
            }
            Ok(_) => {}
            Err(e) => {
                crate::rlog!(ERROR, "ralphus [pr] review {sid} resync bases failed: {e}");
            }
        }
    });
}

/// Poll every PR/MR linked to `id` for a live "merged" state and settle the
/// review accordingly (RAL-300).
///
/// A PR row already recorded `merged` is trusted without a further forge
/// call; every still-`open` row is re-checked. Once at least one row is
/// freshly observed merged, the *whole set* is re-read to decide what "all
/// linked PRs merged" means for this guardian right now:
///
/// - `in_review` and every linked PR merged: approve outright via
///   [`crate::guardian::Store::approve_guardian`] -- the same transition the
///   "Approve" button drives -- so a review whose stack landed on the forge
///   never sits stale waiting for a human to notice.
/// - anything else (`merging`, `merge_failed`, `merge_stopped`: a rebase or
///   feedback pass owns this review's worktrees right now, or a prior one
///   failed) and at least one PR merged: this is an *out-of-band* merge that
///   raced whatever is/was in flight. Forcing an approval here could silently
///   orphan real follow-up work, so instead the freshly-merged row(s) are
///   dropped from the review and a board notice + Cartographer entry explain
///   what happened, leaving the decision to the user (RAL-302 tracks a "view
///   past PR stacks" affordance for this case, out of scope here).
///
/// Fail-safe throughout: a guardian with no linked PRs, or whose forge client
/// can't be resolved, or whose per-PR state check errors (network, missing
/// token, forge down, ...) is left exactly as it was -- an unreachable forge
/// must never be mistaken for "confirmed merged", mirroring
/// [`refresh_open_prs`]'s "assume still open" fallback. Returns whether
/// anything changed (approved, or a PR was dropped) -- callers use this to
/// know the guardian's status may no longer be what they last read.
pub fn check_pr_merges(store: &Arc<Mutex<Store>>, id: &str) -> bool {
    let Ok(guardian) = store.lock().expect("poisoned").get_guardian(id) else {
        return false;
    };
    let prs = store
        .lock()
        .expect("poisoned")
        .list_pull_requests_for_guardian(id)
        .unwrap_or_default();
    if prs.is_empty() {
        return false;
    }
    if prs
        .iter()
        .all(|pull_request| pull_request.state == "merged")
    {
        return settle_pr_merge_states(store, id, &[]);
    }

    let mut freshly_merged = Vec::new();
    for pr in prs
        .iter()
        .filter(|pull_request| pull_request.state == "open")
    {
        let project_root = pr
            .branch_id
            .as_deref()
            .and_then(|branch_id| {
                guardian
                    .branches
                    .iter()
                    .find(|branch| branch.id == branch_id)
            })
            .and_then(|branch| branch.project.as_deref())
            .unwrap_or(&guardian.git_root);
        let root = PathBuf::from(project_root);
        let forge_cfg = crate::config::resolve_forge(&root);
        // RAL-338: a fork-mode review's PRs may be split across two
        // repositories (a cross-repo root against the parent, everything
        // else against the fork) -- resolve both candidates and pick
        // whichever matches this PR's own recorded `repo`, rather than a
        // single `resolve_remote` call that can only ever match one of them.
        let routing = resolve_pr_repo_routing(store, &root, &guardian.base_branch, &forge_cfg);
        let client = match routing.client_for(&pr.repo) {
            Some(client) if client.kind().as_str() == pr.forge => client.clone(),
            Some(client) => {
                log_pr_merge_check_failure(
                    store,
                    id,
                    pr,
                    &format!(
                        "resolved forge {}/{} does not match recorded {}/{}",
                        client.kind().as_str(),
                        client.repo_label(),
                        pr.forge,
                        pr.repo
                    ),
                );
                continue;
            }
            None => {
                log_pr_merge_check_failure(
                    store,
                    id,
                    pr,
                    &format!(
                        "could not resolve a forge client for recorded {}/{}",
                        pr.forge, pr.repo
                    ),
                );
                continue;
            }
        };
        poll_pr_merge_state(store, id, pr, &client, &mut freshly_merged);
    }
    // RAL-338: react to a freshly-observed cross-repository root merge
    // before settling merge states, so a newly-promoted successor's row
    // already exists when `settle_pr_merge_states` re-reads this guardian's
    // current PRs just below.
    maybe_promote_fork_root(store, id, &guardian, &freshly_merged);
    settle_pr_merge_states(store, id, &freshly_merged)
}

/// Reconcile-first promotion (RAL-338 Phase 5): once the stack's cross-
/// repository root PR merges, the next enabled branch's fork-internal PR
/// must become the new root -- filed against the *parent*, which a same-repo
/// base PATCH (the mechanism [`resync_pr_bases`] ordinarily uses to skip a
/// merged branch) cannot express, since that PR currently lives in a
/// different repository (the fork) entirely. Called from [`check_pr_merges`]
/// right after a merge is freshly observed.
///
/// Reconcile-first: reads the successor's live forge base/repo before
/// touching anything, and does nothing if it is already filed at the parent
/// (GitHub/GitLab auto-retargeting and merge-train behavior are unproven for
/// this cross-repository topology -- reconciling first means ralphus never
/// fights whatever the forge already did here; if live testing later proves
/// a forge handles this end-to-end, the close-and-reopen below can be
/// deleted and merge trains remain the forge's own responsibility). Promotes
/// at most one successor per call, even if multiple PRs merged since the
/// last poll -- the next call (this function is invoked on every
/// [`check_pr_merges`] poll) picks up any remaining promotion.
fn maybe_promote_fork_root(
    store: &Arc<Mutex<Store>>,
    id: &str,
    guardian: &GuardianView,
    freshly_merged: &[PullRequestView],
) {
    if freshly_merged.is_empty() {
        return;
    }
    let root = PathBuf::from(&guardian.git_root);
    let forge_cfg = crate::config::resolve_forge(&root);
    // Daemon-internal poller, no per-request acting-user context -- resolves
    // the project-wide default fork row, matching every other poller-driven
    // fork lookup (mirrors `auto_submit_terminal_branches`'s rationale).
    let routing = match resolve_fork_routing(store, &root, guardian, &forge_cfg, "") {
        Ok(Some(routing)) => routing,
        Ok(None) => return, // not a fork-mode review
        Err(e) => {
            crate::rlog!(
                ERROR,
                "ralphus [pr] review {id} could not resolve fork routing for promotion: {e}"
            );
            return;
        }
    };
    // Same computation `submit_pull_requests_inner`/`poll_pr_base_drift` use:
    // the guardian's base branch, unprefixed by whichever *parent* remote it
    // may have named (never the fork's).
    let parent_remote_name = crate::forge::resolve_remote_name_excluding(
        &root,
        &guardian.base_branch,
        &forge_cfg,
        Some(&routing.fork.remote_name),
    );
    let base_branch_name = strip_remote_prefix(&guardian.base_branch, &parent_remote_name);
    // The cross-repository root is the only PR whose base is the guardian's
    // own base branch directly -- every other branch chains onto a preceding
    // alias (see `fork_aware_route`, which both submission and this function
    // share). This is deliberately NOT a `repo`/client comparison: GitLab's
    // cross-project MR is created *on the fork* (with a `target_project_id`
    // pointing at the parent), so a GitLab root's `repo` is the fork's
    // label, not the parent's -- only GitHub's root is filed under the
    // parent's own repo label. Comparing `base_ref` instead works
    // identically for both forges. `branch_id` is required -- a
    // combined-worktree PR (`branch_id = None`) has no single "next" branch
    // of its own to promote.
    let Some(merged_root) = freshly_merged
        .iter()
        .find(|pr| pr.base_ref == base_branch_name && pr.branch_id.is_some())
    else {
        return;
    };
    let mut ordered_branches: Vec<_> = guardian.branches.iter().filter(|b| b.enabled).collect();
    ordered_branches.sort_by_key(|b| b.position);
    let Some(merged_position) = merged_root
        .branch_id
        .as_deref()
        .and_then(|bid| ordered_branches.iter().find(|b| b.id == bid))
        .map(|b| b.position)
    else {
        return;
    };
    let prs = store
        .lock()
        .expect("poisoned")
        .list_pull_requests_for_guardian(id)
        .unwrap_or_default();
    // Walk forward from the merged root's position to the first enabled
    // branch that still has an *open* PR -- not simply the literal next
    // branch by position, which may itself have also merged in this same
    // batch (multiple PRs can merge between polls). Promotes exactly that
    // one branch; any branch beyond it waits for the next call.
    let Some((successor_branch, successor_pr)) = ordered_branches
        .iter()
        .filter(|b| b.position > merged_position)
        .find_map(|b| {
            prs.iter()
                .find(|pr| pr.state == "open" && pr.branch_id.as_deref() == Some(b.id.as_str()))
                .map(|pr| (*b, pr))
        })
    else {
        return; // nothing left in the stack to promote
    };
    if successor_pr.base_ref == base_branch_name {
        // Reconcile-first: the forge (or a prior partial promotion) already
        // has this targeting the parent's base directly -- nothing to
        // reconcile. See this function's doc comment on why `base_ref`, not
        // `repo`, is the forge-agnostic "is this the root" signal.
        crate::rlog!(
            INFO,
            "ralphus [pr] review {id} successor pr={} is already filed at the parent -- \
             promotion already reconciled, nothing to do",
            successor_pr.id
        );
        return;
    }
    let Some(successor_number) = successor_pr.pr_number else {
        return;
    };
    let route = match fork_aware_route(
        &routing,
        &successor_pr.branch_alias,
        &base_branch_name,
        &base_branch_name,
    ) {
        Ok(route) => route,
        Err(e) => {
            crate::rlog!(
                ERROR,
                "ralphus [pr] review {id} cannot promote pr={}: {e}",
                successor_pr.id
            );
            return;
        }
    };
    let fork_client = &routing.fork_client;

    let created = match route.create_pull_request(&successor_pr.title, &successor_pr.description) {
        Ok(created) => created,
        Err(e) => {
            crate::rlog!(
                ERROR,
                "ralphus [pr] review {id} promotion failed for pr={}: {e}",
                successor_pr.id
            );
            return;
        }
    };
    if let Err(e) = fork_client.close_pull_request(successor_number) {
        crate::rlog!(
            WARNING,
            "ralphus [pr] review {id} promoted pr={} to {} but could not close the superseded \
             fork pr: {e}",
            successor_pr.id,
            created.url
        );
    }
    let pointer = format!(
        "Promoted: the branch below this one just merged, so this branch is now the stack's \
         root and has been re-filed directly against the parent repository: {}",
        created.url
    );
    if let Err(e) = fork_client.post_pr_comment(successor_number, &pointer) {
        crate::rlog!(
            WARNING,
            "ralphus [pr] review {id} promoted pr={} but could not post the pointer comment: {e}",
            successor_pr.id
        );
    }
    let new_id = store.lock().expect("poisoned").create_pull_request_ex(
        id,
        Some(successor_branch.id.as_str()),
        route.client.kind().as_str(),
        &route.repo,
        &successor_pr.branch_alias,
        &base_branch_name,
        &successor_pr.title,
        &successor_pr.description,
        Some(created.number),
        Some(&created.url),
        successor_pr.stack_id.as_deref(),
    );
    let _ = store.lock().expect("poisoned").update_pull_request_ex(
        &successor_pr.id,
        None,
        None,
        None,
        Some("closed"),
        None,
        None,
        None,
    );
    match &new_id {
        Ok(new_id) => {
            let _ = store
                .lock()
                .expect("poisoned")
                .set_pr_superseded_by(&successor_pr.id, new_id);
            crate::rlog!(
                INFO,
                "ralphus [pr] review {id} promoted pr={} (old) -> {new_id} (new, number={})",
                successor_pr.id,
                created.number
            );
            let guard = store.lock().expect("poisoned");
            let _ = guard.cartographer_log(crate::cartographer::CartographerEntry {
                level: crate::logging::LogLevel::INFO,
                source: "pr",
                message: "fork-internal pr promoted to the stack's new cross-repository root",
                scope: Some("guardian"),
                squad_id: None,
                guardian_id: Some(id),
                cell_id: None,
                task: None,
                log_path: None,
                payload: serde_json::json!({
                    "old_pr_id": successor_pr.id,
                    "new_pr_id": new_id,
                    "new_pr_number": created.number,
                }),
                admin_only: false,
            });
        }
        Err(e) => {
            crate::rlog!(
                ERROR,
                "ralphus [pr] review {id} promoted pr={} on the forge (new pr {}) but failed to \
                 record the new row locally: {e}",
                successor_pr.id,
                created.number
            );
        }
    }
}

/// The client-agnostic body of [`check_pr_merges`], split out so tests can
/// hand it a [`crate::forge::ForgeClient`] pointed at a mock server without
/// needing a real git remote for [`crate::forge::resolve_remote`] to resolve.
#[cfg(test)]
fn apply_pr_merge_check(
    store: &Arc<Mutex<Store>>,
    id: &str,
    prs: &[PullRequestView],
    client: &crate::forge::ForgeClient,
) -> bool {
    let mut freshly_merged: Vec<PullRequestView> = Vec::new();
    for pr in prs.iter().filter(|p| p.state == "open") {
        poll_pr_merge_state(store, id, pr, client, &mut freshly_merged);
    }

    settle_pr_merge_states(store, id, &freshly_merged)
}

fn poll_pr_merge_state(
    store: &Arc<Mutex<Store>>,
    id: &str,
    pr: &PullRequestView,
    client: &crate::forge::ForgeClient,
    freshly_merged: &mut Vec<PullRequestView>,
) {
    let Some(number) = pr.pr_number else {
        return;
    };
    match client.get_pull_request_state(number) {
        Ok(state) if state != "open" => {
            let _ = store.lock().expect("poisoned").update_pull_request_ex(
                &pr.id,
                None,
                None,
                None,
                Some(&state),
                None,
                None,
                None,
            );
            if state == "merged" {
                freshly_merged.push(pr.clone());
            }
        }
        Ok(_) => {}
        Err(error) => log_pr_merge_check_failure(store, id, pr, &error),
    }
}

fn log_pr_merge_check_failure(
    store: &Arc<Mutex<Store>>,
    id: &str,
    pr: &PullRequestView,
    error: &str,
) {
    crate::rlog!(
        WARNING,
        "ralphus [pr] review {id} pr {} merge check failed, assuming not merged: {error}",
        pr.id
    );
    let guard = store.lock().expect("poisoned");
    let _ = guard.cartographer_log(crate::cartographer::CartographerEntry {
        level: crate::logging::LogLevel::WARNING,
        source: "pr",
        message: "linked pr merge check failed; assuming not merged",
        scope: Some("guardian"),
        squad_id: None,
        guardian_id: Some(id),
        cell_id: None,
        task: None,
        log_path: None,
        payload: serde_json::json!({"pr_id": pr.id, "error": error}),
        admin_only: false,
    });
}

/// Apply the guardian transition implied by the PR states already persisted in
/// the store. This is shared by forge polling and explicit merge notifications,
/// which may record every PR as merged before the maintenance sweep runs.
fn settle_pr_merge_states(
    store: &Arc<Mutex<Store>>,
    id: &str,
    freshly_merged: &[PullRequestView],
) -> bool {
    let current_guardian = match store.lock().expect("poisoned").get_guardian(id) {
        Ok(guardian) => guardian,
        Err(_) => return false,
    };
    let current_prs = store
        .lock()
        .expect("poisoned")
        .list_pull_requests_for_guardian(id)
        .unwrap_or_default();

    if current_guardian.status.as_str() == "in_review" {
        // RAL-302: a dropped row is soft-deleted, not removed, so it stays in
        // `current_prs` forever -- exclude it here or a review that ever had
        // one dropped could never satisfy "every linked pr has merged" again.
        // RAL-338: a superseded (`superseded_by.is_some()`) row is likewise
        // permanently `"closed"`, never `"merged"` -- exclude it too, or a
        // review that was ever promoted could never satisfy "every linked pr
        // has merged" again either. Its replacement row is what actually
        // needs to merge.
        let live_prs: Vec<&PullRequestView> = current_prs
            .iter()
            .filter(|pull_request| {
                pull_request.state != "dropped" && pull_request.superseded_by.is_none()
            })
            .collect();
        let all_merged = !live_prs.is_empty()
            && live_prs
                .iter()
                .all(|pull_request| pull_request.state == "merged");
        if !all_merged {
            return false;
        }
        let approved = store.lock().expect("poisoned").approve_guardian(id).is_ok();
        if approved {
            crate::rlog!(
                INFO,
                "ralphus [pr] review {id} approved: every linked pr has merged"
            );
            let guard = store.lock().expect("poisoned");
            let _ = guard.cartographer_log(crate::cartographer::CartographerEntry {
                level: crate::logging::LogLevel::INFO,
                source: "pr",
                message: "review approved: every linked pr has merged",
                scope: Some("guardian"),
                squad_id: None,
                guardian_id: Some(id),
                cell_id: None,
                task: None,
                log_path: None,
                payload: serde_json::json!({
                    "pr_ids": live_prs.iter().map(|p| p.id.clone()).collect::<Vec<_>>(),
                }),
                admin_only: false,
            });
        }
        return approved;
    }

    if freshly_merged.is_empty() {
        return false;
    }

    // Mid-flight: not idle in `in_review`, so this review has (or recently
    // had) a rebase/feedback pass of its own in flight. Drop the stale PR
    // row(s) rather than force a status change out from under it.
    for pr in freshly_merged {
        let guard = store.lock().expect("poisoned");
        let _ = guard.drop_pull_request(
            &pr.id,
            "linked pr merged out-of-band while review was mid-flight",
        );
        let _ = guard.cartographer_log(crate::cartographer::CartographerEntry {
            level: crate::logging::LogLevel::WARNING,
            source: "pr",
            message: "linked pr merged out-of-band while review was mid-flight; dropped",
            scope: Some("guardian"),
            squad_id: None,
            guardian_id: Some(id),
            cell_id: None,
            task: None,
            log_path: None,
            payload: serde_json::json!({
                "pr_id": pr.id,
                "pr_number": pr.pr_number,
                "branch_id": pr.branch_id,
                "guardian_status": current_guardian.status,
            }),
            admin_only: false,
        });
        crate::rlog!(
            WARNING,
            "ralphus [pr] review {id} pr {} (branch_id={:?}) merged out-of-band while \
             review was {} -- dropped from the review",
            pr.id,
            pr.branch_id,
            current_guardian.status,
        );
    }
    let _ = store.lock().expect("poisoned").set_guardian_notice(
        id,
        "pr_merged_mid_flight",
        "A linked pull request merged on the forge while this review had a merge/feedback pass \
         in flight. It has been dropped from the review -- check whether any in-flight work still \
         applies, and resubmit a fresh PR if needed.",
    );
    true
}

/// Reconcile every already-open PR's remote branch with the review branch it
/// tracks, re-pushing the ones that drifted.
///
/// Restacking a review rebuilds each branch's worktree tip in place but never
/// touches the remote `-review` branches an already-open PR tracks, so without
/// this an open PR silently goes stale — GitHub keeps showing the pre-restack
/// diff, and pre-restack conflicts against a base branch that has since moved.
/// Every path that settles a review reaches the same end state rather than a
/// common notification point (`guardian_merge`'s explicit merge, restart-merge
/// and feedback runs, plus `review_maintenance`'s base-shift rebuild and
/// manual-push restack), so this compares recorded against actual instead of
/// subscribing to any one of them: it is idempotent, and cheap when nothing
/// moved, because a PR whose `last_pushed_sha` still matches its local tip is
/// skipped before any network call.
///
/// Only a settled (`in_review`) review is reconciled — a merge in flight owns
/// the branch tips and will land them itself. Mirrors the push+record-sha
/// pattern [`submit_pull_requests_inner`] and [`pull_pr_commits`] already use.
/// Best-effort per PR: one push failing (most likely `guard_against_clobber`
/// tripping because a reviewer pushed directly to the PR branch) is logged and
/// does not stop the others.
pub fn sync_open_pr_branches(store: &Arc<Mutex<Store>>, id: &str) {
    let Ok(guardian) = store.lock().expect("poisoned").get_guardian(id) else {
        return;
    };
    if guardian.status.as_str() != "in_review" {
        return;
    }
    let prs = store
        .lock()
        .expect("poisoned")
        .list_pull_requests_for_guardian(id)
        .unwrap_or_default();
    let open_prs: Vec<_> = prs.iter().filter(|p| p.state == "open").collect();
    if open_prs.is_empty() {
        return;
    }
    let root = PathBuf::from(&guardian.git_root);
    let forge_cfg = crate::config::resolve_forge(&root);
    // RAL-338: every PR's `branch_alias` lives on whichever remote its own
    // `repo` names -- the fork for every branch in fork mode, including the
    // root (its PR is filed against the parent, but its ref still lives on
    // the fork) -- so resolve per-PR rather than one remote for the whole
    // guardian.
    let routing = resolve_pr_repo_routing(store, &root, &guardian.base_branch, &forge_cfg);

    for pr in open_prs {
        let remote_name = routing.remote_for(&pr.repo);
        let local_ref = match &pr.branch_id {
            Some(bid) => guardian
                .branches
                .iter()
                .find(|b| &b.id == bid)
                .and_then(|b| b.review_branch.clone()),
            None => guardian.review_branch.clone(),
        };
        let Some(local_ref) = local_ref else {
            continue;
        };
        let Ok(local_sha) = git(&root, &["rev-parse", &local_ref]).map(|s| s.trim().to_string())
        else {
            continue;
        };
        if pr.last_pushed_sha.as_deref() == Some(local_sha.as_str()) {
            continue; // already in sync -- nothing moved this branch since
        }
        if let Err(e) = guard_against_clobber(
            &root,
            remote_name,
            &pr.branch_alias,
            &local_ref,
            pr.last_pushed_sha.as_deref(),
        ) {
            crate::rlog!(
                WARNING,
                "ralphus [pr] review {id} pr branch sync skipped pr={} alias={}: {e}",
                pr.id,
                pr.branch_alias
            );
            continue;
        }
        if let Err(e) = push_ref(&root, remote_name, &local_ref, &pr.branch_alias) {
            crate::rlog!(
                WARNING,
                "ralphus [pr] review {id} pr branch sync push failed pr={} alias={}: {e}",
                pr.id,
                pr.branch_alias
            );
            continue;
        }
        let _ = store.lock().expect("poisoned").update_pull_request_ex(
            &pr.id,
            None,
            None,
            None,
            None,
            None,
            Some(Some(local_sha.as_str())),
            None,
        );
        {
            let guard = store.lock().expect("poisoned");
            let _ = guard.cartographer_log(crate::cartographer::CartographerEntry {
                level: crate::logging::LogLevel::INFO,
                source: "pr",
                message: "pr branch re-pushed to match its review branch",
                scope: Some("guardian"),
                squad_id: None,
                guardian_id: Some(id),
                cell_id: None,
                task: None,
                log_path: None,
                payload: serde_json::json!({
                    "pr_id": pr.id,
                    "alias": pr.branch_alias,
                    "sha": local_sha,
                }),
                admin_only: false,
            });
        }
        crate::rlog!(
            INFO,
            "ralphus [pr] review {id} pr={} alias={} re-pushed sha={local_sha}",
            pr.id,
            pr.branch_alias
        );
    }
}

/// Discover the branch order implied by each stacked PR's *live* base ref on
/// the forge (RAL-273): reconstructs the chain by walking, from the
/// guardian's own base branch, whichever open PR currently bases on it, then
/// whichever bases on that PR's alias, and so on. Returns `Some(order)` (open
/// PR branch ids, forge order) only when it differs from the guardian's
/// current `position` order among that same set of branches. Returns `None`
/// when there's nothing to reorder (fewer than two stacked PRs), no drift, or
/// the live bases don't form one unbroken chain (ambiguous -- e.g. mid-edit
/// on the forge side -- safer to do nothing than guess). A branch with no
/// open PR of its own is excluded from the comparison entirely, the same way
/// [`stack_base_for`]/[`resync_pr_bases`] skip it: reordering only ever moves
/// it along with whichever branch it's implicitly attached to via
/// `Store::reorder_guardian_branches`'s "leftover" compaction of unmatched
/// branches.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForgeStackDrift {
    order: Vec<String>,
    base: String,
    base_changed_at_ms: i64,
    base_changed: bool,
    order_changed: bool,
}

pub fn detect_forge_reorder(
    store: &Arc<Mutex<Store>>,
    id: &str,
) -> std::result::Result<Option<ForgeStackDrift>, String> {
    let guardian = store
        .lock()
        .expect("poisoned")
        .get_guardian(id)
        .map_err(|e| e.to_string())?;
    let root = PathBuf::from(&guardian.git_root);
    let forge_cfg = crate::config::resolve_forge(&root);
    // RAL-338: a fork-mode review's PRs may be split across two repositories,
    // so resolve both candidate clients up front and pick per-PR via each
    // row's own stored `repo` -- see [`PrRepoRouting`].
    let routing = resolve_pr_repo_routing(store, &root, &guardian.base_branch, &forge_cfg);
    let base_branch_name = strip_remote_prefix(&guardian.base_branch, &routing.parent_remote_name);

    let prs = store
        .lock()
        .expect("poisoned")
        .list_pull_requests_for_guardian(id)
        .map_err(|e| e.to_string())?;
    let by_branch = open_prs_by_branch(&prs);
    if by_branch.is_empty() {
        return Ok(None);
    }
    let alias_by_branch = open_alias_by_branch(&prs);

    let mut live_state: HashMap<String, crate::forge::PullRequestBaseState> = HashMap::new();
    for (branch_id, pr) in &by_branch {
        let Some(num) = pr.pr_number else { continue };
        let Some(client) = routing.client_for(&pr.repo) else {
            return Err(format!(
                "could not resolve a forge client for recorded {}/{}",
                pr.forge, pr.repo
            ));
        };
        let state = client.get_pull_request_base_state(num)?;
        live_state.insert((*branch_id).to_string(), state);
    }
    let Some((forge_base, base_changed_at_ms, order)) =
        reconstruct_forge_stack(&live_state, &alias_by_branch)
    else {
        return Ok(None);
    };

    let mut ordered_branches: Vec<_> = guardian.branches.iter().filter(|b| b.enabled).collect();
    ordered_branches.sort_by_key(|b| b.position);
    let local_order: Vec<String> = ordered_branches
        .into_iter()
        .filter(|b| by_branch.contains_key(b.id.as_str()))
        .map(|b| b.id.clone())
        .collect();

    let order_changed = local_order != order;
    let base_changed = forge_base != base_branch_name;
    Ok(if !order_changed && !base_changed {
        None
    } else {
        Some(ForgeStackDrift {
            order,
            base: forge_base,
            base_changed_at_ms,
            base_changed,
            order_changed,
        })
    })
}

/// Reconstruct a complete forge stack without assuming ralphus's recorded
/// base is still its root. Exactly one PR/MR must target a ref that is not
/// another open PR's alias; that ref is the forge-authored review base.
fn reconstruct_forge_stack(
    live_state: &HashMap<String, crate::forge::PullRequestBaseState>,
    alias_by_branch: &HashMap<String, String>,
) -> Option<(String, i64, Vec<String>)> {
    let alias_refs: HashSet<&str> = alias_by_branch.values().map(String::as_str).collect();
    let roots: Vec<_> = live_state
        .values()
        .filter(|state| !alias_refs.contains(state.base.as_str()))
        .collect();
    let [root] = roots.as_slice() else {
        return None;
    };
    let live_base: HashMap<String, String> = live_state
        .iter()
        .map(|(id, state)| (id.clone(), state.base.clone()))
        .collect();
    let order = reconstruct_forge_chain(&root.base, &live_base, alias_by_branch)?;
    Some((root.base.clone(), root.updated_at_ms, order))
}

/// Walk `live_base` (branch id -> that branch's live forge base ref) from
/// `base_branch_name`, one hop at a time: at each step, find the one branch
/// whose live base matches the current ref, append it to the order, and
/// advance `current` to that branch's own alias (via `alias_by_branch`) for
/// the next hop. Pure and side-effect-free so [`detect_forge_reorder`]'s
/// reconstruction logic is unit-testable without a real forge/network.
///
/// Returns `None` when the bases don't form one unbroken chain covering
/// every entry in `live_base` -- either a dead end (no branch found for the
/// current ref) or a fork (more than one branch claims the same live base,
/// unresolvable without guessing).
fn reconstruct_forge_chain(
    base_branch_name: &str,
    live_base: &HashMap<String, String>,
    alias_by_branch: &HashMap<String, String>,
) -> Option<Vec<String>> {
    let mut order: Vec<String> = Vec::new();
    let mut current = base_branch_name.to_string();
    let mut remaining: HashSet<String> = live_base.keys().cloned().collect();
    loop {
        let matches: Vec<String> = remaining
            .iter()
            .filter(|bid| live_base.get(bid.as_str()).is_some_and(|b| *b == current))
            .cloned()
            .collect();
        match matches.len() {
            0 => break,
            1 => {
                let next = matches.into_iter().next().expect("checked len==1");
                remaining.remove(&next);
                current = alias_by_branch.get(&next).cloned().unwrap_or(current);
                order.push(next);
            }
            _ => return None,
        }
    }
    remaining.is_empty().then_some(order)
}

/// Claim a guardian for an external-reorder rebuild (RAL-273): the same
/// check-then-set-under-one-lock CAS idiom `rebase_on_manual_push`/
/// `rebuild_on_base_shift` use for their own triggers -- `in_review` is the
/// only claimable state, since a reorder only makes sense once a stack is
/// actually built and has open PRs to compare against.
fn claim_guardian_for_forge_reorder(store: &Arc<Mutex<Store>>, id: &str) -> bool {
    let guard = store.lock().expect("poisoned");
    matches!(guard.get_guardian(id), Ok(gv) if gv.status.as_str() == "in_review")
        && guard
            .set_guardian_status(
                id,
                crate::guardian::GuardianStatus::Merging,
                Some("stack reorder detected on the forge; rebuilding"),
            )
            .is_ok()
}

/// Detect and apply an external (GitHub/GitLab) stack reorder for one
/// guardian (RAL-273): [`detect_forge_reorder`], then, if the forge's order
/// differs from ralphus's, claim the review, reorder locally, resync PR
/// bases, and retrigger a full rebuild so any conflict introduced by the new
/// order surfaces through the existing rebase-run reporting path (per-branch
/// `merge_status`/conflicts, guardian `detail`, Cartographer).
///
/// A GitHub-side reorder always wins over a review already `merging` locally
/// (RAL-273's last-writer-wins rule): if the claim loses because some other
/// trigger for this same guardian (a manual reorder's own rebuild, a
/// base-shift rebuild, a manual-push restack) is in flight, that run is
/// cancelled and the claim retried for a few seconds before giving up. A
/// guardian notice is recorded only in that interrupted case, for the
/// bottom-right toast; a plain (nothing was running) reorder is applied
/// silently other than the usual Cartographer log. Returns whether a reorder
/// was detected and applied.
pub fn check_and_apply_forge_reorder(
    store: &Arc<Mutex<Store>>,
    runner: &dyn Runner,
    id: &str,
    sem: &crate::scheduler::Semaphore,
    cancellations: &crate::cancel::Cancellations,
) -> bool {
    let drift = match detect_forge_reorder(store, id) {
        Ok(Some(drift)) => drift,
        Ok(None) => return false,
        Err(e) => {
            crate::rlog!(
                WARNING,
                "ralphus [pr] review {id} forge reorder check failed: {e}"
            );
            return false;
        }
    };

    let cancel_key = format!("guardian:{id}");
    let mut claimed = claim_guardian_for_forge_reorder(store, id);
    let mut interrupted_local = false;
    if !claimed && cancellations.is_active(&cancel_key) {
        cancellations.cancel(&cancel_key);
        for _ in 0..50 {
            if claim_guardian_for_forge_reorder(store, id) {
                claimed = true;
                interrupted_local = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
    }
    if !claimed {
        crate::rlog!(
            WARNING,
            "ralphus [pr] review {id} forge reorder detected but the review could not be claimed; will retry next check"
        );
        return false;
    }

    crate::rlog!(
        INFO,
        "ralphus [pr] review {id} forge drift detected order={:?} base={} base_changed={} order_changed={} interrupted_local={interrupted_local}",
        drift.order,
        drift.base,
        drift.base_changed,
        drift.order_changed
    );
    {
        let mut guard = store.lock().expect("poisoned");
        let guardian = match guard.get_guardian(id) {
            Ok(g) => g,
            Err(e) => {
                crate::rlog!(
                    ERROR,
                    "ralphus [pr] review {id} forge drift apply failed: {e}"
                );
                return false;
            }
        };
        let root = PathBuf::from(&guardian.git_root);
        let forge_cfg = crate::config::resolve_forge(&root);
        let remote_name =
            crate::forge::resolve_remote_name(&root, &guardian.base_branch, &forge_cfg);
        let local_base = qualify_forge_base(&guardian.base_branch, &remote_name, &drift.base);
        let base_applied = if drift.base_changed {
            match guard.set_guardian_base_branch_if_newer(id, &local_base, drift.base_changed_at_ms)
            {
                Ok(applied) => applied,
                Err(e) => {
                    crate::rlog!(
                        ERROR,
                        "ralphus [pr] review {id} forge base apply failed: {e}"
                    );
                    return false;
                }
            }
        } else {
            false
        };
        if drift.order_changed && guard.reorder_guardian_branches(id, &drift.order).is_err() {
            crate::rlog!(ERROR, "ralphus [pr] review {id} forge reorder apply failed");
            let _ = guard.set_guardian_status(
                id,
                crate::guardian::GuardianStatus::InReview,
                Some("forge reorder detected but could not be applied"),
            );
            return false;
        }
        let _ = guard.cartographer_log(crate::cartographer::CartographerEntry {
            level: crate::logging::LogLevel::INFO,
            source: "pr",
            message: "stack base/order drift detected on the forge; review updated",
            scope: Some("guardian"),
            squad_id: None,
            guardian_id: Some(id),
            cell_id: None,
            task: None,
            log_path: None,
            payload: serde_json::json!({
                "order": drift.order,
                "base": drift.base,
                "base_applied": base_applied,
                "interrupted_local": interrupted_local
            }),
            admin_only: false,
        });
        if interrupted_local {
            let _ = guard.set_guardian_notice(
                id,
                "forge_drift_interrupted_local",
                "A GitHub/GitLab stack edit arrived while a local review edit was in progress; the newest base edit won.",
            );
        }
    }

    start_resync_pr_bases(Arc::clone(store), id);

    let token = cancellations.register(&cancel_key);
    let _permit = sem.acquire();
    guardian_merge::run_merge_staged(store, runner, id, &token);
    cancellations.remove(&cancel_key);
    true
}

/// Poll every guardian with an active stack (`in_review`/`merging` only, to
/// keep forge API usage trivial) for a reorder, each on its own thread
/// (RAL-273). Called periodically by the scheduler loop -- this is the
/// "every 5 minutes" background detection path; `POST .../sync-pr`
/// covers the on-demand/explicit-sync path via the same
/// [`check_and_apply_forge_reorder`]. `guardian_reorder`/`guardian_arrange`
/// deliberately do not also call it -- see the comment in `guardian_reorder`
/// (`daemon/src/server.rs`) on why that would race their own base PATCHes.
pub fn poll_forge_reorders(
    store: &Arc<Mutex<Store>>,
    sem: &Arc<crate::scheduler::Semaphore>,
    cancellations: &crate::cancel::Cancellations,
) {
    let ids: Vec<String> = {
        let guard = store.lock().expect("poisoned");
        guard
            .list_guardians()
            .unwrap_or_default()
            .into_iter()
            .filter(|g| matches!(g.status.as_str(), "in_review" | "merging"))
            .map(|g| g.id)
            .collect()
    };
    for id in ids {
        let store = Arc::clone(store);
        let sem = Arc::clone(sem);
        let cancellations = cancellations.clone();
        std::thread::spawn(move || {
            let runner: Arc<dyn Runner> = Arc::new(
                crate::runner::SubprocessRunner::from_env().with_cartographer(Arc::clone(&store)),
            );
            check_and_apply_forge_reorder(&store, runner.as_ref(), &id, &sem, &cancellations);
        });
    }
}

// ---------------------------------------------------------------------------
// Forge-to-ralphus base drift poll (RAL-279)
// ---------------------------------------------------------------------------

/// Whether a PR's live forge base, compared against its two local baselines,
/// counts as genuine forge-side drift (RAL-279) -- the pure classification
/// step of [`poll_pr_base_drift`], split out for direct unit testing the
/// same way [`stack_base_for`] is split out of [`resync_pr_bases`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BaseDriftKind {
    /// The forge already agrees with `base_ref`; nothing to do.
    InSync,
    /// The forge still matches the last base ralphus itself confirmed
    /// pushing -- `base_ref` moved ahead of it locally (most likely a
    /// resync whose forge PATCH hasn't landed yet, or failed), not a
    /// forge-side change. Left alone so it doesn't get overwritten back to
    /// a stale value.
    PendingLocalPush,
    /// The forge disagrees with both baselines -- a genuine external
    /// retarget to pull into ralphus's own branch order.
    Drifted,
}

fn classify_base_drift(
    base_ref: &str,
    last_pushed_base_ref: Option<&str>,
    forge_base: &str,
) -> BaseDriftKind {
    if forge_base == base_ref {
        BaseDriftKind::InSync
    } else if last_pushed_base_ref == Some(forge_base) {
        BaseDriftKind::PendingLocalPush
    } else {
        BaseDriftKind::Drifted
    }
}

/// Given a drifted PR's branch (`position`) and the forge's new base,
/// figures out which currently-enabled branches (per `ordered_branches`,
/// already position-sorted) sat between the new base and `position` and so
/// must have left the stack -- `Some(vec)` (possibly empty, when the forge
/// base already names the immediate predecessor branch) if `forge_base`
/// names a recognized predecessor (an enabled branch's open-PR alias, or the
/// guardian's own base branch), `None` if it names neither and the caller
/// should leave this PR alone rather than guess.
fn branches_skipped_by_drift<'a>(
    ordered_branches: &[&'a BranchView],
    alias_by_branch: &HashMap<String, String>,
    position: i64,
    base_branch_name: &str,
    forge_base: &str,
) -> Option<Vec<&'a BranchView>> {
    let new_predecessor = ordered_branches
        .iter()
        .filter(|b| b.position < position)
        .find(|b| alias_by_branch.get(b.id.as_str()).map(String::as_str) == Some(forge_base));
    if new_predecessor.is_none() && forge_base != base_branch_name {
        return None;
    }
    let lower_bound = new_predecessor.map_or(-1, |b| b.position);
    Some(
        ordered_branches
            .iter()
            .filter(|b| b.position > lower_bound && b.position < position)
            .copied()
            .collect(),
    )
}

/// Detect a stacked PR whose forge-side base no longer matches ralphus's own
/// bookkeeping -- a reviewer retargeted it directly on GitHub/GitLab, or an
/// intervening branch's PR was closed/merged there -- and pull that change
/// back into this guardian's branch order (the mirror image of
/// [`resync_pr_bases`], which pushes ralphus's own reorders out to the
/// forge).
///
/// Anti-thrash: a PR's `base_ref` is only treated as genuinely forge-drifted
/// if the live forge base differs from BOTH `base_ref` (what ralphus already
/// has recorded) AND `last_pushed_base_ref` (the last base ralphus itself
/// confirmed the forge accepted) -- mirroring `last_pushed_sha`'s role in
/// [`compute_sync_status`]. This keeps a forge PATCH that hasn't landed yet
/// (or failed) from being misread as a forge-side change, and keeps an
/// accepted forge-side change from being re-flagged as drift on the next
/// poll once `base_ref` catches up to it.
///
/// For each drifted PR, disables every currently-enabled branch that sat
/// (per ralphus's current position order) between the forge's new base and
/// this PR's own branch -- forge dropping them from the base chain means
/// they left the stack. A forge base that names neither a known branch alias
/// nor the guardian's own base branch is logged and skipped rather than
/// guessed at. Any branches disabled this way trigger a follow-up
/// [`resync_pr_bases`] so every other stacked PR's base is recomputed (and
/// pushed to the forge) against the new order. Returns the number of PRs
/// pulled into ralphus's order.
pub fn poll_pr_base_drift(
    store: &Arc<Mutex<Store>>,
    id: &str,
) -> std::result::Result<usize, String> {
    let guardian = store
        .lock()
        .expect("poisoned")
        .get_guardian(id)
        .map_err(|e| e.to_string())?;
    let root = PathBuf::from(&guardian.git_root);
    let forge_cfg = crate::config::resolve_forge(&root);
    // RAL-338: resolve both candidate clients so each PR's base-drift check
    // uses whichever repository it's actually filed on.
    let routing = resolve_pr_repo_routing(store, &root, &guardian.base_branch, &forge_cfg);
    let base_branch_name = strip_remote_prefix(&guardian.base_branch, &routing.parent_remote_name);

    let prs = store
        .lock()
        .expect("poisoned")
        .list_pull_requests_for_guardian(id)
        .map_err(|e| e.to_string())?;
    let by_branch = open_prs_by_branch(&prs);
    if by_branch.is_empty() {
        return Ok(0);
    }
    let alias_by_branch = open_alias_by_branch(&prs);

    if routing.parent_client.is_none() && routing.fork_client.is_none() {
        return Ok(0);
    }

    let mut ordered_branches: Vec<_> = guardian.branches.iter().filter(|b| b.enabled).collect();
    ordered_branches.sort_by_key(|b| b.position);

    let mut pulled = 0usize;
    for branch in &ordered_branches {
        let Some(pr) = by_branch.get(branch.id.as_str()) else {
            continue;
        };
        let Some(number) = pr.pr_number else {
            continue;
        };
        let Some(client) = routing.client_for(&pr.repo) else {
            continue;
        };
        let Ok(forge_base) = client.get_pull_request_base(number) else {
            continue;
        };
        match classify_base_drift(
            &pr.base_ref,
            pr.last_pushed_base_ref.as_deref(),
            &forge_base,
        ) {
            BaseDriftKind::InSync | BaseDriftKind::PendingLocalPush => continue,
            BaseDriftKind::Drifted => {}
        }

        let Some(skipped_branches) = branches_skipped_by_drift(
            &ordered_branches,
            &alias_by_branch,
            branch.position,
            &base_branch_name,
            &forge_base,
        ) else {
            // ralphus[ignore-rlog-pair]: per-PR drift-detection detail; the batch summary in poll_pr_base_drift_once records the structured workflow outcome
            crate::rlog!(
                WARNING,
                "ralphus [pr] review {id} pr={} forge base '{forge_base}' matches neither a known \
                 branch alias nor the review's base branch -- skipping auto-resync",
                pr.id
            );
            continue;
        };
        for skipped in skipped_branches {
            // ralphus[ignore-rlog-pair]: per-PR drift-detection detail; the batch summary in poll_pr_base_drift_once records the structured workflow outcome
            crate::rlog!(
                INFO,
                "ralphus [pr] review {id} branch {} disabled -- forge-side base retarget on pr={} \
                 (new base '{forge_base}') dropped it from the stack",
                skipped.id,
                pr.id
            );
            let _ = store.lock().expect("poisoned").set_branch_enabled_by_name(
                id,
                &skipped.branch,
                false,
            );
        }

        // ralphus[ignore-rlog-pair]: per-PR drift-detection detail; the batch summary in poll_pr_base_drift_once records the structured workflow outcome
        crate::rlog!(
            INFO,
            "ralphus [pr] review {id} pr={} base drifted on the forge: old={} new={forge_base}",
            pr.id,
            pr.base_ref
        );
        let _ = store.lock().expect("poisoned").update_pull_request_ex(
            &pr.id,
            None,
            None,
            None,
            None,
            Some(&forge_base),
            None,
            Some(Some(&forge_base)),
        );
        pulled += 1;
    }

    if pulled > 0 {
        // Cascade: other stacked PRs downstream of the ones just pulled in
        // may now need their own base recomputed (and pushed to the forge)
        // against the updated branch order.
        let _ = resync_pr_bases(store, id);
    }
    Ok(pulled)
}

/// Poll every guardian with an open PR stack for forge-side base drift
/// (RAL-279) once, logging a Cartographer entry per guardian where anything
/// changed. Never touches a review with no submitted PRs (RAL-279's "no
/// forge calls for a review that was never submitted" requirement) since
/// [`Store::guardian_ids_with_open_pull_requests`] only returns guardians
/// that already have one.
fn poll_pr_base_drift_once(store: &Arc<Mutex<Store>>) {
    let ids = match store
        .lock()
        .expect("poisoned")
        .guardian_ids_with_open_pull_requests()
    {
        Ok(ids) => ids,
        Err(e) => {
            crate::rlog!(
                ERROR,
                "ralphus [pr] base drift poll: listing guardians failed: {e}"
            );
            return;
        }
    };
    for id in ids {
        match poll_pr_base_drift(store, &id) {
            Ok(n) if n > 0 => {
                let guard = store.lock().expect("poisoned");
                let _ = guard.cartographer_log(crate::cartographer::CartographerEntry {
                    level: crate::logging::LogLevel::INFO,
                    source: "pr",
                    message: "pull request base drift pulled in from the forge",
                    scope: Some("guardian"),
                    squad_id: None,
                    guardian_id: Some(&id),
                    cell_id: None,
                    task: None,
                    log_path: None,
                    payload: serde_json::json!({"count": n}),
                    admin_only: false,
                });
            }
            Ok(_) => {}
            Err(e) => {
                crate::rlog!(
                    ERROR,
                    "ralphus [pr] review {id} base drift poll failed: {e}"
                );
            }
        }
    }
}

/// Interval between forge-side base drift polls (RAL-279) -- infrequent
/// since it's a best-effort reconciliation against manual forge activity,
/// not something latency-sensitive.
const PR_BASE_DRIFT_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_secs(300);

/// Spawn the background loop that periodically calls
/// [`poll_pr_base_drift_once`] for as long as the daemon runs (RAL-279).
pub fn spawn_pr_base_drift_poller(store: Arc<Mutex<Store>>) {
    std::thread::spawn(move || {
        loop {
            std::thread::sleep(PR_BASE_DRIFT_POLL_INTERVAL);
            poll_pr_base_drift_once(&store);
        }
    });
}

// ---------------------------------------------------------------------------
// RAL-338: fork-based stacked PR routing
// ---------------------------------------------------------------------------

/// Resolved fork-mode routing for one guardian's submission: `None` (see
/// [`resolve_fork_routing`]) means the guardian's project has no registered
/// fork, in which case every existing (non-fork) code path is untouched.
struct ForkRouting {
    fork: crate::project_forks::ForkRecord,
    parent_client: crate::forge::ForgeClient,
    fork_client: crate::forge::ForgeClient,
    /// GitLab only: the parent's numeric project id, resolved once per
    /// submission (not per branch) and reused as every root MR's
    /// `target_project_id`.
    parent_project_id: Option<i64>,
}

/// Resolve fork-mode routing for `guardian`'s submission as `user` (RAL-338):
/// finds the registered project owning `guardian.git_root` (if any), then
/// that project's fork row for `user` (falling back to the project-wide
/// default, per [`Store::resolve_fork`]). Ensures the local fork git remote
/// exists/is up to date as a side effect, since every caller is about to
/// push to it. `Ok(None)` covers both "not a registered project" and "no
/// fork registered" -- both mean "byte-identical non-fork routing" to the
/// caller.
fn resolve_fork_routing(
    store: &Arc<Mutex<Store>>,
    root: &Path,
    guardian: &GuardianView,
    forge_cfg: &crate::config::ForgeConfig,
    user: &str,
) -> std::result::Result<Option<ForkRouting>, String> {
    let project_name = store
        .lock()
        .expect("poisoned")
        .project_name_for_path(&guardian.git_root);
    let Some(project_name) = project_name else {
        return Ok(None);
    };
    let fork = store
        .lock()
        .expect("poisoned")
        .resolve_fork(&project_name, user)
        .map_err(|e| e.to_string())?;
    let Some(fork) = fork else {
        return Ok(None);
    };
    crate::project_forks::ensure_fork_remote(root, &fork.remote_name, &fork.fork_url).map_err(
        |e| {
            format!(
                "could not configure fork remote {:?}: {e}",
                fork.remote_name
            )
        },
    )?;
    let parent_remote_name = crate::forge::resolve_remote_name_excluding(
        root,
        &guardian.base_branch,
        forge_cfg,
        Some(&fork.remote_name),
    );
    let parent_client = crate::forge::resolve_remote_for(root, &parent_remote_name, forge_cfg)?;
    let fork_client = crate::forge::resolve_remote_for(root, &fork.remote_name, forge_cfg)?;
    let parent_project_id = if fork_client.kind() == crate::forge::ForgeKind::GitLab {
        Some(
            parent_client
                .resolve_gitlab_project_id()
                .map_err(|e| format!("could not resolve parent GitLab project id: {e}"))?,
        )
    } else {
        None
    };
    Ok(Some(ForkRouting {
        fork,
        parent_client,
        fork_client,
        parent_project_id,
    }))
}

/// Which git remote a push for this guardian's submission should target
/// (RAL-338): the resolved fork's own remote in fork mode, or `default` (the
/// already-resolved non-fork remote) otherwise. Every alias -- including the
/// root's -- pushes to the fork.
fn push_remote_for<'a>(routing: Option<&'a ForkRouting>, default: &'a str) -> &'a str {
    routing.map_or(default, |r| r.fork.remote_name.as_str())
}

/// The fork-aware route for one branch's PR (RAL-338): `computed_base` is
/// [`stack_base_for`]'s already-computed result for this branch, and
/// `is_root` (derived here, not passed in) is whether that equals the
/// guardian's own unprefixed base branch -- i.e. no preceding enabled branch
/// has an open PR yet, so this is the lowest enabled unmerged branch. Both
/// submission ([`submit_stacked_branch_pr`]) and resync
/// (`resolve_pr_repo_routing`) key off the same underlying data so filing
/// repository and base can never disagree.
///
/// GitLab always calls the fork client (setting `target_project_id` only for
/// the root); GitHub calls the *parent's* client for the cross-repo root
/// (requiring a registered `fork_owner`) and the fork's client otherwise.
fn fork_aware_route(
    routing: &ForkRouting,
    alias: &str,
    computed_base: &str,
    base_branch_name: &str,
) -> std::result::Result<crate::forge::PrRoute, String> {
    let is_root = computed_base == base_branch_name;
    match routing.fork_client.kind() {
        crate::forge::ForgeKind::GitLab => Ok(crate::forge::PrRoute {
            client: routing.fork_client.clone(),
            head: alias.to_string(),
            base: computed_base.to_string(),
            target_project_id: if is_root {
                routing.parent_project_id
            } else {
                None
            },
            repo: routing.fork_client.repo_label().to_string(),
        }),
        crate::forge::ForgeKind::GitHub if is_root => {
            if routing.fork.fork_owner.trim().is_empty() {
                return Err(format!(
                    "fork {:?} has no registered fork_owner; set --owner when registering the \
                     fork so ralphus can build the cross-repo PR's \"owner:branch\" head",
                    routing.fork.fork_url
                ));
            }
            Ok(crate::forge::PrRoute {
                client: routing.parent_client.clone(),
                head: format!("{}:{alias}", routing.fork.fork_owner),
                base: computed_base.to_string(),
                target_project_id: None,
                repo: routing.parent_client.repo_label().to_string(),
            })
        }
        crate::forge::ForgeKind::GitHub => Ok(crate::forge::PrRoute {
            client: routing.fork_client.clone(),
            head: alias.to_string(),
            base: computed_base.to_string(),
            target_project_id: None,
            repo: routing.fork_client.repo_label().to_string(),
        }),
    }
}

/// Outcome of the fork-relationship pre-flight (RAL-338), run once per submit
/// before any ref is pushed. See [`crate::forge::ForkRelationship`].
enum ForkPreflight {
    Ok,
    Warn(String),
    UnlinkedOverridden(String),
    Blocked(String),
}

/// Pre-flight fork-relationship check for one submission (RAL-338): compares
/// the fork and parent's forge-side fork networks and classifies the result.
/// A definite [`crate::forge::ForkRelationship::NoRelationship`] blocks the
/// whole submission unless `allow_unlinked` downgrades it to a loud warning;
/// every other outcome proceeds (an indirect relationship or an unreadable
/// project is not proof of anything, and a cross-instance pair is reported
/// as its own distinct failure).
fn preflight_fork_relationship(routing: &ForkRouting, allow_unlinked: bool) -> ForkPreflight {
    if routing.parent_client.kind() == routing.fork_client.kind()
        && routing.parent_client.repo_label() == routing.fork_client.repo_label()
    {
        // Guard parent/fork identity first (RAL-338 Phase 3): a fork
        // registered at the same repo as the parent is trivially fine.
        return ForkPreflight::Ok;
    }
    let fork_lookup = routing.fork_client.lookup_fork_network();
    let parent_lookup = routing.parent_client.lookup_fork_network();
    let relationship = crate::forge::classify_fork_relationship(
        routing.fork_client.kind(),
        routing.parent_client.kind(),
        &fork_lookup,
        &parent_lookup,
    );
    match relationship {
        crate::forge::ForkRelationship::SameNetwork => ForkPreflight::Ok,
        crate::forge::ForkRelationship::SameNetworkIndirect => ForkPreflight::Warn(format!(
            "fork {:?} is only indirectly related to its parent (a fork of a fork, or a \
             sibling) -- proceeding",
            routing.fork.fork_url
        )),
        crate::forge::ForkRelationship::NotVisible => ForkPreflight::Warn(format!(
            "could not confirm the fork relationship for {:?} (the fork or parent project was \
             not visible over the API, which is not proof of no relationship) -- proceeding",
            routing.fork.fork_url
        )),
        crate::forge::ForkRelationship::CrossInstance => ForkPreflight::Blocked(format!(
            "the fork {:?} and its parent are on different forge instances/kinds; there is no \
             cross-repository PR path between them",
            routing.fork.fork_url
        )),
        crate::forge::ForkRelationship::NoRelationship if allow_unlinked => {
            ForkPreflight::UnlinkedOverridden(format!(
                "--allow-unlinked-fork override: submitting despite no confirmed forge \
                 relationship between {:?} and its parent",
                routing.fork.fork_url
            ))
        }
        crate::forge::ForkRelationship::NoRelationship => ForkPreflight::Blocked(format!(
            "fork {:?} does not appear to be a forge-registered fork of the parent repository -- \
             create it through the forge's own \"Fork\" action first, or (if it already is one \
             and this is a false negative) ask a forge admin to link it retroactively; pass \
             --allow-unlinked-fork to override this check",
            routing.fork.fork_url
        )),
    }
}

/// Run [`preflight_fork_relationship`] and translate the outcome into logging
/// plus a hard error where applicable -- shared by every submission entry
/// point so the pre-flight always runs before the first push.
fn run_fork_preflight(
    store: &Arc<Mutex<Store>>,
    id: &str,
    routing: &ForkRouting,
    allow_unlinked_fork: bool,
) -> std::result::Result<(), String> {
    match preflight_fork_relationship(routing, allow_unlinked_fork) {
        ForkPreflight::Ok => Ok(()),
        ForkPreflight::Warn(msg) => {
            crate::rlog!(WARNING, "ralphus [pr] review {id} {msg}");
            Ok(())
        }
        ForkPreflight::UnlinkedOverridden(msg) => {
            crate::rlog!(WARNING, "ralphus [pr] review {id} {msg}");
            let _ = store.lock().expect("poisoned").cartographer_log(
                crate::cartographer::CartographerEntry {
                    level: crate::logging::LogLevel::WARNING,
                    source: "pr",
                    message: "fork submission proceeded without a confirmed relationship",
                    scope: Some("guardian"),
                    squad_id: None,
                    guardian_id: Some(id),
                    cell_id: None,
                    task: None,
                    log_path: None,
                    payload: serde_json::json!({"fork_url": routing.fork.fork_url}),
                    admin_only: false,
                },
            );
            Ok(())
        }
        ForkPreflight::Blocked(msg) => Err(format!("review {id}: {msg}")),
    }
}

/// Submit each request in `requests` as a PR/MR. Stacked requests
/// (`branch_id = Some`) are pushed and opened lowest-position-first
/// (RAL-117 Q4: "start from the branch closest to upstream, proceed
/// outward") so each one's PR base is the previous one's already-pushed
/// alias; combined-worktree requests (`None`) always target the guardian's
/// own base branch. Returns the created rows in submission order, or the
/// first error encountered (earlier PRs in the same call remain created —
/// callers can inspect `list_pull_requests_for_guardian` to see what
/// succeeded before a partial failure).
///
/// `user` (RAL-338) resolves which fork (if any) this submission routes
/// through -- the acting identity from the request context, or
/// `[daemon].default_user` for pollers/daemon-internal callers, per
/// `crate::server::current_user`'s resolution order. `allow_unlinked_fork`
/// downgrades a definite "no forge relationship" pre-flight result from a
/// hard error to a logged warning.
///
/// Runs the whole operation under one `pr.submit_pull_requests` OpenTelemetry
/// span (RAL-96, `SpanKind::Internal` — this is background daemon work, not
/// itself a client/server boundary), started fresh since there is no squad row
/// to have persisted an incoming request's trace context onto (unlike
/// `POST /api/squads`, see `server::route_with_trace`). That span's context is
/// forwarded into the PR-text-synthesis `RunnerSpec` so its
/// `runner.subprocess` span becomes a child of this one instead of an
/// unlinked trace.
#[allow(clippy::too_many_arguments)]
pub fn submit_pull_requests(
    store: &Arc<Mutex<Store>>,
    runner: &dyn Runner,
    id: &str,
    requests: Vec<PrRequest>,
    user: &str,
    allow_unlinked_fork: bool,
) -> std::result::Result<Vec<PullRequestView>, String> {
    let cx = crate::otel::context_from_traceparent(None);
    let span = crate::otel::start_span("pr.submit_pull_requests", &cx, SpanKind::Internal);
    span.set_attribute("guardian_id", id.to_string());
    span.set_attribute("pr.request_count", requests.len() as i64);
    let trace_context = crate::otel::traceparent_from_context(&span.cx);

    crate::rlog!(
        INFO,
        "ralphus [pr] review {id} submitting {} pr request(s)",
        requests.len()
    );
    let result = submit_pull_requests_inner(
        store,
        runner,
        id,
        requests,
        trace_context.as_deref(),
        user,
        allow_unlinked_fork,
    );
    match &result {
        Ok(created) => {
            span.set_status(Status::Ok);
            crate::rlog!(
                INFO,
                "ralphus [pr] review {id} submitted {} pr(s)",
                created.len()
            );
        }
        Err(e) => span.set_status(Status::error(e.clone())),
    }
    result
}

/// Push one branch's review ref and open its PR: resolves a collision-free
/// alias, computes its base via [`stack_base_for`] against `alias_by_branch`,
/// force-pushes, creates the PR, records the row, and folds the newly-created
/// alias into `alias_by_branch` so the next branch in the same batch chains
/// onto it correctly. Shared by the explicit per-branch request path and the
/// "submit the whole stack" combined-request path in
/// [`submit_pull_requests_inner`]/[`submit_stack_for_guardian`].
///
/// `client`/`remote_name` are the caller's already-resolved routing target:
/// in fork mode (`fork_routing: Some`) the caller passes the *fork's* client
/// and remote (every alias, including the root's, pushes to and is scoped
/// unique against the fork -- RAL-338), so this function's push/uniqueness
/// logic needs no fork awareness of its own. Only the actual PR-creation
/// call differs: [`fork_aware_route`] decides whether that goes to the fork
/// or (GitHub cross-repo root) the parent.
#[allow(clippy::too_many_arguments)]
fn submit_stacked_branch_pr(
    store: &Arc<Mutex<Store>>,
    runner: &dyn Runner,
    client: &crate::forge::ForgeClient,
    id: &str,
    root: &Path,
    remote_name: &str,
    guardian: &GuardianView,
    ordered_enabled_branches: &[&BranchView],
    alias_by_branch: &mut HashMap<String, String>,
    base_branch_name: &str,
    branch: &BranchView,
    req: &PrRequest,
    pr_branch_convention: &str,
    trace_context: Option<&str>,
    stack_id: &str,
    fork_routing: Option<&ForkRouting>,
) -> std::result::Result<PullRequestView, String> {
    let branch_id = branch.id.as_str();
    let position = branch.position;
    let review_ref = branch
        .review_branch
        .clone()
        .ok_or_else(|| format!("branch {branch_id} has no review ref yet; run the merge first"))?;
    let desired_alias = resolve_pr_alias(
        req.branch_alias.as_deref(),
        req.use_worktree_branch_name,
        guardian.effective_match_pr_branch_name,
        pr_branch_convention,
        &branch.branch,
    );
    // RAL-190: suffix (`-002`, ...) if another PR already claims this
    // alias, so two branches/reviews that would otherwise default to the
    // same remote branch name don't collide. RAL-338: `client` here is
    // always the fork in fork mode, so uniqueness is scoped to the fork's
    // physical refs even for the root branch, whose PR is filed elsewhere.
    let alias = store
        .lock()
        .expect("poisoned")
        .resolve_unique_pr_alias(
            client.kind().as_str(),
            client.repo_label(),
            Some((id, branch_id)),
            &desired_alias,
        )
        .map_err(|e| e.to_string())?;
    crate::rlog!(
        DEBUG,
        "ralphus [pr] review {id} pushing branch id={branch_id} alias={alias} remote={remote_name}"
    );
    // Nothing has been pushed to a brand-new alias yet, so there is no
    // recorded tip to recognize the remote by.
    guard_against_clobber(root, remote_name, &alias, &review_ref, None)?;
    push_ref(root, remote_name, &review_ref, &alias)?;
    let pushed_sha = git(root, &["rev-parse", &review_ref])
        .map(|s| s.trim().to_string())
        .ok();
    let base = stack_base_for(
        ordered_enabled_branches,
        alias_by_branch,
        position,
        base_branch_name,
    );
    let route = match fork_routing {
        Some(routing) => fork_aware_route(routing, &alias, &base, base_branch_name)?,
        None => crate::forge::PrRoute {
            client: client.clone(),
            head: alias.clone(),
            base: base.clone(),
            target_project_id: None,
            repo: client.repo_label().to_string(),
        },
    };
    let (title, description) = resolve_title_description(
        runner,
        guardian,
        req,
        position,
        &route.client,
        trace_context,
    );
    let created_pr = route.create_pull_request(&title, &description)?;
    let row_id = store
        .lock()
        .expect("poisoned")
        .create_pull_request_ex(
            id,
            Some(branch_id),
            route.client.kind().as_str(),
            &route.repo,
            &alias,
            &base,
            &title,
            &description,
            Some(created_pr.number),
            Some(&created_pr.url),
            Some(stack_id),
        )
        .map_err(|e| e.to_string())?;
    if let Some(sha) = &pushed_sha {
        let _ = store.lock().expect("poisoned").update_pull_request_ex(
            &row_id,
            None,
            None,
            None,
            None,
            None,
            Some(Some(sha.as_str())),
            None,
        );
    }
    {
        let guard = store.lock().expect("poisoned");
        let _ = guard.cartographer_log(crate::cartographer::CartographerEntry {
            level: crate::logging::LogLevel::INFO,
            source: "pr",
            message: "pull request created",
            scope: Some("branch"),
            squad_id: None,
            guardian_id: Some(id),
            cell_id: None,
            task: None,
            log_path: None,
            payload: serde_json::json!({
                "branch_id": branch_id,
                "position": position,
                "alias": alias,
                "pr_number": created_pr.number,
                "pr_url": created_pr.url,
            }),
            admin_only: false,
        });
    }
    // So a later branch in the same batch (or the whole-stack loop) chains
    // its own base onto this one instead of falling back to the guardian's
    // base branch -- this is the fix for the cross-call chaining bug: the
    // map is seeded from *all* already-open PRs up front, not just the ones
    // submitted in this call.
    alias_by_branch.insert(branch_id.to_string(), alias);
    store
        .lock()
        .expect("poisoned")
        .get_pull_request(&row_id)
        .map_err(|e| e.to_string())
}

/// How (if at all) to register/grow the guardian's GitHub-native PR stack
/// after a submission (best-effort, mirrors `resync_pr_bases`'s PATCH calls).
/// A pure decision -- no I/O -- so it's unit-testable without git or network.
#[derive(Debug, Clone, PartialEq, Eq)]
enum StackAction {
    /// No stack recorded yet -- create one from every currently-open PR,
    /// bottom to top.
    Create { all_ordered: Vec<i64> },
    /// A stack is already recorded -- append the newly created PRs (in
    /// position order) on top of it.
    Append {
        stack_number: i64,
        new_ordered: Vec<i64>,
    },
    /// Nothing to do this call.
    Skip { reason: &'static str },
}

/// Decide the [`StackAction`] for one submission. `all_branches_with_prs` is
/// every enabled branch that now has an open PR (existing + just-created),
/// as `(branch_id, position, pr_number)`, in any order.
/// `newly_created_branch_ids` are the branch ids submitted *this* call.
/// `recorded_stack_number` is the guardian's previously-registered stack, if
/// any.
fn decide_stack_action(
    all_branches_with_prs: &[(String, i64, i64)],
    newly_created_branch_ids: &std::collections::HashSet<String>,
    recorded_stack_number: Option<i64>,
) -> StackAction {
    if newly_created_branch_ids.is_empty() {
        return StackAction::Skip {
            reason: "no new PRs to register",
        };
    }
    match recorded_stack_number {
        None => {
            if all_branches_with_prs.len() < 2 {
                return StackAction::Skip {
                    reason: "fewer than 2 PRs in the stack",
                };
            }
            let mut ordered: Vec<(i64, i64)> = all_branches_with_prs
                .iter()
                .map(|(_, pos, num)| (*pos, *num))
                .collect();
            ordered.sort_by_key(|(pos, _)| *pos);
            StackAction::Create {
                all_ordered: ordered.into_iter().map(|(_, num)| num).collect(),
            }
        }
        Some(stack_number) => {
            let prior_top_position = all_branches_with_prs
                .iter()
                .filter(|(bid, _, _)| !newly_created_branch_ids.contains(bid))
                .map(|(_, pos, _)| *pos)
                .max();
            let mut new_ordered: Vec<(i64, i64)> = all_branches_with_prs
                .iter()
                .filter(|(bid, _, _)| newly_created_branch_ids.contains(bid))
                .map(|(_, pos, num)| (*pos, *num))
                .collect();
            new_ordered.sort_by_key(|(pos, _)| *pos);
            let lowest_new_position = new_ordered.first().map(|(pos, _)| *pos);
            let extends_top = match (prior_top_position, lowest_new_position) {
                (Some(prior), Some(new_low)) => new_low > prior,
                (None, Some(_)) => true,
                (_, None) => false,
            };
            if !extends_top {
                return StackAction::Skip {
                    reason: "new PRs do not strictly extend the recorded stack's top",
                };
            }
            StackAction::Append {
                stack_number,
                new_ordered: new_ordered.into_iter().map(|(_, num)| num).collect(),
            }
        }
    }
}

/// Re-checks each already-recorded "open" PR's *live* state on the forge
/// before letting it count toward "this branch already has one" (RAL-190+):
/// a PR closed or merged outside ralphus -- the GitHub/GitLab UI, `gh pr
/// close`, ... -- must not keep silently blocking a fresh submission
/// forever just because the locally recorded row was never told about it.
/// Persists any state change onto the local row (so `GET .../pull-requests`
/// stops reporting it as open too) and returns only the branches whose PR
/// is genuinely still open on the forge right now. Best-effort per PR: a
/// state-lookup failure (network, missing token, ...) is logged and that PR
/// is conservatively left as still-open, so "couldn't check" never gets
/// mistaken for "confirmed closed" and duplicates a PR that's actually
/// still fine.
fn refresh_open_prs<'a>(
    store: &Arc<Mutex<Store>>,
    client: &crate::forge::ForgeClient,
    open_by_branch: HashMap<&'a str, &'a PullRequestView>,
) -> HashMap<&'a str, &'a PullRequestView> {
    open_by_branch
        .into_iter()
        .filter(|(_, pr)| {
            let Some(number) = pr.pr_number else {
                return true;
            };
            match client.get_pull_request_state(number) {
                Ok(state) if state != "open" => {
                    // ralphus[ignore-rlog-pair]: this low-level helper has no Store; its Store-owning caller records the structured workflow outcome
                    crate::rlog!(
                        INFO,
                        "ralphus [pr] pr {} branch_id={:?} found closed externally (state={state}) \
                         -- no longer counts as an open submission",
                        pr.id,
                        pr.branch_id
                    );
                    let _ = store.lock().expect("poisoned").update_pull_request_ex(
                        &pr.id,
                        None,
                        None,
                        None,
                        Some(&state),
                        None,
                        None,
                        None,
                    );
                    false
                }
                Ok(_) => true,
                Err(e) => {
                    // ralphus[ignore-rlog-pair]: this low-level helper has no Store; its Store-owning caller records the structured workflow outcome
                    crate::rlog!(
                        WARNING,
                        "ralphus [pr] pr {} state check failed, assuming still open: {e}",
                        pr.id
                    );
                    true
                }
            }
        })
        .collect()
}

/// Submit a PR for every enabled branch that doesn't already have an open
/// one -- checking each recorded PR's *live* forge state first via
/// [`refresh_open_prs`], so a branch whose old PR was closed/merged outside
/// ralphus gets a fresh one instead of being silently skipped forever --
/// chaining bases via [`submit_stacked_branch_pr`] exactly like an explicit
/// per-branch request would. Also re-runs [`resync_pr_bases`] so any
/// *pre-existing* open PR whose base no longer matches the current chain
/// (e.g. one created before this bug fix landed, still pointed at the
/// guardian's own base branch instead of the branch below it) gets corrected
/// too, not just left alone forever -- "submit the whole stack" should mean
/// the whole stack ends up correctly represented on the forge, not just
/// "create whatever's missing". Then (GitHub only) registers or grows a
/// native GitHub PR stack spanning the whole chain via
/// [`decide_stack_action`]. This is what a `branch_id: null` request in
/// `prs` now means (RAL-190+): "submit the whole stack", not "push the
/// squashed combined worktree as one PR" -- see the [`PrRequest`] doc
/// comment.
#[allow(clippy::too_many_arguments)]
fn submit_stack_for_guardian(
    store: &Arc<Mutex<Store>>,
    runner: &dyn Runner,
    client: &crate::forge::ForgeClient,
    id: &str,
    root: &Path,
    remote_name: &str,
    guardian: &GuardianView,
    ordered_enabled: &[&BranchView],
    alias_by_branch: &mut HashMap<String, String>,
    base_branch_name: &str,
    existing_prs: &[PullRequestView],
    pr_branch_convention: &str,
    trace_context: Option<&str>,
    stack_id: &str,
    use_worktree_branch_name: Option<bool>,
    fork_routing: Option<&ForkRouting>,
) -> std::result::Result<Vec<PullRequestView>, String> {
    let already_open = refresh_open_prs(store, client, open_prs_by_branch(existing_prs));
    let mut created = Vec::new();
    let mut newly_created_branch_ids = std::collections::HashSet::new();

    for branch in ordered_enabled {
        if already_open.contains_key(branch.id.as_str()) {
            continue;
        }
        let req = PrRequest {
            branch_id: Some(branch.id.clone()),
            branch_alias: None,
            title: None,
            description: None,
            use_worktree_branch_name,
        };
        let pr = submit_stacked_branch_pr(
            store,
            runner,
            client,
            id,
            root,
            remote_name,
            guardian,
            ordered_enabled,
            alias_by_branch,
            base_branch_name,
            branch,
            &req,
            pr_branch_convention,
            trace_context,
            stack_id,
            fork_routing,
        )?;
        newly_created_branch_ids.insert(branch.id.clone());
        created.push(pr);
    }

    // Re-target any PR that already existed for this guardian but whose base
    // no longer matches the current stack order/chain (RAL-190) -- without
    // this, a PR left over from before this bug fix (or from a manual
    // resubmission that raced a reorder) is treated as "already submitted,
    // nothing to do" forever, even though it's still pointed at the wrong
    // base and doesn't actually read as part of the stack on the forge.
    let resynced = resync_pr_bases(store, id).unwrap_or_else(|e| {
        crate::rlog!(WARNING, "ralphus [pr] review {id} stack resync failed: {e}");
        0
    });
    {
        let guard = store.lock().expect("poisoned");
        let _ = guard.cartographer_log(crate::cartographer::CartographerEntry {
            level: crate::logging::LogLevel::INFO,
            source: "pr",
            message: "pr stack submission completed",
            scope: Some("guardian"),
            squad_id: None,
            guardian_id: Some(id),
            cell_id: None,
            task: None,
            log_path: None,
            payload: serde_json::json!({"created": created.len(), "resynced": resynced}),
            admin_only: false,
        });
    }

    if client.kind() != crate::forge::ForgeKind::GitHub {
        return Ok(created);
    }

    // RAL-338: GitHub's native stack API is repository-scoped, so a
    // fork-mode root PR -- filed against the *parent* repo, with a PR number
    // in that repo's own namespace -- must never be mixed into a stack
    // registration call made against the fork's repo. Excluding it here
    // means the native stack only ever spans the contiguous fork-local PRs;
    // the root's parent PR is tracked solely through `PullRequestView`
    // itself (Q3.2), not GitHub's stack object.
    let all_with_prs: Vec<(String, i64, i64)> = ordered_enabled
        .iter()
        .filter_map(|branch| {
            let pr_view: &PullRequestView =
                already_open.get(branch.id.as_str()).copied().or_else(|| {
                    created
                        .iter()
                        .find(|p| p.branch_id.as_deref() == Some(branch.id.as_str()))
                })?;
            if let Some(routing) = fork_routing
                && pr_view.repo != routing.fork_client.repo_label()
            {
                return None;
            }
            let number = pr_view.pr_number?;
            Some((branch.id.clone(), branch.position, number))
        })
        .collect();

    let recorded = store
        .lock()
        .expect("poisoned")
        .get_guardian_forge_stack_number(id)
        .map_err(|e| e.to_string())?;
    match decide_stack_action(&all_with_prs, &newly_created_branch_ids, recorded) {
        StackAction::Create { all_ordered } => match client.create_stack(&all_ordered) {
            Ok(Some(stack)) => {
                let _ = store
                    .lock()
                    .expect("poisoned")
                    .set_guardian_forge_stack_number(id, stack.number);
                crate::rlog!(
                    INFO,
                    "ralphus [pr] review {id} registered github pr stack number={}",
                    stack.number
                );
            }
            Ok(None) => {}
            Err(e) => crate::rlog!(WARNING, "ralphus [pr] review {id} create stack failed: {e}"),
        },
        StackAction::Append {
            stack_number,
            new_ordered,
        } => {
            if let Err(e) = client.add_to_stack(stack_number, &new_ordered) {
                crate::rlog!(
                    WARNING,
                    "ralphus [pr] review {id} add to stack {stack_number} failed: {e}"
                );
            }
        }
        StackAction::Skip { reason } => {
            crate::rlog!(
                DEBUG,
                "ralphus [pr] review {id} skipping stack registration: {reason}"
            );
        }
    }

    Ok(created)
}

// ---------------------------------------------------------------------------
// RAL-317: per-branch auto-submit trigger
// ---------------------------------------------------------------------------

/// Submit/grow this guardian's PR stack for every branch that has reached a
/// terminal merge state (`done`/`conflict_resolved`) and doesn't already have
/// a live open PR. This is [`submit_stack_for_guardian`] -- the exact
/// machinery the manual "submit whole stack" (`branch_id: None`) request
/// already exercises -- restricted to terminal branches only: a still-
/// rebasing sibling has no `review_branch` yet and would fail
/// `submit_stacked_branch_pr`'s precondition, so it is simply excluded from
/// consideration here rather than aborting the branches that ARE ready.
fn auto_submit_terminal_branches(
    store: &Arc<Mutex<Store>>,
    runner: &dyn Runner,
    id: &str,
) -> std::result::Result<Vec<PullRequestView>, String> {
    let guardian = store
        .lock()
        .expect("poisoned")
        .get_guardian(id)
        .map_err(|e| e.to_string())?;
    let root = PathBuf::from(&guardian.git_root);
    let forge_cfg = crate::config::resolve_forge(&root);
    // RAL-338: this is a daemon-internal poller, not a per-request submit --
    // no acting-user identity exists to resolve, so it uses
    // `[daemon].default_user` (falling back to the project-wide default fork
    // row when even that is unset), matching `current_user`'s own fallback.
    let poller_user = crate::config::load_daemon_config()
        .default_user
        .unwrap_or_default();
    let fork_routing = resolve_fork_routing(store, &root, &guardian, &forge_cfg, &poller_user)?;
    if fork_routing.is_some() && guardian.machine.is_some() {
        return Err(format!(
            "review {id} declares a remote machine ({:?}) and its project has a registered \
             fork; fork-mode auto-submit is not supported for remote-machine reviews (RAL-338)",
            guardian.machine
        ));
    }
    if let Some(routing) = &fork_routing {
        // Auto-submit is a background convenience trigger, not an explicit
        // user action -- there is no CLI flag here to ask for an override,
        // so an unlinked fork always blocks it (never silently downgraded).
        run_fork_preflight(store, id, routing, false)?;
    }
    let fork_remote_exclude = fork_routing.as_ref().map(|r| r.fork.remote_name.as_str());
    let parent_remote_name = crate::forge::resolve_remote_name_excluding(
        &root,
        &guardian.base_branch,
        &forge_cfg,
        fork_remote_exclude,
    );
    let base_branch_name = strip_remote_prefix(&guardian.base_branch, &parent_remote_name);
    let remote_name = push_remote_for(fork_routing.as_ref(), &parent_remote_name).to_string();
    let client = match &fork_routing {
        Some(routing) => routing.fork_client.clone(),
        None => crate::forge::resolve_remote_for(&root, &parent_remote_name, &forge_cfg)?,
    };
    let pr_branch_convention = forge_cfg.resolved_pr_branch_convention().to_string();

    let mut ordered_enabled: Vec<&BranchView> = guardian
        .branches
        .iter()
        .filter(|b| b.enabled && matches!(b.merge_status.as_str(), "done" | "conflict_resolved"))
        .collect();
    ordered_enabled.sort_by_key(|b| b.position);
    if ordered_enabled.is_empty() {
        return Ok(Vec::new());
    }

    let existing_prs = store
        .lock()
        .expect("poisoned")
        .list_pull_requests_for_guardian(id)
        .map_err(|e| e.to_string())?;
    let mut alias_by_branch = open_alias_by_branch(&existing_prs);

    let stack_id = store
        .lock()
        .expect("poisoned")
        .next_id("guardian_pr_stack_seq", "prstack")
        .map_err(|e| e.to_string())?;

    submit_stack_for_guardian(
        store,
        runner,
        &client,
        id,
        &root,
        &remote_name,
        &guardian,
        &ordered_enabled,
        &mut alias_by_branch,
        &base_branch_name,
        &existing_prs,
        &pr_branch_convention,
        None,
        &stack_id,
        None,
        fork_routing.as_ref(),
    )
}

/// RAL-317: called from [`crate::guardian_merge::promote_branch_terminal`]
/// every time a branch reaches a terminal (`done`/`conflict_resolved`) merge
/// state. A no-op unless the review's
/// [`GuardianView::effective_auto_submit_pr_stack`] setting is on. Never
/// fails or blocks the caller's merge transition -- errors are logged and
/// recorded per-branch via [`Store::set_branch_auto_submit_error`] instead of
/// propagated, and a success clears any previously-recorded error.
///
/// Does a cheap, local-only diff first -- comparing the branch's current
/// review-ref sha against its existing open PR row's `last_pushed_sha`, no
/// push/forge call involved -- so a re-entrant call for a branch whose state
/// hasn't actually changed since its last successful auto-submit is a no-op
/// before any network I/O happens (also clearing a stale failure marker, if
/// any -- the branch's state is fine now regardless of how it got there).
/// This is the anti-spam mechanism: no separate time-based debounce is
/// needed.
pub fn maybe_auto_submit_branch(
    store: &Arc<Mutex<Store>>,
    runner: &dyn Runner,
    id: &str,
    branch_id: &str,
) {
    let guardian = match store.lock().expect("poisoned").get_guardian(id) {
        Ok(g) => g,
        Err(_) => return,
    };
    if !guardian.effective_auto_submit_pr_stack {
        return;
    }
    let Some(branch) = guardian.branches.iter().find(|b| b.id == branch_id) else {
        return;
    };
    if !branch.enabled {
        return;
    }
    let Some(review_ref) = branch.review_branch.as_deref() else {
        return;
    };
    let root = PathBuf::from(&guardian.git_root);
    let Ok(current_sha) = git(&root, &["rev-parse", review_ref]).map(|s| s.trim().to_string())
    else {
        return;
    };

    let existing_prs = match store
        .lock()
        .expect("poisoned")
        .list_pull_requests_for_guardian(id)
    {
        Ok(v) => v,
        Err(_) => return,
    };
    let already_covered = existing_prs.iter().any(|pr| {
        pr.branch_id.as_deref() == Some(branch_id)
            && pr.state == "open"
            && pr.last_pushed_sha.as_deref() == Some(current_sha.as_str())
    });
    if already_covered {
        // The branch's state is already correctly reflected on the forge
        // (e.g. a human ran `review pr submit` manually in the meantime) --
        // clear any stale failure marker from a past attempt rather than
        // leaving a resolved problem shown as still-failing.
        let _ = store
            .lock()
            .expect("poisoned")
            .set_branch_auto_submit_error(id, branch_id, None);
        return;
    }

    match auto_submit_terminal_branches(store, runner, id) {
        Ok(_) => {
            let _ = store
                .lock()
                .expect("poisoned")
                .set_branch_auto_submit_error(id, branch_id, None);
        }
        Err(e) => {
            crate::rlog!(
                WARNING,
                "ralphus [pr] review {id} branch {branch_id} auto-submit-pr-stack failed: {e}"
            );
            let _ = store
                .lock()
                .expect("poisoned")
                .set_branch_auto_submit_error(id, branch_id, Some(&e));
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn submit_pull_requests_inner(
    store: &Arc<Mutex<Store>>,
    runner: &dyn Runner,
    id: &str,
    requests: Vec<PrRequest>,
    trace_context: Option<&str>,
    user: &str,
    allow_unlinked_fork: bool,
) -> std::result::Result<Vec<PullRequestView>, String> {
    let guardian = store
        .lock()
        .expect("poisoned")
        .get_guardian(id)
        .map_err(|e| e.to_string())?;
    let root = PathBuf::from(&guardian.git_root);
    let forge_cfg = crate::config::resolve_forge(&root);
    let fork_routing = resolve_fork_routing(store, &root, &guardian, &forge_cfg, user)?;
    if fork_routing.is_some() && guardian.machine.is_some() {
        return Err(format!(
            "review {id} declares a remote machine ({:?}) and its project has a registered \
             fork; fork-mode submission is not supported for remote-machine reviews (RAL-338)",
            guardian.machine
        ));
    }
    if let Some(routing) = &fork_routing {
        run_fork_preflight(store, id, routing, allow_unlinked_fork)?;
    }
    // `parent_remote_name` never resolves to the fork remote (RAL-338), so
    // `base_branch_name` -- always the *parent's* base branch, unprefixed --
    // is correct regardless of fork mode; only the push target differs.
    let fork_remote_exclude = fork_routing.as_ref().map(|r| r.fork.remote_name.as_str());
    let parent_remote_name = crate::forge::resolve_remote_name_excluding(
        &root,
        &guardian.base_branch,
        &forge_cfg,
        fork_remote_exclude,
    );
    let base_branch_name = strip_remote_prefix(&guardian.base_branch, &parent_remote_name);
    let remote_name = push_remote_for(fork_routing.as_ref(), &parent_remote_name).to_string();
    let client = match &fork_routing {
        Some(routing) => routing.fork_client.clone(),
        None => crate::forge::resolve_remote_for(&root, &parent_remote_name, &forge_cfg)?,
    };
    let pr_branch_convention = forge_cfg.resolved_pr_branch_convention().to_string();

    let mut ordered_enabled: Vec<&BranchView> =
        guardian.branches.iter().filter(|b| b.enabled).collect();
    ordered_enabled.sort_by_key(|b| b.position);

    let existing_prs = store
        .lock()
        .expect("poisoned")
        .list_pull_requests_for_guardian(id)
        .map_err(|e| e.to_string())?;
    // Seeded from *every* already-open PR, not just what's in `requests` --
    // this is what lets a branch submitted on its own (no sibling requests in
    // this call) still chain onto a branch that was submitted earlier, in a
    // previous call.
    let mut alias_by_branch = open_alias_by_branch(&existing_prs);

    let mut stacked: Vec<&PrRequest> = requests.iter().filter(|r| r.branch_id.is_some()).collect();
    stacked.sort_by_key(|r| {
        let bid = r.branch_id.as_deref().unwrap_or("");
        guardian
            .branches
            .iter()
            .find(|b| b.id == bid)
            .map(|b| b.position)
            .unwrap_or(0)
    });
    let submit_whole_stack = requests.iter().any(|r| r.branch_id.is_none());
    // RAL-307: a whole-stack request's own override (if the caller set one)
    // applies to every branch this call creates -- there's exactly one
    // `branch_id: None` request per submission (title/description are
    // likewise ignored/undefined across more than one), so the first match
    // is unambiguous.
    let whole_stack_use_worktree_branch_name = requests
        .iter()
        .find(|r| r.branch_id.is_none())
        .and_then(|r| r.use_worktree_branch_name);

    // RAL-302: one id per call to `submit_pull_requests_inner`, stamped on
    // every PR row this call creates (stacked and/or whole-stack), so a past
    // submission's sibling branches are queryable as one group later.
    let stack_id = store
        .lock()
        .expect("poisoned")
        .next_id("guardian_pr_stack_seq", "prstack")
        .map_err(|e| e.to_string())?;

    let mut created = Vec::new();
    for req in stacked {
        let branch_id = req.branch_id.as_deref().unwrap_or("");
        let branch = guardian
            .branches
            .iter()
            .find(|b| b.id == branch_id)
            .ok_or_else(|| format!("no branch with id {branch_id}"))?;
        let pr = submit_stacked_branch_pr(
            store,
            runner,
            &client,
            id,
            &root,
            &remote_name,
            &guardian,
            &ordered_enabled,
            &mut alias_by_branch,
            &base_branch_name,
            branch,
            req,
            &pr_branch_convention,
            trace_context,
            &stack_id,
            fork_routing.as_ref(),
        )?;
        created.push(pr);
    }

    if submit_whole_stack {
        let stack_prs = submit_stack_for_guardian(
            store,
            runner,
            &client,
            id,
            &root,
            &remote_name,
            &guardian,
            &ordered_enabled,
            &mut alias_by_branch,
            &base_branch_name,
            &existing_prs,
            &pr_branch_convention,
            trace_context,
            &stack_id,
            whole_stack_use_worktree_branch_name,
            fork_routing.as_ref(),
        )?;
        created.extend(stack_prs);
    }

    Ok(created)
}

// ---------------------------------------------------------------------------
// Bidirectional sync (RAL-190)
// ---------------------------------------------------------------------------

/// One lock per repository root, serializing the `git fetch` +
/// `rev-parse FETCH_HEAD` pair in [`compute_sync_status`].
///
/// `FETCH_HEAD` is a single file shared by the whole repository, so two
/// fetches running in it at once can have either one's `rev-parse` read the
/// other's result -- reporting a PR as ahead/behind against a sibling PR's
/// tip. That pairing is reachable: the board fetches every open PR's
/// sync-status for a review concurrently (`pollPullRequests` in board.html),
/// and the daemon answers read-only requests on a pool of threads
/// (`server::ReadPool`), so a stacked review's PRs land here at the same time
/// in the same repository.
static SYNC_FETCH_LOCKS: Mutex<Option<HashMap<PathBuf, Arc<Mutex<()>>>>> = Mutex::new(None);

/// The [`SYNC_FETCH_LOCKS`] entry for `root`, creating it on first use.
fn sync_fetch_lock(root: &Path) -> Arc<Mutex<()>> {
    let mut locks = SYNC_FETCH_LOCKS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    Arc::clone(
        locks
            .get_or_insert_with(HashMap::new)
            .entry(root.to_path_buf())
            .or_default(),
    )
}

/// Drift between a PR's remote branch and its owning review worktree
/// (RAL-190), as returned by [`compute_sync_status`].
#[derive(Debug, Clone, Serialize)]
pub struct PrSyncStatus {
    /// Current tip of the remote `branch_alias` branch, or `None` if it
    /// couldn't be fetched (deleted, no network, no permission, ...).
    pub remote_sha: Option<String>,
    /// Current tip of the owning review worktree's branch/combined branch.
    pub local_sha: Option<String>,
    /// The last sha this daemon itself pushed to `branch_alias`.
    pub last_pushed_sha: Option<String>,
    /// `remote_sha` and `local_sha` are identical.
    pub in_sync: bool,
    /// The PR branch has commits the review worktree does not (e.g. a
    /// reviewer pushed a fix directly to the open PR) — offer "pull PR
    /// commits" in the UI.
    pub pr_ahead: bool,
    /// The review worktree has commits not yet reflected on the PR branch
    /// (e.g. feedback was just resolved) — offer "push to PR" in the UI.
    pub worktree_ahead: bool,
}

/// Compute [`PrSyncStatus`] for `pr_id`: fetches the remote `branch_alias`
/// tip and compares it against the owning review worktree's current tip via
/// `git merge-base --is-ancestor` in both directions. A combined-worktree PR
/// (`branch_id = None`) compares against the guardian's combined review
/// branch; a stacked PR compares against its own branch's review branch.
pub fn compute_sync_status(
    store: &Arc<Mutex<Store>>,
    pr_id: &str,
) -> std::result::Result<PrSyncStatus, String> {
    let pr = store
        .lock()
        .expect("poisoned")
        .get_pull_request(pr_id)
        .map_err(|e| e.to_string())?;
    let guardian = store
        .lock()
        .expect("poisoned")
        .get_guardian(&pr.guardian_id)
        .map_err(|e| e.to_string())?;
    let root = PathBuf::from(&guardian.git_root);
    let forge_cfg = crate::config::resolve_forge(&root);
    // RAL-338: this PR's branch alias lives on the fork's remote in fork
    // mode, including the root's (its PR is filed against the parent, but
    // its ref still lives on the fork).
    let routing = resolve_pr_repo_routing(store, &root, &guardian.base_branch, &forge_cfg);
    let remote_name = routing.remote_for(&pr.repo).to_string();

    let local_ref = if let Some(bid) = &pr.branch_id {
        guardian
            .branches
            .iter()
            .find(|b| &b.id == bid)
            .and_then(|b| b.review_branch.clone())
    } else {
        guardian.review_branch.clone()
    };
    let local_sha = local_ref
        .as_deref()
        .and_then(|r| git(&root, &["rev-parse", r]).ok())
        .map(|s| s.trim().to_string());
    // Held across both commands: the `rev-parse` has to read back the
    // `FETCH_HEAD` this very fetch wrote. See `SYNC_FETCH_LOCKS`.
    let remote_sha = {
        let fetch_lock = sync_fetch_lock(&root);
        let _fetching = fetch_lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        git(&root, &["fetch", &remote_name, &pr.branch_alias])
            .ok()
            .and_then(|_| git(&root, &["rev-parse", "FETCH_HEAD"]).ok())
            .map(|s| s.trim().to_string())
    };

    let is_ancestor = |ancestor: &str, descendant: &str| {
        git(
            &root,
            &["merge-base", "--is-ancestor", ancestor, descendant],
        )
        .is_ok()
    };
    // RAL-190: prefer `last_pushed_sha` -- the SHA this daemon itself last
    // put on the PR branch -- over raw ancestry when classifying which side
    // is "ahead". Ancestry alone can't survive a rebase: replaying a branch
    // onto a shifted base rewrites every commit's SHA, so neither tip stays
    // an ancestor of the other even when nothing genuinely diverged (a
    // clean rebase-through, or a conflict that got resolved -- however much
    // effort that took). `last_pushed_sha` pins the fork point to "the last
    // state both sides are known to have agreed on": if only one side has
    // moved away from it, that side is unambiguously ahead regardless of
    // how it got there. Ancestry is still the fallback when neither side
    // matches the fork point (never synced yet, or both sides changed
    // independently since) -- that's a true two-sided divergence.
    let last_pushed = pr.last_pushed_sha.as_deref();
    let (pr_ahead, worktree_ahead, in_sync) = match (&remote_sha, &local_sha) {
        (Some(r), Some(l)) if r == l => (false, false, true),
        (Some(r), Some(l)) => match last_pushed {
            Some(p) if p == r => (false, true, false),
            Some(p) if p == l => (true, false, false),
            _ => (!is_ancestor(r, l), !is_ancestor(l, r), false),
        },
        (Some(_), None) => (true, false, false),
        (None, Some(_)) => (false, true, false),
        (None, None) => (false, false, false),
    };

    Ok(PrSyncStatus {
        remote_sha,
        local_sha,
        last_pushed_sha: pr.last_pushed_sha,
        in_sync,
        pr_ahead,
        worktree_ahead,
    })
}

/// Pull the PR branch's fetched commits into its owning review worktree,
/// resolving conflicts through the same agent path a normal stack rebase
/// uses, then push the merged result back to the remote (RAL-190). A PR
/// submitted from the combined worktree (`branch_id = None`) routes to the
/// topmost enabled stacked branch, mirroring [`action_pr_feedback_inner`]'s
/// convention for the same case. Returns `Ok(false)` when the PR branch had
/// nothing new to pull.
pub fn pull_pr_commits(
    store: &Arc<Mutex<Store>>,
    runner: &dyn Runner,
    pr_id: &str,
) -> std::result::Result<bool, String> {
    let pr = store
        .lock()
        .expect("poisoned")
        .get_pull_request(pr_id)
        .map_err(|e| e.to_string())?;
    let guardian = store
        .lock()
        .expect("poisoned")
        .get_guardian(&pr.guardian_id)
        .map_err(|e| e.to_string())?;
    let branch_id = match &pr.branch_id {
        Some(bid) => bid.clone(),
        None => guardian
            .branches
            .iter()
            .filter(|b| b.enabled)
            .max_by_key(|b| b.position)
            .map(|b| b.id.clone())
            .ok_or_else(|| "guardian has no enabled branches".to_string())?,
    };
    let root = PathBuf::from(&guardian.git_root);
    let forge_cfg = crate::config::resolve_forge(&root);
    // RAL-338: pull from whichever remote this PR's own alias actually lives
    // on (the fork in fork mode, including the root's).
    let routing = resolve_pr_repo_routing(store, &root, &guardian.base_branch, &forge_cfg);
    let remote_name = routing.remote_for(&pr.repo).to_string();

    let pulled = guardian_merge::pull_pr_commits(
        store,
        runner,
        &pr.guardian_id,
        &branch_id,
        &remote_name,
        &pr.branch_alias,
        pr.last_pushed_sha.as_deref(),
    )?;
    if !pulled {
        return Ok(false);
    }

    let updated = store
        .lock()
        .expect("poisoned")
        .get_guardian(&pr.guardian_id)
        .map_err(|e| e.to_string())?;
    let local_ref = if pr.branch_id.is_some() {
        updated
            .branches
            .iter()
            .find(|b| b.id == branch_id)
            .and_then(|b| b.review_branch.clone())
    } else {
        updated.review_branch.clone()
    }
    .ok_or_else(|| "no review ref to push after pulling PR commits".to_string())?;

    push_ref(&root, &remote_name, &local_ref, &pr.branch_alias)?;
    if let Ok(sha) = git(&root, &["rev-parse", &local_ref]) {
        let _ = store.lock().expect("poisoned").update_pull_request_ex(
            pr_id,
            None,
            None,
            None,
            None,
            None,
            Some(Some(sha.trim())),
            None,
        );
    }
    {
        let guard = store.lock().expect("poisoned");
        let _ = guard.cartographer_log(crate::cartographer::CartographerEntry {
            level: crate::logging::LogLevel::INFO,
            source: "pr",
            message: "pr commits pulled into worktree and pushed back",
            scope: Some("guardian"),
            squad_id: None,
            guardian_id: Some(&pr.guardian_id),
            cell_id: None,
            task: None,
            log_path: None,
            payload: serde_json::json!({"pr_id": pr_id}),
            admin_only: false,
        });
    }
    Ok(true)
}

/// Kick off [`pull_pr_commits`] in the background; returns immediately.
pub fn start_pull_pr_commits(
    store: Arc<Mutex<Store>>,
    runner: Arc<dyn Runner>,
    pr_id: &str,
) -> Reply {
    let pr = {
        let guard = store.lock().expect("poisoned");
        guard.get_pull_request(pr_id)
    };
    let guardian_id = match pr {
        Ok(pr) => pr.guardian_id,
        Err(e) => return error_reply(404, "not_found", &e.to_string()),
    };
    let pid = pr_id.to_string();
    std::thread::spawn(
        move || match pull_pr_commits(&store, runner.as_ref(), &pid) {
            Ok(pulled) => {
                let guard = store.lock().expect("poisoned");
                let _ = guard.cartographer_log(crate::cartographer::CartographerEntry {
                    level: crate::logging::LogLevel::INFO,
                    source: "pr",
                    message: if pulled {
                        "pr pull-from-pr completed"
                    } else {
                        "pr pull-from-pr found nothing new"
                    },
                    scope: Some("guardian"),
                    squad_id: None,
                    guardian_id: Some(&guardian_id),
                    cell_id: None,
                    task: None,
                    log_path: None,
                    payload: serde_json::json!({"pr_id": pid, "pulled": pulled}),
                    admin_only: false,
                });
            }
            Err(e) => {
                crate::rlog!(ERROR, "ralphus [pr] pull-from-pr failed pr={pid}: {e}");
                let guard = store.lock().expect("poisoned");
                let _ = guard.cartographer_log(crate::cartographer::CartographerEntry {
                    level: crate::logging::LogLevel::ERROR,
                    source: "pr",
                    message: "pr pull-from-pr failed",
                    scope: Some("guardian"),
                    squad_id: None,
                    guardian_id: Some(&guardian_id),
                    cell_id: None,
                    task: None,
                    log_path: None,
                    payload: serde_json::json!({"pr_id": pid, "error": e}),
                    admin_only: false,
                });
            }
        },
    );
    reply(202, &serde_json::json!({"status": "pulling_pr_commits"}))
}

/// Fetch un-actioned comments/notes on the PR recorded as `pr_id`, aggregate
/// them into one feedback string, and apply them into the owning review
/// worktree by delegating to `guardian_merge::run_feedback` for the
/// appropriate branch position — reusing its existing edit/commit/downstream-
/// restack pipeline verbatim, so a PR-feedback-driven change rebuilds
/// dependent stacked branches exactly like a manual "Pull in PR feedback"
/// button would. A PR submitted from the combined worktree (`branch_id
/// == None`) routes feedback to the topmost enabled stacked branch, since the
/// combined worktree itself is a read-only view rather than an editable one.
///
/// For a branch-specific PR, `run_feedback` itself already commits and
/// pushes that branch (see its doc comment) -- this just records the sha it
/// pushed against the PR row. For a whole-stack PR (`branch_id: None`), the
/// tracked ref is the COMBINED branch, which `run_feedback` never touches
/// (it only pushes the specific branch it edited, here the topmost enabled
/// one), so that case is still pushed back separately here, under the PR's
/// recorded alias. Returns the number of comments actioned (`0` when there
/// was nothing new).
///
/// Runs under one `pr.action_feedback` OpenTelemetry span (RAL-96,
/// `SpanKind::Internal`), started fresh for the same reason
/// [`submit_pull_requests`] starts its own span. The delegated
/// `guardian_merge::run_feedback` call is not itself given this span's
/// context (its signature is shared with the manual "Pull in PR feedback"
/// button and predates RAL-117), so its own `RunnerSpec` calls still start
/// unlinked traces — only the PR-specific steps around it (the forge comment
/// fetch and the push-back) are covered here.
pub fn action_pr_feedback(
    store: &Arc<Mutex<Store>>,
    runner: &dyn Runner,
    pr_id: &str,
) -> std::result::Result<usize, String> {
    let cx = crate::otel::context_from_traceparent(None);
    let span = crate::otel::start_span("pr.action_feedback", &cx, SpanKind::Internal);
    span.set_attribute("pr_id", pr_id.to_string());

    // ralphus[ignore-rlog-pair]: this low-level helper has no Store; its Store-owning caller records the structured workflow outcome
    crate::rlog!(INFO, "ralphus [pr] pr {pr_id} actioning feedback");
    let result = action_pr_feedback_inner(store, runner, pr_id);
    match &result {
        Ok(n) => {
            span.set_status(Status::Ok);
            span.set_attribute("pr.comments_actioned", *n as i64);
            // ralphus[ignore-rlog-pair]: this low-level helper has no Store; its Store-owning caller records the structured workflow outcome
            crate::rlog!(
                INFO,
                "ralphus [pr] pr {pr_id} feedback actioned comments={n}"
            );
        }
        Err(e) => span.set_status(Status::error(e.clone())),
    }
    result
}

fn action_pr_feedback_inner(
    store: &Arc<Mutex<Store>>,
    runner: &dyn Runner,
    pr_id: &str,
) -> std::result::Result<usize, String> {
    let pr = store
        .lock()
        .expect("poisoned")
        .get_pull_request(pr_id)
        .map_err(|e| e.to_string())?;
    let guardian = store
        .lock()
        .expect("poisoned")
        .get_guardian(&pr.guardian_id)
        .map_err(|e| e.to_string())?;
    let root = PathBuf::from(&guardian.git_root);
    let forge_cfg = crate::config::resolve_forge(&root);
    // RAL-338: this PR may be filed on either the parent or the fork --
    // resolve both candidates and use whichever matches its own recorded
    // `repo` for both the comments read and (below) the push-back remote.
    let routing = resolve_pr_repo_routing(store, &root, &guardian.base_branch, &forge_cfg);
    let remote_name = routing.remote_for(&pr.repo).to_string();
    let client = routing.client_for(&pr.repo).cloned().ok_or_else(|| {
        format!(
            "could not resolve a forge client for recorded {}/{}",
            pr.forge, pr.repo
        )
    })?;
    let pr_number = pr
        .pr_number
        .ok_or_else(|| "PR has no recorded number yet".to_string())?;

    let comments = client.list_pr_comments(pr_number)?;
    let already = store
        .lock()
        .expect("poisoned")
        .actioned_pr_comment_ids(pr_id)
        .map_err(|e| e.to_string())?;
    let fresh: Vec<_> = comments
        .into_iter()
        .filter(|c| !already.contains(&c.external_id))
        .collect();
    // ralphus[ignore-rlog-pair]: this low-level helper has no Store; its Store-owning caller records the structured workflow outcome
    crate::rlog!(
        DEBUG,
        "ralphus [pr] pr {pr_id} comments fresh={} already_actioned={}",
        fresh.len(),
        already.len()
    );
    if fresh.is_empty() {
        // ralphus[ignore-rlog-pair]: this low-level helper has no Store; its Store-owning caller records the structured workflow outcome
        crate::rlog!(DEBUG, "ralphus [pr] pr {pr_id} no new comments to action");
        return Ok(0);
    }

    let feedback = fresh
        .iter()
        .map(|c| format!("{}: {}", c.author, c.body))
        .collect::<Vec<_>>()
        .join("\n\n---\n\n");

    let (position, branch_id) = match &pr.branch_id {
        Some(bid) => guardian
            .branches
            .iter()
            .find(|b| &b.id == bid)
            .map(|b| (b.position, b.id.clone()))
            .ok_or_else(|| format!("no branch with id {bid}"))?,
        None => guardian
            .branches
            .iter()
            .filter(|b| b.enabled)
            .max_by_key(|b| b.position)
            .map(|b| (b.position, b.id.clone()))
            .ok_or_else(|| "guardian has no enabled branches".to_string())?,
    };

    // ralphus[ignore-rlog-pair]: this low-level helper has no Store; its Store-owning caller records the structured workflow outcome
    crate::rlog!(
        DEBUG,
        "ralphus [pr] pr {pr_id} feedback applying {} comment(s) to guardian={} position={position}",
        fresh.len(),
        pr.guardian_id
    );
    let outcome = guardian_merge::run_feedback(
        store,
        runner,
        &pr.guardian_id,
        &branch_id,
        &feedback,
        &crate::cancel::CancelToken::never(),
    );

    for c in &fresh {
        let _ = store
            .lock()
            .expect("poisoned")
            .mark_pr_comment_actioned(pr_id, &c.external_id);
    }

    if pr.branch_id.is_some() {
        // `run_feedback` already committed (amend-aware) and pushed this
        // exact branch's own review ref -- just record the sha it pushed
        // rather than re-pushing (and re-guarding-against-clobber) the
        // identical ref a second time.
        if let Some(sha) = outcome.pushed_sha.filter(|_| outcome.pushed) {
            let _ = store.lock().expect("poisoned").update_pull_request_ex(
                pr_id,
                None,
                None,
                None,
                None,
                None,
                Some(Some(sha.as_str())),
                None,
            );
        } else if outcome.committed {
            return Err("feedback applied but push back to the PR branch failed".to_string());
        }
    } else {
        // A whole-stack PR tracks the COMBINED branch, which `run_feedback`
        // never pushes (it only pushes the specific branch it edited, here
        // the topmost enabled one) -- push it back separately, as before.
        let updated = store
            .lock()
            .expect("poisoned")
            .get_guardian(&pr.guardian_id)
            .map_err(|e| e.to_string())?;
        let local_ref = updated
            .review_branch
            .clone()
            .ok_or_else(|| "no review ref to push after applying feedback".to_string())?;
        guard_against_clobber(
            &root,
            &remote_name,
            &pr.branch_alias,
            &local_ref,
            pr.last_pushed_sha.as_deref(),
        )?;
        // ralphus[ignore-rlog-pair]: this low-level helper has no Store; its Store-owning caller records the structured workflow outcome
        crate::rlog!(
            DEBUG,
            "ralphus [pr] pr {pr_id} pushing updated branch back alias={} remote={remote_name}",
            pr.branch_alias
        );
        push_ref(&root, &remote_name, &local_ref, &pr.branch_alias)?;
        if let Ok(sha) = git(&root, &["rev-parse", &local_ref]) {
            let _ = store.lock().expect("poisoned").update_pull_request_ex(
                pr_id,
                None,
                None,
                None,
                None,
                None,
                Some(Some(sha.trim())),
                None,
            );
        }
    }

    Ok(fresh.len())
}

// ---------------------------------------------------------------------------
// HTTP-facing async wrappers (mirrors `guardian_merge::start_merge`/`start_feedback`)
// ---------------------------------------------------------------------------

fn reply(status: u16, body: &serde_json::Value) -> Reply {
    Reply {
        status,
        body: body.to_string(),
    }
}

fn error_reply(status: u16, code: &str, message: &str) -> Reply {
    reply(
        status,
        &serde_json::json!({"error": {"code": code, "message": message}}),
    )
}

/// Kick off PR submission in the background; returns immediately. The actual
/// forge calls and `git push`es happen off the request thread since they are
/// both networked and potentially slow.
pub fn start_submit_pull_requests(
    store: Arc<Mutex<Store>>,
    runner: Arc<dyn Runner>,
    id: &str,
    requests: Vec<PrRequest>,
    user: String,
    allow_unlinked_fork: bool,
) -> Reply {
    if requests.is_empty() {
        return error_reply(400, "bad_request", "requests must not be empty");
    }
    let guardian = {
        let guard = store.lock().expect("poisoned");
        guard.get_guardian(id)
    };
    if let Err(e) = guardian {
        return error_reply(404, "not_found", &e.to_string());
    }
    let sid = id.to_string();
    std::thread::spawn(move || {
        match submit_pull_requests(
            &store,
            runner.as_ref(),
            &sid,
            requests,
            &user,
            allow_unlinked_fork,
        ) {
            Ok(prs) => {
                let guard = store.lock().expect("poisoned");
                let _ = guard.cartographer_log(crate::cartographer::CartographerEntry {
                    level: crate::logging::LogLevel::INFO,
                    source: "pr",
                    message: "pull request(s) submitted",
                    scope: Some("guardian"),
                    squad_id: None,
                    guardian_id: Some(&sid),
                    cell_id: None,
                    task: None,
                    log_path: None,
                    payload: serde_json::json!({"count": prs.len()}),
                    admin_only: false,
                });
            }
            Err(e) => {
                crate::rlog!(ERROR, "ralphus [pr] review {sid} submit failed: {e}");
                let guard = store.lock().expect("poisoned");
                let _ = guard.cartographer_log(crate::cartographer::CartographerEntry {
                    level: crate::logging::LogLevel::ERROR,
                    source: "pr",
                    message: "pull request submission failed",
                    scope: Some("guardian"),
                    squad_id: None,
                    guardian_id: Some(&sid),
                    cell_id: None,
                    task: None,
                    log_path: None,
                    payload: serde_json::json!({"error": e}),
                    admin_only: false,
                });
            }
        }
    });
    reply(202, &serde_json::json!({"status": "submitting"}))
}

/// Kick off actioning a PR's feedback in the background; returns immediately.
pub fn start_action_pr_feedback(
    store: Arc<Mutex<Store>>,
    runner: Arc<dyn Runner>,
    pr_id: &str,
) -> Reply {
    let pr = {
        let guard = store.lock().expect("poisoned");
        guard.get_pull_request(pr_id)
    };
    let guardian_id = match pr {
        Ok(pr) => pr.guardian_id,
        Err(e) => return error_reply(404, "not_found", &e.to_string()),
    };
    let pid = pr_id.to_string();
    std::thread::spawn(
        move || match action_pr_feedback(&store, runner.as_ref(), &pid) {
            Ok(n) => {
                let guard = store.lock().expect("poisoned");
                let _ = guard.cartographer_log(crate::cartographer::CartographerEntry {
                    level: crate::logging::LogLevel::INFO,
                    source: "pr",
                    message: "pr feedback actioned",
                    scope: Some("guardian"),
                    squad_id: None,
                    guardian_id: Some(&guardian_id),
                    cell_id: None,
                    task: None,
                    log_path: None,
                    payload: serde_json::json!({"pr_id": pid, "comments": n}),
                    admin_only: false,
                });
            }
            Err(e) => {
                crate::rlog!(ERROR, "ralphus [pr] feedback action failed pr={pid}: {e}");
                let guard = store.lock().expect("poisoned");
                let _ = guard.cartographer_log(crate::cartographer::CartographerEntry {
                    level: crate::logging::LogLevel::ERROR,
                    source: "pr",
                    message: "pr feedback action failed",
                    scope: Some("guardian"),
                    squad_id: None,
                    guardian_id: Some(&guardian_id),
                    cell_id: None,
                    task: None,
                    log_path: None,
                    payload: serde_json::json!({"pr_id": pid, "error": e}),
                    admin_only: false,
                });
            }
        },
    );
    reply(202, &serde_json::json!({"status": "actioning_feedback"}))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::guardian::GuardianStatus;
    use crate::store::Store;
    use git2::build::CheckoutBuilder;
    use std::process::Command;

    fn store() -> Store {
        Store::open_in_memory().unwrap()
    }

    fn tmp_dir(tag: &str) -> PathBuf {
        static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("ralphus-pr-test-{tag}-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn g(root: &Path, args: &[&str]) -> String {
        let out = Command::new("git")
            .args(args)
            .current_dir(root)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@t")
            // `rebase --continue` needs these on a CI runner with no
            // interactive terminal/EDITOR -- otherwise a conflict resolution
            // that needs a commit message fails with "Terminal is dumb, but
            // EDITOR unset" instead of completing.
            .env("GIT_EDITOR", "true")
            .env("GIT_SEQUENCE_EDITOR", "true")
            .output()
            .expect("git");
        assert!(
            out.status.success(),
            "git {args:?} in {} failed: {}",
            root.display(),
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    fn gwrite(root: &Path, name: &str, content: &str) {
        std::fs::write(root.join(name), content).unwrap();
    }

    /// Stage every path in the worktree and commit, in-process via libgit2 --
    /// the `git add . && git commit` two-step done by one `Repository`
    /// handle, with no `git.exe` spawn. Used only for fixture scaffolding;
    /// code under test still goes through the real `git()`/`GitVcs` wrapper.
    fn git2_commit_all(
        repo: &git2::Repository,
        sig: &git2::Signature,
        message: &str,
        parents: &[&git2::Commit],
    ) -> git2::Oid {
        let mut index = repo.index().unwrap();
        index
            .add_all(["*"], git2::IndexAddOption::DEFAULT, None)
            .unwrap();
        index.write().unwrap();
        let tree = repo.find_tree(index.write_tree().unwrap()).unwrap();
        repo.commit(Some("HEAD"), sig, sig, message, &tree, parents)
            .unwrap()
    }

    fn git2_checkout(repo: &git2::Repository, branch: &str) {
        repo.set_head(&format!("refs/heads/{branch}")).unwrap();
        repo.checkout_head(Some(CheckoutBuilder::new().force()))
            .unwrap();
    }

    /// Build `root` with a base commit (`write_base`) and one `review-branch`
    /// commit on top of it (`write_review`), push `review-branch` to a fresh
    /// local bare `remote_dir` as `refs/heads/pr-y`, and register a guardian
    /// branch + PR row pointing at it. Returns the review-branch commit's SHA
    /// alongside the usual fixture tuple.
    ///
    /// Pure scaffolding for `compute_sync_status`/`sync_open_pr_branches`
    /// tests -- built via libgit2 in-process rather than one `git.exe` spawn
    /// per step (PR_SLOWNESS.local.md: fixture cost, not assertion cost,
    /// dominates this file's test time on Windows).
    fn review_fixture(
        tag: &str,
        write_base: impl FnOnce(&Path),
        write_review: impl FnOnce(&Path),
    ) -> (PathBuf, PathBuf, Arc<Mutex<Store>>, String, String) {
        let root = tmp_dir(&format!("{tag}-root"));
        let remote_dir = tmp_dir(&format!("{tag}-remote"));

        let mut init_opts = git2::RepositoryInitOptions::new();
        init_opts.initial_head("main");
        let repo = git2::Repository::init_opts(&root, &init_opts).unwrap();
        let sig = git2::Signature::now("t", "t@t").unwrap();

        write_base(&root);
        let base_oid = git2_commit_all(&repo, &sig, "base", &[]);
        let base_commit = repo.find_commit(base_oid).unwrap();

        repo.branch("review-branch", &base_commit, false).unwrap();
        git2_checkout(&repo, "review-branch");
        write_review(&root);
        let review_oid = git2_commit_all(&repo, &sig, "feat", &[&base_commit]);

        git2_checkout(&repo, "main");

        git2::Repository::init_bare(&remote_dir).unwrap();
        repo.remote("origin", remote_dir.to_str().unwrap())
            .unwrap()
            .push(&["refs/heads/review-branch:refs/heads/pr-y"], None)
            .unwrap();

        let s = store();
        let gid = s
            .create_guardian("demo", "main", root.to_str().unwrap())
            .unwrap();
        s.add_guardian_branch(&gid, "review-branch").unwrap();
        let branch_id = s.get_guardian(&gid).unwrap().branches[0].id.clone();
        s.set_branch_review(
            &gid,
            &branch_id,
            "review-branch",
            root.join("wt-unused").to_str().unwrap(),
        )
        .unwrap();
        let pr_id = s
            .create_pull_request(
                &gid,
                Some(&branch_id),
                "github",
                "acme/w",
                "pr-y",
                "main",
                "T",
                "D",
                None,
                None,
            )
            .unwrap();
        (
            root,
            remote_dir,
            Arc::new(Mutex::new(s)),
            pr_id,
            review_oid.to_string(),
        )
    }

    /// Minimal `BranchView` fixture for the pure `stack_base_for`/
    /// `decide_stack_action` tests below -- only `id`/`position` vary.
    fn test_branch(id: &str, position: i64) -> BranchView {
        BranchView {
            id: id.to_string(),
            position,
            branch: id.to_string(),
            merge_status: "ready".to_string(),
            detail: None,
            review_branch: None,
            worktree: None,
            conflicts_found: None,
            conflicts_fixed: None,
            conflicts_committed: None,
            is_empty: false,
            source_cell_machine: None,
            enabled: true,
            project: None,
            source_cell_state: None,
            can_reenable: false,
            source_squad_id: None,
            source_task_idx: None,
            source_cell_idx: None,
            resolver_agent_session_id: None,
            ready: true,
            terminal_modes: Vec::new(),
            moved_from_guardian_id: None,
            rebase_commands_done: None,
            rebase_commands_total: None,
            env_overrides: std::collections::BTreeMap::new(),
            resolved_env: std::collections::BTreeMap::new(),
            inherited_env: std::collections::BTreeMap::new(),
            started_at_ms: None,
            auto_submit_error: None,
        }
    }

    #[test]
    fn stack_base_for_chains_onto_nearest_preceding_alias() {
        let a = test_branch("b-a", 0);
        let b = test_branch("b-b", 1);
        let c = test_branch("b-c", 2);
        let ordered: Vec<&BranchView> = vec![&a, &b, &c];
        let mut aliases = HashMap::new();
        aliases.insert("b-a".to_string(), "alias-a".to_string());
        aliases.insert("b-b".to_string(), "alias-b".to_string());

        assert_eq!(
            stack_base_for(&ordered, &aliases, 0, "main"),
            "main",
            "lowest position with nothing preceding it falls back to the base branch"
        );
        assert_eq!(stack_base_for(&ordered, &aliases, 1, "main"), "alias-a");
        assert_eq!(
            stack_base_for(&ordered, &aliases, 2, "main"),
            "alias-b",
            "must chain onto the immediately preceding branch's alias, not the base branch"
        );
    }

    #[test]
    fn stack_base_for_skips_gaps_without_breaking_the_chain() {
        let a = test_branch("b-a", 0);
        let b = test_branch("b-b", 1);
        let c = test_branch("b-c", 2);
        let ordered: Vec<&BranchView> = vec![&a, &b, &c];
        // b-b (position 1) has no PR of its own -- c should still chain onto
        // a, not fall back to the base branch.
        let mut aliases = HashMap::new();
        aliases.insert("b-a".to_string(), "alias-a".to_string());

        assert_eq!(stack_base_for(&ordered, &aliases, 2, "main"), "alias-a");
    }

    #[test]
    fn stack_base_for_seeded_from_a_prior_call_still_chains() {
        // Reproduces the reported bug: branch 0's PR was submitted in an
        // earlier call (so its alias is already in `alias_by_branch`, not
        // freshly created this call) -- branch 1, submitted alone, must still
        // chain onto it instead of falling back to the base branch.
        let a = test_branch("b-a", 0);
        let b = test_branch("b-b", 1);
        let ordered: Vec<&BranchView> = vec![&a, &b];
        let mut aliases = HashMap::new();
        aliases.insert("b-a".to_string(), "zzzsdfsdf".to_string());

        assert_eq!(
            stack_base_for(&ordered, &aliases, 1, "ral-239-squad-cell-proof"),
            "zzzsdfsdf"
        );
    }

    #[test]
    fn classify_base_drift_in_sync_when_forge_matches_recorded_base() {
        assert_eq!(
            classify_base_drift("main", Some("main"), "main"),
            BaseDriftKind::InSync
        );
        // Even with no last_pushed_base_ref recorded yet (a row from before
        // this poll ever ran), a forge base matching `base_ref` is in sync.
        assert_eq!(
            classify_base_drift("main", None, "main"),
            BaseDriftKind::InSync
        );
    }

    #[test]
    fn classify_base_drift_pending_local_push_when_forge_matches_last_confirmed_push() {
        // `base_ref` already moved on to "b" locally (e.g. a resync just
        // ran), but the forge still reports the last base ralphus itself
        // confirmed pushing ("a") -- the PATCH for the new value hasn't
        // landed yet, or failed. Not a forge-side change.
        assert_eq!(
            classify_base_drift("b", Some("a"), "a"),
            BaseDriftKind::PendingLocalPush
        );
    }

    #[test]
    fn classify_base_drift_drifted_when_forge_disagrees_with_both_baselines() {
        assert_eq!(
            classify_base_drift("a", Some("a"), "c"),
            BaseDriftKind::Drifted
        );
        assert_eq!(classify_base_drift("a", None, "c"), BaseDriftKind::Drifted);
    }

    #[test]
    fn branches_skipped_by_drift_drops_the_intervening_branch() {
        // a -> b -> c; forge retargeted c's PR onto a's alias directly, so b
        // must have left the stack.
        let a = test_branch("b-a", 0);
        let b = test_branch("b-b", 1);
        let c = test_branch("b-c", 2);
        let ordered: Vec<&BranchView> = vec![&a, &b, &c];
        let mut aliases = HashMap::new();
        aliases.insert("b-a".to_string(), "alias-a".to_string());
        aliases.insert("b-b".to_string(), "alias-b".to_string());

        let skipped = branches_skipped_by_drift(&ordered, &aliases, 2, "main", "alias-a").unwrap();
        assert_eq!(
            skipped.iter().map(|b| b.id.as_str()).collect::<Vec<_>>(),
            ["b-b"]
        );
    }

    #[test]
    fn branches_skipped_by_drift_drops_every_preceding_branch_when_forge_base_is_the_review_base() {
        let a = test_branch("b-a", 0);
        let b = test_branch("b-b", 1);
        let ordered: Vec<&BranchView> = vec![&a, &b];
        let mut aliases = HashMap::new();
        aliases.insert("b-a".to_string(), "alias-a".to_string());

        let skipped = branches_skipped_by_drift(&ordered, &aliases, 1, "main", "main").unwrap();
        assert_eq!(
            skipped.iter().map(|b| b.id.as_str()).collect::<Vec<_>>(),
            ["b-a"]
        );
    }

    #[test]
    fn branches_skipped_by_drift_is_empty_when_forge_base_already_names_the_immediate_predecessor()
    {
        let a = test_branch("b-a", 0);
        let b = test_branch("b-b", 1);
        let ordered: Vec<&BranchView> = vec![&a, &b];
        let mut aliases = HashMap::new();
        aliases.insert("b-a".to_string(), "alias-a".to_string());

        let skipped = branches_skipped_by_drift(&ordered, &aliases, 1, "main", "alias-a").unwrap();
        assert!(skipped.is_empty());
    }

    #[test]
    fn branches_skipped_by_drift_is_none_for_an_unrecognized_forge_base() {
        let a = test_branch("b-a", 0);
        let b = test_branch("b-b", 1);
        let ordered: Vec<&BranchView> = vec![&a, &b];
        let mut aliases = HashMap::new();
        aliases.insert("b-a".to_string(), "alias-a".to_string());

        assert!(
            branches_skipped_by_drift(&ordered, &aliases, 1, "main", "some-other-branch").is_none()
        );
    }

    #[test]
    fn decide_stack_action_creates_once_two_prs_exist() {
        let prs = vec![("b-a".to_string(), 0, 3), ("b-b".to_string(), 1, 6)];
        let new_ids: std::collections::HashSet<String> = ["b-b".to_string()].into_iter().collect();
        assert_eq!(
            decide_stack_action(&prs, &new_ids, None),
            StackAction::Create {
                all_ordered: vec![3, 6]
            }
        );
    }

    #[test]
    fn decide_stack_action_skips_a_single_pr() {
        let prs = vec![("b-a".to_string(), 0, 3)];
        let new_ids: std::collections::HashSet<String> = ["b-a".to_string()].into_iter().collect();
        assert_eq!(
            decide_stack_action(&prs, &new_ids, None),
            StackAction::Skip {
                reason: "fewer than 2 PRs in the stack"
            }
        );
    }

    #[test]
    fn decide_stack_action_appends_new_prs_on_top_of_a_recorded_stack() {
        let prs = vec![
            ("b-a".to_string(), 0, 3),
            ("b-b".to_string(), 1, 6),
            ("b-c".to_string(), 2, 9),
        ];
        let new_ids: std::collections::HashSet<String> = ["b-c".to_string()].into_iter().collect();
        assert_eq!(
            decide_stack_action(&prs, &new_ids, Some(42)),
            StackAction::Append {
                stack_number: 42,
                new_ordered: vec![9]
            }
        );
    }

    #[test]
    fn decide_stack_action_skips_when_new_prs_do_not_extend_the_top() {
        // b-b was filled in as a gap below the recorded stack's prior top
        // (b-c, position 2) -- appending it wouldn't chain onto the stack's
        // actual top, so this must be skipped rather than sent to the forge.
        let prs = vec![
            ("b-a".to_string(), 0, 3),
            ("b-b".to_string(), 1, 6),
            ("b-c".to_string(), 2, 9),
        ];
        let new_ids: std::collections::HashSet<String> = ["b-b".to_string()].into_iter().collect();
        assert_eq!(
            decide_stack_action(&prs, &new_ids, Some(42)),
            StackAction::Skip {
                reason: "new PRs do not strictly extend the recorded stack's top"
            }
        );
    }

    #[test]
    fn decide_stack_action_skips_when_nothing_new() {
        let prs = vec![("b-a".to_string(), 0, 3), ("b-b".to_string(), 1, 6)];
        assert_eq!(
            decide_stack_action(&prs, &std::collections::HashSet::new(), Some(42)),
            StackAction::Skip {
                reason: "no new PRs to register"
            }
        );
    }

    #[test]
    fn guard_against_clobber_allows_first_push_and_blocks_divergence() {
        let root = tmp_dir("guard-root");
        g(&root, &["init", "--initial-branch", "main"]);
        gwrite(&root, "base.txt", "base\n");
        g(&root, &["add", "."]);
        g(&root, &["commit", "--message", "base"]);

        let remote_dir = tmp_dir("guard-remote");
        g(&remote_dir, &["init", "--bare"]);
        let remote = remote_dir.to_str().unwrap();

        // No remote branch yet -- safe.
        assert!(guard_against_clobber(&root, remote, "pr-x", "main", None).is_ok());

        g(&root, &["push", remote, "main:refs/heads/pr-x"]);
        // Remote now matches local exactly -- still safe (ancestor of itself).
        assert!(guard_against_clobber(&root, remote, "pr-x", "main", None).is_ok());

        // A reviewer pushes a unique commit straight to the PR branch.
        let clone_dir = tmp_dir("guard-clone");
        let _ = std::fs::remove_dir_all(&clone_dir);
        g(
            clone_dir.parent().unwrap(),
            &[
                "clone",
                remote,
                clone_dir.file_name().unwrap().to_str().unwrap(),
            ],
        );
        g(&clone_dir, &["checkout", "pr-x"]);
        gwrite(&clone_dir, "reviewer.txt", "fix\n");
        g(&clone_dir, &["add", "."]);
        g(&clone_dir, &["commit", "--message", "reviewer fix"]);
        g(&clone_dir, &["push", "origin", "pr-x"]);

        // Local `main` no longer contains the remote's unique commit -- blocked.
        let err = guard_against_clobber(&root, remote, "pr-x", "main", None).unwrap_err();
        assert!(err.contains("pr-x"), "{err}");

        // Once local has pulled that commit in, it's a safe superset again.
        g(&root, &["fetch", remote, "pr-x"]);
        g(&root, &["merge", "--ff-only", "FETCH_HEAD"]);
        assert!(guard_against_clobber(&root, remote, "pr-x", "main", None).is_ok());

        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&remote_dir);
        let _ = std::fs::remove_dir_all(&clone_dir);
    }

    /// A restack leaves the remote tip un-ancestored even though nobody else
    /// touched it, so ancestry alone would refuse every post-restack push.
    /// Recognizing the tip this daemon itself last pushed is what separates
    /// "our own history, rewritten" from "someone else's work".
    #[test]
    fn guard_against_clobber_allows_overwriting_the_tip_it_last_pushed() {
        let root = tmp_dir("guard-lastpushed-root");
        g(&root, &["init", "--initial-branch", "main"]);
        gwrite(&root, "base.txt", "base\n");
        g(&root, &["add", "."]);
        g(&root, &["commit", "--message", "base"]);
        g(&root, &["checkout", "-b", "feature"]);
        gwrite(&root, "feat.txt", "feat\n");
        g(&root, &["add", "."]);
        g(&root, &["commit", "--message", "feat"]);

        let remote_dir = tmp_dir("guard-lastpushed-remote");
        g(&remote_dir, &["init", "--bare"]);
        let remote = remote_dir.to_str().unwrap();
        g(&root, &["push", remote, "feature:refs/heads/pr-x"]);
        let pushed = g(&root, &["rev-parse", "feature"]).trim().to_string();

        // Amending rewrites the SHA exactly the way replaying it would.
        gwrite(&root, "feat.txt", "feat rewritten\n");
        g(&root, &["add", "."]);
        g(&root, &["commit", "--amend", "--message", "feat rewritten"]);
        assert_ne!(g(&root, &["rev-parse", "feature"]).trim(), pushed);

        // Ancestry says divergence; the recorded tip says it is ours to replace.
        assert!(guard_against_clobber(&root, remote, "pr-x", "feature", None).is_err());
        assert!(
            guard_against_clobber(&root, remote, "pr-x", "feature", Some(&pushed)).is_ok(),
            "a remote still sitting on our own last push is safe to overwrite"
        );

        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&remote_dir);
    }

    /// Without a recorded tip to match (a PR adopted from an earlier push, say)
    /// patch ids still tell a replayed commit apart from a reviewer's own.
    #[test]
    fn guard_against_clobber_allows_a_remote_whose_commits_were_all_replayed() {
        let root = tmp_dir("guard-replay-root");
        g(&root, &["init", "--initial-branch", "main"]);
        gwrite(&root, "base.txt", "base\n");
        g(&root, &["add", "."]);
        g(&root, &["commit", "--message", "base"]);
        g(&root, &["checkout", "-b", "feature"]);
        gwrite(&root, "feat.txt", "feat\n");
        g(&root, &["add", "."]);
        g(&root, &["commit", "--message", "feat"]);

        let remote_dir = tmp_dir("guard-replay-remote");
        g(&remote_dir, &["init", "--bare"]);
        let remote = remote_dir.to_str().unwrap();
        g(&root, &["push", remote, "feature:refs/heads/pr-x"]);

        // Advance the base and replay `feature` onto it: same patch, new SHA.
        g(&root, &["checkout", "main"]);
        gwrite(&root, "other.txt", "other\n");
        g(&root, &["add", "."]);
        g(&root, &["commit", "--message", "base advances"]);
        g(&root, &["checkout", "feature"]);
        g(&root, &["rebase", "main"]);

        assert!(
            guard_against_clobber(&root, remote, "pr-x", "feature", None).is_ok(),
            "every remote commit was replayed into feature under a new sha"
        );

        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&remote_dir);
    }

    /// Common fixture for the `compute_sync_status` tests: a repo with a
    /// `review-branch` pushed to a bare remote as `pr-y`, and a guardian/PR
    /// row pointing at it.
    fn sync_status_fixture() -> (PathBuf, PathBuf, Arc<Mutex<Store>>, String) {
        let (root, remote_dir, store, pr_id, _review_sha) = review_fixture(
            "sync",
            |root| gwrite(root, "base.txt", "base\n"),
            |root| gwrite(root, "feat.txt", "feat\n"),
        );
        (root, remote_dir, store, pr_id)
    }

    /// [`sync_status_fixture`], plus recording `last_pushed_sha` the way a
    /// real push through this daemon would -- most RAL-190 rebase-drift
    /// tests need a real fork point on record, not just matching SHAs.
    fn synced_fixture() -> (PathBuf, PathBuf, Arc<Mutex<Store>>, String) {
        let (root, remote_dir, store, pr_id) = sync_status_fixture();
        let sha = git2::Repository::open(&root)
            .unwrap()
            .revparse_single("review-branch")
            .unwrap()
            .id()
            .to_string();
        {
            let guard = store.lock().unwrap();
            guard
                .update_pull_request_ex(
                    &pr_id,
                    None,
                    None,
                    None,
                    None,
                    None,
                    Some(Some(&sha)),
                    None,
                )
                .unwrap();
            // A review with an open PR has settled; `sync_open_pr_branches`
            // only reconciles one that has.
            let gid = guard.get_pull_request(&pr_id).unwrap().guardian_id;
            guard
                .set_guardian_status(&gid, GuardianStatus::InReview, None)
                .unwrap();
        }
        (root, remote_dir, store, pr_id)
    }

    /// Like [`synced_fixture`], but `review-branch`'s one commit edits the
    /// same line of a shared file that a base-advance commit can also touch
    /// -- so a caller can force a real rebase conflict on demand, on either
    /// the worktree or the PR side, via [`rebase_with_conflict`].
    fn conflict_fixture() -> (PathBuf, PathBuf, Arc<Mutex<Store>>, String) {
        let (root, remote_dir, store, pr_id, sha) = review_fixture(
            "sync-conflict",
            |root| gwrite(root, "shared.txt", "line1\nline2\nline3\n"),
            |root| gwrite(root, "shared.txt", "line1\nline2-review\nline3\n"),
        );
        store
            .lock()
            .unwrap()
            .update_pull_request_ex(&pr_id, None, None, None, None, None, Some(Some(&sha)), None)
            .unwrap();
        (root, remote_dir, store, pr_id)
    }

    /// Rebase `target_branch` (as set up by [`conflict_fixture`]: one commit
    /// on top of a base, editing `shared.txt`'s line 2) onto a freshly
    /// diverged sibling of its own base commit, so the rebase conflicts on
    /// that same line. `use_rerere` picks between the two ways RAL-190's
    /// sync classification is supposed to be indifferent to: resolve the
    /// conflict by hand once and replay the identical rebase a second time
    /// to prove `git rerere` auto-stages the recorded resolution (no manual
    /// edit on the second pass), or resolve it by hand a single time --
    /// mirroring "rerere auto-patched it" vs "had to fix it myself".
    fn rebase_with_conflict(dir: &Path, target_branch: &str, use_rerere: bool) {
        let new_base = format!("{target_branch}-newbase");
        {
            let repo = git2::Repository::open(dir).unwrap();
            let sig = git2::Signature::now("t", "t@t").unwrap();
            let target_commit = repo
                .revparse_single(target_branch)
                .unwrap()
                .peel_to_commit()
                .unwrap();
            let parent_commit = target_commit.parent(0).unwrap();
            repo.branch(&new_base, &parent_commit, false).unwrap();
            git2_checkout(&repo, &new_base);
            gwrite(dir, "shared.txt", "line1\nline2-newbase\nline3\n");
            git2_commit_all(&repo, &sig, "advance base (conflicting)", &[&parent_commit]);
            git2_checkout(&repo, target_branch);
        }

        let run_rebase = |d: &Path| -> std::process::Output {
            Command::new("git")
                .args(["rebase", &new_base])
                .current_dir(d)
                .output()
                .unwrap()
        };

        if use_rerere {
            let pre_rebase_oid = {
                let repo = git2::Repository::open(dir).unwrap();
                let mut config = repo.config().unwrap();
                config.set_bool("rerere.enabled", true).unwrap();
                config.set_bool("rerere.autoupdate", true).unwrap();
                repo.revparse_single(target_branch).unwrap().id()
            };

            // First pass: hit the conflict and resolve it by hand -- this
            // teaches rerere the resolution.
            let out = run_rebase(dir);
            assert!(!out.status.success(), "expected a rebase conflict");
            gwrite(dir, "shared.txt", "line1\nline2-resolved\nline3\n");
            g(dir, &["add", "."]);
            g(dir, &["rebase", "--continue"]);

            // Reset target_branch back to its pre-rebase tip and redo the
            // identical rebase -- this time rerere should recognize the
            // recorded resolution and stage it automatically, with no
            // manual edit on our part.
            {
                // Already on `target_branch` at this point (the first
                // `rebase --continue` reattached HEAD there), so this is a
                // `git reset --hard <sha>`, not a branch-ref move -- git2
                // refuses to force-move a branch that is the current HEAD.
                let repo = git2::Repository::open(dir).unwrap();
                let commit = repo.find_commit(pre_rebase_oid).unwrap();
                repo.reset(commit.as_object(), git2::ResetType::Hard, None)
                    .unwrap();
            }
            let out2 = run_rebase(dir);
            assert!(!out2.status.success(), "expected the conflict to recur");
            let out3 = Command::new("git")
                .args(["rebase", "--continue"])
                .current_dir(dir)
                .env("GIT_AUTHOR_NAME", "t")
                .env("GIT_AUTHOR_EMAIL", "t@t")
                .env("GIT_COMMITTER_NAME", "t")
                .env("GIT_COMMITTER_EMAIL", "t@t")
                .env("GIT_EDITOR", "true")
                .env("GIT_SEQUENCE_EDITOR", "true")
                .output()
                .unwrap();
            assert!(
                out3.status.success(),
                "rerere should have auto-resolved and staged the conflict: {}",
                String::from_utf8_lossy(&out3.stderr)
            );
        } else {
            let out = run_rebase(dir);
            assert!(!out.status.success(), "expected a rebase conflict");
            gwrite(dir, "shared.txt", "line1\nline2-resolved\nline3\n");
            g(dir, &["add", "."]);
            g(dir, &["rebase", "--continue"]);
        }

        git2::Repository::open(dir)
            .unwrap()
            .find_branch(&new_base, git2::BranchType::Local)
            .unwrap()
            .delete()
            .unwrap();
    }

    #[test]
    fn compute_sync_status_reports_in_sync_right_after_push() {
        let (root, remote_dir, store, pr_id) = sync_status_fixture();
        let status = compute_sync_status(&store, &pr_id).unwrap();
        assert!(status.in_sync, "{status:?}");
        assert!(!status.pr_ahead && !status.worktree_ahead, "{status:?}");
        assert!(status.remote_sha.is_some() && status.remote_sha == status.local_sha);
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&remote_dir);
    }

    #[test]
    fn compute_sync_status_detects_pr_ahead_after_reviewer_push() {
        let (root, remote_dir, store, pr_id) = sync_status_fixture();
        let remote = remote_dir.to_str().unwrap();

        let clone_dir = tmp_dir("sync-clone");
        let _ = std::fs::remove_dir_all(&clone_dir);
        g(
            clone_dir.parent().unwrap(),
            &[
                "clone",
                remote,
                clone_dir.file_name().unwrap().to_str().unwrap(),
            ],
        );
        g(&clone_dir, &["checkout", "pr-y"]);
        gwrite(&clone_dir, "reviewer.txt", "fix\n");
        g(&clone_dir, &["add", "."]);
        g(&clone_dir, &["commit", "--message", "reviewer fix"]);
        g(&clone_dir, &["push", "origin", "pr-y"]);

        let status = compute_sync_status(&store, &pr_id).unwrap();
        assert!(status.pr_ahead, "{status:?}");
        assert!(!status.worktree_ahead, "{status:?}");
        assert!(!status.in_sync, "{status:?}");

        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&remote_dir);
        let _ = std::fs::remove_dir_all(&clone_dir);
    }

    #[test]
    fn compute_sync_status_detects_worktree_ahead_after_local_commit() {
        let (root, remote_dir, store, pr_id) = sync_status_fixture();
        g(&root, &["checkout", "review-branch"]);
        gwrite(&root, "more.txt", "more\n");
        g(&root, &["add", "."]);
        g(&root, &["commit", "--message", "more work"]);
        g(&root, &["checkout", "main"]);

        let status = compute_sync_status(&store, &pr_id).unwrap();
        assert!(status.worktree_ahead, "{status:?}");
        assert!(!status.pr_ahead, "{status:?}");
        assert!(!status.in_sync, "{status:?}");

        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&remote_dir);
    }

    // -- RAL-190 rebase-drift regressions -----------------------------------
    //
    // Ancestry alone (`git merge-base --is-ancestor`) can't survive a
    // rebase: it rewrites every replayed commit's SHA, so neither branch's
    // tip stays an ancestor of the other even when nothing genuinely
    // diverged. These pin `compute_sync_status`'s `last_pushed_sha`-based
    // classification against every combination of {which side rebased} x
    // {no conflict, conflict auto-resolved by rerere, conflict resolved by
    // hand}, plus the true-divergence and remote-deleted edge cases that
    // fall back to (or bypass) that classification.

    #[test]
    fn compute_sync_status_worktree_rebase_no_conflict_is_worktree_ahead() {
        let (root, remote_dir, store, pr_id) = synced_fixture();

        // Advance `main` with an unrelated file -- rebasing `review-branch`
        // onto it just winds forward through history, no conflict.
        g(&root, &["checkout", "main"]);
        gwrite(&root, "unrelated.txt", "unrelated\n");
        g(&root, &["add", "."]);
        g(&root, &["commit", "--message", "advance base"]);
        g(&root, &["checkout", "review-branch"]);
        g(&root, &["rebase", "main"]);
        g(&root, &["checkout", "main"]);

        let status = compute_sync_status(&store, &pr_id).unwrap();
        assert!(status.worktree_ahead, "{status:?}");
        assert!(!status.pr_ahead, "{status:?}");
        assert!(!status.in_sync, "{status:?}");

        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&remote_dir);
    }

    #[test]
    fn compute_sync_status_pr_rebase_no_conflict_is_pr_ahead() {
        let (root, remote_dir, store, pr_id) = synced_fixture();
        let remote = remote_dir.to_str().unwrap();

        let clone_dir = tmp_dir("sync-rebase-clone");
        g(
            clone_dir.parent().unwrap(),
            &[
                "clone",
                remote,
                clone_dir.file_name().unwrap().to_str().unwrap(),
            ],
        );
        g(&clone_dir, &["checkout", "pr-y"]);

        // Rebase pr-y onto a fresh sibling of its own base -- an external
        // rewrite of the PR branch's history that winds through cleanly.
        g(&clone_dir, &["branch", "newbase", "pr-y~1"]);
        g(&clone_dir, &["checkout", "newbase"]);
        gwrite(&clone_dir, "external.txt", "external\n");
        g(&clone_dir, &["add", "."]);
        g(
            &clone_dir,
            &["commit", "--message", "advance base externally"],
        );
        g(&clone_dir, &["checkout", "pr-y"]);
        g(&clone_dir, &["rebase", "newbase"]);
        g(&clone_dir, &["push", "--force", "origin", "pr-y"]);

        let status = compute_sync_status(&store, &pr_id).unwrap();
        assert!(status.pr_ahead, "{status:?}");
        assert!(!status.worktree_ahead, "{status:?}");
        assert!(!status.in_sync, "{status:?}");

        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&remote_dir);
        let _ = std::fs::remove_dir_all(&clone_dir);
    }

    #[test]
    fn compute_sync_status_worktree_rebase_conflict_rerere_is_worktree_ahead() {
        let (root, remote_dir, store, pr_id) = conflict_fixture();
        rebase_with_conflict(&root, "review-branch", true);
        g(&root, &["checkout", "main"]);

        let status = compute_sync_status(&store, &pr_id).unwrap();
        assert!(status.worktree_ahead, "{status:?}");
        assert!(!status.pr_ahead, "{status:?}");
        assert!(!status.in_sync, "{status:?}");

        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&remote_dir);
    }

    #[test]
    fn compute_sync_status_pr_rebase_conflict_rerere_is_pr_ahead() {
        let (root, remote_dir, store, pr_id) = conflict_fixture();
        let remote = remote_dir.to_str().unwrap();

        let clone_dir = tmp_dir("sync-conflict-rerere-clone");
        g(
            clone_dir.parent().unwrap(),
            &[
                "clone",
                remote,
                clone_dir.file_name().unwrap().to_str().unwrap(),
            ],
        );
        g(&clone_dir, &["checkout", "pr-y"]);
        rebase_with_conflict(&clone_dir, "pr-y", true);
        g(&clone_dir, &["push", "--force", "origin", "pr-y"]);

        let status = compute_sync_status(&store, &pr_id).unwrap();
        assert!(status.pr_ahead, "{status:?}");
        assert!(!status.worktree_ahead, "{status:?}");
        assert!(!status.in_sync, "{status:?}");

        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&remote_dir);
        let _ = std::fs::remove_dir_all(&clone_dir);
    }

    #[test]
    fn compute_sync_status_worktree_rebase_conflict_manual_is_worktree_ahead() {
        let (root, remote_dir, store, pr_id) = conflict_fixture();
        rebase_with_conflict(&root, "review-branch", false);
        g(&root, &["checkout", "main"]);

        let status = compute_sync_status(&store, &pr_id).unwrap();
        assert!(status.worktree_ahead, "{status:?}");
        assert!(!status.pr_ahead, "{status:?}");
        assert!(!status.in_sync, "{status:?}");

        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&remote_dir);
    }

    #[test]
    fn compute_sync_status_pr_rebase_conflict_manual_is_pr_ahead() {
        let (root, remote_dir, store, pr_id) = conflict_fixture();
        let remote = remote_dir.to_str().unwrap();

        let clone_dir = tmp_dir("sync-conflict-manual-clone");
        g(
            clone_dir.parent().unwrap(),
            &[
                "clone",
                remote,
                clone_dir.file_name().unwrap().to_str().unwrap(),
            ],
        );
        g(&clone_dir, &["checkout", "pr-y"]);
        rebase_with_conflict(&clone_dir, "pr-y", false);
        g(&clone_dir, &["push", "--force", "origin", "pr-y"]);

        let status = compute_sync_status(&store, &pr_id).unwrap();
        assert!(status.pr_ahead, "{status:?}");
        assert!(!status.worktree_ahead, "{status:?}");
        assert!(!status.in_sync, "{status:?}");

        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&remote_dir);
        let _ = std::fs::remove_dir_all(&clone_dir);
    }

    #[test]
    fn compute_sync_status_true_divergence_marks_both_ahead() {
        let (root, remote_dir, store, pr_id) = synced_fixture();
        let remote = remote_dir.to_str().unwrap();

        // Worktree gains a commit that's never pushed...
        g(&root, &["checkout", "review-branch"]);
        gwrite(&root, "local-only.txt", "local\n");
        g(&root, &["add", "."]);
        g(&root, &["commit", "--message", "local work"]);
        g(&root, &["checkout", "main"]);

        // ...while the PR branch independently gains a different commit,
        // pushed straight to the remote with no knowledge of the local one.
        let clone_dir = tmp_dir("sync-divergence-clone");
        g(
            clone_dir.parent().unwrap(),
            &[
                "clone",
                remote,
                clone_dir.file_name().unwrap().to_str().unwrap(),
            ],
        );
        g(&clone_dir, &["checkout", "pr-y"]);
        gwrite(&clone_dir, "reviewer-only.txt", "reviewer\n");
        g(&clone_dir, &["add", "."]);
        g(&clone_dir, &["commit", "--message", "reviewer work"]);
        g(&clone_dir, &["push", "origin", "pr-y"]);

        let status = compute_sync_status(&store, &pr_id).unwrap();
        assert!(status.worktree_ahead, "{status:?}");
        assert!(status.pr_ahead, "{status:?}");
        assert!(!status.in_sync, "{status:?}");

        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&remote_dir);
        let _ = std::fs::remove_dir_all(&clone_dir);
    }

    #[test]
    fn compute_sync_status_reports_worktree_ahead_when_remote_branch_deleted() {
        let (root, remote_dir, store, pr_id) = synced_fixture();
        let remote = remote_dir.to_str().unwrap();
        g(&root, &["push", remote, "--delete", "pr-y"]);

        let status = compute_sync_status(&store, &pr_id).unwrap();
        assert!(status.remote_sha.is_none(), "{status:?}");
        assert!(status.worktree_ahead, "{status:?}");
        assert!(!status.pr_ahead, "{status:?}");
        assert!(!status.in_sync, "{status:?}");

        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&remote_dir);
    }

    // -- RAL-285: repoint stacked PRs when the review's base branch moves ------

    #[test]
    fn is_stack_base_restriction_matches_only_githubs_stack_refusal() {
        assert!(is_stack_base_restriction(
            "forge API 422: {\"message\":\"Validation Failed\",\"errors\":[{\"message\":\
             \"Cannot change the base branch because the pull request is part of a stack.\"}]}"
        ));
        // Anything dissolving the stack would not fix must not dissolve it.
        assert!(!is_stack_base_restriction("forge API 401: Bad credentials"));
        assert!(!is_stack_base_restriction(
            "forge API 422: base branch not found"
        ));
        // GitLab has no stacks to dissolve, so none of its rejections may
        // route into the GitHub recovery path.
        assert!(!is_stack_base_restriction(
            "forge API 400: {\"message\":{\"target_branch\":[\"can't be blank\"]}}"
        ));
        assert!(!is_stack_base_restriction(
            "forge API 409: merge request is part of a merge train"
        ));
    }

    /// The PRs must survive with their numbers intact: the recovery dissolves
    /// the stack, retries the base change, and registers the stack again --
    /// it never closes or recreates a pull request.
    #[test]
    fn repoint_stacked_prs_dissolves_retries_the_base_then_re_registers() {
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let addr = server.server_addr().to_string();
        let handle = std::thread::spawn(move || {
            let mut seen: Vec<(String, String)> = Vec::new();
            let mut created_payload = serde_json::Value::Null;
            for _ in 0..3 {
                let mut req = server.recv().unwrap();
                let method = req.method().as_str().to_string();
                let url = req.url().to_string();
                let mut body = String::new();
                req.as_reader().read_to_string(&mut body).unwrap();
                if url == "/repos/acme/widget/stacks" {
                    created_payload = serde_json::from_str(&body).unwrap();
                }
                seen.push((method, url));
                req.respond(
                    tiny_http::Response::from_string("{\"number\": 99}").with_status_code(200),
                )
                .unwrap();
            }
            (seen, created_payload)
        });

        let s = store();
        let gid = s.create_guardian("demo", "main", "/tmp/root").unwrap();
        s.set_guardian_forge_stack_number(&gid, 42).unwrap();
        let store = Arc::new(Mutex::new(s));
        let client = crate::forge::ForgeClient::new(
            crate::forge::ForgeKind::GitHub,
            format!("http://{addr}"),
            "acme/widget".to_string(),
            Some("tok".to_string()),
        );

        repoint_stacked_prs(
            &store,
            &gid,
            &client,
            &[7, 8],
            &[("pr-x".to_string(), 7, "new-base".to_string())],
        )
        .unwrap();

        let (seen, created_payload) = handle.join().unwrap();
        assert_eq!(
            seen,
            vec![
                (
                    "POST".to_string(),
                    "/repos/acme/widget/stacks/42/unstack".to_string()
                ),
                (
                    "PATCH".to_string(),
                    "/repos/acme/widget/pulls/7".to_string()
                ),
                ("POST".to_string(), "/repos/acme/widget/stacks".to_string()),
            ],
            "must dissolve, then move the base, then rebuild the stack -- in that order"
        );
        assert_eq!(created_payload["pull_requests"], serde_json::json!([7, 8]));
        assert_eq!(
            store
                .lock()
                .unwrap()
                .get_guardian_forge_stack_number(&gid)
                .unwrap(),
            Some(99),
            "the rebuilt stack's number must replace the dissolved one"
        );
    }

    #[test]
    fn repoint_stacked_prs_does_nothing_without_a_recorded_stack() {
        let s = store();
        let gid = s.create_guardian("demo", "main", "/tmp/root").unwrap();
        let store = Arc::new(Mutex::new(s));
        // An unreachable base URL: reaching the forge at all would be a bug,
        // since there is no recorded stack to dissolve.
        let client = crate::forge::ForgeClient::new(
            crate::forge::ForgeKind::GitHub,
            "http://127.0.0.1:1".to_string(),
            "acme/widget".to_string(),
            Some("tok".to_string()),
        );

        let result = repoint_stacked_prs(
            &store,
            &gid,
            &client,
            &[7, 8],
            &[("pr-x".to_string(), 7, "new-base".to_string())],
        );
        assert!(result.is_err());

        assert_eq!(
            store
                .lock()
                .unwrap()
                .get_guardian_forge_stack_number(&gid)
                .unwrap(),
            None
        );
    }

    // -- RAL-285: keep already-open PR branches level with their review branch --

    #[test]
    fn sync_open_pr_branches_pushes_a_worktree_tip_that_moved_locally() {
        let (root, remote_dir, store, pr_id) = synced_fixture();
        let gid = store
            .lock()
            .unwrap()
            .get_pull_request(&pr_id)
            .unwrap()
            .guardian_id;

        // The review-branch worktree tip gains a commit in place (e.g. a
        // conflict resolution), with nothing pushed to the remote yet.
        g(&root, &["checkout", "review-branch"]);
        gwrite(&root, "resolved.txt", "resolved\n");
        g(&root, &["add", "."]);
        g(&root, &["commit", "--message", "conflict resolution"]);
        g(&root, &["checkout", "main"]);
        let new_local_sha = g(&root, &["rev-parse", "review-branch"]).trim().to_string();

        sync_open_pr_branches(&store, &gid);

        let pr = store.lock().unwrap().get_pull_request(&pr_id).unwrap();
        assert_eq!(pr.last_pushed_sha.as_deref(), Some(new_local_sha.as_str()));
        let remote_sha = g(&remote_dir, &["rev-parse", "pr-y"]).trim().to_string();
        assert_eq!(remote_sha, new_local_sha);

        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&remote_dir);
    }

    /// The case the whole feature exists for, and the one a tip that merely
    /// moves forward never exercises: a restack REPLAYS the branch, so the
    /// remote tip stops being an ancestor of the local one and a guard that
    /// only knows ancestry refuses the push that would refresh the PR.
    #[test]
    fn sync_open_pr_branches_pushes_a_branch_the_base_shift_restacked() {
        let (root, remote_dir, store, pr_id) = synced_fixture();
        let gid = store
            .lock()
            .unwrap()
            .get_pull_request(&pr_id)
            .unwrap()
            .guardian_id;
        let pre_restack = g(&root, &["rev-parse", "review-branch"]).trim().to_string();

        // The base branch moves, then the review branch is replayed onto it --
        // exactly `rebuild_on_base_shift`'s restack, and equally what an
        // explicit `review merge` leaves behind.
        g(&root, &["checkout", "main"]);
        gwrite(&root, "base-advance.txt", "moved\n");
        g(&root, &["add", "."]);
        g(&root, &["commit", "--message", "base advances"]);
        g(&root, &["checkout", "review-branch"]);
        g(&root, &["rebase", "main"]);
        g(&root, &["checkout", "main"]);

        let restacked = g(&root, &["rev-parse", "review-branch"]).trim().to_string();
        assert_ne!(restacked, pre_restack, "rebase should rewrite the tip");
        assert!(
            Command::new("git")
                .current_dir(&root)
                .args(["merge-base", "--is-ancestor", &pre_restack, &restacked])
                .status()
                .is_ok_and(|s| !s.success()),
            "the replayed tip must not descend from the pushed one, or this \
             test is not covering the restack case"
        );

        sync_open_pr_branches(&store, &gid);

        let pr = store.lock().unwrap().get_pull_request(&pr_id).unwrap();
        assert_eq!(pr.last_pushed_sha.as_deref(), Some(restacked.as_str()));
        let remote_sha = g(&remote_dir, &["rev-parse", "pr-y"]).trim().to_string();
        assert_eq!(remote_sha, restacked);

        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&remote_dir);
    }

    #[test]
    fn sync_open_pr_branches_leaves_a_review_that_is_still_merging_alone() {
        let (root, remote_dir, store, pr_id) = synced_fixture();
        let gid = store
            .lock()
            .unwrap()
            .get_pull_request(&pr_id)
            .unwrap()
            .guardian_id;
        store
            .lock()
            .unwrap()
            .set_guardian_status(&gid, GuardianStatus::Merging, None)
            .unwrap();
        let remote_before = g(&remote_dir, &["rev-parse", "pr-y"]).trim().to_string();

        // A merge in flight owns the branch tips, so a tip that has moved
        // mid-merge is not yet the state the PR should show.
        g(&root, &["checkout", "review-branch"]);
        gwrite(&root, "half-done.txt", "wip\n");
        g(&root, &["add", "."]);
        g(&root, &["commit", "--message", "mid-merge"]);
        g(&root, &["checkout", "main"]);

        sync_open_pr_branches(&store, &gid);

        let remote_after = g(&remote_dir, &["rev-parse", "pr-y"]).trim().to_string();
        assert_eq!(remote_after, remote_before);

        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&remote_dir);
    }

    #[test]
    fn sync_open_pr_branches_is_a_noop_when_nothing_changed() {
        let (root, remote_dir, store, pr_id) = synced_fixture();
        let gid = store
            .lock()
            .unwrap()
            .get_pull_request(&pr_id)
            .unwrap()
            .guardian_id;
        let remote_sha_before = g(&remote_dir, &["rev-parse", "pr-y"]).trim().to_string();

        sync_open_pr_branches(&store, &gid);

        let pr = store.lock().unwrap().get_pull_request(&pr_id).unwrap();
        assert_eq!(
            pr.last_pushed_sha.as_deref(),
            Some(remote_sha_before.as_str())
        );
        let remote_sha_after = g(&remote_dir, &["rev-parse", "pr-y"]).trim().to_string();
        assert_eq!(remote_sha_after, remote_sha_before);

        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&remote_dir);
    }

    #[test]
    fn sync_open_pr_branches_skips_a_pr_a_reviewer_pushed_directly_to() {
        let (root, remote_dir, store, pr_id) = synced_fixture();
        let gid = store
            .lock()
            .unwrap()
            .get_pull_request(&pr_id)
            .unwrap()
            .guardian_id;
        let remote = remote_dir.to_str().unwrap();

        // Reviewer pushes directly to the PR branch -- a commit the worktree
        // doesn't have. The restack also moves the worktree tip locally.
        let clone_dir = tmp_dir("sync-restack-clobber-clone");
        let _ = std::fs::remove_dir_all(&clone_dir);
        g(
            clone_dir.parent().unwrap(),
            &[
                "clone",
                remote,
                clone_dir.file_name().unwrap().to_str().unwrap(),
            ],
        );
        g(&clone_dir, &["checkout", "pr-y"]);
        gwrite(&clone_dir, "reviewer.txt", "fix\n");
        g(&clone_dir, &["add", "."]);
        g(&clone_dir, &["commit", "--message", "reviewer fix"]);
        g(&clone_dir, &["push", "origin", "pr-y"]);
        let remote_sha_before = g(&remote_dir, &["rev-parse", "pr-y"]).trim().to_string();

        g(&root, &["checkout", "review-branch"]);
        gwrite(&root, "resolved.txt", "resolved\n");
        g(&root, &["add", "."]);
        g(&root, &["commit", "--message", "conflict resolution"]);
        g(&root, &["checkout", "main"]);

        sync_open_pr_branches(&store, &gid);

        // Push refused (would clobber the reviewer's commit) -- last_pushed_sha
        // and the remote branch are both left untouched.
        let pr = store.lock().unwrap().get_pull_request(&pr_id).unwrap();
        assert_ne!(
            pr.last_pushed_sha.as_deref(),
            Some(g(&root, &["rev-parse", "review-branch"]).trim())
        );
        let remote_sha_after = g(&remote_dir, &["rev-parse", "pr-y"]).trim().to_string();
        assert_eq!(remote_sha_after, remote_sha_before);

        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&remote_dir);
        let _ = std::fs::remove_dir_all(&clone_dir);
    }

    #[test]
    fn resync_pr_bases_updates_downstream_after_reorder() {
        let store = Arc::new(Mutex::new(store()));
        let gid = store
            .lock()
            .unwrap()
            .create_guardian("demo", "main", "/repo")
            .unwrap();
        for b in ["a", "b", "c"] {
            store.lock().unwrap().add_guardian_branch(&gid, b).unwrap();
        }
        let ids: Vec<String> = store
            .lock()
            .unwrap()
            .get_guardian(&gid)
            .unwrap()
            .branches
            .iter()
            .map(|b| b.id.clone())
            .collect();

        // Original stack order a -> b -> c: each PR's base is the one before it.
        let pr_a = store
            .lock()
            .unwrap()
            .create_pull_request(
                &gid,
                Some(&ids[0]),
                "github",
                "acme/w",
                "a",
                "main",
                "A",
                "",
                None,
                None,
            )
            .unwrap();
        let pr_b = store
            .lock()
            .unwrap()
            .create_pull_request(
                &gid,
                Some(&ids[1]),
                "github",
                "acme/w",
                "b",
                "a",
                "B",
                "",
                None,
                None,
            )
            .unwrap();
        let pr_c = store
            .lock()
            .unwrap()
            .create_pull_request(
                &gid,
                Some(&ids[2]),
                "github",
                "acme/w",
                "c",
                "b",
                "C",
                "",
                None,
                None,
            )
            .unwrap();

        // Reorder to c, a, b.
        store
            .lock()
            .unwrap()
            .reorder_guardian_branches(&gid, &["c".into(), "a".into(), "b".into()])
            .unwrap();

        let changed = resync_pr_bases(&store, &gid).unwrap();
        // c now leads the stack (base -> guardian's own base branch); a now
        // follows c; b is still right after a, so its base is unchanged.
        assert_eq!(changed, 2);
        let s = store.lock().unwrap();
        assert_eq!(s.get_pull_request(&pr_c).unwrap().base_ref, "main");
        assert_eq!(s.get_pull_request(&pr_a).unwrap().base_ref, "c");
        assert_eq!(s.get_pull_request(&pr_b).unwrap().base_ref, "a");
    }

    fn synchronous_multi_branch_base_sync(forge: &str) {
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let addr = server.server_addr().to_string();
        let forge_name = forge.to_string();
        let handle = std::thread::spawn(move || {
            let mut bases = Vec::new();
            for number in 1..=3 {
                let mut req = server.recv().unwrap();
                let expected_method = if forge_name == "github" {
                    tiny_http::Method::Patch
                } else {
                    tiny_http::Method::Put
                };
                assert_eq!(req.method(), &expected_method);
                let expected_path = if forge_name == "github" {
                    format!("/repos/acme/widget/pulls/{number}")
                } else {
                    format!("/projects/acme%2Fwidget/merge_requests/{number}")
                };
                assert_eq!(req.url(), expected_path);
                let mut body = String::new();
                req.as_reader().read_to_string(&mut body).unwrap();
                let payload: serde_json::Value = serde_json::from_str(&body).unwrap();
                bases.push(
                    payload
                        .get(if forge_name == "github" {
                            "base"
                        } else {
                            "target_branch"
                        })
                        .and_then(serde_json::Value::as_str)
                        .unwrap()
                        .to_string(),
                );
                req.respond(tiny_http::Response::from_string("{}").with_status_code(200))
                    .unwrap();
            }
            bases
        });

        let root = tmp_dir(&format!("sync-base-{forge}"));
        g(&root, &["init"]);
        let remote = if forge == "github" {
            "https://github.com/acme/widget.git"
        } else {
            "https://gitlab.com/acme/widget.git"
        };
        g(&root, &["remote", "add", "origin", remote]);
        std::fs::write(
            root.join(".ralphus.toml"),
            format!(
                "[forge]\nkind = \"{forge}\"\napi_base = \"http://{addr}\"\ntoken_env = \"RALPHUS_TEST_FORGE_TOKEN\"\n"
            ),
        )
        .unwrap();
        let store = Arc::new(Mutex::new(store()));
        let gid = store
            .lock()
            .unwrap()
            .create_guardian("demo", "release", root.to_str().unwrap())
            .unwrap();
        for branch in ["a", "b", "c"] {
            store
                .lock()
                .unwrap()
                .add_guardian_branch(&gid, branch)
                .unwrap();
        }
        let ids: Vec<_> = store
            .lock()
            .unwrap()
            .get_guardian(&gid)
            .unwrap()
            .branches
            .into_iter()
            .map(|branch| branch.id)
            .collect();
        for (idx, (alias, old_base)) in [
            ("a-alias", "main"),
            ("b-alias", "a-alias"),
            ("c-alias", "b-alias"),
        ]
        .into_iter()
        .enumerate()
        {
            store
                .lock()
                .unwrap()
                .create_pull_request(
                    &gid,
                    Some(&ids[idx]),
                    forge,
                    "acme/widget",
                    alias,
                    old_base,
                    alias,
                    "",
                    Some(i64::try_from(idx).unwrap() + 1),
                    None,
                )
                .unwrap();
        }

        assert_eq!(resync_pr_bases_synchronously(&store, &gid).unwrap(), 3);
        assert_eq!(
            handle.join().unwrap(),
            vec!["release", "a-alias", "b-alias"]
        );
        let rows = store
            .lock()
            .unwrap()
            .list_pull_requests_for_guardian(&gid)
            .unwrap();
        assert_eq!(
            rows.iter()
                .map(|pr| pr.base_ref.as_str())
                .collect::<Vec<_>>(),
            vec!["release", "a-alias", "b-alias"]
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn synchronous_multi_branch_base_sync_updates_every_github_pr() {
        synchronous_multi_branch_base_sync("github");
    }

    #[test]
    fn synchronous_multi_branch_base_sync_updates_every_gitlab_mr() {
        synchronous_multi_branch_base_sync("gitlab");
    }

    #[test]
    fn reconstruct_forge_chain_walks_bases_in_position_order_when_unchanged() {
        let live_base = HashMap::from([
            ("a".to_string(), "main".to_string()),
            ("b".to_string(), "a-alias".to_string()),
            ("c".to_string(), "b-alias".to_string()),
        ]);
        let alias_by_branch = HashMap::from([
            ("a".to_string(), "a-alias".to_string()),
            ("b".to_string(), "b-alias".to_string()),
            ("c".to_string(), "c-alias".to_string()),
        ]);
        assert_eq!(
            reconstruct_forge_chain("main", &live_base, &alias_by_branch),
            Some(vec!["a".to_string(), "b".to_string(), "c".to_string()])
        );
    }

    #[test]
    fn reconstruct_forge_chain_detects_a_stack_reordered_on_the_forge() {
        // c now leads (bases straight on main); a follows c; b still follows a.
        let live_base = HashMap::from([
            ("c".to_string(), "main".to_string()),
            ("a".to_string(), "c-alias".to_string()),
            ("b".to_string(), "a-alias".to_string()),
        ]);
        let alias_by_branch = HashMap::from([
            ("a".to_string(), "a-alias".to_string()),
            ("b".to_string(), "b-alias".to_string()),
            ("c".to_string(), "c-alias".to_string()),
        ]);
        assert_eq!(
            reconstruct_forge_chain("main", &live_base, &alias_by_branch),
            Some(vec!["c".to_string(), "a".to_string(), "b".to_string()])
        );
    }

    #[test]
    fn reconstruct_forge_stack_detects_a_new_base_for_a_multi_pr_chain() {
        let live = HashMap::from([
            (
                "a".to_string(),
                crate::forge::PullRequestBaseState {
                    base: "release".to_string(),
                    updated_at_ms: 9_000,
                },
            ),
            (
                "b".to_string(),
                crate::forge::PullRequestBaseState {
                    base: "a-alias".to_string(),
                    updated_at_ms: 8_000,
                },
            ),
            (
                "c".to_string(),
                crate::forge::PullRequestBaseState {
                    base: "b-alias".to_string(),
                    updated_at_ms: 8_000,
                },
            ),
        ]);
        let aliases = HashMap::from([
            ("a".to_string(), "a-alias".to_string()),
            ("b".to_string(), "b-alias".to_string()),
            ("c".to_string(), "c-alias".to_string()),
        ]);
        assert_eq!(
            reconstruct_forge_stack(&live, &aliases),
            Some((
                "release".to_string(),
                9_000,
                vec!["a".to_string(), "b".to_string(), "c".to_string()]
            ))
        );
    }

    #[test]
    fn reconstruct_forge_chain_is_none_on_a_fork() {
        // a and b both claim to base directly on main -- unresolvable without guessing.
        let live_base = HashMap::from([
            ("a".to_string(), "main".to_string()),
            ("b".to_string(), "main".to_string()),
        ]);
        let alias_by_branch = HashMap::new();
        assert_eq!(
            reconstruct_forge_chain("main", &live_base, &alias_by_branch),
            None
        );
    }

    #[test]
    fn reconstruct_forge_chain_is_none_on_a_dangling_base() {
        // b's base ref matches nothing reachable from main (a's alias is wrong/stale).
        let live_base = HashMap::from([
            ("a".to_string(), "main".to_string()),
            ("b".to_string(), "some-unrelated-branch".to_string()),
        ]);
        let alias_by_branch = HashMap::from([("a".to_string(), "a-alias".to_string())]);
        assert_eq!(
            reconstruct_forge_chain("main", &live_base, &alias_by_branch),
            None
        );
    }

    #[test]
    fn reconstruct_forge_chain_empty_input_is_an_empty_order() {
        assert_eq!(
            reconstruct_forge_chain("main", &HashMap::new(), &HashMap::new()),
            Some(vec![])
        );
    }

    #[test]
    fn resync_in_flight_guard_rejects_a_second_claim_for_the_same_guardian() {
        let id = "guardian-resync-guard-test".to_string();
        RESYNCING.lock().unwrap().remove(&id); // in case a prior failed run left it set
        assert!(
            RESYNCING.lock().unwrap().insert(id.clone()),
            "first claim should win"
        );
        assert!(
            !RESYNCING.lock().unwrap().insert(id.clone()),
            "second claim while the first is still in flight must be rejected"
        );
        RESYNCING.lock().unwrap().remove(&id);
        assert!(
            RESYNCING.lock().unwrap().insert(id.clone()),
            "claim should be available again once released"
        );
        RESYNCING.lock().unwrap().remove(&id);
    }

    #[test]
    fn claim_guardian_for_forge_reorder_only_succeeds_from_in_review() {
        let store = Arc::new(Mutex::new(store()));
        let gid = store
            .lock()
            .unwrap()
            .create_guardian("demo", "main", "/repo")
            .unwrap();
        // Freshly created guardians start out `collecting`, not `in_review`.
        assert!(!claim_guardian_for_forge_reorder(&store, &gid));
        assert_eq!(
            store.lock().unwrap().get_guardian(&gid).unwrap().status,
            "collecting"
        );

        store
            .lock()
            .unwrap()
            .set_guardian_status(&gid, crate::guardian::GuardianStatus::InReview, None)
            .unwrap();
        assert!(claim_guardian_for_forge_reorder(&store, &gid));
        assert_eq!(
            store.lock().unwrap().get_guardian(&gid).unwrap().status,
            "merging"
        );
        // Already claimed -- a second attempt loses the race.
        assert!(!claim_guardian_for_forge_reorder(&store, &gid));
    }

    #[test]
    fn detect_forge_reorder_is_a_noop_with_fewer_than_two_stacked_prs() {
        let store = Arc::new(Mutex::new(store()));
        let gid = store
            .lock()
            .unwrap()
            .create_guardian("demo", "main", "/repo")
            .unwrap();
        store
            .lock()
            .unwrap()
            .add_guardian_branch(&gid, "a")
            .unwrap();
        // No open PRs at all yet -- nothing to compare against the forge.
        assert_eq!(detect_forge_reorder(&store, &gid).unwrap(), None);
    }

    #[test]
    fn poll_pr_base_drift_is_a_noop_for_a_review_with_no_submitted_prs() {
        let store = Arc::new(Mutex::new(store()));
        let gid = store
            .lock()
            .unwrap()
            .create_guardian("demo", "main", "/repo")
            .unwrap();
        store
            .lock()
            .unwrap()
            .add_guardian_branch(&gid, "a")
            .unwrap();
        assert_eq!(poll_pr_base_drift(&store, &gid).unwrap(), 0);
    }

    #[test]
    fn poll_pr_base_drift_is_a_noop_when_the_forge_remote_cannot_be_resolved() {
        // "/repo" is not a real git repository, so `forge::resolve_remote`
        // fails to read a remote from it -- same fixture other resync_pr_bases
        // tests above use to exercise the local-bookkeeping-only path without
        // any network access.
        let store = Arc::new(Mutex::new(store()));
        let gid = store
            .lock()
            .unwrap()
            .create_guardian("demo", "main", "/repo")
            .unwrap();
        store
            .lock()
            .unwrap()
            .add_guardian_branch(&gid, "a")
            .unwrap();
        let branch_id = store.lock().unwrap().get_guardian(&gid).unwrap().branches[0]
            .id
            .clone();
        store
            .lock()
            .unwrap()
            .create_pull_request(
                &gid,
                Some(&branch_id),
                "github",
                "acme/w",
                "a",
                "main",
                "A",
                "",
                Some(1),
                None,
            )
            .unwrap();
        assert_eq!(poll_pr_base_drift(&store, &gid).unwrap(), 0);
    }

    #[test]
    fn guardian_ids_with_open_pull_requests_only_returns_guardians_with_a_numbered_open_pr() {
        let s = store();
        let with_pr = s.create_guardian("has-pr", "main", "/repo").unwrap();
        let never_submitted = s.create_guardian("no-pr", "main", "/repo").unwrap();
        let unsubmitted_row = s
            .create_guardian("unsubmitted-row", "main", "/repo")
            .unwrap();
        let _ = never_submitted;

        s.create_pull_request(
            &with_pr,
            None,
            "github",
            "acme/w",
            "alias",
            "main",
            "T",
            "",
            Some(1),
            None,
        )
        .unwrap();
        // A row can exist locally before the forge call that assigns
        // `pr_number` ever succeeds -- shouldn't count as "submitted" yet.
        s.create_pull_request(
            &unsubmitted_row,
            None,
            "github",
            "acme/w",
            "alias",
            "main",
            "T",
            "",
            None,
            None,
        )
        .unwrap();

        let ids = s.guardian_ids_with_open_pull_requests().unwrap();
        assert_eq!(ids, vec![with_pr]);
    }

    #[test]
    fn resolve_unique_pr_alias_is_identity_when_free() {
        let s = store();
        let alias = s
            .resolve_unique_pr_alias("github", "acme/widget", None, "feature-x")
            .unwrap();
        assert_eq!(alias, "feature-x");
    }

    #[test]
    fn resolve_unique_pr_alias_suffixes_on_collision() {
        let s = store();
        let gid = s.create_guardian("demo", "main", "/repo").unwrap();
        s.create_pull_request(
            &gid,
            Some("branch-000000000001"),
            "github",
            "acme/widget",
            "feature-x",
            "main",
            "T",
            "D",
            None,
            None,
        )
        .unwrap();
        let alias = s
            .resolve_unique_pr_alias("github", "acme/widget", None, "feature-x")
            .unwrap();
        assert_eq!(alias, "feature-x-002");

        // A different repo is an independent namespace.
        let other_repo = s
            .resolve_unique_pr_alias("github", "acme/other", None, "feature-x")
            .unwrap();
        assert_eq!(other_repo, "feature-x");
    }

    #[test]
    fn resolve_unique_pr_alias_excludes_the_branchs_own_prior_alias() {
        let s = store();
        let gid = s.create_guardian("demo", "main", "/repo").unwrap();
        s.create_pull_request(
            &gid,
            Some("branch-000000000001"),
            "github",
            "acme/widget",
            "feature-x",
            "main",
            "T",
            "D",
            None,
            None,
        )
        .unwrap();
        // Resubmitting the same branch keeps its own alias rather than
        // being suffixed away from itself.
        let alias = s
            .resolve_unique_pr_alias(
                "github",
                "acme/widget",
                Some((&gid, "branch-000000000001")),
                "feature-x",
            )
            .unwrap();
        assert_eq!(alias, "feature-x");
        // A *different* branch still gets suffixed.
        let other = s
            .resolve_unique_pr_alias(
                "github",
                "acme/widget",
                Some((&gid, "branch-000000000002")),
                "feature-x",
            )
            .unwrap();
        assert_eq!(other, "feature-x-002");
    }

    #[test]
    fn update_pull_request_ex_sets_base_ref_and_last_pushed_sha() {
        let s = store();
        let gid = s.create_guardian("demo", "main", "/repo").unwrap();
        let id = s
            .create_pull_request(
                &gid,
                Some("branch-000000000001"),
                "github",
                "acme/widget",
                "feature-x",
                "main",
                "T",
                "D",
                None,
                None,
            )
            .unwrap();
        s.update_pull_request_ex(
            &id,
            None,
            None,
            None,
            None,
            Some("other-base"),
            Some(Some("deadbeef")),
            Some(Some("other-base")),
        )
        .unwrap();
        let pr = s.get_pull_request(&id).unwrap();
        assert_eq!(pr.base_ref, "other-base");
        assert_eq!(pr.last_pushed_sha.as_deref(), Some("deadbeef"));
        assert_eq!(pr.last_pushed_base_ref.as_deref(), Some("other-base"));
    }

    #[test]
    fn create_and_get_pull_request_roundtrips() {
        let s = store();
        let gid = s.create_guardian("demo", "main", "/repo").unwrap();
        let id = s
            .create_pull_request(
                &gid,
                Some("branch-000000000001"),
                "github",
                "acme/widget",
                "feature/foo",
                "main",
                "Add foo",
                "Adds the foo thing.",
                Some(42),
                Some("https://github.com/acme/widget/pull/42"),
            )
            .unwrap();
        let pr = s.get_pull_request(&id).unwrap();
        assert_eq!(pr.guardian_id, gid);
        assert_eq!(pr.branch_id.as_deref(), Some("branch-000000000001"));
        assert_eq!(pr.forge, "github");
        assert_eq!(pr.pr_number, Some(42));
        assert_eq!(pr.state, "open");
    }

    #[test]
    fn refresh_open_prs_drops_a_pr_closed_externally_and_persists_the_new_state() {
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let addr = server.server_addr().to_string();
        let handle = std::thread::spawn(move || {
            let req = server.recv().unwrap();
            req.respond(
                tiny_http::Response::from_string(r#"{"state": "closed", "merged": false}"#)
                    .with_status_code(200),
            )
            .unwrap();
        });

        let store = Arc::new(Mutex::new(store()));
        let gid = store
            .lock()
            .unwrap()
            .create_guardian("demo", "main", "/repo")
            .unwrap();
        let pr_id = store
            .lock()
            .unwrap()
            .create_pull_request(
                &gid,
                Some("branch-000000000001"),
                "github",
                "acme/widget",
                "feature/foo",
                "main",
                "Add foo",
                "Adds the foo thing.",
                Some(3),
                None,
            )
            .unwrap();

        let client = crate::forge::ForgeClient::new(
            crate::forge::ForgeKind::GitHub,
            format!("http://{addr}"),
            "acme/widget".to_string(),
            Some("tok".to_string()),
        );
        let prs = store
            .lock()
            .unwrap()
            .list_pull_requests_for_guardian(&gid)
            .unwrap();
        let by_branch = open_prs_by_branch(&prs);
        assert_eq!(
            by_branch.len(),
            1,
            "sanity: recorded as open before the refresh"
        );

        let refreshed = refresh_open_prs(&store, &client, by_branch);
        assert!(
            refreshed.is_empty(),
            "a PR closed on the forge must not count as still open"
        );

        let updated = store.lock().unwrap().get_pull_request(&pr_id).unwrap();
        assert_eq!(
            updated.state, "closed",
            "the local row must be corrected to match forge reality"
        );
        handle.join().unwrap();
    }

    #[test]
    fn check_pr_merges_approves_when_every_linked_pr_has_merged() {
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let addr = server.server_addr().to_string();
        let handle = std::thread::spawn(move || {
            let req = server.recv().unwrap();
            req.respond(
                tiny_http::Response::from_string(r#"{"state": "closed", "merged": true}"#)
                    .with_status_code(200),
            )
            .unwrap();
        });

        let store = Arc::new(Mutex::new(store()));
        let gid = store
            .lock()
            .unwrap()
            .create_guardian("demo", "main", "/repo")
            .unwrap();
        store
            .lock()
            .unwrap()
            .set_guardian_status(&gid, GuardianStatus::InReview, None)
            .unwrap();
        let pr_id = store
            .lock()
            .unwrap()
            .create_pull_request(
                &gid,
                Some("branch-000000000001"),
                "github",
                "acme/widget",
                "feature/foo",
                "main",
                "Add foo",
                "Adds the foo thing.",
                Some(7),
                None,
            )
            .unwrap();

        let client = crate::forge::ForgeClient::new(
            crate::forge::ForgeKind::GitHub,
            format!("http://{addr}"),
            "acme/widget".to_string(),
            Some("tok".to_string()),
        );
        let prs = store
            .lock()
            .unwrap()
            .list_pull_requests_for_guardian(&gid)
            .unwrap();

        let changed = apply_pr_merge_check(&store, &gid, &prs, &client);
        assert!(changed, "every linked pr merging must report a change");

        let updated_guardian = store.lock().unwrap().get_guardian(&gid).unwrap();
        assert_eq!(updated_guardian.status.as_str(), "approved");
        let updated_pr = store.lock().unwrap().get_pull_request(&pr_id).unwrap();
        assert_eq!(updated_pr.state, "merged");

        handle.join().unwrap();
    }

    #[test]
    fn check_pr_merges_approves_when_all_merges_were_already_recorded() {
        let store = Arc::new(Mutex::new(store()));
        let gid = store
            .lock()
            .unwrap()
            .create_guardian("demo", "main", "/repo")
            .unwrap();
        store
            .lock()
            .unwrap()
            .set_guardian_status(&gid, GuardianStatus::InReview, None)
            .unwrap();
        let pr_id = store
            .lock()
            .unwrap()
            .create_pull_request(
                &gid,
                Some("branch-000000000001"),
                "github",
                "acme/widget",
                "feature/foo",
                "main",
                "Add foo",
                "Adds the foo thing.",
                Some(7),
                None,
            )
            .unwrap();
        store
            .lock()
            .unwrap()
            .update_pull_request(&pr_id, None, None, None, Some("merged"))
            .unwrap();

        let client = crate::forge::ForgeClient::new(
            crate::forge::ForgeKind::GitHub,
            "http://127.0.0.1:1".to_string(),
            "acme/widget".to_string(),
            None,
        );
        let prs = store
            .lock()
            .unwrap()
            .list_pull_requests_for_guardian(&gid)
            .unwrap();

        assert!(apply_pr_merge_check(&store, &gid, &prs, &client));
        assert_eq!(
            store.lock().unwrap().get_guardian(&gid).unwrap().status,
            "approved"
        );
    }

    #[test]
    fn check_pr_merges_drops_a_pr_merged_out_of_band_while_mid_flight() {
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let addr = server.server_addr().to_string();
        let handle = std::thread::spawn(move || {
            let req = server.recv().unwrap();
            req.respond(
                tiny_http::Response::from_string(r#"{"state": "closed", "merged": true}"#)
                    .with_status_code(200),
            )
            .unwrap();
        });

        let store = Arc::new(Mutex::new(store()));
        let gid = store
            .lock()
            .unwrap()
            .create_guardian("demo", "main", "/repo")
            .unwrap();
        // A rebase/feedback pass owns this review's worktrees right now.
        store
            .lock()
            .unwrap()
            .set_guardian_status(&gid, GuardianStatus::Merging, None)
            .unwrap();
        let pr_id = store
            .lock()
            .unwrap()
            .create_pull_request(
                &gid,
                Some("branch-000000000001"),
                "github",
                "acme/widget",
                "feature/foo",
                "main",
                "Add foo",
                "Adds the foo thing.",
                Some(7),
                None,
            )
            .unwrap();

        let client = crate::forge::ForgeClient::new(
            crate::forge::ForgeKind::GitHub,
            format!("http://{addr}"),
            "acme/widget".to_string(),
            Some("tok".to_string()),
        );
        let prs = store
            .lock()
            .unwrap()
            .list_pull_requests_for_guardian(&gid)
            .unwrap();

        let changed = apply_pr_merge_check(&store, &gid, &prs, &client);
        assert!(changed, "an out-of-band merge must still report a change");

        let updated_guardian = store.lock().unwrap().get_guardian(&gid).unwrap();
        assert_eq!(
            updated_guardian.status.as_str(),
            "merging",
            "a mid-flight review must not be silently force-approved"
        );
        assert_eq!(
            updated_guardian.notice_kind.as_deref(),
            Some("pr_merged_mid_flight")
        );
        let dropped_pr = store.lock().unwrap().get_pull_request(&pr_id).unwrap();
        assert_eq!(
            dropped_pr.state, "dropped",
            "the stale pr row must be soft-deleted, not removed"
        );
        assert!(
            dropped_pr.dropped_reason.is_some(),
            "a dropped pr row must record why"
        );

        handle.join().unwrap();
    }

    #[test]
    fn check_pr_merges_is_fail_safe_when_the_forge_call_fails() {
        let store = Arc::new(Mutex::new(store()));
        let gid = store
            .lock()
            .unwrap()
            .create_guardian("demo", "main", "/repo")
            .unwrap();
        store
            .lock()
            .unwrap()
            .set_guardian_status(&gid, GuardianStatus::InReview, None)
            .unwrap();
        let pr_id = store
            .lock()
            .unwrap()
            .create_pull_request(
                &gid,
                Some("branch-000000000001"),
                "github",
                "acme/widget",
                "feature/foo",
                "main",
                "Add foo",
                "Adds the foo thing.",
                Some(7),
                None,
            )
            .unwrap();

        // No token configured -- `get_pull_request_state` fails before any
        // network call, exercising the "forge unreachable" fail-safe path.
        let client = crate::forge::ForgeClient::new(
            crate::forge::ForgeKind::GitHub,
            "http://127.0.0.1:1".to_string(),
            "acme/widget".to_string(),
            None,
        );
        let prs = store
            .lock()
            .unwrap()
            .list_pull_requests_for_guardian(&gid)
            .unwrap();

        let changed = apply_pr_merge_check(&store, &gid, &prs, &client);
        assert!(
            !changed,
            "an unreachable forge must never be mistaken for a merged pr"
        );

        let updated_guardian = store.lock().unwrap().get_guardian(&gid).unwrap();
        assert_eq!(updated_guardian.status.as_str(), "in_review");
        let updated_pr = store.lock().unwrap().get_pull_request(&pr_id).unwrap();
        assert_eq!(updated_pr.state, "open");
    }

    /// Common fork-mode fixture for the RAL-338 promotion tests below: a
    /// two-remote repo (`origin` = parent `acme/widget`, `fork` =
    /// `alice/widget`), a registered project + project-wide fork row, and a
    /// two-branch guardian (`a` = root, `b` = its successor) whose PR rows
    /// already reflect the pre-promotion state (root open at the parent,
    /// successor open at the fork, based on the root's alias).
    fn fork_promotion_fixture(addr: &str) -> (Arc<Mutex<Store>>, String, PathBuf) {
        let root_dir = tmp_dir("promotion");
        g(&root_dir, &["init"]);
        g(
            &root_dir,
            &[
                "remote",
                "add",
                "origin",
                "https://github.com/acme/widget.git",
            ],
        );
        g(
            &root_dir,
            &[
                "remote",
                "add",
                "fork",
                "https://github.com/alice/widget.git",
            ],
        );
        std::fs::write(
            root_dir.join(".ralphus.toml"),
            format!(
                "[forge]\nkind = \"github\"\napi_base = \"http://{addr}\"\ntoken_env = \"RALPHUS_TEST_FORGE_TOKEN\"\n"
            ),
        )
        .unwrap();

        let store = Arc::new(Mutex::new(store()));
        store
            .lock()
            .unwrap()
            .register_project("demo", "orchestrator", root_dir.to_str().unwrap(), "git")
            .unwrap();
        store
            .lock()
            .unwrap()
            .upsert_project_fork(
                "demo",
                "",
                "https://github.com/alice/widget.git",
                "fork",
                "alice",
            )
            .unwrap();

        let gid = store
            .lock()
            .unwrap()
            .create_guardian("demo", "release", root_dir.to_str().unwrap())
            .unwrap();
        store
            .lock()
            .unwrap()
            .set_guardian_status(&gid, GuardianStatus::InReview, None)
            .unwrap();
        for branch in ["a", "b"] {
            store
                .lock()
                .unwrap()
                .add_guardian_branch(&gid, branch)
                .unwrap();
        }
        let ids: Vec<_> = store
            .lock()
            .unwrap()
            .get_guardian(&gid)
            .unwrap()
            .branches
            .into_iter()
            .map(|b| b.id)
            .collect();

        store
            .lock()
            .unwrap()
            .create_pull_request(
                &gid,
                Some(&ids[0]),
                "github",
                "acme/widget",
                "a-alias",
                "release",
                "Add a",
                "",
                Some(10),
                None,
            )
            .unwrap();
        store
            .lock()
            .unwrap()
            .create_pull_request(
                &gid,
                Some(&ids[1]),
                "github",
                "alice/widget",
                "b-alias",
                "a-alias",
                "Add b",
                "",
                Some(20),
                None,
            )
            .unwrap();
        (store, gid, root_dir)
    }

    #[test]
    fn promotion_closes_the_fork_pr_and_reopens_against_the_parent_when_the_root_merges() {
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let addr = server.server_addr().to_string();
        let handle = std::thread::spawn(move || {
            // 1. Root PR (filed at the parent) merge-state check.
            let req = server.recv().unwrap();
            assert_eq!(req.url(), "/repos/acme/widget/pulls/10");
            req.respond(
                tiny_http::Response::from_string(r#"{"state":"closed","merged":true}"#)
                    .with_status_code(200),
            )
            .unwrap();

            // 2. Successor PR (filed at the fork) merge-state check -- still open.
            let req = server.recv().unwrap();
            assert_eq!(req.url(), "/repos/alice/widget/pulls/20");
            req.respond(
                tiny_http::Response::from_string(r#"{"state":"open","merged":false}"#)
                    .with_status_code(200),
            )
            .unwrap();

            // 3. Promotion: create the new cross-repo PR at the parent.
            let mut req = server.recv().unwrap();
            assert_eq!(req.method(), &tiny_http::Method::Post);
            assert_eq!(req.url(), "/repos/acme/widget/pulls");
            let mut body = String::new();
            req.as_reader().read_to_string(&mut body).unwrap();
            let payload: serde_json::Value = serde_json::from_str(&body).unwrap();
            assert_eq!(payload["head"], serde_json::json!("alice:b-alias"));
            assert_eq!(payload["base"], serde_json::json!("release"));
            req.respond(
                tiny_http::Response::from_string(r#"{"number":30,"html_url":"http://x/30"}"#)
                    .with_status_code(201),
            )
            .unwrap();

            // 4. Close the now-superseded fork-internal PR.
            let mut req = server.recv().unwrap();
            assert_eq!(req.method(), &tiny_http::Method::Patch);
            assert_eq!(req.url(), "/repos/alice/widget/pulls/20");
            let mut body = String::new();
            req.as_reader().read_to_string(&mut body).unwrap();
            assert_eq!(
                serde_json::from_str::<serde_json::Value>(&body).unwrap()["state"],
                serde_json::json!("closed")
            );
            req.respond(tiny_http::Response::from_string("{}").with_status_code(200))
                .unwrap();

            // 5. Pointer comment left on the closed PR.
            let req = server.recv().unwrap();
            assert_eq!(req.method(), &tiny_http::Method::Post);
            assert_eq!(req.url(), "/repos/alice/widget/issues/20/comments");
            req.respond(tiny_http::Response::from_string("{}").with_status_code(201))
                .unwrap();
        });

        let (store, gid, root_dir) = fork_promotion_fixture(&addr);
        check_pr_merges(&store, &gid);

        let rows = store
            .lock()
            .unwrap()
            .list_pull_requests_for_guardian(&gid)
            .unwrap();
        let root_row = rows.iter().find(|p| p.pr_number == Some(10)).unwrap();
        assert_eq!(root_row.state, "merged");
        let old_successor = rows.iter().find(|p| p.pr_number == Some(20)).unwrap();
        assert_eq!(old_successor.state, "closed");
        let new_successor = rows.iter().find(|p| p.pr_number == Some(30)).unwrap();
        assert_eq!(new_successor.repo, "acme/widget");
        assert_eq!(new_successor.state, "open");
        assert_eq!(new_successor.branch_alias, "b-alias");
        assert_eq!(
            old_successor.superseded_by.as_deref(),
            Some(new_successor.id.as_str())
        );

        // The stack isn't fully merged yet (the promoted branch is still
        // open) -- the review must stay `in_review`, not jump to `approved`.
        let updated_guardian = store.lock().unwrap().get_guardian(&gid).unwrap();
        assert_eq!(updated_guardian.status.as_str(), "in_review");

        handle.join().unwrap();
        let _ = std::fs::remove_dir_all(root_dir);
    }

    #[test]
    fn promotion_is_a_noop_when_the_forge_already_retargeted_the_successor_at_the_parent() {
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let addr = server.server_addr().to_string();
        let handle = std::thread::spawn(move || {
            let req = server.recv().unwrap();
            assert_eq!(req.url(), "/repos/acme/widget/pulls/10");
            req.respond(
                tiny_http::Response::from_string(r#"{"state":"closed","merged":true}"#)
                    .with_status_code(200),
            )
            .unwrap();
            // Successor's merge-state check: reconcile-first reads this
            // before ever touching anything -- no further requests should
            // follow since it's already filed at the parent below.
            let req = server.recv().unwrap();
            assert_eq!(req.url(), "/repos/acme/widget/pulls/20");
            req.respond(
                tiny_http::Response::from_string(r#"{"state":"open","merged":false}"#)
                    .with_status_code(200),
            )
            .unwrap();
        });

        let (store, gid, root_dir) = fork_promotion_fixture(&addr);
        // Simulate the successor already having been reconciled (by a human,
        // or a forge feature this ticket doesn't yet trust) directly against
        // the parent, still under its own original PR number.
        let rows = store
            .lock()
            .unwrap()
            .list_pull_requests_for_guardian(&gid)
            .unwrap();
        let successor_id = rows
            .iter()
            .find(|p| p.pr_number == Some(20))
            .unwrap()
            .id
            .clone();
        // `update_pull_request_ex` has no `repo` field -- reach the column
        // directly instead, to stand in for "the forge already retargeted
        // this PR to the parent" without inventing a new store method whose
        // only caller would be this test.
        store
            .lock()
            .unwrap()
            .conn
            .execute(
                "UPDATE guardian_pull_requests SET repo='acme/widget' WHERE id=?1",
                rusqlite::params![successor_id],
            )
            .unwrap();

        check_pr_merges(&store, &gid);

        let rows = store
            .lock()
            .unwrap()
            .list_pull_requests_for_guardian(&gid)
            .unwrap();
        assert_eq!(rows.len(), 2, "no new pr row should have been created");
        let successor = rows.iter().find(|p| p.pr_number == Some(20)).unwrap();
        assert_eq!(successor.state, "open");
        assert!(successor.superseded_by.is_none());

        handle.join().unwrap();
        let _ = std::fs::remove_dir_all(root_dir);
    }

    #[test]
    fn a_non_fork_review_is_untouched_by_promotion() {
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let addr = server.server_addr().to_string();
        let handle = std::thread::spawn(move || {
            let req = server.recv().unwrap();
            assert_eq!(req.url(), "/repos/acme/widget/pulls/10");
            req.respond(
                tiny_http::Response::from_string(r#"{"state":"closed","merged":true}"#)
                    .with_status_code(200),
            )
            .unwrap();
            let req = server.recv().unwrap();
            assert_eq!(req.url(), "/repos/acme/widget/pulls/20");
            req.respond(
                tiny_http::Response::from_string(r#"{"state":"open","merged":false}"#)
                    .with_status_code(200),
            )
            .unwrap();
        });

        // Same two-branch shape as the fork fixture, but no project/fork is
        // registered at all -- both PRs live in the same (parent-only) repo,
        // exactly like a pre-RAL-338 stack.
        let root_dir = tmp_dir("promotion-no-fork");
        g(&root_dir, &["init"]);
        g(
            &root_dir,
            &[
                "remote",
                "add",
                "origin",
                "https://github.com/acme/widget.git",
            ],
        );
        std::fs::write(
            root_dir.join(".ralphus.toml"),
            format!(
                "[forge]\nkind = \"github\"\napi_base = \"http://{addr}\"\ntoken_env = \"RALPHUS_TEST_FORGE_TOKEN\"\n"
            ),
        )
        .unwrap();
        let store = Arc::new(Mutex::new(store()));
        let gid = store
            .lock()
            .unwrap()
            .create_guardian("demo", "release", root_dir.to_str().unwrap())
            .unwrap();
        store
            .lock()
            .unwrap()
            .set_guardian_status(&gid, GuardianStatus::InReview, None)
            .unwrap();
        for branch in ["a", "b"] {
            store
                .lock()
                .unwrap()
                .add_guardian_branch(&gid, branch)
                .unwrap();
        }
        let ids: Vec<_> = store
            .lock()
            .unwrap()
            .get_guardian(&gid)
            .unwrap()
            .branches
            .into_iter()
            .map(|b| b.id)
            .collect();
        store
            .lock()
            .unwrap()
            .create_pull_request(
                &gid,
                Some(&ids[0]),
                "github",
                "acme/widget",
                "a-alias",
                "release",
                "Add a",
                "",
                Some(10),
                None,
            )
            .unwrap();
        store
            .lock()
            .unwrap()
            .create_pull_request(
                &gid,
                Some(&ids[1]),
                "github",
                "acme/widget",
                "b-alias",
                "a-alias",
                "Add b",
                "",
                Some(20),
                None,
            )
            .unwrap();

        check_pr_merges(&store, &gid);

        let rows = store
            .lock()
            .unwrap()
            .list_pull_requests_for_guardian(&gid)
            .unwrap();
        assert_eq!(
            rows.len(),
            2,
            "no promotion row should ever be created without a registered fork"
        );
        let root_row = rows.iter().find(|p| p.pr_number == Some(10)).unwrap();
        assert_eq!(root_row.state, "merged");
        let successor = rows.iter().find(|p| p.pr_number == Some(20)).unwrap();
        assert_eq!(
            successor.state, "open",
            "byte-identical routing: no fork, no promotion"
        );
        assert!(successor.superseded_by.is_none());

        handle.join().unwrap();
        let _ = std::fs::remove_dir_all(root_dir);
    }

    #[test]
    fn promotion_skips_a_branch_that_also_merged_and_promotes_only_the_next_still_open_one() {
        // Three branches: a (root, parent) and b both merge in the same poll;
        // c is the only one still open. Promotion must skip b (it also just
        // merged -- nothing to promote there) and promote exactly c, not b.
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let addr = server.server_addr().to_string();
        let handle = std::thread::spawn(move || {
            let req = server.recv().unwrap();
            assert_eq!(req.url(), "/repos/acme/widget/pulls/10");
            req.respond(
                tiny_http::Response::from_string(r#"{"state":"closed","merged":true}"#)
                    .with_status_code(200),
            )
            .unwrap();
            let req = server.recv().unwrap();
            assert_eq!(req.url(), "/repos/alice/widget/pulls/20");
            req.respond(
                tiny_http::Response::from_string(r#"{"state":"closed","merged":true}"#)
                    .with_status_code(200),
            )
            .unwrap();
            let req = server.recv().unwrap();
            assert_eq!(req.url(), "/repos/alice/widget/pulls/30");
            req.respond(
                tiny_http::Response::from_string(r#"{"state":"open","merged":false}"#)
                    .with_status_code(200),
            )
            .unwrap();

            let mut req = server.recv().unwrap();
            assert_eq!(req.method(), &tiny_http::Method::Post);
            assert_eq!(req.url(), "/repos/acme/widget/pulls");
            let mut body = String::new();
            req.as_reader().read_to_string(&mut body).unwrap();
            let payload: serde_json::Value = serde_json::from_str(&body).unwrap();
            assert_eq!(
                payload["head"],
                serde_json::json!("alice:c-alias"),
                "must promote c (the next still-open branch), not b (which also merged)"
            );
            req.respond(
                tiny_http::Response::from_string(r#"{"number":40,"html_url":"http://x/40"}"#)
                    .with_status_code(201),
            )
            .unwrap();

            let req = server.recv().unwrap();
            assert_eq!(req.method(), &tiny_http::Method::Patch);
            assert_eq!(req.url(), "/repos/alice/widget/pulls/30");
            req.respond(tiny_http::Response::from_string("{}").with_status_code(200))
                .unwrap();

            let req = server.recv().unwrap();
            assert_eq!(req.method(), &tiny_http::Method::Post);
            assert_eq!(req.url(), "/repos/alice/widget/issues/30/comments");
            req.respond(tiny_http::Response::from_string("{}").with_status_code(201))
                .unwrap();
        });

        let (store, gid, root_dir) = fork_promotion_fixture(&addr);
        store
            .lock()
            .unwrap()
            .add_guardian_branch(&gid, "c")
            .unwrap();
        let ids: Vec<_> = store
            .lock()
            .unwrap()
            .get_guardian(&gid)
            .unwrap()
            .branches
            .into_iter()
            .map(|b| b.id)
            .collect();
        store
            .lock()
            .unwrap()
            .create_pull_request(
                &gid,
                Some(&ids[2]),
                "github",
                "alice/widget",
                "c-alias",
                "b-alias",
                "Add c",
                "",
                Some(30),
                None,
            )
            .unwrap();

        check_pr_merges(&store, &gid);

        let rows = store
            .lock()
            .unwrap()
            .list_pull_requests_for_guardian(&gid)
            .unwrap();
        assert_eq!(
            rows.iter().find(|p| p.pr_number == Some(10)).unwrap().state,
            "merged"
        );
        assert_eq!(
            rows.iter().find(|p| p.pr_number == Some(20)).unwrap().state,
            "merged"
        );
        let old_c = rows.iter().find(|p| p.pr_number == Some(30)).unwrap();
        assert_eq!(old_c.state, "closed");
        let new_c = rows.iter().find(|p| p.pr_number == Some(40)).unwrap();
        assert_eq!(new_c.repo, "acme/widget");
        assert_eq!(new_c.branch_alias, "c-alias");
        assert_eq!(old_c.superseded_by.as_deref(), Some(new_c.id.as_str()));
        assert_eq!(
            rows.len(),
            4,
            "exactly one promotion happened (b was skipped, not itself promoted)"
        );

        handle.join().unwrap();
        let _ = std::fs::remove_dir_all(root_dir);
    }

    #[test]
    fn gitlab_promotion_uses_the_forks_client_with_a_target_project_id_not_the_parents() {
        // GitLab's cross-project MR is created *on the fork* with a
        // `target_project_id` pointing at the parent (never on the parent's
        // own client, unlike GitHub) -- so a promoted GitLab root's `repo`
        // stays the fork's, not the parent's. Promotion's "is this the root"
        // detection must key off `base_ref`, not `repo`, or it would never
        // fire for GitLab at all.
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let addr = server.server_addr().to_string();
        let handle = std::thread::spawn(move || {
            // 1. Root MR (filed on the fork, target_project_id -> parent) merge-state check.
            let req = server.recv().unwrap();
            assert_eq!(req.url(), "/projects/alice%2Fwidget/merge_requests/10");
            req.respond(
                tiny_http::Response::from_string(r#"{"state":"merged"}"#).with_status_code(200),
            )
            .unwrap();

            // 2. Successor MR (also on the fork) merge-state check -- still open.
            let req = server.recv().unwrap();
            assert_eq!(req.url(), "/projects/alice%2Fwidget/merge_requests/20");
            req.respond(
                tiny_http::Response::from_string(r#"{"state":"opened"}"#).with_status_code(200),
            )
            .unwrap();

            // 3. Promotion resolves the parent's numeric GitLab project id.
            let req = server.recv().unwrap();
            assert_eq!(req.url(), "/projects/acme%2Fwidget");
            req.respond(tiny_http::Response::from_string(r#"{"id":999}"#).with_status_code(200))
                .unwrap();

            // 4. Create the new cross-project MR -- on the FORK client, not
            //    the parent's, with `target_project_id` set.
            let mut req = server.recv().unwrap();
            assert_eq!(req.method(), &tiny_http::Method::Post);
            assert_eq!(req.url(), "/projects/alice%2Fwidget/merge_requests");
            let mut body = String::new();
            req.as_reader().read_to_string(&mut body).unwrap();
            let payload: serde_json::Value = serde_json::from_str(&body).unwrap();
            assert_eq!(payload["source_branch"], serde_json::json!("b-alias"));
            assert_eq!(payload["target_branch"], serde_json::json!("release"));
            assert_eq!(payload["target_project_id"], serde_json::json!(999));
            req.respond(
                tiny_http::Response::from_string(r#"{"iid":40,"web_url":"http://x/40"}"#)
                    .with_status_code(201),
            )
            .unwrap();

            // 5. Close the now-superseded fork-internal MR.
            let req = server.recv().unwrap();
            assert_eq!(req.method(), &tiny_http::Method::Put);
            assert_eq!(req.url(), "/projects/alice%2Fwidget/merge_requests/20");
            req.respond(tiny_http::Response::from_string("{}").with_status_code(200))
                .unwrap();

            // 6. Pointer note on the closed MR.
            let req = server.recv().unwrap();
            assert_eq!(req.method(), &tiny_http::Method::Post);
            assert_eq!(
                req.url(),
                "/projects/alice%2Fwidget/merge_requests/20/notes"
            );
            req.respond(tiny_http::Response::from_string("{}").with_status_code(201))
                .unwrap();
        });

        let root_dir = tmp_dir("gitlab-promotion");
        g(&root_dir, &["init"]);
        g(
            &root_dir,
            &[
                "remote",
                "add",
                "origin",
                "https://gitlab.com/acme/widget.git",
            ],
        );
        g(
            &root_dir,
            &[
                "remote",
                "add",
                "fork",
                "https://gitlab.com/alice/widget.git",
            ],
        );
        std::fs::write(
            root_dir.join(".ralphus.toml"),
            format!(
                "[forge]\nkind = \"gitlab\"\napi_base = \"http://{addr}\"\ntoken_env = \"RALPHUS_TEST_FORGE_TOKEN\"\n"
            ),
        )
        .unwrap();

        let store = Arc::new(Mutex::new(store()));
        store
            .lock()
            .unwrap()
            .register_project("demo", "orchestrator", root_dir.to_str().unwrap(), "git")
            .unwrap();
        store
            .lock()
            .unwrap()
            .upsert_project_fork(
                "demo",
                "",
                "https://gitlab.com/alice/widget.git",
                "fork",
                "",
            )
            .unwrap();

        let gid = store
            .lock()
            .unwrap()
            .create_guardian("demo", "release", root_dir.to_str().unwrap())
            .unwrap();
        store
            .lock()
            .unwrap()
            .set_guardian_status(&gid, GuardianStatus::InReview, None)
            .unwrap();
        for branch in ["a", "b"] {
            store
                .lock()
                .unwrap()
                .add_guardian_branch(&gid, branch)
                .unwrap();
        }
        let ids: Vec<_> = store
            .lock()
            .unwrap()
            .get_guardian(&gid)
            .unwrap()
            .branches
            .into_iter()
            .map(|b| b.id)
            .collect();
        // Root MR: GitLab files it on the FORK's repo (not the parent's),
        // per the routing asymmetry -- `repo` is the fork's encoded path.
        store
            .lock()
            .unwrap()
            .create_pull_request(
                &gid,
                Some(&ids[0]),
                "gitlab",
                "alice%2Fwidget",
                "a-alias",
                "release",
                "Add a",
                "",
                Some(10),
                None,
            )
            .unwrap();
        store
            .lock()
            .unwrap()
            .create_pull_request(
                &gid,
                Some(&ids[1]),
                "gitlab",
                "alice%2Fwidget",
                "b-alias",
                "a-alias",
                "Add b",
                "",
                Some(20),
                None,
            )
            .unwrap();

        check_pr_merges(&store, &gid);

        let rows = store
            .lock()
            .unwrap()
            .list_pull_requests_for_guardian(&gid)
            .unwrap();
        assert_eq!(
            rows.iter().find(|p| p.pr_number == Some(10)).unwrap().state,
            "merged"
        );
        let old_b = rows.iter().find(|p| p.pr_number == Some(20)).unwrap();
        assert_eq!(old_b.state, "closed");
        let new_b = rows.iter().find(|p| p.pr_number == Some(40)).unwrap();
        assert_eq!(
            new_b.repo, "alice%2Fwidget",
            "GitLab always files on the fork's repo"
        );
        assert_eq!(new_b.base_ref, "release");
        assert_eq!(old_b.superseded_by.as_deref(), Some(new_b.id.as_str()));

        handle.join().unwrap();
        let _ = std::fs::remove_dir_all(root_dir);
    }

    /// RAL-338's required offline integration test: two *local bare git
    /// repositories* stand in for the parent and fork remotes, proving fork
    /// push targeting and three-branch base chaining without any network
    /// access (the mock HTTP server below is `127.0.0.1`-only, standing in
    /// for the forge API, which is a separate concern from the git
    /// transport this test exercises for real).
    ///
    /// Calls [`submit_stacked_branch_pr`] directly (once per branch, mirroring
    /// [`submit_stack_for_guardian`]'s own loop) with an explicit
    /// `title`/`description` on each [`PrRequest`] so [`resolve_title_description`]
    /// short-circuits before ever calling `fetch_pr_template`/`synthesize_pr_text`
    /// -- this test is about push/route targeting, not PR-text synthesis
    /// (already covered elsewhere).
    #[test]
    fn fork_mode_submission_pushes_every_alias_to_the_fork_across_two_local_bare_repos() {
        let parent_bare = tmp_dir("fork-it-parent-bare");
        g(&parent_bare, &["init", "--bare"]);
        let fork_bare = tmp_dir("fork-it-fork-bare");
        g(&fork_bare, &["init", "--bare"]);

        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let addr = server.server_addr().to_string();
        let handle = std::thread::spawn(move || {
            // Root branch: cross-repository PR filed at the parent, head is
            // `<fork_owner>:<alias>`.
            let mut req = server.recv().unwrap();
            assert_eq!(req.url(), "/repos/acme/widget/pulls");
            let mut body = String::new();
            req.as_reader().read_to_string(&mut body).unwrap();
            let payload: serde_json::Value = serde_json::from_str(&body).unwrap();
            assert_eq!(payload["head"], serde_json::json!("alice:a-alias"));
            assert_eq!(payload["base"], serde_json::json!("release"));
            req.respond(
                tiny_http::Response::from_string(r#"{"number":1,"html_url":"http://x/1"}"#)
                    .with_status_code(201),
            )
            .unwrap();

            // Branch b: fork-internal PR based on a's own alias.
            let mut req = server.recv().unwrap();
            assert_eq!(req.url(), "/repos/alice/widget/pulls");
            let mut body = String::new();
            req.as_reader().read_to_string(&mut body).unwrap();
            let payload: serde_json::Value = serde_json::from_str(&body).unwrap();
            assert_eq!(payload["head"], serde_json::json!("b-alias"));
            assert_eq!(payload["base"], serde_json::json!("a-alias"));
            req.respond(
                tiny_http::Response::from_string(r#"{"number":2,"html_url":"http://x/2"}"#)
                    .with_status_code(201),
            )
            .unwrap();

            // Branch c: fork-internal PR based on b's own alias.
            let mut req = server.recv().unwrap();
            assert_eq!(req.url(), "/repos/alice/widget/pulls");
            let mut body = String::new();
            req.as_reader().read_to_string(&mut body).unwrap();
            let payload: serde_json::Value = serde_json::from_str(&body).unwrap();
            assert_eq!(payload["head"], serde_json::json!("c-alias"));
            assert_eq!(payload["base"], serde_json::json!("b-alias"));
            req.respond(
                tiny_http::Response::from_string(r#"{"number":3,"html_url":"http://x/3"}"#)
                    .with_status_code(201),
            )
            .unwrap();
        });

        let root_dir = tmp_dir("fork-it-work");
        g(&root_dir, &["init", "--initial-branch", "release"]);
        gwrite(&root_dir, "base.txt", "base\n");
        g(&root_dir, &["add", "."]);
        g(&root_dir, &["commit", "--message", "base"]);
        for (branch, file) in [
            ("review/a", "a.txt"),
            ("review/b", "b.txt"),
            ("review/c", "c.txt"),
        ] {
            g(&root_dir, &["checkout", "-b", branch]);
            gwrite(&root_dir, file, "content\n");
            g(&root_dir, &["add", "."]);
            g(&root_dir, &["commit", "--message", &format!("add {file}")]);
        }
        g(&root_dir, &["checkout", "release"]);
        crate::project_forks::ensure_fork_remote(&root_dir, "fork", fork_bare.to_str().unwrap())
            .unwrap();

        let store = Arc::new(Mutex::new(store()));
        let gid = store
            .lock()
            .unwrap()
            .create_guardian("demo", "release", root_dir.to_str().unwrap())
            .unwrap();
        for branch in ["a", "b", "c"] {
            store
                .lock()
                .unwrap()
                .add_guardian_branch(&gid, branch)
                .unwrap();
        }
        let ids: Vec<_> = store
            .lock()
            .unwrap()
            .get_guardian(&gid)
            .unwrap()
            .branches
            .into_iter()
            .map(|b| b.id)
            .collect();
        for (branch_id, review_branch) in ids.iter().zip(["review/a", "review/b", "review/c"]) {
            store
                .lock()
                .unwrap()
                .set_branch_review(&gid, branch_id, review_branch, "wt")
                .unwrap();
        }

        let fork = crate::project_forks::ForkRecord {
            project: "demo".to_string(),
            user: String::new(),
            fork_url: fork_bare.to_str().unwrap().to_string(),
            remote_name: "fork".to_string(),
            fork_owner: "alice".to_string(),
            created_at_ms: 0,
            updated_at_ms: 0,
        };
        let parent_client = crate::forge::ForgeClient::new(
            crate::forge::ForgeKind::GitHub,
            format!("http://{addr}"),
            "acme/widget".to_string(),
            Some("tok".to_string()),
        );
        let fork_client = crate::forge::ForgeClient::new(
            crate::forge::ForgeKind::GitHub,
            format!("http://{addr}"),
            "alice/widget".to_string(),
            Some("tok".to_string()),
        );
        let routing = ForkRouting {
            fork,
            parent_client,
            fork_client: fork_client.clone(),
            parent_project_id: None,
        };

        let guardian = store.lock().unwrap().get_guardian(&gid).unwrap();
        let mut ordered_enabled: Vec<&BranchView> =
            guardian.branches.iter().filter(|b| b.enabled).collect();
        ordered_enabled.sort_by_key(|b| b.position);
        let mut alias_by_branch: HashMap<String, String> = HashMap::new();
        let runner = NoopRunner;
        for (i, branch) in ordered_enabled.iter().enumerate() {
            let req = PrRequest {
                branch_id: Some(branch.id.clone()),
                branch_alias: None,
                title: Some(format!("Title {i}")),
                description: Some(format!("Description {i}")),
                use_worktree_branch_name: None,
            };
            submit_stacked_branch_pr(
                &store,
                &runner,
                &fork_client,
                &gid,
                &root_dir,
                "fork",
                &guardian,
                &ordered_enabled,
                &mut alias_by_branch,
                "release",
                branch,
                &req,
                "{name}-alias",
                None,
                "stack-1",
                Some(&routing),
            )
            .unwrap();
        }

        // Fork push targeting: every alias -- including the root's --
        // landed on the FORK bare repo, proven via a real (local, no
        // network) `git for-each-ref` against it.
        let fork_refs = g(
            &fork_bare,
            &["for-each-ref", "--format=%(refname:short)", "refs/heads"],
        );
        assert!(fork_refs.contains("a-alias"), "{fork_refs}");
        assert!(fork_refs.contains("b-alias"), "{fork_refs}");
        assert!(fork_refs.contains("c-alias"), "{fork_refs}");

        // Nothing was ever pushed to the parent -- its bare repo stays empty.
        let parent_refs = g(
            &parent_bare,
            &["for-each-ref", "--format=%(refname:short)", "refs/heads"],
        );
        assert!(
            parent_refs.trim().is_empty(),
            "the parent must never receive a git push in fork mode: {parent_refs}"
        );

        // Three-branch base chaining: root -> the guardian's own base
        // branch, each successor -> the preceding branch's own alias.
        let rows = store
            .lock()
            .unwrap()
            .list_pull_requests_for_guardian(&gid)
            .unwrap();
        let base_of = |alias: &str| {
            rows.iter()
                .find(|p| p.branch_alias == alias)
                .unwrap()
                .base_ref
                .clone()
        };
        assert_eq!(base_of("a-alias"), "release");
        assert_eq!(base_of("b-alias"), "a-alias");
        assert_eq!(base_of("c-alias"), "b-alias");

        // Only the root is filed at the parent; both successors stay
        // fork-internal, matching the routing table's asymmetry.
        let repo_of = |alias: &str| {
            rows.iter()
                .find(|p| p.branch_alias == alias)
                .unwrap()
                .repo
                .clone()
        };
        assert_eq!(repo_of("a-alias"), "acme/widget");
        assert_eq!(repo_of("b-alias"), "alice/widget");
        assert_eq!(repo_of("c-alias"), "alice/widget");

        handle.join().unwrap();
        let _ = std::fs::remove_dir_all(&root_dir);
        let _ = std::fs::remove_dir_all(&parent_bare);
        let _ = std::fs::remove_dir_all(&fork_bare);
    }

    #[test]
    fn list_pull_requests_for_guardian_is_ordered() {
        let s = store();
        let gid = s.create_guardian("demo", "main", "/repo").unwrap();
        let id_a = s
            .create_pull_request(
                &gid,
                Some("branch-000000000001"),
                "github",
                "acme/widget",
                "a",
                "main",
                "A",
                "",
                None,
                None,
            )
            .unwrap();
        let id_b = s
            .create_pull_request(
                &gid,
                Some("branch-000000000002"),
                "github",
                "acme/widget",
                "b",
                "a",
                "B",
                "",
                None,
                None,
            )
            .unwrap();
        let prs = s.list_pull_requests_for_guardian(&gid).unwrap();
        assert_eq!(
            prs.iter().map(|p| p.id.clone()).collect::<Vec<_>>(),
            vec![id_a, id_b]
        );
    }

    #[test]
    fn find_pull_request_by_number_both_directions() {
        let s = store();
        let gid = s.create_guardian("demo", "main", "/repo").unwrap();
        let id = s
            .create_pull_request(
                &gid,
                None,
                "gitlab",
                "group%2Fproj",
                "review/demo",
                "main",
                "Demo",
                "",
                Some(7),
                None,
            )
            .unwrap();
        let found = s
            .find_pull_request_by_number("gitlab", "group%2Fproj", 7)
            .unwrap()
            .unwrap();
        assert_eq!(found.id, id);
        assert!(
            s.find_pull_request_by_number("gitlab", "group%2Fproj", 999)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn update_pull_request_mutates_number_and_alias() {
        let s = store();
        let gid = s.create_guardian("demo", "main", "/repo").unwrap();
        let id = s
            .create_pull_request(
                &gid,
                Some("branch-000000000001"),
                "github",
                "acme/widget",
                "old-alias",
                "main",
                "T",
                "D",
                Some(1),
                None,
            )
            .unwrap();
        s.update_pull_request(&id, Some(Some(2)), None, Some("new-alias"), Some("closed"))
            .unwrap();
        let pr = s.get_pull_request(&id).unwrap();
        assert_eq!(pr.pr_number, Some(2));
        assert_eq!(pr.branch_alias, "new-alias");
        assert_eq!(pr.state, "closed");
        // pr_url untouched since we passed None for it.
        assert_eq!(pr.pr_url, None);
    }

    #[test]
    fn update_pull_request_missing_id_is_not_found() {
        let s = store();
        assert!(matches!(
            s.update_pull_request("pr-nope", Some(Some(1)), None, None, None),
            Err(StoreError::NotFound)
        ));
    }

    #[test]
    fn actioned_comments_are_idempotent_and_filterable() {
        let s = store();
        let gid = s.create_guardian("demo", "main", "/repo").unwrap();
        let id = s
            .create_pull_request(
                &gid,
                Some("branch-000000000001"),
                "github",
                "acme/widget",
                "a",
                "main",
                "A",
                "",
                Some(1),
                None,
            )
            .unwrap();
        s.mark_pr_comment_actioned(&id, "c1").unwrap();
        s.mark_pr_comment_actioned(&id, "c1").unwrap(); // idempotent
        s.mark_pr_comment_actioned(&id, "c2").unwrap();
        let ids = s.actioned_pr_comment_ids(&id).unwrap();
        assert_eq!(ids.len(), 2);
        assert!(ids.contains("c1") && ids.contains("c2"));
    }

    #[test]
    fn sanitize_branch_name_replaces_unsafe_chars() {
        assert_eq!(
            sanitize_branch_name("feature: fix bug!"),
            "feature--fix-bug-"
        );
        assert_eq!(sanitize_branch_name("  "), "review");
        assert_eq!(
            sanitize_branch_name("PIPE-1234-some_description"),
            "PIPE-1234-some_description"
        );
    }

    // ── resolve_pr_alias (RAL-307) ──────────────────────────────────────────

    #[test]
    fn resolve_pr_alias_defaults_to_convention_when_nothing_opts_in() {
        assert_eq!(
            resolve_pr_alias(None, None, false, "{name}-review", "feature-x"),
            "feature-x-review"
        );
    }

    #[test]
    fn resolve_pr_alias_explicit_branch_alias_always_wins() {
        // Wins over both a request-level override and the review's own
        // persisted setting.
        assert_eq!(
            resolve_pr_alias(
                Some("custom-alias"),
                Some(true),
                true,
                "{name}-review",
                "feature-x"
            ),
            "custom-alias"
        );
    }

    #[test]
    fn resolve_pr_alias_uses_worktree_branch_name_from_per_request_override() {
        assert_eq!(
            resolve_pr_alias(None, Some(true), false, "{name}-review", "feature-x"),
            "feature-x"
        );
    }

    #[test]
    fn resolve_pr_alias_uses_worktree_branch_name_from_review_default() {
        assert_eq!(
            resolve_pr_alias(None, None, true, "{name}-review", "feature-x"),
            "feature-x"
        );
    }

    #[test]
    fn resolve_pr_alias_per_request_override_wins_over_review_default() {
        // Explicit `Some(false)` opts back out even when the review's own
        // setting is on.
        assert_eq!(
            resolve_pr_alias(None, Some(false), true, "{name}-review", "feature-x"),
            "feature-x-review"
        );
    }

    #[test]
    fn strip_remote_prefix_only_strips_matching_remote() {
        assert_eq!(strip_remote_prefix("origin/main", "origin"), "main");
        assert_eq!(strip_remote_prefix("main", "origin"), "main");
        assert_eq!(
            strip_remote_prefix("upstream/main", "origin"),
            "upstream/main"
        );
    }

    #[test]
    fn parse_suggested_pr_handles_wrapped_json() {
        let (t, d) = parse_suggested_pr(
            "Sure! {\"title\": \"Fix bug\", \"description\": \"Fixes it.\"} done.",
        )
        .unwrap();
        assert_eq!(t, "Fix bug");
        assert_eq!(d, "Fixes it.");
    }

    #[test]
    fn parse_suggested_pr_rejects_garbage() {
        assert!(parse_suggested_pr("no json here").is_none());
    }

    // ── RAL-317: maybe_auto_submit_branch (per-branch auto-submit trigger) ──
    //
    // These deliberately never push over a real git transport or hit a real
    // (or mock-HTTP) forge -- there's no portable, hang-proof way to fake a
    // local push target that `crate::forge::resolve_remote`'s host/path
    // parser also accepts (a plain local remote path doesn't parse as a
    // forge host; `git remote get-url` itself resolves `url.*.insteadOf`
    // rewrites, so that trick can't decouple "real local push target" from
    // "forge-parseable URL" either -- and this workspace's Windows dev
    // environment has no `git-daemon` to fall back on). Every existing
    // `pr.rs` test that touches `submit_stacked_branch_pr`'s push+create
    // path has the same gap, so these follow the same scope precedent:
    // exercise `maybe_auto_submit_branch`'s own decision logic (effective-
    // option gate, cheap local-only diff, error bookkeeping) with a real
    // git repo but no reachable forge, rather than the full submission
    // machinery it delegates to on a cache miss.

    struct NoopRunner;
    impl Runner for NoopRunner {
        fn run(&self, _spec: &RunnerSpec) -> crate::runner::RunnerResult {
            // `synthesize_pr_text` falls back to a plain title/summary
            // whenever the runner doesn't report success -- these tests
            // don't need a real agent call, just a deterministic PR body.
            crate::runner::RunnerResult::failure("noop runner: no agent available in this test")
        }
    }

    /// A repo with one commit on `main` and a `feature/x` branch with one
    /// commit ahead of it (checked back out to `main` before returning).
    /// Deliberately has no `origin` remote configured -- see the module note
    /// above for why these tests never attempt a real push/forge call.
    fn setup_auto_submit_repo(tag: &str) -> PathBuf {
        let root = tmp_dir(tag);
        g(&root, &["init", "--initial-branch", "main"]);
        gwrite(&root, "base.txt", "base\n");
        g(&root, &["add", "."]);
        g(&root, &["commit", "--message", "base"]);

        g(&root, &["checkout", "-b", "feature/x"]);
        gwrite(&root, "x.txt", "from x\n");
        g(&root, &["add", "."]);
        g(&root, &["commit", "--message", "add x"]);
        g(&root, &["checkout", "main"]);
        root
    }

    /// Set up a guardian with one enabled, `done` branch whose review ref is
    /// `feature/x`, returning `(guardian_id, branch_id, feature/x's tip sha)`.
    fn setup_terminal_branch(
        store: &Arc<Mutex<Store>>,
        root: &Path,
        auto_submit: bool,
    ) -> (String, String, String) {
        let gid = store
            .lock()
            .unwrap()
            .create_guardian("demo", "main", root.to_str().unwrap())
            .unwrap();
        if auto_submit {
            store
                .lock()
                .unwrap()
                .set_guardian_auto_submit_pr_stack(&gid, Some(true))
                .unwrap();
        }
        store
            .lock()
            .unwrap()
            .add_guardian_branch(&gid, "feature/x")
            .unwrap();
        let bid = store.lock().unwrap().get_guardian(&gid).unwrap().branches[0]
            .id
            .clone();
        store
            .lock()
            .unwrap()
            .set_branch_review(&gid, &bid, "feature/x", "")
            .unwrap();
        store
            .lock()
            .unwrap()
            .set_branch_status(&gid, &bid, crate::guardian::MergeStatus::Done, None)
            .unwrap();
        let tip = g(root, &["rev-parse", "feature/x"]).trim().to_string();
        (gid, bid, tip)
    }

    #[test]
    fn maybe_auto_submit_branch_is_noop_when_effective_option_is_off() {
        let root = setup_auto_submit_repo("auto-submit-off");
        let store = Arc::new(Mutex::new(store()));
        // effective_auto_submit_pr_stack defaults to false (no override, no
        // registered project default) -- left untouched deliberately.
        let (gid, bid, _tip) = setup_terminal_branch(&store, &root, false);

        maybe_auto_submit_branch(&store, &NoopRunner, &gid, &bid);

        let prs = store
            .lock()
            .unwrap()
            .list_pull_requests_for_guardian(&gid)
            .unwrap();
        assert!(prs.is_empty(), "auto-submit must be a no-op when disabled");
        let gv = store.lock().unwrap().get_guardian(&gid).unwrap();
        assert_eq!(
            gv.branches
                .iter()
                .find(|b| b.id == bid)
                .unwrap()
                .auto_submit_error,
            None
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn maybe_auto_submit_branch_skips_forge_entirely_when_already_covered_by_a_matching_pr() {
        let root = setup_auto_submit_repo("auto-submit-covered");
        let store = Arc::new(Mutex::new(store()));
        let (gid, bid, tip) = setup_terminal_branch(&store, &root, true);

        // Simulate a PR already open for this exact branch state (whether
        // from a prior successful auto-submit, or a human running
        // `review pr submit` manually) -- `last_pushed_sha` matches the
        // branch's current tip exactly.
        store
            .lock()
            .unwrap()
            .create_pull_request(
                &gid,
                Some(&bid),
                "github",
                "acme/widget",
                "feature-x-review",
                "main",
                "Add x",
                "",
                Some(5),
                None,
            )
            .unwrap();
        let existing_id = store
            .lock()
            .unwrap()
            .list_pull_requests_for_guardian(&gid)
            .unwrap()[0]
            .id
            .clone();
        store
            .lock()
            .unwrap()
            .update_pull_request_ex(
                &existing_id,
                None,
                None,
                None,
                None,
                None,
                Some(Some(tip.as_str())),
                None,
            )
            .unwrap();

        // This guardian has NO git remote configured at all -- if
        // `maybe_auto_submit_branch` attempted forge resolution despite
        // already being covered, it would fail fast and record an error
        // (proven by the next test), so "no error recorded" here is direct
        // evidence the cheap diff short-circuited before any forge call.
        maybe_auto_submit_branch(&store, &NoopRunner, &gid, &bid);

        let prs = store
            .lock()
            .unwrap()
            .list_pull_requests_for_guardian(&gid)
            .unwrap();
        assert_eq!(
            prs.len(),
            1,
            "must not create a second pr for the same branch state"
        );
        let gv = store.lock().unwrap().get_guardian(&gid).unwrap();
        assert_eq!(
            gv.branches
                .iter()
                .find(|b| b.id == bid)
                .unwrap()
                .auto_submit_error,
            None
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn maybe_auto_submit_branch_records_error_on_forge_resolution_failure_without_touching_branch_status()
     {
        let root = setup_auto_submit_repo("auto-submit-fail");
        let store = Arc::new(Mutex::new(store()));
        // No git remote configured -- `resolve_remote` fails fast (no
        // network attempted, no hang) with "could not read remote 'origin'".
        let (gid, bid, _tip) = setup_terminal_branch(&store, &root, true);

        maybe_auto_submit_branch(&store, &NoopRunner, &gid, &bid);

        // The failed forge resolution must never be surfaced as a merge-
        // blocking error: the branch's own terminal status (already
        // recorded by the caller before this side effect ever runs) and the
        // guardian's overall status are both untouched.
        let gv = store.lock().unwrap().get_guardian(&gid).unwrap();
        assert_eq!(gv.status, "collecting");
        let branch = gv.branches.iter().find(|b| b.id == bid).unwrap();
        assert_eq!(branch.merge_status, "done");
        assert!(
            branch.auto_submit_error.is_some(),
            "a forge resolution failure must be recorded as a per-branch marker"
        );
        assert!(
            store
                .lock()
                .unwrap()
                .list_pull_requests_for_guardian(&gid)
                .unwrap()
                .is_empty(),
            "a failed submission must not leave a partial pr row behind"
        );

        // Once the branch's state is otherwise covered (e.g. a human
        // resubmitted manually), the stale failure marker is cleared even
        // though it was never cleared by a fresh auto-submit success --
        // `maybe_auto_submit_branch`'s cheap-diff path clears it too, since
        // finding the state already healthy is itself evidence there's
        // nothing left to report as failing.
        store
            .lock()
            .unwrap()
            .create_pull_request(
                &gid,
                Some(&bid),
                "github",
                "acme/widget",
                "feature-x-review",
                "main",
                "Add x",
                "",
                Some(9),
                None,
            )
            .unwrap();
        let pr_id = store
            .lock()
            .unwrap()
            .list_pull_requests_for_guardian(&gid)
            .unwrap()[0]
            .id
            .clone();
        let tip = g(&root, &["rev-parse", "feature/x"]).trim().to_string();
        store
            .lock()
            .unwrap()
            .update_pull_request_ex(
                &pr_id,
                None,
                None,
                None,
                None,
                None,
                Some(Some(tip.as_str())),
                None,
            )
            .unwrap();

        maybe_auto_submit_branch(&store, &NoopRunner, &gid, &bid);
        let gv = store.lock().unwrap().get_guardian(&gid).unwrap();
        assert_eq!(
            gv.branches
                .iter()
                .find(|b| b.id == bid)
                .unwrap()
                .auto_submit_error,
            None,
            "the marker must clear once the branch's state is covered again"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    // ── RAL-317: review pr unlink (bulk drop + stack-number clear) ─────────

    #[test]
    fn bulk_drop_open_pull_requests_drops_only_open_rows_and_is_a_noop_when_none_are_open() {
        let s = store();
        let gid = s.create_guardian("demo", "main", "/repo").unwrap();
        let open_id = s
            .create_pull_request(
                &gid,
                Some("branch-000000000001"),
                "github",
                "acme/widget",
                "a",
                "main",
                "A",
                "",
                Some(1),
                None,
            )
            .unwrap();
        let already_merged_id = s
            .create_pull_request(
                &gid,
                Some("branch-000000000002"),
                "github",
                "acme/widget",
                "b",
                "main",
                "B",
                "",
                Some(2),
                None,
            )
            .unwrap();
        s.update_pull_request(&already_merged_id, None, None, None, Some("merged"))
            .unwrap();

        let dropped = s.bulk_drop_open_pull_requests(&gid, "unlinked").unwrap();
        assert_eq!(dropped, 1, "only the still-open row is dropped");

        let open_pr = s.get_pull_request(&open_id).unwrap();
        assert_eq!(open_pr.state, "dropped");
        let merged_pr = s.get_pull_request(&already_merged_id).unwrap();
        assert_eq!(
            merged_pr.state, "merged",
            "an already-merged row is left untouched, not force-dropped"
        );

        // Calling it again with nothing left open is a silent no-op, not an
        // error (matches the bulk-friendly precedent, unlike the single-row
        // `drop_pull_request`'s `NotFound`).
        assert_eq!(s.bulk_drop_open_pull_requests(&gid, "unlinked").unwrap(), 0);

        // Dropped rows remain visible as history via `PrStackView`.
        let stacks = group_into_stacks(s.list_pull_requests_for_guardian(&gid).unwrap());
        let all_prs: Vec<_> = stacks.iter().flat_map(|s| &s.prs).collect();
        assert_eq!(all_prs.len(), 2, "dropped rows are never hard-deleted");
    }

    #[test]
    fn clear_guardian_forge_stack_number_resets_to_unset() {
        let s = store();
        let gid = s.create_guardian("demo", "main", "/repo").unwrap();
        s.set_guardian_forge_stack_number(&gid, 42).unwrap();
        assert_eq!(s.get_guardian_forge_stack_number(&gid).unwrap(), Some(42));

        s.clear_guardian_forge_stack_number(&gid).unwrap();
        assert_eq!(s.get_guardian_forge_stack_number(&gid).unwrap(), None);

        assert!(matches!(
            s.clear_guardian_forge_stack_number("nope"),
            Err(StoreError::NotFound)
        ));
    }
}
