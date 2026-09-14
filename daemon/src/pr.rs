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
    /// RAL-395: the last polled CI/CD status -- `"pending"`, `"passing"`, or
    /// `"failing"` (mirrors [`crate::forge::PrCiState::as_str`]). `None` for
    /// a PR never polled yet.
    pub ci_status: Option<String>,
    /// RAL-395: the failing job's forge URL from the most recent `Failing`
    /// poll. `None` when the last poll wasn't failing, or failed with no job
    /// URL (e.g. a forge-verdict merge conflict).
    pub ci_failure_job_url: Option<String>,
    /// RAL-395: when auto-fix was last dispatched for the *current* failing
    /// CI state on this PR -- caps auto-fix at a single attempt per failure.
    /// `None` if never attempted for the current failure (or the PR isn't
    /// currently failing).
    pub auto_fix_attempted_at_ms: Option<i64>,
    /// RAL-353: whether the forge currently reports this PR/MR as a draft
    /// (WIP). Written at create/adopt time from the forge's own `draft`
    /// field, then kept fresh by every CI probe
    /// (`crate::forge::ForgeClient::check_pr_ci_status_probe`, which reads it
    /// from the same response as the CI verdict). `None` for a row recorded
    /// before the column existed and never polled since -- the board treats
    /// that as not-draft.
    pub draft: Option<bool>,
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
    /// RAL-395: see [`PullRequestView::ci_status`].
    pub ci_status: Option<String>,
    /// RAL-353: see [`PullRequestView::draft`].
    pub draft: Option<bool>,
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
    ci_status: Option<String>,
    ci_failure_job_url: Option<String>,
    auto_fix_attempted_at_ms: Option<i64>,
    draft: Option<bool>,
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
            ci_status: r.ci_status,
            ci_failure_job_url: r.ci_failure_job_url,
            auto_fix_attempted_at_ms: r.auto_fix_attempted_at_ms,
            draft: r.draft,
        }
    }
}

