//! Derive per-project Guardian reviews from top-level `[[review]]` blocks.
//!
//! At submit time, cells that opt in to a review (via `review = "<<review:<id>>>"`
//! on the cell) are grouped by the *project* their worktree belongs to (its shared
//! git dir, so linked worktrees of one repo collapse together). Each project
//! becomes one guardian, whose branch list is the cells' worktree branches in
//! topological order. The upstream branch is always the worktree's git
//! upstream tracking branch — a hard error if the worktree has none.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};

use opentelemetry::Context;

use ralphus_core::schema::{ReviewActionDef, ReviewDef, TaskFile, review_link_key};

use crate::guardian::{CheckInput, GuardianCheck};
use crate::plan;
use crate::store::{CellRow, ProjectView, Store, TaskRow};
use crate::vcs::{GitOps, GitVcs};
use crate::workspace::Workspace;

/// A submit-time review preflight / derivation failure (surfaced to the client).
#[derive(Debug)]
pub struct ReviewError {
    /// Human-readable explanation.
    pub message: String,
}

impl ReviewError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl std::fmt::Display for ReviewError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

/// Run `git args` in `dir`, returning trimmed stdout.
///
/// Thin wrapper over [`crate::vcs::GitOps::run`] — the actual `git`
/// subprocess spawn lives in `vcs.rs`, not here (RAL-213). Trims the result
/// since every caller here treats the output as a single ref/path/branch
/// name, where a trailing newline would corrupt a subsequent `PathBuf` join
/// or ref comparison.
fn git(dir: &Path, args: &[&str]) -> std::result::Result<String, String> {
    GitVcs.run(dir, args).map(|s| s.trim().to_string())
}

/// The project root for a cell `cwd`, or `None` when it is not inside a git
/// worktree. Used by the API to show the project (shared root) as a field
/// distinct from the worktree (the cell's own working copy) — see CCTL-148.
#[must_use]
pub fn project_root_of(cwd: &str) -> Option<String> {
    worktree_project(Path::new(cwd))
        .ok()
        .map(|p| p.to_string_lossy().into_owned())
}

/// The project a worktree belongs to: the parent of its shared git dir, so that
/// linked worktrees of one repository resolve to the same project root.
fn worktree_project(cwd: &Path) -> std::result::Result<PathBuf, String> {
    let common = git(
        cwd,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    )?;
    let common = PathBuf::from(common);
    // `<root>/.git` -> `<root>`; for a linked worktree the common dir is still the
    // main repo's `.git`, so both map to the same root.
    Ok(common.parent().map_or(common.clone(), Path::to_path_buf))
}

/// The literal top-level directory of the git worktree containing `cwd` (RAL-159).
/// Unlike [`worktree_project`] — which collapses every linked worktree of a
/// repository to one shared project root, so different branches of the same
/// repo match each other — this resolves the specific worktree: a cell at a
/// nested subfolder `cwd` still matches another cell at that worktree's own
/// root, but two different linked worktrees of the same repo do not match.
fn worktree_root(cwd: &Path) -> std::result::Result<PathBuf, String> {
    let top = git(cwd, &["rev-parse", "--show-toplevel"])?;
    Ok(PathBuf::from(top))
}

/// The branch currently checked out in the worktree (error if detached).
pub(crate) fn worktree_branch(cwd: &Path) -> std::result::Result<String, String> {
    let b = git(cwd, &["symbolic-ref", "--quiet", "--short", "HEAD"])
        .map_err(|_| "worktree has no branch checked out (detached HEAD)".to_string())?;
    if b.is_empty() {
        return Err("worktree has no branch checked out".to_string());
    }
    Ok(b)
}

