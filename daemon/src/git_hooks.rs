//! RAL-445: the `prepare-commit-msg` hook that force-adds
//! `Co-authored-by: ralphus-bot <ralphus-bot@users.noreply.github.com>` to
//! commits made in a
//! Ralphus-managed project, plus the per-project switch
//! (`[commits] add_coauthor` in `.ralphus.toml`, see `crate::config::CommitConfig`)
//! that lets a project opt out.
//!
//! [`sync_coauthor_hook`] is called wherever ralphus first touches a
//! project's git repo -- `server.rs`'s `POST /api/projects` handler (right
//! after `validate_project_location` confirms the path is a real, local git
//! repo) and `crate::worktrees::ensure_worktree_with_existing` (every squad's
//! worktree materialization) -- rather than once at install time, so a
//! config change takes effect on the project's next registration or squad
//! run without a separate "reinstall" step, and a hook missing for any
//! reason (a fresh clone, a deleted `.git/hooks` dir) is re-created the same
//! way.
//!
//! The hook script shells out to `git interpret-trailers`, which git itself
//! uses to parse/append trailers -- reimplementing that parsing here would
//! risk disagreeing with git about where the trailer block starts. Passing
//! `--if-exists addIfDifferent` keys "already present" off the exact
//! `(key, value)` pair, not just the `Co-authored-by` key alone, so an
//! existing *different* co-author trailer (e.g. a human pairing partner, or
//! another agent's own attribution) is left in place and this trailer is
//! still added -- while re-running the hook (an amend, a second commit with
//! no new content) never appends a second identical Ralphus trailer.
//!
//! Hooks are never scoped per-worktree by git itself -- `git rev-parse
//! --git-path hooks` resolves to the same shared directory (the common
//! `.git/hooks`, or wherever `core.hooksPath` points) no matter which of a
//! repo's worktrees it's run from -- so installing once, from whichever
//! worktree happens to materialize first, covers every worktree of that
//! project.

use std::path::{Path, PathBuf};

use crate::guardian_merge::git;

/// Marker embedded in every hook this module installs, so a later sync can
/// tell "safe to overwrite/remove" (this exact hook, possibly an older
/// version of its script) apart from a hand-authored `prepare-commit-msg`
/// ralphus must never clobber.
const MARKER: &str = "# ralphus:coauthor-hook (RAL-445)";

/// The hook script installed at `<hooks dir>/prepare-commit-msg`. POSIX `sh`
/// (not bash) so it runs under Git for Windows' bundled `sh.exe` the same way
/// every other git hook does there.
fn hook_script() -> String {
    format!(
        "#!/bin/sh\n\
         {MARKER} -- managed by ralphus, do not edit by hand.\n\
         # Re-running ralphus against this project regenerates or removes this\n\
         # file to match `.ralphus.toml`'s `[commits] add_coauthor`; hand edits\n\
         # made here are lost the next time it syncs.\n\
         exec git interpret-trailers --in-place --if-exists addIfDifferent \\\n\
         \t--trailer \"Co-authored-by: {COAUTHOR_TRAILER}\" \"$1\"\n"
    )
}

/// The exact trailer value every enabled project's commits receive.
pub const COAUTHOR_TRAILER: &str = "ralphus-bot <ralphus-bot@users.noreply.github.com>";

/// Resolve the effective git hooks directory for the repository containing
/// `root` -- `git rev-parse --git-path hooks`, which honors a configured
/// `core.hooksPath` and always names the common (non-per-worktree) hooks
/// directory.
fn hooks_dir(root: &Path) -> Result<PathBuf, String> {
    let out = git(root, &["rev-parse", "--git-path", "hooks"])?;
    let rel = out.trim();
    if rel.is_empty() {
        return Err("git rev-parse --git-path hooks returned empty output".to_string());
    }
    let path = PathBuf::from(rel);
    Ok(if path.is_absolute() {
        path
    } else {
        root.join(path)
    })
}

/// Whether `path` is a `prepare-commit-msg` hook this module previously
/// installed (carries [`MARKER`]).
fn is_ralphus_managed(path: &Path) -> bool {
    std::fs::read_to_string(path)
        .map(|s| s.contains(MARKER))
        .unwrap_or(false)
}

/// Install (when `enabled`) or remove (when not) the RAL-445 co-author hook
/// for the project rooted at `root`.
///
/// Idempotent in both directions: installing over an already-installed copy
/// of this hook rewrites it (picking up a script change after a ralphus
/// upgrade); removing an already-absent hook is a no-op. A `prepare-commit-msg`
/// that exists but doesn't carry [`MARKER`] is a hand-authored hook (or one
/// installed by another tool) and is left untouched either way -- this
/// function never clobbers a project's own hook.
pub fn sync_coauthor_hook(root: &Path, enabled: bool) -> Result<(), String> {
    let dir = hooks_dir(root)?;
    let hook_path = dir.join("prepare-commit-msg");
    if hook_path.exists() && !is_ralphus_managed(&hook_path) {
        return Ok(());
    }
    if enabled {
        std::fs::create_dir_all(&dir)
            .map_err(|e| format!("could not create hooks directory {}: {e}", dir.display()))?;
        std::fs::write(&hook_path, hook_script())
            .map_err(|e| format!("could not write {}: {e}", hook_path.display()))?;
        set_executable(&hook_path)?;
    } else if hook_path.exists() {
        std::fs::remove_file(&hook_path)
            .map_err(|e| format!("could not remove {}: {e}", hook_path.display()))?;
    }
    Ok(())
}

