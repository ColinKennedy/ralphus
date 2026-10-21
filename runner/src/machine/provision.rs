//! The `provision` verb: one persistent clone per project under the root,
//! fetched (never re-cloned) on later calls, and a linked `git worktree` per
//! task branch -- the same layout and steps as the SSH provider's provision
//! script (`ssh-provider/src/provision.rs`), run as git commands directly.

use std::path::Path;
use std::process::Command;
use std::time::Duration;

use serde_json::{Value, json};

use super::Events;
use super::paths::{self, Root};

/// How long a provision waits for another provision of the same project.
const LOCK_TIMEOUT: Duration = Duration::from_secs(30 * 60);

/// Run `provision` and return the worktree path.
///
/// # Errors
/// A malformed request, an unsupported project kind, an origin mismatch, or
/// a failed git command (its output with any credential in the clone URL
/// redacted).
pub fn run(request: &Value, root: &Root, events: &mut Events) -> Result<String, String> {
    let project = super::required_str(request, "project", "provision request")?;
    let source = request
        .get("source")
        .ok_or_else(|| "provision request is missing \"source\"".to_string())?;
    let kind = source
        .get("kind")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if kind != "git" {
        return Err(format!(
            "provision only supports git projects; got kind {kind:?}"
        ));
    }
    let url = super::required_str(source, "url", "provision request's source")?;
    let branch = super::required_str(source, "branch", "provision request's source")?;
    let upstream = super::required_str(source, "upstream", "provision request's source")?;
    if let Some(asked) = request
        .get("remote_root")
        .and_then(Value::as_str)
        .filter(|r| !r.trim().is_empty())
    {
        if paths::normalize(asked) != root.as_str() {
            return Err(format!(
                "provision asked for remote_root {asked:?}, but this machine's is {:?}",
                root.as_str()
            ));
        }
    }
    let env = super::env_map(request.get("env"), "provision request \"env\"")?;
    let git = Git { env: &env, url };

    let project_dir = ralphus_core::machine_layout::project_dir(root.as_str(), project, url);
    let repo = ralphus_core::machine_layout::repository_dir(&project_dir);
    let worktree = ralphus_core::machine_layout::worktree_dir(&project_dir, branch);
    let staging = format!("{repo}.tmp-clone");
    root.contain(&worktree)?;
    std::fs::create_dir_all(ralphus_core::machine_layout::worktrees_parent_dir(
        &project_dir,
    ))
    .map_err(|e| format!("could not create {project_dir}: {e}"))?;
    let _lock = super::lock::lock(
        Path::new(&ralphus_core::machine_layout::lock_path(&project_dir)),
        LOCK_TIMEOUT,
    )?;

    if Path::new(&repo).join(".git").is_dir() {
        let recorded = git
            .output(&["-C", &repo, "config", "--get", "remote.origin.url"])
            .map_err(|_| format!("existing repository at {repo} has no 'origin' remote"))?;
        if recorded.trim() != url {
            return Err(format!(
                "origin mismatch for {project:?}: expected {} but {repo} has origin {} -- a \
                 directory hash collision or manual tampering, refusing to adopt it",
                redact(url),
                redact(recorded.trim())
            ));
        }
        git.output(&["-C", &repo, "fetch", "origin"])?;
    } else {
        super::files::remove_tree(Path::new(&staging))?;
        git.output(&["clone", "--origin", "origin", url, &staging])?;
        std::fs::rename(&staging, &repo)
            .map_err(|e| format!("could not move the new clone into {repo}: {e}"))?;
        let metadata = json!({"project": project, "url": redact(url)});
        super::files::write_atomically(
            Path::new(&ralphus_core::machine_layout::metadata_path(&project_dir)),
            format!("{metadata}\n").as_bytes(),
            false,
        )?;
        events.emit(
            "cloned project",
            "info",
            json!({"project": project, "repository": repo}),
        );
    }
    if !Path::new(&worktree).is_dir() {
        let local_branch = format!("refs/heads/{branch}");
        let exists = git
            .output(&[
                "-C",
                &repo,
                "rev-parse",
                "--verify",
                "--quiet",
                &local_branch,
            ])
            .is_ok();
        if exists {
            git.output(&["-C", &repo, "worktree", "add", &worktree, branch])?;
        } else {
            let origin_ref = format!("origin/{}", upstream_branch_name(upstream));
            git.output(&[
                "-C",
                &repo,
                "worktree",
                "add",
                "--track",
                "-b",
                branch,
                &worktree,
                &origin_ref,
            ])?;
        }
    }
    Ok(worktree)
}

/// The branch half of a `<remote>/<branch>` upstream: the clone's remote is
/// always `origin`, whatever the daemon's own checkout calls its remote.
fn upstream_branch_name(upstream: &str) -> &str {
    match upstream.split_once('/') {
        Some((_, branch)) if !branch.is_empty() => branch,
        _ => upstream,
    }
}

