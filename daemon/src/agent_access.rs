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

use crate::store::Store;

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
/// (`cli/src/agents.rs`'s own description), never bare.
const BUILTIN_AGENTS: &[&str] = &[
    "claude",
    "claude-code",
    "codex",
    "pi",
    "ollama",
    "anthropic",
];

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
    /// Propagates any failure loading `.ralphus.toml` agent profiles for
    /// `cwd`, or listing RAL-473 database-backed profiles from `store`.
    fn available_agents(
        &self,
        user: &UserContext,
        cwd: &Path,
        store: &Store,
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
        store: &Store,
    ) -> Result<Vec<AvailableAgent>, String> {
        let mut agents: Vec<AvailableAgent> = BUILTIN_AGENTS
            .iter()
            .map(|name| AvailableAgent {
                id: (*name).to_string(),
                kind: "builtin",
                backend: (*name).to_string(),
            })
            .collect();
        // RAL-473: database-backed profiles are global (no cwd layering,
        // unlike TOML profiles) and win over a same-named legacy TOML
        // profile -- mirrors the precedence `agent_profiles::resolve_agent_for_path_with`
        // already applies at cell-run time, so a profile listed here
        // resolves to the same thing it's listed as.
        let db_profiles = store
            .list_agent_profiles()
            .map_err(|e| format!("could not list database agent profiles: {e}"))?;
        let db_names: std::collections::BTreeSet<&str> =
            db_profiles.iter().map(|p| p.name.as_str()).collect();
        for (name, profile) in crate::agent_profiles::load_profiles_for_path(cwd)? {
            if db_names.contains(name.as_str()) {
                continue;
            }
            agents.push(AvailableAgent {
                id: name,
                kind: "profile",
                backend: profile.backend,
            });
        }
        for profile in db_profiles {
            agents.push(AvailableAgent {
                id: profile.name,
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
        let store = Store::open_in_memory().expect("open store");
        let agents = DefaultAgentAccess
            .available_agents(&UserContext::default(), &cwd, &store)
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
        let store = Store::open_in_memory().expect("open store");
        let agents = DefaultAgentAccess
            .available_agents(&UserContext::default(), &cwd, &store)
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
        let store = Store::open_in_memory().expect("open store");
        let anonymous = DefaultAgentAccess
            .available_agents(&UserContext::default(), &cwd, &store)
            .expect("agents");
        let named = DefaultAgentAccess
            .available_agents(
                &UserContext {
                    id: Some("colin".to_string()),
                },
                &cwd,
                &store,
            )
            .expect("agents");
        let anon_ids: Vec<&str> = anonymous.iter().map(|a| a.id.as_str()).collect();
        let named_ids: Vec<&str> = named.iter().map(|a| a.id.as_str()).collect();
        assert_eq!(anon_ids, named_ids);
    }

    #[test]
    fn default_agent_access_includes_database_backed_profiles() {
        let cwd = tempdir("db-profile");
        let store = Store::open_in_memory().expect("open store");
        store
            .upsert_agent_profile(
                "openrouter-deepseek",
                "claude-code",
                None,
                None,
                Vec::new(),
                None,
            )
            .expect("save db profile");
        let agents = DefaultAgentAccess
            .available_agents(&UserContext::default(), &cwd, &store)
            .expect("agents");
        let profile = agents
            .iter()
            .find(|a| a.id == "openrouter-deepseek")
            .expect("db-backed profile present regardless of cwd");
        assert_eq!(profile.kind, "profile");
        assert_eq!(profile.backend, "claude-code");
    }

    #[test]
    fn default_agent_access_prefers_database_profile_over_same_named_toml_profile() {
        let cwd = tempdir("db-vs-toml-collision");
        fs::write(
            cwd.join(".ralphus.toml"),
            "[agent.profiles.shared-name]\nbackend = \"codex\"\n",
        )
        .expect("write project config");
        let store = Store::open_in_memory().expect("open store");
        store
            .upsert_agent_profile("shared-name", "claude-code", None, None, Vec::new(), None)
            .expect("save db profile");
        let agents = DefaultAgentAccess
            .available_agents(&UserContext::default(), &cwd, &store)
            .expect("agents");
        let matches: Vec<&AvailableAgent> =
            agents.iter().filter(|a| a.id == "shared-name").collect();
        assert_eq!(matches.len(), 1, "no duplicate entry for the collision");
        assert_eq!(matches[0].backend, "claude-code", "the DB profile wins");
    }
}
