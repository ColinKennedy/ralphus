//! Guardian pull-request submission + feedback loop (RAL-117).
//!
//! Two layers:
//! - **Data model** (`impl Store` below): CRUD over the `guardian_pull_requests`
//!   / `guardian_pr_feedback_actioned` tables (schema already created in
//!   `store.rs`). Mirrors how `guardian.rs`/`cartographer.rs` add `Store`
//!   methods from their own module rather than `store.rs` itself.
//! - **Orchestration** ([`submit_pull_requests`], [`action_pr_feedback`]):
//!   pushes a review branch to the forge and opens a PR/MR, and pulls PR
//!   comments back into the owning review worktree. The feedback path
//!   deliberately delegates to [`crate::guardian_merge::run_feedback`] instead
//!   of re-implementing worktree-edit/commit/downstream-restack logic: that
//!   function already is "apply text feedback to one branch, then restack
//!   everything downstream of it", which is exactly what actioning a PR
//!   comment needs.
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
        }
    }
}

const PR_COLUMNS: &str = "id, guardian_id, branch_id, forge, repo, branch_alias, base_ref, title, description, pr_number, pr_url, state, created_at_ms, updated_at_ms";

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
    pub fn update_pull_request(
        &self,
        id: &str,
        pr_number: Option<Option<i64>>,
        pr_url: Option<Option<&str>>,
        branch_alias: Option<&str>,
        state: Option<&str>,
    ) -> Result<()> {
        let existing = self.get_pull_request(id)?;
        let new_pr_number = pr_number.unwrap_or(existing.pr_number);
        let new_pr_url = pr_url
            .map(|o| o.map(str::to_string))
            .unwrap_or(existing.pr_url);
        let new_alias = branch_alias.unwrap_or(&existing.branch_alias);
        let new_state = state.unwrap_or(&existing.state);
        let n = self.conn.execute(
            "UPDATE guardian_pull_requests
             SET pr_number=?, pr_url=?, branch_alias=?, state=?, updated_at_ms=?
             WHERE id=?",
            params![
                new_pr_number,
                new_pr_url,
                new_alias,
                new_state,
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
/// mechanism `guardian_merge::generate_summary` uses for the change summary.
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
        let alias = sanitize_branch_name(req.branch_alias.as_deref().unwrap_or(&branch.branch));
        crate::rlog!(
            DEBUG,
            "ralphus [pr] review {id} pushing branch id={branch_id} alias={alias} remote={remote_name}"
        );
        push_ref(&root, &remote_name, &review_ref, &alias)?;
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
        let alias = sanitize_branch_name(
            req.branch_alias
                .as_deref()
                .map(str::to_string)
                .unwrap_or_else(|| default_combined_alias(&guardian))
                .as_str(),
        );
        crate::rlog!(
            DEBUG,
            "ralphus [pr] review {id} pushing combined worktree alias={alias} remote={remote_name}"
        );
        push_ref(&root, &remote_name, &tip, &alias)?;
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
    crate::rlog!(
        DEBUG,
        "ralphus [pr] pr {pr_id} pushing updated branch back alias={} remote={remote_name}",
        pr.branch_alias
    );
    push_ref(&root, &remote_name, &local_ref, &pr.branch_alias)?;

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

    fn store() -> Store {
        Store::open_in_memory().unwrap()
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
