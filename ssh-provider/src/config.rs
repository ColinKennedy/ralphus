//! Environment-driven configuration, kept as pure functions taking already
//! read values so they stay unit-testable without touching real env vars.
//!
//! | Variable | Default | Meaning |
//! |---|---|---|
//! | `RALPHUS_SSH_REMOTE_BASE` | `~/.ralphus/ssh-workspaces` | Remote parent directory workspaces are created under. |
//! | `RALPHUS_SSH_EXCLUDE` | *(none)* | Comma-separated patterns added to [`crate::transport::DEFAULT_EXCLUDES`]. |
//! | `RALPHUS_SSH_CONNECT_TIMEOUT_SECS` | `15` | `ssh -o ConnectTimeout=`. |
//! | `RALPHUS_SSH_REMOTE_RUNNER_CMD` | `ralphus-runner` | The command run remotely, mirroring the daemon's own `RALPHUS_RUNNER_CMD`. |

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

/// Default remote parent directory workspaces are created under. Expanded by
/// the remote shell (`~` is meaningful to `sh`/`bash`, not to us) -- we never
/// need to resolve it locally.
pub const DEFAULT_REMOTE_BASE: &str = "~/.ralphus/ssh-workspaces";

/// Default `ssh -o ConnectTimeout=`.
pub const DEFAULT_CONNECT_TIMEOUT_SECS: u32 = 15;

/// Default remote command, mirroring the daemon's own `RALPHUS_RUNNER_CMD`
/// default (`daemon/src/runner.rs`).
pub const DEFAULT_REMOTE_RUNNER_CMD: &str = "ralphus-runner";

/// Split a comma-separated env value into trimmed, non-empty patterns.
#[must_use]
pub fn parse_exclude_list(raw: &str) -> Vec<String> {
    raw.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

/// Derive a stable, filesystem-safe remote workspace directory for a given
/// local source directory, under `base`.
///
/// Deterministic across daemon restarts and repeated `exec` calls against the
/// same session (each of a task's sessions/verify steps typically reuses the
/// same local `cwd`), so a session's remote workspace is reused rather than
/// re-created from scratch every call -- the sync step (`rsync --delete` or a
/// fresh tar extraction) still keeps its *contents* current either way.
///
/// Uses a fixed-key hash ([`DefaultHasher`], not the randomized `HashMap`
/// default) purely to shorten an arbitrarily long local path into a stable,
/// collision-resistant suffix -- not for anything security-sensitive.
#[must_use]
pub fn remote_workspace_dir(base: &str, local_dir: &str) -> String {
    let mut hasher = DefaultHasher::new();
    local_dir.hash(&mut hasher);
    let digest = hasher.finish();
    let slug = sanitize_slug(local_dir);
    format!("{base}/{slug}-{digest:016x}")
}

/// Reduce an arbitrary local path to a short, shell-safe slug: its final
/// path component, with anything that is not alphanumeric/`-`/`_` collapsed
/// to `_`, capped to a reasonable length so it stays readable.
fn sanitize_slug(local_dir: &str) -> String {
    let last = local_dir
        .rsplit(['/', '\\'])
        .find(|s| !s.is_empty())
        .unwrap_or("workspace");
    let cleaned: String = last
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let trimmed = cleaned.trim_matches('_');
    let trimmed = if trimmed.is_empty() {
        "workspace"
    } else {
        trimmed
    };
    trimmed.chars().take(48).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_exclude_list_trims_and_drops_empties() {
        assert_eq!(
            parse_exclude_list(" a , b ,, c "),
            vec!["a".to_string(), "b".to_string(), "c".to_string()]
        );
        assert_eq!(parse_exclude_list(""), Vec::<String>::new());
    }

    #[test]
    fn remote_workspace_dir_is_deterministic_for_the_same_input() {
        let a = remote_workspace_dir(DEFAULT_REMOTE_BASE, "/home/alice/repo");
        let b = remote_workspace_dir(DEFAULT_REMOTE_BASE, "/home/alice/repo");
        assert_eq!(a, b);
    }

    #[test]
    fn remote_workspace_dir_differs_for_different_local_dirs() {
        let a = remote_workspace_dir(DEFAULT_REMOTE_BASE, "/home/alice/repo-one");
        let b = remote_workspace_dir(DEFAULT_REMOTE_BASE, "/home/alice/repo-two");
        assert_ne!(a, b);
    }

    #[test]
    fn remote_workspace_dir_is_rooted_under_base_and_readable() {
        let dir = remote_workspace_dir("~/.ralphus/ssh-workspaces", r"C:\work\my-repo");
        assert!(dir.starts_with("~/.ralphus/ssh-workspaces/"), "{dir}");
        assert!(dir.contains("my-repo"), "{dir}");
    }

    #[test]
    fn sanitize_slug_strips_unsafe_characters() {
        assert_eq!(sanitize_slug(r"C:\work\weird name!@#"), "weird_name");
        assert_eq!(sanitize_slug("/a/b/c"), "c");
        assert_eq!(sanitize_slug(""), "workspace");
    }
}
