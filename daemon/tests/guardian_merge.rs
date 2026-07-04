//! Real-git integration tests for the Guardian merge engine: a clean two-branch
//! stack, and a conflicting branch resolved by a (fake) agent that strips
//! conflict markers.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use ralphus_daemon::guardian_merge::run_merge;
use ralphus_daemon::runner::{Runner, RunnerResult, RunnerSpec};
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

    let _ = git(
        &root,
        &[
            "worktree",
            "remove",
            "--force",
            &format!(".ralphus_guardian/{id}"),
        ],
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
    let _ = git(
        &root,
        &[
            "worktree",
            "remove",
            "--force",
            &format!(".ralphus_guardian/{id}"),
        ],
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
    let _ = git(
        &root,
        &[
            "worktree",
            "remove",
            "--force",
            &format!(".ralphus_guardian/{id}"),
        ],
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

    let _ = git(
        &root,
        &[
            "worktree",
            "remove",
            "--force",
            &format!(".ralphus_guardian/{id}"),
        ],
    );
    let _ = std::fs::remove_dir_all(&root);
}
