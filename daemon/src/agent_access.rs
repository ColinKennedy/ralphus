//! Which agents a user can select, behind a seam a real "User adapter" can
//! later fill in (RAL-252).
//!
//! ralphus has no multi-user authentication today -- [`UserContext`] is a
//! placeholder identity (see `crate::users`/`crate::config::DaemonConfig::default_user`),
//! not a verified one. [`AgentAccess`] exists so the board UI's and daemon's
//! agent-selection surfaces are already written against "ask something for
//! this user's available agents" rather than a hardcoded list, even though
//! the only implementation today ([`DefaultAgentAccess`]) ignores the user
//! entirely and shows everyone the same thing.
//!
//! TODO: Replace with user auth once RAL-252 is done -- `DefaultAgentAccess`
//! should become one of several `AgentAccess` implementations, chosen by
//! whatever real authentication resolves the caller to, and should actually
//! filter by grant rather than always allow.

use std::path::Path;

use serde::Serialize;

/// A caller's identity for agent-access purposes. `id: None` means no user
/// identity was presented or configured -- today's common case.
///
/// Carried as its own type (not a bare `Option<String>`) so a real multi-user
/// "User adapter" can grow this later (roles, org, ...) without changing
/// every call site's signature.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UserContext {
    pub id: Option<String>,
}

/// One agent a user can select -- a built-in backend or a configured custom
/// profile -- for a given project.
#[derive(Debug, Clone, Serialize)]
pub struct AvailableAgent {
    /// What `agent = "..."` in a TOML file, or the board's dropdown value,
    /// should be set to. Equal to `backend` for a built-in; the profile's own
    /// name for a custom profile.
    pub id: String,
    /// `"builtin"` or `"profile"`.
    pub kind: &'static str,
    /// The backend this ultimately resolves to (see
    /// `agent_profiles::resolve_agent_for_path`). Same as `id` for a builtin.
    pub backend: String,
}

/// Built-in backends worth surfacing as a bare selectable option. Mirrors
/// `agent_profiles::PROFILE_BACKENDS` minus `"raw"` -- `raw` is only ever
/// meaningful via a profile that also supplies an `executable`
/// (`cli-rs/src/agents.rs`'s own description), never bare.
const BUILTIN_AGENTS: &[&str] = &["claude", "claude-code", "codex", "ollama", "anthropic"];

/// Decides which agents a given user may see/select for a given project.
///
/// Only one implementation exists today ([`DefaultAgentAccess`]), which
/// ignores [`UserContext`] entirely -- everyone sees the same list. This
/// trait is the seam a future per-user "User adapter" implements once
/// ralphus has real multi-user auth, filtering by the user's granted agents
/// instead of returning everything. Nothing here enforces access on the
/// submission/execution path yet -- see the module doc comment.
pub trait AgentAccess: Send + Sync {
    /// # Errors
    /// Propagates any failure loading `.ralphus.toml` agent profiles for `cwd`.
    fn available_agents(
        &self,
        user: &UserContext,
        cwd: &Path,
    ) -> Result<Vec<AvailableAgent>, String>;
}

/// The only [`AgentAccess`] implementation today: permissive by design, does
/// not consult `user` at all. Not an access-control mechanism -- see the
/// module doc comment.
pub struct DefaultAgentAccess;

impl AgentAccess for DefaultAgentAccess {
    fn available_agents(
        &self,
        _user: &UserContext,
        cwd: &Path,
    ) -> Result<Vec<AvailableAgent>, String> {
        let mut agents: Vec<AvailableAgent> = BUILTIN_AGENTS
            .iter()
            .map(|name| AvailableAgent {
                id: (*name).to_string(),
                kind: "builtin",
                backend: (*name).to_string(),
            })
            .collect();
        for (name, profile) in crate::agent_profiles::load_profiles_for_path(cwd)? {
            agents.push(AvailableAgent {
                id: name,
                kind: "profile",
                backend: profile.backend,
            });
        }
        Ok(agents)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;

    fn tempdir(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "ralphus-agent-access-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("time")
                .as_nanos()
        ));
        fs::create_dir_all(&dir).expect("create tempdir");
        dir
    }

    #[test]
    fn default_agent_access_lists_builtins_with_no_profiles() {
        let cwd = tempdir("no-profiles");
        let agents = DefaultAgentAccess
            .available_agents(&UserContext::default(), &cwd)
            .expect("agents");
        assert!(
            agents
                .iter()
                .any(|a| a.id == "claude-code" && a.kind == "builtin")
        );
        assert!(!agents.iter().any(|a| a.id == "raw"));
    }

    #[test]
    fn default_agent_access_includes_configured_profiles() {
        let cwd = tempdir("with-profile");
        fs::write(
            cwd.join(".ralphus.toml"),
            "[agent.profiles.my-openrouter]\nbackend = \"claude-code\"\n",
        )
        .expect("write project config");
        let agents = DefaultAgentAccess
            .available_agents(&UserContext::default(), &cwd)
            .expect("agents");
        let profile = agents
            .iter()
            .find(|a| a.id == "my-openrouter")
            .expect("profile present");
        assert_eq!(profile.kind, "profile");
        assert_eq!(profile.backend, "claude-code");
    }

    #[test]
    fn default_agent_access_ignores_user_identity() {
        let cwd = tempdir("ignores-user");
        let anonymous = DefaultAgentAccess
            .available_agents(&UserContext::default(), &cwd)
            .expect("agents");
        let named = DefaultAgentAccess
            .available_agents(
                &UserContext {
                    id: Some("colin".to_string()),
                },
                &cwd,
            )
            .expect("agents");
        let anon_ids: Vec<&str> = anonymous.iter().map(|a| a.id.as_str()).collect();
        let named_ids: Vec<&str> = named.iter().map(|a| a.id.as_str()).collect();
        assert_eq!(anon_ids, named_ids);
    }
}
