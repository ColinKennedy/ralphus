//! Builds the local CLI command line that resumes a CLI-agent conversation
//! (`claude --resume <id>` / `codex resume <id>` / `pi --session <id>`).
//!
//! Shared by the CLI (`ralphus cell terminal` / `ralphus review branch
//! terminal` / `ralphus review checks terminal`), which prints the command
//! rather than spawning it, and the MCP server.

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
/// quoted command line a spawner would need. Token quoting is deliberately
/// left to the reader's own shell.
///
/// `claude_program`/`codex_program`/`pi_program` are each caller-resolved
/// (normally the `RALPHUS_CLAUDE_COMMAND`/`RALPHUS_CODEX_COMMAND`/
/// `RALPHUS_PI_COMMAND` env var override, falling back to the bare agent
/// name) so this crate never reads the environment itself.
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
