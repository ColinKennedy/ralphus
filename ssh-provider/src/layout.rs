//! Deterministic remote storage layout for persistent Git provisioning
//! (RAL-355 Phase 2/4).
//!
//! ```text
//! <remote_root>/
//!   projects/
//!     <project-name>-<url-hash>/
//!       repository/                persistent clone, created once
//!       worktrees/
//!         <branch-name>-<suffix>/  persistent linked worktree
//!       .provision.lock            per-project remote lock (`flock`)
//!       metadata.json              identity record, human-readable
//! ```
//!
//! Every path is built from `remote_root` plus components derived here, so
//! nothing this provider ever touches can end up outside `remote_root` by
//! construction (no `..`, no absolute component ever gets concatenated in).
//!
//! The project directory's `<url-hash>` suffix (not just the sanitized name)
//! is what makes a changed `clone_url` provision under a *new* identity
//! rather than silently repointing an existing clone -- the RAL-355 Phase 2
//! design interview's explicit choice (see `REMOTE_IMPROVEMENTS.local.md`).
//! The worktree directory's hash suffix exists for the same reason a literal
//! branch name isn't reused directly: two different branches can sanitize to
//! the same slug (`sanitize_slug` drops everything before the last `/`, so
//! `feature/x` and `bugfix/x` both slug to `x`) -- the hash, computed from
//! the *full* branch name, keeps them from colliding on disk even though
//! their human-readable slugs match.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

use crate::config::sanitize_slug;

fn hash_hex(value: &str) -> String {
    let mut hasher = DefaultHasher::new();
    value.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

/// The deterministic project directory under `remote_root`, derived from the
/// project's registered name and clone URL. Stable across repeated
/// `provision` calls for the same project/URL pair; a different URL (even
/// for the same project name) always derives a different directory.
#[must_use]
pub fn project_dir(remote_root: &str, project_name: &str, clone_url: &str) -> String {
    let slug = sanitize_slug(project_name);
    format!("{remote_root}/projects/{slug}-{}", hash_hex(clone_url))
}

/// The persistent clone inside a project directory.
#[must_use]
pub fn repository_dir(project_dir: &str) -> String {
    format!("{project_dir}/repository")
}

/// The parent directory every worktree for a project lives under.
#[must_use]
pub fn worktrees_parent_dir(project_dir: &str) -> String {
    format!("{project_dir}/worktrees")
}

/// The deterministic linked-worktree directory for `branch` inside a project
/// directory. See the module doc for why the hash suffix is load-bearing,
/// not cosmetic.
#[must_use]
pub fn worktree_dir(project_dir: &str, branch: &str) -> String {
    let slug = sanitize_slug(branch);
    format!(
        "{}/{slug}-{}",
        worktrees_parent_dir(project_dir),
        hash_hex(branch)
    )
}

/// The per-project lock file `provision` holds (via `flock`) around every
/// clone/fetch/worktree-create for that project, so two concurrent
/// provisions of the same project on the same machine never race.
#[must_use]
pub fn lock_path(project_dir: &str) -> String {
    format!("{project_dir}/.provision.lock")
}

/// The identity metadata file written alongside a project's clone --
/// human-auditable record of what a directory represents. Not itself the
/// authoritative identity check (the provider trusts `git remote get-url
/// origin` on the actual clone for that, since it can't be stale the way a
/// separate file theoretically could), but useful for a human who SSHes in
/// to inspect an unfamiliar directory under `remote_root`. Written by a
/// plain POSIX `printf` in `crate::provision`, not a real JSON encoder --
/// valid JSON for the common case (project names and clone URLs without an
/// embedded double quote, which covers every URL form git itself accepts),
/// but not guaranteed for pathological input. That's an acceptable
/// limitation for an advisory file a human reads, not a parser depends on.
#[must_use]
pub fn metadata_path(project_dir: &str) -> String {
    format!("{project_dir}/metadata.json")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn project_dir_is_rooted_under_remote_root_and_readable() {
        let dir = project_dir("/srv/ralphus", "my-project", "git@host:org/repo.git");
        assert!(
            dir.starts_with("/srv/ralphus/projects/my-project-"),
            "{dir}"
        );
    }

    #[test]
    fn project_dir_is_deterministic_for_the_same_inputs() {
        let a = project_dir("/srv/ralphus", "proj", "git@host:org/repo.git");
        let b = project_dir("/srv/ralphus", "proj", "git@host:org/repo.git");
        assert_eq!(a, b);
    }

    #[test]
    fn project_dir_differs_when_the_url_changes_even_for_the_same_name() {
        // RAL-355 Phase 2: a changed clone_url must provision under a new
        // identity, not silently repoint the old clone.
        let a = project_dir("/srv/ralphus", "proj", "git@host:org/repo.git");
        let b = project_dir("/srv/ralphus", "proj", "git@host:org/repo-renamed.git");
        assert_ne!(a, b);
    }

    #[test]
    fn project_dir_differs_when_the_name_changes_even_for_the_same_url() {
        // Two projects registered under different names but (accidentally,
        // or deliberately) the same URL must not collide either.
        let a = project_dir("/srv/ralphus", "proj-a", "git@host:org/repo.git");
        let b = project_dir("/srv/ralphus", "proj-b", "git@host:org/repo.git");
        assert_ne!(a, b);
    }

    #[test]
    fn repository_and_worktrees_parent_dirs_are_nested_under_project_dir() {
        let pd = "/srv/ralphus/projects/proj-aaaa";
        assert_eq!(
            repository_dir(pd),
            "/srv/ralphus/projects/proj-aaaa/repository"
        );
        assert_eq!(
            worktrees_parent_dir(pd),
            "/srv/ralphus/projects/proj-aaaa/worktrees"
        );
    }

    #[test]
    fn worktree_dir_differs_for_branches_that_sanitize_to_the_same_slug() {
        // `sanitize_slug` drops everything before the last '/', so these two
        // distinct branch names would collide on slug alone -- the hash
        // suffix (from the *full* branch name) must keep them apart.
        let pd = "/srv/ralphus/projects/proj-aaaa";
        let a = worktree_dir(pd, "feature/x");
        let b = worktree_dir(pd, "bugfix/x");
        assert_ne!(a, b, "distinct branches must not collide on disk");
        assert!(a.starts_with(&format!("{pd}/worktrees/x-")), "{a}");
        assert!(b.starts_with(&format!("{pd}/worktrees/x-")), "{b}");
    }

    #[test]
    fn worktree_dir_is_deterministic_for_the_same_branch() {
        let pd = "/srv/ralphus/projects/proj-aaaa";
        assert_eq!(worktree_dir(pd, "feature/x"), worktree_dir(pd, "feature/x"));
    }

    #[test]
    fn lock_and_metadata_paths_are_direct_children_of_project_dir() {
        let pd = "/srv/ralphus/projects/proj-aaaa";
        assert_eq!(
            lock_path(pd),
            "/srv/ralphus/projects/proj-aaaa/.provision.lock"
        );
        assert_eq!(
            metadata_path(pd),
            "/srv/ralphus/projects/proj-aaaa/metadata.json"
        );
    }
}