/// Whether `cwd_a` and `cwd_b` are worktrees of the same git repository.
/// Returns `false` when either path is not inside a git repo.
pub(crate) fn same_git_repo(cwd_a: &Path, cwd_b: &Path) -> bool {
    match (worktree_project(cwd_a), worktree_project(cwd_b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

/// Rebase the current branch in `cwd` onto `target_branch`.
///
/// Uncommitted changes are stashed beforehand and restored after so a dirty
/// worktree (e.g. a cell restarted mid-flight) doesn't block the rebase.
/// On failure the rebase is aborted automatically so the worktree is left
/// clean. The error string describes which git step failed and why.
pub(crate) fn rebase_onto(cwd: &Path, target_branch: &str) -> std::result::Result<(), String> {
    // Detect any working-tree or index changes (tracked or untracked).
    let has_changes = !git(cwd, &["status", "--porcelain"])?.is_empty();

    // RAL-283: named + uniquified, not a bare `git stash` — this worktree's
    // stash lives on the shared `refs/stash` stack of the whole repo (git
    // has no per-worktree stash), so a bare push/pop here could restore a
    // different concurrently-rebasing worktree's stashed changes instead of
    // this one's. No guardian id is available at this call site (it also
    // runs from the scheduler's plain cell-dependency rebase), so the scope
    // is just the worktree's own branch, which this repo's linked worktrees
    // never share.
    let stash_name = if has_changes {
        // A detached HEAD has no branch to name the scope after; the name is
        // unique either way, so fall back rather than refusing to rebase.
        let scope = worktree_branch(cwd).unwrap_or_else(|_| "detached-head".to_string());
        let name = crate::stash::unique_stash_name(&scope, "rebase");
        git(
            cwd,
            &["stash", "push", "--include-untracked", "--message", &name],
        )
        .map_err(|e| format!("git stash before rebase failed: {e}"))?;
        Some(name)
    } else {
        None
    };

    if let Err(e) = git(cwd, &["rebase", target_branch]) {
        // Abort the incomplete rebase so the worktree stays usable.
        let _ = git(cwd, &["rebase", "--abort"]);
        // Restore stashed work so nothing is lost.
        if let Some(name) = &stash_name {
            let _ = crate::stash::pop_named(|args| git(cwd, args), name);
        }
        return Err(format!("git rebase {target_branch} failed: {e}"));
    }

    // Rebase succeeded — restore any stashed work.
    if let Some(name) = &stash_name {
        crate::stash::pop_named(|args| git(cwd, args), name).map_err(|e| {
            format!("rebase succeeded but git stash pop failed (stash preserved): {e}")
        })?;
    }

    Ok(())
}

/// Make `baseline` the durable no-new-commits comparison point for the branch
/// checked out at `cwd`. A cell rebased onto an upstream task inherits that
/// task's commits, so its own task finalizer must compare against the rebased
/// upstream rather than the worktree's original creation base.
///
/// `baseline` is resolved to its commit SHA *now* and that SHA -- not the
/// name given -- is what gets stored. `baseline` is routinely a branch name
/// (e.g. an upstream task's own branch, or a remote-tracking ref), and a
/// branch name is a moving target: anything that later advances it (another
/// commit on that branch, a push landing on it, a fetch picking up someone
/// else's push) would otherwise retroactively make it look like *this*
/// worktree's own commits were "already accounted for," even though nothing
/// about this worktree changed. Freezing the resolved commit here is what
/// makes the marker a comparison point fixed at the moment this call ran,
/// rather than a live pointer re-read at guard-check time.
pub(crate) fn set_worktree_commit_baseline(
    cwd: &Path,
    baseline: &str,
) -> std::result::Result<(), String> {
    let branch = worktree_branch(cwd)?;
    let resolved = git(cwd, &["rev-parse", "--verify", baseline])?;
    git(
        cwd,
        &[
            "config",
            &format!("ralphus.{branch}.baseline"),
            resolved.trim(),
        ],
    )
    .map(|_| ())
}

/// The upstream (`branch@{upstream}`) of the worktree's branch, if any.
pub(crate) fn worktree_upstream(cwd: &Path) -> std::result::Result<String, String> {
    git(
        cwd,
        &[
            "rev-parse",
            "--abbrev-ref",
            "--symbolic-full-name",
            "@{upstream}",
        ],
    )
    .map_err(|_| "no upstream".to_string())
}

/// The durable comparison ref [`worktree_has_commits_ahead_of_upstream`]
/// reads: prefers the `ralphus.<branch>.baseline` git config key, falling
/// back to the live `@{upstream}` tracking ref when no such marker exists (a
/// worktree materialized before this fix, or a plain checkout never routed
/// through [`crate::worktrees::ensure_worktree`]).
///
/// The marker, once present, holds a *resolved commit SHA*, frozen at the
/// moment [`crate::worktrees::ensure_worktree`] (via
/// [`set_worktree_commit_baseline`]) last wrote it — never a live branch or
/// remote-tracking ref name. Two independent things can otherwise move such
/// a name out from under this check, both observed in practice:
///
/// - `@{upstream}` itself is exactly what `git push -u`/`--set-upstream`
///   overwrites: a `finalize` cell pushing a brand-new branch for the first
///   time routinely needs `-u` (a bare `git push` fails until *some*
///   upstream is configured), which retargets `branch.<branch>.remote`/
///   `.merge` from the intended base branch onto the branch's own
///   just-pushed remote copy — after which `@{upstream}` always equals
///   `HEAD`, indistinguishable from "no progress".
/// - A remote-tracking ref used as the baseline (e.g. `refs/remotes/origin/
///   staging`, the review's shared base branch) can itself be advanced by
///   *anything* that pushes or fetches into it during the run — including a
///   `finalize` cell that, seeing `git status` describe that ref as "your
///   branch's upstream," pushes its own commit directly onto it. That push
///   updates the local remote-tracking ref as a side effect, so a *live*
///   name-based baseline would immediately read the task's own just-pushed
///   commit as already "in the baseline," reporting real work as no
///   progress at all.
///
/// Resolving to a SHA once, up front, is immune to both: nothing that
/// happens to the name afterward can move the frozen comparison point.
fn workspace_baseline_ref(workspace: &Workspace) -> std::result::Result<String, String> {
    if let Ok(branch) = workspace.git(&["rev-parse", "--abbrev-ref", "HEAD"]) {
        let branch = branch.trim();
        if !branch.is_empty() {
            if let Ok(marker) =
                workspace.git(&["config", "--get", &format!("ralphus.{branch}.baseline")])
            {
                let marker = marker.trim();
                if !marker.is_empty() && workspace.git(&["rev-parse", "--verify", marker]).is_ok() {
                    return Ok(marker.to_string());
                }
            }
        }
    }
    workspace
        .git(&[
            "rev-parse",
            "--abbrev-ref",
            "--symbolic-full-name",
            "@{upstream}",
        ])
        .map(|s| s.trim().to_string())
        .map_err(|_| "no upstream".to_string())
}

/// Whether `workspace`'s checked-out branch has at least one commit its
/// durable baseline ([`workspace_baseline_ref`]) doesn't (RAL-293, made
/// machine-aware for RAL-355 Phase 8).
///
/// This is the no-new-commits guard's (`crate::scheduler::check_task_no_commits_guard`)
/// review-independent "did this task make real progress" signal: unlike a
/// squad-run-scoped baseline sha captured once at cell-start, this baseline is
/// durable git state that survives worktree reuse across squad resubmissions
/// and task restarts alike, so it never confuses "this run made no *new*
/// commits" with "this task never did the work". `false` when the worktree
/// has no resolvable baseline (e.g. a plain checkout never routed through
/// [`crate::worktrees::ensure_worktree`]) — mirrors the guard's existing
/// fail-closed policy of treating an unanswerable question as "no progress"
/// rather than silently passing. Local and remote workspaces are checked
/// identically through [`Workspace::git`].
#[must_use]
pub(crate) fn workspace_has_commits_ahead_of_upstream(workspace: &Workspace) -> bool {
    let Ok(baseline) = workspace_baseline_ref(workspace) else {
        return false;
    };
    workspace
        .git(&["rev-list", "--count", &format!("{baseline}..HEAD")])
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .is_some_and(|n| n > 0)
}

/// Whether *any* workspace in `workspaces` has commits ahead of its durable
/// baseline (RAL-355 Phase 8). Used by the scheduler's no-new-commits guard,
/// which fails a task only when every one of its cells' workspaces reports
/// no progress.
#[must_use]
pub(crate) fn any_workspace_ahead_of_upstream(workspaces: &[Workspace]) -> bool {
    workspaces
        .iter()
        .any(workspace_has_commits_ahead_of_upstream)
}

/// Whether the checked-out `HEAD` is already contained in its configured
/// upstream. Missing or unresolvable upstream state returns `false` so callers
/// never treat an uncertain git state as merged. `Workspace` keeps the check
/// valid for review worktrees on configured remote machines too.
#[must_use]
pub(crate) fn workspace_head_is_ancestor_of_upstream(workspace: &Workspace) -> bool {
    let Ok(upstream) = workspace.git(&[
        "rev-parse",
        "--abbrev-ref",
        "--symbolic-full-name",
        "@{upstream}",
    ]) else {
        return false;
    };
    workspace
        .git(&["merge-base", "--is-ancestor", "HEAD", upstream.trim()])
        .is_ok()
}

/// The read-only "upstream" value to show for a cell's git worktree in the
/// board's detail pane. Two cases, per the RAL-50 branch-chaining sentinel:
///
/// - The cell declares `upstream = "<<task:...>>"`: the displayed upstream
///   is the *referenced* cell's own worktree branch name (what this
///   cell's branch gets rebased onto before it runs) — not a plain
///   tracking-ref lookup, since that sentinel is the authoritative source of
///   truth for what this cell's branch is chained onto.
/// - No sentinel: falls back to the worktree's own git upstream tracking
///   branch (typically the non-worktree base branch it was forked from, e.g.
///   `main`).
///
/// `None` when `cwd` is unset, not inside a git worktree, or no upstream can
/// be resolved (e.g. a chained dependency that hasn't materialized a
/// worktree yet) — the caller shows this as "no upstream" rather than
/// guessing, mirroring the fail-closed/best-effort tolerance used by
/// [`crate::scheduler`]'s own upstream-rebase resolution.
#[must_use]
pub(crate) fn cell_upstream_display(
    cwd: Option<&str>,
    rows: &[CellRow],
    task_idx: i64,
    idx: i64,
) -> Option<String> {
    let cwd = cwd?;
    project_root_of(cwd)?;
    let row = rows.iter().find(|r| r.task_idx == task_idx && r.idx == idx);
    if let Some(sentinel) = row.and_then(|r| r.upstream.as_deref()) {
        let ref_str = ralphus_core::schema::parse_upstream_task_ref(sentinel)?;
        let (task_name, cell_id_filter) = ref_str
            .split_once('/')
            .map_or((ref_str, None), |(t, s)| (t, Some(s)));
        let dep = rows.iter().find(|r| {
            r.task_name == task_name && cell_id_filter.is_none_or(|cid| r.cell_id == cid)
        })?;
        return worktree_branch(Path::new(dep.cwd.as_deref()?)).ok();
    }
    worktree_upstream(Path::new(cwd)).ok()
}

/// One cell's contribution to a review.
struct Membership {
    /// The contributing cell's position, so the guardian this membership's
    /// group resolves to (known only once the group is created, further
    /// below) can be recorded back onto the exact cell row it came from
    /// (RAL-314) rather than just its branch string.
    task_idx: i64,
    idx: i64,
    project: PathBuf,
    /// Registered project declared by the owning task. `None` means this
    /// membership reached review creation through a raw-directory route.
    registered_project: Option<String>,
    branch: String,
    upstream: String,
    name: String,
    order: usize,
    /// The stable link key when the review id is `ralphus:new-review/<key>`; the
    /// review is then shared across submissions instead of grouped by project.
    link_key: Option<String>,
    /// Optional conflict-resolver backend/model declared on the review.
    agent: Option<String>,
    model: Option<String>,
    /// The machine this review's worktrees, rebase and conflict resolution run
    /// on (RAL-185). `None` means the daemon's own host.
    machine: Option<String>,
    /// Optional USD spend cap declared on the review (RAL-193).
    maximum_budget_usd: Option<f64>,
    /// Optional Proof-scope override declared on the review (`[[review]]
    /// proof_scope`), one of `each_branch`/`final_branch`/`nothing`.
    proof_scope: Option<String>,
    /// Optional auto-submit-PR-stack override declared on the review
    /// (`[[review]] auto_submit_pr_stack`, RAL-317).
    auto_submit_pr_stack: Option<bool>,
    /// Optional settings overrides declared on the review.
    skip_worktrees: Option<bool>,
    auto_pr_feedback: Option<bool>,
    skip_base_updates: Option<bool>,
    skip_auto_clean: Option<bool>,
    match_pr_branch_name: Option<bool>,
    separate_pr_branch: Option<bool>,
    /// Declared `[[review.auto_build]]` steps (RAL-342): zero or more build
    /// steps, each either a static `command` or an agent-invocation shape,
    /// mutually exclusive with `skip_auto_build`.
    auto_build: Vec<ralphus_core::schema::AutoBuildDef>,
    /// Explicit opt-out of the auto_build requirement (`[[review]]
    /// skip_auto_build = true`, RAL-342), mutually exclusive with `auto_build`.
    skip_auto_build: bool,
    /// RAL-395: optional auto-fix-PR-errors override declared on the review
    /// (`[[review]] auto_fix_pr_errors`).
    auto_fix_pr_errors: Option<bool>,
    /// RAL-395: optional auto-fix prompt template override declared on the
    /// review (`[[review]] auto_fix_prompt_template`), already validated
    /// (`core::validate`) to contain the literal `<<prompt>>` placeholder.
    auto_fix_prompt_template: Option<String>,
}

/// Build the planner's cell/task rows straight from the task file (same order
/// insertion uses), alongside each cell's cwd and declared review opt-in.
type CellReviewInfo<'a> = Vec<(Option<String>, Option<&'a str>)>;

fn rows_from_file(file: &TaskFile) -> (Vec<CellRow>, Vec<TaskRow>, CellReviewInfo<'_>) {
    let mut cells = Vec::new();
    let mut tasks = Vec::new();
    let mut cell_info: CellReviewInfo = Vec::new();
    for (t_idx, task) in file.task.iter().enumerate() {
        let ti = i64::try_from(t_idx).unwrap_or(0);
        tasks.push(TaskRow {
            idx: ti,
            name: task.name.clone(),
            project: task.project.clone(),
            depends_on: task.depends_on.clone(),
            soloed: false,
        });
        for (s_idx, s) in task.cell.iter().enumerate() {
            let sid = s.id.clone().unwrap_or_else(|| format!("cell-{s_idx}"));
            cells.push(CellRow {
                task_idx: ti,
                idx: i64::try_from(s_idx).unwrap_or(0),
                task_name: task.name.clone(),
                cell_id: sid,
                cwd: s.cwd.clone(),
                subprojects: s.subprojects.clone(),
                prompt: s.prompt.clone(),
                command: s.command.clone(),
                agent: "claude".to_string(),
                model: None,
                system_prompt: s.system_prompt.clone(),
                system_prompt_position: s.system_prompt_position.clone(),
                depends_on: s.depends_on.clone(),
                timeout_sec: None,
                budget_tokens: None,
                maximum_budget_usd: None,
                maximum_context: None,
                auto_compact_threshold: None,
                maximum_tool_output_tokens: None,
                upstream: s.upstream.clone(),
                machine: ralphus_core::schema::resolve_cell_machine(task, s),
                share_session: false,
            });
            // Collect the cell's cwd and its optional review opt-in id. `review`
            // is a `<<review:<id>>>` / `<<ralphus:new-review/<key>>>` sentinel
            // (RAL-269); unwrap it here so every downstream lookup against
            // `[[review]].id` (itself unwrapped) compares like with like.
            let rev_id = s
                .review
                .as_deref()
                .and_then(ralphus_core::schema::parse_cell_review_sentinel);
            cell_info.push((s.cwd.clone(), rev_id));
        }
    }
    (cells, tasks, cell_info)
}

/// Convert `[[review.action]]` entries into `GuardianCheck` values for storage.
fn actions_to_hints(actions: &[ReviewActionDef]) -> Vec<GuardianCheck> {
    actions
        .iter()
        .map(|a| GuardianCheck {
            label: Some(a.label.clone()),
            command: a.command.clone(),
            prompt: a.prompt.clone(),
            cleanup_command: a.cleanup_command.clone(),
            inputs: a
                .input
                .iter()
                .map(|i| CheckInput {
                    name: i.name.clone(),
                    message: i.message.clone(),
                    default: i.default.clone(),
                    // `[[review.action.input]]` (core schema) doesn't declare
                    // a type today (RAL-221) -- every TOML-authored action
                    // input stays unconstrained `String` until that's added.
                    r#type: crate::guardian::CheckInputType::String,
                })
                .collect(),
        })
        .collect()
}

/// What a remote cell contributes to a review, derived without touching the
/// filesystem (RAL-185 Phase 3b).
///
/// A cell that ran on another machine keeps its worktree there, so
/// `worktree_branch` / `worktree_upstream` / `worktree_project` cannot answer
/// for it — this host has no view of that directory. Every piece is instead
/// recoverable from what was already declared: the branch from the cell's
/// `ralphus:new-worktree/<branch>` cwd, and the project root from the owning
/// task's registered `project`. The upstream is the one thing with no
/// declarative source, which is why `[[review]] upstream` exists.
///
/// `None` for a local cell, which keeps the original inference path.
struct RemoteDerivation {
    cell_id: String,
    machine: String,
    branch: String,
    project_root: String,
}

/// Build a [`RemoteDerivation`] for `cell`, or `None` when it ran locally.
///
/// Each missing piece is its own error rather than a fall-through to the local
/// inference path: that path runs git against the cell's `cwd`, which for a
/// remote cell names a directory on another host — producing an opaque
/// "directory name is invalid" instead of saying what is actually wrong.
fn remote_cell_derivation(
    store: &Store,
    cell: &CellRow,
    raw_cwd: Option<&str>,
    task: Option<&TaskRow>,
) -> std::result::Result<Option<RemoteDerivation>, ReviewError> {
    let Some(machine) = cell
        .machine
        .as_deref()
        .map(str::trim)
        .filter(|m| !m.is_empty())
    else {
        return Ok(None);
    };
    if store
        .resolve_machine(Some(machine))
        .is_ok_and(|m| m.is_local())
    {
        return Ok(None);
    }
    // The branch must come from the *placeholder*, not the resolved cwd: by
    // this point `resolve_placeholders` has rewritten cwd to a path on the
    // remote machine, which says nothing about the branch name.
    let branch = raw_cwd
        .and_then(ralphus_core::schema::first_worktree_placeholder_in_text)
        .and_then(ralphus_core::schema::parse_worktree_placeholder)
        .map(str::to_string)
        .ok_or_else(|| {
            ReviewError::new(format!(
                "cell \"{}\" runs on machine \"{machine}\" and opts into a review, so its                  branch must be knowable without reading that machine's filesystem. Give it a                  cwd of the form \"ralphus:new-worktree/<branch>\" instead of a literal path.",
                cell.cell_id
            ))
        })?;
    let project_name = task.and_then(|t| t.project.as_deref()).ok_or_else(|| {
        ReviewError::new(format!(
            "cell \"{}\" runs on machine \"{machine}\" and opts into a review, so its              project cannot be discovered from its worktree. Set 'project' on its task.",
            cell.cell_id
        ))
    })?;
    let project_root = store
        .resolve_project(project_name)
        .ok()
        .flatten()
        .map(|p| p.path)
        .ok_or_else(|| {
            ReviewError::new(format!(
                "cell \"{}\" references unregistered project \"{project_name}\"",
                cell.cell_id
            ))
        })?;
    Ok(Some(RemoteDerivation {
        cell_id: cell.cell_id.clone(),
        machine: machine.to_string(),
        branch,
        project_root,
    }))
}

/// Preflight and materialize the reviews declared in `file` as guardians tagged
/// with `squad_id`. Returns the created guardian ids (empty when no cell
/// declares a review). Any git/worktree problem is a hard error.
///
/// # Errors
/// Returns [`ReviewError`] when a review cell has no cwd, its cwd is not a git
/// worktree, or the worktree has no upstream tracking branch.
pub fn derive_reviews(
    store: &Store,
    squad_id: &str,
    file: &TaskFile,
) -> std::result::Result<Vec<String>, ReviewError> {
    derive_reviews_with_prefetch(store, squad_id, file, &HashMap::new())
}

/// Every `(registered project, bare upstream)` pair this submission's cell
/// cwd worktree placeholders AND its `[[review]]` blocks' own declared
/// `upstream` fields would need a live `git fetch` for -- the union of
/// [`crate::worktrees::collect_remote_upstream_prefetch_targets`] (cell
/// cwds) and this function's own scan of each review's declared `upstream`
/// (the case [`derive_reviews_with_prefetch`]'s own resolution branch below
/// otherwise fetches for), deduped so a target named both ways is only
/// fetched once.
///
/// Read-only over `store`, cheap (no git subprocess), and safe to call under
/// a lock. The caller's job: call this under a lock, drop the lock, run the
/// actual `git fetch` for each result with no lock held at all (call
/// [`crate::worktrees::resolve_registered_remote_upstream`] directly), then
/// call [`derive_reviews_with_prefetch`] with the results keyed by
/// `(project.name.clone(), upstream)`. See
/// [`crate::worktrees::resolve_placeholders_with_prefetch`]'s doc comment
/// for why: without this, `derive_reviews`'s own live fetch runs while its
/// caller (`server::submit`) holds the daemon's single global
/// `Mutex<Store>` for the entire submit request, so a slow or dead remote
/// freezes the whole HTTP API for as long as the fetch takes.
#[must_use]
pub fn collect_remote_upstream_prefetch_targets(
    store: &Store,
    file: &TaskFile,
) -> Vec<(ProjectView, String)> {
    let (cells, tasks, cell_info) = rows_from_file(file);
    let mut seen: HashSet<(String, String)> = HashSet::new();
    let mut out = crate::worktrees::collect_remote_upstream_prefetch_targets(store, &cells, &tasks);
    out.retain(|(project, upstream)| seen.insert((project.name.clone(), upstream.clone())));

    let review_map: HashMap<&str, &ReviewDef> = file
        .review
        .iter()
        .filter_map(|rv| rv.id.as_deref().map(|id| (id, rv)))
        .collect();
    let tasks_by_idx: BTreeMap<i64, &TaskRow> = tasks.iter().map(|t| (t.idx, t)).collect();
    for (pos, (_, rev_id_opt)) in cell_info.iter().enumerate() {
        let Some(rev_id) = rev_id_opt else { continue };
        let Some(upstream) = review_map
            .get(rev_id)
            .copied()
            .and_then(|r| r.upstream.as_deref())
            .map(str::trim)
            .filter(|b| !b.is_empty())
        else {
            continue;
        };
        // `ReviewDef.upstream` never supports a `<<...>>` sentinel (unlike a
        // cell's own `?upstream=`) -- only the already-explicit-remote form
        // (`contains('/')`) needs skipping here, matching
        // `resolve_registered_remote_upstream`'s own no-op check.
        if upstream.contains('/') {
            continue;
        }
        let Some(project_name) = tasks_by_idx
            .get(&cells[pos].task_idx)
            .and_then(|t| t.project.as_deref())
        else {
            continue;
        };
        if !seen.insert((project_name.to_string(), upstream.to_string())) {
            continue;
        }
        let Ok(Some(project)) = store.resolve_project(project_name) else {
            continue;
        };
        if project.clone_url.is_none() {
            continue;
        }
        out.push((project, upstream.to_string()));
    }
    out
}

/// Order cells for review-branch `position` assignment: a deterministic
/// topological sort like [`plan::topo_order`], but tie-broken to put
/// standalone/short dependency chains ahead of long ones instead of always
/// preferring the lowest cell index.
///
/// This exists as its own pass — not a change to [`plan::topo_order`] itself
/// — because that function's order also drives real execution scheduling
/// (`scheduler.rs`, several `store.rs` call sites); changing its tie-break
/// would reorder which cells the scheduler actually dispatches first. Branch
/// *position* only matters here, for how a review's stacked rebase
/// (`guardian_merge.rs`) processes branches: that rebase walks branches
/// strictly in position order and stops a project's build at the first
/// not-yet-done branch, so a long multi-stage chain sitting in the middle of
/// the list blocks every unrelated, already-ready branch behind it. Moving
/// long chains toward the back — while still respecting every dependency
/// edge — lets the rebase make progress on independent branches instead of
/// stalling behind the slowest chain.
///
/// `deps[i]` lists cell `i`'s prerequisite positions (as in
/// [`plan::ExecutionPlan::deps`]); `topo` is any valid topological order of
/// the same positions (e.g. [`plan::ExecutionPlan::order`]), used only to
/// drive the two linear DP passes below in a safe order.
///
/// Algorithm: compute each cell's "chain weight" — the length of the
/// longest dependency chain running through it, counting both its ancestors
/// and its descendants once each (a standalone cell has weight 1; a cell in
/// the middle of a straight 4-cell chain has weight 4). Then run the same
/// Kahn's-algorithm topological sort as [`plan::topo_order`], but at each
/// step choose the ready cell with the *smallest* chain weight instead of
/// the lowest index (ties still break on index, so the result stays fully
/// deterministic).
fn review_branch_order(deps: &[Vec<usize>], topo: &[usize]) -> Vec<usize> {
    let n = deps.len();

    // Longest chain ending at `i` (1 + the longest chain ending at any of
    // its prerequisites). `topo` guarantees every prerequisite of `i` is
    // visited before `i` itself.
    let mut ending_at = vec![1usize; n];
    for &i in topo {
        if let Some(longest_prereq) = deps[i].iter().map(|&d| ending_at[d]).max() {
            ending_at[i] = longest_prereq + 1;
        }
    }

    // Longest chain starting at `i` (1 + the longest chain starting at any
    // of its dependents) — the mirror image, computed by walking `topo` in
    // reverse over the reversed edges.
    let mut dependents: Vec<Vec<usize>> = vec![Vec::new(); n];
    for (i, prereqs) in deps.iter().enumerate() {
        for &d in prereqs {
            dependents[d].push(i);
        }
    }
    let mut starting_at = vec![1usize; n];
    for &i in topo.iter().rev() {
        if let Some(longest_dependent) = dependents[i].iter().map(|&j| starting_at[j]).max() {
            starting_at[i] = longest_dependent + 1;
        }
    }

    // `i` itself is counted in both passes, so subtract 1 to avoid double-counting.
    let weight: Vec<usize> = (0..n).map(|i| ending_at[i] + starting_at[i] - 1).collect();

    let mut order = Vec::with_capacity(n);
    let mut done = vec![false; n];
    while order.len() < n {
        let next = (0..n)
            .filter(|&i| !done[i] && deps[i].iter().all(|&d| done[d]))
            .min_by_key(|&i| (weight[i], i));
        match next {
            Some(i) => {
                done[i] = true;
                order.push(i);
            }
            // `deps` already produced a valid `topo` via `plan()`, so every
            // remaining cell always has a ready prerequisite-satisfied
            // choice here.
            None => unreachable!("deps already passed a topological sort in plan()"),
        }
    }
    order
}

/// Like [`derive_reviews`], but `prefetched_upstreams` supplies
/// `(registered project name, bare upstream) -> "<remote>/<branch>"`
/// results the caller already fetched OUTSIDE any store lock -- see
/// [`collect_remote_upstream_prefetch_targets`]'s doc comment for how to
/// build this map, and why. A cache miss falls back to fetching live, right
/// where the fetch used to happen unconditionally, so correctness never
/// depends on this cache being complete.
pub fn derive_reviews_with_prefetch(
    store: &Store,
    squad_id: &str,
    file: &TaskFile,
    prefetched_upstreams: &HashMap<(String, String), String>,
) -> std::result::Result<Vec<String>, ReviewError> {
    if file.review.is_empty() {
        return Ok(Vec::new());
    }
    let (mut cells, tasks, cell_info) = rows_from_file(file);

    // Check early: are there any cells that opt into any review?
    if cell_info.iter().all(|(_, rev_id)| rev_id.is_none()) {
        return Ok(Vec::new());
    }

    // RAL-<pending>: a cheap, best-effort pass for `require_auto_build_declaration`
    // (below) BEFORE paying for `resolve_placeholders_with_prefetch`'s real `git
    // worktree add` calls -- a missing `[[review.auto_build]]`/`skip_auto_build`
    // is a purely syntactic mistake and shouldn't cost a whole batch's worth of
    // worktree creation to discover. See its own doc comment for why this only
    // covers link-key reviews and is advisory (the real check below still runs
    // regardless, so an imprecise answer here only costs time, never correctness).
    require_auto_build_declaration_early(store, file, &tasks, &cells, &cell_info)?;

    // A review-opted-in cell's `cwd` may still be an unmaterialized
    // `ralphus:new-worktree/<branch>` placeholder (RAL-100): normally the
    // scheduler only resolves those when the squad is claimed to execute, but
    // this preflight needs a real worktree path *now* to run git against it.
    // Resolving here (persisted via `Store::set_cell_cwd`, same as the
    // scheduler's resolution) means a restarted squad never re-resolves it.
    crate::worktrees::resolve_placeholders_with_prefetch(
        store,
        squad_id,
        &mut cells,
        &tasks,
        prefetched_upstreams,
        &Context::new(),
    )
    .map_err(ReviewError::new)?;

    // Topological rank per cell position (for branch ordering) — see
    // `review_branch_order`'s doc comment for why this is a separate pass
    // from the execution plan's own scheduling order.
    let execution = plan::plan(&cells, &tasks).map_err(ReviewError::new)?;
    let review_order = review_branch_order(&execution.deps, &execution.order);
    let mut rank = vec![0usize; cells.len()];
    for (r, &pos) in review_order.iter().enumerate() {
        rank[pos] = r;
    }

    // Build a map from review id → ReviewDef for quick lookup.
    let review_map: std::collections::HashMap<&str, &ReviewDef> = file
        .review
        .iter()
        .filter_map(|rv| rv.id.as_deref().map(|id| (id, rv)))
        .collect();

    // RAL-159: the worktree root and assigned branch of every explicitly
    // review-linked cell, so a second pass below can attach cells that
    // merely share that worktree (e.g. a nested cwd subfolder) but declared no
    // `review = "<<review:<id>>>"` of their own.
    let mut explicit_roots: Vec<(PathBuf, String)> = Vec::new();

    // Task rows indexed for the remote-derivation lookup below, which needs the
    // owning task's `project` (a remote cell's project grouping cannot come
    // from its worktree, since that lives on another machine).
    let tasks_by_idx: BTreeMap<i64, Option<&TaskRow>> =
        tasks.iter().map(|t| (t.idx, Some(t))).collect();

    let mut memberships: Vec<Membership> = Vec::new();
    for (pos, (_, rev_id_opt)) in cell_info.iter().enumerate() {
        let Some(rev_id) = rev_id_opt else { continue };
        // Use the (now-resolved) cwd from `cells`, not the raw placeholder
        // string captured in `cell_info` before `resolve_placeholders` ran above.
        let cwd = cells[pos]
            .cwd
            .as_deref()
            .ok_or_else(|| ReviewError::new("a cell declaring a review has no cwd"))?;
        let cwd_path = Path::new(cwd);
        let declared_upstream = review_map
            .get(rev_id)
            .copied()
            .and_then(|r| r.upstream.clone())
            .map(|b| b.trim().to_string())
            .filter(|b| !b.is_empty());
        // RAL-185: a cell that ran on another machine keeps its worktree
        // there, so none of the filesystem reads below can answer for it.
        // Everything needed is already known declaratively instead: the branch
        // from its `ralphus:new-worktree/<branch>` cwd, the upstream from the
        // review's own `upstream`, and the project from the owning task.
        let remote = remote_cell_derivation(
            store,
            &cells[pos],
            cell_info[pos].0.as_deref(),
            tasks_by_idx.get(&cells[pos].task_idx).copied().flatten(),
        )?;
        let (project, branch, upstream) = if let Some(rd) = &remote {
            let upstream = declared_upstream.clone().ok_or_else(|| {
                ReviewError::new(format!(
                    "review \"{rev_id}\" is fed by cell \"{}\" running on machine \"{}\", so \
                     its upstream branch cannot be read from that worktree's git upstream — \
                     this daemon cannot see another machine's filesystem. Declare it explicitly \
                     on the review: [[review]] upstream = \"main\".",
                    rd.cell_id, rd.machine
                ))
            })?;
            (PathBuf::from(&rd.project_root), rd.branch.clone(), upstream)
        } else {
            let project =
                worktree_project(cwd_path).map_err(|e| ReviewError::new(format!("{cwd}: {e}")))?;
            let branch =
                worktree_branch(cwd_path).map_err(|e| ReviewError::new(format!("{cwd}: {e}")))?;
            // A declared upstream wins; otherwise infer it from the worktree's
            // own git upstream, exactly as an all-local review always has.
            let upstream = match declared_upstream.clone() {
                // A review's declared `upstream` is the submitter's literal
                // text -- unlike a cell's own `?upstream=`, it never passes
                // through `resolve_placeholders`/`ensure_worktree`, so a bare
                // name here needs the same registered-project-resolves-
                // against-its-remote treatment those give a cell's cwd
                // placeholder (see `resolve_registered_remote_upstream`'s doc
                // comment): otherwise it silently falls back to whatever
                // locally-named branch the shared checkout happens to have on
                // disk right now, instead of that remote's actual branch.
                Some(b) => {
                    let registered_project = tasks_by_idx
                        .get(&cells[pos].task_idx)
                        .copied()
                        .flatten()
                        .and_then(|task| task.project.as_deref())
                        .and_then(|name| store.resolve_project(name).ok().flatten());
                    match registered_project {
                        // Prefer an already-fetched result from
                        // `prefetched_upstreams` (computed by the caller
                        // OUTSIDE the store lock this whole function runs
                        // under) over fetching live right here -- see
                        // `derive_reviews_with_prefetch`'s doc comment.
                        Some(pv) => match prefetched_upstreams.get(&(pv.name.clone(), b.clone())) {
                            Some(resolved) => resolved.clone(),
                            None => crate::worktrees::resolve_registered_remote_upstream(
                                &project, &pv, &b,
                            )
                            .map_err(ReviewError::new)?,
                        },
                        None => b,
                    }
                }
                None => worktree_upstream(cwd_path).map_err(|_| {
                    ReviewError::new(format!(
                        "{cwd}: review upstream requires a git upstream tracking branch for \
                         '{branch}', but none is configured (set one with \
                         'git branch --set-upstream-to=<branch>', or declare it on the review \
                         as [[review]] upstream = \"<branch>\")"
                    ))
                })?,
            };
            (project, branch, upstream)
        };
        // Record this cell's review branch so the board can link the cell
        // back to its review(s) (RAL-17).
        let crow = &cells[pos];
        let registered_project = tasks_by_idx
            .get(&crow.task_idx)
            .copied()
            .flatten()
            .and_then(|task| task.project.as_deref())
            .and_then(|name| store.resolve_project(name).ok().flatten())
            .map(|registered| registered.name);
        store
            .set_cell_review_branch(squad_id, crow.task_idx, crow.idx, &branch)
            .map_err(|e| ReviewError::new(e.to_string()))?;
        // Only meaningful for a local worktree: a remote cell's cwd names a
        // directory this host cannot stat, and its project grouping already
        // came from the owning task above.
        if remote.is_none() {
            if let Ok(root) = worktree_root(cwd_path) {
                explicit_roots.push((root, branch.clone()));
            }
        }

        // Look up the top-level review definition by id to get name/agent/model/actions.
        let rv = review_map.get(rev_id).copied();
        let link_key = review_link_key(rev_id).map(str::to_string);
        memberships.push(Membership {
            task_idx: crow.task_idx,
            idx: crow.idx,
            project: project.clone(),
            registered_project,
            branch: branch.clone(),
            upstream,
            name: rv
                .and_then(|r| r.name.clone())
                .or_else(|| link_key.clone())
                .unwrap_or_else(|| rev_id.to_string()),
            order: rank[pos],
            link_key,
            agent: rv
                .and_then(|r| r.agent.clone())
                .filter(|s| !s.trim().is_empty()),
            model: rv
                .and_then(|r| r.model.clone())
                .filter(|s| !s.trim().is_empty()),
            machine: rv
                .and_then(|r| r.machine.clone())
                .filter(|s| !s.trim().is_empty()),
            maximum_budget_usd: rv.and_then(|r| r.maximum_budget_usd),
            proof_scope: rv
                .and_then(|r| r.proof_scope.clone())
                .filter(|s| !s.trim().is_empty()),
            auto_submit_pr_stack: rv.and_then(|r| r.auto_submit_pr_stack),
            skip_worktrees: rv.and_then(|r| r.skip_worktrees),
            auto_pr_feedback: rv.and_then(|r| r.auto_pr_feedback),
            skip_base_updates: rv.and_then(|r| r.skip_base_updates),
            skip_auto_clean: rv.and_then(|r| r.skip_auto_clean),
            match_pr_branch_name: rv.and_then(|r| r.match_pr_branch_name),
            separate_pr_branch: rv.and_then(|r| r.separate_pr_branch),
            auto_build: rv.map(|r| r.auto_build.clone()).unwrap_or_default(),
            skip_auto_build: rv.is_some_and(|r| r.skip_auto_build),
            auto_fix_pr_errors: rv.and_then(|r| r.auto_fix_pr_errors),
            auto_fix_prompt_template: rv
                .and_then(|r| r.auto_fix_prompt_template.clone())
                .filter(|s| !s.trim().is_empty()),
        });
    }

    if memberships.is_empty() {
        return Ok(Vec::new());
    }

    // RAL-159: cells that share a worktree with an explicitly review-linked
    // cell -- e.g. one at the worktree root, another at a nested cwd
    // subfolder -- implicitly belong to that same branch's review too, even
    // without their own `review = "<<review:<id>>>"`: they can commit to the exact same
    // branch, since a worktree checks out exactly one branch at a time. This
    // makes the readiness gate (`Store::mark_ready_branches_with_done_cells`)
    // wait for them, and surfaces them in the cell's "in reviews" list
    // (RAL-17) via the same `cells.review_branch` join used for explicit
    // members -- no separate UI/query path needed. Matched by literal
    // worktree root, not mere project identity, so a sibling *linked*
    // worktree of the same repo (a different branch) does not cross-match.
    // RAL-314: cells picked up here have no `Membership` of their own (they
    // never declared a review), so the guardian each one belongs to isn't
    // known until the group its matched branch ends up in is created, below.
    // Keyed by branch since that's all `explicit_roots` records; a worktree
    // checks out exactly one branch, so every implicit cell matching a given
    // branch always belongs to the same guardian as the explicit member(s)
    // that share it.
    let mut implicit_cells_by_branch: BTreeMap<String, Vec<(i64, i64)>> = BTreeMap::new();
    for (pos, (_, rev_id_opt)) in cell_info.iter().enumerate() {
        if rev_id_opt.is_some() {
            continue; // already handled explicitly above
        }
        let Some(cwd) = cells[pos].cwd.as_deref() else {
            continue;
        };
        let Ok(root) = worktree_root(Path::new(cwd)) else {
            continue;
        };
        if let Some((_, branch)) = explicit_roots.iter().find(|(r, _)| *r == root) {
            let crow = &cells[pos];
            store
                .set_cell_review_branch(squad_id, crow.task_idx, crow.idx, branch)
                .map_err(|e| ReviewError::new(e.to_string()))?;
            implicit_cells_by_branch
                .entry(branch.clone())
                .or_default()
                .push((crow.task_idx, crow.idx));
        }
    }

    // Build per-review action hints, keyed by review id (or link key).
    // For now: action hints from all opted-in reviews are merged per guardian.
    // Since there is one [[review]] per submission, this is straightforward.
    let hints_by_id: std::collections::HashMap<&str, Vec<GuardianCheck>> = file
        .review
        .iter()
        .filter_map(|rv| {
            rv.id
                .as_deref()
                .map(|id| (id, actions_to_hints(&rv.action)))
        })
        .collect();

    // Split memberships into link groups (grouped within THIS submission by the
    // `ralphus:new-review/<key>` placeholder) and project groups (the classic
    // "one review per repo per submit"). BTreeMap keys give a deterministic order.
    let mut link_groups: BTreeMap<String, Vec<&Membership>> = BTreeMap::new();
    let mut proj_groups: BTreeMap<String, Vec<&Membership>> = BTreeMap::new();
    for m in &memberships {
        if let Some(key) = &m.link_key {
            link_groups.entry(key.clone()).or_default().push(m);
        } else {
            proj_groups
                .entry(m.project.to_string_lossy().into_owned())
                .or_default()
                .push(m);
        }
    }

    let mut created = Vec::new();

    // Project groups: one fresh guardian each. Several in one submit get a numeric
    // suffix so their names stay distinct (review-001, review-002, …).
    let multi = proj_groups.len() > 1;
    for (k, (project, members)) in proj_groups.iter().enumerate() {
        let mut members = members.clone();
        members.sort_by_key(|m| m.order);
        let upstream = members
            .first()
            .map_or_else(|| "main".to_string(), |m| m.upstream.clone());
        let suggested = members
            .iter()
            .find(|m| !m.name.is_empty())
            .map_or_else(|| "review".to_string(), |m| m.name.clone());
        let name = if multi {
            format!("{suggested}-{:03}", k + 1)
        } else {
            suggested
        };
        require_auto_build_declaration(store, &members, std::slice::from_ref(project), &name)?;
        let registered_project = single_registered_project(&members);
        let gid = store
            .create_guardian_keyed(
                &name,
                &upstream,
                project,
                Some(squad_id),
                None,
                registered_project,
            )
            .map_err(|e| ReviewError::new(e.to_string()))?;
        apply_resolver(store, &gid, &members)?;
        apply_project_review_defaults(store, &gid, project)?;
        apply_auto_build(store, &gid, &members)?;
        apply_action_hints(store, &gid, &members, &hints_by_id)?;
        // Single-project: no need to tag branches with a project (they share git_root).
        add_new_branches(store, &gid, &[], &members, false)?;
        record_review_guardian(store, squad_id, &gid, &members, &implicit_cells_by_branch)?;
        created.push(gid);
    }

    // Link groups: `ralphus:new-review/<key>` is a submission-LOCAL placeholder
    // for a review id that does not exist yet. Each key group ALWAYS mints a fresh
    // guardian for this submission — tasks sharing the same <key> within this one
    // submission collapse into that new guardian, while different keys make
    // different new guardians. A later submission that reuses the same <key> string
    // gets its own brand-new guardian; the placeholder never attaches to a guardian
    // from a previous submission (RAL-* placeholder semantics). The `review_key` is
    // still recorded on the guardian purely as provenance (which placeholder minted
    // it), never as a cross-submission link. Branches are tagged with their project
    // root so the merge engine processes each repo independently (RAL-29).
    for (key, members) in &link_groups {
        let mut members = members.clone();
        members.sort_by_key(|m| m.order);
        // Use the first member's project as the primary git_root.
        let project = members
            .first()
            .map(|m| m.project.to_string_lossy().into_owned())
            .unwrap_or_default();
        let upstream = members
            .first()
            .map_or_else(|| "main".to_string(), |m| m.upstream.clone());
        let name = members
            .iter()
            .find(|m| !m.name.is_empty())
            .map_or_else(|| key.clone(), |m| m.name.clone());
        // Computed before the guardian is created: every distinct project in
        // the group, and the required-declaration check both need this.
        let distinct_projects: Vec<String> = {
            let mut seen = std::collections::HashSet::new();
            members
                .iter()
                .map(|m| m.project.to_string_lossy().into_owned())
                .filter(|p| seen.insert(p.clone()))
                .collect()
        };
        let review_ref = format!("{}{key}", ralphus_core::schema::REVIEW_LINK_PREFIX);
        require_auto_build_declaration(store, &members, &distinct_projects, &review_ref)?;
        let registered_project = single_registered_project(&members);
        let gid = store
            .create_guardian_keyed(
                &name,
                &upstream,
                &project,
                Some(squad_id),
                Some(key),
                registered_project,
            )
            .map_err(|e| ReviewError::new(e.to_string()))?;
        // Apply skip_worktrees for every distinct project in the group.
        for proj in &distinct_projects {
            apply_project_review_defaults(store, &gid, proj)?;
        }
        apply_resolver(store, &gid, &members)?;
        apply_auto_build(store, &gid, &members)?;
        apply_action_hints(store, &gid, &members, &hints_by_id)?;
        // Freshly minted guardian: no branches attached yet. Tag each branch with
        // its project root (multi-project link group).
        add_new_branches(store, &gid, &[], &members, true)?;
        record_review_guardian(store, squad_id, &gid, &members, &implicit_cells_by_branch)?;
        created.push(gid);
    }

    Ok(created)
}

/// Return the one registered project shared by every member. Mixed-project
/// and raw-directory groups intentionally have no singular project identity.
fn single_registered_project<'a>(members: &[&'a Membership]) -> Option<&'a str> {
    let first = members.first()?.registered_project.as_deref()?;
    members
        .iter()
        .all(|member| member.registered_project.as_deref() == Some(first))
        .then_some(first)
}

