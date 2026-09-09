//! Helpers shared by the two external-CLI-agent backends
//! ([`crate::claude_code_backend`], [`crate::codex_backend`]), ported from
//! `cli/src/ralphus/runner/cli_agent_common.py`.

use std::path::{Path, PathBuf};

use crate::tools::ToolError;
use crate::{backend::BackendError, shellcmd};

/// Whether `value` should be routed through a shell rather than exec'd
/// directly: has a space, and isn't a single quoted/wrapped path token (a
/// fully quoted value -- e.g. `"C:\path with space\claude.exe"` -- is a
/// single program path despite containing spaces, so it stays a direct exec).
/// The shell command line that runs `program` with `args`.
///
/// The two shell-routed cases need opposite treatment of `program`, and
/// conflating them is a bug in both directions. A user-authored compound
/// command (`cd /foo && claude`) is already a shell line: quoting it would
/// turn the whole thing into one filename. A batch launcher is a *path*, and
/// leaving it bare splits it at any space -- which is why an npm shim under
/// `C:\Program Files\...` never launched (RAL-385).
/// A launcher path is told apart from a shell line by whether it names a real
/// file: [`is_compound_command`] only sees a space, and a path like
/// `C:\Program Files\nodejs\pi.cmd` has one for an innocent reason. An
/// already-quoted value is not a file by that test, so it keeps flowing
/// through untouched rather than being quoted a second time.
#[must_use]
pub fn shell_command_line(shell: &str, program: &str, args: &[String]) -> String {
    let trimmed = program.trim();
    if !trimmed.is_empty() && Path::new(trimmed).is_file() {
        return shellcmd::build_program_command_line(shell, trimmed, args);
    }
    shellcmd::build_compound_command_line(shell, program, args)
}

#[must_use]
pub fn is_compound_command(value: &str) -> bool {
    let trimmed = value.trim();
    if !trimmed.contains(' ') {
        return false;
    }
    !is_quote_wrapped(trimmed)
}

/// Whether a launcher must be passed through a shell instead of directly to
/// `Command::new`. Windows batch wrappers need `cmd /C` even when their path
/// is already fully resolved; npm commonly installs CLIs in that form.
#[must_use]
pub fn launcher_requires_shell(value: &str) -> bool {
    if is_compound_command(value) {
        return true;
    }
    is_windows_batch_launcher(value)
}

/// Whether this direct launcher is a Windows batch wrapper. Unlike a
/// user-authored compound command, it has one required interpreter: cmd.exe.
#[must_use]
pub fn is_windows_batch_launcher(value: &str) -> bool {
    if !cfg!(target_os = "windows") {
        return false;
    }
    matches!(
        Path::new(value)
            .extension()
            .and_then(|extension| extension.to_str())
            .map(str::to_ascii_lowercase)
            .as_deref(),
        Some("cmd" | "bat")
    )
}

/// Check a backend's default direct CLI launcher without executing it.
///
/// A configured command override is deliberately skipped: it can contain
/// user-authored shell syntax, and validating it would require executing that
/// command during a review preflight.
pub fn preflight_default_program(
    program: &str,
    custom_program_configured: bool,
) -> Result<(), BackendError> {
    if custom_program_configured || is_compound_command(program) {
        return Ok(());
    }
    shellcmd::find_program(program)
        .ok_or_else(|| BackendError(format!("program {program:?} is not resolvable on PATH")))?;
    Ok(())
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
    fn batch_launcher_requires_a_shell_only_on_windows() {
        assert_eq!(
            launcher_requires_shell("C:\\npm\\pi.cmd"),
            cfg!(target_os = "windows")
        );
    }

    #[test]
    fn write_prompt_file_is_content_addressed() {
        let p1 = write_prompt_file("hello world").unwrap();
        let p2 = write_prompt_file("hello world").unwrap();
        assert_eq!(p1, p2);
        std::fs::remove_file(&p1).ok();
    }
}
