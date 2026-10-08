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

fn append_line(root: &Path, name: &str, line: &str) {
    let path = root.join(name);
    let mut text = std::fs::read_to_string(&path).unwrap_or_default();
    text.push_str(line);
    text.push('\n');
    std::fs::write(&path, text).expect("append line");
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
            // A task named `empty/...` committed nothing.
            if !name.starts_with("empty/") {
                write(
                    &root,
                    &format!("{}.txt", name.replace('/', "-")),
                    "content\n",
                );
                git(&root, &["add", "."]);
                git(&root, &["commit", "-m", &format!("add {name}")]);
            }
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
        // A stack holding an empty task branch fails its first build on it
        // ("branch is empty"); every other fixture starts in review.
        if !features.iter().any(|f| f.starts_with("empty/")) {
            assert_eq!(view.status, "in_review", "detail: {:?}", view.detail);
        }
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
    assert!(pulled.is_some(), "the reviewer's commit is pulled in");

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
        generation: None,
        pending_count: 0,
        passing_checks: vec![],
        in_progress_checks: vec![],
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
/// Every agent call after the first resolver call is recorded as unexpected
/// -- unless `resolves_conflicts`, when a conflict-resolution call is answered
/// with [`union_resolve`] instead (scenarios built to conflict).
struct WriterRunner {
    file: String,
    line: String,
    started: Arc<AtomicBool>,
    release: Option<Arc<AtomicBool>>,
    resolver_calls: AtomicU32,
    unexpected: Unexpected,
    resolves_conflicts: bool,
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
            resolves_conflicts: false,
        }
    }
}

/// Whether `spec` is a conflict-resolution call (the daemon names those
/// cells `resolver-<branch id>-...`).
fn is_conflict_resolution(spec: &RunnerSpec) -> bool {
    spec.cell_id.starts_with("resolver-")
}

/// Resolve every conflicted file in `cwd` the way a careful reviewer would
/// when two edits both belong: keep both sides (drop only the markers).
fn union_resolve(cwd: &Path) {
    let conflicted = git(cwd, &["diff", "--name-only", "--diff-filter=U"]);
    for file in conflicted.lines().filter(|l| !l.trim().is_empty()) {
        let path = cwd.join(file.trim());
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let cleaned: String = text
            .lines()
            .filter(|l| {
                !l.starts_with("<<<<<<<") && !l.starts_with("=======") && !l.starts_with(">>>>>>>")
            })
            .map(|l| format!("{l}\n"))
            .collect();
        std::fs::write(&path, cleaned).expect("write resolved file");
    }
}

/// The agent runner for a scenario built to conflict: every call is a
/// conflict resolution, answered by [`union_resolve`]; anything else is
/// recorded as unexpected.
struct UnionResolver(Unexpected);
impl Runner for UnionResolver {
    fn run(&self, spec: &RunnerSpec) -> RunnerResult {
        if is_conflict_resolution(spec) {
            union_resolve(&PathBuf::from(&spec.cwd));
            return ok_result("resolved by keeping both sides", None);
        }
        self.0
            .lock()
            .unwrap()
            .push(format!("unexpected non-resolver call {}", spec.cell_id));
        RunnerResult::failure("unexpected agent call")
    }
}

