//! Live actions: carrying a live run's commits back into its review, against
//! real git -- a two-branch stacked review with a bare `origin`, merged by the
//! real merge engine, then a live run's commits integrated onto the top branch
//! through the same pass feedback uses.

mod common;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use common::{git, init_repo};
use ralphus_daemon::guardian_merge::run_merge;
use ralphus_daemon::live_actions::{LiveRunEnd, LiveRunOutcome, live_ref_name};
use ralphus_daemon::live_runs::{LiveRunKind, integrate_run};
use ralphus_daemon::runner::{Runner, RunnerResult, RunnerSpec};
use ralphus_daemon::store::Store;
use ralphus_daemon::store_lock::{StoreHandle, StoreMutex};

fn write(root: &Path, name: &str, content: &str) {
    std::fs::write(root.join(name), content).expect("write file");
}

fn temp_dir(tag: &str) -> PathBuf {
    static N: AtomicU32 = AtomicU32::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    let dir =
        std::env::temp_dir().join(format!("ralphus-live-int-{tag}-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("mkdir");
    dir
}

fn done(summary: &str) -> RunnerResult {
    let mut result = RunnerResult::failure("");
    result.status = "done".into();
    result.error = None;
    result.summary = summary.into();
    result
}

/// Passes every proof gate and makes no edits: a cherry-pick needs no agent.
struct PassingProofs;
impl Runner for PassingProofs {
    fn run(&self, spec: &RunnerSpec) -> RunnerResult {
        if spec.proof {
            return done("RALPHUS_PROOF: PASS");
        }
        RunnerResult::failure("no agent edit expected")
    }
}

/// The resolver applying the live fix by hand after a conflict: writes the
/// fixed file and commits it, the way a real resolver would.
struct ResolverApplies;
impl Runner for ResolverApplies {
    fn run(&self, spec: &RunnerSpec) -> RunnerResult {
        if spec.proof {
            return done("RALPHUS_PROOF: PASS");
        }
        let cwd = PathBuf::from(&spec.cwd);
        if spec.cell_id.ends_with("-commit") {
            return done("nothing to commit");
        }
        assert!(
            spec.prompt
                .as_deref()
                .unwrap_or_default()
                .contains("The change, as a patch"),
            "the resolver is handed the live run's patch"
        );
        write(
            &cwd,
            "b.txt",
            "from b, fixed live\nand kept the newer line\n",
        );
        git(&cwd, &["add", "b.txt"]);
        git(&cwd, &["commit", "-m", "apply the live fix by hand"]);
        done("applied")
    }
}

struct Review {
    store: StoreHandle,
    id: String,
    root: PathBuf,
    remote: PathBuf,
}

impl Drop for Review {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
        let _ = std::fs::remove_dir_all(&self.remote);
    }
}

/// A merged two-branch review (`feature/a` then `feature/b`).
fn merged_review(tag: &str) -> Review {
    let root = temp_dir(tag);
    init_repo(&root);
    let remote = temp_dir(&format!("{tag}-remote"));
    git(&remote, &["init", "--bare"]);
    git(
        &root,
        &["remote", "add", "origin", remote.to_str().unwrap()],
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

    let store: StoreHandle = Arc::new(StoreMutex::new(Store::open_in_memory().unwrap()));
    let id = {
        let g = store.lock();
        let id = g
            .create_guardian("r", "main", root.to_str().unwrap())
            .unwrap();
        g.add_guardian_branch(&id, "feature/a").unwrap();
        g.add_guardian_branch(&id, "feature/b").unwrap();
        id
    };
    run_merge(&store, &PassingProofs, &id);
    let view = store.lock().get_guardian(&id).unwrap();
    assert_eq!(view.status, "in_review", "detail: {:?}", view.detail);
    Review {
        store,
        id,
        root,
        remote,
    }
}

impl Review {
    fn top_worktree(&self) -> PathBuf {
        let view = self.store.lock().get_guardian(&self.id).unwrap();
        let top = view.branches.iter().max_by_key(|b| b.position).unwrap();
        PathBuf::from(top.worktree.clone().unwrap())
    }

    fn top_review_branch(&self) -> String {
        let view = self.store.lock().get_guardian(&self.id).unwrap();
        let top = view.branches.iter().max_by_key(|b| b.position).unwrap();
        top.review_branch.clone().unwrap()
    }

    /// What a live run leaves behind: commits made in a separate checkout of
    /// the review's tip (T0), pinned on its live ref, and a finished run row.
    fn live_run_with_commit(&self, run_tag: &str, file: &str, content: &str) -> String {
        let base = git(&self.top_worktree(), &["rev-parse", "HEAD"])
            .trim()
            .to_string();
        let checkout = temp_dir(&format!("checkout-{run_tag}"));
        let _ = std::fs::remove_dir_all(&checkout);
        git(
            &self.root,
            &[
                "worktree",
                "add",
                "--detach",
                checkout.to_str().unwrap(),
                &base,
            ],
        );
        write(&checkout, file, content);
        git(&checkout, &["add", file]);
        git(&checkout, &["commit", "-m", "Fix found while testing live"]);
        let head = git(&checkout, &["rev-parse", "HEAD"]).trim().to_string();

        let run_id = self
            .store
            .lock()
            .queue_live_run(&self.id, LiveRunKind::Live, 0, "Smoke", "auto")
            .unwrap();
        let live_ref = live_ref_name(&self.id, &run_id);
        git(&checkout, &["update-ref", &live_ref, "HEAD"]);
        let outcome = LiveRunOutcome {
            end: LiveRunEnd::Finished,
            report: "b.txt was missing its fix".to_string(),
            error: None,
            cost_usd: 0.0,
            base_tip: Some(base),
            commits: vec![head],
            live_ref: Some(live_ref),
            cell_id: "live-0-x".to_string(),
        };
        {
            let guard = self.store.lock();
            guard
                .start_live_run(&run_id, checkout.to_str().unwrap(), "live-0-x")
                .unwrap();
            guard.finish_live_run(&run_id, &outcome, "pending").unwrap();
        }
        git(
            &self.root,
            &["worktree", "remove", "--force", checkout.to_str().unwrap()],
        );
        run_id
    }

    fn ref_exists(&self, name: &str) -> bool {
        std::process::Command::new("git")
            .args(["rev-parse", "--verify", "--quiet", name])
            .current_dir(&self.root)
            .status()
            .unwrap()
            .success()
    }
}

#[test]
fn a_clean_live_fix_is_cherry_picked_onto_the_top_branch_and_pushed() {
    let r = merged_review("clean");
    let run_id = r.live_run_with_commit("clean", "live-fix.txt", "fixed live\n");
    let live_ref = r.store.lock().live_run(&run_id).unwrap().live_ref.unwrap();

    integrate_run(&r.store, &PassingProofs, &run_id);

    let run = r.store.lock().live_run(&run_id).unwrap();
    assert_eq!(
        run.integration, "integrated",
        "{:?}",
        run.integration_detail
    );
    let review_branch = r.top_review_branch();
    let files = git(&r.root, &["ls-tree", "-r", "--name-only", &review_branch]);
    assert!(
        files.contains("live-fix.txt"),
        "the fix is on the top review branch: {files}"
    );
    let log = git(&r.root, &["log", "-1", "--format=%s", &review_branch]);
    assert_eq!(
        log.trim(),
        "Fix found while testing live",
        "the agent's own message is kept"
    );
    assert!(!r.ref_exists(&live_ref), "a confirmed live ref is deleted");

    // Pushed to the PR remote, and the combined worktree rebuilt on top.
    let pushed = git(&r.remote, &["ls-tree", "-r", "--name-only", &review_branch]);
    assert!(pushed.contains("live-fix.txt"), "pushed: {pushed}");
    let view = r.store.lock().get_guardian(&r.id).unwrap();
    let combined = PathBuf::from(view.combined_worktree.unwrap());
    assert!(combined.join("live-fix.txt").exists());
    assert!(combined.join("a.txt").exists() && combined.join("b.txt").exists());
}

#[test]
fn a_live_fix_already_on_the_branch_counts_as_integrated() {
    let r = merged_review("already");
    let run_id = r.live_run_with_commit("already", "live-fix.txt", "fixed live\n");
    // Feedback made the same change first.
    let wt = r.top_worktree();
    write(&wt, "live-fix.txt", "fixed live\n");
    git(&wt, &["add", "live-fix.txt"]);
    git(&wt, &["commit", "-m", "same fix via feedback"]);

    integrate_run(&r.store, &PassingProofs, &run_id);

    let run = r.store.lock().live_run(&run_id).unwrap();
    assert_eq!(run.integration, "integrated");
    assert!(
        run.integration_detail.unwrap().contains("already"),
        "recognized as already landed"
    );
}

#[test]
fn a_conflicting_live_fix_that_cannot_be_applied_is_kept_on_its_live_ref() {
    let r = merged_review("conflict-kept");
    let run_id = r.live_run_with_commit("conflict-kept", "b.txt", "from b, fixed live\n");
    // The top branch moved on b.txt since the live run started.
    let wt = r.top_worktree();
    write(&wt, "b.txt", "from b\nand a newer line\n");
    git(&wt, &["add", "b.txt"]);
    git(&wt, &["commit", "-m", "newer b"]);
    let before = git(&wt, &["rev-parse", "HEAD"]);

    struct NoChange;
    impl Runner for NoChange {
        fn run(&self, spec: &RunnerSpec) -> RunnerResult {
            if spec.proof {
                return done("RALPHUS_PROOF: PASS");
            }
            done("could not apply it")
        }
    }
    integrate_run(&r.store, &NoChange, &run_id);

    let run = r.store.lock().live_run(&run_id).unwrap();
    assert_eq!(run.integration, "failed", "{:?}", run.integration_detail);
    assert!(
        r.ref_exists(run.live_ref.as_deref().unwrap()),
        "nothing is dropped"
    );
    assert_eq!(
        git(&wt, &["rev-parse", "HEAD"]),
        before,
        "an aborted pick leaves the branch exactly as it was"
    );
    assert!(
        git(&wt, &["status", "--porcelain"]).trim().is_empty(),
        "no half-applied pick left behind"
    );
}

#[test]
fn a_conflicting_live_fix_is_applied_by_the_resolver() {
    let r = merged_review("conflict-resolved");
    let run_id = r.live_run_with_commit("conflict-resolved", "b.txt", "from b, fixed live\n");
    let wt = r.top_worktree();
    write(&wt, "b.txt", "from b\nand a newer line\n");
    git(&wt, &["add", "b.txt"]);
    git(&wt, &["commit", "-m", "newer b"]);

    integrate_run(&r.store, &ResolverApplies, &run_id);

    let run = r.store.lock().live_run(&run_id).unwrap();
    assert_eq!(
        run.integration, "integrated",
        "{:?}",
        run.integration_detail
    );
    assert!(run.integration_detail.unwrap().contains("resolver"));
    let review_branch = r.top_review_branch();
    let b = git(&r.root, &["show", &format!("{review_branch}:b.txt")]);
    assert!(b.contains("fixed live"), "{b}");
}
