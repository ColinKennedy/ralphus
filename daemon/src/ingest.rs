//! `ralphus ingest` (RAL-702): attach existing branches, and any open pull
//! requests on them, to a review.
//!
//! One request builds a [`TaskFile`] in memory (no TOML text): each branch
//! becomes a task with one ordinary prompt cell, chained by `depends_on` in the
//! order given, and every cell works in a worktree on the branch's real tip
//! (existing-branch mode). A review is created (or `review_id` is extended)
//! with the branches in order and each cell linked to its branch.
//!
//! The same request runs in two modes: `dry_run` returns the plan (resolved
//! base and agent with their rules, the pull request found per branch and the
//! warnings) and changes nothing; otherwise the squad and review are created.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::forge::{ExistingPr, ForgeClient};
use crate::ingest_defaults::{self, ResolvedAgent, ResolvedBase};
use crate::store_lock::StoreHandle;
use crate::vcs::{GitOps, GitVcs};
use ralphus_core::schema::TaskFile;

/// What ralphus will do to an ingested branch from now on, shown before any
/// ingest is confirmed.
pub const FORCE_PUSH_WARNING: &str = "ralphus will rebase this branch onto its base as the \
review moves and force-push it (with lease) from now on. Anyone with a local copy of the branch \
will need `git pull --rebase` after each push.";

/// The line appended to every ingested cell's prompt.
const ALREADY_DONE_LINE: &str = "This branch may already contain some or all of the work \
described above. Check what is there first and only do what is still missing.";

/// A failed ingest: the HTTP status, a machine-readable code and a message that
/// names the way out.
#[derive(Debug)]
pub struct IngestError {
    pub status: u16,
    pub code: &'static str,
    pub message: String,
}

impl IngestError {
    fn new(status: u16, code: &'static str, message: impl Into<String>) -> Self {
        Self {
            status,
            code,
            message: message.into(),
        }
    }
}

/// The `POST /api/ingest` body.
#[derive(Debug, Deserialize)]
pub struct IngestRequest {
    /// The repository's root (or any path inside it).
    pub path: String,
    /// The branches to ingest, in stack order.
    pub branches: Vec<String>,
    /// An existing review to extend instead of creating one.
    #[serde(default)]
    pub review_id: Option<String>,
    /// The pull request number the caller expects on the (single) branch.
    #[serde(default)]
    pub pr: Option<i64>,
    #[serde(default)]
    pub base: Option<String>,
    /// Replaces the description derived from the pull request or commits.
    #[serde(default)]
    pub prompt: Option<String>,
    #[serde(default)]
    pub label: Option<String>,
    #[serde(default)]
    pub dry_run: bool,
    /// Umbrella for the three extras below.
    #[serde(default)]
    pub ralphus_extras: bool,
    /// Infer live actions from each branch's diff and pull request text.
    #[serde(default)]
    pub infer_live_actions: bool,
    /// Add recipe and inferred exhibit requests to the pull requests.
    #[serde(default)]
    pub extras_exhibits: bool,
    /// Choose the insights shown on adopted pull requests with a model.
    #[serde(default)]
    pub extras_insights: bool,
}

/// The extras an ingest request opts into, after the umbrella is expanded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct ExtrasSelection {
    pub live_actions: bool,
    pub exhibits: bool,
    pub insights: bool,
}

impl ExtrasSelection {
    fn from_request(req: &IngestRequest) -> Self {
        Self {
            live_actions: req.ralphus_extras || req.infer_live_actions,
            exhibits: req.ralphus_extras || req.extras_exhibits,
            insights: req.ralphus_extras || req.extras_insights,
        }
    }

    fn any(self) -> bool {
        self.live_actions || self.exhibits || self.insights
    }
}

/// The most diff text handed to live-action inference for one branch.
pub const INFERENCE_DIFF_CAP: usize = 30_000;

