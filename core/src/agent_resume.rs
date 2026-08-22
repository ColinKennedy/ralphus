//! Builds the local CLI command line that resumes a CLI-agent conversation
//! (`claude --resume <id>` / `codex exec resume <id>`).
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
#[must_use]
pub fn resume_agent_command(program: &str, session_id: &str) -> String {
    let safe_program = program.replace('\'', "''");
    let safe_session = session_id.replace('\'', "''");
    format!("& '{safe_program}' --resume '{safe_session}' --dangerously-skip-permissions")
}

/// The Codex analog of [`resume_agent_command`] -- Codex resumes via a
/// subcommand (`exec resume <id>`), not a flag, and its permission-bypass
/// flag is spelled differently.
#[must_use]
pub fn resume_codex_agent_command(program: &str, session_id: &str) -> String {
    let safe_program = program.replace('\'', "''");
    let safe_session = session_id.replace('\'', "''");
    format!(
        "& '{safe_program}' exec resume '{safe_session}' --dangerously-bypass-approvals-and-sandbox"
    )
}

/// True when `agent` identifies a Codex-family backend (`codex`/`codex-cli`).
/// Anything else -- including `claude-code`/`claude-cli`, `None` (an older
/// row from before the `agent` column was threaded through this lookup), or
/// any other string -- resumes via the Claude Code CLI.
#[must_use]
pub fn is_codex_agent(agent: Option<&str>) -> bool {
    matches!(agent, Some("codex" | "codex-cli"))
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

    #[test]
    fn resume_agent_command_escapes_single_quotes() {
        let cmd = resume_agent_command("my'claude", "sess'123");
        assert!(cmd.contains("my''claude"));
        assert!(cmd.contains("sess''123"));
    }

    #[test]
    fn resume_codex_agent_command_always_bypasses_approvals() {
        let cmd = resume_codex_agent_command("codex", "thread-abc-123");
        assert!(cmd.contains("--dangerously-bypass-approvals-and-sandbox"));
        assert!(cmd.contains("exec resume 'thread-abc-123'"));
        assert!(cmd.contains("& 'codex'"));
    }

    #[test]
    fn resume_codex_agent_command_escapes_single_quotes() {
        let cmd = resume_codex_agent_command("my'codex", "sess'123");
        assert!(cmd.contains("my''codex"));
        assert!(cmd.contains("sess''123"));
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
}
