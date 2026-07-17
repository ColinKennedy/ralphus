//! Git commit/dirty-state lookup for benchmark records (RAL-94). Mirrors
//! `cli/src/ralphus/bench/gitinfo.py`. The harness never refuses to record
//! based on dirtiness — it only annotates the record so graph rendering can
//! flag it (trailing `*` on the commit label).

use std::path::Path;
use std::process::Command;

use thiserror::Error;

#[derive(Debug, Error)]
pub enum GitError {
    #[error("failed to run `git {args}`: {source}")]
    Spawn {
        args: String,
        #[source]
        source: std::io::Error,
    },
    #[error("`git {args}` exited with a non-zero status")]
    NonZero { args: String },
}

#[derive(Debug, Clone)]
pub struct GitState {
    pub commit: String,
    pub dirty: bool,
}

/// Returns the current HEAD sha and whether the working tree has
/// uncommitted changes, as seen from `cwd`.
pub fn current_git_state(cwd: &Path) -> Result<GitState, GitError> {
    let commit = run_git(cwd, &["rev-parse", "HEAD"])?;
    let status = run_git(cwd, &["status", "--porcelain"])?;
    Ok(GitState {
        commit,
        dirty: !status.is_empty(),
    })
}

fn run_git(cwd: &Path, args: &[&str]) -> Result<String, GitError> {
    let args_display = args.join(" ");
    let output = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .map_err(|source| GitError::Spawn {
            args: args_display.clone(),
            source,
        })?;
    if !output.status.success() {
        return Err(GitError::NonZero { args: args_display });
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::env;

    #[test]
    fn reads_current_repo_state() {
        // Run from the workspace root the test binary is compiled under —
        // this repo itself is guaranteed to be a git checkout in CI/dev.
        let cwd = env::current_dir().expect("cwd");
        let state = current_git_state(&cwd).expect("this repo is a git checkout");
        assert_eq!(
            state.commit.len(),
            40,
            "HEAD sha should be a full 40-char hex string"
        );
    }

    #[test]
    fn detects_dirty_working_tree() {
        let repo =
            env::temp_dir().join(format!("ralphus-bench-gitinfo-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&repo);
        std::fs::create_dir_all(&repo).expect("create temp repo dir");

        run_git(&repo, &["init", "--quiet"]).expect("git init");
        run_git(&repo, &["config", "user.email", "test@example.com"]).expect("git config email");
        run_git(&repo, &["config", "user.name", "Test"]).expect("git config name");
        std::fs::write(repo.join("a.txt"), "hello").expect("write file");
        run_git(&repo, &["add", "."]).expect("git add");
        run_git(&repo, &["commit", "--quiet", "-m", "initial"]).expect("git commit");

        let clean = current_git_state(&repo).expect("git state after commit");
        assert!(!clean.dirty);

        std::fs::write(repo.join("a.txt"), "changed").expect("modify file");
        let dirty = current_git_state(&repo).expect("git state after edit");
        assert!(dirty.dirty);
        assert_eq!(
            dirty.commit, clean.commit,
            "commit unchanged, only working tree"
        );

        std::fs::remove_dir_all(&repo).ok();
    }
}
