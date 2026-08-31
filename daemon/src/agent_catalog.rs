//! Cwd-independent agent + model catalog for the board's Simple task form
//! (RAL-297).
//!
//! `GET /api/agents?cwd=...` (`crate::agent_access`) scopes its answer to one
//! project's `.ralphus.toml`, so a review's agent picker can only be
//! populated once a `cwd` is already in hand. The Simple tab's agent picker
//! is deliberately independent of the project/`cwd` picker (an interview
//! decision for RAL-297: the two selections happen side by side, neither
//! blocking the other), so it needs a catalog that doesn't require a `cwd`
//! at all -- this module reuses [`crate::agent_access::DefaultAgentAccess`]
//! against the daemon process's own current directory (the same
//! current-dir-based convention `crate::config`'s daemon-level loaders use)
//! and layers on a small per-backend model list so the board can also scope
//! its model dropdown to whichever agent is selected.

use serde::Serialize;

use crate::agent_access::AgentAccess;

/// One agent the Simple tab's agent dropdown can offer.
#[derive(Debug, Clone, Serialize)]
pub struct CatalogAgent {
    /// What `agent = "..."` in the generated task TOML should be set to.
    pub id: String,
    /// `"builtin"` or `"profile"` -- see [`crate::agent_access::AvailableAgent`].
    pub kind: &'static str,
    /// The backend this ultimately resolves to. Same as `id` for a builtin.
    pub backend: String,
    /// Known model choices for `backend`. Empty means "any model accepted"
    /// -- the board falls back to free-text model entry (per RAL-297's
    /// design: raw free-text is always allowed even when a dropdown is
    /// shown).
    pub models: Vec<String>,
}

/// Built-in backends' known model choices, mirroring
/// `cli-rs/src/agents.rs::KNOWN_AGENTS`. Kept as a small duplicate here
/// since `daemon` cannot depend on `cli-rs` (the dependency runs the other
/// way). A backend absent from this table (e.g. `"ollama"`, `"codex"`)
/// accepts any model name -- see [`CatalogAgent::models`].
const BUILTIN_MODELS: &[(&str, &[&str])] =
    &[("claude-code", &["sonnet", "opus", "haiku", "fable"])];

/// The known model choices for `backend`, or empty when any model is
/// accepted.
#[must_use]
fn models_for_backend(backend: &str) -> Vec<String> {
    BUILTIN_MODELS
        .iter()
        .find(|(b, _)| *b == backend)
        .map(|(_, models)| models.iter().map(|m| (*m).to_string()).collect())
        .unwrap_or_default()
}

/// The cwd-independent agent catalog: built-in backends plus any
/// globally-discoverable `[agent.profiles.*]` entries, each with its known
/// model list. Never fails -- an error loading profiles (e.g. a malformed
/// `.ralphus.toml`) degrades to the builtin-only list, matching this
/// codebase's "malformed config never blocks" rule (see `crate::config`'s
/// module doc comment).
#[must_use]
pub fn agent_catalog() -> Vec<CatalogAgent> {
    let cwd = std::env::current_dir().unwrap_or_default();
    let user = crate::agent_access::UserContext::default();
    let agents = crate::agent_access::DefaultAgentAccess
        .available_agents(&user, &cwd)
        .unwrap_or_default();
    agents
        .into_iter()
        .map(|a| CatalogAgent {
            models: models_for_backend(&a.backend),
            id: a.id,
            kind: a.kind,
            backend: a.backend,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_catalog_includes_claude_code_with_its_restricted_models() {
        let agents = agent_catalog();
        let claude_code = agents
            .iter()
            .find(|a| a.id == "claude-code")
            .expect("claude-code is a builtin");
        assert_eq!(claude_code.kind, "builtin");
        assert_eq!(claude_code.models, vec!["sonnet", "opus", "haiku", "fable"]);
    }

    #[test]
    fn agent_catalog_backend_with_no_known_models_is_empty_not_missing() {
        let agents = agent_catalog();
        let ollama = agents
            .iter()
            .find(|a| a.id == "ollama")
            .expect("ollama is a builtin");
        assert!(ollama.models.is_empty());
    }
}