/// Appended to a diff that was cut at [`INFERENCE_DIFF_CAP`].
const DIFF_TRUNCATED_NOTE: &str = "\n[diff truncated]\n";

/// `branch`'s diff against `beneath` (the branch under it in the stack, or the
/// review base for the root), cut to `cap` characters on a character boundary
/// with a trailing note when anything was dropped. `None` when git cannot
/// produce the diff.
pub fn capped_branch_diff(
    root: &Path,
    beneath: &str,
    branch: &str,
    cap: usize,
) -> Option<(String, bool)> {
    let range = format!("{beneath}...{branch}");
    let diff = GitVcs.run(root, &["diff", &range]).ok()?;
    Some(cap_text(&diff, cap))
}

fn cap_text(text: &str, cap: usize) -> (String, bool) {
    match text.char_indices().nth(cap) {
        None => (text.to_string(), false),
        Some((end, _)) => (format!("{}{DIFF_TRUNCATED_NOTE}", &text[..end]), true),
    }
}

#[derive(Debug, Serialize)]
struct BranchPlan {
    branch: String,
    pr_number: Option<i64>,
    pr_url: Option<String>,
    /// Where the cell's description came from: `prompt`, `pull request`,
    /// `commits` or `branch name`.
    description_source: &'static str,
    /// Characters of diff live-action inference would read (opted in only).
    #[serde(skip_serializing_if = "Option::is_none")]
    inference_diff_chars: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    inference_diff_truncated: Option<bool>,
}

