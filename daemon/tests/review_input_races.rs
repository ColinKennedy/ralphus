//! Regression tests for commits being dropped when several inputs reach one
//! review: reviewer feedback, unattended PR auto-fixes, reviewer pushes made
//! directly to a PR branch, and the restacks/rebuilds each of them triggers.
//!
//! Every test builds a real git repo with a bare `origin`, drives the public
//! guardian entry points with fake resolver agents, and asserts that each
//! input's commit is still on the branch afterwards -- locally *and*, where a
//! push is involved, on the remote ref a PR would show.

mod common;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::{Duration, Instant};

use common::{git, init_repo};
use ralphus_daemon::cancel::CancelToken;
use ralphus_daemon::guardian::{GuardianStatus, MergeStatus};
use ralphus_daemon::guardian_merge::{rebase_on_manual_push, run_feedback, run_merge};
use ralphus_daemon::runner::{Runner, RunnerResult, RunnerSpec};
use ralphus_daemon::scheduler::Semaphore;
use ralphus_daemon::store::Store;
use ralphus_daemon::store_lock::StoreMutex;

fn temp_dir() -> PathBuf {
    static N: AtomicU32 = AtomicU32::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("ralphus-races-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("mkdir");
    dir
}

fn write(root: &Path, name: &str, content: &str) {
    std::fs::write(root.join(name), content).expect("write file");
}

fn ok_result(summary: &str, proofed: Option<bool>) -> RunnerResult {
    RunnerResult {
        status: "done".into(),
        summary: summary.into(),
        error: None,
        proofed,
        ..RunnerResult::failure("")
    }
}

/// A fake resolver agent: writes `file` into the worktree it is handed, and
/// plays `run_feedback`'s separate commit-step agent (cell id suffixed
/// `-commit`) by committing everything dirty.
struct WriteFileRunner {
    file: &'static str,
}

impl Runner for WriteFileRunner {
    fn run(&self, spec: &RunnerSpec) -> RunnerResult {
        let cwd = PathBuf::from(&spec.cwd);
        if spec.cell_id.ends_with("-commit") {
            let dirty = !git(&cwd, &["status", "--porcelain"]).trim().is_empty();
            if dirty {
                git(&cwd, &["add", "--all"]);
                git(&cwd, &["commit", "-m", &format!("feedback: {}", self.file)]);
            }
            return ok_result(
                if dirty {
                    "committed"
                } else {
                    "nothing to commit"
                },
                Some(dirty),
            );
        }
        write(&cwd, self.file, &format!("{}\n", self.file));
        ok_result("edited\nRALPHUS_PROOF: PASS", Some(true))
    }
}

/// [`WriteFileRunner`] whose resolver call blocks until `release` is set, so a
/// test can hold this feedback round's worktree lease while other inputs land.
struct GatedWriteFileRunner {
    inner: WriteFileRunner,
    started: Arc<AtomicBool>,
    release: Arc<AtomicBool>,
}

impl Runner for GatedWriteFileRunner {
    fn run(&self, spec: &RunnerSpec) -> RunnerResult {
        if !spec.cell_id.ends_with("-commit") {
            self.started.store(true, Ordering::SeqCst);
            let deadline = Instant::now() + Duration::from_secs(120);
            while !self.release.load(Ordering::SeqCst) {
                assert!(Instant::now() < deadline, "gated runner never released");
                std::thread::sleep(Duration::from_millis(10));
            }
        }
        self.inner.run(spec)
    }
}

fn wait_for(flag: &AtomicBool, what: &str) {
    let deadline = Instant::now() + Duration::from_secs(120);
    while !flag.load(Ordering::SeqCst) {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// A runner that is never expected to be invoked (no conflicts).
struct NoopRunner;
impl Runner for NoopRunner {
    fn run(&self, _spec: &RunnerSpec) -> RunnerResult {
        RunnerResult::failure("noop runner should not be called")
    }
}

/// A repo with a bare `origin`, a `main` base and one commit per feature
/// branch (`<name>.txt`), plus a review stacking `features` in order and
/// already merged to `in_review`.
struct Fixture {
    root: PathBuf,
    remote: PathBuf,
    store: Arc<StoreMutex>,
    id: String,
    branch_ids: Vec<String>,
}

impl Fixture {
    fn new(features: &[&str]) -> Self {
        Self::build(features, "main")
    }

    /// A review whose base is the remote-tracking `origin/main`, so the base
    /// moves only when a commit is pushed to the bare `origin` and the
    /// daemon's own base poll fetches it -- exactly like a real upstream.
    /// Proofs are off: every agent call in these scenarios is a writer's.
    fn with_upstream(features: &[&str]) -> Self {
        let fx = Self::build(features, "origin/main");
        fx.store
            .lock()
            .set_guardian_proof_scope(&fx.id, Some("nothing"))
            .unwrap();
        fx
    }

    fn build(features: &[&str], base: &str) -> Self {
        let root = temp_dir();
        init_repo(&root);
        let remote = temp_dir();
        git(&remote, &["init", "--bare"]);
        git(
            &root,
            &["remote", "add", "origin", remote.to_str().unwrap()],
        );
        write(&root, "base.txt", "base\n");
        git(&root, &["add", "."]);
        git(&root, &["commit", "-m", "base"]);
        git(&root, &["push", "-q", "origin", "main"]);
        for name in features {
            git(&root, &["checkout", "-q", "-b", name]);
            write(
                &root,
                &format!("{}.txt", name.replace('/', "-")),
                "content\n",
            );
            git(&root, &["add", "."]);
            git(&root, &["commit", "-m", &format!("add {name}")]);
            git(&root, &["checkout", "-q", "main"]);
        }
        // A file-backed WAL store, like the daemon's own: the in-memory store's
        // shared cache raises SQLITE_LOCKED under the concurrent writers these
        // scenarios run, which the real daemon never sees.
        let store = Arc::new(StoreMutex::new(
            Store::open(&root.join(".git").join("ralphus-test.db")).unwrap(),
        ));
        let id = {
            let g = store.lock();
            let id = g
                .create_guardian("r", base, root.to_str().unwrap())
                .unwrap();
            for name in features {
                g.add_guardian_branch(&id, name).unwrap();
            }
            id
        };
        run_merge(&store, &NoopRunner, &id);
        let view = store.lock().get_guardian(&id).unwrap();
        assert_eq!(view.status, "in_review", "detail: {:?}", view.detail);
        let branch_ids = view.branches.iter().map(|b| b.id.clone()).collect();
        Self {
            root,
            remote,
            store,
            id,
            branch_ids,
        }
    }

    fn review_ref(&self, position: usize) -> String {
        self.store.lock().get_guardian(&self.id).unwrap().branches[position]
            .review_branch
            .clone()
            .expect("review branch built")
    }

    fn files_on(&self, repo: &Path, rev: &str) -> String {
        git(repo, &["ls-tree", "-r", "--name-only", rev])
    }

    fn feedback(&self, position: usize, file: &'static str) {
        run_feedback(
            &self.store,
            &WriteFileRunner { file },
            &self.id,
            &self.branch_ids[position],
            &format!("add {file}"),
            None,
            false,
            &CancelToken::never(),
        );
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
        let _ = std::fs::remove_dir_all(&self.remote);
    }
}

/// A feedback commit on a downstream branch must survive a later feedback on
/// an upstream branch: the upstream round restacks every branch above it, and
/// that restack used to reset each downstream review branch to its task
/// branch tip, silently discarding every commit that only ever existed on the
/// review branch.
#[test]
fn upstream_feedback_restack_keeps_a_downstream_feedback_commit() {
    let fx = Fixture::new(&["feature/a", "feature/b"]);

    fx.feedback(1, "fb-b.txt");
    let rev1 = fx.review_ref(1);
    assert!(
        fx.files_on(&fx.root, &rev1).contains("fb-b.txt"),
        "precondition: branch 1 carries its own feedback commit"
    );

    fx.feedback(0, "fb-a.txt");

    let view = fx.store.lock().get_guardian(&fx.id).unwrap();
    assert_eq!(view.status, "in_review", "detail: {:?}", view.detail);
    let files1 = fx.files_on(&fx.root, &fx.review_ref(1));
    assert!(
        files1.contains("fb-a.txt"),
        "branch 1 is restacked onto branch 0's new feedback commit: {files1}"
    );
    assert!(
        files1.contains("fb-b.txt"),
        "branch 1's own feedback commit must survive the upstream restack: {files1}"
    );
    let combined = view.review_branch.clone().unwrap();
    let combined_files = fx.files_on(&fx.root, &combined);
    assert!(
        combined_files.contains("fb-a.txt") && combined_files.contains("fb-b.txt"),
        "the combined review branch carries both feedback commits: {combined_files}"
    );
}

/// Two feedback rounds on different branches at once. Branch 0's round
/// finishes first while branch 1's resolver still holds its worktree lease,
/// so branch 0's restack is queued; branch 1's round then claims the
/// coalesced restack starting at position 0. That restack must rebuild
/// branch 1 on top of branch 0 -- not on branch 1 itself -- and keep branch
/// 1's own feedback commit.
#[test]
fn coalesced_restack_from_a_lower_branch_keeps_both_feedback_commits() {
    let fx = Fixture::new(&["feature/a", "feature/b"]);
    let started = Arc::new(AtomicBool::new(false));
    let release = Arc::new(AtomicBool::new(false));
    let gated = GatedWriteFileRunner {
        inner: WriteFileRunner { file: "fb-b.txt" },
        started: Arc::clone(&started),
        release: Arc::clone(&release),
    };
    let store = Arc::clone(&fx.store);
    let id = fx.id.clone();
    let bid1 = fx.branch_ids[1].clone();
    let slow = std::thread::spawn(move || {
        run_feedback(
            &store,
            &gated,
            &id,
            &bid1,
            "add fb-b.txt",
            None,
            false,
            &CancelToken::never(),
        );
    });
    wait_for(&started, "branch 1's resolver to start");

    fx.feedback(0, "fb-a.txt");
    release.store(true, Ordering::SeqCst);
    slow.join().expect("branch 1 feedback thread");

    let view = fx.store.lock().get_guardian(&fx.id).unwrap();
    assert_eq!(view.status, "in_review", "detail: {:?}", view.detail);
    let files1 = fx.files_on(&fx.root, &fx.review_ref(1));
    assert!(
        files1.contains("fb-a.txt") && files1.contains("fb-b.txt"),
        "branch 1 carries branch 0's feedback and its own: {files1}"
    );
    assert!(
        files1.contains("feature-a.txt") && files1.contains("feature-b.txt"),
        "branch 1 still carries both task branches: {files1}"
    );
}

/// Scenario A: a reviewer commits straight into branch 0's review worktree.
/// The maintenance sweep's manual-push restack must rebuild branch 1 on top
/// of it without dropping the feedback commit branch 1 already carried.
#[test]
fn manual_commit_in_review_worktree_restack_keeps_downstream_feedback() {
    let fx = Fixture::new(&["feature/a", "feature/b"]);
    fx.feedback(1, "fb-b.txt");

    let wt0 = fx.store.lock().get_guardian(&fx.id).unwrap().branches[0]
        .worktree
        .clone()
        .expect("branch 0 review worktree");
    let wt0 = PathBuf::from(wt0);
    write(&wt0, "manual.txt", "by hand\n");
    git(&wt0, &["add", "manual.txt"]);
    git(&wt0, &["commit", "-m", "reviewer: manual edit"]);

    let ran = rebase_on_manual_push(&fx.store, &NoopRunner, &fx.id, &Semaphore::new(1));
    assert!(ran, "the manual commit must be detected and restacked");

    let files1 = fx.files_on(&fx.root, &fx.review_ref(1));
    assert!(
        files1.contains("manual.txt"),
        "branch 1 is restacked onto the manual commit: {files1}"
    );
    assert!(
        files1.contains("fb-b.txt"),
        "branch 1's feedback commit must survive the manual-push restack: {files1}"
    );
}

impl Fixture {
    /// Advance the base branch (`main`) with a new commit adding `file`.
    fn advance_base(&self, file: &str) {
        write(&self.root, file, "on base\n");
        git(&self.root, &["add", file]);
        git(&self.root, &["commit", "-m", &format!("base: {file}")]);
    }

    fn assert_branch_has(&self, position: usize, files: &[&str], context: &str) {
        let on_branch = self.files_on(&self.root, &self.review_ref(position));
        for f in files {
            assert!(
                on_branch.contains(f),
                "{context}: branch {position} lost {f}: {on_branch}"
            );
        }
    }
}

/// Scenario D: the base branch moves on while both branches carry
/// review-only feedback commits. The base-shift rebuild must replay those
/// commits onto the new base, not rebuild from the task branches alone.
#[test]
fn base_shift_rebuild_keeps_review_only_commits() {
    let fx = Fixture::new(&["feature/a", "feature/b"]);
    fx.feedback(1, "fb-b.txt");
    fx.feedback(0, "fb-a.txt");
    fx.advance_base("base2.txt");

    let rebuilt = ralphus_daemon::guardian_merge::rebuild_on_base_shift(
        &fx.store,
        &NoopRunner,
        &fx.id,
        &Semaphore::new(4),
        &CancelToken::never(),
    );
    assert!(rebuilt, "a base shift triggers a rebuild");
    let view = fx.store.lock().get_guardian(&fx.id).unwrap();
    assert_eq!(view.status, "in_review", "detail: {:?}", view.detail);
    fx.assert_branch_has(0, &["base2.txt", "fb-a.txt"], "base shift");
    fx.assert_branch_has(1, &["base2.txt", "fb-a.txt", "fb-b.txt"], "base shift");
}

/// A manual "Merge / rebase" of an already-built review must keep the
/// review-only commits it carries.
#[test]
fn manual_merge_keeps_review_only_commits() {
    let fx = Fixture::new(&["feature/a", "feature/b"]);
    fx.feedback(1, "fb-b.txt");
    fx.feedback(0, "fb-a.txt");
    fx.advance_base("base2.txt");

    run_merge(&fx.store, &NoopRunner, &fx.id);
    let view = fx.store.lock().get_guardian(&fx.id).unwrap();
    assert_eq!(view.status, "in_review", "detail: {:?}", view.detail);
    fx.assert_branch_has(0, &["base2.txt", "fb-a.txt"], "manual merge");
    fx.assert_branch_has(1, &["base2.txt", "fb-a.txt", "fb-b.txt"], "manual merge");
}

/// Scenario F: a new task branch joins a review whose existing branches
/// already carry review-only commits. Building the newcomer onto the stack
/// must not rebuild the existing branches from their task tips.
#[test]
fn appending_a_branch_keeps_existing_review_only_commits() {
    let fx = Fixture::new(&["feature/a", "feature/b"]);
    fx.feedback(1, "fb-b.txt");
    fx.feedback(0, "fb-a.txt");

    git(&fx.root, &["checkout", "-q", "-b", "feature/c"]);
    write(&fx.root, "feature-c.txt", "content\n");
    git(&fx.root, &["add", "."]);
    git(&fx.root, &["commit", "-m", "add feature/c"]);
    git(&fx.root, &["checkout", "-q", "main"]);
    fx.store
        .lock()
        .add_guardian_branch(&fx.id, "feature/c")
        .unwrap();
    let bid2 = fx.store.lock().get_guardian(&fx.id).unwrap().branches[2]
        .id
        .clone();
    fx.store
        .lock()
        .set_branch_status(&fx.id, &bid2, MergeStatus::Ready, None)
        .unwrap();
    fx.store
        .lock()
        .set_guardian_status(&fx.id, GuardianStatus::Collecting, None)
        .unwrap();

    ralphus_daemon::guardian_merge::run_merge_staged(
        &fx.store,
        &NoopRunner,
        &fx.id,
        &CancelToken::never(),
    );
    let view = fx.store.lock().get_guardian(&fx.id).unwrap();
    assert_eq!(view.status, "in_review", "detail: {:?}", view.detail);
    fx.assert_branch_has(0, &["fb-a.txt"], "append");
    fx.assert_branch_has(1, &["fb-a.txt", "fb-b.txt"], "append");
    fx.assert_branch_has(2, &["fb-a.txt", "fb-b.txt", "feature-c.txt"], "append");
}

impl Fixture {
    /// Commit `file` onto task branch `branch` (never checked out by a review).
    fn commit_on_task_branch(&self, branch: &str, file: &str) {
        git(&self.root, &["checkout", "-q", branch]);
        write(&self.root, file, "more task work\n");
        git(&self.root, &["add", file]);
        git(&self.root, &["commit", "-m", &format!("task: {file}")]);
        git(&self.root, &["checkout", "-q", "main"]);
    }

    fn rebuild_staged(&self) {
        self.store
            .lock()
            .set_guardian_status(&self.id, GuardianStatus::Collecting, None)
            .unwrap();
        ralphus_daemon::guardian_merge::run_merge_staged(
            &self.store,
            &NoopRunner,
            &self.id,
            &CancelToken::never(),
        );
        let view = self.store.lock().get_guardian(&self.id).unwrap();
        assert_eq!(view.status, "in_review", "detail: {:?}", view.detail);
    }

    fn assert_branch_lacks(&self, position: usize, files: &[&str], context: &str) {
        let on_branch = self.files_on(&self.root, &self.review_ref(position));
        for f in files {
            assert!(
                !on_branch.contains(f),
                "{context}: branch {position} must not carry {f}: {on_branch}"
            );
        }
    }
}

/// Scenario F: a task gains a commit after its review branch was built (its
/// cell re-ran). A later restack carries the review branch forward -- it must
/// fold in the new task commit as well as keep the review-only commit.
#[test]
fn restack_folds_in_new_task_commits_and_keeps_review_only_ones() {
    let fx = Fixture::new(&["feature/a", "feature/b"]);
    fx.feedback(1, "fb-b.txt");
    fx.commit_on_task_branch("feature/b", "feature-b-more.txt");

    fx.feedback(0, "fb-a.txt");

    fx.assert_branch_has(
        1,
        &[
            "fb-a.txt",
            "fb-b.txt",
            "feature-b.txt",
            "feature-b-more.txt",
        ],
        "restack after a task commit",
    );
}

/// Reordering a review's branches must keep each branch's review-only
/// commits with that branch, and nothing of the branch now above it.
#[test]
fn reorder_keeps_each_branchs_review_only_commits() {
    let fx = Fixture::new(&["feature/a", "feature/b"]);
    fx.feedback(0, "fb-a.txt");
    fx.feedback(1, "fb-b.txt");
    fx.store
        .lock()
        .reorder_guardian_branches(&fx.id, &["feature/b".to_string(), "feature/a".to_string()])
        .unwrap();

    fx.rebuild_staged();

    let view = fx.store.lock().get_guardian(&fx.id).unwrap();
    assert_eq!(view.branches[0].branch, "feature/b");
    fx.assert_branch_has(0, &["feature-b.txt", "fb-b.txt"], "reorder");
    fx.assert_branch_lacks(0, &["feature-a.txt", "fb-a.txt"], "reorder");
    fx.assert_branch_has(
        1,
        &["feature-a.txt", "fb-a.txt", "feature-b.txt", "fb-b.txt"],
        "reorder",
    );
}

/// Disabling a middle branch must drop its work from the branches stacked
/// above it while keeping their own review-only commits.
#[test]
fn disabling_a_middle_branch_drops_only_its_own_commits() {
    let fx = Fixture::new(&["feature/a", "feature/b", "feature/c"]);
    fx.feedback(1, "fb-b.txt");
    fx.feedback(2, "fb-c.txt");
    fx.store
        .lock()
        .set_branch_enabled_by_name(&fx.id, "feature/b", false)
        .unwrap();

    fx.rebuild_staged();

    fx.assert_branch_has(
        2,
        &["feature-a.txt", "feature-c.txt", "fb-c.txt"],
        "disable",
    );
    fx.assert_branch_lacks(2, &["feature-b.txt", "fb-b.txt"], "disable");
}

/// A resolver that appends a line to a tracked file -- an in-progress edit a
/// worktree reset would silently discard -- then blocks until released.
struct GatedTrackedEditRunner {
    file: &'static str,
    line: &'static str,
    started: Arc<AtomicBool>,
    release: Arc<AtomicBool>,
}

impl Runner for GatedTrackedEditRunner {
    fn run(&self, spec: &RunnerSpec) -> RunnerResult {
        let cwd = PathBuf::from(&spec.cwd);
        if spec.cell_id.ends_with("-commit") {
            return WriteFileRunner { file: self.file }.run(spec);
        }
        let path = cwd.join(self.file);
        let mut text = std::fs::read_to_string(&path).unwrap_or_default();
        text.push_str(self.line);
        text.push('\n');
        std::fs::write(&path, text).expect("edit tracked file");
        self.started.store(true, Ordering::SeqCst);
        let deadline = Instant::now() + Duration::from_secs(120);
        while !self.release.load(Ordering::SeqCst) {
            assert!(Instant::now() < deadline, "gated runner never released");
            std::thread::sleep(Duration::from_millis(10));
        }
        ok_result("edited\nRALPHUS_PROOF: PASS", Some(true))
    }
}

/// A merge that starts while a feedback round is still editing a branch's
/// worktree -- possible because another branch's feedback finishing hands the
/// review back to `in_review` -- must wait for that round instead of
/// force-resetting the worktree under it and discarding its edit.
#[test]
fn merge_waits_for_an_in_flight_feedback_edit() {
    let fx = Fixture::new(&["feature/a", "feature/b"]);
    let started = Arc::new(AtomicBool::new(false));
    let release = Arc::new(AtomicBool::new(false));
    let editor = GatedTrackedEditRunner {
        file: "feature-b.txt",
        line: "reviewer-requested change",
        started: Arc::clone(&started),
        release: Arc::clone(&release),
    };
    let (store, id, bid1) = (
        Arc::clone(&fx.store),
        fx.id.clone(),
        fx.branch_ids[1].clone(),
    );
    let slow = std::thread::spawn(move || {
        run_feedback(
            &store,
            &editor,
            &id,
            &bid1,
            "change feature-b.txt",
            None,
            false,
            &CancelToken::never(),
        );
    });
    wait_for(&started, "branch 1's resolver to start editing");
    fx.feedback(0, "fb-a.txt");

    let (store, id) = (Arc::clone(&fx.store), fx.id.clone());
    let merge = std::thread::spawn(move || run_merge(&store, &NoopRunner, &id));
    std::thread::sleep(Duration::from_secs(2));
    release.store(true, Ordering::SeqCst);
    slow.join().expect("feedback thread");
    merge.join().expect("merge thread");

    let rev1 = fx.review_ref(1);
    let content = git(&fx.root, &["show", &format!("{rev1}:feature-b.txt")]);
    assert!(
        content.contains("reviewer-requested change"),
        "branch 1's in-flight feedback edit must survive the merge: {content:?}"
    );
    fx.assert_branch_has(1, &["fb-a.txt"], "merge after feedback");
}

/// A resolver that leaves only junk the commit step declines to commit (so
/// the round commits nothing), after blocking until released.
struct GatedNothingToCommitRunner {
    started: Arc<AtomicBool>,
    release: Arc<AtomicBool>,
}

impl Runner for GatedNothingToCommitRunner {
    fn run(&self, spec: &RunnerSpec) -> RunnerResult {
        if spec.cell_id.ends_with("-commit") {
            return ok_result("nothing genuine to commit", Some(false));
        }
        write(&PathBuf::from(&spec.cwd), "scratch.log", "junk\n");
        self.started.store(true, Ordering::SeqCst);
        let deadline = Instant::now() + Duration::from_secs(120);
        while !self.release.load(Ordering::SeqCst) {
            assert!(Instant::now() < deadline, "gated runner never released");
            std::thread::sleep(Duration::from_millis(10));
        }
        ok_result("looked, changed nothing\nRALPHUS_PROOF: PASS", Some(true))
    }
}

/// A feedback round that commits nothing must still run a restack another
/// branch's round queued behind its worktree lease -- otherwise that branch's
/// new commit never reaches the branches stacked above it.
#[test]
fn a_round_that_commits_nothing_runs_the_restack_queued_behind_it() {
    let fx = Fixture::new(&["feature/a", "feature/b", "feature/c"]);
    let started = Arc::new(AtomicBool::new(false));
    let release = Arc::new(AtomicBool::new(false));
    let idle = GatedNothingToCommitRunner {
        started: Arc::clone(&started),
        release: Arc::clone(&release),
    };
    let (store, id, bid0) = (
        Arc::clone(&fx.store),
        fx.id.clone(),
        fx.branch_ids[0].clone(),
    );
    let slow = std::thread::spawn(move || {
        run_feedback(
            &store,
            &idle,
            &id,
            &bid0,
            "take a look",
            None,
            false,
            &CancelToken::never(),
        );
    });
    wait_for(&started, "branch 0's resolver to start");
    fx.feedback(1, "fb-b.txt");
    release.store(true, Ordering::SeqCst);
    slow.join().expect("branch 0 feedback thread");

    let view = fx.store.lock().get_guardian(&fx.id).unwrap();
    assert_eq!(view.status, "in_review", "detail: {:?}", view.detail);
    fx.assert_branch_has(2, &["fb-b.txt", "feature-c.txt"], "queued restack");
}

/// Scenario B racing D: the review branch is rebuilt onto a moved base while
/// a reviewer pushes to its PR branch from the old tip. Pulling the reviewer's
/// commit in must replay just that commit onto the rebuilt branch -- not
/// rebase the rebuilt branch onto the reviewer's old-base tip, which re-plays
/// the new base commit as an ordinary commit, unstacks the branch from the
/// review's base, and makes the next base-shift rebuild drop every
/// review-only commit.
#[test]
fn pulling_a_reviewer_push_after_a_base_shift_keeps_the_branch_on_its_base() {
    let fx = Fixture::new(&["feature/a", "feature/b"]);
    fx.feedback(0, "fb-a.txt");
    let alias = "pr-a";
    let pr_id = fx.open_pr(0, alias);
    let published = fx.remote_tip(alias);
    fx.store
        .lock()
        .update_pull_request_ex(
            &pr_id,
            None,
            None,
            None,
            None,
            None,
            Some(Some(&published)),
            None,
        )
        .unwrap();

    // The base moves and the review rebuilds onto it (not yet re-published).
    fx.advance_base("base2.txt");
    assert!(ralphus_daemon::guardian_merge::rebuild_on_base_shift(
        &fx.store,
        &NoopRunner,
        &fx.id,
        &Semaphore::new(4),
        &CancelToken::never(),
    ));

    // Meanwhile a reviewer pushes onto the PR branch as they last saw it.
    let clone = temp_dir();
    let _ = std::fs::remove_dir_all(&clone);
    git(
        clone.parent().unwrap(),
        &[
            "clone",
            "-q",
            fx.remote.to_str().unwrap(),
            clone.file_name().unwrap().to_str().unwrap(),
        ],
    );
    git(&clone, &["checkout", "-q", alias]);
    write(&clone, "reviewer.txt", "reviewer\n");
    git(&clone, &["add", "."]);
    git(&clone, &["commit", "-m", "reviewer fix"]);
    git(&clone, &["push", "-q", "origin", alias]);

    let pulled = ralphus_daemon::guardian_merge::pull_pr_commits(
        &fx.store,
        &NoopRunner,
        &fx.id,
        &fx.branch_ids[0],
        "origin",
        alias,
        Some(&published),
    )
    .expect("pull succeeds");
    assert!(pulled, "the reviewer's commit is pulled in");

    let rev0 = fx.review_ref(0);
    fx.assert_branch_has(0, &["reviewer.txt", "fb-a.txt", "base2.txt"], "pull");
    assert!(
        git(&fx.root, &["merge-base", "--is-ancestor", "main", &rev0]).is_empty(),
        "branch 0 is still stacked on the review's current base"
    );
    let base_commits = git(&fx.root, &["log", "--format=%s", "main..", &rev0]);
    assert!(
        !base_commits.contains("base: base2.txt"),
        "the base commit must not be replayed onto the branch: {base_commits}"
    );

    // And a later base shift still carries every review-only commit.
    fx.advance_base("base3.txt");
    assert!(ralphus_daemon::guardian_merge::rebuild_on_base_shift(
        &fx.store,
        &NoopRunner,
        &fx.id,
        &Semaphore::new(4),
        &CancelToken::never(),
    ));
    fx.assert_branch_has(
        0,
        &["reviewer.txt", "fb-a.txt", "base2.txt", "base3.txt"],
        "base shift after pull",
    );
    fx.assert_branch_has(
        1,
        &["reviewer.txt", "fb-a.txt", "base3.txt"],
        "base shift after pull",
    );
    let _ = std::fs::remove_dir_all(&clone);
}

/// The same as [`disabling_a_middle_branch_drops_only_its_own_commits`], via
/// a manual "Merge / rebase" (a full merge) instead of a staged rebuild.
#[test]
fn full_merge_after_disabling_a_middle_branch_drops_only_its_own_commits() {
    let fx = Fixture::new(&["feature/a", "feature/b", "feature/c"]);
    fx.feedback(1, "fb-b.txt");
    fx.feedback(2, "fb-c.txt");
    fx.store
        .lock()
        .set_branch_enabled_by_name(&fx.id, "feature/b", false)
        .unwrap();

    run_merge(&fx.store, &NoopRunner, &fx.id);

    let view = fx.store.lock().get_guardian(&fx.id).unwrap();
    assert_eq!(view.status, "in_review", "detail: {:?}", view.detail);
    fx.assert_branch_has(
        2,
        &["feature-a.txt", "feature-c.txt", "fb-c.txt"],
        "full merge",
    );
    fx.assert_branch_lacks(2, &["feature-b.txt", "fb-b.txt"], "full merge");
}

/// Feedback sent to a review whose PRs already merged must be refused up
/// front, not committed to a merged PR branch where it never lands.
#[test]
fn feedback_on_a_merged_review_is_refused() {
    let fx = Fixture::new(&["feature/a"]);
    fx.store
        .lock()
        .set_guardian_status(&fx.id, GuardianStatus::Merged, None)
        .unwrap();
    let rev0 = fx.review_ref(0);
    let before = git(&fx.root, &["rev-parse", &rev0]);

    let reply = ralphus_daemon::guardian_merge::start_feedback(
        Arc::clone(&fx.store),
        Arc::new(WriteFileRunner { file: "late.txt" }),
        ralphus_daemon::cancel::Cancellations::new(),
        &fx.id,
        &fx.branch_ids[0],
        "add late.txt".to_string(),
        None,
        None,
    );
    assert_eq!(reply.status, 409, "a merged review refuses feedback");
    assert!(
        fx.store
            .lock()
            .guardian_branch_messages(&fx.id, &fx.branch_ids[0])
            .unwrap()
            .is_empty(),
        "no feedback message is queued"
    );
    assert_eq!(git(&fx.root, &["rev-parse", &rev0]), before);
    assert_eq!(
        fx.store.lock().get_guardian(&fx.id).unwrap().status,
        "merged",
        "the review stays merged"
    );
}

fn gitlab_client() -> ralphus_daemon::forge::ForgeClient {
    ralphus_daemon::forge::ForgeClient::new(
        ralphus_daemon::forge::ForgeKind::GitLab,
        "http://unused.invalid".to_string(),
        "acme/w".to_string(),
        None,
    )
}

fn ci_failure() -> ralphus_daemon::forge::PrFailure {
    ralphus_daemon::forge::PrFailure {
        reason: "check 'build' failed".to_string(),
        job_url: None,
        log_text: None,
        checks: vec![],
    }
}

impl Fixture {
    /// Publish branch `position`'s review ref to `origin` as `alias` and
    /// record an open PR for it, with unattended auto-fix enabled.
    fn open_pr(&self, position: usize, alias: &str) -> String {
        let rev = self.review_ref(position);
        git(
            &self.root,
            &["push", "-q", "origin", &format!("{rev}:refs/heads/{alias}")],
        );
        let pr_id = self
            .store
            .lock()
            .create_pull_request(
                &self.id,
                Some(&self.branch_ids[position]),
                "gitlab",
                "acme/w",
                alias,
                "main",
                "T",
                "",
                Some(100 + position as i64),
                Some(&format!(
                    "https://gitlab.com/acme/w/-/merge_requests/{}",
                    100 + position
                )),
            )
            .unwrap();
        self.store
            .lock()
            .set_guardian_auto_fix_pr_errors(&self.id, Some(true))
            .unwrap();
        pr_id
    }

    fn auto_fix(&self, pr_id: &str, file: &'static str) {
        let guardian = self.store.lock().get_guardian(&self.id).unwrap();
        let pr = self.store.lock().get_pull_request(pr_id).unwrap();
        ralphus_daemon::ci_watch::dispatch_pr_auto_fix(
            &self.store,
            &WriteFileRunner { file },
            &guardian,
            &pr,
            &ci_failure(),
            &gitlab_client(),
        );
    }

    fn remote_tip(&self, alias: &str) -> String {
        git(&self.remote, &["rev-parse", &format!("refs/heads/{alias}")])
            .trim()
            .to_string()
    }
}

/// An unattended PR auto-fix must reach the branch the PR is actually opened
/// from. When the PR's remote branch (its alias) differs from the review
/// branch's own name, the fix used to be pushed to a remote branch named after
/// the review branch -- a branch no PR shows -- while the PR row still recorded
/// the new SHA as pushed, so nothing ever re-published it.
#[test]
fn auto_fix_is_pushed_to_the_prs_own_branch_alias() {
    let fx = Fixture::new(&["feature/a", "feature/b"]);
    let alias = "pr-a-alias";
    let pr_id = fx.open_pr(0, alias);
    let before = fx.remote_tip(alias);

    fx.auto_fix(&pr_id, "fix.txt");

    let remote_files = fx.files_on(&fx.remote, &format!("refs/heads/{alias}"));
    assert!(
        remote_files.contains("fix.txt"),
        "the auto-fix commit must be on the PR's remote branch: {remote_files}"
    );
    let after = fx.remote_tip(alias);
    assert_ne!(after, before, "the PR branch moved");
    let pr_after = fx.store.lock().get_pull_request(&pr_id).unwrap();
    assert_eq!(
        pr_after.last_pushed_sha.as_deref(),
        Some(after.as_str()),
        "the recorded pushed SHA is what the PR branch actually holds"
    );
}

// ---------------------------------------------------------------------------
// Several writers in flight at once: an upstream rebase (base shift and an
// upstream feedback restack) while a feedback round edits one downstream
// branch and an unattended PR fix edits another.
// ---------------------------------------------------------------------------

/// Agent calls nobody expected -- a conflict resolution, a second resolver
/// pass. In these scenarios every writer edits a different file, so any such
/// call means a rebuild replayed commits it should not have.
type Unexpected = Arc<std::sync::Mutex<Vec<String>>>;

/// A writer's resolver: appends `line` to the tracked file `file` (an edit a
/// worktree reset would discard and a duplicate replay would conflict with),
/// optionally holding until `release` is set. Its commit step commits it.
/// Every agent call after the first resolver call is recorded as unexpected.
struct WriterRunner {
    file: String,
    line: String,
    started: Arc<AtomicBool>,
    release: Option<Arc<AtomicBool>>,
    resolver_calls: AtomicU32,
    unexpected: Unexpected,
}

impl WriterRunner {
    fn new(
        file: &str,
        line: &str,
        release: Option<Arc<AtomicBool>>,
        unexpected: &Unexpected,
    ) -> Self {
        Self {
            file: file.to_string(),
            line: line.to_string(),
            started: Arc::new(AtomicBool::new(false)),
            release,
            resolver_calls: AtomicU32::new(0),
            unexpected: Arc::clone(unexpected),
        }
    }
}

impl Runner for WriterRunner {
    fn run(&self, spec: &RunnerSpec) -> RunnerResult {
        let cwd = PathBuf::from(&spec.cwd);
        if spec.cell_id.ends_with("-commit") {
            let dirty = !git(&cwd, &["status", "--porcelain"]).trim().is_empty();
            if dirty {
                git(&cwd, &["add", "--all"]);
                git(&cwd, &["commit", "-m", &format!("edit: {}", self.line)]);
            }
            return ok_result(
                if dirty {
                    "committed"
                } else {
                    "nothing to commit"
                },
                Some(dirty),
            );
        }
        if self.resolver_calls.fetch_add(1, Ordering::SeqCst) > 0 {
            self.unexpected
                .lock()
                .unwrap()
                .push(format!("{} ({})", spec.cell_id, self.line));
            return RunnerResult::failure("unexpected agent call");
        }
        let path = cwd.join(&self.file);
        let mut text = std::fs::read_to_string(&path).unwrap_or_default();
        text.push_str(&self.line);
        text.push('\n');
        std::fs::write(&path, text).expect("edit tracked file");
        self.started.store(true, Ordering::SeqCst);
        if let Some(release) = &self.release {
            let deadline = Instant::now() + Duration::from_secs(300);
            while !release.load(Ordering::SeqCst) {
                assert!(Instant::now() < deadline, "writer never released");
                std::thread::sleep(Duration::from_millis(10));
            }
        }
        ok_result("edited\nRALPHUS_PROOF: PASS", Some(true))
    }
}

impl Fixture {
    /// Open a PR for every branch, each already published at its review tip.
    fn open_pr_stack(&self) -> Vec<String> {
        (0..self.branch_ids.len())
            .map(|p| {
                let alias = format!("pr-{p}");
                let pr_id = self.open_pr(p, &alias);
                let tip = self.remote_tip(&alias);
                self.store
                    .lock()
                    .update_pull_request_ex(
                        &pr_id,
                        None,
                        None,
                        None,
                        None,
                        None,
                        Some(Some(&tip)),
                        None,
                    )
                    .unwrap();
                pr_id
            })
            .collect()
    }

    fn file_on(&self, repo: &Path, rev: &str, file: &str) -> String {
        std::process::Command::new("git")
            .args(["show", &format!("{rev}:{file}")])
            .current_dir(repo)
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
            .unwrap_or_default()
    }

    /// The review's status, base, and each branch's state and local/remote tips.
    fn describe(&self) -> String {
        let view = self.store.lock().get_guardian(&self.id).unwrap();
        let mut out = format!(
            "status={} detail={:?} base_commit={:?} main={}",
            view.status,
            view.detail,
            view.base_commit,
            git(&self.root, &["rev-parse", "main"]).trim()
        );
        for (p, b) in view.branches.iter().enumerate() {
            let local = b
                .review_branch
                .as_deref()
                .map(|r| git(&self.root, &["rev-parse", r]).trim().to_string())
                .unwrap_or_default();
            let remote = std::process::Command::new("git")
                .args(["rev-parse", &format!("refs/heads/pr-{p}")])
                .current_dir(&self.remote)
                .output()
                .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
                .unwrap_or_default();
            out.push_str(&format!(
                "\n  [{p}] {} {} detail={:?} local={local:.9} remote={remote:.9}",
                b.branch, b.merge_status, b.detail
            ));
        }
        out
    }

    /// Whether the review is idle and every PR branch on the remote holds
    /// exactly its review branch.
    fn quiescent(&self) -> bool {
        let view = self.store.lock().get_guardian(&self.id).unwrap();
        if view.status != "in_review" {
            return false;
        }
        (0..self.branch_ids.len()).all(|p| {
            let local = git(&self.root, &["rev-parse", &self.review_ref(p)]);
            let remote = std::process::Command::new("git")
                .args(["rev-parse", &format!("refs/heads/pr-{p}")])
                .current_dir(&self.remote)
                .output()
                .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
                .unwrap_or_default();
            local.trim() == remote.trim()
        })
    }
}

/// How often [`DaemonPump`] runs the daemon's background review work.
const PUMP_INTERVAL: Duration = Duration::from_millis(100);
/// How long a scenario waits after pushing upstream for the daemon to fetch
/// the new base and start (and park) its rebuild behind the held writers --
/// several [`PUMP_INTERVAL`]s.
const PARK_WAIT: Duration = Duration::from_millis(1500);
/// A review counts as settled after this many consecutive idle polls,
/// [`SETTLE_POLL`] apart.
const SETTLE_STABLE_POLLS: u32 = 8;
const SETTLE_POLL: Duration = Duration::from_millis(150);

/// The daemon's own background review work, running for real: the same two
/// calls `scheduler::run_loop` makes -- `poll_base_branch_freshness_once`
/// (fetch every maintained review's base from its remote) and
/// `review_maintenance` (pull reviewer commits into review branches, rebuild
/// on a base shift, restack after a manual push, publish PR branches, repair
/// the stack) -- every [`PUMP_INTERVAL`] instead of every 60 s / 5 s. Nothing
/// in it is faked; it spawns its own workers and, for any agent call, the
/// real runner, which none of these scenarios should need.
///
/// `review_maintenance` keeps in-process "already running" sets keyed by
/// review id, and every test's review is the first in its own store, so
/// these tests rely on nextest's one-process-per-test model (the repo's
/// standard runner) to keep their pumps apart.
struct DaemonPump {
    stop: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl DaemonPump {
    fn start(fx: &Fixture) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let (store, flag) = (Arc::clone(&fx.store), Arc::clone(&stop));
        let handle = std::thread::spawn(move || {
            let sem = Arc::new(Semaphore::new(4));
            let cancellations = ralphus_daemon::cancel::Cancellations::new();
            while !flag.load(Ordering::SeqCst) {
                ralphus_daemon::guardian_merge::poll_base_branch_freshness_once(&store);
                ralphus_daemon::guardian_merge::review_maintenance(&store, &sem, &cancellations);
                std::thread::sleep(PUMP_INTERVAL);
            }
        });
        Self {
            stop,
            handle: Some(handle),
        }
    }
}

impl Drop for DaemonPump {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

impl Fixture {
    /// Push a commit adding `file` to the upstream `main` on the bare
    /// `origin`, from an unrelated clone -- the review's own repo only learns
    /// of it through the daemon's base poll.
    fn push_upstream(&self, file: &str) -> String {
        let clone = temp_dir();
        let _ = std::fs::remove_dir_all(&clone);
        git(
            clone.parent().unwrap(),
            &[
                "clone",
                "-q",
                "--branch",
                "main",
                self.remote.to_str().unwrap(),
                clone.file_name().unwrap().to_str().unwrap(),
            ],
        );
        write(&clone, file, "upstream\n");
        git(&clone, &["add", file]);
        git(&clone, &["commit", "-m", &format!("upstream: {file}")]);
        git(&clone, &["push", "-q", "origin", "main"]);
        let sha = git(&clone, &["rev-parse", "HEAD"]).trim().to_string();
        let _ = std::fs::remove_dir_all(&clone);
        sha
    }

    /// Wait (while a [`DaemonPump`] runs) until the review has been rebuilt on
    /// `upstream` and has stayed idle with every PR branch published for
    /// [`SETTLE_STABLE_POLLS`] consecutive polls.
    fn wait_settled_on(&self, upstream: &str, what: &str) {
        let deadline = Instant::now() + Duration::from_secs(300);
        let mut stable = 0;
        while stable < SETTLE_STABLE_POLLS {
            assert!(
                Instant::now() < deadline,
                "{what}: review never settled on upstream {upstream:.9}: {}",
                self.describe()
            );
            let on_upstream = self
                .store
                .lock()
                .get_guardian(&self.id)
                .unwrap()
                .base_commit
                .is_some_and(|b| b.trim() == upstream);
            if on_upstream && self.quiescent() {
                stable += 1;
            } else {
                stable = 0;
            }
            std::thread::sleep(SETTLE_POLL);
        }
    }

    /// Assert that every `(position, file, line)` edit is on review branch
    /// `p` and on PR branch `pr-p` for every `p >= position`, along with every
    /// task's own file and every upstream file.
    fn assert_everything_published(
        &self,
        what: &str,
        edits: &[(usize, &str, &str)],
        upstream_files: &[&str],
    ) {
        let tasks: Vec<String> = self
            .store
            .lock()
            .get_guardian(&self.id)
            .unwrap()
            .branches
            .iter()
            .map(|b| format!("{}.txt", b.branch.replace('/', "-")))
            .collect();
        for p in 0..self.branch_ids.len() {
            let local = self.review_ref(p);
            let remote = format!("refs/heads/pr-{p}");
            for (repo, rev, which) in [
                (&self.root, local.as_str(), "review branch"),
                (&self.remote, remote.as_str(), "PR branch"),
            ] {
                for &(from, file, line) in edits {
                    if p >= from {
                        let content = self.file_on(repo, rev, file);
                        assert!(
                            content.contains(line),
                            "{what}: {which} {p} lost {line:?} ({file}: {content:?})\n{}",
                            self.describe()
                        );
                    }
                }
                let files = self.files_on(repo, rev);
                for f in upstream_files {
                    assert!(
                        files.contains(f),
                        "{what}: {which} {p} is missing upstream {f}"
                    );
                }
                for task in tasks.iter().take(p + 1) {
                    assert!(
                        files.contains(task.as_str()),
                        "{what}: {which} {p} lost task file {task}"
                    );
                }
            }
        }
    }
}

/// Start a feedback round (`runner`) on branch `position` on its own thread.
fn spawn_feedback(
    fx: &Fixture,
    position: usize,
    runner: &Arc<WriterRunner>,
) -> std::thread::JoinHandle<()> {
    let (store, id, bid, runner) = (
        Arc::clone(&fx.store),
        fx.id.clone(),
        fx.branch_ids[position].clone(),
        Arc::clone(runner),
    );
    std::thread::spawn(move || {
        run_feedback(
            &store,
            runner.as_ref(),
            &id,
            &bid,
            "please change this",
            None,
            false,
            &CancelToken::never(),
        );
    })
}

/// Start an unattended PR fix (`runner`) for PR `pr_id` on its own thread --
/// the same `dispatch_pr_auto_fix` the CI poll calls on a failing PR.
fn spawn_auto_fix(
    fx: &Fixture,
    pr_id: &str,
    runner: &Arc<WriterRunner>,
) -> std::thread::JoinHandle<()> {
    let (store, id, pr_id, runner) = (
        Arc::clone(&fx.store),
        fx.id.clone(),
        pr_id.to_string(),
        Arc::clone(runner),
    );
    std::thread::spawn(move || {
        let guardian = store.lock().get_guardian(&id).unwrap();
        let pr = store.lock().get_pull_request(&pr_id).unwrap();
        ralphus_daemon::ci_watch::dispatch_pr_auto_fix(
            &store,
            runner.as_ref(),
            &guardian,
            &pr,
            &ci_failure(),
            &gitlab_client(),
        );
    })
}

#[derive(Clone, Copy, Debug)]
enum Release {
    FeedbackFirst,
    FixFirst,
    Together,
}

/// A held writer: its runner, the thread running it, and its release flag.
struct Held {
    runner: Arc<WriterRunner>,
    release: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Held {
    fn new(file: &str, line: &str, unexpected: &Unexpected) -> Self {
        let release = Arc::new(AtomicBool::new(false));
        Self {
            runner: Arc::new(WriterRunner::new(
                file,
                line,
                Some(Arc::clone(&release)),
                unexpected,
            )),
            release,
            thread: None,
        }
    }

    fn feedback(mut self, fx: &Fixture, position: usize) -> Self {
        self.thread = Some(spawn_feedback(fx, position, &self.runner));
        wait_for(&self.runner.started, &self.runner.line);
        self
    }

    fn auto_fix(mut self, fx: &Fixture, pr_id: &str) -> Self {
        self.thread = Some(spawn_auto_fix(fx, pr_id, &self.runner));
        wait_for(&self.runner.started, &self.runner.line);
        self
    }

    fn finish(&mut self) {
        self.release.store(true, Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            thread.join().expect("writer thread");
        }
    }
}

impl Fixture {
    /// A reviewer pushes a commit adding `file` straight onto PR branch
    /// `pr-<position>`, from an unrelated clone. Like a real reviewer, a push
    /// rejected because the daemon republished the branch meanwhile is
    /// retried on top of the new tip.
    fn push_to_pr(&self, position: usize, file: &str) {
        let alias = format!("pr-{position}");
        let clone = temp_dir();
        let _ = std::fs::remove_dir_all(&clone);
        git(
            clone.parent().unwrap(),
            &[
                "clone",
                "-q",
                "--branch",
                &alias,
                self.remote.to_str().unwrap(),
                clone.file_name().unwrap().to_str().unwrap(),
            ],
        );
        write(&clone, file, "reviewer\n");
        git(&clone, &["add", file]);
        git(&clone, &["commit", "-m", &format!("reviewer: {file}")]);
        let mut pushed = false;
        for _ in 0..10 {
            let status = std::process::Command::new("git")
                .args(["push", "-q", "origin", &alias])
                .current_dir(&clone)
                .status()
                .expect("run git push");
            if status.success() {
                pushed = true;
                break;
            }
            git(&clone, &["pull", "-q", "--rebase", "origin", &alias]);
        }
        assert!(pushed, "reviewer push to {alias} kept being rejected");
        let _ = std::fs::remove_dir_all(&clone);
    }

    fn assert_no_unexpected_agent_calls(&self, what: &str, unexpected: &Unexpected) {
        let calls = unexpected.lock().unwrap().clone();
        assert!(
            calls.is_empty(),
            "{what}: a rebuild replayed commits it should not have (agent calls: {calls:?})"
        );
    }
}

/// Four stacked branches with a PR each, the daemon's own maintenance
/// running. While a feedback round edits branch 1 and an unattended PR fix
/// edits branch 2 (both held mid-edit), a feedback round on branch 0 lands
/// (queueing an upstream restack behind them) and a commit is pushed to the
/// upstream base, so the daemon's base poll fetches it and its maintenance
/// starts a base-shift rebuild. The held writers are then released in
/// `order`. Every edit, the upstream commit and every task's own commit must
/// end up on every PR branch they belong to.
fn upstream_rebase_with_feedback_and_auto_fix_in_flight(order: Release) {
    let what = format!("{order:?}");
    let fx = Fixture::with_upstream(&["feature/a", "feature/b", "feature/c", "feature/d"]);
    let pr_ids = fx.open_pr_stack();
    let unexpected: Unexpected = Arc::default();
    let _daemon = DaemonPump::start(&fx);

    let mut feedback1 = Held::new("feature-b.txt", "feedback on b", &unexpected).feedback(&fx, 1);
    let mut fix2 =
        Held::new("feature-c.txt", "auto-fix on c", &unexpected).auto_fix(&fx, &pr_ids[2]);
    let feedback0 = WriterRunner::new("feature-a.txt", "feedback on a", None, &unexpected);
    run_feedback(
        &fx.store,
        &feedback0,
        &fx.id,
        &fx.branch_ids[0],
        "change a",
        None,
        false,
        &CancelToken::never(),
    );
    let upstream = fx.push_upstream("upstream1.txt");
    // Long enough for the daemon to fetch the new base and start (and park)
    // its rebuild behind the held writers.
    std::thread::sleep(PARK_WAIT);

    match order {
        Release::FeedbackFirst => {
            feedback1.finish();
            fix2.finish();
        }
        Release::FixFirst => {
            fix2.finish();
            feedback1.finish();
        }
        Release::Together => {
            feedback1.release.store(true, Ordering::SeqCst);
            fix2.release.store(true, Ordering::SeqCst);
            feedback1.finish();
            fix2.finish();
        }
    }

    fx.wait_settled_on(&upstream, &what);
    fx.assert_no_unexpected_agent_calls(&what, &unexpected);
    fx.assert_everything_published(
        &what,
        &[
            (0, "feature-a.txt", "feedback on a"),
            (1, "feature-b.txt", "feedback on b"),
            (2, "feature-c.txt", "auto-fix on c"),
        ],
        &["upstream1.txt"],
    );
}

#[test]
fn upstream_rebase_with_feedback_and_auto_fix_in_flight_feedback_released_first() {
    upstream_rebase_with_feedback_and_auto_fix_in_flight(Release::FeedbackFirst);
}

#[test]
fn upstream_rebase_with_feedback_and_auto_fix_in_flight_fix_released_first() {
    upstream_rebase_with_feedback_and_auto_fix_in_flight(Release::FixFirst);
}

#[test]
fn upstream_rebase_with_feedback_and_auto_fix_in_flight_released_together() {
    upstream_rebase_with_feedback_and_auto_fix_in_flight(Release::Together);
}

/// Five inputs at once on a four-branch stack: two unattended PR fixes
/// (branches 1 and 3), two feedback rounds (branches 0 and 2) and a
/// reviewer's direct push to PR 0, all in flight while the upstream base
/// moves. Released from the bottom of the stack up (or top down), the
/// daemon must fold every one of them into every PR branch above it.
fn upstream_rebase_with_two_fixes_two_feedbacks_and_a_reviewer_push(bottom_up: bool) {
    {
        let what = if bottom_up { "bottom-up" } else { "top-down" };
        let fx = Fixture::with_upstream(&["feature/a", "feature/b", "feature/c", "feature/d"]);
        let pr_ids = fx.open_pr_stack();
        let unexpected: Unexpected = Arc::default();
        let _daemon = DaemonPump::start(&fx);

        let mut writers = vec![
            Held::new("feature-a.txt", "feedback on a", &unexpected).feedback(&fx, 0),
            Held::new("feature-b.txt", "auto-fix on b", &unexpected).auto_fix(&fx, &pr_ids[1]),
            Held::new("feature-c.txt", "feedback on c", &unexpected).feedback(&fx, 2),
            Held::new("feature-d.txt", "auto-fix on d", &unexpected).auto_fix(&fx, &pr_ids[3]),
        ];
        fx.push_to_pr(0, "reviewer-a.txt");
        let upstream = fx.push_upstream("upstream1.txt");
        std::thread::sleep(PARK_WAIT);
        if !bottom_up {
            writers.reverse();
        }
        for w in &mut writers {
            w.finish();
            std::thread::sleep(Duration::from_millis(300));
        }

        fx.wait_settled_on(&upstream, what);
        fx.assert_no_unexpected_agent_calls(what, &unexpected);
        let reviewer_line = "reviewer";
        fx.assert_everything_published(
            what,
            &[
                (0, "feature-a.txt", "feedback on a"),
                (0, "reviewer-a.txt", reviewer_line),
                (1, "feature-b.txt", "auto-fix on b"),
                (2, "feature-c.txt", "feedback on c"),
                (3, "feature-d.txt", "auto-fix on d"),
            ],
            &["upstream1.txt"],
        );
    }
}

#[test]
fn upstream_rebase_with_two_fixes_two_feedbacks_and_a_reviewer_push_released_bottom_up() {
    upstream_rebase_with_two_fixes_two_feedbacks_and_a_reviewer_push(true);
}

#[test]
fn upstream_rebase_with_two_fixes_two_feedbacks_and_a_reviewer_push_released_top_down() {
    upstream_rebase_with_two_fixes_two_feedbacks_and_a_reviewer_push(false);
}

/// The upstream base moves twice while a feedback round and a PR fix are in
/// flight -- once before either finishes, once between them.
#[test]
fn two_upstream_moves_while_feedback_and_auto_fix_are_in_flight() {
    let fx = Fixture::with_upstream(&["feature/a", "feature/b", "feature/c"]);
    let pr_ids = fx.open_pr_stack();
    let unexpected: Unexpected = Arc::default();
    let _daemon = DaemonPump::start(&fx);

    let mut feedback1 = Held::new("feature-b.txt", "feedback on b", &unexpected).feedback(&fx, 1);
    let mut fix2 =
        Held::new("feature-c.txt", "auto-fix on c", &unexpected).auto_fix(&fx, &pr_ids[2]);
    fx.push_upstream("upstream1.txt");
    std::thread::sleep(PARK_WAIT);
    feedback1.finish();
    let upstream = fx.push_upstream("upstream2.txt");
    std::thread::sleep(PARK_WAIT);
    fix2.finish();

    fx.wait_settled_on(&upstream, "two upstream moves");
    fx.assert_no_unexpected_agent_calls("two upstream moves", &unexpected);
    fx.assert_everything_published(
        "two upstream moves",
        &[
            (1, "feature-b.txt", "feedback on b"),
            (2, "feature-c.txt", "auto-fix on c"),
        ],
        &["upstream1.txt", "upstream2.txt"],
    );
}

/// Every branch already carries review-only commits (feedback, a PR fix, a
/// reviewer push) when the upstream base moves: the daemon's rebase must
/// carry all of them onto the new base and republish every PR.
#[test]
fn upstream_rebase_carries_every_branchs_review_only_commits() {
    let fx = Fixture::with_upstream(&["feature/a", "feature/b", "feature/c"]);
    let pr_ids = fx.open_pr_stack();
    let unexpected: Unexpected = Arc::default();
    let _daemon = DaemonPump::start(&fx);

    let mut fb0 = Held::new("feature-a.txt", "feedback on a", &unexpected).feedback(&fx, 0);
    fb0.finish();
    let mut fix1 =
        Held::new("feature-b.txt", "auto-fix on b", &unexpected).auto_fix(&fx, &pr_ids[1]);
    fix1.finish();
    fx.push_to_pr(2, "reviewer-c.txt");
    let first = fx
        .store
        .lock()
        .get_guardian(&fx.id)
        .unwrap()
        .base_commit
        .unwrap_or_default();
    fx.wait_settled_on(first.trim(), "before the upstream move");

    let upstream = fx.push_upstream("upstream1.txt");
    fx.wait_settled_on(&upstream, "after the upstream move");
    fx.assert_no_unexpected_agent_calls("upstream move", &unexpected);
    fx.assert_everything_published(
        "upstream move",
        &[
            (0, "feature-a.txt", "feedback on a"),
            (1, "feature-b.txt", "auto-fix on b"),
            (2, "reviewer-c.txt", "reviewer"),
        ],
        &["upstream1.txt"],
    );
}

/// Seeded stress: several rounds on one review. Each round holds a
/// pseudo-random set of writers (feedback or PR fix) on random branches,
/// pushes upstream, releases them in a random order, and lets the daemon
/// settle; every edit from every round must still be on every PR branch
/// above it at the end of each round.
#[test]
#[ignore = "multi-minute stress run; runs every PR in the CI perf-tests job"]
fn stress_upstream_rebases_with_random_concurrent_writers() {
    let fx = Fixture::with_upstream(&["feature/a", "feature/b", "feature/c", "feature/d"]);
    let pr_ids = fx.open_pr_stack();
    let unexpected: Unexpected = Arc::default();
    let _daemon = DaemonPump::start(&fx);
    // xorshift: deterministic across runs, no extra dependency.
    let mut seed: u64 = 0x9E37_79B9_7F4A_7C15;
    let mut next = move |n: u64| {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed % n
    };
    let mut edits: Vec<(usize, String, String)> = Vec::new();
    let mut upstream_files: Vec<String> = Vec::new();
    for round in 0..4 {
        let mut positions: Vec<usize> = (0..4).filter(|_| next(2) == 0).collect();
        if positions.is_empty() {
            positions.push(next(4) as usize);
        }
        let mut writers = Vec::new();
        for &p in &positions {
            let file = format!("feature-{}.txt", ["a", "b", "c", "d"][p]);
            let fix = next(2) == 0;
            let line = format!(
                "round {round} {} on {p}",
                if fix { "fix" } else { "feedback" }
            );
            let held = Held::new(&file, &line, &unexpected);
            writers.push(if fix {
                held.auto_fix(&fx, &pr_ids[p])
            } else {
                held.feedback(&fx, p)
            });
            edits.push((p, file, line));
        }
        let upstream_file = format!("upstream{round}.txt");
        let upstream = fx.push_upstream(&upstream_file);
        upstream_files.push(upstream_file);
        std::thread::sleep(Duration::from_millis(500 + 500 * next(4)));
        while !writers.is_empty() {
            let i = next(writers.len() as u64) as usize;
            writers.remove(i).finish();
            std::thread::sleep(Duration::from_millis(100 * next(5)));
        }
        let what = format!("stress round {round}");
        fx.wait_settled_on(&upstream, &what);
        fx.assert_no_unexpected_agent_calls(&what, &unexpected);
        let edit_refs: Vec<(usize, &str, &str)> = edits
            .iter()
            .map(|(p, f, l)| (*p, f.as_str(), l.as_str()))
            .collect();
        let upstream_refs: Vec<&str> = upstream_files.iter().map(String::as_str).collect();
        fx.assert_everything_published(&what, &edit_refs, &upstream_refs);
    }
}
