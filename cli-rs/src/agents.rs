//! Registry of agent backends ralphus knows how to run (`ralphus agent
//! list`), ported from `cli/src/ralphus/agents.py`. Purely informational --
//! there is no dynamic/runtime agent registry; the daemon and runner accept
//! `agent` as a free-form string. Update this when a new agent backend is
//! added or a known agent's model constraints change.

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentInfo {
    pub name: &'static str,
    pub aliases: &'static [&'static str],
    pub description: &'static str,
    /// `None` means any model string is accepted (passed through unchecked).
    pub models: Option<&'static [&'static str]>,
    pub default_model: Option<&'static str>,
}

pub const KNOWN_AGENTS: &[AgentInfo] = &[
    AgentInfo {
        name: "claude",
        aliases: &["anthropic"],
        description: "Native Anthropic API backend. Uses ANTHROPIC_API_KEY, or your Claude subscription via claude-code if unset.",
        models: None,
        default_model: Some("claude-sonnet-4-5"),
    },
    AgentInfo {
        name: "claude-code",
        aliases: &["claude-cli"],
        description: "Claude Code CLI, run as a subprocess (subscription or ANTHROPIC_API_KEY). Only the CLI's own model aliases are accepted.",
        models: Some(&["sonnet", "opus", "haiku", "fable"]),
        default_model: None,
    },
    AgentInfo {
        name: "ollama",
        aliases: &[],
        description: "Local Ollama server. Any model you've pulled locally.",
        models: None,
        default_model: Some("qwen3:8b"),
    },
    AgentInfo {
        name: "codex",
        aliases: &["codex-cli"],
        description: "OpenAI Codex CLI, run as a subprocess.",
        models: None,
        default_model: None,
    },
    AgentInfo {
        name: "pi",
        aliases: &[],
        description: "Pi coding-agent CLI, run as a subprocess with JSON event parsing.",
        models: None,
        default_model: None,
    },
    AgentInfo {
        name: "raw",
        aliases: &[],
        description: "Generic external executable backend. Only valid when selected via an agent profile that also provides an executable.",
        models: None,
        default_model: None,
    },
];

pub const OTHER_AGENTS_NOTE: &str = "Any other agent name must be defined as an agent profile in .ralphus.toml. The old implicit generic-harness fallback is gone.";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_known_agent_has_a_name_and_description() {
        for agent in KNOWN_AGENTS {
            assert!(!agent.name.is_empty());
            assert!(!agent.description.is_empty());
        }
    }

    #[test]
    fn claude_code_restricts_models_others_do_not() {
        let claude_code = KNOWN_AGENTS
            .iter()
            .find(|a| a.name == "claude-code")
            .unwrap();
        assert_eq!(
            claude_code.models,
            Some(["sonnet", "opus", "haiku", "fable"].as_slice())
        );
        let claude = KNOWN_AGENTS.iter().find(|a| a.name == "claude").unwrap();
        assert_eq!(claude.models, None);
    }
}