struct Prepared {
    root: PathBuf,
    project: Option<String>,
    base: ResolvedBase,
    agent: ResolvedAgent,
    prs: Vec<Option<ExistingPr>>,
    descriptions: Vec<(String, &'static str)>,
    warnings: Vec<String>,
}

/// Run an ingest request.
///
/// # Errors
/// A request that cannot be satisfied; the message says what to change.
pub fn ingest(store: &StoreHandle, req: &IngestRequest) -> Result<Value, IngestError> {
    let prepared = prepare(store, req)?;
    let extras = ExtrasSelection::from_request(req);
    let branches: Vec<BranchPlan> = req
        .branches
        .iter()
        .enumerate()
        .map(|(i, branch)| {
            let beneath = if i == 0 {
                prepared.base.branch.as_str()
            } else {
                req.branches[i - 1].as_str()
            };
            let diff = extras
                .live_actions
                .then(|| capped_branch_diff(&prepared.root, beneath, branch, INFERENCE_DIFF_CAP))
                .flatten();
            BranchPlan {
                branch: branch.clone(),
                pr_number: prepared.prs[i].as_ref().map(|p| p.number),
                pr_url: prepared.prs[i].as_ref().map(|p| p.url.clone()),
                description_source: prepared.descriptions[i].1,
                inference_diff_chars: diff.as_ref().map(|(d, _)| d.chars().count()),
                inference_diff_truncated: diff.map(|(_, t)| t),
            }
        })
        .collect();
    let mut out = json!({
        "dry_run": req.dry_run,
        "base": prepared.base.branch,
        "base_rule": prepared.base.rule,
        "agent": prepared.agent.agent,
        "model": prepared.agent.model,
        "agent_rule": prepared.agent.rule,
        "branches": branches,
        "warnings": prepared.warnings,
        "force_push_warning": FORCE_PUSH_WARNING,
        "review_id": req.review_id,
        "project": prepared.project,
        "extras": extras,
    });
    if extras.any() && prepared.prs.iter().all(Option::is_none) {
        out["warnings"].as_array_mut().expect("array").push(json!(
            "ralphus extras were requested but no branch has an open pull request"
        ));
    }
    if req.dry_run {
        return Ok(out);
    }
    let (squad_id, review_id) = create(store, req, &prepared)?;
    out["squad_id"] = json!(squad_id);
    out["review_id"] = json!(review_id);
    Ok(out)
}

fn prepare(store: &StoreHandle, req: &IngestRequest) -> Result<Prepared, IngestError> {
    validate_request(req)?;
    let (root, project) = resolve_root(store, &req.path)?;
    let review_base = match &req.review_id {
        Some(id) => {
            let review = store
                .lock()
                .get_guardian(id)
                .map_err(|e| IngestError::new(404, "review_not_found", e.to_string()))?;
            if !same_path(Path::new(&review.git_root), &root) {
                return Err(IngestError::new(
                    409,
                    "review_other_repository",
                    format!(
                        "review {id} belongs to {}, not {}. Run the command from that repository \
                         or pick a review of this one",
                        review.git_root,
                        root.display()
                    ),
                ));
            }
            Some(review.base_branch)
        }
        None => None,
    };
    for branch in &req.branches {
        crate::worktrees::check_existing_branch_available(&root, branch)
            .map_err(|e| IngestError::new(409, "branch_unavailable", e))?;
    }

    let config = store.lock().resolve_review_config(&root);
    let route_base = review_base
        .clone()
        .or_else(|| req.base.clone())
        .or_else(|| crate::worktrees::default_branch(&root).ok())
        .unwrap_or_else(|| "main".to_string());
    let forge_cfg = crate::config::resolve_forge(&root);
    let client = crate::pr::forge_client_for_pr(store, &root, &route_base, &forge_cfg, "", None);
    let mut warnings = Vec::new();
    let prs: Vec<Option<ExistingPr>> = req
        .branches
        .iter()
        .map(|branch| lookup_pr(client.as_ref(), branch, &mut warnings))
        .collect();
    check_pr_flag(req, &prs)?;

    let base = match review_base {
        Some(branch) => ResolvedBase {
            branch,
            rule: "the review's base branch".to_string(),
        },
        None => {
            let lookup = |branch: &str| -> Result<Option<String>, String> {
                Ok(req
                    .branches
                    .iter()
                    .position(|b| b == branch)
                    .and_then(|i| prs[i].as_ref())
                    .map(|p| p.base.clone()))
            };
            ingest_defaults::resolve_base(
                &root,
                &req.branches,
                req.base.as_deref(),
                &config,
                &lookup,
            )
            .map_err(|e| IngestError::new(409, "base_unresolved", e))?
        }
    };
    let agent = ingest_defaults::resolve_agent(&config)
        .map_err(|e| IngestError::new(409, "agent_unresolved", e))?;
    let descriptions = req
        .branches
        .iter()
        .enumerate()
        .map(|(i, branch)| describe(&root, branch, &base.branch, prs[i].as_ref(), req))
        .collect();
    Ok(Prepared {
        root,
        project,
        base,
        agent,
        prs,
        descriptions,
        warnings,
    })
}

fn validate_request(req: &IngestRequest) -> Result<(), IngestError> {
    if req.branches.is_empty() {
        return Err(IngestError::new(
            400,
            "bad_request",
            "name at least one branch to ingest",
        ));
    }
    let mut seen = std::collections::HashSet::new();
    for branch in &req.branches {
        if branch.trim().is_empty() {
            return Err(IngestError::new(
                400,
                "bad_request",
                "a branch name is empty",
            ));
        }
        if !seen.insert(branch.as_str()) {
            return Err(IngestError::new(
                400,
                "bad_request",
                format!("branch \"{branch}\" is named twice; list each branch once"),
            ));
        }
    }
    if req.pr.is_some() && req.branches.len() != 1 {
        return Err(IngestError::new(
            400,
            "bad_request",
            "--pr names one pull request, so ingest exactly one branch with it",
        ));
    }
    Ok(())
}

/// The repository root and, when it is registered, its project name.
fn resolve_root(store: &StoreHandle, path: &str) -> Result<(PathBuf, Option<String>), IngestError> {
    let given = Path::new(path);
    if !given.is_dir() {
        return Err(IngestError::new(
            400,
            "bad_request",
            format!("{path} is not a directory"),
        ));
    }
    let toplevel = GitVcs
        .run(given, &["rev-parse", "--show-toplevel"])
        .map_err(|_| {
            IngestError::new(
                400,
                "not_a_git_repository",
                format!("{path} is not inside a git repository; run ingest from the repository"),
            )
        })?;
    let root = PathBuf::from(toplevel.trim());
    let project = {
        let store = store.lock();
        store
            .project_name_for_path(&root.to_string_lossy())
            .and_then(|name| store.get_project(&name).ok().flatten())
    };
    match project {
        Some(p) => Ok((PathBuf::from(p.path), Some(p.name))),
        None => Ok((root, None)),
    }
}

fn same_path(a: &Path, b: &Path) -> bool {
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => a == b,
    }
}

