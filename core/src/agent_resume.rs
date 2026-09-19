//! Builds the local CLI command line that resumes a CLI-agent conversation
//! (`claude --resume <id>` / `codex resume <id>` / `pi --session <id>`).
//!
//! Shared by the daemon (which prints this command for its `open-terminal`
//! HTTP endpoints) and the CLI (`ralphus session terminal` / `ralphus review
//! branch terminal` / `ralphus review checks terminal`, which print it
//! directly since a CLI has no GUI terminal to spawn), so both call the exact
//! same logic instead of each deriving it separately.

/// Builds the PowerShell command line that resumes `session_id` under
/// `program` (the Claude Code CLI or a compatible fork) with permission
/// prompts pre-approved -- "Open Agent" always resumes non-interactively,
/// so it must never immediately stall on a prompt a human has to notice and
/// click through.
///
/// `program` may be a compound shell line (RAL-468: a `RALPHUS_CLAUDE_COMMAND`
/// wrapper like `rez-env foo -- claude`, not a single executable) -- see
/// [`crate::shellcmd::is_compound_command`]. That case is shell-routed:
/// `program` runs through the shell verbatim (it is already a valid command
/// line) with the resume flags appended as quoted tokens, the same way the
/// runner's headless launch path already routes a compound override rather
/// than exec'ing it directly. A plain program name keeps the exact quoting
/// this function always used, unchanged.
#[must_use]
pub fn resume_agent_command(program: &str, session_id: &str) -> String {
    if crate::shellcmd::is_compound_command(program) {
        return crate::shellcmd::build_compound_command_line(
            "powershell",
            program,
            &[
                "--resume".to_string(),
                session_id.to_string(),
                "--dangerously-skip-permissions".to_string(),
            ],
        );
    }
    let safe_program = program.replace('\'', "''");
    let safe_session = session_id.replace('\'', "''");
    format!("& '{safe_program}' --resume '{safe_session}' --dangerously-skip-permissions")
}

/// The Codex analog of [`resume_agent_command`] -- Codex resumes via a
/// subcommand (`resume <id>`), not a flag, and its permission-bypass flag is
/// spelled differently.
///
/// This is the top-level, *interactive* `codex resume` (TUI), not `codex
/// exec resume` -- the latter is Codex's non-interactive headless mode and
/// requires a prompt argument (or piped stdin) or it fails immediately with
/// "No prompt provided", which is exactly the reported symptom of using it
/// here for an interactive terminal resume with nothing to pipe in. `codex
/// exec resume <id> <prompt>` remains correct and unchanged for the
/// runner's own headless cross-cell session sharing (RAL-248,
/// `runner/src/codex_backend.rs`), which always has a real prompt to send —
/// a different code path from this one.
#[must_use]
pub fn resume_codex_agent_command(program: &str, session_id: &str) -> String {
    if crate::shellcmd::is_compound_command(program) {
        return crate::shellcmd::build_compound_command_line(
            "powershell",
            program,
            &[
                "resume".to_string(),
                session_id.to_string(),
                "--dangerously-bypass-approvals-and-sandbox".to_string(),
            ],
        );
    }
    let safe_program = program.replace('\'', "''");
    let safe_session = session_id.replace('\'', "''");
    format!("& '{safe_program}' resume '{safe_session}' --dangerously-bypass-approvals-and-sandbox")
}

/// Pi resumes a concrete session via `--session <id>` and uses `--approve`
/// to trust project-local files for the resumed run.
#[must_use]
pub fn resume_pi_agent_command(program: &str, session_id: &str) -> String {
    if crate::shellcmd::is_compound_command(program) {
        return crate::shellcmd::build_compound_command_line(
            "powershell",
            program,
            &[
                "--session".to_string(),
                session_id.to_string(),
                "--approve".to_string(),
            ],
        );
    }
    let safe_program = program.replace('\'', "''");
    let safe_session = session_id.replace('\'', "''");
    format!("& '{safe_program}' --session '{safe_session}' --approve")
}