fn redact(url: &str) -> String {
    ralphus_core::redact::redact_url_credentials(url)
}

/// Git commands for one provision: the request's environment (a forge
/// credential) on every command, stdin closed.
struct Git<'a> {
    env: &'a std::collections::BTreeMap<String, String>,
    url: &'a str,
}

impl Git<'_> {
    /// Run git with `args`; its stdout on success, its output (with the
    /// clone URL's credentials redacted) on failure.
    fn output(&self, args: &[&str]) -> Result<String, String> {
        let mut command = Command::new("git");
        command
            .args(args)
            .envs(self.env)
            .env("GIT_TERMINAL_PROMPT", "0");
        let output = command
            .stdin(std::process::Stdio::null())
            .output()
            .map_err(|e| format!("could not run git: {e}"))?;
        if output.status.success() {
            return Ok(String::from_utf8_lossy(&output.stdout).into_owned());
        }
        let mut said = String::from_utf8_lossy(&output.stderr).into_owned();
        if said.contains(self.url) {
            said = said.replace(self.url, &redact(self.url));
        }
        let said = ralphus_core::redact::redact_secrets(said.trim()).into_owned();
        let shown: Vec<String> = args
            .iter()
            .map(|a| {
                if *a == self.url {
                    redact(a)
                } else {
                    (*a).to_string()
                }
            })
            .collect();
        Err(format!(
            "git {} failed (exit {}): {said}",
            shown.join(" "),
            output.status.code().unwrap_or(-1)
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn git(dir: &Path, args: &[&str]) {
        let status = Command::new("git")
            .args(args)
            .current_dir(dir)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@example.invalid")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@example.invalid")
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?}");
    }

    /// An origin repository with one commit on `main`, as a `file://` URL.
    fn origin(dir: &Path) -> String {
        let origin = dir.join("origin");
        std::fs::create_dir_all(&origin).unwrap();
        git(&origin, &["init", "-q", "-b", "main"]);
        std::fs::write(origin.join("README"), "hi\n").unwrap();
        git(&origin, &["add", "README"]);
        git(&origin, &["commit", "-q", "-m", "first"]);
        format!(
            "file:///{}",
            origin
                .to_string_lossy()
                .replace('\\', "/")
                .trim_start_matches('/')
        )
    }

    fn request(url: &str, branch: &str) -> Value {
        json!({
            "project": "demo",
            "source": {"kind": "git", "url": url, "branch": branch, "upstream": "origin/main"},
        })
    }

    #[test]
    fn provision_clones_once_and_adds_a_worktree_per_branch() {
        let dir = ralphus_test_support::TestDir::new("machine-provision");
        let url = origin(&dir);
        let root_dir = dir.join("root");
        std::fs::create_dir_all(&root_dir).unwrap();
        let root = Root::new(&root_dir.to_string_lossy()).unwrap();
        let mut events = Events::default();

        let first = run(&request(&url, "feature/a"), &root, &mut events).unwrap();
        assert!(first.starts_with(root.as_str()), "{first}");
        assert!(Path::new(&first).join("README").is_file());
        assert!(events.0.iter().any(|e| e.contains("cloned project")));

        let again = run(&request(&url, "feature/a"), &root, &mut Events::default()).unwrap();
        assert_eq!(again, first, "the same branch gets the same worktree");
        let second = run(&request(&url, "feature/b"), &root, &mut Events::default()).unwrap();
        assert_ne!(second, first);
        assert!(Path::new(&second).join("README").is_file());
    }

    #[test]
    fn a_non_git_project_and_a_foreign_remote_root_are_refused() {
        let dir = ralphus_test_support::TestDir::new("machine-provision-refused");
        let root = Root::new(&dir.to_string_lossy()).unwrap();
        let mut bad = request("file:///x", "b");
        bad["source"]["kind"] = json!("svn");
        assert!(
            run(&bad, &root, &mut Events::default())
                .unwrap_err()
                .contains("git")
        );
        let mut foreign = request("file:///x", "b");
        foreign["remote_root"] = json!(if cfg!(windows) {
            "C:/elsewhere"
        } else {
            "/elsewhere"
        });
        let err = run(&foreign, &root, &mut Events::default()).unwrap_err();
        assert!(err.contains("remote_root"), "{err}");
    }

    #[test]
    fn a_failed_clone_never_shows_the_credential_in_its_url() {
        let dir = ralphus_test_support::TestDir::new("machine-provision-redact");
        let root = Root::new(&dir.to_string_lossy()).unwrap();
        let url = "https://user:hunter2@127.0.0.1:9/none.git";
        let err = run(&request(url, "b"), &root, &mut Events::default()).unwrap_err();
        assert!(!err.contains("hunter2"), "{err}");
    }

    #[test]
    fn upstream_branch_name_strips_a_remote_alias() {
        assert_eq!(upstream_branch_name("origin/main"), "main");
        assert_eq!(upstream_branch_name("main"), "main");
    }
}