/// Stamp `gid` as the guardian a group's cells resolved to (RAL-314), both
/// for the members whose own `review = "<<review:<id>>>"` declaration formed
/// the group, and for any cell that implicitly joined it by sharing an
/// explicit member's worktree (`implicit_cells_by_branch`, populated by the
/// RAL-159 pass above `derive_reviews` runs before grouping). Called once
/// per freshly created guardian, right after `add_new_branches` -- `gid` is
/// only known at this point, which is why this can't happen alongside the
/// earlier `set_cell_review_branch` calls.
fn record_review_guardian(
    store: &Store,
    squad_id: &str,
    gid: &str,
    members: &[&Membership],
    implicit_cells_by_branch: &BTreeMap<String, Vec<(i64, i64)>>,
) -> std::result::Result<(), ReviewError> {
    for m in members {
        store
            .set_cell_review_guardian(squad_id, m.task_idx, m.idx, gid)
            .map_err(|e| ReviewError::new(e.to_string()))?;
        if let Some(implicit) = implicit_cells_by_branch.get(&m.branch) {
            for &(task_idx, idx) in implicit {
                store
                    .set_cell_review_guardian(squad_id, task_idx, idx, gid)
                    .map_err(|e| ReviewError::new(e.to_string()))?;
            }
        }
    }
    Ok(())
}

/// Layered review config (global under per-project) fills in whatever this
/// guardian doesn't already have a concrete value for: opting reviews out of
/// per-branch worktrees (CCTL-156), and -- RAL-342/RAL-338 -- a `machine`/
/// `maximum_budget_usd` default. This is the single call site both guardian-
/// creation paths share (an explicit `[[review]]` submission, after
/// `apply_resolver` has already applied anything the block itself declared;
/// and the Arbiter's `create_review_from_triage_pool`, which has no
/// `[[review]]` block to read from at all), so it must never clobber a value
/// that's already set -- it only fills gaps.
///
/// `agent`/`model`/`proof_scope` don't need an equivalent here: they're
/// resolved lazily against the same project config, at the point each is
/// actually used (`guardian_merge::resolve_resolver_agent`,
/// `guardian.rs`'s `effective_proof_scope`), so they already pick up a
/// project default regardless of how the guardian was created.
/// `auto_submit_pr_stack` also doesn't need one: `create_guardian_keyed`
/// already stamps it at INSERT time for every guardian, both paths included.
fn apply_project_review_defaults(
    store: &Store,
    gid: &str,
    project: &str,
) -> std::result::Result<(), ReviewError> {
    // RAL-408: layers the project's database-backed review-setting defaults
    // (edited via the board/CLI) over the file-based `.ralphus.toml [review]`
    // ones -- see `Store::resolve_review_config`.
    let cfg = store.resolve_review_config(Path::new(project));
    if cfg.skip_worktrees() {
        store
            .set_guardian_skip_worktrees(gid, true)
            .map_err(|e| ReviewError::new(e.to_string()))?;
    }
    let row = store
        .get_guardian(gid)
        .map_err(|e| ReviewError::new(e.to_string()))?;
    if row.machine.is_none() {
        if let Some(machine) = cfg.default_machine() {
            store
                .set_guardian_machine(gid, Some(machine))
                .map_err(|e| ReviewError::new(e.to_string()))?;
        }
    }
    if row.maximum_budget_usd.is_none() {
        if let Some(cap) = cfg.default_maximum_budget_usd() {
            store
                .set_guardian_maximum_budget_usd(gid, Some(cap))
                .map_err(|e| ReviewError::new(e.to_string()))?;
        }
    }
    // RAL-395: fill the auto-fix-PR-errors/template gap from the project
    // config -- the single call site that guarantees an Arbiter/Triage
    // review (no `[[review]]` block of its own) always ends up using the
    // project default unconditionally, same as `machine`/
    // `maximum_budget_usd` above.
    if row.auto_fix_pr_errors.is_none() && cfg.auto_fix_pr_errors() {
        store
            .set_guardian_auto_fix_pr_errors(gid, Some(true))
            .map_err(|e| ReviewError::new(e.to_string()))?;
    }
    if row.auto_fix_prompt_template.is_none() {
        if let Some(template) = cfg.auto_fix_prompt_template() {
            store
                .set_guardian_auto_fix_prompt_template(gid, Some(template))
                .map_err(|e| ReviewError::new(e.to_string()))?;
        }
    }
    Ok(())
}

/// Set the guardian's conflict-resolver backend/model from the first member that
/// declares each (members are pre-sorted in branch order). A no-op when no member
/// sets one, leaving the guardian on the env/default resolver.
fn apply_resolver(
    store: &Store,
    gid: &str,
    members: &[&Membership],
) -> std::result::Result<(), ReviewError> {
    let agent = members.iter().find_map(|m| m.agent.clone());
    let model = members.iter().find_map(|m| m.model.clone());
    if agent.is_some() || model.is_some() {
        // Each column is written only when some member declares it -- a member
        // that sets `model` alone must not wipe a `resolver_agent` naming a
        // custom agent profile (see `Store::set_guardian_resolver`).
        store
            .set_guardian_resolver(gid, agent.as_deref().map(Some), model.as_deref().map(Some))
            .map_err(|e| ReviewError::new(e.to_string()))?;
    }
    // RAL-185: where this review's own work runs. Independent of any
    // contributing task's machine (D3) -- a review may be assigned a machine
    // none of its tasks used.
    if let Some(machine) = members.iter().find_map(|m| m.machine.clone()) {
        store
            .set_guardian_machine(gid, Some(&machine))
            .map_err(|e| ReviewError::new(e.to_string()))?;
    }
    // RAL-193: this review's own USD spend cap.
    if let Some(cap) = members.iter().find_map(|m| m.maximum_budget_usd) {
        store
            .set_guardian_maximum_budget_usd(gid, Some(cap))
            .map_err(|e| ReviewError::new(e.to_string()))?;
    }
    // This review's own Proof-scope override, authored via `[[review]]
    // proof_scope`. Validated offline (`core::validate`) against
    // `PROOF_SCOPE_VALUES`, so any value reaching here is already one of
    // `each_branch`/`final_branch`/`nothing`.
    if let Some(scope) = members.iter().find_map(|m| m.proof_scope.clone()) {
        store
            .set_guardian_proof_scope(gid, Some(&scope))
            .map_err(|e| ReviewError::new(e.to_string()))?;
    }
    // This review's own auto-submit-PR-stack override, authored via
    // `[[review]] auto_submit_pr_stack` (RAL-317).
    if let Some(enabled) = members.iter().find_map(|m| m.auto_submit_pr_stack) {
        store
            .set_guardian_auto_submit_pr_stack(gid, Some(enabled))
            .map_err(|e| ReviewError::new(e.to_string()))?;
    }
    if let Some(skip) = members.iter().find_map(|m| m.skip_worktrees) {
        store
            .set_guardian_skip_worktrees(gid, skip)
            .map_err(|e| ReviewError::new(e.to_string()))?;
    }
    if let Some(enabled) = members.iter().find_map(|m| m.auto_pr_feedback) {
        store
            .set_guardian_auto_pr_feedback(gid, enabled)
            .map_err(|e| ReviewError::new(e.to_string()))?;
    }
    if let Some(skip) = members.iter().find_map(|m| m.skip_base_updates) {
        store
            .set_guardian_skip_base_updates(gid, Some(skip))
            .map_err(|e| ReviewError::new(e.to_string()))?;
    }
    if let Some(skip) = members.iter().find_map(|m| m.skip_auto_clean) {
        store
            .set_guardian_proof_skip_auto_clean(gid, Some(skip))
            .map_err(|e| ReviewError::new(e.to_string()))?;
    }
    if let Some(enabled) = members.iter().find_map(|m| m.match_pr_branch_name) {
        store
            .set_guardian_match_pr_branch_name(gid, Some(enabled))
            .map_err(|e| ReviewError::new(e.to_string()))?;
    }
    if let Some(enabled) = members.iter().find_map(|m| m.separate_pr_branch) {
        store
            .set_guardian_separate_pr_branch(gid, Some(enabled))
            .map_err(|e| ReviewError::new(e.to_string()))?;
    }
    // RAL-395: this review's own auto-fix-PR-errors override, authored via
    // `[[review]] auto_fix_pr_errors`.
    if let Some(enabled) = members.iter().find_map(|m| m.auto_fix_pr_errors) {
        store
            .set_guardian_auto_fix_pr_errors(gid, Some(enabled))
            .map_err(|e| ReviewError::new(e.to_string()))?;
    }
    // RAL-395: this review's own auto-fix prompt template override, authored
    // via `[[review]] auto_fix_prompt_template`. Validated offline
    // (`core::validate`) to contain the literal `<<prompt>>` placeholder.
    if let Some(template) = members
        .iter()
        .find_map(|m| m.auto_fix_prompt_template.clone())
    {
        store
            .set_guardian_auto_fix_prompt_template(gid, Some(&template))
            .map_err(|e| ReviewError::new(e.to_string()))?;
    }
    Ok(())
}

/// Field-for-field conversion from the offline `core::schema` shape (as parsed
/// from `[[review.auto_build]]`) to the runtime `guardian::GuardianAutoBuild`
/// shape persisted in the store.
fn into_guardian_auto_build(
    def: &ralphus_core::schema::AutoBuildDef,
) -> crate::guardian::GuardianAutoBuild {
    crate::guardian::GuardianAutoBuild {
        command: def.command.clone(),
        prompt: def.prompt.clone(),
        system_prompt: def.system_prompt.clone(),
        system_prompt_position: def.system_prompt_position.clone(),
        agent: def.agent.clone(),
        model: def.model.clone(),
    }
}

/// Persist this review's declared build steps (RAL-342), from the first member
/// that sets `[[review.auto_build]]` entries, or -- failing that -- the first
/// member that sets `skip_auto_build = true`. Currently only the first step
/// is persisted; multiple steps will be supported in the future. A no-op when
/// no member declares either, leaving the guardian to fall back to the
/// project-config default at merge time.
fn apply_auto_build(
    store: &Store,
    gid: &str,
    members: &[&Membership],
) -> std::result::Result<(), ReviewError> {
    if let Some(def) = members.iter().find_map(|m| m.auto_build.first()) {
        store
            .set_guardian_auto_build(gid, Some(&into_guardian_auto_build(def)))
            .map_err(|e| ReviewError::new(e.to_string()))?;
    } else if members.iter().any(|m| m.skip_auto_build) {
        store
            .set_guardian_skip_auto_build(gid, true)
            .map_err(|e| ReviewError::new(e.to_string()))?;
    }
    Ok(())
}

/// Preflight review build declarations before squad insertion, using only
/// registered project configuration. It covers LINK-KEY reviews
/// (`ralphus:new-review/<key>`, `id` starting with
/// [`ralphus_core::schema::REVIEW_LINK_PREFIX`]) — a
/// plain-name review's membership can still be SPLIT into multiple guardians
/// by *resolved git root* once worktrees exist (see the "project groups" loop
/// in [`derive_reviews_with_prefetch`]), so this function cannot safely
/// predict its final per-guardian project set and leaves those to the late
/// check exactly as before. A link-key review is never split that way — every
/// cell sharing a `<key>` always folds into one guardian — so its full
/// project set is already knowable from each referencing cell's *owning
/// task's declared `project` name*, no worktree resolution required.
///
/// That declared name is then proxied through the project's *registered*
/// root path (`Store::resolve_project`), not the eventual per-branch
/// worktree path the late check uses, when asking
/// `crate::config::resolve` whether a project-level `auto_build` default
/// covers it — the two paths can differ (a worktree is a subdirectory
/// alongside the main checkout), but `.ralphus.toml` config is layered
/// upward from wherever you start, so both normally resolve the same files.
/// The materialization path repeats the check against resolved worktree paths
/// before creating a guardian.
pub(crate) fn preflight_auto_build_declarations(
    store: &Store,
    file: &TaskFile,
) -> std::result::Result<(), ReviewError> {
    let (cells, tasks, cell_info) = rows_from_file(file);
    require_auto_build_declaration_early(store, file, &tasks, &cells, &cell_info)
}

fn require_auto_build_declaration_early(
    store: &Store,
    file: &TaskFile,
    tasks: &[TaskRow],
    cells: &[CellRow],
    cell_info: &CellReviewInfo,
) -> std::result::Result<(), ReviewError> {
    let review_map: std::collections::HashMap<&str, &ReviewDef> = file
        .review
        .iter()
        .filter_map(|rv| rv.id.as_deref().map(|id| (id, rv)))
        .collect();
    let tasks_by_idx: BTreeMap<i64, &TaskRow> = tasks.iter().map(|t| (t.idx, t)).collect();

    let mut projects_by_review: BTreeMap<&str, HashSet<String>> = BTreeMap::new();
    for (pos, (_, rev_id_opt)) in cell_info.iter().enumerate() {
        let Some(rev_id) = rev_id_opt else { continue };
        if !rev_id.starts_with(ralphus_core::schema::REVIEW_LINK_PREFIX) {
            continue; // Plain-name reviews may still split by git root later.
        }
        let Some(project_name) = tasks_by_idx
            .get(&cells[pos].task_idx)
            .and_then(|t| t.project.as_deref())
        else {
            continue;
        };
        projects_by_review
            .entry(rev_id)
            .or_default()
            .insert(project_name.to_string());
    }

    for (rev_id, project_names) in projects_by_review {
        let Some(rv) = review_map.get(rev_id) else {
            continue;
        };
        if !rv.auto_build.is_empty() || rv.skip_auto_build {
            continue;
        }
        let covered = !project_names.is_empty()
            && project_names.iter().all(|name| {
                store.resolve_project(name).ok().flatten().is_some_and(|p| {
                    crate::config::resolve(Path::new(&p.path))
                        .auto_build
                        .is_some()
                })
            });
        if covered {
            continue;
        }
        return Err(ReviewError::new(format!(
            "{rev_id} must declare [[review.auto_build]] or skip_auto_build = true \
             (or configure a project-level auto_build default in .ralphus.toml)"
        )));
    }
    Ok(())
}

/// RAL-342: every review must explicitly declare its finalize-time build step
/// -- `[[review.auto_build]]` or `skip_auto_build = true` -- unless every
/// distinct project in the group already has a project-level `auto_build`
/// default configured (`.ralphus.toml [review] auto_build`). Runs before the
/// guardian is created, so a rejection here never leaves behind a partial
/// guardian (mirrors the whole-squad rollback-on-`Err` at the submit call
/// site). Submit-time only -- reopen/restart-merge do not re-check this.
///
/// `review_ref` identifies the pending review in the error message: for a
/// link group this is its `ralphus:new-review/<key>` placeholder URI (no
/// guardian id exists yet to reference instead); for a project group it is
/// the review's resolved name.
fn require_auto_build_declaration(
    store: &Store,
    members: &[&Membership],
    distinct_projects: &[String],
    review_ref: &str,
) -> std::result::Result<(), ReviewError> {
    let declared = members
        .iter()
        .any(|m| !m.auto_build.is_empty() || m.skip_auto_build);
    if declared {
        return Ok(());
    }
    let covered_by_config = !distinct_projects.is_empty()
        && distinct_projects.iter().all(|p| {
            store
                .resolve_review_config(Path::new(p))
                .auto_build
                .is_some()
        });
    if covered_by_config {
        return Ok(());
    }
    Err(ReviewError::new(format!(
        "{review_ref} must declare [[review.auto_build]] or skip_auto_build = true \
         (or configure a project-level auto_build default in .ralphus.toml)"
    )))
}

/// Persist user-declared action hints from the top-level `[[review.action]]`
/// entries onto the guardian. Uses the first member's review id to look up hints.
fn apply_action_hints(
    store: &Store,
    gid: &str,
    members: &[&Membership],
    hints_by_id: &std::collections::HashMap<&str, Vec<GuardianCheck>>,
) -> std::result::Result<(), ReviewError> {
    // Collect hints from the review ids referenced by the members (deduplicated).
    // In practice there is typically one review id per group, so this is a single
    // lookup. For link-key groups that span multiple review declarations, we merge.
    let mut seen_ids: HashSet<String> = HashSet::new();
    let mut all_hints: Vec<GuardianCheck> = Vec::new();
    for m in members {
        // Re-derive the review id: for non-link members use the name; for link
        // members reconstruct from the link key. Actually simpler: walk the file's
        // review map we already have via hints_by_id.
        // The member's name is the resolved display name, not the raw id. So look up
        // all review entries whose resolved hints match. Instead: just collect ALL
        // unique hint sets from the known review ids for this group.
        // Simplest: iterate ALL review ids in hints_by_id and include the ones whose
        // link_key matches m.link_key, or whose name matches m.name.
        // Even simpler: just iterate hints_by_id and take any entry. For the common
        // case (one [[review]] block), this includes all action hints.
        let _ = m; // used for ordering; hint lookup is below
    }
    for (id, hints) in hints_by_id {
        if seen_ids.insert((*id).to_string()) {
            all_hints.extend_from_slice(hints);
        }
    }
    if !all_hints.is_empty() {
        store
            .set_guardian_action_hints(gid, &all_hints)
            .map_err(|e| ReviewError::new(e.to_string()))?;
    }
    Ok(())
}

/// Append each member branch to a guardian, skipping any already present (either
/// already attached from a prior submission or a duplicate within this group).
/// For link-group guardians, each branch is tagged with its project root so the
/// merge engine can process projects independently (RAL-29).
fn add_new_branches(
    store: &Store,
    gid: &str,
    existing: &[String],
    members: &[&Membership],
    tag_project: bool,
) -> std::result::Result<(), ReviewError> {
    let mut seen: HashSet<String> = existing.iter().cloned().collect();
    for m in members {
        if seen.insert(m.branch.clone()) {
            let project = if tag_project {
                Some(m.project.to_string_lossy().into_owned())
            } else {
                None
            };
            store
                .add_guardian_branch_with_project(gid, &m.branch, project.as_deref())
                .map_err(|e| ReviewError::new(e.to_string()))?;
        }
    }
    Ok(())
}

// ── Triage pooling (RAL-318) ─────────────────────────────────────────────────

