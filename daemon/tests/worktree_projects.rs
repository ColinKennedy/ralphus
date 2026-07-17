//! Integration tests for project registration + placeholder `cwd` worktree
//! materialization (RAL-100): a session `cwd` of the form
//! `ralphus:new-worktree/<branch>` names a branch to check out, and the
//! owning task's `project` field names a project registered via
//! `POST /api/projects` (or `ralphus project git`) to materialize it under,
//! instead of a real filesystem path; the scheduler resolves it to a real git
//! worktree before running the session.
//!
//! Two levels, both always run (no live agent/model needed -- placeholder
//! resolution and validation are pure daemon/store logic):
//!
//! 1. **HTTP-level** -- drives `POST /api/projects` and `POST /api/runs`
//!    through `server::route` (no socket) to prove submit-time validation
//!    (missing/unregistered `project`) fails fast, before a run is ever
//!    scheduled.
//! 2. **Pipeline-level** -- submits a placeholder-`cwd` task, runs it with a
//!    `CapturingRunner` (mirrors `monorepo.rs`), and asserts the resolved real
//!    worktree path reaches the runner. A second `execute_run` pass (as a
//!    restart would trigger) proves the worktree is reused, not recreated.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use ralphus_core::schema::TaskFile;
use ralphus_daemon::runner::{Runner, RunnerResult, RunnerSpec};
use ralphus_daemon::scheduler::execute_run;
use ralphus_daemon::server::{Daemon, route};
use ralphus_daemon::store::{RunState, Store};

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
            verified: None,
            claude_session_id: None,
            ghost: None,
        }
    }
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
                [[task.session]]\ncwd=\"ralphus:new-worktree/feat\"\nprompt=\"do work\"\n";
    let submit_body = serde_json::json!({"toml": toml}).to_string();
    let reply = route(&daemon, "POST", "/api/runs", &submit_body);
    assert_eq!(reply.status, 201, "submit: {}", reply.body);

    let _ = std::fs::remove_dir_all(&base);
}

#[test]
fn placeholder_cwd_with_unregistered_project_is_rejected_at_submit() {
    let daemon = Daemon::new(Store::open_in_memory().unwrap(), 4);

    let toml = "[[task]]\nname=\"t\"\nproject=\"ghost\"\n\
                [[task.session]]\ncwd=\"ralphus:new-worktree/feat\"\nprompt=\"do work\"\n";
    let submit_body = serde_json::json!({"toml": toml}).to_string();
    let reply = route(&daemon, "POST", "/api/runs", &submit_body);
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
                [[task.session]]\ncwd=\"ralphus:new-worktree/feat\"\nprompt=\"do work\"\n";
    let submit_body = serde_json::json!({"toml": toml}).to_string();
    let reply = route(&daemon, "POST", "/api/runs", &submit_body);
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

/// A placeholder-cwd session, once submitted and run, must reach the runner
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

    let toml = "[[task]]\nname=\"t\"\nproject=\"proj\"\n\
                [[task.session]]\ncwd=\"ralphus:new-worktree/feat-x\"\nprompt=\"do work\"\n";
    let file: TaskFile = toml::from_str(toml).unwrap();

    let store = daemon.store_handle();
    let run_id = store
        .lock()
        .unwrap()
        .insert_run(&file, None, false)
        .unwrap();

    let runner = CapturingRunner::default();
    execute_run(&store, &runner, &run_id);

    assert_eq!(
        store.lock().unwrap().run_state(&run_id).unwrap(),
        RunState::Done,
        "run must complete"
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
            .ends_with(".git/.ralphus_worktrees/feat-x"),
        "must live under .git/.ralphus_worktrees/<branch>: {resolved_cwd}"
    );

    let _ = std::fs::remove_dir_all(&base);
}

/// The same placeholder repeated across two sessions in one submission must
/// materialize exactly one worktree, and both sessions must resolve to the
/// identical real path.
#[test]
fn shared_placeholder_across_sessions_builds_one_worktree() {
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

    let toml = "[[task]]\nname=\"t1\"\nproject=\"proj\"\n\
                [[task.session]]\ncwd=\"ralphus:new-worktree/shared-branch\"\nprompt=\"a\"\n\
                [[task]]\nname=\"t2\"\nproject=\"proj\"\n\
                [[task.session]]\ncwd=\"ralphus:new-worktree/shared-branch\"\nprompt=\"b\"\n";
    let file: TaskFile = toml::from_str(toml).unwrap();

    let store = daemon.store_handle();
    let run_id = store
        .lock()
        .unwrap()
        .insert_run(&file, None, false)
        .unwrap();

    let runner = CapturingRunner::default();
    execute_run(&store, &runner, &run_id);

    assert_eq!(
        store.lock().unwrap().run_state(&run_id).unwrap(),
        RunState::Done
    );

    let specs = runner.captured();
    assert_eq!(specs.len(), 2);
    assert_eq!(
        specs[0].cwd, specs[1].cwd,
        "both sessions must resolve to the identical worktree path"
    );

    let list = git(Path::new(&repo), &["worktree", "list", "--porcelain"]);
    let count = list
        .lines()
        .filter(|l| l.starts_with("branch") && l.ends_with("shared-branch"))
        .count();
    assert_eq!(count, 1, "exactly one worktree for the shared branch");

    let _ = std::fs::remove_dir_all(&base);
}

/// A restarted run whose placeholder was already materialized must not
/// recreate (or error on) the worktree -- it reuses the persisted real path.
#[test]
fn restarted_run_reuses_already_materialized_worktree() {
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

    let toml = "[[task]]\nname=\"t\"\nproject=\"proj\"\n\
                [[task.session]]\ncwd=\"ralphus:new-worktree/restart-branch\"\nprompt=\"do work\"\n";
    let file: TaskFile = toml::from_str(toml).unwrap();

    let store = daemon.store_handle();
    let run_id = store
        .lock()
        .unwrap()
        .insert_run(&file, None, false)
        .unwrap();

    let runner = CapturingRunner::default();
    execute_run(&store, &runner, &run_id);
    let first_cwd = runner.captured()[0].cwd.clone();

    // Leave a marker so a wipe-and-recreate would be caught.
    std::fs::write(Path::new(&first_cwd).join("marker.txt"), "keep me\n").unwrap();

    // Simulate a restart: reset run/task/session state to Pending (mirrors
    // Store::restart_run) without touching the already-resolved `cwd` column.
    store.lock().unwrap().reset_run_to_pending(&run_id).unwrap();

    let runner2 = CapturingRunner::default();
    execute_run(&store, &runner2, &run_id);

    assert_eq!(
        store.lock().unwrap().run_state(&run_id).unwrap(),
        RunState::Done,
        "restarted run must complete"
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
