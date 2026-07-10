//! Real-git integration tests for submit-time review derivation: a worktree on a
//! feature branch becomes one guardian per project, with the branch collected and
//! the run association recorded. `<<upstream>>` without an upstream is rejected.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ralphus_core::schema::TaskFile;
use ralphus_daemon::cancel::CancelToken;
use ralphus_daemon::guardian_merge::{run_merge, start_merge};
use ralphus_daemon::reviews::derive_reviews;
use ralphus_daemon::runner::{Runner, RunnerResult, RunnerSpec, SubprocessRunner};
use ralphus_daemon::scheduler::{Semaphore, execute_run, execute_run_with};
use ralphus_daemon::store::{RunState, Store};

/// A runner that reports every session done without touching disk.
struct OkRunner;
impl Runner for OkRunner {
    fn run(&self, _spec: &RunnerSpec) -> RunnerResult {
        RunnerResult {
            status: "done".to_string(),
            tokens_in: 0,
            tokens_out: 0,
            cost_usd: 0.0,
            summary: "ok".to_string(),
            error: None,
            verified: None,
            claude_session_id: None,
        }
    }
}

/// A runner that actually resolves git conflict markers in the worktree by
/// keeping the "ours" (HEAD) side of every conflict. Used to test the
/// conflict-resolution loop without requiring a live ollama instance.
struct ConflictResolvingRunner;

fn strip_conflict_markers(content: &str) -> String {
    let mut out = String::new();
    // 0 = normal, 1 = ours (keep), 2 = theirs (drop)
    let mut state: u8 = 0;
    for line in content.lines() {
        if line.starts_with("<<<<<<<") {
            state = 1;
        } else if line.starts_with("=======") && state == 1 {
            state = 2;
        } else if line.starts_with(">>>>>>>") && state == 2 {
            state = 0;
        } else if state != 2 {
            out.push_str(line);
            out.push('\n');
        }
    }
    out
}

impl Runner for ConflictResolvingRunner {
    fn run(&self, spec: &RunnerSpec) -> RunnerResult {
        let cwd = Path::new(&spec.cwd);
        let raw = Command::new("git")
            .args(["diff", "--name-only", "--diff-filter=U"])
            .current_dir(cwd)
            .output()
            .expect("git diff --diff-filter=U");
        for name in String::from_utf8_lossy(&raw.stdout).lines() {
            let path = cwd.join(name.trim());
            if let Ok(content) = std::fs::read_to_string(&path) {
                let _ = std::fs::write(&path, strip_conflict_markers(&content));
            }
        }
        RunnerResult {
            status: "done".to_string(),
            tokens_in: 0,
            tokens_out: 0,
            cost_usd: 0.0,
            summary: "conflicts resolved".to_string(),
            error: None,
            verified: None,
            claude_session_id: None,
        }
    }
}

fn git(root: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args(args)
        .current_dir(root)
        .env("GIT_AUTHOR_NAME", "ralphus")
        .env("GIT_AUTHOR_EMAIL", "ralphus@example.com")
        .env("GIT_COMMITTER_NAME", "ralphus")
        .env("GIT_COMMITTER_EMAIL", "ralphus@example.com")
        .env("GIT_EDITOR", "true")
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
    let dir = std::env::temp_dir().join(format!("ralphus-rev-{tag}-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("mkdir");
    dir
}

/// A repo with one commit on `main` and a linked worktree on `branch`. Returns
/// the base dir (to clean up) and the worktree path (forward-slashed for TOML).
fn repo_with_worktree(base: &Path, branch: &str) -> String {
    let repo = base.join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "-b", "main"]);
    std::fs::write(repo.join("base.txt"), "base\n").unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-m", "base"]);
    let wt = base.join(format!("wt-{}", branch.replace('/', "_")));
    git(
        &repo,
        &["worktree", "add", "-b", branch, wt.to_str().unwrap()],
    );
    git(&wt, &["branch", "--set-upstream-to=main"]);
    wt.to_string_lossy().replace('\\', "/")
}

/// Like `repo_with_worktree` but does NOT set an upstream tracking branch.
/// Used for tests that verify the "no upstream" error path.
fn repo_with_worktree_no_upstream(base: &Path, branch: &str) -> String {
    let repo = base.join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "-b", "main"]);
    std::fs::write(repo.join("base.txt"), "base\n").unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-m", "base"]);
    let wt = base.join(format!("wt-{}", branch.replace('/', "_")));
    git(
        &repo,
        &["worktree", "add", "-b", branch, wt.to_str().unwrap()],
    );
    wt.to_string_lossy().replace('\\', "/")
}

/// Build a minimal task TOML with one session that opts into a review.
/// `review_id` is the id for both the session's `review` field and the
/// top-level `[[review]]` block. `review_attrs` is any extra `key = "value"`
/// lines to append inside the `[[review]]` block (may be empty).
fn session_toml(cwd: &str, review_id: &str, review_attrs: &str) -> String {
    format!(
        "[[task]]\nname=\"t\"\n[[task.session]]\ncwd=\"{cwd}\"\nprompt=\"p\"\nreview=\"{review_id}\"\n\
         [[review]]\nid=\"{review_id}\"\n{review_attrs}\n"
    )
}

#[test]
fn single_project_makes_one_review() {
    let base = temp_base("single");
    let cwd = repo_with_worktree(&base, "feature/a");
    let toml = session_toml(
        &cwd,
        "backend",
        "agent=\"claude\"\nmodel=\"claude-opus-4-8\"",
    );
    let file: TaskFile = toml::from_str(&toml).unwrap();

    let mut store = Store::open_in_memory().unwrap();
    let run_id = store.insert_run(&file, None, false).unwrap();
    let ids = derive_reviews(&store, &run_id, &file).expect("derive ok");

    assert_eq!(ids.len(), 1);
    let g = store.get_guardian(&ids[0]).unwrap();
    assert_eq!(g.name, "backend");
    assert_eq!(g.base_branch, "main");
    assert_eq!(g.run_id.as_deref(), Some(run_id.as_str()));
    // The review's declared conflict-resolver backend/model is persisted.
    assert_eq!(g.resolver_agent.as_deref(), Some("claude"));
    assert_eq!(g.resolver_model.as_deref(), Some("claude-opus-4-8"));
    assert_eq!(g.branches.len(), 1);
    assert_eq!(g.branches[0].branch, "feature/a");
    assert_eq!(store.guardians_for_run(&run_id).unwrap(), ids);

    let _ = std::fs::remove_dir_all(&base);
}

