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

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

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
    /// `"open"`, `"merged"`, `"closed"` — free-form, mirrors forge state.
    pub state: String,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
    /// The commit sha last pushed to `branch_alias` (RAL-190), used as the
    /// baseline for [`compute_sync_status`]'s drift detection. `None` for a
    /// row created before this column existed or whose push has not
    /// completed yet.
    pub last_pushed_sha: Option<String>,
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
        }
    }
}

const PR_COLUMNS: &str = "id, guardian_id, branch_id, forge, repo, branch_alias, base_ref, title, description, pr_number, pr_url, state, created_at_ms, updated_at_ms, last_pushed_sha";

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
        let id = self.next_id("guardian_pr_seq", "pr")?;
        let now = now_ms();
        self.conn.execute(
            "INSERT INTO guardian_pull_requests(
                id, guardian_id, branch_id, forge, repo, branch_alias, base_ref,
                title, description, pr_number, pr_url, state, created_at_ms, updated_at_ms
             ) VALUES(?,?,?,?,?,?,?,?,?,?,?,'open',?,?)",
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
                now
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
        self.update_pull_request_ex(id, pr_number, pr_url, branch_alias, state, None, None)
    }

    /// Full form of [`Self::update_pull_request`] that also allows updating
    /// `base_ref` (RAL-190: recomputed after a branch reorder) and
    /// `last_pushed_sha` (RAL-190: recorded after every push to the alias, the
    /// baseline [`compute_sync_status`] drifts against). Only fields passed as
    /// `Some` are changed; `last_pushed_sha` follows the same
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
        let n = self.conn.execute(
            "UPDATE guardian_pull_requests
             SET pr_number=?, pr_url=?, branch_alias=?, state=?, base_ref=?, last_pushed_sha=?, updated_at_ms=?
             WHERE id=?",
            params![
                new_pr_number,
                new_pr_url,
                new_alias,
                new_state,
                new_base_ref,
                new_last_pushed_sha,
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

/// Guess which git remote a guardian's `base_branch` refers to, when it's
/// written in `<remote>/<branch>` form (e.g. `gitlab/main`) and that leading
/// segment names a remote actually configured on this repo. Returns `None`
/// when `base_branch` has no `/` at all, or its leading segment isn't a real
/// remote (so it's read as a literal branch name that happens to contain a
/// slash, e.g. a personal `colin/main`).
fn remote_from_base_branch(root: &Path, base_branch: &str) -> Option<String> {
    let (candidate, _) = base_branch.split_once('/')?;
    git(root, &["remote", "get-url", candidate]).ok()?;
    Some(candidate.to_string())
}

/// The effective git remote name to resolve the forge against for one
/// guardian (RAL-190+): its own `base_branch`, via
/// [`remote_from_base_branch`], when it names a real configured remote;
/// otherwise the project's configured `[forge].remote`, else `"origin"`.
///
/// Without this, a review based on e.g. `gitlab/main` still silently
/// resolved/pushed/PATCHed against `origin` (and, on GitHub, GitHub's API):
/// [`strip_remote_prefix`] already handled an arbitrary `<remote>/` prefix
/// generically for the *branch name itself*, but forge *kind*/*remote*
/// detection was hardwired to the project config's remote regardless of
/// what the guardian's own base branch said -- a mismatch would send a
/// still-prefixed `"gitlab/main"` as the `base` field to GitHub's API
/// (rejected as invalid) instead of ever reaching GitLab at all.
fn effective_remote_name(
    root: &Path,
    base_branch: &str,
    forge_cfg: &crate::config::ForgeConfig,
) -> String {
    remote_from_base_branch(root, base_branch).unwrap_or_else(|| {
        forge_cfg
            .remote
            .clone()
            .unwrap_or_else(|| "origin".to_string())
    })
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
fn guard_against_clobber(
    root: &Path,
    remote: &str,
    alias: &str,
    local_ref: &str,
) -> std::result::Result<(), String> {
    if git(root, &["fetch", remote, alias]).is_err() {
        // No remote branch yet (or it's unreachable) -- nothing to clobber.
        return Ok(());
    }
    let Ok(remote_sha) = git(root, &["rev-parse", "FETCH_HEAD"]) else {
        return Ok(());
    };
    let remote_sha = remote_sha.trim();
    if git(
        root,
        &["merge-base", "--is-ancestor", remote_sha, local_ref],
    )
    .is_ok()
    {
        return Ok(());
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
        crate::rlog!(
            DEBUG,
            "ralphus [pr] synthesize pr text position={position:?} skipped: empty commit log"
        );
        return (fallback_title, fallback_description);
    }

    let agent = guardian_merge::resolver_agent(guardian.resolver_agent.as_deref(), &root);
    let model = guardian_merge::resolver_model(guardian.resolver_model.as_deref(), &agent);
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
        proof: false,
        trace_context: trace_context.map(str::to_string),
        resume_agent_session_id: None,
        env_overrides: std::collections::BTreeMap::new(),
        machine: None,
    };
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
                crate::rlog!(
                    DEBUG,
                    "ralphus [pr] synthesize pr text position={position:?} done: used llm suggestion"
                );
                return (title, description);
            }
        }
    }
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
    let guardian = store
        .lock()
        .expect("poisoned")
        .get_guardian(id)
        .map_err(|e| e.to_string())?;
    let root = PathBuf::from(&guardian.git_root);
    let mut forge_cfg = crate::config::resolve_forge(&root);
    let remote_name = effective_remote_name(&root, &guardian.base_branch, &forge_cfg);
    forge_cfg.remote = Some(remote_name.clone());
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

    let client = crate::forge::resolve_remote(&root, &forge_cfg).ok();
    let mut ordered_branches: Vec<_> = guardian.branches.iter().filter(|b| b.enabled).collect();
    ordered_branches.sort_by_key(|b| b.position);

    let mut changed = 0usize;
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
        if new_base != pr.base_ref {
            crate::rlog!(
                INFO,
                "ralphus [pr] review {id} resync base pr={} old={} new={new_base}",
                pr.id,
                pr.base_ref
            );
            let _ = store.lock().expect("poisoned").update_pull_request_ex(
                &pr.id,
                None,
                None,
                None,
                None,
                Some(&new_base),
                None,
            );
            changed += 1;
            if let (Some(c), Some(num)) = (&client, pr.pr_number) {
                if let Err(e) = c.update_pull_request_base(num, &new_base) {
                    crate::rlog!(
                        WARNING,
                        "ralphus [pr] review {id} resync base forge update failed pr={}: {e}",
                        pr.id
                    );
                }
            }
        }
    }
    Ok(changed)
}

