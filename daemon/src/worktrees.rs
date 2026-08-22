//! Placeholder `cwd` resolution + git worktree materialization (RAL-100).
//!
//! A session `cwd` of the form `ralphus:new-worktree/<branch>` names a branch
//! to check out in a dedicated worktree under `.git/.ralphus/w/<short>`
//! (`<short>` is a truncated form of `branch`; see `crate::short_paths`. Git
//! branch names are never shortened, only this directory name), rather than a
//! real filesystem path. The project to materialize it under is
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

#[derive(Debug, Clone, PartialEq, Eq)]
enum BranchMaterialization {
    ExistingLocal,
    NewFromRemote {
        remote_ref: String,
    },
    NewFromHead {
        base: Option<String>,
        warning: Option<String>,
    },
}

/// The on-disk worktree directory for `branch` under a project's `root`:
/// `.git/.ralphus/w/<short>`, `<short>` a truncated form of `branch` (see
/// `crate::short_paths`) so a deeply nested `root` still fits inside
/// Windows' `MAX_PATH`; the git branch itself keeps its full name.
#[must_use]
pub fn worktree_dir(root: &Path, branch: &str) -> PathBuf {
    crate::short_paths::ralphus_root(root)
        .join("w")
        .join(crate::short_paths::short_name(branch))
}

/// The OS path-length budget a worktree checkout is held to. Windows'
/// `MAX_PATH` is 260 characters; other platforms' limits are high enough in
/// practice that enforcing it there too would only produce false failures.
fn path_budget_limit() -> usize {
    if cfg!(windows) { 260 } else { usize::MAX }
}

/// Preflight (RAL-211): before checking anything out, measure whether the
/// worktree directory plus the deepest tracked path in `git_ref` would
/// overflow `limit`, and fail with the arithmetic spelled out rather than
/// letting git fail deep inside `worktree add` with an opaque `Filename too
/// long`. Callers pass [`path_budget_limit`]; taken as a plain parameter here
/// (rather than read directly) so this is testable without depending on the
/// host OS.
///
/// Reads the tree from the object database (`git ls-tree`, not `ls-files`,
/// which would describe the current checkout rather than the tree about to be
/// materialized) -- no checkout, no network.
fn preflight_worktree_budget(
    root: &Path,
    wt: &Path,
    git_ref: &str,
    limit: usize,
) -> Result<(), String> {
    if limit == usize::MAX {
        return Ok(());
    }
    let listing = git(root, &["ls-tree", "-r", "-z", "--name-only", git_ref]).map_err(|e| {
        format!("could not measure \"{git_ref}\" for a worktree path preflight check: {e}")
    })?;
    crate::short_paths::check_worktree_path_budget(wt, &listing, limit)
}

fn validate_branch_name(root: &Path, branch: &str) -> Result<(), String> {
    git(root, &["check-ref-format", "--branch", branch]).map(|_| ())
}

fn branch_materialization(root: &Path, branch: &str) -> Result<BranchMaterialization, String> {
    validate_branch_name(root, branch)?;
    let branch_ref = format!("refs/heads/{branch}");
    if git(root, &["rev-parse", "--verify", &branch_ref]).is_ok() {
        return Ok(BranchMaterialization::ExistingLocal);
    }
    if branch.contains('/') {
        let remote_ref = format!("refs/remotes/{branch}");
        if git(root, &["rev-parse", "--verify", &remote_ref]).is_ok() {
            return Ok(BranchMaterialization::NewFromRemote { remote_ref });
        }
    }
    let base = git(root, &["rev-parse", "--abbrev-ref", "HEAD"]).ok();
    let warning = branch.contains('/').then(|| {
        format!(
            "placeholder branch \"{branch}\" looks like <remote>/<branch>, but \
             refs/remotes/{branch} does not exist in {}. Reusing or creating a \
             literal local branch named \"{branch}\" instead.",
            root.display()
        )
    });
    Ok(BranchMaterialization::NewFromHead { base, warning })
}

