//! Projects with no clone on the daemon's host ("remote only"): registered
//! with a clone URL alone, for work that runs entirely on remote machines.
//!
//! Reviews, worktree lookups and per-project settings are keyed by a
//! project's root path, so a remote-only project still gets one: a reserved
//! path under the daemon's state directory, [`root_for`], that is never
//! created. It is only ever used as a key -- nothing reads or runs git in it
//! -- and project config discovery never walks up from it into unrelated
//! `.ralphus.toml` files.
//!
//! The project's own `.ralphus.toml` is read from a machine's clone instead
//! ([`refresh_config`]): whenever ralphus works with the project on a machine
//! (a cell provisioned there, a review's workspace), it reads the file at the
//! clone's default branch (`origin/HEAD`) and keeps a copy per project and
//! commit under the state directory, which config discovery then returns for
//! the project's root ([`fetched_config`]). The copy goes through exactly the
//! same loaders, and so the same rules about what a project file may set, as
//! a `.ralphus.toml` in a local checkout.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex, PoisonError};
use std::time::{Duration, Instant};

/// Directory, under the daemon's state directory, that remote-only project
/// roots are named under.
pub const REMOTE_ONLY_DIR: &str = "remote-only-projects";

/// Directory, under the daemon's state directory, holding the `.ralphus.toml`
/// copies read from machines: `<project>/<commit>/.ralphus.toml`.
pub const FETCHED_CONFIG_DIR: &str = "remote-project-configs";

/// How long a project's default-branch commit is trusted before a machine is
/// asked again. Each ask is a remote `git` command, and the board and merge
/// paths build workspaces far more often than a default branch moves.
const REFRESH_INTERVAL: Duration = Duration::from_secs(60);

/// The reserved root that keys remote-only project `name`.
#[must_use]
pub fn root_for(name: &str) -> PathBuf {
    crate::state_dir().join(REMOTE_ONLY_DIR).join(name)
}

/// Whether `path` is (or is under) a remote-only project's reserved root.
#[must_use]
pub fn is_remote_only_root(path: &Path) -> bool {
    path.starts_with(crate::state_dir().join(REMOTE_ONLY_DIR))
}

/// The reserved root of the remote-only project `path` is (or is under).
fn project_root_of(path: &Path) -> Option<PathBuf> {
    let base = crate::state_dir().join(REMOTE_ONLY_DIR);
    let name = path.strip_prefix(&base).ok()?.components().next()?;
    Some(base.join(name))
}

/// What was last read from a machine for one remote-only project.
struct Fetched {
    /// The default-branch commit the copy was read at.
    commit: String,
    /// The local copy, `None` when that commit has no `.ralphus.toml`.
    file: Option<PathBuf>,
    /// When the machine was last asked for the commit.
    checked_at: Instant,
}

/// Every remote-only project's latest [`Fetched`], by reserved root.
static FETCHED: LazyLock<Mutex<HashMap<PathBuf, Fetched>>> = LazyLock::new(Default::default);

/// The local copy of the `.ralphus.toml` read from a machine for the
/// remote-only project `path` is (or is under), if one has been read.
#[must_use]
pub fn fetched_config(path: &Path) -> Option<PathBuf> {
    let root = project_root_of(path)?;
    FETCHED
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .get(&root)
        .and_then(|f| f.file.clone())
        .filter(|file| file.is_file())
}

/// Read remote-only project `root`'s `.ralphus.toml` from `machine_repo` (any
/// checkout of the project's clone on a machine) at its default branch, unless
/// that was done less than a minute ago. A no-op for a project with a local
/// checkout. Failures are logged and leave the previous copy in place.
pub fn refresh_config(root: &Path, machine_repo: &crate::workspace::Workspace) {
    refresh_config_in(
        root,
        machine_repo,
        &crate::state_dir().join(FETCHED_CONFIG_DIR),
        REFRESH_INTERVAL,
    );
}