#[cfg(unix)]
fn set_executable(path: &Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    let mut perm = std::fs::metadata(path)
        .map_err(|e| e.to_string())?
        .permissions();
    perm.set_mode(perm.mode() | 0o111);
    std::fs::set_permissions(path, perm).map_err(|e| e.to_string())
}

/// Windows has no executable bit; git for Windows invokes hooks via its
/// bundled `sh.exe` regardless of file permissions, so there is nothing to
/// set here.
#[cfg(not(unix))]
fn set_executable(_path: &Path) -> Result<(), String> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn init_repo(dir: &Path) {
        std::fs::create_dir_all(dir).unwrap();
        git(dir, &["init", "-q"]).unwrap();
        git(dir, &["config", "user.email", "test@example.com"]).unwrap();
        git(dir, &["config", "user.name", "Test"]).unwrap();
    }

    fn temp_repo(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("ralphus-git-hooks-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        init_repo(&dir);
        dir
    }

    #[test]
    fn sync_installs_hook_that_appends_trailer_exactly_once() {
        let repo = temp_repo("install-once");

        sync_coauthor_hook(&repo, true).unwrap();

        let hooks = hooks_dir(&repo).unwrap();
        let hook_path = hooks.join("prepare-commit-msg");
        assert!(hook_path.exists());
        assert!(is_ralphus_managed(&hook_path));

        std::fs::write(repo.join("a.txt"), "hi").unwrap();
        git(&repo, &["add", "a.txt"]).unwrap();
        git(&repo, &["commit", "-m", "first commit"]).unwrap();

        let log = git(&repo, &["log", "-1", "--format=%B"]).unwrap();
        let trailer = format!("Co-authored-by: {COAUTHOR_TRAILER}");
        assert_eq!(
            log.matches(&trailer).count(),
            1,
            "expected exactly one Ralphus trailer, got log:\n{log}"
        );

        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn sync_preserves_existing_trailers() {
        let repo = temp_repo("preserve-trailers");
        sync_coauthor_hook(&repo, true).unwrap();

        std::fs::write(repo.join("a.txt"), "hi").unwrap();
        git(&repo, &["add", "a.txt"]).unwrap();
        git(
            &repo,
            &[
                "commit",
                "-m",
                "first commit\n\nReviewed-by: Someone <someone@example.com>",
            ],
        )
        .unwrap();

        let log = git(&repo, &["log", "-1", "--format=%B"]).unwrap();
        assert!(log.contains("Reviewed-by: Someone <someone@example.com>"));
        assert!(log.contains(&format!("Co-authored-by: {COAUTHOR_TRAILER}")));

        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn sync_disabled_removes_previously_installed_hook() {
        let repo = temp_repo("disable-removes");
        sync_coauthor_hook(&repo, true).unwrap();
        let hook_path = hooks_dir(&repo).unwrap().join("prepare-commit-msg");
        assert!(hook_path.exists());

        sync_coauthor_hook(&repo, false).unwrap();
        assert!(!hook_path.exists());

        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn sync_never_overwrites_a_foreign_hook() {
        let repo = temp_repo("foreign-hook");
        let hooks = hooks_dir(&repo).unwrap();
        std::fs::create_dir_all(&hooks).unwrap();
        let hook_path = hooks.join("prepare-commit-msg");
        std::fs::write(&hook_path, "#!/bin/sh\necho not-ralphus\n").unwrap();

        sync_coauthor_hook(&repo, true).unwrap();
        let contents = std::fs::read_to_string(&hook_path).unwrap();
        assert!(contents.contains("not-ralphus"));
        assert!(!contents.contains(MARKER));

        sync_coauthor_hook(&repo, false).unwrap();
        assert!(
            hook_path.exists(),
            "a foreign hook must survive a disable sync too"
        );

        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn sync_disabled_without_prior_install_is_a_noop() {
        let repo = temp_repo("disable-noop");
        sync_coauthor_hook(&repo, false).unwrap();
        let hook_path = hooks_dir(&repo).unwrap().join("prepare-commit-msg");
        assert!(!hook_path.exists());

        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn repeat_commits_never_duplicate_the_trailer() {
        let repo = temp_repo("no-duplicate-on-amend");
        sync_coauthor_hook(&repo, true).unwrap();

        std::fs::write(repo.join("a.txt"), "hi").unwrap();
        git(&repo, &["add", "a.txt"]).unwrap();
        git(&repo, &["commit", "-m", "first commit"]).unwrap();
        git(
            &repo,
            &["commit", "--amend", "-m", "first commit, reworded"],
        )
        .unwrap();

        let log = git(&repo, &["log", "-1", "--format=%B"]).unwrap();
        let trailer = format!("Co-authored-by: {COAUTHOR_TRAILER}");
        assert_eq!(log.matches(&trailer).count(), 1);

        let _ = std::fs::remove_dir_all(&repo);
    }
}