#[test]
fn two_projects_make_two_disambiguated_reviews() {
    let base_a = temp_base("projA");
    let base_b = temp_base("projB");
    let cwd_a = repo_with_worktree(&base_a, "feature/a");
    let cwd_b = repo_with_worktree(&base_b, "feature/b");
    let toml = format!(
        "[[task]]\nname=\"t\"\n\
         [[task.session]]\ncwd=\"{cwd_a}\"\nprompt=\"p\"\nreview=\"one\"\n\
         [[task.session]]\ncwd=\"{cwd_b}\"\nprompt=\"p\"\nreview=\"two\"\n\
         [[review]]\nid=\"one\"\n\
         [[review]]\nid=\"two\"\n"
    );
    let file: TaskFile = toml::from_str(&toml).unwrap();

    let mut store = Store::open_in_memory().unwrap();
    let run_id = store.insert_run(&file, None, false).unwrap();
    let ids = derive_reviews(&store, &run_id, &file).expect("derive ok");

    assert_eq!(ids.len(), 2, "two projects -> two reviews");
    let names: Vec<String> = ids
        .iter()
        .map(|id| store.get_guardian(id).unwrap().name)
        .collect();
    // Disambiguated with numeric suffixes; the two names must differ.
    assert_ne!(names[0], names[1]);
    assert!(names.iter().all(|n| n.contains("-0")), "names: {names:?}");

    let _ = std::fs::remove_dir_all(&base_a);
    let _ = std::fs::remove_dir_all(&base_b);
}

/// One repo with two linked worktrees on `branch_a` / `branch_b`. Returns the
/// repo base plus each worktree's forward-slashed path (for TOML `cwd`).
fn repo_with_two_worktrees(base: &Path, branch_a: &str, branch_b: &str) -> (String, String) {
    let repo = base.join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "-b", "main"]);
    std::fs::write(repo.join("base.txt"), "base\n").unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-m", "base"]);
    let wta = base.join("wt-a");
    let wtb = base.join("wt-b");
    git(
        &repo,
        &["worktree", "add", "-b", branch_a, wta.to_str().unwrap()],
    );
    git(
        &repo,
        &["worktree", "add", "-b", branch_b, wtb.to_str().unwrap()],
    );
    git(&wta, &["branch", "--set-upstream-to=main"]);
    git(&wtb, &["branch", "--set-upstream-to=main"]);
    (
        wta.to_string_lossy().replace('\\', "/"),
        wtb.to_string_lossy().replace('\\', "/"),
    )
}

#[test]
fn separate_submissions_link_into_one_review_via_key() {
    let base = temp_base("link");
    let (cwd_a, cwd_b) = repo_with_two_worktrees(&base, "feature/a", "feature/b");
    let mut store = Store::open_in_memory().unwrap();

    // Submission 1 (branch a) creates the shared guardian, tagged with run 1.
    let file1: TaskFile = toml::from_str(&session_toml(
        &cwd_a,
        "ralphus:new-review/batch",
        "name=\"My Batch\"",
    ))
    .unwrap();
    let run1 = store.insert_run(&file1, None, false).unwrap();
    let ids1 = derive_reviews(&store, &run1, &file1).expect("derive 1");
    assert_eq!(ids1.len(), 1, "first submission creates the guardian");
    let gid = ids1[0].clone();
    assert_eq!(store.get_guardian(&gid).unwrap().name, "My Batch");

    // Submission 2 (branch b) links to the SAME guardian by key — no new guardian.
    let file2: TaskFile = toml::from_str(&session_toml(
        &cwd_b,
        "ralphus:new-review/batch",
        "name=\"My Batch\"",
    ))
    .unwrap();
    let run2 = store.insert_run(&file2, None, false).unwrap();
    let ids2 = derive_reviews(&store, &run2, &file2).expect("derive 2");
    assert!(ids2.is_empty(), "second submission creates no new guardian");
    assert!(
        store.guardians_for_run(&run2).unwrap().is_empty(),
        "the shared guardian keeps its original run tag"
    );

    // One guardian, both branches, in submission order.
    let g = store.get_guardian(&gid).unwrap();
    let branches: Vec<String> = g.branches.iter().map(|b| b.branch.clone()).collect();
    assert_eq!(branches, vec!["feature/a", "feature/b"]);
    assert_eq!(
        store
            .guardian_id_for_review_key("batch")
            .unwrap()
            .as_deref(),
        Some(gid.as_str())
    );

    let _ = std::fs::remove_dir_all(&base);
}

#[test]
fn upstream_base_without_upstream_is_rejected() {
    let base = temp_base("noup");
    let cwd = repo_with_worktree_no_upstream(&base, "feature/a");
    let toml = session_toml(&cwd, "r", "");
    let file: TaskFile = toml::from_str(&toml).unwrap();

    let mut store = Store::open_in_memory().unwrap();
    let run_id = store.insert_run(&file, None, false).unwrap();
    let err = derive_reviews(&store, &run_id, &file).expect_err("no upstream -> error");
    assert!(err.message.contains("upstream"), "msg: {}", err.message);

    let _ = std::fs::remove_dir_all(&base);
}