/// POSIX-shell equivalent of [`resume_agent_command`], for a remote Linux
/// target reached over SSH (RAL-355 Phase 10). Same flag shape and the same
/// unconditional permission-prompt bypass as the local, PowerShell-quoted
/// version -- only the quoting convention differs (POSIX single-quote
/// escaping, `'\''`, not PowerShell's doubled `''`). Claude Code only for
/// now; the Codex/Pi analogs are deliberately not added until their remote
/// terminal paths are actually exercised (matching this workspace's existing
/// precedent of deferring an unexercised harness's event/command shape
/// rather than guessing at it).
#[must_use]
pub fn resume_agent_command_posix(program: &str, session_id: &str) -> String {
    if crate::shellcmd::is_compound_command(program) {
        return crate::shellcmd::build_compound_command_line(
            "bash",
            program,
            &[
                "--resume".to_string(),
                session_id.to_string(),
                "--dangerously-skip-permissions".to_string(),
            ],
        );
    }
    format!(
        "{} --resume {} --dangerously-skip-permissions",
        posix_quote_single(program),
        posix_quote_single(session_id)
    )
}

/// POSIX single-quote a value: wraps it in `'...'`, ending/re-opening the
/// quote around any embedded `'` (the standard POSIX-shell escape, since a
/// single-quoted string cannot itself contain an escaped quote).
fn posix_quote_single(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

/// True when `agent` identifies a Claude Code-family backend, resumable via
/// [`resume_agent_command`]/[`resume_agent_command_posix`] (RAL-355 Phase
/// 10: the remote terminal relay is Claude Code-only for its first version,
/// and needs to distinguish this from Codex/Pi/`ollama`/`raw`, none of which
/// resume the same way).
#[must_use]
pub fn is_claude_agent(agent: Option<&str>) -> bool {
    matches!(
        agent,
        Some("claude" | "anthropic" | "claude-code" | "claude-cli")
    )
}

/// True when `agent` identifies a Codex-family backend (`codex`/`codex-cli`).
#[must_use]
pub fn is_codex_agent(agent: Option<&str>) -> bool {
    matches!(agent, Some("codex" | "codex-cli"))
}

/// True when `agent` identifies the Pi backend.
#[must_use]
pub fn is_pi_agent(agent: Option<&str>) -> bool {
    matches!(agent, Some("pi"))
}

/// Appended to a resumed agent's context in `--mode readonly`.
pub const READONLY_RESUME_INSTRUCTIONS: &str = "You are in read-only mode. You may only read files. Do NOT write, \
edit, delete, commit, or push anything.";

/// Builds the resume argv (unquoted tokens) printed by `ralphus cell
/// terminal` / `ralphus review branch terminal` / `ralphus review checks
/// terminal` for a human to copy into their own shell, rather than the
/// quoted PowerShell command line [`resume_agent_command`] and friends build
/// for the daemon to spawn directly. Token quoting is deliberately left to
/// the reader's own shell here -- unlike the spawn path, there is no shell
/// this crate controls to quote for.
///
/// `claude_program`/`codex_program`/`pi_program` are each caller-resolved
/// (normally the `RALPHUS_CLAUDE_COMMAND`/`RALPHUS_CODEX_COMMAND`/
/// `RALPHUS_PI_COMMAND` env var override, falling back to the bare agent
/// name) so this crate never reads the environment itself -- RAL-468: this
/// used to be duplicated per-caller with the agent name hardcoded, silently
/// ignoring that same override the daemon's real "Open Agent" spawn already
/// respected.
#[must_use]
pub fn agent_resume_argv(
    agent: Option<&str>,
    agent_session_id: &str,
    mode: &str,
    claude_program: &str,
    codex_program: &str,
    pi_program: &str,
) -> Vec<String> {
    if is_codex_agent(agent) {
        let mut cmd = vec![codex_program.to_string()];
        if mode == "readonly" {
            cmd.push("-c".to_string());
            cmd.push(format!(
                "developer_instructions={READONLY_RESUME_INSTRUCTIONS}"
            ));
        }
        cmd.push("resume".to_string());
        cmd.push(agent_session_id.to_string());
        cmd
    } else if is_pi_agent(agent) {
        let mut cmd = vec![
            pi_program.to_string(),
            "--session".to_string(),
            agent_session_id.to_string(),
            "--approve".to_string(),
        ];
        if mode == "readonly" {
            cmd.push("--append-system-prompt".to_string());
            cmd.push(READONLY_RESUME_INSTRUCTIONS.to_string());
        }
        cmd
    } else {
        let mut cmd = vec![
            claude_program.to_string(),
            "--resume".to_string(),
            agent_session_id.to_string(),
        ];
        if mode == "readonly" {
            cmd.push("--dangerously-skip-permissions".to_string());
            cmd.push("--append-system-prompt".to_string());
            cmd.push(READONLY_RESUME_INSTRUCTIONS.to_string());
        }
        cmd
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resume_agent_command_always_skips_permissions() {
        let cmd = resume_agent_command("claude", "abc-123");
        assert!(cmd.contains("--dangerously-skip-permissions"));
        assert!(cmd.contains("--resume 'abc-123'"));
        assert!(cmd.contains("& 'claude'"));
    }

    /// RAL-468: a `RALPHUS_CLAUDE_COMMAND` wrapper like `foo bar -- claude`
    /// is a compound shell line, not a single executable -- it must run
    /// through the shell verbatim, not get quoted as one literal program
    /// name (which is what made `& 'foo bar -- claude' --resume ...` try to
    /// exec a file literally named `foo bar -- claude`).
    #[test]
    fn resume_agent_command_shell_routes_a_compound_program() {
        let cmd = resume_agent_command("foo bar -- claude", "abc-123");
        assert_eq!(
            cmd,
            "foo bar -- claude '--resume' 'abc-123' '--dangerously-skip-permissions'"
        );
        assert!(!cmd.starts_with("& '"));
    }

    #[test]
    fn resume_agent_command_escapes_single_quotes() {
        let cmd = resume_agent_command("my'claude", "sess'123");
        assert!(cmd.contains("my''claude"));
        assert!(cmd.contains("sess''123"));
    }

    #[test]
    fn resume_agent_command_posix_always_skips_permissions() {
        let cmd = resume_agent_command_posix("claude", "abc-123");
        assert!(cmd.contains("--dangerously-skip-permissions"));
        assert!(cmd.contains("--resume 'abc-123'"));
        assert!(cmd.starts_with("'claude'"));
    }

    #[test]
    fn resume_agent_command_posix_escapes_single_quotes() {
        let cmd = resume_agent_command_posix("my'claude", "sess'123");
        assert!(cmd.contains("my'\\''claude"), "{cmd}");
        assert!(cmd.contains("sess'\\''123"), "{cmd}");
    }

    #[test]
    fn resume_codex_agent_command_always_bypasses_approvals() {
        let cmd = resume_codex_agent_command("codex", "thread-abc-123");
        assert!(cmd.contains("--dangerously-bypass-approvals-and-sandbox"));
        // The top-level interactive `codex resume`, not `codex exec resume`
        // (the non-interactive mode, which requires a prompt argument or
        // piped stdin and fails immediately without one).
        assert!(cmd.contains("resume 'thread-abc-123'"));
        assert!(!cmd.contains("exec"));
        assert!(cmd.contains("& 'codex'"));
    }

    #[test]
    fn resume_codex_agent_command_shell_routes_a_compound_program() {
        let cmd = resume_codex_agent_command("foo bar -- codex", "thread-abc-123");
        assert_eq!(
            cmd,
            "foo bar -- codex 'resume' 'thread-abc-123' '--dangerously-bypass-approvals-and-sandbox'"
        );
    }

    #[test]
    fn resume_codex_agent_command_escapes_single_quotes() {
        let cmd = resume_codex_agent_command("my'codex", "sess'123");
        assert!(cmd.contains("my''codex"));
        assert!(cmd.contains("sess''123"));
    }

    #[test]
    fn resume_pi_agent_command_uses_session_and_approve() {
        let cmd = resume_pi_agent_command("pi", "session-123");
        assert!(cmd.contains("--session 'session-123'"));
        assert!(cmd.contains("--approve"));
        assert!(cmd.contains("& 'pi'"));
    }

    #[test]
    fn resume_pi_agent_command_shell_routes_a_compound_program() {
        let cmd = resume_pi_agent_command("foo bar -- pi", "session-123");
        assert_eq!(cmd, "foo bar -- pi '--session' 'session-123' '--approve'");
    }

    #[test]
    fn resume_agent_command_posix_shell_routes_a_compound_program() {
        let cmd = resume_agent_command_posix("foo bar -- claude", "abc-123");
        assert_eq!(
            cmd,
            "foo bar -- claude --resume abc-123 --dangerously-skip-permissions"
        );
    }

    #[test]
    fn agent_resume_argv_uses_caller_supplied_program_override() {
        let cmd = agent_resume_argv(
            Some("claude"),
            "sess-1",
            "open",
            "foo bar -- claude",
            "codex",
            "pi",
        );
        assert_eq!(cmd, vec!["foo bar -- claude", "--resume", "sess-1"]);
    }

    #[test]
    fn agent_resume_argv_dispatches_per_agent_and_mode() {
        assert_eq!(
            agent_resume_argv(Some("codex"), "sess-1", "open", "claude", "codex", "pi"),
            vec!["codex", "resume", "sess-1"]
        );
        assert_eq!(
            agent_resume_argv(Some("pi"), "sess-1", "open", "claude", "codex", "pi"),
            vec!["pi", "--session", "sess-1", "--approve"]
        );
        let readonly = agent_resume_argv(
            Some("claude"),
            "sess-1",
            "readonly",
            "claude",
            "codex",
            "pi",
        );
        assert!(readonly.contains(&"--dangerously-skip-permissions".to_string()));
        assert!(readonly.contains(&READONLY_RESUME_INSTRUCTIONS.to_string()));
    }

    #[test]
    fn is_claude_agent_matches_every_alias() {
        assert!(is_claude_agent(Some("claude")));
        assert!(is_claude_agent(Some("anthropic")));
        assert!(is_claude_agent(Some("claude-code")));
        assert!(is_claude_agent(Some("claude-cli")));
    }

    #[test]
    fn is_claude_agent_false_for_other_backends_and_unset() {
        assert!(!is_claude_agent(Some("codex")));
        assert!(!is_claude_agent(Some("pi")));
        assert!(!is_claude_agent(Some("ollama")));
        assert!(!is_claude_agent(None));
    }

    #[test]
    fn is_codex_agent_matches_both_aliases() {
        assert!(is_codex_agent(Some("codex")));
        assert!(is_codex_agent(Some("codex-cli")));
    }

    #[test]
    fn is_codex_agent_false_for_claude_and_unset() {
        assert!(!is_codex_agent(Some("claude-code")));
        assert!(!is_codex_agent(Some("claude-cli")));
        assert!(!is_codex_agent(Some("ollama")));
        assert!(!is_codex_agent(None));
    }

    #[test]
    fn is_pi_agent_only_matches_pi() {
        assert!(is_pi_agent(Some("pi")));
        assert!(!is_pi_agent(Some("codex")));
        assert!(!is_pi_agent(Some("claude-code")));
        assert!(!is_pi_agent(None));
    }
}
