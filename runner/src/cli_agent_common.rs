//! Helpers shared by the two external-CLI-agent backends
//! ([`crate::claude_code_backend`], [`crate::codex_backend`]), ported from
//! `cli/src/ralphus/runner/cli_agent_common.py`.

use std::path::{Path, PathBuf};

use crate::tools::ToolError;

/// Whether `value` should be routed through a shell rather than exec'd
/// directly: has a space, and isn't a single quoted/wrapped path token (a
/// fully quoted value -- e.g. `"C:\path with space\claude.exe"` -- is a
/// single program path despite containing spaces, so it stays a direct exec).
#[must_use]
pub fn is_compound_command(value: &str) -> bool {
    let trimmed = value.trim();
    if !trimmed.contains(' ') {
        return false;
    }
    !is_quote_wrapped(trimmed)
}

fn is_quote_wrapped(value: &str) -> bool {
    let bytes = value.as_bytes();
    bytes.len() >= 2
        && ((bytes[0] == b'"' && bytes[bytes.len() - 1] == b'"')
            || (bytes[0] == b'\'' && bytes[bytes.len() - 1] == b'\''))
}

fn task_prompts_dir() -> PathBuf {
    ralphus_state_dir().join("task_prompts")
}

fn ralphus_state_dir() -> PathBuf {
    let home = std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map_or_else(|| PathBuf::from("."), PathBuf::from);
    home.join(".ralphus")
}

/// Writes `prompt` to a content-addressed file under `~/.ralphus/task_prompts/`
/// and returns its path -- avoids OS command-line length limits by having the
/// agent CLI read `@<path>` instead of taking the prompt as an argv token.
pub fn write_prompt_file(prompt: &str) -> Result<PathBuf, ToolError> {
    let digest = short_sha256(prompt.as_bytes());
    let dir = task_prompts_dir();
    std::fs::create_dir_all(&dir).map_err(|e| ToolError(format!("{}: {e}", dir.display())))?;
    let path = dir.join(format!("{digest}.md"));
    std::fs::write(&path, prompt).map_err(|e| ToolError(format!("{}: {e}", path.display())))?;
    Ok(path)
}

/// First 16 hex chars of the SHA-256 digest -- matches Python's
/// `hashlib.sha256(prompt.encode()).hexdigest()[:16]`.
fn short_sha256(data: &[u8]) -> String {
    use sha2::{Digest as _, Sha256};
    let digest = Sha256::digest(data);
    digest[..8]
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>()
}

/// A side-channel file the daemon polls to learn the agent's session/thread
/// id before the session finishes (enables "Watch Live"/"Open Agent" early).
#[must_use]
pub fn live_session_path(workspace_root: &Path) -> PathBuf {
    let basename = workspace_root
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "workspace".to_string());
    std::env::temp_dir()
        .join("ralphus")
        .join(format!("{basename}.live_session"))
}

pub fn write_live_session_id(workspace_root: &Path, id: &str) {
    let path = live_session_path(workspace_root);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(path, id);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_sha256_matches_known_vector_prefix() {
        // NIST test vector: SHA-256("abc") = ba7816bf...; first 16 hex chars.
        assert_eq!(short_sha256(b"abc"), "ba7816bf8f01cfea");
    }

    #[test]
    fn is_compound_command_detects_multiword() {
        assert!(is_compound_command("cd /foo && claude"));
        assert!(!is_compound_command("claude"));
        assert!(!is_compound_command("\"C:\\path with space\\claude.exe\""));
    }

    #[test]
    fn write_prompt_file_is_content_addressed() {
        let p1 = write_prompt_file("hello world").unwrap();
        let p2 = write_prompt_file("hello world").unwrap();
        assert_eq!(p1, p2);
        std::fs::remove_file(&p1).ok();
    }
}
