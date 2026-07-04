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

use ralphus_core::schema::{REVIEW_BASE_UPSTREAM, ReviewDef, TaskFile};

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
fn worktree_branch(cwd: &Path) -> std::result::Result<String, String> {
    let b = git(cwd, &["symbolic-ref", "--quiet", "--short", "HEAD"])
        .map_err(|_| "worktree has no branch checked out (detached HEAD)".to_string())?;
    if b.is_empty() {
        return Err("worktree has no branch checked out".to_string());
    }
    Ok(b)
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
                prompt: s.prompt.clone(),
                command: s.command.clone(),
                agent: "claude".to_string(),
                model: None,
                depends_on: s.depends_on.clone(),
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
            memberships.push(Membership {
                project: project.clone(),
                branch: branch.clone(),
                base,
                name: rv
                    .name
                    .clone()
                    .or_else(|| rv.id.clone())
                    .unwrap_or_default(),
                order: rank[pos],
            });
        }
    }

    // Group by project (BTreeMap key = path string for a deterministic order).
    let mut groups: BTreeMap<String, Vec<&Membership>> = BTreeMap::new();
    for m in &memberships {
        groups
            .entry(m.project.to_string_lossy().into_owned())
            .or_default()
            .push(m);
    }

    let multi = groups.len() > 1;
    let mut created = Vec::new();
    for (k, (project, members)) in groups.iter().enumerate() {
        let mut members = members.clone();
        members.sort_by_key(|m| m.order);
        let base = members
            .first()
            .map_or_else(|| "main".to_string(), |m| m.base.clone());
        let suggested = members
            .iter()
            .find(|m| !m.name.is_empty())
            .map_or_else(|| "review".to_string(), |m| m.name.clone());
        // A single review keeps its suggested name; splitting across projects
        // disambiguates with a numeric suffix (review-001, review-002, …).
        let name = if multi {
            format!("{suggested}-{:03}", k + 1)
        } else {
            suggested
        };
        let gid = store
            .create_guardian_for_run(&name, &base, project, Some(run_id))
            .map_err(|e| ReviewError::new(e.to_string()))?;
        let mut seen = HashSet::new();
        for m in &members {
            if seen.insert(m.branch.clone()) {
                store
                    .add_guardian_branch(&gid, &m.branch)
                    .map_err(|e| ReviewError::new(e.to_string()))?;
            }
        }
        created.push(gid);
    }
    Ok(created)
}
