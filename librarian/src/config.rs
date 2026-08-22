//! Minimal `.ralphus.toml`/global config loading for the librarian (RAL-220).
//!
//! The librarian's proxy sits in front of the daemon API, so a browser
//! hitting the librarian's port directly must be gated by CORS at the
//! librarian's own HTTP boundary too -- the daemon's own gate never sees
//! that request. This mirrors `ralphus-daemon`'s `daemon/src/config.rs` file
//! discovery rules (global `$RALPHUS_CONFIG_HOME/config.toml`, else
//! `~/.config/ralphus/config.toml`, layered under the nearest per-project
//! `.ralphus.toml`, current-dir-based since the librarian has no per-request
//! "project root") for just the one table the librarian itself needs to read
//! directly: `[cors]`. Parsing/merging logic itself lives in
//! `ralphus_core::cors` and is shared with the daemon; only this file
//! discovery is duplicated, since `ralphus-core` is deliberately
//! side-effect-free (no file I/O) and the librarian must not depend back on
//! the daemon crate.

use std::path::{Path, PathBuf};

use ralphus_core::cors::CorsConfig;

fn global_config_path() -> Option<PathBuf> {
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

fn find_project_config(start: &Path) -> Option<PathBuf> {
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

/// Load the effective CORS allow-list: the global config file layered under
/// the nearest per-project `.ralphus.toml` (list field, unioned).
#[must_use]
pub fn load_cors_config() -> CorsConfig {
    let global = global_config_path()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .map(|s| ralphus_core::cors::from_toml_str(&s))
        .unwrap_or_default();
    let local = std::env::current_dir()
        .ok()
        .as_deref()
        .and_then(find_project_config)
        .and_then(|p| std::fs::read_to_string(p).ok())
        .map(|s| ralphus_core::cors::from_toml_str(&s))
        .unwrap_or_default();
    global.merge(local)
}