fn lookup_pr(
    client: Option<&ForgeClient>,
    branch: &str,
    warnings: &mut Vec<String>,
) -> Option<ExistingPr> {
    let client = client?;
    match client.find_open_pull_request(branch) {
        Ok(found) => found,
        Err(e) => {
            warnings.push(format!(
                "could not look up an open pull request for \"{branch}\" ({e}); it is ingested \
                 without one"
            ));
            None
        }
    }
}

/// `--pr N` must name the open pull request whose head is the branch.
fn check_pr_flag(req: &IngestRequest, prs: &[Option<ExistingPr>]) -> Result<(), IngestError> {
    let Some(wanted) = req.pr else {
        return Ok(());
    };
    let branch = &req.branches[0];
    match prs[0].as_ref() {
        Some(found) if found.number == wanted => Ok(()),
        Some(found) => Err(IngestError::new(
            409,
            "pr_head_mismatch",
            format!(
                "pull request #{wanted} is not the open pull request for \"{branch}\" (that is \
                 #{}). Pass the matching number or drop --pr",
                found.number
            ),
        )),
        None => Err(IngestError::new(
            409,
            "pr_head_mismatch",
            format!(
                "no open pull request has \"{branch}\" as its head, so #{wanted} cannot be \
                 adopted for it. Check the number and that the branch is pushed"
            ),
        )),
    }
}

/// A cell's description: the `--prompt` override, else the pull request's title
/// and body, else the branch's commit subjects, else the branch name.
fn describe(
    root: &Path,
    branch: &str,
    base: &str,
    pr: Option<&ExistingPr>,
    req: &IngestRequest,
) -> (String, &'static str) {
    if let Some(prompt) = req
        .prompt
        .as_deref()
        .map(str::trim)
        .filter(|p| !p.is_empty())
    {
        return (prompt.to_string(), "prompt");
    }
    if let Some(pr) = pr {
        let title = pr.title.trim();
        let body = pr.description.trim();
        if !title.is_empty() {
            let text = if body.is_empty() {
                title.to_string()
            } else {
                format!("{title}\n\n{body}")
            };
            return (text, "pull request");
        }
    }
    let range = format!("{base}..{branch}");
    if let Ok(log) = GitVcs.run(root, &["log", "--format=%s", &range]) {
        let subjects: Vec<&str> = log
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .collect();
        if !subjects.is_empty() {
            let list = subjects
                .iter()
                .rev()
                .map(|s| format!("- {s}"))
                .collect::<Vec<_>>()
                .join("\n");
            return (
                format!("Complete the work on `{branch}`:\n{list}"),
                "commits",
            );
        }
    }
    (format!("Complete the work on `{branch}`."), "branch name")
}

fn task_name(branch: &str, taken: &[String]) -> String {
    let base: String = branch
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
                c
            } else {
                '-'
            }
        })
        .collect();
    let mut name = format!("ingest-{base}");
    let mut n = 2;
    while taken.contains(&name) {
        name = format!("ingest-{base}-{n}");
        n += 1;
    }
    name
}

