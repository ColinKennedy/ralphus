//! Real-git integration tests for the Guardian merge engine: a clean two-branch
//! stack, and a conflicting branch resolved by a (fake) agent that strips
//! conflict markers.

mod common;

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::{git, init_repo};
use ralphus_core::schema::TaskFile;
use ralphus_daemon::cancel::{CancelToken, Cancellations};
use ralphus_daemon::cartographer::CartographerFilter;
use ralphus_daemon::guardian::{GuardianCheck, GuardianStatus, MergeStatus};
use ralphus_daemon::guardian_merge::{
    pull_pr_commits, purge_worktrees, rebase_command_progress, rebase_on_manual_push,
    rebuild_on_base_shift, reopen_cancelled_guardian_merge, reopen_straggler,
    restart_guardian_merge, run_feedback, run_merge, run_merge_staged, start_feedback, start_merge,
    stop_guardian_merge, stop_merge_worker_for_cancel,
};
use ralphus_daemon::reviews::derive_reviews;
use ralphus_daemon::runner::{Runner, RunnerResult, RunnerSpec};
use ralphus_daemon::scheduler::Semaphore;
use ralphus_daemon::server::{Daemon, route};
use ralphus_daemon::store::{NodeState, Store};
use ralphus_daemon::workspace::Workspace;

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
            cache_creation_tokens: 0,
            cache_read_tokens: 0,
            cost_usd: 0.0,
            cost_is_estimated: false,
            summary: "resolved\nRALPHUS_STAGE: DONE".into(),
            error: None,
            proofed: None,
            agent_session_id: None,
            ghost: None,
        }
    }
}

/// RAL-330: a fake conflict resolver that resolves the real conflict
/// correctly but also silently drops `new_in_y.txt` -- a wholly unrelated
/// file the branch's own commit added, which the base never touched. Stands
/// in for whatever mechanism (a stale rerere replay, or a resolver's own
/// pass) can produce this class of failure in production, without needing to
/// reproduce the exact git-level trigger: the content-preservation guard
/// must catch this regardless of cause.
struct LossyRunner;
impl Runner for LossyRunner {
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
        let _ = std::fs::remove_file(cwd.join("new_in_y.txt"));
        let ok = Command::new("git")
            .args(["add", "-A"])
            .current_dir(&cwd)
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        assert!(ok, "LossyRunner: git add -A failed in {}", cwd.display());
        RunnerResult {
            status: "done".into(),
            tokens_in: 0,
            tokens_out: 0,
            cache_creation_tokens: 0,
            cache_read_tokens: 0,
            cost_usd: 0.0,
            cost_is_estimated: false,
            summary: "resolved\nRALPHUS_STAGE: DONE".into(),
            error: None,
            proofed: None,
            agent_session_id: None,
            ghost: None,
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
            cache_creation_tokens: 0,
            cache_read_tokens: 0,
            cost_usd: 0.0,
            cost_is_estimated: false,
            summary: "resolved".into(),
            error: None,
            proofed: None,
            agent_session_id: None,
            ghost: None,
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
            cache_creation_tokens: 0,
            cache_read_tokens: 0,
            cost_usd: 0.0,
            cost_is_estimated: false,
            summary: "edited".into(),
            error: None,
            proofed: None,
            agent_session_id: None,
            ghost: None,
        }
    }
}

/// Same as `FeedbackRunner`, but also simulates a concurrent failure landing
/// on the guardian mid-flight -- e.g. a racing "Merge / rebase" click hitting
/// a transient worktree error -- by stamping `MergeFailed` on the guardian
/// directly from inside the resolver call, before `run_feedback` goes on to
/// restack the downstream branches.
struct RaceInjectingRunner {
    store: Arc<Mutex<Store>>,
    id: String,
}
impl Runner for RaceInjectingRunner {
    fn run(&self, spec: &RunnerSpec) -> RunnerResult {
        let _ = std::fs::write(PathBuf::from(&spec.cwd).join("note.txt"), "reviewed\n");
        let _ = self.store.lock().unwrap().set_guardian_status(
            &self.id,
            GuardianStatus::MergeFailed,
            Some("simulated concurrent failure"),
        );
        RunnerResult {
            status: "done".into(),
            tokens_in: 0,
            tokens_out: 0,
            cache_creation_tokens: 0,
            cache_read_tokens: 0,
            cost_usd: 0.0,
            cost_is_estimated: false,
            summary: "edited".into(),
            error: None,
            proofed: None,
            agent_session_id: None,
            ghost: None,
        }
    }
}

/// RAL-241 follow-up: a fake reviewer agent that runs successfully but makes
/// no edits at all -- exercises `run_feedback`'s silent-no-op path (the
/// worktree stays clean, so nothing gets committed even though `no_commit`
/// wasn't requested).
struct SilentNoOpFeedbackRunner;
impl Runner for SilentNoOpFeedbackRunner {
    fn run(&self, _spec: &RunnerSpec) -> RunnerResult {
        RunnerResult {
            status: "done".into(),
            tokens_in: 0,
            tokens_out: 0,
            cache_creation_tokens: 0,
            cache_read_tokens: 0,
            cost_usd: 0.0,
            cost_is_estimated: false,
            summary: "nothing to change".into(),
            error: None,
            proofed: None,
            agent_session_id: None,
            ghost: None,
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
            cache_creation_tokens: 0,
            cache_read_tokens: 0,
            cost_usd: 0.0,
            cost_is_estimated: false,
            summary: "edited".into(),
            error: None,
            proofed: None,
            agent_session_id: None,
            ghost: None,
        }
    }
}