/// If `wt`'s checked-out branch tracks a remote (its `@{upstream}` resolves
/// to a ref under `refs/remotes/...`), fetch that remote branch and rebase
/// `wt`'s local branch onto the freshly fetched commit — so a worktree
/// materialized from a `ralphus:new-worktree/<remote>/<branch>` placeholder
/// picks up new pushes to that remote branch on every resolution, rather than
/// freezing forever at whatever the remote-tracking ref held at first
/// materialization (the [`ensure_worktree`] reuse path never touched it
/// before this).
///
/// A no-op for a branch with no upstream, or whose upstream is a local branch
/// (e.g. the `NewFromHead` case's `--set-upstream-to <base>`) — only a
/// genuinely remote-tracked branch is resynced.
///
/// A rebase, deliberately not a hard reset: any commits already made in this
/// worktree (by a prior agent session, or by hand) are replayed on top of the
/// updated remote history rather than discarded. If the rebase can't
/// complete cleanly — a real conflict, or local history that has diverged
/// from the remote in an unresolvable way — it is aborted and the worktree is
/// left exactly as it was before this call; resolving that is left to a
/// human, not attempted here.
fn resync_remote_tracking_branch(wt: &Path) -> Result<(), String> {
    let Ok(upstream) = git(wt, &["rev-parse", "--symbolic-full-name", "@{upstream}"]) else {
        return Ok(());
    };
    let upstream = upstream.trim();
    let Some(rest) = upstream.strip_prefix("refs/remotes/") else {
        return Ok(());
    };
    let Some((remote, remote_branch)) = rest.split_once('/') else {
        return Ok(());
    };
    let refspec = format!("{remote_branch}:{upstream}");
    git(wt, &["fetch", remote, &refspec]).map_err(|e| {
        format!(
            "could not fetch \"{remote}\" branch \"{remote_branch}\" to resync {}: {e}",
            wt.display()
        )
    })?;
    if git(wt, &["rebase", upstream]).is_err() {
        let _ = git(wt, &["rebase", "--abort"]);
        return Err(format!(
            "could not rebase {} onto updated \"{upstream}\" -- it likely has local commits \
             that conflict with new commits on the remote; resolve manually in that worktree \
             (e.g. run `git rebase {upstream}` there and fix conflicts) and rerun",
            wt.display()
        ));
    }
    Ok(())
}

/// Create (or reuse) a git worktree for `branch` under `root`, returning its
/// path.
///
/// Restart-safe: if the worktree directory's `.git` already exists, the
/// directory is assumed to be a previously materialized worktree and is
/// reused rather than recreated (or erroring because the branch or directory
/// already exists) — except that a branch tracking a remote is first resynced
/// to that remote; see [`resync_remote_tracking_branch`].
///
/// Branch names are first validated through `git check-ref-format --branch`,
/// so the daemon inherits git's exact acceptance rules instead of trying to
/// mirror them (including rejecting path-traversal shapes like `../foo`
/// before any path join or filesystem write happens).
///
/// If no local branch by that literal name exists but a remote-tracking ref
/// `refs/remotes/<branch>` does, the new local branch is created from that
/// remote-tracking ref and configured to track it — a real, attached branch
/// checkout (never a detached HEAD), so ordinary git operations (commit,
/// diff, `@{upstream}`) behave normally inside it. It does not stay frozen at
/// whatever the remote held at creation time: every later call to
/// `ensure_worktree` for the same worktree resyncs it to the remote (see
/// [`resync_remote_tracking_branch`]), so a review worktree keeps up with
/// pushes to that remote branch over the life of the project. Otherwise the
/// branch name is treated literally — slashes included — and a new local
/// branch is forked from `root`'s current `HEAD`; when that literal name
/// *looks* like `<remote>/<branch>` but no matching remote-tracking ref
/// exists, a warning is emitted so the fallback is explicit rather than
/// silently guessed.
///
/// A branch created from local `HEAD` has its upstream tracking set to that
/// branch (best effort). `derive_reviews` (reviews.rs) requires an upstream
/// tracking branch on every review-opted-in session's branch to determine the
/// review's base — without this, a `ralphus:new-worktree/...` placeholder
/// combined with `review = ...` would always fail preflight, since
/// `git worktree add -b` alone never configures one.
pub fn ensure_worktree(root: &Path, branch: &str) -> Result<PathBuf, String> {
    let materialization = branch_materialization(root, branch)?;
    let wt = worktree_dir(root, branch);
    if wt.join(".git").exists() {
        resync_remote_tracking_branch(&wt)?;
        return Ok(wt);
    }
    if let Some(parent) = wt.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("could not create worktree parent directory: {e}"))?;
    }
    let wt_str = wt.to_string_lossy().to_string();
    match materialization {
        BranchMaterialization::ExistingLocal => {
            preflight_worktree_budget(root, &wt, branch, path_budget_limit())?;
            git(root, &["worktree", "add", &wt_str, branch])?;
        }
        BranchMaterialization::NewFromRemote { remote_ref } => {
            preflight_worktree_budget(root, &wt, &remote_ref, path_budget_limit())?;
            git(
                root,
                &[
                    "worktree",
                    "add",
                    "--track",
                    "-b",
                    branch,
                    &wt_str,
                    &remote_ref,
                ],
            )?;
        }
        BranchMaterialization::NewFromHead { base, warning } => {
            if let Some(warning) = warning {
                crate::rlog!(WARNING, "ralphus [scheduler] {warning}");
            }
            preflight_worktree_budget(root, &wt, "HEAD", path_budget_limit())?;
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
    }
    Ok(wt)
}