/// Kick off [`resync_pr_bases`] in the background — called after a reorder,
/// so the reorder's own HTTP response is not held up by the forge network
/// calls this makes.
pub fn start_resync_pr_bases(store: Arc<Mutex<Store>>, id: &str) {
    let sid = id.to_string();
    std::thread::spawn(move || match resync_pr_bases(&store, &sid) {
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
            });
        }
        Ok(_) => {}
        Err(e) => {
            crate::rlog!(ERROR, "ralphus [pr] review {sid} resync bases failed: {e}");
        }
    });
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
/// Runs the whole operation under one `pr.submit_pull_requests` OpenTelemetry
/// span (RAL-96, `SpanKind::Internal` — this is background daemon work, not
/// itself a client/server boundary), started fresh since there is no squad row
/// to have persisted an incoming request's trace context onto (unlike
/// `POST /api/squads`, see `server::route_with_trace`). That span's context is
/// forwarded into the PR-text-synthesis `RunnerSpec` so its
/// `runner.subprocess` span becomes a child of this one instead of an
/// unlinked trace.
pub fn submit_pull_requests(
    store: &Arc<Mutex<Store>>,
    runner: &dyn Runner,
    id: &str,
    requests: Vec<PrRequest>,
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
    let result = submit_pull_requests_inner(store, runner, id, requests, trace_context.as_deref());
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
) -> std::result::Result<PullRequestView, String> {
    let branch_id = branch.id.as_str();
    let position = branch.position;
    let review_ref = branch
        .review_branch
        .clone()
        .ok_or_else(|| format!("branch {branch_id} has no review ref yet; run the merge first"))?;
    // RAL-244: an explicit `branch_alias` always wins verbatim; otherwise the
    // default is templated from the convention rather than reusing
    // `branch.branch` bare -- an identically-named remote branch would mask
    // the fact that its content is the review's (possibly rebased/
    // conflict-resolved/squashed) output, not the task branch's own commits.
    let desired_alias = sanitize_branch_name(&match &req.branch_alias {
        Some(alias) => alias.clone(),
        None => apply_pr_branch_convention(pr_branch_convention, &branch.branch),
    });
    // RAL-190: suffix (`-002`, ...) if another PR already claims this
    // alias, so two branches/reviews that would otherwise default to the
    // same remote branch name don't collide.
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
    guard_against_clobber(root, remote_name, &alias, &review_ref)?;
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
    let (title, description) =
        resolve_title_description(runner, guardian, req, position, client, trace_context);
    let created_pr = client.create_pull_request(&title, &description, &alias, &base)?;
    let row_id = store
        .lock()
        .expect("poisoned")
        .create_pull_request(
            id,
            Some(branch_id),
            client.kind().as_str(),
            client.repo_label(),
            &alias,
            &base,
            &title,
            &description,
            Some(created_pr.number),
            Some(&created_pr.url),
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
                    );
                    false
                }
                Ok(_) => true,
                Err(e) => {
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
        });
    }

    if client.kind() != crate::forge::ForgeKind::GitHub {
        return Ok(created);
    }

    let all_with_prs: Vec<(String, i64, i64)> = ordered_enabled
        .iter()
        .filter_map(|branch| {
            let number = already_open
                .get(branch.id.as_str())
                .and_then(|pr| pr.pr_number)
                .or_else(|| {
                    created
                        .iter()
                        .find(|p| p.branch_id.as_deref() == Some(branch.id.as_str()))
                        .and_then(|p| p.pr_number)
                })?;
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

fn submit_pull_requests_inner(
    store: &Arc<Mutex<Store>>,
    runner: &dyn Runner,
    id: &str,
    requests: Vec<PrRequest>,
    trace_context: Option<&str>,
) -> std::result::Result<Vec<PullRequestView>, String> {
    let guardian = store
        .lock()
        .expect("poisoned")
        .get_guardian(id)
        .map_err(|e| e.to_string())?;
    let root = PathBuf::from(&guardian.git_root);
    let mut forge_cfg = crate::config::resolve_forge(&root);
    let remote_name = effective_remote_name(&root, &guardian.base_branch, &forge_cfg);
    forge_cfg.remote = Some(remote_name.clone());
    let client = crate::forge::resolve_remote(&root, &forge_cfg)?;
    let base_branch_name = strip_remote_prefix(&guardian.base_branch, &remote_name);
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
        )?;
        created.extend(stack_prs);
    }

    Ok(created)
}

