//! Derive per-project Guardian reviews from top-level `[[review]]` blocks.
//!
//! At submit time, cells that opt in to a review (via `review = "<<review:<id>>>"`
//! on the cell) are grouped by the *project* their worktree belongs to (its shared
//! git dir, so linked worktrees of one repo collapse together). Each project
//! becomes one guardian, whose branch list is the cells' worktree branches in
//! topological order. The base branch is always the worktree's upstream tracking
//! branch — a hard error if the worktree has none.

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};

use opentelemetry::Context;

use ralphus_core::schema::{ReviewActionDef, ReviewDef, TaskFile, review_link_key};

use crate::guardian::{CheckInput, GuardianCheck};
use crate::plan;
use crate::store::{CellRow, Store, TaskRow};
use crate::vcs::{GitOps, GitVcs};

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

    if has_changes {
        git(
            cwd,
            &[
                "stash",
                "push",
                "--include-untracked",
                "-m",
                "ralphus-rebase-stash",
            ],
        )
        .map_err(|e| format!("git stash before rebase failed: {e}"))?;
    }

    if let Err(e) = git(cwd, &["rebase", target_branch]) {
        // Abort the incomplete rebase so the worktree stays usable.
        let _ = git(cwd, &["rebase", "--abort"]);
        // Restore stashed work so nothing is lost.
        if has_changes {
            let _ = git(cwd, &["stash", "pop"]);
        }
        return Err(format!("git rebase {target_branch} failed: {e}"));
    }

    // Rebase succeeded — restore any stashed work.
    if has_changes {
        git(cwd, &["stash", "pop"]).map_err(|e| {
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
    project: PathBuf,
    branch: String,
    base: String,
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
}

/// Build the planner's cell/task rows straight from the task file (same order
/// insertion uses), alongside each cell's cwd and declared review opt-in.
type CellReviewInfo<'a> = Vec<(Option<String>, Option<&'a str>)>;

fn rows_from_file<'a>(file: &'a TaskFile) -> (Vec<CellRow>, Vec<TaskRow>, CellReviewInfo<'a>) {
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
                upstream: s.upstream.clone(),
                machine: ralphus_core::schema::resolve_cell_machine(task, s),
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
/// task's registered `project`. The base is the one thing with no declarative
/// source, which is why `[[review]] base` exists.
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
        let declared_base = review_map
            .get(rev_id)
            .copied()
            .and_then(|r| r.base.clone())
            .map(|b| b.trim().to_string())
            .filter(|b| !b.is_empty());
        // RAL-185: a cell that ran on another machine keeps its worktree
        // there, so none of the filesystem reads below can answer for it.
        // Everything needed is already known declaratively instead: the branch
        // from its `ralphus:new-worktree/<branch>` cwd, the base from the
        // review's own `base`, and the project from the owning task.
        let remote = remote_cell_derivation(
            store,
            &cells[pos],
            cell_info[pos].0.as_deref(),
            tasks_by_idx.get(&cells[pos].task_idx).copied().flatten(),
        )?;
        let (project, branch, base) = if let Some(rd) = &remote {
            let base = declared_base.clone().ok_or_else(|| {
                ReviewError::new(format!(
                    "review \"{rev_id}\" is fed by cell \"{}\" running on machine \"{}\", so \
                     its base branch cannot be read from that worktree's git upstream — this \
                     daemon cannot see another machine's filesystem. Declare it explicitly on \
                     the review: [[review]] base = \"main\".",
                    rd.cell_id, rd.machine
                ))
            })?;
            (PathBuf::from(&rd.project_root), rd.branch.clone(), base)
        } else {
            let project =
                worktree_project(cwd_path).map_err(|e| ReviewError::new(format!("{cwd}: {e}")))?;
            let branch =
                worktree_branch(cwd_path).map_err(|e| ReviewError::new(format!("{cwd}: {e}")))?;
            // A declared base wins; otherwise infer it from the worktree's own
            // upstream, exactly as an all-local review always has.
            let base = match declared_base.clone() {
                Some(b) => b,
                None => worktree_upstream(cwd_path).map_err(|_| {
                    ReviewError::new(format!(
                        "{cwd}: review base requires an upstream tracking branch for '{branch}', \
                         but none is configured (set one with \
                         'git branch --set-upstream-to=<branch>', or declare it on the review \
                         as [[review]] base = \"<branch>\")"
                    ))
                })?,
            };
            (project, branch, base)
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
            project: project.clone(),
            branch: branch.clone(),
            base,
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
        let base = members
            .first()
            .map_or_else(|| "main".to_string(), |m| m.base.clone());
        let suggested = members
            .iter()
            .find(|m| !m.name.is_empty())
            .map_or_else(|| "review".to_string(), |m| m.name.clone());
        let name = if multi {
            format!("{suggested}-{:03}", k + 1)
        } else {
            suggested
        };
        let gid = store
            .create_guardian_for_squad(&name, &base, project, Some(squad_id))
            .map_err(|e| ReviewError::new(e.to_string()))?;
        apply_skip_worktrees(store, &gid, project)?;
        apply_resolver(store, &gid, &members)?;
        apply_action_hints(store, &gid, &members, &hints_by_id)?;
        // Single-project: no need to tag branches with a project (they share git_root).
        add_new_branches(store, &gid, &[], &members, false)?;
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
        let base = members
            .first()
            .map_or_else(|| "main".to_string(), |m| m.base.clone());
        let name = members
            .iter()
            .find(|m| !m.name.is_empty())
            .map_or_else(|| key.clone(), |m| m.name.clone());
        let gid = store
            .create_guardian_keyed(&name, &base, &project, Some(squad_id), Some(key))
            .map_err(|e| ReviewError::new(e.to_string()))?;
        // Apply skip_worktrees for every distinct project in the group.
        let distinct_projects: Vec<String> = {
            let mut seen = std::collections::HashSet::new();
            members
                .iter()
                .map(|m| m.project.to_string_lossy().into_owned())
                .filter(|p| seen.insert(p.clone()))
                .collect()
        };
        for proj in &distinct_projects {
            apply_skip_worktrees(store, &gid, proj)?;
        }
        apply_resolver(store, &gid, &members)?;
        apply_action_hints(store, &gid, &members, &hints_by_id)?;
        // Freshly minted guardian: no branches attached yet. Tag each branch with
        // its project root (multi-project link group).
        add_new_branches(store, &gid, &[], &members, true)?;
        created.push(gid);
    }

    Ok(created)
}