/// Classify a session `cwd` as a placeholder needing materialization, a plain
/// path, or an unsupported scheme (RAL-185).
///
/// This is the registry-dispatch seam that generalizes the original hardcoded
/// `ralphus:new-worktree/` match. Today exactly one scheme materializes
/// anything — `ralphus:` — but a `cwd` carrying any *other* registered
/// provider's scheme is now recognized and rejected explicitly instead of
/// being silently treated as a literal directory name.
///
/// That silent-literal behavior is the failure this guards against: a `cwd` of
/// `incredibuild:/build/wt` would previously fall through
/// `parse_worktree_placeholder` as `None` and be handed to the runner verbatim,
/// which on Windows creates a *directory named `incredibuild:`* rather than
/// erroring — the agent would then run in the wrong place with no diagnostic.
///
/// Returns `Ok(Some(branch))` for a `ralphus:new-worktree/<branch>` placeholder,
/// `Ok(None)` for a plain filesystem path, and `Err` for a recognized-but-
/// unsupported scheme.
fn classify_placeholder(cwd: &str) -> Result<Option<&str>, String> {
    if let Some(branch) = ralphus_core::schema::parse_worktree_placeholder(cwd) {
        return Ok(Some(branch));
    }
    // A bare `ralphus:`-scheme value that isn't a well-formed new-worktree
    // placeholder is a typo, not a path -- `ralphus:` is this project's own
    // reserved scheme, so nothing legitimate starts with it.
    if let Some(rest) = cwd.strip_prefix("ralphus:") {
        return Err(format!(
            "\"ralphus:{rest}\" is not a valid placeholder; expected \
             \"{}<branch>\"",
            ralphus_core::schema::WORKTREE_PLACEHOLDER_PREFIX
        ));
    }
    // Any other `scheme:uri` that parses as a *machine* URI is a scheme in the
    // wrong field: the machine belongs in `machine = "..."`, and `cwd` stays a
    // path (or a `ralphus:new-worktree/` placeholder, which the session's
    // machine then provisions remotely). Saying so beats silently creating a
    // directory named after the scheme.
    if let Ok(ralphus_core::schema::MachineRef::Provider { scheme, .. }) =
        ralphus_core::schema::parse_machine(cwd)
    {
        return Err(format!(
            "\"{scheme}:...\" is a machine, not a working directory — set it as \
             this session's `machine` and give `cwd` a plain path or \
             \"{}<branch>\"",
            ralphus_core::schema::WORKTREE_PLACEHOLDER_PREFIX
        ));
    }
    Ok(None)
}

