//! Shared helpers for isolating a spawned agent session (Claude Code, Codex,
//! Pi) from the operator's personal CLI configuration and memory on the host
//! machine (RAL-336). By default a ralphus-spawned session should not read or
//! write the operator's real `~/.claude`, `~/.codex`, or Pi config directory
//! -- each backend redirects its own config-dir env var to a per-worktree
//! directory under here instead, only when its resolved
//! `allow_personal_settings`/`allow_personal_memory` options call for it.

use std::path::{Path, PathBuf};

/// The operator's home directory (`USERPROFILE` on Windows, `HOME`
/// elsewhere) -- used as the fallback root for a backend's real, ambient
/// config directory when its own dedicated env var
/// (`CLAUDE_CONFIG_DIR`/`CODEX_HOME`/`PI_CODING_AGENT_DIR`) isn't set, so
/// isolation still has somewhere to look for an existing login/settings file
/// worth preserving.
#[must_use]
pub fn home_dir() -> Option<PathBuf> {
    std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from)
}

/// A per-worktree, per-backend directory to redirect an agent CLI's config-dir
/// env var to, so isolated runs don't read or write the operator's real
/// config/memory. Distinct workspaces never share a directory, so concurrent
/// cells never collide -- same "keyed off the workspace root's basename"
/// convention as [`crate::cli_agent_common::live_session_path`].
#[must_use]
pub fn isolated_config_dir(workspace_root: &Path, backend: &str) -> PathBuf {
    let basename = workspace_root
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "workspace".to_string());
    std::env::temp_dir()
        .join("ralphus")
        .join("agent_isolation")
        .join(backend)
        .join(basename)
}

/// Best-effort: copy `filename` from `real_dir` into `isolated_dir` (creating
/// `isolated_dir` first if needed) before a backend's config dir is
/// redirected, so an OAuth/subscription login already stored on disk in the
/// operator's real config dir keeps working under isolation instead of
/// silently forcing a re-login. Every failure (missing source file, missing
/// real dir, a create/copy error) is swallowed -- isolation must never fail a
/// cell just because there was nothing to preserve.
pub fn preserve_auth_file(real_dir: Option<&Path>, isolated_dir: &Path, filename: &str) {
    let Some(real_dir) = real_dir else {
        return;
    };
    let source = real_dir.join(filename);
    if !source.is_file() {
        return;
    }
    if std::fs::create_dir_all(isolated_dir).is_err() {
        return;
    }
    let _ = std::fs::copy(source, isolated_dir.join(filename));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn isolated_config_dir_is_keyed_by_backend_and_workspace_basename() {
        let ws = Path::new("/tmp/some/workspace-abc");
        let claude = isolated_config_dir(ws, "claude-code");
        let codex = isolated_config_dir(ws, "codex");
        assert_ne!(claude, codex);
        assert!(claude.ends_with("claude-code/workspace-abc"));
        assert!(codex.ends_with("codex/workspace-abc"));
    }

    #[test]
    fn isolated_config_dir_differs_across_workspaces() {
        let a = isolated_config_dir(Path::new("/tmp/a"), "codex");
        let b = isolated_config_dir(Path::new("/tmp/b"), "codex");
        assert_ne!(a, b);
    }

    #[test]
    fn preserve_auth_file_copies_when_present() {
        let real = std::env::temp_dir().join(format!("ralphus-auth-real-{}", std::process::id()));
        let isolated =
            std::env::temp_dir().join(format!("ralphus-auth-isolated-{}", std::process::id()));
        std::fs::create_dir_all(&real).unwrap();
        std::fs::write(real.join("auth.json"), "{\"token\":\"secret\"}").unwrap();

        preserve_auth_file(Some(&real), &isolated, "auth.json");

        assert_eq!(
            std::fs::read_to_string(isolated.join("auth.json")).unwrap(),
            "{\"token\":\"secret\"}"
        );

        let _ = std::fs::remove_dir_all(&real);
        let _ = std::fs::remove_dir_all(&isolated);
    }

    #[test]
    fn preserve_auth_file_is_a_no_op_when_source_is_missing() {
        let real = std::env::temp_dir().join(format!("ralphus-auth-empty-{}", std::process::id()));
        let isolated = std::env::temp_dir().join(format!(
            "ralphus-auth-empty-isolated-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&real).unwrap();

        preserve_auth_file(Some(&real), &isolated, "auth.json");

        assert!(!isolated.exists());

        let _ = std::fs::remove_dir_all(&real);
    }

    #[test]
    fn preserve_auth_file_is_a_no_op_when_real_dir_is_none() {
        let isolated =
            std::env::temp_dir().join(format!("ralphus-auth-none-isolated-{}", std::process::id()));
        preserve_auth_file(None, &isolated, "auth.json");
        assert!(!isolated.exists());
    }
}
