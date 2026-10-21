//! RAL-690 hygiene: the guard deletes git trees and stops child processes, and
//! the sweep removes a dead run while sparing a live one.

use ralphus_test_support::{TestDir, scratch, scratch_root, sweep_stale_runs};
use std::path::Path;
use std::process::{Command, Stdio};

fn git(dir: &Path, args: &[&str]) {
    let status = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@example.com")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@example.com")
        .status()
        .expect("run git");
    assert!(status.success(), "git {args:?} failed");
}

#[test]
fn drop_removes_a_git_repo_with_read_only_pack_files() {
    let dir = TestDir::new("hygiene-git");
    let path = dir.path().to_path_buf();
    git(&path, &["init", "-q"]);
    std::fs::write(path.join("f.txt"), "x").unwrap();
    git(&path, &["add", "."]);
    git(&path, &["commit", "-q", "-m", "c"]);
    git(&path, &["gc", "-q"]);
    drop(dir);
    assert!(!path.exists());
}

#[test]
fn drop_kills_a_registered_child_holding_the_directory() {
    let mut dir = TestDir::new("hygiene-child");
    let path = dir.path().to_path_buf();
    let mut cmd = if cfg!(windows) {
        let mut c = Command::new("cmd");
        c.args(["/c", "pause"]);
        c
    } else {
        let mut c = Command::new("sleep");
        c.arg("30");
        c
    };
    let child = cmd
        .current_dir(&path)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .spawn()
        .expect("spawn child");
    dir.register_child(child);
    drop(dir);
    assert!(!path.exists());
}

#[test]
fn sweep_removes_a_dead_run_and_spares_a_live_one() {
    let parent = TestDir::new("hygiene-sweep");
    let dead = parent.path().join("4000000000-0");
    let live = parent
        .path()
        .join(format!("{}-{}", std::process::id(), u64::MAX / 2));
    std::fs::create_dir_all(dead.join("x")).unwrap();
    std::fs::write(dead.join("x").join("f"), "1").unwrap();
    std::fs::create_dir_all(&live).unwrap();
    assert_eq!(sweep_stale_runs(parent.path(), None), 1);
    assert!(!dead.exists());
    assert!(live.exists());
}

#[test]
fn scratch_paths_live_under_the_run_root_and_vanish_with_the_test_thread() {
    let root = scratch_root();
    let path = std::thread::spawn(|| {
        let p = scratch("hygiene-scratch-leaf");
        std::fs::create_dir_all(p.join("inner")).unwrap();
        p
    })
    .join()
    .unwrap();
    assert!(path.starts_with(&root));
    assert!(!path.exists());
}
