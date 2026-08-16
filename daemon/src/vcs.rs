//! Version-control adapters (RAL-185).
//!
//! The daemon needs a small number of *content* questions answered about a
//! project — "does this branch add anything?", "pull this named branch from the
//! shared remote" — and those answers are VCS-specific. This module is the seam
//! that keeps the rest of the daemon from assuming git, mirroring how
//! `cli/src/ralphus/runner/backend.py`'s `Backend` Protocol keeps the runner
//! from assuming any one agent CLI.
//!
//! Why it matters here specifically: a project is registered with a `vcs` kind
//! (`Store::register_project`), and `machine`-based remote execution hands a
//! provider a VCS-agnostic [`crate::remote_runner::WorkspaceSource`]. If the
//! review path then shelled out to `git` directly, a non-git project would
//! reach provisioning fine and fall over the moment a review touched it — the
//! worst place to discover the assumption.
//!
//! **Scope is deliberately narrow.** This is not an abstraction over all of
//! git. `crate::guardian_merge`'s stacked rebase remains git-specific and
//! unapologetically so — rebasing is not a concept every VCS shares, and
//! pretending otherwise would produce a worse abstraction than none. What lives
//! here is only the handful of operations a *non-git* project could plausibly
//! implement differently and still participate.

use std::path::Path;
use std::process::Command;

/// The VCS kind assumed when a project's kind is unknown or unregistered.
/// Matches `Store::register_project`'s own default.
pub const DEFAULT_KIND: &str = "git";

/// The content operations the review path needs from a project's VCS.
pub trait Vcs: Send + Sync {
    /// The registered `vcs` kind this adapter implements, e.g. `"git"`.
    fn kind(&self) -> &'static str;

    /// Whether `head` introduces any change over `base`.
    ///
    /// `Ok(false)` means the two are identical in content — for a review
    /// branch, that it contributes nothing. An `Err` means the question could
    /// not be answered (an unknown revision, the tool missing); callers must
    /// treat that as "don't know" rather than "no changes", since failing a
    /// review on an unanswered question is worse than missing one empty branch.
    ///
    /// # Errors
    /// When the underlying tool fails or the revisions cannot be resolved.
    fn differs(&self, root: &Path, base: &str, head: &str) -> Result<bool, String>;

    /// Pull `branch` from the shared remote into the checkout at `root`,
    /// overwriting any local ref of that name.
    ///
    /// Used when a branch's work was produced on another machine and published
    /// there (RAL-185 D2): the daemon never publishes, but fetching a branch
    /// whose name and remote are both already known is fully deterministic.
    ///
    /// # Errors
    /// When the branch cannot be retrieved — most often because it was never
    /// published.
    fn fetch_branch(&self, root: &Path, remote: &str, branch: &str) -> Result<(), String>;

    /// Resolve `branch` to an opaque revision id, for logging and provenance.
    ///
    /// # Errors
    /// When the branch does not resolve.
    fn revision_of(&self, root: &Path, branch: &str) -> Result<String, String>;
}

/// The git adapter — the only kind implemented today.
pub struct GitVcs;

impl GitVcs {
    fn run(root: &Path, args: &[&str]) -> Result<std::process::Output, String> {
        Command::new("git")
            .args(args)
            .current_dir(root)
            .output()
            .map_err(|e| format!("could not run git: {e}"))
    }
}

impl Vcs for GitVcs {
    fn kind(&self) -> &'static str {
        "git"
    }

    fn differs(&self, root: &Path, base: &str, head: &str) -> Result<bool, String> {
        let out = Self::run(root, &["diff", "--quiet", base, head])?;
        match out.status.code() {
            // `git diff --quiet` is an exit-code predicate: 0 = identical,
            // 1 = differs. Anything else is git failing to answer at all.
            Some(0) => Ok(false),
            Some(1) => Ok(true),
            other => Err(format!(
                "git diff {base}..{head} could not be evaluated (exit {}): {}",
                other.map_or_else(|| "signal".to_string(), |c| c.to_string()),
                String::from_utf8_lossy(&out.stderr).trim()
            )),
        }
    }

    fn fetch_branch(&self, root: &Path, remote: &str, branch: &str) -> Result<(), String> {
        // `+` forces the update, so a force-push on the producing machine is
        // honoured rather than rejected as a non-fast-forward.
        let refspec = format!("+refs/heads/{branch}:refs/heads/{branch}");
        let out = Self::run(root, &["fetch", remote, &refspec])?;
        if out.status.success() {
            return Ok(());
        }
        Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
    }

    fn revision_of(&self, root: &Path, branch: &str) -> Result<String, String> {
        let out = Self::run(root, &["rev-parse", "--verify", branch])?;
        if out.status.success() {
            Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
        } else {
            Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
        }
    }
}

/// The adapter for a registered project's `vcs` kind, or `None` when no adapter
/// implements it.
///
/// An unrecognized kind returns `None` rather than silently falling back to
/// git: a project registered as something else would otherwise have git
/// commands run against it, which is exactly the assumption this module exists
/// to prevent.
#[must_use]
pub fn for_kind(kind: &str) -> Option<Box<dyn Vcs>> {
    match kind.trim().to_lowercase().as_str() {
        "git" => Some(Box::new(GitVcs)),
        _ => None,
    }
}

/// The adapter for the project rooted at `root`, resolved through the project
/// registry.
///
/// A path that is not a registered project falls back to [`DEFAULT_KIND`] —
/// guardians can be created against an arbitrary directory, and treating those
/// as git preserves the behavior every pre-RAL-185 review already had.
///
/// # Errors
/// When the project is registered with a kind no adapter implements.
pub fn for_project_root(store: &crate::store::Store, root: &Path) -> Result<Box<dyn Vcs>, String> {
    let want = store
        .list_projects()
        .unwrap_or_default()
        .into_iter()
        .find(|p| Path::new(&p.path) == root)
        .map_or_else(|| DEFAULT_KIND.to_string(), |p| p.vcs);
    for_kind(&want).ok_or_else(|| {
        format!(
            "project at {} is registered as vcs \"{want}\", which has no adapter — \
             only \"{DEFAULT_KIND}\" is implemented today",
            root.display()
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn git_is_the_only_implemented_kind() {
        assert!(for_kind("git").is_some());
        assert!(
            for_kind("GIT").is_some(),
            "kind matching is case-insensitive"
        );
        assert!(
            for_kind("perforce").is_none(),
            "an unimplemented kind must not silently fall back to git"
        );
    }

    #[test]
    fn an_unregistered_root_falls_back_to_git() {
        // Guardians can be created against an arbitrary directory; treating
        // those as git preserves every pre-RAL-185 review's behavior.
        let store = crate::store::Store::open_in_memory().unwrap();
        let vcs = for_project_root(&store, Path::new("/not/registered")).expect("falls back");
        assert_eq!(vcs.kind(), "git");
    }

    #[test]
    fn a_project_registered_with_an_unimplemented_kind_errors_instead_of_using_git() {
        let store = crate::store::Store::open_in_memory().unwrap();
        let dir = std::env::temp_dir().join("ral185-vcs-kind");
        std::fs::create_dir_all(&dir).unwrap();
        store
            .register_project("p4proj", "", &dir.to_string_lossy(), "perforce")
            .unwrap();
        let Err(err) = for_project_root(&store, &dir) else {
            panic!("must refuse an unknown kind rather than falling back to git");
        };
        assert!(err.contains("perforce"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
