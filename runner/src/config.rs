//! Minimal `.ralphus.toml` reader for the runner, ported from the subset of
//! `cli/src/ralphus/config.py` the runner itself actually consumes:
//! `daemon.keep_temporary_files` and `task.maximum_timeout_seconds` (used as
//! [`crate::harness_backend`]'s fallback when a session sets no
//! `timeout_sec` of its own). The daemon's own richer config
//! (`daemon/src/config.rs`) and the CLI's full config surface are separate;
//! this is deliberately just the two fields the runner subprocess needs.

use std::path::{Path, PathBuf};

const DEFAULT_MAX_TIMEOUT_SECS: u64 = 1800;

#[derive(Debug, Clone, Copy)]
pub struct RunnerConfig {
    pub keep_temporary_files: bool,
    /// `None` means unbounded (a `0` or negative value in the TOML file).
    pub maximum_timeout_seconds: Option<u64>,
}

impl Default for RunnerConfig {
    fn default() -> Self {
        Self {
            keep_temporary_files: false,
            maximum_timeout_seconds: Some(DEFAULT_MAX_TIMEOUT_SECS),
        }
    }
}

/// Loads config starting from `start_dir` (the session's workspace root),
/// merging `$RALPHUS_CONFIGURATION_PATH` entries (left-to-right, later wins)
/// and finally the nearest git-repo-root `.ralphus.toml` (highest priority),
/// matching `config.py`'s precedence.
#[must_use]
pub fn load(start_dir: &Path) -> RunnerConfig {
    load_with(start_dir, configuration_path_entries().as_slice())
}

/// [`load`] with the `$RALPHUS_CONFIGURATION_PATH` entries passed in
/// explicitly, so tests are hermetic against whatever that variable happens
/// to be set to in the real process environment (a real, common dev-machine
/// setting -- `std::env::set_var`/`remove_var` can't be used to override it
/// in-process, since both are `unsafe fn` and this workspace forbids
/// `unsafe_code` outright).
fn load_with(start_dir: &Path, configuration_path_entries: &[PathBuf]) -> RunnerConfig {
    let mut config = RunnerConfig::default();

    for path in configuration_path_entries {
        apply_file(&mut config, path);
    }
    if let Some(root_config) = find_git_root_config(start_dir) {
        apply_file(&mut config, &root_config);
    }

    config
}

fn configuration_path_entries() -> Vec<PathBuf> {
    let Ok(raw) = std::env::var("RALPHUS_CONFIGURATION_PATH") else {
        return Vec::new();
    };
    std::env::split_paths(&raw).collect()
}

fn find_git_root_config(start_dir: &Path) -> Option<PathBuf> {
    let mut dir = start_dir.canonicalize().ok()?;
    loop {
        if dir.join(".git").exists() {
            let candidate = dir.join(".ralphus.toml");
            return candidate.is_file().then_some(candidate);
        }
        dir = dir.parent()?.to_path_buf();
    }
}

fn apply_file(config: &mut RunnerConfig, path: &Path) {
    let Ok(text) = std::fs::read_to_string(path) else {
        return;
    };
    let Ok(parsed) = text.parse::<toml::Table>() else {
        return;
    };
    if let Some(keep) = parsed
        .get("daemon")
        .and_then(|d| d.get("keep_temporary_files"))
        .and_then(toml::Value::as_bool)
    {
        config.keep_temporary_files = keep;
    }
    if let Some(secs) = parsed
        .get("task")
        .and_then(|t| t.get("maximum_timeout_seconds"))
        .and_then(toml::Value::as_integer)
    {
        config.maximum_timeout_seconds = if secs > 0 { Some(secs as u64) } else { None };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_when_no_files_present() {
        let dir = std::env::temp_dir().join(format!("ralphus-config-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let config = load_with(&dir, &[]);
        assert!(!config.keep_temporary_files);
        assert_eq!(
            config.maximum_timeout_seconds,
            Some(DEFAULT_MAX_TIMEOUT_SECS)
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn applies_explicit_configuration_path_file() {
        let dir = std::env::temp_dir().join(format!(
            "ralphus-config-test-explicit-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let cfg_path = dir.join("override.toml");
        std::fs::write(&cfg_path, "[daemon]\nkeep_temporary_files = true\n").unwrap();

        let mut config = RunnerConfig::default();
        apply_file(&mut config, &cfg_path);
        assert!(config.keep_temporary_files);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn zero_or_negative_timeout_means_unbounded() {
        let dir = std::env::temp_dir().join(format!(
            "ralphus-config-test-unbounded-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let cfg_path = dir.join("t.toml");
        std::fs::write(&cfg_path, "[task]\nmaximum_timeout_seconds = 0\n").unwrap();
        let mut config = RunnerConfig::default();
        apply_file(&mut config, &cfg_path);
        assert_eq!(config.maximum_timeout_seconds, None);
        std::fs::remove_dir_all(&dir).ok();
    }
}