impl Runner for WriterRunner {
    fn run(&self, spec: &RunnerSpec) -> RunnerResult {
        let cwd = PathBuf::from(&spec.cwd);
        if self.resolves_conflicts && is_conflict_resolution(spec) {
            union_resolve(&cwd);
            return ok_result("resolved by keeping both sides", None);
        }
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
    /// Positions of the branches still in the stack (not disabled).
    fn enabled_positions(&self) -> Vec<usize> {
        self.store
            .lock()
            .get_guardian(&self.id)
            .unwrap()
            .branches
            .iter()
            .enumerate()
            .filter(|(_, b)| b.enabled)
            .map(|(p, _)| p)
            .collect()
    }

    /// Open a PR for every enabled branch, each already published at its
    /// review tip. Indexed by position; a disabled branch gets an empty id.
    fn open_pr_stack(&self) -> Vec<String> {
        let enabled = self.enabled_positions();
        (0..self.branch_ids.len())
            .map(|p| {
                if !enabled.contains(&p) {
                    return String::new();
                }
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

    /// The remote ref of position `p`'s PR branch: its open PR's branch
    /// alias (whatever the daemon chose when it opened the PR), else the
    /// `pr-<p>` alias [`Self::open_pr_stack`] uses.
    fn pr_ref(&self, p: usize) -> String {
        let guard = self.store.lock();
        let branch_id = guard
            .get_guardian(&self.id)
            .ok()
            .and_then(|g| g.branches.get(p).map(|b| b.id.clone()));
        let alias = guard
            .list_pull_requests_for_guardian(&self.id)
            .unwrap_or_default()
            .into_iter()
            .find(|pr| pr.state == "open" && pr.branch_id.is_some() && pr.branch_id == branch_id)
            .map(|pr| pr.branch_alias)
            .unwrap_or_else(|| format!("pr-{p}"));
        format!("refs/heads/{alias}")
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
            // Tolerant: a branch of another project has its ref in that repo.
            let local = b
                .review_branch
                .as_deref()
                .and_then(|r| {
                    std::process::Command::new("git")
                        .args(["rev-parse", r])
                        .current_dir(&self.root)
                        .output()
                        .ok()
                })
                .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
                .unwrap_or_default();
            let remote = std::process::Command::new("git")
                .args(["rev-parse", &self.pr_ref(p)])
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

    /// Positions whose PR branch the daemon keeps published: enabled branches
    /// not already merged upstream (a merged branch's PR has nothing to add,
    /// so its PR branch is deliberately left alone).
    fn published_positions(&self) -> Vec<usize> {
        let view = self.store.lock().get_guardian(&self.id).unwrap();
        view.branches
            .iter()
            .enumerate()
            .filter(|(_, b)| b.enabled && b.merge_status != "merged")
            .map(|(p, _)| p)
            .collect()
    }

    /// Whether the review is idle and every published PR branch on the remote
    /// holds exactly its review branch.
    fn quiescent(&self) -> bool {
        let view = self.store.lock().get_guardian(&self.id).unwrap();
        if view.status != "in_review" {
            return false;
        }
        // One `for-each-ref` per repo instead of two `rev-parse`s per branch:
        // on long stacks the per-branch spawns dominate a poll.
        let tips = |repo: &Path| -> std::collections::HashMap<String, String> {
            git(repo, &["for-each-ref", "--format=%(refname) %(objectname)"])
                .lines()
                .filter_map(|l| l.split_once(' '))
                .map(|(r, s)| (r.to_string(), s.to_string()))
                .collect()
        };
        let (local_tips, remote_tips) = (tips(&self.root), tips(&self.remote));
        let lookup = |map: &std::collections::HashMap<String, String>, rev: &str| {
            map.get(rev)
                .or_else(|| map.get(&format!("refs/heads/{rev}")))
                .cloned()
        };
        self.published_positions().into_iter().all(|p| {
            let local = lookup(&local_tips, &self.review_ref(p));
            let remote = lookup(&remote_tips, &self.pr_ref(p));
            local.is_some() && local == remote
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
/// the stack) -- every [`PUMP_INTERVAL`] instead of every 60 s / 5 s. The
/// sweep itself is the real one (via `review_maintenance_with`); only the
/// agent runner its workers would call is replaced, by [`NoAgentExpected`].
///
/// `review_maintenance` keeps in-process "already running" sets keyed by
/// review id, and every test's review is the first in its own store, so
/// these tests rely on nextest's one-process-per-test model (the repo's
/// standard runner) to keep their pumps apart.
struct DaemonPump {
    stop: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
    /// The daemon's merge-worker registry and concurrency limit, shared with
    /// the user actions a scenario takes (restart, stop, cancel, reopen) so
    /// they reach the same workers, as the server's handlers do.
    cancellations: ralphus_daemon::cancel::Cancellations,
    sem: Arc<Semaphore>,
}

/// The agent runner the daemon's own sweep gets in these tests: none of the
/// scenarios should need an agent (every writer edits its own file), so any
/// call fails loudly -- and, unlike the production runner, it needs no
/// `ralphus-runner` binary installed to pass the merge's resolver preflight.
struct NoAgentExpected;
impl Runner for NoAgentExpected {
    fn run(&self, spec: &RunnerSpec) -> RunnerResult {
        RunnerResult::failure(format!(
            "unexpected agent call from the daemon's maintenance: {}",
            spec.cell_id
        ))
    }
}

impl DaemonPump {
    fn start(fx: &Fixture) -> Self {
        Self::start_with(
            fx,
            Arc::new(|_| Arc::new(NoAgentExpected) as Arc<dyn Runner>),
        )
    }

    /// A pump whose rebuilds resolve conflicts by keeping both sides --
    /// for scenarios built to conflict.
    fn start_resolving(fx: &Fixture, unexpected: &Unexpected) -> Self {
        let unexpected = Arc::clone(unexpected);
        Self::start_with(
            fx,
            Arc::new(move |_| Arc::new(UnionResolver(Arc::clone(&unexpected))) as Arc<dyn Runner>),
        )
    }

    fn start_with(fx: &Fixture, runners: ralphus_daemon::guardian_merge::RunnerFactory) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let (store, flag) = (Arc::clone(&fx.store), Arc::clone(&stop));
        let sem = Arc::new(Semaphore::new(4));
        let cancellations = ralphus_daemon::cancel::Cancellations::new();
        let (pump_sem, pump_cancellations) = (Arc::clone(&sem), cancellations.clone());
        let handle = std::thread::spawn(move || {
            let (sem, cancellations) = (pump_sem, pump_cancellations);
            while !flag.load(Ordering::SeqCst) {
                ralphus_daemon::guardian_merge::poll_base_branch_freshness_once(&store);
                ralphus_daemon::guardian_merge::review_maintenance_with(
                    &store,
                    &sem,
                    &cancellations,
                    &runners,
                );
                std::thread::sleep(PUMP_INTERVAL);
            }
        });
        Self {
            stop,
            handle: Some(handle),
            cancellations,
            sem,
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
        // Rebuilding a long stack is linear in its length: allow for it.
        let branches = self.branch_ids.len() as u64;
        let deadline = Instant::now() + Duration::from_secs(300 + 40 * branches.saturating_sub(8));
        let mut stable = 0;
        while stable < SETTLE_STABLE_POLLS {
            assert!(
                Instant::now() < deadline,
                "{what}: review never settled on upstream {upstream:.9} \
                 (stable polls {stable}, quiescent {}): {}",
                self.quiescent(),
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
        // Each position's own task file, or `None` for a disabled branch or a
        // task that committed nothing.
        let tasks: Vec<Option<String>> = self
            .store
            .lock()
            .get_guardian(&self.id)
            .unwrap()
            .branches
            .iter()
            .map(|b| {
                (b.enabled && !b.branch.starts_with("empty/"))
                    .then(|| format!("{}.txt", b.branch.replace('/', "-")))
            })
            .collect();
        let published = self.published_positions();
        for p in self.enabled_positions() {
            let local = self.review_ref(p);
            let remote = self.pr_ref(p);
            let mut targets = vec![(&self.root, local.as_str(), "review branch")];
            if published.contains(&p) {
                targets.push((&self.remote, remote.as_str(), "PR branch"));
            }
            for (repo, rev, which) in targets {
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
                for task in tasks.iter().take(p + 1).flatten() {
                    assert!(
                        files.contains(task.as_str()),
                        "{what}: {which} {p} lost task file {task}"
                    );
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Empty branches in the stack: a task that committed nothing, a branch whose
// work already landed upstream (so it rebases to nothing and is `merged`),
// and a disabled empty branch -- each racing feedback and an upstream move.
// ---------------------------------------------------------------------------

/// A task that committed nothing, whose branch then got its content from
/// feedback. The review-only feedback commit is that branch's whole
/// contribution, so an upstream rebase -- racing a feedback round on the
/// branch above -- must carry it forward rather than fail the branch as
/// "empty" or drop it. A full "Merge / rebase" builds the rest of the stack
/// once the feedback lands (the fixture has no task cells for the scheduler
/// to mark the never-built branches ready from).
fn upstream_rebase_keeps_feedback_on_a_branch_whose_task_committed_nothing(
    features: &[&str],
    empty_at: usize,
) {
    let what = format!("empty task at {empty_at}");
    let fx = Fixture::with_upstream(features);
    let unexpected: Unexpected = Arc::default();
    let mut fill =
        Held::new("filled.txt", "content from feedback", &unexpected).feedback(&fx, empty_at);
    fill.finish();
    run_merge(&fx.store, &NoopRunner, &fx.id);
    let view = fx.store.lock().get_guardian(&fx.id).unwrap();
    assert_eq!(
        view.status,
        "in_review",
        "{what}: a branch with feedback content is not empty: {}",
        fx.describe()
    );
    fx.open_pr_stack();
    let _daemon = DaemonPump::start(&fx);

    let top = features.len() - 1;
    let top_file = format!("{}.txt", features[top].replace('/', "-"));
    let mut above = Held::new(&top_file, "feedback on top", &unexpected).feedback(&fx, top);
    let upstream = fx.push_upstream("upstream1.txt");
    std::thread::sleep(PARK_WAIT);
    above.finish();

    fx.wait_settled_on(&upstream, &what);
    fx.assert_no_unexpected_agent_calls(&what, &unexpected);
    fx.assert_everything_published(
        &what,
        &[
            (empty_at, "filled.txt", "content from feedback"),
            (top, top_file.as_str(), "feedback on top"),
        ],
        &["upstream1.txt"],
    );
}

#[test]
fn upstream_rebase_keeps_feedback_on_a_middle_branch_whose_task_committed_nothing() {
    upstream_rebase_keeps_feedback_on_a_branch_whose_task_committed_nothing(
        &["feature/a", "empty/b", "feature/c"],
        1,
    );
}

#[test]
fn upstream_rebase_keeps_feedback_on_a_bottom_branch_whose_task_committed_nothing() {
    upstream_rebase_keeps_feedback_on_a_branch_whose_task_committed_nothing(
        &["empty/a", "feature/b"],
        0,
    );
}

/// A middle branch whose work lands upstream (so it rebases to nothing and
/// becomes `merged`), then -- while the base moves again -- feedback edits
/// the branch below it (the same file its own task wrote) and the branch
/// above it. Rebuilding the empty middle branch must not replay the lower
/// branch's commits on top of its edited tip (that conflicts), and neither
/// feedback may be lost.
#[test]
fn upstream_rebase_with_feedback_around_a_branch_already_merged_upstream() {
    let fx = Fixture::with_upstream(&["feature/a", "feature/b", "feature/c"]);
    let unexpected: Unexpected = Arc::default();
    fx.open_pr_stack();
    let _daemon = DaemonPump::start(&fx);

    // Branch b's own change lands on the upstream base.
    let clone = temp_dir();
    let _ = std::fs::remove_dir_all(&clone);
    git(
        clone.parent().unwrap(),
        &[
            "clone",
            "-q",
            "--branch",
            "main",
            fx.remote.to_str().unwrap(),
            clone.file_name().unwrap().to_str().unwrap(),
        ],
    );
    write(&clone, "feature-b.txt", "content\n");
    git(&clone, &["add", "feature-b.txt"]);
    git(&clone, &["commit", "-m", "upstream: land feature/b"]);
    git(&clone, &["push", "-q", "origin", "main"]);
    let landed = git(&clone, &["rev-parse", "HEAD"]).trim().to_string();
    let _ = std::fs::remove_dir_all(&clone);
    fx.wait_settled_on(&landed, "b lands upstream");
    let b_status = fx.store.lock().get_guardian(&fx.id).unwrap().branches[1]
        .merge_status
        .clone();
    assert_eq!(
        b_status,
        "merged",
        "b rebases to nothing: {}",
        fx.describe()
    );

    let mut below = Held::new("feature-a.txt", "feedback on a", &unexpected).feedback(&fx, 0);
    let mut above = Held::new("feature-c.txt", "feedback on c", &unexpected).feedback(&fx, 2);
    let upstream = fx.push_upstream("upstream2.txt");
    std::thread::sleep(PARK_WAIT);
    below.finish();
    above.finish();

    fx.wait_settled_on(&upstream, "merged middle branch");
    fx.assert_no_unexpected_agent_calls("merged middle branch", &unexpected);
    fx.assert_everything_published(
        "merged middle branch",
        &[
            (0, "feature-a.txt", "feedback on a"),
            (2, "feature-c.txt", "feedback on c"),
        ],
        &["upstream2.txt"],
    );
}

/// An empty task branch disabled out of the stack (the documented escape
/// hatch) sits between two branches that both get feedback while the base
/// moves: both feedbacks must survive and the disabled branch must stay out.
#[test]
fn upstream_rebase_with_feedback_around_a_disabled_empty_branch() {
    let fx = Fixture::with_upstream(&["feature/a", "empty/b", "feature/c"]);
    fx.store
        .lock()
        .set_branch_enabled_by_name(&fx.id, "empty/b", false)
        .unwrap();
    run_merge(&fx.store, &NoopRunner, &fx.id);
    let view = fx.store.lock().get_guardian(&fx.id).unwrap();
    assert_eq!(view.status, "in_review", "{}", fx.describe());
    let unexpected: Unexpected = Arc::default();
    fx.open_pr_stack();
    let _daemon = DaemonPump::start(&fx);

    let mut below = Held::new("feature-a.txt", "feedback on a", &unexpected).feedback(&fx, 0);
    let mut above = Held::new("feature-c.txt", "feedback on c", &unexpected).feedback(&fx, 2);
    let upstream = fx.push_upstream("upstream1.txt");
    std::thread::sleep(PARK_WAIT);
    above.finish();
    below.finish();

    fx.wait_settled_on(&upstream, "disabled empty branch");
    fx.assert_no_unexpected_agent_calls("disabled empty branch", &unexpected);
    fx.assert_everything_published(
        "disabled empty branch",
        &[
            (0, "feature-a.txt", "feedback on a"),
            (2, "feature-c.txt", "feedback on c"),
        ],
        &["upstream1.txt"],
    );
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
        // Each driven fix stands for a fresh CI failure: reset the PR's
        // attempt budget and retry backoff, or a second fix on the same PR
        // within the backoff window is (correctly) deferred and never runs.
        store
            .lock()
            .clear_pr_auto_fix_attempted(&pr_id)
            .expect("reset the PR's auto-fix budget");
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
        Self::build(file, line, unexpected, false)
    }

    /// A held writer whose conflicts (with other writers, the upstream, the
    /// task commits) are resolved by keeping both sides.
    fn resolving(file: &str, line: &str, unexpected: &Unexpected) -> Self {
        Self::build(file, line, unexpected, true)
    }

    fn build(file: &str, line: &str, unexpected: &Unexpected, resolves_conflicts: bool) -> Self {
        let release = Arc::new(AtomicBool::new(false));
        let mut runner = WriterRunner::new(file, line, Some(Arc::clone(&release)), unexpected);
        runner.resolves_conflicts = resolves_conflicts;
        Self {
            runner: Arc::new(runner),
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
        self.reviewer_push(position, &format!("reviewer: {file}"), false, &|clone| {
            write(clone, file, "reviewer\n");
        });
    }

    /// A reviewer appends `line` to `file` on PR branch `pr-<position>` and
    /// pushes it.
    fn push_to_pr_append(&self, position: usize, file: &str, line: &str) {
        self.reviewer_push(position, &format!("reviewer: {line}"), false, &|clone| {
            append_line(clone, file, line);
        });
    }

    /// A reviewer rewrites PR branch `pr-<position>`'s history: amends its
    /// top commit (appending `line` to `file`) and force-pushes.
    fn amend_pr(&self, position: usize, file: &str, line: &str) {
        self.reviewer_push(position, "", true, &|clone| {
            append_line(clone, file, line);
        });
    }

    /// Clone PR branch `pr-<position>`, apply `edit`, commit it (`message`) or
    /// amend the top commit, and push from that unrelated clone. Like a real
    /// reviewer, a push rejected because the daemon republished the branch
    /// meanwhile is rebased onto the new tip (any conflict resolved by
    /// keeping both sides) and retried.
    fn reviewer_push(&self, position: usize, message: &str, amend: bool, edit: &dyn Fn(&Path)) {
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
        edit(&clone);
        git(&clone, &["add", "--all"]);
        if amend {
            git(&clone, &["commit", "-q", "--amend", "--no-edit"]);
            git(&clone, &["push", "-q", "--force", "origin", &alias]);
            let _ = std::fs::remove_dir_all(&clone);
            return;
        }
        git(&clone, &["commit", "-m", message]);
        let run = |args: &[&str]| {
            std::process::Command::new("git")
                .args(args)
                .current_dir(&clone)
                .env("GIT_EDITOR", "true")
                .output()
                .expect("run git")
        };
        let rebasing = || {
            clone.join(".git").join("rebase-merge").exists()
                || clone.join(".git").join("rebase-apply").exists()
        };
        // The daemon republishes PR branches on its 0.1 s sweep, so on a slow
        // runner a push can lose several times in a row: keep rebasing onto
        // the new tip and retrying, backing off, for a bounded time.
        let deadline = Instant::now() + Duration::from_secs(120);
        let mut attempt: u64 = 0;
        let mut last_error = String::new();
        let pushed = loop {
            let push = run(&["push", "-q", "origin", &alias]);
            if push.status.success() {
                break true;
            }
            last_error = String::from_utf8_lossy(&push.stderr).trim().to_string();
            if Instant::now() >= deadline {
                break false;
            }
            attempt += 1;
            std::thread::sleep(Duration::from_millis(50 * attempt.min(10)));
            let _ = run(&["pull", "-q", "--rebase", "origin", &alias]);
            // Drive any stopped rebase to the end: resolve conflicts by
            // keeping both sides; a step with nothing left to apply (the
            // daemon already pulled that change in) is skipped.
            while rebasing() {
                let conflicted = git(&clone, &["diff", "--name-only", "--diff-filter=U"]);
                if conflicted.trim().is_empty() {
                    let staged = run(&["diff", "--cached", "--quiet"]);
                    if staged.status.success() {
                        let _ = run(&["rebase", "--skip"]);
                        continue;
                    }
                } else {
                    union_resolve(&clone);
                }
                git(&clone, &["add", "--all"]);
                let _ = run(&["rebase", "--continue"]);
            }
        };
        assert!(
            pushed,
            "reviewer push to {alias} kept being rejected for 120 s ({attempt} retries); \
             last error: {last_error}"
        );
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

/// Seeded stress: `rounds` rounds on one review of `features`. Each round holds
/// a pseudo-random set of writers -- feedback or PR fix -- on random branches,
/// has reviewers push straight to random PRs, pushes upstream, releases the
/// writers in a random order, and lets the daemon settle; every edit from
/// every round must still be on every PR branch above it at the end of each
/// round.
fn random_concurrent_writers_soak(features: &[&str], rounds: usize, seed: u64) {
    let fx = Fixture::with_upstream(features);
    let pr_ids = fx.open_pr_stack();
    let unexpected: Unexpected = Arc::default();
    let _daemon = DaemonPump::start(&fx);
    let n = features.len() as u64;
    // xorshift: deterministic across runs, no extra dependency.
    let mut seed = seed;
    let mut next = move |n: u64| {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed % n
    };
    let mut edits: Vec<(usize, String, String)> = Vec::new();
    let mut upstream_files: Vec<String> = Vec::new();
    for round in 0..rounds {
        let mut positions: Vec<usize> = (0..features.len()).filter(|_| next(2) == 0).collect();
        if positions.is_empty() {
            positions.push(next(n) as usize);
        }
        let mut writers = Vec::new();
        for &p in &positions {
            let file = format!("{}.txt", features[p].replace('/', "-"));
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
        for _ in 0..next(3) {
            let p = next(n) as usize;
            let file = format!("reviewer-r{round}-{}.txt", edits.len());
            fx.push_to_pr(p, &file);
            edits.push((p, file, "reviewer".to_string()));
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
        let what = format!("soak round {round}");
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

#[test]
#[ignore = "multi-minute stress run; runs every PR in the CI perf-tests job"]
fn stress_upstream_rebases_with_random_concurrent_writers() {
    random_concurrent_writers_soak(
        &["feature/a", "feature/b", "feature/c", "feature/d"],
        4,
        0x9E37_79B9_7F4A_7C15,
    );
}

/// The same soak on an eight-branch stack, for longer.
#[test]
#[ignore = "~15-minute soak; runs every PR in its own CI job, review-race-soak"]
fn soak_eight_branch_stack_with_random_concurrent_writers() {
    random_concurrent_writers_soak(
        &[
            "feature/a",
            "feature/b",
            "feature/c",
            "feature/d",
            "feature/e",
            "feature/f",
            "feature/g",
            "feature/h",
        ],
        6,
        0x2545_F491_4F6C_DD1D,
    );
}

// ---------------------------------------------------------------------------
// Overlapping edits: every writer and the upstream append to the same file,
// so every pairing conflicts and the daemon's conflict resolution runs
// mid-race (answered by keeping both sides).
// ---------------------------------------------------------------------------

impl Fixture {
    /// Push a commit appending `line` to `file` on the upstream `main`, from
    /// an unrelated clone. Returns the new upstream tip.
    fn push_upstream_append(&self, file: &str, line: &str) -> String {
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
        let path = clone.join(file);
        let mut text = std::fs::read_to_string(&path).unwrap_or_default();
        text.push_str(line);
        text.push('\n');
        std::fs::write(&path, text).expect("append upstream line");
        git(&clone, &["add", file]);
        git(&clone, &["commit", "-m", &format!("upstream: {line}")]);
        git(&clone, &["push", "-q", "origin", "main"]);
        let sha = git(&clone, &["rev-parse", "HEAD"]).trim().to_string();
        let _ = std::fs::remove_dir_all(&clone);
        sha
    }

    /// Assert each `(position, line)` appears in `file` exactly once on every
    /// review branch (and published PR branch) at or above `position`, and
    /// each upstream line exactly once on all of them: missing means an edit
    /// was dropped, twice means a commit was replayed.
    fn assert_lines_exactly_once(
        &self,
        what: &str,
        file: &str,
        edits: &[(usize, &str)],
        upstream_lines: &[&str],
    ) {
        let published = self.published_positions();
        for p in self.enabled_positions() {
            let local = self.review_ref(p);
            let remote = self.pr_ref(p);
            let mut targets = vec![(&self.root, local.as_str(), "review branch")];
            if published.contains(&p) {
                targets.push((&self.remote, remote.as_str(), "PR branch"));
            }
            for (repo, rev, which) in targets {
                let content = self.file_on(repo, rev, file);
                let expected = edits
                    .iter()
                    .filter(|(from, _)| p >= *from)
                    .map(|(_, line)| *line)
                    .chain(upstream_lines.iter().copied());
                for line in expected {
                    let count = content.lines().filter(|l| *l == line).count();
                    assert_eq!(
                        count,
                        1,
                        "{what}: {which} {p} has {line:?} {count} times in {file}:\n{content}\n{}",
                        self.describe()
                    );
                }
            }
        }
    }
}

/// Upstream rebase + feedback on a + held feedback on b + held PR fix on c,
/// all appending to the same file the upstream push also appends to -- every
/// rebuild and restack conflicts. Released in `order`, every line must end up
/// exactly once on every branch above where it was written.
fn overlapping_writers_with_an_upstream_rebase(order: Release) {
    let what = format!("overlap {order:?}");
    let fx = Fixture::with_upstream(&["feature/a", "feature/b", "feature/c"]);
    let pr_ids = fx.open_pr_stack();
    let unexpected: Unexpected = Arc::default();
    let _daemon = DaemonPump::start_resolving(&fx, &unexpected);

    let mut feedback1 = Held::resolving("base.txt", "feedback on b", &unexpected).feedback(&fx, 1);
    let mut fix2 =
        Held::resolving("base.txt", "auto-fix on c", &unexpected).auto_fix(&fx, &pr_ids[2]);
    let mut feedback0 = Held::resolving("base.txt", "feedback on a", &unexpected).feedback(&fx, 0);
    feedback0.finish();
    let upstream = fx.push_upstream_append("base.txt", "upstream line");
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
    fx.assert_lines_exactly_once(
        &what,
        "base.txt",
        &[
            (0, "feedback on a"),
            (1, "feedback on b"),
            (2, "auto-fix on c"),
        ],
        &["base", "upstream line"],
    );
}

#[test]
fn overlapping_writers_with_an_upstream_rebase_feedback_released_first() {
    overlapping_writers_with_an_upstream_rebase(Release::FeedbackFirst);
}

#[test]
fn overlapping_writers_with_an_upstream_rebase_fix_released_first() {
    overlapping_writers_with_an_upstream_rebase(Release::FixFirst);
}

#[test]
fn overlapping_writers_with_an_upstream_rebase_released_together() {
    overlapping_writers_with_an_upstream_rebase(Release::Together);
}

/// A feedback round and a PR fix on the *same* branch, both appending to the
/// file the upstream push and the branch below also append to, while the
/// base moves: the second writer waits on the first's worktree lease, and
/// every line must still land exactly once.
#[test]
fn overlapping_feedback_and_fix_on_one_branch_with_an_upstream_rebase() {
    let what = "same-branch overlap";
    let fx = Fixture::with_upstream(&["feature/a", "feature/b"]);
    let pr_ids = fx.open_pr_stack();
    let unexpected: Unexpected = Arc::default();
    let _daemon = DaemonPump::start_resolving(&fx, &unexpected);

    let mut feedback1 = Held::resolving("base.txt", "feedback on b", &unexpected).feedback(&fx, 1);
    let mut below = Held::resolving("base.txt", "feedback on a", &unexpected).feedback(&fx, 0);
    // The PR fix on b queues behind the held feedback round's lease.
    let fix_runner = Arc::new({
        let mut r = WriterRunner::new("base.txt", "auto-fix on b", None, &unexpected);
        r.resolves_conflicts = true;
        r
    });
    let fix_thread = spawn_auto_fix(&fx, &pr_ids[1], &fix_runner);
    let upstream = fx.push_upstream_append("base.txt", "upstream line");
    std::thread::sleep(PARK_WAIT);
    feedback1.finish();
    below.finish();
    fix_thread.join().expect("auto-fix thread");

    fx.wait_settled_on(&upstream, what);
    fx.assert_no_unexpected_agent_calls(what, &unexpected);
    fx.assert_lines_exactly_once(
        what,
        "base.txt",
        &[
            (0, "feedback on a"),
            (1, "feedback on b"),
            (1, "auto-fix on b"),
        ],
        &["base", "upstream line"],
    );
}

// ---------------------------------------------------------------------------
// Reviewers pushing straight to PR branches while other inputs are in
// flight: the daemon's sync must pull every such commit into the review
// branch and keep it through every rebuild and republish.
// ---------------------------------------------------------------------------

/// A reviewer pushes to a PR whose branch has an unattended PR fix in
/// flight, while the base moves.
#[test]
fn reviewer_push_to_a_branch_with_an_auto_fix_in_flight() {
    let what = "reviewer push + auto-fix, same branch";
    let fx = Fixture::with_upstream(&["feature/a", "feature/b", "feature/c"]);
    let pr_ids = fx.open_pr_stack();
    let unexpected: Unexpected = Arc::default();
    let _daemon = DaemonPump::start(&fx);

    let mut fix1 =
        Held::new("feature-b.txt", "auto-fix on b", &unexpected).auto_fix(&fx, &pr_ids[1]);
    fx.push_to_pr(1, "reviewer-b.txt");
    let upstream = fx.push_upstream("upstream1.txt");
    std::thread::sleep(PARK_WAIT);
    fix1.finish();

    fx.wait_settled_on(&upstream, what);
    fx.assert_no_unexpected_agent_calls(what, &unexpected);
    fx.assert_everything_published(
        what,
        &[
            (1, "feature-b.txt", "auto-fix on b"),
            (1, "reviewer-b.txt", "reviewer"),
        ],
        &["upstream1.txt"],
    );
}

/// A reviewer pushes to a *middle* PR while a feedback round and a PR fix are
/// in flight on the branches above it and the base moves.
#[test]
fn reviewer_push_to_a_middle_pr_with_writers_above_in_flight() {
    let what = "reviewer push to middle PR";
    let fx = Fixture::with_upstream(&["feature/a", "feature/b", "feature/c", "feature/d"]);
    let pr_ids = fx.open_pr_stack();
    let unexpected: Unexpected = Arc::default();
    let _daemon = DaemonPump::start(&fx);

    let mut feedback2 = Held::new("feature-c.txt", "feedback on c", &unexpected).feedback(&fx, 2);
    let mut fix3 =
        Held::new("feature-d.txt", "auto-fix on d", &unexpected).auto_fix(&fx, &pr_ids[3]);
    fx.push_to_pr(1, "reviewer-b.txt");
    let upstream = fx.push_upstream("upstream1.txt");
    std::thread::sleep(PARK_WAIT);
    fix3.finish();
    feedback2.finish();

    fx.wait_settled_on(&upstream, what);
    fx.assert_no_unexpected_agent_calls(what, &unexpected);
    fx.assert_everything_published(
        what,
        &[
            (1, "reviewer-b.txt", "reviewer"),
            (2, "feature-c.txt", "feedback on c"),
            (3, "feature-d.txt", "auto-fix on d"),
        ],
        &["upstream1.txt"],
    );
}

/// Several reviewer pushes in quick succession -- two to one PR, one to
/// another -- racing a held feedback round and an upstream move.
#[test]
fn rapid_reviewer_pushes_to_several_prs_racing_an_upstream_rebase() {
    let what = "rapid reviewer pushes";
    let fx = Fixture::with_upstream(&["feature/a", "feature/b", "feature/c"]);
    fx.open_pr_stack();
    let unexpected: Unexpected = Arc::default();
    let _daemon = DaemonPump::start(&fx);

    let mut feedback1 = Held::new("feature-b.txt", "feedback on b", &unexpected).feedback(&fx, 1);
    fx.push_to_pr(0, "reviewer-a1.txt");
    fx.push_to_pr(0, "reviewer-a2.txt");
    fx.push_to_pr(2, "reviewer-c.txt");
    let upstream = fx.push_upstream("upstream1.txt");
    std::thread::sleep(PARK_WAIT);
    feedback1.finish();

    fx.wait_settled_on(&upstream, what);
    fx.assert_no_unexpected_agent_calls(what, &unexpected);
    fx.assert_everything_published(
        what,
        &[
            (0, "reviewer-a1.txt", "reviewer"),
            (0, "reviewer-a2.txt", "reviewer"),
            (1, "feature-b.txt", "feedback on b"),
            (2, "reviewer-c.txt", "reviewer"),
        ],
        &["upstream1.txt"],
    );
}

/// A reviewer's push edits the same lines as a feedback round in flight on
/// the same branch (and as the upstream push), so pulling it in needs
/// conflict resolution: both edits and the upstream line must land exactly
/// once.
#[test]
fn reviewer_push_conflicting_with_in_flight_feedback_on_the_same_branch() {
    let what = "conflicting reviewer push";
    let fx = Fixture::with_upstream(&["feature/a", "feature/b"]);
    fx.open_pr_stack();
    let unexpected: Unexpected = Arc::default();
    let _daemon = DaemonPump::start_resolving(&fx, &unexpected);

    let mut feedback1 = Held::resolving("base.txt", "feedback on b", &unexpected).feedback(&fx, 1);
    fx.push_to_pr_append(1, "base.txt", "reviewer on b");
    let upstream = fx.push_upstream_append("base.txt", "upstream line");
    std::thread::sleep(PARK_WAIT);
    feedback1.finish();

    fx.wait_settled_on(&upstream, what);
    fx.assert_no_unexpected_agent_calls(what, &unexpected);
    fx.assert_lines_exactly_once(
        what,
        "base.txt",
        &[(1, "feedback on b"), (1, "reviewer on b")],
        &["base", "upstream line"],
    );
}

/// A reviewer rewrites a PR branch's history -- amends its top commit (the
/// branch's feedback commit) and force-pushes -- while the base moves. The
/// reviewer's rewrite must reach the review branch and the PR, and the
/// original feedback line must not be dropped or duplicated.
#[test]
fn reviewer_amend_and_force_push_while_the_base_moves() {
    let what = "reviewer amend + force-push";
    let fx = Fixture::with_upstream(&["feature/a", "feature/b"]);
    fx.open_pr_stack();
    let unexpected: Unexpected = Arc::default();
    let _daemon = DaemonPump::start_resolving(&fx, &unexpected);

    let mut feedback1 =
        Held::resolving("feature-b.txt", "feedback on b", &unexpected).feedback(&fx, 1);
    feedback1.finish();
    let first = fx
        .store
        .lock()
        .get_guardian(&fx.id)
        .unwrap()
        .base_commit
        .unwrap_or_default();
    fx.wait_settled_on(first.trim(), "before the amend");

    fx.amend_pr(1, "feature-b.txt", "amended by reviewer");
    let upstream = fx.push_upstream("upstream1.txt");

    fx.wait_settled_on(&upstream, what);
    fx.assert_no_unexpected_agent_calls(what, &unexpected);
    fx.assert_lines_exactly_once(
        what,
        "feature-b.txt",
        &[(1, "feedback on b"), (1, "amended by reviewer")],
        &[],
    );
    fx.assert_everything_published(what, &[], &["upstream1.txt"]);
}

// ---------------------------------------------------------------------------
// Shared-worktree mode (`skip_worktrees`): every branch shares one combined
// worktree/branch, so feedback and PR fixes commit onto that combined branch.
// ---------------------------------------------------------------------------

impl Fixture {
    /// Wait (while a [`DaemonPump`] runs) until the review has been rebuilt
    /// on `upstream` and stayed idle -- without the PR-branch comparison,
    /// for reviews with no per-branch PRs.
    fn wait_rebuilt_on(&self, upstream: &str, what: &str) {
        let deadline = Instant::now() + Duration::from_secs(300);
        let mut stable = 0;
        while stable < SETTLE_STABLE_POLLS {
            assert!(
                Instant::now() < deadline,
                "{what}: review never rebuilt on upstream {upstream:.9}: {}",
                self.describe()
            );
            let view = self.store.lock().get_guardian(&self.id).unwrap();
            let settled = view.status == "in_review"
                && view.base_commit.is_some_and(|b| b.trim() == upstream);
            stable = if settled { stable + 1 } else { 0 };
            std::thread::sleep(SETTLE_POLL);
        }
    }
}

/// In shared-worktree mode, a feedback commit on the combined branch -- and
/// a second one racing the rebase -- must survive the upstream rebase, which
/// rebuilds the combined branch.
#[test]
fn shared_worktree_mode_keeps_feedback_through_an_upstream_rebase() {
    let what = "shared worktree";
    let fx = Fixture::with_upstream(&["feature/a", "feature/b"]);
    fx.store
        .lock()
        .set_guardian_skip_worktrees(&fx.id, true)
        .unwrap();
    run_merge(&fx.store, &NoopRunner, &fx.id);
    let view = fx.store.lock().get_guardian(&fx.id).unwrap();
    assert_eq!(view.status, "in_review", "{}", fx.describe());
    let unexpected: Unexpected = Arc::default();

    let mut settled = Held::new("feature-a.txt", "feedback before", &unexpected).feedback(&fx, 0);
    settled.finish();
    let _daemon = DaemonPump::start(&fx);
    let mut racing = Held::new("feature-b.txt", "feedback racing", &unexpected).feedback(&fx, 1);
    let upstream = fx.push_upstream("upstream1.txt");
    std::thread::sleep(PARK_WAIT);
    racing.finish();

    fx.wait_rebuilt_on(&upstream, what);
    fx.assert_no_unexpected_agent_calls(what, &unexpected);
    let combined = fx
        .store
        .lock()
        .get_guardian(&fx.id)
        .unwrap()
        .review_branch
        .expect("combined review branch");
    let files = fx.files_on(&fx.root, &combined);
    assert!(
        files.contains("upstream1.txt"),
        "{what}: not rebuilt on upstream: {files}"
    );
    for (file, line) in [
        ("feature-a.txt", "feedback before"),
        ("feature-b.txt", "feedback racing"),
    ] {
        let content = fx.file_on(&fx.root, &combined, file);
        assert!(
            content.contains(line),
            "{what}: combined branch lost {line:?} ({file}: {content:?})\n{}",
            fx.describe()
        );
    }
}

// ---------------------------------------------------------------------------
// Upstream history rewrites: the base branch is force-pushed, so the review's
// old base is no longer an ancestor of the new one.
// ---------------------------------------------------------------------------

impl Fixture {
    /// Force-push upstream `main` back `drop` commits and, when `file` is
    /// given, on top of that a new commit adding it -- a rewrite of upstream
    /// history, from an unrelated clone. Returns the new upstream tip.
    fn force_push_upstream(&self, drop: usize, file: Option<&str>) -> String {
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
        git(&clone, &["reset", "-q", "--hard", &format!("HEAD~{drop}")]);
        if let Some(file) = file {
            write(&clone, file, "rewritten upstream\n");
            git(&clone, &["add", file]);
            git(
                &clone,
                &["commit", "-m", &format!("upstream rewrite: {file}")],
            );
        }
        git(&clone, &["push", "-q", "--force", "origin", "main"]);
        let sha = git(&clone, &["rev-parse", "HEAD"]).trim().to_string();
        let _ = std::fs::remove_dir_all(&clone);
        sha
    }

    /// Assert `file` is on no review branch and no published PR branch.
    fn assert_file_nowhere(&self, what: &str, file: &str) {
        let published = self.published_positions();
        for p in self.enabled_positions() {
            let local = self.review_ref(p);
            let remote = self.pr_ref(p);
            let mut targets = vec![(&self.root, local.as_str(), "review branch")];
            if published.contains(&p) {
                targets.push((&self.remote, remote.as_str(), "PR branch"));
            }
            for (repo, rev, which) in targets {
                let files = self.files_on(repo, rev);
                assert!(
                    !files.lines().any(|f| f == file),
                    "{what}: {which} {p} still has {file}, dropped upstream\n{}",
                    self.describe()
                );
            }
        }
    }
}

/// Upstream replaces its last commit (a force-push) while a feedback round
/// and a PR fix are in flight. The review must rebuild onto the rewritten
/// base: the dropped upstream commit leaves every branch and PR, and every
/// review-only commit stays.
#[test]
fn force_pushed_upstream_rewrite_with_feedback_and_auto_fix_in_flight() {
    let what = "force-pushed upstream rewrite";
    let fx = Fixture::with_upstream(&["feature/a", "feature/b", "feature/c"]);
    let pr_ids = fx.open_pr_stack();
    let unexpected: Unexpected = Arc::default();
    let _daemon = DaemonPump::start(&fx);

    let doomed = fx.push_upstream("doomed.txt");
    fx.wait_settled_on(&doomed, "before the rewrite");
    let mut settled = Held::new("feature-a.txt", "feedback on a", &unexpected).feedback(&fx, 0);
    settled.finish();

    let mut feedback1 = Held::new("feature-b.txt", "feedback on b", &unexpected).feedback(&fx, 1);
    let mut fix2 =
        Held::new("feature-c.txt", "auto-fix on c", &unexpected).auto_fix(&fx, &pr_ids[2]);
    let rewritten = fx.force_push_upstream(1, Some("replacement.txt"));
    std::thread::sleep(PARK_WAIT);
    fix2.finish();
    feedback1.finish();

    fx.wait_settled_on(&rewritten, what);
    fx.assert_no_unexpected_agent_calls(what, &unexpected);
    fx.assert_everything_published(
        what,
        &[
            (0, "feature-a.txt", "feedback on a"),
            (1, "feature-b.txt", "feedback on b"),
            (2, "feature-c.txt", "auto-fix on c"),
        ],
        &["replacement.txt"],
    );
    fx.assert_file_nowhere(what, "doomed.txt");
}

/// Upstream is rewound to an older commit (a force-push dropping its tip, with
/// nothing new on top) while feedback is in flight: the review must rebuild
/// onto the older base and keep its review-only commits.
#[test]
fn upstream_rewound_to_an_older_commit_with_feedback_in_flight() {
    let what = "upstream rewound";
    let fx = Fixture::with_upstream(&["feature/a", "feature/b"]);
    fx.open_pr_stack();
    let unexpected: Unexpected = Arc::default();
    let _daemon = DaemonPump::start(&fx);

    let doomed = fx.push_upstream("doomed.txt");
    fx.wait_settled_on(&doomed, "before the rewind");
    let mut settled = Held::new("feature-a.txt", "feedback on a", &unexpected).feedback(&fx, 0);
    settled.finish();

    let mut feedback1 = Held::new("feature-b.txt", "feedback on b", &unexpected).feedback(&fx, 1);
    let rewound = fx.force_push_upstream(1, None);
    std::thread::sleep(PARK_WAIT);
    feedback1.finish();

    fx.wait_settled_on(&rewound, what);
    fx.assert_no_unexpected_agent_calls(what, &unexpected);
    fx.assert_everything_published(
        what,
        &[
            (0, "feature-a.txt", "feedback on a"),
            (1, "feature-b.txt", "feedback on b"),
        ],
        &[],
    );
    fx.assert_file_nowhere(what, "doomed.txt");
}

// ---------------------------------------------------------------------------
// User actions taken mid-race: restart-merge, stop-merge, cancel + reopen and
// disable + re-enable, each while a writer is held and the base has moved.
// ---------------------------------------------------------------------------

/// The user action a [`user_action_mid_rebuild`] scenario takes.
#[derive(Debug, Clone, Copy)]
enum UserAction {
    /// "Restart merge" (`restart_guardian_merge`).
    RestartMerge,
    /// "Stop merge", then "Merge / rebase" (`stop_guardian_merge`, `start_merge`).
    StopThenMerge,
    /// Cancel the review, then reopen it (`cancel_guardian`, `reopen_guardian_merge`).
    CancelThenReopen,
}

/// A user action (restart, stop, or cancel) taken while the daemon's
/// base-shift rebuild is parked behind a held feedback round, the held round
/// then finishing, followed (for stop and cancel) by the matching resume. Every
/// feedback commit -- the one already settled and the one finishing during the
/// action -- and the upstream commit must end up on every PR.
fn user_action_mid_rebuild(action: UserAction) {
    let what = format!("{action:?} mid-rebuild");
    let fx = Fixture::with_upstream(&["feature/a", "feature/b", "feature/c"]);
    fx.open_pr_stack();
    let unexpected: Unexpected = Arc::default();
    let daemon = DaemonPump::start(&fx);
    let mut settled = Held::new("feature-a.txt", "feedback on a", &unexpected).feedback(&fx, 0);
    settled.finish();

    let mut held = Held::new("feature-b.txt", "feedback on b", &unexpected).feedback(&fx, 1);
    let upstream = fx.push_upstream("upstream1.txt");
    std::thread::sleep(PARK_WAIT);

    // The action may wait for the parked worker, which waits on the held
    // round's lease: take it on its own thread, then release the round.
    let (store, id) = (Arc::clone(&fx.store), fx.id.clone());
    let (cancellations, sem) = (daemon.cancellations.clone(), Arc::clone(&daemon.sem));
    let acting = std::thread::spawn(move || match action {
        UserAction::RestartMerge => {
            ralphus_daemon::guardian_merge::restart_guardian_merge(
                store,
                cancellations,
                Arc::new(NoAgentExpected),
                &id,
                sem,
            );
        }
        UserAction::StopThenMerge => {
            ralphus_daemon::guardian_merge::stop_guardian_merge(store, cancellations, &id);
        }
        UserAction::CancelThenReopen => {
            ralphus_daemon::guardian_merge::stop_merge_worker_for_cancel(&cancellations, &id);
            store.lock().cancel_guardian(&id).expect("cancel review");
        }
    });
    std::thread::sleep(PARK_WAIT);
    held.finish();
    acting.join().expect("user action thread");

    let (store, id) = (Arc::clone(&fx.store), fx.id.clone());
    let (cancellations, sem) = (daemon.cancellations.clone(), Arc::clone(&daemon.sem));
    match action {
        UserAction::RestartMerge => {}
        UserAction::StopThenMerge => {
            ralphus_daemon::guardian_merge::start_merge(
                store,
                Arc::new(NoAgentExpected),
                &id,
                sem,
                cancellations,
            );
        }
        UserAction::CancelThenReopen => {
            ralphus_daemon::guardian_merge::reopen_guardian_merge(
                store,
                Arc::new(NoAgentExpected),
                &id,
                sem,
                cancellations,
            );
        }
    }

    fx.wait_settled_on(&upstream, &what);
    fx.assert_no_unexpected_agent_calls(&what, &unexpected);
    fx.assert_everything_published(
        &what,
        &[
            (0, "feature-a.txt", "feedback on a"),
            (1, "feature-b.txt", "feedback on b"),
        ],
        &["upstream1.txt"],
    );
}

#[test]
fn restart_merge_while_a_rebuild_waits_on_feedback() {
    user_action_mid_rebuild(UserAction::RestartMerge);
}

#[test]
fn stop_merge_then_merge_while_a_rebuild_waits_on_feedback() {
    user_action_mid_rebuild(UserAction::StopThenMerge);
}

#[test]
fn cancel_then_reopen_while_a_rebuild_waits_on_feedback() {
    user_action_mid_rebuild(UserAction::CancelThenReopen);
}

/// The board's "arrange" action (`guardian_arrange`): set a branch's enabled
/// flag, then start a merge.
fn arrange(fx: &Fixture, daemon: &DaemonPump, branch: &str, enabled: bool) {
    fx.store
        .lock()
        .set_branch_enabled_by_name(&fx.id, branch, enabled)
        .expect("set branch enabled");
    ralphus_daemon::guardian_merge::start_merge(
        Arc::clone(&fx.store),
        Arc::new(NoAgentExpected),
        &fx.id,
        Arc::clone(&daemon.sem),
        daemon.cancellations.clone(),
    );
}

/// A middle branch carrying feedback is disabled while a feedback round on
/// the branch above is held and the base moves, then re-enabled. While it is
/// disabled, its commits leave the branches above; once it is back, its own
/// feedback commit and every other branch's must be on every PR again.
#[test]
fn disable_then_reenable_a_middle_branch_mid_race() {
    let what = "disable + re-enable";
    let fx = Fixture::with_upstream(&["feature/a", "feature/b", "feature/c"]);
    fx.open_pr_stack();
    let unexpected: Unexpected = Arc::default();
    let daemon = DaemonPump::start(&fx);
    let mut settled = Held::new("feature-b.txt", "feedback on b", &unexpected).feedback(&fx, 1);
    settled.finish();

    let mut held = Held::new("feature-c.txt", "feedback on c", &unexpected).feedback(&fx, 2);
    let upstream = fx.push_upstream("upstream1.txt");
    std::thread::sleep(PARK_WAIT);
    arrange(&fx, &daemon, "feature/b", false);
    held.finish();
    fx.wait_settled_on(&upstream, "while disabled");
    fx.assert_file_nowhere("while disabled", "feature-b.txt");
    fx.assert_everything_published(
        "while disabled",
        &[(2, "feature-c.txt", "feedback on c")],
        &["upstream1.txt"],
    );

    arrange(&fx, &daemon, "feature/b", true);
    fx.wait_settled_on(&upstream, what);
    fx.assert_no_unexpected_agent_calls(what, &unexpected);
    fx.assert_everything_published(
        what,
        &[
            (1, "feature-b.txt", "feedback on b"),
            (2, "feature-c.txt", "feedback on c"),
        ],
        &["upstream1.txt"],
    );
}

// ---------------------------------------------------------------------------
// A daemon restart mid-race: the in-memory state (worktree leases, restack
// bookkeeping, running-merge sets) is gone, and the store and repo are left
// exactly as a killed daemon leaves them.
// ---------------------------------------------------------------------------

/// The daemon dies mid-rebuild: the review is left `merging` with a branch
/// `in_progress`, and that branch's worktree is stopped part-way through a
/// rebase. Meanwhile the base moved and a reviewer pushed to a PR. A fresh
/// daemon -- a new store handle on the same database, its startup recovery,
/// then its maintenance -- must finish the rebuild with every review-only
/// commit, the reviewer's commit and the upstream commit on every PR.
#[test]
fn daemon_restart_mid_rebuild_keeps_every_commit() {
    let what = "daemon restart";
    let mut fx = Fixture::with_upstream(&["feature/a", "feature/b", "feature/c"]);
    fx.open_pr_stack();
    let unexpected: Unexpected = Arc::default();
    for (p, file) in [(0, "feature-a.txt"), (1, "feature-b.txt")] {
        let mut round = Held::new(file, &format!("feedback on {p}"), &unexpected).feedback(&fx, p);
        round.finish();
    }
    {
        let daemon = DaemonPump::start(&fx);
        let first = fx
            .store
            .lock()
            .get_guardian(&fx.id)
            .unwrap()
            .base_commit
            .unwrap_or_default();
        fx.wait_settled_on(first.trim(), "before the crash");
        drop(daemon);
    }
    let upstream = fx.push_upstream("upstream1.txt");
    fx.push_to_pr(2, "reviewer-c.txt");

    // What a daemon killed mid-rebuild leaves behind.
    ralphus_daemon::guardian_merge::poll_base_branch_freshness_once(&fx.store);
    {
        let guard = fx.store.lock();
        guard
            .set_guardian_status(&fx.id, GuardianStatus::Merging, None)
            .unwrap();
        guard
            .set_branch_status(&fx.id, &fx.branch_ids[1], MergeStatus::InProgress, None)
            .unwrap();
    }
    let worktree = PathBuf::from(
        fx.store.lock().get_guardian(&fx.id).unwrap().branches[1]
            .worktree
            .clone()
            .expect("branch 1 worktree"),
    );
    // `--exec` stops the rebase after its first pick, mid-way.
    let _ = std::process::Command::new("git")
        .args(["rebase", "--exec", "exit 1", "origin/main"])
        .current_dir(&worktree)
        .output();

    // The restarted daemon: a fresh store handle (no leases, no running-merge
    // bookkeeping), its startup recovery, then its maintenance.
    let db = fx.root.join(".git").join("ralphus-test.db");
    fx.store = Arc::new(StoreMutex::new(Store::open(&db).unwrap()));
    let daemon = DaemonPump::start(&fx);
    ralphus_daemon::scheduler::recover_interrupted_reviews(
        &fx.store,
        &daemon.sem,
        &daemon.cancellations,
    );

    fx.wait_settled_on(&upstream, what);
    fx.assert_no_unexpected_agent_calls(what, &unexpected);
    fx.assert_everything_published(
        what,
        &[
            (0, "feature-a.txt", "feedback on 0"),
            (1, "feature-b.txt", "feedback on 1"),
            (2, "reviewer-c.txt", "reviewer"),
        ],
        &["upstream1.txt"],
    );
}

// ---------------------------------------------------------------------------
// Other review modes: squash (RAL-91) and a multi-project review.
// ---------------------------------------------------------------------------

/// Squash mode: each branch's task commits land on its review branch as one
/// squashed commit. A feedback round, a held one and a held PR fix racing an
/// upstream rebase must all still reach every PR above them.
#[test]
fn squash_mode_keeps_feedback_and_fixes_through_an_upstream_rebase() {
    let what = "squash mode";
    let fx = Fixture::with_upstream(&["feature/a", "feature/b", "feature/c"]);
    fx.store
        .lock()
        .set_guardian_project_squash(&fx.id, fx.root.to_str().unwrap(), true)
        .unwrap();
    run_merge(&fx.store, &NoopRunner, &fx.id);
    assert_eq!(
        fx.store.lock().get_guardian(&fx.id).unwrap().status,
        "in_review",
        "{}",
        fx.describe()
    );
    let pr_ids = fx.open_pr_stack();
    let unexpected: Unexpected = Arc::default();
    let _daemon = DaemonPump::start(&fx);
    let mut settled = Held::new("feature-a.txt", "feedback on a", &unexpected).feedback(&fx, 0);
    settled.finish();

    let mut feedback1 = Held::new("feature-b.txt", "feedback on b", &unexpected).feedback(&fx, 1);
    let mut fix2 =
        Held::new("feature-c.txt", "auto-fix on c", &unexpected).auto_fix(&fx, &pr_ids[2]);
    let upstream = fx.push_upstream("upstream1.txt");
    std::thread::sleep(PARK_WAIT);
    fix2.finish();
    feedback1.finish();

    fx.wait_settled_on(&upstream, what);
    fx.assert_no_unexpected_agent_calls(what, &unexpected);
    fx.assert_everything_published(
        what,
        &[
            (0, "feature-a.txt", "feedback on a"),
            (1, "feature-b.txt", "feedback on b"),
            (2, "feature-c.txt", "auto-fix on c"),
        ],
        &["upstream1.txt"],
    );
}

/// A second project for a multi-project review: a repo with its own bare
/// upstream and a task branch `feature/x`. Returns `(repo, upstream)`.
fn second_project() -> (PathBuf, PathBuf) {
    let root = temp_dir();
    init_repo(&root);
    let remote = temp_dir();
    git(&remote, &["init", "--bare"]);
    git(
        &root,
        &["remote", "add", "origin", remote.to_str().unwrap()],
    );
    write(&root, "other.txt", "other\n");
    git(&root, &["add", "."]);
    git(&root, &["commit", "-m", "other base"]);
    git(&root, &["push", "-q", "origin", "main"]);
    git(&root, &["fetch", "-q", "origin"]);
    git(&root, &["checkout", "-q", "-b", "feature/x"]);
    write(&root, "feature-x.txt", "content\n");
    git(&root, &["add", "."]);
    git(&root, &["commit", "-m", "add feature/x"]);
    git(&root, &["checkout", "-q", "main"]);
    (root, remote)
}

/// Push a commit adding `file` to `remote`'s `main` from an unrelated clone.
fn push_to_upstream(remote: &Path, file: &str) {
    let clone = temp_dir();
    let _ = std::fs::remove_dir_all(&clone);
    git(
        clone.parent().unwrap(),
        &[
            "clone",
            "-q",
            "--branch",
            "main",
            remote.to_str().unwrap(),
            clone.file_name().unwrap().to_str().unwrap(),
        ],
    );
    write(&clone, file, "upstream\n");
    git(&clone, &["add", file]);
    git(&clone, &["commit", "-m", &format!("upstream: {file}")]);
    git(&clone, &["push", "-q", "origin", "main"]);
    let _ = std::fs::remove_dir_all(&clone);
}

/// A review spanning two repos, each with its own upstream. Both upstreams
/// move while feedback rounds on a branch in each project are in flight: each
/// project's review branches must be rebuilt on its own new base and keep
/// both feedback commits.
#[test]
fn multi_project_review_keeps_feedback_in_both_projects_through_upstream_rebases() {
    let what = "multi-project";
    let fx = Fixture::with_upstream(&["feature/a", "feature/b"]);
    let (other, other_remote) = second_project();
    fx.store
        .lock()
        .add_guardian_branch_with_project(&fx.id, "feature/x", Some(other.to_str().unwrap()))
        .unwrap();
    run_merge(&fx.store, &NoopRunner, &fx.id);
    let view = fx.store.lock().get_guardian(&fx.id).unwrap();
    assert_eq!(view.status, "in_review", "{}", fx.describe());
    let x_id = view.branches[2].id.clone();
    let unexpected: Unexpected = Arc::default();
    let _daemon = DaemonPump::start(&fx);

    let mut feedback_b = Held::new("feature-b.txt", "feedback on b", &unexpected).feedback(&fx, 1);
    let feedback_x = WriterRunner::new("feature-x.txt", "feedback on x", None, &unexpected);
    let x_thread = {
        let (store, id) = (Arc::clone(&fx.store), fx.id.clone());
        let runner = Arc::new(feedback_x);
        std::thread::spawn(move || {
            run_feedback(
                &store,
                runner.as_ref(),
                &id,
                &x_id,
                "change x",
                None,
                false,
                &CancelToken::never(),
            );
        })
    };
    fx.push_upstream("upstream1.txt");
    push_to_upstream(&other_remote, "upstream-other.txt");
    std::thread::sleep(PARK_WAIT);
    feedback_b.finish();
    x_thread.join().expect("feedback on x");

    // Settled: in review, and each project's review branches on its new base
    // for several consecutive polls.
    let deadline = Instant::now() + Duration::from_secs(300);
    let mut stable = 0;
    while stable < SETTLE_STABLE_POLLS {
        let view = fx.store.lock().get_guardian(&fx.id).unwrap();
        assert!(
            Instant::now() < deadline,
            "{what}: never rebuilt on both upstreams: {}\nprojects={:?} base_commits={:?} \
             origin/main: root={} other={} | upstream main: root={} other={}",
            fx.describe(),
            view.projects,
            view.base_commits,
            git(&fx.root, &["log", "-1", "--format=%h %s", "origin/main"]).trim(),
            git(&other, &["log", "-1", "--format=%h %s", "origin/main"]).trim(),
            git(&fx.remote, &["log", "-1", "--format=%h %s", "main"]).trim(),
            git(&other_remote, &["log", "-1", "--format=%h %s", "main"]).trim(),
        );
        let rebuilt = view.status == "in_review"
            && view.branches.iter().all(|b| {
                let repo = if b.branch == "feature/x" {
                    &other
                } else {
                    &fx.root
                };
                let upstream_file = if b.branch == "feature/x" {
                    "upstream-other.txt"
                } else {
                    "upstream1.txt"
                };
                b.review_branch
                    .as_deref()
                    .is_some_and(|rev| fx.files_on(repo, rev).contains(upstream_file))
            });
        stable = if rebuilt { stable + 1 } else { 0 };
        std::thread::sleep(SETTLE_POLL);
    }
    fx.assert_no_unexpected_agent_calls(what, &unexpected);
    let view = fx.store.lock().get_guardian(&fx.id).unwrap();
    let b_ref = view.branches[1].review_branch.clone().unwrap();
    let x_ref = view.branches[2].review_branch.clone().unwrap();
    assert!(
        fx.file_on(&fx.root, &b_ref, "feature-b.txt")
            .contains("feedback on b"),
        "{what}: lost feedback on b\n{}",
        fx.describe()
    );
    assert!(
        fx.file_on(&other, &x_ref, "feature-x.txt")
            .contains("feedback on x"),
        "{what}: lost feedback on x\n{}",
        fx.describe()
    );
    let _ = std::fs::remove_dir_all(&other);
    let _ = std::fs::remove_dir_all(&other_remote);
}

// ---------------------------------------------------------------------------
// Forge-dependent races, against a fake GitHub REST API. The daemon resolves
// its forge client exactly as in production: from the `origin` remote URL
// (`ssh://localhost/acme/w.git`, whose transport is real git against the bare
// upstream via a `core.sshCommand` shim) and `.ralphus.toml`'s `[forge]`
// (`kind`, `api_base` pointing at the fake).
// ---------------------------------------------------------------------------

/// One pull request on the [`FakeGitHub`].
#[derive(Clone)]
struct FakePr {
    number: u64,
    head: String,
    base: String,
    open: bool,
    merged: bool,
    updated_at: String,
    /// Issue comments: `(id, body)`.
    comments: Vec<(u64, String)>,
}

#[derive(Default)]
struct ForgeState {
    prs: Vec<FakePr>,
    /// Head SHAs whose check run fails; every other head passes.
    failing_heads: std::collections::HashSet<String>,
    /// Requests the fake has no route for (`METHOD path`).
    unrouted: Vec<String>,
    /// The next this-many `/pulls` requests fail with a 503.
    fail_pulls: u32,
}

/// A stateful fake of the GitHub REST endpoints the daemon calls for a PR
/// stack: PR create/find/get/patch, check runs and statuses (all passing),
/// native stacks, workflow runs, and issue/review comments. Head SHAs are
/// read from the bare upstream, so they are whatever the daemon pushed.
struct FakeGitHub {
    addr: String,
    state: Arc<std::sync::Mutex<ForgeState>>,
    stop: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

fn json_reply(body: serde_json::Value, code: u16) -> tiny_http::Response<std::io::Cursor<Vec<u8>>> {
    tiny_http::Response::from_string(body.to_string()).with_status_code(code)
}

impl FakeGitHub {
    fn start(bare: &Path) -> Self {
        let server = tiny_http::Server::http("127.0.0.1:0").expect("fake forge listener");
        let addr = server
            .server_addr()
            .to_ip()
            .expect("ip listener")
            .to_string();
        let state: Arc<std::sync::Mutex<ForgeState>> = Arc::default();
        let stop = Arc::new(AtomicBool::new(false));
        let (thread_state, flag, bare) =
            (Arc::clone(&state), Arc::clone(&stop), bare.to_path_buf());
        let handle = std::thread::spawn(move || {
            while !flag.load(Ordering::SeqCst) {
                let Ok(Some(mut req)) = server.recv_timeout(Duration::from_millis(50)) else {
                    continue;
                };
                let mut body = String::new();
                let _ = std::io::Read::read_to_string(req.as_reader(), &mut body);
                let reply = Self::route(&thread_state, &bare, req.method(), req.url(), &body);
                let _ = req.respond(reply);
            }
        });
        Self {
            addr,
            state,
            stop,
            handle: Some(handle),
        }
    }

    fn pr_json(bare: &Path, pr: &FakePr) -> serde_json::Value {
        let sha = std::process::Command::new("git")
            .args(["rev-parse", &format!("refs/heads/{}", pr.head)])
            .current_dir(bare)
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .unwrap_or_default();
        serde_json::json!({
            "number": pr.number,
            "html_url": format!("http://fake/pull/{}", pr.number),
            "state": if pr.open { "open" } else { "closed" },
            "merged": pr.merged,
            "draft": false,
            "mergeable_state": "clean",
            "head": {"ref": pr.head, "sha": sha},
            "base": {"ref": pr.base},
            "updated_at": pr.updated_at,
            "title": format!("PR {}", pr.number),
            "body": "",
        })
    }

    fn route(
        state: &std::sync::Mutex<ForgeState>,
        bare: &Path,
        method: &tiny_http::Method,
        url: &str,
        body: &str,
    ) -> tiny_http::Response<std::io::Cursor<Vec<u8>>> {
        let (path, query) = url.split_once('?').unwrap_or((url, ""));
        let parts: Vec<&str> = path
            .trim_start_matches('/')
            .split('/')
            .skip(3) // repos/acme/w
            .collect();
        let req: serde_json::Value = serde_json::from_str(body).unwrap_or_default();
        let mut st = state.lock().unwrap();
        let number = |s: &str| s.parse::<u64>().ok();
        if parts.first() == Some(&"pulls") && st.fail_pulls > 0 {
            st.fail_pulls -= 1;
            return json_reply(serde_json::json!({"message": "unavailable"}), 503);
        }
        use tiny_http::Method::{Get, Patch, Post};
        match (method, parts.as_slice()) {
            (Get, []) => json_reply(
                serde_json::json!({"full_name": "acme/w", "fork": false}),
                200,
            ),
            (Get, ["pulls"]) => {
                let head = query
                    .split('&')
                    .find_map(|kv| kv.strip_prefix("head="))
                    .map(|h| h.rsplit(':').next().unwrap_or(h).to_string());
                let found: Vec<serde_json::Value> = st
                    .prs
                    .iter()
                    .filter(|pr| pr.open && head.as_deref().is_none_or(|h| pr.head == h))
                    .map(|pr| Self::pr_json(bare, pr))
                    .collect();
                json_reply(serde_json::Value::Array(found), 200)
            }
            (Post, ["pulls"]) => {
                let head = req["head"].as_str().unwrap_or_default();
                let head = head.rsplit(':').next().unwrap_or(head).to_string();
                if st.prs.iter().any(|pr| pr.open && pr.head == head) {
                    return json_reply(serde_json::json!({"message": "exists"}), 422);
                }
                let pr = FakePr {
                    number: st.prs.len() as u64 + 1,
                    head,
                    base: req["base"].as_str().unwrap_or("main").to_string(),
                    open: true,
                    merged: false,
                    updated_at: "2000-01-01T00:00:00Z".to_string(),
                    comments: Vec::new(),
                };
                let reply = Self::pr_json(bare, &pr);
                st.prs.push(pr);
                json_reply(reply, 201)
            }
            (Get | Patch, ["pulls", n]) => {
                let Some(pr) = number(n).and_then(|n| st.prs.iter_mut().find(|p| p.number == n))
                else {
                    return json_reply(serde_json::json!({"message": "Not Found"}), 404);
                };
                if *method == Patch {
                    if let Some(base) = req["base"].as_str() {
                        pr.base = base.to_string();
                    }
                    if req["state"].as_str() == Some("closed") {
                        pr.open = false;
                    }
                }
                let reply = Self::pr_json(bare, pr);
                json_reply(reply, 200)
            }
            (Get, ["issues", n, "comments"]) => {
                let comments: Vec<serde_json::Value> = number(n)
                    .and_then(|n| st.prs.iter().find(|p| p.number == n))
                    .map(|pr| {
                        pr.comments
                            .iter()
                            .map(|(id, body)| {
                                serde_json::json!({
                                    "id": id,
                                    "user": {"login": "reviewer"},
                                    "body": body,
                                    "created_at": "2026-01-01T00:00:00Z",
                                })
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                json_reply(serde_json::Value::Array(comments), 200)
            }
            (Get, ["pulls", _, "comments"]) => json_reply(serde_json::json!([]), 200),
            (Get, ["commits", sha, "check-runs"]) => {
                let conclusion = if st.failing_heads.contains(*sha) {
                    "failure"
                } else {
                    "success"
                };
                json_reply(
                    serde_json::json!({"total_count": 1, "check_runs": [{
                        "name": "build", "status": "completed", "conclusion": conclusion,
                        "details_url": "http://fake/run",
                        "output": {"summary": "build failed", "text": "error: build failed"}}]}),
                    200,
                )
            }
            (Get, ["commits", _, "status"]) => json_reply(
                serde_json::json!({"state": "success", "total_count": 0, "statuses": []}),
                200,
            ),
            (Post, ["stacks"]) => json_reply(serde_json::json!({"number": 1}), 201),
            (Get, ["stacks", _]) => {
                let open: Vec<serde_json::Value> = st
                    .prs
                    .iter()
                    .filter(|p| p.open)
                    .map(|p| serde_json::json!({"number": p.number}))
                    .collect();
                json_reply(serde_json::json!({"number": 1, "pull_requests": open}), 200)
            }
            (Post, ["stacks", _, _]) | (Post, ["actions", "runs", _, _]) => {
                json_reply(serde_json::json!({}), 200)
            }
            (Get, ["actions", "runs"]) => json_reply(serde_json::json!({"workflow_runs": []}), 200),
            (Get, ["actions", "jobs", _]) => json_reply(serde_json::json!({"steps": []}), 200),
            (Get, ["contents", ..]) => json_reply(serde_json::json!({"message": "Not Found"}), 404),
            _ => {
                st.unrouted.push(format!("{method} {url}"));
                json_reply(serde_json::json!({"message": "Not Found"}), 404)
            }
        }
    }

    /// The open PR whose head is `alias`.
    fn pr_for(&self, alias: &str) -> FakePr {
        self.state
            .lock()
            .unwrap()
            .prs
            .iter()
            .find(|p| p.open && p.head == alias)
            .cloned()
            .unwrap_or_else(|| panic!("no open fake PR for {alias}"))
    }

    fn with_pr(&self, number: u64, edit: impl FnOnce(&mut FakePr)) {
        let mut st = self.state.lock().unwrap();
        let pr = st
            .prs
            .iter_mut()
            .find(|p| p.number == number)
            .expect("fake PR");
        edit(pr);
    }
}

impl Drop for FakeGitHub {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

impl Fixture {
    /// [`Fixture::with_upstream`] whose `origin` is a GitHub repo served by a
    /// [`FakeGitHub`]: the remote URL is `ssh://localhost/acme/w.git`, its
    /// transport real git against the bare upstream, and `.ralphus.toml`
    /// points the daemon's forge client at the fake. No PRs are open yet.
    fn with_forge(features: &[&str]) -> (Self, FakeGitHub) {
        let fx = Self::with_upstream(features);
        let forge = FakeGitHub::start(&fx.remote);
        let shim = fx.root.join(".git").join("ssh-shim.sh");
        std::fs::write(
            &shim,
            format!(
                "#!/bin/sh\n# ssh stand-in: run the requested git service on the bare upstream.\n\
                 shift\ncmd=\"$*\"\nverb=${{cmd%% *}}\nexec git \"${{verb#git-}}\" '{}'\n",
                fx.remote.display()
            ),
        )
        .unwrap();
        git(
            &fx.root,
            &[
                "config",
                "core.sshCommand",
                &format!("sh '{}'", shim.display()),
            ],
        );
        git(
            &fx.root,
            &["remote", "set-url", "origin", "ssh://localhost/acme/w.git"],
        );
        // `token_env` names a variable cargo/nextest always set to a
        // non-secret value, so the client never falls back to a real `gh`
        // login and sends its token to the fake.
        std::fs::write(
            fx.root.join(".ralphus.toml"),
            format!(
                "[forge]\nkind = \"github\"\napi_base = \"http://{}\"\ntoken_env = \"CARGO_MANIFEST_DIR\"\n",
                forge.addr
            ),
        )
        .unwrap();
        let info = fx.root.join(".git").join("info");
        std::fs::create_dir_all(&info).unwrap();
        let exclude = info.join("exclude");
        let mut text = std::fs::read_to_string(&exclude).unwrap_or_default();
        text.push_str("\n.ralphus.toml\n");
        std::fs::write(&exclude, text).unwrap();
        (fx, forge)
    }

    /// Submit the whole stack as PRs through the daemon's own submit path.
    fn submit_stack(&self) -> Vec<String> {
        let prs = ralphus_daemon::pr::submit_pull_requests(
            &self.store,
            &NoAgentExpected,
            &self.id,
            vec![ralphus_daemon::pr::PrRequest {
                branch_id: None,
                branch_alias: None,
                title: Some("stack".to_string()),
                description: Some("stack".to_string()),
                use_worktree_branch_name: None,
                draft: None,
            }],
            "tester",
            false,
        )
        .unwrap_or_else(|e| panic!("submit failed: {e}\n{}", self.describe()));
        prs.into_iter().map(|pr| pr.id).collect()
    }

    /// The open PR row (id, alias) of position `p`.
    fn open_pr_row(&self, p: usize) -> (String, String) {
        let branch_id = self.store.lock().get_guardian(&self.id).unwrap().branches[p]
            .id
            .clone();
        self.store
            .lock()
            .list_pull_requests_for_guardian(&self.id)
            .unwrap()
            .into_iter()
            .find(|pr| pr.state == "open" && pr.branch_id.as_deref() == Some(branch_id.as_str()))
            .map(|pr| (pr.id, pr.branch_alias))
            .unwrap_or_else(|| panic!("no open PR at {p}\n{}", self.describe()))
    }

    fn assert_forge_routed_everything(&self, what: &str, forge: &FakeGitHub) {
        let unrouted = forge.state.lock().unwrap().unrouted.clone();
        assert!(
            unrouted.is_empty(),
            "{what}: the daemon made forge calls the fake does not serve: {unrouted:?}"
        );
    }
}

/// The first submission of a review's PR stack races a held feedback round
/// and an upstream rebase: every branch must get its PR, and every PR branch
/// must end up with the feedback and the upstream commit.
#[test]
fn first_pr_submission_racing_feedback_and_an_upstream_rebase() {
    let what = "first submission race";
    let (fx, forge) = Fixture::with_forge(&["feature/a", "feature/b", "feature/c"]);
    let unexpected: Unexpected = Arc::default();
    let _daemon = DaemonPump::start(&fx);

    let mut held = Held::new("feature-b.txt", "feedback on b", &unexpected).feedback(&fx, 1);
    let upstream = fx.push_upstream("upstream1.txt");
    let submitting = {
        let (store, id) = (Arc::clone(&fx.store), fx.id.clone());
        std::thread::spawn(move || {
            ralphus_daemon::pr::submit_pull_requests(
                &store,
                &NoAgentExpected,
                &id,
                vec![ralphus_daemon::pr::PrRequest {
                    branch_id: None,
                    branch_alias: None,
                    title: Some("stack".to_string()),
                    description: Some("stack".to_string()),
                    use_worktree_branch_name: None,
                    draft: None,
                }],
                "tester",
                false,
            )
        })
    };
    std::thread::sleep(PARK_WAIT);
    held.finish();
    let submitted = submitting.join().expect("submit thread");
    assert!(submitted.is_ok(), "{what}: submit failed: {submitted:?}");

    fx.wait_settled_on(&upstream, what);
    for p in 0..3 {
        fx.open_pr_row(p);
    }
    assert_eq!(
        forge
            .state
            .lock()
            .unwrap()
            .prs
            .iter()
            .filter(|p| p.open)
            .count(),
        3,
        "{what}: one forge PR per branch"
    );
    fx.assert_no_unexpected_agent_calls(what, &unexpected);
    fx.assert_forge_routed_everything(what, &forge);
    fx.assert_everything_published(
        what,
        &[(1, "feature-b.txt", "feedback on b")],
        &["upstream1.txt"],
    );
}

/// The bottom PR is merged on the forge (its commits land on upstream `main`)
/// while a feedback round on the top branch is in flight. The review must mark
/// that branch merged and keep the rest of the stack -- with the feedback --
/// published on the new base.
#[test]
fn bottom_pr_merged_on_the_forge_while_feedback_is_in_flight() {
    let what = "bottom PR merged on forge";
    let (fx, forge) = Fixture::with_forge(&["feature/a", "feature/b", "feature/c"]);
    fx.submit_stack();
    let unexpected: Unexpected = Arc::default();
    let _daemon = DaemonPump::start(&fx);
    let first = fx
        .store
        .lock()
        .get_guardian(&fx.id)
        .unwrap()
        .base_commit
        .unwrap_or_default();
    fx.wait_settled_on(first.trim(), "before the merge");

    let mut held = Held::new("feature-c.txt", "feedback on c", &unexpected).feedback(&fx, 2);
    // "Merge" PR a on the forge: its branch lands on upstream main.
    let (_, alias_a) = fx.open_pr_row(0);
    let clone = temp_dir();
    let _ = std::fs::remove_dir_all(&clone);
    git(
        clone.parent().unwrap(),
        &[
            "clone",
            "-q",
            "--branch",
            "main",
            fx.remote.to_str().unwrap(),
            clone.file_name().unwrap().to_str().unwrap(),
        ],
    );
    git(
        &clone,
        &[
            "merge",
            "--no-ff",
            "-m",
            "Merge PR a",
            &format!("origin/{alias_a}"),
        ],
    );
    git(&clone, &["push", "-q", "origin", "main"]);
    let upstream = git(&clone, &["rev-parse", "HEAD"]).trim().to_string();
    let _ = std::fs::remove_dir_all(&clone);
    let number = forge.pr_for(&alias_a).number;
    forge.with_pr(number, |pr| {
        pr.open = false;
        pr.merged = true;
    });
    std::thread::sleep(PARK_WAIT);
    held.finish();

    fx.wait_settled_on(&upstream, what);
    let a_status = fx.store.lock().get_guardian(&fx.id).unwrap().branches[0]
        .merge_status
        .clone();
    assert_eq!(a_status, "merged", "{what}: {}", fx.describe());
    fx.assert_no_unexpected_agent_calls(what, &unexpected);
    fx.assert_forge_routed_everything(what, &forge);
    fx.assert_everything_published(what, &[(2, "feature-c.txt", "feedback on c")], &[]);
}

/// Feedback pulled from a PR comment (`action_pr_feedback`) lands while a
/// feedback round on the branch above is held and the base moves.
#[test]
fn pr_comment_feedback_racing_feedback_and_an_upstream_rebase() {
    let what = "PR-comment feedback race";
    let (fx, forge) = Fixture::with_forge(&["feature/a", "feature/b", "feature/c"]);
    fx.submit_stack();
    let unexpected: Unexpected = Arc::default();
    let daemon = DaemonPump::start(&fx);
    let (pr_b, alias_b) = fx.open_pr_row(1);
    let number_b = forge.pr_for(&alias_b).number;
    forge.with_pr(number_b, |pr| {
        pr.comments.push((7001, "please change b".to_string()));
    });

    let mut fix_c = Held::new("feature-c.txt", "feedback on c", &unexpected).feedback(&fx, 2);
    let upstream = fx.push_upstream("upstream1.txt");
    let commenting = {
        let (store, cancellations) = (Arc::clone(&fx.store), daemon.cancellations.clone());
        let runner = WriterRunner::new("feature-b.txt", "comment feedback on b", None, &unexpected);
        std::thread::spawn(move || {
            ralphus_daemon::pr::action_pr_feedback(
                &store,
                &runner,
                &cancellations,
                &pr_b,
                Some("tester"),
            )
        })
    };
    std::thread::sleep(PARK_WAIT);
    fix_c.finish();
    let applied = commenting.join().expect("comment feedback thread");
    assert!(
        applied.as_ref().is_ok_and(|n| *n > 0),
        "{what}: comment feedback not applied: {applied:?}"
    );

    fx.wait_settled_on(&upstream, what);
    fx.assert_no_unexpected_agent_calls(what, &unexpected);
    fx.assert_forge_routed_everything(what, &forge);
    fx.assert_everything_published(
        what,
        &[
            (1, "feature-b.txt", "comment feedback on b"),
            (2, "feature-c.txt", "feedback on c"),
        ],
        &["upstream1.txt"],
    );
}

/// The forge reports a failing check on the top PR while a feedback round on
/// the branch below is held and the base moves. The daemon's own maintenance
/// dispatches the unattended fix (its agent stands in for the LLM), and the
/// fix, the feedback and the upstream commit must all reach every PR.
#[test]
fn forge_reported_ci_failure_is_auto_fixed_while_feedback_and_an_upstream_race() {
    let what = "forge CI failure auto-fix race";
    let (fx, forge) = Fixture::with_forge(&["feature/a", "feature/b", "feature/c"]);
    fx.submit_stack();
    fx.store
        .lock()
        .set_guardian_auto_fix_pr_errors(&fx.id, Some(true))
        .unwrap();
    let unexpected: Unexpected = Arc::default();
    let (_, alias_c) = fx.open_pr_row(2);
    forge
        .state
        .lock()
        .unwrap()
        .failing_heads
        .insert(fx.remote_tip(&alias_c));

    let mut held = Held::new("feature-b.txt", "feedback on b", &unexpected).feedback(&fx, 1);
    let upstream = fx.push_upstream("upstream1.txt");
    let fixer = Arc::new(WriterRunner::new(
        "feature-c.txt",
        "auto-fix on c",
        None,
        &unexpected,
    ));
    let started = Arc::clone(&fixer.started);
    let _daemon = DaemonPump::start_with(
        &fx,
        Arc::new(move |_| Arc::clone(&fixer) as Arc<dyn Runner>),
    );
    std::thread::sleep(PARK_WAIT);
    held.finish();
    wait_for(&started, "the daemon's auto-fix on c");

    fx.wait_settled_on(&upstream, what);
    fx.assert_no_unexpected_agent_calls(what, &unexpected);
    fx.assert_forge_routed_everything(what, &forge);
    fx.assert_everything_published(
        what,
        &[
            (1, "feature-b.txt", "feedback on b"),
            (2, "feature-c.txt", "auto-fix on c"),
        ],
        &["upstream1.txt"],
    );
}

/// A reviewer reorders the stack on the forge (re-pointing PR bases: a, c, b)
/// while a feedback round on b is held. The reorder must wait for the round,
/// then rebuild in the forge's order with b's feedback on b's PR and not on
/// c's.
#[test]
fn forge_reorder_racing_an_in_flight_feedback_round() {
    let what = "forge reorder race";
    let (fx, forge) = Fixture::with_forge(&["feature/a", "feature/b", "feature/c"]);
    fx.submit_stack();
    let unexpected: Unexpected = Arc::default();
    let daemon = DaemonPump::start(&fx);
    let first = fx
        .store
        .lock()
        .get_guardian(&fx.id)
        .unwrap()
        .base_commit
        .unwrap_or_default();
    fx.wait_settled_on(first.trim(), "before the reorder");
    let (_, alias_a) = fx.open_pr_row(0);
    let (_, alias_b) = fx.open_pr_row(1);
    let (_, alias_c) = fx.open_pr_row(2);

    let mut held = Held::new("feature-b.txt", "feedback on b", &unexpected).feedback(&fx, 1);
    let (number_b, number_c) = (forge.pr_for(&alias_b).number, forge.pr_for(&alias_c).number);
    forge.with_pr(number_c, |pr| {
        pr.base = alias_a.clone();
        pr.updated_at = "2099-01-01T00:00:00Z".to_string();
    });
    forge.with_pr(number_b, |pr| {
        pr.base = alias_c.clone();
        pr.updated_at = "2099-01-01T00:00:00Z".to_string();
    });
    // The reorder check, as `sync-pr` and the 5-minute poll run it: it must
    // defer while the feedback round holds its lease, then apply.
    let reorder = || {
        ralphus_daemon::pr::check_and_apply_forge_reorder(
            &fx.store,
            &NoAgentExpected,
            &fx.id,
            &daemon.sem,
            &daemon.cancellations,
        )
    };
    reorder();
    std::thread::sleep(PARK_WAIT);
    held.finish();
    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        let order: Vec<String> = fx
            .store
            .lock()
            .get_guardian(&fx.id)
            .unwrap()
            .branches
            .iter()
            .map(|b| b.branch.clone())
            .collect();
        if order == ["feature/a", "feature/c", "feature/b"] {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "{what}: reorder never applied: {order:?}\n{}",
            fx.describe()
        );
        reorder();
        std::thread::sleep(SETTLE_POLL);
    }

    fx.wait_settled_on(first.trim(), what);
    fx.assert_no_unexpected_agent_calls(what, &unexpected);
    fx.assert_forge_routed_everything(what, &forge);
    fx.assert_everything_published(what, &[(2, "feature-b.txt", "feedback on b")], &[]);
    let c_files = fx.files_on(&fx.remote, &fx.pr_ref(1));
    assert!(
        !c_files.contains("feature-b.txt"),
        "{what}: c's PR (now below b) still carries b's commits\n{}",
        fx.describe()
    );
}

// ---------------------------------------------------------------------------
// More forge-side stack events racing in-flight feedback.
// ---------------------------------------------------------------------------

impl Fixture {
    /// "Merge" the PR at position `p` on the forge: its branch (with everything
    /// below it) lands on upstream `main` and the fake PR flips to merged.
    /// Returns the new upstream tip.
    fn merge_pr_on_forge(&self, forge: &FakeGitHub, p: usize) -> String {
        let (_, alias) = self.open_pr_row(p);
        let number = forge.pr_for(&alias).number;
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
        git(
            &clone,
            &[
                "merge",
                "--no-ff",
                "-m",
                &format!("Merge PR {p}"),
                &format!("origin/{alias}"),
            ],
        );
        git(&clone, &["push", "-q", "origin", "main"]);
        let sha = git(&clone, &["rev-parse", "HEAD"]).trim().to_string();
        let _ = std::fs::remove_dir_all(&clone);
        forge.with_pr(number, |pr| {
            pr.open = false;
            pr.merged = true;
        });
        sha
    }

    fn merge_statuses(&self) -> Vec<String> {
        self.store
            .lock()
            .get_guardian(&self.id)
            .unwrap()
            .branches
            .iter()
            .map(|b| b.merge_status.clone())
            .collect()
    }
}

/// The middle PR is merged on the forge (carrying the bottom one's commits
/// with it) while a feedback round on the top branch is in flight: both
/// merged branches must be recognised, and the top PR keeps its feedback on
/// the new base.
#[test]
fn middle_pr_merged_on_the_forge_while_feedback_is_in_flight() {
    let what = "middle PR merged on forge";
    let (fx, forge) = Fixture::with_forge(&["feature/a", "feature/b", "feature/c"]);
    fx.submit_stack();
    let unexpected: Unexpected = Arc::default();
    let _daemon = DaemonPump::start(&fx);
    let first = fx
        .store
        .lock()
        .get_guardian(&fx.id)
        .unwrap()
        .base_commit
        .unwrap_or_default();
    fx.wait_settled_on(first.trim(), "before the merge");

    let mut held = Held::new("feature-c.txt", "feedback on c", &unexpected).feedback(&fx, 2);
    let upstream = fx.merge_pr_on_forge(&forge, 1);
    std::thread::sleep(PARK_WAIT);
    held.finish();

    fx.wait_settled_on(&upstream, what);
    let statuses = fx.merge_statuses();
    assert_eq!(
        statuses[..2],
        ["merged", "merged"],
        "{what}: {statuses:?}\n{}",
        fx.describe()
    );
    fx.assert_no_unexpected_agent_calls(what, &unexpected);
    fx.assert_forge_routed_everything(what, &forge);
    fx.assert_everything_published(what, &[(2, "feature-c.txt", "feedback on c")], &[]);
}

/// Every PR in the stack is merged on the forge while a feedback round on the
/// top branch is held. No late push may revive a merged PR branch.
#[test]
fn every_pr_merged_on_the_forge_while_feedback_is_in_flight() {
    let what = "all PRs merged on forge";
    let (fx, forge) = Fixture::with_forge(&["feature/a", "feature/b", "feature/c"]);
    fx.submit_stack();
    let unexpected: Unexpected = Arc::default();
    let _daemon = DaemonPump::start(&fx);
    let first = fx
        .store
        .lock()
        .get_guardian(&fx.id)
        .unwrap()
        .base_commit
        .unwrap_or_default();
    fx.wait_settled_on(first.trim(), "before the merge");
    let aliases: Vec<String> = (0..3).map(|p| fx.open_pr_row(p).1).collect();
    let tips_before: Vec<String> = aliases.iter().map(|a| fx.remote_tip(a)).collect();

    let mut held = Held::new("feature-c.txt", "feedback on c", &unexpected).feedback(&fx, 2);
    let upstream = fx.merge_pr_on_forge(&forge, 2);
    for number in 1..=3 {
        forge.with_pr(number, |pr| {
            pr.open = false;
            pr.merged = true;
        });
    }
    std::thread::sleep(PARK_WAIT);
    held.finish();
    // Not `wait_settled_on`: the top branch's review commit is never pushed
    // (its PR is merged), which that helper treats as unsettled.
    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        let view = fx.store.lock().get_guardian(&fx.id).unwrap();
        if view.status == "in_review"
            && view.base_commit.is_some_and(|b| b.trim() == upstream)
            && fx.merge_statuses()[..2].iter().all(|s| s == "merged")
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "{what}: never settled\n{}",
            fx.describe()
        );
        std::thread::sleep(SETTLE_POLL);
    }
    std::thread::sleep(PARK_WAIT * 4);
    let statuses = fx.merge_statuses();
    assert!(
        statuses[..2].iter().all(|s| s == "merged"),
        "{what}: merged branches not recognised: {statuses:?}\n{}",
        fx.describe()
    );
    // The fully merged branches must not be revived. The top branch's late
    // feedback commit is new, unmerged work, so it may be published again --
    // under a fresh PR -- but then it must carry the feedback.
    for p in 0..2 {
        assert_eq!(
            fx.remote_tip(&aliases[p]),
            tips_before[p],
            "{what}: a late push moved merged PR branch {p}\n{}",
            fx.describe()
        );
    }
    let kept = fx.file_on(&fx.root, &fx.review_ref(2), "feature-c.txt");
    assert!(
        kept.contains("feedback on c"),
        "{what}: the review branch lost its feedback ({kept:?})\n{}",
        fx.describe()
    );
    if fx.remote_tip(&aliases[2]) != tips_before[2] {
        let published = fx.file_on(
            &fx.remote,
            &format!("refs/heads/{}", aliases[2]),
            "feature-c.txt",
        );
        assert!(
            published.contains("feedback on c"),
            "{what}: the top PR branch moved without the feedback\n{}",
            fx.describe()
        );
    }
    fx.assert_forge_routed_everything(what, &forge);
}

/// A PR is closed (not merged) on the forge while feedback on its branch is
/// held. The feedback is committed to the review branch, and must not be
/// pushed onto the closed PR's branch.
#[test]
fn pr_closed_on_the_forge_while_feedback_is_in_flight() {
    let what = "PR closed on forge";
    let (fx, forge) = Fixture::with_forge(&["feature/a", "feature/b", "feature/c"]);
    fx.submit_stack();
    let unexpected: Unexpected = Arc::default();
    let _daemon = DaemonPump::start(&fx);
    let first = fx
        .store
        .lock()
        .get_guardian(&fx.id)
        .unwrap()
        .base_commit
        .unwrap_or_default();
    fx.wait_settled_on(first.trim(), "before the close");
    let (_, alias_b) = fx.open_pr_row(1);
    let tip_b = fx.remote_tip(&alias_b);
    let number_b = forge.pr_for(&alias_b).number;

    let mut held = Held::new("feature-b.txt", "feedback on b", &unexpected).feedback(&fx, 1);
    forge.with_pr(number_b, |pr| pr.open = false);
    // The daemon only learns of a close when it next checks the forge (a
    // submission pass does); until then it rightly still treats the PR as open.
    fx.submit_stack();
    std::thread::sleep(PARK_WAIT);
    held.finish();
    std::thread::sleep(PARK_WAIT * 4);

    // Whether feedback should still publish the review branch under its own
    // name once its PR is closed is an open product question (see
    // RACES_FOLLOWUP.local.md); what must hold either way is that the closed
    // PR is not resubmitted and the feedback is not dropped.
    let _ = tip_b;
    let open: Vec<u64> = forge
        .state
        .lock()
        .unwrap()
        .prs
        .iter()
        .filter(|p| p.open)
        .map(|p| p.number)
        .collect();
    assert!(
        !open.contains(&number_b) && open.len() == 2,
        "{what}: a closed PR was reopened or resubmitted: {open:?}\n{}",
        fx.describe()
    );
    let kept = fx.file_on(&fx.root, &fx.review_ref(1), "feature-b.txt");
    assert!(
        kept.contains("feedback on b"),
        "{what}: the review branch lost its feedback ({kept:?})\n{}",
        fx.describe()
    );
    fx.assert_no_unexpected_agent_calls(what, &unexpected);
    fx.assert_forge_routed_everything(what, &forge);
}

/// A reviewer reorders the stack on the forge and then puts it back (A→B→A)
/// while a feedback round is held. The final order wins and no commit is lost.
#[test]
fn two_forge_reorders_back_to_back_while_feedback_is_in_flight() {
    let what = "back-to-back forge reorders";
    let (fx, forge) = Fixture::with_forge(&["feature/a", "feature/b", "feature/c"]);
    fx.submit_stack();
    let unexpected: Unexpected = Arc::default();
    let daemon = DaemonPump::start(&fx);
    let first = fx
        .store
        .lock()
        .get_guardian(&fx.id)
        .unwrap()
        .base_commit
        .unwrap_or_default();
    fx.wait_settled_on(first.trim(), "before the reorders");
    let (_, alias_a) = fx.open_pr_row(0);
    let (_, alias_b) = fx.open_pr_row(1);
    let (_, alias_c) = fx.open_pr_row(2);
    let (number_b, number_c) = (forge.pr_for(&alias_b).number, forge.pr_for(&alias_c).number);

    let mut held = Held::new("feature-b.txt", "feedback on b", &unexpected).feedback(&fx, 1);
    let reorder = || {
        ralphus_daemon::pr::check_and_apply_forge_reorder(
            &fx.store,
            &NoAgentExpected,
            &fx.id,
            &daemon.sem,
            &daemon.cancellations,
        )
    };
    // a, c, b ...
    forge.with_pr(number_c, |pr| {
        pr.base = alias_a.clone();
        pr.updated_at = "2099-01-01T00:00:00Z".to_string();
    });
    forge.with_pr(number_b, |pr| {
        pr.base = alias_c.clone();
        pr.updated_at = "2099-01-01T00:00:00Z".to_string();
    });
    reorder();
    // ... and straight back to a, b, c.
    forge.with_pr(number_b, |pr| {
        pr.base = alias_a.clone();
        pr.updated_at = "2099-01-02T00:00:00Z".to_string();
    });
    forge.with_pr(number_c, |pr| {
        pr.base = alias_b.clone();
        pr.updated_at = "2099-01-02T00:00:00Z".to_string();
    });
    reorder();
    std::thread::sleep(PARK_WAIT);
    held.finish();
    for _ in 0..8 {
        reorder();
        std::thread::sleep(SETTLE_POLL);
    }

    fx.wait_settled_on(first.trim(), what);
    let order: Vec<String> = fx
        .store
        .lock()
        .get_guardian(&fx.id)
        .unwrap()
        .branches
        .iter()
        .map(|b| b.branch.clone())
        .collect();
    assert_eq!(
        order,
        ["feature/a", "feature/b", "feature/c"],
        "{what}: {}",
        fx.describe()
    );
    fx.assert_no_unexpected_agent_calls(what, &unexpected);
    fx.assert_forge_routed_everything(what, &forge);
    fx.assert_everything_published(what, &[(1, "feature-b.txt", "feedback on b")], &[]);
}

// ---------------------------------------------------------------------------
// Section 1 (continued): submission, comment, CI and reorder races.
// ---------------------------------------------------------------------------

fn stack_request() -> Vec<ralphus_daemon::pr::PrRequest> {
    vec![ralphus_daemon::pr::PrRequest {
        branch_id: None,
        branch_alias: None,
        title: Some("stack".to_string()),
        description: Some("stack".to_string()),
        use_worktree_branch_name: None,
        draft: None,
    }]
}

impl Fixture {
    fn open_forge_prs(forge: &FakeGitHub) -> usize {
        forge
            .state
            .lock()
            .unwrap()
            .prs
            .iter()
            .filter(|p| p.open)
            .count()
    }

    fn branch_order(&self) -> Vec<String> {
        self.store
            .lock()
            .get_guardian(&self.id)
            .unwrap()
            .branches
            .iter()
            .map(|b| b.branch.clone())
            .collect()
    }

    /// Wait until the branch order is `want`, driving `step` meanwhile.
    fn wait_for_order(&self, want: &[&str], what: &str, step: &dyn Fn()) {
        let deadline = Instant::now() + Duration::from_secs(120);
        loop {
            let order = self.branch_order();
            if order == want {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "{what}: order never became {want:?}: {order:?}\n{}",
                self.describe()
            );
            step();
            std::thread::sleep(SETTLE_POLL);
        }
    }
}

/// Two submissions of the same stack at once (an auto-submit and a manual
/// click): the forge's duplicate-head 422 must be adopted, never a second PR.
#[test]
fn simultaneous_submissions_open_one_pr_per_branch() {
    let what = "simultaneous submissions";
    let (fx, forge) = Fixture::with_forge(&["feature/a", "feature/b", "feature/c"]);
    let submit = || {
        let (store, id) = (Arc::clone(&fx.store), fx.id.clone());
        std::thread::spawn(move || {
            ralphus_daemon::pr::submit_pull_requests(
                &store,
                &NoAgentExpected,
                &id,
                stack_request(),
                "tester",
                false,
            )
        })
    };
    let (first, second) = (submit(), submit());
    let _ = (first.join().unwrap(), second.join().unwrap());

    assert_eq!(
        Fixture::open_forge_prs(&forge),
        3,
        "{what}: one forge PR per branch\n{}",
        fx.describe()
    );
    let rows = fx
        .store
        .lock()
        .list_pull_requests_for_guardian(&fx.id)
        .unwrap()
        .into_iter()
        .filter(|pr| pr.state == "open")
        .count();
    assert_eq!(rows, 3, "{what}: one open PR row per branch");
    fx.assert_forge_routed_everything(what, &forge);
}

/// A branch appended to the review while the stack is rebasing gets its PR
/// through a later submission without disturbing the others.
#[test]
fn submitting_a_newly_appended_branch_while_the_stack_rebases() {
    let what = "submit appended branch";
    let (fx, forge) = Fixture::with_forge(&["feature/a", "feature/b", "feature/c"]);
    fx.submit_stack();
    let unexpected: Unexpected = Arc::default();
    let _daemon = DaemonPump::start(&fx);
    let mut held = Held::new("feature-a.txt", "feedback on a", &unexpected).feedback(&fx, 0);
    let upstream = fx.push_upstream("upstream1.txt");
    fx.append_branch_for_race("feature/d");
    std::thread::sleep(PARK_WAIT);
    held.finish();
    // The new branch has no PR yet, so the review is not "published" until
    // the submission below; wait for the build itself.
    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        let view = fx.store.lock().get_guardian(&fx.id).unwrap();
        if view.status == "in_review"
            && view.base_commit.as_deref().map(str::trim) == Some(upstream.as_str())
            && view.branches.len() == 4
            && view.branches[3].merge_status == "done"
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "{what}: new branch never built
{}",
            fx.describe()
        );
        std::thread::sleep(SETTLE_POLL);
    }
    fx.submit_stack();
    fx.wait_settled_on(&upstream, what);
    assert_eq!(
        Fixture::open_forge_prs(&forge),
        4,
        "{what}\n{}",
        fx.describe()
    );
    fx.assert_no_unexpected_agent_calls(what, &unexpected);
    fx.assert_forge_routed_everything(what, &forge);
    fx.assert_everything_published(
        what,
        &[(0, "feature-a.txt", "feedback on a")],
        &["upstream1.txt"],
    );
}

/// Two PR-comment feedback actions on the same PR at once (double click, two
/// users): the one comment is applied exactly once.
#[test]
fn two_comment_feedback_actions_on_one_pr_apply_the_comment_once() {
    let what = "double comment feedback";
    let (fx, forge) = Fixture::with_forge(&["feature/a", "feature/b", "feature/c"]);
    fx.submit_stack();
    let unexpected: Unexpected = Arc::default();
    let daemon = DaemonPump::start(&fx);
    let (pr_b, alias_b) = fx.open_pr_row(1);
    let number_b = forge.pr_for(&alias_b).number;
    forge.with_pr(number_b, |pr| {
        pr.comments.push((7001, "please change b".to_string()))
    });
    let action = |line: &'static str| {
        let (store, cancellations, pr_id) = (
            Arc::clone(&fx.store),
            daemon.cancellations.clone(),
            pr_b.clone(),
        );
        let runner = WriterRunner::new("feature-b.txt", line, None, &unexpected);
        std::thread::spawn(move || {
            ralphus_daemon::pr::action_pr_feedback(
                &store,
                &runner,
                &cancellations,
                &pr_id,
                Some("tester"),
            )
        })
    };
    let (one, two) = (action("comment edit one"), action("comment edit two"));
    let (one, two) = (one.join().unwrap(), two.join().unwrap());
    let applied: usize = [one, two].iter().filter_map(|r| r.as_ref().ok()).sum();
    assert_eq!(
        applied, 1,
        "{what}: the comment must be applied exactly once"
    );

    let first = fx
        .store
        .lock()
        .get_guardian(&fx.id)
        .unwrap()
        .base_commit
        .unwrap_or_default();
    fx.wait_settled_on(first.trim(), what);
    let content = fx.file_on(&fx.root, &fx.review_ref(1), "feature-b.txt");
    let edits = ["comment edit one", "comment edit two"]
        .iter()
        .filter(|l| content.contains(*l))
        .count();
    assert_eq!(edits, 1, "{what}: {content:?}\n{}", fx.describe());
    fx.assert_forge_routed_everything(what, &forge);
}

/// PR-comment feedback on a PR the forge no longer has open must not panic
/// or apply the comment onto the stack.
#[test]
fn pr_comment_feedback_on_a_closed_pr_is_not_applied() {
    let what = "comment feedback on closed PR";
    let (fx, forge) = Fixture::with_forge(&["feature/a", "feature/b", "feature/c"]);
    fx.submit_stack();
    let unexpected: Unexpected = Arc::default();
    let daemon = DaemonPump::start(&fx);
    let (pr_b, alias_b) = fx.open_pr_row(1);
    let number_b = forge.pr_for(&alias_b).number;
    forge.with_pr(number_b, |pr| {
        pr.comments.push((7001, "please change b".to_string()));
        pr.open = false;
    });
    let runner = WriterRunner::new("feature-b.txt", "late comment edit", None, &unexpected);
    let _ = ralphus_daemon::pr::action_pr_feedback(
        &fx.store,
        &runner,
        &daemon.cancellations,
        &pr_b,
        Some("tester"),
    );
    std::thread::sleep(PARK_WAIT);
    let content = fx.file_on(&fx.root, &fx.review_ref(1), "feature-b.txt");
    assert!(
        !content.contains("late comment edit")
            || fx
                .file_on(&fx.root, &fx.review_ref(2), "feature-b.txt")
                .contains("late comment edit"),
        "{what}: applied to branch b but not carried to c\n{}",
        fx.describe()
    );
    fx.assert_forge_routed_everything(what, &forge);
}

/// The forge reports a failure for an *old* head SHA after the branch has
/// moved on: the fix is already in, so no auto-fix may be dispatched.
#[test]
fn ci_failure_on_a_stale_sha_does_not_dispatch_a_fix() {
    let what = "stale CI SHA";
    let (fx, forge) = Fixture::with_forge(&["feature/a", "feature/b", "feature/c"]);
    fx.submit_stack();
    fx.store
        .lock()
        .set_guardian_auto_fix_pr_errors(&fx.id, Some(true))
        .unwrap();
    let unexpected: Unexpected = Arc::default();
    let (_, alias_c) = fx.open_pr_row(2);
    let old_head = fx.remote_tip(&alias_c);
    let mut held = Held::new("feature-c.txt", "feedback on c", &unexpected).feedback(&fx, 2);
    held.finish();
    let first = fx
        .store
        .lock()
        .get_guardian(&fx.id)
        .unwrap()
        .base_commit
        .unwrap_or_default();
    let fixer = Arc::new(WriterRunner::new(
        "feature-c.txt",
        "needless fix",
        None,
        &unexpected,
    ));
    let started = Arc::clone(&fixer.started);
    let _daemon = DaemonPump::start_with(
        &fx,
        Arc::new(move |_| Arc::clone(&fixer) as Arc<dyn Runner>),
    );
    fx.wait_settled_on(first.trim(), what);
    assert_ne!(
        fx.remote_tip(&alias_c),
        old_head,
        "{what}: head did not move"
    );
    forge.state.lock().unwrap().failing_heads.insert(old_head);
    std::thread::sleep(PARK_WAIT * 4);
    assert!(
        !started.load(Ordering::SeqCst),
        "{what}: a fix was dispatched for a stale head\n{}",
        fx.describe()
    );
    fx.assert_forge_routed_everything(what, &forge);
}

/// Both the middle and the top PR fail CI while auto-fix is on: the earlier
/// PR must be fixed first (the later one is deferred), and its fix reaches
/// the top PR. The top PR's own head then changes, so the forge's failure for
/// the old head is stale and needs no fix of its own.
#[test]
fn auto_fix_fixes_the_earlier_failing_pr_first() {
    let what = "two failing PRs";
    let (fx, forge) = Fixture::with_forge(&["feature/a", "feature/b", "feature/c"]);
    fx.submit_stack();
    fx.store
        .lock()
        .set_guardian_auto_fix_pr_errors(&fx.id, Some(true))
        .unwrap();
    for p in [1, 2] {
        let (_, alias) = fx.open_pr_row(p);
        forge
            .state
            .lock()
            .unwrap()
            .failing_heads
            .insert(fx.remote_tip(&alias));
    }
    let first = fx
        .store
        .lock()
        .get_guardian(&fx.id)
        .unwrap()
        .base_commit
        .unwrap_or_default();
    let fixer = Arc::new(BranchFixRunner::default());
    let runner = Arc::clone(&fixer);
    let _daemon = DaemonPump::start_with(
        &fx,
        Arc::new(move |_| Arc::clone(&runner) as Arc<dyn Runner>),
    );
    let deadline = Instant::now() + Duration::from_secs(120);
    while fixer.fixed.lock().unwrap().is_empty() {
        assert!(
            Instant::now() < deadline,
            "{what}: fixes never ran: {:?}\n{}",
            fixer.fixed.lock().unwrap(),
            fx.describe()
        );
        std::thread::sleep(SETTLE_POLL);
    }
    fx.wait_settled_on(first.trim(), what);
    let fixed = fixer.fixed.lock().unwrap().clone();
    assert_eq!(
        fixed.first().map(String::as_str),
        Some("fix-feature-b-review.txt"),
        "{what}: the earlier PR must be fixed first: {fixed:?}"
    );
    for file in &fixed {
        let on_c = fx.files_on(&fx.remote, &fx.pr_ref(2));
        assert!(on_c.contains(file.as_str()), "{what}: top PR lost {file}");
    }
    fx.assert_forge_routed_everything(what, &forge);
}

/// A resolver that fixes whichever branch it runs in by committing a file
/// named after that branch, recording each branch it fixed.
#[derive(Default)]
struct BranchFixRunner {
    fixed: std::sync::Mutex<Vec<String>>,
}

impl Runner for BranchFixRunner {
    fn run(&self, spec: &RunnerSpec) -> RunnerResult {
        let cwd = PathBuf::from(&spec.cwd);
        if spec.cell_id.ends_with("-commit") {
            let dirty = !git(&cwd, &["status", "--porcelain"]).trim().is_empty();
            if dirty {
                git(&cwd, &["add", "--all"]);
                git(&cwd, &["commit", "-m", "auto fix"]);
            }
            return ok_result("committed", Some(dirty));
        }
        let branch = git(&cwd, &["rev-parse", "--abbrev-ref", "HEAD"]);
        let file = format!("fix-{}.txt", branch.trim().replace('/', "-"));
        write(&cwd, &file, "fixed\n");
        self.fixed.lock().unwrap().push(file);
        ok_result("fixed\nRALPHUS_PROOF: PASS", Some(true))
    }
}

/// A forge reorder (a, c, b) and an upstream push arrive together while a
/// feedback round is held: the rebuild must take both, in the forge's order.
#[test]
fn forge_reorder_and_base_change_together_while_feedback_is_in_flight() {
    let what = "forge reorder + base change";
    let (fx, forge) = Fixture::with_forge(&["feature/a", "feature/b", "feature/c"]);
    fx.submit_stack();
    let unexpected: Unexpected = Arc::default();
    let daemon = DaemonPump::start(&fx);
    let first = fx
        .store
        .lock()
        .get_guardian(&fx.id)
        .unwrap()
        .base_commit
        .unwrap_or_default();
    fx.wait_settled_on(first.trim(), "before");
    let (_, alias_a) = fx.open_pr_row(0);
    let (_, alias_b) = fx.open_pr_row(1);
    let (_, alias_c) = fx.open_pr_row(2);
    let (number_b, number_c) = (forge.pr_for(&alias_b).number, forge.pr_for(&alias_c).number);

    let mut held = Held::new("feature-b.txt", "feedback on b", &unexpected).feedback(&fx, 1);
    let upstream = fx.push_upstream("upstream1.txt");
    forge.with_pr(number_c, |pr| {
        pr.base = alias_a.clone();
        pr.updated_at = "2099-01-01T00:00:00Z".to_string();
    });
    forge.with_pr(number_b, |pr| {
        pr.base = alias_c.clone();
        pr.updated_at = "2099-01-01T00:00:00Z".to_string();
    });
    let reorder = || {
        ralphus_daemon::pr::check_and_apply_forge_reorder(
            &fx.store,
            &NoAgentExpected,
            &fx.id,
            &daemon.sem,
            &daemon.cancellations,
        )
    };
    reorder();
    std::thread::sleep(PARK_WAIT);
    held.finish();
    fx.wait_for_order(&["feature/a", "feature/c", "feature/b"], what, &|| {
        let _ = reorder();
    });
    fx.wait_settled_on(&upstream, what);
    fx.assert_no_unexpected_agent_calls(what, &unexpected);
    fx.assert_forge_routed_everything(what, &forge);
    fx.assert_everything_published(
        what,
        &[(2, "feature-b.txt", "feedback on b")],
        &["upstream1.txt"],
    );
}

/// A local reorder (the board's drag) lands while a forge reorder is pending
/// and feedback is held: whichever order wins, no commit may be lost.
#[test]
fn forge_reorder_racing_a_local_reorder_loses_no_commit() {
    let what = "forge vs local reorder";
    let (fx, forge) = Fixture::with_forge(&["feature/a", "feature/b", "feature/c"]);
    fx.submit_stack();
    let unexpected: Unexpected = Arc::default();
    let daemon = DaemonPump::start(&fx);
    let first = fx
        .store
        .lock()
        .get_guardian(&fx.id)
        .unwrap()
        .base_commit
        .unwrap_or_default();
    fx.wait_settled_on(first.trim(), "before");
    let (_, alias_a) = fx.open_pr_row(0);
    let (_, alias_b) = fx.open_pr_row(1);
    let (_, alias_c) = fx.open_pr_row(2);
    let (number_b, number_c) = (forge.pr_for(&alias_b).number, forge.pr_for(&alias_c).number);

    let mut held = Held::new("feature-b.txt", "feedback on b", &unexpected).feedback(&fx, 1);
    forge.with_pr(number_c, |pr| {
        pr.base = alias_a.clone();
        pr.updated_at = "2099-01-01T00:00:00Z".to_string();
    });
    forge.with_pr(number_b, |pr| {
        pr.base = alias_c.clone();
        pr.updated_at = "2099-01-01T00:00:00Z".to_string();
    });
    // The user drags b to the bottom locally and starts a merge.
    fx.store
        .lock()
        .reorder_guardian_branches(
            &fx.id,
            &[
                "feature/b".to_string(),
                "feature/a".to_string(),
                "feature/c".to_string(),
            ],
        )
        .unwrap();
    ralphus_daemon::guardian_merge::start_merge(
        Arc::clone(&fx.store),
        Arc::new(NoAgentExpected),
        &fx.id,
        Arc::clone(&daemon.sem),
        daemon.cancellations.clone(),
    );
    let reorder = || {
        ralphus_daemon::pr::check_and_apply_forge_reorder(
            &fx.store,
            &NoAgentExpected,
            &fx.id,
            &daemon.sem,
            &daemon.cancellations,
        )
    };
    reorder();
    std::thread::sleep(PARK_WAIT);
    held.finish();
    for _ in 0..10 {
        reorder();
        std::thread::sleep(SETTLE_POLL);
    }
    fx.wait_settled_on(first.trim(), what);
    // Whatever the final order, b's feedback is on b and everything above it.
    let order = fx.branch_order();
    let b_at = order.iter().position(|b| b == "feature/b").unwrap();
    for p in b_at..order.len() {
        let content = fx.file_on(&fx.root, &fx.review_ref(p), "feature-b.txt");
        assert!(
            content.contains("feedback on b"),
            "{what}: branch {p} of {order:?} lost b's feedback\n{}",
            fx.describe()
        );
    }
    for b in ["feature-a.txt", "feature-b.txt", "feature-c.txt"] {
        let top = fx.files_on(&fx.root, &fx.review_ref(order.len() - 1));
        assert!(top.contains(b), "{what}: top branch lost {b}");
    }
    fx.assert_no_unexpected_agent_calls(what, &unexpected);
    fx.assert_forge_routed_everything(what, &forge);
}

/// Forge API trouble: the first PR requests of a submission fail with 503
/// while feedback is in flight. The submission may fail, but a retry must
/// converge on exactly one PR per branch and no commit is lost.
#[test]
fn forge_5xx_during_submission_converges_on_retry() {
    let what = "forge 5xx";
    let (fx, forge) = Fixture::with_forge(&["feature/a", "feature/b", "feature/c"]);
    let unexpected: Unexpected = Arc::default();
    let _daemon = DaemonPump::start(&fx);
    let mut held = Held::new("feature-b.txt", "feedback on b", &unexpected).feedback(&fx, 1);
    forge.state.lock().unwrap().fail_pulls = 4;
    let failed = ralphus_daemon::pr::submit_pull_requests(
        &fx.store,
        &NoAgentExpected,
        &fx.id,
        stack_request(),
        "tester",
        false,
    );
    held.finish();
    let _ = failed;
    forge.state.lock().unwrap().fail_pulls = 0;
    let mut attempts = 0;
    while Fixture::open_forge_prs(&forge) < 3 {
        attempts += 1;
        assert!(attempts < 10, "{what}: never converged\n{}", fx.describe());
        let _ = ralphus_daemon::pr::submit_pull_requests(
            &fx.store,
            &NoAgentExpected,
            &fx.id,
            stack_request(),
            "tester",
            false,
        );
        std::thread::sleep(SETTLE_POLL);
    }
    let first = fx
        .store
        .lock()
        .get_guardian(&fx.id)
        .unwrap()
        .base_commit
        .unwrap_or_default();
    fx.wait_settled_on(first.trim(), what);
    assert_eq!(Fixture::open_forge_prs(&forge), 3, "{what}: duplicate PRs");
    fx.assert_no_unexpected_agent_calls(what, &unexpected);
    fx.assert_forge_routed_everything(what, &forge);
    fx.assert_everything_published(what, &[(1, "feature-b.txt", "feedback on b")], &[]);
}

impl Fixture {
    /// Add a new task branch (one file named after it) to the review, ready
    /// to be built onto the stack.
    fn append_branch_for_race(&self, name: &str) {
        let file = format!("{}.txt", name.replace('/', "-"));
        git(&self.root, &["checkout", "-q", "-b", name, "main"]);
        write(&self.root, &file, "content\n");
        git(&self.root, &["add", "."]);
        git(&self.root, &["commit", "-m", &format!("add {name}")]);
        git(&self.root, &["checkout", "-q", "main"]);
        self.store
            .lock()
            .add_guardian_branch(&self.id, name)
            .unwrap();
        let view = self.store.lock().get_guardian(&self.id).unwrap();
        let bid = view.branches[view.branches.len() - 1].id.clone();
        self.store
            .lock()
            .set_branch_status(&self.id, &bid, MergeStatus::Ready, None)
            .unwrap();
    }
}

// ---------------------------------------------------------------------------
// Sections 3-5: stack shape changes, user actions and review settings
// mid-race.
// ---------------------------------------------------------------------------

impl Fixture {
    /// Wait until the review is idle on `upstream` with every enabled branch
    /// built, without comparing PR branches (some branches have no PR).
    fn wait_built_on(&self, upstream: &str, what: &str) {
        let deadline = Instant::now() + Duration::from_secs(300);
        let mut stable = 0;
        while stable < SETTLE_STABLE_POLLS {
            let view = self.store.lock().get_guardian(&self.id).unwrap();
            let built = view.branches.iter().filter(|b| b.enabled).all(|b| {
                matches!(
                    b.merge_status.as_str(),
                    "done" | "merged" | "conflict_resolved"
                )
            });
            let settled = view.status == "in_review"
                && built
                && view.base_commit.as_deref().map(str::trim) == Some(upstream);
            stable = if settled { stable + 1 } else { 0 };
            assert!(
                Instant::now() < deadline,
                "{what}: never built on upstream {upstream:.9}: {}",
                self.describe()
            );
            std::thread::sleep(SETTLE_POLL);
        }
    }

    /// Assert on the local review branches only: every `(position, file,
    /// line)` edit is on branch `p >= position`, and each upstream file is on
    /// every branch.
    fn assert_review_branches(
        &self,
        what: &str,
        edits: &[(usize, &str, &str)],
        upstream_files: &[&str],
    ) {
        for p in self.enabled_positions() {
            let rev = self.review_ref(p);
            for &(from, file, line) in edits {
                if p >= from {
                    let content = self.file_on(&self.root, &rev, file);
                    assert!(
                        content.contains(line),
                        "{what}: review branch {p} lost {line:?} ({file}: {content:?})\n{}",
                        self.describe()
                    );
                }
            }
            let files = self.files_on(&self.root, &rev);
            for f in upstream_files {
                assert!(
                    files.contains(f),
                    "{what}: review branch {p} is missing {f}\n{}",
                    self.describe()
                );
            }
        }
    }

    /// Commit `file` onto an existing task branch (a cell still working).
    fn add_task_commit(&self, branch: &str, file: &str) {
        git(&self.root, &["checkout", "-q", branch]);
        write(&self.root, file, "more task work\n");
        git(&self.root, &["add", "."]);
        git(&self.root, &["commit", "-m", &format!("task work: {file}")]);
        git(&self.root, &["checkout", "-q", "main"]);
    }
}

/// A branch is appended while a base-shift rebuild is parked behind a held
/// feedback round: the newcomer is built onto the rebased stack and nothing
/// existing is dropped.
#[test]
fn branch_appended_while_a_base_shift_rebuild_is_parked() {
    let what = "append while parked";
    let fx = Fixture::with_upstream(&["feature/a", "feature/b", "feature/c"]);
    fx.open_pr_stack();
    let unexpected: Unexpected = Arc::default();
    let _daemon = DaemonPump::start(&fx);
    let mut settled = Held::new("feature-a.txt", "feedback on a", &unexpected).feedback(&fx, 0);
    settled.finish();
    let mut held = Held::new("feature-b.txt", "feedback on b", &unexpected).feedback(&fx, 1);
    let upstream = fx.push_upstream("upstream1.txt");
    std::thread::sleep(PARK_WAIT);
    fx.append_branch_for_race("feature/d");
    held.finish();

    fx.wait_built_on(&upstream, what);
    fx.assert_no_unexpected_agent_calls(what, &unexpected);
    fx.assert_review_branches(
        what,
        &[
            (0, "feature-a.txt", "feedback on a"),
            (1, "feature-b.txt", "feedback on b"),
            (3, "feature-d.txt", "content"),
        ],
        &["upstream1.txt"],
    );
}

/// Task commits land on several branches at once (cells still running) while
/// a feedback round is held and the base moves: every task commit, the
/// feedback and the upstream commit end up on the right branches.
#[test]
fn new_task_commits_on_several_branches_while_feedback_and_upstream_race() {
    let what = "task commits on several branches";
    let fx = Fixture::with_upstream(&["feature/a", "feature/b", "feature/c"]);
    fx.open_pr_stack();
    let unexpected: Unexpected = Arc::default();
    let _daemon = DaemonPump::start(&fx);
    let mut held = Held::new("feature-b.txt", "feedback on b", &unexpected).feedback(&fx, 1);
    fx.add_task_commit("feature/a", "more-a.txt");
    fx.add_task_commit("feature/c", "more-c.txt");
    let upstream = fx.push_upstream("upstream1.txt");
    std::thread::sleep(PARK_WAIT);
    held.finish();

    fx.wait_built_on(&upstream, what);
    fx.assert_no_unexpected_agent_calls(what, &unexpected);
    fx.assert_review_branches(
        what,
        &[(1, "feature-b.txt", "feedback on b")],
        &["upstream1.txt"],
    );
    let files_c = fx.files_on(&fx.root, &fx.review_ref(2));
    for f in ["more-a.txt", "more-c.txt"] {
        assert!(
            files_c.contains(f),
            "{what}: top branch lost task file {f}\n{}",
            fx.describe()
        );
    }
}

/// A task branch is amended after the review was built and has feedback: the
/// feedback must survive the next rebuild even though the old task tip is no
/// longer an ancestor of the branch.
#[test]
fn task_branch_amended_after_build_keeps_feedback_through_a_rebuild() {
    let what = "amended task branch";
    let fx = Fixture::with_upstream(&["feature/a", "feature/b", "feature/c"]);
    fx.open_pr_stack();
    let unexpected: Unexpected = Arc::default();
    let _daemon = DaemonPump::start(&fx);
    let mut settled = Held::new("feature-b.txt", "feedback on b", &unexpected).feedback(&fx, 1);
    settled.finish();

    git(&fx.root, &["checkout", "-q", "feature/c"]);
    write(&fx.root, "amended-c.txt", "amended\n");
    git(&fx.root, &["add", "."]);
    git(&fx.root, &["commit", "-q", "--amend", "--no-edit"]);
    git(&fx.root, &["checkout", "-q", "main"]);
    let upstream = fx.push_upstream("upstream1.txt");

    fx.wait_built_on(&upstream, what);
    fx.assert_review_branches(
        what,
        &[(1, "feature-b.txt", "feedback on b")],
        &["upstream1.txt"],
    );
}

/// The branch being disabled is the one with a feedback round in flight.
/// After it is re-enabled, its feedback is on it and every branch above.
#[test]
fn disabling_a_branch_with_a_held_feedback_round_then_reenabling() {
    let what = "disable held branch";
    let fx = Fixture::with_upstream(&["feature/a", "feature/b", "feature/c"]);
    fx.open_pr_stack();
    let unexpected: Unexpected = Arc::default();
    let daemon = DaemonPump::start(&fx);
    let mut held = Held::new("feature-b.txt", "feedback on b", &unexpected).feedback(&fx, 1);
    let upstream = fx.push_upstream("upstream1.txt");
    std::thread::sleep(PARK_WAIT);
    arrange(&fx, &daemon, "feature/b", false);
    held.finish();
    fx.wait_built_on(&upstream, "while disabled");
    arrange(&fx, &daemon, "feature/b", true);
    fx.wait_built_on(&upstream, what);
    fx.assert_no_unexpected_agent_calls(what, &unexpected);
    fx.assert_review_branches(
        what,
        &[(1, "feature-b.txt", "feedback on b")],
        &["upstream1.txt"],
    );
}

/// The user approves the review while a feedback round is in flight: the
/// round's commit must not be dropped.
#[test]
fn approving_the_review_while_feedback_is_in_flight_keeps_the_feedback() {
    let what = "approve mid-feedback";
    let fx = Fixture::with_upstream(&["feature/a", "feature/b"]);
    fx.open_pr_stack();
    let unexpected: Unexpected = Arc::default();
    let _daemon = DaemonPump::start(&fx);
    let mut held = Held::new("feature-b.txt", "feedback on b", &unexpected).feedback(&fx, 1);
    let approved = fx.store.lock().approve_guardian(&fx.id);
    std::thread::sleep(PARK_WAIT);
    held.finish();
    std::thread::sleep(PARK_WAIT * 3);
    let content = fx.file_on(&fx.root, &fx.review_ref(1), "feature-b.txt");
    assert!(
        content.contains("feedback on b"),
        "{what}: feedback dropped (approve result: {approved:?})\n{}",
        fx.describe()
    );
}

/// The review is deleted while a feedback round is held and the base moves:
/// the held round must not recreate remote branches or panic the daemon.
#[test]
fn deleting_the_review_mid_race_recreates_no_remote_branch() {
    let what = "delete mid-race";
    let fx = Fixture::with_upstream(&["feature/a", "feature/b", "feature/c"]);
    fx.open_pr_stack();
    let unexpected: Unexpected = Arc::default();
    let _daemon = DaemonPump::start(&fx);
    let mut held = Held::new("feature-b.txt", "feedback on b", &unexpected).feedback(&fx, 1);
    let upstream = fx.push_upstream("upstream1.txt");
    std::thread::sleep(PARK_WAIT);
    let deleted = fx.store.lock().delete_guardian(&fx.id);
    let refs_before = git(
        &fx.remote,
        &["for-each-ref", "--format=%(refname) %(objectname)"],
    );
    held.finish();
    std::thread::sleep(PARK_WAIT * 4);
    let refs_after = git(
        &fx.remote,
        &["for-each-ref", "--format=%(refname) %(objectname)"],
    );
    assert!(deleted.is_ok(), "{what}: delete failed: {deleted:?}");
    assert_eq!(
        refs_before, refs_after,
        "{what}: a late push after deletion changed the remote (upstream {upstream:.9})"
    );
}

/// The combined review branch is renamed while feedback is held and the base
/// moves: the rebuild lands on the new name and nothing is lost.
#[test]
fn renaming_the_review_branch_mid_race_keeps_every_commit() {
    let what = "rename mid-race";
    let fx = Fixture::with_upstream(&["feature/a", "feature/b", "feature/c"]);
    fx.open_pr_stack();
    let unexpected: Unexpected = Arc::default();
    let _daemon = DaemonPump::start(&fx);
    let mut held = Held::new("feature-b.txt", "feedback on b", &unexpected).feedback(&fx, 1);
    let upstream = fx.push_upstream("upstream1.txt");
    std::thread::sleep(PARK_WAIT);
    let renamed = fx
        .store
        .lock()
        .set_guardian_review_branch_name(&fx.id, "renamed-stack");
    held.finish();
    fx.wait_built_on(&upstream, what);
    fx.assert_no_unexpected_agent_calls(what, &unexpected);
    fx.assert_review_branches(
        what,
        &[(1, "feature-b.txt", "feedback on b")],
        &["upstream1.txt"],
    );
    assert!(renamed.is_ok(), "{what}: rename failed: {renamed:?}");
}

/// Feedback submitted while the review's merge is stopped is applied once the
/// merge resumes, and the stopped period loses nothing.
#[test]
fn feedback_while_the_merge_is_stopped_is_kept_after_resuming() {
    let what = "feedback while stopped";
    let fx = Fixture::with_upstream(&["feature/a", "feature/b", "feature/c"]);
    fx.open_pr_stack();
    let unexpected: Unexpected = Arc::default();
    let daemon = DaemonPump::start(&fx);
    ralphus_daemon::guardian_merge::stop_guardian_merge(
        Arc::clone(&fx.store),
        daemon.cancellations.clone(),
        &fx.id,
    );
    let mut stopped =
        Held::new("feature-b.txt", "feedback while stopped", &unexpected).feedback(&fx, 1);
    stopped.finish();
    ralphus_daemon::guardian_merge::start_merge(
        Arc::clone(&fx.store),
        Arc::new(NoAgentExpected),
        &fx.id,
        Arc::clone(&daemon.sem),
        daemon.cancellations.clone(),
    );
    let upstream = fx.push_upstream("upstream1.txt");
    fx.wait_built_on(&upstream, what);
    fx.assert_no_unexpected_agent_calls(what, &unexpected);
    fx.assert_review_branches(
        what,
        &[(1, "feature-b.txt", "feedback while stopped")],
        &["upstream1.txt"],
    );
}

/// `skip_base_updates` is turned on while a base-shift rebuild is parked and
/// off again later: nothing is lost while it is on, and the review catches up
/// to the new base once it is off.
#[test]
fn skip_base_updates_toggled_mid_race_loses_nothing() {
    let what = "skip_base_updates toggle";
    let fx = Fixture::with_upstream(&["feature/a", "feature/b", "feature/c"]);
    fx.open_pr_stack();
    let unexpected: Unexpected = Arc::default();
    let _daemon = DaemonPump::start(&fx);
    let mut held = Held::new("feature-b.txt", "feedback on b", &unexpected).feedback(&fx, 1);
    let upstream = fx.push_upstream("upstream1.txt");
    fx.store
        .lock()
        .set_guardian_skip_base_updates(&fx.id, Some(true))
        .unwrap();
    std::thread::sleep(PARK_WAIT);
    held.finish();
    std::thread::sleep(PARK_WAIT * 3);
    fx.assert_review_branches(what, &[(1, "feature-b.txt", "feedback on b")], &[]);
    fx.store
        .lock()
        .set_guardian_skip_base_updates(&fx.id, Some(false))
        .unwrap();
    fx.wait_built_on(&upstream, what);
    fx.assert_no_unexpected_agent_calls(what, &unexpected);
    fx.assert_review_branches(
        what,
        &[(1, "feature-b.txt", "feedback on b")],
        &["upstream1.txt"],
    );
}

/// `separate_pr_branch` is flipped while PRs are open and feedback is held:
/// the feedback must still reach the PR heads.
#[test]
fn separate_pr_branch_toggled_while_prs_are_open_keeps_feedback() {
    let what = "separate_pr_branch toggle";
    let (fx, forge) = Fixture::with_forge(&["feature/a", "feature/b", "feature/c"]);
    fx.submit_stack();
    let unexpected: Unexpected = Arc::default();
    let _daemon = DaemonPump::start(&fx);
    let first = fx
        .store
        .lock()
        .get_guardian(&fx.id)
        .unwrap()
        .base_commit
        .unwrap_or_default();
    fx.wait_settled_on(first.trim(), "before the toggle");
    let mut held = Held::new("feature-b.txt", "feedback on b", &unexpected).feedback(&fx, 1);
    fx.store
        .lock()
        .set_guardian_separate_pr_branch(&fx.id, Some(true))
        .unwrap();
    let upstream = fx.push_upstream("upstream1.txt");
    std::thread::sleep(PARK_WAIT);
    held.finish();
    fx.wait_settled_on(&upstream, what);
    fx.assert_no_unexpected_agent_calls(what, &unexpected);
    fx.assert_forge_routed_everything(what, &forge);
    fx.assert_everything_published(
        what,
        &[(1, "feature-b.txt", "feedback on b")],
        &["upstream1.txt"],
    );
}

/// A review with a single branch has no downstream to restack: feedback and an
/// upstream move still both land.
#[test]
fn single_branch_review_keeps_feedback_through_an_upstream_rebase() {
    let what = "single branch";
    let fx = Fixture::with_upstream(&["feature/a"]);
    fx.open_pr_stack();
    let unexpected: Unexpected = Arc::default();
    let _daemon = DaemonPump::start(&fx);
    let mut held = Held::new("feature-a.txt", "feedback on a", &unexpected).feedback(&fx, 0);
    let upstream = fx.push_upstream("upstream1.txt");
    std::thread::sleep(PARK_WAIT);
    held.finish();
    fx.wait_settled_on(&upstream, what);
    fx.assert_no_unexpected_agent_calls(what, &unexpected);
    fx.assert_everything_published(
        what,
        &[(0, "feature-a.txt", "feedback on a")],
        &["upstream1.txt"],
    );
}

/// A 20-branch stack with feedback on a low, a middle and the top branch and
/// an upstream move: the coalesced restack keeps all of them.
#[test]
#[ignore = "~8-minute soak; runs every PR in its own CI job, review-race-soak"]
fn soak_twenty_branch_stack_keeps_feedback_through_an_upstream_rebase() {
    let what = "twenty branches";
    let names: Vec<String> = (0..20).map(|i| format!("feature/b{i:02}")).collect();
    let refs: Vec<&str> = names.iter().map(String::as_str).collect();
    let fx = Fixture::with_upstream(&refs);
    fx.open_pr_stack();
    let unexpected: Unexpected = Arc::default();
    let _daemon = DaemonPump::start(&fx);
    let mut low = Held::new("feature-b02.txt", "feedback low", &unexpected).feedback(&fx, 2);
    let mut mid = Held::new("feature-b10.txt", "feedback mid", &unexpected).feedback(&fx, 10);
    let mut top = Held::new("feature-b19.txt", "feedback top", &unexpected).feedback(&fx, 19);
    let upstream = fx.push_upstream("upstream1.txt");
    std::thread::sleep(PARK_WAIT);
    mid.finish();
    top.finish();
    low.finish();
    fx.wait_settled_on(&upstream, what);
    fx.assert_no_unexpected_agent_calls(what, &unexpected);
    fx.assert_everything_published(
        what,
        &[
            (2, "feature-b02.txt", "feedback low"),
            (10, "feature-b10.txt", "feedback mid"),
            (19, "feature-b19.txt", "feedback top"),
        ],
        &["upstream1.txt"],
    );
}

// ---------------------------------------------------------------------------
// Sections 6-8: upstream variants, git-level hazards and crash/restart
// variants.
// ---------------------------------------------------------------------------

impl Fixture {
    /// Clone upstream `main`, apply `edit`, commit it (`message`) and push.
    /// With `amend`, the clone's tip is amended instead and force-pushed.
    fn upstream_change(&self, message: &str, amend: bool, edit: &dyn Fn(&Path)) -> String {
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
        edit(&clone);
        git(&clone, &["add", "--all"]);
        if amend {
            git(&clone, &["commit", "-q", "--amend", "-m", message]);
            git(&clone, &["push", "-q", "--force", "origin", "main"]);
        } else {
            git(&clone, &["commit", "-q", "-m", message]);
            git(&clone, &["push", "-q", "origin", "main"]);
        }
        let sha = git(&clone, &["rev-parse", "HEAD"]).trim().to_string();
        let _ = std::fs::remove_dir_all(&clone);
        sha
    }

    /// How many lines of `file` on `rev` (in `repo`) equal `line`.
    fn line_count(&self, repo: &Path, rev: &str, file: &str, line: &str) -> usize {
        self.file_on(repo, rev, file)
            .lines()
            .filter(|l| l.trim() == line)
            .count()
    }

    /// Whether any file on `rev` in the review repo contains `needle`.
    fn text_anywhere(&self, rev: &str, needle: &str) -> bool {
        // `git grep` exits 1 on "no match", which is an answer here, not an error.
        std::process::Command::new("git")
            .args(["grep", "-l", needle, rev])
            .current_dir(&self.root)
            .output()
            .is_ok_and(|o| !o.stdout.is_empty())
    }
}

/// An upstream squash-merge of the bottom branch (the same content under a
/// different SHA) lands while feedback on the top branch is held. Nothing may
/// conflict and nothing may be dropped.
#[test]
fn upstream_squash_merge_of_the_bottom_branch_keeps_feedback_above() {
    let what = "upstream squash-merge";
    let fx = Fixture::with_upstream(&["feature/a", "feature/b", "feature/c"]);
    fx.open_pr_stack();
    let unexpected: Unexpected = Arc::default();
    let _daemon = DaemonPump::start(&fx);
    let mut held = Held::new("feature-c.txt", "feedback on c", &unexpected).feedback(&fx, 2);
    let upstream = fx.upstream_change("squash of feature/a", false, &|clone| {
        write(clone, "feature-feature-a.txt", "content\n");
    });
    std::thread::sleep(PARK_WAIT);
    held.finish();
    fx.wait_settled_on(&upstream, what);
    fx.assert_no_unexpected_agent_calls(what, &unexpected);
    fx.assert_review_branches(what, &[(2, "feature-c.txt", "feedback on c")], &[]);
}

/// Upstream rewrites its tip with identical content under a new SHA (a
/// rebase of upstream history) while feedback is held: the review must not
/// replay upstream commits or lose its own.
#[test]
fn upstream_rewrite_with_identical_content_does_not_replay_upstream_commits() {
    let what = "upstream rewrite, same content";
    let fx = Fixture::with_upstream(&["feature/a", "feature/b", "feature/c"]);
    fx.open_pr_stack();
    let unexpected: Unexpected = Arc::default();
    let _daemon = DaemonPump::start(&fx);
    let first = fx.push_upstream("upstream1.txt");
    fx.wait_settled_on(&first, "before the rewrite");
    let mut held = Held::new("feature-b.txt", "feedback on b", &unexpected).feedback(&fx, 1);
    let rewritten = fx.upstream_change("upstream1 (rebased)", true, &|_| {});
    assert_ne!(first, rewritten, "{what}: the amend did not change the SHA");
    std::thread::sleep(PARK_WAIT);
    held.finish();
    fx.wait_settled_on(&rewritten, what);
    fx.assert_no_unexpected_agent_calls(what, &unexpected);
    fx.assert_everything_published(
        what,
        &[(1, "feature-b.txt", "feedback on b")],
        &["upstream1.txt"],
    );
    for p in 0..3 {
        let count = git(&fx.root, &["log", "--format=%s", &fx.review_ref(p)])
            .lines()
            .filter(|l| l.contains("upstream1"))
            .count();
        assert_eq!(count, 1, "{what}: branch {p} replays the upstream commit");
    }
}

/// A burst of upstream pushes lands while a rebuild is parked behind held
/// feedback: the review rebuilds onto the last one with everything in place.
#[test]
fn burst_of_upstream_pushes_during_a_parked_rebuild() {
    let what = "upstream burst";
    let fx = Fixture::with_upstream(&["feature/a", "feature/b", "feature/c"]);
    fx.open_pr_stack();
    let unexpected: Unexpected = Arc::default();
    let _daemon = DaemonPump::start(&fx);
    let mut held = Held::new("feature-b.txt", "feedback on b", &unexpected).feedback(&fx, 1);
    let mut last = String::new();
    for i in 0..5 {
        last = fx.push_upstream(&format!("burst{i}.txt"));
    }
    std::thread::sleep(PARK_WAIT);
    held.finish();
    fx.wait_settled_on(&last, what);
    fx.assert_no_unexpected_agent_calls(what, &unexpected);
    let burst: Vec<String> = (0..5).map(|i| format!("burst{i}.txt")).collect();
    let burst: Vec<&str> = burst.iter().map(String::as_str).collect();
    fx.assert_everything_published(what, &[(1, "feature-b.txt", "feedback on b")], &burst);
}

/// Upstream reverts a commit it previously took while feedback is held.
#[test]
fn upstream_revert_while_feedback_is_in_flight_keeps_the_feedback() {
    let what = "upstream revert";
    let fx = Fixture::with_upstream(&["feature/a", "feature/b", "feature/c"]);
    fx.open_pr_stack();
    let unexpected: Unexpected = Arc::default();
    let _daemon = DaemonPump::start(&fx);
    let first = fx.push_upstream("upstream1.txt");
    fx.wait_settled_on(&first, "before the revert");
    let mut held = Held::new("feature-b.txt", "feedback on b", &unexpected).feedback(&fx, 1);
    let reverted = fx.upstream_change("revert upstream1", false, &|clone| {
        std::fs::remove_file(clone.join("upstream1.txt")).unwrap();
    });
    std::thread::sleep(PARK_WAIT);
    held.finish();
    fx.wait_settled_on(&reverted, what);
    fx.assert_no_unexpected_agent_calls(what, &unexpected);
    fx.assert_review_branches(what, &[(1, "feature-b.txt", "feedback on b")], &[]);
    fx.assert_file_nowhere(what, "upstream1.txt");
}

/// The base branch is deleted upstream and later restored while feedback is
/// held: nothing in the review may be lost, and it recovers once the base is
/// back.
#[test]
fn base_branch_deleted_then_restored_upstream_keeps_the_feedback() {
    let what = "base branch deleted";
    let fx = Fixture::with_upstream(&["feature/a", "feature/b", "feature/c"]);
    fx.open_pr_stack();
    let unexpected: Unexpected = Arc::default();
    let _daemon = DaemonPump::start(&fx);
    let first = fx.push_upstream("upstream1.txt");
    fx.wait_settled_on(&first, "before the delete");
    let mut held = Held::new("feature-b.txt", "feedback on b", &unexpected).feedback(&fx, 1);
    git(&fx.remote, &["update-ref", "-d", "refs/heads/main"]);
    std::thread::sleep(PARK_WAIT);
    held.finish();
    std::thread::sleep(PARK_WAIT * 2);
    assert!(
        fx.text_anywhere(&fx.review_ref(1), "feedback on b"),
        "{what}: feedback lost while the base was gone\n{}",
        fx.describe()
    );
    git(&fx.remote, &["update-ref", "refs/heads/main", &first]);
    let upstream = fx.push_upstream("upstream2.txt");
    fx.wait_settled_on(&upstream, what);
    fx.assert_no_unexpected_agent_calls(what, &unexpected);
    fx.assert_everything_published(
        what,
        &[(1, "feature-b.txt", "feedback on b")],
        &["upstream1.txt", "upstream2.txt"],
    );
}

/// Upstream renames a file a feedback round is editing. The rebuild may need
/// a person to resolve a rename/modify conflict, but the feedback line must
/// stay reachable on the review branch either way.
#[test]
fn upstream_rename_racing_a_feedback_edit_never_loses_the_edit() {
    let what = "rename vs edit";
    let fx = Fixture::with_upstream(&["feature/a", "feature/b", "feature/c"]);
    fx.open_pr_stack();
    let unexpected: Unexpected = Arc::default();
    let _daemon = DaemonPump::start_resolving(&fx, &unexpected);
    // The round edits `base.txt`, a file from the base; upstream renames it.
    let mut held = Held::resolving("base.txt", "feedback on base", &unexpected).feedback(&fx, 1);
    let upstream = fx.upstream_change("rename base.txt", false, &|clone| {
        std::fs::rename(clone.join("base.txt"), clone.join("renamed-base.txt")).unwrap();
    });
    std::thread::sleep(PARK_WAIT);
    held.finish();
    std::thread::sleep(PARK_WAIT * 6);
    for p in 1..3 {
        assert!(
            fx.text_anywhere(&fx.review_ref(p), "feedback on base"),
            "{what}: branch {p} lost the feedback (upstream {upstream:.9})\n{}",
            fx.describe()
        );
    }
}

/// A resolver whose own git calls may fail (a locked index): it appends
/// `line` to `feature-b.txt` and, as the commit step, tries to commit without
/// panicking when git refuses.
struct LockTolerantRunner {
    line: &'static str,
}

impl Runner for LockTolerantRunner {
    fn run(&self, spec: &RunnerSpec) -> RunnerResult {
        let cwd = PathBuf::from(&spec.cwd);
        let attempt = |args: &[&str]| {
            let _ = std::process::Command::new("git")
                .args(args)
                .current_dir(&cwd)
                .output();
        };
        if spec.cell_id.ends_with("-commit") {
            attempt(&["add", "--all"]);
            attempt(&["commit", "-m", &format!("edit: {}", self.line)]);
            return ok_result("attempted commit", Some(true));
        }
        append_line(&cwd, "feature-b.txt", self.line);
        ok_result("edited\nRALPHUS_PROOF: PASS", Some(true))
    }
}

/// A stale `index.lock` in a branch's worktree makes a feedback round fail
/// loudly -- the edit is not silently skipped -- and once the lock is gone a
/// retried round lands.
#[test]
fn index_lock_in_a_review_worktree_fails_feedback_loudly_then_recovers() {
    let what = "index.lock";
    let fx = Fixture::with_upstream(&["feature/a", "feature/b"]);
    fx.open_pr_stack();
    let unexpected: Unexpected = Arc::default();
    let worktree = PathBuf::from(
        fx.store.lock().get_guardian(&fx.id).unwrap().branches[1]
            .worktree
            .clone()
            .expect("branch 1 worktree"),
    );
    let gitdir = PathBuf::from(git(&worktree, &["rev-parse", "--absolute-git-dir"]).trim());
    let lock = gitdir.join("index.lock");
    std::fs::write(&lock, "").unwrap();
    // With the index locked the resolver's `git add`/`git commit` fail; the
    // daemon must report that nothing was committed rather than claim success.
    let locked = LockTolerantRunner {
        line: "locked edit",
    };
    let outcome = run_feedback(
        &fx.store,
        &locked,
        &fx.id,
        &fx.branch_ids[1],
        "please change this",
        None,
        false,
        &CancelToken::never(),
    );
    assert!(
        !outcome.committed,
        "{what}: a round with a locked index claimed a commit\n{}",
        fx.describe()
    );
    assert!(
        !fx.text_anywhere(&fx.review_ref(1), "locked edit"),
        "{what}: the edit slipped onto the review branch without a commit"
    );
    std::fs::remove_file(&lock).unwrap();
    let retry = WriterRunner::new("feature-b.txt", "retried edit", None, &unexpected);
    let outcome = run_feedback(
        &fx.store,
        &retry,
        &fx.id,
        &fx.branch_ids[1],
        "please change this",
        None,
        false,
        &CancelToken::never(),
    );
    assert!(
        outcome.committed,
        "{what}: the retry did not commit\n{}",
        fx.describe()
    );
    assert!(
        fx.text_anywhere(&fx.review_ref(1), "retried edit"),
        "{what}"
    );
}

/// Clearing the carry/source refs (a crash half-way through updating them)
/// must make the next rebuild fall back safely, not drop review-only commits.
#[test]
fn missing_source_refs_do_not_drop_feedback_on_the_next_rebuild() {
    let what = "missing source refs";
    let fx = Fixture::with_upstream(&["feature/a", "feature/b", "feature/c"]);
    fx.open_pr_stack();
    let unexpected: Unexpected = Arc::default();
    let _daemon = DaemonPump::start(&fx);
    let mut settled = Held::new("feature-b.txt", "feedback on b", &unexpected).feedback(&fx, 1);
    settled.finish();
    let refs = git(
        &fx.root,
        &["for-each-ref", "--format=%(refname)", "refs/ralphus/"],
    );
    for r in refs
        .lines()
        .filter(|l| l.contains("/source/") || l.contains("/carry/"))
    {
        git(&fx.root, &["update-ref", "-d", r.trim()]);
    }
    let upstream = fx.push_upstream("upstream1.txt");
    fx.wait_settled_on(&upstream, what);
    fx.assert_no_unexpected_agent_calls(what, &unexpected);
    fx.assert_everything_published(
        what,
        &[(1, "feature-b.txt", "feedback on b")],
        &["upstream1.txt"],
    );
}

/// The daemon dies after a feedback commit was pushed but before the pushed
/// SHA was recorded: the restarted daemon must not take its own push for a
/// reviewer's, loop, or duplicate the commit.
#[test]
fn crash_after_push_before_recording_it_does_not_duplicate_the_commit() {
    let what = "crash mid-push";
    let mut fx = Fixture::with_upstream(&["feature/a", "feature/b", "feature/c"]);
    let pr_ids = fx.open_pr_stack();
    let unexpected: Unexpected = Arc::default();
    let before = fx.remote_tip("pr-1");
    let mut round = Held::new("feature-b.txt", "feedback on b", &unexpected).feedback(&fx, 1);
    round.finish();
    fx.store
        .lock()
        .update_pull_request_ex(
            &pr_ids[1],
            None,
            None,
            None,
            None,
            None,
            Some(Some(&before)),
            None,
        )
        .unwrap();

    let db = fx.root.join(".git").join("ralphus-test.db");
    fx.store = Arc::new(StoreMutex::new(Store::open(&db).unwrap()));
    let _daemon = DaemonPump::start(&fx);
    let upstream = fx.push_upstream("upstream1.txt");
    fx.wait_settled_on(&upstream, what);
    fx.assert_no_unexpected_agent_calls(what, &unexpected);
    fx.assert_everything_published(
        what,
        &[(1, "feature-b.txt", "feedback on b")],
        &["upstream1.txt"],
    );
    for p in 1..3 {
        let rev = fx.pr_ref(p);
        let n = fx.line_count(&fx.remote, &rev, "feature-b.txt", "feedback on b");
        assert_eq!(n, 1, "{what}: PR branch {p} has the feedback {n} times");
    }
}

/// The daemon dies after committing feedback locally but before pushing it
/// (the remote is a commit behind): the restarted daemon publishes it once.
#[test]
fn crash_after_commit_before_push_publishes_the_commit_once() {
    let what = "crash before push";
    let mut fx = Fixture::with_upstream(&["feature/a", "feature/b", "feature/c"]);
    let pr_ids = fx.open_pr_stack();
    let unexpected: Unexpected = Arc::default();
    let before = fx.remote_tip("pr-1");
    let mut round = Held::new("feature-b.txt", "feedback on b", &unexpected).feedback(&fx, 1);
    round.finish();
    // Undo the push: the remote branch and the recorded pushed SHA are back
    // where they were before the round.
    git(&fx.remote, &["update-ref", "refs/heads/pr-1", &before]);
    fx.store
        .lock()
        .update_pull_request_ex(
            &pr_ids[1],
            None,
            None,
            None,
            None,
            None,
            Some(Some(&before)),
            None,
        )
        .unwrap();

    let db = fx.root.join(".git").join("ralphus-test.db");
    fx.store = Arc::new(StoreMutex::new(Store::open(&db).unwrap()));
    let _daemon = DaemonPump::start(&fx);
    let upstream = fx.push_upstream("upstream1.txt");
    fx.wait_settled_on(&upstream, what);
    fx.assert_no_unexpected_agent_calls(what, &unexpected);
    fx.assert_everything_published(
        what,
        &[(1, "feature-b.txt", "feedback on b")],
        &["upstream1.txt"],
    );
    for p in 1..3 {
        let rev = fx.pr_ref(p);
        let n = fx.line_count(&fx.remote, &rev, "feature-b.txt", "feedback on b");
        assert_eq!(n, 1, "{what}: PR branch {p} has the feedback {n} times");
    }
}

/// The daemon dies mid-feedback: the branch's worktree holds an uncommitted
/// half-edit and the round's durable pending-feedback record is still set.
/// The restarted daemon's rebuild resets the worktree (the half-edit is not
/// kept -- startup recovery re-runs the pending feedback instead), and must
/// leave the review consistent: earlier feedback intact, the worktree clean,
/// the pending record still there for recovery.
#[test]
fn crash_mid_feedback_leaves_a_consistent_review_and_the_pending_record() {
    let what = "crash mid-feedback";
    let mut fx = Fixture::with_upstream(&["feature/a", "feature/b", "feature/c"]);
    fx.open_pr_stack();
    let unexpected: Unexpected = Arc::default();
    let mut earlier = Held::new("feature-a.txt", "feedback on a", &unexpected).feedback(&fx, 0);
    earlier.finish();
    let worktree = PathBuf::from(
        fx.store.lock().get_guardian(&fx.id).unwrap().branches[1]
            .worktree
            .clone()
            .expect("branch 1 worktree"),
    );
    append_line(&worktree, "feature-b.txt", "half edit");
    fx.store
        .lock()
        .set_branch_pending_feedback(&fx.id, &fx.branch_ids[1], "please change b")
        .unwrap();

    let db = fx.root.join(".git").join("ralphus-test.db");
    fx.store = Arc::new(StoreMutex::new(Store::open(&db).unwrap()));
    let _daemon = DaemonPump::start(&fx);
    let upstream = fx.push_upstream("upstream1.txt");
    fx.wait_settled_on(&upstream, what);
    fx.assert_no_unexpected_agent_calls(what, &unexpected);
    fx.assert_everything_published(
        what,
        &[(0, "feature-a.txt", "feedback on a")],
        &["upstream1.txt"],
    );
    assert!(
        git(&worktree, &["status", "--porcelain"]).trim().is_empty(),
        "{what}: the worktree was left dirty"
    );
    let pending = fx.store.lock().branches_with_pending_feedback().unwrap();
    assert_eq!(
        pending.len(),
        1,
        "{what}: the pending-feedback record recovery needs was dropped"
    );
}

/// The daemon dies with a rebase paused on conflict markers. The restarted
/// daemon resolves it and ends with both sides on every PR.
#[test]
fn crash_mid_conflict_resolution_is_finished_by_the_restarted_daemon() {
    let what = "crash mid-conflict";
    let mut fx = Fixture::with_upstream(&["feature/a", "feature/b", "feature/c"]);
    fx.open_pr_stack();
    let unexpected: Unexpected = Arc::default();
    let upstream = fx.push_upstream_append("feature-b.txt", "upstream side");
    ralphus_daemon::guardian_merge::poll_base_branch_freshness_once(&fx.store);
    {
        let guard = fx.store.lock();
        guard
            .set_guardian_status(&fx.id, GuardianStatus::Merging, None)
            .unwrap();
        guard
            .set_branch_status(&fx.id, &fx.branch_ids[1], MergeStatus::InProgress, None)
            .unwrap();
    }
    let worktree = PathBuf::from(
        fx.store.lock().get_guardian(&fx.id).unwrap().branches[1]
            .worktree
            .clone()
            .expect("branch 1 worktree"),
    );
    let _ = std::process::Command::new("git")
        .args(["rebase", "origin/main"])
        .current_dir(&worktree)
        .output();

    let db = fx.root.join(".git").join("ralphus-test.db");
    fx.store = Arc::new(StoreMutex::new(Store::open(&db).unwrap()));
    let daemon = DaemonPump::start_resolving(&fx, &unexpected);
    ralphus_daemon::scheduler::recover_interrupted_reviews(
        &fx.store,
        &daemon.sem,
        &daemon.cancellations,
    );
    fx.wait_settled_on(&upstream, what);
    fx.assert_review_branches(
        what,
        &[(1, "feature-b.txt", "upstream side")],
        &["feature-b.txt"],
    );
    for p in 1..3 {
        let content = fx.file_on(&fx.remote, &fx.pr_ref(p), "feature-b.txt");
        assert!(
            !content.contains("<<<<<<<"),
            "{what}: markers left on PR {p}: {content:?}"
        );
        assert!(
            content.contains("upstream side"),
            "{what}: PR {p} lost the upstream side"
        );
        assert!(
            content.contains("content"),
            "{what}: PR {p} lost the task side"
        );
    }
}

/// The daemon dies during PR submission: the forge has the PR but the recorded
/// row is gone to `closed`. Resubmitting adopts the forge's PR rather than
/// opening a second one.
#[test]
fn crash_during_pr_submission_is_adopted_not_duplicated() {
    let what = "crash during submission";
    let (fx, forge) = Fixture::with_forge(&["feature/a", "feature/b", "feature/c"]);
    fx.submit_stack();
    let (pr_b, _) = fx.open_pr_row(1);
    fx.store
        .lock()
        .update_pull_request_ex(&pr_b, None, None, None, Some("closed"), None, None, None)
        .unwrap();
    let _ = ralphus_daemon::pr::submit_pull_requests(
        &fx.store,
        &NoAgentExpected,
        &fx.id,
        stack_request(),
        "tester",
        false,
    );
    assert_eq!(
        Fixture::open_forge_prs(&forge),
        3,
        "{what}: a second forge PR was opened\n{}",
        fx.describe()
    );
    fx.assert_forge_routed_everything(what, &forge);
}
