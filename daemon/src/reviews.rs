//! Derive per-project Guardian reviews from `[[task.session.review]]`.
//!
//! At submit time, sessions that declare a review are grouped by the *project*
//! their worktree belongs to (its shared git dir, so separate worktrees of one
//! repo collapse together). Each project becomes one guardian, whose branch list
//! is the sessions' worktree branches in topological order. A `base` of
//! `<<upstream>>` resolves to the worktree branch's upstream and is a hard error
//! if there is none. See `REVIEWS.local.md`.

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Command;

use ralphus_core::schema::{REVIEW_BASE_UPSTREAM, ReviewDef, TaskFile, review_link_key};

use crate::plan;
use crate::store::{SessionRow, Store, TaskRow};

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

/// Run `git args` in `dir`, returning trimmed stdout or the trimmed stderr.
fn git(dir: &Path, args: &[&str]) -> std::result::Result<String, String> {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .map_err(|e| format!("could not run git: {e}"))?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
    }
}

/// The project root for a session `cwd`, or `None` when it is not inside a git
/// worktree. Used by the API to show the project (shared root) as a field
/// distinct from the worktree (the session's own working copy) — see CCTL-148.
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
/// On failure the rebase is aborted automatically so the worktree is left
/// clean. Returns the git stderr as the error string.
pub(crate) fn rebase_onto(cwd: &Path, target_branch: &str) -> std::result::Result<(), String> {
    let out = Command::new("git")
        .args(["rebase", target_branch])
        .current_dir(cwd)
        .output()
        .map_err(|e| format!("could not run git rebase: {e}"))?;
    if out.status.success() {
        return Ok(());
    }
    // Abort the incomplete rebase so the worktree stays usable.
    let _ = Command::new("git")
        .args(["rebase", "--abort"])
        .current_dir(cwd)
        .output();
    Err(format!(
        "git rebase {} failed: {}",
        target_branch,
        String::from_utf8_lossy(&out.stderr).trim()
    ))
}

/// The upstream (`branch@{upstream}`) of the worktree's branch, if any.
fn worktree_upstream(cwd: &Path) -> std::result::Result<String, String> {
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

/// One session's contribution to a review.
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
}

/// Build the planner's session/task rows straight from the task file (same order
/// insertion uses), alongside each session's cwd and declared reviews.
type SessionReviews<'a> = Vec<(Option<String>, &'a [ReviewDef])>;
fn rows_from_file(file: &TaskFile) -> (Vec<SessionRow>, Vec<TaskRow>, SessionReviews<'_>) {
    let mut sessions = Vec::new();
    let mut tasks = Vec::new();
    let mut sess_reviews: SessionReviews = Vec::new();
    for (t_idx, task) in file.task.iter().enumerate() {
        let ti = i64::try_from(t_idx).unwrap_or(0);
        tasks.push(TaskRow {
            idx: ti,
            name: task.name.clone(),
            depends_on: task.depends_on.clone(),
        });
        for (s_idx, s) in task.session.iter().enumerate() {
            let sid = s.id.clone().unwrap_or_else(|| format!("session-{s_idx}"));
            sessions.push(SessionRow {
                task_idx: ti,
                idx: i64::try_from(s_idx).unwrap_or(0),
                task_name: task.name.clone(),
                session_id: sid,
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
                upstream: s.upstream.clone(),
            });
            sess_reviews.push((s.cwd.clone(), s.review.as_slice()));
        }
    }
    (sessions, tasks, sess_reviews)
}