/// [`refresh_config`] keeping copies under `cache_dir`, asking again only
/// after `interval`.
fn refresh_config_in(
    root: &Path,
    machine_repo: &crate::workspace::Workspace,
    cache_dir: &Path,
    interval: Duration,
) {
    if machine_repo.is_local() {
        return;
    }
    let Some(root) = project_root_of(root) else {
        return;
    };
    let previous = {
        let mut fetched = FETCHED.lock().unwrap_or_else(PoisonError::into_inner);
        match fetched.get_mut(&root) {
            Some(f) if f.checked_at.elapsed() < interval => return,
            Some(f) => {
                // Claimed now, so concurrent callers do not all ask.
                f.checked_at = Instant::now();
                Some(f.commit.clone())
            }
            None => None,
        }
    };
    let commit = match machine_repo.git(&[
        "rev-parse",
        "--verify",
        "--quiet",
        "refs/remotes/origin/HEAD^{commit}",
    ]) {
        Ok(out) if !out.trim().is_empty() => out.trim().to_string(),
        result => {
            let error = result.err().unwrap_or_default();
            note(
                machine_repo,
                crate::logging::LogLevel::WARNING,
                &format!(
                    "could not find the default branch of {} on machine {}: {error}",
                    root.display(),
                    machine_repo.machine().unwrap_or_default(),
                ),
                serde_json::json!({"project_root": root, "error": error}),
            );
            return;
        }
    };
    if previous.as_deref() == Some(commit.as_str()) {
        return;
    }
    let name = root.file_name().map_or_else(
        || "project".to_string(),
        |n| n.to_string_lossy().into_owned(),
    );
    let file = cache_dir.join(&name).join(&commit).join(".ralphus.toml");
    let file = if file.is_file() {
        Some(file)
    } else {
        match machine_repo.git(&["show", &format!("{commit}:.ralphus.toml")]) {
            Ok(text) => match write_copy(&file, &text) {
                Ok(()) => Some(file),
                Err(e) => {
                    note(
                        machine_repo,
                        crate::logging::LogLevel::WARNING,
                        &format!("could not keep {name}'s .ralphus.toml: {e}"),
                        serde_json::json!({"project": name, "commit": commit, "error": e}),
                    );
                    return;
                }
            },
            // No `.ralphus.toml` at that commit: the project sets nothing.
            Err(_) => None,
        }
    };
    note(
        machine_repo,
        crate::logging::LogLevel::INFO,
        &format!(
            "{name} .ralphus.toml at {commit} read from machine {}: {}",
            machine_repo.machine().unwrap_or_default(),
            if file.is_some() { "found" } else { "none" }
        ),
        serde_json::json!({"project": name, "commit": commit, "found": file.is_some()}),
    );
    FETCHED
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .insert(
            root,
            Fetched {
                commit,
                file,
                checked_at: Instant::now(),
            },
        );
}

/// Log `message` to stderr and, when `ws` can reach the store, Cartographer.
fn note(
    ws: &crate::workspace::Workspace,
    level: crate::logging::LogLevel,
    message: &str,
    payload: serde_json::Value,
) {
    match ws.store() {
        Some(store) => crate::cartographer::Note::new("remote_projects")
            .level(level)
            .scope("project")
            .emit(&store.lock(), message, payload),
        None => {
            // A workspace built without a store handle cannot reach Cartographer.
            crate::logging::write_line(level, &format!("ralphus [remote_projects] {message}"));
        }
    }
}