// ---------------------------------------------------------------------------
// Bidirectional sync (RAL-190)
// ---------------------------------------------------------------------------

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
    let remote_name = effective_remote_name(&root, &guardian.base_branch, &forge_cfg);

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
    let remote_sha = git(&root, &["fetch", &remote_name, &pr.branch_alias])
        .ok()
        .and_then(|_| git(&root, &["rev-parse", "FETCH_HEAD"]).ok())
        .map(|s| s.trim().to_string());

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
    let remote_name = effective_remote_name(&root, &guardian.base_branch, &forge_cfg);

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

    crate::rlog!(INFO, "ralphus [pr] pr {pr_id} actioning feedback");
    let result = action_pr_feedback_inner(store, runner, pr_id);
    match &result {
        Ok(n) => {
            span.set_status(Status::Ok);
            span.set_attribute("pr.comments_actioned", *n as i64);
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
    let mut forge_cfg = crate::config::resolve_forge(&root);
    let remote_name = effective_remote_name(&root, &guardian.base_branch, &forge_cfg);
    forge_cfg.remote = Some(remote_name.clone());
    let client = crate::forge::resolve_remote(&root, &forge_cfg)?;
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
    crate::rlog!(
        DEBUG,
        "ralphus [pr] pr {pr_id} comments fresh={} already_actioned={}",
        fresh.len(),
        already.len()
    );
    if fresh.is_empty() {
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

    crate::rlog!(
        DEBUG,
        "ralphus [pr] pr {pr_id} feedback applying {} comment(s) to guardian={} position={position}",
        fresh.len(),
        pr.guardian_id
    );
    let outcome =
        guardian_merge::run_feedback(store, runner, &pr.guardian_id, &branch_id, &feedback);

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
        guard_against_clobber(&root, &remote_name, &pr.branch_alias, &local_ref)?;
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
        match submit_pull_requests(&store, runner.as_ref(), &sid, requests) {
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
                });
            }
        },
    );
    reply(202, &serde_json::json!({"status": "actioning_feedback"}))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Store;
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
        g(&root, &["init", "-b", "main"]);
        gwrite(&root, "base.txt", "base\n");
        g(&root, &["add", "."]);
        g(&root, &["commit", "-m", "base"]);

        let remote_dir = tmp_dir("guard-remote");
        g(&remote_dir, &["init", "--bare"]);
        let remote = remote_dir.to_str().unwrap();

        // No remote branch yet -- safe.
        assert!(guard_against_clobber(&root, remote, "pr-x", "main").is_ok());

        g(&root, &["push", remote, "main:refs/heads/pr-x"]);
        // Remote now matches local exactly -- still safe (ancestor of itself).
        assert!(guard_against_clobber(&root, remote, "pr-x", "main").is_ok());

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
        g(&clone_dir, &["commit", "-m", "reviewer fix"]);
        g(&clone_dir, &["push", "origin", "pr-x"]);

        // Local `main` no longer contains the remote's unique commit -- blocked.
        let err = guard_against_clobber(&root, remote, "pr-x", "main").unwrap_err();
        assert!(err.contains("pr-x"), "{err}");

        // Once local has pulled that commit in, it's a safe superset again.
        g(&root, &["fetch", remote, "pr-x"]);
        g(&root, &["merge", "--ff-only", "FETCH_HEAD"]);
        assert!(guard_against_clobber(&root, remote, "pr-x", "main").is_ok());

        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&remote_dir);
        let _ = std::fs::remove_dir_all(&clone_dir);
    }

    /// Common fixture for the `compute_sync_status` tests: a repo with a
    /// `review-branch` pushed to a bare remote as `pr-y`, and a guardian/PR
    /// row pointing at it.
    fn sync_status_fixture() -> (PathBuf, PathBuf, Arc<Mutex<Store>>, String) {
        let root = tmp_dir("sync-root");
        g(&root, &["init", "-b", "main"]);
        gwrite(&root, "base.txt", "base\n");
        g(&root, &["add", "."]);
        g(&root, &["commit", "-m", "base"]);
        g(&root, &["checkout", "-b", "review-branch"]);
        gwrite(&root, "feat.txt", "feat\n");
        g(&root, &["add", "."]);
        g(&root, &["commit", "-m", "feat"]);
        g(&root, &["checkout", "main"]);

        let remote_dir = tmp_dir("sync-remote");
        g(&remote_dir, &["init", "--bare"]);
        let remote = remote_dir.to_str().unwrap();
        g(&root, &["remote", "add", "origin", remote]);
        g(&root, &["push", "origin", "review-branch:refs/heads/pr-y"]);

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
        (root, remote_dir, Arc::new(Mutex::new(s)), pr_id)
    }

    /// [`sync_status_fixture`], plus recording `last_pushed_sha` the way a
    /// real push through this daemon would -- most RAL-190 rebase-drift
    /// tests need a real fork point on record, not just matching SHAs.
    fn synced_fixture() -> (PathBuf, PathBuf, Arc<Mutex<Store>>, String) {
        let (root, remote_dir, store, pr_id) = sync_status_fixture();
        let sha = g(&root, &["rev-parse", "review-branch"]).trim().to_string();
        store
            .lock()
            .unwrap()
            .update_pull_request_ex(&pr_id, None, None, None, None, None, Some(Some(&sha)))
            .unwrap();
        (root, remote_dir, store, pr_id)
    }

    /// Like [`synced_fixture`], but `review-branch`'s one commit edits the
    /// same line of a shared file that a base-advance commit can also touch
    /// -- so a caller can force a real rebase conflict on demand, on either
    /// the worktree or the PR side, via [`rebase_with_conflict`].
    fn conflict_fixture() -> (PathBuf, PathBuf, Arc<Mutex<Store>>, String) {
        let root = tmp_dir("sync-conflict-root");
        g(&root, &["init", "-b", "main"]);
        gwrite(&root, "shared.txt", "line1\nline2\nline3\n");
        g(&root, &["add", "."]);
        g(&root, &["commit", "-m", "base"]);
        g(&root, &["checkout", "-b", "review-branch"]);
        gwrite(&root, "shared.txt", "line1\nline2-review\nline3\n");
        g(&root, &["add", "."]);
        g(&root, &["commit", "-m", "feat"]);
        g(&root, &["checkout", "main"]);

        let remote_dir = tmp_dir("sync-conflict-remote");
        g(&remote_dir, &["init", "--bare"]);
        let remote = remote_dir.to_str().unwrap();
        g(&root, &["remote", "add", "origin", remote]);
        g(&root, &["push", "origin", "review-branch:refs/heads/pr-y"]);

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
        let sha = g(&root, &["rev-parse", "review-branch"]).trim().to_string();
        let store = Arc::new(Mutex::new(s));
        store
            .lock()
            .unwrap()
            .update_pull_request_ex(&pr_id, None, None, None, None, None, Some(Some(&sha)))
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
        g(dir, &["branch", &new_base, &format!("{target_branch}~1")]);
        g(dir, &["checkout", &new_base]);
        gwrite(dir, "shared.txt", "line1\nline2-newbase\nline3\n");
        g(dir, &["add", "."]);
        g(dir, &["commit", "-m", "advance base (conflicting)"]);
        g(dir, &["checkout", target_branch]);

        let run_rebase = |d: &Path| -> std::process::Output {
            Command::new("git")
                .args(["rebase", &new_base])
                .current_dir(d)
                .output()
                .unwrap()
        };

        if use_rerere {
            g(dir, &["config", "rerere.enabled", "true"]);
            g(dir, &["config", "rerere.autoupdate", "true"]);
            let pre_rebase_sha = g(dir, &["rev-parse", target_branch]).trim().to_string();

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
            g(dir, &["checkout", "-B", target_branch, &pre_rebase_sha]);
            let out2 = run_rebase(dir);
            assert!(!out2.status.success(), "expected the conflict to recur");
            let out3 = Command::new("git")
                .args(["rebase", "--continue"])
                .current_dir(dir)
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

        g(dir, &["branch", "-D", &new_base]);
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
        g(&clone_dir, &["commit", "-m", "reviewer fix"]);
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
        g(&root, &["commit", "-m", "more work"]);
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
        g(&root, &["commit", "-m", "advance base"]);
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
        g(&clone_dir, &["commit", "-m", "advance base externally"]);
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
        g(&root, &["commit", "-m", "local work"]);
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
        g(&clone_dir, &["commit", "-m", "reviewer work"]);
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
        )
        .unwrap();
        let pr = s.get_pull_request(&id).unwrap();
        assert_eq!(pr.base_ref, "other-base");
        assert_eq!(pr.last_pushed_sha.as_deref(), Some("deadbeef"));
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
    fn remote_from_base_branch_detects_a_real_configured_remote() {
        let root = tmp_dir("remote-detect");
        g(&root, &["init", "-b", "main"]);
        g(
            &root,
            &["remote", "add", "origin", "https://example.com/a/b.git"],
        );
        g(
            &root,
            &["remote", "add", "gitlab", "https://gitlab.com/a/b.git"],
        );

        assert_eq!(
            remote_from_base_branch(&root, "gitlab/main"),
            Some("gitlab".to_string())
        );
        assert_eq!(
            remote_from_base_branch(&root, "origin/main"),
            Some("origin".to_string())
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn remote_from_base_branch_is_none_for_a_plain_branch_or_unknown_remote() {
        let root = tmp_dir("remote-detect-none");
        g(&root, &["init", "-b", "main"]);
        g(
            &root,
            &["remote", "add", "origin", "https://example.com/a/b.git"],
        );

        assert_eq!(remote_from_base_branch(&root, "main"), None);
        assert_eq!(
            remote_from_base_branch(&root, "colin/feature"),
            None,
            "a personal branch name that happens to contain a slash must not be mistaken for a remote"
        );
        assert_eq!(
            remote_from_base_branch(&root, "features/foo/bar"),
            None,
            "a slash-namespaced branch name (e.g. features/foo/bar) whose leading segment isn't a \
             configured remote must not be mistaken for one either"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn effective_remote_name_falls_back_to_default_for_a_slash_namespaced_branch_name() {
        let root = tmp_dir("effective-remote-namespaced");
        g(&root, &["init", "-b", "main"]);
        g(
            &root,
            &["remote", "add", "origin", "https://example.com/a/b.git"],
        );

        let cfg = crate::config::ForgeConfig::default();
        assert_eq!(
            effective_remote_name(&root, "features/foo/bar", &cfg),
            "origin",
            "the leading segment ('features') isn't a real remote, so this must fall back to the \
             config/origin default instead of misreading it as a remote name"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn effective_remote_name_prefers_base_branchs_own_remote_over_the_config_default() {
        let root = tmp_dir("effective-remote");
        g(&root, &["init", "-b", "main"]);
        g(
            &root,
            &["remote", "add", "origin", "https://example.com/a/b.git"],
        );
        g(
            &root,
            &["remote", "add", "gitlab", "https://gitlab.com/a/b.git"],
        );

        let cfg = crate::config::ForgeConfig::default();
        assert_eq!(
            effective_remote_name(&root, "gitlab/main", &cfg),
            "gitlab",
            "the base branch's own remote prefix must win over the project config default"
        );
        assert_eq!(
            effective_remote_name(&root, "main", &cfg),
            "origin",
            "falls back to the config/origin default when base_branch names no remote"
        );
        let _ = std::fs::remove_dir_all(&root);
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
}
