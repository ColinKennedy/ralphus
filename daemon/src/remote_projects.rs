//! Projects with no clone on the daemon's host ("remote only"): registered
//! with a clone URL alone, for work that runs entirely on remote machines.
//!
//! Reviews, worktree lookups and per-project settings are keyed by a
//! project's root path, so a remote-only project still gets one: a reserved
//! path under the daemon's state directory, [`root_for`], that is never
//! created. It is only ever used as a key -- nothing reads or runs git in it
//! -- and project config discovery stops there instead of walking up into
//! unrelated `.ralphus.toml` files.

use std::path::{Path, PathBuf};

/// Directory, under the daemon's state directory, that remote-only project
/// roots are named under.
pub const REMOTE_ONLY_DIR: &str = "remote-only-projects";

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
    }
}
