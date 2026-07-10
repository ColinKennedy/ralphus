//! Layered review configuration (CCTL-156).
//!
//! Review defaults can be set in a TOML file under a `[review]` (preferred) or
//! `[defaults]` table. Two files are consulted: a global one
//! (`$RALPHUS_CONFIG_HOME/config.toml`, defaulting to `~/.config/ralphus/`) and a
//! per-project one (`.ralphus.toml`, discovered by walking up from the review's
//! cwd). When both are present they are merged with **per-project values winning
//! on scalar conflicts** while **list fields (e.g. `checks`) are unioned**.
//!
//! Today the only consumed scalar is `skip_worktrees`, which lets large repos
//! avoid a git worktree copy per branch.

use std::path::{Path, PathBuf};

use serde::Deserialize;

/// Resolved review configuration (after layering global under per-project).
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct ReviewConfig {
    /// Skip creating a git worktree per branch during merge. `None` means unset
    /// (so a lower layer can supply it); resolved callers treat `None` as false.
    #[serde(default)]
    pub skip_worktrees: Option<bool>,
    /// Check-gate commands (a list field: layers are unioned, not overridden).
    #[serde(default)]
    pub checks: Vec<String>,
}

impl ReviewConfig {
    /// Whether worktrees should be skipped (unset resolves to `false`).
    #[must_use]
    pub fn skip_worktrees(&self) -> bool {
        self.skip_worktrees.unwrap_or(false)
    }

    /// Layer `self` (global) under `over` (per-project). Per-project scalars win
    /// when present; list fields are unioned (global first, then new per-project
    /// entries, order-preserving and de-duplicated).
    #[must_use]
    pub fn merge(self, over: ReviewConfig) -> ReviewConfig {
        let mut checks = self.checks;
        for c in over.checks {
            if !checks.contains(&c) {
                checks.push(c);
            }
        }
        ReviewConfig {
            skip_worktrees: over.skip_worktrees.or(self.skip_worktrees),
            checks,
        }
    }
}

/// Daemon-level configuration (`[daemon]` table).
#[derive(Debug, Default, Deserialize, Clone)]
pub struct DaemonConfig {
    /// Path to the log file; when absent all output goes to stderr.
    #[serde(default)]
    pub log_path: Option<String>,
    /// Minimum log level: `"error"`, `"warn"`, `"info"`, `"debug"`, `"trace"`.
    /// Defaults to `"debug"` when unset.
    #[serde(default)]
    pub log_level: Option<String>,
}

/// The on-disk file shape: either a `[review]` or a `[defaults]` table.
#[derive(Debug, Default, Deserialize)]
struct ConfigFile {
    #[serde(default)]
    review: Option<ReviewConfig>,
    #[serde(default)]
    defaults: Option<ReviewConfig>,
    #[serde(default)]
    daemon: Option<DaemonConfig>,
}

/// Parse a config from TOML text, preferring `[review]` over `[defaults]`.
/// Malformed TOML yields the default (empty) config rather than an error, so a
/// broken file never blocks a review.
#[must_use]
pub fn from_toml_str(s: &str) -> ReviewConfig {
    let cf: ConfigFile = toml::from_str(s).unwrap_or_default();
    cf.review.or(cf.defaults).unwrap_or_default()
}

/// Read and parse a config file, or the default when it is absent/unreadable.
#[must_use]
pub fn load_file(path: &Path) -> ReviewConfig {
    std::fs::read_to_string(path)
        .map(|s| from_toml_str(&s))
        .unwrap_or_default()
}

/// The global config path: `$RALPHUS_CONFIG_HOME/config.toml`, else
/// `~/.config/ralphus/config.toml` (`USERPROFILE`/`HOME`). `None` if no home.
#[must_use]
pub fn global_config_path() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("RALPHUS_CONFIG_HOME") {
        return Some(PathBuf::from(dir).join("config.toml"));
    }
    let home = std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME"))?;
    Some(
        PathBuf::from(home)
            .join(".config")
            .join("ralphus")
            .join("config.toml"),
    )
}

/// Discover the per-project config by walking up from `start` until a
/// `.ralphus.toml` is found or the filesystem root is reached.
#[must_use]
pub fn find_project_config(start: &Path) -> Option<PathBuf> {
    let mut dir = Some(start);
    while let Some(d) = dir {
        let candidate = d.join(".ralphus.toml");
        if candidate.is_file() {
            return Some(candidate);
        }
        dir = d.parent();
    }
    None
}

/// Parse a `DaemonConfig` from the given TOML text.
#[must_use]
pub fn daemon_from_toml_str(s: &str) -> DaemonConfig {
    toml::from_str::<ConfigFile>(s)
        .unwrap_or_default()
        .daemon
        .unwrap_or_default()
}

