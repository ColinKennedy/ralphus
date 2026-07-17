//! Placeholder `cwd` resolution + git worktree materialization (RAL-100).
//!
//! A session `cwd` of the form `ralphus:new-worktree/<branch>` names a branch
//! to check out in a dedicated worktree under `.git/.ralphus_worktrees/<branch>`,
//! rather than a real filesystem path. The project to materialize it under is
//! NOT embedded in the `cwd` string -- it's the owning task's `project` field
//! (required whenever any of its sessions uses this placeholder).
//! [`resolve_placeholders`] resolves every placeholder among a run's sessions
//! exactly once (memoized by the literal placeholder string, so the same
//! value repeated across sessions/tasks only materializes one worktree),
//! rewriting each session's stored `cwd` to the real resolved path.
//!
//! Restart safety falls out of that rewrite: once a session's `cwd` has been
//! resolved and persisted, a restarted run reads the real path back from the
//! store — the placeholder string is gone; there's nothing left to resolve.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use opentelemetry::Context;
use opentelemetry::trace::{SpanKind, Status};

use crate::guardian_merge::git;
use crate::otel;
use crate::store::{SessionRow, Store, TaskRow};

/// The on-disk worktree directory for `branch` under a project's `root`.
#[must_use]
pub fn worktree_dir(root: &Path, branch: &str) -> PathBuf {
    root.join(".git").join(".ralphus_worktrees").join(branch)
}

/// Create (or reuse) a git worktree for `branch` under `root`, returning its
/// path.
///
/// Restart-safe: if `<root>/.git/.ralphus_worktrees/<branch>/.git` already
/// exists, the directory is assumed to be a previously materialized worktree
/// and is reused as-is rather than recreated (or erroring because the branch
/// or directory already exists).
///
/// When `branch` doesn't exist yet, the new branch is forked from `root`'s
/// current `HEAD` and its upstream tracking is set to that branch (best
/// effort). `derive_reviews` (reviews.rs) requires an upstream tracking
/// branch on every review-opted-in session's branch to determine the review's
/// base — without this, a `ralphus:new-worktree/...` placeholder combined
/// with `review = ...` would always fail preflight, since `git worktree add
/// -b` alone never configures one.
pub fn ensure_worktree(root: &Path, branch: &str) -> Result<PathBuf, String> {
    let wt = worktree_dir(root, branch);
    if wt.join(".git").exists() {
        return Ok(wt);
    }
    if let Some(parent) = wt.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("could not create worktree parent directory: {e}"))?;
    }
    let wt_str = wt.to_string_lossy().to_string();
    let branch_ref = format!("refs/heads/{branch}");
    let branch_exists = git(root, &["rev-parse", "--verify", &branch_ref]).is_ok();
    if branch_exists {
        git(root, &["worktree", "add", &wt_str, branch])?;
    } else {
        let base = git(root, &["rev-parse", "--abbrev-ref", "HEAD"]).ok();
        git(root, &["worktree", "add", "-b", branch, &wt_str])?;
        if let Some(base) = base
            .as_deref()
            .map(str::trim)
            .filter(|b| !b.is_empty() && *b != "HEAD")
        {
            // Best-effort: a detached HEAD or other oddity in `root` just means
            // no upstream gets set, and the review preflight's own error message
            // already tells the user how to configure one manually.
            let _ = git(&wt, &["branch", "--set-upstream-to", base]);
        }
    }
    Ok(wt)
}