fn setup_review_with_pending_last_branch(store: &mut Store) -> (PathBuf, String) {
    let root = temp_repo();
    init_repo(&root);
    write(&root, "base.txt", "base\n");
    git(&root, &["add", "."]);
    git(&root, &["commit", "-m", "base"]);

    let wt_a = root.join("wt-a");
    let wt_b = root.join("wt-b");
    git(
        &root,
        &["worktree", "add", "-b", "feature/a", wt_a.to_str().unwrap()],
    );
    git(
        &root,
        &["worktree", "add", "-b", "feature/b", wt_b.to_str().unwrap()],
    );
    git(&wt_a, &["branch", "--set-upstream-to=main"]);
    git(&wt_b, &["branch", "--set-upstream-to=main"]);
    write(&wt_a, "a.txt", "from a\n");
    git(&wt_a, &["add", "."]);
    git(&wt_a, &["commit", "-m", "add a"]);
    write(&wt_b, "b.txt", "from b\n");
    git(&wt_b, &["add", "."]);
    git(&wt_b, &["commit", "-m", "add b"]);

    let cwd_a = wt_a.to_string_lossy().replace('\\', "/");
    let cwd_b = wt_b.to_string_lossy().replace('\\', "/");
    let toml = format!(
        "[[task]]\nname=\"a\"\n\
         [[task.cell]]\ncwd=\"{cwd_a}\"\ncommand=\"noop\"\nreview=\"<<review:rev>>\"\n\
         [[task]]\nname=\"b\"\ndepends_on=[\"a\"]\n\
         [[task.cell]]\ncwd=\"{cwd_b}\"\ncommand=\"noop\"\nreview=\"<<review:rev>>\"\n\
         [[review]]\nid=\"rev\"\n"
    );
    let file: TaskFile = toml::from_str(&toml).unwrap();
    let run_id = store.insert_squad(&file, None, false).unwrap();
    let gid = derive_reviews(store, &run_id, &file).unwrap()[0].clone();
    store
        .set_cell_state(&run_id, 0, 0, NodeState::Done)
        .unwrap();
    store.mark_ready_branches_with_done_cells(&gid).unwrap();

    let guardian = store.get_guardian(&gid).unwrap();
    assert_eq!(guardian.status, "collecting");
    assert_eq!(guardian.branches[0].merge_status, "ready");
    assert_eq!(guardian.branches[1].merge_status, "pending");
    (root, gid)
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
fn start_merge_defers_while_an_enabled_branch_is_still_pending() {
    let store = Arc::new(Mutex::new(Store::open_in_memory().unwrap()));
    let (root, gid) = {
        let mut guard = store.lock().unwrap();
        setup_review_with_pending_last_branch(&mut guard)
    };

    let reply = start_merge(
        Arc::clone(&store),
        Arc::new(NoopRunner),
        &gid,
        Arc::new(Semaphore::new(4)),
        Cancellations::new(),
    );
    assert_eq!(reply.status, 202, "body={}", reply.body);
    assert!(
        reply.body.contains("\"status\":\"deferred\""),
        "body={}",
        reply.body
    );

    let guardian = store.lock().unwrap().get_guardian(&gid).unwrap();
    assert_eq!(guardian.status, "collecting");
    assert_eq!(guardian.base_branch, "main");
    assert_eq!(guardian.branches[0].merge_status, "ready");
    assert_eq!(guardian.branches[1].merge_status, "pending");

    let _ = std::fs::remove_dir_all(&root);
}

/// RAL-265: reopening a cancelled review must stage the already-ready prefix
/// immediately rather than waiting for every branch to be ready -- otherwise
/// a review reopened while its last branch's cell is still running would sit
/// idle until that cell finishes, even though earlier branches were done and
/// would already have been rebased in had the review never been cancelled.
#[test]
fn reopen_cancelled_guardian_merge_stages_the_ready_prefix_while_a_branch_is_pending() {
    let (root, store, gid, bids) = staged_feature_repo(&["feature/a", "feature/b"]);
    mark_ready(&store, &gid, &bids[0]);

    store.lock().unwrap().cancel_guardian(&gid).unwrap();
    assert_eq!(
        store.lock().unwrap().get_guardian(&gid).unwrap().status,
        "cancelled"
    );

    let reply = reopen_cancelled_guardian_merge(
        Arc::clone(&store),
        Arc::new(NoopRunner),
        &gid,
        Arc::new(Semaphore::new(4)),
        Cancellations::new(),
    );
    assert_eq!(reply.status, 202, "body={}", reply.body);
    assert!(
        reply.body.contains("\"status\":\"merging\""),
        "body={}",
        reply.body
    );

    // The staged pass runs on a spawned thread; poll until it lands somewhere
    // other than the transient `merging` state `claim_guardian_merge` set.
    let mut guardian = store.lock().unwrap().get_guardian(&gid).unwrap();
    for _ in 0..600 {
        if guardian.status != "merging" {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
        guardian = store.lock().unwrap().get_guardian(&gid).unwrap();
    }
    assert_eq!(
        guardian.status, "collecting",
        "must not finalize early: {:?}",
        guardian.detail
    );
    assert_eq!(
        guardian.branches[0].merge_status, "done",
        "the already-ready branch must be staged in immediately"
    );
    assert_eq!(guardian.branches[1].merge_status, "pending");

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn reopen_cancelled_guardian_merge_rejects_a_guardian_that_is_not_cancelled() {
    let store = Arc::new(Mutex::new(Store::open_in_memory().unwrap()));
    let (root, gid) = {
        let mut guard = store.lock().unwrap();
        setup_review_with_pending_last_branch(&mut guard)
    };

    // Still `collecting`, never cancelled: reopen must be rejected and the
    // guardian state left untouched.
    let reply = reopen_cancelled_guardian_merge(
        Arc::clone(&store),
        Arc::new(NoopRunner),
        &gid,
        Arc::new(Semaphore::new(4)),
        Cancellations::new(),
    );
    assert_eq!(reply.status, 500, "body={}", reply.body);

    let guardian = store.lock().unwrap().get_guardian(&gid).unwrap();
    assert_eq!(guardian.status, "collecting");

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn change_base_route_records_base_but_defers_merge_while_last_branch_is_pending() {
    let daemon = Daemon::new(Store::open_in_memory().unwrap(), 4);
    let store = daemon.store_handle();
    let (root, gid) = {
        let mut guard = store.lock().unwrap();
        setup_review_with_pending_last_branch(&mut guard)
    };

    let reply = route(
        &daemon,
        "POST",
        &format!("/api/guardians/{gid}/base"),
        &serde_json::json!({ "branch": "release/2026" }).to_string(),
    );
    assert_eq!(reply.status, 200, "body={}", reply.body);
    let body: serde_json::Value = serde_json::from_str(&reply.body).unwrap();
    assert_eq!(body["guardian"]["base_branch"], "release/2026");
    assert_eq!(body["base_change"]["status"], "deferred");
    assert_eq!(
        body["base_change"]["message"],
        "We will use 'release/2026' once the branches are ready to merge."
    );

    let guardian = store.lock().unwrap().get_guardian(&gid).unwrap();
    assert_eq!(guardian.status, "collecting");
    assert_eq!(guardian.base_branch, "release/2026");
    assert_eq!(guardian.branches[0].merge_status, "ready");
    assert_eq!(guardian.branches[1].merge_status, "pending");

    let _ = std::fs::remove_dir_all(&root);
}

/// RAL-145: `rebase_command_progress` reads git's own interactive-rebase
/// todo-list bookkeeping (`rebase-merge/done` + `rebase-merge/git-rebase-todo`)
/// straight off disk -- no rebase actually needs to run; a synthetic pair of
/// files exercises the counting logic (non-blank, non-comment lines) directly.
#[test]
fn rebase_command_progress_reads_synthetic_done_and_todo_files() {
    let root = temp_repo();
    init_repo(&root);
    write(&root, "base.txt", "base\n");
    git(&root, &["add", "."]);
    git(&root, &["commit", "-m", "base"]);

    // No rebase in progress yet: `None`, not a spurious `0`/`0`.
    assert_eq!(rebase_command_progress(&Workspace::local(&root)), None);

    // Two commands already done; three queued in the todo file, padded with a
    // blank line and a comment line that must not be counted as commands.
    let state_dir = root.join(".git").join("rebase-merge");
    std::fs::create_dir_all(&state_dir).expect("mkdir rebase-merge");
    std::fs::write(
        state_dir.join("done"),
        "pick aaaaaaa first commit\npick bbbbbbb second commit\n",
    )
    .expect("write done");
    std::fs::write(
        state_dir.join("git-rebase-todo"),
        "pick ccccccc third commit\n\n# comment line, should not count\n\
         pick ddddddd fourth commit\npick eeeeeee fifth commit\n",
    )
    .expect("write git-rebase-todo");

    assert_eq!(
        rebase_command_progress(&Workspace::local(&root)),
        Some((2, 5))
    );

    let _ = std::fs::remove_dir_all(&root);
}

/// RAL-91: a branch that produced several commits collapses to a single commit
/// in the review worktree when squash is enabled for its project, while a
/// non-squashed control review keeps every working commit.
#[test]
fn squash_collapses_multi_commit_branch_to_single_commit() {
    let root = temp_repo();
    init_repo(&root);
    write(&root, "base.txt", "base\n");
    git(&root, &["add", "."]);
    git(&root, &["commit", "-m", "base"]);

    // feature/a carries THREE separate commits.
    git(&root, &["checkout", "-b", "feature/a"]);
    for (f, c) in [
        ("a1.txt", "one\n"),
        ("a2.txt", "two\n"),
        ("a3.txt", "three\n"),
    ] {
        write(&root, f, c);
        git(&root, &["add", "."]);
        git(&root, &["commit", "-m", &format!("add {f}")]);
    }
    git(&root, &["checkout", "main"]);

    let root_str = root.to_str().unwrap().to_string();
    let store = Arc::new(Mutex::new(Store::open_in_memory().unwrap()));
    let id = {
        let g = store.lock().unwrap();
        let id = g.create_guardian("r", "main", &root_str).unwrap();
        g.add_guardian_branch(&id, "feature/a").unwrap();
        // Enable squash for this (single) project.
        g.set_guardian_project_squash(&id, &root_str, true).unwrap();
        id
    };
    run_merge(&store, &NoopRunner, &id);

    let view = store.lock().unwrap().get_guardian(&id).unwrap();
    assert_eq!(view.status, "in_review", "detail: {:?}", view.detail);
    assert!(
        view.squash_projects.contains(&root_str),
        "squash_projects should record the project: {:?}",
        view.squash_projects
    );
    let review = view.review_branch.expect("review branch set");

    // The three working commits collapse to exactly one over the base.
    let count = git(&root, &["rev-list", "--count", &format!("main..{review}")]);
    assert_eq!(count.trim(), "1", "expected a single squashed commit");
    // …and the squashed commit still carries every file.
    let files = git(&root, &["ls-tree", "-r", "--name-only", &review]);
    assert!(
        files.contains("a1.txt") && files.contains("a2.txt") && files.contains("a3.txt"),
        "squashed review is missing files: {files}"
    );
    // The feature branch itself is untouched — still three commits over main.
    let feat = git(&root, &["rev-list", "--count", "main..feature/a"]);
    assert_eq!(feat.trim(), "3", "feature branch must not be rewritten");

    let _ = std::fs::remove_dir_all(&root);
}

/// RAL-91: a review spanning two git projects honours each project's squash
/// setting independently — squash on for project A collapses its branch, while
/// project B (squash off) keeps every commit.
#[test]
fn squash_setting_is_per_project_independent() {
    let make_repo = |feature: &str, commits: &[(&str, &str)]| -> PathBuf {
        let root = temp_repo();
        init_repo(&root);
        write(&root, "base.txt", "base\n");
        git(&root, &["add", "."]);
        git(&root, &["commit", "-m", "base"]);
        git(&root, &["checkout", "-b", feature]);
        for (f, c) in commits {
            write(&root, f, c);
            git(&root, &["add", "."]);
            git(&root, &["commit", "-m", &format!("add {f}")]);
        }
        git(&root, &["checkout", "main"]);
        root
    };
    // Project A: two commits, squash ON. Project B: two commits, squash OFF.
    let root_a = make_repo("feature/a", &[("a1.txt", "1\n"), ("a2.txt", "2\n")]);
    let root_b = make_repo("feature/b", &[("b1.txt", "1\n"), ("b2.txt", "2\n")]);
    let a_str = root_a.to_str().unwrap().to_string();
    let b_str = root_b.to_str().unwrap().to_string();

    let store = Arc::new(Mutex::new(Store::open_in_memory().unwrap()));
    let id = {
        let g = store.lock().unwrap();
        let id = g.create_guardian("multi", "main", &a_str).unwrap();
        g.add_guardian_branch_with_project(&id, "feature/a", Some(&a_str))
            .unwrap();
        g.add_guardian_branch_with_project(&id, "feature/b", Some(&b_str))
            .unwrap();
        // Squash only project A.
        g.set_guardian_project_squash(&id, &a_str, true).unwrap();
        id
    };
    run_merge(&store, &NoopRunner, &id);

    let view = store.lock().unwrap().get_guardian(&id).unwrap();
    assert_eq!(view.status, "in_review", "detail: {:?}", view.detail);

    let rb = |branch: &str| -> String {
        view.branches
            .iter()
            .find(|b| b.branch == branch)
            .and_then(|b| b.review_branch.clone())
            .unwrap_or_else(|| panic!("no review branch for {branch}"))
    };
    // Project A's branch is squashed to one commit; project B's keeps both.
    let count_a = git(
        &root_a,
        &["rev-list", "--count", &format!("main..{}", rb("feature/a"))],
    );
    assert_eq!(count_a.trim(), "1", "project A should be squashed");
    let count_b = git(
        &root_b,
        &["rev-list", "--count", &format!("main..{}", rb("feature/b"))],
    );
    assert_eq!(count_b.trim(), "2", "project B should NOT be squashed");

    let _ = std::fs::remove_dir_all(&root_a);
    let _ = std::fs::remove_dir_all(&root_b);
}

/// Control: with squash disabled (the default) the same three-commit branch
/// keeps all three commits in the review worktree.
#[test]
fn without_squash_multi_commit_branch_is_preserved() {
    let root = temp_repo();
    init_repo(&root);
    write(&root, "base.txt", "base\n");
    git(&root, &["add", "."]);
    git(&root, &["commit", "-m", "base"]);

    git(&root, &["checkout", "-b", "feature/a"]);
    for (f, c) in [
        ("a1.txt", "one\n"),
        ("a2.txt", "two\n"),
        ("a3.txt", "three\n"),
    ] {
        write(&root, f, c);
        git(&root, &["add", "."]);
        git(&root, &["commit", "-m", &format!("add {f}")]);
    }
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
    run_merge(&store, &NoopRunner, &id);

    let view = store.lock().unwrap().get_guardian(&id).unwrap();
    assert_eq!(view.status, "in_review", "detail: {:?}", view.detail);
    let review = view.review_branch.expect("review branch set");
    let count = git(&root, &["rev-list", "--count", &format!("main..{review}")]);
    assert_eq!(
        count.trim(),
        "3",
        "non-squashed review must keep all commits"
    );

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
    // `run_feedback` now pushes the review branch to the repo's default
    // remote (`remote.pushDefault`, falling back to `origin`) after each
    // feedback commit (see `push_feedback_branch`). Give the temp repo a
    // bare `origin` so that push succeeds; without one the feedback is
    // still committed but the detail becomes "feedback committed but push
    // failed" instead of an applied-success marker.
    let remote_dir = temp_repo();
    git(&remote_dir, &["init", "--bare"]);
    git(
        &root,
        &["remote", "add", "origin", remote_dir.to_str().unwrap()],
    );
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
    let bid0 = store.lock().unwrap().get_guardian(&id).unwrap().branches[0]
        .id
        .clone();
    run_feedback(
        &store,
        &FeedbackRunner,
        &id,
        &bid0,
        "add a note file",
        &CancelToken::never(),
    );

    let view = store.lock().unwrap().get_guardian(&id).unwrap();
    assert_eq!(view.status, "in_review", "detail: {:?}", view.detail);

    // The feedback lands on branch 0's review branch...
    let rev0 = view.branches[0].review_branch.clone().unwrap();
    let files0 = git(&root, &["ls-tree", "-r", "--name-only", &rev0]);
    assert!(files0.contains("note.txt"), "feedback commit on branch 0");
    let detail0 = view.branches[0].detail.as_deref().unwrap_or("");
    assert!(
        detail0.starts_with("feedback applied"),
        "feedback success detail, got: {detail0:?}"
    );
    assert!(
        !detail0.contains("push failed"),
        "feedback must not report a push failure, got: {detail0:?}"
    );

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
    let _ = std::fs::remove_dir_all(&remote_dir);
}

#[test]
fn restack_after_feedback_recovers_guardian_status_from_a_racing_merge_failed() {
    // A concurrent failure (e.g. a racing "Merge / rebase" click hitting a
    // transient worktree error) can stamp the guardian `MergeFailed` while
    // `run_feedback`'s downstream restack (`restack_from_position`) is still
    // actively landing later branches -- the resolver-agent call it dispatches
    // before restacking can take long enough for another caller to race in.
    // Before this fix, nothing re-affirmed `Merging` once the restack
    // resumed, so the top-level status stayed stuck on the stale failure for
    // the rest of the restack even though the downstream branch kept
    // advancing underneath it. Simulate the race deterministically: the fake
    // resolver stamps `MergeFailed` on the guardian itself, from inside the
    // same call `run_feedback` is synchronously waiting on.
    let root = temp_repo();
    init_repo(&root);
    let remote_dir = temp_repo();
    git(&remote_dir, &["init", "--bare"]);
    git(
        &root,
        &["remote", "add", "origin", remote_dir.to_str().unwrap()],
    );
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

    let bid0 = store.lock().unwrap().get_guardian(&id).unwrap().branches[0]
        .id
        .clone();
    let runner = RaceInjectingRunner {
        store: Arc::clone(&store),
        id: id.clone(),
    };
    run_feedback(
        &store,
        &runner,
        &id,
        &bid0,
        "add a note file",
        &CancelToken::never(),
    );

    // The restack must have noticed the injected `merge_failed` stamp and
    // corrected it back to `merging` before advancing the downstream branch,
    // not left it stuck until `finalize_review` overwrites it at the very end.
    let events = store.lock().unwrap().events_for_guardian(&id, 100).unwrap();
    let failed_idx = events
        .iter()
        .position(|e| e.scope == "guardian" && e.message.contains("merge_failed"))
        .expect("the injected failure must be logged");
    let recovered = events[failed_idx + 1..]
        .iter()
        .any(|e| e.scope == "guardian" && e.message == "review → merging");
    assert!(
        recovered,
        "guardian status must be re-affirmed as merging after a racing merge_failed \
         stamp, got: {events:#?}"
    );

    // ...and the review still finishes normally afterwards.
    let view = store.lock().unwrap().get_guardian(&id).unwrap();
    assert_eq!(view.status, "in_review", "detail: {:?}", view.detail);

    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_dir_all(&remote_dir);
}

#[test]
fn start_feedback_persists_reviewer_message_scoped_to_its_branch() {
    // RAL-272: giving feedback on one branch must record it in that branch's
    // own read-only thread, synchronously (before the spawned background
    // apply/reply work even starts), and it must not show up under any
    // other branch's thread.
    let (root, store, id) = single_feature_repo();
    // Deterministic, network-free: "claude-code" is resolvable but not
    // supported by chat_client::call_direct, so the best-effort reply
    // generation always no-ops instead of racing a real API call.
    store
        .lock()
        .unwrap()
        .set_guardian_resolver(&id, Some("claude-code"), None)
        .unwrap();
    run_merge(&store, &NoopRunner, &id);
    let bid0 = store.lock().unwrap().get_guardian(&id).unwrap().branches[0]
        .id
        .clone();

    let reply = start_feedback(
        store.clone(),
        Arc::new(FeedbackRunner),
        &id,
        &bid0,
        "please add a note file".to_string(),
    );
    assert_eq!(reply.status, 202);

    let msgs = store
        .lock()
        .unwrap()
        .guardian_branch_messages(&id, &bid0)
        .unwrap();
    assert_eq!(msgs.len(), 1);
    assert_eq!(msgs[0].role, "reviewer");
    assert_eq!(msgs[0].text, "please add a note file");
    assert!(
        store
            .lock()
            .unwrap()
            .guardian_branch_messages(&id, "some-other-branch")
            .unwrap()
            .is_empty()
    );

    // Let the background apply + best-effort reply generation finish before
    // the repo is removed out from under it.
    std::thread::sleep(Duration::from_millis(500));
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn feedback_silent_no_op_gets_a_distinct_detail_not_conflated_with_applied() {
    // RAL-241 follow-up regression: an agent run that completes successfully
    // but edits nothing (and `no_commit` was never requested in the
    // feedback text) previously landed no commit yet still reported the
    // branch detail as "feedback applied" -- identical to a real fix. A
    // reviewer polling `review status` had no way to tell the two apart
    // without manually `git log`-ing the worktree.
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
    run_merge(&store, &NoopRunner, &id);

    let bid0 = store.lock().unwrap().get_guardian(&id).unwrap().branches[0]
        .id
        .clone();
    let review_tip_before = {
        let rb = store.lock().unwrap().get_guardian(&id).unwrap().branches[0]
            .review_branch
            .clone()
            .unwrap();
        git(&root, &["rev-parse", &rb])
    };

    run_feedback(
        &store,
        &SilentNoOpFeedbackRunner,
        &id,
        &bid0,
        "tighten up the error messages",
        &CancelToken::never(),
    );

    let view = store.lock().unwrap().get_guardian(&id).unwrap();
    let detail = view.branches[0].detail.clone();
    assert_ne!(
        detail.as_deref(),
        Some("feedback applied"),
        "a no-op run must not be reported the same as a real fix"
    );
    assert!(
        detail.as_deref().is_some_and(|d| d.contains("no changes")),
        "detail: {detail:?}"
    );

    // Nothing was actually committed onto the review branch.
    let rb = view.branches[0].review_branch.clone().unwrap();
    let review_tip_after = git(&root, &["rev-parse", &rb]);
    assert_eq!(
        review_tip_before, review_tip_after,
        "no-op feedback run must not create a commit"
    );

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

// RAL-239: cancelling a review must not leave its check-gate subprocess
// running against the worktree — before this, `run_commit_checks` blocked on
// the command's own `Output` with no cancel polling, so a long-running check
// (`cargo test`, `npm run build`) kept executing to completion regardless of
// how quickly the DB flipped to `cancelled`.
#[test]
fn cancelling_a_review_kills_an_in_flight_check_gate_command() {
    let (root, store, id) = single_feature_repo();

    let marker_dir = temp_repo();
    let started = marker_dir.join("started.txt");
    let started_str = started.to_string_lossy().to_string();

    // A check gate that announces it started, then sleeps far longer than
    // this test's cancellation budget below -- if cancel doesn't actually
    // kill the subprocess, the merge worker stays "active" for the whole
    // sleep instead of stopping within the tight budget.
    let check_cmd = if cfg!(windows) {
        format!("echo x > {started_str} & ping -n 21 127.0.0.1 >NUL")
    } else {
        format!("touch {started_str}; sleep 20")
    };
    store
        .lock()
        .unwrap()
        .set_guardian_checks(&id, &[check_cmd])
        .unwrap();

    let cancellations = Cancellations::new();
    let sem = Arc::new(Semaphore::new(4));
    let runner: Arc<dyn Runner> = Arc::new(NoopRunner);

    let reply = start_merge(
        Arc::clone(&store),
        Arc::clone(&runner),
        &id,
        Arc::clone(&sem),
        cancellations.clone(),
    );
    assert_eq!(reply.status, 202);

    for _ in 0..2000 {
        if started.exists() {
            break;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(started.exists(), "check gate command never started");

    // Cancel the review exactly the way the `review cancel` HTTP handler does.
    stop_merge_worker_for_cancel(&cancellations, &id);
    store.lock().unwrap().cancel_guardian(&id).unwrap();

    // The worker must stop well within the ~20s the check gate would
    // otherwise keep sleeping for -- a generous but bounded budget so this
    // test fails fast (rather than hanging ~20s) if cancellation regresses.
    let key = format!("guardian:{id}");
    for _ in 0..600 {
        if !cancellations.is_active(&key) {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(
        !cancellations.is_active(&key),
        "merge worker kept running the check-gate command well past the cancel"
    );
    assert_eq!(
        store.lock().unwrap().get_guardian(&id).unwrap().status,
        "cancelled"
    );

    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_dir_all(&marker_dir);
}

// RAL-101: a review with no `checks` configured (and not opted out) still gets
// an automatic build/test signal from the project's `.ralphus.toml` default.
#[test]
fn auto_build_runs_when_no_checks_configured() {
    let (root, store, id) = single_feature_repo();
    write(
        &root,
        ".ralphus.toml",
        "[review]\nauto_build = \"test -f a.txt\"\n",
    );
    run_merge(&store, &NoopRunner, &id);
    let view = store.lock().unwrap().get_guardian(&id).unwrap();
    assert_eq!(view.status, "in_review", "detail: {:?}", view.detail);
    assert!(
        view.detail.unwrap_or_default().contains("auto-built"),
        "expected the auto-build note to surface in the review detail"
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn failing_auto_build_fails_the_merge() {
    let (root, store, id) = single_feature_repo();
    write(
        &root,
        ".ralphus.toml",
        "[review]\nauto_build = \"test -f does_not_exist.txt\"\n",
    );
    run_merge(&store, &NoopRunner, &id);
    let view = store.lock().unwrap().get_guardian(&id).unwrap();
    assert_eq!(view.status, "merge_failed");
    assert!(
        view.detail
            .unwrap_or_default()
            .contains("auto-build failed")
    );
    let _ = std::fs::remove_dir_all(&root);
}

// A review with configured `checks` must not be double-built: the auto_build
// default here deliberately fails, so if it ran alongside the (passing)
// explicit check the merge would incorrectly fail.
#[test]
fn configured_checks_are_not_double_built_by_auto_build() {
    let (root, store, id) = single_feature_repo();
    write(
        &root,
        ".ralphus.toml",
        "[review]\nauto_build = \"test -f does_not_exist.txt\"\n",
    );
    store
        .lock()
        .unwrap()
        .set_guardian_checks(&id, &["test -f a.txt".to_string()])
        .unwrap();
    run_merge(&store, &NoopRunner, &id);
    let view = store.lock().unwrap().get_guardian(&id).unwrap();
    assert_eq!(view.status, "in_review", "detail: {:?}", view.detail);
    assert!(
        !view.detail.unwrap_or_default().contains("auto-built"),
        "the configured check gate should take priority over the auto-build default"
    );
    let _ = std::fs::remove_dir_all(&root);
}

// `guardian_skip_auto_build` opts out of config auto-build too, not just
// explicit checks (again, the auto_build default here deliberately fails to
// prove it never ran).
#[test]
fn skip_auto_build_also_opts_out_of_config_auto_build() {
    let (root, store, id) = single_feature_repo();
    write(
        &root,
        ".ralphus.toml",
        "[review]\nauto_build = \"test -f does_not_exist.txt\"\n",
    );
    store
        .lock()
        .unwrap()
        .set_guardian_skip_auto_build(&id, true)
        .unwrap();
    run_merge(&store, &NoopRunner, &id);
    let view = store.lock().unwrap().get_guardian(&id).unwrap();
    assert_eq!(view.status, "in_review", "detail: {:?}", view.detail);
    let _ = std::fs::remove_dir_all(&root);
}

// RAL-110: nothing configured (no explicit checks, no `.ralphus.toml
// auto_build`) -> `generate_manual_commands` asks the resolver agent for both
// the manual check commands AND a build command in the same call, then runs
// that build command against the combined worktree in advance.
struct InferredBuildRunner;
impl Runner for InferredBuildRunner {
    fn run(&self, spec: &RunnerSpec) -> RunnerResult {
        if spec.task == "manual_commands" {
            return RunnerResult {
                status: "done".into(),
                tokens_in: 0,
                tokens_out: 0,
                cache_creation_tokens: 0,
                cache_read_tokens: 0,
                cost_usd: 0.0,
                cost_is_estimated: false,
                summary: r#"{"manual_commands": ["echo verify"], "build_command": "echo built > built_marker.txt"}"#
                    .into(),
                error: None,
                proofed: None,
                agent_session_id: None,
                ghost: None,
            };
        }
        // Every other task (e.g. "summary") in this single-clean-branch
        // fixture should never be exercised.
        RunnerResult::failure("only manual_commands is faked in this test")
    }
}

#[test]
fn nothing_configured_runs_ai_inferred_build_in_advance() {
    let (root, store, id) = single_feature_repo();
    run_merge(&store, &InferredBuildRunner, &id);
    let view = store.lock().unwrap().get_guardian(&id).unwrap();
    assert_eq!(view.status, "in_review", "detail: {:?}", view.detail);
    assert_eq!(
        view.detail.as_deref(),
        Some("auto-built via inferred build command: echo built > built_marker.txt")
    );
    let combined = PathBuf::from(view.combined_worktree.expect("combined worktree set"));
    assert!(
        combined.join("built_marker.txt").exists(),
        "the inferred build command should have run against the combined worktree"
    );
    assert_eq!(view.manual_commands.len(), 1);
    assert_eq!(
        view.manual_commands[0].command.as_deref(),
        Some("echo verify")
    );
    let _ = std::fs::remove_dir_all(&root);
}

// RAL-110: `skip_auto_build` also opts out of the AI-inferred build tier (not
// just explicit checks / config auto_build) -- manual_commands are still
// generated (that's independent), but no build runs.
#[test]
fn skip_auto_build_opts_out_of_ai_inferred_build_too() {
    let (root, store, id) = single_feature_repo();
    store
        .lock()
        .unwrap()
        .set_guardian_skip_auto_build(&id, true)
        .unwrap();
    run_merge(&store, &InferredBuildRunner, &id);
    let view = store.lock().unwrap().get_guardian(&id).unwrap();
    assert_eq!(view.status, "in_review", "detail: {:?}", view.detail);
    assert_eq!(view.detail, None);
    let combined = PathBuf::from(view.combined_worktree.expect("combined worktree set"));
    assert!(
        !combined.join("built_marker.txt").exists(),
        "skip_auto_build must prevent the inferred build from running"
    );
    // Manual commands generation is independent of skip_auto_build -- it still
    // runs (the InferredBuildRunner's fake response still gets parsed and
    // stored) since only the *build* execution is skipped, not the LLM call.
    let _ = std::fs::remove_dir_all(&root);
}

// RAL-285: Proof scope "nothing" is an independent axis from `skip_auto_build`
// -- it must not affect the finalize-time AI-inferred auto-build.
#[test]
fn proof_scope_nothing_does_not_affect_auto_build() {
    let (root, store, id) = single_feature_repo();
    store
        .lock()
        .unwrap()
        .set_guardian_proof_scope(&id, Some("nothing"))
        .unwrap();
    run_merge(&store, &InferredBuildRunner, &id);
    let view = store.lock().unwrap().get_guardian(&id).unwrap();
    assert_eq!(view.status, "in_review", "detail: {:?}", view.detail);
    assert_eq!(
        view.detail.as_deref(),
        Some("auto-built via inferred build command: echo built > built_marker.txt"),
        "proof_scope=\"nothing\" must not suppress the auto-build tier"
    );
    let combined = PathBuf::from(view.combined_worktree.expect("combined worktree set"));
    assert!(combined.join("built_marker.txt").exists());
    let _ = std::fs::remove_dir_all(&root);
}

// Regression: Proof scope "nothing" must suppress the dedicated
// `run_final_proof` call itself, not just the quality-bar instructions
// handed to it (RAL-285 closed the gap where the quality-bar prompt was
// still synthesized regardless of scope). Exercises both `ProofGate` call
// sites in one pass: feature/x rebases cleanly onto main but contributes
// real changes (`allows_for_clean_branch`), and feature/y then conflicts
// against it and is resolved by the fake agent (`allows_after_conflict`).
#[test]
fn proof_scope_nothing_suppresses_final_verify() {
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
    let guardian_id = {
        let g = store.lock().unwrap();
        let id = g
            .create_guardian("skip-checks", "main", root.to_str().unwrap())
            .unwrap();
        g.add_guardian_branch(&id, "feature/x").unwrap();
        g.add_guardian_branch(&id, "feature/y").unwrap();
        g.set_guardian_proof_scope(&id, Some("nothing")).unwrap();
        id
    };

    struct MarkerStrippingRunner {
        specs: Arc<Mutex<Vec<RunnerSpec>>>,
    }
    impl Runner for MarkerStrippingRunner {
        fn run(&self, spec: &RunnerSpec) -> RunnerResult {
            self.specs.lock().unwrap().push(spec.clone());
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
                cache_creation_tokens: 0,
                cache_read_tokens: 0,
                cost_usd: 0.0,
                cost_is_estimated: false,
                summary: "resolved".into(),
                error: None,
                proofed: spec.proof.then_some(true),
                agent_session_id: None,
                ghost: None,
            }
        }
    }

    let captured: Arc<Mutex<Vec<RunnerSpec>>> = Arc::new(Mutex::new(Vec::new()));
    let runner = MarkerStrippingRunner {
        specs: captured.clone(),
    };

    run_merge(&store, &runner, &guardian_id);

    let view = store.lock().unwrap().get_guardian(&guardian_id).unwrap();
    assert_eq!(view.status, "in_review", "merge failed: {:?}", view.detail);
    let x = view
        .branches
        .iter()
        .find(|b| b.branch == "feature/x")
        .expect("feature/x branch view");
    let y = view
        .branches
        .iter()
        .find(|b| b.branch == "feature/y")
        .expect("feature/y branch view");
    // Neither branch's final status reflects a proof call having run.
    assert_eq!(
        x.merge_status, "done",
        "feature/x (clean rebase, real changes) must skip final-proof entirely"
    );
    assert_eq!(
        y.merge_status, "conflict_resolved",
        "feature/y (agent-resolved conflict) must skip final-proof entirely"
    );

    let specs = captured.lock().unwrap();
    assert!(
        specs.iter().all(|s| s.task != "resolve-proof"),
        "proof_scope=\"nothing\" must suppress the dedicated final-proof call, \
         but a resolve-proof spec was issued: {specs:?}"
    );

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

// A feature branch that adds nothing over the branch beneath it fails the
// merge outright (RAL-190) — a review must never silently approve a stack
// containing a branch whose work it does not actually carry. The branch is
// also flagged `is_empty` so the board can label it, and the failure message
// names the escape hatch (disable the branch) rather than just refusing.
#[test]
fn branch_with_no_new_commits_fails_the_merge() {
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
        assert_eq!(
            view.status, "merge_failed",
            "an empty branch must fail the review, not quietly pass it: detail: {:?}",
            view.detail
        );
        assert_eq!(view.branches[0].merge_status, "failed");
        assert!(
            view.branches[0].is_empty,
            "the branch must also carry the `is_empty` flag the board renders"
        );
        let detail = view.branches[0].detail.clone().unwrap_or_default();
        assert!(
            detail.contains("branch is empty"),
            "detail must say why: {detail}"
        );
        assert!(
            detail.contains("disable it"),
            "detail must name the escape hatch for a deliberately-empty branch: {detail}"
        );
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
    let rebuilt = rebuild_on_base_shift(&store, &NoopRunner, &id, &sem, &CancelToken::never());
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
        !rebuild_on_base_shift(&store, &NoopRunner, &id, &sem, &CancelToken::never()),
        "no rebuild when base is unchanged"
    );

    let _ = std::fs::remove_dir_all(&root);
}

// Shared-worktree reviews cannot preserve a per-branch staged prefix. A base
// shift entering through `run_merge_staged` must therefore take its documented
// all-or-nothing fallback rather than executing the staged engine.
#[test]
fn base_shift_for_skip_worktrees_uses_cancellable_fallback() {
    let (root, store, id) = single_feature_repo();
    store
        .lock()
        .unwrap()
        .set_guardian_skip_worktrees(&id, true)
        .unwrap();
    run_merge(&store, &NoopRunner, &id);

    let event_counts = || {
        let page = store
            .lock()
            .unwrap()
            .cartographer_query(&CartographerFilter {
                guardian_id: Some(id.clone()),
                limit: 100,
                ..CartographerFilter::default()
            })
            .unwrap();
        (
            page.rows
                .iter()
                .filter(|row| row.message == "merge executing")
                .count(),
            page.rows
                .iter()
                .filter(|row| row.message == "staged merge executing")
                .count(),
        )
    };
    let before = event_counts();

    git(&root, &["checkout", "main"]);
    write(&root, "base-shift.txt", "new base\n");
    git(&root, &["add", "."]);
    git(&root, &["commit", "-m", "base shifts"]);

    let sem = Semaphore::new(4);
    assert!(rebuild_on_base_shift(
        &store,
        &NoopRunner,
        &id,
        &sem,
        &CancelToken::never()
    ));
    let after = event_counts();
    assert_eq!(after.0, before.0 + 1, "legacy fallback must execute once");
    assert_eq!(
        after.1, before.1,
        "shared-worktree fallback must return before staged execution"
    );
    let view = store.lock().unwrap().get_guardian(&id).unwrap();
    assert_eq!(view.status, "in_review", "detail: {:?}", view.detail);
    assert!(
        Path::new(view.combined_worktree.as_deref().expect("combined"))
            .join("base-shift.txt")
            .exists(),
        "fallback rebuild includes the shifted base"
    );

    let _ = std::fs::remove_dir_all(&root);
}

// RAL-300: when the base branch's shift IS the review's own work landing (a
// fast-forward merge that happened outside any tracked PR), the base
// shift is not new upstream work to rebase onto -- it must approve the
// review instead of wasting a rebuild against a base that already has it.
#[test]
fn base_shift_that_already_contains_the_review_approves_instead_of_rebuilding() {
    let (root, store, id) = single_feature_repo();
    run_merge(&store, &NoopRunner, &id);
    let before = store.lock().unwrap().get_guardian(&id).unwrap();
    assert_eq!(before.status, "in_review");
    let review_branch = before.branches[0]
        .review_branch
        .clone()
        .expect("review branch built");

    // Simulate the feature landing on `main` directly (outside any tracked
    // PR): fast-forward main to the review branch's own tip.
    git(&root, &["checkout", "main"]);
    git(&root, &["merge", "--ff-only", &review_branch]);

    let sem = Semaphore::new(4);
    // `rebuild_on_base_shift` reports `true` here too (it "handled" the base
    // shift, just by approving instead of rebuilding) -- what actually
    // matters is the guardian's status below, not this return value.
    rebuild_on_base_shift(&store, &NoopRunner, &id, &sem, &CancelToken::never());

    let after = store.lock().unwrap().get_guardian(&id).unwrap();
    assert_eq!(
        after.status, "approved",
        "must approve instead of rebuilding: detail {:?}",
        after.detail
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn manual_merge_approves_when_the_review_worktree_is_already_in_its_upstream() {
    let (root, store, id) = single_feature_repo();
    run_merge(&store, &NoopRunner, &id);
    let before = store.lock().unwrap().get_guardian(&id).unwrap();
    let branch = &before.branches[0];
    let review_branch = branch.review_branch.as_deref().expect("review branch");
    let worktree = branch.worktree.as_deref().expect("review worktree");

    git(Path::new(worktree), &["branch", "landed", "HEAD"]);
    git(
        Path::new(worktree),
        &["branch", "--set-upstream-to=landed", review_branch],
    );

    let reply = start_merge(
        Arc::clone(&store),
        Arc::new(NoopRunner),
        &id,
        Arc::new(Semaphore::new(4)),
        Cancellations::new(),
    );
    assert_eq!(reply.status, 200, "body={}", reply.body);
    assert!(reply.body.contains("\"status\":\"approved\""));
    assert_eq!(
        store.lock().unwrap().get_guardian(&id).unwrap().status,
        "approved"
    );

    let _ = std::fs::remove_dir_all(&root);
}

// RAL-250: a review that opts out of base-branch auto-updates is left alone by
// the maintenance base-shift pass even when its base advances — and the old
// baseline is kept, so opting back in immediately catches the review up.
#[test]
fn skip_base_updates_prevents_auto_rebuild_on_base_shift() {
    let (root, store, id) = single_feature_repo();
    run_merge(&store, &NoopRunner, &id);
    let before = store.lock().unwrap().get_guardian(&id).unwrap();
    assert_eq!(before.status, "in_review");
    let base_before = before.base_commit.clone().expect("base recorded");

    // Opt this review out of base-branch auto-updates.
    store
        .lock()
        .unwrap()
        .set_guardian_skip_base_updates(&id, Some(true))
        .unwrap();
    assert!(
        store
            .lock()
            .unwrap()
            .get_guardian(&id)
            .unwrap()
            .effective_skip_base_updates
    );

    // Advance the base branch (main) with a new commit.
    git(&root, &["checkout", "main"]);
    write(&root, "c.txt", "on base\n");
    git(&root, &["add", "."]);
    git(&root, &["commit", "-m", "base moves forward"]);

    let sem = Semaphore::new(4);
    assert!(
        !rebuild_on_base_shift(&store, &NoopRunner, &id, &sem, &CancelToken::never()),
        "opted-out review must not auto-rebuild on a base shift"
    );
    let after = store.lock().unwrap().get_guardian(&id).unwrap();
    assert_eq!(after.status, "in_review", "status untouched");
    assert_eq!(
        after.base_commit.as_deref(),
        Some(base_before.as_str()),
        "baseline not advanced while opted out, so re-enabling catches up"
    );

    // Re-enable (opt back in): the base is now ahead of the recorded baseline,
    // so the very next sweep rebuilds — restoring the auto-update behavior.
    store
        .lock()
        .unwrap()
        .set_guardian_skip_base_updates(&id, Some(false))
        .unwrap();
    assert!(
        rebuild_on_base_shift(&store, &NoopRunner, &id, &sem, &CancelToken::never()),
        "re-enabling restores the base-shift rebuild"
    );

    let _ = std::fs::remove_dir_all(&root);
}

// RAL-97/98 regression: a "linked" guardian whose branches are contributed by
// SEPARATE runs (joined by a shared `review` key rather than one multi-file
// submission) can leave a later branch stuck at `merge_status = "pending"`
// forever. This happens when the first run's task finishes, the guardian sees
// all blocking tasks IT knows about done, and leaves `collecting` (→
// `in_review`) before the second run's task — and therefore its branch —
// exists at all. Once the guardian is no longer `collecting`,
// `try_start_ready_reviews_for_task`'s `collecting_guardians_for_cells`
// query never finds it again, so the straggler branch never gets its
// `worktree`/`review_branch` populated even though its session is `done`.
// `reopen_straggler` (wired into the periodic `review_maintenance` sweep) must
// detect and heal exactly this.
#[test]
fn straggler_branch_from_a_later_run_is_reopened_and_merged() {
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

    let mut owned_store = Store::open_in_memory().unwrap();
    let sample: TaskFile =
        toml::from_str("[[task]]\nname=\"t\"\n[[task.cell]]\ncwd=\"/repo\"\nprompt=\"p\"\n")
            .unwrap();

    // Guardian created for run A, carrying only feature/a (mirrors RAL-97's task
    // finishing first, in its own run).
    let run_a = owned_store.insert_squad(&sample, Some("a"), false).unwrap();
    let id = {
        let g = &owned_store;
        let id = g
            .create_guardian_for_squad("linked", "main", root.to_str().unwrap(), Some(&run_a))
            .unwrap();
        g.add_guardian_branch(&id, "feature/a").unwrap();
        id
    };
    owned_store
        .set_cell_review_branch(&run_a, 0, 0, "feature/a")
        .unwrap();
    owned_store
        .set_cell_state(&run_a, 0, 0, NodeState::Done)
        .unwrap();

    let store = Arc::new(Mutex::new(owned_store));
    // Run A's task completing drives the guardian all the way to `in_review`,
    // exactly as the scheduler would once `feature/a`'s branch is the only one
    // the guardian knows about yet.
    run_merge(&store, &NoopRunner, &id);
    assert_eq!(
        store.lock().unwrap().get_guardian(&id).unwrap().status,
        "in_review"
    );

    // RAL-98's task now finishes, in a SEPARATE run, contributing a second
    // branch to the SAME (already `in_review`) guardian by review-key linkage.
    let run_b = {
        let mut g = store.lock().unwrap();
        g.insert_squad(&sample, Some("b"), false).unwrap()
    };
    {
        let g = store.lock().unwrap();
        g.add_guardian_branch(&id, "feature/b").unwrap();
        g.set_cell_review_branch(&run_b, 0, 0, "feature/b").unwrap();
        g.set_cell_state(&run_b, 0, 0, NodeState::Done).unwrap();
    }

    // Sanity check: this is the bug. The straggler branch is stuck `pending`
    // with no worktree even though its contributing session is `done`.
    let stuck = store.lock().unwrap().get_guardian(&id).unwrap();
    assert_eq!(stuck.status, "in_review");
    let straggler = stuck
        .branches
        .iter()
        .find(|b| b.branch == "feature/b")
        .expect("feature/b present");
    assert_eq!(straggler.merge_status, "pending");
    assert!(straggler.worktree.is_none());

    // The self-heal: reopen_straggler (as called by review_maintenance's
    // periodic sweep) must detect the done-but-pending branch, reopen the
    // guardian, and rebuild the stack to pick it up.
    let sem = Semaphore::new(4);
    let reopened = reopen_straggler(&store, &NoopRunner, &id, &sem, &CancelToken::never());
    assert!(reopened, "a ready straggler branch must trigger a reopen");

    let healed = store.lock().unwrap().get_guardian(&id).unwrap();
    assert_eq!(healed.status, "in_review", "detail: {:?}", healed.detail);
    assert!(
        healed.branches.iter().all(|b| b.merge_status == "done"),
        "both branches must be merged: {:?}",
        healed.branches
    );
    let review = healed.review_branch.expect("review branch set");
    let files = git(&root, &["ls-tree", "-r", "--name-only", &review]);
    assert!(
        files.contains("a.txt") && files.contains("b.txt"),
        "review branch must contain both linked branches' commits: {files}"
    );

    // A second sweep with nothing new pending is a no-op.
    assert!(
        !reopen_straggler(&store, &NoopRunner, &id, &sem, &CancelToken::never()),
        "no reopen when there is no ready straggler"
    );

    let _ = std::fs::remove_dir_all(&root);
}

// A staged base-shift rebuild must match the full rebuild's resolved-conflict
// end state without deleting a healthy linked worktree. The prior resolved
// commit is replayed onto the new base even with rerere disabled, and the
// resolver is only needed for the first build.
#[test]
fn staged_base_shift_preserves_prior_resolution_and_worktree() {
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

    // Build 1 through the staged engine: rebasing feature/x onto the advanced
    // base conflicts; the agent resolves it (StageDoneRunner strips markers,
    // keeping both sides) and records the build signature used below.
    let branch_id = store.lock().unwrap().get_guardian(&id).unwrap().branches[0]
        .id
        .clone();
    mark_ready(&store, &id, &branch_id);
    run_merge_staged(&store, &StageDoneRunner, &id, &CancelToken::never());
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
    let worktree1 = PathBuf::from(x1.worktree.clone().expect("branch worktree"));
    let sentinel = worktree1.join("keep-worktree-sentinel.tmp");
    std::fs::write(&sentinel, "preserved\n").expect("write sentinel");

    // The base shifts AGAIN, in an unrelated file — no new conflict on conflict.txt.
    git(&root, &["checkout", "main"]);
    write(&root, "unrelated.txt", "later\n");
    git(&root, &["add", "."]);
    git(&root, &["commit", "-m", "unrelated base move"]);

    // Build 2 via the auto-rebuild path, with a runner that MUST NOT be called.
    // The staged replay carries the already-resolved commit onto the new base, so the
    // resolver agent is never invoked. If it regressed and re-derived from the
    // feature tip, the old conflict would resurface, NoopRunner would be called,
    // and the guardian would end merge_failed — caught by the assertion below.
    let sem = Semaphore::new(4);
    assert!(
        rebuild_on_base_shift(&store, &NoopRunner, &id, &sem, &CancelToken::never()),
        "the second base shift should trigger a rebuild"
    );

    let v2 = store.lock().unwrap().get_guardian(&id).unwrap();
    assert_eq!(
        v2.status, "in_review",
        "carry-forward should reach review without the agent; detail: {:?}",
        v2.detail
    );
    let review2 = v2.review_branch.clone().expect("review branch");
    let x2 = v2
        .branches
        .iter()
        .find(|b| b.branch == "feature/x")
        .unwrap();
    assert_eq!(
        x2.merge_status, "done",
        "replaying the resolved commit onto an unrelated base shift is clean, matching the full rebuild"
    );
    assert_eq!(
        x2.worktree.as_deref(),
        Some(worktree1.to_string_lossy().as_ref()),
        "base-shift rebuild must retain the branch worktree path"
    );
    assert!(
        sentinel.exists(),
        "an untracked sentinel proves cleanup did not delete and recreate the worktree"
    );
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

/// A resolver that behaves like [`StageDoneRunner`] and, the first time it is
/// called, lands a new commit on the guardian's base branch -- the real-world
/// shape where upstream advances while a staged pass is still resolving
/// conflicts and running proofs.
struct BaseAdvancingRunner {
    root: PathBuf,
    advanced: AtomicBool,
}

impl Runner for BaseAdvancingRunner {
    fn run(&self, spec: &RunnerSpec) -> RunnerResult {
        let result = StageDoneRunner.run(spec);
        if !self.advanced.swap(true, Ordering::Relaxed) {
            write(&self.root, "upstream.txt", "landed mid-merge\n");
            git(&self.root, &["add", "."]);
            git(&self.root, &["commit", "-m", "base advances mid-merge"]);
        }
        result
    }
}

// A base branch that advances *during* a staged pass is still a shift once that
// pass finishes: the finalize step records the commit the stack was actually
// rebased onto rather than re-resolving the (by then newer) base, so the
// maintenance sweep sees the difference and rebuilds instead of reading a stale
// review as current.
#[test]
fn staged_finalize_keeps_the_base_the_stack_was_built_on() {
    let root = temp_repo();
    init_repo(&root);
    git(&root, &["config", "rerere.enabled", "false"]);

    write(&root, "conflict.txt", "line1\nBASE\nline3\n");
    git(&root, &["add", "."]);
    git(&root, &["commit", "-m", "base"]);

    git(&root, &["checkout", "-b", "feature/x"]);
    write(&root, "conflict.txt", "line1\nX\nline3\n");
    git(&root, &["commit", "-am", "x"]);
    git(&root, &["checkout", "main"]);

    // The base moves before the merge starts so the stack rebase conflicts and
    // the resolver -- which is what advances the base again mid-pass -- runs.
    write(&root, "conflict.txt", "line1\nMAIN2\nline3\n");
    git(&root, &["commit", "-am", "main advances"]);
    let built_on = git(&root, &["rev-parse", "main"]).trim().to_string();

    let store = Arc::new(Mutex::new(Store::open_in_memory().unwrap()));
    let id = {
        let g = store.lock().unwrap();
        let id = g
            .create_guardian("mid-merge-shift", "main", root.to_str().unwrap())
            .unwrap();
        g.add_guardian_branch(&id, "feature/x").unwrap();
        id
    };
    let branch_id = store.lock().unwrap().get_guardian(&id).unwrap().branches[0]
        .id
        .clone();
    mark_ready(&store, &id, &branch_id);

    let runner = BaseAdvancingRunner {
        root: root.clone(),
        advanced: AtomicBool::new(false),
    };
    run_merge_staged(&store, &runner, &id, &CancelToken::never());
    assert!(
        runner.advanced.load(Ordering::Relaxed),
        "the resolver never ran, so the base never advanced mid-merge"
    );

    let moved = git(&root, &["rev-parse", "main"]).trim().to_string();
    assert_ne!(moved, built_on, "the base must have advanced mid-merge");

    let v1 = store.lock().unwrap().get_guardian(&id).unwrap();
    assert_eq!(v1.status, "in_review", "detail: {:?}", v1.detail);
    assert_eq!(v1.base_commits.len(), 1, "single-project guardian");
    assert_eq!(
        v1.base_commits.values().next().map(String::as_str),
        Some(built_on.as_str()),
        "finalize must record the base the stack was rebased onto, not the newer tip"
    );

    // Because the baseline still names what was built, the sweep sees the shift.
    let sem = Semaphore::new(4);
    assert!(
        rebuild_on_base_shift(&store, &StageDoneRunner, &id, &sem, &CancelToken::never()),
        "a base that moved mid-merge must still trigger a rebuild afterwards"
    );

    let v2 = store.lock().unwrap().get_guardian(&id).unwrap();
    assert_eq!(v2.status, "in_review", "detail: {:?}", v2.detail);
    assert_eq!(
        v2.base_commits.values().next().map(String::as_str),
        Some(moved.as_str()),
        "the rebuild rebases onto the newer base and records it"
    );
    let combined = v2.combined_worktree.as_deref().expect("combined");
    assert!(
        Path::new(combined).join("upstream.txt").exists(),
        "the rebuilt review must contain the commit that landed mid-merge"
    );

    let _ = std::fs::remove_dir_all(&root);
}

// The all-or-nothing merge remains the parity baseline for a resolved conflict:
// replaying its prior review tip across an unrelated base shift reaches the same
// clean branch/guardian state and preserves the resolved file content.
#[test]
fn full_rebuild_preserves_prior_resolution_state() {
    let root = temp_repo();
    init_repo(&root);
    git(&root, &["config", "rerere.enabled", "false"]);

    write(&root, "conflict.txt", "line1\nBASE\nline3\n");
    git(&root, &["add", "."]);
    git(&root, &["commit", "-m", "base"]);

    git(&root, &["checkout", "-b", "feature/x"]);
    write(&root, "conflict.txt", "line1\nX\nline3\n");
    git(&root, &["commit", "-am", "x"]);
    git(&root, &["checkout", "main"]);
    write(&root, "conflict.txt", "line1\nMAIN2\nline3\n");
    git(&root, &["commit", "-am", "main advances"]);

    let store = Arc::new(Mutex::new(Store::open_in_memory().unwrap()));
    let id = {
        let guard = store.lock().unwrap();
        let id = guard
            .create_guardian("full rebuild parity", "main", root.to_str().unwrap())
            .unwrap();
        guard.add_guardian_branch(&id, "feature/x").unwrap();
        id
    };
    run_merge(&store, &StageDoneRunner, &id);
    assert_eq!(
        store.lock().unwrap().get_guardian(&id).unwrap().branches[0].merge_status,
        "conflict_resolved"
    );

    git(&root, &["checkout", "main"]);
    write(&root, "unrelated.txt", "later\n");
    git(&root, &["add", "."]);
    git(&root, &["commit", "-m", "unrelated base move"]);
    run_merge(&store, &NoopRunner, &id);

    let rebuilt = store.lock().unwrap().get_guardian(&id).unwrap();
    assert_eq!(rebuilt.status, "in_review", "detail: {:?}", rebuilt.detail);
    assert_eq!(rebuilt.branches[0].merge_status, "done");
    let review = rebuilt.review_branch.expect("review branch");
    let resolved = git(&root, &["show", &format!("{review}:conflict.txt")]);
    assert!(resolved.contains('X') && resolved.contains("MAIN2"));
    assert!(!resolved.contains("<<<<<<<"));
    assert!(
        Path::new(rebuilt.combined_worktree.as_deref().expect("combined"))
            .join("unrelated.txt")
            .exists()
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
    assert!(rebuild_on_base_shift(
        &store,
        &NoopRunner,
        &id,
        &sem,
        &CancelToken::never()
    ));
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

// RAL-72 regression (superseded by RAL-144, see below): `conflicts_found`
// used to be a one-time snapshot taken before the resolve loop started, so a
// branch whose rebase hits conflicts on more than one commit kept reporting
// only the *first* commit's marker count forever after — while
// `conflicts_committed` kept climbing. This left the board's "N found / M
// fixed / K committed" progress line either stuck or (when the first commit
// had zero markers, e.g. a rerere fast path) hidden entirely, even while the
// resolver was actively fixing later commits.
//
// RAL-144 replaced RAL-72's fix (which accumulated `found`/`committed`
// across the whole branch) with per-commit rescoping: `found` is recomputed
// fresh from disk every loop iteration, and `committed` resets to 0 every
// time the rebase advances to its next commit. Both values are now scoped to
// whichever commit the rebase is presently stopped on, and neither
// accumulates across the two sequential conflicting commits below —
// `committed` in particular must reset between resolving y1's conflict and
// resolving y2's, rather than carrying y1's staged count forward.
#[test]
fn conflict_counters_reset_across_sequential_conflicting_commits() {
    let root = temp_repo();
    init_repo(&root);
    write(&root, "conflict1.txt", "line1\nBASE\nline3\n");
    write(&root, "conflict2.txt", "line1\nBASE\nline3\n");
    git(&root, &["add", "."]);
    git(&root, &["commit", "-m", "base"]);

    // feature/x changes both files -> stacks cleanly first.
    git(&root, &["checkout", "-b", "feature/x"]);
    write(&root, "conflict1.txt", "line1\nX1\nline3\n");
    write(&root, "conflict2.txt", "line1\nX2\nline3\n");
    git(&root, &["commit", "-am", "x"]);

    // feature/y (from main) touches the SAME two files across TWO separate
    // commits, so rebasing it onto the stack hits two distinct conflict
    // episodes, not one.
    git(&root, &["checkout", "main"]);
    git(&root, &["checkout", "-b", "feature/y"]);
    write(&root, "conflict1.txt", "line1\nY1\nline3\n");
    git(&root, &["commit", "-am", "y1"]);
    write(&root, "conflict2.txt", "line1\nY2\nline3\n");
    git(&root, &["commit", "-am", "y2"]);
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
    let y = view
        .branches
        .iter()
        .find(|b| b.branch == "feature/y")
        .unwrap();
    assert_eq!(y.merge_status, "conflict_resolved");
    // `found` reflects only the last commit resolved (y2's single marker), not
    // an accumulated total across both of y's commits.
    assert_eq!(
        y.conflicts_found,
        Some(1),
        "found must be rescoped to the current commit, not accumulated: detail: {:?}",
        y.detail
    );
    // `committed` must reset to 0 once the rebase finishes advancing past the
    // final resolved commit, not keep climbing across y1 and y2.
    assert_eq!(
        y.conflicts_committed,
        Some(0),
        "committed must reset between sequential conflicting commits: detail: {:?}",
        y.detail
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

/// A fake conflict resolver that also reports a self-summarized handoff note,
/// like a real backend would when it parses a `RALPHUS_GHOST:` marker out of
/// the agent's reply (RAL-136).
struct MarkerStrippingWithGhostRunner;
impl Runner for MarkerStrippingWithGhostRunner {
    fn run(&self, spec: &RunnerSpec) -> RunnerResult {
        let mut r = MarkerStrippingRunner.run(spec);
        r.ghost = Some("kept both sides of the conflict; worth a follow-up review".to_string());
        r
    }
}

/// RAL-136: a review worktree's conflict resolver publishes a ghost from its
/// self-summarized handoff note, keyed by the review branch's URI.
#[test]
fn conflict_resolution_publishes_a_review_ghost() {
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
            .create_guardian("conflict review", "main", root.to_str().unwrap())
            .unwrap();
        g.add_guardian_branch(&id, "feature/x").unwrap();
        g.add_guardian_branch(&id, "feature/y").unwrap();
        id
    };

    run_merge(&store, &MarkerStrippingWithGhostRunner, &id);

    let view = store.lock().unwrap().get_guardian(&id).unwrap();
    let y = view
        .branches
        .iter()
        .find(|b| b.branch == "feature/y")
        .unwrap();
    assert_eq!(y.merge_status, "conflict_resolved");

    let uri = ralphus_daemon::ghost::review_uri(&id, Some(&y.id));
    let ghost = store
        .lock()
        .unwrap()
        .get_ghost(&uri)
        .unwrap()
        .expect("resolver's handoff note was published as a review ghost");
    assert!(ghost.content.contains("kept both sides of the conflict"));
    assert_eq!(ghost.kind, "review");
    assert_eq!(ghost.guardian_id.as_deref(), Some(id.as_str()));

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

// Proof-synthesis integration: when a guardian is linked to a squad whose cell
// has proof steps, `resolve_conflicts_with_agent` must:
//   1. invoke a "proof-synthesis" LLM call whose prompt lists every step
//      (command-kind, prompt-kind, and task-level) in the expected format; and
//   2. inject the synthesised quality bar into the subsequent "resolve" call's
//      prompt so the conflict-resolver agent knows what standard to meet.
#[test]
fn conflict_resolution_synthesizes_proof_steps_into_resolver_prompt() {
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

    // --- squad with cell proof steps (command + prompt) and a task proof ---
    const TASK_TOML: &str = r#"
[[task]]
name = "my-task"
[[task.cell]]
id = "impl"
cwd = "/repo"
prompt = "implement the feature"
[[task.cell.proof]]
command = "cargo fmt --check"
[[task.cell.proof]]
prompt = "The code must follow project style guidelines and be readable"
[[task.proof]]
command = "cargo test --workspace"
"#;

    let store = Arc::new(Mutex::new(Store::open_in_memory().unwrap()));

    // Insert the squad then link the guardian to it. feature/y is the branch
    // that will conflict (rebased on top of feature/x), so its cell gets the
    // proof steps.
    let run_id = {
        let task_file: ralphus_core::schema::TaskFile = toml::from_str(TASK_TOML).unwrap();
        store
            .lock()
            .unwrap()
            .insert_squad(&task_file, Some("synthesis test"), false)
            .unwrap()
    };

    let guardian_id = {
        let g = store.lock().unwrap();
        let id = g
            .create_guardian_for_squad("review", "main", root.to_str().unwrap(), Some(&run_id))
            .unwrap();
        g.add_guardian_branch(&id, "feature/x").unwrap();
        g.add_guardian_branch(&id, "feature/y").unwrap();
        // Link task 0, cell 0 to feature/y so its proof steps are picked up
        // during synthesis.
        g.set_cell_review_branch(&run_id, 0, 0, "feature/y")
            .unwrap();
        // RAL-168: "each_branch" scope now also verifies a branch that rebases
        // cleanly with no conflict at all (feature/x here) -- opt that back out
        // so this test's single `resolve-proof` spec stays scoped to the
        // conflict-resolution path (feature/y) it's actually testing.
        g.set_guardian_proof_skip_auto_clean(&id, Some(true))
            .unwrap();
        id
    };

    // --- capturing runner ---
    // On a "proof-synthesis" call: record the spec and return a fixed summary.
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

            if spec.task == "proof-synthesis" {
                return RunnerResult {
                    status: "done".into(),
                    tokens_in: 15,
                    tokens_out: 30,
                    cache_creation_tokens: 0,
                    cache_read_tokens: 0,
                    cost_usd: 0.0,
                    cost_is_estimated: false,
                    summary: self.synth_summary.into(),
                    error: None,
                    proofed: None,
                    agent_session_id: None,
                    ghost: None,
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
                cache_creation_tokens: 0,
                cache_read_tokens: 0,
                cost_usd: 0.0,
                cost_is_estimated: false,
                summary: "resolved".into(),
                error: None,
                // RAL-149: the dedicated final-proof call is `proof: true`;
                // report a PASS verdict for it so the branch's merge status
                // still lands on `conflict_resolved` (a FAIL verdict is
                // otherwise a valid, non-blocking outcome, but this test is
                // about the resolver/synthesis prompts, not proof verdicts).
                proofed: spec.proof.then_some(true),
                agent_session_id: None,
                ghost: None,
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
        .find(|s| s.task == "proof-synthesis")
        .expect("proof-synthesis spec not found — synthesis was never invoked");

    let synth_prompt = synth.prompt.as_deref().unwrap_or("");

    // Cell-level command proof step must appear.
    assert!(
        synth_prompt.contains("[command] cargo fmt --check"),
        "synthesis prompt missing cell command step:\n{synth_prompt}"
    );
    // Cell-level prompt proof step must appear.
    assert!(
        synth_prompt.contains("[prompt]") && synth_prompt.contains("style guidelines"),
        "synthesis prompt missing cell prompt-kind step:\n{synth_prompt}"
    );
    // Task-level command proof step must appear.
    assert!(
        synth_prompt.contains("[command] cargo test --workspace"),
        "synthesis prompt missing task-level proof step:\n{synth_prompt}"
    );

    // 2. The synthesis system prompt is present and mentions the rebase constraint.
    let synth_sys = synth.system_prompt.as_deref().unwrap_or("");
    assert!(
        synth_sys.contains("CANNOT") && synth_sys.contains("commit"),
        "synthesis system prompt should forbid commit/push:\n{synth_sys}"
    );

    // 3. RAL-168: the fix pass's own ("resolve") prompt never carries the
    //    quality bar -- that responsibility belongs solely to the dedicated
    //    final-proof call.
    let resolve = specs
        .iter()
        .find(|s| s.task == "resolve")
        .expect("resolve spec not found — resolver was never invoked");
    let resolve_prompt = resolve.prompt.as_deref().unwrap_or("");
    assert!(
        !resolve_prompt.contains(SYNTH_SUMMARY),
        "fix pass prompt should never carry the quality bar (RAL-168):\n{resolve_prompt}"
    );

    // ...it must instead appear in the dedicated final-proof call, which
    // runs the quality-bar instructions under the default "each_branch"
    // Proof scope (RAL-168).
    let proof_call = specs
        .iter()
        .find(|s| s.task == "resolve-proof")
        .expect("resolve-proof spec not found — final-proof call was never invoked");
    assert!(proof_call.proof, "final-proof call must be proof: true");
    let proof_prompt = proof_call.prompt.as_deref().unwrap_or("");
    assert!(
        proof_prompt.contains("quality bar"),
        "final-proof prompt missing synthesised quality bar:\n{proof_prompt}"
    );
    assert!(
        proof_prompt.contains(SYNTH_SUMMARY),
        "final-proof prompt does not contain the synthesised text:\n{proof_prompt}"
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
            .any(|e| e.message.contains("synthesizing proof instructions")),
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
    let bid0 = view.branches[0].id.clone();

    // Record the review-branch HEAD before feedback so we can check it didn't move.
    let head_before = git(&root, &["rev-parse", &rev]);

    run_feedback(
        &store,
        &NamedFeedbackRunner("note.txt"),
        &id,
        &bid0,
        "add a note file, don't commit",
        &CancelToken::never(),
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
    let bid0 = view.branches[0].id.clone();

    // Turn 1: no-commit — agent writes note1.txt, which stays uncommitted.
    run_feedback(
        &store,
        &NamedFeedbackRunner("note1.txt"),
        &id,
        &bid0,
        "add note1, don't commit",
        &CancelToken::never(),
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
        &bid0,
        "add note2",
        &CancelToken::never(),
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
    let rebuilt = rebuild_on_base_shift(&store, &NoopRunner, &id, &sem, &CancelToken::never());
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
                cache_creation_tokens: 0,
                cache_read_tokens: 0,
                cost_usd: 0.0,
                cost_is_estimated: false,
                summary: "resolved".into(),
                error: None,
                proofed: None,
                agent_session_id: None,
                ghost: None,
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
fn review_resolver_agent_resolves_a_custom_agent_profile() {
    // A review's `resolver_agent` naming a configured `.ralphus.toml` custom
    // profile (the pattern used to reach e.g. OpenRouter through the
    // claude-code harness) must resolve to that profile's real backend and
    // pick up its env vars -- not reach the runner as the raw, unresolved
    // profile name. This is the regression test for the bug where the review
    // path never called `agent_profiles::resolve_agent_for_path` at all.
    let root = temp_repo();
    init_repo(&root);
    write(
        &root,
        ".ralphus.toml",
        "[agent.profiles.test-profile]\n\
         backend = \"claude-code\"\n\
         [agent.profiles.test-profile.env]\n\
         TEST_MARKER = \"openrouter-value\"\n",
    );
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

    let captured: Arc<Mutex<Vec<RunnerSpec>>> = Arc::new(Mutex::new(Vec::new()));
    struct CapturingStageDoneRunner {
        specs: Arc<Mutex<Vec<RunnerSpec>>>,
    }
    impl Runner for CapturingStageDoneRunner {
        fn run(&self, spec: &RunnerSpec) -> RunnerResult {
            self.specs.lock().unwrap().push(spec.clone());
            StageDoneRunner.run(spec)
        }
    }
    let runner = CapturingStageDoneRunner {
        specs: captured.clone(),
    };

    let store = Arc::new(Mutex::new(Store::open_in_memory().unwrap()));
    let id = {
        let g = store.lock().unwrap();
        let id = g
            .create_guardian("resolver-profile-check", "main", root.to_str().unwrap())
            .unwrap();
        g.add_guardian_branch(&id, "feature/x").unwrap();
        g.add_guardian_branch(&id, "feature/y").unwrap();
        g.set_guardian_resolver(&id, Some("test-profile"), None)
            .unwrap();
        id
    };

    run_merge(&store, &runner, &id);

    let specs = captured.lock().unwrap();
    let resolve = specs
        .iter()
        .find(|s| s.task == "resolve")
        .expect("resolve spec not found — conflict resolver was never invoked");
    assert_eq!(
        resolve.agent, "claude-code",
        "the resolver spec's agent must be the profile's resolved backend, not the raw profile name"
    );
    assert_eq!(
        resolve.env_overrides.get("TEST_MARKER").map(String::as_str),
        Some("openrouter-value"),
        "the profile's env vars must be merged into the resolver's env_overrides"
    );

    let _ = std::fs::remove_dir_all(&root);
}

/// A fake conflict resolver that clears exactly one conflicted file per
/// invocation (call 1: `conflict.txt`, call 2 and later: `other.txt`),
/// leaving the rest of the conflict markers untouched and never emitting
/// `RALPHUS_STAGE: DONE`. This forces `resolve_conflicts_with_agent`'s
/// fallback path to re-invoke the resolver for the same still-conflicting
/// commit, with a strictly smaller marker count on the second pass --
/// exercising the fresh-every-iteration `found` recompute (RAL-144) without
/// depending on git's rebase auto-continuing straight through a second,
/// separately-conflicting commit (which it does within a single
/// `rebase --continue` and which this orchestrator's blind
/// `is_err() -> --skip` fallback cannot currently pause on).
struct PartialResolutionRunner {
    calls: Arc<Mutex<u32>>,
}
impl Runner for PartialResolutionRunner {
    fn run(&self, spec: &RunnerSpec) -> RunnerResult {
        // run_merge also invokes the runner for non-conflict tasks (change
        // summary, manual-commands generation) -- only count/act on actual
        // conflict-resolver invocations.
        if spec.task == "resolve" {
            let call = {
                let mut n = self.calls.lock().unwrap();
                *n += 1;
                *n
            };
            let target = if call == 1 {
                "conflict.txt"
            } else {
                "other.txt"
            };
            let path = PathBuf::from(&spec.cwd).join(target);
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
        RunnerResult {
            status: "done".into(),
            tokens_in: 0,
            tokens_out: 0,
            cache_creation_tokens: 0,
            cache_read_tokens: 0,
            cost_usd: 0.0,
            cost_is_estimated: false,
            summary: "resolved".into(),
            error: None,
            proofed: None,
            agent_session_id: None,
            ghost: None,
        }
    }
}

#[test]
fn conflict_counters_are_rescoped_across_resolver_passes() {
    // RAL-144: `found` must be recomputed fresh every loop iteration instead
    // of staying frozen at the marker count seeded before the loop started,
    // and `committed` must reset when the rebase advances past a resolved
    // commit instead of accumulating for the life of the branch's rebase.
    //
    // A single feature/y commit conflicts on two files at once (2 marker
    // blocks total). PartialResolutionRunner clears only one file per
    // invocation, so the fallback path re-invokes it for a second pass with
    // a strictly smaller remaining-marker count (1) before finally staging
    // and advancing. Against the pre-fix code, `found` stays frozen at the
    // pre-loop seed (2) and `committed` is never reset after the advance
    // (staying at 1); the fix yields found=1 (the last fresh recompute) and
    // committed=0 (reset once the branch's rebase finishes advancing).
    let root = temp_repo();
    init_repo(&root);
    write(&root, "conflict.txt", "line1\nBASE\nline3\n");
    write(&root, "other.txt", "a\nBASE1\nb\n");
    git(&root, &["add", "."]);
    git(&root, &["commit", "-m", "base"]);

    // feature/x changes both files in one commit -- feature/y's single
    // commit (also changing both files) conflicts on both simultaneously.
    git(&root, &["checkout", "-b", "feature/x"]);
    write(&root, "conflict.txt", "line1\nX\nline3\n");
    write(&root, "other.txt", "a\nX1\nb\n");
    git(&root, &["commit", "-am", "x"]);
    git(&root, &["checkout", "main"]);

    git(&root, &["checkout", "-b", "feature/y"]);
    write(&root, "conflict.txt", "line1\nY\nline3\n");
    write(&root, "other.txt", "a\nY1\nb\n");
    git(&root, &["commit", "-am", "y"]);
    git(&root, &["checkout", "main"]);

    let store = Arc::new(Mutex::new(Store::open_in_memory().unwrap()));
    let id = {
        let g = store.lock().unwrap();
        let id = g
            .create_guardian("counter-rescope", "main", root.to_str().unwrap())
            .unwrap();
        g.add_guardian_branch(&id, "feature/x").unwrap();
        g.add_guardian_branch(&id, "feature/y").unwrap();
        id
    };

    let calls = Arc::new(Mutex::new(0u32));
    let runner = PartialResolutionRunner {
        calls: calls.clone(),
    };
    run_merge(&store, &runner, &id);

    assert_eq!(
        *calls.lock().unwrap(),
        2,
        "resolver must be invoked twice: once per file, one file per pass"
    );

    let view = store.lock().unwrap().get_guardian(&id).unwrap();
    assert_eq!(view.status, "in_review", "detail: {:?}", view.detail);

    let y = view
        .branches
        .iter()
        .find(|b| b.branch == "feature/y")
        .unwrap();
    assert_eq!(y.merge_status, "conflict_resolved");
    // A frozen `found` would still read 2 (the pre-loop seed, from before
    // the first file was cleared).
    assert_eq!(
        y.conflicts_found,
        Some(1),
        "found must be recomputed fresh on the final pass, not frozen at the pre-loop seed"
    );
    // An un-reset `committed` would read 1 (never reset after the advance
    // that finished the branch's rebase).
    assert_eq!(
        y.conflicts_committed,
        Some(0),
        "committed must reset once the rebase advances past the resolved commit"
    );

    let _ = std::fs::remove_dir_all(&root);
}

/// A fake conflict resolver that never actually clears the conflict markers
/// (every `run` call is a no-op on disk) but always reports `done`, and hands
/// back a distinct `agent_session_id` each call -- standing in for an agent
/// that keeps trying but never converges. Used to exercise the per-commit
/// give-up budget: a commit stuck behind this runner must fail after exactly
/// `MAX_ATTEMPTS_PER_COMMIT` (2) passes, not run away for 32 like the old flat
/// per-branch cap.
struct NeverResolvesRunner {
    specs: Arc<Mutex<Vec<RunnerSpec>>>,
    calls: Arc<Mutex<u32>>,
}
impl Runner for NeverResolvesRunner {
    fn run(&self, spec: &RunnerSpec) -> RunnerResult {
        // Other branches in the same merge (e.g. a cleanly-rebasing one still
        // proved per RAL-168) call this runner too and would otherwise steal
        // session numbers out from under the "resolve" sequence this test
        // asserts on -- so only "resolve" calls consume the shared counter.
        let is_resolve = spec.task == "resolve";
        if is_resolve {
            self.specs.lock().unwrap().push(spec.clone());
        }
        let agent_session_id = if is_resolve {
            let mut n = self.calls.lock().unwrap();
            *n += 1;
            Some(format!("sess-{n}"))
        } else {
            Some("sess-other-task".to_string())
        };
        RunnerResult {
            status: "done".into(),
            tokens_in: 0,
            tokens_out: 0,
            cache_creation_tokens: 0,
            cache_read_tokens: 0,
            cost_usd: 0.0,
            cost_is_estimated: false,
            summary: "still conflicted".into(),
            error: None,
            proofed: None,
            agent_session_id,
            ghost: None,
        }
    }
}

#[test]
fn stuck_commit_resumes_the_session_and_gives_up_after_two_attempts() {
    // A single conflicting commit that the resolver never actually clears
    // must: (1) get exactly MAX_ATTEMPTS_PER_COMMIT (2) resolution passes, not
    // the old flat 32-iteration budget, and (2) resume the same agent session
    // on the retry instead of starting cold.
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
            .create_guardian("stuck-commit-check", "main", root.to_str().unwrap())
            .unwrap();
        g.add_guardian_branch(&id, "feature/x").unwrap();
        g.add_guardian_branch(&id, "feature/y").unwrap();
        id
    };

    let specs: Arc<Mutex<Vec<RunnerSpec>>> = Arc::new(Mutex::new(Vec::new()));
    let calls = Arc::new(Mutex::new(0u32));
    let runner = NeverResolvesRunner {
        specs: specs.clone(),
        calls: calls.clone(),
    };
    run_merge(&store, &runner, &id);

    let specs = specs.lock().unwrap();
    assert_eq!(
        specs.len(),
        2,
        "a permanently stuck commit must give up after exactly \
         MAX_ATTEMPTS_PER_COMMIT (2) passes, not the old flat 32-iteration budget: {specs:#?}"
    );
    assert_eq!(
        specs[0].resume_agent_session_id, None,
        "the first pass on a commit must start a fresh session"
    );
    assert_eq!(
        specs[1].resume_agent_session_id.as_deref(),
        Some("sess-1"),
        "the retry must resume the first pass's own agent session instead of starting cold"
    );

    let view = store.lock().unwrap().get_guardian(&id).unwrap();
    let y = view
        .branches
        .iter()
        .find(|b| b.branch == "feature/y")
        .unwrap();
    assert_eq!(
        y.merge_status,
        MergeStatus::Failed.as_str(),
        "detail: {:?}",
        y.detail
    );
    assert!(
        y.detail
            .as_deref()
            .unwrap_or_default()
            .contains("exhausted its attempt budget on this commit"),
        "detail must explain the per-commit give-up, not a generic failure: {:?}",
        y.detail
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

// RAL-330: a resolution that drops real, unrelated content must never be
// silently accepted, whether it comes from `git rerere`'s fast path or (as
// exercised here, deterministically) from the resolver's own pass.
#[test]
fn resolver_content_loss_is_detected_and_the_branch_is_not_silently_finished() {
    let root = temp_repo();
    init_repo(&root);

    write(&root, "conflict.txt", "line1\nBASE\nline3\n");
    git(&root, &["add", "."]);
    git(&root, &["commit", "-m", "base"]);

    // feature/x: BASE → X
    git(&root, &["checkout", "-b", "feature/x"]);
    write(&root, "conflict.txt", "line1\nX\nline3\n");
    git(&root, &["commit", "-am", "x"]);
    git(&root, &["checkout", "main"]);

    // feature/y: conflicts with feature/x on conflict.txt, AND in the SAME
    // commit adds a brand-new file the base never touches -- the shape that
    // must survive a resolution intact regardless of how the conflict itself
    // gets fixed.
    git(&root, &["checkout", "-b", "feature/y"]);
    write(&root, "conflict.txt", "line1\nY\nline3\n");
    write(&root, "new_in_y.txt", "brand new content from y\n");
    git(&root, &["add", "."]);
    git(&root, &["commit", "-m", "y"]);
    git(&root, &["checkout", "main"]);

    let store = Arc::new(Mutex::new(Store::open_in_memory().unwrap()));
    let id = {
        let g = store.lock().unwrap();
        let gid = g
            .create_guardian("lossy", "main", root.to_str().unwrap())
            .unwrap();
        g.add_guardian_branch(&gid, "feature/x").unwrap();
        g.add_guardian_branch(&gid, "feature/y").unwrap();
        gid
    };
    run_merge(&store, &LossyRunner, &id);

    let view = store.lock().unwrap().get_guardian(&id).unwrap();
    assert_eq!(
        view.status, "merge_failed",
        "a resolution that drops real content must not silently reach in_review; detail: {:?}",
        view.detail
    );

    let y = view
        .branches
        .iter()
        .find(|b| b.branch == "feature/y")
        .unwrap();
    assert_ne!(
        y.merge_status, "conflict_resolved",
        "feature/y must not be reported resolved when new_in_y.txt was silently dropped"
    );

    let _ = std::fs::remove_dir_all(&root);
}

// RAL-92: a reviewer manually commits into an upstream branch's review worktree.
// A maintenance sweep must detect the moved review-branch ref, rebase the
// downstream branch onto the manual commit (one at a time, as "Merge / rebase"
// does), and rebuild the combined worktree — then leave a stable baseline so a
// second sweep does nothing.
#[test]
fn manual_push_to_review_worktree_rebases_downstream() {
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
            .create_guardian("manual-push", "main", root.to_str().unwrap())
            .unwrap();
        g.add_guardian_branch(&id, "feature/a").unwrap();
        g.add_guardian_branch(&id, "feature/b").unwrap();
        id
    };
    run_merge(&store, &NoopRunner, &id);

    // Right after the build, a sweep must NOT see a manual push (the daemon's own
    // writes were baselined) — this is the feedback-loop guard.
    let sem = Semaphore::new(4);
    assert!(
        !rebase_on_manual_push(&store, &NoopRunner, &id, &sem),
        "the daemon's own build must not be mistaken for a manual push"
    );

    // Reviewer opens a terminal in branch 0's review worktree and commits a fix.
    let view = store.lock().unwrap().get_guardian(&id).unwrap();
    let wt0 = PathBuf::from(view.branches[0].worktree.as_deref().expect("worktree"));
    write(&wt0, "manual.txt", "reviewer fix\n");
    git(&wt0, &["add", "-A"]);
    git(&wt0, &["commit", "-m", "manual reviewer fix"]);

    // The sweep detects it and rebases downstream onto the manual commit.
    assert!(
        rebase_on_manual_push(&store, &NoopRunner, &id, &sem),
        "a manual push to the review worktree must trigger a downstream rebase"
    );

    let after = store.lock().unwrap().get_guardian(&id).unwrap();
    assert_eq!(after.status, "in_review", "detail: {:?}", after.detail);

    // The downstream review branch and the combined worktree carry the manual
    // commit stacked under branch b's own change.
    let rev1 = after.branches[1].review_branch.clone().unwrap();
    let files1 = git(&root, &["ls-tree", "-r", "--name-only", &rev1]);
    assert!(
        files1.contains("manual.txt") && files1.contains("a.txt") && files1.contains("b.txt"),
        "downstream branch must be rebased onto the manual commit: {files1}"
    );
    let combined = after.combined_worktree.as_deref().expect("combined");
    assert!(Path::new(combined).join("manual.txt").exists());
    assert!(Path::new(combined).join("b.txt").exists());

    // A second sweep is a no-op: the baseline advanced to the new tips.
    assert!(
        !rebase_on_manual_push(&store, &NoopRunner, &id, &sem),
        "no rebase when nothing moved since the last build"
    );

    let _ = std::fs::remove_dir_all(&root);
}

// RAL-92: a manual push to the LAST branch (no downstream) still refreshes the
// combined review worktree so the reviewer's commit is reflected there.
#[test]
fn manual_push_to_last_branch_refreshes_combined() {
    let (root, store, id) = single_feature_repo();
    run_merge(&store, &NoopRunner, &id);

    let view = store.lock().unwrap().get_guardian(&id).unwrap();
    let wt0 = PathBuf::from(view.branches[0].worktree.as_deref().expect("worktree"));
    write(&wt0, "manual.txt", "reviewer fix\n");
    git(&wt0, &["add", "-A"]);
    git(&wt0, &["commit", "-m", "manual reviewer fix"]);

    let sem = Semaphore::new(4);
    assert!(
        rebase_on_manual_push(&store, &NoopRunner, &id, &sem),
        "a manual push must trigger a rebuild even with no downstream branch"
    );

    let after = store.lock().unwrap().get_guardian(&id).unwrap();
    assert_eq!(after.status, "in_review", "detail: {:?}", after.detail);
    let combined = after.combined_worktree.as_deref().expect("combined");
    assert!(
        Path::new(combined).join("manual.txt").exists(),
        "combined worktree must reflect the manual commit"
    );

    let _ = std::fs::remove_dir_all(&root);
}

// RAL-103: a manual push is a forced regeneration -- the stale manual-checks
// commands from the previous build must be cleared for the duration of the
// restack, not left showing as current (which would make `checks_state`
// report "ready" with out-of-date commands while a rebuild is actually
// happening underneath).
#[test]
fn manual_push_clears_stale_manual_commands() {
    let (root, store, id) = single_feature_repo();
    run_merge(&store, &NoopRunner, &id);

    // Seed stale commands as if a prior generation had succeeded.
    store
        .lock()
        .unwrap()
        .set_guardian_manual_commands(
            &id,
            &[GuardianCheck {
                label: None,
                command: Some("echo stale".to_string()),
                prompt: None,
                cleanup_command: None,
                inputs: vec![],
            }],
            None,
            None,
        )
        .unwrap();
    assert_eq!(
        store
            .lock()
            .unwrap()
            .get_guardian(&id)
            .unwrap()
            .checks_state,
        "ready"
    );

    let view = store.lock().unwrap().get_guardian(&id).unwrap();
    let wt0 = PathBuf::from(view.branches[0].worktree.as_deref().expect("worktree"));
    write(&wt0, "manual.txt", "reviewer fix\n");
    git(&wt0, &["add", "-A"]);
    git(&wt0, &["commit", "-m", "manual reviewer fix"]);

    let sem = Semaphore::new(4);
    assert!(rebase_on_manual_push(&store, &NoopRunner, &id, &sem));

    // NoopRunner fails every call, so `generate_manual_commands` cannot have
    // repopulated the list -- if it's empty, the stale entry was cleared.
    let after = store.lock().unwrap().get_guardian(&id).unwrap();
    assert!(
        after.manual_commands.is_empty(),
        "stale manual commands must not survive a forced regeneration: {:?}",
        after.manual_commands
    );
    assert_eq!(after.checks_state, "waiting");

    let _ = std::fs::remove_dir_all(&root);
}

// RAL-92: when a manual push introduces a change that conflicts with a downstream
// branch, the restack must route the conflict through the existing agent
// conflict-resolution path (not fail outright).
#[test]
fn manual_push_conflict_flows_through_resolver() {
    let root = temp_repo();
    init_repo(&root);
    write(&root, "conflict.txt", "line1\nBASE\nline3\n");
    git(&root, &["add", "."]);
    git(&root, &["commit", "-m", "base"]);

    // feature/a leaves conflict.txt alone (adds an unrelated file), so the initial
    // stack builds cleanly.
    git(&root, &["checkout", "-b", "feature/a"]);
    write(&root, "a.txt", "from a\n");
    git(&root, &["add", "."]);
    git(&root, &["commit", "-m", "add a"]);
    git(&root, &["checkout", "main"]);

    // feature/b changes the middle line to Y.
    git(&root, &["checkout", "-b", "feature/b"]);
    write(&root, "conflict.txt", "line1\nY\nline3\n");
    git(&root, &["commit", "-am", "y"]);
    git(&root, &["checkout", "main"]);

    let store = Arc::new(Mutex::new(Store::open_in_memory().unwrap()));
    let id = {
        let g = store.lock().unwrap();
        let id = g
            .create_guardian("manual-conflict", "main", root.to_str().unwrap())
            .unwrap();
        g.add_guardian_branch(&id, "feature/a").unwrap();
        g.add_guardian_branch(&id, "feature/b").unwrap();
        id
    };
    // Initial build is clean (feature/a and feature/b touch different lines/files).
    run_merge(&store, &NoopRunner, &id);
    assert_eq!(
        store.lock().unwrap().get_guardian(&id).unwrap().status,
        "in_review"
    );

    // Reviewer manually changes the SAME middle line to X on feature/a's review
    // worktree — this now conflicts with feature/b's Y when restacked.
    let view = store.lock().unwrap().get_guardian(&id).unwrap();
    let wt0 = PathBuf::from(view.branches[0].worktree.as_deref().expect("worktree"));
    write(&wt0, "conflict.txt", "line1\nX\nline3\n");
    git(&wt0, &["add", "-A"]);
    git(&wt0, &["commit", "-m", "manual conflicting edit"]);

    // The resolver (marker-stripping) must be driven to resolve the downstream
    // conflict; the merge reaches review rather than failing.
    let sem = Semaphore::new(4);
    assert!(rebase_on_manual_push(
        &store,
        &MarkerStrippingRunner,
        &id,
        &sem
    ));

    let after = store.lock().unwrap().get_guardian(&id).unwrap();
    assert_eq!(after.status, "in_review", "detail: {:?}", after.detail);
    let b = after
        .branches
        .iter()
        .find(|b| b.branch == "feature/b")
        .unwrap();
    assert_eq!(
        b.merge_status, "conflict_resolved",
        "downstream conflict must be resolved by the agent"
    );
    let review = after.review_branch.expect("review branch");
    let show = git(&root, &["show", &format!("{review}:conflict.txt")]);
    assert!(!show.contains("<<<<<<<"), "markers remain: {show}");
    assert!(
        show.contains('X') && show.contains('Y'),
        "both sides kept: {show}"
    );

    let _ = std::fs::remove_dir_all(&root);
}

/// RAL-137: disabling ("dropping") a branch while its review is still
/// Collecting -- e.g. because the contributing task is stuck -- lets the
/// review proceed immediately using only the remaining enabled branches. The
/// dropped branch is not removed: it keeps its stack position and is reset to
/// a clean `pending` state (RAL-43) so re-enabling it later and re-running the
/// merge rebuilds it back into its original place in the stack, between the
/// branches that surround it.
#[test]
fn disabled_branch_is_skipped_and_reintroduced_at_its_original_position() {
    let root = temp_repo();
    init_repo(&root);
    write(&root, "base.txt", "base\n");
    git(&root, &["add", "."]);
    git(&root, &["commit", "-m", "base"]);

    for (branch, file) in [
        ("feature/a", "a.txt"),
        ("feature/b", "b.txt"),
        ("feature/c", "c.txt"),
    ] {
        git(&root, &["checkout", "main"]);
        git(&root, &["checkout", "-b", branch]);
        write(&root, file, "content\n");
        git(&root, &["add", "."]);
        git(&root, &["commit", "-m", &format!("add {file}")]);
    }
    git(&root, &["checkout", "main"]);

    let store = Arc::new(Mutex::new(Store::open_in_memory().unwrap()));
    let id = {
        let g = store.lock().unwrap();
        let id = g
            .create_guardian("drop-branch", "main", root.to_str().unwrap())
            .unwrap();
        g.add_guardian_branch(&id, "feature/a").unwrap();
        g.add_guardian_branch(&id, "feature/b").unwrap();
        g.add_guardian_branch(&id, "feature/c").unwrap();
        id
    };

    // feature/b's task is struggling; drop its branch while the review is
    // still Collecting and the other two branches' tasks may still be
    // running too -- run_merge does not require branches to be "done", only
    // enabled, so this exercises the same immediate-proceed path a real
    // Collecting review takes once its remaining blocking tasks finish.
    {
        let g = store.lock().unwrap();
        assert_eq!(g.get_guardian(&id).unwrap().status, "collecting");
        g.set_branch_enabled_by_name(&id, "feature/b", false)
            .unwrap();
    }

    run_merge(&store, &NoopRunner, &id);

    let view = store.lock().unwrap().get_guardian(&id).unwrap();
    assert_eq!(
        view.status, "in_review",
        "review must proceed without the dropped branch: {:?}",
        view.detail
    );
    let review = view.review_branch.clone().expect("review branch set");
    let files = git(&root, &["ls-tree", "-r", "--name-only", &review]);
    assert!(
        files.contains("a.txt") && files.contains("c.txt"),
        "{files}"
    );
    assert!(
        !files.contains("b.txt"),
        "dropped branch's content must not be in the review: {files}"
    );

    // The dropped branch keeps its position and a clean pending state instead
    // of being removed or left showing stale done/failed status.
    let b = view
        .branches
        .iter()
        .find(|b| b.branch == "feature/b")
        .expect("dropped branch retained in the stack");
    assert_eq!(b.position, 1, "dropped branch retains its stack position");
    assert!(!b.enabled);
    assert_eq!(b.merge_status, "pending");
    assert!(b.review_branch.is_none());
    assert!(b.worktree.is_none());

    // Re-enable the branch and re-run the merge: it must be rebuilt back into
    // its original place in the stack (between a and c), not appended at the
    // end.
    store
        .lock()
        .unwrap()
        .set_branch_enabled_by_name(&id, "feature/b", true)
        .unwrap();
    run_merge(&store, &NoopRunner, &id);

    let after = store.lock().unwrap().get_guardian(&id).unwrap();
    assert_eq!(after.status, "in_review", "detail: {:?}", after.detail);
    let review2 = after.review_branch.clone().expect("review branch set");
    let files2 = git(&root, &["ls-tree", "-r", "--name-only", &review2]);
    assert!(
        files2.contains("a.txt") && files2.contains("b.txt") && files2.contains("c.txt"),
        "{files2}"
    );

    let subjects = git(
        &root,
        &[
            "log",
            "--format=%s",
            "--reverse",
            &format!("main..{review2}"),
        ],
    );
    let subjects: Vec<&str> = subjects.lines().collect();
    assert_eq!(
        subjects,
        vec!["add a.txt", "add b.txt", "add c.txt"],
        "re-enabled branch must be rebuilt between a and c, at its original position: {subjects:?}"
    );

    let b2 = after
        .branches
        .iter()
        .find(|b| b.branch == "feature/b")
        .expect("branch b retained");
    assert_eq!(
        b2.position, 1,
        "position unchanged across disable/re-enable"
    );
    assert!(b2.enabled);
    assert_eq!(b2.merge_status, "done");

    let _ = std::fs::remove_dir_all(&root);
}

/// RAL-118: moving a branch from one review into another must recompute the
/// stacked rebase correctly on both sides -- the source renumbers and rebuilds
/// without the moved branch, and the destination rebases it into its own
/// stack (on top of its own base/previous branch), not anything left over
/// from its original review. This is also the git-level guarantee PR
/// submission (RAL-117, `pr.rs::submit_pull_requests`) depends on: it reads a
/// branch's `review_branch` ref and the owning guardian's own `base_branch`
/// with no special-casing for a moved branch, so if the physical rebase here
/// is correct, PR base resolution is correct too.
#[test]
fn move_branch_rebuilds_correctly_in_both_source_and_destination() {
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
    git(&root, &["checkout", "-b", "feature/c"]);
    write(&root, "c.txt", "from c\n");
    git(&root, &["add", "."]);
    git(&root, &["commit", "-m", "add c"]);
    git(&root, &["checkout", "main"]);

    let store = Arc::new(Mutex::new(Store::open_in_memory().unwrap()));
    let (r1, r2) = {
        let g = store.lock().unwrap();
        let r1 = g
            .create_guardian("r1", "main", root.to_str().unwrap())
            .unwrap();
        g.add_guardian_branch(&r1, "feature/a").unwrap();
        g.add_guardian_branch(&r1, "feature/b").unwrap();
        let r2 = g
            .create_guardian("r2", "main", root.to_str().unwrap())
            .unwrap();
        g.add_guardian_branch(&r2, "feature/c").unwrap();
        (r1, r2)
    };

    // Build both stacks once before the move, same as any real workflow.
    run_merge(&store, &NoopRunner, &r1);
    run_merge(&store, &NoopRunner, &r2);
    assert_eq!(
        store.lock().unwrap().get_guardian(&r1).unwrap().status,
        "in_review"
    );
    assert_eq!(
        store.lock().unwrap().get_guardian(&r2).unwrap().status,
        "in_review"
    );

    // Move feature/a (r1 position 0) into r2.
    let bid_a = store.lock().unwrap().get_guardian(&r1).unwrap().branches[0]
        .id
        .clone();
    let new_pos = store
        .lock()
        .unwrap()
        .move_guardian_branch(&r1, &bid_a, &r2)
        .unwrap();
    assert_eq!(new_pos, 1, "appended after r2's existing feature/c");

    // Same carry-forward purge the HTTP handler (`server::guardian_move_branch`)
    // performs on the source guardian before rebuilding it.
    purge_worktrees(&store, root.to_str().unwrap(), &r1);

    // Rebuild both, exactly as the HTTP handler does.
    run_merge(&store, &NoopRunner, &r1);
    run_merge(&store, &NoopRunner, &r2);

    // Source (r1): only feature/b remains, renumbered to position 0, and its
    // review branch no longer contains feature/a's file.
    let after_r1 = store.lock().unwrap().get_guardian(&r1).unwrap();
    assert_eq!(
        after_r1.status, "in_review",
        "detail: {:?}",
        after_r1.detail
    );
    assert_eq!(after_r1.branches.len(), 1);
    assert_eq!(after_r1.branches[0].branch, "feature/b");
    assert_eq!(after_r1.branches[0].position, 0);
    let r1_review = after_r1.review_branch.expect("r1 review branch");
    let r1_files = git(&root, &["ls-tree", "-r", "--name-only", &r1_review]);
    assert!(r1_files.contains("b.txt"));
    assert!(
        !r1_files.contains("a.txt"),
        "r1's rebuilt stack must not carry the moved-out branch's file: {r1_files}"
    );

    // Destination (r2): feature/c then feature/a, correctly stacked on r2's
    // own base -- not on anything from r1.
    let after_r2 = store.lock().unwrap().get_guardian(&r2).unwrap();
    assert_eq!(
        after_r2.status, "in_review",
        "detail: {:?}",
        after_r2.detail
    );
    assert_eq!(after_r2.branches.len(), 2);
    assert_eq!(after_r2.branches[0].branch, "feature/c");
    assert_eq!(after_r2.branches[1].branch, "feature/a");
    assert_eq!(after_r2.branches[1].position, 1);
    assert_eq!(
        after_r2.branches[1].moved_from_guardian_id.as_deref(),
        Some(r1.as_str()),
        "provenance must record the original owning review"
    );
    assert!(after_r2.branches.iter().all(|b| b.merge_status == "done"));
    let r2_review = after_r2.review_branch.expect("r2 review branch");
    let r2_files = git(&root, &["ls-tree", "-r", "--name-only", &r2_review]);
    assert!(
        r2_files.contains("base.txt") && r2_files.contains("c.txt") && r2_files.contains("a.txt")
    );

    // The moved branch's own stacked commit -- exactly what PR submission
    // would push and diff against r2.base_branch -- is reachable from r2's
    // base and contains only its own file plus what came before it in r2's
    // stack (c.txt), never b.txt (which stayed behind in r1).
    let position_1_tip = after_r2.branches[1]
        .review_branch
        .clone()
        .expect("position 1 review ref");
    let base_sha = git(&root, &["rev-parse", "main"]).trim().to_string();
    let is_ancestor = Command::new("git")
        .args(["merge-base", "--is-ancestor", &base_sha, &position_1_tip])
        .current_dir(&root)
        .status()
        .expect("run git")
        .success();
    assert!(
        is_ancestor,
        "the moved branch's stacked commit must descend from r2's own base"
    );
    let position_1_files = git(&root, &["ls-tree", "-r", "--name-only", &position_1_tip]);
    assert!(position_1_files.contains("a.txt") && position_1_files.contains("c.txt"));
    assert!(
        !position_1_files.contains("b.txt"),
        "the moved branch must not carry r1-only history: {position_1_files}"
    );

    let _ = std::fs::remove_dir_all(&root);
}

// ── RAL-124: bullet-format change summary, live Ollama ──────────────────────

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

/// A real end-to-end run of a two-branch stack (ticket-shaped branch names,
/// one project) through a live ollama resolver, asserting the produced
/// `change_summary` is a bullet list with one bullet per branch (RAL-124
/// default format), each labelled with the branch's extracted ticket id.
/// Skips unless ollama + a model + `ralphus-runner` are all available locally.
#[test]
#[ignore = "calls a live local Ollama model; run explicitly with `cargo test -- --ignored`"]
fn generate_summary_live_ollama_produces_one_bullet_per_branch() {
    let Some(runner_cmd) = find_runner() else {
        eprintln!(
            "SKIP generate_summary_live_ollama: ralphus-runner not found (set RALPHUS_RUNNER_CMD)"
        );
        return;
    };
    if !pydantic_ai_available(&runner_cmd) {
        eprintln!(
            "SKIP generate_summary_live_ollama: pydantic-ai not installed in runner environment (run `uv sync --extra runner` in cli/)"
        );
        return;
    }
    if !ollama_up() {
        eprintln!("SKIP generate_summary_live_ollama: ollama not reachable on 127.0.0.1:11434");
        return;
    }
    let model = std::env::var("RALPHUS_RESOLVER_MODEL").unwrap_or_else(|_| "qwen3:8b".to_string());
    if !ollama_has_model(&model) {
        eprintln!("SKIP generate_summary_live_ollama: ollama model '{model}' not pulled");
        return;
    }

    let root = temp_repo();
    init_repo(&root);
    write(&root, "base.txt", "base\n");
    git(&root, &["add", "."]);
    git(&root, &["commit", "-m", "base"]);

    git(&root, &["checkout", "-b", "RAL-201-add-widget"]);
    write(&root, "widget.txt", "widget\n");
    git(&root, &["add", "."]);
    git(&root, &["commit", "-m", "Add the widget feature"]);

    git(&root, &["checkout", "main"]);
    git(&root, &["checkout", "-b", "RAL-202-fix-gadget"]);
    write(&root, "gadget.txt", "gadget\n");
    git(&root, &["add", "."]);
    git(&root, &["commit", "-m", "Fix a bug in the gadget"]);
    git(&root, &["checkout", "main"]);

    let store = Arc::new(Mutex::new(Store::open_in_memory().unwrap()));
    let id = {
        let g = store.lock().unwrap();
        let id = g
            .create_guardian("review", "main", root.to_str().unwrap())
            .unwrap();
        g.add_guardian_branch(&id, "RAL-201-add-widget").unwrap();
        g.add_guardian_branch(&id, "RAL-202-fix-gadget").unwrap();
        id
    };

    let runner = ralphus_daemon::runner::SubprocessRunner::new(&runner_cmd);
    run_merge(&store, &runner, &id);

    let view = store.lock().unwrap().get_guardian(&id).unwrap();
    assert_eq!(view.status, "in_review", "detail: {:?}", view.detail);
    let summary = view
        .change_summary
        .expect("live ollama should have produced a change summary");
    eprintln!("generated change summary:\n{summary}");

    let bullet_lines: Vec<&str> = summary
        .lines()
        .map(str::trim)
        .filter(|l| l.starts_with('-'))
        .collect();
    assert!(
        bullet_lines.len() >= 2,
        "expected at least one bullet per branch, got: {summary:?}"
    );
    assert!(
        summary.contains("RAL-201") && summary.contains("RAL-202"),
        "each bullet should be labelled with its branch's ticket id: {summary:?}"
    );

    let _ = std::fs::remove_dir_all(&root);
}

// ---------------------------------------------------------------------------
// RAL-190: PR bidirectional sync -- pulling reviewer-pushed PR commits back
// into the review worktree.
// ---------------------------------------------------------------------------

/// A reviewer pushing a fix directly to the open PR branch (rather than
/// leaving a comment) must flow back into the review worktree, and the
/// downstream branch in the stack must be restacked on top of it -- the same
/// guarantee `rebase_on_manual_push` gives for a *local* manual push, now for
/// the *remote* PR branch.
#[test]
fn pull_pr_commits_rebases_reviewer_pushed_commits_and_restacks_downstream() {
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
    let before = store.lock().unwrap().get_guardian(&id).unwrap();
    assert_eq!(before.status, "in_review", "detail: {:?}", before.detail);
    let branch_a = before
        .branches
        .iter()
        .find(|b| b.branch == "feature/a")
        .unwrap();
    let branch_a_id = branch_a.id.clone();
    let review_a = branch_a.review_branch.clone().expect("branch a built");
    let last_synced_sha = git(&root, &["rev-parse", &review_a]).trim().to_string();

    // A bare "remote" and the PR branch pushed to it, exactly as
    // `pr::submit_pull_requests` would.
    let remote_dir = temp_repo();
    git(&remote_dir, &["init", "--bare"]);
    let remote = remote_dir.to_str().unwrap();
    git(
        &root,
        &["push", remote, &format!("{review_a}:refs/heads/pr-a")],
    );

    // A reviewer pushes a fix straight to the open PR branch.
    let reviewer_clone = temp_repo();
    let _ = std::fs::remove_dir_all(&reviewer_clone);
    git(
        reviewer_clone.parent().unwrap(),
        &[
            "clone",
            remote,
            reviewer_clone.file_name().unwrap().to_str().unwrap(),
        ],
    );
    git(&reviewer_clone, &["checkout", "pr-a"]);
    write(&reviewer_clone, "reviewer.txt", "fixed by reviewer\n");
    git(&reviewer_clone, &["add", "."]);
    git(&reviewer_clone, &["commit", "-m", "reviewer fix"]);
    git(&reviewer_clone, &["push", "origin", "pr-a"]);

    let pulled = pull_pr_commits(
        &store,
        &NoopRunner,
        &id,
        &branch_a_id,
        remote,
        "pr-a",
        Some(&last_synced_sha),
    )
    .expect("pull_pr_commits should succeed");
    assert!(pulled, "reviewer's commit should have been pulled");

    let after = store.lock().unwrap().get_guardian(&id).unwrap();
    assert_eq!(after.status, "in_review", "detail: {:?}", after.detail);
    let a2 = after.branches.iter().find(|b| b.id == branch_a_id).unwrap();
    let review_a2 = a2.review_branch.clone().unwrap();
    let a_files = git(&root, &["ls-tree", "-r", "--name-only", &review_a2]);
    assert!(
        a_files.contains("reviewer.txt"),
        "branch a's review branch should carry the reviewer's fix: {a_files}"
    );

    // Downstream branch b was restacked on top of a's new tip, so it still
    // carries a's content (including the reviewer's fix) plus its own.
    let b = after
        .branches
        .iter()
        .find(|b| b.branch == "feature/b")
        .unwrap();
    assert_eq!(b.merge_status, "done", "detail: {:?}", b.detail);
    let review_b = b.review_branch.clone().expect("branch b restacked");
    let b_files = git(&root, &["ls-tree", "-r", "--name-only", &review_b]);
    assert!(
        b_files.contains("reviewer.txt") && b_files.contains("a.txt") && b_files.contains("b.txt"),
        "branch b should be restacked on top of a's new content: {b_files}"
    );

    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_dir_all(&remote_dir);
    let _ = std::fs::remove_dir_all(&reviewer_clone);
}

/// Nothing to pull when the PR branch's tip is already contained in the
/// review worktree's history (no reviewer push happened).
#[test]
fn pull_pr_commits_is_a_noop_when_already_up_to_date() {
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
            .create_guardian("review", "main", root.to_str().unwrap())
            .unwrap();
        g.add_guardian_branch(&id, "feature/a").unwrap();
        id
    };
    run_merge(&store, &NoopRunner, &id);
    let g = store.lock().unwrap().get_guardian(&id).unwrap();
    let branch_a = g.branches.iter().find(|b| b.branch == "feature/a").unwrap();
    let branch_a_id = branch_a.id.clone();
    let review_a = branch_a.review_branch.clone().unwrap();

    let remote_dir = temp_repo();
    git(&remote_dir, &["init", "--bare"]);
    let remote = remote_dir.to_str().unwrap();
    git(
        &root,
        &["push", remote, &format!("{review_a}:refs/heads/pr-a")],
    );
    let synced = git(&root, &["rev-parse", &review_a]).trim().to_string();

    let pulled = pull_pr_commits(
        &store,
        &NoopRunner,
        &id,
        &branch_a_id,
        remote,
        "pr-a",
        Some(&synced),
    )
    .expect("pull_pr_commits should succeed");
    assert!(!pulled, "nothing new on the PR branch to pull");

    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_dir_all(&remote_dir);
}

/// RAL-213: a guardian-settings change made while a merge is in flight must
/// actually stop the stale merge (not just flip a DB column underneath it)
/// and restart it, and the restarted attempt must pick up the new setting.
///
/// `feature/x` rebases cleanly; `feature/y` (stacked on top of `x`) conflicts
/// on the same line, routing it through `resolve_conflicts_with_agent`. The
/// fake runner blocks on the very first "resolve" call — standing in for a
/// long-running fix pass, the same way `scheduler.rs`'s own `BlockingRunner`
/// test stands in for a long-running session — so the test can deterministically
/// catch the merge stuck mid-resolve, flip a setting, and drive the restart
/// through [`restart_guardian_merge`] (the same function `guardian_settings`'s
/// HTTP handler now calls for this exact purpose). Every subsequent "resolve"
/// call behaves like the plain marker-stripping runner used elsewhere in this
/// file, so the restarted attempt actually completes.
///
/// `proof_scope` is the setting flipped mid-flight here, to "nothing": the
/// first (stuck) attempt never gets far enough to invoke "resolve-proof" for
/// `feature/y` at all (it is still blocked resolving markers), so the only
/// way this test's "no resolve-proof call ever ran" assertion can pass is if
/// the *restarted* attempt actually observed the new setting rather than the
/// "each_branch" default it started with.
#[test]
fn settings_change_restarts_a_stuck_merge_and_new_setting_takes_effect() {
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
            .create_guardian("cancellable merge", "main", root.to_str().unwrap())
            .unwrap();
        g.add_guardian_branch(&id, "feature/x").unwrap();
        g.add_guardian_branch(&id, "feature/y").unwrap();
        // RAL-168: keep the clean branch (feature/x) out of the picture so the
        // only "resolve-proof" call this test could ever observe is the one
        // gated behind feature/y's conflict resolution -- see the doc comment.
        g.set_guardian_proof_skip_auto_clean(&id, Some(true))
            .unwrap();
        id
    };

    struct BlockingThenResolvingRunner {
        resolve_started: Arc<AtomicBool>,
        resolve_calls: Arc<AtomicU32>,
        specs: Arc<Mutex<Vec<RunnerSpec>>>,
    }
    impl Runner for BlockingThenResolvingRunner {
        fn run(&self, spec: &RunnerSpec) -> RunnerResult {
            self.run_cancellable(spec, &CancelToken::never())
        }
        fn run_cancellable(&self, spec: &RunnerSpec, cancel: &CancelToken) -> RunnerResult {
            self.specs.lock().unwrap().push(spec.clone());
            if spec.task == "resolve" {
                let n = self.resolve_calls.fetch_add(1, Ordering::SeqCst);
                if n == 0 {
                    // First-ever resolve call: stand in for a long-running fix
                    // pass by blocking until the test's restart cancels it.
                    self.resolve_started.store(true, Ordering::SeqCst);
                    for _ in 0..1000 {
                        if cancel.is_cancelled() {
                            return RunnerResult::failure("cancelled");
                        }
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    return RunnerResult::failure("blocking runner was never cancelled");
                }
                // Restarted attempt's resolve call: strip conflict markers for
                // real, mirroring `MarkerStrippingRunner`.
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
            }
            RunnerResult {
                status: "done".into(),
                tokens_in: 0,
                tokens_out: 0,
                cache_creation_tokens: 0,
                cache_read_tokens: 0,
                cost_usd: 0.0,
                cost_is_estimated: false,
                summary: "resolved".into(),
                error: None,
                proofed: spec.proof.then_some(true),
                agent_session_id: None,
                ghost: None,
            }
        }
    }

    let resolve_started = Arc::new(AtomicBool::new(false));
    let resolve_calls = Arc::new(AtomicU32::new(0));
    let specs: Arc<Mutex<Vec<RunnerSpec>>> = Arc::new(Mutex::new(Vec::new()));
    let runner: Arc<dyn Runner> = Arc::new(BlockingThenResolvingRunner {
        resolve_started: Arc::clone(&resolve_started),
        resolve_calls: Arc::clone(&resolve_calls),
        specs: Arc::clone(&specs),
    });

    let cancellations = Cancellations::new();
    let sem = Arc::new(Semaphore::new(4));

    // Kick off the merge exactly the way `POST /api/guardians/{id}/merge` does.
    let reply = start_merge(
        Arc::clone(&store),
        Arc::clone(&runner),
        &id,
        Arc::clone(&sem),
        cancellations.clone(),
    );
    assert_eq!(reply.status, 202);

    // Wait until the merge is genuinely stuck mid-resolve (not just "started").
    // 24000 * 5ms = 120s: generous headroom for `cargo test --all-targets`,
    // where this repo's git-heavy tests all contend for CPU/IO at once and
    // the background merge thread can take much longer than in isolation to
    // reach the resolve call.
    for _ in 0..24000 {
        if resolve_started.load(Ordering::SeqCst) {
            break;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(
        resolve_started.load(Ordering::SeqCst),
        "merge never reached the blocking resolve call"
    );
    assert_eq!(
        store.lock().unwrap().get_guardian(&id).unwrap().status,
        "merging"
    );

    // The settings change: turn Proof off entirely, then trigger the same
    // restart path `guardian_settings`'s HTTP handler now calls whenever a
    // setting is changed while `status == "merging"`.
    store
        .lock()
        .unwrap()
        .set_guardian_proof_scope(&id, Some("nothing"))
        .unwrap();
    let restart_reply = restart_guardian_merge(
        Arc::clone(&store),
        cancellations.clone(),
        Arc::clone(&runner),
        &id,
        Arc::clone(&sem),
    );
    assert_eq!(
        restart_reply.status, 202,
        "restart must be accepted: {}",
        restart_reply.body
    );

    // Same 120s headroom as the wait above, for the same reason.
    let mut status = String::new();
    for _ in 0..24000 {
        status = store.lock().unwrap().get_guardian(&id).unwrap().status;
        if status == "in_review" || status == "merge_failed" {
            break;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(
        status, "in_review",
        "the restarted merge must reach in_review"
    );

    // (a) The merge actually restarted: a second "resolve" pass ran (the first
    // was the stuck one this test cancelled).
    assert!(
        resolve_calls.load(Ordering::SeqCst) >= 2,
        "resolve must have been invoked again after the restart, got {} call(s)",
        resolve_calls.load(Ordering::SeqCst)
    );

    // (b) Every branch ends in a clean, consistent terminal status -- not
    // stuck `in_progress`/`proof_pending` from the cancelled first attempt.
    let view = store.lock().unwrap().get_guardian(&id).unwrap();
    for b in &view.branches {
        assert!(
            matches!(b.merge_status.as_str(), "done" | "conflict_resolved"),
            "branch {} left in non-terminal status {:?}",
            b.branch,
            b.merge_status
        );
    }

    // (c) No orphaned/broken worktree entries: every path `git worktree list`
    // still knows about actually exists on disk.
    let wt_list = git(&root, &["worktree", "list", "--porcelain"]);
    for line in wt_list.lines() {
        if let Some(path) = line.strip_prefix("worktree ") {
            assert!(
                Path::new(path).exists(),
                "worktree list references a missing path: {path}"
            );
        }
    }

    // (d) The NEW setting (proof_scope = "nothing") took effect on the
    // restarted attempt: `resolve-proof` never ran at all -- see this test's
    // doc comment for why that's proof the restarted pass, not the original
    // "each_branch" one, is what resolved feature/y's conflict.
    let has_resolve_proof = specs
        .lock()
        .unwrap()
        .iter()
        .any(|s| s.task == "resolve-proof");
    assert!(
        !has_resolve_proof,
        "resolve-proof must not run once proof_scope is \"nothing\" on the restarted attempt"
    );

    let _ = std::fs::remove_dir_all(&root);
}

/// RAL-265 harness helper: a repo with `base` plus one feature branch per name
/// (each adding its own `<name>.txt`), a guardian over all of them, and their
/// branch ids in position order. Nothing is marked ready — callers set
/// `MergeStatus::Ready` on the branches they want buildable.
fn staged_feature_repo(names: &[&str]) -> (PathBuf, Arc<Mutex<Store>>, String, Vec<String>) {
    let root = temp_repo();
    init_repo(&root);
    write(&root, "base.txt", "base\n");
    git(&root, &["add", "."]);
    git(&root, &["commit", "-m", "base"]);
    for name in names {
        git(&root, &["checkout", "-b", name]);
        let file = format!("{}.txt", name.split('/').next_back().unwrap());
        write(&root, &file, "content\n");
        git(&root, &["add", "."]);
        git(&root, &["commit", "-m", &format!("add {file}")]);
        git(&root, &["checkout", "main"]);
    }
    let store = Arc::new(Mutex::new(Store::open_in_memory().unwrap()));
    let id = {
        let g = store.lock().unwrap();
        let id = g
            .create_guardian("r", "main", root.to_str().unwrap())
            .unwrap();
        for name in names {
            g.add_guardian_branch(&id, name).unwrap();
        }
        id
    };
    let branch_ids: Vec<String> = store
        .lock()
        .unwrap()
        .get_guardian(&id)
        .unwrap()
        .branches
        .into_iter()
        .map(|b| b.id)
        .collect();
    (root, store, id, branch_ids)
}

fn mark_ready(store: &Arc<Mutex<Store>>, id: &str, branch_id: &str) {
    store
        .lock()
        .unwrap()
        .set_branch_status(id, branch_id, MergeStatus::Ready, None)
        .unwrap();
}

/// RAL-265: a staged merge rebases just the contiguous ready prefix of branches
/// and stops (returning to `collecting`) at the first branch still waiting on
/// its upstream task — it neither waits for the whole stack nor finalizes to
/// `InReview` (no combined worktree, no manual checks) early.
#[test]
fn staged_merge_builds_ready_prefix_and_waits_in_collecting() {
    let (root, store, id, bids) = staged_feature_repo(&["feature/a", "feature/b", "feature/c"]);
    mark_ready(&store, &id, &bids[0]);

    run_merge_staged(&store, &NoopRunner, &id, &CancelToken::never());

    let view = store.lock().unwrap().get_guardian(&id).unwrap();
    assert_eq!(
        view.status, "collecting",
        "must not finalize early: {:?}",
        view.detail
    );
    assert_eq!(view.branches[0].merge_status, "done");
    assert_eq!(view.branches[1].merge_status, "pending");
    assert_eq!(view.branches[2].merge_status, "pending");
    assert!(
        view.combined_worktree.is_none(),
        "no combined worktree on a partial build"
    );
    let rev_a = view.branches[0]
        .review_branch
        .clone()
        .expect("branch a got a review branch");
    let files_a = git(&root, &["ls-tree", "-r", "--name-only", &rev_a]);
    assert!(
        files_a.contains("base.txt") && files_a.contains("a.txt"),
        "branch a stacked on base: {files_a}"
    );
    // The unreached b/c branches get no worktree yet.
    assert!(view.branches[1].worktree.is_none());
    assert!(view.branches[2].worktree.is_none());

    let _ = std::fs::remove_dir_all(&root);
}

/// RAL-265: when the next branch becomes ready, the staged merge resumes from
/// the already-built tip of the prior branch rather than rebuilding it from the
/// base — branch b stacks on branch a's preserved tip, and a's tip is untouched.
#[test]
fn staged_merge_resumes_from_prior_built_tip() {
    let (root, store, id, bids) = staged_feature_repo(&["feature/a", "feature/b", "feature/c"]);
    mark_ready(&store, &id, &bids[0]);
    run_merge_staged(&store, &NoopRunner, &id, &CancelToken::never());
    let rev_a = format!("guardian/{id}/wt-feature/a");
    let tip_a_before = git(&root, &["rev-parse", &rev_a]).trim().to_string();

    // Branch b becomes ready; branch c still pending.
    mark_ready(&store, &id, &bids[1]);
    run_merge_staged(&store, &NoopRunner, &id, &CancelToken::never());

    let view = store.lock().unwrap().get_guardian(&id).unwrap();
    assert_eq!(view.status, "collecting");
    assert_eq!(view.branches[0].merge_status, "done");
    assert_eq!(view.branches[1].merge_status, "done");
    assert_eq!(view.branches[2].merge_status, "pending");

    let tip_a_after = git(&root, &["rev-parse", &rev_a]).trim().to_string();
    assert_eq!(
        tip_a_before, tip_a_after,
        "a's already-built tip must be preserved on resume, not rebuilt"
    );
    let rev_b = format!("guardian/{id}/wt-feature/b");
    let tip_b = git(&root, &["rev-parse", &rev_b]).trim().to_string();
    // b is a descendant of the preserved a tip (stacked on top of it).
    git(
        &root,
        &["merge-base", "--is-ancestor", &tip_a_after, &tip_b],
    );
    let files_b = git(&root, &["ls-tree", "-r", "--name-only", &tip_b]);
    assert!(
        files_b.contains("a.txt") && files_b.contains("b.txt"),
        "b layered on a: {files_b}"
    );

    let _ = std::fs::remove_dir_all(&root);
}

/// RAL-265: if the base branch moves after a prefix was built, the recorded
/// build signature changes so the previously-`Done` prefix is rebuilt onto the
/// new base rather than a stale tip being carried forward.
#[test]
fn staged_merge_rebuilds_prefix_when_base_moves() {
    let (root, store, id, bids) = staged_feature_repo(&["feature/a", "feature/b"]);
    mark_ready(&store, &id, &bids[0]);
    run_merge_staged(&store, &NoopRunner, &id, &CancelToken::never());
    let rev_a = format!("guardian/{id}/wt-feature/a");
    let tip_a_old = git(&root, &["rev-parse", &rev_a]).trim().to_string();

    // Move the base forward while only branch a is built.
    git(&root, &["checkout", "main"]);
    write(&root, "extra.txt", "moved base\n");
    git(&root, &["add", "."]);
    git(&root, &["commit", "-m", "base moves forward"]);

    // Branch b becomes ready after the base moved.
    mark_ready(&store, &id, &bids[1]);
    run_merge_staged(&store, &NoopRunner, &id, &CancelToken::never());

    let view = store.lock().unwrap().get_guardian(&id).unwrap();
    assert_eq!(
        view.status, "in_review",
        "both branches done: {:?}",
        view.detail
    );
    assert!(view.branches.iter().all(|b| b.merge_status == "done"));

    let tip_a_new = git(&root, &["rev-parse", &rev_a]).trim().to_string();
    assert_ne!(
        tip_a_old, tip_a_new,
        "a must be rebuilt onto the new base, not carried forward stale"
    );
    let files_a = git(&root, &["ls-tree", "-r", "--name-only", &tip_a_new]);
    assert!(
        files_a.contains("extra.txt") && files_a.contains("a.txt"),
        "a rebuilt on the moved base: {files_a}"
    );
    let combined = view.combined_worktree.expect("combined after finalize");
    assert!(
        Path::new(&combined).join("extra.txt").exists()
            && Path::new(&combined).join("a.txt").exists()
            && Path::new(&combined).join("b.txt").exists(),
        "combined contains new base + both features"
    );

    let _ = std::fs::remove_dir_all(&root);
}

/// RAL-265: if every consecutive branch becomes ready together, one staged merge
/// rebuilds all of them in a single pass and finalizes to `InReview`.
#[test]
fn staged_merge_rebuilds_whole_stack_in_one_pass_when_all_ready() {
    let (root, store, id, bids) = staged_feature_repo(&["feature/a", "feature/b"]);
    mark_ready(&store, &id, &bids[0]);
    mark_ready(&store, &id, &bids[1]);
    run_merge_staged(&store, &NoopRunner, &id, &CancelToken::never());

    let view = store.lock().unwrap().get_guardian(&id).unwrap();
    assert_eq!(view.status, "in_review", "detail: {:?}", view.detail);
    assert!(view.branches.iter().all(|b| b.merge_status == "done"));
    let combined = view.combined_worktree.expect("combined worktree");
    assert!(
        Path::new(&combined).join("a.txt").exists() && Path::new(&combined).join("b.txt").exists(),
        "combined contains both features"
    );

    let _ = std::fs::remove_dir_all(&root);
}

/// RAL-265: the config-signature guard also invalidates a `Done` prefix when a
/// project's branch set changes (here: a branch is reordered) rather than
/// trusting a stale tip.
#[test]
fn staged_merge_rebuilds_when_branch_set_changes() {
    let (root, store, id, bids) = staged_feature_repo(&["feature/a", "feature/b"]);
    mark_ready(&store, &id, &bids[0]);
    run_merge_staged(&store, &NoopRunner, &id, &CancelToken::never());
    let rev_a = format!("guardian/{id}/wt-feature/a");
    let tip_a_old = git(&root, &["rev-parse", &rev_a]).trim().to_string();

    // Reorder the branches: feature/b now sits at position 0. The enabled
    // branch set / order fed to the signature differs, so the built prefix is
    // no longer considered valid to resume from.
    store
        .lock()
        .unwrap()
        .reorder_guardian_branches(&id, &["feature/b".to_string(), "feature/a".to_string()])
        .unwrap();

    mark_ready(&store, &id, &bids[1]);
    run_merge_staged(&store, &NoopRunner, &id, &CancelToken::never());

    let view = store.lock().unwrap().get_guardian(&id).unwrap();
    assert_eq!(view.status, "in_review", "detail: {:?}", view.detail);
    assert!(view.branches.iter().all(|b| b.merge_status == "done"));
    let tip_a_new = git(&root, &["rev-parse", &rev_a]).trim().to_string();
    assert_ne!(
        tip_a_old, tip_a_new,
        "a's tip must be rebuilt after the branch set changed"
    );

    let _ = std::fs::remove_dir_all(&root);
}

/// RAL-249: stopping a mid-rebase merge via `stop_guardian_merge` (the
/// function behind the new `POST /api/guardians/{id}/stop` handler) halts a
/// live merge worker at its next checkpoint and leaves the review in the
/// recoverable `merge_stopped` state — not `cancelled` — with the worker
/// actually stopped and the review still claimable (resumable) and
/// cancellable (abandonable).
#[test]
fn stopping_a_mid_rebase_leaves_the_review_resumable_not_cancelled() {
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
            .create_guardian("stopped merge", "main", root.to_str().unwrap())
            .unwrap();
        g.add_guardian_branch(&id, "feature/x").unwrap();
        g.add_guardian_branch(&id, "feature/y").unwrap();
        id
    };

    // A runner that blocks inside the first conflict-resolution call until
    // its cancel token trips — standing in for a long in-flight rebase.
    struct BlockingRunner {
        resolve_started: Arc<AtomicBool>,
        resolve_calls: Arc<AtomicU32>,
    }
    impl Runner for BlockingRunner {
        fn run(&self, spec: &RunnerSpec) -> RunnerResult {
            self.run_cancellable(spec, &CancelToken::never())
        }
        fn run_cancellable(&self, spec: &RunnerSpec, cancel: &CancelToken) -> RunnerResult {
            if spec.task == "resolve" {
                let n = self.resolve_calls.fetch_add(1, Ordering::SeqCst);
                self.resolve_started.store(true, Ordering::SeqCst);
                if n == 0 {
                    for _ in 0..1000 {
                        if cancel.is_cancelled() {
                            return RunnerResult::failure("cancelled");
                        }
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    return RunnerResult::failure("blocking runner was never cancelled");
                }
            }
            MarkerStrippingRunner.run_cancellable(spec, cancel)
        }
    }

    let resolve_started = Arc::new(AtomicBool::new(false));
    let resolve_calls = Arc::new(AtomicU32::new(0));
    let runner: Arc<dyn Runner> = Arc::new(BlockingRunner {
        resolve_started: Arc::clone(&resolve_started),
        resolve_calls: Arc::clone(&resolve_calls),
    });

    let cancellations = Cancellations::new();
    let sem = Arc::new(Semaphore::new(4));

    // Kick off the merge exactly the way `POST /api/guardians/{id}/merge` does.
    let reply = start_merge(
        Arc::clone(&store),
        Arc::clone(&runner),
        &id,
        Arc::clone(&sem),
        cancellations.clone(),
    );
    assert_eq!(reply.status, 202);

    // Wait until the merge is genuinely stuck mid-resolve (same 120s headroom
    // as the settings-restart test, for the same contended-test reasons).
    for _ in 0..24000 {
        if resolve_started.load(Ordering::SeqCst) {
            break;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(
        resolve_started.load(Ordering::SeqCst),
        "merge never reached the blocking resolve call"
    );
    assert_eq!(
        store.lock().unwrap().get_guardian(&id).unwrap().status,
        "merging"
    );

    // Stop it mid-rebase, the way `POST /api/guardians/{id}/stop` does.
    let stop_reply = stop_guardian_merge(Arc::clone(&store), cancellations.clone(), &id);
    assert_eq!(stop_reply.status, 200, "{}", stop_reply.body);
    assert!(
        stop_reply.body.contains("\"merge_stopped\""),
        "{}",
        stop_reply.body
    );

    // The worker actually halted: its token was tripped and it exited
    // (not just the DB column flipped underneath a still-running thread).
    let key = format!("guardian:{id}");
    for _ in 0..1200 {
        if !cancellations.is_active(&key) {
            break;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(
        !cancellations.is_active(&key),
        "merge worker did not stop after being told to"
    );
    assert_eq!(
        store.lock().unwrap().get_guardian(&id).unwrap().status,
        "merge_stopped",
        "a stopped review is merge_stopped, not cancelled"
    );

    // The blocked resolve was genuinely cancelled by the stop.
    assert_eq!(resolve_calls.load(Ordering::SeqCst), 1);

    // The stopped review is resumable (claimable for a fresh merge) and
    // cancellable (abandonable) — RAL-249's "recoverable, not dead".
    {
        let g = store.lock().unwrap();
        assert!(g.claim_guardian_merge(&id).unwrap());
        assert_eq!(
            g.get_guardian(&id).unwrap().status,
            "merging",
            "resume works"
        );
        assert_eq!(
            g.cancel_guardian(&id).unwrap(),
            ralphus_daemon::guardian::GuardianStatus::Cancelled,
            "a stopped review can still be abandoned"
        );
    }

    let _ = std::fs::remove_dir_all(&root);
}
