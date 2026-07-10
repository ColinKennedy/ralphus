//! Real-git integration tests for the Guardian merge engine: a clean two-branch
//! stack, and a conflicting branch resolved by a (fake) agent that strips
//! conflict markers.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use ralphus_daemon::guardian_merge::{rebuild_on_base_shift, run_chat, run_feedback, run_merge};
use ralphus_daemon::runner::{Runner, RunnerResult, RunnerSpec};
use ralphus_daemon::scheduler::Semaphore;
use ralphus_daemon::store::Store;

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

fn write(root: &Path, name: &str, content: &str) {
    std::fs::write(root.join(name), content).expect("write file");
}

fn temp_repo() -> PathBuf {
    static N: AtomicU32 = AtomicU32::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("ralphus-guardian-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("mkdir");
    dir
}

/// A runner that is never expected to be invoked (no conflicts).
struct NoopRunner;
impl Runner for NoopRunner {
    fn run(&self, _spec: &RunnerSpec) -> RunnerResult {
        RunnerResult::failure("noop runner should not be called")
    }
}

/// A fake conflict resolver that strips conflict-marker lines, stages everything
/// with `git add -A`, and emits `RALPHUS_STAGE: DONE` — exercising the fast path
/// where the orchestrator advances the rebase without re-scanning for markers.
struct StageDoneRunner;
impl Runner for StageDoneRunner {
    fn run(&self, spec: &RunnerSpec) -> RunnerResult {
        let cwd = PathBuf::from(&spec.cwd);
        if let Ok(entries) = std::fs::read_dir(&cwd) {
            for entry in entries.flatten() {
                let path = entry.path();
                if !path.is_file() {
                    continue;
                }
                if let Ok(content) = std::fs::read_to_string(&path) {
                    if content.contains("<<<<<<<") {
                        let cleaned: String = content
                            .lines()
                            .filter(|l| {
                                !l.starts_with("<<<<<<<")
                                    && !l.starts_with("=======")
                                    && !l.starts_with(">>>>>>>")
                            })
                            .map(|l| format!("{l}\n"))
                            .collect();
                        let _ = std::fs::write(&path, cleaned);
                    }
                }
            }
        }
        // Stage the resolved files so `git rebase --continue` can proceed.
        let ok = Command::new("git")
            .args(["add", "-A"])
            .current_dir(&cwd)
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        assert!(
            ok,
            "StageDoneRunner: git add -A failed in {}",
            cwd.display()
        );
        RunnerResult {
            status: "done".into(),
            tokens_in: 0,
            tokens_out: 0,
            cost_usd: 0.0,
            summary: "resolved\nRALPHUS_STAGE: DONE".into(),
            error: None,
            verified: None,
            claude_session_id: None,
        }
    }
}

/// A fake conflict resolver: strips conflict-marker lines from every top-level
/// file in the worktree, leaving a marker-free (both-sides) result.
struct MarkerStrippingRunner;
impl Runner for MarkerStrippingRunner {
    fn run(&self, spec: &RunnerSpec) -> RunnerResult {
        let cwd = PathBuf::from(&spec.cwd);
        if let Ok(entries) = std::fs::read_dir(&cwd) {
            for entry in entries.flatten() {
                let path = entry.path();
                if !path.is_file() {
                    continue;
                }
                if let Ok(content) = std::fs::read_to_string(&path) {
                    if content.contains("<<<<<<<") {
                        let cleaned: String = content
                            .lines()
                            .filter(|l| {
                                !l.starts_with("<<<<<<<")
                                    && !l.starts_with("=======")
                                    && !l.starts_with(">>>>>>>")
                            })
                            .map(|l| format!("{l}\n"))
                            .collect();
                        let _ = std::fs::write(&path, cleaned);
                    }
                }
            }
        }
        RunnerResult {
            status: "done".into(),
            tokens_in: 0,
            tokens_out: 0,
            cost_usd: 0.0,
            summary: "resolved".into(),
            error: None,
            verified: None,
            claude_session_id: None,
        }
    }
}

/// A fake reviewer agent: writes a `note.txt` into the worktree it is given.
struct FeedbackRunner;
impl Runner for FeedbackRunner {
    fn run(&self, spec: &RunnerSpec) -> RunnerResult {
        let _ = std::fs::write(PathBuf::from(&spec.cwd).join("note.txt"), "reviewed\n");
        RunnerResult {
            status: "done".into(),
            tokens_in: 0,
            tokens_out: 0,
            cost_usd: 0.0,
            summary: "edited".into(),
            error: None,
            verified: None,
            claude_session_id: None,
        }
    }
}

/// A fake reviewer agent: writes a named file into the worktree it is given.
struct NamedFeedbackRunner(pub &'static str);
impl Runner for NamedFeedbackRunner {
    fn run(&self, spec: &RunnerSpec) -> RunnerResult {
        let _ = std::fs::write(PathBuf::from(&spec.cwd).join(self.0), "reviewed\n");
        RunnerResult {
            status: "done".into(),
            tokens_in: 0,
            tokens_out: 0,
            cost_usd: 0.0,
            summary: "edited".into(),
            error: None,
            verified: None,
            claude_session_id: None,
        }
    }
}

fn init_repo(root: &Path) {
    git(root, &["init", "-b", "main"]);
}

#[test]
fn builds_review_branch_from_two_features() {
    let root = temp_repo();
    init_repo(&root);
    write(&root, "base.txt", "base\n");
    git(&root, &["add", "."]);
    git(&root, &["commit", "-m", "base"]);

    git(&root, &["checkout", "-b", "feature/a"]);
    write(&root, "a.txt", "from a\n");
    git(&root, &["add", "."]);
    git(&root, &["commit", "-m", "add a"]);

    git(&root, &["checkout", "main"]);
    git(&root, &["checkout", "-b", "feature/b"]);
    write(&root, "b.txt", "from b\n");
    git(&root, &["add", "."]);
    git(&root, &["commit", "-m", "add b"]);
    git(&root, &["checkout", "main"]);

    let store = Arc::new(Mutex::new(Store::open_in_memory().unwrap()));
    let id = {
        let g = store.lock().unwrap();
        let id = g
            .create_guardian("review", "main", root.to_str().unwrap())
            .unwrap();
        g.add_guardian_branch(&id, "feature/a").unwrap();
        g.add_guardian_branch(&id, "feature/b").unwrap();
        id
    };

    run_merge(&store, &NoopRunner, &id);

    let view = store.lock().unwrap().get_guardian(&id).unwrap();
    assert_eq!(view.status, "in_review", "detail: {:?}", view.detail);
    let review = view.review_branch.expect("review branch set");
    assert!(view.branches.iter().all(|b| b.merge_status == "done"));

    let files = git(&root, &["ls-tree", "-r", "--name-only", &review]);
    assert!(files.contains("base.txt") && files.contains("a.txt") && files.contains("b.txt"));

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn per_branch_and_combined_worktrees_are_recorded() {
    let root = temp_repo();
    init_repo(&root);
    write(&root, "base.txt", "base\n");
    git(&root, &["add", "."]);
    git(&root, &["commit", "-m", "base"]);

    git(&root, &["checkout", "-b", "feature/a"]);
    write(&root, "a.txt", "from a\n");
    git(&root, &["add", "."]);
    git(&root, &["commit", "-m", "add a"]);
    git(&root, &["checkout", "main"]);
    git(&root, &["checkout", "-b", "feature/b"]);
    write(&root, "b.txt", "from b\n");
    git(&root, &["add", "."]);
    git(&root, &["commit", "-m", "add b"]);
    git(&root, &["checkout", "main"]);

    let store = Arc::new(Mutex::new(Store::open_in_memory().unwrap()));
    let id = {
        let g = store.lock().unwrap();
        let id = g
            .create_guardian("r", "main", root.to_str().unwrap())
            .unwrap();
        g.add_guardian_branch(&id, "feature/a").unwrap();
        g.add_guardian_branch(&id, "feature/b").unwrap();
        id
    };
    run_merge(&store, &NoopRunner, &id);

    let view = store.lock().unwrap().get_guardian(&id).unwrap();
    assert_eq!(view.status, "in_review", "detail: {:?}", view.detail);

    // Each feature has its own recorded review branch + worktree on disk.
    for b in &view.branches {
        let rb = b.review_branch.as_deref().expect("review branch recorded");
        assert!(
            rb.starts_with(&format!("guardian/{id}/wt-")),
            "review branch: {rb}"
        );
        let wt = b.worktree.as_deref().expect("worktree recorded");
        assert!(Path::new(wt).is_dir(), "worktree dir exists: {wt}");
    }
    // The combined worktree is a real dir holding both features' files.
    let combined = view
        .combined_worktree
        .as_deref()
        .expect("combined worktree");
    assert!(Path::new(combined).join("a.txt").exists());
    assert!(Path::new(combined).join("b.txt").exists());

    // The task branches themselves are untouched (read-only): feature/a still
    // carries only a.txt.
    let files_a = git(&root, &["ls-tree", "-r", "--name-only", "feature/a"]);
    assert!(files_a.contains("a.txt") && !files_a.contains("b.txt"));

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn feedback_edits_review_worktree_and_restacks_downstream() {
    let root = temp_repo();
    init_repo(&root);
    write(&root, "base.txt", "base\n");
    git(&root, &["add", "."]);
    git(&root, &["commit", "-m", "base"]);

    git(&root, &["checkout", "-b", "feature/a"]);
    write(&root, "a.txt", "from a\n");
    git(&root, &["add", "."]);
    git(&root, &["commit", "-m", "add a"]);
    git(&root, &["checkout", "main"]);
    git(&root, &["checkout", "-b", "feature/b"]);
    write(&root, "b.txt", "from b\n");
    git(&root, &["add", "."]);
    git(&root, &["commit", "-m", "add b"]);
    git(&root, &["checkout", "main"]);

    let store = Arc::new(Mutex::new(Store::open_in_memory().unwrap()));
    let id = {
        let g = store.lock().unwrap();
        let id = g
            .create_guardian("r", "main", root.to_str().unwrap())
            .unwrap();
        g.add_guardian_branch(&id, "feature/a").unwrap();
        g.add_guardian_branch(&id, "feature/b").unwrap();
        id
    };
    run_merge(&store, &NoopRunner, &id);

    // Give feedback on branch 0 (feature/a); the agent adds note.txt.
    run_feedback(&store, &FeedbackRunner, &id, 0, "add a note file");

    let view = store.lock().unwrap().get_guardian(&id).unwrap();
    assert_eq!(view.status, "in_review", "detail: {:?}", view.detail);

    // The feedback lands on branch 0's review branch...
    let rev0 = view.branches[0].review_branch.clone().unwrap();
    let files0 = git(&root, &["ls-tree", "-r", "--name-only", &rev0]);
    assert!(files0.contains("note.txt"), "feedback commit on branch 0");
    assert_eq!(view.branches[0].detail.as_deref(), Some("feedback applied"));

    // ...and the downstream branch + combined worktree are rebuilt on top of it.
    let combined = view
        .combined_worktree
        .as_deref()
        .expect("combined worktree");
    assert!(Path::new(combined).join("note.txt").exists());
    assert!(Path::new(combined).join("b.txt").exists());
    let review = view.review_branch.unwrap();
    let files = git(&root, &["ls-tree", "-r", "--name-only", &review]);
    assert!(files.contains("note.txt") && files.contains("a.txt") && files.contains("b.txt"));

    let _ = std::fs::remove_dir_all(&root);
}

fn single_feature_repo() -> (PathBuf, Arc<Mutex<Store>>, String) {
    let root = temp_repo();
    init_repo(&root);
    write(&root, "base.txt", "base\n");
    git(&root, &["add", "."]);
    git(&root, &["commit", "-m", "base"]);
    git(&root, &["checkout", "-b", "feature/a"]);
    write(&root, "a.txt", "from a\n");
    git(&root, &["add", "."]);
    git(&root, &["commit", "-m", "add a"]);
    git(&root, &["checkout", "main"]);

    let store = Arc::new(Mutex::new(Store::open_in_memory().unwrap()));
    let id = {
        let g = store.lock().unwrap();
        let id = g
            .create_guardian("r", "main", root.to_str().unwrap())
            .unwrap();
        g.add_guardian_branch(&id, "feature/a").unwrap();
        id
    };
    (root, store, id)
}

#[test]
fn passing_check_gate_reaches_review() {
    let (root, store, id) = single_feature_repo();
    store
        .lock()
        .unwrap()
        .set_guardian_checks(&id, &["test -f a.txt".to_string()])
        .unwrap();
    run_merge(&store, &NoopRunner, &id);
    assert_eq!(
        store.lock().unwrap().get_guardian(&id).unwrap().status,
        "in_review"
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn failing_check_gate_fails_the_merge() {
    let (root, store, id) = single_feature_repo();
    store
        .lock()
        .unwrap()
        .set_guardian_checks(&id, &["test -f does_not_exist.txt".to_string()])
        .unwrap();
    run_merge(&store, &NoopRunner, &id);
    let view = store.lock().unwrap().get_guardian(&id).unwrap();
    assert_eq!(view.status, "merge_failed");
    assert!(view.detail.unwrap_or_default().contains("check failed"));
    let _ = std::fs::remove_dir_all(&root);
}

// Regression (the reported bug): a feature branch stacked ON TOP of an earlier
// one (so it contains the earlier branch's commit) must keep its OWN commit and
// not collapse to a no-op. The old range cherry-pick halted on the shared,
// already-applied commit and silently dropped the branch; rebase drops the
// redundant commit and replays the unique one.
#[test]
fn stacked_feature_branch_keeps_its_own_commit() {
    let root = temp_repo();
    init_repo(&root);
    write(&root, "base.txt", "base\n");
    git(&root, &["add", "."]);
    git(&root, &["commit", "-m", "base"]);

    // feature/a off main adds a.txt.
    git(&root, &["checkout", "-b", "feature/a"]);
    write(&root, "a.txt", "from a\n");
    git(&root, &["add", "."]);
    git(&root, &["commit", "-m", "add a"]);

    // feature/b is cut FROM feature/a (so it also contains "add a") and adds b.txt.
    git(&root, &["checkout", "-b", "feature/b"]);
    write(&root, "b.txt", "from b\n");
    git(&root, &["add", "."]);
    git(&root, &["commit", "-m", "add b"]);
    git(&root, &["checkout", "main"]);

    let store = Arc::new(Mutex::new(Store::open_in_memory().unwrap()));
    let id = {
        let g = store.lock().unwrap();
        let id = g
            .create_guardian("stacked", "main", root.to_str().unwrap())
            .unwrap();
        g.add_guardian_branch(&id, "feature/a").unwrap();
        g.add_guardian_branch(&id, "feature/b").unwrap();
        id
    };

    run_merge(&store, &NoopRunner, &id);

    let view = store.lock().unwrap().get_guardian(&id).unwrap();
    assert_eq!(view.status, "in_review", "detail: {:?}", view.detail);
    assert!(view.branches.iter().all(|b| b.merge_status == "done"));
    // The combined review has BOTH features' files — b's commit was not dropped.
    let combined = view.combined_worktree.as_deref().expect("combined");
    assert!(Path::new(combined).join("a.txt").exists(), "a.txt present");
    assert!(
        Path::new(combined).join("b.txt").exists(),
        "b.txt present (feature/b's own commit was not dropped)"
    );
    // The base commit the stack was built against is recorded.
    assert!(view.base_commit.is_some(), "base_commit recorded");

    let _ = std::fs::remove_dir_all(&root);
}

// CCTL-156 skip-worktrees path: the stack is assembled in ONE shared worktree by
// rebasing each feature onto the combined branch (detached-HEAD, then advance the
// branch). Both features must land, and every branch points at the shared branch.
#[test]
fn skip_worktrees_shared_stack_rebases_all_branches() {
    let root = temp_repo();
    init_repo(&root);
    write(&root, "base.txt", "base\n");
    git(&root, &["add", "."]);
    git(&root, &["commit", "-m", "base"]);

    git(&root, &["checkout", "-b", "feature/a"]);
    write(&root, "a.txt", "from a\n");
    git(&root, &["add", "."]);
    git(&root, &["commit", "-m", "add a"]);
    git(&root, &["checkout", "main"]);
    git(&root, &["checkout", "-b", "feature/b"]);
    write(&root, "b.txt", "from b\n");
    git(&root, &["add", "."]);
    git(&root, &["commit", "-m", "add b"]);
    git(&root, &["checkout", "main"]);

    let store = Arc::new(Mutex::new(Store::open_in_memory().unwrap()));
    let id = {
        let g = store.lock().unwrap();
        let id = g
            .create_guardian("shared", "main", root.to_str().unwrap())
            .unwrap();
        g.set_guardian_skip_worktrees(&id, true).unwrap();
        g.add_guardian_branch(&id, "feature/a").unwrap();
        g.add_guardian_branch(&id, "feature/b").unwrap();
        id
    };

    run_merge(&store, &NoopRunner, &id);

    let view = store.lock().unwrap().get_guardian(&id).unwrap();
    assert_eq!(view.status, "in_review", "detail: {:?}", view.detail);
    assert!(view.branches.iter().all(|b| b.merge_status == "done"));
    // Every branch shares the one combined review branch.
    let expected_combined = format!("guardian/{id}/review");
    assert!(
        view.branches
            .iter()
            .all(|b| b.review_branch.as_deref() == Some(expected_combined.as_str())),
        "branches: {:?}",
        view.branches
    );
    let combined = view.combined_worktree.as_deref().expect("combined");
    assert!(Path::new(combined).join("a.txt").exists());
    assert!(Path::new(combined).join("b.txt").exists());

    let _ = std::fs::remove_dir_all(&root);
}

// A feature branch that adds nothing over the base (already merged) is reported
// `done` but with an explanatory detail — not a silent, work-free success.
#[test]
fn branch_with_no_new_commits_is_surfaced() {
    let (root, store, id) = single_feature_repo();
    // Replace the single feature with one that has NO commits beyond main.
    {
        let g = store.lock().unwrap();
        let g2 = g
            .create_guardian("empty", "main", root.to_str().unwrap())
            .unwrap();
        git(&root, &["branch", "feature/empty", "main"]);
        g.add_guardian_branch(&g2, "feature/empty").unwrap();
        drop(g);
        run_merge(&store, &NoopRunner, &g2);
        let view = store.lock().unwrap().get_guardian(&g2).unwrap();
        assert_eq!(view.status, "in_review", "detail: {:?}", view.detail);
        assert_eq!(view.branches[0].merge_status, "done");
        let detail = view.branches[0].detail.clone().unwrap_or_default();
        assert!(detail.contains("no new commits"), "detail: {detail}");
    }
    let _ = std::fs::remove_dir_all(&root);
    let _ = id;
}

// When the base branch gains new commits after a review is built, a maintenance
// sweep rebuilds the stack onto the new base automatically, back to in_review.
#[test]
fn base_branch_shift_triggers_rebuild() {
    let (root, store, id) = single_feature_repo();
    run_merge(&store, &NoopRunner, &id);
    let before = store.lock().unwrap().get_guardian(&id).unwrap();
    assert_eq!(before.status, "in_review");
    let base_before = before.base_commit.clone().expect("base recorded");

    // Advance the base branch (main) with a new commit.
    git(&root, &["checkout", "main"]);
    write(&root, "c.txt", "on base\n");
    git(&root, &["add", "."]);
    git(&root, &["commit", "-m", "base moves forward"]);

    let sem = Semaphore::new(4);
    let rebuilt = rebuild_on_base_shift(&store, &NoopRunner, &id, &sem);
    assert!(rebuilt, "a base-branch shift should trigger a rebuild");

    let after = store.lock().unwrap().get_guardian(&id).unwrap();
    assert_eq!(after.status, "in_review", "detail: {:?}", after.detail);
    assert_ne!(
        after.base_commit.as_deref(),
        Some(base_before.as_str()),
        "base_commit advanced"
    );
    // The rebuilt combined review contains BOTH the feature file and the new base
    // commit's file.
    let combined = after.combined_worktree.as_deref().expect("combined");
    assert!(Path::new(combined).join("a.txt").exists(), "feature kept");
    assert!(
        Path::new(combined).join("c.txt").exists(),
        "new base commit picked up"
    );

    // A second sweep with the base unchanged is a no-op.
    assert!(
        !rebuild_on_base_shift(&store, &NoopRunner, &id, &sem),
        "no rebuild when base is unchanged"
    );

    let _ = std::fs::remove_dir_all(&root);
}

// Lever 2 (carry-forward) regression: once a review branch's conflict is resolved,
// a LATER base shift must NOT re-run the resolver for the same conflict — even with
// git rerere OFF. The prior resolved commit is replayed onto the new base. This
// simulates the exact reported scenario: a task branch that goes out of date with
// its upstream, is resolved once, then the upstream moves again and the resolution
// must survive without any agent call. Pure git; the agent is only used for build 1.
#[test]
fn base_shift_replays_prior_resolution_without_agent_or_rerere() {
    let root = temp_repo();
    init_repo(&root);
    // Prove the point WITHOUT rerere: force it off so a passing test can only be
    // explained by carry-forward, not by a resolution the machine's git replayed.
    git(&root, &["config", "rerere.enabled", "false"]);

    write(&root, "conflict.txt", "line1\nBASE\nline3\n");
    git(&root, &["add", "."]);
    git(&root, &["commit", "-m", "base"]);

    // The task/feature branch is cut from main and changes the middle line to X.
    git(&root, &["checkout", "-b", "feature/x"]);
    write(&root, "conflict.txt", "line1\nX\nline3\n");
    git(&root, &["commit", "-am", "x"]);
    git(&root, &["checkout", "main"]);

    // The upstream (base) moves and touches the SAME line → out of date, conflicts.
    write(&root, "conflict.txt", "line1\nMAIN2\nline3\n");
    git(&root, &["commit", "-am", "main advances"]);

    let store = Arc::new(Mutex::new(Store::open_in_memory().unwrap()));
    let id = {
        let g = store.lock().unwrap();
        let id = g
            .create_guardian("carry", "main", root.to_str().unwrap())
            .unwrap();
        g.add_guardian_branch(&id, "feature/x").unwrap();
        id
    };

    // Build 1: rebasing feature/x onto the advanced base conflicts; the agent
    // resolves it (StageDoneRunner strips markers, keeping both sides).
    run_merge(&store, &StageDoneRunner, &id);
    let v1 = store.lock().unwrap().get_guardian(&id).unwrap();
    assert_eq!(v1.status, "in_review", "detail: {:?}", v1.detail);
    let x1 = v1
        .branches
        .iter()
        .find(|b| b.branch == "feature/x")
        .unwrap();
    assert_eq!(x1.merge_status, "conflict_resolved");
    let review1 = v1.review_branch.clone().expect("review branch");
    let show1 = git(&root, &["show", &format!("{review1}:conflict.txt")]);
    assert!(
        !show1.contains("<<<<<<<"),
        "markers remain after build 1: {show1}"
    );
    assert!(
        show1.contains('X') && show1.contains("MAIN2"),
        "both sides kept: {show1}"
    );

    // The base shifts AGAIN, in an unrelated file — no new conflict on conflict.txt.
    git(&root, &["checkout", "main"]);
    write(&root, "unrelated.txt", "later\n");
    git(&root, &["add", "."]);
    git(&root, &["commit", "-m", "unrelated base move"]);

    // Build 2 via the auto-rebuild path, with a runner that MUST NOT be called.
    // Carry-forward replays the already-resolved commit onto the new base, so the
    // resolver agent is never invoked. If it regressed and re-derived from the
    // feature tip, the old conflict would resurface, NoopRunner would be called,
    // and the guardian would end merge_failed — caught by the assertion below.
    let sem = Semaphore::new(4);
    assert!(
        rebuild_on_base_shift(&store, &NoopRunner, &id, &sem),
        "the second base shift should trigger a rebuild"
    );

    let v2 = store.lock().unwrap().get_guardian(&id).unwrap();
    assert_eq!(
        v2.status, "in_review",
        "carry-forward should reach review without the agent; detail: {:?}",
        v2.detail
    );
    let review2 = v2.review_branch.clone().expect("review branch");
    let show2 = git(&root, &["show", &format!("{review2}:conflict.txt")]);
    assert!(
        !show2.contains("<<<<<<<"),
        "conflict reappeared on rebuild: {show2}"
    );
    assert!(
        show2.contains('X') && show2.contains("MAIN2"),
        "resolved content did not survive the rebuild: {show2}"
    );
    let combined = v2.combined_worktree.as_deref().expect("combined");
    assert!(
        Path::new(combined).join("unrelated.txt").exists(),
        "new base commit picked up"
    );

    // The protection refs pinned during the rebuild are cleaned up afterwards.
    let carry = git(
        &root,
        &[
            "for-each-ref",
            "--format=%(refname)",
            &format!("refs/ralphus/carry/{id}"),
        ],
    );
    assert!(
        carry.trim().is_empty(),
        "carry-forward refs leaked: {carry}"
    );

    let _ = std::fs::remove_dir_all(&root);
}

// The carry-forward protection refs must be cleaned up even when the rebuild
// fails partway through — the `CarryRefs` guard runs on the early-return path.
#[test]
fn carry_forward_refs_are_cleaned_up_when_a_rebuild_fails() {
    let root = temp_repo();
    init_repo(&root);
    write(&root, "base.txt", "base\n");
    git(&root, &["add", "."]);
    git(&root, &["commit", "-m", "base"]);

    git(&root, &["checkout", "-b", "feature/a"]);
    write(&root, "a.txt", "from a\n");
    git(&root, &["add", "."]);
    git(&root, &["commit", "-m", "add a"]);
    git(&root, &["checkout", "main"]);

    let store = Arc::new(Mutex::new(Store::open_in_memory().unwrap()));
    let id = {
        let g = store.lock().unwrap();
        let id = g
            .create_guardian("carry-fail", "main", root.to_str().unwrap())
            .unwrap();
        g.add_guardian_branch(&id, "feature/a").unwrap();
        id
    };

    // Build 1 succeeds and records a review branch (no conflict).
    run_merge(&store, &NoopRunner, &id);
    assert_eq!(
        store.lock().unwrap().get_guardian(&id).unwrap().status,
        "in_review"
    );

    // Arm a check gate that always fails, then shift the base so a rebuild runs.
    // During that rebuild carry-forward pins build 1's review commit, then the
    // check gate fails and the merge returns early — the guard must still fire.
    store
        .lock()
        .unwrap()
        .set_guardian_checks(&id, &["test -f nonexistent.txt".to_string()])
        .unwrap();
    git(&root, &["checkout", "main"]);
    write(&root, "c.txt", "on base\n");
    git(&root, &["add", "."]);
    git(&root, &["commit", "-m", "base moves"]);

    let sem = Semaphore::new(4);
    assert!(rebuild_on_base_shift(&store, &NoopRunner, &id, &sem));
    assert_eq!(
        store.lock().unwrap().get_guardian(&id).unwrap().status,
        "merge_failed"
    );

    let carry = git(
        &root,
        &[
            "for-each-ref",
            "--format=%(refname)",
            &format!("refs/ralphus/carry/{id}"),
        ],
    );
    assert!(
        carry.trim().is_empty(),
        "carry-forward refs leaked after a failed rebuild: {carry}"
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn resolves_a_conflict_with_the_agent() {
    let root = temp_repo();
    init_repo(&root);
    write(&root, "conflict.txt", "line1\nBASE\nline3\n");
    git(&root, &["add", "."]);
    git(&root, &["commit", "-m", "base"]);

    // feature/x changes the middle line to X.
    git(&root, &["checkout", "-b", "feature/x"]);
    write(&root, "conflict.txt", "line1\nX\nline3\n");
    git(&root, &["commit", "-am", "x"]);

    // feature/y (from main) changes the same line to Y -> conflicts with x.
    git(&root, &["checkout", "main"]);
    git(&root, &["checkout", "-b", "feature/y"]);
    write(&root, "conflict.txt", "line1\nY\nline3\n");
    git(&root, &["commit", "-am", "y"]);
    git(&root, &["checkout", "main"]);

    let store = Arc::new(Mutex::new(Store::open_in_memory().unwrap()));
    let id = {
        let g = store.lock().unwrap();
        let id = g
            .create_guardian("conflict review", "main", root.to_str().unwrap())
            .unwrap();
        g.add_guardian_branch(&id, "feature/x").unwrap();
        g.add_guardian_branch(&id, "feature/y").unwrap();
        id
    };

    run_merge(&store, &MarkerStrippingRunner, &id);

    let view = store.lock().unwrap().get_guardian(&id).unwrap();
    assert_eq!(view.status, "in_review", "detail: {:?}", view.detail);
    let review = view.review_branch.expect("review branch");
    // The second branch conflicted and was resolved by the agent.
    let y = view
        .branches
        .iter()
        .find(|b| b.branch == "feature/y")
        .unwrap();
    assert_eq!(y.merge_status, "conflict_resolved");

    // The review branch's file has no conflict markers left.
    let show = git(&root, &["show", &format!("{review}:conflict.txt")]);
    assert!(!show.contains("<<<<<<<"), "markers remain: {show}");
    assert!(
        show.contains('X') && show.contains('Y'),
        "both sides kept: {show}"
    );

    let _ = std::fs::remove_dir_all(&root);
}

// New naming convention: per-branch review refs are
// `guardian/<id>/wt-<feature_branch>` and the combined ref is
// `guardian/<id>/review`.
#[test]
fn new_naming_convention_per_branch_and_combined() {
    let root = temp_repo();
    init_repo(&root);
    write(&root, "base.txt", "base\n");
    git(&root, &["add", "."]);
    git(&root, &["commit", "-m", "base"]);

    git(&root, &["checkout", "-b", "feature/a"]);
    write(&root, "a.txt", "from a\n");
    git(&root, &["add", "."]);
    git(&root, &["commit", "-m", "add a"]);
    git(&root, &["checkout", "main"]);
    git(&root, &["checkout", "-b", "feature/b"]);
    write(&root, "b.txt", "from b\n");
    git(&root, &["add", "."]);
    git(&root, &["commit", "-m", "add b"]);
    git(&root, &["checkout", "main"]);

    let store = Arc::new(Mutex::new(Store::open_in_memory().unwrap()));
    let id = {
        let g = store.lock().unwrap();
        let id = g
            .create_guardian("naming", "main", root.to_str().unwrap())
            .unwrap();
        g.add_guardian_branch(&id, "feature/a").unwrap();
        g.add_guardian_branch(&id, "feature/b").unwrap();
        id
    };
    run_merge(&store, &NoopRunner, &id);

    let view = store.lock().unwrap().get_guardian(&id).unwrap();
    assert_eq!(view.status, "in_review", "detail: {:?}", view.detail);

    // Per-branch refs follow `guardian/<id>/wt-<feature_branch>`.
    let rb0 = view.branches[0].review_branch.as_deref().unwrap();
    let rb1 = view.branches[1].review_branch.as_deref().unwrap();
    assert_eq!(rb0, format!("guardian/{id}/wt-feature/a"));
    assert_eq!(rb1, format!("guardian/{id}/wt-feature/b"));

    // The combined review branch is named `guardian/<id>/review`.
    assert_eq!(
        view.review_branch.as_deref(),
        Some(format!("guardian/{id}/review").as_str())
    );

    let _ = std::fs::remove_dir_all(&root);
}

// Verify-synthesis integration: when a guardian is linked to a run whose session
// has verify steps, `resolve_conflicts_with_agent` must:
//   1. invoke a "verify-synthesis" LLM call whose prompt lists every step
//      (command-kind, prompt-kind, and task-level) in the expected format; and
//   2. inject the synthesised quality bar into the subsequent "resolve" call's
//      prompt so the conflict-resolver agent knows what standard to meet.
#[test]
fn conflict_resolution_synthesizes_verify_steps_into_resolver_prompt() {
    // --- git repo with a conflicting branch pair ---
    let root = temp_repo();
    init_repo(&root);
    write(&root, "conflict.txt", "line1\nBASE\nline3\n");
    git(&root, &["add", "."]);
    git(&root, &["commit", "-m", "base"]);

    git(&root, &["checkout", "-b", "feature/x"]);
    write(&root, "conflict.txt", "line1\nX\nline3\n");
    git(&root, &["commit", "-am", "x"]);

    git(&root, &["checkout", "main"]);
    git(&root, &["checkout", "-b", "feature/y"]);
    write(&root, "conflict.txt", "line1\nY\nline3\n");
    git(&root, &["commit", "-am", "y"]);
    git(&root, &["checkout", "main"]);

    // --- run with session verify steps (command + prompt) and a task verify ---
    const TASK_TOML: &str = r#"
[[task]]
name = "my-task"
[[task.session]]
id = "impl"
cwd = "/repo"
prompt = "implement the feature"
[[task.session.verify]]
command = "cargo fmt --check"
[[task.session.verify]]
prompt = "The code must follow project style guidelines and be readable"
[[task.verify]]
command = "cargo test --workspace"
"#;

    let store = Arc::new(Mutex::new(Store::open_in_memory().unwrap()));

    // Insert the run then link the guardian to it. feature/y is the branch that
    // will conflict (rebased on top of feature/x), so its session gets the verify
    // steps.
    let run_id = {
        let task_file: ralphus_core::schema::TaskFile = toml::from_str(TASK_TOML).unwrap();
        store
            .lock()
            .unwrap()
            .insert_run(&task_file, Some("synthesis test"), false)
            .unwrap()
    };

    let guardian_id = {
        let g = store.lock().unwrap();
        let id = g
            .create_guardian_for_run("review", "main", root.to_str().unwrap(), Some(&run_id))
            .unwrap();
        g.add_guardian_branch(&id, "feature/x").unwrap();
        g.add_guardian_branch(&id, "feature/y").unwrap();
        // Link task 0, session 0 to feature/y so its verify steps are picked up
        // during synthesis.
        g.set_session_review_branch(&run_id, 0, 0, "feature/y")
            .unwrap();
        id
    };

    // --- capturing runner ---
    // On a "verify-synthesis" call: record the spec and return a fixed summary.
    // On a "resolve" call: record the spec and strip conflict markers so the
    // rebase can complete (mirrors MarkerStrippingRunner).
    const SYNTH_SUMMARY: &str = "Run `cargo fmt --check` and fix failures. Run `cargo test --workspace`. \
         Code must be clean and readable.";

    struct TestRunner {
        specs: Arc<Mutex<Vec<RunnerSpec>>>,
        synth_summary: &'static str,
    }
    impl Runner for TestRunner {
        fn run(&self, spec: &RunnerSpec) -> RunnerResult {
            self.specs.lock().unwrap().push(spec.clone());

            if spec.task == "verify-synthesis" {
                return RunnerResult {
                    status: "done".into(),
                    tokens_in: 15,
                    tokens_out: 30,
                    cost_usd: 0.0,
                    summary: self.synth_summary.into(),
                    error: None,
                    verified: None,
                    claude_session_id: None,
                };
            }

            // Resolution call: strip conflict markers and signal done.
            let cwd = PathBuf::from(&spec.cwd);
            if let Ok(entries) = std::fs::read_dir(&cwd) {
                for entry in entries.flatten() {
                    let path = entry.path();
                    if path.is_file() {
                        if let Ok(content) = std::fs::read_to_string(&path) {
                            if content.contains("<<<<<<<") {
                                let cleaned: String = content
                                    .lines()
                                    .filter(|l| {
                                        !l.starts_with("<<<<<<<")
                                            && !l.starts_with("=======")
                                            && !l.starts_with(">>>>>>>")
                                    })
                                    .map(|l| format!("{l}\n"))
                                    .collect();
                                let _ = std::fs::write(&path, cleaned);
                            }
                        }
                    }
                }
            }
            RunnerResult {
                status: "done".into(),
                tokens_in: 0,
                tokens_out: 0,
                cost_usd: 0.0,
                summary: "resolved".into(),
                error: None,
                verified: None,
                claude_session_id: None,
            }
        }
    }

    let captured: Arc<Mutex<Vec<RunnerSpec>>> = Arc::new(Mutex::new(Vec::new()));
    let runner = TestRunner {
        specs: captured.clone(),
        synth_summary: SYNTH_SUMMARY,
    };

    run_merge(&store, &runner, &guardian_id);

    // The merge must complete successfully.
    let view = store.lock().unwrap().get_guardian(&guardian_id).unwrap();
    assert_eq!(view.status, "in_review", "merge failed: {:?}", view.detail);

    let specs = captured.lock().unwrap();

    // 1. A synthesis call was issued for the conflicting branch (feature/y).
    let synth = specs
        .iter()
        .find(|s| s.task == "verify-synthesis")
        .expect("verify-synthesis spec not found — synthesis was never invoked");

    let synth_prompt = synth.prompt.as_deref().unwrap_or("");

    // Session-level command verify step must appear.
    assert!(
        synth_prompt.contains("[command] cargo fmt --check"),
        "synthesis prompt missing session command step:\n{synth_prompt}"
    );
    // Session-level prompt verify step must appear.
    assert!(
        synth_prompt.contains("[prompt]") && synth_prompt.contains("style guidelines"),
        "synthesis prompt missing session prompt-kind step:\n{synth_prompt}"
    );
    // Task-level command verify step must appear.
    assert!(
        synth_prompt.contains("[command] cargo test --workspace"),
        "synthesis prompt missing task-level verify step:\n{synth_prompt}"
    );

    // 2. The synthesis system prompt is present and mentions the rebase constraint.
    let synth_sys = synth.system_prompt.as_deref().unwrap_or("");
    assert!(
        synth_sys.contains("CANNOT") && synth_sys.contains("commit"),
        "synthesis system prompt should forbid commit/push:\n{synth_sys}"
    );

    // 3. The resolver prompt contains the synthesised quality bar.
    let resolve = specs
        .iter()
        .find(|s| s.task == "resolve")
        .expect("resolve spec not found — resolver was never invoked");

    let resolve_prompt = resolve.prompt.as_deref().unwrap_or("");
    assert!(
        resolve_prompt.contains("quality bar"),
        "resolver prompt missing synthesised quality bar:\n{resolve_prompt}"
    );
    assert!(
        resolve_prompt.contains(SYNTH_SUMMARY),
        "resolver prompt does not contain the synthesised text:\n{resolve_prompt}"
    );

    // 4. Synthesis events were written to the guardian log.
    let events = store
        .lock()
        .unwrap()
        .events_for_guardian(&guardian_id, 50)
        .unwrap();
    assert!(
        events
            .iter()
            .any(|e| e.message.contains("synthesizing verify instructions")),
        "expected synthesis-start event in guardian log; got: {events:?}"
    );
    assert!(
        events
            .iter()
            .any(|e| e.message.contains("synthesis complete")),
        "expected synthesis-complete event in guardian log; got: {events:?}"
    );

    let _ = std::fs::remove_dir_all(&root);
}

// RAL-52: a feedback message containing "don't commit" must leave edits as
// uncommitted working-tree changes — no git commit should be created.
#[test]
fn no_commit_feedback_skips_commit_and_leaves_dirty_worktree() {
    let (root, store, id) = single_feature_repo();
    run_merge(&store, &NoopRunner, &id);

    let view = store.lock().unwrap().get_guardian(&id).unwrap();
    let wt_str = view.branches[0].worktree.clone().expect("worktree");
    let wt = PathBuf::from(&wt_str);
    let rev = view.branches[0]
        .review_branch
        .clone()
        .expect("review branch");

    // Record the review-branch HEAD before feedback so we can check it didn't move.
    let head_before = git(&root, &["rev-parse", &rev]);

    run_feedback(
        &store,
        &NamedFeedbackRunner("note.txt"),
        &id,
        0,
        "add a note file, don't commit",
    );

    // The review branch HEAD must not have moved — no new commit was created.
    let head_after = git(&root, &["rev-parse", &rev]);
    assert_eq!(
        head_before, head_after,
        "review branch must not advance on no-commit feedback"
    );

    // The edit is visible as an uncommitted change in the review worktree.
    let status = git(&wt, &["status", "--porcelain"]);
    assert!(
        status.contains("note.txt"),
        "note.txt must appear as a dirty file; status:\n{status}"
    );

    // note.txt must NOT appear in the committed tree of the review branch.
    let committed = git(&root, &["ls-tree", "-r", "--name-only", &rev]);
    assert!(
        !committed.contains("note.txt"),
        "note.txt must not be committed; committed tree:\n{committed}"
    );

    // Guardian must be back in review after the no-commit turn.
    let view2 = store.lock().unwrap().get_guardian(&id).unwrap();
    assert_eq!(
        view2.status, "in_review",
        "guardian status after no-commit: {:?}",
        view2.detail
    );

    let _ = std::fs::remove_dir_all(&root);
}

// RAL-52: after a no-commit turn (which leaves changes in the worktree), a
// subsequent normal feedback turn must commit ONLY the new agent's changes and
// must NOT commit the leftover changes from the prior no-commit turn.
#[test]
fn subsequent_normal_feedback_commits_only_agent_changes_not_prior_no_commit_leftovers() {
    let (root, store, id) = single_feature_repo();
    run_merge(&store, &NoopRunner, &id);

    let view = store.lock().unwrap().get_guardian(&id).unwrap();
    let wt_str = view.branches[0].worktree.clone().expect("worktree");
    let wt = PathBuf::from(&wt_str);

    // Turn 1: no-commit — agent writes note1.txt, which stays uncommitted.
    run_feedback(
        &store,
        &NamedFeedbackRunner("note1.txt"),
        &id,
        0,
        "add note1, don't commit",
    );
    assert!(
        wt.join("note1.txt").exists(),
        "note1.txt must exist in worktree after no-commit turn"
    );

    // Turn 2: normal — agent writes note2.txt, which should be committed.
    run_feedback(
        &store,
        &NamedFeedbackRunner("note2.txt"),
        &id,
        0,
        "add note2",
    );

    let view2 = store.lock().unwrap().get_guardian(&id).unwrap();
    let rev = view2.branches[0]
        .review_branch
        .clone()
        .expect("review branch");

    // note2.txt from turn 2 must be in the committed tree.
    let committed = git(&root, &["ls-tree", "-r", "--name-only", &rev]);
    assert!(
        committed.contains("note2.txt"),
        "note2.txt must be committed; committed tree:\n{committed}"
    );

    // note1.txt from the no-commit turn must NOT have been committed by turn 2.
    assert!(
        !committed.contains("note1.txt"),
        "note1.txt must NOT be committed; committed tree:\n{committed}"
    );

    // note1.txt must still be present in the working tree (restored from stash).
    assert!(
        wt.join("note1.txt").exists(),
        "note1.txt must remain in worktree after turn 2 (stash-restored)"
    );

    let _ = std::fs::remove_dir_all(&root);
}

// RAL-52: the global feedback chat (dispatch_routes path) must also respect a
// no-commit instruction — routed agents apply edits but no commit is created.

/// For the triage turn (task == "chat") returns a route block targeting
/// `target_branch`; for dispatch turns (task == "route") writes a named file.
struct RouteBlockRunner {
    target_branch: String,
    dispatch_file: &'static str,
}
impl Runner for RouteBlockRunner {
    fn run(&self, spec: &RunnerSpec) -> RunnerResult {
        if spec.task == "chat" {
            return RunnerResult {
                status: "done".into(),
                tokens_in: 0,
                tokens_out: 0,
                cost_usd: 0.0,
                summary: format!(
                    "Routing to {}.\n<route branch=\"{}\">\nAdd a note\n</route>",
                    self.target_branch, self.target_branch
                ),
                error: None,
                verified: None,
                claude_session_id: None,
            };
        }
        let _ = std::fs::write(
            PathBuf::from(&spec.cwd).join(self.dispatch_file),
            "dispatched\n",
        );
        RunnerResult {
            status: "done".into(),
            tokens_in: 0,
            tokens_out: 0,
            cost_usd: 0.0,
            summary: "edited".into(),
            error: None,
            verified: None,
            claude_session_id: None,
        }
    }
}

#[test]
fn chat_no_commit_leaves_working_tree_dirty() {
    let (root, store, id) = single_feature_repo();
    // Use an unsupported resolver agent so call_direct returns Err immediately and
    // the subprocess fallback (our mock runner) is used for the triage call.
    store
        .lock()
        .unwrap()
        .set_guardian_resolver(&id, Some("noop"), None)
        .unwrap();
    run_merge(&store, &NoopRunner, &id);

    let view = store.lock().unwrap().get_guardian(&id).unwrap();
    let wt = PathBuf::from(view.branches[0].worktree.as_deref().unwrap());
    let rev = view.branches[0].review_branch.clone().unwrap();
    let head_before = git(&root, &["rev-parse", &rev]);

    let runner: Arc<dyn Runner> = Arc::new(RouteBlockRunner {
        target_branch: "feature/a".to_string(),
        dispatch_file: "dispatch_note.txt",
    });
    run_chat(&store, runner, &id, "apply the change, don't commit", None);

    // The review-branch HEAD must not have moved.
    let head_after = git(&root, &["rev-parse", &rev]);
    assert_eq!(
        head_before, head_after,
        "review branch must not advance on no-commit chat"
    );

    // dispatch_note.txt must NOT appear in the committed tree.
    let committed = git(&root, &["ls-tree", "-r", "--name-only", &rev]);
    assert!(
        !committed.contains("dispatch_note.txt"),
        "dispatch_note.txt must not be committed; ls-tree: {committed}"
    );

    // dispatch_note.txt must appear as an uncommitted change in the worktree.
    let status = git(&wt, &["status", "--porcelain"]);
    assert!(
        status.contains("dispatch_note.txt"),
        "dispatch_note.txt must be a dirty working-tree file; status: {status}"
    );

    let view2 = store.lock().unwrap().get_guardian(&id).unwrap();
    assert_eq!(
        view2.status, "in_review",
        "guardian status after no-commit chat: {:?}",
        view2.detail
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn rerun_resets_downstream_branches_to_pending_before_processing() {
    // RAL-54: when run_merge is triggered a second time, downstream branches must
    // not linger in their terminal state from the prior run. To observe the reset,
    // we make the second run fail on branch 0 (by deleting its feature branch) —
    // if the reset fires correctly, branch 1 will be in "pending" rather than
    // "done" when the run aborts early.
    let root = temp_repo();
    init_repo(&root);
    write(&root, "base.txt", "base\n");
    git(&root, &["add", "."]);
    git(&root, &["commit", "-m", "base"]);

    git(&root, &["checkout", "-b", "feature/a"]);
    write(&root, "a.txt", "from a\n");
    git(&root, &["add", "."]);
    git(&root, &["commit", "-m", "add a"]);

    git(&root, &["checkout", "main"]);
    git(&root, &["checkout", "-b", "feature/b"]);
    write(&root, "b.txt", "from b\n");
    git(&root, &["add", "."]);
    git(&root, &["commit", "-m", "add b"]);
    git(&root, &["checkout", "main"]);

    let store = Arc::new(Mutex::new(Store::open_in_memory().unwrap()));
    let id = {
        let g = store.lock().unwrap();
        let id = g
            .create_guardian("r", "main", root.to_str().unwrap())
            .unwrap();
        g.add_guardian_branch(&id, "feature/a").unwrap();
        g.add_guardian_branch(&id, "feature/b").unwrap();
        id
    };

    // First run: both branches reach Done.
    run_merge(&store, &NoopRunner, &id);
    {
        let view = store.lock().unwrap().get_guardian(&id).unwrap();
        assert_eq!(view.status, "in_review");
        assert!(view.branches.iter().all(|b| b.merge_status == "done"));
    }

    // Remove feature/a so the second run fails at position 0.
    git(&root, &["branch", "-D", "feature/a"]);

    // Second run: branch 0 fails early; branch 1 must be "pending" (reset by the
    // new RAL-54 bulk-reset) rather than "done" (stale from the first run).
    run_merge(&store, &NoopRunner, &id);
    {
        let view = store.lock().unwrap().get_guardian(&id).unwrap();
        assert_eq!(view.branches[0].merge_status, "failed", "branch 0 failed");
        assert_eq!(
            view.branches[1].merge_status, "pending",
            "branch 1 must be reset to pending, not linger as done"
        );
    }

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn rebase_succeeds_when_worktree_has_untracked_file_introduced_by_new_base() {
    // Regression: if the worktree has an untracked file that also exists in the
    // new base commit, `git rebase --onto` fails with
    // "untracked working tree files would be overwritten by checkout".
    // The engine must clean the worktree before rebasing so this never blocks a
    // rebuild.
    let (root, store, id) = single_feature_repo();
    run_merge(&store, &NoopRunner, &id);
    assert_eq!(
        store.lock().unwrap().get_guardian(&id).unwrap().status,
        "in_review"
    );

    // Advance the base branch with a NEW file (leftover.txt).
    git(&root, &["checkout", "main"]);
    write(&root, "leftover.txt", "new base file\n");
    git(&root, &["add", "."]);
    git(&root, &["commit", "-m", "main adds leftover.txt"]);

    // Simulate the scenario: an agent session created leftover.txt in the
    // worktree but never staged or committed it (an untracked leftover).
    let view = store.lock().unwrap().get_guardian(&id).unwrap();
    let wt_path = view
        .branches
        .first()
        .and_then(|b| b.worktree.as_deref())
        .map(PathBuf::from)
        .expect("worktree path");
    std::fs::write(wt_path.join("leftover.txt"), "stale agent output\n")
        .expect("write untracked file");

    // The rebuild must succeed — not fail with "untracked files would be
    // overwritten".
    let sem = Semaphore::new(4);
    let rebuilt = rebuild_on_base_shift(&store, &NoopRunner, &id, &sem);
    assert!(rebuilt, "base shift should trigger rebuild");

    let after = store.lock().unwrap().get_guardian(&id).unwrap();
    assert_eq!(
        after.status, "in_review",
        "rebuild must reach in_review; detail: {:?}",
        after.detail
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn stage_done_signal_triggers_fast_path_rebase_continue() {
    // When the conflict-resolver agent emits RALPHUS_STAGE: DONE after calling
    // git add -A, the orchestrator must advance the rebase immediately via the
    // fast path instead of waiting for a marker re-scan.
    let root = temp_repo();
    init_repo(&root);
    write(&root, "conflict.txt", "line1\nBASE\nline3\n");
    git(&root, &["add", "."]);
    git(&root, &["commit", "-m", "base"]);

    git(&root, &["checkout", "-b", "feature/x"]);
    write(&root, "conflict.txt", "line1\nX\nline3\n");
    git(&root, &["commit", "-am", "x"]);

    git(&root, &["checkout", "main"]);
    git(&root, &["checkout", "-b", "feature/y"]);
    write(&root, "conflict.txt", "line1\nY\nline3\n");
    git(&root, &["commit", "-am", "y"]);
    git(&root, &["checkout", "main"]);

    let store = Arc::new(Mutex::new(Store::open_in_memory().unwrap()));
    let id = {
        let g = store.lock().unwrap();
        let id = g
            .create_guardian("stage-done", "main", root.to_str().unwrap())
            .unwrap();
        g.add_guardian_branch(&id, "feature/x").unwrap();
        g.add_guardian_branch(&id, "feature/y").unwrap();
        id
    };

    run_merge(&store, &StageDoneRunner, &id);

    let view = store.lock().unwrap().get_guardian(&id).unwrap();
    assert_eq!(view.status, "in_review", "detail: {:?}", view.detail);

    let y = view
        .branches
        .iter()
        .find(|b| b.branch == "feature/y")
        .unwrap();
    assert_eq!(y.merge_status, "conflict_resolved");

    // The review branch file must be marker-free.
    let review = view.review_branch.expect("review branch");
    let show = git(&root, &["show", &format!("{review}:conflict.txt")]);
    assert!(
        !show.contains("<<<<<<<"),
        "markers remain in review branch: {show}"
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn stage_done_marker_present_in_resolver_system_prompt() {
    // The system prompt sent to the conflict-resolver agent must contain the
    // RALPHUS_STAGE: DONE protocol instruction so the agent knows to emit it
    // after git add -A.
    let root = temp_repo();
    init_repo(&root);
    write(&root, "conflict.txt", "line1\nBASE\nline3\n");
    git(&root, &["add", "."]);
    git(&root, &["commit", "-m", "base"]);

    git(&root, &["checkout", "-b", "feature/x"]);
    write(&root, "conflict.txt", "line1\nX\nline3\n");
    git(&root, &["commit", "-am", "x"]);

    git(&root, &["checkout", "main"]);
    git(&root, &["checkout", "-b", "feature/y"]);
    write(&root, "conflict.txt", "line1\nY\nline3\n");
    git(&root, &["commit", "-am", "y"]);
    git(&root, &["checkout", "main"]);

    struct CapturingRunner {
        specs: Arc<Mutex<Vec<RunnerSpec>>>,
    }
    impl Runner for CapturingRunner {
        fn run(&self, spec: &RunnerSpec) -> RunnerResult {
            self.specs.lock().unwrap().push(spec.clone());
            // Strip conflict markers so the rebase completes via the fallback path.
            let cwd = PathBuf::from(&spec.cwd);
            if let Ok(entries) = std::fs::read_dir(&cwd) {
                for entry in entries.flatten() {
                    let path = entry.path();
                    if path.is_file() {
                        if let Ok(content) = std::fs::read_to_string(&path) {
                            if content.contains("<<<<<<<") {
                                let cleaned: String = content
                                    .lines()
                                    .filter(|l| {
                                        !l.starts_with("<<<<<<<")
                                            && !l.starts_with("=======")
                                            && !l.starts_with(">>>>>>>")
                                    })
                                    .map(|l| format!("{l}\n"))
                                    .collect();
                                let _ = std::fs::write(&path, cleaned);
                            }
                        }
                    }
                }
            }
            RunnerResult {
                status: "done".into(),
                tokens_in: 0,
                tokens_out: 0,
                cost_usd: 0.0,
                summary: "resolved".into(),
                error: None,
                verified: None,
                claude_session_id: None,
            }
        }
    }

    let captured: Arc<Mutex<Vec<RunnerSpec>>> = Arc::new(Mutex::new(Vec::new()));
    let runner = CapturingRunner {
        specs: captured.clone(),
    };

    let store = Arc::new(Mutex::new(Store::open_in_memory().unwrap()));
    let id = {
        let g = store.lock().unwrap();
        let id = g
            .create_guardian("sys-prompt-check", "main", root.to_str().unwrap())
            .unwrap();
        g.add_guardian_branch(&id, "feature/x").unwrap();
        g.add_guardian_branch(&id, "feature/y").unwrap();
        id
    };

    run_merge(&store, &runner, &id);

    let specs = captured.lock().unwrap();
    let resolve = specs
        .iter()
        .find(|s| s.task == "resolve")
        .expect("resolve spec not found — conflict resolver was never invoked");
    let sys = resolve.system_prompt.as_deref().unwrap_or("");
    assert!(
        sys.contains("RALPHUS_STAGE: DONE"),
        "resolver system prompt must contain the RALPHUS_STAGE: DONE instruction:\n{sys}"
    );
    assert!(
        sys.contains("git add -A"),
        "resolver system prompt must instruct the agent to run git add -A:\n{sys}"
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn rerere_autoupdate_resolves_conflict_without_agent() {
    // Regression test for the rerere.autoupdate=true interaction. When that
    // config is set, git automatically stages previously-resolved conflicts,
    // leaving `git diff --name-only --diff-filter=U` empty even though the
    // rebase is still paused. drive_rebase must detect this (no unmerged files
    // but rebase_in_progress) and continue rather than aborting.
    let root = temp_repo();
    init_repo(&root);
    git(&root, &["config", "rerere.enabled", "true"]);
    git(&root, &["config", "rerere.autoupdate", "true"]);

    write(&root, "conflict.txt", "line1\nBASE\nline3\n");
    git(&root, &["add", "."]);
    git(&root, &["commit", "-m", "base"]);

    // feature/x: BASE → X
    git(&root, &["checkout", "-b", "feature/x"]);
    write(&root, "conflict.txt", "line1\nX\nline3\n");
    git(&root, &["commit", "-am", "x"]);
    git(&root, &["checkout", "main"]);

    // feature/y: BASE → Y (will conflict with feature/x when stacked)
    git(&root, &["checkout", "-b", "feature/y"]);
    write(&root, "conflict.txt", "line1\nY\nline3\n");
    git(&root, &["commit", "-am", "y"]);
    git(&root, &["checkout", "main"]);

    // Train rerere: cherry-pick feature/y onto feature/x to produce the same
    // conflict the guardian rebase will see, then resolve it so rerere records
    // the postimage. The cherry-pick is expected to fail (conflict).
    git(&root, &["checkout", "-b", "rerere-train", "feature/x"]);
    let _ = Command::new("git")
        .args(["cherry-pick", "feature/y"])
        .current_dir(&root)
        .env("GIT_AUTHOR_NAME", "ralphus")
        .env("GIT_AUTHOR_EMAIL", "ralphus@example.com")
        .env("GIT_COMMITTER_NAME", "ralphus")
        .env("GIT_COMMITTER_EMAIL", "ralphus@example.com")
        .env("GIT_EDITOR", "true")
        .output()
        .expect("git cherry-pick");
    // Resolve the conflict — rerere will replay this resolution later.
    write(&root, "conflict.txt", "line1\nX and Y\nline3\n");
    git(&root, &["add", "conflict.txt"]); // rerere records postimage
    let _ = Command::new("git")
        .args(["cherry-pick", "--continue", "--no-edit"])
        .current_dir(&root)
        .env("GIT_AUTHOR_NAME", "ralphus")
        .env("GIT_AUTHOR_EMAIL", "ralphus@example.com")
        .env("GIT_COMMITTER_NAME", "ralphus")
        .env("GIT_COMMITTER_EMAIL", "ralphus@example.com")
        .env("GIT_EDITOR", "true")
        .output()
        .expect("git cherry-pick --continue");
    git(&root, &["checkout", "main"]);
    let _ = Command::new("git")
        .args(["branch", "-D", "rerere-train"])
        .current_dir(&root)
        .output();

    // Run the guardian merge. NoopRunner must not be invoked for conflict
    // resolution — rerere handles it transparently via rerere.autoupdate.
    // If the agent were called and failed, the guardian would end up
    // merge_failed and the status assertion below would catch it.
    let store = Arc::new(Mutex::new(Store::open_in_memory().unwrap()));
    let id = {
        let g = store.lock().unwrap();
        let gid = g
            .create_guardian("rerere", "main", root.to_str().unwrap())
            .unwrap();
        g.add_guardian_branch(&gid, "feature/x").unwrap();
        g.add_guardian_branch(&gid, "feature/y").unwrap();
        gid
    };
    run_merge(&store, &NoopRunner, &id);

    let view = store.lock().unwrap().get_guardian(&id).unwrap();
    assert_eq!(view.status, "in_review", "detail: {:?}", view.detail);

    let y = view
        .branches
        .iter()
        .find(|b| b.branch == "feature/y")
        .unwrap();
    assert_eq!(
        y.merge_status, "conflict_resolved",
        "feature/y should be conflict_resolved (rerere handled it)"
    );

    let review = view.review_branch.expect("review branch");
    let show = git(&root, &["show", &format!("{review}:conflict.txt")]);
    assert!(!show.contains("<<<<<<<"), "conflict markers remain: {show}");
    assert!(
        show.contains("X and Y"),
        "rerere resolution not applied: {show}"
    );

    let _ = std::fs::remove_dir_all(&root);
}