/// Resolve every placeholder `cwd` (`ralphus:new-worktree/<branch>`) among
/// `sessions` in place, persisting each resolution to the store. Each
/// session's project comes from its owning task's `project` field in `tasks`,
/// not from the placeholder string itself. A placeholder string repeated
/// across sessions/tasks is only materialized once per call (memoized in
/// `cache`), so concurrent sessions sharing one worktree never race to create
/// it twice.
///
/// Wrapped in its own `scheduler.resolve_worktrees` span (RAL-96/RAL-100), a
/// child of `parent` (the owning run's span), so a slow `git worktree add`
/// against a large repo is visible as its own timed step in the trace rather
/// than being folded into the surrounding `scheduler.run_execute` span.
///
/// Returns an error naming the first session whose project can't be resolved
/// or whose worktree can't be created — submit-time validation should already
/// have ruled this out, but the scheduler must fail the run cleanly rather
/// than panic on stale or hand-edited data (e.g. a project deregistered after
/// submit). The error is logged here (WARNING) before being returned, since
/// the scheduler's own failure path only records run/task state transitions,
/// not the reason string itself.
pub fn resolve_placeholders(
    store: &Store,
    run_id: &str,
    sessions: &mut [SessionRow],
    tasks: &[TaskRow],
    parent: &Context,
) -> Result<(), String> {
    let span = otel::start_span("scheduler.resolve_worktrees", parent, SpanKind::Internal);
    span.set_attribute("run_id", run_id.to_string());
    match resolve_placeholders_inner(store, run_id, sessions, tasks) {
        Ok(materialized) => {
            span.set_attribute("worktrees.materialized", materialized as i64);
            span.set_status(Status::Ok);
            Ok(())
        }
        Err(e) => {
            span.set_status(Status::error(e.clone()));
            crate::rlog!(
                WARNING,
                "ralphus [scheduler] run {run_id} worktree placeholder resolution failed: {e}"
            );
            Err(e)
        }
    }
}