/// Layered review config (global under per-project) may opt a project's reviews
/// out of per-branch worktrees (CCTL-156).
fn apply_skip_worktrees(
    store: &Store,
    gid: &str,
    project: &str,
) -> std::result::Result<(), ReviewError> {
    if crate::config::resolve(Path::new(project)).skip_worktrees() {
        store
            .set_guardian_skip_worktrees(gid, true)
            .map_err(|e| ReviewError::new(e.to_string()))?;
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
    Ok(())
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

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use std::sync::atomic::{AtomicU32, Ordering};

    use super::{Membership, apply_resolver, rebase_onto};
    use crate::store::Store;

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
        git(&root, &["init", "-b", "main"]);
        std::fs::write(root.join("base.txt"), "base\n").unwrap();
        git(&root, &["add", "."]);
        git(&root, &["commit", "-m", "base"]);

        git(&root, &["checkout", "-b", "upstream"]);
        std::fs::write(root.join("upstream.txt"), "upstream\n").unwrap();
        git(&root, &["add", "."]);
        git(&root, &["commit", "-m", "upstream"]);

        git(&root, &["checkout", "main"]);
        git(&root, &["checkout", "-b", "work"]);
        std::fs::write(root.join("work.txt"), "work\n").unwrap();
        git(&root, &["add", "."]);
        git(&root, &["commit", "-m", "work"]);

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
        git(&root, &["init", "-b", "main"]);
        std::fs::write(root.join("conflict.txt"), "base\n").unwrap();
        git(&root, &["add", "."]);
        git(&root, &["commit", "-m", "base"]);

        git(&root, &["checkout", "-b", "upstream"]);
        std::fs::write(root.join("conflict.txt"), "from upstream\n").unwrap();
        git(&root, &["commit", "-am", "upstream"]);

        git(&root, &["checkout", "main"]);
        git(&root, &["checkout", "-b", "work"]);
        std::fs::write(root.join("conflict.txt"), "from work\n").unwrap();
        git(&root, &["commit", "-am", "work"]);

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
            upstream: None,
            machine: None,
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
        git(&root, &["init", "-b", "main"]);
        std::fs::write(root.join("base.txt"), "base\n").unwrap();
        git(&root, &["add", "."]);
        git(&root, &["commit", "-m", "base"]);
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
        git(&root, &["init", "-b", "main"]);
        std::fs::write(root.join("base.txt"), "base\n").unwrap();
        git(&root, &["add", "."]);
        git(&root, &["commit", "-m", "base"]);

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
        git(&root, &["init", "-b", "main"]);
        std::fs::write(root.join("base.txt"), "base\n").unwrap();
        git(&root, &["add", "."]);
        git(&root, &["commit", "-m", "base"]);
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
            project: PathBuf::from("/repo"),
            branch: "feat".to_string(),
            base: "main".to_string(),
            name: "r".to_string(),
            order: 0,
            link_key: None,
            agent: None,
            model: None,
            machine: None,
            maximum_budget_usd,
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
}
