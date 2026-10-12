//! One-time backfill of the `upstream_remote` / `upstream_branch` columns from
//! the combined base-branch strings rows were stored with before the pair
//! existed. This is the only place a combined `<remote>/<branch>` string is
//! still taken apart: the split is decided by the remote names configured in
//! the row's repository (read from its `config` file), never by the shape of
//! the string alone, and a value naming no configured remote is a local base.

use std::collections::HashSet;
use std::path::Path;

use ralphus_core::schema::UpstreamRef;

/// The remote names configured in `git_root`'s repository, read from the
/// `[remote "<name>"]` section headers of its `.git/config`. Empty when the
/// config cannot be read (a worktree's `.git` file, a missing repository).
fn configured_remotes(git_root: &Path) -> HashSet<String> {
    let Ok(config) = std::fs::read_to_string(git_root.join(".git").join("config")) else {
        return HashSet::new();
    };
    config
        .lines()
        .filter_map(|line| {
            let line = line.trim();
            let inner = line.strip_prefix("[remote \"")?.strip_suffix("\"]")?;
            Some(inner.to_string())
        })
        .collect()
}

/// Split a combined legacy base-branch string into a pair against the remotes
/// configured in `git_root`.
pub(crate) fn split_legacy_base(combined: &str, git_root: &Path) -> UpstreamRef {
    let combined = combined.strip_prefix("remotes/").unwrap_or(combined);
    if let Some((head, rest)) = combined.split_once('/') {
        if !rest.is_empty() && configured_remotes(git_root).contains(head) {
            return UpstreamRef::new(head, rest);
        }
    }
    UpstreamRef::local(combined)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo_with_remotes(names: &[&str]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let git = dir.path().join(".git");
        std::fs::create_dir_all(&git).unwrap();
        let mut config = String::from("[core]\n\tbare = false\n");
        for name in names {
            config.push_str(&format!(
                "[remote \"{name}\"]\n\turl = x\n\tfetch = +refs/heads/*:refs/remotes/{name}/*\n"
            ));
        }
        std::fs::write(git.join("config"), config).unwrap();
        dir
    }

    #[test]
    fn a_configured_remote_prefix_is_split_off() {
        let dir = repo_with_remotes(&["origin", "alt"]);
        assert_eq!(
            split_legacy_base("alt/release/1.2", dir.path()),
            UpstreamRef::new("alt", "release/1.2")
        );
    }

    #[test]
    fn a_slashed_local_branch_stays_local() {
        let dir = repo_with_remotes(&["origin"]);
        assert_eq!(
            split_legacy_base("colin/feature", dir.path()),
            UpstreamRef::local("colin/feature")
        );
        assert_eq!(
            split_legacy_base("main", dir.path()),
            UpstreamRef::local("main")
        );
    }

    #[test]
    fn an_unreadable_repository_yields_a_local_base() {
        assert_eq!(
            split_legacy_base("origin/main", Path::new("Z:/nope")),
            UpstreamRef::local("origin/main")
        );
    }
}