/// Ask a machine provider to provision a workspace for `branch`, returning
/// the path **on that machine** (RAL-185).
///
/// The `source` handed over is VCS-agnostic: `kind` comes from the registered
/// project's own `vcs` column, and `url` is only resolved for git. A provider
/// backing a non-git project reads `kind`, ignores the git-shaped fields, and
/// does whatever that source system needs — which is what keeps the daemon
/// from re-acquiring the hard git dependency RAL-175/184 baked in.
fn provision_remote(
    store: &Store,
    machine: &str,
    project: &crate::store::ProjectView,
    branch: &str,
    session: &SessionRow,
    run_id: &str,
) -> Result<String, String> {
    let provider = crate::remote_runner::provider_from_store(store, machine)
        .map_err(|e| format!("session '{}': {e}", session.session_id))?
        .ok_or_else(|| {
            // `provision_remote` is only called for a non-empty machine, so a
            // local resolution here means the value changed under us.
            format!(
                "session '{}': machine \"{machine}\" resolved to the local host",
                session.session_id
            )
        })?;
    // Only git has a clone URL to resolve; every other kind gets `None` and
    // the provider decides how to obtain the source.
    let url = if project.vcs == "git" {
        crate::guardian_merge::remote_clone_url(Path::new(&project.path)).ok()
    } else {
        None
    };
    let req = crate::remote_runner::ProvisionRequest {
        project: project.name.clone(),
        source: crate::remote_runner::WorkspaceSource {
            kind: project.vcs.clone(),
            url,
            branch: Some(branch.to_string()),
        },
        run_id: run_id.to_string(),
        session_id: session.session_id.clone(),
    };
    let spec = crate::runner::RunnerSpec::from_row(run_id, session);
    let result = provider
        .provision(&req, &spec)
        .map_err(|e| format!("session '{}': {e}", session.session_id));
    // RAL-201: `provision` had no Cartographer coverage at all -- a failure
    // was only visible via the caller's generic "worktree placeholder
    // resolution failed" `rlog!` line, and success left no record whatsoever.
    crate::cartographer::Note::new("worktrees")
        .level(if result.is_ok() {
            crate::logging::LogLevel::INFO
        } else {
            crate::logging::LogLevel::WARNING
        })
        .scope("session")
        .run(run_id)
        .session(&session.session_id)
        .emit(
            store,
            "machine provision",
            serde_json::json!({
                "machine": machine,
                "branch": branch,
                "ok": result.is_ok(),
                "workspace": result.as_ref().ok(),
                "error": result.as_ref().err(),
            }),
        );
    result
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
        let Some(branch) = classify_placeholder(&cwd).map_err(|e| {
            format!(
                "session '{}': could not resolve cwd \"{cwd}\": {e}",
                session.session_id
            )
        })?
        else {
            continue;
        };
        // RAL-185: the same placeholder on two different machines must
        // materialize two different workspaces, so the memo key carries the
        // machine. A purely `cwd`-keyed cache would hand machine B the path
        // machine A was given.
        let cache_key = format!("{}\u{0}{cwd}", session.machine.as_deref().unwrap_or(""));
        let resolved = if let Some(cached) = cache.get(&cache_key) {
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
            let resolved = match session.machine.as_deref() {
                // Remote: the provider owns workspace creation on its own
                // machine. Materializing locally and shipping the path would
                // point the agent at a directory that either doesn't exist
                // there or — on a fleet with a matching layout — is the wrong
                // checkout entirely.
                Some(machine) if !machine.trim().is_empty() => {
                    provision_remote(store, machine, &project, branch, session, run_id)?
                }
                _ => {
                    let wt = ensure_worktree(Path::new(&project.path), branch).map_err(|e| {
                        format!(
                            "session '{}': could not materialize worktree for \"{cwd}\": {e}",
                            session.session_id
                        )
                    })?;
                    wt.to_string_lossy().into_owned()
                }
            };
            cache.insert(cache_key, resolved.clone());
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

    fn init_repo_with_remote_branch(tag: &str, branch: &str) -> (PathBuf, String) {
        let base = tmp_dir(tag);
        let remote = base.join("remote.git");
        let (_, remote_branch) = branch
            .split_once('/')
            .expect("remote-qualified branch placeholder");
        g(
            &base,
            &[
                "init",
                "--bare",
                "--initial-branch=main",
                remote.to_str().expect("remote path"),
            ],
        );

        let seed = base.join("seed");
        std::fs::create_dir_all(&seed).unwrap();
        g(&seed, &["init", "-b", "main"]);
        std::fs::write(seed.join("base.txt"), "base\n").unwrap();
        g(&seed, &["add", "."]);
        g(&seed, &["commit", "-m", "base"]);
        g(
            &seed,
            &[
                "remote",
                "add",
                "origin",
                remote.to_str().expect("remote path"),
            ],
        );
        g(&seed, &["push", "-u", "origin", "main"]);

        g(&seed, &["checkout", "-b", remote_branch]);
        std::fs::write(seed.join("remote-only.txt"), format!("{branch}\n")).unwrap();
        g(&seed, &["add", "."]);
        g(&seed, &["commit", "-m", "remote branch"]);
        let branch_sha = git(&seed, &["rev-parse", "HEAD"])
            .expect("branch sha")
            .trim()
            .to_string();
        g(&seed, &["push", "-u", "origin", remote_branch]);

        let clone = base.join("clone");
        g(
            &base,
            &[
                "clone",
                remote.to_str().expect("remote path"),
                clone.to_str().expect("clone path"),
            ],
        );
        (clone, branch_sha)
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
            maximum_budget_usd: None,
            upstream: None,
            machine: None,
        }
    }

    fn task_row(idx: i64, project: Option<&str>) -> TaskRow {
        TaskRow {
            idx,
            name: format!("task{idx}"),
            project: project.map(str::to_string),
            depends_on: vec![],
            soloed: false,
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
    fn preflight_worktree_budget_fails_fast_with_the_arithmetic_spelled_out() {
        // The limit is passed explicitly rather than read from the host OS
        // (see `path_budget_limit`), so this is deterministic on every
        // platform CI runs on.
        let repo = init_repo("preflight-tight");
        let err = preflight_worktree_budget(&repo, Path::new("/short/wt"), "HEAD", 5)
            .expect_err("must fail when the budget is obviously too tight");
        assert!(err.contains("5-character"), "{err}");
    }

    #[test]
    fn preflight_worktree_budget_passes_with_a_generous_limit() {
        let repo = init_repo("preflight-loose");
        preflight_worktree_budget(&repo, Path::new("/short/wt"), "HEAD", 4096)
            .expect("must pass with a generous budget");
    }

    #[test]
    fn preflight_worktree_budget_is_a_noop_when_the_limit_is_max() {
        // The non-Windows branch of `path_budget_limit` -- never measures,
        // never fails, regardless of how deep the tree is.
        let repo = init_repo("preflight-unlimited");
        preflight_worktree_budget(&repo, Path::new("/short/wt"), "HEAD", usize::MAX)
            .expect("usize::MAX must always pass");
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
    fn ensure_worktree_uses_the_new_short_layout_and_ignores_old_layout_leftovers() {
        // An old-layout `.ralphus_worktrees/<branch>` directory left over
        // from before RAL-211 must not confuse or block a fresh
        // materialization under the `.ralphus/w/<short>` layout -- old-layout
        // state is simply left alone.
        let repo = init_repo("old-layout-coexist");
        let old_dir = repo
            .join(".git")
            .join(".ralphus_worktrees")
            .join("feature-long-name");
        std::fs::create_dir_all(&old_dir).unwrap();
        std::fs::write(old_dir.join("leftover.txt"), "abandoned\n").unwrap();

        let wt = ensure_worktree(&repo, "feature-long-name").expect("materialize");
        assert_eq!(wt, worktree_dir(&repo, "feature-long-name"));
        assert!(
            wt.to_string_lossy().contains(".ralphus")
                && !wt.to_string_lossy().contains(".ralphus_worktrees"),
            "must use the new layout, not the old one: {}",
            wt.display()
        );
        assert!(wt.join(".git").exists());
        // The old leftover must still be untouched -- no migration/purge.
        assert!(old_dir.join("leftover.txt").exists());
    }

    #[test]
    fn ensure_worktree_bootstraps_a_remote_tracking_branch_when_it_exists() {
        let (repo, remote_sha) = init_repo_with_remote_branch("remote-branch", "origin/foo");
        let wt = ensure_worktree(&repo, "origin/foo").expect("materialize from remote");
        assert_eq!(wt, worktree_dir(&repo, "origin/foo"));
        assert!(
            wt.join("remote-only.txt").exists(),
            "the remote branch's content must be present in the worktree"
        );
        assert_eq!(
            git(&wt, &["symbolic-ref", "--short", "HEAD"])
                .unwrap()
                .trim(),
            "origin/foo"
        );
        assert_eq!(git(&wt, &["rev-parse", "HEAD"]).unwrap().trim(), remote_sha);
        assert_eq!(
            git(
                &wt,
                &[
                    "rev-parse",
                    "--abbrev-ref",
                    "--symbolic-full-name",
                    "@{upstream}",
                ],
            )
            .unwrap()
            .trim(),
            "remotes/origin/foo"
        );
    }

    #[test]
    fn ensure_worktree_resyncs_a_remote_tracking_branch_to_new_pushes() {
        let (repo, first_sha) = init_repo_with_remote_branch("resync-new-push", "origin/foo");
        let wt = ensure_worktree(&repo, "origin/foo").expect("first materialize");
        assert_eq!(git(&wt, &["rev-parse", "HEAD"]).unwrap().trim(), first_sha);

        // Simulate a new push to the remote branch, from the seed clone that
        // pushed the original commit.
        let seed = repo.parent().unwrap().join("seed");
        std::fs::write(seed.join("remote-only.txt"), "origin/foo v2\n").unwrap();
        g(&seed, &["commit", "-am", "second remote commit"]);
        let second_sha = git(&seed, &["rev-parse", "HEAD"])
            .unwrap()
            .trim()
            .to_string();
        g(&seed, &["push", "origin", "foo"]);

        // Re-resolving the same (already materialized) worktree must pick up
        // the new remote commit, not stay frozen at the first-materialize SHA.
        let wt2 = ensure_worktree(&repo, "origin/foo").expect("resync on reuse");
        assert_eq!(wt2, wt);
        assert_eq!(git(&wt, &["rev-parse", "HEAD"]).unwrap().trim(), second_sha);
    }

    #[test]
    fn ensure_worktree_rebases_local_commits_onto_the_resynced_remote_instead_of_discarding_them() {
        let (repo, _first_sha) = init_repo_with_remote_branch("resync-rebase-local", "origin/bar");
        let wt = ensure_worktree(&repo, "origin/bar").expect("first materialize");

        // A prior agent session committed local work directly in the worktree.
        std::fs::write(wt.join("local-work.txt"), "agent work\n").unwrap();
        g(&wt, &["add", "."]);
        g(&wt, &["commit", "-m", "local agent commit"]);

        // A new, unrelated commit lands on the remote branch.
        let seed = repo.parent().unwrap().join("seed");
        std::fs::write(seed.join("remote-only.txt"), "origin/bar v2\n").unwrap();
        g(&seed, &["commit", "-am", "second remote commit"]);
        g(&seed, &["push", "origin", "bar"]);

        ensure_worktree(&repo, "origin/bar").expect("resync via rebase");

        assert!(
            wt.join("local-work.txt").exists(),
            "the local commit must survive the resync, replayed on top of the new remote commit \
             rather than discarded by a hard reset"
        );
        assert_eq!(
            git(&wt, &["log", "--format=%s", "-1"]).unwrap().trim(),
            "local agent commit",
            "the local commit must remain the tip after being rebased forward"
        );
    }

    #[test]
    fn ensure_worktree_aborts_and_errors_when_the_resync_rebase_conflicts() {
        let (repo, _first_sha) = init_repo_with_remote_branch("resync-conflict", "origin/baz");
        let wt = ensure_worktree(&repo, "origin/baz").expect("first materialize");

        // A local commit that edits the same file the remote is about to change.
        std::fs::write(wt.join("remote-only.txt"), "local edit\n").unwrap();
        g(&wt, &["commit", "-am", "local conflicting edit"]);

        let seed = repo.parent().unwrap().join("seed");
        std::fs::write(seed.join("remote-only.txt"), "remote edit\n").unwrap();
        g(&seed, &["commit", "-am", "conflicting remote edit"]);
        g(&seed, &["push", "origin", "baz"]);

        let err = ensure_worktree(&repo, "origin/baz").expect_err("conflicting rebase must fail");
        assert!(err.contains("rebase"), "{err}");

        // A failed resync must leave the worktree attached to its branch, not
        // mid-rebase (detached) -- i.e. the abort actually ran.
        assert_eq!(
            git(&wt, &["symbolic-ref", "--short", "HEAD"])
                .expect("worktree must not be left mid-rebase")
                .trim(),
            "origin/baz"
        );
    }

    #[test]
    fn ensure_worktree_warns_and_falls_back_to_a_literal_local_slash_branch() {
        let repo = init_repo("literal-slash-branch");
        let plan = branch_materialization(&repo, "alternative/foo").expect("plan");
        assert_eq!(
            plan,
            BranchMaterialization::NewFromHead {
                base: Some("main\n".to_string()),
                warning: Some(format!(
                    "placeholder branch \"alternative/foo\" looks like <remote>/<branch>, but \
             refs/remotes/alternative/foo does not exist in {}. Reusing or creating a \
             literal local branch named \"alternative/foo\" instead.",
                    repo.display()
                ),),
            }
        );

        let wt = ensure_worktree(&repo, "alternative/foo").expect("materialize");
        assert_eq!(wt, worktree_dir(&repo, "alternative/foo"));
        assert_eq!(
            git(&wt, &["symbolic-ref", "--short", "HEAD"])
                .unwrap()
                .trim(),
            "alternative/foo"
        );
    }

    #[test]
    fn ensure_worktree_rejects_invalid_branch_names_before_path_joining() {
        let repo = init_repo("invalid-branch");
        let err = ensure_worktree(&repo, "../escape").expect_err("invalid branch must fail");
        assert!(err.contains("check-ref-format"), "{err}");
        assert!(
            !repo.join(".git").join(".ralphus").exists(),
            "no worktree directory tree may be created for an invalid branch name"
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
    fn classify_placeholder_recognizes_a_worktree_placeholder() {
        assert_eq!(
            classify_placeholder("ralphus:new-worktree/feat-a"),
            Ok(Some("feat-a"))
        );
    }

    #[test]
    fn classify_placeholder_passes_plain_paths_through() {
        assert_eq!(classify_placeholder("/home/me/repo"), Ok(None));
        assert_eq!(classify_placeholder(r"C:\repo\wt"), Ok(None));
        assert_eq!(classify_placeholder("."), Ok(None));
    }

    #[test]
    fn classify_placeholder_rejects_a_malformed_ralphus_scheme() {
        let err = classify_placeholder("ralphus:new-worktre/typo")
            .expect_err("a ralphus: typo must not be treated as a directory name");
        assert!(err.contains("not a valid placeholder"), "{err}");
    }

    #[test]
    fn classify_placeholder_rejects_a_provider_scoped_cwd_instead_of_making_a_directory() {
        // Regression guard (RAL-185): before scheme dispatch this fell through
        // as a plain path and Windows happily created a directory literally
        // named `incredibuild:`, so the agent ran in the wrong place with no
        // diagnostic at all.
        let err = classify_placeholder("incredibuild:/build/wt")
            .expect_err("a machine-shaped cwd must be rejected, not silently used as a path");
        assert!(
            err.contains("is a machine, not a working directory"),
            "{err}"
        );
        assert!(err.contains("incredibuild"), "{err}");
    }

    #[test]
    fn resolve_placeholders_surfaces_an_unsupported_scheme_as_a_run_failure() {
        let store = Store::open_in_memory().unwrap();
        let mut sessions = vec![session_row(0, 0, "s0", Some("incredibuild:/build/wt"))];
        let err = resolve_placeholders(&store, "run-1", &mut sessions, &[], &Context::new())
            .expect_err("unsupported cwd scheme must fail the run");
        assert!(err.contains("s0"), "error should name the session: {err}");
        assert!(
            err.contains("is a machine, not a working directory"),
            "{err}"
        );
    }

    /// A provider script that echoes a fixed `provision` response.
    fn fake_provisioner(tag: &str, json: &str) -> PathBuf {
        let dir = tmp_dir(tag);
        let (path, body) = if cfg!(windows) {
            (dir.join("p.cmd"), format!("@echo off\r\necho {json}\r\n"))
        } else {
            (dir.join("p.sh"), format!("#!/bin/sh\necho '{json}'\n"))
        };
        std::fs::write(&path, body).expect("write provisioner");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        path
    }

    fn session_row_on(
        task_idx: i64,
        idx: i64,
        session_id: &str,
        cwd: Option<&str>,
        machine: Option<&str>,
    ) -> SessionRow {
        let mut row = session_row(task_idx, idx, session_id, cwd);
        row.machine = machine.map(str::to_string);
        row
    }

    #[test]
    fn a_remote_session_provisions_through_its_provider_instead_of_locally() {
        let repo = init_repo("remote-provision");
        let script = fake_provisioner(
            "remote-provision-p",
            r#"{"ok":true,"protocol_version":1,"workspace":"/remote/wt/feat-r"}"#,
        );
        let store = Store::open_in_memory().unwrap();
        store
            .register_project("proj", "", &repo.to_string_lossy(), "git")
            .unwrap();
        store
            .register_machine_provider(
                "ib",
                "",
                &script.to_string_lossy(),
                &[],
                crate::machines::PROTOCOL_VERSION,
                false,
            )
            .unwrap();
        let mut sessions = vec![session_row_on(
            0,
            0,
            "s0",
            Some("ralphus:new-worktree/feat-r"),
            Some("ib:A"),
        )];
        let tasks = vec![task_row(0, Some("proj"))];
        resolve_placeholders(&store, "run-1", &mut sessions, &tasks, &Context::new())
            .expect("remote provision");
        assert_eq!(sessions[0].cwd.as_deref(), Some("/remote/wt/feat-r"));
        // Nothing may be created on the daemon's own disk for a remote session.
        assert!(
            !worktree_dir(&repo, "feat-r").exists(),
            "a remote session must not materialize a local worktree"
        );
    }

    #[test]
    fn the_same_placeholder_on_two_machines_resolves_to_two_workspaces() {
        // A cwd-only memo key would hand the second machine the first's path.
        let repo = init_repo("two-machines");
        let a = fake_provisioner(
            "two-machines-a",
            r#"{"ok":true,"protocol_version":1,"workspace":"/on/a"}"#,
        );
        let b = fake_provisioner(
            "two-machines-b",
            r#"{"ok":true,"protocol_version":1,"workspace":"/on/b"}"#,
        );
        let store = Store::open_in_memory().unwrap();
        store
            .register_project("proj", "", &repo.to_string_lossy(), "git")
            .unwrap();
        for (scheme, script) in [("ma", &a), ("mb", &b)] {
            store
                .register_machine_provider(
                    scheme,
                    "",
                    &script.to_string_lossy(),
                    &[],
                    crate::machines::PROTOCOL_VERSION,
                    false,
                )
                .unwrap();
        }
        let mut sessions = vec![
            session_row_on(
                0,
                0,
                "s0",
                Some("ralphus:new-worktree/shared"),
                Some("ma:1"),
            ),
            session_row_on(
                1,
                0,
                "s1",
                Some("ralphus:new-worktree/shared"),
                Some("mb:1"),
            ),
        ];
        let tasks = vec![task_row(0, Some("proj")), task_row(1, Some("proj"))];
        resolve_placeholders(&store, "run-1", &mut sessions, &tasks, &Context::new())
            .expect("provision both");
        assert_eq!(sessions[0].cwd.as_deref(), Some("/on/a"));
        assert_eq!(sessions[1].cwd.as_deref(), Some("/on/b"));
    }

    #[test]
    fn a_provider_that_returns_no_workspace_path_fails_the_run() {
        let repo = init_repo("no-workspace");
        let script = fake_provisioner("no-workspace-p", r#"{"ok":true,"protocol_version":1}"#);
        let store = Store::open_in_memory().unwrap();
        store
            .register_project("proj", "", &repo.to_string_lossy(), "git")
            .unwrap();
        store
            .register_machine_provider(
                "ib",
                "",
                &script.to_string_lossy(),
                &[],
                crate::machines::PROTOCOL_VERSION,
                false,
            )
            .unwrap();
        let mut sessions = vec![session_row_on(
            0,
            0,
            "s0",
            Some("ralphus:new-worktree/x"),
            Some("ib:A"),
        )];
        let tasks = vec![task_row(0, Some("proj"))];
        let err = resolve_placeholders(&store, "run-1", &mut sessions, &tasks, &Context::new())
            .expect_err("a provider that provisions nothing must fail the run");
        assert!(err.contains("workspace"), "{err}");
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