fn write_copy(file: &Path, text: &str) -> Result<(), String> {
    let parent = file
        .parent()
        .ok_or_else(|| format!("{} has no parent", file.display()))?;
    std::fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
    // Written whole under a temporary name, so a reader never sees half a file.
    let partial = parent.join(".ralphus.toml.partial");
    std::fs::write(&partial, text).map_err(|e| format!("{}: {e}", partial.display()))?;
    std::fs::rename(&partial, file).map_err(|e| format!("{}: {e}", file.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_remote_only_root_is_recognized_and_others_are_not() {
        let root = root_for("widgets");
        assert!(is_remote_only_root(&root));
        assert!(is_remote_only_root(&root.join("sub")));
        assert!(!is_remote_only_root(Path::new("/home/me/widgets")));
        assert_ne!(
            root_for("a"),
            root_for("b"),
            "each project gets its own key"
        );
        assert_eq!(project_root_of(&root.join("sub/dir")), Some(root));
    }

    fn git(dir: &Path, args: &[&str]) -> String {
        crate::guardian_merge::git(dir, args).unwrap()
    }

    /// A machine provider whose `run` verb runs git in `clone` for the machine
    /// path `/remote/repo`, logging each command to `calls.txt` beside it.
    fn proxy_provider(dir: &Path, clone: &Path) -> PathBuf {
        let py = dir.join("provider.py");
        std::fs::write(
            &py,
            format!(
                r#"import json, os, subprocess, sys
verb = sys.argv[1]
req = json.load(sys.stdin)
with open(os.path.join(os.path.dirname(__file__), "calls.txt"), "a") as f:
    f.write(" ".join(req.get("args", [])) + "\n")
assert req["cwd"].replace("\\", "/") == "/remote/repo", req["cwd"]
out = subprocess.run([req["program"]] + req["args"], cwd={clone:?}, capture_output=True, text=True)
print(json.dumps({{"ok": True, "protocol_version": 1, "exit_code": out.returncode, "stdout": out.stdout}}))
"#,
                clone = clone.to_string_lossy(),
            ),
        )
        .unwrap();
        py
    }

    #[test]
    fn a_remote_only_projects_config_is_read_from_its_machine_per_commit() {
        let dir = ralphus_test_support::scratch(format!("remote-config-{}", std::process::id()));
        let origin = dir.join("origin");
        std::fs::create_dir_all(&origin).unwrap();
        git(&origin, &["init", "--initial-branch", "main"]);
        git(&origin, &["config", "user.email", "t@example.com"]);
        git(&origin, &["config", "user.name", "t"]);
        git(&origin, &["config", "commit.gpgsign", "false"]);
        std::fs::write(origin.join(".ralphus.toml"), "[review]\nauto_run = true\n").unwrap();
        git(&origin, &["add", "."]);
        git(&origin, &["commit", "-m", "config"]);
        let clone = dir.join("clone");
        git(
            &dir,
            &[
                "clone",
                "--origin",
                "origin",
                &origin.to_string_lossy(),
                &clone.to_string_lossy(),
            ],
        );

        let py = proxy_provider(&dir, &clone);
        let store = std::sync::Arc::new(crate::store_lock::StoreMutex::new(
            crate::store::Store::open_in_memory().unwrap(),
        ));
        store
            .lock()
            .register_machine_provider(
                "cfgtest",
                "",
                "python",
                &[py.to_string_lossy().into_owned()],
                crate::machines::PROTOCOL_VERSION,
                false,
            )
            .unwrap();
        let ws = crate::workspace::Workspace::on("/remote/repo", Some("cfgtest:A"))
            .with_store(std::sync::Arc::clone(&store));
        let root = root_for(&format!("remote-config-{}", std::process::id()));
        let cache = dir.join("cache");

        assert!(crate::config::find_project_config(&root).is_none());
        refresh_config_in(&root, &ws, &cache, Duration::ZERO);
        let found = crate::config::find_project_config(&root).expect("read from the machine");
        assert!(found.starts_with(&cache), "{}", found.display());
        assert!(crate::config::project_review_config(&root).auto_run());

        // Same commit: asked for the commit again, never for the file.
        refresh_config_in(&root, &ws, &cache, Duration::ZERO);
        let calls = std::fs::read_to_string(dir.join("calls.txt")).unwrap();
        assert_eq!(calls.lines().filter(|l| l.starts_with("show")).count(), 1);

        // A new default-branch commit on the machine is read again.
        std::fs::write(origin.join(".ralphus.toml"), "[review]\nauto_run = false\n").unwrap();
        git(&origin, &["commit", "-am", "flip"]);
        git(&clone, &["fetch", "origin"]);
        refresh_config_in(&root, &ws, &cache, Duration::ZERO);
        assert!(!crate::config::project_review_config(&root).auto_run());

        // Within the interval the machine is not asked at all.
        let before = std::fs::read_to_string(dir.join("calls.txt")).unwrap();
        refresh_config_in(&root, &ws, &cache, Duration::from_secs(3600));
        assert_eq!(
            std::fs::read_to_string(dir.join("calls.txt")).unwrap(),
            before
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