/// Pool every Triage-opted-in cell (`triage = true`) from `file` into each of
/// its resolved `(project, triage_type)` pools -- a cell that resolved to
/// more than one type (inline `triage_type` list, or a multi-type Arbiter
/// classification) is pooled into every one of them independently -- then
/// check whether any of those pools' count thresholds has now fired (a cron
/// schedule can also fire one independently -- see `crate::scheduler`'s
/// Triage tick). A firing pool is drained and turned into a fresh review
/// guardian through the same Collecting -> Approved -> Deployed pipeline
/// [`derive_reviews`] uses, flagged [`crate::guardian::GUARDIAN_ORIGIN_ARBITER`].
/// Returns the created guardian ids (empty when no cell opts into Triage, or
/// no pool fired).
///
/// Each cell's resolved Triage type(s) must already be persisted (see
/// `Store::set_cell_triage_types`) -- classification itself
/// (`crate::arbiter::classify`) runs earlier in the submit pipeline, before
/// this is called; a cell with no resolved type yet is skipped rather than
/// pooled with an unknown type (should not happen in the normal submit path).
///
/// A firing pool's reviews ask `orderer` for a semantic order of their
/// candidates (RAL-412) exactly like the cron-drain path: see
/// [`fire_triage_pool_in_threshold_batches`]. Production callers pass the
/// configured-Arbiter request (`arbiter_pool_order`); tests substitute a
/// canned closure.
///
/// Race-safety: the threshold-check here and the scheduler's independent
/// cron-check race on the same pool, but both ultimately call
/// [`Store::drain_triage_pool`], a single atomic `DELETE ... RETURNING`
/// executed while holding the daemon's one `crate::store_lock::StoreHandle` (same
/// reliance every other cumulative-then-act sequence in this module makes) --
/// whichever caller drains first empties the pool for the other, so no cell
/// is ever double-counted across two forced reviews.
///
/// # Errors
/// Returns [`ReviewError`] for the same class of problems [`derive_reviews`]
/// does: a missing/unresolvable worktree, or no upstream tracking branch.
pub fn derive_triage_pools(
    store: &Store,
    squad_id: &str,
    file: &TaskFile,
    orderer: impl Fn(&[crate::arbiter::OrderingCandidate]) -> Option<Vec<String>>,
) -> std::result::Result<Vec<String>, ReviewError> {
    if !file.task.iter().any(|t| t.cell.iter().any(|c| c.triage)) {
        return Ok(Vec::new());
    }
    let (mut cells, tasks, _cell_info) = rows_from_file(file);
    crate::worktrees::resolve_placeholders(store, squad_id, &mut cells, &tasks, &Context::new())
        .map_err(ReviewError::new)?;
    let tasks_by_idx: BTreeMap<i64, Option<&TaskRow>> =
        tasks.iter().map(|t| (t.idx, Some(t))).collect();

    let mut flat_cells: Vec<&ralphus_core::schema::CellDef> = Vec::new();
    for task in &file.task {
        for cell in &task.cell {
            flat_cells.push(cell);
        }
    }

    let mut touched_keys: HashSet<(String, String)> = HashSet::new();
    // RAL-159 parity: cells that share a worktree with a Triage-opted-in cell
    // (e.g. a "finalize" cell at the worktree root sharing it with a "work"
    // cell that alone declares `triage = true`) must block the readiness
    // gate (`Store::mark_ready_branches_with_done_cells`) the same way an
    // implicit `derive_reviews` sibling does -- see the matching pass there.
    // Populated as each Triage-opted-in cell resolves its worktree root below,
    // then matched against every non-Triage cell in a second pass afterward.
    let mut explicit_roots: Vec<(PathBuf, String)> = Vec::new();
    for (cell_def, row) in flat_cells.iter().zip(cells.iter()) {
        if !cell_def.triage {
            continue;
        }
        let triage_types = store
            .get_cell_triage_types(squad_id, row.task_idx, row.idx)
            .map_err(|e| ReviewError::new(e.to_string()))?;
        if triage_types.is_empty() {
            continue;
        }
        let remote = remote_cell_derivation(
            store,
            row,
            row.cwd.as_deref(),
            tasks_by_idx.get(&row.task_idx).copied().flatten(),
        )?;
        if let Some(rd) = &remote {
            // A remote review-linked cell can declare its upstream on the
            // `[[review]]` block; Triage has no per-cell equivalent to
            // declare one on. Rather than guess or hard-error the whole
            // submission, a remote Triage cell is skipped with a clear
            // Cartographer note -- local-worktree Triage pooling is fully
            // supported; remote-machine Triage is a documented follow-up.
            crate::cartographer::Note::new("arbiter")
                .squad(squad_id)
                .cell(&row.cell_id)
                .emit(
                    store,
                    format!(
                        "cell \"{}\" runs on machine \"{}\" and opts into Triage; \
                         remote-machine Triage pooling is not yet supported, skipping",
                        row.cell_id, rd.machine
                    ),
                    serde_json::json!({}),
                );
            continue;
        }
        let Some(cwd) = row.cwd.as_deref() else {
            return Err(ReviewError::new(format!(
                "cell \"{}\" opts into triage but has no cwd",
                row.cell_id
            )));
        };
        let cwd_path = Path::new(cwd);
        let project =
            worktree_project(cwd_path).map_err(|e| ReviewError::new(format!("{cwd}: {e}")))?;
        let branch =
            worktree_branch(cwd_path).map_err(|e| ReviewError::new(format!("{cwd}: {e}")))?;
        if let Ok(root) = worktree_root(cwd_path) {
            explicit_roots.push((root, branch.clone()));
        }
        let upstream = worktree_upstream(cwd_path).map_err(|_| {
            ReviewError::new(format!(
                "{cwd}: Triage pooling requires a git upstream tracking branch for '{branch}', \
                 but none is configured (set one with 'git branch --set-upstream-to=<branch>')"
            ))
        })?;
        store
            .set_cell_review_branch(squad_id, row.task_idx, row.idx, &branch)
            .map_err(|e| ReviewError::new(e.to_string()))?;
        // RAL-346: resolve this cell's monorepo subproject state (already
        // written, whether human-declared or Arbiter-inferred, by the
        // submit-time/background steps that ran before this) so its pool
        // key(s) can include a subproject dimension when applicable --
        // NotApplicable/Unresolved both fall back to the plain
        // project-name key unchanged, keeping a single-project repo's
        // keying identical to its pre-RAL-346 behavior.
        let subproject_resolution = crate::triage::resolve_cell_subprojects(
            store,
            squad_id,
            row.task_idx,
            row.idx,
            &project,
        )
        .map_err(|e| ReviewError::new(e.to_string()))?;
        // RAL-318 Bug 3 fix: resolve to the registered project's stable name
        // when one's path matches this worktree root, rather than the raw
        // git-reported path -- keeps this key in agreement with whatever
        // `resolve_pool_key_input` computes for a threshold set against the
        // same project by name (`crate::server`'s pool/schedule handlers).
        let pool_keys = crate::triage::pool_keys_for_cell(store, &project, &subproject_resolution);
        // A cell resolved to more than one type (inline `triage_type` list,
        // or a multi-type Arbiter classification) is pooled into every one
        // of its types' `(project, triage_type)` pools independently --
        // draining one pool never removes it from the others, since each is
        // its own row in `triage_pool_cells`. A cell resolved to more than
        // one subproject (RAL-346) is likewise pooled into every one of
        // `pool_keys`' composite keys independently -- the cross product of
        // pool keys x triage types is what gives two cells a "shared impact"
        // overlap test rather than requiring an exact-set match.
        for pool_key in &pool_keys {
            for triage_type in &triage_types {
                store
                    .record_triage_pool_cell(
                        pool_key,
                        triage_type,
                        squad_id,
                        row.task_idx,
                        row.idx,
                        &branch,
                        &upstream,
                    )
                    .map_err(|e| ReviewError::new(e.to_string()))?;
                crate::cartographer::Note::new("arbiter")
                    .squad(squad_id)
                    .cell(&row.cell_id)
                    .emit(
                        store,
                        format!(
                            "cell \"{}\" pooled for Triage type {triage_type:?}",
                            row.cell_id
                        ),
                        serde_json::json!({"project": pool_key, "triage_type": triage_type}),
                    );
                touched_keys.insert((pool_key.clone(), triage_type.clone()));
            }
        }
    }

    // Second pass (RAL-159 parity, see the comment on `explicit_roots` above):
    // any cell that did NOT itself opt into Triage, but whose cwd resolves to
    // the same worktree root as one that did, gets that same branch recorded
    // as its `review_branch` too -- purely so the readiness gate waits for it.
    // It is deliberately never added to the triage pool itself (only actual
    // Triage-opted-in cells are pool members); `reviews_by_branch`'s existing
    // `cells.review_branch = guardian_branches.branch` fallback join is what
    // then also surfaces it in the board's "in reviews" list once the pool
    // fires and the branch is attached to a guardian, with no further wiring
    // needed here.
    if !explicit_roots.is_empty() {
        for (cell_def, row) in flat_cells.iter().zip(cells.iter()) {
            if cell_def.triage {
                continue; // already handled above
            }
            let Some(cwd) = row.cwd.as_deref() else {
                continue;
            };
            let Ok(root) = worktree_root(Path::new(cwd)) else {
                continue;
            };
            if let Some((_, branch)) = explicit_roots.iter().find(|(r, _)| *r == root) {
                store
                    .set_cell_review_branch(squad_id, row.task_idx, row.idx, branch)
                    .map_err(|e| ReviewError::new(e.to_string()))?;
            }
        }
    }

    let mut created = Vec::new();
    for (project, triage_type) in touched_keys {
        let count = store
            .triage_pool_count(&project, &triage_type)
            .map_err(|e| ReviewError::new(e.to_string()))?;
        let threshold = store
            .get_triage_pool_threshold(&project, &triage_type)
            .map_err(|e| ReviewError::new(e.to_string()))?;
        if threshold.is_some_and(|t| count >= t) {
            if let Some(gid) =
                create_review_from_triage_pool(store, &project, &triage_type, &orderer)?
            {
                created.push(gid);
            }
        }
    }
    Ok(created)
}

/// Re-check every Triage pool touched by `squad_id` after one of its tasks
/// completes. Cells enter a pool at submission time so they remain visible as
/// scheduled candidates, but its count threshold must not create a review
/// until enough cells have actually completed successfully.
pub(crate) fn fire_ready_triage_thresholds(
    store: &Store,
    squad_id: &str,
) -> std::result::Result<Vec<String>, ReviewError> {
    let mut keys = HashSet::new();
    for (project, triage_type, row) in store
        .all_pooled_cells()
        .map_err(|e| ReviewError::new(e.to_string()))?
    {
        if row.squad_id == squad_id {
            keys.insert((project, triage_type));
        }
    }

    let mut created = Vec::new();
    for (project, triage_type) in keys {
        let count = store
            .triage_pool_count(&project, &triage_type)
            .map_err(|e| ReviewError::new(e.to_string()))?;
        let threshold = store
            .get_triage_pool_threshold(&project, &triage_type)
            .map_err(|e| ReviewError::new(e.to_string()))?;
        if threshold.is_some_and(|t| count >= t) {
            if let Some(gid) =
                create_review_from_triage_pool(store, &project, &triage_type, |cands| {
                    arbiter_pool_order(store, cands)
                })?
            {
                created.push(gid);
            }
        }
    }
    Ok(created)
}

/// Drain the `(project, triage_type)` pool and, if it wasn't already emptied
/// by a concurrent caller (the scheduler's cron tick, or another submission's
/// own threshold check), create a fresh review guardian from its cells.
/// Returns `None` when the pool was already empty by the time this drained it
/// -- not an error, just "someone else already fired it".
///
/// The review asks `orderer` for a semantic order of its candidates
/// (RAL-412): one aggregate Arbiter request carrying every candidate's
/// bounded prompt context, applied when the reply is a valid permutation,
/// otherwise the deterministic pool order.
///
/// `project` may be a plain project key or a RAL-346 `base::subproject`
/// composite key -- either way the guardian's `git_root`/project identity
/// resolves off [`crate::triage::base_project_key`], since a subproject is
/// never itself a separately registered project.
///
/// # Errors
/// Returns [`ReviewError`] on any store failure while creating the guardian
/// or attaching its branches.
pub(crate) fn create_review_from_triage_pool(
    store: &Store,
    project: &str,
    triage_type: &str,
    orderer: impl Fn(&[crate::arbiter::OrderingCandidate]) -> Option<Vec<String>>,
) -> std::result::Result<Option<String>, ReviewError> {
    let drained = store
        .drain_triage_pool(project, triage_type)
        .map_err(|e| ReviewError::new(e.to_string()))?;
    build_review_from_drained_pool(store, project, triage_type, drained, orderer)
}

/// The production RAL-412 ordering request: ask the currently configured
/// Arbiter (`crate::arbiter::Arbiter::current()`) to propose a semantic
/// review order over the drained pool's stable candidate ids. Returns
/// `None` on every failure path (budget cap, unsupported backend, transport
/// error, or a reply that is not an exact permutation), which
/// [`build_review_from_drained_pool`] treats as "keep the deterministic
/// pool order".
#[must_use]
fn arbiter_pool_order(
    store: &Store,
    candidates: &[crate::arbiter::OrderingCandidate],
) -> Option<Vec<String>> {
    crate::arbiter::order_pooled_candidates(store, &crate::arbiter::Arbiter::current(), candidates)
}

/// Reorder `drained` to the Arbiter's proposed order (RAL-412): rows named
/// in `proposed` (the pool's stable candidate ids, see
/// [`crate::arbiter::ordering_candidate_id`]) move to `proposed`'s
/// positions; any row not named — impossible for a validated permutation,
/// defended against anyway for a misbehaving `orderer` — keeps its
/// original pool-relative position. Every drained row appears exactly once
/// in the result, so the review's linear branch stack can never omit or
/// duplicate a pooled candidate.
#[must_use]
fn apply_arbiter_pool_order(
    drained: Vec<crate::triage::TriagePoolCellRow>,
    proposed: &[String],
) -> Vec<crate::triage::TriagePoolCellRow> {
    let mut placed: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut ordered: Vec<crate::triage::TriagePoolCellRow> = Vec::with_capacity(drained.len());
    for id in proposed {
        if !placed.insert(id.clone()) {
            continue; // duplicate in a broken proposal: first position wins
        }
        if let Some(cell) = drained.iter().find(|c| {
            crate::arbiter::ordering_candidate_id(&c.squad_id, c.task_idx, c.idx) == id.as_str()
        }) {
            ordered.push(cell.clone());
        }
    }
    for cell in drained {
        let id = crate::arbiter::ordering_candidate_id(&cell.squad_id, cell.task_idx, cell.idx);
        if !placed.contains(&id) {
            ordered.push(cell);
        }
    }
    ordered
}

/// RAL-346: the Arbiter's cron straggler sweep. Unlike
/// [`create_review_from_triage_pool`] (which fires exactly one pool key --
/// the coherent, threshold-driven unit of work), this drains *every* pool
/// key sharing `base_project`'s namespace for `triage_type` (the plain
/// `base_project` key plus every `base_project::subproject` composite key --
/// see [`crate::triage::project_pool_keys`]) and combines them into one
/// review, so straggler work sitting in per-subproject pools that never hit
/// their own count threshold (e.g. one bug fix each in `core`, `utils`, and
/// `steam`) doesn't get stranded indefinitely just because a schedule was
/// only ever registered against the project's plain base key. Called by
/// [`crate::triage::run_schedule_tick`] instead of
/// [`create_review_from_triage_pool`] when a configured cron schedule fires.
/// Returns `None` when every matching pool was already empty.
///
/// # Errors
/// Returns [`ReviewError`] on any store failure while creating the guardian
/// or attaching its branches.
pub(crate) fn create_review_from_triage_project_sweep(
    store: &Store,
    base_project: &str,
    triage_type: &str,
    orderer: impl Fn(&[crate::arbiter::OrderingCandidate]) -> Option<Vec<String>>,
) -> std::result::Result<Option<String>, ReviewError> {
    let keys = crate::triage::project_pool_keys(store, base_project, triage_type);
    let mut drained = Vec::new();
    for key in &keys {
        drained.extend(
            store
                .drain_triage_pool(key, triage_type)
                .map_err(|e| ReviewError::new(e.to_string()))?,
        );
    }
    build_review_from_drained_pool(store, base_project, triage_type, drained, orderer)
}

/// Shared tail of [`create_review_from_triage_pool`]/
/// [`create_review_from_triage_project_sweep`]: retain only completed cells
/// and -- if anything eligible remains -- build one fresh review guardian
/// from them. `pool_key` is used only for logging
/// (the Cartographer note and its payload); the *real* project identity
/// always resolves off [`crate::triage::base_project_key`].
///
/// RAL-412: with viable candidates in hand, every one contributes a bounded
/// prompt excerpt to one aggregate Arbiter ordering request (`orderer`),
/// and the proposed order — accepted only as an exact permutation of the
/// pool's stable candidate ids — becomes the order its branches are
/// attached in, i.e. the review's linear worktree/branch stack. `orderer`
/// returning `None` (or a reply that fails validation) leaves the
/// deterministic pool order, which is [`arbiter_pool_order`]'s default
/// behavior; tests substitute a canned closure. The negligible cost of a
/// failed/failed-validating request is that the review is simply created in
/// pool order — every candidate still appears exactly once either way.
fn build_review_from_drained_pool(
    store: &Store,
    pool_key: &str,
    triage_type: &str,
    drained: Vec<crate::triage::TriagePoolCellRow>,
    orderer: impl Fn(&[crate::arbiter::OrderingCandidate]) -> Option<Vec<String>>,
) -> std::result::Result<Option<String>, ReviewError> {
    // Defense in depth: `drain_triage_pool` already selects only completed
    // cells. Keep that invariant here in case a future caller supplies rows
    // without going through the drain filter.
    let mut viable = Vec::with_capacity(drained.len());
    for cell in drained {
        let effective = store
            .effective_state_for_cell(&cell.squad_id, cell.task_idx, cell.idx)
            .map_err(|e| ReviewError::new(e.to_string()))?;
        if effective.as_deref() == Some("done") {
            viable.push(cell);
        }
    }
    let drained = viable;
    if drained.is_empty() {
        return Ok(None);
    }
    // RAL-412: hand the Arbiter one bounded, labeled request covering every
    // candidate's prompt context, and apply a validated proposed order. A
    // cell whose prompt/command text can't be read (row absent, e.g. in a
    // test, or a store hiccup) still participates by stable id with an empty
    // excerpt -- it must never be dropped from the review.
    let candidates: Vec<crate::arbiter::OrderingCandidate> = drained
        .iter()
        .map(|cell| {
            let context = store
                .get_cell_prompt_context(&cell.squad_id, cell.task_idx, cell.idx)
                // A squad/cell row missing from the store (e.g. in a test
                // fixture) contributes an empty context, never an error.
                .unwrap_or_default()
                .unwrap_or_default();
            let cell_id = store
                .get_cell_id(&cell.squad_id, cell.task_idx, cell.idx)
                .unwrap_or_default();
            crate::arbiter::OrderingCandidate {
                id: crate::arbiter::ordering_candidate_id(&cell.squad_id, cell.task_idx, cell.idx),
                cell_id,
                context,
            }
        })
        .collect();
    let drained = match orderer(&candidates) {
        Some(proposed) => apply_arbiter_pool_order(drained, &proposed),
        None => drained,
    };
    let upstream = drained
        .first()
        .map(|c| c.upstream.clone())
        .unwrap_or_else(|| "main".to_string());
    let base_project = crate::triage::base_project_key(pool_key);
    let registered_project = store
        .get_project(base_project)
        .map_err(|e| ReviewError::new(e.to_string()))?;
    let project_root = registered_project.as_ref().map_or_else(
        || base_project.to_string(),
        |registered| registered.path.clone(),
    );
    // RAL-346: fold the subproject component (if any) into the review's
    // display name so an operator can tell a `core`-only auto-review apart
    // from a `utils`-only one at a glance, and a slash-separated subproject
    // path (`CellDef.subprojects`-style, e.g. `"packages/foo"`) doesn't
    // collide with the branch-naming conventions a plain `/` would imply.
    let name = match crate::triage::split_subproject_pool_key(pool_key) {
        Some((_, subproject)) => format!("triage-{triage_type}-{}", subproject.replace('/', "-")),
        None => format!("triage-{triage_type}"),
    };
    let gid = store
        .create_guardian_keyed(
            &name,
            &upstream,
            &project_root,
            None,
            None,
            registered_project
                .as_ref()
                .map(|registered| registered.name.as_str()),
        )
        .map_err(|e| ReviewError::new(e.to_string()))?;
    store
        .set_guardian_origin(&gid, crate::guardian::GUARDIAN_ORIGIN_ARBITER)
        .map_err(|e| ReviewError::new(e.to_string()))?;
    apply_project_review_defaults(store, &gid, &project_root)?;
    let mut seen: HashSet<String> = HashSet::new();
    for cell in &drained {
        if seen.insert(cell.branch.clone()) {
            store
                .add_guardian_branch_with_project(&gid, &cell.branch, None)
                .map_err(|e| ReviewError::new(e.to_string()))?;
        }
        store
            .set_cell_review_guardian(&cell.squad_id, cell.task_idx, cell.idx, &gid)
            .map_err(|e| ReviewError::new(e.to_string()))?;
    }
    crate::cartographer::Note::new("arbiter")
        .guardian(&gid)
        .emit(
            store,
            format!(
                "Triage pool ({pool_key}, {triage_type}) fired -> created review {gid} from {} cell(s)",
                drained.len()
            ),
            serde_json::json!({
                "project": pool_key,
                "triage_type": triage_type,
                "cell_count": drained.len(),
            }),
        );
    Ok(Some(gid))
}

/// Repair Arbiter reviews created with a registered project name in the
/// filesystem-facing `git_root` field. Triage pools use stable project names
/// as keys, while review worktrees and project config require the registered
/// absolute path.
pub fn repair_arbiter_review_project_roots(store: &Store) {
    let guardians = match store.list_guardians() {
        Ok(guardians) => guardians,
        Err(e) => {
            crate::cartographer::Note::new("recovery")
                .level(crate::logging::LogLevel::ERROR)
                .emit(
                    store,
                    format!("Arbiter review project-root repair failed to list reviews: {e}"),
                    serde_json::json!({"error": e.to_string()}),
                );
            return;
        }
    };
    for guardian in guardians {
        if guardian.origin != crate::guardian::GUARDIAN_ORIGIN_ARBITER {
            continue;
        }
        let registered = match store.get_project(&guardian.git_root) {
            Ok(Some(project)) => project,
            Ok(None) => continue,
            Err(e) => {
                crate::cartographer::Note::new("recovery")
                    .level(crate::logging::LogLevel::ERROR)
                    .guardian(&guardian.id)
                    .emit(
                        store,
                        format!("review project-root lookup failed: {e}"),
                        serde_json::json!({"error": e.to_string()}),
                    );
                continue;
            }
        };
        let repaired = match store.repair_arbiter_guardian_project_root(
            &guardian.id,
            &guardian.git_root,
            &registered.path,
        ) {
            Ok(repaired) => repaired,
            Err(e) => {
                crate::cartographer::Note::new("recovery")
                    .level(crate::logging::LogLevel::ERROR)
                    .guardian(&guardian.id)
                    .emit(
                        store,
                        format!("review project-root repair failed: {e}"),
                        serde_json::json!({"error": e.to_string()}),
                    );
                continue;
            }
        };
        if !repaired {
            continue;
        }
        if let Err(e) = store.set_guardian_project_if_unset(&guardian.id, &registered.name) {
            crate::cartographer::Note::new("recovery")
                .level(crate::logging::LogLevel::ERROR)
                .guardian(&guardian.id)
                .emit(
                    store,
                    format!("could not record project identity after project-root repair: {e}"),
                    serde_json::json!({"error": e.to_string()}),
                );
        }
        if let Err(e) = apply_project_review_defaults(store, &guardian.id, &registered.path) {
            crate::cartographer::Note::new("recovery")
                .level(crate::logging::LogLevel::ERROR)
                .guardian(&guardian.id)
                .emit(
                    store,
                    format!("could not apply project defaults after project-root repair: {e}"),
                    serde_json::json!({"error": e.to_string()}),
                );
        }
        crate::cartographer::Note::new("recovery")
            .level(crate::logging::LogLevel::WARNING)
            .guardian(&guardian.id)
            .emit(
                store,
                format!(
                    "review {} repaired Arbiter project root '{}' -> '{}'",
                    guardian.id, guardian.git_root, registered.path
                ),
                serde_json::json!({
                    "project": registered.name.as_str(),
                    "old_git_root": guardian.git_root.as_str(),
                    "git_root": registered.path.as_str(),
                    "old_status": guardian.status.as_str(),
                    "status": if guardian.status == "merge_failed" { "collecting" } else { guardian.status.as_str() },
                }),
            );
    }
}

/// Backfill the registered-project identity for reviews whose source tasks
/// unambiguously used one registered project. This uses submission provenance,
/// never path matching, so directory-backed reviews remain directory-backed.
pub fn repair_review_project_identities(store: &Store) {
    let guardians = match store.list_guardians() {
        Ok(guardians) => guardians,
        Err(e) => {
            crate::cartographer::Note::new("recovery")
                .level(crate::logging::LogLevel::ERROR)
                .emit(
                    store,
                    format!("review project-identity repair failed to list reviews: {e}"),
                    serde_json::json!({"error": e.to_string()}),
                );
            return;
        }
    };
    for guardian in guardians {
        if guardian.project.is_some() || guardian.branches.is_empty() {
            continue;
        }
        let mut project_name: Option<String> = None;
        let mut unambiguous = true;
        for branch in &guardian.branches {
            let Some(squad_id) = branch.source_squad_id.as_deref() else {
                unambiguous = false;
                break;
            };
            let Some(task_idx) = branch.source_task_idx else {
                unambiguous = false;
                break;
            };
            let registered = store
                .task_project_at(squad_id, task_idx)
                .ok()
                .flatten()
                .and_then(|name| store.resolve_project(&name).ok().flatten());
            let Some(registered) = registered else {
                unambiguous = false;
                break;
            };
            match project_name.as_deref() {
                None => project_name = Some(registered.name),
                Some(existing) if existing == registered.name => {}
                Some(_) => {
                    unambiguous = false;
                    break;
                }
            }
        }
        let Some(project_name) = project_name.filter(|_| unambiguous) else {
            continue;
        };
        match store.set_guardian_project_if_unset(&guardian.id, &project_name) {
            Ok(true) => crate::cartographer::Note::new("recovery")
                .level(crate::logging::LogLevel::WARNING)
                .guardian(&guardian.id)
                .emit(
                    store,
                    format!(
                        "review {} recovered registered project identity '{}' from its source tasks",
                        guardian.id, project_name
                    ),
                    serde_json::json!({"project": project_name}),
                ),
            Ok(false) => {}
            Err(e) => crate::cartographer::Note::new("recovery")
                .level(crate::logging::LogLevel::ERROR)
                .guardian(&guardian.id)
                .emit(
                    store,
                    format!("review project-identity repair failed: {e}"),
                    serde_json::json!({"error": e.to_string()}),
                ),
        }
    }
}