/// The count returned is how many distinct placeholders were newly
/// materialized (i.e. not already resolved from a prior run/restart or a
/// dedup hit within this call) — reported on the span as
/// `worktrees.materialized`.
fn resolve_placeholders_inner(
    store: &Store,
    run_id: &str,
    sessions: &mut [SessionRow],
    tasks: &[TaskRow],
) -> Result<usize, String> {
    let task_projects: HashMap<i64, Option<&str>> = tasks
        .iter()
        .map(|t| (t.idx, t.project.as_deref()))
        .collect();
    let mut cache: HashMap<String, String> = HashMap::new();
    let mut materialized = 0usize;
    for session in sessions.iter_mut() {
        let Some(cwd) = session.cwd.clone() else {
            continue;
        };
        let Some(branch) = ralphus_core::schema::parse_worktree_placeholder(&cwd) else {
            continue;
        };
        let resolved = if let Some(cached) = cache.get(&cwd) {
            cached.clone()
        } else {
            let project_name = task_projects
                .get(&session.task_idx)
                .copied()
                .flatten()
                .ok_or_else(|| {
                    format!(
                        "session '{}': task \"{}\" has no 'project' set for its placeholder cwd",
                        session.session_id, session.task_name
                    )
                })?;
            let project = store
                .resolve_project(project_name)
                .map_err(|e| e.to_string())?
                .ok_or_else(|| {
                    format!(
                        "session '{}': project \"{project_name}\" is not registered",
                        session.session_id
                    )
                })?;
            let wt = ensure_worktree(Path::new(&project.path), branch).map_err(|e| {
                format!(
                    "session '{}': could not materialize worktree for \"{cwd}\": {e}",
                    session.session_id
                )
            })?;
            let resolved = wt.to_string_lossy().into_owned();
            cache.insert(cwd.clone(), resolved.clone());
            materialized += 1;
            resolved
        };
        store
            .set_session_cwd(run_id, session.task_idx, session.idx, &resolved)
            .map_err(|e| e.to_string())?;
        crate::rlog!(
            INFO,
            "ralphus [scheduler] session {run_id}/{} cwd placeholder \"{cwd}\" resolved to {resolved}",
            session.session_id
        );
        session.cwd = Some(resolved);
    }
    Ok(materialized)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    static TEST_N: AtomicU32 = AtomicU32::new(0);

    fn tmp_dir(tag: &str) -> PathBuf {
        let n = TEST_N.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("ral100-wt-{tag}-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("mkdir tmp_dir");
        dir
    }

    fn g(root: &Path, args: &[&str]) {
        let status = std::process::Command::new("git")
            .args(args)
            .current_dir(root)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@t")
            .status()
            .expect("git");
        assert!(
            status.success(),
            "git {args:?} in {} failed",
            root.display()
        );
    }

    /// A fresh repo with one commit on `main`.
    fn init_repo(tag: &str) -> PathBuf {
        let repo = tmp_dir(tag);
        g(&repo, &["init", "-b", "main"]);
        std::fs::write(repo.join("base.txt"), "base\n").unwrap();
        g(&repo, &["add", "."]);
        g(&repo, &["commit", "-m", "base"]);
        repo
    }

    fn session_row(task_idx: i64, idx: i64, session_id: &str, cwd: Option<&str>) -> SessionRow {
        SessionRow {
            task_idx,
            idx,
            task_name: format!("task{task_idx}"),
            session_id: session_id.to_string(),
            cwd: cwd.map(str::to_string),
            subprojects: vec![],
            prompt: Some("do work".to_string()),
            command: None,
            agent: "claude".to_string(),
            model: None,
            system_prompt: None,
            system_prompt_position: None,
            depends_on: vec![],
            timeout_sec: None,
            budget_tokens: None,
            upstream: None,
        }
    }

    fn task_row(idx: i64, project: Option<&str>) -> TaskRow {
        TaskRow {
            idx,
            name: format!("task{idx}"),
            project: project.map(str::to_string),
            depends_on: vec![],
        }
    }

    #[test]
    fn ensure_worktree_creates_new_branch_and_worktree() {
        let repo = init_repo("new-branch");
        let wt = ensure_worktree(&repo, "feature-x").expect("materialize");
        assert_eq!(wt, worktree_dir(&repo, "feature-x"));
        assert!(
            wt.join(".git").exists(),
            "worktree must be a real linked worktree"
        );
        assert_eq!(
            git(&wt, &["symbolic-ref", "--short", "HEAD"])
                .unwrap()
                .trim(),
            "feature-x"
        );
    }

    #[test]
    fn ensure_worktree_sets_upstream_to_the_branch_it_forked_from() {
        // reviews.rs::derive_reviews requires `@{upstream}` to be resolvable on
        // every review-opted-in session's branch to determine the review base.
        // A brand-new `ralphus:new-worktree/...` branch must come out of
        // `ensure_worktree` with that already configured, or every such
        // placeholder+review combination would fail preflight.
        let repo = init_repo("new-branch-upstream");
        let wt = ensure_worktree(&repo, "feature-z").expect("materialize");
        let upstream = git(
            &wt,
            &[
                "rev-parse",
                "--abbrev-ref",
                "--symbolic-full-name",
                "@{upstream}",
            ],
        )
        .expect("new branch must have an upstream configured");
        assert_eq!(upstream.trim(), "main");
    }

    #[test]
    fn ensure_worktree_attaches_to_pre_existing_branch() {
        let repo = init_repo("existing-branch");
        g(&repo, &["branch", "already-here"]);
        let wt = ensure_worktree(&repo, "already-here").expect("materialize");
        assert_eq!(
            git(&wt, &["symbolic-ref", "--short", "HEAD"])
                .unwrap()
                .trim(),
            "already-here"
        );
    }

    #[test]
    fn ensure_worktree_reuses_existing_worktree_without_wiping_it() {
        let repo = init_repo("reuse");
        let wt = ensure_worktree(&repo, "feature-y").expect("first materialize");
        std::fs::write(wt.join("in-progress.txt"), "agent work\n").unwrap();

        // A second call (simulating a restarted run) must reuse the same
        // worktree rather than recreating it, so the agent's uncommitted work
        // survives.
        let wt2 = ensure_worktree(&repo, "feature-y").expect("second materialize (restart)");
        assert_eq!(wt, wt2);
        assert!(
            wt2.join("in-progress.txt").exists(),
            "restart must not wipe uncommitted work in an already-materialized worktree"
        );
    }

    #[test]
    fn resolve_placeholders_ignores_plain_cwd() {
        let repo = init_repo("plain-cwd");
        let store = Store::open_in_memory().unwrap();
        let plain = repo.to_string_lossy().into_owned();
        let mut sessions = vec![session_row(0, 0, "s0", Some(&plain))];
        resolve_placeholders(&store, "run-1", &mut sessions, &[], &Context::new())
            .expect("no-op for plain cwd");
        assert_eq!(sessions[0].cwd.as_deref(), Some(plain.as_str()));
    }

    #[test]
    fn resolve_placeholders_fails_for_unregistered_project() {
        let store = Store::open_in_memory().unwrap();
        let mut sessions = vec![session_row(0, 0, "s0", Some("ralphus:new-worktree/feat"))];
        let tasks = vec![task_row(0, Some("ghost-project"))];
        let err = resolve_placeholders(&store, "run-1", &mut sessions, &tasks, &Context::new())
            .expect_err("unregistered project must fail");
        assert!(
            err.contains("ghost-project"),
            "error should name the project: {err}"
        );
    }

    #[test]
    fn resolve_placeholders_fails_when_task_has_no_project() {
        // Submit-time validation should already rule this out, but the
        // scheduler must still fail cleanly on stale/hand-edited data.
        let store = Store::open_in_memory().unwrap();
        let mut sessions = vec![session_row(0, 0, "s0", Some("ralphus:new-worktree/feat"))];
        let tasks = vec![task_row(0, None)];
        let err = resolve_placeholders(&store, "run-1", &mut sessions, &tasks, &Context::new())
            .expect_err("missing task project must fail");
        assert!(
            err.contains("no 'project' set"),
            "error should explain the missing project: {err}"
        );
    }

    #[test]
    fn resolve_placeholders_materializes_a_registered_placeholder() {
        let repo = init_repo("materialize");
        let store = Store::open_in_memory().unwrap();
        store
            .register_project("myproj", "", &repo.to_string_lossy(), "git")
            .unwrap();
        let mut sessions = vec![session_row(0, 0, "s0", Some("ralphus:new-worktree/feat-a"))];
        let tasks = vec![task_row(0, Some("myproj"))];
        resolve_placeholders(&store, "run-1", &mut sessions, &tasks, &Context::new())
            .expect("materialize");
        let resolved = sessions[0].cwd.clone().expect("resolved cwd");
        assert_eq!(resolved, worktree_dir(&repo, "feat-a").to_string_lossy());
        assert!(Path::new(&resolved).join(".git").exists());
    }

    #[test]
    fn resolve_placeholders_reuses_one_worktree_across_sessions() {
        // The same placeholder string repeated across two sessions (as if two
        // tasks in one submission both referenced it) must materialize exactly
        // one worktree and resolve both sessions to the identical real path.
        let repo = init_repo("dedupe");
        let store = Store::open_in_memory().unwrap();
        store
            .register_project("shared", "", &repo.to_string_lossy(), "git")
            .unwrap();
        let mut sessions = vec![
            session_row(0, 0, "s0", Some("ralphus:new-worktree/feat-b")),
            session_row(1, 0, "s1", Some("ralphus:new-worktree/feat-b")),
        ];
        let tasks = vec![task_row(0, Some("shared")), task_row(1, Some("shared"))];
        resolve_placeholders(&store, "run-1", &mut sessions, &tasks, &Context::new())
            .expect("materialize");
        assert_eq!(sessions[0].cwd, sessions[1].cwd);

        // Only one worktree is registered with git for that branch.
        let list = git(&repo, &["worktree", "list", "--porcelain"]).unwrap();
        let count = list
            .lines()
            .filter(|l| l.starts_with("branch") && l.ends_with("feat-b"))
            .count();
        assert_eq!(
            count, 1,
            "expected exactly one worktree for the shared branch"
        );
    }

    #[test]
    fn resolve_placeholders_is_restart_safe() {
        // Simulate a restart: after the first resolution the session's cwd is a
        // real path (as it would be, re-read from the store), so a second call
        // must be a no-op that neither errors nor recreates the worktree.
        let repo = init_repo("restart-safe");
        let store = Store::open_in_memory().unwrap();
        store
            .register_project("proj", "", &repo.to_string_lossy(), "git")
            .unwrap();
        let mut sessions = vec![session_row(0, 0, "s0", Some("ralphus:new-worktree/feat-c"))];
        let tasks = vec![task_row(0, Some("proj"))];
        resolve_placeholders(&store, "run-1", &mut sessions, &tasks, &Context::new())
            .expect("first resolution");
        let resolved = sessions[0].cwd.clone().unwrap();

        std::fs::write(Path::new(&resolved).join("marker.txt"), "kept\n").unwrap();

        // Second call over freshly-loaded rows carrying the already-resolved cwd.
        resolve_placeholders(&store, "run-1", &mut sessions, &tasks, &Context::new())
            .expect("restart no-op");
        assert_eq!(sessions[0].cwd.as_deref(), Some(resolved.as_str()));
        assert!(Path::new(&resolved).join("marker.txt").exists());
    }
}