/// Preflight and materialize the reviews declared in `file` as guardians tagged
/// with `run_id`. Returns the created guardian ids (empty when no session
/// declares a review). Any git/worktree problem is a hard error.
///
/// # Errors
/// Returns [`ReviewError`] when a review session has no cwd, its cwd is not a git
/// worktree, or a `<<upstream>>` base cannot be resolved.
pub fn derive_reviews(
    store: &Store,
    run_id: &str,
    file: &TaskFile,
) -> std::result::Result<Vec<String>, ReviewError> {
    let (sessions, tasks, sess_reviews) = rows_from_file(file);
    if sess_reviews.iter().all(|(_, r)| r.is_empty()) {
        return Ok(Vec::new());
    }

    // Topological rank per session position (for branch ordering).
    let execution = plan::plan(&sessions, &tasks).map_err(ReviewError::new)?;
    let mut rank = vec![0usize; sessions.len()];
    for (r, &pos) in execution.order.iter().enumerate() {
        rank[pos] = r;
    }

    let mut memberships: Vec<Membership> = Vec::new();
    for (pos, (cwd, reviews)) in sess_reviews.iter().enumerate() {
        if reviews.is_empty() {
            continue;
        }
        let cwd = cwd
            .as_deref()
            .ok_or_else(|| ReviewError::new("a session declaring a review has no cwd"))?;
        let cwd_path = Path::new(cwd);
        let project =
            worktree_project(cwd_path).map_err(|e| ReviewError::new(format!("{cwd}: {e}")))?;
        let branch =
            worktree_branch(cwd_path).map_err(|e| ReviewError::new(format!("{cwd}: {e}")))?;
        // Record this session's review branch so the board can link the session
        // back to its review(s) (RAL-17).
        let srow = &sessions[pos];
        store
            .set_session_review_branch(run_id, srow.task_idx, srow.idx, &branch)
            .map_err(|e| ReviewError::new(e.to_string()))?;
        for rv in *reviews {
            let base_spec = rv.base.as_deref().unwrap_or(REVIEW_BASE_UPSTREAM);
            let base = if base_spec == REVIEW_BASE_UPSTREAM {
                worktree_upstream(cwd_path).map_err(|_| {
                    ReviewError::new(format!(
                        "{cwd}: review base is '<<upstream>>' but branch '{branch}' has no upstream"
                    ))
                })?
            } else {
                base_spec.to_string()
            };
            let link_key = rv
                .id
                .as_deref()
                .and_then(review_link_key)
                .map(str::to_string);
            memberships.push(Membership {
                project: project.clone(),
                branch: branch.clone(),
                base,
                // Prefer an explicit name; for a link review fall back to the key
                // (never the raw `ralphus:new-review/...` id).
                name: rv
                    .name
                    .clone()
                    .or_else(|| link_key.clone())
                    .or_else(|| rv.id.clone())
                    .unwrap_or_default(),
                order: rank[pos],
                link_key,
                agent: rv.agent.clone().filter(|s| !s.trim().is_empty()),
                model: rv.model.clone().filter(|s| !s.trim().is_empty()),
            });
        }
    }

    // Split memberships into link groups (shared across submissions via a stable
    // `review_key`) and project groups (the classic "one review per repo per
    // submit"). BTreeMap keys give a deterministic order.
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
            .create_guardian_for_run(&name, &base, project, Some(run_id))
            .map_err(|e| ReviewError::new(e.to_string()))?;
        apply_skip_worktrees(store, &gid, project)?;
        apply_resolver(store, &gid, &members)?;
        // Single-project: no need to tag branches with a project (they share git_root).
        add_new_branches(store, &gid, &[], &members, false)?;
        created.push(gid);
    }

    // Link groups: find-or-create ONE shared guardian per key, appending this
    // submission's branches (deduped against whatever is already attached). A
    // guardian created here is tagged with this run; a pre-existing one keeps its
    // original run tag and just grows. Branches are tagged with their project root
    // so the merge engine processes each repo independently (RAL-29).
    for (key, members) in &link_groups {
        let mut members = members.clone();
        members.sort_by_key(|m| m.order);
        let gid = match store
            .guardian_id_for_review_key(key)
            .map_err(|e| ReviewError::new(e.to_string()))?
        {
            Some(gid) => gid,
            None => {
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
                    .create_guardian_keyed(&name, &base, &project, Some(run_id), Some(key))
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
                created.push(gid.clone());
                gid
            }
        };
        let already: Vec<String> = store
            .guardian_branches(&gid)
            .map_err(|e| ReviewError::new(e.to_string()))?
            .into_iter()
            .map(|b| b.branch)
            .collect();
        // Tag each branch with its project root (multi-project link group).
        add_new_branches(store, &gid, &already, &members, true)?;
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
