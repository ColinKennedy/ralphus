//! Integration tests for project registration + placeholder `cwd` worktree
//! materialization (RAL-100): a cell `cwd` of the form
//! `ralphus:new-worktree/<branch>?upstream=<upstream>` names a branch to
//! check out, and the owning task's `project` field names a project
//! registered via `POST /api/projects` (or `ralphus project git`) to
//! materialize it under, instead of a real filesystem path; the scheduler
//! resolves it to a real git worktree before running the cell. The
//! `?upstream=` suffix is required (validated at submit time and re-checked
//! defensively at resolution time) and decides what the freshly materialized
//! branch tracks.
//!
//! Two levels, both always run (no live agent/model needed -- placeholder
//! resolution and validation are pure daemon/store logic):
//!
//! 1. **HTTP-level** -- drives `POST /api/projects` and `POST /api/squads`
//!    through `server::route` (no socket) to prove submit-time validation
//!    (missing/unregistered `project`) fails fast, before a squad is ever
//!    scheduled.
//! 2. **Pipeline-level** -- submits a placeholder-`cwd` task, runs it with a
//!    `CapturingRunner` (mirrors `monorepo.rs`), and asserts the resolved real
//!    worktree path reaches the runner. A second `execute_squad` pass (as a
//!    restart would trigger) proves the worktree is reused, not recreated.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use ralphus_core::schema::TaskFile;
use ralphus_daemon::runner::{Runner, RunnerResult, RunnerSpec};
use ralphus_daemon::scheduler::execute_squad;
use ralphus_daemon::server::{Daemon, route};
use ralphus_daemon::store::{SquadState, Store};

// ── Helpers ───────────────────────────────────────────────────────────────