/// Load the effective daemon config by layering the global config file under
/// the nearest per-project `.ralphus.toml` (per-project scalars win).
#[must_use]
pub fn load_daemon_config() -> DaemonConfig {
    let global = global_config_path()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .map(|s| daemon_from_toml_str(&s))
        .unwrap_or_default();
    let local = std::env::current_dir()
        .ok()
        .as_deref()
        .and_then(find_project_config)
        .and_then(|p| std::fs::read_to_string(p).ok())
        .map(|s| daemon_from_toml_str(&s))
        .unwrap_or_default();
    DaemonConfig {
        log_path: local.log_path.or(global.log_path),
        log_level: local.log_level.or(global.log_level),
    }
}

/// Resolve the effective review config for a review rooted at `cwd`: the global
/// config layered under the nearest per-project `.ralphus.toml`.
#[must_use]
pub fn resolve(cwd: &Path) -> ReviewConfig {
    let global = global_config_path()
        .map(|p| load_file(&p))
        .unwrap_or_default();
    let project = find_project_config(cwd)
        .map(|p| load_file(&p))
        .unwrap_or_default();
    global.merge(project)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(skip: Option<bool>, checks: &[&str]) -> ReviewConfig {
        ReviewConfig {
            skip_worktrees: skip,
            checks: checks.iter().map(|s| (*s).to_string()).collect(),
        }
    }

    #[test]
    fn parse_review_table() {
        let c = from_toml_str("[review]\nskip_worktrees = true\nchecks = [\"cargo test\"]\n");
        assert_eq!(c.skip_worktrees, Some(true));
        assert_eq!(c.checks, vec!["cargo test".to_string()]);
    }

    #[test]
    fn parse_defaults_table_fallback() {
        let c = from_toml_str("[defaults]\nskip_worktrees = false\n");
        assert_eq!(c.skip_worktrees, Some(false));
    }

    #[test]
    fn review_table_preferred_over_defaults() {
        let c =
            from_toml_str("[defaults]\nskip_worktrees = false\n[review]\nskip_worktrees = true\n");
        assert_eq!(c.skip_worktrees, Some(true));
    }

    #[test]
    fn malformed_toml_is_default() {
        assert_eq!(from_toml_str("not = = valid"), ReviewConfig::default());
    }

    // ── merge cases (the four required by CCTL-156) ──────────────────────────

    #[test]
    fn merge_global_only() {
        let merged = cfg(Some(true), &["a"]).merge(ReviewConfig::default());
        assert!(merged.skip_worktrees());
        assert_eq!(merged.checks, vec!["a".to_string()]);
    }

    #[test]
    fn merge_project_only() {
        let merged = ReviewConfig::default().merge(cfg(Some(true), &["b"]));
        assert!(merged.skip_worktrees());
        assert_eq!(merged.checks, vec!["b".to_string()]);
    }

    #[test]
    fn merge_conflict_project_wins() {
        // Global says false, project says true -> project (true) wins.
        let merged = cfg(Some(false), &["a"]).merge(cfg(Some(true), &["a", "b"]));
        assert_eq!(merged.skip_worktrees, Some(true));
        // list union, de-duplicated, order-preserving
        assert_eq!(merged.checks, vec!["a".to_string(), "b".to_string()]);
    }

    #[test]
    fn merge_disjoint_keys() {
        // Global sets skip, project sets a check; both survive.
        let merged = cfg(Some(true), &[]).merge(cfg(None, &["only-project"]));
        assert_eq!(merged.skip_worktrees, Some(true));
        assert_eq!(merged.checks, vec!["only-project".to_string()]);
    }

    #[test]
    fn find_project_config_walks_up() {
        let base = std::env::temp_dir().join(format!("ralphus-cfg-{}", std::process::id()));
        let nested = base.join("a").join("b").join("c");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(
            base.join(".ralphus.toml"),
            "[review]\nskip_worktrees=true\n",
        )
        .unwrap();

        let found = find_project_config(&nested).expect("should find the ancestor config");
        assert_eq!(found, base.join(".ralphus.toml"));
        assert!(load_file(&found).skip_worktrees());

        // No config anywhere above a fresh, unrelated temp dir.
        let empty = std::env::temp_dir().join(format!("ralphus-empty-{}", std::process::id()));
        std::fs::create_dir_all(&empty).unwrap();
        // (may still find one if the temp root has a stray file; assert only our positive case)

        let _ = std::fs::remove_dir_all(&base);
        let _ = std::fs::remove_dir_all(&empty);
    }
}
