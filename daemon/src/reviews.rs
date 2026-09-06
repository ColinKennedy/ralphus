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
use crate::store::{CellRow, Store, TaskRow};
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
/// reads: prefers the `ralphus.<branch>.baseline` git config key
/// [`crate::worktrees::set_explicit_upstream`] mirrors the resolved
/// `?upstream=` ref into, falling back to the live `@{upstream}` tracking ref
/// when no such marker exists (a worktree materialized before this fix, or a
/// plain checkout never routed through [`crate::worktrees::ensure_worktree`]).
///
/// The fallback-only marker matters because `@{upstream}` itself is exactly
/// what `git push -u`/`--set-upstream` overwrites: a `finalize` cell pushing a
/// brand-new branch for the first time routinely needs `-u` (a bare `git
/// push` fails until *some* upstream is configured), which retargets
/// `branch.<branch>.remote`/`.merge` from the intended base branch onto the
/// branch's own just-pushed remote copy — after which `@{upstream}` always
/// equals `HEAD`, indistinguishable from "no progress". The
/// `ralphus.<branch>.baseline` key lives in a config namespace git itself
/// never writes to, so it survives that push untouched.
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
    /// Optional `[review.auto_build]` declaration (RAL-342): either a static
    /// `command` or an agent-invocation shape, mutually exclusive with
    /// `skip_auto_build`.
    auto_build: Option<ralphus_core::schema::AutoBuildDef>,
    /// Explicit opt-out of the auto_build requirement (`[[review]]
    /// skip_auto_build = true`, RAL-342), mutually exclusive with `auto_build`.
    skip_auto_build: bool,
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
    if file.review.is_empty() {
        return Ok(Vec::new());
    }
    let (mut cells, tasks, cell_info) = rows_from_file(file);

    // Check early: are there any cells that opt into any review?
    if cell_info.iter().all(|(_, rev_id)| rev_id.is_none()) {
        return Ok(Vec::new());
    }

    // A review-opted-in cell's `cwd` may still be an unmaterialized
    // `ralphus:new-worktree/<branch>` placeholder (RAL-100): normally the
    // scheduler only resolves those when the squad is claimed to execute, but
    // this preflight needs a real worktree path *now* to run git against it.
    // Resolving here (persisted via `Store::set_cell_cwd`, same as the
    // scheduler's resolution) means a restarted squad never re-resolves it.
    crate::worktrees::resolve_placeholders(store, squad_id, &mut cells, &tasks, &Context::new())
        .map_err(ReviewError::new)?;

    // Topological rank per cell position (for branch ordering).
    let execution = plan::plan(&cells, &tasks).map_err(ReviewError::new)?;
    let mut rank = vec![0usize; cells.len()];
    for (r, &pos) in execution.order.iter().enumerate() {
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
                Some(b) => b,
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
            auto_build: rv.and_then(|r| r.auto_build.clone()),
            skip_auto_build: rv.is_some_and(|r| r.skip_auto_build),
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
        require_auto_build_declaration(&members, std::slice::from_ref(project), &name)?;
        let gid = store
            .create_guardian_for_squad(&name, &upstream, project, Some(squad_id))
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
        require_auto_build_declaration(&members, &distinct_projects, &review_ref)?;
        let gid = store
            .create_guardian_keyed(&name, &upstream, &project, Some(squad_id), Some(key))
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
/// actually used (`guardian_merge::resolver_agent`/`resolver_model`,
/// `guardian.rs`'s `effective_proof_scope`), so they already pick up a
/// project default regardless of how the guardian was created.
/// `auto_submit_pr_stack` also doesn't need one: `create_guardian_keyed`
/// already stamps it at INSERT time for every guardian, both paths included.
fn apply_project_review_defaults(
    store: &Store,
    gid: &str,
    project: &str,
) -> std::result::Result<(), ReviewError> {
    let cfg = crate::config::resolve(Path::new(project));
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
        store
            .set_guardian_resolver(gid, agent.as_deref(), model.as_deref())
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
    Ok(())
}

/// Field-for-field conversion from the offline `core::schema` shape (as parsed
/// from `[review.auto_build]`) to the runtime `guardian::GuardianAutoBuild`
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

/// Persist this review's declared build step (RAL-342), from the first member
/// that sets `[review.auto_build]`, or -- failing that -- the first member
/// that sets `skip_auto_build = true`. A no-op when no member declares
/// either, leaving the guardian to fall back to the project-config default at
/// merge time.
fn apply_auto_build(
    store: &Store,
    gid: &str,
    members: &[&Membership],
) -> std::result::Result<(), ReviewError> {
    if let Some(def) = members.iter().find_map(|m| m.auto_build.as_ref()) {
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

/// RAL-342: every review must explicitly declare its finalize-time build step
/// -- `[review.auto_build]` or `skip_auto_build = true` -- unless every
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
    members: &[&Membership],
    distinct_projects: &[String],
    review_ref: &str,
) -> std::result::Result<(), ReviewError> {
    let declared = members
        .iter()
        .any(|m| m.auto_build.is_some() || m.skip_auto_build);
    if declared {
        return Ok(());
    }
    let covered_by_config = !distinct_projects.is_empty()
        && distinct_projects
            .iter()
            .all(|p| crate::config::resolve(Path::new(p)).auto_build.is_some());
    if covered_by_config {
        return Ok(());
    }
    Err(ReviewError::new(format!(
        "{review_ref} must declare [review.auto_build] or skip_auto_build = true \
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
/// Race-safety: the threshold-check here and the scheduler's independent
/// cron-check race on the same pool, but both ultimately call
/// [`Store::drain_triage_pool`], a single atomic `DELETE ... RETURNING`
/// executed while holding the daemon's one `Arc<Mutex<Store>>` (same
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
        // RAL-318 Bug 3 fix: resolve to the registered project's stable name
        // when one's path matches this worktree root, rather than the raw
        // git-reported path -- keeps this key in agreement with whatever
        // `resolve_pool_key_input` computes for a threshold set against the
        // same project by name (`crate::server`'s pool/schedule handlers).
        let project_str = crate::triage::pool_key_for_path(store, &project);
        // A cell resolved to more than one type (inline `triage_type` list,
        // or a multi-type Arbiter classification) is pooled into every one
        // of its types' `(project, triage_type)` pools independently --
        // draining one pool never removes it from the others, since each is
        // its own row in `triage_pool_cells`.
        for triage_type in &triage_types {
            store
                .record_triage_pool_cell(
                    &project_str,
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
                    serde_json::json!({"project": project_str, "triage_type": triage_type}),
                );
            touched_keys.insert((project_str.clone(), triage_type.clone()));
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
            if let Some(gid) = create_review_from_triage_pool(store, &project, &triage_type)? {
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
/// # Errors
/// Returns [`ReviewError`] on any store failure while creating the guardian
/// or attaching its branches.
pub(crate) fn create_review_from_triage_pool(
    store: &Store,
    project: &str,
    triage_type: &str,
) -> std::result::Result<Option<String>, ReviewError> {
    let drained = store
        .drain_triage_pool(project, triage_type)
        .map_err(|e| ReviewError::new(e.to_string()))?;
    // Defense in depth: `drain_triage_pool` already excludes a failed cell,
    // but a failed cell must never reach a review under any circumstance
    // (RAL-318 bug 2), so re-check here too in case some future code path
    // ever inserts into `triage_pool_cells` without going through the same
    // drain filter (e.g. a hypothetical manual "force-fire" admin action).
    let mut viable = Vec::with_capacity(drained.len());
    for cell in drained {
        let effective = store
            .effective_state_for_cell(&cell.squad_id, cell.task_idx, cell.idx)
            .map_err(|e| ReviewError::new(e.to_string()))?;
        if effective.as_deref() != Some("failed") {
            viable.push(cell);
        }
    }
    let drained = viable;
    if drained.is_empty() {
        return Ok(None);
    }
    let upstream = drained
        .first()
        .map(|c| c.upstream.clone())
        .unwrap_or_else(|| "main".to_string());
    let name = format!("triage-{triage_type}");
    let gid = store
        .create_guardian_for_squad(&name, &upstream, project, None)
        .map_err(|e| ReviewError::new(e.to_string()))?;
    store
        .set_guardian_origin(&gid, crate::guardian::GUARDIAN_ORIGIN_ARBITER)
        .map_err(|e| ReviewError::new(e.to_string()))?;
    apply_project_review_defaults(store, &gid, project)?;
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
    crate::cartographer::Note::new("arbiter").guardian(&gid).emit(
        store,
        format!(
            "Triage pool ({project}, {triage_type}) fired -> created review {gid} from {} cell(s)",
            drained.len()
        ),
        serde_json::json!({
            "project": project,
            "triage_type": triage_type,
            "cell_count": drained.len(),
        }),
    );
    Ok(Some(gid))
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
            match create_review_from_triage_pool(store, &project, &triage_type) {
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
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use std::sync::atomic::{AtomicU32, Ordering};

    use super::{
        Membership, any_workspace_ahead_of_upstream, apply_auto_build,
        apply_project_review_defaults, apply_resolver, create_review_from_triage_pool,
        derive_triage_pools, rebase_onto, repair_triage_pool_keys,
        require_auto_build_declaration, workspace_has_commits_ahead_of_upstream,
        workspace_head_is_ancestor_of_upstream,
    };
    use crate::store::Store;
    use crate::workspace::Workspace;

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
            auto_build: None,
            skip_auto_build: false,
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
            auto_build: Some(def),
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
            auto_build: Some(def),
            ..membership(None)
        };
        let root = temp_repo();
        let project = root.to_string_lossy().into_owned();
        assert!(require_auto_build_declaration(&[&m], &[project], "r").is_ok());
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
        assert!(require_auto_build_declaration(&[&m], &[project], "r").is_ok());
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
        assert!(require_auto_build_declaration(&[&m], &[project], "r").is_ok());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn require_auto_build_declaration_err_when_nothing_declared_and_no_config_fallback() {
        let root = temp_repo();
        let m = membership(None);
        let project = root.to_string_lossy().into_owned();
        let err = require_auto_build_declaration(&[&m], &[project], "ralphus:new-review/abc123")
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
        assert!(require_auto_build_declaration(&[&m], &projects, "r").is_err());
        let _ = std::fs::remove_dir_all(&covered);
        let _ = std::fs::remove_dir_all(&uncovered);
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
        let store = std::sync::Arc::new(std::sync::Mutex::new(Store::open_in_memory().unwrap()));
        store
            .lock()
            .unwrap()
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
        let store = std::sync::Arc::new(std::sync::Mutex::new(Store::open_in_memory().unwrap()));
        store
            .lock()
            .unwrap()
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
        let store = Store::open_in_memory().unwrap();
        store
            .record_triage_pool_cell("proj", "security", "squad-1", 0, 0, "b1", "main")
            .unwrap();
        store
            .record_triage_pool_cell("proj", "security", "squad-2", 0, 0, "b2", "main")
            .unwrap();

        let gid = create_review_from_triage_pool(&store, "proj", "security")
            .unwrap()
            .expect("pool was non-empty, must create a review");
        let g = store.get_guardian(&gid).unwrap();
        assert_eq!(g.origin, crate::guardian::GUARDIAN_ORIGIN_ARBITER);
        assert_eq!(g.git_root, "proj");
        assert_eq!(g.branches.len(), 2);
        assert_eq!(store.triage_pool_count("proj", "security").unwrap(), 0);

        // Firing an already-drained pool is a no-op, not an error (the race
        // the scheduler tick and a concurrent submission's own threshold
        // check must both tolerate).
        assert!(
            create_review_from_triage_pool(&store, "proj", "security")
                .unwrap()
                .is_none()
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
        store
            .record_triage_pool_cell("proj", "security", "squad-ok", 0, 0, "b-ok", "main")
            .unwrap();

        let gid = create_review_from_triage_pool(&store, "proj", "security")
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
        let project = root.to_string_lossy().into_owned();
        let store = Store::open_in_memory().unwrap();
        store
            .record_triage_pool_cell(&project, "security", "squad-1", 0, 0, "b1", "main")
            .unwrap();

        let gid = create_review_from_triage_pool(&store, &project, "security")
            .unwrap()
            .expect("pool was non-empty, must create a review");

        let g = store.get_guardian(&gid).unwrap();
        assert_eq!(
            g.machine.as_deref(),
            Some("ib:A"),
            "the Arbiter has no [[review]] block to declare a machine, so it must pick up \
             the project's default_machine"
        );
        assert_eq!(g.maximum_budget_usd, Some(2.5));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn repair_triage_pool_keys_rekeys_a_stale_path_based_pool_and_fires_when_now_past_threshold() {
        let root = temp_repo();
        let store = Store::open_in_memory().unwrap();
        store
            .register_project("proj", "", &root.to_string_lossy(), "git")
            .unwrap();

        let stale_key = root.to_string_lossy().replace('\\', "/");
        store
            .record_triage_pool_cell(&stale_key, "bug", "squad-x", 0, 0, "b1", "main")
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
        let key_b = root.to_string_lossy().into_owned();
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
        let key_b = root.to_string_lossy().into_owned();
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
        let created = derive_triage_pools(&store, &squad_id, &file).unwrap();
        assert!(created.is_empty());
        let keys = store.triage_pool_keys().unwrap();
        assert_eq!(keys.len(), 1);
        let (project, triage_type) = keys[0].clone();
        assert_eq!(
            project, "proj",
            "pool key must resolve to the registered project's name (RAL-318 bug 3), not its raw worktree path"
        );
        assert_eq!(triage_type, "security");
        assert_eq!(store.triage_pool_count(&project, &triage_type).unwrap(), 1);

        // A second submission of the same file re-pools (a fresh squad_id),
        // and with a threshold of 2 now configured, this second call fires.
        store
            .set_triage_pool_threshold(&project, &triage_type, Some(2))
            .unwrap();
        let squad_id_2 = store.insert_squad(&file, None, false).unwrap();
        store
            .set_cell_triage_types(&squad_id_2, 0, 0, &["security".to_string()])
            .unwrap();
        let created = derive_triage_pools(&store, &squad_id_2, &file).unwrap();
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

        let created = derive_triage_pools(&store, &squad_id, &file).unwrap();
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

    #[test]
    fn derive_triage_pools_is_a_no_op_when_no_cell_opts_in() {
        let store = Store::open_in_memory().unwrap();
        let src = "[[task]]\nname=\"t\"\n[[task.cell]]\ncwd=\"/repo\"\nprompt=\"p\"\n";
        let file: ralphus_core::schema::TaskFile = toml::from_str(src).unwrap();
        assert!(
            derive_triage_pools(&store, "squad-1", &file)
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
        // Fire immediately: threshold of 1 on the pool this submission's own
        // "work" cell resolves to.
        let triage_project =
            crate::reviews::project_root_of(&cwd).expect("cwd resolves to a git worktree project");
        store
            .set_triage_pool_threshold(&triage_project, "security", Some(1))
            .unwrap();

        let created = derive_triage_pools(&store, &squad_id, &file).unwrap();
        assert_eq!(created.len(), 1, "threshold of 1 fires on submit");
        let gid = created[0].clone();
        let g = store.get_guardian(&gid).unwrap();
        assert_eq!(g.branches.len(), 1);
        assert_eq!(g.branches[0].branch, "feature");

        // "work" (task 0, cell 0) finishes -- branch must stay `pending`:
        // "finalize" (task 0, cell 1), the worktree sibling that never
        // itself opted into Triage, hasn't finished yet.
        store
            .set_cell_state(&squad_id, 0, 0, crate::store::NodeState::Done)
            .unwrap();
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
        store
            .set_cell_state(&squad_id, 0, 1, crate::store::NodeState::Done)
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

        let created = derive_triage_pools(&store, &squad_id, &file).unwrap();
        assert_eq!(created.len(), 1, "threshold of 1 fires on submit");
        let gid = created[0].clone();
        let g = store.get_guardian(&gid).unwrap();
        assert_eq!(g.branches.len(), 1);
        assert_eq!(g.branches[0].branch, "feature");

        // "work" (root cwd) finishes -- branch must stay `pending`:
        // "finalize" (nested `sub` cwd, same worktree, no triage/review of
        // its own) hasn't finished yet.
        store
            .set_cell_state(&squad_id, 0, 0, crate::store::NodeState::Done)
            .unwrap();
        let n = store.mark_ready_branches_with_done_cells(&gid).unwrap();
        assert_eq!(
            n, 0,
            "must not promote while the nested-cwd worktree sibling is still pending"
        );
        assert_eq!(
            store.get_guardian(&gid).unwrap().branches[0].merge_status,
            "pending"
        );

        // "finalize" finishes too.
        store
            .set_cell_state(&squad_id, 0, 1, crate::store::NodeState::Done)
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
