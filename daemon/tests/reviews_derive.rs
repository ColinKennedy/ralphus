//! Real-git integration tests for submit-time review derivation: a worktree on a
//! feature branch becomes one guardian per project, with the branch collected and
//! the run association recorded. `<<upstream>>` without an upstream is rejected.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ralphus_core::schema::TaskFile;
use ralphus_daemon::reviews::derive_reviews;
use ralphus_daemon::runner::{Runner, RunnerResult, RunnerSpec};
use ralphus_daemon::scheduler::execute_run;
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
    wt.to_string_lossy().replace('\\', "/")
}

fn session_toml(cwd: &str, review: &str) -> String {
    format!("[[task]]\nname=\"t\"\n[[task.session]]\ncwd=\"{cwd}\"\nprompt=\"p\"\n{review}\n")
}

#[test]
fn single_project_makes_one_review() {
    let base = temp_base("single");
    let cwd = repo_with_worktree(&base, "feature/a");
    let toml = session_toml(
        &cwd,
        "[[task.session.review]]\nid=\"backend\"\nbase=\"main\"",
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
         [[task.session]]\ncwd=\"{cwd_a}\"\nprompt=\"p\"\n[[task.session.review]]\nid=\"one\"\nbase=\"main\"\n\
         [[task.session]]\ncwd=\"{cwd_b}\"\nprompt=\"p\"\n[[task.session.review]]\nid=\"two\"\nbase=\"main\"\n"
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

#[test]
fn upstream_base_without_upstream_is_rejected() {
    let base = temp_base("noup");
    let cwd = repo_with_worktree(&base, "feature/a");
    let toml = session_toml(&cwd, "[[task.session.review]]\nbase=\"<<upstream>>\"");
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
        "[[task]]\nname=\"t\"\n[[task.session]]\ncwd=\"{cwd}\"\ncommand=\"noop\"\n[[task.session.review]]\nid=\"r\"\nbase=\"main\"\n"
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

    let _ = git(
        &Path::new(&base).join("repo"),
        &[
            "worktree",
            "remove",
            "--force",
            &format!(".ralphus_guardian/{gid}"),
        ],
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