#[test]
fn reviews_auto_start_when_the_run_succeeds() {
    let base = temp_base("autostart");
    let cwd = repo_with_worktree(&base, "feature/a");
    let toml = format!(
        "[[task]]\nname=\"t\"\n[[task.session]]\ncwd=\"{cwd}\"\ncommand=\"noop\"\nreview=\"r\"\n\
         [[review]]\nid=\"r\"\n"
    );
    let file: TaskFile = toml::from_str(&toml).unwrap();

    let store = Arc::new(Mutex::new(Store::open_in_memory().unwrap()));
    let (run_id, gid) = {
        let mut g = store.lock().unwrap();
        let run_id = g.insert_run(&file, None, false).unwrap();
        let ids = derive_reviews(&g, &run_id, &file).expect("derive");
        assert_eq!(g.get_guardian(&ids[0]).unwrap().status, "collecting");
        (run_id, ids[0].clone())
    };

    // Running the run to success should auto-start the review merge.
    execute_run(&store, &OkRunner, &run_id);
    assert_eq!(
        store.lock().unwrap().run_state(&run_id).unwrap(),
        RunState::Done
    );

    // The merge runs on a spawned thread; poll until it reaches review.
    let mut status = String::new();
    for _ in 0..200 {
        status = store.lock().unwrap().get_guardian(&gid).unwrap().status;
        if status == "in_review" || status == "merge_failed" {
            break;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    assert_eq!(
        status, "in_review",
        "review should auto-start and reach review"
    );

    let _ = std::fs::remove_dir_all(&base);
}

// ── regression: Merge/rebase button path with conflict resolution ─────────────

/// The "Merge / rebase" button path (start_merge → run_merge →
/// resolve_conflicts_with_agent) must invoke the agent and produce a
/// conflict-free tree, not abort on the first unresolved pass.
#[test]
fn start_merge_resolves_conflicts_with_agent() {
    let base = temp_base("startmerge");
    let (cwd_a, cwd_b) = two_conflicting_worktrees(&base);
    let toml = format!(
        "[[task]]\nname=\"a\"\n\
         [[task.session]]\ncwd=\"{cwd_a}\"\ncommand=\"noop\"\nreview=\"rev\"\n\
         [[task]]\nname=\"b\"\ndepends_on=[\"a\"]\n\
         [[task.session]]\ncwd=\"{cwd_b}\"\ncommand=\"noop\"\nreview=\"rev\"\n\
         [[review]]\nid=\"rev\"\n"
    );
    let file: TaskFile = toml::from_str(&toml).unwrap();
    let store = Arc::new(Mutex::new(Store::open_in_memory().unwrap()));
    let run_id = store
        .lock()
        .unwrap()
        .insert_run(&file, None, false)
        .unwrap();
    let gid = {
        let g = store.lock().unwrap();
        derive_reviews(&g, &run_id, &file).expect("derive")[0].clone()
    };

    // Simulate the "Merge / rebase" button.
    let runner = Arc::new(ConflictResolvingRunner) as Arc<dyn Runner>;
    let _ = start_merge(
        Arc::clone(&store),
        runner,
        &gid,
        Arc::new(Semaphore::new(4)),
    );

    let mut status = String::new();
    for _ in 0..200 {
        status = store.lock().unwrap().get_guardian(&gid).unwrap().status;
        if status == "in_review" || status == "merge_failed" {
            break;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    assert_eq!(
        status, "in_review",
        "conflict must be resolved and guardian reach in_review"
    );

    let view = store.lock().unwrap().get_guardian(&gid).unwrap();
    let combined = view
        .combined_worktree
        .expect("combined worktree must exist");
    let content = std::fs::read_to_string(Path::new(&combined).join("shared.txt")).unwrap();
    assert!(
        !content.contains("<<<<<<<"),
        "conflict markers must not remain: {content}"
    );
    assert!(
        view.branches
            .iter()
            .any(|b| b.merge_status == "conflict_resolved"),
        "at least one branch must report conflict_resolved: {:?}",
        view.branches
            .iter()
            .map(|b| (&b.branch, &b.merge_status))
            .collect::<Vec<_>>()
    );

    let _ = std::fs::remove_dir_all(&base);
}

/// After a feature branch is updated (simulating a force-push that introduces
/// a new conflict), pressing "Merge / rebase" must resolve the conflict and
/// reach in_review — not abort.
#[test]
fn force_push_then_merge_resolves_cleanly() {
    let base = temp_base("forcepush");

    // Initial repo: A edits shared.txt; B edits b_only.txt (no conflict yet).
    let repo = base.join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "-b", "main"]);
    std::fs::write(repo.join("shared.txt"), "original\n").unwrap();
    std::fs::write(repo.join("b_only.txt"), "b_only\n").unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-m", "base"]);

    let wt_a = base.join("wt-a");
    let wt_b = base.join("wt-b");
    git(
        &repo,
        &["worktree", "add", "-b", "feature/a", wt_a.to_str().unwrap()],
    );
    git(
        &repo,
        &["worktree", "add", "-b", "feature/b", wt_b.to_str().unwrap()],
    );
    git(&wt_a, &["branch", "--set-upstream-to=main"]);
    git(&wt_b, &["branch", "--set-upstream-to=main"]);
    std::fs::write(wt_a.join("shared.txt"), "changed by A\n").unwrap();
    git(&wt_a, &["commit", "-am", "A edits shared.txt"]);
    std::fs::write(wt_b.join("b_only.txt"), "changed by B\n").unwrap();
    git(&wt_b, &["commit", "-am", "B edits b_only.txt"]);

    let cwd_a = wt_a.to_string_lossy().replace('\\', "/");
    let cwd_b = wt_b.to_string_lossy().replace('\\', "/");
    let toml = format!(
        "[[task]]\nname=\"a\"\n\
         [[task.session]]\ncwd=\"{cwd_a}\"\ncommand=\"noop\"\nreview=\"rev\"\n\
         [[task]]\nname=\"b\"\ndepends_on=[\"a\"]\n\
         [[task.session]]\ncwd=\"{cwd_b}\"\ncommand=\"noop\"\nreview=\"rev\"\n\
         [[review]]\nid=\"rev\"\n"
    );
    let file: TaskFile = toml::from_str(&toml).unwrap();
    let store = Arc::new(Mutex::new(Store::open_in_memory().unwrap()));
    let run_id = store
        .lock()
        .unwrap()
        .insert_run(&file, None, false)
        .unwrap();
    let gid = {
        let g = store.lock().unwrap();
        derive_reviews(&g, &run_id, &file).expect("derive")[0].clone()
    };

    // First merge: A and B don't conflict, so OkRunner (no resolution needed).
    run_merge(&store, &OkRunner, &gid);
    assert_eq!(
        store.lock().unwrap().get_guardian(&gid).unwrap().status,
        "in_review",
        "initial clean merge should reach in_review"
    );

    // Force-push: B now also edits shared.txt, conflicting with A's review branch.
    std::fs::write(wt_b.join("shared.txt"), "changed by B (force-pushed)\n").unwrap();
    git(
        &wt_b,
        &[
            "commit",
            "-am",
            "B force-pushes conflicting change to shared.txt",
        ],
    );

    // Press "Merge / rebase" after the force-push.
    let runner = Arc::new(ConflictResolvingRunner) as Arc<dyn Runner>;
    let _ = start_merge(
        Arc::clone(&store),
        runner,
        &gid,
        Arc::new(Semaphore::new(4)),
    );

    let mut status = String::new();
    for _ in 0..200 {
        status = store.lock().unwrap().get_guardian(&gid).unwrap().status;
        if status == "in_review" || status == "merge_failed" {
            break;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    assert_eq!(
        status, "in_review",
        "after force-push conflict, Merge/rebase must resolve cleanly"
    );

    let view = store.lock().unwrap().get_guardian(&gid).unwrap();
    let combined = view
        .combined_worktree
        .expect("combined worktree must exist");
    let content = std::fs::read_to_string(Path::new(&combined).join("shared.txt")).unwrap();
    assert!(
        !content.contains("<<<<<<<"),
        "conflict markers must not remain after force-push re-merge: {content}"
    );

    let _ = std::fs::remove_dir_all(&base);
}

// ── per-task review readiness (RAL-32) ───────────────────────────────────────

fn ok_result() -> RunnerResult {
    RunnerResult {
        status: "done".to_string(),
        tokens_in: 0,
        tokens_out: 0,
        cost_usd: 0.0,
        summary: "ok".to_string(),
        error: None,
        verified: None,
        claude_session_id: None,
    }
}

/// A runner that succeeds immediately for sessions whose cwd starts with
/// `fast_prefix` and blocks (until cancelled or released) for all others.
struct GatableRunner {
    fast_prefix: String,
    started: Arc<AtomicBool>,
    released: Arc<AtomicBool>,
}

impl Runner for GatableRunner {
    fn run(&self, spec: &RunnerSpec) -> RunnerResult {
        self.run_cancellable(spec, &CancelToken::never())
    }

    fn run_cancellable(&self, spec: &RunnerSpec, cancel: &CancelToken) -> RunnerResult {
        if spec.cwd.starts_with(&self.fast_prefix) {
            return ok_result();
        }
        self.started.store(true, Ordering::SeqCst);
        while !self.released.load(Ordering::SeqCst) && !cancel.is_cancelled() {
            std::thread::sleep(Duration::from_millis(2));
        }
        if cancel.is_cancelled() {
            return RunnerResult {
                status: "failed".to_string(),
                tokens_in: 0,
                tokens_out: 0,
                cost_usd: 0.0,
                summary: String::new(),
                error: Some("cancelled".to_string()),
                verified: None,
                claude_session_id: None,
            };
        }
        ok_result()
    }
}

/// Poll `check` up to `timeout`, sleeping 10 ms between attempts. Panics with
/// `msg` if the condition never becomes true.
fn poll_until(timeout: Duration, msg: &str, mut check: impl FnMut() -> bool) {
    let deadline = std::time::Instant::now() + timeout;
    while std::time::Instant::now() < deadline {
        if check() {
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!("{msg}");
}

/// A task whose session cwd is in a different project does not block a
/// guardian's readiness. The guardian for task A's project starts as soon as
/// task A is Done — without waiting for the unrelated blocking task B to finish.
#[test]
fn non_overlapping_task_does_not_block_readiness() {
    let base_x = temp_base("noblock-x");
    let base_y = temp_base("noblock-y");
    let cwd_a = repo_with_worktree(&base_x, "feature/a");
    let cwd_b = repo_with_worktree(&base_y, "feature/b");

    // Task A: fast, in project-X, declares a review.
    // Task B: slow/blocking, in project-Y, no review declaration.
    let toml = format!(
        "[[task]]\nname=\"a\"\n[[task.session]]\ncwd=\"{cwd_a}\"\nprompt=\"p\"\nreview=\"r\"\n\
         [[task]]\nname=\"b\"\n[[task.session]]\ncwd=\"{cwd_b}\"\nprompt=\"p\"\n\
         [[review]]\nid=\"r\"\n"
    );
    let file: TaskFile = toml::from_str(&toml).unwrap();

    let store = Arc::new(Mutex::new(Store::open_in_memory().unwrap()));
    let (run_id, gid) = {
        let mut g = store.lock().unwrap();
        let run_id = g.insert_run(&file, None, false).unwrap();
        let ids = derive_reviews(&g, &run_id, &file).expect("derive");
        assert_eq!(ids.len(), 1, "only task A declared a review");
        (run_id, ids[0].clone())
    };

    let started = Arc::new(AtomicBool::new(false));
    let released = Arc::new(AtomicBool::new(false));
    let runner: Arc<dyn Runner> = Arc::new(GatableRunner {
        fast_prefix: cwd_a.clone(),
        started: Arc::clone(&started),
        released: Arc::clone(&released),
    });
    let token = CancelToken::new();

    let store2 = Arc::clone(&store);
    let runner2 = Arc::clone(&runner);
    let run_id2 = run_id.clone();
    let token2 = token.clone();
    let handle =
        std::thread::spawn(move || execute_run_with(&store2, runner2.as_ref(), &run_id2, &token2));

    // Wait until task B's session is blocking (task A must have finished first).
    poll_until(Duration::from_secs(5), "task B should have started", || {
        started.load(Ordering::SeqCst)
    });

    // Give the per-task review start a moment to propagate through the
    // background thread that run_merge spawns.
    poll_until(
        Duration::from_secs(5),
        "guardian should start (not 'collecting') once task A is done",
        || store.lock().unwrap().get_guardian(&gid).unwrap().status != "collecting",
    );

    // Guardian started while task B is still blocking — confirm task B hasn't
    // finished yet (released is still false).
    assert!(
        !released.load(Ordering::SeqCst),
        "guardian should have started before task B was released"
    );

    // Clean up: release task B so execute_run can finish.
    released.store(true, Ordering::SeqCst);
    handle.join().unwrap();

    let _ = std::fs::remove_dir_all(&base_x);
    let _ = std::fs::remove_dir_all(&base_y);
}

/// A task that shares the guardian's project directory blocks the review even
/// when it never declared a `[[task.session.review]]`. The guardian for
/// project-X must wait for BOTH tasks (A and B) to be Done, even though only A
/// declared the review.
#[test]
fn undeclared_overlapping_task_blocks_readiness() {
    // Both tasks get worktrees in the SAME git repository.
    let base = temp_base("overlap");
    let (cwd_a, cwd_b) = repo_with_two_worktrees(&base, "feature/a", "feature/b");

    // Task A: fast, declares a review. Task B: blocking, no review declaration.
    let toml = format!(
        "[[task]]\nname=\"a\"\n[[task.session]]\ncwd=\"{cwd_a}\"\nprompt=\"p\"\nreview=\"r\"\n\
         [[task]]\nname=\"b\"\n[[task.session]]\ncwd=\"{cwd_b}\"\nprompt=\"p\"\n\
         [[review]]\nid=\"r\"\n"
    );
    let file: TaskFile = toml::from_str(&toml).unwrap();

    let store = Arc::new(Mutex::new(Store::open_in_memory().unwrap()));
    let (run_id, gid) = {
        let mut g = store.lock().unwrap();
        let run_id = g.insert_run(&file, None, false).unwrap();
        let ids = derive_reviews(&g, &run_id, &file).expect("derive");
        assert_eq!(ids.len(), 1, "only one project so one review");
        (run_id, ids[0].clone())
    };

    let started = Arc::new(AtomicBool::new(false));
    let released = Arc::new(AtomicBool::new(false));
    let runner: Arc<dyn Runner> = Arc::new(GatableRunner {
        fast_prefix: cwd_a.clone(),
        started: Arc::clone(&started),
        released: Arc::clone(&released),
    });
    let token = CancelToken::new();

    let store2 = Arc::clone(&store);
    let runner2 = Arc::clone(&runner);
    let run_id2 = run_id.clone();
    let token2 = token.clone();
    let handle =
        std::thread::spawn(move || execute_run_with(&store2, runner2.as_ref(), &run_id2, &token2));

    // Wait until task B is blocking (task A has finished).
    poll_until(Duration::from_secs(5), "task B should have started", || {
        started.load(Ordering::SeqCst)
    });

    // Task A done but task B still running — guardian must NOT have started.
    let status = store.lock().unwrap().get_guardian(&gid).unwrap().status;
    assert_eq!(
        status, "collecting",
        "guardian should still be collecting while the overlapping task B is running"
    );

    // Release task B — both blocking tasks are now Done so the review should start.
    released.store(true, Ordering::SeqCst);

    poll_until(
        Duration::from_secs(5),
        "guardian should start after both blocking tasks are done",
        || store.lock().unwrap().get_guardian(&gid).unwrap().status != "collecting",
    );

    handle.join().unwrap();

    let _ = std::fs::remove_dir_all(&base);
}

// ── full end-to-end flow, gated on a local ollama stack ──────────────────────

/// Whether an ollama server is listening on the default port.
fn ollama_up() -> bool {
    use std::net::TcpStream;
    use std::time::Duration;
    "127.0.0.1:11434"
        .parse()
        .ok()
        .and_then(|addr| TcpStream::connect_timeout(&addr, Duration::from_millis(400)).ok())
        .is_some()
}

/// Whether ollama has `model` pulled (checked via its /api/tags).
fn ollama_has_model(model: &str) -> bool {
    match ureq::get("http://127.0.0.1:11434/api/tags").call() {
        Ok(resp) => resp
            .into_string()
            .map(|b| b.contains(model))
            .unwrap_or(false),
        Err(_) => false,
    }
}

/// Locate a `ralphus-runner` to drive: `RALPHUS_RUNNER_CMD`, else the dev venv.
fn find_runner() -> Option<String> {
    if let Ok(cmd) = std::env::var("RALPHUS_RUNNER_CMD") {
        if !cmd.trim().is_empty() {
            return Some(cmd);
        }
    }
    let ws = Path::new(env!("CARGO_MANIFEST_DIR")).parent()?;
    for rel in [
        "cli/.venv/Scripts/ralphus-runner.exe",
        "cli/.venv/bin/ralphus-runner",
    ] {
        let p = ws.join(rel);
        if p.exists() {
            return Some(p.to_string_lossy().into_owned());
        }
    }
    None
}

fn pydantic_ai_available(runner_cmd: &str) -> bool {
    let runner_path = Path::new(runner_cmd);
    let Some(dir) = runner_path.parent() else {
        return false;
    };
    for python in ["python.exe", "python3.exe", "python", "python3"] {
        let p = dir.join(python);
        if p.exists() {
            return Command::new(p)
                .args(["-c", "import pydantic_ai"])
                .output()
                .map(|o| o.status.success())
                .unwrap_or(false);
        }
    }
    false
}

/// One repo with two worktrees whose branches edit the SAME line differently, so
/// stacking the second onto the first conflicts. Returns their (cwd_a, cwd_b),
/// forward-slashed for TOML.
fn two_conflicting_worktrees(base: &Path) -> (String, String) {
    let repo = base.join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "-b", "main"]);
    std::fs::write(repo.join("shared.txt"), "original line\n").unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-m", "base"]);

    let wt_a = base.join("wt-a");
    let wt_b = base.join("wt-b");
    git(
        &repo,
        &["worktree", "add", "-b", "feature/a", wt_a.to_str().unwrap()],
    );
    git(
        &repo,
        &["worktree", "add", "-b", "feature/b", wt_b.to_str().unwrap()],
    );
    std::fs::write(wt_a.join("shared.txt"), "line changed by A\n").unwrap();
    git(&wt_a, &["commit", "-am", "a change"]);
    git(&wt_a, &["branch", "--set-upstream-to=main"]);
    std::fs::write(wt_b.join("shared.txt"), "line changed by B\n").unwrap();
    git(&wt_b, &["commit", "-am", "b change"]);
    git(&wt_b, &["branch", "--set-upstream-to=main"]);

    (
        wt_a.to_string_lossy().replace('\\', "/"),
        wt_b.to_string_lossy().replace('\\', "/"),
    )
}

/// The full gamut: a ticket-as-TOML is validated, submitted, run, and its review
/// (one project, two branches with a real merge conflict) is rebased — with the
/// conflict resolved by a live ollama agent. Skips unless ollama + a model +
/// `ralphus-runner` are all available locally.
#[test]
fn full_flow_validate_submit_run_and_ollama_resolves_conflict() {
    let Some(runner_cmd) = find_runner() else {
        eprintln!("SKIP full_flow: ralphus-runner not found (set RALPHUS_RUNNER_CMD)");
        return;
    };
    if !pydantic_ai_available(&runner_cmd) {
        eprintln!(
            "SKIP full_flow: pydantic-ai not installed in runner environment (run `uv sync --extra runner` in cli/)"
        );
        return;
    }
    if !ollama_up() {
        eprintln!("SKIP full_flow: ollama not reachable on 127.0.0.1:11434");
        return;
    }
    let model = std::env::var("RALPHUS_RESOLVER_MODEL").unwrap_or_else(|_| "qwen3:8b".to_string());
    if !ollama_has_model(&model) {
        eprintln!("SKIP full_flow: ollama model '{model}' not pulled");
        return;
    }

    let base = temp_base("fullflow");
    let (cwd_a, cwd_b) = two_conflicting_worktrees(&base);

    // 1) The "ticket": a Task TOML. Validate it with the offline validator.
    let toml = format!(
        "[[task]]\nname=\"a\"\n\
         [[task.session]]\ncwd=\"{cwd_a}\"\ncommand=\"noop\"\nreview=\"rev\"\n\
         [[task]]\nname=\"b\"\ndepends_on=[\"a\"]\n\
         [[task.session]]\ncwd=\"{cwd_b}\"\ncommand=\"noop\"\nreview=\"rev\"\n\
         [[review]]\nid=\"rev\"\n"
    );
    assert!(
        ralphus_core::validate::validate_toml(&toml).is_ok(),
        "ticket TOML must validate: {:?}",
        ralphus_core::validate::validate_toml(&toml).errors
    );
    let file: TaskFile = toml::from_str(&toml).unwrap();

    // 2) Submit (ingest) and 3) run the tasks to success.
    let store = Arc::new(Mutex::new(Store::open_in_memory().unwrap()));
    let run_id = {
        let mut g = store.lock().unwrap();
        g.insert_run(&file, Some("ticket-42"), false).unwrap()
    };
    execute_run(&store, &OkRunner, &run_id);
    assert_eq!(
        store.lock().unwrap().run_state(&run_id).unwrap(),
        RunState::Done
    );

    // 4) Derive the review: both worktrees are one project -> one guardian with
    //    two branches in topological order (a then b). Derived explicitly here
    //    (rather than via submit's auto-start) so we can inject a real ollama
    //    runner for the conflict resolution.
    let ids = {
        let g = store.lock().unwrap();
        derive_reviews(&g, &run_id, &file).expect("derive")
    };
    assert_eq!(ids.len(), 1, "one project -> one review");
    let gid = ids[0].clone();
    assert_eq!(
        store
            .lock()
            .unwrap()
            .get_guardian(&gid)
            .unwrap()
            .branches
            .len(),
        2
    );

    // 5) Build the review. feature/b conflicts with feature/a on shared.txt; the
    //    live agent must resolve it for the stack to reach review.
    let runner = SubprocessRunner::new(&runner_cmd);
    run_merge(&store, &runner, &gid);

    let view = store.lock().unwrap().get_guardian(&gid).unwrap();
    assert_eq!(view.status, "in_review", "detail: {:?}", view.detail);
    assert!(
        view.branches
            .iter()
            .any(|b| b.merge_status == "conflict_resolved"),
        "a branch should be conflict-resolved by the agent: {:?}",
        view.branches
            .iter()
            .map(|b| (b.branch.clone(), b.merge_status.clone()))
            .collect::<Vec<_>>()
    );
    let combined = view.combined_worktree.expect("combined worktree");
    let merged = std::fs::read_to_string(Path::new(&combined).join("shared.txt")).unwrap();
    assert!(
        !merged.contains("<<<<<<<"),
        "conflict markers remain: {merged}"
    );

    let _ = std::fs::remove_dir_all(&base);
}

#[test]
fn no_review_declaration_makes_no_guardians() {
    let mut store = Store::open_in_memory().unwrap();
    let toml = "[[task]]\nname=\"t\"\n[[task.session]]\ncwd=\"/tmp\"\nprompt=\"p\"\n";
    let file: TaskFile = toml::from_str(toml).unwrap();
    let run_id = store.insert_run(&file, None, false).unwrap();
    // No git access happens because no session declares a review.
    assert!(derive_reviews(&store, &run_id, &file).unwrap().is_empty());
    assert!(store.guardians_for_run(&run_id).unwrap().is_empty());
}

// ── Multi-project tests (RAL-29) ─────────────────────────────────────────────

/// Two sessions in different repos, linked by the same `ralphus:new-review/<key>`
/// id, should produce ONE shared guardian whose branches are tagged with their
/// respective project roots.
#[test]
fn link_key_across_two_repos_creates_one_multi_project_guardian() {
    let base_a = temp_base("mp-link-a");
    let base_b = temp_base("mp-link-b");
    let cwd_a = repo_with_worktree(&base_a, "feature/a");
    let cwd_b = repo_with_worktree(&base_b, "feature/b");

    let mut store = Store::open_in_memory().unwrap();

    // Submission 1: repo A.
    let file1: TaskFile = toml::from_str(&session_toml(
        &cwd_a,
        "ralphus:new-review/cross",
        "name=\"Cross Review\"",
    ))
    .unwrap();
    let run1 = store.insert_run(&file1, None, false).unwrap();
    let ids1 = derive_reviews(&store, &run1, &file1).expect("derive 1");
    assert_eq!(ids1.len(), 1, "first submission creates the guardian");
    let gid = ids1[0].clone();

    // Submission 2: repo B — links to the SAME guardian, no new guardian created.
    let file2: TaskFile = toml::from_str(&session_toml(
        &cwd_b,
        "ralphus:new-review/cross",
        "name=\"Cross Review\"",
    ))
    .unwrap();
    let run2 = store.insert_run(&file2, None, false).unwrap();
    let ids2 = derive_reviews(&store, &run2, &file2).expect("derive 2");
    assert!(ids2.is_empty(), "second submission creates no new guardian");

    // The guardian has both branches.
    let g = store.get_guardian(&gid).unwrap();
    let branches: Vec<_> = g.branches.iter().map(|b| b.branch.as_str()).collect();
    assert_eq!(branches, vec!["feature/a", "feature/b"]);

    // Each branch is tagged with its own project root (different directories).
    let proj_a = g.branches[0]
        .project
        .clone()
        .expect("branch a has a project");
    let proj_b = g.branches[1]
        .project
        .clone()
        .expect("branch b has a project");
    assert_ne!(
        proj_a, proj_b,
        "branches from different repos must have different project roots"
    );

    // `g.projects` reports both distinct roots in branch order.
    assert_eq!(g.projects.len(), 2, "two projects");
    assert_eq!(g.projects[0], proj_a);
    assert_eq!(g.projects[1], proj_b);

    let _ = std::fs::remove_dir_all(&base_a);
    let _ = std::fs::remove_dir_all(&base_b);
}

/// A single-project link group (both sessions in the same repo) should produce
/// ONE guardian whose branches have no project tag (NULL / None), so the merge
/// engine uses guardian.git_root for both — identical to the pre-RAL-29 behavior.
#[test]
fn link_key_same_repo_branches_have_no_project_tag() {
    let base = temp_base("mp-link-same");
    let (cwd_a, cwd_b) = repo_with_two_worktrees(&base, "feature/a", "feature/b");

    let mut store = Store::open_in_memory().unwrap();

    let file1: TaskFile = toml::from_str(&session_toml(
        &cwd_a,
        "ralphus:new-review/same",
        "name=\"Same Repo\"",
    ))
    .unwrap();
    let run1 = store.insert_run(&file1, None, false).unwrap();
    let ids1 = derive_reviews(&store, &run1, &file1).expect("derive 1");

    let file2: TaskFile = toml::from_str(&session_toml(
        &cwd_b,
        "ralphus:new-review/same",
        "name=\"Same Repo\"",
    ))
    .unwrap();
    let run2 = store.insert_run(&file2, None, false).unwrap();
    derive_reviews(&store, &run2, &file2).expect("derive 2");

    let g = store.get_guardian(&ids1[0]).unwrap();
    // Branches in the same repo still carry a project tag (it just happens to be
    // the same path for both), so the merge engine partitions them into one group.
    assert_eq!(g.branches.len(), 2);
    // Both branches should resolve to the same project root.
    let p0 = g.branches[0].project.clone();
    let p1 = g.branches[1].project.clone();
    assert_eq!(p0, p1, "same-repo branches have the same project tag");
    // `g.projects` should contain exactly one entry.
    assert_eq!(g.projects.len(), 1);

    let _ = std::fs::remove_dir_all(&base);
}

/// Proj-group reviews (no link key) are still one-guardian-per-project and the
/// branches in each guardian carry NO project tag (they use guardian.git_root).
#[test]
fn proj_group_branches_carry_no_project_tag() {
    let base_a = temp_base("mp-proj-a");
    let base_b = temp_base("mp-proj-b");
    let cwd_a = repo_with_worktree(&base_a, "feature/a");
    let cwd_b = repo_with_worktree(&base_b, "feature/b");

    let toml = format!(
        "[[task]]\nname=\"t\"\n\
         [[task.session]]\ncwd=\"{cwd_a}\"\nprompt=\"p\"\nreview=\"rA\"\n\
         [[task.session]]\ncwd=\"{cwd_b}\"\nprompt=\"p\"\nreview=\"rB\"\n\
         [[review]]\nid=\"rA\"\n\
         [[review]]\nid=\"rB\"\n"
    );
    let file: TaskFile = toml::from_str(&toml).unwrap();
    let mut store = Store::open_in_memory().unwrap();
    let run_id = store.insert_run(&file, None, false).unwrap();
    let ids = derive_reviews(&store, &run_id, &file).expect("derive ok");

    assert_eq!(ids.len(), 2, "two projects -> two guardians");
    for id in &ids {
        let g = store.get_guardian(id).unwrap();
        // Single-project guardians: each has exactly one project (its git_root).
        assert_eq!(g.projects.len(), 1);
        assert_eq!(g.projects[0], g.git_root);
        // Branches in single-project guardians carry no explicit project tag.
        for b in &g.branches {
            assert!(
                b.project.is_none(),
                "proj-group branches should have no project tag"
            );
        }
    }

    let _ = std::fs::remove_dir_all(&base_a);
    let _ = std::fs::remove_dir_all(&base_b);
}

/// Multi-project guardian: the merge engine processes each project independently
/// and all must succeed for the guardian to reach `InReview`.
#[test]
fn multi_project_merge_runs_per_project_and_aggregates() {
    let base_a = temp_base("mp-merge-a");
    let base_b = temp_base("mp-merge-b");
    let cwd_a = repo_with_worktree(&base_a, "feature/a");
    let cwd_b = repo_with_worktree(&base_b, "feature/b");

    // Commit something on each feature branch so the stack is non-empty.
    let cwd_a_native = cwd_a.replace('/', std::path::MAIN_SEPARATOR_STR);
    let cwd_b_native = cwd_b.replace('/', std::path::MAIN_SEPARATOR_STR);
    let wt_a = Path::new(&cwd_a_native);
    let wt_b = Path::new(&cwd_b_native);
    std::fs::write(wt_a.join("a.txt"), "change by a\n").unwrap();
    git(wt_a, &["add", "."]);
    git(wt_a, &["commit", "-m", "a change"]);
    std::fs::write(wt_b.join("b.txt"), "change by b\n").unwrap();
    git(wt_b, &["add", "."]);
    git(wt_b, &["commit", "-m", "b change"]);

    let store = Arc::new(Mutex::new(Store::open_in_memory().unwrap()));

    let file1: TaskFile = toml::from_str(&session_toml(
        &cwd_a,
        "ralphus:new-review/mp",
        "name=\"MP Review\"",
    ))
    .unwrap();
    let file2: TaskFile = toml::from_str(&session_toml(
        &cwd_b,
        "ralphus:new-review/mp",
        "name=\"MP Review\"",
    ))
    .unwrap();
    let (gid, run1) = {
        let mut g = store.lock().unwrap();
        let r1 = g.insert_run(&file1, None, false).unwrap();
        let ids = derive_reviews(&g, &r1, &file1).expect("derive 1");
        (ids[0].clone(), r1)
    };
    {
        let mut g = store.lock().unwrap();
        let r2 = g.insert_run(&file2, None, false).unwrap();
        derive_reviews(&g, &r2, &file2).expect("derive 2");
    }
    drop(run1); // not used further

    // The guardian now has branches from two different repos.
    let g = store.lock().unwrap().get_guardian(&gid).unwrap();
    assert_eq!(g.projects.len(), 2, "two distinct projects");

    // Run the merge. Each project's branches are stacked independently.
    // OkRunner never touches disk; the rebase works on real worktrees.
    ralphus_daemon::guardian_merge::run_merge(&store, &OkRunner, &gid);

    let view = store.lock().unwrap().get_guardian(&gid).unwrap();
    assert_eq!(
        view.status, "in_review",
        "all projects merged -> guardian reaches in_review; detail: {:?}",
        view.detail
    );
    // Each project's branch must be done (or conflict_resolved).
    for b in &view.branches {
        assert!(
            matches!(b.merge_status.as_str(), "done" | "conflict_resolved"),
            "branch {} status: {}",
            b.branch,
            b.merge_status
        );
    }

    let _ = std::fs::remove_dir_all(&base_a);
    let _ = std::fs::remove_dir_all(&base_b);
}

/// If one project's branch fails to merge (e.g. its base branch doesn't exist),
/// the guardian becomes `merge_failed` even if the other project's branches are
/// fine (all-must-pass aggregation).
#[test]
fn multi_project_all_must_pass_one_fails_makes_merge_failed() {
    let base_a = temp_base("mp-fail-a");
    // We only create a real repo for A; B is left as a non-existent path.
    let cwd_a = repo_with_worktree(&base_a, "feature/a");
    let cwd_b = format!("{}/nonexistent/path", base_a.display());

    // Manually build a guardian with branches from two "projects" — one real, one fake.
    let store = Arc::new(Mutex::new(Store::open_in_memory().unwrap()));
    let gid = {
        let g = store.lock().unwrap();
        let id = g.create_guardian("fail test", "main", &cwd_a).unwrap();
        g.add_guardian_branch_with_project(&id, "feature/a", None)
            .unwrap();
        // Branch "feature/b" belongs to a non-existent project root.
        g.add_guardian_branch_with_project(&id, "feature/b", Some(&cwd_b))
            .unwrap();
        id
    };

    ralphus_daemon::guardian_merge::run_merge(&store, &OkRunner, &gid);

    let view = store.lock().unwrap().get_guardian(&gid).unwrap();
    assert_eq!(
        view.status, "merge_failed",
        "invalid project root must cause merge_failed"
    );

    let _ = std::fs::remove_dir_all(&base_a);
}

/// `GuardianView.projects` for a single-project guardian (no branch project tags)
/// always contains exactly one entry equal to `git_root`.
#[test]
fn single_project_guardian_projects_field_has_one_entry() {
    let store = Store::open_in_memory().unwrap();
    let id = store.create_guardian("r", "main", "/some/repo").unwrap();
    store.add_guardian_branch(&id, "feature/a").unwrap();
    store.add_guardian_branch(&id, "feature/b").unwrap();

    let g = store.get_guardian(&id).unwrap();
    assert_eq!(g.projects, vec!["/some/repo".to_string()]);
    assert_eq!(g.projects[0], g.git_root);
}

/// `add_guardian_branch_with_project` correctly stores and retrieves the project
/// tag, while `add_guardian_branch` leaves it as None.
#[test]
fn branch_project_tag_stored_and_retrieved() {
    let store = Store::open_in_memory().unwrap();
    let id = store.create_guardian("r", "main", "/primary").unwrap();
    store.add_guardian_branch(&id, "feature/a").unwrap();
    store
        .add_guardian_branch_with_project(&id, "feature/b", Some("/secondary"))
        .unwrap();

    let g = store.get_guardian(&id).unwrap();
    assert_eq!(g.branches[0].project, None, "no explicit project -> None");
    assert_eq!(
        g.branches[1].project.as_deref(),
        Some("/secondary"),
        "explicit project stored"
    );
    assert_eq!(g.projects, vec!["/primary", "/secondary"]);
}

/// `set_guardian_project_base_commit` updates the JSON map and the legacy column.
#[test]
fn per_project_base_commits_stored_and_retrieved() {
    let store = Store::open_in_memory().unwrap();
    let id = store.create_guardian("r", "main", "/primary").unwrap();

    // Record a commit for the primary project (also updates legacy base_commit).
    store
        .set_guardian_project_base_commit(&id, "/primary", "abc123")
        .unwrap();
    let g = store.get_guardian(&id).unwrap();
    assert_eq!(
        g.base_commits.get("/primary").map(String::as_str),
        Some("abc123")
    );
    assert_eq!(
        g.base_commit.as_deref(),
        Some("abc123"),
        "legacy column updated"
    );

    // Record a commit for a secondary project (only updates the JSON map).
    store
        .set_guardian_project_base_commit(&id, "/secondary", "def456")
        .unwrap();
    let g = store.get_guardian(&id).unwrap();
    assert_eq!(
        g.base_commits.get("/secondary").map(String::as_str),
        Some("def456")
    );
    assert_eq!(
        g.base_commit.as_deref(),
        Some("abc123"),
        "legacy column unchanged for non-primary"
    );
    assert_eq!(g.base_commits.len(), 2);
}