/// Startup repair (RAL-318 bug 3) for Triage pool/threshold/schedule keys
/// stored under the pre-fix path-based key instead of the resolved project
/// name (see `crate::triage::pool_key_for_path`). Called once per daemon
/// restart, alongside the other startup recovery passes in `server::serve`
/// -- naturally idempotent (once every row's key already matches its
/// corrected form, re-running this is a no-op, the same "runs every
/// startup, self-heals" idiom `Store::init_schema`'s other backfills use),
/// so there is no separate "have I run this" marker to track.
///
/// A pooled cell whose proof has definitively failed is dropped from the
/// pool outright rather than migrated (a failed cell can never pass review);
/// it remains visible forever in the Triage candidate list with a "failed"
/// status regardless (`Store::triage_candidates`). Every other pooled cell
/// is moved onto its corrected key. Thresholds and cron schedules are moved
/// the same way; if two old keys collide onto the same corrected key with
/// two *different* non-null threshold values, the corrected key's threshold
/// is left unset rather than guessed -- a threshold is always the user's
/// deliberate choice, never invented here -- and the conflict is logged so
/// it can be resolved explicitly via `ralphus triage pool threshold`.
/// Finally, every corrected key's count is checked against its (possibly
/// just-carried-forward) threshold, so a pool that's now counted correctly
/// and already past its threshold fires a review immediately, instead of
/// waiting for the next unrelated submission to touch that key.
pub fn repair_triage_pool_keys(store: &Store) {
    let mut touched_keys: HashSet<(String, String)> = HashSet::new();

    let cells = match store.all_pooled_cells() {
        Ok(c) => c,
        Err(e) => {
            // ralphus[ignore-rlog-pair]: transient startup recovery diagnostic; the repair loop emits its structured outcome per migrated cell
            crate::rlog!(
                ERROR,
                "ralphus [triage] pool-key repair: failed to list pooled cells: {e}"
            );
            Vec::new()
        }
    };
    for (old_project, triage_type, cell) in cells {
        let effective = match store.effective_state_for_cell(
            &cell.squad_id,
            cell.task_idx,
            cell.idx,
        ) {
            Ok(s) => s.unwrap_or_default(),
            Err(e) => {
                // ralphus[ignore-rlog-pair]: transient per-cell read diagnostic; migrated cells each log their own structured outcome
                crate::rlog!(
                    ERROR,
                    "ralphus [triage] pool-key repair: failed to read cell state for {}/{}/{}: {e}",
                    cell.squad_id,
                    cell.task_idx,
                    cell.idx
                );
                continue;
            }
        };
        if effective == "failed" {
            if let Err(e) = store.remove_triage_pool_cell(
                &old_project,
                &triage_type,
                &cell.squad_id,
                cell.task_idx,
                cell.idx,
            ) {
                crate::rlog!(
                    ERROR,
                    "ralphus [triage] pool-key repair: failed to drop failed cell from pool: {e}"
                );
            }
            continue;
        }
        let new_project = crate::triage::pool_key_for_path(store, Path::new(&old_project));
        if new_project != old_project {
            if let Err(e) = store.rekey_triage_pool_cell(
                &old_project,
                &triage_type,
                &cell.squad_id,
                cell.task_idx,
                cell.idx,
                &new_project,
            ) {
                crate::rlog!(
                    ERROR,
                    "ralphus [triage] pool-key repair: failed to rekey pooled cell: {e}"
                );
                continue;
            }
        }
        touched_keys.insert((new_project, triage_type));
    }

    let thresholds = match store.all_triage_pool_thresholds() {
        Ok(t) => t,
        Err(e) => {
            crate::rlog!(
                ERROR,
                "ralphus [triage] pool-key repair: failed to list thresholds: {e}"
            );
            Vec::new()
        }
    };
    let mut by_new_key: HashMap<(String, String), Vec<(String, i64)>> = HashMap::new();
    for (old_project, triage_type, value) in thresholds {
        let new_project = crate::triage::pool_key_for_path(store, Path::new(&old_project));
        by_new_key
            .entry((new_project, triage_type))
            .or_default()
            .push((old_project, value));
    }
    for ((new_project, triage_type), old_rows) in by_new_key {
        touched_keys.insert((new_project.clone(), triage_type.clone()));
        for (old_project, _) in &old_rows {
            if *old_project != new_project {
                let _ = store.set_triage_pool_threshold(old_project, &triage_type, None);
            }
        }
        let mut distinct_values: Vec<i64> = old_rows.iter().map(|(_, v)| *v).collect();
        distinct_values.sort_unstable();
        distinct_values.dedup();
        match distinct_values.as_slice() {
            [value] => {
                if let Err(e) =
                    store.set_triage_pool_threshold(&new_project, &triage_type, Some(*value))
                {
                    crate::rlog!(
                        ERROR,
                        "ralphus [triage] pool-key repair: failed to carry threshold forward for ({new_project}, {triage_type}): {e}"
                    );
                }
            }
            [] => {}
            _ => {
                let _ = store.set_triage_pool_threshold(&new_project, &triage_type, None);
                crate::rlog!(
                    WARNING,
                    "ralphus [triage] pool-key repair: ({new_project}, {triage_type}) had conflicting thresholds {old_rows:?} under different old keys -- left unset, set it explicitly with `ralphus triage pool threshold`"
                );
                crate::cartographer::Note::new("arbiter").emit(
                    store,
                    format!(
                        "Triage pool key repair found conflicting thresholds for ({new_project}, {triage_type}); left unset"
                    ),
                    serde_json::json!({
                        "project": new_project,
                        "triage_type": triage_type,
                        "conflicting": old_rows,
                    }),
                );
            }
        }
    }

    if let Ok(schedules) = store.list_triage_schedules(None) {
        for sched in schedules {
            let new_project = crate::triage::pool_key_for_path(store, Path::new(&sched.project));
            if new_project != sched.project {
                touched_keys.insert((new_project.clone(), sched.triage_type.clone()));
                if let Err(e) = store.rekey_triage_schedule(sched.id, &new_project) {
                    crate::rlog!(
                        ERROR,
                        "ralphus [triage] pool-key repair: failed to rekey schedule {}: {e}",
                        sched.id
                    );
                }
            }
        }
    }

    for (project, triage_type) in touched_keys {
        let count = match store.triage_pool_count(&project, &triage_type) {
            Ok(c) => c,
            Err(_) => continue,
        };
        let threshold = match store.get_triage_pool_threshold(&project, &triage_type) {
            Ok(t) => t,
            Err(_) => continue,
        };
        if threshold.is_some_and(|t| count >= t) {
            match create_review_from_triage_pool(store, &project, &triage_type, |cands| {
                arbiter_pool_order(store, cands)
            }) {
                Ok(Some(gid)) => crate::rlog!(
                    INFO,
                    "ralphus [triage] pool-key repair fired ({project}, {triage_type}) -> review {gid}"
                ),
                Ok(None) => {}
                Err(e) => crate::rlog!(
                    ERROR,
                    "ralphus [triage] pool-key repair: failed to fire ({project}, {triage_type}): {e}"
                ),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use std::sync::atomic::{AtomicU32, Ordering};

    use super::{
        Membership, any_workspace_ahead_of_upstream, apply_arbiter_pool_order, apply_auto_build,
        apply_project_review_defaults, apply_resolver, create_review_from_triage_pool,
        derive_reviews_with_prefetch, derive_triage_pools, fire_ready_triage_thresholds, plan,
        rebase_onto, repair_arbiter_review_project_roots, repair_review_project_identities,
        repair_triage_pool_keys, require_auto_build_declaration,
        require_auto_build_declaration_early, review_branch_order, rows_from_file,
        set_worktree_commit_baseline, workspace_has_commits_ahead_of_upstream,
        workspace_head_is_ancestor_of_upstream,
    };
    use crate::store::{Store, TaskRow};
    use crate::workspace::Workspace;

    fn completed_pool_cell(store: &mut Store) -> String {
        let file: ralphus_core::schema::TaskFile = toml::from_str(
            "[[task]]\nname=\"t\"\n[[task.cell]]\nid=\"work\"\ncwd=\"/repo\"\nprompt=\"do it\"\n",
        )
        .unwrap();
        let squad_id = store.insert_squad(&file, None, false).unwrap();
        store
            .set_cell_state(&squad_id, 0, 0, crate::store::NodeState::Done)
            .unwrap();
        squad_id
    }

    fn git(dir: &Path, args: &[&str]) -> String {
        let out = Command::new("git")
            .args(args)
            .current_dir(dir)
            .env("GIT_AUTHOR_NAME", "ralphus")
            .env("GIT_AUTHOR_EMAIL", "ralphus@example.com")
            .env("GIT_COMMITTER_NAME", "ralphus")
            .env("GIT_COMMITTER_EMAIL", "ralphus@example.com")
            .output()
            .expect("run git");
        assert!(
            out.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    // ── `review_branch_order` (RAL sprint batch 2026-09-11 review reordering) ──

    fn plan_cell(task_idx: i64) -> CellRow {
        CellRow {
            task_idx,
            idx: 0,
            task_name: format!("task{task_idx}"),
            cell_id: "work".to_string(),
            cwd: Some(".".to_string()),
            subprojects: vec![],
            prompt: None,
            command: Some("do".to_string()),
            agent: "claude".to_string(),
            model: None,
            system_prompt: None,
            system_prompt_position: None,
            depends_on: vec![],
            timeout_sec: None,
            budget_tokens: None,
            maximum_budget_usd: None,
            maximum_context: None,
            auto_compact_threshold: None,
            maximum_tool_output_tokens: None,
            upstream: None,
            machine: None,
            share_session: false,
        }
    }

    fn plan_task(idx: i64, deps: &[&str]) -> TaskRow {
        TaskRow {
            idx,
            name: format!("task{idx}"),
            project: None,
            depends_on: deps.iter().map(|s| (*s).to_string()).collect(),
            soloed: false,
        }
    }

    /// Reproduces guardian-000000000085 / squad-000000000133 exactly (13
    /// tasks, one cell each): three independent chains of different
    /// lengths — `ral-406`→`ral-401` (2), `ral-349`→`ral-350`→`ral-365`→
    /// `ral-345` (4), `ral-393`→`ral-392` (2) — plus five standalone tasks
    /// (`ral-403`, `ral-405`, `ral-402`, `ral-347`, `ral-348`), submitted in
    /// this exact interleaved order. `plan::topo_order`'s lowest-index
    /// tie-break reproduces plain submission order here — `403, 405, 406,
    /// 401, 349, 350, 365, 345, 402, 347, 348, 393, 392` — which is what
    /// the review actually shipped with: the 4-stage chain sits at
    /// positions 4-7 and blocks the stacked rebase from reaching the four
    /// already-ready standalone/short-chain branches behind it.
    ///
    /// `review_branch_order` must produce a *different*, still fully valid,
    /// topological order: standalone tasks and the two 2-stage chains
    /// bubble to the front, and the 4-stage chain — the long pole — sinks
    /// to the very back.
    #[test]
    fn review_branch_order_defers_the_longest_chain_past_shorter_and_standalone_tasks() {
        // Task index ↔ real name, for readability below:
        // 0 ral-403   1 ral-405   2 ral-406   3 ral-401   4 ral-349
        // 5 ral-350   6 ral-365   7 ral-345   8 ral-402   9 ral-347
        // 10 ral-348  11 ral-393  12 ral-392
        let tasks = vec![
            plan_task(0, &[]),
            plan_task(1, &[]),
            plan_task(2, &[]),
            plan_task(3, &["task2"]),
            plan_task(4, &[]),
            plan_task(5, &["task4"]),
            plan_task(6, &["task5"]),
            plan_task(7, &["task6"]),
            plan_task(8, &[]),
            plan_task(9, &[]),
            plan_task(10, &[]),
            plan_task(11, &[]),
            plan_task(12, &["task11"]),
        ];
        let cells: Vec<CellRow> = (0..13).map(plan_cell).collect();

        let execution = plan::plan(&cells, &tasks).expect("acyclic plan");
        // Confirm the premise: plain `topo_order` really does reproduce
        // submission order for this graph, so the improvement below isn't
        // an artifact of a mismatched fixture.
        assert_eq!(
            execution.order,
            vec![0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12],
            "fixture must reproduce the observed review's plain scheduling order"
        );

        let order = review_branch_order(&execution.deps, &execution.order);

        // Standalone tasks and both 2-stage chains come first (in their
        // original relative order); the 4-stage chain is deferred entirely
        // to the back, in its required internal order.
        assert_eq!(
            order,
            vec![0, 1, 8, 9, 10, 2, 3, 11, 12, 4, 5, 6, 7],
            "ral-403,405,402,347,348, ral-406,401, ral-393,392, ral-349,350,365,345"
        );

        // Every dependency edge must still be respected regardless of the
        // new tie-break: a task's position must come after all its deps'.
        let position: HashMap<usize, usize> =
            order.iter().enumerate().map(|(pos, &i)| (i, pos)).collect();
        for (i, prereqs) in execution.deps.iter().enumerate() {
            for &dep in prereqs {
                assert!(
                    position[&dep] < position[&i],
                    "cell {dep} must be positioned before dependent cell {i}"
                );
            }
        }

        // The long chain's own internal order must still hold: 349 < 350 < 365 < 345.
        let pos_of = |task_idx: usize| position[&task_idx];
        assert!(pos_of(4) < pos_of(5));
        assert!(pos_of(5) < pos_of(6));
        assert!(pos_of(6) < pos_of(7));

        // Deterministic: re-running on the same input always gives the same answer.
        for _ in 0..20 {
            assert_eq!(
                review_branch_order(&execution.deps, &execution.order),
                order
            );
        }
    }

    fn temp_repo() -> PathBuf {
        static N: AtomicU32 = AtomicU32::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("ralphus-rebase-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("mkdir");
        dir
    }

    // Dirty worktree + clean rebase: stash → rebase → pop restores changes.
    #[test]
    fn stashes_dirty_changes_before_rebase_and_restores_them_after() {
        let root = temp_repo();
        git(&root, &["init", "--initial-branch", "main"]);
        // `rebase_onto` below shells out through `GitVcs::exec_raw`, which
        // (correctly, for real repos) never injects an identity -- so this
        // throwaway repo needs one in its own local config, not just on this
        // test's own `git()` helper's per-invocation env vars, or the commit
        // `rebase_onto` creates fails identity checks on a CI runner with no
        // global gitconfig.
        git(&root, &["config", "user.name", "ralphus"]);
        git(&root, &["config", "user.email", "ralphus@example.com"]);
        // Stash pop runs restored content through the same clean/smudge
        // filters as a checkout -- on a Windows runner whose git defaults to
        // `core.autocrlf=true`, that silently turns this LF-written file
        // into CRLF, which has nothing to do with what's under test here
        // (whether ralphus's stash/rebase round-trips content at all).
        git(&root, &["config", "core.autocrlf", "false"]);
        std::fs::write(root.join("base.txt"), "base\n").unwrap();
        git(&root, &["add", "."]);
        git(&root, &["commit", "--message", "base"]);

        git(&root, &["checkout", "-b", "upstream"]);
        std::fs::write(root.join("upstream.txt"), "upstream\n").unwrap();
        git(&root, &["add", "."]);
        git(&root, &["commit", "--message", "upstream"]);

        git(&root, &["checkout", "main"]);
        git(&root, &["checkout", "-b", "work"]);
        std::fs::write(root.join("work.txt"), "work\n").unwrap();
        git(&root, &["add", "."]);
        git(&root, &["commit", "--message", "work"]);

        // Simulate a cell that stalled mid-flight: committed work + dirty file.
        std::fs::write(root.join("in_progress.txt"), "half done\n").unwrap();

        let result = rebase_onto(&root, "upstream");
        assert!(result.is_ok(), "rebase failed: {result:?}");

        // Committed files from both branches must be present.
        assert!(root.join("upstream.txt").exists());
        assert!(root.join("work.txt").exists());

        // The uncommitted file must be restored exactly.
        assert!(
            root.join("in_progress.txt").exists(),
            "in_progress.txt must be restored after rebase"
        );
        assert_eq!(
            std::fs::read_to_string(root.join("in_progress.txt")).unwrap(),
            "half done\n"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    // Dirty worktree + rebase conflict: stash → rebase aborts → pop preserves work.
    #[test]
    fn restores_stash_after_rebase_failure() {
        let root = temp_repo();
        git(&root, &["init", "--initial-branch", "main"]);
        // See the identical config in `stashes_dirty_changes_before_rebase_and_restores_them_after`
        // above: `rebase_onto` needs an identity, and stash pop must not let
        // a Windows runner's `core.autocrlf=true` default mangle line
        // endings out from under this test's exact-content assertion.
        git(&root, &["config", "user.name", "ralphus"]);
        git(&root, &["config", "user.email", "ralphus@example.com"]);
        git(&root, &["config", "core.autocrlf", "false"]);
        std::fs::write(root.join("conflict.txt"), "base\n").unwrap();
        git(&root, &["add", "."]);
        git(&root, &["commit", "--message", "base"]);

        git(&root, &["checkout", "-b", "upstream"]);
        std::fs::write(root.join("conflict.txt"), "from upstream\n").unwrap();
        git(&root, &["commit", "--all", "--message", "upstream"]);

        git(&root, &["checkout", "main"]);
        git(&root, &["checkout", "-b", "work"]);
        std::fs::write(root.join("conflict.txt"), "from work\n").unwrap();
        git(&root, &["commit", "--all", "--message", "work"]);

        // Leave an untracked in-progress file.
        std::fs::write(root.join("in_progress.txt"), "half done\n").unwrap();

        let result = rebase_onto(&root, "upstream");
        assert!(result.is_err(), "expected conflict-rebase to fail");

        // The in-progress file must survive the failed rebase.
        assert!(
            root.join("in_progress.txt").exists(),
            "in_progress.txt must be restored after failed rebase"
        );
        assert_eq!(
            std::fs::read_to_string(root.join("in_progress.txt")).unwrap(),
            "half done\n"
        );

        // Worktree must not be stuck in a mid-rebase state.
        let status = git(&root, &["status", "--porcelain"]);
        assert!(
            !status.contains("UU"),
            "no unmerged files after abort; status:\n{status}"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    // ── cell_upstream_display ─────────────────────────────────────────────

    use super::cell_upstream_display;
    use crate::store::CellRow;

    fn row(task_idx: i64, task_name: &str, cell_id: &str, cwd: Option<&Path>) -> CellRow {
        CellRow {
            task_idx,
            idx: 0,
            task_name: task_name.to_string(),
            cell_id: cell_id.to_string(),
            cwd: cwd.map(|p| p.to_string_lossy().into_owned()),
            subprojects: vec![],
            prompt: None,
            command: Some("x".to_string()),
            agent: "claude".to_string(),
            model: None,
            system_prompt: None,
            system_prompt_position: None,
            depends_on: vec![],
            timeout_sec: None,
            budget_tokens: None,
            maximum_budget_usd: None,
            maximum_context: None,
            auto_compact_threshold: None,
            maximum_tool_output_tokens: None,
            upstream: None,
            machine: None,
            share_session: false,
        }
    }

    #[test]
    fn upstream_display_is_none_for_non_git_cwd() {
        let dir = temp_repo(); // created but never `git init`'d
        let rows = [row(0, "t", "s", Some(&dir))];
        assert_eq!(
            cell_upstream_display(rows[0].cwd.as_deref(), &rows, 0, 0),
            None
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn upstream_display_falls_back_to_tracking_branch() {
        let root = temp_repo();
        git(&root, &["init", "--initial-branch", "main"]);
        std::fs::write(root.join("base.txt"), "base\n").unwrap();
        git(&root, &["add", "."]);
        git(&root, &["commit", "--message", "base"]);
        git(&root, &["checkout", "-b", "feature"]);
        git(&root, &["branch", "--set-upstream-to", "main"]);

        let rows = [row(0, "t", "s", Some(&root))];
        assert_eq!(
            cell_upstream_display(rows[0].cwd.as_deref(), &rows, 0, 0),
            Some("main".to_string())
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn upstream_display_resolves_chained_dependency_branch_over_tracking_ref() {
        let root = temp_repo();
        git(&root, &["init", "--initial-branch", "main"]);
        std::fs::write(root.join("base.txt"), "base\n").unwrap();
        git(&root, &["add", "."]);
        git(&root, &["commit", "--message", "base"]);

        let dep_wt = root.join("wt-dep");
        let work_wt = root.join("wt-work");
        git(
            &root,
            &[
                "worktree",
                "add",
                "-b",
                "dep-branch",
                dep_wt.to_str().unwrap(),
            ],
        );
        git(
            &root,
            &[
                "worktree",
                "add",
                "-b",
                "work-branch",
                work_wt.to_str().unwrap(),
            ],
        );
        // The work branch also tracks main — this must be ignored in favor of
        // the chained dependency's own branch.
        git(&work_wt, &["branch", "--set-upstream-to", "main"]);

        let dep_row = row(0, "dep-task", "work", Some(&dep_wt));
        let mut work_row = row(1, "work-task", "work", Some(&work_wt));
        work_row.upstream = Some("<<task:dep-task>>".to_string());
        let rows = [dep_row, work_row];

        assert_eq!(
            cell_upstream_display(rows[1].cwd.as_deref(), &rows, 1, 0),
            Some("dep-branch".to_string())
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn upstream_display_none_when_chained_dependency_not_yet_materialized() {
        let root = temp_repo();
        git(&root, &["init", "--initial-branch", "main"]);
        std::fs::write(root.join("base.txt"), "base\n").unwrap();
        git(&root, &["add", "."]);
        git(&root, &["commit", "--message", "base"]);
        git(&root, &["checkout", "-b", "work-branch"]);

        let dep_row = row(0, "dep-task", "work", None); // not materialized yet
        let mut work_row = row(1, "work-task", "work", Some(&root));
        work_row.upstream = Some("<<task:dep-task>>".to_string());
        let rows = [dep_row, work_row];

        assert_eq!(
            cell_upstream_display(rows[1].cwd.as_deref(), &rows, 1, 0),
            None,
            "must not fall back to the tracking ref when a chained dependency is declared but unresolved"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    // ── RAL-193: [[review]] maximum_budget_usd wiring ────────────────────

    fn membership(maximum_budget_usd: Option<f64>) -> Membership {
        Membership {
            task_idx: 0,
            idx: 0,
            project: PathBuf::from("/repo"),
            registered_project: None,
            branch: "feat".to_string(),
            upstream: "main".to_string(),
            name: "r".to_string(),
            order: 0,
            link_key: None,
            agent: None,
            model: None,
            machine: None,
            maximum_budget_usd,
            proof_scope: None,
            auto_submit_pr_stack: None,
            skip_worktrees: None,
            auto_pr_feedback: None,
            skip_base_updates: None,
            skip_auto_clean: None,
            match_pr_branch_name: None,
            separate_pr_branch: None,
            auto_build: Vec::new(),
            skip_auto_build: false,
            auto_fix_pr_errors: None,
            auto_fix_prompt_template: None,
        }
    }

    #[test]
    fn apply_resolver_sets_maximum_budget_usd_from_declaring_member() {
        let store = Store::open_in_memory().unwrap();
        let gid = store.create_guardian("r", "main", "/repo").unwrap();
        let m = membership(Some(3.5));
        apply_resolver(&store, &gid, &[&m]).unwrap();
        assert_eq!(store.guardian_maximum_budget_usd(&gid).unwrap(), Some(3.5));
    }

    #[test]
    fn apply_resolver_leaves_maximum_budget_usd_unset_when_no_member_declares_one() {
        let store = Store::open_in_memory().unwrap();
        let gid = store.create_guardian("r", "main", "/repo").unwrap();
        let m = membership(None);
        apply_resolver(&store, &gid, &[&m]).unwrap();
        assert_eq!(store.guardian_maximum_budget_usd(&gid).unwrap(), None);
    }

    // ── [[review]] proof_scope wiring ─────────────────────────────────────

    #[test]
    fn apply_resolver_sets_proof_scope_from_declaring_member() {
        let store = Store::open_in_memory().unwrap();
        let gid = store.create_guardian("r", "main", "/repo").unwrap();
        let m = Membership {
            proof_scope: Some("final_branch".to_string()),
            ..membership(None)
        };
        apply_resolver(&store, &gid, &[&m]).unwrap();
        assert_eq!(
            store.get_guardian(&gid).unwrap().proof_scope.as_deref(),
            Some("final_branch")
        );
    }

    #[test]
    fn apply_resolver_leaves_proof_scope_unset_when_no_member_declares_one() {
        let store = Store::open_in_memory().unwrap();
        let gid = store.create_guardian("r", "main", "/repo").unwrap();
        let m = membership(None);
        apply_resolver(&store, &gid, &[&m]).unwrap();
        assert_eq!(store.get_guardian(&gid).unwrap().proof_scope, None);
    }

    // ── [[review]] auto_submit_pr_stack wiring (RAL-317) ────────────────────

    #[test]
    fn apply_resolver_sets_auto_submit_pr_stack_from_declaring_member() {
        let store = Store::open_in_memory().unwrap();
        let gid = store.create_guardian("r", "main", "/repo").unwrap();
        let m = Membership {
            auto_submit_pr_stack: Some(true),
            ..membership(None)
        };
        apply_resolver(&store, &gid, &[&m]).unwrap();
        assert_eq!(
            store.get_guardian(&gid).unwrap().auto_submit_pr_stack,
            Some(true)
        );
    }

    #[test]
    fn apply_resolver_leaves_auto_submit_pr_stack_at_its_creation_stamp_when_no_member_declares_one()
     {
        let store = Store::open_in_memory().unwrap();
        let gid = store.create_guardian("r", "main", "/repo").unwrap();
        // `create_guardian` already stamps a concrete `Some(false)` at creation
        // (no project default here) -- `apply_resolver` must leave that alone.
        let before = store.get_guardian(&gid).unwrap().auto_submit_pr_stack;
        let m = membership(None);
        apply_resolver(&store, &gid, &[&m]).unwrap();
        assert_eq!(
            store.get_guardian(&gid).unwrap().auto_submit_pr_stack,
            before
        );
    }

    #[test]
    fn apply_resolver_sets_declared_review_settings() {
        let store = Store::open_in_memory().unwrap();
        let gid = store.create_guardian("r", "main", "/repo").unwrap();
        let m = Membership {
            skip_worktrees: Some(true),
            auto_pr_feedback: Some(true),
            skip_base_updates: Some(true),
            skip_auto_clean: Some(true),
            match_pr_branch_name: Some(true),
            separate_pr_branch: Some(true),
            ..membership(None)
        };
        apply_resolver(&store, &gid, &[&m]).unwrap();
        let guardian = store.get_guardian(&gid).unwrap();
        assert!(guardian.skip_worktrees);
        assert!(guardian.auto_pr_feedback);
        assert_eq!(guardian.skip_base_updates, Some(true));
        assert_eq!(guardian.proof_skip_auto_clean, Some(true));
        assert_eq!(guardian.match_pr_branch_name, Some(true));
        assert_eq!(guardian.separate_pr_branch, Some(true));
    }

    // ── [[review]] auto_build wiring / required-declaration (RAL-342) ────

    #[test]
    fn apply_auto_build_sets_auto_build_from_declaring_member() {
        let store = Store::open_in_memory().unwrap();
        let gid = store.create_guardian("r", "main", "/repo").unwrap();
        let def = ralphus_core::schema::AutoBuildDef {
            command: Some("make build".to_string()),
            ..Default::default()
        };
        let m = Membership {
            auto_build: vec![def],
            ..membership(None)
        };
        apply_auto_build(&store, &gid, &[&m]).unwrap();
        let stored = store.guardian_auto_build(&gid).unwrap();
        assert_eq!(
            stored.and_then(|b| b.command),
            Some("make build".to_string())
        );
        assert!(!store.guardian_skip_auto_build(&gid).unwrap());
    }

    #[test]
    fn apply_auto_build_sets_skip_auto_build_when_declared_and_no_auto_build_present() {
        let store = Store::open_in_memory().unwrap();
        let gid = store.create_guardian("r", "main", "/repo").unwrap();
        let m = Membership {
            skip_auto_build: true,
            ..membership(None)
        };
        apply_auto_build(&store, &gid, &[&m]).unwrap();
        assert!(store.guardian_skip_auto_build(&gid).unwrap());
        assert_eq!(store.guardian_auto_build(&gid).unwrap(), None);
    }

    #[test]
    fn apply_auto_build_leaves_unset_when_no_member_declares_either() {
        let store = Store::open_in_memory().unwrap();
        let gid = store.create_guardian("r", "main", "/repo").unwrap();
        let m = membership(None);
        apply_auto_build(&store, &gid, &[&m]).unwrap();
        assert_eq!(store.guardian_auto_build(&gid).unwrap(), None);
        assert!(!store.guardian_skip_auto_build(&gid).unwrap());
    }

    #[test]
    fn require_auto_build_declaration_ok_when_member_declares_auto_build() {
        let def = ralphus_core::schema::AutoBuildDef {
            command: Some("make build".to_string()),
            ..Default::default()
        };
        let m = Membership {
            auto_build: vec![def],
            ..membership(None)
        };
        let root = temp_repo();
        let project = root.to_string_lossy().into_owned();
        let store = Store::open_in_memory().unwrap();
        assert!(require_auto_build_declaration(&store, &[&m], &[project], "r").is_ok());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn require_auto_build_declaration_ok_when_member_declares_skip() {
        let m = Membership {
            skip_auto_build: true,
            ..membership(None)
        };
        let root = temp_repo();
        let project = root.to_string_lossy().into_owned();
        let store = Store::open_in_memory().unwrap();
        assert!(require_auto_build_declaration(&store, &[&m], &[project], "r").is_ok());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn require_auto_build_declaration_ok_when_project_config_declares_default() {
        let root = temp_repo();
        std::fs::write(
            root.join(".ralphus.toml"),
            "[review]\nauto_build = \"make build\"\n",
        )
        .unwrap();
        let m = membership(None);
        let project = root.to_string_lossy().into_owned();
        let store = Store::open_in_memory().unwrap();
        assert!(require_auto_build_declaration(&store, &[&m], &[project], "r").is_ok());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn require_auto_build_declaration_err_when_nothing_declared_and_no_config_fallback() {
        let root = temp_repo();
        let m = membership(None);
        let project = root.to_string_lossy().into_owned();
        let store = Store::open_in_memory().unwrap();
        let err =
            require_auto_build_declaration(&store, &[&m], &[project], "ralphus:new-review/abc123")
                .unwrap_err();
        assert!(
            err.to_string().contains("ralphus:new-review/abc123"),
            "error must identify the pending review: {err}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn require_auto_build_declaration_err_when_link_group_only_partially_covered_by_config() {
        // Two distinct projects in a link group; only one has a project-level
        // `auto_build` default. A partial fallback doesn't count for the whole
        // group -- the merge could silently pick up the covered project's
        // default while running with none for the other.
        let covered = temp_repo();
        std::fs::write(
            covered.join(".ralphus.toml"),
            "[review]\nauto_build = \"make build\"\n",
        )
        .unwrap();
        let uncovered = temp_repo();
        let m = membership(None);
        let projects = [
            covered.to_string_lossy().into_owned(),
            uncovered.to_string_lossy().into_owned(),
        ];
        let store = Store::open_in_memory().unwrap();
        assert!(require_auto_build_declaration(&store, &[&m], &projects, "r").is_err());
        let _ = std::fs::remove_dir_all(&covered);
        let _ = std::fs::remove_dir_all(&uncovered);
    }

    // ── RAL-<pending>: `require_auto_build_declaration_early` ────────────
    // (the pre-`resolve_placeholders_with_prefetch` fast-fail pass) and the
    // end-to-end proof that `derive_reviews_with_prefetch` actually runs it
    // before paying for worktree materialization.

    /// A one-task, one-cell, one-link-review TOML referencing project `"proj"`
    /// (must already be registered on `store` by the caller), with `review_toml`
    /// spliced verbatim into the `[[review]]` block (e.g. `"skip_auto_build = true"`,
    /// or `""` for neither declared).
    fn link_review_toml(review_toml: &str) -> String {
        format!(
            "[[task]]\nname=\"t\"\nproject=\"proj\"\n\n  [[task.cell]]\n  \
             cwd=\"<<ralphus:new-worktree/feat-x?upstream=main>>\"\n  prompt=\"p\"\n  \
             review=\"<<ralphus:new-review/k>>\"\n\n[[review]]\nid=\"ralphus:new-review/k\"\n{review_toml}\n"
        )
    }

    #[test]
    fn require_auto_build_declaration_early_ok_when_auto_build_declared() {
        let file: ralphus_core::schema::TaskFile = toml::from_str(&link_review_toml(
            "[[review.auto_build]]\ncommand=\"make build\"\n",
        ))
        .unwrap();
        let (cells, tasks, cell_info) = rows_from_file(&file);
        let store = Store::open_in_memory().unwrap();
        assert!(
            require_auto_build_declaration_early(&store, &file, &tasks, &cells, &cell_info).is_ok()
        );
    }

    #[test]
    fn require_auto_build_declaration_early_ok_when_skip_auto_build_declared() {
        let file: ralphus_core::schema::TaskFile =
            toml::from_str(&link_review_toml("skip_auto_build = true")).unwrap();
        let (cells, tasks, cell_info) = rows_from_file(&file);
        let store = Store::open_in_memory().unwrap();
        assert!(
            require_auto_build_declaration_early(&store, &file, &tasks, &cells, &cell_info).is_ok()
        );
    }

    #[test]
    fn require_auto_build_declaration_early_err_when_neither_declared_and_no_config_fallback() {
        let root = temp_repo();
        let file: ralphus_core::schema::TaskFile = toml::from_str(&link_review_toml("")).unwrap();
        let (cells, tasks, cell_info) = rows_from_file(&file);
        let store = Store::open_in_memory().unwrap();
        store
            .register_project("proj", "", &root.to_string_lossy(), "git")
            .unwrap();
        let err = require_auto_build_declaration_early(&store, &file, &tasks, &cells, &cell_info)
            .unwrap_err();
        assert!(
            err.message.contains("ralphus:new-review/k"),
            "error must identify the pending review: {}",
            err.message
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn require_auto_build_declaration_early_ok_when_project_config_covers_it() {
        let root = temp_repo();
        std::fs::write(
            root.join(".ralphus.toml"),
            "[review]\nauto_build = \"make build\"\n",
        )
        .unwrap();
        let file: ralphus_core::schema::TaskFile = toml::from_str(&link_review_toml("")).unwrap();
        let (cells, tasks, cell_info) = rows_from_file(&file);
        let store = Store::open_in_memory().unwrap();
        store
            .register_project("proj", "", &root.to_string_lossy(), "git")
            .unwrap();
        assert!(
            require_auto_build_declaration_early(&store, &file, &tasks, &cells, &cell_info).is_ok()
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn require_auto_build_declaration_early_skips_plain_name_reviews() {
        // A plain (non-link) review's final membership can still be SPLIT by
        // *resolved* git root once worktrees exist -- this function must not
        // guess at that early and risk a false-positive rejection; it defers
        // entirely to the late `require_auto_build_declaration` check.
        let toml = "[[task]]\nname=\"t\"\nproject=\"proj\"\n\n  [[task.cell]]\n  \
                    cwd=\"<<ralphus:new-worktree/feat-x?upstream=main>>\"\n  prompt=\"p\"\n  \
                    review=\"<<review:backend>>\"\n\n[[review]]\nid=\"backend\"\n";
        let file: ralphus_core::schema::TaskFile = toml::from_str(toml).unwrap();
        let (cells, tasks, cell_info) = rows_from_file(&file);
        let root = temp_repo();
        let store = Store::open_in_memory().unwrap();
        store
            .register_project("proj", "", &root.to_string_lossy(), "git")
            .unwrap();
        assert!(
            require_auto_build_declaration_early(&store, &file, &tasks, &cells, &cell_info).is_ok(),
            "a plain-name review must be left to the late check, never rejected early"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn derive_reviews_with_prefetch_rejects_missing_auto_build_before_materializing_any_worktree() {
        let root = temp_repo();
        git(&root, &["init", "--initial-branch", "main"]);
        std::fs::write(root.join("base.txt"), "base\n").unwrap();
        git(&root, &["add", "."]);
        git(&root, &["commit", "-m", "base"]);

        let store = Store::open_in_memory().unwrap();
        store
            .register_project("proj", "", &root.to_string_lossy(), "git")
            .unwrap();
        let file: ralphus_core::schema::TaskFile = toml::from_str(&link_review_toml("")).unwrap();

        let err =
            derive_reviews_with_prefetch(&store, "squad-1", &file, &HashMap::new()).unwrap_err();
        assert!(
            err.message.contains("ralphus:new-review/k"),
            "must fail on the missing auto_build declaration, not something else: {}",
            err.message
        );
        // The real proof this runs BEFORE `resolve_placeholders_with_prefetch`:
        // no worktree/branch was ever created for the rejected review's cell.
        assert!(
            !crate::worktrees::worktree_dir(&root, "feat-x").exists(),
            "the early auto_build check must reject before any worktree is materialized"
        );
        assert!(
            git(&root, &["branch", "--list", "feat-x"]).is_empty(),
            "no branch should have been created either"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    // ── RAL-293: worktree_has_commits_ahead_of_upstream ──────────────────

    #[test]
    fn false_when_head_equals_upstream() {
        let root = temp_repo();
        git(&root, &["init", "--initial-branch", "main"]);
        std::fs::write(root.join("base.txt"), "base\n").unwrap();
        git(&root, &["add", "."]);
        git(&root, &["commit", "--message", "base"]);
        git(&root, &["branch", "base"]);
        git(&root, &["branch", "--set-upstream-to=base", "main"]);

        assert!(!workspace_has_commits_ahead_of_upstream(&Workspace::local(
            root.clone()
        )));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn true_when_head_has_a_commit_the_upstream_lacks() {
        let root = temp_repo();
        git(&root, &["init", "--initial-branch", "main"]);
        std::fs::write(root.join("base.txt"), "base\n").unwrap();
        git(&root, &["add", "."]);
        git(&root, &["commit", "--message", "base"]);
        git(&root, &["branch", "base"]);
        git(&root, &["branch", "--set-upstream-to=base", "main"]);

        std::fs::write(root.join("more.txt"), "more\n").unwrap();
        git(&root, &["add", "."]);
        git(&root, &["commit", "--message", "more"]);

        assert!(workspace_has_commits_ahead_of_upstream(&Workspace::local(
            root.clone()
        )));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn dependency_rebase_baseline_excludes_inherited_commits_from_task_progress() {
        let root = temp_repo();
        git(&root, &["init", "--initial-branch", "main"]);
        std::fs::write(root.join("base.txt"), "base\n").unwrap();
        git(&root, &["add", "."]);
        git(&root, &["commit", "--message", "base"]);
        git(&root, &["checkout", "-b", "upstream"]);
        std::fs::write(root.join("upstream.txt"), "inherited\n").unwrap();
        git(&root, &["add", "."]);
        git(&root, &["commit", "--message", "upstream work"]);
        git(&root, &["checkout", "-b", "child", "main"]);

        rebase_onto(&root, "upstream").unwrap();
        set_worktree_commit_baseline(&root, "upstream").unwrap();
        assert!(
            !workspace_has_commits_ahead_of_upstream(&Workspace::local(root.clone())),
            "inherited upstream commits must not satisfy the child's no-commits guard"
        );

        std::fs::write(root.join("child.txt"), "child work\n").unwrap();
        git(&root, &["add", "."]);
        git(&root, &["commit", "--message", "child work"]);
        assert!(
            workspace_has_commits_ahead_of_upstream(&Workspace::local(root.clone())),
            "the child's own commit must satisfy the no-commits guard"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn true_regardless_of_which_run_made_the_commit() {
        // RAL-293: the whole point of switching to @{upstream} is that it
        // doesn't matter *when* the commit landed -- unlike a squad-run-scoped
        // baseline sha, this must read the same whether the commit was made
        // just now or by an earlier, unrelated run that reused this worktree.
        let root = temp_repo();
        git(&root, &["init", "--initial-branch", "main"]);
        std::fs::write(root.join("base.txt"), "base\n").unwrap();
        git(&root, &["add", "."]);
        git(&root, &["commit", "--message", "base"]);
        git(&root, &["branch", "base"]);
        git(&root, &["branch", "--set-upstream-to=base", "main"]);

        // Simulate a commit made by a prior squad run against this same
        // worktree, well before this check ever runs.
        std::fs::write(root.join("prior-run.txt"), "already done\n").unwrap();
        git(&root, &["add", "."]);
        git(&root, &["commit", "--message", "prior run's work"]);

        assert!(workspace_has_commits_ahead_of_upstream(&Workspace::local(
            root.clone()
        )));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn false_when_no_upstream_is_configured() {
        let root = temp_repo();
        git(&root, &["init", "--initial-branch", "main"]);
        std::fs::write(root.join("base.txt"), "base\n").unwrap();
        git(&root, &["add", "."]);
        git(&root, &["commit", "--message", "base"]);

        assert!(
            !workspace_has_commits_ahead_of_upstream(&Workspace::local(root.clone())),
            "no upstream must fail closed, not silently pass"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    // ── ralphus.<branch>.baseline: survives a finalize cell's `git push -u` ──

    #[test]
    fn true_after_push_dash_u_retargets_upstream_onto_the_branchs_own_remote() {
        // Reproduces the real-world false failure: a `finalize` cell's own
        // first-time `git push -u origin <branch>` (needed because a bare
        // `git push` refuses until *some* upstream exists) retargets
        // `branch.<branch>.remote`/`.merge` from the daemon-configured base
        // branch onto the branch's own just-pushed remote copy. Once that
        // happens `@{upstream}` always equals `HEAD`, which the guard would
        // read as "no progress" -- unless it consults the durable
        // `ralphus.<branch>.baseline` marker `set_explicit_upstream` writes
        // alongside `@{upstream}`, which this push never touches.
        let root = temp_repo();
        git(&root, &["init", "--initial-branch", "main"]);
        std::fs::write(root.join("base.txt"), "base\n").unwrap();
        git(&root, &["add", "."]);
        git(&root, &["commit", "--message", "base"]);
        // Stand in for `ensure_worktree("feature-x", "main")`: a feature
        // branch forked from `main`, with the daemon's real config writes
        // (`branch.<b>.merge` -- i.e. the pre-push `@{upstream}` -- and the
        // durable `ralphus.<b>.baseline` marker) both pointed at `main`.
        git(&root, &["checkout", "-b", "feature-x"]);
        git(&root, &["config", "branch.feature-x.remote", "."]);
        git(
            &root,
            &["config", "branch.feature-x.merge", "refs/heads/main"],
        );
        git(
            &root,
            &["config", "ralphus.feature-x.baseline", "refs/heads/main"],
        );

        std::fs::write(root.join("work.txt"), "work\n").unwrap();
        git(&root, &["add", "."]);
        git(&root, &["commit", "--message", "real work"]);

        // A bare local "remote" to push to, plus the push itself with `-u`,
        // exactly as an agent reaches for on a branch with no upstream yet.
        let remote = temp_repo();
        git(&remote, &["init", "--bare", "--initial-branch=main"]);
        git(
            &root,
            &[
                "remote",
                "add",
                "origin",
                remote.to_str().expect("remote path"),
            ],
        );
        git(&root, &["push", "--set-upstream", "origin", "feature-x"]);

        // The push must have actually retargeted `@{upstream}` -- otherwise
        // this test isn't reproducing the bug at all.
        let upstream_after_push = git(
            &root,
            &[
                "rev-parse",
                "--abbrev-ref",
                "--symbolic-full-name",
                "@{upstream}",
            ],
        );
        assert_eq!(upstream_after_push.trim(), "origin/feature-x");

        assert!(
            workspace_has_commits_ahead_of_upstream(&Workspace::local(root.clone())),
            "the durable ralphus.<branch>.baseline marker must survive `git push -u` \
             retargeting @{{upstream}}, so real work isn't reported as no progress"
        );
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&remote);
    }

    #[test]
    fn head_is_ancestor_of_upstream_only_after_upstream_contains_it() {
        let root = temp_repo();
        git(&root, &["init", "--initial-branch", "main"]);
        std::fs::write(root.join("base.txt"), "base\n").unwrap();
        git(&root, &["add", "."]);
        git(&root, &["commit", "--message", "base"]);
        git(&root, &["branch", "upstream"]);
        git(&root, &["branch", "--set-upstream-to=upstream", "main"]);

        let workspace = Workspace::on(&root, None);
        assert!(workspace_head_is_ancestor_of_upstream(&workspace));
        std::fs::write(root.join("more.txt"), "more\n").unwrap();
        git(&root, &["add", "."]);
        git(&root, &["commit", "--message", "more"]);
        assert!(!workspace_head_is_ancestor_of_upstream(&workspace));
        git(&root, &["branch", "--force", "upstream", "HEAD"]);
        assert!(workspace_head_is_ancestor_of_upstream(&workspace));

        let _ = std::fs::remove_dir_all(&root);
    }

    /// A fake provider script that answers the exact sequence
    /// [`workspace_baseline_ref`]/[`workspace_has_commits_ahead_of_upstream`]
    /// issues against a `main` branch with no `ralphus.main.baseline` marker
    /// configured (so it falls through to `@{upstream}`), reporting
    /// `ahead_count` commits ahead of it.
    fn fake_baseline_check_provider(dir: &std::path::Path, ahead_count: u32) -> PathBuf {
        std::fs::create_dir_all(dir).unwrap();
        let py = dir.join("provider.py");
        std::fs::write(
            &py,
            format!(
                r#"import json, sys
req = json.load(sys.stdin)
args = req.get("args", [])
if args == ["rev-parse", "--abbrev-ref", "HEAD"]:
    result = {{"ok": True, "protocol_version": 1, "exit_code": 0, "stdout": "main"}}
elif args[:2] == ["config", "--get"]:
    result = {{"ok": True, "protocol_version": 1, "exit_code": 1, "stdout": ""}}
elif args == ["rev-parse", "--abbrev-ref", "--symbolic-full-name", "@{{upstream}}"]:
    result = {{"ok": True, "protocol_version": 1, "exit_code": 0, "stdout": "origin/main"}}
elif args[:2] == ["rev-list", "--count"]:
    result = {{"ok": True, "protocol_version": 1, "exit_code": 0, "stdout": "{ahead_count}"}}
else:
    result = {{"ok": False, "protocol_version": 1, "error": "unexpected args " + repr(args)}}
print(json.dumps(result))
"#
            ),
        )
        .unwrap();
        py
    }

    #[test]
    fn any_workspace_ahead_of_upstream_dispatches_a_remote_workspace_through_its_provider() {
        // RAL-355 Phase 8: the no-new-commits guard must read a *remote*
        // cell's workspace the same way it reads a local one -- proven here
        // end-to-end through a real registered provider program, not just by
        // trusting `Workspace::git`'s local branch (already covered above).
        let dir = std::env::temp_dir().join(format!("ral355-guard-ahead-{}", std::process::id()));
        let py = fake_baseline_check_provider(&dir, 2);
        let store = std::sync::Arc::new(crate::store_lock::StoreMutex::new(
            Store::open_in_memory().unwrap(),
        ));
        store
            .lock()
            .register_machine_provider(
                "guardtest",
                "",
                "python",
                &[py.to_string_lossy().into_owned()],
                crate::machines::PROTOCOL_VERSION,
                false,
            )
            .unwrap();
        let ws = Workspace::on("/remote/repo", Some("guardtest:A"))
            .with_store(std::sync::Arc::clone(&store));
        assert!(
            any_workspace_ahead_of_upstream(std::slice::from_ref(&ws)),
            "a remote workspace with commits ahead of its baseline must be reported as progress"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn any_workspace_ahead_of_upstream_is_false_when_no_workspace_has_progress() {
        let dir =
            std::env::temp_dir().join(format!("ral355-guard-no-ahead-{}", std::process::id()));
        let py = fake_baseline_check_provider(&dir, 0);
        let store = std::sync::Arc::new(crate::store_lock::StoreMutex::new(
            Store::open_in_memory().unwrap(),
        ));
        store
            .lock()
            .register_machine_provider(
                "guardtest2",
                "",
                "python",
                &[py.to_string_lossy().into_owned()],
                crate::machines::PROTOCOL_VERSION,
                false,
            )
            .unwrap();
        let ws = Workspace::on("/remote/repo", Some("guardtest2:A"))
            .with_store(std::sync::Arc::clone(&store));
        assert!(!any_workspace_ahead_of_upstream(std::slice::from_ref(&ws)));
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ── Triage pooling (RAL-318) ─────────────────────────────────────────────

    #[test]
    fn create_review_from_triage_pool_creates_an_arbiter_origin_guardian_and_drains_the_pool() {
        let mut store = Store::open_in_memory().unwrap();
        let squad_1 = completed_pool_cell(&mut store);
        let squad_2 = completed_pool_cell(&mut store);
        store
            .record_triage_pool_cell("proj", "security", &squad_1, 0, 0, "b1", "main")
            .unwrap();
        store
            .record_triage_pool_cell("proj", "security", &squad_2, 0, 0, "b2", "main")
            .unwrap();

        let gid = create_review_from_triage_pool(&store, "proj", "security", |_| None)
            .unwrap()
            .expect("pool was non-empty, must create a review");
        let g = store.get_guardian(&gid).unwrap();
        assert_eq!(g.origin, crate::guardian::GUARDIAN_ORIGIN_ARBITER);
        assert_eq!(g.git_root, "proj");
        assert_eq!(g.project, None);
        assert_eq!(g.branches.len(), 2);
        assert_eq!(store.triage_pool_count("proj", "security").unwrap(), 0);

        // Firing an already-drained pool is a no-op, not an error (the race
        // the scheduler tick and a concurrent submission's own threshold
        // check must both tolerate).
        assert!(
            create_review_from_triage_pool(&store, "proj", "security", |_| None)
                .unwrap()
                .is_none()
        );
    }

    /// RAL-412: the review's branch stack follows the Arbiter's proposed
    /// semantic order rather than pool membership order.
    #[test]
    fn create_review_from_triage_pool_applies_the_proposed_semantic_order_to_the_branch_stack() {
        let mut store = Store::open_in_memory().unwrap();
        let squad_1 = completed_pool_cell(&mut store);
        let squad_2 = completed_pool_cell(&mut store);
        let squad_3 = completed_pool_cell(&mut store);
        store
            .record_triage_pool_cell("proj", "security", &squad_1, 0, 0, "b1", "main")
            .unwrap();
        store
            .record_triage_pool_cell("proj", "security", &squad_2, 0, 0, "b2", "main")
            .unwrap();
        store
            .record_triage_pool_cell("proj", "security", &squad_3, 0, 0, "b3", "main")
            .unwrap();
        let gid = create_review_from_triage_pool(
            &store,
            "proj",
            "security",
            // A canned Arbiter reply proposing the exact reverse of pool order.
            |cands| Some(cands.iter().map(|c| c.id.clone()).rev().collect::<Vec<_>>()),
        )
        .unwrap()
        .expect("pool was non-empty, must create a review");
        let g = store.get_guardian(&gid).unwrap();
        assert_eq!(
            g.branches
                .iter()
                .map(|b| b.branch.clone())
                .collect::<Vec<_>>(),
            vec!["b3".to_string(), "b2".to_string(), "b1".to_string()],
            "the Arbiter's proposed order becomes the review's branch-stack order"
        );
    }

    /// RAL-412: a proposal that is not an exact permutation of the drained
    /// pool must fall back to the deterministic pool order -- every candidate
    /// still appears in the review exactly once, in pool order.
    #[test]
    fn create_review_from_triage_pool_falls_back_to_pool_order_for_an_invalid_proposal() {
        let mut store = Store::open_in_memory().unwrap();
        let squad_1 = completed_pool_cell(&mut store);
        let squad_2 = completed_pool_cell(&mut store);
        store
            .record_triage_pool_cell("proj", "security", &squad_1, 0, 0, "b1", "main")
            .unwrap();
        store
            .record_triage_pool_cell("proj", "security", &squad_2, 0, 0, "b2", "main")
            .unwrap();
        let gid = create_review_from_triage_pool(
            &store,
            "proj",
            "security",
            // Not a permutation: names an id outside the pool and omits one.
            |_| Some(vec!["made-up-id".to_string(), format!("{squad_1}/t0:c0")]),
        )
        .unwrap()
        .expect("pool was non-empty, must create a review");
        let g = store.get_guardian(&gid).unwrap();
        assert_eq!(
            g.branches
                .iter()
                .map(|b| b.branch.clone())
                .collect::<Vec<_>>(),
            vec!["b1".to_string(), "b2".to_string()],
            "an invalid proposal must not drop, duplicate, or reorder a candidate"
        );
    }

    /// RAL-412: the whole-pool (cron/straggler) drain hands the Arbiter every
    /// candidate's cell prompt context (prompt, else command), labeled with
    /// the candidate's stable id.
    #[test]
    fn build_review_from_drained_pool_carries_each_candidates_prompt_context() {
        let mut store = Store::open_in_memory().unwrap();
        let src = "[[task]]\nname=\"t\"\nproject=\"proj\"\n[[task.cell]]\nid=\"work\"\ncwd=\".\"\nprompt=\"fix the core module\"\n";
        let file: ralphus_core::schema::TaskFile = toml::from_str(src).unwrap();
        let squad_id = store.insert_squad(&file, None, false).unwrap();
        store
            .set_cell_state(&squad_id, 0, 0, crate::store::NodeState::Done)
            .unwrap();
        store
            .record_triage_pool_cell("proj", "security", &squad_id, 0, 0, "b-work", "main")
            .unwrap();
        let gid = create_review_from_triage_pool(&store, "proj", "security", |cands| {
            assert_eq!(cands.len(), 1, "one pooled cell, one candidate");
            assert_eq!(
                cands[0].id,
                format!("{squad_id}/t0:c0"),
                "the candidate id is the stable squad/task:cell form"
            );
            assert_eq!(
                cands[0].context, "fix the core module",
                "the cell's prompt is the candidate's context"
            );
            Some(vec![cands[0].id.clone()])
        })
        .unwrap()
        .expect("pool was non-empty, must create a review");
        let g = store.get_guardian(&gid).unwrap();
        assert_eq!(g.branches.len(), 1);
    }

    /// RAL-412: `apply_arbiter_pool_order` is the safety net that turns any
    /// proposal -- even one that is not an exact permutation -- into a
    /// complete, duplication-free candidate ordering: proposed ids reorder
    /// their rows, everything else keeps pool-relative order, and every row
    /// appears exactly once.
    #[test]
    fn apply_arbiter_pool_order_defends_against_an_incomplete_or_duplicate_proposal() {
        use crate::triage::TriagePoolCellRow;
        let row = |(squad, t, i, branch): (&str, i64, i64, &str)| TriagePoolCellRow {
            squad_id: squad.to_string(),
            task_idx: t,
            idx: i,
            branch: branch.to_string(),
            upstream: "main".to_string(),
        };
        let drained = vec![
            row(("squad-1", 0, 0, "b1")),
            row(("squad-2", 0, 0, "b2")),
            row(("squad-3", 0, 0, "b3")),
        ];
        // Valid permutation: b3 then b1 then b2.
        let ordered = apply_arbiter_pool_order(
            drained.clone(),
            &[
                "squad-3/t0:c0".to_string(),
                "squad-1/t0:c0".to_string(),
                "squad-2/t0:c0".to_string(),
            ],
        );
        assert_eq!(
            ordered.iter().map(|c| c.branch.clone()).collect::<Vec<_>>(),
            vec!["b3".to_string(), "b1".to_string(), "b2".to_string()]
        );
        // Broken proposal: duplicate of b2's id, one unknown id, b1 omitted.
        let ordered = apply_arbiter_pool_order(
            drained.clone(),
            &[
                "squad-3/t0:c0".to_string(),
                "squad-2/t0:c0".to_string(),
                "squad-2/t0:c0".to_string(),
                "not-a-candidate".to_string(),
            ],
        );
        assert_eq!(
            ordered.iter().map(|c| c.branch.clone()).collect::<Vec<_>>(),
            vec!["b3".to_string(), "b2".to_string(), "b1".to_string()],
            "the unproposed row keeps its place and nothing is dropped or duplicated"
        );
        assert_eq!(
            ordered.len(),
            drained.len(),
            "every drained candidate still appears exactly once"
        );
    }

    #[test]
    fn create_review_from_triage_pool_never_includes_a_failed_cell() {
        let mut store = Store::open_in_memory().unwrap();
        let file: ralphus_core::schema::TaskFile = toml::from_str(
            "[[task]]\nname=\"t\"\n[[task.cell]]\nid=\"work\"\ncwd=\"/repo\"\nprompt=\"do it\"\n[[task.cell.proof]]\ncommand=\"cargo test\"\n",
        )
        .unwrap();
        let failed_squad = store.insert_squad(&file, None, false).unwrap();
        store
            .set_cell_state(&failed_squad, 0, 0, crate::store::NodeState::Done)
            .unwrap();
        store
            .set_proof_state(
                &failed_squad,
                0,
                "cell",
                0,
                0,
                crate::store::NodeState::Failed,
            )
            .unwrap();

        store
            .record_triage_pool_cell("proj", "security", &failed_squad, 0, 0, "b-failed", "main")
            .unwrap();
        let ok_squad = completed_pool_cell(&mut store);
        store
            .record_triage_pool_cell("proj", "security", &ok_squad, 0, 0, "b-ok", "main")
            .unwrap();

        let gid = create_review_from_triage_pool(&store, "proj", "security", |_| None)
            .unwrap()
            .expect("one viable cell remains, must still create a review");
        let g = store.get_guardian(&gid).unwrap();
        assert_eq!(
            g.branches.len(),
            1,
            "the failed cell's branch must never be attached to the review"
        );
        assert_eq!(g.branches[0].branch, "b-ok");
    }

    // ── RAL-342/RAL-338: per-project auto-review defaults ────────────────────

    #[test]
    fn apply_project_review_defaults_fills_machine_and_maximum_budget_usd_from_project_config() {
        let root = temp_repo();
        std::fs::write(
            root.join(".ralphus.toml"),
            "[review]\ndefault_machine = \"ib:A\"\ndefault_maximum_budget_usd = 5.0\n",
        )
        .unwrap();
        let store = Store::open_in_memory().unwrap();
        let gid = store
            .create_guardian_for_squad("r", "main", &root.to_string_lossy(), None)
            .unwrap();

        apply_project_review_defaults(&store, &gid, &root.to_string_lossy()).unwrap();

        let g = store.get_guardian(&gid).unwrap();
        assert_eq!(g.machine.as_deref(), Some("ib:A"));
        assert_eq!(g.maximum_budget_usd, Some(5.0));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn apply_project_review_defaults_does_not_clobber_an_already_set_machine() {
        let root = temp_repo();
        std::fs::write(
            root.join(".ralphus.toml"),
            "[review]\ndefault_machine = \"ib:from-config\"\n",
        )
        .unwrap();
        let store = Store::open_in_memory().unwrap();
        let gid = store
            .create_guardian_for_squad("r", "main", &root.to_string_lossy(), None)
            .unwrap();
        store
            .set_guardian_machine(&gid, Some("ib:explicit"))
            .unwrap();

        apply_project_review_defaults(&store, &gid, &root.to_string_lossy()).unwrap();

        let g = store.get_guardian(&gid).unwrap();
        assert_eq!(
            g.machine.as_deref(),
            Some("ib:explicit"),
            "an already-set machine must never be overwritten by the project default"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn create_review_from_triage_pool_applies_project_default_machine_and_budget() {
        let root = temp_repo();
        std::fs::write(
            root.join(".ralphus.toml"),
            "[review]\ndefault_machine = \"ib:A\"\ndefault_maximum_budget_usd = 2.5\n",
        )
        .unwrap();
        let mut store = Store::open_in_memory().unwrap();
        store
            .register_project("proj", "", &root.to_string_lossy(), "git")
            .unwrap();
        let squad_id = completed_pool_cell(&mut store);
        store
            .record_triage_pool_cell("proj", "security", &squad_id, 0, 0, "b1", "main")
            .unwrap();

        let gid = create_review_from_triage_pool(&store, "proj", "security", |_| None)
            .unwrap()
            .expect("pool was non-empty, must create a review");

        let g = store.get_guardian(&gid).unwrap();
        assert_eq!(g.git_root, root.to_string_lossy());
        assert_eq!(g.project.as_deref(), Some("proj"));
        assert_eq!(
            g.machine.as_deref(),
            Some("ib:A"),
            "the Arbiter has no [[review]] block to declare a machine, so it must pick up \
             the project's default_machine"
        );
        assert_eq!(g.maximum_budget_usd, Some(2.5));
        let _ = std::fs::remove_dir_all(&root);
    }

    // ── RAL-395: per-project auto-fix defaults ────────────────────────────

    #[test]
    fn apply_project_review_defaults_fills_auto_fix_settings_from_project_config() {
        let root = temp_repo();
        std::fs::write(
            root.join(".ralphus.toml"),
            "[review]\nauto_fix_pr_errors = true\nauto_fix_prompt_template = \"fix: <<prompt>>\"\n",
        )
        .unwrap();
        let store = Store::open_in_memory().unwrap();
        let gid = store
            .create_guardian_for_squad("r", "main", &root.to_string_lossy(), None)
            .unwrap();

        apply_project_review_defaults(&store, &gid, &root.to_string_lossy()).unwrap();

        let g = store.get_guardian(&gid).unwrap();
        assert_eq!(g.auto_fix_pr_errors, Some(true));
        assert_eq!(
            g.auto_fix_prompt_template.as_deref(),
            Some("fix: <<prompt>>")
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn apply_project_review_defaults_does_not_clobber_an_already_set_auto_fix_template() {
        let root = temp_repo();
        std::fs::write(
            root.join(".ralphus.toml"),
            "[review]\nauto_fix_prompt_template = \"from-config: <<prompt>>\"\n",
        )
        .unwrap();
        let store = Store::open_in_memory().unwrap();
        let gid = store
            .create_guardian_for_squad("r", "main", &root.to_string_lossy(), None)
            .unwrap();
        store
            .set_guardian_auto_fix_prompt_template(&gid, Some("explicit: <<prompt>>"))
            .unwrap();

        apply_project_review_defaults(&store, &gid, &root.to_string_lossy()).unwrap();

        let g = store.get_guardian(&gid).unwrap();
        assert_eq!(
            g.auto_fix_prompt_template.as_deref(),
            Some("explicit: <<prompt>>"),
            "an already-set template must never be overwritten by the project default"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn create_review_from_triage_pool_applies_project_default_auto_fix_settings() {
        let root = temp_repo();
        std::fs::write(
            root.join(".ralphus.toml"),
            "[review]\nauto_fix_pr_errors = true\nauto_fix_prompt_template = \"pooled: <<prompt>>\"\n",
        )
        .unwrap();
        let mut store = Store::open_in_memory().unwrap();
        store
            .register_project("proj", "", &root.to_string_lossy(), "git")
            .unwrap();
        let squad_id = completed_pool_cell(&mut store);
        store
            .record_triage_pool_cell("proj", "security", &squad_id, 0, 0, "b1", "main")
            .unwrap();

        let gid = create_review_from_triage_pool(&store, "proj", "security", |_| None)
            .unwrap()
            .expect("pool was non-empty, must create a review");

        let g = store.get_guardian(&gid).unwrap();
        assert_eq!(
            g.auto_fix_pr_errors,
            Some(true),
            "the Arbiter has no [[review]] block to declare auto_fix_pr_errors, so it must \
             pick up the project's default unconditionally"
        );
        assert_eq!(
            g.auto_fix_prompt_template.as_deref(),
            Some("pooled: <<prompt>>")
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    // ── RAL-395: [[review]] auto_fix_pr_errors / auto_fix_prompt_template wiring ──

    #[test]
    fn apply_resolver_sets_auto_fix_pr_errors_from_declaring_member() {
        let store = Store::open_in_memory().unwrap();
        let gid = store.create_guardian("r", "main", "/repo").unwrap();
        let m = Membership {
            auto_fix_pr_errors: Some(true),
            ..membership(None)
        };
        apply_resolver(&store, &gid, &[&m]).unwrap();
        assert_eq!(
            store.get_guardian(&gid).unwrap().auto_fix_pr_errors,
            Some(true)
        );
    }

    #[test]
    fn apply_resolver_leaves_auto_fix_pr_errors_unset_when_no_member_declares_one() {
        let store = Store::open_in_memory().unwrap();
        let gid = store.create_guardian("r", "main", "/repo").unwrap();
        let m = membership(None);
        apply_resolver(&store, &gid, &[&m]).unwrap();
        assert_eq!(store.get_guardian(&gid).unwrap().auto_fix_pr_errors, None);
    }

    #[test]
    fn apply_resolver_sets_auto_fix_prompt_template_from_declaring_member() {
        let store = Store::open_in_memory().unwrap();
        let gid = store.create_guardian("r", "main", "/repo").unwrap();
        let m = Membership {
            auto_fix_prompt_template: Some("member: <<prompt>>".to_string()),
            ..membership(None)
        };
        apply_resolver(&store, &gid, &[&m]).unwrap();
        assert_eq!(
            store
                .get_guardian(&gid)
                .unwrap()
                .auto_fix_prompt_template
                .as_deref(),
            Some("member: <<prompt>>")
        );
    }

    #[test]
    fn repair_arbiter_review_project_roots_repairs_and_reopens_a_failed_review() {
        let root = temp_repo();
        let store = Store::open_in_memory().unwrap();
        store
            .register_project("proj", "", &root.to_string_lossy(), "git")
            .unwrap();
        let gid = store
            .create_guardian_for_squad("triage-bug", "main", "proj", None)
            .unwrap();
        store
            .set_guardian_origin(&gid, crate::guardian::GUARDIAN_ORIGIN_ARBITER)
            .unwrap();
        store
            .set_guardian_status(
                &gid,
                crate::guardian::GuardianStatus::MergeFailed,
                Some("invalid directory"),
            )
            .unwrap();

        repair_arbiter_review_project_roots(&store);

        let guardian = store.get_guardian(&gid).unwrap();
        assert_eq!(guardian.git_root, root.to_string_lossy());
        assert_eq!(guardian.project.as_deref(), Some("proj"));
        assert_eq!(guardian.status, "collecting");
        assert!(guardian.detail.is_none());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn repair_review_project_identities_uses_source_task_provenance() {
        let root = temp_repo();
        let mut store = Store::open_in_memory().unwrap();
        store
            .register_project("proj", "", &root.to_string_lossy(), "git")
            .unwrap();
        let file: ralphus_core::schema::TaskFile = toml::from_str(
            "[[task]]\nname=\"t\"\nproject=\"proj\"\n[[task.cell]]\ncwd=\"/repo\"\nprompt=\"do it\"\n",
        )
        .unwrap();
        let squad = store.insert_squad(&file, None, false).unwrap();
        let gid = store.create_guardian("r", "main", "/remote/repo").unwrap();
        store.add_guardian_branch(&gid, "feature").unwrap();
        store
            .set_cell_review_branch(&squad, 0, 0, "feature")
            .unwrap();

        repair_review_project_identities(&store);

        assert_eq!(
            store.get_guardian(&gid).unwrap().project.as_deref(),
            Some("proj")
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn repair_triage_pool_keys_rekeys_a_stale_path_based_pool_and_fires_when_now_past_threshold() {
        let root = temp_repo();
        let mut store = Store::open_in_memory().unwrap();
        store
            .register_project("proj", "", &root.to_string_lossy(), "git")
            .unwrap();

        let stale_key = root.to_string_lossy().replace('\\', "/");
        let squad_id = completed_pool_cell(&mut store);
        store
            .record_triage_pool_cell(&stale_key, "bug", &squad_id, 0, 0, "b1", "main")
            .unwrap();
        store
            .set_triage_pool_threshold(&stale_key, "bug", Some(1))
            .unwrap();

        repair_triage_pool_keys(&store);

        assert_eq!(store.triage_pool_count(&stale_key, "bug").unwrap(), 0);
        assert!(
            store
                .get_triage_pool_threshold(&stale_key, "bug")
                .unwrap()
                .is_none()
        );
        // Its single cell, now correctly counted under "proj", already met
        // its carried-forward threshold of 1 -- the repair fires a real
        // review immediately rather than waiting for a future submission.
        assert!(store.triage_pool_keys().unwrap().is_empty());
        let guardians = store.list_guardians().unwrap();
        assert_eq!(guardians.len(), 1);
        assert_eq!(
            guardians[0].origin,
            crate::guardian::GUARDIAN_ORIGIN_ARBITER
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn repair_triage_pool_keys_drops_a_failed_cell_without_migrating_it() {
        let root = temp_repo();
        let mut store = Store::open_in_memory().unwrap();
        store
            .register_project("proj", "", &root.to_string_lossy(), "git")
            .unwrap();
        let file: ralphus_core::schema::TaskFile = toml::from_str(
            "[[task]]\nname=\"t\"\n[[task.cell]]\nid=\"work\"\ncwd=\"/repo\"\nprompt=\"do it\"\n[[task.cell.proof]]\ncommand=\"cargo test\"\n",
        )
        .unwrap();
        let squad_id = store.insert_squad(&file, None, false).unwrap();
        store
            .set_cell_state(&squad_id, 0, 0, crate::store::NodeState::Done)
            .unwrap();
        store
            .set_proof_state(&squad_id, 0, "cell", 0, 0, crate::store::NodeState::Failed)
            .unwrap();

        let stale_key = root.to_string_lossy().replace('\\', "/");
        store
            .record_triage_pool_cell(&stale_key, "bug", &squad_id, 0, 0, "b1", "main")
            .unwrap();

        repair_triage_pool_keys(&store);

        assert!(
            store.triage_pool_keys().unwrap().is_empty(),
            "a failed cell must be dropped from the pool, never migrated to the corrected key"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn repair_triage_pool_keys_carries_forward_a_single_consistent_threshold() {
        let root = temp_repo();
        let store = Store::open_in_memory().unwrap();
        store
            .register_project("proj", "", &root.to_string_lossy(), "git")
            .unwrap();
        let key_a = root.to_string_lossy().replace('\\', "/");
        // Appending `/.` keeps the raw database keys byte-distinct on every
        // platform while both paths still canonicalize to the same directory.
        let key_b = format!("{key_a}/.");
        assert_ne!(key_a, key_b);
        store
            .set_triage_pool_threshold(&key_a, "bug", Some(3))
            .unwrap();
        store
            .set_triage_pool_threshold(&key_b, "bug", Some(3))
            .unwrap();

        repair_triage_pool_keys(&store);

        assert_eq!(
            store.get_triage_pool_threshold("proj", "bug").unwrap(),
            Some(3)
        );
        assert!(
            store
                .get_triage_pool_threshold(&key_a, "bug")
                .unwrap()
                .is_none()
        );
        assert!(
            store
                .get_triage_pool_threshold(&key_b, "bug")
                .unwrap()
                .is_none()
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn repair_triage_pool_keys_leaves_conflicting_thresholds_unset() {
        let root = temp_repo();
        let store = Store::open_in_memory().unwrap();
        store
            .register_project("proj", "", &root.to_string_lossy(), "git")
            .unwrap();
        let key_a = root.to_string_lossy().replace('\\', "/");
        // Appending `/.` keeps the raw database keys byte-distinct on every
        // platform while both paths still canonicalize to the same directory.
        let key_b = format!("{key_a}/.");
        assert_ne!(key_a, key_b);
        store
            .set_triage_pool_threshold(&key_a, "bug", Some(3))
            .unwrap();
        store
            .set_triage_pool_threshold(&key_b, "bug", Some(5))
            .unwrap();

        repair_triage_pool_keys(&store);

        assert!(
            store
                .get_triage_pool_threshold("proj", "bug")
                .unwrap()
                .is_none(),
            "conflicting thresholds set under different old keys must never be silently merged"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    fn task_file_toml(cwd: &Path, triage_type: &str) -> String {
        let cwd = cwd.to_string_lossy().replace('\\', "/");
        format!(
            "[[task]]\nname=\"t\"\nproject=\"proj\"\n[[task.cell]]\nid=\"work\"\ncwd=\"{cwd}\"\nprompt=\"do it\"\ntriage=true\ntriage_type=\"{triage_type}\"\n"
        )
    }

    #[test]
    fn derive_triage_pools_pools_a_local_cell_and_fires_on_threshold() {
        let root = temp_repo();
        git(&root, &["init", "--initial-branch", "main"]);
        std::fs::write(root.join("base.txt"), "base\n").unwrap();
        git(&root, &["add", "."]);
        git(&root, &["commit", "--message", "base"]);
        git(&root, &["checkout", "-b", "feature"]);
        git(&root, &["branch", "--set-upstream-to", "main"]);

        let mut store = Store::open_in_memory().unwrap();
        store
            .register_triage_type("security", "Security", "")
            .unwrap();
        store
            .register_project("proj", "", &root.to_string_lossy(), "git")
            .unwrap();

        let src = task_file_toml(&root, "security");
        let file: ralphus_core::schema::TaskFile = toml::from_str(&src).unwrap();
        let squad_id = store.insert_squad(&file, None, false).unwrap();
        // Simulate the submit pipeline's earlier classification step, which
        // always runs before `derive_triage_pools`.
        store
            .set_cell_triage_types(&squad_id, 0, 0, &["security".to_string()])
            .unwrap();

        // No threshold configured yet: pools, but does not fire.
        let created = derive_triage_pools(&store, &squad_id, &file, |_| None).unwrap();
        assert!(created.is_empty());
        let keys = store.triage_pool_keys().unwrap();
        assert_eq!(keys.len(), 1);
        let (project, triage_type) = keys[0].clone();
        assert_eq!(
            project, "proj",
            "pool key must resolve to the registered project's name (RAL-318 bug 3), not its raw worktree path"
        );
        assert_eq!(triage_type, "security");
        assert_eq!(store.triage_pool_count(&project, &triage_type).unwrap(), 0);

        store
            .set_cell_state(&squad_id, 0, 0, crate::store::NodeState::Done)
            .unwrap();
        assert_eq!(store.triage_pool_count(&project, &triage_type).unwrap(), 1);

        // A second completed candidate brings the configured threshold to two.
        store
            .set_triage_pool_threshold(&project, &triage_type, Some(2))
            .unwrap();
        let squad_id_2 = store.insert_squad(&file, None, false).unwrap();
        store
            .set_cell_triage_types(&squad_id_2, 0, 0, &["security".to_string()])
            .unwrap();
        let created = derive_triage_pools(&store, &squad_id_2, &file, |_| None).unwrap();
        assert!(
            created.is_empty(),
            "pending work must not fire the threshold"
        );
        store
            .set_cell_state(&squad_id_2, 0, 0, crate::store::NodeState::Done)
            .unwrap();
        let created = fire_ready_triage_thresholds(&store, &squad_id_2).unwrap();
        assert_eq!(created.len(), 1);
        let g = store.get_guardian(&created[0]).unwrap();
        assert_eq!(g.origin, crate::guardian::GUARDIAN_ORIGIN_ARBITER);
        assert_eq!(
            g.branches.len(),
            1,
            "both pooled cells share the same worktree branch"
        );
        assert_eq!(store.triage_pool_count(&project, &triage_type).unwrap(), 0);

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn derive_triage_pools_pools_a_multi_type_cell_into_every_type_independently() {
        let root = temp_repo();
        git(&root, &["init", "--initial-branch", "main"]);
        std::fs::write(root.join("base.txt"), "base\n").unwrap();
        git(&root, &["add", "."]);
        git(&root, &["commit", "--message", "base"]);
        git(&root, &["checkout", "-b", "feature"]);
        git(&root, &["branch", "--set-upstream-to", "main"]);

        let mut store = Store::open_in_memory().unwrap();
        store
            .register_project("proj", "", &root.to_string_lossy(), "git")
            .unwrap();

        let cwd = root.to_string_lossy().replace('\\', "/");
        let src = format!(
            "[[task]]\nname=\"t\"\nproject=\"proj\"\n[[task.cell]]\nid=\"work\"\ncwd=\"{cwd}\"\nprompt=\"do it\"\ntriage=true\ntriage_type=[\"bug\",\"investigation\"]\n"
        );
        let file: ralphus_core::schema::TaskFile = toml::from_str(&src).unwrap();
        let squad_id = store.insert_squad(&file, None, false).unwrap();
        // Simulate the submit pipeline's earlier classification step, which
        // always runs before `derive_triage_pools`.
        store
            .set_cell_triage_types(
                &squad_id,
                0,
                0,
                &["bug".to_string(), "investigation".to_string()],
            )
            .unwrap();

        let created = derive_triage_pools(&store, &squad_id, &file, |_| None).unwrap();
        assert!(created.is_empty(), "no threshold configured yet");
        // The pool key's `project` resolves to the registered project's name
        // (RAL-318 bug 3 fix), so it agrees with a threshold set against
        // "proj" by name via `resolve_pool_key_input`.
        let mut keys = store.triage_pool_keys().unwrap();
        keys.sort();
        assert_eq!(
            keys,
            vec![
                ("proj".to_string(), "bug".to_string()),
                ("proj".to_string(), "investigation".to_string()),
            ]
        );
        let project = keys[0].0.clone();
        // A pooled cell is only *counted* once it has actually finished --
        // pooling records the candidate, the count gates a threshold. Mark it
        // done so this test measures per-type independence rather than the
        // counting rule (which `triage_pool_count_only_includes_a_cell_after_its_proof_passes`
        // covers on its own).
        store
            .set_cell_state(&squad_id, 0, 0, crate::store::NodeState::Done)
            .unwrap();
        assert_eq!(store.triage_pool_count(&project, "bug").unwrap(), 1);
        assert_eq!(
            store.triage_pool_count(&project, "investigation").unwrap(),
            1
        );

        // Draining the "bug" pool (e.g. its own threshold/schedule firing)
        // must not remove the cell from the still-pending "investigation"
        // pool -- each type's pooling is independent.
        let drained = store.drain_triage_pool(&project, "bug").unwrap();
        assert_eq!(drained.len(), 1);
        assert_eq!(store.triage_pool_count(&project, "bug").unwrap(), 0);
        assert_eq!(
            store.triage_pool_count(&project, "investigation").unwrap(),
            1
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    /// RAL-346: two cells resolved to overlapping (not identical) subproject
    /// sets share the composite pool their overlap falls in, while a third,
    /// disjoint cell lands in its own separate pool -- the "shared impact"
    /// overlap test rather than exact-set equality.
    #[test]
    fn derive_triage_pools_keys_by_subproject_when_resolved() {
        let root = temp_repo();
        git(&root, &["init", "--initial-branch", "main"]);
        std::fs::write(root.join("base.txt"), "base\n").unwrap();
        git(&root, &["add", "."]);
        git(&root, &["commit", "--message", "base"]);
        git(&root, &["checkout", "-b", "feature"]);
        git(&root, &["branch", "--set-upstream-to", "main"]);
        std::fs::write(
            root.join(".ralphus.toml"),
            "[monorepo]\nsubprojects = [\"core\", \"utils\", \"steam\"]\n",
        )
        .unwrap();

        let mut store = Store::open_in_memory().unwrap();
        store.register_triage_type("bug", "Bug", "").unwrap();
        store
            .register_project("proj", "", &root.to_string_lossy(), "git")
            .unwrap();

        let src = task_file_toml(&root, "bug");
        let file: ralphus_core::schema::TaskFile = toml::from_str(&src).unwrap();

        let squad_a = store.insert_squad(&file, None, false).unwrap();
        store
            .set_cell_triage_types(&squad_a, 0, 0, &["bug".to_string()])
            .unwrap();
        store
            .set_cell_subprojects(&squad_a, 0, 0, &["core".to_string()], false)
            .unwrap();
        assert!(
            derive_triage_pools(&store, &squad_a, &file, |_| None)
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            store.triage_pool_keys().unwrap(),
            vec![("proj::core".to_string(), "bug".to_string())]
        );
        // Pooling records a candidate; only a finished cell is *counted*
        // toward a threshold. Each squad below is marked done so the counts
        // in this test measure subproject-overlap bucketing, not the
        // done-gating rule.
        store
            .set_cell_state(&squad_a, 0, 0, crate::store::NodeState::Done)
            .unwrap();

        // An overlapping-but-not-identical set ({core, utils} vs {core})
        // still shares the "core" pool.
        let squad_b = store.insert_squad(&file, None, false).unwrap();
        store
            .set_cell_triage_types(&squad_b, 0, 0, &["bug".to_string()])
            .unwrap();
        store
            .set_cell_subprojects(
                &squad_b,
                0,
                0,
                &["core".to_string(), "utils".to_string()],
                true,
            )
            .unwrap();
        assert!(
            derive_triage_pools(&store, &squad_b, &file, |_| None)
                .unwrap()
                .is_empty()
        );
        store
            .set_cell_state(&squad_b, 0, 0, crate::store::NodeState::Done)
            .unwrap();
        assert_eq!(
            store.triage_pool_count("proj::core", "bug").unwrap(),
            2,
            "overlapping subprojects must share the 'core' pool"
        );
        assert_eq!(store.triage_pool_count("proj::utils", "bug").unwrap(), 1);

        // A disjoint set ({steam}) never lands in the "core" pool.
        let squad_c = store.insert_squad(&file, None, false).unwrap();
        store
            .set_cell_triage_types(&squad_c, 0, 0, &["bug".to_string()])
            .unwrap();
        store
            .set_cell_subprojects(&squad_c, 0, 0, &["steam".to_string()], false)
            .unwrap();
        assert!(
            derive_triage_pools(&store, &squad_c, &file, |_| None)
                .unwrap()
                .is_empty()
        );
        store
            .set_cell_state(&squad_c, 0, 0, crate::store::NodeState::Done)
            .unwrap();
        assert_eq!(store.triage_pool_count("proj::steam", "bug").unwrap(), 1);
        assert_eq!(
            store.triage_pool_count("proj::core", "bug").unwrap(),
            2,
            "a disjoint subproject cell must not land in the 'core' pool"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    /// RAL-346: a monorepo cell whose subproject hasn't been resolved yet
    /// (no manual declaration, Arbiter inference hasn't run/matched) falls
    /// back to the plain project-name key -- same as a plain single-project
    /// repo -- rather than being mis-bucketed or blocked from pooling.
    #[test]
    fn derive_triage_pools_falls_back_to_the_plain_key_for_an_unresolved_monorepo_cell() {
        let root = temp_repo();
        git(&root, &["init", "--initial-branch", "main"]);
        std::fs::write(root.join("base.txt"), "base\n").unwrap();
        git(&root, &["add", "."]);
        git(&root, &["commit", "--message", "base"]);
        git(&root, &["checkout", "-b", "feature"]);
        git(&root, &["branch", "--set-upstream-to", "main"]);
        std::fs::write(
            root.join(".ralphus.toml"),
            "[monorepo]\nsubprojects = [\"core\", \"utils\"]\n",
        )
        .unwrap();

        let mut store = Store::open_in_memory().unwrap();
        store.register_triage_type("bug", "Bug", "").unwrap();
        store
            .register_project("proj", "", &root.to_string_lossy(), "git")
            .unwrap();

        let src = task_file_toml(&root, "bug");
        let file: ralphus_core::schema::TaskFile = toml::from_str(&src).unwrap();
        let squad_id = store.insert_squad(&file, None, false).unwrap();
        store
            .set_cell_triage_types(&squad_id, 0, 0, &["bug".to_string()])
            .unwrap();
        // No `set_cell_subprojects` call -- this cell's resolution stays
        // `Unresolved` even though the project IS configured as a monorepo.

        assert!(
            derive_triage_pools(&store, &squad_id, &file, |_| None)
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            store.triage_pool_keys().unwrap(),
            vec![("proj".to_string(), "bug".to_string())],
            "an unresolved monorepo cell must fall back to the plain project key"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn derive_triage_pools_is_a_no_op_when_no_cell_opts_in() {
        let store = Store::open_in_memory().unwrap();
        let src = "[[task]]\nname=\"t\"\n[[task.cell]]\ncwd=\"/repo\"\nprompt=\"p\"\n";
        let file: ralphus_core::schema::TaskFile = toml::from_str(src).unwrap();
        assert!(
            derive_triage_pools(&store, "squad-1", &file, |_| None)
                .unwrap()
                .is_empty()
        );
    }

    // ── RAL-159 parity for Triage pooling ───────────────────────────────────
    //
    // The same "work" cell opts into Triage (`triage = true`) while a
    // downstream "finalize" cell in the same task shares its worktree but
    // declares neither `triage` nor `review` -- the layout the tutorial
    // recommends. The readiness gate (`Store::mark_ready_branches_with_done_cells`)
    // must withhold `MergeStatus::Ready` on the Triage-derived guardian's
    // branch until BOTH cells are done, exactly like the explicit
    // `derive_reviews` path's `worktree_sharing_gates_branch_ready_until_all_sessions_done`.

    /// Two cells at the exact same worktree cwd: only "work" opts into
    /// Triage, "finalize" is a plain downstream cell with no review/triage
    /// fields of its own.
    #[test]
    fn triage_pool_worktree_sibling_gates_review_ready_until_both_cells_done() {
        let root = temp_repo();
        git(&root, &["init", "--initial-branch", "main"]);
        std::fs::write(root.join("base.txt"), "base\n").unwrap();
        git(&root, &["add", "."]);
        git(&root, &["commit", "--message", "base"]);
        git(&root, &["checkout", "-b", "feature"]);
        git(&root, &["branch", "--set-upstream-to", "main"]);

        let mut store = Store::open_in_memory().unwrap();
        store
            .register_triage_type("security", "Security", "")
            .unwrap();

        let cwd = root.to_string_lossy().replace('\\', "/");
        let src = format!(
            "[[task]]\nname=\"t\"\n\
             [[task.cell]]\nid=\"work\"\ncwd=\"{cwd}\"\nprompt=\"do it\"\ntriage=true\ntriage_type=\"security\"\n\
             [[task.cell]]\nid=\"finalize\"\ncwd=\"{cwd}\"\nprompt=\"wrap up\"\ndepends_on=[\"work\"]\n"
        );
        let file: ralphus_core::schema::TaskFile = toml::from_str(&src).unwrap();
        let squad_id = store.insert_squad(&file, None, false).unwrap();
        // Simulate the submit pipeline's earlier classification step, which
        // always runs before `derive_triage_pools`; only "work" opts in.
        store
            .set_cell_triage_types(&squad_id, 0, 0, &["security".to_string()])
            .unwrap();
        // The threshold is configured before the candidate is submitted.
        let triage_project =
            crate::reviews::project_root_of(&cwd).expect("cwd resolves to a git worktree project");
        store
            .set_triage_pool_threshold(&triage_project, "security", Some(1))
            .unwrap();

        let created = derive_triage_pools(&store, &squad_id, &file, |_| None).unwrap();
        assert!(created.is_empty(), "pending work must not create a review");

        // Completing the Triage candidate makes it eligible and re-checks the
        // threshold. Its non-Triage worktree sibling still gates merge
        // readiness below.
        store
            .set_cell_state(&squad_id, 0, 0, crate::store::NodeState::Done)
            .unwrap();
        let created = fire_ready_triage_thresholds(&store, &squad_id).unwrap();
        assert_eq!(created.len(), 1, "completed work fires threshold of 1");
        let gid = created[0].clone();
        let g = store.get_guardian(&gid).unwrap();
        assert_eq!(g.branches.len(), 1);
        assert_eq!(g.branches[0].branch, "feature");

        // "work" (task 0, cell 0) finishes -- branch must stay `pending`:
        // "finalize" (task 0, cell 1), the worktree sibling that never
        // itself opted into Triage, hasn't finished yet.
        let n = store.mark_ready_branches_with_done_cells(&gid).unwrap();
        assert_eq!(
            n, 0,
            "must not promote while the non-Triage worktree sibling is still pending"
        );
        assert_eq!(
            store.get_guardian(&gid).unwrap().branches[0].merge_status,
            "pending"
        );

        // "finalize" finishes too -- now every worktree-sharing cell is done.
        // Its owning task (RAL-442) must also reach `done` -- mirroring
        // `run_task_finalizer` clearing task-level proofs -- before the
        // branch may promote.
        store
            .set_cell_state(&squad_id, 0, 1, crate::store::NodeState::Done)
            .unwrap();
        store
            .set_task_state(&squad_id, 0, crate::store::NodeState::Done)
            .unwrap();
        let n = store.mark_ready_branches_with_done_cells(&gid).unwrap();
        assert_eq!(n, 1, "promotes exactly the one branch");
        assert_eq!(
            store.get_guardian(&gid).unwrap().branches[0].merge_status,
            "ready"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    /// Same as above, but "finalize"'s cwd is a nested SUBFOLDER of "work"'s
    /// worktree root rather than the identical path -- still the same
    /// worktree (`git rev-parse --show-toplevel` normalizes both to the same
    /// root), so the gate must still wait for it.
    #[test]
    fn triage_pool_nested_cwd_sibling_gates_review_ready_until_both_cells_done() {
        let root = temp_repo();
        git(&root, &["init", "--initial-branch", "main"]);
        std::fs::write(root.join("base.txt"), "base\n").unwrap();
        git(&root, &["add", "."]);
        git(&root, &["commit", "--message", "base"]);
        git(&root, &["checkout", "-b", "feature"]);
        git(&root, &["branch", "--set-upstream-to", "main"]);
        std::fs::create_dir_all(root.join("sub")).unwrap();

        let mut store = Store::open_in_memory().unwrap();
        store
            .register_triage_type("security", "Security", "")
            .unwrap();

        let cwd = root.to_string_lossy().replace('\\', "/");
        let sub_cwd = format!("{cwd}/sub");
        let src = format!(
            "[[task]]\nname=\"t\"\n\
             [[task.cell]]\nid=\"work\"\ncwd=\"{cwd}\"\nprompt=\"do it\"\ntriage=true\ntriage_type=\"security\"\n\
             [[task.cell]]\nid=\"finalize\"\ncwd=\"{sub_cwd}\"\nprompt=\"wrap up\"\ndepends_on=[\"work\"]\n"
        );
        let file: ralphus_core::schema::TaskFile = toml::from_str(&src).unwrap();
        let squad_id = store.insert_squad(&file, None, false).unwrap();
        store
            .set_cell_triage_types(&squad_id, 0, 0, &["security".to_string()])
            .unwrap();
        let triage_project =
            crate::reviews::project_root_of(&cwd).expect("cwd resolves to a git worktree project");
        store
            .set_triage_pool_threshold(&triage_project, "security", Some(1))
            .unwrap();

        let created = derive_triage_pools(&store, &squad_id, &file, |_| None).unwrap();
        assert!(created.is_empty(), "pending work must not create a review");

        store
            .set_cell_state(&squad_id, 0, 0, crate::store::NodeState::Done)
            .unwrap();
        let created = fire_ready_triage_thresholds(&store, &squad_id).unwrap();
        assert_eq!(created.len(), 1, "completed work fires threshold of 1");
        let gid = created[0].clone();
        let g = store.get_guardian(&gid).unwrap();
        assert_eq!(g.branches.len(), 1);
        assert_eq!(g.branches[0].branch, "feature");

        // "work" (root cwd) finishes -- branch must stay `pending`:
        // "finalize" (nested `sub` cwd, same worktree, no triage/review of
        // its own) hasn't finished yet.
        let n = store.mark_ready_branches_with_done_cells(&gid).unwrap();
        assert_eq!(
            n, 0,
            "must not promote while the nested-cwd worktree sibling is still pending"
        );
        assert_eq!(
            store.get_guardian(&gid).unwrap().branches[0].merge_status,
            "pending"
        );

        // "finalize" finishes too. Its owning task (RAL-442) must also reach
        // `done` before the branch may promote.
        store
            .set_cell_state(&squad_id, 0, 1, crate::store::NodeState::Done)
            .unwrap();
        store
            .set_task_state(&squad_id, 0, crate::store::NodeState::Done)
            .unwrap();
        let n = store.mark_ready_branches_with_done_cells(&gid).unwrap();
        assert_eq!(n, 1, "promotes exactly the one branch");
        assert_eq!(
            store.get_guardian(&gid).unwrap().branches[0].merge_status,
            "ready"
        );

        let _ = std::fs::remove_dir_all(&root);
    }
}