/// The in-memory task file: one task per branch, chained in order.
fn build_task_file(
    req: &IngestRequest,
    prepared: &Prepared,
    cwds: &[String],
) -> Result<(TaskFile, Vec<String>), IngestError> {
    let mut names: Vec<String> = Vec::new();
    let mut tasks = Vec::new();
    for (i, branch) in req.branches.iter().enumerate() {
        let name = task_name(branch, &names);
        let prompt = format!("{}\n\n{ALREADY_DONE_LINE}", prepared.descriptions[i].0);
        let mut task = json!({
            "name": name,
            "agent": prepared.agent.agent,
            "cell": [{"cwd": cwds[i], "prompt": prompt}],
        });
        if let Some(model) = &prepared.agent.model {
            task["model"] = json!(model);
        }
        if let Some(project) = &prepared.project {
            task["project"] = json!(project);
        }
        if let Some(previous) = names.last() {
            task["depends_on"] = json!([previous]);
        }
        names.push(name);
        tasks.push(task);
    }
    let file = serde_json::from_value::<TaskFile>(json!({"task": tasks})).map_err(|e| {
        IngestError::new(500, "internal", format!("could not build the squad: {e}"))
    })?;
    Ok((file, names))
}

fn create(
    store: &StoreHandle,
    req: &IngestRequest,
    prepared: &Prepared,
) -> Result<(String, String), IngestError> {
    // A registered remote-backed project's base resolves against its remote,
    // so the commit baseline is not frozen against a stale local branch.
    let project = match &prepared.project {
        Some(name) => store
            .lock()
            .get_project(name)
            .map_err(|e| IngestError::new(500, "store_error", e.to_string()))?,
        None => None,
    };
    let base = match &project {
        Some(project) => crate::worktrees::resolve_registered_remote_upstream(
            &prepared.root,
            project,
            &prepared.base.branch,
        )
        .map_err(|e| IngestError::new(409, "worktree_failed", e))?,
        None => prepared.base.branch.clone(),
    };
    let mut cwds = Vec::new();
    for branch in &req.branches {
        let path =
            crate::worktrees::ensure_existing_branch_worktree(&prepared.root, branch, Some(&base))
                .map_err(|e| IngestError::new(409, "worktree_failed", e))?;
        cwds.push(path.to_string_lossy().into_owned());
    }
    let (file, _) = build_task_file(req, prepared, &cwds)?;
    let store_err = |e: &dyn std::fmt::Display| IngestError::new(500, "store_error", e.to_string());
    let mut store = store.lock();
    let squad_id = store
        .insert_squad(&file, req.label.as_deref(), false)
        .map_err(|e| store_err(&e))?;

    let root = prepared.root.to_string_lossy().into_owned();
    let (review_id, existing) = match &req.review_id {
        Some(id) => {
            let existing = store
                .guardian_branches(id)
                .map_err(|e| store_err(&e))?
                .into_iter()
                .map(|b| b.branch)
                .collect::<Vec<_>>();
            (id.clone(), existing)
        }
        None => {
            let name = format!("ingest-{}", req.branches[0].replace('/', "-"));
            let gid = store
                .create_guardian_keyed(
                    &name,
                    &prepared.base.branch,
                    &root,
                    Some(&squad_id),
                    None,
                    prepared.project.as_deref(),
                )
                .map_err(|e| store_err(&e))?;
            crate::reviews::apply_project_review_defaults(&store, &gid, &root)
                .map_err(|e| store_err(&e))?;
            if prepared.prs.iter().any(Option::is_some) {
                store
                    .set_guardian_auto_submit_pr_stack(&gid, Some(true))
                    .map_err(|e| store_err(&e))?;
            }
            (gid, Vec::new())
        }
    };
    for (i, branch) in req.branches.iter().enumerate() {
        if !existing.contains(branch) {
            store
                .add_guardian_branch_with_project(&review_id, branch, None)
                .map_err(|e| store_err(&e))?;
        }
        let task_idx = i64::try_from(i).unwrap_or(0);
        store
            .set_cell_review_branch(&squad_id, task_idx, 0, branch)
            .map_err(|e| store_err(&e))?;
        store
            .set_cell_review_guardian(&squad_id, task_idx, 0, &review_id)
            .map_err(|e| store_err(&e))?;
    }
    Ok((squad_id, review_id))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::Daemon;
    use crate::store::Store;
    use std::process::Command;
    use std::sync::atomic::{AtomicU32, Ordering};

    fn git(dir: &Path, args: &[&str]) {
        let out = Command::new("git")
            .current_dir(dir)
            .args(["-c", "user.name=t", "-c", "user.email=t@example.com"])
            .args(args)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    /// A repo on `main` with branches `feat-a` and `feat-b` (stacked), and the
    /// checkout left on `main`.
    fn repo() -> PathBuf {
        static N: AtomicU32 = AtomicU32::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let dir =
            ralphus_test_support::scratch(format!("ralphus-ingest-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        git(&dir, &["init", "--initial-branch", "main"]);
        git(&dir, &["commit", "--allow-empty", "-m", "root"]);
        for (branch, file) in [("feat-a", "a.txt"), ("feat-b", "b.txt")] {
            git(&dir, &["checkout", "-b", branch]);
            std::fs::write(dir.join(file), file).unwrap();
            git(&dir, &["add", file]);
            git(&dir, &["commit", "-m", &format!("add {file}")]);
        }
        git(&dir, &["checkout", "main"]);
        dir
    }

    fn handle() -> StoreHandle {
        Daemon::new(Store::open_in_memory().unwrap(), 4).store_handle()
    }

    fn request(root: &Path, branches: &[&str], dry_run: bool) -> IngestRequest {
        IngestRequest {
            path: root.to_string_lossy().into_owned(),
            branches: branches.iter().map(ToString::to_string).collect(),
            review_id: None,
            pr: None,
            base: None,
            prompt: None,
            label: None,
            dry_run,
            ralphus_extras: false,
            infer_live_actions: false,
            extras_exhibits: false,
            extras_insights: false,
        }
    }

    #[test]
    fn extras_umbrella_expands_and_flags_stand_alone() {
        let mut req = request(Path::new("."), &["a"], true);
        assert!(!ExtrasSelection::from_request(&req).any());
        req.extras_exhibits = true;
        let one = ExtrasSelection::from_request(&req);
        assert!(one.exhibits && !one.live_actions && !one.insights);
        req.ralphus_extras = true;
        let all = ExtrasSelection::from_request(&req);
        assert!(all.live_actions && all.exhibits && all.insights);
    }

    #[test]
    fn diff_is_capped_on_a_character_boundary() {
        assert_eq!(cap_text("abc", 3), ("abc".to_string(), false));
        let (cut, truncated) = cap_text("ééééé", 2);
        assert!(truncated);
        assert!(cut.starts_with("éé") && cut.ends_with("[diff truncated]\n"));
    }

    #[test]
    fn dry_run_reports_inference_diff_for_each_stacked_branch() {
        let root = repo();
        let mut req = request(&root, &["feat-a", "feat-b"], true);
        req.infer_live_actions = true;
        let out = ingest(&handle(), &req).unwrap();
        assert_eq!(out["extras"]["live_actions"], true);
        assert_eq!(out["extras"]["insights"], false);
        for b in out["branches"].as_array().unwrap() {
            assert!(b["inference_diff_chars"].as_u64().unwrap() > 0);
            assert_eq!(b["inference_diff_truncated"], false);
        }
    }

    #[test]
    fn dry_run_reports_base_agent_and_description_sources_without_creating_anything() {
        let root = repo();
        let store = handle();
        let out = ingest(&store, &request(&root, &["feat-a", "feat-b"], true)).unwrap();
        assert_eq!(out["base"], "main");
        assert_eq!(out["agent"], "claude-code");
        assert_eq!(out["branches"][0]["description_source"], "commits");
        assert!(out.get("squad_id").is_none());
        assert!(store.lock().list_guardians().unwrap().is_empty());
    }

    #[test]
    fn ingesting_two_branches_stacks_them_in_order_with_chained_tasks_in_an_unregistered_repo() {
        let root = repo();
        let store = handle();
        let out = ingest(&store, &request(&root, &["feat-a", "feat-b"], false)).unwrap();
        let review_id = out["review_id"].as_str().unwrap();
        let squad_id = out["squad_id"].as_str().unwrap();

        let store = store.lock();
        let branches: Vec<String> = store
            .guardian_branches(review_id)
            .unwrap()
            .into_iter()
            .map(|b| b.branch)
            .collect();
        assert_eq!(branches, ["feat-a", "feat-b"]);
        let review = store.get_guardian(review_id).unwrap();
        assert_eq!(review.base_branch, "main");
        let squad = store.get_squad(squad_id).unwrap();
        assert_eq!(squad.tasks.len(), 2);
        assert_eq!(squad.tasks[1].depends_on, [squad.tasks[0].name.clone()]);
    }

    #[test]
    fn a_registered_project_branch_is_checked_out_under_its_own_name_before_the_squad_exists() {
        let root = repo();
        let store = handle();
        store
            .lock()
            .register_project("ingest-proj", "", &root.to_string_lossy(), "git")
            .unwrap();
        let out = ingest(&store, &request(&root, &["feat-a"], false)).unwrap();
        assert_eq!(out["project"], "ingest-proj");

        let squad = store
            .lock()
            .get_squad(out["squad_id"].as_str().unwrap())
            .unwrap();
        let cwd = squad.tasks[0].cells[0].cwd.clone().unwrap();
        assert!(!cwd.contains("new-worktree"), "{cwd}");
        let head = GitVcs
            .run(Path::new(&cwd), &["rev-parse", "--abbrev-ref", "HEAD"])
            .unwrap();
        assert_eq!(head.trim(), "feat-a");
    }

    #[test]
    fn a_branch_checked_out_in_the_users_checkout_is_refused() {
        let root = repo();
        git(&root, &["checkout", "feat-a"]);
        let err = ingest(&handle(), &request(&root, &["feat-a"], true)).unwrap_err();
        assert_eq!(err.code, "branch_unavailable");
        assert!(err.message.contains("checked out"), "{}", err.message);
    }

    #[test]
    fn review_flag_extends_an_existing_review_and_keeps_its_base() {
        let root = repo();
        let store = handle();
        let first = ingest(&store, &request(&root, &["feat-a"], false)).unwrap();
        let review_id = first["review_id"].as_str().unwrap().to_string();
        let mut req = request(&root, &["feat-b"], false);
        req.review_id = Some(review_id.clone());
        let second = ingest(&store, &req).unwrap();
        assert_eq!(second["review_id"], review_id.as_str());
        assert_eq!(second["base_rule"], "the review's base branch");
        let names: Vec<String> = store
            .lock()
            .guardian_branches(&review_id)
            .unwrap()
            .into_iter()
            .map(|b| b.branch)
            .collect();
        assert_eq!(names, ["feat-a", "feat-b"]);
    }

    #[test]
    fn pr_flag_is_refused_when_no_open_pull_request_has_the_branch_as_head() {
        let root = repo();
        let mut req = request(&root, &["feat-a"], true);
        req.pr = Some(7);
        let err = ingest(&handle(), &req).unwrap_err();
        assert_eq!(err.code, "pr_head_mismatch");
    }

    #[test]
    fn prompt_override_replaces_the_derived_description() {
        let root = repo();
        let mut req = request(&root, &["feat-a"], true);
        req.prompt = Some("do the thing".to_string());
        let out = ingest(&handle(), &req).unwrap();
        assert_eq!(out["branches"][0]["description_source"], "prompt");
    }
}