const PR_COLUMNS: &str = "id, guardian_id, branch_id, forge, repo, branch_alias, base_ref, title, description, pr_number, pr_url, state, created_at_ms, updated_at_ms, last_pushed_sha, last_pushed_base_ref, stack_id, dropped_reason, superseded_by, ci_status, ci_failure_job_url, auto_fix_attempted_at_ms, draft";

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
        ci_status: r.get(19)?,
        ci_failure_job_url: r.get(20)?,
        auto_fix_attempted_at_ms: r.get(21)?,
        draft: r.get(22)?,
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
            false,
        )
    }

    /// Full form of [`Self::create_pull_request`] that also stamps `stack_id`
    /// (RAL-302): the same value passed for every PR row created by one
    /// "submit a stack" call, so those sibling rows are queryable as a single
    /// past submission later, even if some of them are since dropped
    /// (see [`Self::drop_pull_request`]). `draft` (RAL-353) is the forge's
    /// own draft (WIP) state from the create/adopt response, recorded verbatim
    /// so an adoption/refresh can't clobber it.
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
        draft: bool,
    ) -> Result<String> {
        let id = self.next_id("guardian_pr_seq", "pr")?;
        let now = now_ms();
        self.conn.execute(
            "INSERT INTO guardian_pull_requests(
                id, guardian_id, branch_id, forge, repo, branch_alias, base_ref,
                title, description, pr_number, pr_url, state, created_at_ms, updated_at_ms,
                stack_id, draft
             ) VALUES(?,?,?,?,?,?,?,?,?,?,?,'open',?,?,?,?)",
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
                draft,
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
                    s.idx AS source_cell_idx,
                    pr.ci_status,
                    pr.draft
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
                    ci_status: r.get(14)?,
                    draft: r.get(15)?,
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

    /// Atomically claim a batch of PR comment ids as actioned, returning only
    /// the ids this call actually won -- an id already claimed (by an earlier
    /// call, or one racing concurrently) is silently excluded rather than
    /// re-actioned. Each id is inserted via its own `INSERT ... WHERE NOT
    /// EXISTS`, a single atomic statement, so two overlapping callers that
    /// both read the same "not yet actioned" comments from the forge can
    /// never both win the same id: whichever statement runs second finds the
    /// row already there and claims nothing for it.
    ///
    /// Callers must claim a comment *before* applying its feedback, not
    /// after -- the same ordering [`crate::ci_watch::dispatch_pr_auto_fix`]
    /// uses for `mark_pr_auto_fix_attempted`, and for the same reason: a
    /// claim recorded only after the (potentially slow) feedback round
    /// finishes leaves a window where a second caller can start applying the
    /// same comments before the first one's claim lands.
    pub fn try_claim_pr_comments(
        &self,
        pr_id: &str,
        external_comment_ids: &[String],
    ) -> Result<Vec<String>> {
        let mut won = Vec::new();
        for external_comment_id in external_comment_ids {
            let n = self.conn.execute(
                "INSERT INTO guardian_pr_feedback_actioned(id, pr_id, external_comment_id, actioned_at_ms)
                 SELECT ?, ?, ?, ? WHERE NOT EXISTS (
                     SELECT 1 FROM guardian_pr_feedback_actioned WHERE pr_id=? AND external_comment_id=?
                 )",
                params![
                    format!("{pr_id}:{external_comment_id}"),
                    pr_id,
                    external_comment_id,
                    now_ms(),
                    pr_id,
                    external_comment_id,
                ],
            )?;
            if n > 0 {
                won.push(external_comment_id.clone());
            }
        }
        Ok(won)
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

    /// Record the result of the most recent CI/CD poll for this PR (RAL-395)
    /// -- `status` is one of `"pending"`/`"passing"`/`"failing"` (mirrors
    /// [`crate::forge::PrCiState::as_str`]). `job_url` is the failing job's
    /// forge URL when `status == "failing"` and one exists, `None`
    /// otherwise. Clears `auto_fix_attempted_at_ms` whenever the status is
    /// anything other than `"failing"`, so a *new* failure (after a passing
    /// or pending interval) gets a fresh auto-fix attempt rather than being
    /// permanently capped by a stale marker from a prior failure.
    pub fn set_pr_ci_status(&self, id: &str, status: &str, job_url: Option<&str>) -> Result<()> {
        let n = self.conn.execute(
            "UPDATE guardian_pull_requests
             SET ci_status=?, ci_failure_job_url=?, updated_at_ms=?,
                 auto_fix_attempted_at_ms = CASE WHEN ?='failing' THEN auto_fix_attempted_at_ms ELSE NULL END
             WHERE id=?",
            params![status, job_url, now_ms(), status, id],
        )?;
        if n == 0 {
            Err(StoreError::NotFound)
        } else {
            Ok(())
        }
    }

    /// RAL-353: record a PR/MR's current draft (WIP) state as observed from
    /// the forge -- written at create/adopt time and refreshed by every CI
    /// probe (`crate::forge::ForgeClient::check_pr_ci_status_probe`), which
    /// reads it from the same PR response that yields the CI verdict, so the
    /// two can never disagree about which forge observation they came from.
    pub fn set_pr_draft(&self, id: &str, draft: bool) -> Result<()> {
        let n = self.conn.execute(
            "UPDATE guardian_pull_requests SET draft=?, updated_at_ms=? WHERE id=?",
            params![draft, now_ms(), id],
        )?;
        if n == 0 {
            Err(StoreError::NotFound)
        } else {
            Ok(())
        }
    }

    /// Mark that auto-fix has just been dispatched for this PR's *current*
    /// failing CI state (RAL-395) -- caps auto-fix at a single attempt per
    /// failure (interview Q5). Cleared automatically by
    /// [`Self::set_pr_ci_status`] the next time the PR is observed as
    /// anything other than `"failing"`.
    pub fn mark_pr_auto_fix_attempted(&self, id: &str) -> Result<()> {
        let n = self.conn.execute(
            "UPDATE guardian_pull_requests SET auto_fix_attempted_at_ms=?, updated_at_ms=? WHERE id=?",
            params![now_ms(), now_ms(), id],
        )?;
        if n == 0 {
            Err(StoreError::NotFound)
        } else {
            Ok(())
        }
    }

    // -----------------------------------------------------------------
    // RAL-366: cached forge state (background poller)
    // -----------------------------------------------------------------
    // See `HalfOutcome` below for how a pass reports each half.

    /// Upsert the background poller's most recent probe of one PR.
    ///
    /// A poll pass has two independent halves -- git-side drift and the
    /// forge-side comment fetch -- and either can succeed, fail, or not be
    /// attempted at all without the other being affected (the drift lock was
    /// held by an interactive `sync-status` call this cycle; the forge
    /// rejected the token). Each is therefore passed as its own
    /// [`HalfOutcome`], and each records its own timestamp, status and error.
    ///
    /// This matters because the row is read to decide whether cached data is
    /// fresh enough to serve. One shared timestamp cannot answer that: a pass
    /// that refreshed only comments would still stamp the row "just checked",
    /// and a caller asking about drift would be handed hours-old values
    /// labelled as current.
    ///
    /// Value columns are `COALESCE`d, so a half that failed or was skipped
    /// leaves the other half's last-known-good values untouched.
    /// `last_checked_at_ms`/`status`/`last_error` remain as the rolled-up
    /// "most recent of either", for callers that only want one number.
    pub(crate) fn upsert_pr_forge_cache(
        &self,
        pr_id: &str,
        drift: HalfOutcome<DriftObservation<'_>>,
        comments: HalfOutcome<CommentsObservation<'_>>,
    ) -> Result<()> {
        let drift_ok = drift.as_ref().map(std::result::Result::is_ok);
        let comments_ok = comments.as_ref().map(std::result::Result::is_ok);
        // The rolled-up status is pessimistic on purpose: if either half this
        // pass attempted failed, the row as a whole is not trustworthy.
        let ok = drift_ok.unwrap_or(true) && comments_ok.unwrap_or(true);
        let half_status = |o: Option<bool>| o.map(|ok| if ok { "ok" } else { "unknown" });
        let drift_status = half_status(drift_ok);
        let comments_status = half_status(comments_ok);
        let drift_error = drift.as_ref().and_then(|r| r.as_ref().err().cloned());
        let comments_error = comments.as_ref().and_then(|r| r.as_ref().err().cloned());
        let last_error = drift_error.clone().or_else(|| comments_error.clone());
        let observed = drift.and_then(std::result::Result::ok);
        let etags = comments.and_then(std::result::Result::ok);
        let (in_sync, pr_ahead, worktree_ahead, remote_sha, local_sha) = match &observed {
            Some(d) => (
                Some(d.in_sync),
                Some(d.pr_ahead),
                Some(d.worktree_ahead),
                d.remote_sha,
                d.local_sha,
            ),
            None => (None, None, None, None, None),
        };
        let (etag_conversation, etag_review) = match &etags {
            Some(c) => (c.etag_conversation, c.etag_review),
            None => (None, None),
        };
        let stamp_if = |attempted: Option<bool>| attempted.map(|_| now_ms());
        let drift_checked_at_ms = stamp_if(drift_ok);
        let comments_checked_at_ms = stamp_if(comments_ok);
        let last_error = last_error.as_deref();
        let drift_error = drift_error.as_deref();
        let comments_error = comments_error.as_deref();
        self.conn.execute(
            "INSERT INTO guardian_pr_forge_cache(
                pr_id, last_checked_at_ms, status, last_error,
                in_sync, pr_ahead, worktree_ahead, remote_sha, local_sha,
                etag_conversation, etag_review,
                drift_checked_at_ms, drift_status, drift_error,
                comments_checked_at_ms, comments_status, comments_error
             ) VALUES(?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)
             ON CONFLICT(pr_id) DO UPDATE SET
                last_checked_at_ms = excluded.last_checked_at_ms,
                status             = excluded.status,
                last_error         = excluded.last_error,
                in_sync            = COALESCE(excluded.in_sync, in_sync),
                pr_ahead           = COALESCE(excluded.pr_ahead, pr_ahead),
                worktree_ahead     = COALESCE(excluded.worktree_ahead, worktree_ahead),
                remote_sha         = COALESCE(excluded.remote_sha, remote_sha),
                local_sha          = COALESCE(excluded.local_sha, local_sha),
                etag_conversation  = COALESCE(excluded.etag_conversation, etag_conversation),
                etag_review        = COALESCE(excluded.etag_review, etag_review),
                drift_checked_at_ms    = COALESCE(excluded.drift_checked_at_ms, drift_checked_at_ms),
                drift_status           = COALESCE(excluded.drift_status, drift_status),
                drift_error            = CASE WHEN excluded.drift_status IS NULL
                                              THEN drift_error ELSE excluded.drift_error END,
                comments_checked_at_ms = COALESCE(excluded.comments_checked_at_ms, comments_checked_at_ms),
                comments_status        = COALESCE(excluded.comments_status, comments_status),
                comments_error         = CASE WHEN excluded.comments_status IS NULL
                                              THEN comments_error ELSE excluded.comments_error END",
            params![
                pr_id,
                now_ms(),
                if ok { "ok" } else { "unknown" },
                last_error,
                in_sync,
                pr_ahead,
                worktree_ahead,
                remote_sha,
                local_sha,
                etag_conversation,
                etag_review,
                drift_checked_at_ms,
                drift_status,
                drift_error,
                comments_checked_at_ms,
                comments_status,
                comments_error,
            ],
        )?;
        Ok(())
    }

    /// Upsert only the branch-drift half of the RAL-366 cache row for one PR
    /// (RAL-423): the poller's *drift pass* refreshes `in_sync`/`pr_ahead`/
    /// `worktree_ahead`/`remote_sha`/`local_sha` while holding that PR's own
    /// fetch lock, and must drop every such lock before its *comment pass*
    /// issues forge network calls. The status/etag columns are deliberately
    /// untouched here -- `ok`/`last_error`/etags describe the forge-comment
    /// probe, which the comment pass owns (and which runs after the drift
    /// locks are released); a drift-only write must neither make a row read
    /// as freshly "checked" when its comment probe hasn't run this cycle,
    /// nor transiently flip its status, nor evict a prior pass's etags. On a
    /// fresh row (first poll ever) the placeholder `status='unknown'` is
    /// corrected moments later by the same poll's comment-pass upsert.
    pub(crate) fn upsert_pr_forge_cache_drift(
        &self,
        pr_id: &str,
        in_sync: bool,
        pr_ahead: bool,
        worktree_ahead: bool,
        remote_sha: Option<&str>,
        local_sha: Option<&str>,
    ) -> Result<()> {
        self.conn.execute(
            "INSERT INTO guardian_pr_forge_cache(
                pr_id, last_checked_at_ms, status, last_error,
                in_sync, pr_ahead, worktree_ahead, remote_sha, local_sha,
                etag_conversation, etag_review
             ) VALUES(?,?,?,?,?,?,?,?,?,?,?)
             ON CONFLICT(pr_id) DO UPDATE SET
                in_sync            = excluded.in_sync,
                pr_ahead           = excluded.pr_ahead,
                worktree_ahead     = excluded.worktree_ahead,
                remote_sha         = excluded.remote_sha,
                local_sha          = excluded.local_sha",
            params![
                pr_id,
                now_ms(),
                "unknown",
                None::<&str>,
                in_sync,
                pr_ahead,
                worktree_ahead,
                remote_sha,
                local_sha,
                None::<&str>,
                None::<&str>,
            ],
        )?;
        Ok(())
    }

    /// Read this PR's own stored ETags (RAL-366), if a prior poll pass ever
    /// recorded one -- so the next conditional fetch can send
    /// `If-None-Match` and cost no forge quota when nothing changed.
    pub(crate) fn pr_forge_cache_etags(
        &self,
        pr_id: &str,
    ) -> Result<(Option<String>, Option<String>)> {
        Ok(self
            .conn
            .query_row(
                "SELECT etag_conversation, etag_review FROM guardian_pr_forge_cache WHERE pr_id=?",
                params![pr_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?
            .unwrap_or((None, None)))
    }

    /// Wholesale-replace the comment/note ids the RAL-366 poller last fetched
    /// for `(pr_id, endpoint)` -- called only after a fresh (non-304)
    /// conditional fetch, so a comment deleted on the forge between polls
    /// disappears here too. Never stores comment bodies -- see
    /// `guardian_pr_forge_comments`'s schema comment for why.
    pub(crate) fn replace_pr_forge_comments(
        &self,
        pr_id: &str,
        endpoint: &str,
        comments: &[crate::forge::PrComment],
    ) -> Result<()> {
        // One transaction: the delete and the re-insert are a single
        // replacement. Run loose, a failure part-way through leaves the PR
        // showing a truncated comment set -- which reads as "reviewers
        // withdrew their feedback", not as an error.
        let tx = self.conn.unchecked_transaction()?;
        tx.execute(
            "DELETE FROM guardian_pr_forge_comments WHERE pr_id=? AND endpoint=?",
            params![pr_id, endpoint],
        )?;
        {
            let mut stmt = tx.prepare(
                "INSERT OR REPLACE INTO guardian_pr_forge_comments(
                    pr_id, endpoint, external_id, author, created_at
                 ) VALUES(?,?,?,?,?)",
            )?;
            for c in comments {
                stmt.execute(params![
                    pr_id,
                    endpoint,
                    c.external_id,
                    c.author,
                    c.created_at
                ])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Drop cached forge state for every PR that is no longer open.
    ///
    /// The poller only visits open PRs, so once a PR merges or closes its row
    /// stops refreshing but keeps being returned by
    /// [`Self::list_pr_forge_cache`] at whatever values it last held. The
    /// `ON DELETE CASCADE` does not help: PR rows are soft-closed, not
    /// deleted. Without this the table only ever grows, and every list view
    /// has to filter it.
    pub(crate) fn prune_pr_forge_cache(&self) -> Result<usize> {
        let tx = self.conn.unchecked_transaction()?;
        tx.execute(
            "DELETE FROM guardian_pr_forge_comments WHERE pr_id IN (
                 SELECT id FROM guardian_pull_requests WHERE state <> 'open')",
            [],
        )?;
        let n = tx.execute(
            "DELETE FROM guardian_pr_forge_cache WHERE pr_id IN (
                 SELECT id FROM guardian_pull_requests WHERE state <> 'open')",
            [],
        )?;
        tx.commit()?;
        Ok(n)
    }

    /// This PR's cached forge state (RAL-366), if the background poller has
    /// ever reached it -- `None` for a PR never polled yet (e.g. just
    /// submitted this cycle). Comment count/un-actioned count/latest-comment
    /// fields are always a live join against `guardian_pr_forge_comments` and
    /// `guardian_pr_feedback_actioned`, never a second cached integer, so
    /// they can never drift out of sync with those tables' own data.
    pub fn get_pr_forge_cache(&self, pr_id: &str) -> Result<Option<PrForgeCacheView>> {
        self.conn
            .query_row(
                &format!("{PR_FORGE_CACHE_SELECT} WHERE c.pr_id=?"),
                params![pr_id],
                map_pr_forge_cache_row,
            )
            .optional()
            .map_err(StoreError::from)
    }

    /// Every PR's cached forge state in one query (RAL-366) -- backs the
    /// list-view read route so a board rendering many rows never makes one
    /// lookup per row. Only PRs the poller has reached at least once appear;
    /// a caller wanting to render every open PR should left-join this
    /// against [`Self::list_pull_requests_index`] on `pr_id`/`id`.
    pub fn list_pr_forge_cache(&self) -> Result<Vec<PrForgeCacheView>> {
        let mut stmt = self.conn.prepare(PR_FORGE_CACHE_SELECT)?;
        let rows = stmt
            .query_map([], map_pr_forge_cache_row)?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }
}

/// Shared `SELECT` behind [`Store::get_pr_forge_cache`]/[`Store::list_pr_forge_cache`]
/// (RAL-366): the comment-derived columns are correlated subqueries rather
/// than a `JOIN ... GROUP BY`, since a plain join would double-count once a
/// PR has both `'conversation'` and `'review'` comment rows.
const PR_FORGE_CACHE_SELECT: &str = "
    SELECT
        c.pr_id, c.last_checked_at_ms, c.status, c.last_error,
        c.in_sync, c.pr_ahead, c.worktree_ahead, c.remote_sha, c.local_sha,
        (SELECT COUNT(*) FROM guardian_pr_forge_comments cm WHERE cm.pr_id = c.pr_id),
        (SELECT COUNT(*) FROM guardian_pr_forge_comments cm
           WHERE cm.pr_id = c.pr_id
             AND NOT EXISTS (
                 SELECT 1 FROM guardian_pr_feedback_actioned a
                  WHERE a.pr_id = cm.pr_id AND a.external_comment_id = cm.external_id
             )),
        (SELECT cm.author FROM guardian_pr_forge_comments cm
          WHERE cm.pr_id = c.pr_id ORDER BY cm.created_at DESC, cm.external_id DESC LIMIT 1),
        (SELECT cm.created_at FROM guardian_pr_forge_comments cm
          WHERE cm.pr_id = c.pr_id ORDER BY cm.created_at DESC, cm.external_id DESC LIMIT 1),
        c.drift_checked_at_ms, c.drift_status, c.drift_error,
        c.comments_checked_at_ms, c.comments_status, c.comments_error
    FROM guardian_pr_forge_cache c";

fn map_pr_forge_cache_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<PrForgeCacheView> {
    Ok(PrForgeCacheView {
        pr_id: r.get(0)?,
        last_checked_at_ms: r.get(1)?,
        status: r.get(2)?,
        last_error: r.get(3)?,
        in_sync: r.get(4)?,
        pr_ahead: r.get(5)?,
        worktree_ahead: r.get(6)?,
        remote_sha: r.get(7)?,
        local_sha: r.get(8)?,
        comment_count: r.get(9)?,
        unactioned_count: r.get(10)?,
        latest_comment_author: r.get(11)?,
        latest_comment_at: r.get(12)?,
        drift_checked_at_ms: r.get(13)?,
        drift_status: r.get(14)?,
        drift_error: r.get(15)?,
        comments_checked_at_ms: r.get(16)?,
        comments_status: r.get(17)?,
        comments_error: r.get(18)?,
    })
}

/// One PR's cached forge state (RAL-366): populated only by the background
/// poller, plus write-throughs from the on-demand `sync-status`/`comments`
/// routes.
///
/// `status`/`last_checked_at_ms`/`last_error` are a rolled-up view of two
/// independently-refreshed halves and are only safe for a coarse "when was
/// this row last touched" display. **Anything deciding whether cached data is
/// fresh enough to act on must read the half it actually cares about**
/// (`drift_*` or `comments_*`): a pass that refreshed only comments still
/// moves `last_checked_at_ms`, so the rolled-up timestamp can read as seconds
/// old while the drift columns are hours stale.
///
/// A `"unknown"` half means the last attempt failed (see its error); the
/// values it covers may still carry a usable reading from an earlier pass.
#[derive(Debug, Clone, Serialize)]
pub struct PrForgeCacheView {
    pub pr_id: String,
    pub last_checked_at_ms: i64,
    pub status: String,
    pub last_error: Option<String>,
    pub in_sync: Option<bool>,
    pub pr_ahead: Option<bool>,
    pub worktree_ahead: Option<bool>,
    pub remote_sha: Option<String>,
    pub local_sha: Option<String>,
    pub comment_count: i64,
    pub unactioned_count: i64,
    pub latest_comment_author: Option<String>,
    pub latest_comment_at: Option<String>,
    /// When the git-side drift half was last *attempted*, and how it went.
    /// `None` until a pass has attempted it at least once.
    pub drift_checked_at_ms: Option<i64>,
    pub drift_status: Option<String>,
    pub drift_error: Option<String>,
    /// When the forge-side comment half was last *attempted*, and how it went.
    pub comments_checked_at_ms: Option<i64>,
    pub comments_status: Option<String>,
    pub comments_error: Option<String>,
}

impl Store {
    /// Undo [`Self::mark_pr_auto_fix_attempted`] (RAL-395 follow-up): called
    /// when [`crate::guardian_merge::run_feedback`] bailed out before the
    /// resolver agent ever ran (e.g. the branch's review worktree was
    /// transiently missing, mid-rebuild, when auto-fix raced the review's own
    /// background merge loop) so that infrastructure hiccup doesn't
    /// permanently consume the single-attempt-per-failure budget while CI
    /// keeps reporting the same `"failing"` status forever.
    pub fn clear_pr_auto_fix_attempted(&self, id: &str) -> Result<()> {
        let n = self.conn.execute(
            "UPDATE guardian_pull_requests SET auto_fix_attempted_at_ms=NULL, updated_at_ms=? WHERE id=?",
            params![now_ms(), id],
        )?;
        if n == 0 {
            Err(StoreError::NotFound)
        } else {
            Ok(())
        }
    }

    /// Give every open PR in a review a fresh unattended auto-fix attempt.
    ///
    /// An explicit Merge / rebase is a user-directed retry boundary. It is
    /// deliberately separate from the background CI poller's one-attempt cap:
    /// ordinary polling must not keep spending attempts on an unchanged
    /// failure, while a person who asks to rebuild the review has requested a
    /// new chance to address it.
    pub fn reset_open_pr_auto_fix_attempts(&self, guardian_id: &str) -> Result<usize> {
        Ok(self.conn.execute(
            "UPDATE guardian_pull_requests
             SET auto_fix_attempted_at_ms=NULL, updated_at_ms=?
             WHERE guardian_id=? AND state='open' AND auto_fix_attempted_at_ms IS NOT NULL",
            params![now_ms(), guardian_id],
        )?)
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

/// The PR branch alias `submit_stacked_branch_pr` pushes to.
///
/// An explicit `branch_alias` always wins verbatim -- it is a literal name the
/// caller asked for, and overrides everything below.
///
/// RAL-378: otherwise the default is the review branch's own readable name
/// (`readable_review_branch`), so the branch the review was built on *is* the
/// branch the PR is opened from. Nothing has to be derived, reconciled or kept
/// in step; pushing the review branch is the whole operation. Returning `None`
/// for that argument (a branch registered before readable naming, whose review
/// ref is still the internal `guardian/<id>/wt-<branch>`) falls through to the
/// derived path below, since an internal ref is not a name to publish.
///
/// `separate_pr_branch` opts back out, restoring the older behavior: the alias
/// is templated from `pr_branch_convention` rather than reusing `branch_name`
/// bare, because an identically-named remote branch would mask the fact that
/// its content is the review's (possibly rebased/conflict-resolved/squashed)
/// output, not the task branch's own commits. RAL-307's
/// `use_worktree_branch_name` (this submission's own override) and
/// `effective_match_pr_branch_name` (the review's persisted setting, used when
/// the override is `None`) accept that masking as an explicit tradeoff and ask
/// for `branch_name` verbatim. Both are read *only* on this path: with a
/// single branch serving both roles there is no second name for them to
/// select, so neither means anything.
fn resolve_pr_alias(
    branch_alias: Option<&str>,
    use_worktree_branch_name: Option<bool>,
    effective_match_pr_branch_name: bool,
    separate_pr_branch: bool,
    readable_review_branch: Option<&str>,
    pr_branch_convention: &str,
    branch_name: &str,
) -> String {
    if let Some(alias) = branch_alias {
        return sanitize_branch_name(alias);
    }
    if !separate_pr_branch {
        if let Some(review_branch) = readable_review_branch.filter(|n| !n.is_empty()) {
            return sanitize_branch_name(review_branch);
        }
    }
    let use_worktree_branch_name =
        use_worktree_branch_name.unwrap_or(effective_match_pr_branch_name);
    sanitize_branch_name(&if use_worktree_branch_name {
        branch_name.to_string()
    } else {
        apply_pr_branch_convention(pr_branch_convention, branch_name)
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

/// Git range a PR for `position` should be described by: the branch's own
/// commits, from its immediate predecessor's tip (or `base_sha` for the lowest
/// enabled position) through this branch's review ref.
fn commit_range_for(
    base_sha: &str,
    guardian: &GuardianView,
    position: i64,
) -> Option<(String, String)> {
    let prev = guardian
        .branches
        .iter()
        .filter(|b| b.position < position && b.enabled)
        .max_by_key(|b| b.position)
        .and_then(|b| b.review_branch.clone())
        .unwrap_or_else(|| base_sha.to_string());
    let tip = guardian
        .branches
        .iter()
        .find(|b| b.position == position)
        .and_then(|b| b.review_branch.clone())?;
    Some((prev, tip))
}

const MAX_PR_PATCH_CHARS: usize = 120_000;

fn truncate_pr_patch(patch: String) -> String {
    if patch.chars().count() <= MAX_PR_PATCH_CHARS {
        return patch;
    }
    let mut truncated: String = patch.chars().take(MAX_PR_PATCH_CHARS).collect();
    truncated.push_str("\n\n[diff truncated by ralphus]\n");
    truncated
}

/// Branch-local evidence supplied to the PR writer. The range is deliberately
/// identical to the range the forge will display for this stacked PR, so no
/// predecessor or downstream sibling can enter the generated description.
fn branch_change_context(
    root: &Path,
    base_sha: &str,
    guardian: &GuardianView,
    position: i64,
) -> Option<(String, String, String)> {
    let (prev, tip) = commit_range_for(base_sha, guardian, position)?;
    let range = format!("{prev}..{tip}");
    let commits = git(
        root,
        &[
            "log",
            "--reverse",
            "--format=commit %H%nsubject: %s%n%n%b%n---",
            &range,
        ],
    )
    .unwrap_or_default();
    if commits.trim().is_empty() {
        return None;
    }
    let diff_stat = git(root, &["diff", "--no-ext-diff", "--stat", &range]).unwrap_or_default();
    let patch = git(root, &["diff", "--no-ext-diff", "--unified=3", &range])
        .map(truncate_pr_patch)
        .unwrap_or_default();
    Some((commits, diff_stat, patch))
}

fn fallback_pr_description(commits: &str, template: Option<&str>) -> String {
    let subjects = commits
        .lines()
        .filter_map(|line| line.strip_prefix("subject: "))
        .map(|subject| format!("- {subject}"))
        .collect::<Vec<_>>()
        .join("\n");
    match (template, subjects.is_empty()) {
        (Some(template), false) => format!("{template}\n\n## Branch changes\n\n{subjects}"),
        (Some(template), true) => template.to_string(),
        (None, _) => subjects,
    }
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

/// Synthesize a suggested PR title + description from one branch's unique
/// commit range, using the same `Runner`/`RunnerSpec` mechanism
/// `guardian_merge::generate_final_summary` uses for the change summary.
/// `template`, if given (see `ForgeClient::fetch_pr_template`), is folded into
/// the prompt so the repo's PR template is honoured. Falls back to a plain
/// branch-name title and a deterministic list of this branch's commit subjects
/// when the agent call fails. The review-wide change summary is never used for
/// an individual PR. `trace_context`
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
    let base_sha = match git(&root, &["rev-parse", &guardian.base_branch]) {
        Ok(s) => s.trim().to_string(),
        Err(_) => return (fallback_title, template.unwrap_or_default().to_string()),
    };
    let Some((commits, diff_stat, patch)) =
        branch_change_context(&root, &base_sha, guardian, position)
    else {
        // ralphus[ignore-rlog-pair]: this low-level helper has no Store; its Store-owning caller records the structured workflow outcome
        crate::rlog!(
            DEBUG,
            "ralphus [pr] synthesize pr text position={position:?} skipped: empty commit log"
        );
        return (fallback_title, template.unwrap_or_default().to_string());
    };
    let fallback_description = fallback_pr_description(&commits, template);

    let agent = guardian_merge::resolver_agent(guardian.resolver_agent.as_deref(), &root);
    let model = guardian_merge::resolver_model(guardian.resolver_model.as_deref(), &agent, &root);
    let template_note = template.map_or_else(String::new, |t| {
        format!(
            "\n\nThe target repository has a pull-request template you MUST \
             follow -- fill it in, keeping its section headers intact:\n\n{t}"
        )
    });
    let prompt = format!(
        "Suggest a pull request title and description for one branch in a \
         stacked review. Use ONLY the commits and diff in the unique range \
         below. Do not describe predecessor branches, downstream branches, \
         the wider review, or unrelated files visible in the workspace.\n\n\
         UNIQUE COMMITS (oldest first):\n\n{commits}\n\n\
         UNIQUE DIFF STAT:\n\n{diff_stat}\n\n\
         UNIQUE DIFF:\n\n{patch}\n\n\
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
        cwd: guardian
            .branches
            .iter()
            .find(|b| b.position == position)
            .and_then(|b| b.worktree.clone())
            .unwrap_or_else(|| guardian.git_root.clone()),
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
pub fn resync_pr_bases(
    store: &crate::store_lock::StoreHandle,
    id: &str,
) -> std::result::Result<usize, String> {
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

/// The remote name to strip/qualify a guardian's `base_branch` against when
/// comparing or writing it relative to a forge PR's bare base ref -- the
/// same fork-excluded [`PrRepoRouting::parent_remote_name`]
/// [`detect_forge_reorder`] itself compares against, kept as a named entry
/// point so [`check_and_apply_forge_reorder`]'s apply step can't drift back
/// to a plain, non-excluding resolution (RAL-273/RAL-338: that mismatch let a
/// project whose registered fork shares `base_branch`'s own remote name loop
/// `merging`<->`in_review` forever, since the "fix" kept re-qualifying with
/// the excluded remote and writing back the exact value the next detection
/// pass would flag as drifted again).
fn forge_parent_remote_name(
    store: &crate::store_lock::StoreHandle,
    root: &Path,
    base_branch: &str,
    forge_cfg: &crate::config::ForgeConfig,
) -> String {
    resolve_pr_repo_routing(store, root, base_branch, forge_cfg).parent_remote_name
}

/// Resolve [`PrRepoRouting`] for `guardian`'s project (by `guardian.git_root`
/// unless the caller has a more specific `root`/project path for a
/// multi-project branch, e.g. [`check_pr_merges`]'s per-branch resolution).
/// The forge client that answers for one PR row's own repository.
///
/// Exposed so the on-demand routes in `server.rs` resolve the *same* client the
/// cache poller does. Under fork routing (RAL-338) a PR's `repo` decides which
/// repository owns it, and that need not be the remote behind the guardian's
/// base branch -- so resolving one way here and another way there had the two
/// reading different repositories and reporting different answers for the same
/// PR.
pub fn forge_client_for_pr(
    store: &crate::store_lock::StoreHandle,
    root: &Path,
    base_branch: &str,
    forge_cfg: &crate::config::ForgeConfig,
    repo: &str,
) -> Option<crate::forge::ForgeClient> {
    resolve_pr_repo_routing(store, root, base_branch, forge_cfg)
        .client_for(repo)
        .cloned()
}

fn resolve_pr_repo_routing(
    store: &crate::store_lock::StoreHandle,
    root: &Path,
    base_branch: &str,
    forge_cfg: &crate::config::ForgeConfig,
) -> PrRepoRouting {
    let project_name = store
        .lock()
        .project_name_for_path(root.to_str().unwrap_or_default());
    let fork = project_name
        .as_deref()
        .and_then(|p| store.lock().resolve_fork(p, "").ok().flatten());
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
    store: &crate::store_lock::StoreHandle,
    root: &Path,
) -> Option<String> {
    let project_name = store.lock().project_name_for_path(root.to_str()?)?;
    let fork = store
        .lock()
        .resolve_fork(&project_name, "")
        .ok()
        .flatten()?;
    crate::project_forks::ensure_fork_remote(root, &fork.remote_name, &fork.fork_url).ok()?;
    Some(fork.remote_name)
}

fn resync_pr_bases_inner(
    store: &crate::store_lock::StoreHandle,
    id: &str,
    require_forge_success: bool,
) -> std::result::Result<usize, String> {
    let guardian = store.lock().get_guardian(id).map_err(|e| e.to_string())?;
    if crate::guardian::GuardianStatus::is_terminal_status(&guardian.status) {
        return Ok(0);
    }
    let root = PathBuf::from(&guardian.git_root);
    let forge_cfg = crate::config::resolve_forge(&root);
    let remote_name = crate::forge::resolve_remote_name(&root, &guardian.base_branch, &forge_cfg);
    let base_branch_name = strip_remote_prefix(&guardian.base_branch, &remote_name);

    let prs = store
        .lock()
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
        // A synchronous resync verifies every open member on the forge, but
        // does not PATCH a base that is already correct. GitHub rejects even
        // a no-op base update while a PR belongs to a native stack; treating
        // that rejection as a real move would unnecessarily rebuild it.
        let local_base_changed = new_base != pr.base_ref;
        if local_base_changed || require_forge_success {
            // ralphus[ignore-rlog-pair]: this low-level helper has no Store; its Store-owning caller records the structured workflow outcome
            crate::rlog!(
                INFO,
                "ralphus [pr] review {id} resync base pr={} old={} new={new_base}",
                pr.id,
                pr.base_ref
            );
            if !require_forge_success {
                let _ = store.lock().update_pull_request_ex(
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
                let forge_base_matches = if require_forge_success {
                    match c.get_pull_request_base_state(num) {
                        Ok(state) if state.base == new_base => {
                            let _ = store.lock().update_pull_request_ex(
                                &pr.id,
                                None,
                                None,
                                None,
                                None,
                                Some(&new_base),
                                None,
                                Some(Some(&new_base)),
                            );
                            true
                        }
                        Ok(_) => false,
                        Err(e) => {
                            forge_errors.push(format!("pr {}: {e}", pr.id));
                            continue;
                        }
                    }
                } else {
                    false
                };
                if forge_base_matches {
                    if local_base_changed {
                        changed += 1;
                    }
                    continue;
                }
                if local_base_changed {
                    changed += 1;
                }
                match c.update_pull_request_base(num, &new_base) {
                    // RAL-279: only record `last_pushed_base_ref` once the
                    // forge has actually confirmed the new base -- this is
                    // exactly the "last state both sides are known to have
                    // agreed on" baseline `poll_pr_base_drift` needs to tell
                    // a genuine forge-side retarget apart from a PATCH that
                    // silently failed here and never landed.
                    Ok(()) => {
                        let _ = store.lock().update_pull_request_ex(
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
            } else if local_base_changed {
                changed += 1;
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
                        let _ = store.lock().update_pull_request_ex(
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

fn is_forge_not_found(err: &str) -> bool {
    err.starts_with("forge API 404:")
}

fn create_and_record_native_stack(
    store: &crate::store_lock::StoreHandle,
    id: &str,
    client: &crate::forge::ForgeClient,
    ordered_pr_numbers: &[i64],
) -> std::result::Result<Option<i64>, String> {
    let Some(stack) = client.create_stack(ordered_pr_numbers)? else {
        return Ok(None);
    };
    store
        .lock()
        .set_guardian_forge_stack_number(id, stack.number)
        .map_err(|e| e.to_string())?;
    Ok(Some(stack.number))
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
    store: &crate::store_lock::StoreHandle,
    id: &str,
    client: &crate::forge::ForgeClient,
    ordered_pr_numbers: &[i64],
    blocked: &[(String, i64, String)],
) -> std::result::Result<(), String> {
    let recorded = store
        .lock()
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
    store
        .lock()
        .clear_guardian_forge_stack_number(id)
        .map_err(|e| e.to_string())?;
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
    match create_and_record_native_stack(store, id, client, ordered_pr_numbers) {
        Ok(Some(created_number)) => {
            // ralphus[ignore-rlog-pair]: this low-level helper has no Store; its Store-owning caller records the structured workflow outcome
            crate::rlog!(
                INFO,
                "ralphus [pr] review {id} re-registered stack {} after moving PR bases \
                 (was {stack_number})",
                created_number
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
    store: &crate::store_lock::StoreHandle,
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
pub fn start_resync_pr_bases(store: crate::store_lock::StoreHandle, id: &str) {
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
                let guard = store.lock();
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
pub fn check_pr_merges(store: &crate::store_lock::StoreHandle, id: &str) -> bool {
    let Ok(guardian) = store.lock().get_guardian(id) else {
        return false;
    };
    if crate::guardian::GuardianStatus::is_terminal_status(&guardian.status) {
        return false;
    }
    let prs = store
        .lock()
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

    // Resolve each open PR's forge client up front (cheap, local config/DB
    // reads); the actual forge call is the only genuinely slow part here,
    // and is fired off concurrently below since one PR's merge state has no
    // bearing on any other's.
    struct MergeCheckJob<'a> {
        pr: &'a PullRequestView,
        client: crate::forge::ForgeClient,
    }
    let mut jobs = Vec::new();
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
        match routing.client_for(&pr.repo) {
            Some(client) if client.kind().as_str() == pr.forge => {
                jobs.push(MergeCheckJob {
                    pr,
                    client: client.clone(),
                });
            }
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
            }
        }
    }

    let fetched: Vec<Option<std::result::Result<String, String>>> = std::thread::scope(|scope| {
        let handles: Vec<_> = jobs
            .iter()
            .map(|job| scope.spawn(|| fetch_pr_merge_state(job.pr, &job.client)))
            .collect();
        handles
            .into_iter()
            .map(|handle| {
                handle
                    .join()
                    .unwrap_or_else(|_| Some(Err("forge merge check panicked".to_string())))
            })
            .collect()
    });

    let mut freshly_merged = Vec::new();
    for (job, result) in jobs.iter().zip(fetched) {
        apply_pr_merge_state(store, id, job.pr, result, &mut freshly_merged);
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
    store: &crate::store_lock::StoreHandle,
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
            // ralphus[ignore-rlog-pair]: poll-time routing diagnostic; a successful promotion logs its structured outcome
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
        // ralphus[ignore-rlog-pair]: no-op reconciliation diagnostic; an actual promotion emits the structured outcome
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
    let new_id = store.lock().create_pull_request_ex(
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
        created.draft,
    );
    let _ = store.lock().update_pull_request_ex(
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
            let _ = store.lock().set_pr_superseded_by(&successor_pr.id, new_id);
            crate::rlog!(
                INFO,
                "ralphus [pr] review {id} promoted pr={} (old) -> {new_id} (new, number={})",
                successor_pr.id,
                created.number
            );
            let guard = store.lock();
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
    store: &crate::store_lock::StoreHandle,
    id: &str,
    prs: &[PullRequestView],
    client: &crate::forge::ForgeClient,
) -> bool {
    let mut freshly_merged: Vec<PullRequestView> = Vec::new();
    for pr in prs.iter().filter(|p| p.state == "open") {
        let fetched = fetch_pr_merge_state(pr, client);
        apply_pr_merge_state(store, id, pr, fetched, &mut freshly_merged);
    }

    settle_pr_merge_states(store, id, &freshly_merged)
}

/// The network half of a single PR's merge-state check: just the forge call,
/// with no store access, so [`check_pr_merges`] can run it concurrently
/// across independent PRs. `None` mirrors the "no PR number, nothing to
/// check" early return.
fn fetch_pr_merge_state(
    pr: &PullRequestView,
    client: &crate::forge::ForgeClient,
) -> Option<std::result::Result<String, String>> {
    let number = pr.pr_number?;
    Some(client.get_pull_request_state(number))
}

/// The store-writing half of a single PR's merge-state check: apply an
/// already-fetched forge state ([`fetch_pr_merge_state`]) to the PR row and
/// `freshly_merged`. Kept serial (unlike the fetch) since it mutates shared
/// state.
fn apply_pr_merge_state(
    store: &crate::store_lock::StoreHandle,
    id: &str,
    pr: &PullRequestView,
    fetched: Option<std::result::Result<String, String>>,
    freshly_merged: &mut Vec<PullRequestView>,
) {
    let Some(result) = fetched else {
        return;
    };
    match result {
        Ok(state) if state != "open" => {
            let _ = store.lock().update_pull_request_ex(
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
    store: &crate::store_lock::StoreHandle,
    id: &str,
    pr: &PullRequestView,
    error: &str,
) {
    crate::rlog!(
        WARNING,
        "ralphus [pr] review {id} pr {} merge check failed, assuming not merged: {error}",
        pr.id
    );
    let guard = store.lock();
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
    store: &crate::store_lock::StoreHandle,
    id: &str,
    freshly_merged: &[PullRequestView],
) -> bool {
    let current_guardian = match store.lock().get_guardian(id) {
        Ok(guardian) => guardian,
        Err(_) => return false,
    };
    let current_prs = store
        .lock()
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
        let approved = store.lock().approve_guardian(id).is_ok();
        if approved {
            crate::rlog!(
                INFO,
                "ralphus [pr] review {id} approved: every linked pr has merged"
            );
            let guard = store.lock();
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
        let guard = store.lock();
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
    let _ = store.lock().set_guardian_notice(
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
/// Reconciled per-branch, not per-review: a stacked branch that has finished
/// its own rebase pass (`done`/`conflict_resolved`) is pushed as soon as it
/// gets here even while sibling branches later in the stack are still being
/// processed — a long-running conflict resolution elsewhere in the same
/// stack must not hold a finished branch's PR hostage. A branch still being
/// worked (`pending`/`in_progress`/`actioning`/...) is skipped: its worktree
/// tip can still be rewritten again before this merge attempt settles. The
/// combined-worktree PR (`branch_id: None`) has no single branch to check
/// against, so it still waits for the whole review to reach `in_review`.
/// Mirrors the push+record-sha pattern [`submit_pull_requests_inner`] and
/// [`pull_pr_commits`] already use. Best-effort per PR: one push failing
/// (most likely `guard_against_clobber` tripping because a reviewer pushed
/// directly to the PR branch) is logged and does not stop the others.
pub fn sync_open_pr_branches(store: &crate::store_lock::StoreHandle, id: &str) {
    let Ok(guardian) = store.lock().get_guardian(id) else {
        return;
    };
    if crate::guardian::GuardianStatus::is_terminal_status(&guardian.status) {
        return;
    }
    let prs = store
        .lock()
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
            Some(bid) => {
                let Some(branch) = guardian.branches.iter().find(|b| &b.id == bid) else {
                    continue;
                };
                if !matches!(branch.merge_status.as_str(), "done" | "conflict_resolved") {
                    continue;
                }
                branch.review_branch.clone()
            }
            None => {
                if guardian.status.as_str() != "in_review" {
                    continue;
                }
                guardian.review_branch.clone()
            }
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
        let _ = store.lock().update_pull_request_ex(
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
            let guard = store.lock();
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
    store: &crate::store_lock::StoreHandle,
    id: &str,
) -> std::result::Result<Option<ForgeStackDrift>, String> {
    let guardian = store.lock().get_guardian(id).map_err(|e| e.to_string())?;
    let root = PathBuf::from(&guardian.git_root);
    let forge_cfg = crate::config::resolve_forge(&root);
    // RAL-338: a fork-mode review's PRs may be split across two repositories,
    // so resolve both candidate clients up front and pick per-PR via each
    // row's own stored `repo` -- see [`PrRepoRouting`].
    let routing = resolve_pr_repo_routing(store, &root, &guardian.base_branch, &forge_cfg);
    let base_branch_name = strip_remote_prefix(&guardian.base_branch, &routing.parent_remote_name);

    let prs = store
        .lock()
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
fn claim_guardian_for_forge_reorder(store: &crate::store_lock::StoreHandle, id: &str) -> bool {
    let guard = store.lock();
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
    store: &crate::store_lock::StoreHandle,
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
            let guard = store.lock();
            crate::cartographer::Note::new("pr")
                .guardian(id)
                .scope("guardian")
                .level(crate::logging::LogLevel::WARNING)
                .emit(
                    &guard,
                    "forge reorder check failed",
                    serde_json::json!({"error": e.to_string()}),
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
    let (root, base_branch, forge_cfg) = {
        let guardian = match store.lock().get_guardian(id) {
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
        (root, guardian.base_branch, forge_cfg)
    };
    let remote_name = forge_parent_remote_name(store, &root, &base_branch, &forge_cfg);
    let local_base = qualify_forge_base(&base_branch, &remote_name, &drift.base);
    {
        let mut guard = store.lock();
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
    store: &crate::store_lock::StoreHandle,
    sem: &Arc<crate::scheduler::Semaphore>,
    cancellations: &crate::cancel::Cancellations,
) {
    let ids: Vec<String> = {
        let guard = store.lock();
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
    store: &crate::store_lock::StoreHandle,
    id: &str,
) -> std::result::Result<usize, String> {
    let guardian = store.lock().get_guardian(id).map_err(|e| e.to_string())?;
    if crate::guardian::GuardianStatus::is_terminal_status(&guardian.status) {
        return Ok(0);
    }
    let root = PathBuf::from(&guardian.git_root);
    let forge_cfg = crate::config::resolve_forge(&root);
    // RAL-338: resolve both candidate clients so each PR's base-drift check
    // uses whichever repository it's actually filed on.
    let routing = resolve_pr_repo_routing(store, &root, &guardian.base_branch, &forge_cfg);
    let base_branch_name = strip_remote_prefix(&guardian.base_branch, &routing.parent_remote_name);

    let prs = store
        .lock()
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
            let _ = store
                .lock()
                .set_branch_enabled_by_name(id, &skipped.branch, false);
        }

        // ralphus[ignore-rlog-pair]: per-PR drift-detection detail; the batch summary in poll_pr_base_drift_once records the structured workflow outcome
        crate::rlog!(
            INFO,
            "ralphus [pr] review {id} pr={} base drifted on the forge: old={} new={forge_base}",
            pr.id,
            pr.base_ref
        );
        let _ = store.lock().update_pull_request_ex(
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

// ---------------------------------------------------------------------------
// RAL-366: cached forge state (background poller)
// ---------------------------------------------------------------------------

/// How long the cache poller waits before retrying a forge client that just
/// returned a rate-limit response, when the forge gave no `Retry-After`
/// header to honor instead.
const DEFAULT_RATE_LIMIT_BACKOFF: std::time::Duration = std::time::Duration::from_secs(60);

/// Per-forge-client rate-limit backoff state, keyed by `"{kind}:{repo_label}"`
/// -- shared across every guardian's poll pass in this process, so a
/// 429/403 observed while polling one guardian's PRs also holds off comment
/// fetches for another guardian on the same repo within the same cycle,
/// rather than each rediscovering the rate limit independently.
static PR_CACHE_BACKOFF: LazyLock<Mutex<HashMap<String, std::time::Instant>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn forge_client_backoff_key(client: &crate::forge::ForgeClient) -> String {
    format!("{}:{}", client.kind().as_str(), client.repo_label())
}

/// Whether `client` is still cooling down from a prior rate-limit response
/// this process has already seen.
fn is_backed_off(client: &crate::forge::ForgeClient) -> bool {
    let key = forge_client_backoff_key(client);
    PR_CACHE_BACKOFF
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(&key)
        .is_some_and(|resume_at| std::time::Instant::now() < *resume_at)
}

/// Start (or extend) a rate-limit backoff window for `client`, honoring the
/// forge's own `Retry-After` when it sent one instead of guessing.
fn start_backoff(client: &crate::forge::ForgeClient, retry_after: Option<std::time::Duration>) {
    let key = forge_client_backoff_key(client);
    let resume_at = std::time::Instant::now() + retry_after.unwrap_or(DEFAULT_RATE_LIMIT_BACKOFF);
    PR_CACHE_BACKOFF
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(key, resume_at);
}

/// Poll one PR's comment endpoint(s) -- both of GitHub's conversation/review
/// endpoints, or GitLab's single notes endpoint -- via
/// [`crate::forge::ForgeClient::list_pr_comments_conditional`], replacing
/// `guardian_pr_forge_comments` rows for any endpoint that returned a fresh
/// (non-304) body. Returns the etags to persist (unchanged when a poll was
/// skipped/304/errored) and an error message when a call actually failed --
/// a client already cooling down from a prior rate-limit response is
/// silently skipped rather than re-attempted and re-erroring every cycle.
fn poll_pr_comments(
    store: &crate::store_lock::StoreHandle,
    client: &crate::forge::ForgeClient,
    pr_id: &str,
    number: i64,
    etag_conversation: Option<String>,
    etag_review: Option<String>,
) -> (Option<String>, Option<String>, Option<String>) {
    if is_backed_off(client) {
        return (
            etag_conversation,
            etag_review,
            Some("forge rate-limited; backing off".to_string()),
        );
    }
    let mut error = None;
    let poll_one =
        |endpoint: crate::forge::PrCommentEndpoint, prior_etag: Option<String>| match client
            .list_pr_comments_conditional(number, endpoint, prior_etag.as_deref())
        {
            Ok(crate::forge::CommentsPoll::NotModified) => (prior_etag, None),
            Ok(crate::forge::CommentsPoll::Modified { comments, etag }) => {
                let table_endpoint = match endpoint {
                    crate::forge::PrCommentEndpoint::Conversation => "conversation",
                    crate::forge::PrCommentEndpoint::Review => "review",
                };
                let _ = store
                    .lock()
                    .replace_pr_forge_comments(pr_id, table_endpoint, &comments);
                (etag.or(prior_etag), None)
            }
            Err(e) => {
                if e.is_rate_limited() {
                    start_backoff(client, e.retry_after);
                }
                (prior_etag, Some(e.to_string()))
            }
        };
    let (new_conv, conv_err) = poll_one(
        crate::forge::PrCommentEndpoint::Conversation,
        etag_conversation,
    );
    error = error.or(conv_err);
    // GitLab's single `/notes` endpoint already returned both conversation
    // and review comments above -- polling it a second time under `Review`
    // would just duplicate the exact same rows.
    let new_review = if client.kind() == crate::forge::ForgeKind::GitHub {
        let (new_review, review_err) =
            poll_one(crate::forge::PrCommentEndpoint::Review, etag_review);
        error = error.or(review_err);
        new_review
    } else {
        etag_review
    };
    (new_conv, new_review, error)
}

/// Refresh cached forge state for every currently-open, forge-numbered PR
/// belonging to guardian `id`: one batched `git fetch` covering every PR's
/// branch, plus a conditional comment fetch per PR, writing straight to
/// `guardian_pr_forge_cache`/`guardian_pr_forge_comments`
/// ([`Store::upsert_pr_forge_cache`]/[`Store::replace_pr_forge_comments`]).
/// Never touches a scheduler concurrency permit, and never holds the store
/// lock across a network call -- mirrors `summary_worker`'s precedent for
/// background work that must not block a request thread or the store lock on
/// a subprocess/network call.
///
/// Runs as two ordered passes, and the split is load-bearing in both
/// directions:
///
/// 1. *Drift* holds each PR's [`SYNC_FETCH_LOCKS`] entry, taken via `try_lock`
///    so it never blocks. A PR whose lock an interactive `sync-status` call
///    already holds is left out of this cycle's batch and keeps its
///    last-known drift, so this poller never makes foreground work wait.
/// 2. *Comments* runs only after every one of those guards has been dropped,
///    so the reverse is true as well: `compute_sync_status` takes the same
///    per-PR lock *blocking* (see [`fetch_remote_tip`]), and a foreground
///    drift check therefore waits at most for one PR's own `git fetch` --
///    never for this poller's two forge round-trips per PR.
///
/// Fusing the two passes into one loop is what makes an interactive
/// `sync-status` take an order of magnitude longer than the git fetch it
/// performs, and since each blocked request occupies a read-pool worker for
/// its whole wait, that also starves unrelated reads queued behind it. Keep
/// them separate.
///
/// Every PR under one guardian shares the same remote
/// ([`PrRepoRouting::remote_for`] ignores its `repo` argument once a fork is
/// registered, and is a single plain fallback otherwise), so one `git fetch`
/// with every PR's refspec here replaces what would otherwise be one fetch
/// per PR -- the batching this ticket asks for. Two guardians that happen to
/// share a git root still each fetch separately: collapsing across
/// guardians would mean restructuring this whole poll loop from per-guardian
/// to per-root across every guardian in the daemon, which isn't worth the
/// risk for what is already an N-to-1 reduction per guardian's own stack.
fn refresh_pr_forge_cache_for_guardian(store: &crate::store_lock::StoreHandle, id: &str) {
    let Ok(guardian) = store.lock().get_guardian(id) else {
        return;
    };
    // Checked before any work: the backoff window is per forge client, and a
    // 429 seen while polling one guardian means the next guardian on the same
    // repo must not immediately go and ask again. Previously only the comment
    // fetch consulted this, so a rate-limited forge still got a full round of
    // git fetches and base-drift calls every cycle -- the retry storm the
    // backoff exists to prevent.
    let backed_off = {
        let root = PathBuf::from(&guardian.git_root);
        let forge_cfg = crate::config::resolve_forge(&root);
        resolve_pr_repo_routing(store, &root, &guardian.base_branch, &forge_cfg)
            .client_for("")
            .is_some_and(is_backed_off)
    };
    if backed_off {
        // ralphus[ignore-rlog-pair]: rate-limit path records the structured outcome; this routine only skips a poll
        crate::rlog!(
            DEBUG,
            "ralphus [pr] review {id} forge cache poll skipped: still backing off from a rate limit"
        );
        return;
    }
    let prs: Vec<PullRequestView> = store
        .lock()
        .list_pull_requests_for_guardian(id)
        .unwrap_or_default()
        .into_iter()
        .filter(|p| p.state == "open" && p.pr_number.is_some())
        .collect();
    if prs.is_empty() {
        return;
    }
    let root = PathBuf::from(&guardian.git_root);
    let forge_cfg = crate::config::resolve_forge(&root);
    let routing = resolve_pr_repo_routing(store, &root, &guardian.base_branch, &forge_cfg);
    let remote_name = routing.remote_for("").to_string();

    // Drift pass. Every `SYNC_FETCH_LOCKS` guard this takes is confined to this
    // block and dropped before the comment pass below issues a single forge
    // call.
    //
    // That separation is the whole point of the block: `fetch_remote_tip` --
    // reached from `compute_sync_status`, i.e. an interactive "is this PR in
    // sync?" from the board -- takes the very same per-PR lock with a
    // *blocking* `lock()`. Holding these guards across the comment pass's two
    // network round-trips per PR (as one fused loop would) makes every
    // foreground drift check queue behind this poller's forge latency instead
    // of its own git fetch, and each blocked request holds a read-pool worker
    // for that whole time, starving unrelated reads behind it.
    //
    // `try_lock` (not `lock`) is retained: a PR an interactive caller is
    // already checking is skipped for this cycle and keeps its last-known
    // drift, so the poller still never makes foreground work wait.
    type DriftReading =
        std::result::Result<(bool, bool, bool, Option<String>, Option<String>), String>;
    let drift_by_pr: Vec<Option<DriftReading>> = {
        let locks: Vec<Arc<Mutex<()>>> =
            prs.iter().map(|p| sync_fetch_lock(&root, &p.id)).collect();
        let guards: Vec<Option<std::sync::MutexGuard<'_, ()>>> =
            locks.iter().map(|l| l.try_lock().ok()).collect();
        let refspecs: Vec<String> = prs
            .iter()
            .zip(guards.iter())
            .filter(|(_, g)| g.is_some())
            .map(|(pr, _)| format!("+{}:{}", pr.branch_alias, sync_fetch_ref(&pr.id)))
            .collect();
        // Whether the batched fetch actually succeeded decides whether what
        // follows is a fresh observation or a re-read of whatever the last
        // successful fetch left behind. Discarding this made a network failure
        // indistinguishable from success: `rev-parse` still resolves the
        // *previous* cycle's sync ref, so stale drift was written back stamped
        // as a current, successful reading.
        let fetch_error: Option<String> = if refspecs.is_empty() {
            None
        } else {
            let mut args: Vec<&str> = vec!["fetch", &remote_name];
            args.extend(refspecs.iter().map(String::as_str));
            git(&root, &args).err()
        };
        prs.iter()
            .zip(guards.iter())
            .map(|(pr, guard)| {
                // `None` (lock held by an interactive caller) and
                // `Some(Err(_))` (fetch failed) are deliberately different:
                // the first must not move the stored timestamp at all, the
                // second moves it but records the half as unknown.
                guard.is_some().then(|| {
                    if let Some(e) = &fetch_error {
                        return Err(e.clone());
                    }
                    let local_ref = local_ref_for_pr(&guardian, pr);
                    let local_sha = local_ref
                        .as_deref()
                        .and_then(|r| git(&root, &["rev-parse", r]).ok())
                        .map(|s| s.trim().to_string());
                    let remote_sha = git(&root, &["rev-parse", &sync_fetch_ref(&pr.id)])
                        .ok()
                        .map(|s| s.trim().to_string());
                    let (pr_ahead, worktree_ahead, in_sync) = classify_sync_drift(
                        &root,
                        remote_sha.as_deref(),
                        local_sha.as_deref(),
                        pr.last_pushed_sha.as_deref(),
                    );
                    Ok((in_sync, pr_ahead, worktree_ahead, remote_sha, local_sha))
                })
            })
            .collect()
    };

    // Comment pass -- no drift lock is held from here on.
    for (pr, drift) in prs.iter().zip(drift_by_pr) {
        let number = pr.pr_number.expect("filtered to pr_number.is_some() above");

        // Fetched before either write path below so the post-write
        // comparison reflects an actual state transition, not this pass's
        // own new value.
        let previous_status = store
            .lock()
            .get_pr_forge_cache(&pr.id)
            .ok()
            .flatten()
            .map(|c| c.status);

        let (ok, comment_error, new_conversation_etag, new_review_etag) =
            match routing.client_for(&pr.repo) {
                Some(client) => {
                    let (etag_conversation, etag_review) = store
                        .lock()
                        .pr_forge_cache_etags(&pr.id)
                        .unwrap_or((None, None));
                    let (new_conversation_etag, new_review_etag, comment_error) = poll_pr_comments(
                        store,
                        client,
                        &pr.id,
                        number,
                        etag_conversation,
                        etag_review,
                    );
                    (
                        comment_error.is_none(),
                        comment_error,
                        new_conversation_etag,
                        new_review_etag,
                    )
                }
                // No resolvable forge client (missing token, unregistered
                // remote, ...) -- degrade the comments half to "unknown" but
                // still persist whatever drift this pass computed.
                None => (
                    false,
                    Some("no forge client could be resolved for this PR's repo".to_string()),
                    None,
                    None,
                ),
            };

        let drift_half: HalfOutcome<DriftObservation<'_>> = drift.as_ref().map(|r| match r {
            Ok((in_sync, pr_ahead, worktree_ahead, remote_sha, local_sha)) => {
                Ok(DriftObservation {
                    in_sync: *in_sync,
                    pr_ahead: *pr_ahead,
                    worktree_ahead: *worktree_ahead,
                    remote_sha: remote_sha.as_deref(),
                    local_sha: local_sha.as_deref(),
                })
            }
            Err(e) => Err(e.clone()),
        });
        let comments_half: HalfOutcome<CommentsObservation<'_>> = Some(match &comment_error {
            Some(e) => Err(e.clone()),
            None => Ok(CommentsObservation {
                etag_conversation: new_conversation_etag.as_deref(),
                etag_review: new_review_etag.as_deref(),
            }),
        });
        let _ = store
            .lock()
            .upsert_pr_forge_cache(&pr.id, drift_half, comments_half);
        let new_status = if ok { "ok" } else { "unknown" };
        if previous_status.as_deref() != Some(new_status) {
            // A genuine reachability transition (first poll, forge recovered,
            // or forge just became unreachable) -- logged per
            // `.agent/logging-policy.md`'s Cartographer pairing, unlike the
            // per-cycle "still unknown"/"still ok" case below, which would
            // spam every poll interval for as long as an outage lasts.
            let level = if ok {
                crate::logging::LogLevel::INFO
            } else {
                crate::logging::LogLevel::WARNING
            };
            crate::cartographer::Note::new("pr")
                .level(level)
                .scope("guardian")
                .guardian(id)
                .emit(
                    &store.lock(),
                    format!(
                        "ralphus [pr] review {id} pr={} forge cache poll: now {new_status}",
                        pr.id
                    ),
                    serde_json::json!({
                        "pr_id": pr.id,
                        "status": new_status,
                        "error": comment_error,
                    }),
                );
        } else if let Some(err) = comment_error {
            // DEBUG, not WARNING/ERROR: an unreachable forge or expired
            // token is expected to recur every cycle until fixed, and this
            // poller must not spam logs for a condition the on-demand routes
            // already surface loudly when a human actually asks.
            // ralphus[ignore-rlog-pair]: per-PR poll detail already covered by the state-transition Cartographer emit above on first occurrence
            crate::rlog!(
                DEBUG,
                "ralphus [pr] review {id} pr={} forge cache poll: {err}",
                pr.id
            );
        }
    }
}

/// Poll every guardian with an open PR stack once (RAL-279's forge-side base
/// drift, plus RAL-366's cached branch-drift/comment state), logging a
/// Cartographer entry per guardian where the base-drift half changed. Never
/// touches a review with no submitted PRs (RAL-279's "no forge calls for a
/// review that was never submitted" requirement) since
/// [`Store::guardian_ids_with_open_pull_requests`] only returns guardians
/// that already have one. One target list shared by both halves per RAL-366's
/// "one thread, one target list" requirement; each guardian's own base-drift
/// check and cache refresh stay separate calls below rather than one fused
/// function, since they hit unrelated forge endpoints and RAL-279's existing
/// base-drift/cascading-resync logic is already well-exercised -- merging
/// their call sites would add risk without saving a network round-trip.
fn run_pr_forge_poll_cycle(store: &crate::store_lock::StoreHandle, cache_enabled: bool) {
    let ids = match store.lock().guardian_ids_with_open_pull_requests() {
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
                let guard = store.lock();
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
        if cache_enabled {
            refresh_pr_forge_cache_for_guardian(store, &id);
        }
    }
    if cache_enabled {
        // Once per cycle, not per guardian: PRs that merged or closed since
        // the last pass stop being polled but keep their rows forever
        // otherwise.
        match store.lock().prune_pr_forge_cache() {
            Ok(n) if n > 0 => {
                crate::rlog!(
                    DEBUG,
                    "ralphus [pr] pruned {n} closed PRs from the forge cache"
                );
            }
            Ok(_) => {}
            Err(e) => crate::rlog!(WARNING, "ralphus [pr] forge cache prune failed: {e}"),
        }
    }
}

/// Spawn the background loop that periodically calls
/// [`run_pr_forge_poll_cycle`] for as long as the daemon runs (RAL-279,
/// folding in RAL-366's cached-forge-state refresh per that ticket's "do not
/// spawn a second independent poller" decision). The interval and an
/// on/off switch are read fresh from [`crate::config::load_pr_cache_config`]
/// at the top of every cycle (not just once at startup), so a config edit
/// takes effect on this daemon without a restart -- matching this module's
/// "load config fresh where needed" style. Skips a cycle entirely during a
/// configured `[daemon].downtime` window (RAL-122): this is opportunistic
/// background reconciliation, not user-facing work, so it yields the same
/// way scheduled cell claims do.
pub fn spawn_pr_base_drift_poller(store: crate::store_lock::StoreHandle) {
    std::thread::spawn(move || {
        loop {
            let cache_cfg = crate::config::load_pr_cache_config();
            std::thread::sleep(cache_cfg.poll_interval());
            if crate::config::scheduler_in_downtime() {
                continue;
            }
            // `[pr_cache].enabled = false` turns off the RAL-366 cache
            // refresh only. RAL-279's base-drift reconciliation shares this
            // thread but is unrelated functionality with real consequences --
            // it is what notices a forge-side base edit -- and a key named
            // `pr_cache` must not silently disable it.
            //
            // Caught so one panicking cycle costs that cycle rather than the
            // thread. Unguarded, a single unwrap anywhere in the pass would
            // silently end base-drift reconciliation for the daemon's whole
            // lifetime, with nothing in the UI to indicate it had stopped.
            let cache_enabled = cache_cfg.enabled();
            let pass = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                run_pr_forge_poll_cycle(&store, cache_enabled);
            }));
            if pass.is_err() {
                crate::rlog!(
                    ERROR,
                    "ralphus [pr] forge poll cycle panicked; the poller continues with the next cycle"
                );
            }
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
    store: &crate::store_lock::StoreHandle,
    root: &Path,
    guardian: &GuardianView,
    forge_cfg: &crate::config::ForgeConfig,
    user: &str,
) -> std::result::Result<Option<ForkRouting>, String> {
    let project_name = store.lock().project_name_for_path(&guardian.git_root);
    let Some(project_name) = project_name else {
        return Ok(None);
    };
    let fork = store
        .lock()
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
            head: routing.fork_client.same_repo_head(alias),
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
    store: &crate::store_lock::StoreHandle,
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
            let _ = store
                .lock()
                .cartographer_log(crate::cartographer::CartographerEntry {
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
                });
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
    store: &crate::store_lock::StoreHandle,
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
    store: &crate::store_lock::StoreHandle,
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
    // RAL-378: `None` unless this branch owns a readable review-branch name,
    // which is what makes "the PR branch is the review branch" expressible at
    // all -- see `resolve_pr_alias`.
    let readable_review_branch = branch
        .readable_review_branch
        .then_some(branch.review_branch_name.as_deref())
        .flatten();
    let alias_is_the_review_branch =
        !guardian.effective_separate_pr_branch && req.branch_alias.is_none();
    let desired_alias = resolve_pr_alias(
        req.branch_alias.as_deref(),
        req.use_worktree_branch_name,
        guardian.effective_match_pr_branch_name,
        guardian.effective_separate_pr_branch,
        readable_review_branch,
        pr_branch_convention,
        &branch.branch,
    );
    // RAL-190: suffix (`-002`, ...) if another PR already claims this
    // alias, so two branches/reviews that would otherwise default to the
    // same remote branch name don't collide. RAL-338: `client` here is
    // always the fork in fork mode, so uniqueness is scoped to the fork's
    // physical refs even for the root branch, whose PR is filed elsewhere.
    //
    // RAL-378: skipped when the alias *is* the review branch's name -- that
    // name was already made unique against open PR aliases (among other
    // things) when the branch claimed it, and re-suffixing here would break
    // the identity the whole mode exists for, pushing `x-review` to a remote
    // `x-review-002`.
    let alias = if alias_is_the_review_branch && readable_review_branch.is_some() {
        desired_alias
    } else {
        store
            .lock()
            .resolve_unique_pr_alias(
                client.kind().as_str(),
                client.repo_label(),
                Some((id, branch_id)),
                &desired_alias,
            )
            .map_err(|e| e.to_string())?
    };
    crate::rlog!(
        DEBUG,
        "ralphus [pr] review {id} pushing branch id={branch_id} alias={alias} remote={remote_name}"
    );
    // Nothing has been pushed to a brand-new alias yet, so there is no
    // recorded tip to recognize the remote by. If the remote alias already
    // carries commits this worktree doesn't have -- a reviewer pushed
    // directly to it, or the branch previously had its PR unlinked and
    // drifted -- reconcile through the same rebase/conflict-resolution path
    // a tracked-PR sync (`pull_pr_commits`) already uses, rather than
    // refusing outright. This is the one push site every PR-creation path
    // (whole-stack submit, explicit per-branch submit, resubmission after an
    // unlink) funnels through, so every caller gets the reconciliation.
    if let Err(clobber_err) = guard_against_clobber(root, remote_name, &alias, &review_ref, None) {
        crate::rlog!(
            WARNING,
            "ralphus [pr] review {id} branch {branch_id} alias {alias} diverged from \
             {remote_name} ({clobber_err}); reconciling before push"
        );
        guardian_merge::pull_pr_commits(store, runner, id, branch_id, remote_name, &alias, None)
            .map_err(|e| {
                format!("could not reconcile remote branch '{alias}' before pushing: {e}")
            })?;
        guard_against_clobber(root, remote_name, &alias, &review_ref, None)?;
    }
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
            head: client.same_repo_head(&alias),
            base: base.clone(),
            target_project_id: None,
            repo: client.repo_label().to_string(),
        },
    };
    // RAL-<new>: a branch can reach here with no local PR row (a prior one
    // was unlinked, or a resubmit races an earlier attempt) while the forge
    // still has an open PR/MR for this exact head -- creating would just
    // 422. Ask the forge directly via its documented head/source-branch
    // filter (see `find_open_pull_request`'s doc for why this is a
    // structured query, never a parse of the creation error's free-text
    // message) and adopt what's already there instead of failing.
    let (created_pr, base, title, description, adopted) =
        match route.find_existing_pull_request()? {
            Some(existing) => {
                crate::rlog!(
                    INFO,
                    "ralphus [pr] review {id} branch {branch_id} alias {alias} adopting \
                     pre-existing open PR/MR #{} (base={}) instead of creating a duplicate",
                    existing.number,
                    existing.base
                );
                (
                    crate::forge::CreatedPr {
                        number: existing.number,
                        url: existing.url,
                        draft: existing.draft,
                    },
                    existing.base,
                    existing.title,
                    existing.description,
                    true,
                )
            }
            None => {
                let (title, description) = resolve_title_description(
                    runner,
                    guardian,
                    req,
                    position,
                    &route.client,
                    trace_context,
                );
                let created = route.create_pull_request(&title, &description)?;
                (created, base, title, description, false)
            }
        };
    let row_id = store
        .lock()
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
            created_pr.draft,
        )
        .map_err(|e| e.to_string())?;
    if let Some(sha) = &pushed_sha {
        let _ = store.lock().update_pull_request_ex(
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
        let guard = store.lock();
        let _ = guard.cartographer_log(crate::cartographer::CartographerEntry {
            level: crate::logging::LogLevel::INFO,
            source: "pr",
            message: if adopted {
                "pull request adopted"
            } else {
                "pull request created"
            },
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
                "adopted": adopted,
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
    // RAL-<new>: `run_auto_submit_pass` clears a stale `auto_submit_error`
    // badge (RAL-317) only for the branches its own pass covered, so a PR
    // fixed through any other path entirely -- a manual "submit PR stack",
    // the periodic reconcile sweep -- would keep the badge up long after the
    // real problem is fixed. This is the one site every PR creation/adoption
    // funnels through, so clearing it here covers all of them uniformly.
    let _ = store
        .lock()
        .set_branch_auto_submit_error(id, branch_id, None);
    // RAL-<new>: start watching this PR's CI status the moment it exists,
    // rather than leaving it to `ci_watch::poll_open_pr_ci_status`'s coarse,
    // per-guardian-throttled standing poll. A stack submits one PR at a time
    // (this whole function is a git push plus a forge API call, often tens of
    // seconds per branch), so without this a branch submitted early in the
    // same pass could show a known CI status well before a sibling submitted
    // moments later -- whose first status then waited on the next standing
    // poll, up to `ci_watch::STANDING_POLL_INTERVAL` away.
    crate::ci_watch::start_ci_watch(store, id, branch_id);
    store
        .lock()
        .get_pull_request(&row_id)
        .map_err(|e| e.to_string())
}

/// How (if at all) to reconcile the guardian's GitHub-native PR stack after a
/// submission. A pure decision -- no I/O -- so it is unit-testable without
/// git or network.
#[derive(Debug, Clone, PartialEq, Eq)]
enum StackAction {
    /// No stack recorded yet -- create one from every currently-open PR,
    /// bottom to top.
    Create { all_ordered: Vec<i64> },
    /// A recorded stack is a strict prefix of the review's expected PR order.
    Append {
        stack_number: i64,
        missing_ordered: Vec<i64>,
        all_ordered: Vec<i64>,
    },
    /// A recorded stack contains the wrong members or order. Rebuild the
    /// native grouping while retaining every existing PR.
    Rebuild {
        stack_number: i64,
        all_ordered: Vec<i64>,
    },
    /// Nothing to do this call.
    Skip { reason: &'static str },
}

/// Decide the [`StackAction`] for one submission. `all_branches_with_prs` is
/// every enabled branch that now has an open PR (existing + just-created),
/// as `(branch_id, position, pr_number)`, in any order.
/// `recorded_stack` is the guardian's previously-registered native stack and
/// its live ordered member PR numbers, if any. `chain_is_valid` is whether
/// this guardian's locally-resynced PR rows already form a valid bottom-to-top
/// base-ref chain (RAL-401 follow-up) -- see [`ordered_prs_form_a_chain`]'s
/// caller. GitHub's stack API rejects `create`/`add`/rebuild-`create` calls
/// whenever the chain isn't valid yet (e.g. a mid-stack branch was just
/// disabled and downstream bases haven't finished repointing around it), and
/// `Rebuild` in particular starts by dissolving whatever stack is currently
/// registered -- attempting it against a chain we already know is broken
/// would tear down a working stack only to fail to replace it, leaving the
/// review fully unstacked on the forge until a later pass happens to retry
/// (RAL-401 follow-up: this is what actually happened, confirmed against
/// both the daemon log and GitHub's own "added to stack / removed from
/// stack" PR timeline). Skipping here instead costs nothing: the review
/// keeps whatever stack registration it already has (however stale) and the
/// next reconcile pass -- run again on every terminal branch transition --
/// retries once the chain is actually consistent.
///
/// `Rebuild` is likewise withheld whenever `all_branches_with_prs` merely
/// describes *fewer* PRs than the registered stack in an otherwise identical
/// order -- see [`is_ordered_subset`].
fn decide_stack_action(
    all_branches_with_prs: &[(String, i64, i64)],
    recorded_stack: Option<(i64, Vec<i64>)>,
    chain_is_valid: bool,
) -> StackAction {
    let mut all_ordered: Vec<(i64, i64)> = all_branches_with_prs
        .iter()
        .map(|(_, pos, num)| (*pos, *num))
        .collect();
    all_ordered.sort_by_key(|(pos, _)| *pos);
    let all_ordered: Vec<i64> = all_ordered.into_iter().map(|(_, num)| num).collect();
    if all_ordered.len() < 2 {
        return StackAction::Skip {
            reason: "fewer than 2 PRs in the stack",
        };
    }
    match recorded_stack {
        None => {
            if !chain_is_valid {
                return StackAction::Skip {
                    reason: "base-ref chain isn't fully resynced yet; postponing native stack registration until it is",
                };
            }
            StackAction::Create { all_ordered }
        }
        Some((stack_number, members)) => {
            if members == all_ordered {
                return StackAction::Skip {
                    reason: "native stack already matches the review PR order",
                };
            }
            if !chain_is_valid {
                return StackAction::Skip {
                    reason: "base-ref chain isn't fully resynced yet; leaving the existing native stack alone until it is",
                };
            }
            if all_ordered.starts_with(&members) {
                return StackAction::Append {
                    stack_number,
                    missing_ordered: all_ordered[members.len()..].to_vec(),
                    all_ordered,
                };
            }
            if is_ordered_subset(&all_ordered, &members) {
                return StackAction::Skip {
                    reason: "the registered native stack already contains every expected PR in this order",
                };
            }
            StackAction::Rebuild {
                stack_number,
                all_ordered,
            }
        }
    }
}

/// Whether every PR in `expected` also appears in `members`, in the same
/// relative order -- i.e. `expected` is a subsequence of `members`.
///
/// This is the signature of a *partial* view of the review: everything we can
/// see agrees with the registered stack, the stack simply knows about PRs we
/// do not. Dissolving a stack is destructive and unconditionally visible on
/// every member PR's timeline, so it must never be the answer to "I can see
/// less than the forge can". A genuine membership change reads differently:
/// dropping a branch from a review closes its PR, and GitHub drops a closed
/// PR from the stack on its own, so the forge's members shrink rather than
/// ours; reordering, or gaining a mid-stack PR, breaks the relative order
/// and still reaches `Rebuild`.
fn is_ordered_subset(expected: &[i64], members: &[i64]) -> bool {
    let mut remaining = members.iter();
    expected.iter().all(|n| remaining.any(|m| m == n))
}

/// Whether `ordered_prs` (bottom-to-top, one entry per stack member) already
/// forms a valid GitHub PR-stack chain per our own locally-resynced records:
/// every PR's `base_ref` equal to the previous PR's pushed `branch_alias`.
/// Pure and independent of API calls -- it trusts the same `base_ref` values
/// [`resync_pr_bases_synchronously`] just confirmed the forge accepted
/// (RAL-279's `last_pushed_base_ref` invariant), so it can tell "still
/// mid-cascade" apart from "ready" without an extra round trip, and without
/// ever having to discover a broken chain via a failed forge call.
fn ordered_prs_form_a_chain(ordered_prs: &[&PullRequestView]) -> bool {
    ordered_prs
        .windows(2)
        .all(|w| w[1].base_ref == w[0].branch_alias)
}

/// Drops any recorded "open" PR whose stored `repo` matches neither of the
/// clients the guardian's *current* base branch resolves to (RAL-397): once
/// a review's upstream branch is changed to point at a different remote, an
/// open PR row still naming the old remote's repository can never again be
/// reached through the client(s) submission now resolves, so
/// [`refresh_open_prs`]'s live-state check would just fail against the
/// wrong repository (or, worse, a same-numbered PR that happens to exist
/// there) and conservatively keep counting it as still open, permanently
/// blocking a fresh PR from ever being filed against the new remote.
/// Retargeting that old PR is out of scope -- it is left exactly as
/// recorded and simply excluded from "already submitted" bookkeeping, so
/// [`submit_stack_for_guardian`] files a new one against the current remote
/// instead of silently skipping the branch forever.
fn retain_prs_reachable_via_current_routing<'a>(
    by_branch: &mut HashMap<&'a str, &'a PullRequestView>,
    client: &crate::forge::ForgeClient,
    fork_routing: Option<&ForkRouting>,
) {
    let candidates: Vec<&crate::forge::ForgeClient> = match fork_routing {
        Some(routing) => vec![&routing.parent_client, &routing.fork_client],
        None => vec![client],
    };
    by_branch.retain(|_, pr| crate::forge::client_for_repo(&pr.repo, &candidates).is_some());
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
/// still fine. Callers should first narrow `open_by_branch` with
/// [`retain_prs_reachable_via_current_routing`] so that narrowing applies
/// before this best-effort fallback ever comes into play.
fn refresh_open_prs<'a>(
    store: &crate::store_lock::StoreHandle,
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
                    let _ = store.lock().update_pull_request_ex(
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

/// Created PRs alongside `(branch_id, error)` for whichever branch(es)
/// failed to submit -- shared by [`submit_stack_for_guardian`] and
/// [`auto_submit_terminal_branches`] so a batch failure can be attributed to
/// the specific branch that produced it instead of the whole batch.
type StackSubmitOutcome = (Vec<PullRequestView>, Vec<(String, String)>);

/// Submit a PR for every enabled branch that doesn't already have an open
/// one -- first dropping any recorded "open" PR that a review's upstream
/// change has made unreachable via [`retain_prs_reachable_via_current_routing`]
/// (RAL-397), then checking what's left's *live* forge state via
/// [`refresh_open_prs`], so a branch whose old PR was closed/merged outside
/// ralphus (or whose remote changed) gets a fresh one instead of being
/// silently skipped forever -- chaining bases via [`submit_stacked_branch_pr`]
/// exactly like an explicit per-branch request would. Also re-runs
/// [`resync_pr_bases`] so any
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
///
/// One branch failing to submit does not stop the rest of the stack: the
/// returned `Vec<(branch_id, error)>` names exactly which branch(es) failed
/// so a caller can attribute each failure to its own branch instead of the
/// whole batch. The outer `Result` is reserved for failures that aren't
/// about any one branch (e.g. [`reconcile_native_pr_stack`]'s own DB/forge
/// errors).
#[allow(clippy::too_many_arguments)]
fn submit_stack_for_guardian(
    store: &crate::store_lock::StoreHandle,
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
) -> std::result::Result<StackSubmitOutcome, String> {
    let mut open_by_branch = open_prs_by_branch(existing_prs);
    retain_prs_reachable_via_current_routing(&mut open_by_branch, client, fork_routing);
    let already_open = refresh_open_prs(store, client, open_by_branch);
    let mut created = Vec::new();
    // One branch's PR failing to submit (e.g. GitHub's "no commits between
    // X and Y" once a stacked branch's diff is already in its base) must
    // never cost every *other* branch in the same batch its own attempt, and
    // the error belongs to the branch that actually produced it -- not to
    // whichever branch's own terminal transition happened to be the one that
    // triggered this batch. Collecting per-branch failures here (instead of
    // bailing out with `?` on the first one) is what lets callers like
    // `run_auto_submit_pass` stamp each failure on its own branch.
    let mut failed = Vec::new();

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
        match submit_stacked_branch_pr(
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
        ) {
            Ok(pr) => created.push(pr),
            Err(e) => failed.push((branch.id.clone(), e)),
        }
    }

    // RAL-395: native-stack reconciliation is a shared self-heal, not
    // whole-stack-only -- see `reconcile_native_pr_stack` for why it also
    // runs from the explicit per-branch/positional submission path in
    // `submit_pull_requests_inner`.
    reconcile_native_pr_stack(store, client, id, fork_routing, created.len())?;

    Ok((created, failed))
}

/// Reconcile this guardian's native GitHub PR-stack grouping after a
/// submission that touched its PR rows (RAL-395). Shared by
/// [`submit_stack_for_guardian`] (the "submit the whole stack" path) and the
/// explicit per-branch/positional request path in
/// [`submit_pull_requests_inner`] -- every PR submission method funnels
/// through this one function, so a stack assembled one branch at a time
/// (e.g. `ralphus review pr submit <id> --position N` called once per
/// branch) still ends up registered as a native stack on the forge, not
/// just chained by base ref. Re-reads the guardian's PR rows fresh from the
/// store rather than trusting a caller-held list, since branch creation may
/// have just written new rows this same call.
///
/// It re-reads the review's *branches* for the same reason, and deliberately
/// takes every enabled branch rather than the subset a caller happened to be
/// working on. Native-stack membership is a property of the review, not of
/// which branches have finished rebasing: `auto_submit_terminal_branches`
/// narrows to `done`/`conflict_resolved` branches because only those can
/// have a PR *submitted* for them, and a restack resets every enabled branch
/// to `pending` and re-promotes them one at a time. Reconciling against that
/// caller's narrowed list made every restack look like "the review just lost
/// most of its PRs", which is exactly the condition [`decide_stack_action`]
/// answers with `Rebuild` -- so each rebase dissolved the native stack on
/// GitHub and rebuilt it from scratch as the branches trickled back in,
/// leaving the upper PRs visibly unstacked for minutes at a time even though
/// the review's membership and order had not changed at all.
fn reconcile_native_pr_stack(
    store: &crate::store_lock::StoreHandle,
    client: &crate::forge::ForgeClient,
    id: &str,
    fork_routing: Option<&ForkRouting>,
    created_count: usize,
) -> std::result::Result<(), String> {
    let guardian = store.lock().get_guardian(id).map_err(|e| e.to_string())?;
    let mut ordered_enabled: Vec<&BranchView> =
        guardian.branches.iter().filter(|b| b.enabled).collect();
    ordered_enabled.sort_by_key(|b| b.position);
    let ordered_enabled = ordered_enabled.as_slice();
    let existing_prs = store
        .lock()
        .list_pull_requests_for_guardian(id)
        .map_err(|e| e.to_string())?;
    let mut open_by_branch = open_prs_by_branch(&existing_prs);
    retain_prs_reachable_via_current_routing(&mut open_by_branch, client, fork_routing);
    let already_open = refresh_open_prs(store, client, open_by_branch);

    // Re-target any PR that already existed for this guardian but whose base
    // no longer matches the current stack order/chain (RAL-190) -- without
    // this, a PR left over from before this bug fix (or from a manual
    // resubmission that raced a reorder) is treated as "already submitted,
    // nothing to do" forever, even though it's still pointed at the wrong
    // base and doesn't actually read as part of the stack on the forge.
    // Submission is an explicit stack-reconciliation boundary for both the
    // button and auto-submit. Confirm every forge-side base here instead of
    // trusting a prior best-effort local update that may have failed after
    // its row was written.
    let resynced = resync_pr_bases_synchronously(store, id).unwrap_or_else(|e| {
        crate::rlog!(WARNING, "ralphus [pr] review {id} stack resync failed: {e}");
        0
    });
    // A branch reconciled (and restacked downstream) during this same
    // submission -- see `submit_stacked_branch_pr`'s clobber-guard handling
    // -- can leave an earlier, already-open PR's pushed content stale even
    // though its `base` field above is still correct. `sync_open_pr_branches`
    // is the same settle-time repair the periodic maintenance sweep already
    // relies on for this; running it here too closes that gap for both
    // submission paths that funnel through this function.
    sync_open_pr_branches(store, id);
    {
        let guard = store.lock();
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
            payload: serde_json::json!({"created": created_count, "resynced": resynced}),
            admin_only: false,
        });
    }

    if client.kind() != crate::forge::ForgeKind::GitHub {
        return Ok(());
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
            let pr_view = already_open.get(branch.id.as_str()).copied()?;
            if let Some(routing) = fork_routing {
                if pr_view.repo != routing.fork_client.repo_label() {
                    return None;
                }
            }
            let number = pr_view.pr_number?;
            Some((branch.id.clone(), branch.position, number))
        })
        .collect();
    // `ordered_enabled` is already position-sorted (every caller sorts it
    // before passing it in), so this filter_map preserves bottom-to-top
    // order without a separate sort -- exactly the order `ordered_prs_form_a_chain`
    // needs to check each PR's base against the previous one's pushed branch.
    let ordered_prs: Vec<&PullRequestView> = ordered_enabled
        .iter()
        .filter_map(|branch| {
            let pr_view = already_open.get(branch.id.as_str()).copied()?;
            if let Some(routing) = fork_routing {
                if pr_view.repo != routing.fork_client.repo_label() {
                    return None;
                }
            }
            pr_view.pr_number.is_some().then_some(pr_view)
        })
        .collect();
    let chain_is_valid = ordered_prs_form_a_chain(&ordered_prs);

    let recorded = store
        .lock()
        .get_guardian_forge_stack_number(id)
        .map_err(|e| e.to_string())?;
    let recorded = match recorded {
        Some(stack_number) => match client.get_stack_pull_requests(stack_number) {
            Ok(Some(members)) => Some((stack_number, members)),
            Ok(None) => {
                store
                    .lock()
                    .clear_guardian_forge_stack_number(id)
                    .map_err(|e| e.to_string())?;
                crate::rlog!(
                    WARNING,
                    "ralphus [pr] review {id} recorded github pr stack {stack_number} no longer exists; recreating it"
                );
                None
            }
            Err(e) => {
                crate::rlog!(
                    WARNING,
                    "ralphus [pr] review {id} could not inspect github pr stack {stack_number}; leaving it unchanged: {e}"
                );
                return Ok(());
            }
        },
        None => None,
    };
    match decide_stack_action(&all_with_prs, recorded, chain_is_valid) {
        StackAction::Create { all_ordered } => {
            match create_and_record_native_stack(store, id, client, &all_ordered) {
                Ok(Some(stack_number)) => {
                    crate::rlog!(
                        INFO,
                        "ralphus [pr] review {id} registered github pr stack number={stack_number}"
                    );
                    let guard = store.lock();
                    let _ = guard.cartographer_log(crate::cartographer::CartographerEntry {
                        level: crate::logging::LogLevel::INFO,
                        source: "pr",
                        message: "registered github pr stack",
                        scope: Some("guardian"),
                        squad_id: None,
                        guardian_id: Some(id),
                        cell_id: None,
                        task: None,
                        log_path: None,
                        payload: serde_json::json!({"stack_number": stack_number}),
                        admin_only: false,
                    });
                }
                Ok(None) => {}
                Err(e) => {
                    // ralphus[ignore-rlog-pair]: the surrounding stack-submission handler records the durable workflow outcome after this best-effort native-stack action
                    crate::rlog!(WARNING, "ralphus [pr] review {id} create stack failed: {e}");
                }
            }
        }
        StackAction::Append {
            stack_number,
            missing_ordered,
            all_ordered,
        } => {
            if let Err(e) = client.add_to_stack(stack_number, &missing_ordered) {
                if is_forge_not_found(&e) {
                    let clear_result = store.lock().clear_guardian_forge_stack_number(id);
                    if let Err(clear_error) = clear_result {
                        // ralphus[ignore-rlog-pair]: the surrounding stack-submission handler records the durable workflow outcome after this best-effort native-stack action
                        crate::rlog!(
                            WARNING,
                            "ralphus [pr] review {id} could not clear missing github pr stack {stack_number}: {clear_error}"
                        );
                    } else {
                        match create_and_record_native_stack(store, id, client, &all_ordered) {
                            Ok(Some(new_stack_number)) => {
                                // ralphus[ignore-rlog-pair]: the surrounding stack-submission handler records the durable workflow outcome after this best-effort native-stack action
                                crate::rlog!(
                                    INFO,
                                    "ralphus [pr] review {id} replaced missing github pr stack {stack_number} with stack {}",
                                    new_stack_number
                                );
                            }
                            Ok(None) => {}
                            Err(create_error) => {
                                // ralphus[ignore-rlog-pair]: the surrounding stack-submission handler records the durable workflow outcome after this best-effort native-stack action
                                crate::rlog!(
                                    WARNING,
                                    "ralphus [pr] review {id} could not replace missing github pr stack {stack_number}: {create_error}"
                                );
                            }
                        }
                    }
                } else {
                    // ralphus[ignore-rlog-pair]: the surrounding stack-submission handler records the durable workflow outcome after this best-effort native-stack action
                    crate::rlog!(
                        WARNING,
                        "ralphus [pr] review {id} add to stack {stack_number} failed: {e}"
                    );
                }
            }
        }
        StackAction::Rebuild {
            stack_number,
            all_ordered,
        } => match client.unstack(stack_number) {
            Ok(()) => {
                store
                    .lock()
                    .clear_guardian_forge_stack_number(id)
                    .map_err(|e| e.to_string())?;
                match create_and_record_native_stack(store, id, client, &all_ordered) {
                    Ok(Some(new_stack_number)) => {
                        // ralphus[ignore-rlog-pair]: the surrounding stack-submission handler records the durable workflow outcome after this best-effort native-stack action
                        crate::rlog!(
                            INFO,
                            "ralphus [pr] review {id} rebuilt github pr stack {stack_number} as {new_stack_number}"
                        );
                    }
                    Ok(None) => {}
                    Err(e) => {
                        // ralphus[ignore-rlog-pair]: the surrounding stack-submission handler records the durable workflow outcome after this best-effort native-stack action
                        crate::rlog!(
                            WARNING,
                            "ralphus [pr] review {id} could not rebuild github pr stack {stack_number}: {e}"
                        );
                    }
                }
            }
            Err(e) => {
                // ralphus[ignore-rlog-pair]: the surrounding stack-submission handler records the durable workflow outcome after this best-effort native-stack action
                crate::rlog!(
                    WARNING,
                    "ralphus [pr] review {id} could not dissolve mismatched github pr stack {stack_number}: {e}"
                );
            }
        },
        StackAction::Skip { reason } => {
            // ralphus[ignore-rlog-pair]: the surrounding stack-submission handler records the durable workflow outcome after this best-effort native-stack action
            crate::rlog!(
                DEBUG,
                "ralphus [pr] review {id} skipping stack registration: {reason}"
            );
        }
    }

    Ok(())
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
///
/// The restriction scopes *submission* only. Native-stack membership is
/// reconciled from the review's full enabled branch list, which
/// [`reconcile_native_pr_stack`] reads for itself -- see its doc comment for
/// what handing it this narrowed list used to cost.
///
/// Returns the created PRs alongside `Vec<(branch_id, error)>` for whichever
/// branch(es) failed to submit -- see [`submit_stack_for_guardian`]'s own
/// doc for why one branch's failure doesn't cost its siblings their own
/// attempt or get misattributed to them.
fn auto_submit_terminal_branches(
    store: &crate::store_lock::StoreHandle,
    runner: &dyn Runner,
    id: &str,
) -> std::result::Result<StackSubmitOutcome, String> {
    let guardian = store.lock().get_guardian(id).map_err(|e| e.to_string())?;
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
        return Ok((Vec::new(), Vec::new()));
    }

    let existing_prs = store
        .lock()
        .list_pull_requests_for_guardian(id)
        .map_err(|e| e.to_string())?;
    let mut alias_by_branch = open_alias_by_branch(&existing_prs);

    let stack_id = store
        .lock()
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

/// RAL-317: run one auto-submit-PR-stack pass over a whole review.
///
/// Driven by [`sweep_pending_pr_auto_submits_once`] off the durable,
/// guardian-scoped request queue that every terminal branch transition feeds
/// through [`schedule_auto_submit_branch`]. A no-op unless the review's
/// [`GuardianView::effective_auto_submit_pr_stack`] setting is on.
///
/// One pass already covers the review's whole terminal portion -- that is
/// what [`auto_submit_terminal_branches`] does, and it reports an outcome for
/// every branch it touched -- so this runs it once per sweep tick and stamps
/// each branch from that single shared result. Running it once per terminal
/// branch instead re-walked the entire review N times per tick: only the
/// first pass could create anything, and every later one re-ran a full forge
/// verification (a live-state GET per PR plus the native-stack read) and a
/// full [`sync_open_pr_branches`] git sweep against the shared repository
/// root purely to confirm work already done -- on a repository the merge
/// worker is usually rebasing in at the same time. It also left one review's
/// branches carrying markers stamped from N different forge snapshots.
///
/// Never fails or blocks its caller: every failure is logged and recorded on
/// the branch it belongs to via [`Store::set_branch_auto_submit_error`]
/// rather than propagated, and a success clears any previously-recorded
/// error.
///
/// Reconciles the terminal portion even when every branch in it already has a
/// current PR. A PR row and its pushed SHA prove only that the branch was
/// submitted; they do not prove that its forge-side base and GitHub-native
/// stack membership still match the review.
pub fn run_auto_submit_pass(store: &crate::store_lock::StoreHandle, runner: &dyn Runner, id: &str) {
    let guardian = match store.lock().get_guardian(id) {
        Ok(g) => g,
        Err(_) => return,
    };
    if !guardian.effective_auto_submit_pr_stack {
        return;
    }
    if crate::guardian::GuardianStatus::is_terminal_status(&guardian.status) {
        return;
    }
    let covered: Vec<String> = guardian
        .branches
        .iter()
        .filter(|b| b.enabled && matches!(b.merge_status.as_str(), "done" | "conflict_resolved"))
        .map(|b| b.id.clone())
        .collect();
    if covered.is_empty() {
        return;
    }
    match auto_submit_terminal_branches(store, runner, id) {
        Ok((_created, failed)) => {
            // Every branch this pass covered takes its own outcome from the
            // one shared result, so a sibling's failure never stands in for a
            // branch that succeeded -- that misattribution is what RAL-317's
            // badge appearing on every worktree in a review came down to.
            for branch_id in &covered {
                match failed.iter().find(|(fbid, _)| fbid == branch_id) {
                    Some((_, e)) => record_auto_submit_failure(store, id, branch_id, e),
                    None => record_auto_submit_success(store, id, branch_id),
                }
            }
            // A failure attributed to a branch that is not in `covered` (it
            // left the terminal set after the list above was taken) still
            // belongs to that branch rather than to any of these.
            for (fbid, ferr) in failed.iter().filter(|(fbid, _)| !covered.contains(fbid)) {
                record_auto_submit_failure(store, id, fbid, ferr);
            }
        }
        Err(e) => {
            // A failure that isn't about any one branch (forge/fork
            // resolution, a DB error): no branch owns it, and every branch
            // the pass covered is genuinely blocked by it.
            for branch_id in &covered {
                record_auto_submit_failure(store, id, branch_id, &e);
            }
        }
    }
}

/// Stamp one branch's successful auto-submit-PR-stack outcome: logs, clears
/// the per-branch marker, and emits the matching Cartographer entry. The
/// mirror of [`record_auto_submit_failure`], so both outcomes of a pass are
/// recorded the same way for every branch it covered.
fn record_auto_submit_success(store: &crate::store_lock::StoreHandle, id: &str, branch_id: &str) {
    crate::rlog!(
        INFO,
        "ralphus [pr] review {id} branch {branch_id} auto-submit-pr-stack completed"
    );
    let _ = store
        .lock()
        .set_branch_auto_submit_error(id, branch_id, None);
    let guard = store.lock();
    let _ = guard.cartographer_log(crate::cartographer::CartographerEntry {
        level: crate::logging::LogLevel::INFO,
        source: "pr",
        message: "auto-submit completed",
        scope: Some("branch"),
        squad_id: None,
        guardian_id: Some(id),
        cell_id: None,
        task: None,
        log_path: None,
        payload: serde_json::json!({"branch_id": branch_id}),
        admin_only: false,
    });
}

/// Stamp one branch's auto-submit-PR-stack failure: logs, records the
/// per-branch marker, and emits the matching Cartographer entry. Shared by
/// [`run_auto_submit_pass`]'s two failure sources (a branch named in the
/// pass's own per-branch `failed` list, and a whole-pass error fanned out to
/// every branch the pass covered) so both stamp the *correct* branch
/// identically.
fn record_auto_submit_failure(
    store: &crate::store_lock::StoreHandle,
    id: &str,
    branch_id: &str,
    error: &str,
) {
    crate::rlog!(
        WARNING,
        "ralphus [pr] review {id} branch {branch_id} auto-submit-pr-stack failed: {error}"
    );
    let _ = store
        .lock()
        .set_branch_auto_submit_error(id, branch_id, Some(error));
    let guard = store.lock();
    let _ = guard.cartographer_log(crate::cartographer::CartographerEntry {
        level: crate::logging::LogLevel::WARNING,
        source: "pr",
        message: "auto-submit failed",
        scope: Some("branch"),
        squad_id: None,
        guardian_id: Some(id),
        cell_id: None,
        task: None,
        log_path: None,
        payload: serde_json::json!({"branch_id": branch_id, "error": error}),
        admin_only: false,
    });
}

/// RAL-389: trailing-debounce window that coalesces branches completing in
/// the same restack pass before PR submission begins off-thread.
const AUTO_SUBMIT_DEBOUNCE_MS: i64 = 400;

/// Queue a guardian for asynchronous PR-stack submission. The durable row is
/// guardian-scoped; the worker reads the terminal branches fresh when it runs.
pub(crate) fn schedule_auto_submit_branch(
    store: &crate::store_lock::StoreHandle,
    id: &str,
    branch_id: &str,
) {
    let requested_at_ms = now_ms();
    if let Err(e) = store.lock().request_auto_submit_branch(id, requested_at_ms) {
        crate::rlog!(
            WARNING,
            "ralphus [pr] review {id} branch {branch_id} failed to queue auto-submit request: {e}"
        );
        let guard = store.lock();
        let _ = guard.cartographer_log(crate::cartographer::CartographerEntry {
            level: crate::logging::LogLevel::WARNING,
            source: "pr",
            message: "auto-submit request queue failed",
            scope: Some("guardian"),
            squad_id: None,
            guardian_id: Some(id),
            cell_id: None,
            task: None,
            log_path: None,
            payload: serde_json::json!({"branch_id": branch_id, "error": e.to_string()}),
            admin_only: false,
        });
    } else {
        crate::rlog!(
            INFO,
            "ralphus [pr] review {id} branch {branch_id} auto-submit-pr-stack queued"
        );
        let guard = store.lock();
        let _ = guard.cartographer_log(crate::cartographer::CartographerEntry {
            level: crate::logging::LogLevel::INFO,
            source: "pr",
            message: "auto-submit queued",
            scope: Some("branch"),
            squad_id: None,
            guardian_id: Some(id),
            cell_id: None,
            task: None,
            log_path: None,
            payload: serde_json::json!({"branch_id": branch_id, "requested_at_ms": requested_at_ms}),
            admin_only: false,
        });
    }
}

/// Guardian ids currently being processed by the asynchronous auto-submit
/// sweep. The claim prevents later ticks from racing a slow forge operation.
static AUTO_SUBMIT_IN_FLIGHT: LazyLock<Mutex<HashSet<String>>> =
    LazyLock::new(|| Mutex::new(HashSet::new()));

/// Claim due requests and run one [`run_auto_submit_pass`] per guardian on
/// its own background thread. That single pass covers every enabled terminal
/// branch through the normal stack-aware submission path, preserving the
/// review's linear PR/MR chain.
pub fn sweep_pending_pr_auto_submits_once(store: &crate::store_lock::StoreHandle) {
    let due = match store
        .lock()
        .take_due_auto_submits(now_ms(), AUTO_SUBMIT_DEBOUNCE_MS)
    {
        Ok(due) => due,
        Err(e) => {
            crate::rlog!(
                ERROR,
                "ralphus [pr] auto-submit sweep: listing due guardians failed: {e}"
            );
            return;
        }
    };
    for id in due {
        let Some(claim) = guardian_merge::InFlightClaim::acquire(&AUTO_SUBMIT_IN_FLIGHT, &id)
        else {
            let _ = store.lock().request_auto_submit_branch(&id, now_ms());
            continue;
        };
        let store = Arc::clone(store);
        std::thread::spawn(move || {
            let _claim = claim;
            let runner = crate::runner::SubprocessRunner::from_env();
            run_auto_submit_pass(&store, &runner, &id);
        });
    }
}

/// Re-queue eligible terminal branches at startup, closing the crash window
/// between their terminal-status update and durable request insertion.
pub fn recover_pending_auto_submits_on_startup(store: &crate::store_lock::StoreHandle) {
    let guard = store.lock();
    let guardians = match guard.list_guardians() {
        Ok(guardians) => guardians,
        Err(e) => {
            crate::rlog!(
                ERROR,
                "ralphus [pr] auto-submit startup recovery: listing guardians failed: {e}"
            );
            let _ = guard.cartographer_log(crate::cartographer::CartographerEntry {
                level: crate::logging::LogLevel::ERROR,
                source: "pr",
                message: "auto-submit startup recovery failed",
                scope: None,
                squad_id: None,
                guardian_id: None,
                cell_id: None,
                task: None,
                log_path: None,
                payload: serde_json::json!({"error": e.to_string()}),
                admin_only: false,
            });
            return;
        }
    };
    let now = now_ms();
    for guardian in guardians {
        if !guardian.effective_auto_submit_pr_stack {
            continue;
        }
        if crate::guardian::GuardianStatus::is_terminal_status(&guardian.status) {
            continue;
        }
        let has_terminal = guardian.branches.iter().any(|branch| {
            branch.enabled && matches!(branch.merge_status.as_str(), "done" | "conflict_resolved")
        });
        if has_terminal {
            let _ = guard.request_auto_submit_branch(&guardian.id, now);
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn submit_pull_requests_inner(
    store: &crate::store_lock::StoreHandle,
    runner: &dyn Runner,
    id: &str,
    requests: Vec<PrRequest>,
    trace_context: Option<&str>,
    user: &str,
    allow_unlinked_fork: bool,
) -> std::result::Result<Vec<PullRequestView>, String> {
    let guardian = store.lock().get_guardian(id).map_err(|e| e.to_string())?;
    if crate::guardian::GuardianStatus::is_terminal_status(&guardian.status) {
        return Err(format!(
            "review {id} is {} and read-only until reopened",
            guardian.status
        ));
    }
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
        let (stack_prs, failed) = submit_stack_for_guardian(
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
        // An explicit "submit the whole stack" request is a user action, not
        // a background poll -- report every branch that failed (not just the
        // first) rather than the per-branch auto-submit path's silent
        // per-branch attribution, so the caller sees the full picture in one
        // response.
        if !failed.is_empty() {
            return Err(failed
                .into_iter()
                .map(|(branch_id, e)| format!("branch {branch_id}: {e}"))
                .collect::<Vec<_>>()
                .join("; "));
        }
    } else if !created.is_empty() {
        // RAL-395: an explicit per-branch/positional request (no `branch_id:
        // null` in this call) skips `submit_stack_for_guardian` entirely, so
        // without this it never reconciled the native GitHub PR-stack
        // grouping -- a review submitted one branch at a time (e.g. the CLI's
        // `ralphus review pr submit <id> --position N`, called once per
        // branch) ended up with correctly chained base refs but no forge-side
        // stack object linking them. Every submission path now funnels
        // through the same reconciler.
        reconcile_native_pr_stack(store, &client, id, fork_routing.as_ref(), created.len())?;
    }

    Ok(created)
}

// ---------------------------------------------------------------------------
// Bidirectional sync (RAL-190)
// ---------------------------------------------------------------------------

/// One lock per `(repository root, PR)`, serializing repeat/concurrent
/// `git fetch` + `rev-parse` pairs in [`compute_sync_status`] for the *same*
/// PR. Also used by the RAL-366 forge-cache poller's drift pass, which
/// acquires a PR's entry via `try_lock` and releases it again before making
/// any forge *comment* network call for that PR (RAL-423) -- so a poller
/// pass can never extend an interactive [`compute_sync_status`] wait beyond
/// that PR's own git fetch.
///
/// Originally this was one lock per repository root: `git fetch` with no
/// explicit destination writes `FETCH_HEAD`, a single file shared by the
/// whole repository, so two fetches racing in it could have either one's
/// `rev-parse FETCH_HEAD` read the other's result -- reporting a PR as
/// ahead/behind against a sibling PR's tip. That made every open PR on a
/// stacked review serialize behind one lock, even though they have nothing
/// to do with each other -- the board fetches every open PR's sync-status
/// for a review concurrently (`pollPullRequests` in `70-sse.js`), and the
/// daemon answers read-only requests on a pool of threads
/// (`server::ReadPool`), so a stacked review's PRs land here at the same
/// time in the same repository.
///
/// [`compute_sync_status`] now fetches into a PR-scoped ref
/// (`refs/ralphus/sync/<pr_id>`) instead of relying on `FETCH_HEAD`, so two
/// different PRs' fetches no longer share any mutable state and don't need
/// to serialize at all. The lock is kept, scoped down to `(root, pr_id)`,
/// only to protect a PR's *own* ref against a genuinely concurrent re-check
/// of that same PR (e.g. two board tabs polling at once).
type SyncFetchLocks = HashMap<(PathBuf, String), Arc<Mutex<()>>>;
static SYNC_FETCH_LOCKS: Mutex<Option<SyncFetchLocks>> = Mutex::new(None);

/// The [`SYNC_FETCH_LOCKS`] entry for `(root, pr_id)`, creating it on first use.
fn sync_fetch_lock(root: &Path, pr_id: &str) -> Arc<Mutex<()>> {
    let mut locks = SYNC_FETCH_LOCKS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    Arc::clone(
        locks
            .get_or_insert_with(HashMap::new)
            .entry((root.to_path_buf(), pr_id.to_string()))
            .or_default(),
    )
}

/// The local ref [`compute_sync_status`] fetches a PR's remote branch tip
/// into, scoped per-PR so concurrent fetches for different PRs never
/// contend for the same ref (or, pre-this-fix, the same `FETCH_HEAD`).
///
/// `pr_id`s are daemon-generated (`Store::next_id`, always `[a-z0-9-]+`), so
/// this sanitization is defense-in-depth rather than a load-bearing check --
/// a git ref component may not contain whitespace, `~^:?*[`, `..`, or a
/// leading/trailing `/`.
fn sync_fetch_ref(pr_id: &str) -> String {
    let cleaned: String = pr_id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    format!("refs/ralphus/sync/{cleaned}")
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

/// What one half of a poll pass did, as reported to
/// [`Store::upsert_pr_forge_cache`].
///
/// Three distinct states, and the difference between the last two is the whole
/// point: `None` means this pass never attempted the half (so its stored
/// timestamp must not move), `Some(Err(_))` means it tried and failed (so the
/// timestamp moves but the status goes `unknown` and the stale values stand).
/// Collapsing those two into one made a skipped half indistinguishable from a
/// freshly-verified one.
pub(crate) type HalfOutcome<T> = Option<std::result::Result<T, String>>;

/// The git-side half of a poll pass: how the PR's remote branch compares to
/// its review worktree. Mirrors the fields of [`PrSyncStatus`].
pub(crate) struct DriftObservation<'a> {
    pub in_sync: bool,
    pub pr_ahead: bool,
    pub worktree_ahead: bool,
    pub remote_sha: Option<&'a str>,
    pub local_sha: Option<&'a str>,
}

/// The forge-side half of a poll pass: the ETags to send on the next
/// conditional comment fetch. `None` for an endpoint this forge does not have
/// or that returned no ETag.
pub(crate) struct CommentsObservation<'a> {
    pub etag_conversation: Option<&'a str>,
    pub etag_review: Option<&'a str>,
}

/// How long [`compute_sync_status`] may take before it is reported as slow.
///
/// Its own work is one `git fetch` of a single refspec plus a few `rev-parse`
/// calls -- a couple of seconds against a real forge. This sits well clear of
/// that, so an ordinary call never logs and only genuine lock contention does.
const SYNC_STATUS_SLOW_THRESHOLD: std::time::Duration = std::time::Duration::from_secs(5);

/// The review worktree ref a PR compares its remote branch against (RAL-190):
/// a stacked branch's own `review_branch` when `pr.branch_id` names one,
/// else the guardian's combined `review_branch`. Shared by
/// [`compute_sync_status`] and the RAL-366 cache poller's batched drift check
/// so the two never disagree about which local ref a PR's drift is measured
/// against.
fn local_ref_for_pr(guardian: &GuardianView, pr: &PullRequestView) -> Option<String> {
    if let Some(bid) = &pr.branch_id {
        guardian
            .branches
            .iter()
            .find(|b| &b.id == bid)
            .and_then(|b| b.review_branch.clone())
    } else {
        guardian.review_branch.clone()
    }
}

/// Classify drift between a PR's remote branch tip and its review worktree
/// tip into `(pr_ahead, worktree_ahead, in_sync)` (RAL-190) -- the
/// comparison [`compute_sync_status`] and the RAL-366 cache poller's batched
/// drift check both need, extracted so the two can never classify the same
/// (remote, local, last_pushed) triple differently. See
/// [`compute_sync_status`]'s prior inline version for the full rationale on
/// preferring `last_pushed` over raw ancestry.
fn classify_sync_drift(
    root: &Path,
    remote_sha: Option<&str>,
    local_sha: Option<&str>,
    last_pushed: Option<&str>,
) -> (bool, bool, bool) {
    let is_ancestor = |ancestor: &str, descendant: &str| {
        git(root, &["merge-base", "--is-ancestor", ancestor, descendant]).is_ok()
    };
    match (remote_sha, local_sha) {
        (Some(r), Some(l)) if r == l => (false, false, true),
        (Some(r), Some(l)) => match last_pushed {
            Some(p) if p == r => (false, true, false),
            Some(p) if p == l => (true, false, false),
            _ => (!is_ancestor(r, l), !is_ancestor(l, r), false),
        },
        (Some(_), None) => (true, false, false),
        (None, Some(_)) => (false, true, false),
        (None, None) => (false, false, false),
    }
}

/// The network half of [`compute_sync_status`]: fetch a PR branch's current
/// remote tip into its private sync ref, guarded by `sync_fetch_lock` exactly
/// like the combined function. Split out so [`sync_remote_pr_commits`] can run
/// it concurrently across every open PR -- the remote side is independent of
/// every other PR's -- while the local-side comparison stays serial (see that
/// function's comment on why).
fn fetch_remote_pr_tip(
    store: &crate::store_lock::StoreHandle,
    pr_id: &str,
) -> std::result::Result<Option<String>, String> {
    let pr = store
        .lock()
        .get_pull_request(pr_id)
        .map_err(|e| e.to_string())?;
    let guardian = store
        .lock()
        .get_guardian(&pr.guardian_id)
        .map_err(|e| e.to_string())?;
    let root = PathBuf::from(&guardian.git_root);
    let forge_cfg = crate::config::resolve_forge(&root);
    // RAL-338: this PR's branch alias lives on the fork's remote in fork
    // mode, including the root's (its PR is filed against the parent, but
    // its ref still lives on the fork).
    let routing = resolve_pr_repo_routing(store, &root, &guardian.base_branch, &forge_cfg);
    let remote_name = routing.remote_for(&pr.repo).to_string();

    // Held across both commands so a concurrent re-check of this same PR
    // can't read back a fetch this one hasn't written yet. See
    // `SYNC_FETCH_LOCKS`.
    let fetch_lock = sync_fetch_lock(&root, pr_id);
    let _fetching = fetch_lock
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let dest_ref = sync_fetch_ref(pr_id);
    // `+` forces the update: a reviewer force-pushing the PR branch
    // (an amend, a rebase onto a new base) makes its new tip a
    // non-fast-forward from whatever this ref last pointed at, which a
    // plain refspec would otherwise refuse to write.
    let refspec = format!("+{}:{dest_ref}", pr.branch_alias);
    Ok(git(&root, &["fetch", &remote_name, &refspec])
        .ok()
        .and_then(|_| git(&root, &["rev-parse", &dest_ref]).ok())
        .map(|s| s.trim().to_string()))
}

/// The comparison logic shared by [`compute_sync_status`] and
/// [`sync_remote_pr_commits`]: given a remote tip, a local tip, and the last
/// SHA this daemon itself pushed, classify which side (if either) is ahead as
/// `(pr_ahead, worktree_ahead, in_sync)`.
///
/// RAL-190: prefers `last_pushed_sha` -- the SHA this daemon itself last put
/// on the PR branch -- over raw ancestry. Ancestry alone can't survive a
/// rebase: replaying a branch onto a shifted base rewrites every commit's
/// SHA, so neither tip stays an ancestor of the other even when nothing
/// genuinely diverged (a clean rebase-through, or a conflict that got
/// resolved -- however much effort that took). `last_pushed_sha` pins the
/// fork point to "the last state both sides are known to have agreed on": if
/// only one side has moved away from it, that side is unambiguously ahead
/// regardless of how it got there. Ancestry is still the fallback when
/// neither side matches the fork point (never synced yet, or both sides
/// changed independently since) -- that's a true two-sided divergence.
fn classify_pr_sync(
    root: &Path,
    remote_sha: Option<&str>,
    local_sha: Option<&str>,
    last_pushed: Option<&str>,
) -> (bool, bool, bool) {
    let is_ancestor = |ancestor: &str, descendant: &str| {
        git(root, &["merge-base", "--is-ancestor", ancestor, descendant]).is_ok()
    };
    match (remote_sha, local_sha) {
        (Some(r), Some(l)) if r == l => (false, false, true),
        (Some(r), Some(l)) => match last_pushed {
            Some(p) if p == r => (false, true, false),
            Some(p) if p == l => (true, false, false),
            _ => (!is_ancestor(r, l), !is_ancestor(l, r), false),
        },
        (Some(_), None) => (true, false, false),
        (None, Some(_)) => (false, true, false),
        (None, None) => (false, false, false),
    }
}

/// This PR's cached remote branch tip, if the drift half of a poll pass
/// recorded one recently and successfully.
///
/// Reads `drift_checked_at_ms`/`drift_status` -- **not** the rolled-up
/// `last_checked_at_ms`/`status`, which also move when only the comment half
/// ran and would therefore report a stale remote tip as fresh.
fn fresh_cached_remote_tip(store: &crate::store_lock::StoreHandle, pr_id: &str) -> Option<String> {
    let cache = store.lock().get_pr_forge_cache(pr_id).ok().flatten()?;
    if cache.drift_status.as_deref() != Some("ok") {
        return None;
    }
    let checked_at = cache.drift_checked_at_ms?;
    let age_ms = now_ms().saturating_sub(checked_at);
    if age_ms < 0 || age_ms as u128 > REMOTE_TIP_MAX_AGE.as_millis() {
        return None;
    }
    cache.remote_sha
}

/// Compute [`PrSyncStatus`] for `pr_id`: fetches the remote `branch_alias`
/// tip and compares it against the owning review worktree's current tip via
/// `git merge-base --is-ancestor` in both directions. A combined-worktree PR
/// (`branch_id = None`) compares against the guardian's combined review
/// branch; a stacked PR compares against its own branch's review branch.
///
/// Backs `GET /api/pull-requests/{id}/sync-status`, which the board calls once
/// per open PR of the selected review. It performs a real `git fetch` behind a
/// per-PR lock, so it is inherently slower than a store read and is timed: a
/// call that takes far longer than its own fetch should is reported, since the
/// only way that happens is contention on [`SYNC_FETCH_LOCKS`] -- and a call
/// blocked there holds one of the daemon's read-pool workers for the duration,
/// starving unrelated reads queued behind it.
pub fn compute_sync_status(
    store: &crate::store_lock::StoreHandle,
    pr_id: &str,
) -> std::result::Result<PrSyncStatus, String> {
    let t0 = std::time::Instant::now();
    {
        let guard = store.lock();
        let _ = guard.cartographer_log(crate::cartographer::CartographerEntry {
            level: crate::logging::LogLevel::INFO,
            source: "pr",
            message: "PR sync status check starting",
            scope: Some("guardian"),
            squad_id: None,
            guardian_id: None,
            cell_id: None,
            task: None,
            log_path: None,
            payload: serde_json::json!({"pr_id": pr_id}),
            admin_only: false,
        });
    }
    crate::rlog!(
        INFO,
        "ralphus [pr] PR sync status check starting pr_id={pr_id}"
    );

    // RAL-422: the completion record must fire even on early-return errors,
    // so the cartographer log stays paired. The heavy work is wrapped; the
    // outer scope only logs and returns.
    let (result, guardian_id_str): (std::result::Result<PrSyncStatus, String>, Option<String>) =
        match compute_sync_status_inner(store, pr_id, RemoteTip::Live) {
            Ok(status) => {
                let gid = store
                    .lock()
                    .get_pull_request(pr_id)
                    .ok()
                    .map(|pr| pr.guardian_id);
                (Ok(status), gid)
            }
            Err(e) => (Err(e), None),
        };
    let elapsed = t0.elapsed().as_secs_f64();
    let level = if result.is_ok() {
        crate::logging::LogLevel::INFO
    } else {
        crate::logging::LogLevel::WARNING
    };
    {
        let guard = store.lock();
        let _ = guard.cartographer_log(crate::cartographer::CartographerEntry {
            level,
            source: "pr",
            message: &format!("PR sync status check completed ({:.1}s)", elapsed),
            scope: Some("guardian"),
            squad_id: None,
            guardian_id: guardian_id_str.as_deref(),
            cell_id: None,
            task: None,
            log_path: None,
            payload: serde_json::json!({
                "pr_id": pr_id,
                "elapsed_s": elapsed,
                "ok": result.is_ok(),
                "error": result.as_ref().err(),
            }),
            admin_only: false,
        });
    }
    crate::rlog!(
        INFO,
        "ralphus [pr] PR sync status check completed pr_id={pr_id} elapsed={elapsed:.1}s"
    );
    result
}

/// [`compute_sync_status`], but allowed to reuse the cache poller's recent
/// remote-tip observation instead of fetching one.
///
/// Returns the status plus **whether the remote tip was fetched live**. A
/// caller writing the result back into the cache must honour that flag:
/// writing a cached reading back would move its `drift_checked_at_ms` without
/// anything having been re-verified, so a single stale observation could keep
/// renewing its own freshness indefinitely.
pub fn compute_sync_status_cached(
    store: &crate::store_lock::StoreHandle,
    pr_id: &str,
) -> std::result::Result<(PrSyncStatus, bool), String> {
    let served_from_cache = fresh_cached_remote_tip(store, pr_id).is_some();
    let status = compute_sync_status_inner(store, pr_id, RemoteTip::CachedIfFresh)?;
    Ok((status, !served_from_cache))
}

/// Where [`compute_sync_status_inner`] gets the PR's remote branch tip.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemoteTip {
    /// Fetch it from the forge now. Correct at the instant of the call, and
    /// costs a `git fetch` behind this PR's [`SYNC_FETCH_LOCKS`] entry.
    Live,
    /// Reuse the cache poller's last observation when it is recent enough,
    /// falling back to [`Self::Live`] when it is missing, stale, or was
    /// recorded by a failed pass.
    CachedIfFresh,
}

/// How old the cache poller's remote-tip observation may be before
/// [`RemoteTip::CachedIfFresh`] refuses it and fetches live instead.
///
/// Bounds one thing only: how long a reviewer's brand-new push can go
/// unnoticed by a list view. Everything else in the comparison is recomputed
/// live on every call.
const REMOTE_TIP_MAX_AGE: std::time::Duration = std::time::Duration::from_secs(120);

/// Shared body of [`compute_sync_status`].
///
/// The local tip is **always** read live, and that is the load-bearing part.
/// Drift compares two tips, and the local one is moved by ralphus itself on
/// every rebase, restack and feedback pass -- nothing invalidates the cache
/// when that happens, so a cached local tip can be several rebases out of date
/// while the row still looks recently checked. Reading it live costs one
/// `git rev-parse`: no network, no lock, sub-millisecond.
///
/// Only the remote tip is ever served from cache, because only it needs a
/// network round-trip. The worst case is therefore bounded and explainable:
/// "a push made in the last [`REMOTE_TIP_MAX_AGE`] may not show yet", never
/// "this comparison is against a branch state that no longer exists".
fn compute_sync_status_inner(
    store: &crate::store_lock::StoreHandle,
    pr_id: &str,
    remote_tip: RemoteTip,
) -> std::result::Result<PrSyncStatus, String> {
    let started = std::time::Instant::now();
    let pr = store
        .lock()
        .get_pull_request(pr_id)
        .map_err(|e| e.to_string())?;
    let guardian = store
        .lock()
        .get_guardian(&pr.guardian_id)
        .map_err(|e| e.to_string())?;
    let root = PathBuf::from(&guardian.git_root);

    let local_ref = local_ref_for_pr(&guardian, &pr);
    let local_sha = local_ref
        .as_deref()
        .and_then(|r| git(&root, &["rev-parse", r]).ok())
        .map(|s| s.trim().to_string());
    let cached_remote = match remote_tip {
        RemoteTip::Live => None,
        RemoteTip::CachedIfFresh => fresh_cached_remote_tip(store, pr_id),
    };
    let remote_sha = match cached_remote {
        Some(sha) => Some(sha),
        None => fetch_remote_pr_tip(store, pr_id)?,
    };

    let (pr_ahead, worktree_ahead, in_sync) = classify_pr_sync(
        &root,
        remote_sha.as_deref(),
        local_sha.as_deref(),
        pr.last_pushed_sha.as_deref(),
    );

    // A single `git fetch` of one refspec is the dominant cost here; anything
    // far past that is time spent waiting on `SYNC_FETCH_LOCKS`, not working.
    // Reported at WARNING because it is invisible from the endpoint's own
    // response and is exactly what makes the board look frozen.
    let elapsed = started.elapsed();
    if elapsed >= SYNC_STATUS_SLOW_THRESHOLD {
        crate::cartographer::Note::new("pr")
            .level(crate::logging::LogLevel::WARNING)
            .scope("guardian")
            .guardian(&pr.guardian_id)
            .emit(
                &store.lock(),
                format!(
                    "review {} pr={pr_id} sync-status took {}ms -- likely contention on \
                     this PR's fetch lock",
                    pr.guardian_id,
                    elapsed.as_millis()
                ),
                serde_json::json!({
                    "pr_id": pr_id,
                    "elapsed_ms": u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX),
                }),
            );
    }

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
    store: &crate::store_lock::StoreHandle,
    runner: &dyn Runner,
    pr_id: &str,
) -> std::result::Result<bool, String> {
    let pr = store
        .lock()
        .get_pull_request(pr_id)
        .map_err(|e| e.to_string())?;
    let guardian = store
        .lock()
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
        let _ = store.lock().update_pull_request_ex(
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
        let guard = store.lock();
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

/// Fetch every open PR branch in `id` and incorporate any forge-side commits
/// into its review worktree before another local rebase can run. PRs are
/// considered in stack order, so pulling a lower branch restacks its
/// descendants before their own remote tips are checked.
///
/// This is deliberately the same conflict-aware pull path the explicit board
/// action uses, rather than a plain `git pull`: it preserves remote commits,
/// resolves conflicts through the review resolver, restacks downstream review
/// branches, and writes the resulting tips back to the PR branches.
///
/// Returns how many PR branches supplied commits. An idle review with no
/// remote changes only performs the inexpensive fetch-and-compare checks.
pub fn sync_remote_pr_commits(
    store: &crate::store_lock::StoreHandle,
    runner: &dyn Runner,
    id: &str,
) -> std::result::Result<usize, String> {
    let guardian = store.lock().get_guardian(id).map_err(|e| e.to_string())?;
    if guardian.status.as_str() != "in_review" {
        return Ok(0);
    }

    let positions: HashMap<&str, i64> = guardian
        .branches
        .iter()
        .map(|branch| (branch.id.as_str(), branch.position))
        .collect();
    let mut pr_ids: Vec<_> = store
        .lock()
        .list_pull_requests_for_guardian(id)
        .map_err(|e| e.to_string())?
        .into_iter()
        .filter(|pr| pr.state == "open")
        .collect();
    pr_ids.sort_by_key(|pr| {
        pr.branch_id
            .as_deref()
            .and_then(|branch_id| positions.get(branch_id).copied())
            .unwrap_or(i64::MAX)
    });

    // Every open PR's remote tip is independent of every other PR's -- fetch
    // them all at once instead of one at a time, cutting this from N
    // sequential network round trips to one. The local-side comparison and
    // the actual pull stay serial below, in stack order: pulling a lower
    // branch restacks its descendants' worktrees, so a descendant's local
    // tip is only meaningful once every earlier branch has already been
    // pulled.
    let remote_tips: Vec<std::result::Result<Option<String>, String>> =
        std::thread::scope(|scope| {
            let handles: Vec<_> = pr_ids
                .iter()
                .map(|pr| scope.spawn(|| fetch_remote_pr_tip(store, &pr.id)))
                .collect();
            handles
                .into_iter()
                .map(|handle| {
                    handle
                        .join()
                        .unwrap_or_else(|_| Err("remote PR fetch panicked".to_string()))
                })
                .collect()
        });

    let mut pulled = 0;
    for (pr, remote_sha) in pr_ids.iter().zip(remote_tips) {
        let remote_sha = remote_sha?;
        let guardian = store
            .lock()
            .get_guardian(&pr.guardian_id)
            .map_err(|e| e.to_string())?;
        let root = PathBuf::from(&guardian.git_root);
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
        let (pr_ahead, _, _) = classify_pr_sync(
            &root,
            remote_sha.as_deref(),
            local_sha.as_deref(),
            pr.last_pushed_sha.as_deref(),
        );
        if !pr_ahead {
            continue;
        }
        if pull_pr_commits(store, runner, &pr.id)
            .map_err(|e| format!("could not pull remote commits for PR {}: {e}", pr.id))?
        {
            pulled += 1;
        }
    }
    Ok(pulled)
}

/// Kick off [`pull_pr_commits`] in the background; returns immediately.
pub fn start_pull_pr_commits(
    store: crate::store_lock::StoreHandle,
    runner: Arc<dyn Runner>,
    pr_id: &str,
) -> Reply {
    let pr = {
        let guard = store.lock();
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
                let guard = store.lock();
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
                let guard = store.lock();
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

/// Fallback `author` for [`action_pr_feedback`] when no requester identity
/// resolved at the HTTP boundary (e.g. no `X-Ralphus-User` header and no
/// `[daemon].default_user` configured) -- distinct from
/// [`crate::ci_watch::AUTO_FIX_AUTHOR`] so a manual, person-initiated action
/// never renders under the automated system's own name.
const MANUAL_PR_FEEDBACK_AUTHOR: &str = "Manual (PR feedback)";

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
///
/// `submitted_by` is the registered user who triggered this action (resolved
/// at the HTTP boundary, same as [`guardian_merge::start_feedback`]) -- it is
/// posted as this round's message `author`/`submitted_by` (RAL-379
/// semantics) so the board's chat thread clearly shows a person asked for
/// this, never something that reads as coming from an automated system.
pub fn action_pr_feedback(
    store: &crate::store_lock::StoreHandle,
    runner: &dyn Runner,
    pr_id: &str,
    submitted_by: Option<&str>,
) -> std::result::Result<usize, String> {
    let cx = crate::otel::context_from_traceparent(None);
    let span = crate::otel::start_span("pr.action_feedback", &cx, SpanKind::Internal);
    span.set_attribute("pr_id", pr_id.to_string());

    // ralphus[ignore-rlog-pair]: this low-level helper has no Store; its Store-owning caller records the structured workflow outcome
    crate::rlog!(INFO, "ralphus [pr] pr {pr_id} actioning feedback");
    let result = action_pr_feedback_inner(store, runner, pr_id, submitted_by);
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
    store: &crate::store_lock::StoreHandle,
    runner: &dyn Runner,
    pr_id: &str,
    submitted_by: Option<&str>,
) -> std::result::Result<usize, String> {
    let pr = store
        .lock()
        .get_pull_request(pr_id)
        .map_err(|e| e.to_string())?;
    let guardian = store
        .lock()
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

    // Resolved before claiming any comments below (see `try_claim_pr_comments`'s
    // doc comment on claim-before-apply ordering): this lookup can fail (e.g.
    // the branch was removed from the guardian since the PR was filed), and a
    // claim taken before that failure would strand those comments as
    // permanently "actioned" without ever having actually been applied.
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

    let comments = client.list_pr_comments(pr_number)?;
    // RAL-<new>: claim before applying, not after -- see `try_claim_pr_comments`'s
    // doc comment. Whatever this call wins is exactly what it (and only it)
    // is responsible for applying; any id it doesn't win was already claimed
    // by an earlier, or concurrently overlapping, call.
    let all_ids: Vec<String> = comments.iter().map(|c| c.external_id.clone()).collect();
    let claimed = store
        .lock()
        .try_claim_pr_comments(pr_id, &all_ids)
        .map_err(|e| e.to_string())?;
    let fresh: Vec<_> = comments
        .into_iter()
        .filter(|c| claimed.contains(&c.external_id))
        .collect();
    // ralphus[ignore-rlog-pair]: this low-level helper has no Store; its Store-owning caller records the structured workflow outcome
    crate::rlog!(
        DEBUG,
        "ralphus [pr] pr {pr_id} comments fresh={} already_actioned={}",
        fresh.len(),
        all_ids.len() - fresh.len()
    );
    // RAL-<new>: unlike the old version of this function, an empty `fresh`
    // no longer early-returns -- "Action feedback" is a manual override a
    // person clicks expecting *something* to happen, and a PR can easily
    // have zero un-actioned comments while still having failing CI (see the
    // live-CI-fix step below). Bailing out here on comments alone silently
    // did nothing for that case.
    if fresh.is_empty() {
        // ralphus[ignore-rlog-pair]: this low-level helper has no Store; its Store-owning caller records the structured workflow outcome
        crate::rlog!(DEBUG, "ralphus [pr] pr {pr_id} no new comments to action");
    } else {
        let feedback = fresh
            .iter()
            .map(|c| format!("{}: {}", c.author, c.body))
            .collect::<Vec<_>>()
            .join("\n\n---\n\n");

        // RAL-<new>: post this round into the branch's feedback thread the
        // same way `guardian_merge::start_feedback` does for a human
        // reviewer and `ci_watch::dispatch_pr_auto_fix` does for its
        // automated fixes -- attributed to the person who triggered this
        // action (`submitted_by`, resolved at the HTTP boundary), never to a
        // value that could read as automated. Falls back to
        // `MANUAL_PR_FEEDBACK_AUTHOR` only when no requester identity was
        // resolvable at all, so the bubble still reads as a manual,
        // person-initiated action rather than defaulting to silence.
        // Superseding any still-pending feedback first mirrors
        // `start_feedback`'s own invariant: an older bubble must never read
        // as in-progress once this round has overtaken it.
        let author = submitted_by
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or(MANUAL_PR_FEEDBACK_AUTHOR);
        let _ = store
            .lock()
            .supersede_pending_branch_feedback(&pr.guardian_id, &branch_id);
        let message_seq = store
            .lock()
            .add_guardian_message(
                &pr.guardian_id,
                "reviewer",
                &feedback,
                None,
                Some(&branch_id),
                Some(author),
                submitted_by,
            )
            .ok();

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
            message_seq,
            false,
            &crate::cancel::CancelToken::never(),
        );

        if pr.branch_id.is_some() {
            // `run_feedback` already committed (amend-aware) and pushed this
            // exact branch's own review ref -- just record the sha it pushed
            // rather than re-pushing (and re-guarding-against-clobber) the
            // identical ref a second time.
            if let Some(sha) = outcome.pushed_sha.filter(|_| outcome.pushed) {
                let _ = store.lock().update_pull_request_ex(
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
                let _ = store.lock().update_pull_request_ex(
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
    }

    // RAL-<new>: "Action feedback" is a manual override -- besides pulling in
    // comments above, also check this PR's live CI/mergeability status and,
    // if it's currently failing, dispatch a fix right now via
    // `ci_watch::dispatch_pr_fix_manual`, which deliberately bypasses both
    // `auto_fix_pr_errors` (that toggle gates the *unattended* background
    // poller, not an explicit person-initiated click) and the single-
    // attempt-per-failure cap (a person retrying by hand is exactly the case
    // that cap must not block). Best-effort: a transient forge error here
    // must not undo the comment-feedback work already applied above.
    if let Ok(state) = client.check_pr_ci_status(pr_number) {
        let job_url = match &state {
            crate::forge::PrCiState::Failing(f) => f.job_url.clone(),
            _ => None,
        };
        let _ = store
            .lock()
            .set_pr_ci_status(pr_id, state.as_str(), job_url.as_deref());
        if let crate::forge::PrCiState::Failing(failure) = state {
            crate::ci_watch::dispatch_pr_fix_manual(
                store,
                runner,
                &guardian,
                &pr,
                &branch_id,
                &failure,
                &client,
                submitted_by,
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
    store: crate::store_lock::StoreHandle,
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
        let guard = store.lock();
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
                let guard = store.lock();
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
                let guard = store.lock();
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
/// `submitted_by` is threaded through to [`action_pr_feedback`] for message
/// attribution (RAL-379) -- see its doc comment.
pub fn start_action_pr_feedback(
    store: crate::store_lock::StoreHandle,
    runner: Arc<dyn Runner>,
    pr_id: &str,
    submitted_by: Option<String>,
) -> Reply {
    let pr = {
        let guard = store.lock();
        guard.get_pull_request(pr_id)
    };
    let guardian_id = match pr {
        Ok(pr) => pr.guardian_id,
        Err(e) => return error_reply(404, "not_found", &e.to_string()),
    };
    let pid = pr_id.to_string();
    std::thread::spawn(move || {
        match action_pr_feedback(&store, runner.as_ref(), &pid, submitted_by.as_deref()) {
            Ok(n) => {
                let guard = store.lock();
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
                let guard = store.lock();
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
        }
    });
    reply(202, &serde_json::json!({"status": "actioning_feedback"}))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::guardian::GuardianStatus;
    use crate::guardian::MergeStatus;
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
    ) -> (
        PathBuf,
        PathBuf,
        crate::store_lock::StoreHandle,
        String,
        String,
    ) {
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
            Arc::new(crate::store_lock::StoreMutex::new(s)),
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
            readable_review_branch: true,
            review_branch_name: None,
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
            finished_at_ms: None,
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

    fn test_pr(branch_alias: &str, base_ref: &str) -> PullRequestView {
        PullRequestView {
            id: "pr-test".to_string(),
            guardian_id: "guardian-test".to_string(),
            branch_id: None,
            forge: "github".to_string(),
            repo: "o/r".to_string(),
            branch_alias: branch_alias.to_string(),
            base_ref: base_ref.to_string(),
            title: String::new(),
            description: String::new(),
            pr_number: Some(1),
            pr_url: None,
            state: "open".to_string(),
            created_at_ms: 0,
            updated_at_ms: 0,
            last_pushed_sha: None,
            last_pushed_base_ref: None,
            stack_id: None,
            dropped_reason: None,
            superseded_by: None,
            ci_status: None,
            ci_failure_job_url: None,
            auto_fix_attempted_at_ms: None,
            draft: None,
        }
    }

    #[test]
    fn ordered_prs_form_a_chain_accepts_a_valid_bottom_to_top_chain() {
        let a = test_pr("a-review", "main");
        let b = test_pr("b-review", "a-review");
        let c = test_pr("c-review", "b-review");
        assert!(ordered_prs_form_a_chain(&[&a, &b, &c]));
    }

    #[test]
    fn ordered_prs_form_a_chain_rejects_a_gap_left_by_a_disabled_branch() {
        // `c`'s base still points at a branch that no longer precedes it in
        // the stack -- exactly the mid-cascade state a just-disabled branch
        // leaves until resync repoints it.
        let a = test_pr("a-review", "main");
        let b = test_pr("b-review", "a-review");
        let c = test_pr("c-review", "disabled-review");
        assert!(!ordered_prs_form_a_chain(&[&a, &b, &c]));
    }

    #[test]
    fn ordered_prs_form_a_chain_accepts_fewer_than_two_prs() {
        let a = test_pr("a-review", "main");
        assert!(ordered_prs_form_a_chain(&[&a]));
        assert!(ordered_prs_form_a_chain(&[]));
    }

    #[test]
    fn decide_stack_action_creates_once_two_prs_exist() {
        let prs = vec![("b-a".to_string(), 0, 3), ("b-b".to_string(), 1, 6)];
        assert_eq!(
            decide_stack_action(&prs, None, true),
            StackAction::Create {
                all_ordered: vec![3, 6]
            }
        );
    }

    #[test]
    fn decide_stack_action_skips_a_single_pr() {
        let prs = vec![("b-a".to_string(), 0, 3)];
        assert_eq!(
            decide_stack_action(&prs, None, true),
            StackAction::Skip {
                reason: "fewer than 2 PRs in the stack"
            }
        );
    }

    #[test]
    fn decide_stack_action_appends_a_missing_terminal_pr_to_a_recorded_stack() {
        let prs = vec![
            ("b-a".to_string(), 0, 3),
            ("b-b".to_string(), 1, 6),
            ("b-c".to_string(), 2, 9),
        ];
        assert_eq!(
            decide_stack_action(&prs, Some((42, vec![3, 6])), true),
            StackAction::Append {
                stack_number: 42,
                missing_ordered: vec![9],
                all_ordered: vec![3, 6, 9]
            }
        );
    }

    #[test]
    fn decide_stack_action_rebuilds_a_recorded_stack_with_wrong_members() {
        let prs = vec![
            ("b-a".to_string(), 0, 3),
            ("b-b".to_string(), 1, 6),
            ("b-c".to_string(), 2, 9),
        ];
        assert_eq!(
            decide_stack_action(&prs, Some((42, vec![3, 9])), true),
            StackAction::Rebuild {
                stack_number: 42,
                all_ordered: vec![3, 6, 9]
            }
        );
    }

    #[test]
    fn decide_stack_action_skips_when_recorded_stack_already_matches() {
        let prs = vec![("b-a".to_string(), 0, 3), ("b-b".to_string(), 1, 6)];
        assert_eq!(
            decide_stack_action(&prs, Some((42, vec![3, 6])), true),
            StackAction::Skip {
                reason: "native stack already matches the review PR order"
            }
        );
    }

    #[test]
    fn decide_stack_action_repairs_an_unrecorded_stack_when_nothing_is_new() {
        let prs = vec![("b-a".to_string(), 0, 3), ("b-b".to_string(), 1, 6)];
        assert_eq!(
            decide_stack_action(&prs, None, true),
            StackAction::Create {
                all_ordered: vec![3, 6]
            }
        );
    }

    /// RAL-401 follow-up: a mid-stack branch disable (or any other cause)
    /// leaving downstream base refs mid-cascade must never be treated as
    /// "create a fresh stack" -- GitHub would reject it anyway, but checking
    /// locally first means ralphus never has to find that out by touching
    /// the forge at all.
    #[test]
    fn decide_stack_action_skips_create_when_chain_is_not_yet_valid() {
        let prs = vec![("b-a".to_string(), 0, 3), ("b-b".to_string(), 1, 6)];
        assert_eq!(
            decide_stack_action(&prs, None, false),
            StackAction::Skip {
                reason: "base-ref chain isn't fully resynced yet; postponing native stack registration until it is"
            }
        );
    }

    /// RAL-401 follow-up: this is the core fix. Previously a stale-looking
    /// recorded stack plus an inconsistent chain led straight to `Rebuild`,
    /// which dissolves the existing (still working) native stack before
    /// attempting to recreate it -- if the chain is genuinely still
    /// mid-cascade, that recreate fails and the review is left completely
    /// unstacked until a later pass happens to retry. Skipping instead keeps
    /// the old (stale but real) stack registration intact.
    #[test]
    fn decide_stack_action_skips_rebuild_when_chain_is_not_yet_valid() {
        let prs = vec![
            ("b-a".to_string(), 0, 3),
            ("b-b".to_string(), 1, 6),
            ("b-c".to_string(), 2, 9),
        ];
        assert_eq!(
            decide_stack_action(&prs, Some((42, vec![3, 9])), false),
            StackAction::Skip {
                reason: "base-ref chain isn't fully resynced yet; leaving the existing native stack alone until it is"
            }
        );
    }

    /// The bug this whole guard exists for: a restack resets every enabled
    /// branch to `pending` and promotes them back to `done` one at a time,
    /// and each promotion fires an auto-submit. If the reconciler judges
    /// membership from the branches that are terminal *right now*, the second
    /// promotion presents a two-PR view of an eleven-PR review -- which is
    /// neither a match nor an appendable prefix, so the old code answered
    /// `Rebuild` and dissolved a stack that was completely correct. Every
    /// intermediate view of a restack must be a no-op.
    /// End-to-end counterpart to the pure `decide_stack_action` guards: the
    /// reconciler must judge native-stack membership from the review's
    /// enabled branches, never from whichever of them happen to be terminal.
    /// Here the restack has re-promoted the bottom two branches and has not
    /// finished the top two -- the exact shape every rebase passes through --
    /// while all four PRs are open and correctly chained. The registered
    /// stack is therefore already right, so
    /// the only acceptable forge traffic is reads: the mock server fails the
    /// test if it is ever asked to unstack or to register a new stack.
    #[test]
    fn reconcile_native_pr_stack_ignores_a_mid_restack_branch() {
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let addr = server.server_addr().to_string();
        let handle = std::thread::spawn(move || {
            let mut writes: Vec<String> = Vec::new();
            loop {
                let req = server.recv().unwrap();
                let method = req.method().clone();
                let url = req.url().to_string();
                if url == "/done" {
                    req.respond(tiny_http::Response::empty(204)).unwrap();
                    break;
                } else if method == tiny_http::Method::Get
                    && url.starts_with("/repos/acme/widget/pulls/")
                {
                    req.respond(
                        tiny_http::Response::from_string(
                            r#"{"state":"open","base":{"ref":"x"},"updated_at":"2026-01-01T00:00:00Z"}"#,
                        )
                        .with_status_code(200),
                    )
                    .unwrap();
                } else if method == tiny_http::Method::Get && url == "/repos/acme/widget/stacks/42"
                {
                    req.respond(
                        tiny_http::Response::from_string(
                            r#"{"number":42,"pull_requests":[{"number":10},{"number":11},{"number":12},{"number":13}]}"#,
                        )
                        .with_status_code(200),
                    )
                    .unwrap();
                } else {
                    writes.push(format!("{method} {url}"));
                    req.respond(
                        tiny_http::Response::from_string(r#"{"number":99}"#).with_status_code(200),
                    )
                    .unwrap();
                }
            }
            writes
        });

        let root_dir = tmp_dir("mid-restack-stack-work");
        g(&root_dir, &["init", "--initial-branch", "release"]);
        gwrite(&root_dir, "base.txt", "base\n");
        g(&root_dir, &["add", "."]);
        g(&root_dir, &["commit", "--message", "base"]);

        let s = store();
        let gid = s
            .create_guardian("demo", "release", root_dir.to_str().unwrap())
            .unwrap();
        for branch in ["a", "b", "c", "d"] {
            s.add_guardian_branch(&gid, branch).unwrap();
        }
        s.set_guardian_forge_stack_number(&gid, 42).unwrap();
        let branch_ids: Vec<String> = s
            .get_guardian(&gid)
            .unwrap()
            .branches
            .into_iter()
            .map(|b| b.id)
            .collect();
        // Bottom-to-top, as the review is ordered: release <- 10 <- 11 <- 12 <- 13.
        let chain = [
            ("a-review", "release", 10_i64),
            ("b-review", "a-review", 11),
            ("c-review", "b-review", 12),
            ("d-review", "c-review", 13),
        ];
        for (branch_id, (alias, base, number)) in branch_ids.iter().zip(chain) {
            s.set_branch_review(&gid, branch_id, alias, "wt").unwrap();
            s.create_pull_request(
                &gid,
                Some(branch_id),
                "github",
                "acme/widget",
                alias,
                base,
                "Title",
                "Description",
                Some(number),
                Some("http://x"),
            )
            .unwrap();
        }
        // The restack has re-promoted the bottom two branches, is working on
        // the third, and has not reached the fourth. Two terminal branches is
        // the smallest view that used to reach `Rebuild`: a one-branch view is
        // below the two-PR floor and gets skipped for that reason instead.
        for (branch_id, status) in branch_ids.iter().zip([
            MergeStatus::Done,
            MergeStatus::Done,
            MergeStatus::InProgress,
            MergeStatus::Pending,
        ]) {
            s.set_branch_status(&gid, branch_id, status, None).unwrap();
        }

        let store = Arc::new(crate::store_lock::StoreMutex::new(s));
        let client = crate::forge::ForgeClient::new(
            crate::forge::ForgeKind::GitHub,
            format!("http://{addr}"),
            "acme/widget".to_string(),
            Some("tok".to_string()),
        );

        reconcile_native_pr_stack(&store, &client, &gid, None, 0).unwrap();

        assert_eq!(
            store.lock().get_guardian_forge_stack_number(&gid).unwrap(),
            Some(42),
            "a mid-restack reconcile must leave the registered stack in place"
        );
        // Retire the server thread: it serves reads until this sentinel and
        // reports whatever writes it was asked for along the way.
        let _ = ureq::get(&format!("http://{addr}/done"))
            .timeout(std::time::Duration::from_secs(5))
            .call();
        assert!(
            handle.join().unwrap().is_empty(),
            "reconciling a correct stack must not write to the forge"
        );

        let _ = std::fs::remove_dir_all(&root_dir);
    }

    #[test]
    fn decide_stack_action_never_dissolves_a_stack_for_a_partial_restack_view() {
        let full = vec![3, 6, 9, 12];
        for partial in [vec![3, 6], vec![3, 6, 9]] {
            let prs: Vec<(String, i64, i64)> = partial
                .iter()
                .enumerate()
                .map(|(i, n)| (format!("b-{i}"), i as i64, *n))
                .collect();
            assert_eq!(
                decide_stack_action(&prs, Some((42, full.clone())), true),
                StackAction::Skip {
                    reason: "the registered native stack already contains every expected PR in this order",
                },
                "a {}-PR view of a {}-PR stack must not dissolve it",
                partial.len(),
                full.len()
            );
        }
    }

    /// The same partial view, but with the hole in the middle -- a branch
    /// that left the terminal set to action reviewer feedback while the ones
    /// above and below it stayed done.
    #[test]
    fn decide_stack_action_never_dissolves_a_stack_for_a_gapped_view() {
        let prs = vec![
            ("b-a".to_string(), 0, 3),
            ("b-b".to_string(), 1, 6),
            ("b-d".to_string(), 3, 12),
        ];
        assert_eq!(
            decide_stack_action(&prs, Some((42, vec![3, 6, 9, 12])), true),
            StackAction::Skip {
                reason: "the registered native stack already contains every expected PR in this order",
            }
        );
    }

    /// The guard above must not swallow the changes a rebuild is actually
    /// for: a reorder, and a PR appearing mid-stack.
    #[test]
    fn decide_stack_action_still_rebuilds_a_reordered_or_grown_stack() {
        let reordered = vec![
            ("b-a".to_string(), 0, 3),
            ("b-c".to_string(), 1, 9),
            ("b-b".to_string(), 2, 6),
        ];
        assert_eq!(
            decide_stack_action(&reordered, Some((42, vec![3, 6, 9])), true),
            StackAction::Rebuild {
                stack_number: 42,
                all_ordered: vec![3, 9, 6]
            }
        );

        let grown = vec![
            ("b-a".to_string(), 0, 3),
            ("b-b".to_string(), 1, 6),
            ("b-c".to_string(), 2, 9),
        ];
        assert_eq!(
            decide_stack_action(&grown, Some((42, vec![3, 9])), true),
            StackAction::Rebuild {
                stack_number: 42,
                all_ordered: vec![3, 6, 9]
            }
        );
    }

    #[test]
    fn is_ordered_subset_requires_the_same_relative_order() {
        assert!(is_ordered_subset(&[3, 9], &[3, 6, 9]));
        assert!(is_ordered_subset(&[], &[3, 6]));
        assert!(is_ordered_subset(&[3, 6], &[3, 6]));
        assert!(!is_ordered_subset(&[9, 3], &[3, 6, 9]));
        assert!(!is_ordered_subset(&[3, 7], &[3, 6, 9]));
        assert!(!is_ordered_subset(&[3, 6, 9], &[3, 9]));
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
    fn sync_status_fixture() -> (PathBuf, PathBuf, crate::store_lock::StoreHandle, String) {
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
    fn synced_fixture() -> (PathBuf, PathBuf, crate::store_lock::StoreHandle, String) {
        let (root, remote_dir, store, pr_id) = sync_status_fixture();
        let sha = git2::Repository::open(&root)
            .unwrap()
            .revparse_single("review-branch")
            .unwrap()
            .id()
            .to_string();
        {
            let guard = store.lock();
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
            // only reconciles a branch that has too.
            let pr = guard.get_pull_request(&pr_id).unwrap();
            guard
                .set_guardian_status(&pr.guardian_id, GuardianStatus::InReview, None)
                .unwrap();
            guard
                .set_branch_status(
                    &pr.guardian_id,
                    pr.branch_id.as_deref().unwrap(),
                    MergeStatus::Done,
                    None,
                )
                .unwrap();
        }
        (root, remote_dir, store, pr_id)
    }

    /// Like [`synced_fixture`], but `review-branch`'s one commit edits the
    /// same line of a shared file that a base-advance commit can also touch
    /// -- so a caller can force a real rebase conflict on demand, on either
    /// the worktree or the PR side, via [`rebase_with_conflict`].
    fn conflict_fixture() -> (PathBuf, PathBuf, crate::store_lock::StoreHandle, String) {
        let (root, remote_dir, store, pr_id, sha) = review_fixture(
            "sync-conflict",
            |root| gwrite(root, "shared.txt", "line1\nline2\nline3\n"),
            |root| gwrite(root, "shared.txt", "line1\nline2-review\nline3\n"),
        );
        store
            .lock()
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

    /// RAL-422: the sync-status poll is part of the merge pipeline's silent
    /// maintenance phase, so it must leave paired start/completion records
    /// (with elapsed time) in the Cartographer log -- asserted here through
    /// the same query the board's timeline renders.
    #[test]
    fn compute_sync_status_emits_start_and_completion_records() {
        let (root, remote_dir, store, pr_id) = sync_status_fixture();
        let status = compute_sync_status(&store, &pr_id).unwrap();
        assert!(status.in_sync, "{status:?}");
        let page = store
            .lock()
            .cartographer_query(&crate::cartographer::CartographerFilter {
                q: Some("PR sync status check".to_string()),
                ..crate::cartographer::CartographerFilter::recent(10)
            })
            .unwrap();
        let messages = page
            .rows
            .iter()
            .map(|r| r.message.as_str())
            .collect::<Vec<_>>();
        assert!(
            messages.contains(&"PR sync status check starting"),
            "expected a start record: {messages:?}"
        );
        assert!(
            messages
                .iter()
                .any(|m| m.starts_with("PR sync status check completed")),
            "expected a completion record with the elapsed duration: {messages:?}"
        );
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
            for request_index in 0..3 {
                let mut req = server.recv().unwrap();
                let method = req.method().as_str().to_string();
                let url = req.url().to_string();
                let mut body = String::new();
                req.as_reader().read_to_string(&mut body).unwrap();
                if url == "/repos/acme/widget/stacks" {
                    created_payload = serde_json::from_str(&body).unwrap();
                }
                seen.push((method, url));
                if request_index == 0 {
                    req.respond(tiny_http::Response::empty(204)).unwrap();
                } else {
                    req.respond(
                        tiny_http::Response::from_string("{\"number\": 99}").with_status_code(200),
                    )
                    .unwrap();
                }
            }
            (seen, created_payload)
        });

        let s = store();
        let gid = s.create_guardian("demo", "main", "/tmp/root").unwrap();
        s.set_guardian_forge_stack_number(&gid, 42).unwrap();
        let store = Arc::new(crate::store_lock::StoreMutex::new(s));
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
            store.lock().get_guardian_forge_stack_number(&gid).unwrap(),
            Some(99),
            "the rebuilt stack's number must replace the dissolved one"
        );
    }

    #[test]
    fn repoint_stacked_prs_does_not_retain_a_dissolved_stack_number() {
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let addr = server.server_addr().to_string();
        let handle = std::thread::spawn(move || {
            for request_index in 0..3 {
                let req = server.recv().unwrap();
                match request_index {
                    0 => req.respond(tiny_http::Response::empty(204)).unwrap(),
                    1 => req.respond(tiny_http::Response::from_string("{}")).unwrap(),
                    _ => req
                        .respond(
                            tiny_http::Response::from_string("unavailable").with_status_code(500),
                        )
                        .unwrap(),
                }
            }
        });

        let s = store();
        let gid = s.create_guardian("demo", "main", "/tmp/root").unwrap();
        s.set_guardian_forge_stack_number(&gid, 42).unwrap();
        let store = Arc::new(crate::store_lock::StoreMutex::new(s));
        let client = crate::forge::ForgeClient::new(
            crate::forge::ForgeKind::GitHub,
            format!("http://{addr}"),
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
            store.lock().get_guardian_forge_stack_number(&gid).unwrap(),
            None,
            "a dissolved stack must not remain recorded when rebuilding it fails"
        );
        handle.join().unwrap();
    }

    #[test]
    fn repoint_stacked_prs_does_nothing_without_a_recorded_stack() {
        let s = store();
        let gid = s.create_guardian("demo", "main", "/tmp/root").unwrap();
        let store = Arc::new(crate::store_lock::StoreMutex::new(s));
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
            store.lock().get_guardian_forge_stack_number(&gid).unwrap(),
            None
        );
    }

    // -- RAL-285: keep already-open PR branches level with their review branch --

    #[test]
    fn sync_remote_pr_commits_pulls_a_reviewer_push_before_a_rebase() {
        let (root, remote_dir, store, pr_id) = synced_fixture();
        let (guardian_id, branch_id) = {
            let guard = store.lock();
            let pr = guard.get_pull_request(&pr_id).unwrap();
            (pr.guardian_id, pr.branch_id.unwrap())
        };
        // The lightweight sync fixture normally records a deliberately
        // nonexistent worktree because its other tests only inspect refs.
        // This path drives a real rebase, so point it at the actual fixture
        // checkout and make that checkout the review branch.
        store
            .lock()
            .set_branch_review(
                &guardian_id,
                &branch_id,
                "review-branch",
                root.to_str().unwrap(),
            )
            .unwrap();
        g(&root, &["checkout", "review-branch"]);
        let clone_dir = tmp_dir("remote-pr-sync-clone");
        let _ = std::fs::remove_dir_all(&clone_dir);
        g(
            clone_dir.parent().unwrap(),
            &[
                "clone",
                remote_dir.to_str().unwrap(),
                clone_dir.file_name().unwrap().to_str().unwrap(),
            ],
        );
        g(&clone_dir, &["checkout", "pr-y"]);
        gwrite(&clone_dir, "reviewer.txt", "preserve this remote commit\n");
        g(&clone_dir, &["add", "."]);
        g(&clone_dir, &["commit", "--message", "reviewer fix"]);
        g(&clone_dir, &["push", "origin", "pr-y"]);

        assert_eq!(
            sync_remote_pr_commits(&store, &NoopRunner, &guardian_id).unwrap(),
            1
        );
        let status = compute_sync_status(&store, &pr_id).unwrap();
        assert!(status.in_sync, "{status:?}");
        assert_eq!(
            g(&remote_dir, &["rev-parse", "pr-y"]).trim(),
            g(&root, &["rev-parse", "review-branch"]).trim()
        );

        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&remote_dir);
        let _ = std::fs::remove_dir_all(&clone_dir);
    }

    #[test]
    fn sync_open_pr_branches_pushes_a_worktree_tip_that_moved_locally() {
        let (root, remote_dir, store, pr_id) = synced_fixture();
        let gid = store.lock().get_pull_request(&pr_id).unwrap().guardian_id;

        // The review-branch worktree tip gains a commit in place (e.g. a
        // conflict resolution), with nothing pushed to the remote yet.
        g(&root, &["checkout", "review-branch"]);
        gwrite(&root, "resolved.txt", "resolved\n");
        g(&root, &["add", "."]);
        g(&root, &["commit", "--message", "conflict resolution"]);
        g(&root, &["checkout", "main"]);
        let new_local_sha = g(&root, &["rev-parse", "review-branch"]).trim().to_string();

        sync_open_pr_branches(&store, &gid);

        let pr = store.lock().get_pull_request(&pr_id).unwrap();
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
        let gid = store.lock().get_pull_request(&pr_id).unwrap().guardian_id;
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

        let pr = store.lock().get_pull_request(&pr_id).unwrap();
        assert_eq!(pr.last_pushed_sha.as_deref(), Some(restacked.as_str()));
        let remote_sha = g(&remote_dir, &["rev-parse", "pr-y"]).trim().to_string();
        assert_eq!(remote_sha, restacked);

        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&remote_dir);
    }

    #[test]
    fn sync_open_pr_branches_leaves_a_branch_still_being_rebased_alone() {
        let (root, remote_dir, store, pr_id) = synced_fixture();
        let (gid, bid) = {
            let pr = store.lock().get_pull_request(&pr_id).unwrap();
            (pr.guardian_id, pr.branch_id.unwrap())
        };
        // This branch's own pass hasn't finished -- its worktree tip can
        // still be rewritten again before this merge attempt settles, so it
        // is not yet the state the PR should show, independent of what the
        // rest of the review is doing.
        store
            .lock()
            .set_branch_status(&gid, &bid, MergeStatus::InProgress, None)
            .unwrap();
        let remote_before = g(&remote_dir, &["rev-parse", "pr-y"]).trim().to_string();

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

    /// The fix this review's dogfooding surfaced: a stacked branch that
    /// finishes early must not have its PR held hostage by slower siblings
    /// still working further down the same stack.
    #[test]
    fn sync_open_pr_branches_pushes_a_finished_branch_while_the_review_is_still_merging() {
        let (root, remote_dir, store, pr_id) = synced_fixture();
        let gid = store.lock().get_pull_request(&pr_id).unwrap().guardian_id;
        // The overall review hasn't settled yet (some other branch further
        // down the stack is still being worked), but THIS branch reached
        // `conflict_resolved` -- its own pass is done, so its PR should not
        // wait on the rest of the stack.
        store
            .lock()
            .set_guardian_status(&gid, GuardianStatus::Merging, None)
            .unwrap();

        g(&root, &["checkout", "review-branch"]);
        gwrite(&root, "resolved.txt", "resolved\n");
        g(&root, &["add", "."]);
        g(&root, &["commit", "--message", "conflict resolution"]);
        g(&root, &["checkout", "main"]);
        let new_local_sha = g(&root, &["rev-parse", "review-branch"]).trim().to_string();

        sync_open_pr_branches(&store, &gid);

        let pr = store.lock().get_pull_request(&pr_id).unwrap();
        assert_eq!(pr.last_pushed_sha.as_deref(), Some(new_local_sha.as_str()));
        let remote_sha = g(&remote_dir, &["rev-parse", "pr-y"]).trim().to_string();
        assert_eq!(remote_sha, new_local_sha);

        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&remote_dir);
    }

    #[test]
    fn sync_open_pr_branches_is_a_noop_when_nothing_changed() {
        let (root, remote_dir, store, pr_id) = synced_fixture();
        let gid = store.lock().get_pull_request(&pr_id).unwrap().guardian_id;
        let remote_sha_before = g(&remote_dir, &["rev-parse", "pr-y"]).trim().to_string();

        sync_open_pr_branches(&store, &gid);

        let pr = store.lock().get_pull_request(&pr_id).unwrap();
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
        let gid = store.lock().get_pull_request(&pr_id).unwrap().guardian_id;
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
        let pr = store.lock().get_pull_request(&pr_id).unwrap();
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
        let store = Arc::new(crate::store_lock::StoreMutex::new(store()));
        let gid = store
            .lock()
            .create_guardian("demo", "main", "/repo")
            .unwrap();
        for b in ["a", "b", "c"] {
            store.lock().add_guardian_branch(&gid, b).unwrap();
        }
        let ids: Vec<String> = store
            .lock()
            .get_guardian(&gid)
            .unwrap()
            .branches
            .iter()
            .map(|b| b.id.clone())
            .collect();

        // Original stack order a -> b -> c: each PR's base is the one before it.
        let pr_a = store
            .lock()
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
            .reorder_guardian_branches(&gid, &["c".into(), "a".into(), "b".into()])
            .unwrap();

        let changed = resync_pr_bases(&store, &gid).unwrap();
        // c now leads the stack (base -> guardian's own base branch); a now
        // follows c; b is still right after a, so its base is unchanged.
        assert_eq!(changed, 2);
        let s = store.lock();
        assert_eq!(s.get_pull_request(&pr_c).unwrap().base_ref, "main");
        assert_eq!(s.get_pull_request(&pr_a).unwrap().base_ref, "c");
        assert_eq!(s.get_pull_request(&pr_b).unwrap().base_ref, "a");
    }

    fn synchronous_multi_branch_base_sync(forge: &str, already_synced: bool) {
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let addr = server.server_addr().to_string();
        let forge_name = forge.to_string();
        let handle = std::thread::spawn(move || {
            let mut bases = Vec::new();
            for (number, forge_base) in (1..=3).zip(if already_synced {
                ["release", "a-alias", "b-alias"]
            } else {
                ["main", "a-alias", "b-alias"]
            }) {
                let req = server.recv().unwrap();
                assert_eq!(req.method(), &tiny_http::Method::Get);
                let expected_get_path = if forge_name == "github" {
                    format!("/repos/acme/widget/pulls/{number}")
                } else {
                    format!("/projects/acme%2Fwidget/merge_requests/{number}")
                };
                assert_eq!(req.url(), expected_get_path);
                let state = if forge_name == "github" {
                    serde_json::json!({
                        "base": {"ref": forge_base},
                        "updated_at": "2026-01-01T00:00:00Z",
                    })
                } else {
                    serde_json::json!({
                        "target_branch": forge_base,
                        "updated_at": "2026-01-01T00:00:00Z",
                    })
                };
                req.respond(
                    tiny_http::Response::from_string(state.to_string()).with_status_code(200),
                )
                .unwrap();

                if already_synced || number != 1 {
                    continue;
                }

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
        let store = Arc::new(crate::store_lock::StoreMutex::new(store()));
        let gid = store
            .lock()
            .create_guardian("demo", "release", root.to_str().unwrap())
            .unwrap();
        for branch in ["a", "b", "c"] {
            store.lock().add_guardian_branch(&gid, branch).unwrap();
        }
        let ids: Vec<_> = store
            .lock()
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
                .create_pull_request(
                    &gid,
                    Some(&ids[idx]),
                    forge,
                    "acme/widget",
                    alias,
                    if already_synced {
                        ["release", "a-alias", "b-alias"][idx]
                    } else {
                        old_base
                    },
                    alias,
                    "",
                    Some(i64::try_from(idx).unwrap() + 1),
                    None,
                )
                .unwrap();
        }

        assert_eq!(
            resync_pr_bases_synchronously(&store, &gid).unwrap(),
            if already_synced { 0 } else { 1 }
        );
        assert_eq!(
            handle.join().unwrap(),
            if already_synced {
                Vec::new()
            } else {
                vec!["release"]
            }
        );
        let rows = store.lock().list_pull_requests_for_guardian(&gid).unwrap();
        assert_eq!(
            rows.iter()
                .map(|pr| pr.base_ref.as_str())
                .collect::<Vec<_>>(),
            vec!["release", "a-alias", "b-alias"]
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn synchronous_multi_branch_base_sync_retargets_only_drifted_github_prs() {
        synchronous_multi_branch_base_sync("github", false);
    }

    #[test]
    fn synchronous_multi_branch_base_sync_retargets_only_drifted_gitlab_mrs() {
        synchronous_multi_branch_base_sync("gitlab", false);
    }

    #[test]
    fn synchronous_multi_branch_base_sync_does_not_retarget_an_already_correct_github_stack() {
        synchronous_multi_branch_base_sync("github", true);
    }

    #[test]
    fn synchronous_multi_branch_base_sync_does_not_retarget_an_already_correct_gitlab_stack() {
        synchronous_multi_branch_base_sync("gitlab", true);
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
        let store = Arc::new(crate::store_lock::StoreMutex::new(store()));
        let gid = store
            .lock()
            .create_guardian("demo", "main", "/repo")
            .unwrap();
        // Freshly created guardians start out `collecting`, not `in_review`.
        assert!(!claim_guardian_for_forge_reorder(&store, &gid));
        assert_eq!(
            store.lock().get_guardian(&gid).unwrap().status,
            "collecting"
        );

        store
            .lock()
            .set_guardian_status(&gid, crate::guardian::GuardianStatus::InReview, None)
            .unwrap();
        assert!(claim_guardian_for_forge_reorder(&store, &gid));
        assert_eq!(store.lock().get_guardian(&gid).unwrap().status, "merging");
        // Already claimed -- a second attempt loses the race.
        assert!(!claim_guardian_for_forge_reorder(&store, &gid));
    }

    #[test]
    fn detect_forge_reorder_is_a_noop_with_fewer_than_two_stacked_prs() {
        let store = Arc::new(crate::store_lock::StoreMutex::new(store()));
        let gid = store
            .lock()
            .create_guardian("demo", "main", "/repo")
            .unwrap();
        store.lock().add_guardian_branch(&gid, "a").unwrap();
        // No open PRs at all yet -- nothing to compare against the forge.
        assert_eq!(detect_forge_reorder(&store, &gid).unwrap(), None);
    }

    /// Regression test for a real-world infinite loop: when a project's
    /// registered fork happens to use the same remote name that `base_branch`
    /// itself is qualified with (e.g. `base_branch = "alt/main"` and the
    /// fork's `remote_name` is also `"alt"`), [`resolve_pr_repo_routing`]
    /// excludes that remote from step 1 of
    /// [`crate::forge::resolve_remote_name_excluding`] while computing the
    /// *parent's* remote, so it falls through to the `origin`/`[forge].remote`
    /// default instead of `"alt"` -- by design
    /// (`resolve_remote_name_excluding_skips_a_registered_fork_remote` in
    /// `forge.rs` pins exactly this fallback). A first [`detect_forge_reorder`]
    /// call is therefore *correctly* going to report `base_changed` here: the
    /// fork-excluded parent remote ("origin") can't strip `"alt/"` off
    /// `base_branch`, so it reads as `"alt/main"` against the forge's bare
    /// `"main"`.
    ///
    /// The bug was that applying that drift never converged: the apply block
    /// in [`check_and_apply_forge_reorder`] re-qualified the new base with a
    /// *plain, non-excluding* [`crate::forge::resolve_remote_name`] call,
    /// which resolves `"alt"` from `base_branch` just fine (no exclusion) and
    /// re-wrote `base_branch` right back to `"alt/main"` -- the exact value
    /// the next detection pass would flag as drifted again. In production
    /// this drove a guardian to cycle `in_review` -> `merging` -> `in_review`
    /// every ~5 minutes, indefinitely, each cycle triggering a full stack
    /// rebuild for nothing. The fix makes the apply block resolve the remote
    /// the same fork-excluded way the detector did, so it writes back a value
    /// the *next* detection pass agrees is already correct.
    #[test]
    fn detect_forge_reorder_converges_after_one_apply_when_the_registered_fork_remote_matches_base_branchs_own_remote()
     {
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let addr = server.server_addr().to_string();
        let handle = std::thread::spawn(move || {
            // Two full rounds: one for the initial (correctly reported)
            // detection, one for the re-check after applying the fix, which
            // must come back clean. `detect_forge_reorder` fetches each
            // branch's PR base state out of a HashMap, so within a round the
            // two GET requests can arrive in either order -- key the response
            // on the PR number in the URL rather than assuming request order.
            for _round in 0..2 {
                for _ in 0..2 {
                    let req = server.recv().unwrap();
                    let forge_base = if req.url() == "/repos/acme/widget/pulls/1" {
                        "main"
                    } else if req.url() == "/repos/acme/widget/pulls/2" {
                        "a-alias"
                    } else {
                        panic!("unexpected request: {}", req.url());
                    };
                    let state = serde_json::json!({
                        "base": {"ref": forge_base},
                        // Far enough in the future to always beat a freshly
                        // created guardian's own `base_changed_at_ms` (set to
                        // wall-clock `now_ms()` at creation), so the
                        // newer-than check in `set_guardian_base_branch_if_newer`
                        // accepts the apply regardless of when this test runs.
                        "updated_at": "2099-01-01T00:00:00Z",
                    });
                    req.respond(
                        tiny_http::Response::from_string(state.to_string()).with_status_code(200),
                    )
                    .unwrap();
                }
            }
        });

        let root_dir = tmp_dir("fork-remote-collision");
        g(&root_dir, &["init"]);
        // The only remote this repo has is named "alt" -- both `base_branch`
        // (in `<remote>/<branch>` form) and the registered fork use it,
        // exactly like a repo whose only push-able remote for the project's
        // real GitHub repo happens to also be the one a fork row names.
        g(
            &root_dir,
            &["remote", "add", "alt", "https://github.com/acme/widget.git"],
        );
        std::fs::write(
            root_dir.join(".ralphus.toml"),
            format!(
                "[forge]\nkind = \"github\"\napi_base = \"http://{addr}\"\ntoken_env = \"RALPHUS_TEST_FORGE_TOKEN\"\n"
            ),
        )
        .unwrap();

        let store = Arc::new(crate::store_lock::StoreMutex::new(store()));
        store
            .lock()
            .register_project("demo", "orchestrator", root_dir.to_str().unwrap(), "git")
            .unwrap();
        store
            .lock()
            .upsert_project_fork(
                "demo",
                "",
                "https://github.com/acme/widget.git",
                "alt",
                "acme",
            )
            .unwrap();

        let gid = store
            .lock()
            .create_guardian("demo", "alt/main", root_dir.to_str().unwrap())
            .unwrap();
        for branch in ["a", "b"] {
            store.lock().add_guardian_branch(&gid, branch).unwrap();
        }
        let ids: Vec<_> = store
            .lock()
            .get_guardian(&gid)
            .unwrap()
            .branches
            .into_iter()
            .map(|b| b.id)
            .collect();
        store
            .lock()
            .create_pull_request(
                &gid,
                Some(&ids[0]),
                "github",
                "acme/widget",
                "a-alias",
                "main",
                "Add a",
                "",
                Some(1),
                None,
            )
            .unwrap();
        store
            .lock()
            .create_pull_request(
                &gid,
                Some(&ids[1]),
                "github",
                "acme/widget",
                "b-alias",
                "a-alias",
                "Add b",
                "",
                Some(2),
                None,
            )
            .unwrap();

        // Round 1: the fork-excluded parent remote can't strip "alt/" off
        // base_branch, so this correctly reports a base mismatch.
        let drift = detect_forge_reorder(&store, &gid)
            .unwrap()
            .expect("base_branch's own remote is fork-excluded, so a mismatch is expected here");
        assert!(drift.base_changed, "expected the base to look drifted");

        // Apply it through the exact same helper `check_and_apply_forge_reorder`'s
        // apply block calls, so this test breaks if that call site ever
        // drifts back to a plain, non-excluding remote resolution.
        let forge_cfg = crate::config::resolve_forge(&root_dir);
        let remote_name = forge_parent_remote_name(&store, &root_dir, "alt/main", &forge_cfg);
        let local_base = qualify_forge_base("alt/main", &remote_name, &drift.base);
        assert!(
            store
                .lock()
                .set_guardian_base_branch_if_newer(&gid, &local_base, drift.base_changed_at_ms)
                .unwrap(),
            "the newer-than check must accept this apply"
        );

        // Round 2: re-checking against the value just written must come back
        // clean -- this is the property that was broken before the fix.
        assert_eq!(
            detect_forge_reorder(&store, &gid).unwrap(),
            None,
            "applying the reported drift once must make the next detection pass agree \
             nothing is left to fix, instead of looping forever"
        );
        handle.join().unwrap();
        let _ = std::fs::remove_dir_all(root_dir);
    }

    #[test]
    fn poll_pr_base_drift_is_a_noop_for_a_review_with_no_submitted_prs() {
        let store = Arc::new(crate::store_lock::StoreMutex::new(store()));
        let gid = store
            .lock()
            .create_guardian("demo", "main", "/repo")
            .unwrap();
        store.lock().add_guardian_branch(&gid, "a").unwrap();
        assert_eq!(poll_pr_base_drift(&store, &gid).unwrap(), 0);
    }

    #[test]
    fn poll_pr_base_drift_is_a_noop_when_the_forge_remote_cannot_be_resolved() {
        // "/repo" is not a real git repository, so `forge::resolve_remote`
        // fails to read a remote from it -- same fixture other resync_pr_bases
        // tests above use to exercise the local-bookkeeping-only path without
        // any network access.
        let store = Arc::new(crate::store_lock::StoreMutex::new(store()));
        let gid = store
            .lock()
            .create_guardian("demo", "main", "/repo")
            .unwrap();
        store.lock().add_guardian_branch(&gid, "a").unwrap();
        let branch_id = store.lock().get_guardian(&gid).unwrap().branches[0]
            .id
            .clone();
        store
            .lock()
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

        let store = Arc::new(crate::store_lock::StoreMutex::new(store()));
        let gid = store
            .lock()
            .create_guardian("demo", "main", "/repo")
            .unwrap();
        let pr_id = store
            .lock()
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
        let prs = store.lock().list_pull_requests_for_guardian(&gid).unwrap();
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

        let updated = store.lock().get_pull_request(&pr_id).unwrap();
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

        let store = Arc::new(crate::store_lock::StoreMutex::new(store()));
        let gid = store
            .lock()
            .create_guardian("demo", "main", "/repo")
            .unwrap();
        store
            .lock()
            .set_guardian_status(&gid, GuardianStatus::InReview, None)
            .unwrap();
        let pr_id = store
            .lock()
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
        let prs = store.lock().list_pull_requests_for_guardian(&gid).unwrap();

        let changed = apply_pr_merge_check(&store, &gid, &prs, &client);
        assert!(changed, "every linked pr merging must report a change");

        let updated_guardian = store.lock().get_guardian(&gid).unwrap();
        assert_eq!(updated_guardian.status.as_str(), "approved");
        let updated_pr = store.lock().get_pull_request(&pr_id).unwrap();
        assert_eq!(updated_pr.state, "merged");

        handle.join().unwrap();
    }

    #[test]
    fn check_pr_merges_approves_when_all_merges_were_already_recorded() {
        let store = Arc::new(crate::store_lock::StoreMutex::new(store()));
        let gid = store
            .lock()
            .create_guardian("demo", "main", "/repo")
            .unwrap();
        store
            .lock()
            .set_guardian_status(&gid, GuardianStatus::InReview, None)
            .unwrap();
        let pr_id = store
            .lock()
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
            .update_pull_request(&pr_id, None, None, None, Some("merged"))
            .unwrap();

        let client = crate::forge::ForgeClient::new(
            crate::forge::ForgeKind::GitHub,
            "http://127.0.0.1:1".to_string(),
            "acme/widget".to_string(),
            None,
        );
        let prs = store.lock().list_pull_requests_for_guardian(&gid).unwrap();

        assert!(apply_pr_merge_check(&store, &gid, &prs, &client));
        assert_eq!(store.lock().get_guardian(&gid).unwrap().status, "approved");
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

        let store = Arc::new(crate::store_lock::StoreMutex::new(store()));
        let gid = store
            .lock()
            .create_guardian("demo", "main", "/repo")
            .unwrap();
        // A rebase/feedback pass owns this review's worktrees right now.
        store
            .lock()
            .set_guardian_status(&gid, GuardianStatus::Merging, None)
            .unwrap();
        let pr_id = store
            .lock()
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
        let prs = store.lock().list_pull_requests_for_guardian(&gid).unwrap();

        let changed = apply_pr_merge_check(&store, &gid, &prs, &client);
        assert!(changed, "an out-of-band merge must still report a change");

        let updated_guardian = store.lock().get_guardian(&gid).unwrap();
        assert_eq!(
            updated_guardian.status.as_str(),
            "merging",
            "a mid-flight review must not be silently force-approved"
        );
        assert_eq!(
            updated_guardian.notice_kind.as_deref(),
            Some("pr_merged_mid_flight")
        );
        let dropped_pr = store.lock().get_pull_request(&pr_id).unwrap();
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
        let store = Arc::new(crate::store_lock::StoreMutex::new(store()));
        let gid = store
            .lock()
            .create_guardian("demo", "main", "/repo")
            .unwrap();
        store
            .lock()
            .set_guardian_status(&gid, GuardianStatus::InReview, None)
            .unwrap();
        let pr_id = store
            .lock()
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
        let prs = store.lock().list_pull_requests_for_guardian(&gid).unwrap();

        let changed = apply_pr_merge_check(&store, &gid, &prs, &client);
        assert!(
            !changed,
            "an unreachable forge must never be mistaken for a merged pr"
        );

        let updated_guardian = store.lock().get_guardian(&gid).unwrap();
        assert_eq!(updated_guardian.status.as_str(), "in_review");
        let updated_pr = store.lock().get_pull_request(&pr_id).unwrap();
        assert_eq!(updated_pr.state, "open");
    }

    /// Common fork-mode fixture for the RAL-338 promotion tests below: a
    /// two-remote repo (`origin` = parent `acme/widget`, `fork` =
    /// `alice/widget`), a registered project + project-wide fork row, and a
    /// two-branch guardian (`a` = root, `b` = its successor) whose PR rows
    /// already reflect the pre-promotion state (root open at the parent,
    /// successor open at the fork, based on the root's alias).
    fn fork_promotion_fixture(addr: &str) -> (crate::store_lock::StoreHandle, String, PathBuf) {
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

        let store = Arc::new(crate::store_lock::StoreMutex::new(store()));
        store
            .lock()
            .register_project("demo", "orchestrator", root_dir.to_str().unwrap(), "git")
            .unwrap();
        store
            .lock()
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
            .create_guardian("demo", "release", root_dir.to_str().unwrap())
            .unwrap();
        store
            .lock()
            .set_guardian_status(&gid, GuardianStatus::InReview, None)
            .unwrap();
        for branch in ["a", "b"] {
            store.lock().add_guardian_branch(&gid, branch).unwrap();
        }
        let ids: Vec<_> = store
            .lock()
            .get_guardian(&gid)
            .unwrap()
            .branches
            .into_iter()
            .map(|b| b.id)
            .collect();

        store
            .lock()
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
            // Answered in whichever order the concurrent fan-out delivers them.
            // 1+2. Root PR (at the parent, merged) and successor (at the fork, open).
            answer_merge_probes(
                &server,
                &[
                    (
                        "/repos/acme/widget/pulls/10",
                        r#"{"state":"closed","merged":true}"#,
                    ),
                    (
                        "/repos/alice/widget/pulls/20",
                        r#"{"state":"open","merged":false}"#,
                    ),
                ],
            );

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

        let rows = store.lock().list_pull_requests_for_guardian(&gid).unwrap();
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
        let updated_guardian = store.lock().get_guardian(&gid).unwrap();
        assert_eq!(updated_guardian.status.as_str(), "in_review");

        handle.join().unwrap();
        let _ = std::fs::remove_dir_all(root_dir);
    }

    #[test]
    fn promotion_is_a_noop_when_the_forge_already_retargeted_the_successor_at_the_parent() {
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let addr = server.server_addr().to_string();
        let handle = std::thread::spawn(move || {
            // Answered in whichever order the concurrent fan-out delivers them.
            // Reconcile-first reads both before touching anything; no further
            // requests should follow, since the successor is already filed at the parent.
            answer_merge_probes(
                &server,
                &[
                    (
                        "/repos/acme/widget/pulls/10",
                        r#"{"state":"closed","merged":true}"#,
                    ),
                    (
                        "/repos/acme/widget/pulls/20",
                        r#"{"state":"open","merged":false}"#,
                    ),
                ],
            );
        });

        let (store, gid, root_dir) = fork_promotion_fixture(&addr);
        // Simulate the successor already having been reconciled (by a human,
        // or a forge feature this ticket doesn't yet trust) directly against
        // the parent, still under its own original PR number.
        let rows = store.lock().list_pull_requests_for_guardian(&gid).unwrap();
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
            .conn
            .execute(
                "UPDATE guardian_pull_requests SET repo='acme/widget' WHERE id=?1",
                rusqlite::params![successor_id],
            )
            .unwrap();

        check_pr_merges(&store, &gid);

        let rows = store.lock().list_pull_requests_for_guardian(&gid).unwrap();
        assert_eq!(rows.len(), 2, "no new pr row should have been created");
        let successor = rows.iter().find(|p| p.pr_number == Some(20)).unwrap();
        assert_eq!(successor.state, "open");
        assert!(successor.superseded_by.is_none());

        handle.join().unwrap();
        let _ = std::fs::remove_dir_all(root_dir);
    }

    /// Answer `check_pr_merges`'s per-PR merge-state probes in whatever order
    /// they arrive, returning the URLs actually seen (sorted).
    ///
    /// Those probes are issued concurrently -- `check_pr_merges` fans them out
    /// with `thread::scope`, since one PR's merge state has no bearing on
    /// another's -- so the order they reach a mock server in is undefined. A
    /// server script that `recv()`s them in a fixed sequence and asserts each
    /// URL passes only while the machine is quiet enough for the threads to
    /// finish in spawn order; under load it flips and the test fails.
    ///
    /// Worse, it *hangs* rather than fails: the assertion panics on the server
    /// thread, so every later request finds nobody left to answer it and the
    /// client blocks forever. Hence the deliberate non-panicking 500 below --
    /// an unexpected request must still get a response, and the caller asserts
    /// on the returned list once every probe has been answered.
    fn answer_merge_probes(server: &tiny_http::Server, routes: &[(&str, &str)]) -> Vec<String> {
        let mut seen = Vec::new();
        for _ in 0..routes.len() {
            let req = server.recv().unwrap();
            let url = req.url().to_string();
            let response = match routes.iter().find(|(route, _)| *route == url) {
                Some((_, body)) => {
                    tiny_http::Response::from_string(body.to_string()).with_status_code(200)
                }
                None => tiny_http::Response::from_string("unexpected request".to_string())
                    .with_status_code(500),
            };
            req.respond(response).unwrap();
            seen.push(url);
        }
        seen.sort();
        seen
    }

    #[test]
    fn a_non_fork_review_is_untouched_by_promotion() {
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let addr = server.server_addr().to_string();
        let handle = std::thread::spawn(move || {
            answer_merge_probes(
                &server,
                &[
                    (
                        "/repos/acme/widget/pulls/10",
                        r#"{"state":"closed","merged":true}"#,
                    ),
                    (
                        "/repos/acme/widget/pulls/20",
                        r#"{"state":"open","merged":false}"#,
                    ),
                ],
            )
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
        let store = Arc::new(crate::store_lock::StoreMutex::new(store()));
        let gid = store
            .lock()
            .create_guardian("demo", "release", root_dir.to_str().unwrap())
            .unwrap();
        store
            .lock()
            .set_guardian_status(&gid, GuardianStatus::InReview, None)
            .unwrap();
        for branch in ["a", "b"] {
            store.lock().add_guardian_branch(&gid, branch).unwrap();
        }
        let ids: Vec<_> = store
            .lock()
            .get_guardian(&gid)
            .unwrap()
            .branches
            .into_iter()
            .map(|b| b.id)
            .collect();
        store
            .lock()
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

        let rows = store.lock().list_pull_requests_for_guardian(&gid).unwrap();
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

        assert_eq!(
            handle.join().unwrap(),
            vec![
                "/repos/acme/widget/pulls/10".to_string(),
                "/repos/acme/widget/pulls/20".to_string(),
            ],
            "both PRs must be probed, in whichever order the concurrent fan-out lands"
        );
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
            // Answered in whichever order the concurrent fan-out delivers them.
            answer_merge_probes(
                &server,
                &[
                    (
                        "/repos/acme/widget/pulls/10",
                        r#"{"state":"closed","merged":true}"#,
                    ),
                    (
                        "/repos/alice/widget/pulls/20",
                        r#"{"state":"closed","merged":true}"#,
                    ),
                    (
                        "/repos/alice/widget/pulls/30",
                        r#"{"state":"open","merged":false}"#,
                    ),
                ],
            );

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
        store.lock().add_guardian_branch(&gid, "c").unwrap();
        let ids: Vec<_> = store
            .lock()
            .get_guardian(&gid)
            .unwrap()
            .branches
            .into_iter()
            .map(|b| b.id)
            .collect();
        store
            .lock()
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

        let rows = store.lock().list_pull_requests_for_guardian(&gid).unwrap();
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
            // Answered in whichever order the concurrent fan-out delivers them.
            // 1+2. Root MR and successor MR, both filed on the fork.
            answer_merge_probes(
                &server,
                &[
                    (
                        "/projects/alice%2Fwidget/merge_requests/10",
                        r#"{"state":"merged"}"#,
                    ),
                    (
                        "/projects/alice%2Fwidget/merge_requests/20",
                        r#"{"state":"opened"}"#,
                    ),
                ],
            );

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

        let store = Arc::new(crate::store_lock::StoreMutex::new(store()));
        store
            .lock()
            .register_project("demo", "orchestrator", root_dir.to_str().unwrap(), "git")
            .unwrap();
        store
            .lock()
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
            .create_guardian("demo", "release", root_dir.to_str().unwrap())
            .unwrap();
        store
            .lock()
            .set_guardian_status(&gid, GuardianStatus::InReview, None)
            .unwrap();
        for branch in ["a", "b"] {
            store.lock().add_guardian_branch(&gid, branch).unwrap();
        }
        let ids: Vec<_> = store
            .lock()
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

        let rows = store.lock().list_pull_requests_for_guardian(&gid).unwrap();
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
        // Every `submit_stacked_branch_pr` call now checks "does an open
        // PR/MR already exist for this head?" via a GET before it POSTs a
        // new one (RAL-<new>) -- expect and answer "no" for each branch.
        let expect_none_then_create =
            |server: &tiny_http::Server, repo: &str, number: i64, url: &str| {
                let req = server.recv().unwrap();
                assert_eq!(req.method(), &tiny_http::Method::Get);
                assert!(
                    req.url().starts_with(&format!("/repos/{repo}/pulls?")),
                    "{}",
                    req.url()
                );
                req.respond(tiny_http::Response::from_string("[]").with_status_code(200))
                    .unwrap();

                let mut req = server.recv().unwrap();
                assert_eq!(req.url(), format!("/repos/{repo}/pulls"));
                let mut body = String::new();
                req.as_reader().read_to_string(&mut body).unwrap();
                let payload: serde_json::Value = serde_json::from_str(&body).unwrap();
                req.respond(
                    tiny_http::Response::from_string(format!(
                        r#"{{"number":{number},"html_url":"{url}"}}"#
                    ))
                    .with_status_code(201),
                )
                .unwrap();
                payload
            };
        let handle = std::thread::spawn(move || {
            // Root branch: cross-repository PR filed at the parent, head is
            // `<fork_owner>:<alias>`.
            let payload = expect_none_then_create(&server, "acme/widget", 1, "http://x/1");
            assert_eq!(payload["head"], serde_json::json!("alice:a-alias"));
            assert_eq!(payload["base"], serde_json::json!("release"));

            // Branch b: fork-internal PR based on a's own alias. Still a
            // same-repo (alice/widget) head, but GitHub's `head` filter
            // silently ignores a bare branch name (see
            // `ForgeClient::same_repo_head`), so even a fork-internal head
            // must carry the fork's own `owner:` prefix.
            let payload = expect_none_then_create(&server, "alice/widget", 2, "http://x/2");
            assert_eq!(payload["head"], serde_json::json!("alice:b-alias"));
            assert_eq!(payload["base"], serde_json::json!("a-alias"));

            // Branch c: fork-internal PR based on b's own alias.
            let payload = expect_none_then_create(&server, "alice/widget", 3, "http://x/3");
            assert_eq!(payload["head"], serde_json::json!("alice:c-alias"));
            assert_eq!(payload["base"], serde_json::json!("b-alias"));
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

        let store = Arc::new(crate::store_lock::StoreMutex::new(store()));
        let gid = store
            .lock()
            .create_guardian("demo", "release", root_dir.to_str().unwrap())
            .unwrap();
        for branch in ["a", "b", "c"] {
            store.lock().add_guardian_branch(&gid, branch).unwrap();
        }
        let ids: Vec<_> = store
            .lock()
            .get_guardian(&gid)
            .unwrap()
            .branches
            .into_iter()
            .map(|b| b.id)
            .collect();
        for (branch_id, review_branch) in ids.iter().zip(["review/a", "review/b", "review/c"]) {
            store
                .lock()
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

        let guardian = store.lock().get_guardian(&gid).unwrap();
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
        let rows = store.lock().list_pull_requests_for_guardian(&gid).unwrap();
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

    /// RAL-395 regression: a stack submitted one branch at a time via
    /// explicit `branch_id: Some` requests -- what the CLI's `ralphus review
    /// pr submit <id> --position N` sends, called once per branch -- must
    /// still end up registered as a native GitHub PR stack, not just
    /// chained by base ref. Reproduces `submit_pull_requests_inner`'s
    /// per-branch (non-whole-stack) dispatch exactly: each call creates its
    /// own PR via `submit_stacked_branch_pr`, then reconciles the native
    /// stack via `reconcile_native_pr_stack`, mirroring two independent CLI
    /// invocations rather than one "submit the whole stack" request.
    #[test]
    fn per_branch_submission_still_registers_the_native_github_stack() {
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let addr = server.server_addr().to_string();
        let handle = std::thread::spawn(move || {
            // Requests arrive in a mix of fixed (PR creation, stack
            // registration) and non-deterministic (per-PR live-state checks,
            // iterated from a `HashMap`) order, so this dispatches by
            // method+url rather than asserting a strict sequence. Each
            // branch's `submit_stacked_branch_pr` call also now opens with a
            // "does a PR already exist for this head?" GET (RAL-<new>) --
            // two more requests than before this fix.
            let mut next_pr_number = 10_i64;
            let mut stack_payload = serde_json::Value::Null;
            for _ in 0..8 {
                let mut req = server.recv().unwrap();
                let method = req.method().clone();
                let url = req.url().to_string();
                if method == tiny_http::Method::Get && url.starts_with("/repos/acme/widget/pulls?")
                {
                    req.respond(tiny_http::Response::from_string("[]").with_status_code(200))
                        .unwrap();
                } else if method == tiny_http::Method::Post && url == "/repos/acme/widget/pulls" {
                    let number = next_pr_number;
                    next_pr_number += 1;
                    req.respond(
                        tiny_http::Response::from_string(format!(
                            r#"{{"number":{number},"html_url":"http://x/{number}"}}"#
                        ))
                        .with_status_code(201),
                    )
                    .unwrap();
                } else if method == tiny_http::Method::Get
                    && url.starts_with("/repos/acme/widget/pulls/")
                {
                    req.respond(
                        tiny_http::Response::from_string(r#"{"state":"open"}"#)
                            .with_status_code(200),
                    )
                    .unwrap();
                } else if method == tiny_http::Method::Post && url == "/repos/acme/widget/stacks" {
                    let mut body = String::new();
                    req.as_reader().read_to_string(&mut body).unwrap();
                    stack_payload = serde_json::from_str(&body).unwrap();
                    req.respond(
                        tiny_http::Response::from_string(r#"{"number": 77}"#).with_status_code(201),
                    )
                    .unwrap();
                    // The second call's reconciliation registering the
                    // native stack is the behavior under test -- this
                    // request never happened before RAL-395.
                    break;
                } else {
                    panic!("unexpected request: {method:?} {url}");
                }
            }
            stack_payload
        });

        let origin_bare = tmp_dir("per-branch-stack-origin-bare");
        g(&origin_bare, &["init", "--bare"]);

        let root_dir = tmp_dir("per-branch-stack-work");
        g(&root_dir, &["init", "--initial-branch", "release"]);
        gwrite(&root_dir, "base.txt", "base\n");
        g(&root_dir, &["add", "."]);
        g(&root_dir, &["commit", "--message", "base"]);
        for (branch, file) in [("review/a", "a.txt"), ("review/b", "b.txt")] {
            g(&root_dir, &["checkout", "-b", branch]);
            gwrite(&root_dir, file, "content\n");
            g(&root_dir, &["add", "."]);
            g(&root_dir, &["commit", "--message", &format!("add {file}")]);
        }
        g(&root_dir, &["checkout", "release"]);
        g(
            &root_dir,
            &["remote", "add", "origin", origin_bare.to_str().unwrap()],
        );

        let store = Arc::new(crate::store_lock::StoreMutex::new(store()));
        let gid = store
            .lock()
            .create_guardian("demo", "release", root_dir.to_str().unwrap())
            .unwrap();
        for branch in ["a", "b"] {
            store.lock().add_guardian_branch(&gid, branch).unwrap();
        }
        let ids: Vec<_> = store
            .lock()
            .get_guardian(&gid)
            .unwrap()
            .branches
            .into_iter()
            .map(|b| b.id)
            .collect();
        for (branch_id, review_branch) in ids.iter().zip(["review/a", "review/b"]) {
            store
                .lock()
                .set_branch_review(&gid, branch_id, review_branch, "wt")
                .unwrap();
        }

        let client = crate::forge::ForgeClient::new(
            crate::forge::ForgeKind::GitHub,
            format!("http://{addr}"),
            "acme/widget".to_string(),
            Some("tok".to_string()),
        );
        let runner = NoopRunner;

        let guardian = store.lock().get_guardian(&gid).unwrap();
        let mut ordered_enabled: Vec<&BranchView> =
            guardian.branches.iter().filter(|b| b.enabled).collect();
        ordered_enabled.sort_by_key(|b| b.position);

        // Two independent calls -- exactly what `ralphus review pr submit
        // <id> --position 0` then `--position 1` produces, each going
        // through the exact dispatch `submit_pull_requests_inner` uses for
        // an explicit `branch_id: Some` request: create the PR, then
        // reconcile the native stack.
        for branch in &ordered_enabled {
            let existing_prs = store.lock().list_pull_requests_for_guardian(&gid).unwrap();
            let mut alias_by_branch = open_alias_by_branch(&existing_prs);
            let req = PrRequest {
                branch_id: Some(branch.id.clone()),
                branch_alias: None,
                title: Some("Title".to_string()),
                description: Some("Description".to_string()),
                use_worktree_branch_name: None,
            };
            submit_stacked_branch_pr(
                &store,
                &runner,
                &client,
                &gid,
                &root_dir,
                "origin",
                &guardian,
                &ordered_enabled,
                &mut alias_by_branch,
                "release",
                branch,
                &req,
                "{name}-alias",
                None,
                "stack-1",
                None,
            )
            .unwrap();

            reconcile_native_pr_stack(&store, &client, &gid, None, 1).unwrap();
        }

        assert_eq!(
            store.lock().get_guardian_forge_stack_number(&gid).unwrap(),
            Some(77),
            "the second per-branch submission must have registered the native stack"
        );

        let stack_payload = handle.join().unwrap();
        assert_eq!(stack_payload["pull_requests"], serde_json::json!([10, 11]));

        let _ = std::fs::remove_dir_all(&root_dir);
        let _ = std::fs::remove_dir_all(&origin_bare);
    }

    /// RAL-397 regression: submit a review's PR stack against `origin`,
    /// change its upstream to a different remote (`ralphus review upstream
    /// set`, modeled here via [`Store::set_guardian_base_branch`]), then
    /// resubmit -- the fresh PR must land on the *new* remote, not be
    /// silently blocked because a still-recorded-open PR row names the old
    /// remote's repository. `client`/`remote_name` are passed in manually
    /// for each round rather than re-derived through
    /// `crate::forge::resolve_remote_name_excluding`/`resolve_remote_for`,
    /// exactly like every other push+create test in this module (see the
    /// note above [`setup_auto_submit_repo`]: there is no portable way to
    /// combine a real local push target with a forge-host-parseable remote
    /// URL in this test environment) -- they stand in for exactly what
    /// `submit_pull_requests_inner` freshly resolves from the guardian's
    /// `base_branch` on every call.
    fn resubmit_after_upstream_change_targets_the_new_remote(forge: &str) {
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let addr = server.server_addr().to_string();
        let forge_name = forge.to_string();
        let handle = std::thread::spawn(move || {
            let mut next_number = 101_i64;
            loop {
                let req = match server.recv_timeout(std::time::Duration::from_secs(20)) {
                    Ok(Some(r)) => r,
                    Ok(None) | Err(_) => break,
                };
                let method = req.method().clone();
                let url = req.url().to_string();
                let path = url.split('?').next().unwrap_or(&url).to_string();
                let is_list = if forge_name == "github" {
                    path.ends_with("/pulls")
                } else {
                    path.ends_with("/merge_requests")
                };
                let is_item = if forge_name == "github" {
                    path.contains("/pulls/")
                } else {
                    path.contains("/merge_requests/")
                };
                if method == tiny_http::Method::Get && is_list {
                    req.respond(tiny_http::Response::from_string("[]").with_status_code(200))
                        .unwrap();
                } else if method == tiny_http::Method::Post && is_list {
                    let number = next_number;
                    next_number += 1;
                    let body = if forge_name == "github" {
                        format!(r#"{{"number":{number},"html_url":"http://x/{number}"}}"#)
                    } else {
                        format!(r#"{{"iid":{number},"web_url":"http://x/{number}"}}"#)
                    };
                    req.respond(tiny_http::Response::from_string(body).with_status_code(201))
                        .unwrap();
                } else if method == tiny_http::Method::Get && is_item {
                    let body = if forge_name == "github" {
                        r#"{"state":"open"}"#
                    } else {
                        r#"{"state":"opened"}"#
                    };
                    req.respond(tiny_http::Response::from_string(body).with_status_code(200))
                        .unwrap();
                } else if method == tiny_http::Method::Get
                    && forge_name == "github"
                    && path.contains("/contents/")
                {
                    // `synthesize_pr_text`'s PR-template lookup -- 404 on
                    // every candidate path means "no template found", which
                    // it already treats as a normal fallback case.
                    req.respond(tiny_http::Response::from_string("{}").with_status_code(404))
                        .unwrap();
                } else if method == tiny_http::Method::Get
                    && forge_name == "gitlab"
                    && path.starts_with("/projects/")
                    && !path.contains("/merge_requests")
                {
                    // `synthesize_pr_text`'s PR-template lookup starts by
                    // fetching the project's default branch -- 404 means "no
                    // default branch found", which it already treats as
                    // "no template".
                    req.respond(tiny_http::Response::from_string("{}").with_status_code(404))
                        .unwrap();
                } else {
                    panic!(
                        "unexpected request in resubmit-after-upstream-change test: \
                         {method:?} {url}"
                    );
                }
            }
        });

        let origin_bare = tmp_dir(&format!("resubmit-remote-origin-bare-{forge}"));
        g(&origin_bare, &["init", "--bare"]);
        let alt_bare = tmp_dir(&format!("resubmit-remote-alt-bare-{forge}"));
        g(&alt_bare, &["init", "--bare"]);

        let root_dir = tmp_dir(&format!("resubmit-remote-work-{forge}"));
        g(&root_dir, &["init", "--initial-branch", "release"]);
        gwrite(&root_dir, "base.txt", "base\n");
        g(&root_dir, &["add", "."]);
        g(&root_dir, &["commit", "--message", "base"]);
        g(&root_dir, &["checkout", "-b", "review/a"]);
        gwrite(&root_dir, "a.txt", "content\n");
        g(&root_dir, &["add", "."]);
        g(&root_dir, &["commit", "--message", "add a.txt"]);
        g(&root_dir, &["checkout", "release"]);
        g(
            &root_dir,
            &["remote", "add", "origin", origin_bare.to_str().unwrap()],
        );
        g(
            &root_dir,
            &["remote", "add", "alt", alt_bare.to_str().unwrap()],
        );

        let store = Arc::new(crate::store_lock::StoreMutex::new(store()));
        let gid = store
            .lock()
            .create_guardian("demo", "origin/release", root_dir.to_str().unwrap())
            .unwrap();
        store.lock().add_guardian_branch(&gid, "a").unwrap();
        let branch_id = store.lock().get_guardian(&gid).unwrap().branches[0]
            .id
            .clone();
        store
            .lock()
            .set_branch_review(&gid, &branch_id, "review/a", "wt")
            .unwrap();

        let (origin_repo, alt_repo) = if forge == "github" {
            (
                "acme/origin-widget".to_string(),
                "acme/alt-widget".to_string(),
            )
        } else {
            (
                "acme%2Forigin-widget".to_string(),
                "acme%2Falt-widget".to_string(),
            )
        };
        let kind = if forge == "github" {
            crate::forge::ForgeKind::GitHub
        } else {
            crate::forge::ForgeKind::GitLab
        };
        let client_origin = crate::forge::ForgeClient::new(
            kind,
            format!("http://{addr}"),
            origin_repo.clone(),
            Some("tok".to_string()),
        );
        let client_alt = crate::forge::ForgeClient::new(
            kind,
            format!("http://{addr}"),
            alt_repo.clone(),
            Some("tok".to_string()),
        );
        let runner = NoopRunner;

        // Round 1: submit the (one-branch) stack against `origin`, exactly
        // what a fresh review's first "submit PR stack" does.
        let guardian1 = store.lock().get_guardian(&gid).unwrap();
        let mut ordered_enabled1: Vec<&BranchView> =
            guardian1.branches.iter().filter(|b| b.enabled).collect();
        ordered_enabled1.sort_by_key(|b| b.position);
        let mut alias_by_branch: HashMap<String, String> = HashMap::new();
        let (created1, failed1) = submit_stack_for_guardian(
            &store,
            &runner,
            &client_origin,
            &gid,
            &root_dir,
            "origin",
            &guardian1,
            &ordered_enabled1,
            &mut alias_by_branch,
            "release",
            &[],
            "{name}-alias",
            None,
            "stack-1",
            None,
            None,
        )
        .unwrap();
        assert!(
            failed1.is_empty(),
            "round 1 should not report any per-branch failures: {failed1:?}"
        );
        assert_eq!(
            created1.len(),
            1,
            "round 1 should file one PR against origin"
        );
        assert_eq!(created1[0].repo, origin_repo);
        assert!(created1[0].pr_number.is_some());
        let origin_pr_id = created1[0].id.clone();

        // The review's upstream branch is switched to a different remote,
        // exactly what `ralphus review upstream set` does.
        store
            .lock()
            .set_guardian_base_branch(&gid, "alt/release")
            .unwrap();
        assert_eq!(
            store.lock().get_guardian(&gid).unwrap().base_branch,
            "alt/release"
        );

        // Round 2: resubmit. `client_alt`/`"alt"` stand in for what
        // `submit_pull_requests_inner` freshly resolves from the guardian's
        // now-changed `base_branch` -- this is the regression under test:
        // the still-recorded-open PR against `origin` must not block a
        // fresh PR from being filed against `alt`.
        let guardian2 = store.lock().get_guardian(&gid).unwrap();
        let mut ordered_enabled2: Vec<&BranchView> =
            guardian2.branches.iter().filter(|b| b.enabled).collect();
        ordered_enabled2.sort_by_key(|b| b.position);
        let existing_prs = store.lock().list_pull_requests_for_guardian(&gid).unwrap();
        let mut alias_by_branch = open_alias_by_branch(&existing_prs);
        let (created2, failed2) = submit_stack_for_guardian(
            &store,
            &runner,
            &client_alt,
            &gid,
            &root_dir,
            "alt",
            &guardian2,
            &ordered_enabled2,
            &mut alias_by_branch,
            "release",
            &existing_prs,
            "{name}-alias",
            None,
            "stack-2",
            None,
            None,
        )
        .unwrap();
        assert!(
            failed2.is_empty(),
            "round 2 should not report any per-branch failures: {failed2:?}"
        );
        assert_eq!(
            created2.len(),
            1,
            "resubmitting after an upstream change must file a fresh PR against \
             the new remote instead of treating the old remote's PR as still blocking"
        );
        assert_eq!(created2[0].repo, alt_repo);
        assert!(created2[0].pr_number.is_some());

        // The branch's alias actually landed on the ALT bare repo, not the
        // original one.
        let alt_refs = g(
            &alt_bare,
            &["for-each-ref", "--format=%(refname:short)", "refs/heads"],
        );
        assert!(
            alt_refs.contains(&created2[0].branch_alias),
            "alias must have been pushed to the new remote: {alt_refs}"
        );

        // The old, now-unreachable PR against origin is left exactly as
        // recorded -- retargeting it is explicitly out of scope for RAL-397.
        let old_pr = store.lock().get_pull_request(&origin_pr_id).unwrap();
        assert_eq!(old_pr.state, "open");
        assert_eq!(old_pr.repo, origin_repo);

        handle.join().unwrap();
        let _ = std::fs::remove_dir_all(&root_dir);
        let _ = std::fs::remove_dir_all(&origin_bare);
        let _ = std::fs::remove_dir_all(&alt_bare);
    }

    #[test]
    fn resubmit_after_upstream_change_targets_the_new_remote_github() {
        resubmit_after_upstream_change_targets_the_new_remote("github");
    }

    #[test]
    fn resubmit_after_upstream_change_targets_the_new_remote_gitlab() {
        resubmit_after_upstream_change_targets_the_new_remote("gitlab");
    }

    #[test]
    fn submit_stacked_branch_pr_reconciles_a_diverged_remote_alias_instead_of_failing() {
        // A branch whose PR was previously dropped/unlinked (or whose remote
        // alias a reviewer pushed straight to) can have a remote branch that
        // already carries a commit this review worktree doesn't. Before this
        // fix, `guard_against_clobber` would refuse the push outright and
        // the whole PR creation would fail. Now `submit_stacked_branch_pr`
        // must reconcile (pull the remote's extra commit into the worktree)
        // and complete the submission instead.
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let addr = server.server_addr().to_string();
        let handle = std::thread::spawn(move || {
            // `submit_stacked_branch_pr` checks "does an open PR already
            // exist for this head?" before creating (RAL-<new>) -- answer no.
            let req = server.recv().unwrap();
            assert_eq!(req.method(), &tiny_http::Method::Get);
            assert!(
                req.url().starts_with("/repos/acme/w/pulls?"),
                "{}",
                req.url()
            );
            req.respond(tiny_http::Response::from_string("[]").with_status_code(200))
                .unwrap();

            let req = server.recv().unwrap();
            assert_eq!(req.method(), &tiny_http::Method::Post);
            assert_eq!(req.url(), "/repos/acme/w/pulls");
            req.respond(
                tiny_http::Response::from_string(r#"{"number":42,"html_url":"http://x/42"}"#)
                    .with_status_code(201),
            )
            .unwrap();
        });

        let root = tmp_dir("reconcile-push-root");
        let remote_dir = tmp_dir("reconcile-push-remote");
        let mut init_opts = git2::RepositoryInitOptions::new();
        init_opts.initial_head("main");
        let repo = git2::Repository::init_opts(&root, &init_opts).unwrap();
        let sig = git2::Signature::now("t", "t@t").unwrap();
        gwrite(&root, "base.txt", "base\n");
        let base_oid = git2_commit_all(&repo, &sig, "base", &[]);
        let base_commit = repo.find_commit(base_oid).unwrap();
        repo.branch("review-branch", &base_commit, false).unwrap();
        git2_checkout(&repo, "review-branch");
        gwrite(&root, "feat.txt", "feat\n");
        git2_commit_all(&repo, &sig, "feat", &[&base_commit]);

        git2::Repository::init_bare(&remote_dir).unwrap();
        repo.remote("origin", remote_dir.to_str().unwrap()).unwrap();
        g(&root, &["push", "origin", "review-branch:refs/heads/pr-y"]);

        // Simulate the real-world case: someone (a reviewer, or a prior
        // submission attempt before its PR row got unlinked) pushed a commit
        // straight to the remote alias that this worktree never received.
        let clone_dir = tmp_dir("reconcile-push-clone");
        let _ = std::fs::remove_dir_all(&clone_dir);
        g(
            clone_dir.parent().unwrap(),
            &[
                "clone",
                remote_dir.to_str().unwrap(),
                clone_dir.file_name().unwrap().to_str().unwrap(),
            ],
        );
        g(&clone_dir, &["checkout", "pr-y"]);
        gwrite(&clone_dir, "reviewer.txt", "preserve this remote commit\n");
        g(&clone_dir, &["add", "."]);
        g(&clone_dir, &["commit", "--message", "reviewer fix"]);
        g(&clone_dir, &["push", "origin", "pr-y"]);
        let reviewer_sha = g(&clone_dir, &["rev-parse", "pr-y"]).trim().to_string();

        let store = Arc::new(crate::store_lock::StoreMutex::new(store()));
        let gid = store
            .lock()
            .create_guardian("demo", "main", root.to_str().unwrap())
            .unwrap();
        store
            .lock()
            .add_guardian_branch(&gid, "review-branch")
            .unwrap();
        let branch_id = store.lock().get_guardian(&gid).unwrap().branches[0]
            .id
            .clone();
        store
            .lock()
            .set_branch_review(&gid, &branch_id, "review-branch", root.to_str().unwrap())
            .unwrap();
        store
            .lock()
            .set_guardian_status(&gid, GuardianStatus::InReview, None)
            .unwrap();

        let client = crate::forge::ForgeClient::new(
            crate::forge::ForgeKind::GitHub,
            format!("http://{addr}"),
            "acme/w".to_string(),
            Some("tok".to_string()),
        );
        let runner = NoopRunner;
        let guardian = store.lock().get_guardian(&gid).unwrap();
        let ordered_enabled: Vec<&BranchView> = guardian.branches.iter().collect();
        let branch = ordered_enabled[0];
        let mut alias_by_branch = HashMap::new();
        let req = PrRequest {
            branch_id: Some(branch_id.clone()),
            branch_alias: Some("pr-y".to_string()),
            title: Some("Title".to_string()),
            description: Some("Description".to_string()),
            use_worktree_branch_name: None,
        };

        let pr = submit_stacked_branch_pr(
            &store,
            &runner,
            &client,
            &gid,
            &root,
            "origin",
            &guardian,
            &ordered_enabled,
            &mut alias_by_branch,
            "main",
            branch,
            &req,
            "{name}-alias",
            None,
            "stack-1",
            None,
        )
        .expect("submission must reconcile the diverged remote alias instead of erroring");

        assert_eq!(pr.pr_number, Some(42));
        assert_eq!(pr.branch_alias, "pr-y");

        // The reviewer's commit made it into the local review branch...
        g(
            &root,
            &[
                "merge-base",
                "--is-ancestor",
                &reviewer_sha,
                "review-branch",
            ],
        );
        // ...and the remote alias now matches what was actually pushed back.
        assert_eq!(
            g(&remote_dir, &["rev-parse", "pr-y"]).trim(),
            g(&root, &["rev-parse", "review-branch"]).trim()
        );

        handle.join().unwrap();
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&remote_dir);
        let _ = std::fs::remove_dir_all(&clone_dir);
    }

    #[test]
    fn submit_stack_for_guardian_does_not_let_one_branchs_failure_touch_its_siblings() {
        // Regression test for the "auto-submit failed" badge showing up on
        // every branch in a review instead of just the one whose PR actually
        // failed (e.g. GitHub's "no commits between X and Y" once a stacked
        // branch's own diff is already in its base). Before this fix, this
        // loop bailed out entirely via `?` on the first branch that failed
        // to submit -- so a later branch in the same batch never even got
        // attempted, and the batch's single error had nowhere to go but
        // whichever branch its caller happened to be stamping, so a failure
        // landed on branches that had not produced it.
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let addr = server.server_addr().to_string();
        // A lenient dispatcher (rather than a hand-counted request sequence)
        // so this test doesn't have to track every incidental call
        // `submit_stacked_branch_pr` makes along the way (e.g. probing for a
        // PR template) -- only the two calls this test actually cares about:
        // branch A's create 422s, branch B's create succeeds.
        let handle = std::thread::spawn(move || {
            let mut received_any = false;
            loop {
                // The first `recv` is generous: under a full parallel
                // `nextest` run this thread can be waiting behind real
                // git2/filesystem setup work in the main thread while dozens
                // of other tests contend for CPU, and too short a timeout
                // here reads as "no more requests coming" and drops the
                // listening socket before the real first request ever
                // arrives. Once the client is mid-flow, a much shorter idle
                // wait is enough to notice "done" without every run paying
                // the full timeout as dead time at the end.
                let timeout = if received_any {
                    std::time::Duration::from_secs(5)
                } else {
                    std::time::Duration::from_secs(30)
                };
                let mut req = match server.recv_timeout(timeout) {
                    Ok(Some(r)) => r,
                    _ => break,
                };
                received_any = true;
                let method = req.method().clone();
                let url = req.url().to_string();
                if method == tiny_http::Method::Get && url.starts_with("/repos/acme/w/pulls?") {
                    // `find_existing_pull_request`: report no pre-existing PR
                    // for either branch, same as a fresh submission.
                    req.respond(tiny_http::Response::from_string("[]").with_status_code(200))
                        .unwrap();
                } else if method == tiny_http::Method::Get
                    && url.starts_with("/repos/acme/w/contents/")
                {
                    // PR-template probe: none of the candidate paths exist.
                    req.respond(
                        tiny_http::Response::from_string("Not Found").with_status_code(404),
                    )
                    .unwrap();
                } else if method == tiny_http::Method::Post && url == "/repos/acme/w/pulls" {
                    let mut body = String::new();
                    req.as_reader().read_to_string(&mut body).unwrap();
                    if body.contains("branch-a") {
                        req.respond(
                            tiny_http::Response::from_string(
                                r#"{"message":"Validation Failed","errors":[{"resource":"PullRequest","code":"custom","message":"No commits between main and branch-a"}]}"#,
                            )
                            .with_status_code(422),
                        )
                        .unwrap();
                    } else {
                        req.respond(
                            tiny_http::Response::from_string(
                                r#"{"number":77,"html_url":"http://x/77"}"#,
                            )
                            .with_status_code(201),
                        )
                        .unwrap();
                    }
                } else if method == tiny_http::Method::Get && url == "/repos/acme/w/pulls/77" {
                    // `reconcile_native_pr_stack`'s `refresh_open_prs` re-checks
                    // branch B's freshly-created PR is still open.
                    req.respond(
                        tiny_http::Response::from_string(r#"{"state":"open"}"#)
                            .with_status_code(200),
                    )
                    .unwrap();
                } else {
                    panic!("unexpected request: {method:?} {url}");
                }
            }
        });

        let root = tmp_dir("split-failure-root");
        let remote_dir = tmp_dir("split-failure-remote");
        let mut init_opts = git2::RepositoryInitOptions::new();
        init_opts.initial_head("main");
        let repo = git2::Repository::init_opts(&root, &init_opts).unwrap();
        let sig = git2::Signature::now("t", "t@t").unwrap();
        gwrite(&root, "base.txt", "base\n");
        let base_oid = git2_commit_all(&repo, &sig, "base", &[]);
        let base_commit = repo.find_commit(base_oid).unwrap();

        repo.branch("branch-a", &base_commit, false).unwrap();
        git2_checkout(&repo, "branch-a");
        gwrite(&root, "a.txt", "a\n");
        git2_commit_all(&repo, &sig, "commit a", &[&base_commit]);

        git2_checkout(&repo, "main");
        repo.branch("branch-b", &base_commit, false).unwrap();
        git2_checkout(&repo, "branch-b");
        gwrite(&root, "b.txt", "b\n");
        git2_commit_all(&repo, &sig, "commit b", &[&base_commit]);

        git2_checkout(&repo, "main");

        git2::Repository::init_bare(&remote_dir).unwrap();
        repo.remote("origin", remote_dir.to_str().unwrap()).unwrap();

        let store = Arc::new(crate::store_lock::StoreMutex::new(store()));
        let gid = store
            .lock()
            .create_guardian("demo", "main", root.to_str().unwrap())
            .unwrap();
        store.lock().add_guardian_branch(&gid, "branch-a").unwrap();
        store.lock().add_guardian_branch(&gid, "branch-b").unwrap();
        let ids_guardian = store.lock().get_guardian(&gid).unwrap();
        let branch_a_id = ids_guardian.branches[0].id.clone();
        let branch_b_id = ids_guardian.branches[1].id.clone();
        store
            .lock()
            .set_branch_review(&gid, &branch_a_id, "branch-a", root.to_str().unwrap())
            .unwrap();
        store
            .lock()
            .set_branch_review(&gid, &branch_b_id, "branch-b", root.to_str().unwrap())
            .unwrap();

        let client = crate::forge::ForgeClient::new(
            crate::forge::ForgeKind::GitHub,
            format!("http://{addr}"),
            "acme/w".to_string(),
            Some("tok".to_string()),
        );
        let runner = NoopRunner;
        let guardian = store.lock().get_guardian(&gid).unwrap();
        let ordered_enabled: Vec<&BranchView> = guardian.branches.iter().collect();
        let mut alias_by_branch = HashMap::new();

        let (created, failed) = submit_stack_for_guardian(
            &store,
            &runner,
            &client,
            &gid,
            &root,
            "origin",
            &guardian,
            &ordered_enabled,
            &mut alias_by_branch,
            "main",
            &[],
            "{name}-pr",
            None,
            "stack-1",
            None,
            None,
        )
        .expect("a per-branch failure must not fail the whole batch");

        assert_eq!(created.len(), 1, "branch B must still be submitted");
        assert_eq!(created[0].branch_id.as_deref(), Some(branch_b_id.as_str()));
        assert_eq!(
            failed.len(),
            1,
            "exactly branch A's own failure must be reported, not one per branch"
        );
        assert_eq!(failed[0].0, branch_a_id);
        assert!(failed[0].1.contains("422"), "{}", failed[0].1);

        handle.join().unwrap();
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&remote_dir);
    }

    #[test]
    fn submit_stacked_branch_pr_adopts_a_pre_existing_open_pull_request_instead_of_creating_a_duplicate()
     {
        // A branch's PR row was dropped/unlinked locally (or a resubmit
        // races a prior attempt) while the forge still has an open PR for
        // that exact head -- creating a new one would 422 ("A pull request
        // already exists for ..."). `submit_stacked_branch_pr` must discover
        // it via the documented head-filter query and adopt it, never by
        // parsing that creation error's free-text message.
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let addr = server.server_addr().to_string();
        let handle = std::thread::spawn(move || {
            let req = server.recv().unwrap();
            assert_eq!(req.method(), &tiny_http::Method::Get);
            assert!(
                req.url().starts_with("/repos/acme/w/pulls?"),
                "{}",
                req.url()
            );
            req.respond(
                tiny_http::Response::from_string(
                    r#"[{"number":21,"html_url":"http://x/21","base":{"ref":"predecessor-review"},"title":"Existing title","body":"Existing body"}]"#,
                )
                .with_status_code(200),
            )
            .unwrap();
            // No creation POST must follow -- adoption skips it entirely.
        });

        let root = tmp_dir("adopt-existing-root");
        let mut init_opts = git2::RepositoryInitOptions::new();
        init_opts.initial_head("main");
        let repo = git2::Repository::init_opts(&root, &init_opts).unwrap();
        let sig = git2::Signature::now("t", "t@t").unwrap();
        gwrite(&root, "base.txt", "base\n");
        let base_oid = git2_commit_all(&repo, &sig, "base", &[]);
        let base_commit = repo.find_commit(base_oid).unwrap();
        repo.branch("review-branch", &base_commit, false).unwrap();
        git2_checkout(&repo, "review-branch");
        gwrite(&root, "feat.txt", "feat\n");
        git2_commit_all(&repo, &sig, "feat", &[&base_commit]);

        let remote_dir = tmp_dir("adopt-existing-remote");
        git2::Repository::init_bare(&remote_dir).unwrap();
        repo.remote("origin", remote_dir.to_str().unwrap()).unwrap();
        // The remote alias already matches local exactly, so the push is a
        // safe no-op superset -- only the create-vs-adopt decision that
        // follows it is under test here.
        g(&root, &["push", "origin", "review-branch:refs/heads/pr-y"]);

        let store = Arc::new(crate::store_lock::StoreMutex::new(store()));
        let gid = store
            .lock()
            .create_guardian("demo", "main", root.to_str().unwrap())
            .unwrap();
        store
            .lock()
            .add_guardian_branch(&gid, "review-branch")
            .unwrap();
        let branch_id = store.lock().get_guardian(&gid).unwrap().branches[0]
            .id
            .clone();
        store
            .lock()
            .set_branch_review(&gid, &branch_id, "review-branch", root.to_str().unwrap())
            .unwrap();

        let client = crate::forge::ForgeClient::new(
            crate::forge::ForgeKind::GitHub,
            format!("http://{addr}"),
            "acme/w".to_string(),
            Some("tok".to_string()),
        );
        let runner = NoopRunner;
        let guardian = store.lock().get_guardian(&gid).unwrap();
        let ordered_enabled: Vec<&BranchView> = guardian.branches.iter().collect();
        let branch = ordered_enabled[0];
        let mut alias_by_branch = HashMap::new();
        // No title/description: if adoption were skipped, this would fall
        // through to `resolve_title_description`'s template-fetch/agent-
        // synthesis path and then a real creation POST -- both hit a mock
        // server that has already exited, failing loudly rather than
        // silently passing.
        let req = PrRequest {
            branch_id: Some(branch_id.clone()),
            branch_alias: Some("pr-y".to_string()),
            title: None,
            description: None,
            use_worktree_branch_name: None,
        };

        let pr = submit_stacked_branch_pr(
            &store,
            &runner,
            &client,
            &gid,
            &root,
            "origin",
            &guardian,
            &ordered_enabled,
            &mut alias_by_branch,
            "main",
            branch,
            &req,
            "{name}-alias",
            None,
            "stack-1",
            None,
        )
        .expect("submission must adopt the pre-existing open PR instead of erroring");

        assert_eq!(pr.pr_number, Some(21));
        assert_eq!(pr.base_ref, "predecessor-review");
        assert_eq!(pr.title, "Existing title");
        assert_eq!(pr.description, "Existing body");

        handle.join().unwrap();
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&remote_dir);
    }

    #[test]
    fn submit_stacked_branch_pr_clears_a_stale_auto_submit_error_on_success() {
        // RAL-317's `auto_submit_error` badge is cleared by
        // `run_auto_submit_pass` only for the branches that pass covered --
        // so a PR that succeeds through any other route (this function, the
        // single site every creation/adoption path funnels through) must
        // clear the branch's own stale marker itself, or the board keeps
        // showing "auto-submit failed" forever after the real problem is
        // already fixed.
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let addr = server.server_addr().to_string();
        let handle = std::thread::spawn(move || {
            let req = server.recv().unwrap();
            assert_eq!(req.method(), &tiny_http::Method::Get);
            req.respond(tiny_http::Response::from_string("[]").with_status_code(200))
                .unwrap();
            let mut req = server.recv().unwrap();
            let mut body = String::new();
            req.as_reader().read_to_string(&mut body).unwrap();
            req.respond(
                tiny_http::Response::from_string(r#"{"number":5,"html_url":"http://x/5"}"#)
                    .with_status_code(201),
            )
            .unwrap();
        });

        let root = tmp_dir("clear-auto-submit-error-root");
        let mut init_opts = git2::RepositoryInitOptions::new();
        init_opts.initial_head("main");
        let repo = git2::Repository::init_opts(&root, &init_opts).unwrap();
        let sig = git2::Signature::now("t", "t@t").unwrap();
        gwrite(&root, "base.txt", "base\n");
        let base_oid = git2_commit_all(&repo, &sig, "base", &[]);
        let base_commit = repo.find_commit(base_oid).unwrap();
        repo.branch("review-branch", &base_commit, false).unwrap();
        git2_checkout(&repo, "review-branch");
        gwrite(&root, "feat.txt", "feat\n");
        git2_commit_all(&repo, &sig, "feat", &[&base_commit]);

        let remote_dir = tmp_dir("clear-auto-submit-error-remote");
        git2::Repository::init_bare(&remote_dir).unwrap();
        repo.remote("origin", remote_dir.to_str().unwrap()).unwrap();

        let store = Arc::new(crate::store_lock::StoreMutex::new(store()));
        let gid = store
            .lock()
            .create_guardian("demo", "main", root.to_str().unwrap())
            .unwrap();
        store
            .lock()
            .add_guardian_branch(&gid, "review-branch")
            .unwrap();
        let branch_id = store.lock().get_guardian(&gid).unwrap().branches[0]
            .id
            .clone();
        store
            .lock()
            .set_branch_review(&gid, &branch_id, "review-branch", root.to_str().unwrap())
            .unwrap();
        // Simulate an earlier failed auto-submit attempt that left its
        // marker set, e.g. from before this branch's underlying problem was
        // fixed by hand.
        store
            .lock()
            .set_branch_auto_submit_error(&gid, &branch_id, Some("earlier failure"))
            .unwrap();

        let client = crate::forge::ForgeClient::new(
            crate::forge::ForgeKind::GitHub,
            format!("http://{addr}"),
            "acme/w".to_string(),
            Some("tok".to_string()),
        );
        let runner = NoopRunner;
        let guardian = store.lock().get_guardian(&gid).unwrap();
        let ordered_enabled: Vec<&BranchView> = guardian.branches.iter().collect();
        let branch = ordered_enabled[0];
        let mut alias_by_branch = HashMap::new();
        let req = PrRequest {
            branch_id: Some(branch_id.clone()),
            branch_alias: None,
            title: Some("Title".to_string()),
            description: Some("Description".to_string()),
            use_worktree_branch_name: None,
        };

        submit_stacked_branch_pr(
            &store,
            &runner,
            &client,
            &gid,
            &root,
            "origin",
            &guardian,
            &ordered_enabled,
            &mut alias_by_branch,
            "main",
            branch,
            &req,
            "{name}-alias",
            None,
            "stack-1",
            None,
        )
        .unwrap();

        let after = store.lock().get_guardian(&gid).unwrap();
        assert_eq!(
            after.branches[0].auto_submit_error, None,
            "a successful submission must clear the stale auto-submit-error marker, \
             not just leave a real failure's badge stuck forever"
        );

        handle.join().unwrap();
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&remote_dir);
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
        let won = s.try_claim_pr_comments(&id, &["c1".to_string()]).unwrap();
        assert_eq!(won, vec!["c1".to_string()]);
        // Re-claiming an already-claimed id wins nothing -- this is the
        // invariant a second, overlapping caller relies on to avoid
        // double-actioning the same comment.
        let rewon = s.try_claim_pr_comments(&id, &["c1".to_string()]).unwrap();
        assert!(rewon.is_empty());
        let won2 = s.try_claim_pr_comments(&id, &["c2".to_string()]).unwrap();
        assert_eq!(won2, vec!["c2".to_string()]);
        let ids = s.actioned_pr_comment_ids(&id).unwrap();
        assert_eq!(ids.len(), 2);
        assert!(ids.contains("c1") && ids.contains("c2"));
    }

    #[test]
    fn try_claim_pr_comments_only_lets_one_caller_win_each_id() {
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
        // Simulates two overlapping callers (e.g. a manual click racing the
        // standing poll) both reading the same fresh comment set before
        // either claims it.
        let ids = vec!["c1".to_string(), "c2".to_string()];
        let first = s.try_claim_pr_comments(&id, &ids).unwrap();
        let second = s.try_claim_pr_comments(&id, &ids).unwrap();
        assert_eq!(first, ids);
        assert!(
            second.is_empty(),
            "a second overlapping claim must win nothing the first call already won"
        );
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

    // ── resolve_pr_alias (RAL-307, RAL-378) ────────────────────────

    #[test]
    fn resolve_pr_alias_is_the_review_branch_name_by_default() {
        assert_eq!(
            resolve_pr_alias(
                None,
                None,
                false,
                false,
                Some("feature-x-review"),
                "{name}-review",
                "feature-x"
            ),
            "feature-x-review"
        );
    }

    #[test]
    fn resolve_pr_alias_keeps_the_review_branchs_collision_suffix() {
        // The name the branch actually claimed is used verbatim -- not
        // re-derived from the convention, which would drop the `-2`.
        assert_eq!(
            resolve_pr_alias(
                None,
                None,
                false,
                false,
                Some("feature-x-review-2"),
                "{name}-review",
                "feature-x"
            ),
            "feature-x-review-2"
        );
    }

    #[test]
    fn resolve_pr_alias_ignores_match_pr_branch_name_unless_separated() {
        // Neither RAL-307 lever means anything when the PR branch and the
        // review branch are the same branch.
        assert_eq!(
            resolve_pr_alias(
                None,
                Some(true),
                true,
                false,
                Some("feature-x-review"),
                "{name}-review",
                "feature-x"
            ),
            "feature-x-review"
        );
    }

    #[test]
    fn resolve_pr_alias_falls_back_to_the_convention_without_a_readable_name() {
        // A branch registered before readable naming has only an internal
        // `guardian/<id>/wt-<branch>` ref, which must never be published.
        assert_eq!(
            resolve_pr_alias(None, None, false, false, None, "{name}-review", "feature-x"),
            "feature-x-review"
        );
    }

    #[test]
    fn resolve_pr_alias_defaults_to_convention_when_nothing_opts_in() {
        assert_eq!(
            resolve_pr_alias(
                None,
                None,
                false,
                true,
                Some("feature-x-review"),
                "{name}-review",
                "feature-x"
            ),
            "feature-x-review"
        );
    }

    #[test]
    fn resolve_pr_alias_explicit_branch_alias_always_wins() {
        // Wins over both a request-level override and the review's own
        // persisted setting -- and over the review-branch identity too.
        assert_eq!(
            resolve_pr_alias(
                Some("custom-alias"),
                Some(true),
                true,
                true,
                Some("feature-x-review"),
                "{name}-review",
                "feature-x"
            ),
            "custom-alias"
        );
        assert_eq!(
            resolve_pr_alias(
                Some("custom-alias"),
                None,
                false,
                false,
                Some("feature-x-review"),
                "{name}-review",
                "feature-x"
            ),
            "custom-alias"
        );
    }

    #[test]
    fn resolve_pr_alias_uses_worktree_branch_name_from_per_request_override() {
        assert_eq!(
            resolve_pr_alias(
                None,
                Some(true),
                false,
                true,
                Some("feature-x-review"),
                "{name}-review",
                "feature-x"
            ),
            "feature-x"
        );
    }

    #[test]
    fn resolve_pr_alias_uses_worktree_branch_name_from_review_default() {
        assert_eq!(
            resolve_pr_alias(
                None,
                None,
                true,
                true,
                Some("feature-x-review"),
                "{name}-review",
                "feature-x"
            ),
            "feature-x"
        );
    }

    #[test]
    fn resolve_pr_alias_per_request_override_wins_over_review_default() {
        // Explicit `Some(false)` opts back out even when the review's own
        // setting is on.
        assert_eq!(
            resolve_pr_alias(
                None,
                Some(false),
                true,
                true,
                Some("feature-x-review"),
                "{name}-review",
                "feature-x"
            ),
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

    struct CapturingFailureRunner(Mutex<Option<RunnerSpec>>);

    impl Runner for CapturingFailureRunner {
        fn run(&self, spec: &RunnerSpec) -> crate::runner::RunnerResult {
            *self.0.lock().unwrap() = Some(spec.clone());
            crate::runner::RunnerResult::failure("writer unavailable")
        }
    }

    #[test]
    fn pr_text_uses_only_the_branchs_unique_range_and_never_the_review_summary() {
        let root = tmp_dir("pr-text-unique-range");
        g(&root, &["init", "--initial-branch", "main"]);
        gwrite(&root, "base.txt", "base\n");
        g(&root, &["add", "."]);
        g(&root, &["commit", "--message", "base"]);

        for (branch, file, subject) in [
            ("review/a", "a.txt", "add predecessor behavior"),
            ("review/b", "b.txt", "add this branch behavior"),
            ("review/c", "c.txt", "add downstream behavior"),
        ] {
            g(&root, &["checkout", "-b", branch]);
            gwrite(&root, file, &format!("{subject}\n"));
            g(&root, &["add", "."]);
            g(&root, &["commit", "--message", subject]);
        }
        g(&root, &["checkout", "main"]);

        let store = store();
        let gid = store
            .create_guardian("whole-review", "main", root.to_str().unwrap())
            .unwrap();
        for branch in ["a", "b", "c"] {
            store.add_guardian_branch(&gid, branch).unwrap();
        }
        let ids = store
            .get_guardian(&gid)
            .unwrap()
            .branches
            .iter()
            .map(|branch| branch.id.clone())
            .collect::<Vec<_>>();
        for ((id, review_branch), worktree) in ids
            .iter()
            .zip(["review/a", "review/b", "review/c"])
            .zip(["worktree-a", "worktree-b", "worktree-c"])
        {
            store
                .set_branch_review(&gid, id, review_branch, worktree)
                .unwrap();
        }
        store
            .set_guardian_summary(
                &gid,
                "predecessor and downstream contaminated review summary",
                Some("claude-code"),
                None,
            )
            .unwrap();
        let guardian = store.get_guardian(&gid).unwrap();
        let runner = CapturingFailureRunner(Mutex::new(None));

        let (title, description) = synthesize_pr_text(
            &runner,
            &guardian,
            1,
            Some("## Summary\n\n<!-- fill this in -->"),
            None,
        );

        assert_eq!(title, "b");
        assert!(description.starts_with("## Summary"), "{description}");
        assert!(
            description.contains("add this branch behavior"),
            "{description}"
        );
        assert!(!description.contains("predecessor"), "{description}");
        assert!(!description.contains("downstream"), "{description}");
        assert_eq!(
            fallback_pr_description(
                "commit abc\nsubject: add this branch behavior\n\nbody\n---\n",
                None,
            ),
            "- add this branch behavior"
        );

        let spec = runner.0.lock().unwrap().clone().unwrap();
        assert_eq!(spec.cwd, "worktree-b");
        let prompt = spec.prompt.unwrap();
        assert!(prompt.contains("add this branch behavior"), "{prompt}");
        assert!(prompt.contains("b.txt"), "{prompt}");
        assert!(!prompt.contains("add predecessor behavior"), "{prompt}");
        assert!(!prompt.contains("add downstream behavior"), "{prompt}");
        assert!(prompt.contains("<!-- fill this in -->"), "{prompt}");

        let _ = std::fs::remove_dir_all(root);
    }

    // ── RAL-317: run_auto_submit_pass (the review-scoped auto-submit pass) ──
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
    // exercise `run_auto_submit_pass`'s own decision logic (effective-option
    // gate, which branches a pass covers, error bookkeeping) with a real git
    // repo but no reachable forge, rather than the full submission machinery
    // it delegates to.

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
        store: &crate::store_lock::StoreHandle,
        root: &Path,
        auto_submit: bool,
    ) -> (String, String, String) {
        let gid = store
            .lock()
            .create_guardian("demo", "main", root.to_str().unwrap())
            .unwrap();
        if auto_submit {
            store
                .lock()
                .set_guardian_auto_submit_pr_stack(&gid, Some(true))
                .unwrap();
        }
        store.lock().add_guardian_branch(&gid, "feature/x").unwrap();
        let bid = store.lock().get_guardian(&gid).unwrap().branches[0]
            .id
            .clone();
        store
            .lock()
            .set_branch_review(&gid, &bid, "feature/x", "")
            .unwrap();
        store
            .lock()
            .set_branch_status(&gid, &bid, crate::guardian::MergeStatus::Done, None)
            .unwrap();
        let tip = g(root, &["rev-parse", "feature/x"]).trim().to_string();
        (gid, bid, tip)
    }

    #[test]
    fn startup_recovery_queues_terminal_auto_submit_guardian() {
        let root = setup_auto_submit_repo("recovery-queues");
        let store = Arc::new(crate::store_lock::StoreMutex::new(store()));
        let (id, _branch_id, _tip) = setup_terminal_branch(&store, &root, true);
        assert!(
            store
                .lock()
                .take_due_auto_submits(i64::MAX / 2, 0)
                .unwrap()
                .is_empty()
        );
        recover_pending_auto_submits_on_startup(&store);
        assert_eq!(
            store.lock().take_due_auto_submits(i64::MAX / 2, 0).unwrap(),
            vec![id]
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn startup_recovery_ignores_guardian_with_auto_submit_off() {
        let root = setup_auto_submit_repo("recovery-ignores-off");
        let store = Arc::new(crate::store_lock::StoreMutex::new(store()));
        setup_terminal_branch(&store, &root, false);
        recover_pending_auto_submits_on_startup(&store);
        assert!(
            store
                .lock()
                .take_due_auto_submits(i64::MAX / 2, 0)
                .unwrap()
                .is_empty()
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn auto_submit_does_not_create_or_reconcile_a_stack_after_approval() {
        let root = setup_auto_submit_repo("auto-submit-approved-read-only");
        let store = Arc::new(crate::store_lock::StoreMutex::new(store()));
        let (id, branch_id, _tip) = setup_terminal_branch(&store, &root, true);
        store
            .lock()
            .set_guardian_status(&id, GuardianStatus::InReview, None)
            .unwrap();
        store.lock().approve_guardian(&id).unwrap();

        run_auto_submit_pass(&store, &NoopRunner, &id);

        assert!(
            store
                .lock()
                .list_pull_requests_for_guardian(&id)
                .unwrap()
                .is_empty(),
            "a queued auto-submit must not create a PR after approval"
        );
        assert!(
            store
                .lock()
                .get_guardian(&id)
                .unwrap()
                .branches
                .iter()
                .find(|branch| branch.id == branch_id)
                .unwrap()
                .auto_submit_error
                .is_none(),
            "a terminal review must not receive new auto-submit bookkeeping"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn startup_recovery_does_not_requeue_an_approved_review() {
        let root = setup_auto_submit_repo("recovery-approved-read-only");
        let store = Arc::new(crate::store_lock::StoreMutex::new(store()));
        let (id, _branch_id, _tip) = setup_terminal_branch(&store, &root, true);
        store
            .lock()
            .set_guardian_status(&id, GuardianStatus::InReview, None)
            .unwrap();
        store.lock().approve_guardian(&id).unwrap();

        recover_pending_auto_submits_on_startup(&store);

        assert!(
            store
                .lock()
                .take_due_auto_submits(i64::MAX / 2, 0)
                .unwrap()
                .is_empty(),
            "startup recovery must not revive a terminal review's queue entry"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// RAL-389 follow-up: the sweep runs exactly one pass per review per
    /// tick, and that pass owns the outcome of every terminal branch it
    /// covered -- not just one of them. Before this, the sweep called a
    /// branch-scoped entry point once per terminal branch, and each call
    /// re-ran the whole review (a live-state GET per PR, the native-stack
    /// read, and a full `sync_open_pr_branches` git sweep against the shared
    /// repository root) only to stamp its own single branch. Here a
    /// whole-pass failure -- forge resolution, which no branch owns -- must
    /// reach all three branches from the one pass.
    #[test]
    fn one_auto_submit_pass_stamps_every_branch_it_covered() {
        let root = setup_auto_submit_repo("auto-submit-fan-out");
        for branch in ["feature/y", "feature/z"] {
            g(&root, &["checkout", "-b", branch, "feature/x"]);
            gwrite(&root, "more.txt", branch);
            g(&root, &["add", "."]);
            g(&root, &["commit", "--message", "more"]);
        }
        g(&root, &["checkout", "main"]);

        let store = Arc::new(crate::store_lock::StoreMutex::new(store()));
        // No git remote is configured, so `resolve_remote` fails fast (no
        // network attempted) and the whole pass errors out.
        let (gid, first_bid, _tip) = setup_terminal_branch(&store, &root, true);
        for branch in ["feature/y", "feature/z"] {
            store.lock().add_guardian_branch(&gid, branch).unwrap();
        }
        let branch_ids: Vec<String> = store
            .lock()
            .get_guardian(&gid)
            .unwrap()
            .branches
            .into_iter()
            .map(|b| b.id)
            .collect();
        assert_eq!(branch_ids.len(), 3);
        assert_eq!(branch_ids[0], first_bid);
        for (branch_id, branch) in branch_ids
            .iter()
            .zip(["feature/x", "feature/y", "feature/z"])
        {
            store
                .lock()
                .set_branch_review(&gid, branch_id, branch, "")
                .unwrap();
            store
                .lock()
                .set_branch_status(&gid, branch_id, crate::guardian::MergeStatus::Done, None)
                .unwrap();
        }

        run_auto_submit_pass(&store, &NoopRunner, &gid);

        let gv = store.lock().get_guardian(&gid).unwrap();
        let errors: Vec<Option<String>> = branch_ids
            .iter()
            .map(|id| {
                gv.branches
                    .iter()
                    .find(|b| &b.id == id)
                    .unwrap()
                    .auto_submit_error
                    .clone()
            })
            .collect();
        assert!(
            errors.iter().all(Option::is_some),
            "one pass must stamp every branch it covered, got {errors:?}"
        );
        assert!(
            errors.windows(2).all(|w| w[0] == w[1]),
            "every branch must carry the same whole-pass error, got {errors:?}"
        );
        // The branch statuses stay exactly where the merge worker left them:
        // a submission failure is never a merge-blocking one.
        assert_eq!(gv.status, "collecting");
        assert!(gv.branches.iter().all(|b| b.merge_status == "done"));

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn auto_submit_pass_is_noop_when_effective_option_is_off() {
        let root = setup_auto_submit_repo("auto-submit-off");
        let store = Arc::new(crate::store_lock::StoreMutex::new(store()));
        // effective_auto_submit_pr_stack defaults to false (no override, no
        // registered project default) -- left untouched deliberately.
        let (gid, bid, _tip) = setup_terminal_branch(&store, &root, false);

        run_auto_submit_pass(&store, &NoopRunner, &gid);

        let prs = store.lock().list_pull_requests_for_guardian(&gid).unwrap();
        assert!(prs.is_empty(), "auto-submit must be a no-op when disabled");
        let gv = store.lock().get_guardian(&gid).unwrap();
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
    fn auto_submit_pass_reconciles_even_when_a_branchs_pr_sha_is_current() {
        let root = setup_auto_submit_repo("auto-submit-covered");
        let store = Arc::new(crate::store_lock::StoreMutex::new(store()));
        let (gid, bid, tip) = setup_terminal_branch(&store, &root, true);

        // Simulate a PR already open for this exact branch state (whether
        // from a prior successful auto-submit, or a human running
        // `review pr submit` manually) -- `last_pushed_sha` matches the
        // branch's current tip exactly.
        store
            .lock()
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
        let existing_id = store.lock().list_pull_requests_for_guardian(&gid).unwrap()[0]
            .id
            .clone();
        store
            .lock()
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

        // This guardian has no git remote configured. A current PR SHA is not
        // enough to establish stack health, so auto-submit must attempt the
        // shared reconciliation path and report that it could not do so.
        run_auto_submit_pass(&store, &NoopRunner, &gid);

        let prs = store.lock().list_pull_requests_for_guardian(&gid).unwrap();
        assert_eq!(
            prs.len(),
            1,
            "must not create a second pr for the same branch state"
        );
        let gv = store.lock().get_guardian(&gid).unwrap();
        assert!(
            gv.branches
                .iter()
                .find(|b| b.id == bid)
                .unwrap()
                .auto_submit_error
                .is_some(),
            "auto-submit must reconcile an existing PR rather than treating its SHA as stack proof"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn auto_submit_pass_records_error_on_forge_resolution_failure_without_touching_branch_status() {
        let root = setup_auto_submit_repo("auto-submit-fail");
        let store = Arc::new(crate::store_lock::StoreMutex::new(store()));
        // No git remote configured -- `resolve_remote` fails fast (no
        // network attempted, no hang) with "could not read remote 'origin'".
        let (gid, bid, _tip) = setup_terminal_branch(&store, &root, true);

        run_auto_submit_pass(&store, &NoopRunner, &gid);

        // The failed forge resolution must never be surfaced as a merge-
        // blocking error: the branch's own terminal status (already
        // recorded by the caller before this side effect ever runs) and the
        // guardian's overall status are both untouched.
        let gv = store.lock().get_guardian(&gid).unwrap();
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
                .list_pull_requests_for_guardian(&gid)
                .unwrap()
                .is_empty(),
            "a failed submission must not leave a partial pr row behind"
        );

        // A current PR row alone cannot clear the failure marker: the
        // forge-side base and native stack membership still need a successful
        // shared reconciliation.
        store
            .lock()
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
        let pr_id = store.lock().list_pull_requests_for_guardian(&gid).unwrap()[0]
            .id
            .clone();
        let tip = g(&root, &["rev-parse", "feature/x"]).trim().to_string();
        store
            .lock()
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

        run_auto_submit_pass(&store, &NoopRunner, &gid);
        let gv = store.lock().get_guardian(&gid).unwrap();
        assert!(
            gv.branches
                .iter()
                .find(|b| b.id == bid)
                .unwrap()
                .auto_submit_error
                .is_some(),
            "the marker must remain until stack reconciliation succeeds"
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

    // -----------------------------------------------------------------
    // RAL-366: cached forge state
    // -----------------------------------------------------------------

    fn make_pr(s: &Store) -> (String, String) {
        let gid = s.create_guardian("demo", "main", "/repo").unwrap();
        let pr_id = s
            .create_pull_request(
                &gid,
                None,
                "github",
                "acme/w",
                "alias",
                "main",
                "T",
                "D",
                Some(1),
                None,
            )
            .unwrap();
        (gid, pr_id)
    }

    #[test]
    fn get_pr_forge_cache_is_none_before_any_poll() {
        let s = store();
        let (_gid, pr_id) = make_pr(&s);
        assert!(s.get_pr_forge_cache(&pr_id).unwrap().is_none());
        assert!(s.list_pr_forge_cache().unwrap().is_empty());
    }

    #[test]
    fn upsert_pr_forge_cache_coalesces_fields_a_partial_write_omits() {
        let s = store();
        let (_gid, pr_id) = make_pr(&s);

        s.upsert_pr_forge_cache(
            &pr_id,
            Some(Ok(DriftObservation {
                in_sync: true,
                pr_ahead: false,
                worktree_ahead: false,
                remote_sha: Some("abc"),
                local_sha: Some("abc"),
            })),
            Some(Ok(CommentsObservation {
                etag_conversation: Some("etag-1"),
                etag_review: None,
            })),
        )
        .unwrap();

        // A later pass that only refreshed the drift half (comments/etags
        // omitted as `None`) must not blank out the etag a prior pass wrote.
        s.upsert_pr_forge_cache(
            &pr_id,
            Some(Ok(DriftObservation {
                in_sync: false,
                pr_ahead: true,
                worktree_ahead: false,
                remote_sha: Some("def"),
                local_sha: Some("abc"),
            })),
            None,
        )
        .unwrap();

        let cache = s.get_pr_forge_cache(&pr_id).unwrap().unwrap();
        assert_eq!(cache.status, "ok");
        assert_eq!(cache.in_sync, Some(false));
        assert_eq!(cache.pr_ahead, Some(true));
        assert_eq!(cache.remote_sha.as_deref(), Some("def"));
        // Etag survived the second, etag-omitting write untouched.
        let etags = s.pr_forge_cache_etags(&pr_id).unwrap();
        assert_eq!(etags.0.as_deref(), Some("etag-1"));
    }

    #[test]
    fn upsert_pr_forge_cache_unknown_keeps_prior_drift_visible() {
        let s = store();
        let (_gid, pr_id) = make_pr(&s);
        s.upsert_pr_forge_cache(
            &pr_id,
            Some(Ok(DriftObservation {
                in_sync: true,
                pr_ahead: false,
                worktree_ahead: false,
                remote_sha: Some("abc"),
                local_sha: Some("abc"),
            })),
            None,
        )
        .unwrap();

        // Forge became unreachable this cycle -- status flips, but the
        // stale-but-known drift from the last successful pass must remain
        // readable rather than being wiped to `None`.
        s.upsert_pr_forge_cache(
            &pr_id,
            None,
            Some(Err("forge API 503: offline".to_string())),
        )
        .unwrap();

        let cache = s.get_pr_forge_cache(&pr_id).unwrap().unwrap();
        assert_eq!(cache.status, "unknown");
        assert_eq!(cache.last_error.as_deref(), Some("forge API 503: offline"));
        assert_eq!(
            cache.in_sync,
            Some(true),
            "stale drift must survive an unknown-status write"
        );
    }

    #[test]
    fn pr_forge_cache_unactioned_count_is_a_live_join_not_a_cached_integer() {
        let s = store();
        let (_gid, pr_id) = make_pr(&s);
        s.replace_pr_forge_comments(
            &pr_id,
            "conversation",
            &[
                crate::forge::PrComment {
                    external_id: "1".to_string(),
                    author: "alice".to_string(),
                    body: "first".to_string(),
                    created_at: "2024-01-01T00:00:00Z".to_string(),
                },
                crate::forge::PrComment {
                    external_id: "2".to_string(),
                    author: "bob".to_string(),
                    body: "second".to_string(),
                    created_at: "2024-01-02T00:00:00Z".to_string(),
                },
            ],
        )
        .unwrap();
        s.upsert_pr_forge_cache(
            &pr_id,
            None,
            Some(Ok(CommentsObservation {
                etag_conversation: None,
                etag_review: None,
            })),
        )
        .unwrap();

        let cache = s.get_pr_forge_cache(&pr_id).unwrap().unwrap();
        assert_eq!(cache.comment_count, 2);
        assert_eq!(cache.unactioned_count, 2);
        assert_eq!(cache.latest_comment_author.as_deref(), Some("bob"));
        assert_eq!(
            cache.latest_comment_at.as_deref(),
            Some("2024-01-02T00:00:00Z")
        );

        // Marking one actioned changes the *join result* on the next read --
        // never a second stored count that could drift out of step.
        s.try_claim_pr_comments(&pr_id, &["1".to_string()]).unwrap();
        let cache = s.get_pr_forge_cache(&pr_id).unwrap().unwrap();
        assert_eq!(cache.comment_count, 2);
        assert_eq!(cache.unactioned_count, 1);
    }

    #[test]
    fn replace_pr_forge_comments_wholesale_replaces_only_its_own_endpoint() {
        let s = store();
        let (_gid, pr_id) = make_pr(&s);
        let comment = |id: &str| crate::forge::PrComment {
            external_id: id.to_string(),
            author: "a".to_string(),
            body: "b".to_string(),
            created_at: "2024-01-01T00:00:00Z".to_string(),
        };
        s.replace_pr_forge_comments(&pr_id, "conversation", &[comment("1"), comment("2")])
            .unwrap();
        s.replace_pr_forge_comments(&pr_id, "review", &[comment("9")])
            .unwrap();
        s.upsert_pr_forge_cache(
            &pr_id,
            None,
            Some(Ok(CommentsObservation {
                etag_conversation: None,
                etag_review: None,
            })),
        )
        .unwrap();
        assert_eq!(
            s.get_pr_forge_cache(&pr_id).unwrap().unwrap().comment_count,
            3
        );

        // A fresh fetch of just `conversation` (e.g. a GitHub 200, not a
        // 304) replaces only that endpoint's rows -- `review`'s untouched.
        s.replace_pr_forge_comments(&pr_id, "conversation", &[comment("3")])
            .unwrap();
        assert_eq!(
            s.get_pr_forge_cache(&pr_id).unwrap().unwrap().comment_count,
            2,
            "conversation went from 2 rows to 1; review's 1 row is untouched"
        );
    }

    #[test]
    fn pr_forge_cache_is_cascade_deleted_with_its_pr_row() {
        let s = store();
        let (gid, pr_id) = make_pr(&s);
        s.upsert_pr_forge_cache(
            &pr_id,
            Some(Ok(DriftObservation {
                in_sync: true,
                pr_ahead: false,
                worktree_ahead: false,
                remote_sha: None,
                local_sha: None,
            })),
            None,
        )
        .unwrap();
        s.replace_pr_forge_comments(
            &pr_id,
            "conversation",
            &[crate::forge::PrComment {
                external_id: "1".to_string(),
                author: "a".to_string(),
                body: "b".to_string(),
                created_at: "2024-01-01T00:00:00Z".to_string(),
            }],
        )
        .unwrap();
        assert!(s.get_pr_forge_cache(&pr_id).unwrap().is_some());

        // No public hard-delete for a single PR row exists (only soft
        // `drop_pull_request`) -- deleting the owning guardian is the one
        // path that actually removes `guardian_pull_requests` rows, so it's
        // the cascade this test exercises.
        s.delete_guardian(&gid).unwrap();
        assert!(
            s.get_pr_forge_cache(&pr_id).unwrap().is_none(),
            "the cache row must not outlive the PR row it caches"
        );
    }

    /// Build a real git repo + remote + guardian + open, numbered PR
    /// (via [`review_fixture`]) suitable for exercising
    /// [`refresh_pr_forge_cache_for_guardian`] end to end. The fixture's
    /// remote is a plain local bare repo, not an actual forge host, so any
    /// forge *HTTP* client resolution is expected to fail here -- this
    /// fixture is for proving the batched-git-fetch/drift half of the
    /// poller works against a real repo, and that a PR degrades to
    /// `"unknown"` (not a panic, not silence) when no forge client resolves.
    fn cache_poll_fixture(tag: &str) -> (PathBuf, crate::store_lock::StoreHandle, String, String) {
        let (root, _remote_dir, store, pr_id, sha) = review_fixture(
            tag,
            |root| gwrite(root, "f.txt", "base\n"),
            |root| gwrite(root, "f.txt", "review\n"),
        );
        store
            .lock()
            .update_pull_request_ex(&pr_id, Some(Some(1)), None, None, None, None, None, None)
            .unwrap();
        (root, store, pr_id, sha)
    }

    #[test]
    fn refresh_pr_forge_cache_computes_drift_and_degrades_comments_to_unknown() {
        let (_root, store, pr_id, sha) = cache_poll_fixture("cache-drift");
        let guardian_id = store.lock().get_pull_request(&pr_id).unwrap().guardian_id;

        refresh_pr_forge_cache_for_guardian(&store, &guardian_id);

        let cache = store
            .lock()
            .get_pr_forge_cache(&pr_id)
            .unwrap()
            .expect("a poll pass must always write a cache row, even a degraded one");
        // The git side is real and does work even though the "forge" is just
        // a bare local repo: the pushed `pr-y` branch's tip is exactly the
        // review-branch commit `review_fixture` created.
        assert_eq!(cache.remote_sha.as_deref(), Some(sha.as_str()));
        assert_eq!(cache.local_sha.as_deref(), Some(sha.as_str()));
        assert_eq!(cache.in_sync, Some(true));
        // No forge client resolves from a plain local bare-repo remote --
        // must degrade visibly, not silently claim success.
        assert_eq!(cache.status, "unknown");
        assert!(cache.last_error.is_some());
    }

    #[test]
    fn refresh_pr_forge_cache_yields_drift_to_a_held_interactive_lock() {
        let (root, store, pr_id, _sha) = cache_poll_fixture("cache-lock-yield");
        let guardian_id = store.lock().get_pull_request(&pr_id).unwrap().guardian_id;

        // Pre-seed a known-good drift reading, as if a prior successful pass
        // (or an on-demand `sync-status` write-through) already ran.
        store
            .lock()
            .upsert_pr_forge_cache(
                &pr_id,
                Some(Ok(DriftObservation {
                    in_sync: true,
                    pr_ahead: false,
                    worktree_ahead: false,
                    remote_sha: Some("stale-remote"),
                    local_sha: Some("stale-local"),
                })),
                None,
            )
            .unwrap();

        // Simulate a concurrent interactive `sync-status` call already
        // holding this PR's fetch lock.
        let held = sync_fetch_lock(&root, &pr_id);
        let _guard = held.lock().unwrap();

        refresh_pr_forge_cache_for_guardian(&store, &guardian_id);

        let cache = store.lock().get_pr_forge_cache(&pr_id).unwrap().unwrap();
        assert_eq!(
            cache.remote_sha.as_deref(),
            Some("stale-remote"),
            "a PR whose lock is held by interactive work must keep its last-known drift, \
             not be blocked on or overwritten by the poller"
        );
        assert_eq!(cache.local_sha.as_deref(), Some("stale-local"));
        assert_eq!(cache.in_sync, Some(true));
    }

    #[test]
    fn poller_holds_no_pr_fetch_lock_across_the_comment_network_pass() {
        // Regression (RAL-423): the RAL-366 poller used to acquire every open
        // PR's drift-check lock with `try_lock` and then hold the whole set
        // across the loop that made the per-PR forge comment fetches (two
        // network round-trips each), so an interactive `sync-status` for any
        // one of those PRs could block behind *every other* PR's comment
        // calls -- minutes, not the milliseconds of that PR's own git fetch.
        // The fix splits the poller into a drift pass that owns the locks and
        // a comment pass that runs with none held.
        //
        // This test proves the split behaviorally: a local forge server
        // *holds* the comment request open once it arrives, the poller is
        // allowed to run until it is provably inside that comment pass, and
        // only then is a concurrent `compute_sync_status` for the same PR
        // timed. It must complete while the comment call is still held open.
        // Under the old shape the probe would block on this PR's fetch lock
        // until the comment pass finished (which only happens when this test
        // releases it below), so the 10s probe timeout fires.
        let (root, store, pr_id, _sha) = cache_poll_fixture("cache-comment-split");
        let guardian_id = store.lock().get_pull_request(&pr_id).unwrap().guardian_id;

        // The forge server: answers ordinary requests with an empty JSON
        // array (the GitHub conversation/review list shapes both parse as),
        // but holds the *first* comment-API request open until released --
        // signalling that it has been reached -- pinning the poller inside
        // its comment pass for as long as the test needs.
        let forge = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let forge_addr = forge.server_addr().to_string();
        let (comment_reached_tx, comment_reached_rx) = std::sync::mpsc::channel::<()>();
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let forge_arc = Arc::new(forge);
        std::thread::spawn(move || {
            let mut held = false;
            loop {
                let req = match forge_arc.recv_timeout(std::time::Duration::from_secs(20)) {
                    Ok(Some(r)) => r,
                    Ok(None) | Err(_) => break,
                };
                let url = req.url().to_string();
                let is_comment = url.contains("/issues/") || url.contains("/pulls/");
                if is_comment && !held {
                    held = true;
                    let _ = comment_reached_tx.send(());
                    let _ = release_rx.recv();
                }
                req.respond(tiny_http::Response::from_string("[]").with_status_code(200))
                    .unwrap();
            }
        });

        // The git remote must stay host-parseable (so a forge kind + repo
        // resolve for the comment client) but must *not* be reachable: point
        // it at 127.0.0.1 port 1 (tcpmux -- never bound here, never in the
        // OS's ephemeral range that other tests' `Server::http(127.0.0.1:0)`
        // listeners draw from, so no parallel test can steal it), so the
        // poller's `git fetch` fails in milliseconds with connection refused
        // instead of touching a real network. The comment API calls, by
        // contrast, go to `forge_addr` via the config's `[forge] api_base`,
        // where the server above holds them open.
        let remote_url = "http://127.0.0.1:1/acme/widget.git";
        g(&root, &["remote", "set-url", "origin", remote_url]);
        std::fs::write(
            root.join(".ralphus.toml"),
            format!(
                "[forge]\nkind = \"github\"\napi_base = \"http://{forge_addr}\"\ntoken_env = \"RALPHUS_TEST_FORGE_TOKEN\"\n"
            ),
        )
        .unwrap();

        // Run the poller to completion on a background thread.
        let store2 = Arc::clone(&store);
        let poller = std::thread::spawn(move || {
            refresh_pr_forge_cache_for_guardian(&store2, &guardian_id);
        });

        // Wait until the poller is *inside* its comment pass: the forge
        // server has received the comment request and is holding it.
        let reached = match comment_reached_rx.recv_timeout(std::time::Duration::from_secs(10)) {
            Ok(()) => true,
            Err(_) => false,
        };
        assert!(
            reached,
            "the poller never reached its comment pass; did the pre-pass drift \
             fetch to the unreachable remote hang?"
        );

        // Now time an interactive drift check for the same PR, on its own
        // thread so a regression that makes it block again fails with an
        // assertion instead of hanging the suite. The 8s budget is generous
        // but deliberate: on Windows, even a connection-refused loopback
        // connect costs ~2s in git before it reports (SYN retransmit), and a
        // loaded dev box can add more -- the *regression* backstop is the 10s
        // receiver below, which fires when the probe never completes at all
        // (i.e. it blocked on the poller's comment pass).
        let store3 = Arc::clone(&store);
        let (check_tx, check_rx) = std::sync::mpsc::channel::<u64>();
        std::thread::spawn(move || {
            let t0 = std::time::Instant::now();
            let _ = compute_sync_status(&store3, &pr_id).unwrap();
            let _ = check_tx.send(t0.elapsed().as_millis() as u64);
        });
        match check_rx.recv_timeout(std::time::Duration::from_secs(10)) {
            Ok(elapsed_ms) => assert!(
                elapsed_ms < 8000,
                "sync-status for the PR the poller is actively comment-polling took \
                 {elapsed_ms}ms -- it blocked on the comment pass. The poller must \
                 drop every PR fetch lock before its comment network fetches (RAL-423)."
            ),
            Err(_) => {
                let _ = release_tx.send(());
                panic!(
                    "compute_sync_status was still blocked 10s into the poller's comment \
                     pass. The poller must drop every PR fetch lock before its comment \
                     network fetches (RAL-423)."
                );
            }
        }

        // Let the comment pass finish so the poller thread terminates.
        let _ = release_tx.send(());
        poller.join().unwrap();
    }

    #[test]
    fn create_pull_request_ex_persists_the_forges_own_draft_state() {
        let s = store();
        let gid = s.create_guardian("demo", "main", "/repo").unwrap();
        let id = s
            .create_pull_request_ex(
                &gid,
                None,
                "github",
                "acme/widget",
                "feature/x",
                "main",
                "X",
                "",
                Some(9),
                Some("https://example.invalid/pr/9"),
                Some("stack-1"),
                true,
            )
            .unwrap();
        assert_eq!(s.get_pull_request(&id).unwrap().draft, Some(true));
    }

    #[test]
    fn create_pull_request_plain_wrapper_records_no_draft() {
        let s = store();
        let gid = s.create_guardian("demo", "main", "/repo").unwrap();
        let id = s
            .create_pull_request(
                &gid,
                None,
                "github",
                "acme/widget",
                "feature/x",
                "main",
                "X",
                "",
                Some(9),
                Some("https://example.invalid/pr/9"),
            )
            .unwrap();
        assert_eq!(s.get_pull_request(&id).unwrap().draft, Some(false));
    }

    #[test]
    fn set_pr_draft_records_and_overwrites_the_forge_observed_state() {
        let s = store();
        let gid = s.create_guardian("demo", "main", "/repo").unwrap();
        let id = s
            .create_pull_request(
                &gid,
                None,
                "github",
                "acme/widget",
                "feature/x",
                "main",
                "X",
                "",
                None,
                None,
            )
            .unwrap();
        // Simulate a legacy row recorded before the `draft` column existed
        // (the plain wrapper writes Some(false) today): force the column back
        // to NULL the way an old DB's migration would leave it, and confirm
        // it reads back as None -- the board treats that as not-draft.
        s.conn
            .execute(
                "UPDATE guardian_pull_requests SET draft=NULL WHERE id=?",
                params![id],
            )
            .unwrap();
        assert_eq!(s.get_pull_request(&id).unwrap().draft, None);
        s.set_pr_draft(&id, true).unwrap();
        assert_eq!(s.get_pull_request(&id).unwrap().draft, Some(true));
        s.set_pr_draft(&id, false).unwrap();
        assert_eq!(s.get_pull_request(&id).unwrap().draft, Some(false));
    }

    #[test]
    fn list_pull_requests_index_carries_draft_state() {
        let s = store();
        let gid = s.create_guardian("demo", "main", "/repo").unwrap();
        s.add_guardian_branch(&gid, "feature/x").unwrap();
        let bid = s.get_guardian(&gid).unwrap().branches[0].id.clone();
        let id = s
            .create_pull_request_ex(
                &gid,
                Some(&bid),
                "github",
                "acme/widget",
                "feature/x",
                "main",
                "X",
                "",
                Some(9),
                Some("https://example.invalid/pr/9"),
                None,
                true,
            )
            .unwrap();
        s.set_pr_ci_status(&id, "failing", None).unwrap();
        let row = s
            .list_pull_requests_index()
            .unwrap()
            .into_iter()
            .find(|r| r.id == id)
            .unwrap();
        assert_eq!(row.draft, Some(true));
        assert_eq!(row.ci_status, Some("failing".to_string()));
    }
}
