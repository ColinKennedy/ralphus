//! Guardian pull-request submission + bidirectional sync (RAL-117, RAL-190).
//!
//! Three layers:
//! - **Data model** (`impl Store` below): CRUD over the `guardian_pull_requests`
//!   / `guardian_pr_feedback_actioned` tables (schema already created in
//!   `store.rs`). Mirrors how `guardian.rs`/`cartographer.rs` add `Store`
//!   methods from their own module rather than `store.rs` itself.
//! - **Submission + feedback** ([`submit_pull_requests`], [`action_pr_feedback`]):
//!   pushes a review branch to the forge and opens a PR/MR, and pulls PR
//!   comments back into the owning review worktree. The feedback path
//!   deliberately delegates to [`crate::guardian_merge::run_feedback`] instead
//!   of re-implementing worktree-edit/commit/downstream-restack logic: that
//!   function already is "apply text feedback to one branch, then restack
//!   everything downstream of it", which is exactly what actioning a PR
//!   comment needs.
//! - **Bidirectional sync** (RAL-190 — [`compute_sync_status`], [`pull_pr_commits`],
//!   [`resync_pr_bases`]): drift detection between a PR's remote branch and its
//!   review worktree, pulling a reviewer's direct push to the PR branch back
//!   into the worktree (delegating to [`crate::guardian_merge::pull_pr_commits`]
//!   for the actual rebase/conflict-resolution/restack, the same pattern
//!   `action_pr_feedback` uses), and keeping a stacked PR's base in sync after
//!   its review is reordered. Every push site (`submit_pull_requests_inner`,
//!   `action_pr_feedback_inner`) is guarded by [`guard_against_clobber`] so a
//!   reviewer's commits on the PR branch are never silently force-pushed over.
//!
//! See `crate::forge` for the forge-auth model (RAL-117 Q8).

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use opentelemetry::trace::{SpanKind, Status};
use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};

use crate::guardian::GuardianView;
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
}

// ---------------------------------------------------------------------------
// Orchestration
// ---------------------------------------------------------------------------

/// One PR/MR to submit: either one stacked branch (`branch_id = Some(id)`)
/// or the combined worktree (`None`). Omitted `branch_alias`
/// defaults to the feature branch's own name (stacked) or a sanitised form of
/// the guardian's name (combined) — either way, never the internal
/// `guardian/guardian-<id>/...` ref. Omitted `title`/`description` are
/// synthesised from the branch's commits (see [`synthesize_pr_text`]).
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

fn default_combined_alias(guardian: &GuardianView) -> String {
    format!(
        "review/{}",
        sanitize_branch_name(&guardian.name).to_lowercase()
    )
}