fn git(root: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args(args)
        .current_dir(root)
        .env("GIT_AUTHOR_NAME", "ralphus")
        .env("GIT_AUTHOR_EMAIL", "ralphus@example.com")
        .env("GIT_COMMITTER_NAME", "ralphus")
        .env("GIT_COMMITTER_EMAIL", "ralphus@example.com")
        .output()
        .expect("run git");
    assert!(
        out.status.success(),
        "git {:?} failed: {}",
        args,
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn temp_base(tag: &str) -> PathBuf {
    static N: AtomicU32 = AtomicU32::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("ralphus-ral100-{tag}-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("mkdir");
    dir
}

/// A fresh repo with one commit on `main`. Forward-slashed for TOML/JSON.
fn init_repo(base: &Path) -> String {
    let repo = base.join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "-b", "main"]);
    std::fs::write(repo.join("base.txt"), "base\n").unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-m", "base"]);
    repo.to_string_lossy().replace('\\', "/")
}

fn init_repo_with_remote_branch(base: &Path, branch: &str) -> (String, String) {
    let remote = base.join("remote.git");
    let (_, remote_branch) = branch
        .split_once('/')
        .expect("remote-qualified branch placeholder");
    git(
        base,
        &[
            "init",
            "--bare",
            "--initial-branch=main",
            &remote.to_string_lossy(),
        ],
    );

    let seed = base.join("seed");
    std::fs::create_dir_all(&seed).unwrap();
    git(&seed, &["init", "-b", "main"]);
    std::fs::write(seed.join("base.txt"), "base\n").unwrap();
    git(&seed, &["add", "."]);
    git(&seed, &["commit", "-m", "base"]);
    git(
        &seed,
        &["remote", "add", "origin", &remote.to_string_lossy()],
    );
    git(&seed, &["push", "-u", "origin", "main"]);

    git(&seed, &["checkout", "-b", remote_branch]);
    std::fs::write(seed.join("remote-only.txt"), format!("{branch}\n")).unwrap();
    git(&seed, &["add", "."]);
    git(&seed, &["commit", "-m", "remote branch"]);
    let remote_sha = git(&seed, &["rev-parse", "HEAD"]).trim().to_string();
    git(&seed, &["push", "-u", "origin", remote_branch]);

    let clone = base.join("repo");
    git(
        base,
        &["clone", &remote.to_string_lossy(), &clone.to_string_lossy()],
    );
    (clone.to_string_lossy().replace('\\', "/"), remote_sha)
}

#[derive(Clone, Default)]
struct CapturingRunner {
    specs: Arc<Mutex<Vec<RunnerSpec>>>,
}

impl CapturingRunner {
    fn captured(&self) -> Vec<RunnerSpec> {
        self.specs.lock().unwrap().clone()
    }
}

impl Runner for CapturingRunner {
    fn run(&self, spec: &RunnerSpec) -> RunnerResult {
        self.specs.lock().unwrap().push(spec.clone());
        RunnerResult {
            status: "done".to_string(),
            tokens_in: 0,
            tokens_out: 0,
            cost_usd: 0.0,
            summary: "captured".to_string(),
            error: None,
            proofed: None,
            agent_session_id: None,
            ghost: None,
        }
    }
}

fn run_placeholder_task(repo: &str, branch: &str, upstream: &str) -> String {
    let daemon = Daemon::new(Store::open_in_memory().unwrap(), 4);
    let reg_body =
        serde_json::json!({"name": "proj", "description": "", "path": repo, "vcs": "git"})
            .to_string();
    assert_eq!(
        route(&daemon, "POST", "/api/projects", &reg_body).status,
        201
    );

    // no_commit_required: this helper is about worktree/branch placeholder
    // resolution, not the RAL-156 commit guard, and CapturingRunner never
    // actually commits.
    let toml = format!(
        "[[task]]\nname=\"t\"\nproject=\"proj\"\nno_commit_required=true\n\
         [[task.cell]]\ncwd=\"ralphus:new-worktree/{branch}?upstream={upstream}\"\nprompt=\"do work\"\n"
    );
    let file: TaskFile = toml::from_str(&toml).unwrap();
    let store = daemon.store_handle();
    let squad_id = store
        .lock()
        .unwrap()
        .insert_squad(&file, None, false)
        .unwrap();

    let runner = CapturingRunner::default();
    execute_squad(&store, &runner, &squad_id);
    assert_eq!(
        store.lock().unwrap().squad_state(&squad_id).unwrap(),
        SquadState::Done,
        "squad must complete"
    );

    let specs = runner.captured();
    assert_eq!(specs.len(), 1);
    specs[0].cwd.clone()
}

// ── HTTP-level: submit-time project validation ──────────────────────────────

#[test]
fn placeholder_cwd_with_registered_project_submits_successfully() {
    let base = temp_base("submit-ok");
    let repo = init_repo(&base);
    let daemon = Daemon::new(Store::open_in_memory().unwrap(), 4);

    let reg_body =
        serde_json::json!({"name": "proj", "description": "d", "path": repo, "vcs": "git"})
            .to_string();
    let reply = route(&daemon, "POST", "/api/projects", &reg_body);
    assert_eq!(reply.status, 201, "register: {}", reply.body);

    let toml = "[[task]]\nname=\"t\"\nproject=\"proj\"\n\
                [[task.cell]]\ncwd=\"<<ralphus:new-worktree/feat?upstream=main>>\"\nprompt=\"do work\"\n";
    let submit_body = serde_json::json!({"toml": toml}).to_string();
    let reply = route(&daemon, "POST", "/api/squads", &submit_body);
    assert_eq!(reply.status, 201, "submit: {}", reply.body);

    let _ = std::fs::remove_dir_all(&base);
}

#[test]
fn placeholder_cwd_with_unregistered_project_is_rejected_at_submit() {
    let daemon = Daemon::new(Store::open_in_memory().unwrap(), 4);

    let toml = "[[task]]\nname=\"t\"\nproject=\"ghost\"\n\
                [[task.cell]]\ncwd=\"<<ralphus:new-worktree/feat?upstream=main>>\"\nprompt=\"do work\"\n";
    let submit_body = serde_json::json!({"toml": toml}).to_string();
    let reply = route(&daemon, "POST", "/api/squads", &submit_body);
    assert_eq!(reply.status, 400, "submit: {}", reply.body);
    assert!(
        reply.body.contains("ghost"),
        "error should name the unregistered project: {}",
        reply.body
    );
}

#[test]
fn placeholder_cwd_without_task_project_fails_structural_validation() {
    // core's validate_toml already requires `project` whenever a placeholder
    // cwd is used; confirm that failure surfaces through the submit endpoint
    // (not just `ralphus validate`), before any registry lookup happens.
    let daemon = Daemon::new(Store::open_in_memory().unwrap(), 4);

    let toml = "[[task]]\nname=\"t\"\n\
                [[task.cell]]\ncwd=\"ralphus:new-worktree/feat\"\nprompt=\"do work\"\n";
    let submit_body = serde_json::json!({"toml": toml}).to_string();
    let reply = route(&daemon, "POST", "/api/squads", &submit_body);
    assert_eq!(reply.status, 400, "submit: {}", reply.body);
    assert!(
        reply.body.contains("validation_failed"),
        "must fail core structural validation: {}",
        reply.body
    );
}

#[test]
fn register_project_rejects_non_git_path() {
    let base = temp_base("not-git");
    std::fs::create_dir_all(&base).unwrap();
    let daemon = Daemon::new(Store::open_in_memory().unwrap(), 4);

    let reg_body = serde_json::json!({
        "name": "proj",
        "description": "",
        "path": base.to_string_lossy().replace('\\', "/"),
        "vcs": "git",
    })
    .to_string();
    let reply = route(&daemon, "POST", "/api/projects", &reg_body);
    assert_eq!(reply.status, 400, "register: {}", reply.body);

    let _ = std::fs::remove_dir_all(&base);
}

// ── Pipeline-level: placeholder resolves through submit -> schedule -> run ──

/// A placeholder-cwd cell, once submitted and run, must reach the runner
/// with `cwd` rewritten to the real materialized worktree path -- and that
/// worktree must actually exist on disk as a linked git worktree.
#[test]
fn placeholder_cwd_resolves_to_a_real_worktree_through_full_pipeline() {
    let base = temp_base("resolve");
    let repo = init_repo(&base);

    let daemon = Daemon::new(Store::open_in_memory().unwrap(), 4);
    let reg_body =
        serde_json::json!({"name": "proj", "description": "", "path": repo, "vcs": "git"})
            .to_string();
    assert_eq!(
        route(&daemon, "POST", "/api/projects", &reg_body).status,
        201
    );

    // no_commit_required: this test is about worktree materialization, not
    // the RAL-156 commit guard, and CapturingRunner never actually commits.
    let toml = "[[task]]\nname=\"t\"\nproject=\"proj\"\nno_commit_required=true\n\
                [[task.cell]]\ncwd=\"ralphus:new-worktree/feat-x?upstream=main\"\nprompt=\"do work\"\n";
    let file: TaskFile = toml::from_str(toml).unwrap();

    let store = daemon.store_handle();
    let squad_id = store
        .lock()
        .unwrap()
        .insert_squad(&file, None, false)
        .unwrap();

    let runner = CapturingRunner::default();
    execute_squad(&store, &runner, &squad_id);

    assert_eq!(
        store.lock().unwrap().squad_state(&squad_id).unwrap(),
        SquadState::Done,
        "squad must complete"
    );

    let specs = runner.captured();
    assert_eq!(specs.len(), 1);
    let resolved_cwd = specs[0].cwd.clone();
    assert!(
        !resolved_cwd.contains("ralphus:new-worktree/"),
        "cwd must be rewritten to a real path, got {resolved_cwd}"
    );
    let wt = Path::new(&resolved_cwd);
    assert!(wt.join(".git").exists(), "must be a real git worktree");
    assert!(
        resolved_cwd
            .replace('\\', "/")
            .ends_with(".git/.ralphus/w/feat-x"),
        "must live under .git/.ralphus/w/<short>: {resolved_cwd}"
    );

    let _ = std::fs::remove_dir_all(&base);
}

/// The same placeholder repeated across two cells in one submission must
/// materialize exactly one worktree, and both cells must resolve to the
/// identical real path.
#[test]
fn shared_placeholder_across_cells_builds_one_worktree() {
    let base = temp_base("shared");
    let repo = init_repo(&base);

    let daemon = Daemon::new(Store::open_in_memory().unwrap(), 4);
    let reg_body =
        serde_json::json!({"name": "proj", "description": "", "path": repo, "vcs": "git"})
            .to_string();
    assert_eq!(
        route(&daemon, "POST", "/api/projects", &reg_body).status,
        201
    );

    // no_commit_required: this test is about worktree deduplication, not
    // the RAL-156 commit guard, and CapturingRunner never actually commits.
    let toml = "[[task]]\nname=\"t1\"\nproject=\"proj\"\nno_commit_required=true\n\
                [[task.cell]]\ncwd=\"ralphus:new-worktree/shared-branch?upstream=main\"\nprompt=\"a\"\n\
                [[task]]\nname=\"t2\"\nproject=\"proj\"\nno_commit_required=true\n\
                [[task.cell]]\ncwd=\"ralphus:new-worktree/shared-branch?upstream=main\"\nprompt=\"b\"\n";
    let file: TaskFile = toml::from_str(toml).unwrap();

    let store = daemon.store_handle();
    let squad_id = store
        .lock()
        .unwrap()
        .insert_squad(&file, None, false)
        .unwrap();

    let runner = CapturingRunner::default();
    execute_squad(&store, &runner, &squad_id);

    assert_eq!(
        store.lock().unwrap().squad_state(&squad_id).unwrap(),
        SquadState::Done
    );

    let specs = runner.captured();
    assert_eq!(specs.len(), 2);
    assert_eq!(
        specs[0].cwd, specs[1].cwd,
        "both cells must resolve to the identical worktree path"
    );

    let list = git(Path::new(&repo), &["worktree", "list", "--porcelain"]);
    let count = list
        .lines()
        .filter(|l| l.starts_with("branch") && l.ends_with("shared-branch"))
        .count();
    assert_eq!(count, 1, "exactly one worktree for the shared branch");

    let _ = std::fs::remove_dir_all(&base);
}

/// A restarted squad whose placeholder was already materialized must not
/// recreate (or error on) the worktree -- it reuses the persisted real path.
#[test]
fn restarted_squad_reuses_already_materialized_worktree() {
    let base = temp_base("restart");
    let repo = init_repo(&base);

    let daemon = Daemon::new(Store::open_in_memory().unwrap(), 4);
    let reg_body =
        serde_json::json!({"name": "proj", "description": "", "path": repo, "vcs": "git"})
            .to_string();
    assert_eq!(
        route(&daemon, "POST", "/api/projects", &reg_body).status,
        201
    );

    // no_commit_required: this test is about worktree reuse across a
    // restart, not the RAL-156 commit guard, and CapturingRunner never
    // actually commits.
    let toml = "[[task]]\nname=\"t\"\nproject=\"proj\"\nno_commit_required=true\n\
                [[task.cell]]\ncwd=\"ralphus:new-worktree/restart-branch?upstream=main\"\nprompt=\"do work\"\n";
    let file: TaskFile = toml::from_str(toml).unwrap();

    let store = daemon.store_handle();
    let squad_id = store
        .lock()
        .unwrap()
        .insert_squad(&file, None, false)
        .unwrap();

    let runner = CapturingRunner::default();
    execute_squad(&store, &runner, &squad_id);
    let first_cwd = runner.captured()[0].cwd.clone();

    // Leave a marker so a wipe-and-recreate would be caught.
    std::fs::write(Path::new(&first_cwd).join("marker.txt"), "keep me\n").unwrap();

    // Simulate a restart: reset squad/task/cell state to Pending (mirrors
    // Store::restart_squad) without touching the already-resolved `cwd` column.
    store
        .lock()
        .unwrap()
        .reset_squad_to_pending(&squad_id)
        .unwrap();

    let runner2 = CapturingRunner::default();
    execute_squad(&store, &runner2, &squad_id);

    assert_eq!(
        store.lock().unwrap().squad_state(&squad_id).unwrap(),
        SquadState::Done,
        "restarted squad must complete"
    );
    let second_cwd = runner2.captured()[0].cwd.clone();
    assert_eq!(
        first_cwd, second_cwd,
        "restart must resolve to the same worktree path"
    );
    assert!(
        Path::new(&first_cwd).join("marker.txt").exists(),
        "restart must not wipe/recreate the existing worktree"
    );

    let list = git(Path::new(&repo), &["worktree", "list", "--porcelain"]);
    let count = list
        .lines()
        .filter(|l| l.starts_with("branch") && l.ends_with("restart-branch"))
        .count();
    assert_eq!(count, 1, "restart must not create a second worktree");

    let _ = std::fs::remove_dir_all(&base);
}

#[test]
fn placeholder_cwd_origin_foo_uses_the_remote_tracking_branch_when_present() {
    let base = temp_base("origin-foo");
    let (repo, remote_sha) = init_repo_with_remote_branch(&base, "origin/foo");

    let resolved_cwd = run_placeholder_task(&repo, "origin/foo", "origin/foo");
    let wt = Path::new(&resolved_cwd);
    assert!(
        resolved_cwd
            .replace('\\', "/")
            .ends_with(".git/.ralphus/w/origin-foo"),
        "must live under the short .git/.ralphus/w/<short> layout (RAL-211): {resolved_cwd}"
    );
    assert_eq!(
        git(wt, &["symbolic-ref", "--short", "HEAD"]).trim(),
        "origin/foo"
    );
    assert_eq!(git(wt, &["rev-parse", "HEAD"]).trim(), remote_sha);
    assert_eq!(
        git(
            wt,
            &[
                "rev-parse",
                "--abbrev-ref",
                "--symbolic-full-name",
                "@{upstream}",
            ]
        )
        .trim(),
        "remotes/origin/foo"
    );
    assert!(wt.join("remote-only.txt").exists());
    let _ = git(
        Path::new(&repo),
        &["show-ref", "--verify", "refs/heads/origin/foo"],
    );

    let _ = std::fs::remove_dir_all(&base);
}

/// A worktree materialized from a remote-tracking placeholder must not stay
/// frozen at whatever the remote held on first materialization: a second,
/// independent run submitted later against the same placeholder resyncs it
/// (fetch + rebase) to whatever has since been pushed.
#[test]
fn placeholder_cwd_origin_foo_resyncs_across_separate_run_submissions() {
    let base = temp_base("origin-foo-resync");
    let (repo, first_sha) = init_repo_with_remote_branch(&base, "origin/foo");

    let first_cwd = run_placeholder_task(&repo, "origin/foo", "origin/foo");
    assert_eq!(
        git(Path::new(&first_cwd), &["rev-parse", "HEAD"]).trim(),
        first_sha
    );

    // A new commit lands on the remote branch between the two runs.
    let seed = base.join("seed");
    std::fs::write(seed.join("remote-only.txt"), "origin/foo v2\n").unwrap();
    git(&seed, &["commit", "-am", "second remote commit"]);
    let second_sha = git(&seed, &["rev-parse", "HEAD"]).trim().to_string();
    git(&seed, &["push", "origin", "foo"]);

    let second_cwd = run_placeholder_task(&repo, "origin/foo", "origin/foo");
    assert_eq!(first_cwd, second_cwd, "the same worktree path is reused");
    assert_eq!(
        git(Path::new(&second_cwd), &["rev-parse", "HEAD"]).trim(),
        second_sha,
        "the worktree must resync to the new remote push on the second resolution"
    );

    let _ = std::fs::remove_dir_all(&base);
}

#[test]
fn placeholder_cwd_alternative_foo_creates_a_literal_local_branch_when_no_remote_ref_exists() {
    let base = temp_base("alternative-foo");
    let repo = init_repo(&base);

    let resolved_cwd = run_placeholder_task(&repo, "alternative/foo", "main");
    let wt = Path::new(&resolved_cwd);
    assert!(
        resolved_cwd
            .replace('\\', "/")
            .ends_with(".git/.ralphus/w/alternative"),
        "must live under the short .git/.ralphus/w/<short> layout (RAL-211): {resolved_cwd}"
    );
    assert_eq!(
        git(wt, &["symbolic-ref", "--short", "HEAD"]).trim(),
        "alternative/foo"
    );
    assert_eq!(
        git(wt, &["rev-parse", "HEAD"]).trim(),
        git(Path::new(&repo), &["rev-parse", "main"]).trim()
    );
    let _ = git(
        Path::new(&repo),
        &["show-ref", "--verify", "refs/heads/alternative/foo"],
    );

    let _ = std::fs::remove_dir_all(&base);
}

#[test]
fn placeholder_cwd_nested_remote_looking_name_keeps_literal_branch_but_collapses_worktree_path() {
    let base = temp_base("alternative-nested");
    let repo = init_repo(&base);

    // The git *branch* keeps every segment of a deep slash-containing name --
    // only the on-disk worktree directory is collapsed to a short, truncated
    // name (RAL-211), so a deeply nested repo root still fits Windows'
    // MAX_PATH regardless of how long the placeholder branch name is.
    let branch = "alternative/feature/nested/foo";
    let resolved_cwd = run_placeholder_task(&repo, branch, "main");
    let wt = Path::new(&resolved_cwd);
    assert!(
        resolved_cwd
            .replace('\\', "/")
            .ends_with(".git/.ralphus/w/alternative"),
        "must live under the short .git/.ralphus/w/<short> layout (RAL-211): {resolved_cwd}"
    );
    assert_eq!(git(wt, &["symbolic-ref", "--short", "HEAD"]).trim(), branch);
    let _ = git(
        Path::new(&repo),
        &[
            "show-ref",
            "--verify",
            "refs/heads/alternative/feature/nested/foo",
        ],
    );

    let _ = std::fs::remove_dir_all(&base);
}
