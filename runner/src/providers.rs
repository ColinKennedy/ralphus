//! Agent+model -> provider resolution, ported from the `_build_model`
//! function duplicated verbatim between `cli/src/ralphus/author/agent.py` and
//! `cli/src/ralphus/runner/pydantic_backend.py`. Factored into one shared
//! function here, reused by both [`crate::agent_backend`] (the runner's own
//! tool-loop backend) and the CLI's `author` command.

/// A resolved LLM provider + model, ready to hand to [`crate::llm_client`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Provider {
    Anthropic { model: String },
    Ollama { model: String, base_url: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnknownAgent(pub String);

impl std::fmt::Display for UnknownAgent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "unknown agent for provider resolution: {}", self.0)
    }
}

impl std::error::Error for UnknownAgent {}

const DEFAULT_ANTHROPIC_MODEL: &str = "claude-sonnet-4-5";
const DEFAULT_OLLAMA_MODEL: &str = "qwen3:8b";
const DEFAULT_OLLAMA_BASE_URL: &str = "http://localhost:11434/v1";

/// Resolves `agent`/`model` to a concrete [`Provider`]. `agent` is matched
/// exactly as Python's `_build_model` does: `"claude"`/`"anthropic"` ->
/// Anthropic, `"ollama"` -> Ollama (base URL from `$RALPHUS_OLLAMA_URL`,
/// default `http://localhost:11434/v1`), anything else -> error.
pub fn resolve(agent: &str, model: Option<&str>) -> Result<Provider, UnknownAgent> {
    let ollama_url_override = std::env::var("RALPHUS_OLLAMA_URL").ok();
    resolve_with(agent, model, ollama_url_override.as_deref())
}

/// [`resolve`] with the `$RALPHUS_OLLAMA_URL` lookup passed in explicitly, so
/// tests can exercise both branches without mutating real process
/// environment (`std::env::set_var`/`remove_var` are `unsafe` and this
/// workspace forbids `unsafe_code` outright).
fn resolve_with(
    agent: &str,
    model: Option<&str>,
    ollama_url_override: Option<&str>,
) -> Result<Provider, UnknownAgent> {
    match agent {
        "claude" | "anthropic" => Ok(Provider::Anthropic {
            model: model.unwrap_or(DEFAULT_ANTHROPIC_MODEL).to_string(),
        }),
        "ollama" => Ok(Provider::Ollama {
            model: model.unwrap_or(DEFAULT_OLLAMA_MODEL).to_string(),
            base_url: ollama_url_override
                .unwrap_or(DEFAULT_OLLAMA_BASE_URL)
                .to_string(),
        }),
        other => Err(UnknownAgent(other.to_string())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_claude_alias() {
        assert_eq!(
            resolve("claude", None).unwrap(),
            Provider::Anthropic {
                model: DEFAULT_ANTHROPIC_MODEL.to_string()
            }
        );
        assert_eq!(
            resolve("anthropic", Some("claude-opus-5")).unwrap(),
            Provider::Anthropic {
                model: "claude-opus-5".to_string()
            }
        );
    }

    #[test]
    fn resolves_ollama_with_default_url() {
        assert_eq!(
            resolve_with("ollama", None, None).unwrap(),
            Provider::Ollama {
                model: DEFAULT_OLLAMA_MODEL.to_string(),
                base_url: DEFAULT_OLLAMA_BASE_URL.to_string()
            }
        );
    }

    #[test]
    fn resolves_ollama_with_overridden_url() {
        assert_eq!(
            resolve_with("ollama", Some("qwen3:32b"), Some("http://gpu-box:11434/v1")).unwrap(),
            Provider::Ollama {
                model: "qwen3:32b".to_string(),
                base_url: "http://gpu-box:11434/v1".to_string()
            }
        );
    }

    #[test]
    fn rejects_unknown_agent() {
        assert!(resolve("carrier-pigeon", None).is_err());
    }
}