/// Strip a `<remote>/` prefix from a base-branch string like `origin/main`, so
/// the forge PR base is a plain branch name (`main`).
fn strip_remote_prefix(branch: &str, remote: &str) -> String {
    branch
        .strip_prefix(&format!("{remote}/"))
        .unwrap_or(branch)
        .to_string()
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
/// branch's own commits (from its immediate predecessor's tip, or the base,
/// for position 0) when `position` is `Some`; the whole stack's commits (base
/// to the combined branch tip) when `None`.
fn commit_log_for(
    root: &Path,
    base_sha: &str,
    guardian: &GuardianView,
    position: Option<i64>,
) -> String {
    match position {
        Some(pos) => {
            let prev = guardian
                .branches
                .iter()
                .filter(|b| b.position < pos && b.enabled)
                .max_by_key(|b| b.position)
                .and_then(|b| b.review_branch.clone())
                .unwrap_or_else(|| base_sha.to_string());
            let Some(tip) = guardian
                .branches
                .iter()
                .find(|b| b.position == pos)
                .and_then(|b| b.review_branch.clone())
            else {
                return String::new();
            };
            git(root, &["log", "--format=%s", &format!("{prev}..{tip}")]).unwrap_or_default()
        }
        None => {
            let Some(tip) = guardian.review_branch.clone() else {
                return String::new();
            };
            git(root, &["log", "--format=%s", &format!("{base_sha}..{tip}")]).unwrap_or_default()
        }
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
    position: Option<i64>,
    template: Option<&str>,
    trace_context: Option<&str>,
) -> (String, String) {
    let root = PathBuf::from(&guardian.git_root);
    let fallback_title = position
        .and_then(|p| guardian.branches.iter().find(|b| b.position == p))
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

    let agent = guardian_merge::resolver_agent(guardian.resolver_agent.as_deref());
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
        run_id: "guardian".to_string(),
        task: "pr-description".to_string(),
        session_id: "pr-writer".to_string(),
        cwd: guardian.git_root.clone(),
        prompt: Some(prompt),
        command: None,
        agent,
        model,
        system_prompt: None,
        system_prompt_position: None,
        timeout_sec: None,
        budget_tokens: None,
        maximum_budget_usd: None,
        verify: false,
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
    position: Option<i64>,
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
    let forge_cfg = crate::config::resolve_forge(&root);
    let remote_name = forge_cfg
        .remote
        .clone()
        .unwrap_or_else(|| "origin".to_string());
    let base_branch_name = strip_remote_prefix(&guardian.base_branch, &remote_name);

    let prs = store
        .lock()
        .expect("poisoned")
        .list_pull_requests_for_guardian(id)
        .map_err(|e| e.to_string())?;
    // "The" PR for a branch, when it has more than one historical row
    // (resubmitted), is its most recently created still-open one.
    let mut by_branch: std::collections::HashMap<&str, &PullRequestView> =
        std::collections::HashMap::new();
    for pr in &prs {
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
    if by_branch.is_empty() {
        return Ok(0);
    }

    let client = crate::forge::resolve_remote(&root, &forge_cfg).ok();
    let mut ordered_branches: Vec<_> = guardian.branches.iter().filter(|b| b.enabled).collect();
    ordered_branches.sort_by_key(|b| b.position);

    let mut changed = 0usize;
    let mut prev_alias: Option<String> = None;
    for branch in &ordered_branches {
        let Some(pr) = by_branch.get(branch.id.as_str()) else {
            continue;
        };
        let new_base = prev_alias
            .clone()
            .unwrap_or_else(|| base_branch_name.clone());
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
        prev_alias = Some(pr.branch_alias.clone());
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
                run_id: None,
                guardian_id: Some(&sid),
                session_id: None,
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
/// itself a client/server boundary), started fresh since there is no run row
/// to have persisted an incoming request's trace context onto (unlike
/// `POST /api/runs`, see `server::route_with_trace`). That span's context is
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
    let forge_cfg = crate::config::resolve_forge(&root);
    let client = crate::forge::resolve_remote(&root, &forge_cfg)?;
    let remote_name = forge_cfg
        .remote
        .clone()
        .unwrap_or_else(|| "origin".to_string());
    let base_branch_name = strip_remote_prefix(&guardian.base_branch, &remote_name);

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
    let combined: Vec<&PrRequest> = requests.iter().filter(|r| r.branch_id.is_none()).collect();

    let mut created = Vec::new();
    let mut prev_alias: Option<String> = None;
    for req in stacked {
        let branch_id = req.branch_id.as_deref().unwrap_or("");
        let branch = guardian
            .branches
            .iter()
            .find(|b| b.id == branch_id)
            .ok_or_else(|| format!("no branch with id {branch_id}"))?;
        let position = branch.position;
        let review_ref = branch.review_branch.clone().ok_or_else(|| {
            format!("branch {branch_id} has no review ref yet; run the merge first")
        })?;
        let desired_alias =
            sanitize_branch_name(req.branch_alias.as_deref().unwrap_or(&branch.branch));
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
        guard_against_clobber(&root, &remote_name, &alias, &review_ref)?;
        push_ref(&root, &remote_name, &review_ref, &alias)?;
        let pushed_sha = git(&root, &["rev-parse", &review_ref])
            .map(|s| s.trim().to_string())
            .ok();
        let base = prev_alias
            .clone()
            .unwrap_or_else(|| base_branch_name.clone());
        let (title, description) = resolve_title_description(
            runner,
            &guardian,
            req,
            Some(position),
            &client,
            trace_context,
        );
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
                run_id: None,
                guardian_id: Some(id),
                session_id: None,
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
        created.push(
            store
                .lock()
                .expect("poisoned")
                .get_pull_request(&row_id)
                .map_err(|e| e.to_string())?,
        );
        prev_alias = Some(alias);
    }
    for req in combined {
        let tip = guardian.review_branch.clone().ok_or_else(|| {
            "guardian has no combined review branch yet; run the merge first".to_string()
        })?;
        let desired_alias = sanitize_branch_name(
            req.branch_alias
                .as_deref()
                .map(str::to_string)
                .unwrap_or_else(|| default_combined_alias(&guardian))
                .as_str(),
        );
        let alias = store
            .lock()
            .expect("poisoned")
            .resolve_unique_pr_alias(
                client.kind().as_str(),
                client.repo_label(),
                Some((id, "")),
                &desired_alias,
            )
            .map_err(|e| e.to_string())?;
        crate::rlog!(
            DEBUG,
            "ralphus [pr] review {id} pushing combined worktree alias={alias} remote={remote_name}"
        );
        guard_against_clobber(&root, &remote_name, &alias, &tip)?;
        push_ref(&root, &remote_name, &tip, &alias)?;
        let pushed_sha = git(&root, &["rev-parse", &tip])
            .map(|s| s.trim().to_string())
            .ok();
        let (title, description) =
            resolve_title_description(runner, &guardian, req, None, &client, trace_context);
        let created_pr =
            client.create_pull_request(&title, &description, &alias, &base_branch_name)?;
        let row_id = store
            .lock()
            .expect("poisoned")
            .create_pull_request(
                id,
                None,
                client.kind().as_str(),
                client.repo_label(),
                &alias,
                &base_branch_name,
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
                scope: Some("guardian"),
                run_id: None,
                guardian_id: Some(id),
                session_id: None,
                task: None,
                log_path: None,
                payload: serde_json::json!({
                    "position": null,
                    "alias": alias,
                    "pr_number": created_pr.number,
                    "pr_url": created_pr.url,
                }),
            });
        }
        created.push(
            store
                .lock()
                .expect("poisoned")
                .get_pull_request(&row_id)
                .map_err(|e| e.to_string())?,
        );
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
    let remote_name = forge_cfg
        .remote
        .clone()
        .unwrap_or_else(|| "origin".to_string());

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
    let remote_name = forge_cfg
        .remote
        .clone()
        .unwrap_or_else(|| "origin".to_string());

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
            run_id: None,
            guardian_id: Some(&pr.guardian_id),
            session_id: None,
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
                    run_id: None,
                    guardian_id: Some(&guardian_id),
                    session_id: None,
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
                    run_id: None,
                    guardian_id: Some(&guardian_id),
                    session_id: None,
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
/// Once applied, the resulting branch is pushed back to the remote under this
/// PR's recorded alias so the open PR/MR reflects the fix. Returns the number
/// of comments actioned (`0` when there was nothing new).
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
    let forge_cfg = crate::config::resolve_forge(&root);
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
    guardian_merge::run_feedback(store, runner, &pr.guardian_id, &branch_id, &feedback);

    for c in &fresh {
        let _ = store
            .lock()
            .expect("poisoned")
            .mark_pr_comment_actioned(pr_id, &c.external_id);
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
            .find(|b| b.position == position)
            .and_then(|b| b.review_branch.clone())
    } else {
        updated.review_branch.clone()
    }
    .ok_or_else(|| "no review ref to push after applying feedback".to_string())?;
    let remote_name = forge_cfg.remote.unwrap_or_else(|| "origin".to_string());
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
                    run_id: None,
                    guardian_id: Some(&sid),
                    session_id: None,
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
                    run_id: None,
                    guardian_id: Some(&sid),
                    session_id: None,
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
                    run_id: None,
                    guardian_id: Some(&guardian_id),
                    session_id: None,
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
                    run_id: None,
                    guardian_id: Some(&guardian_id),
                    session_id: None,
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
