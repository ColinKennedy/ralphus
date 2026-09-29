//! Database-backed agent profiles + built-in-backend command overrides
//! (RAL-473).
//!
//! Replaces `config.toml`/`.ralphus.toml` agent-profile definitions with
//! admin-editable rows in the daemon's own database. Profiles are
//! global-only -- there is no project-local layering the way TOML profiles
//! used to merge (config home -> `$RALPHUS_CONFIGURATION_PATH` -> nearest
//! project `.ralphus.toml`); see `agent_profiles.rs`'s module doc comment
//! for how DB profiles, legacy TOML profiles, and builtin backends are
//! layered at resolution time.
//!
//! A profile picks a backend (one of `agent_profiles::PROFILE_BACKENDS`) and
//! optionally a default model, plus an ordered [`AgentEnvEntry`] table (see
//! `agent_profile_env`). `agent_backend_commands` is a *separate*,
//! backend-keyed table -- not part of any profile -- because a built-in
//! backend's invoked command is a global setting shared by every profile
//! that selects that backend (interview Q4): editing it has a blast radius
//! of every such profile, which is why `list_agent_profiles_for_backend`
//! exists (for the admin UI to show that blast radius before saving).

use std::collections::BTreeSet;

use rusqlite::OptionalExtension as _;
use serde::Serialize;

use crate::agent_profile_env::{AgentEnvEntry, LinkCycle, detect_link_cycle};
use crate::agent_profiles::{PROFILE_BACKENDS, RAW_BACKEND, is_native_backend};
use crate::store::{Result as StoreResult, Store, StoreError, now_ms};
use ralphus_core::schema::RESERVED_AGENT_NAMES;

/// A stored agent profile.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct AgentProfileView {
    pub name: String,
    pub backend: String,
    /// Only ever set for the `raw` backend -- every other backend's invoked
    /// command is a global [`AgentBackendCommandView`] override shared by
    /// every profile that selects it (interview Q4). `raw` has no default to
    /// override, so it keeps a per-profile executable instead.
    pub executable: Option<String>,
    pub model: Option<String>,
    pub env: Vec<AgentEnvEntry>,
    /// RAL-516: override of whether this profile's agent can emit
    /// thinking/reasoning output the Live View's "Show Thinking" control can
    /// fold. `None` inherits `ralphus_core::schema::agent_supports_thinking`
    /// for [`Self::backend`]; `Some(_)` is an explicit override.
    pub thinking_capable: Option<bool>,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
}

impl AgentProfileView {
    /// The effective answer to "can a Live View pane running this profile
    /// show a Show Thinking control" -- [`Self::thinking_capable`] if set,
    /// else the backend's own declared default (RAL-516).
    #[must_use]
    pub fn effective_thinking_capable(&self) -> bool {
        self.thinking_capable
            .unwrap_or_else(|| ralphus_core::schema::agent_supports_thinking(&self.backend))
    }
}

/// Stored entities that name an agent profile, gathered so a delete can warn
/// an admin about the blast radius first (interview Q9). Running invocations
/// are unaffected by a delete either way -- this only reports what is
/// *stored*, for the admin's own judgment call before confirming.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct AgentProfileReferences {
    /// Squads with at least one cell/task/proof naming this profile.
    pub squad_ids: Vec<String>,
    /// Reviews (guardians) naming this profile as one of their agent roles.
    pub guardian_ids: Vec<String>,
}

impl AgentProfileReferences {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.squad_ids.is_empty() && self.guardian_ids.is_empty()
    }
}

/// A stored built-in-backend command override.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct AgentBackendCommandView {
    pub backend: String,
    pub command: String,
    pub updated_at_ms: i64,
}

/// Failure modes specific to saving a profile -- distinct from
/// [`StoreError`] so callers (the server API in particular) can report a
/// `Cycle`'s offending keys without parsing a formatted string back out of
/// `StoreError::InvalidTransition`.
#[derive(Debug)]
pub enum AgentProfileSaveError {
    /// The profile's `env` table's `Link` entries form a cycle; must be
    /// fixed client-side before the save can be retried.
    Cycle(LinkCycle),
    /// `name` collides with a reserved built-in backend name/alias.
    ReservedName(String),
    /// `backend` is not one of [`PROFILE_BACKENDS`].
    UnknownBackend(String),
    /// `backend` is native (`claude`/`anthropic`/`ollama`) but an
    /// `executable` was set -- native backends call an SDK/API directly and
    /// never take one.
    ExecutableNotAllowedForNativeBackend(String),
    /// `backend` is `claude-code`/`codex`/`pi` but an `executable` was set --
    /// that backend's command is now a global [`AgentBackendCommandView`]
    /// override, not a per-profile field (interview Q4).
    ExecutableNotAllowedForOverridableBackend(String),
    /// `backend` is `raw` but no `executable` was set -- `raw` has no
    /// default command to fall back to.
    ExecutableRequiredForRawBackend,
    Store(StoreError),
}

impl std::fmt::Display for AgentProfileSaveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Cycle(c) => write!(f, "environment Link cycle: {c}"),
            Self::ReservedName(name) => write!(
                f,
                "agent profile name {name:?} collides with a reserved built-in backend name"
            ),
            Self::UnknownBackend(backend) => write!(
                f,
                "unknown backend {backend:?}; expected one of {}",
                PROFILE_BACKENDS.join(", ")
            ),
            Self::ExecutableNotAllowedForNativeBackend(backend) => write!(
                f,
                "backend {backend:?} is native and never takes an executable"
            ),
            Self::ExecutableNotAllowedForOverridableBackend(backend) => write!(
                f,
                "backend {backend:?}'s command is a global backend-command override, not a \
                 per-profile executable -- edit its `agent_backend_commands` entry instead"
            ),
            Self::ExecutableRequiredForRawBackend => {
                write!(f, "the \"raw\" backend requires an executable")
            }
            Self::Store(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for AgentProfileSaveError {}

impl From<StoreError> for AgentProfileSaveError {
    fn from(e: StoreError) -> Self {
        Self::Store(e)
    }
}

impl From<rusqlite::Error> for AgentProfileSaveError {
    fn from(e: rusqlite::Error) -> Self {
        Self::Store(StoreError::Sqlite(e))
    }
}

#[allow(clippy::too_many_arguments)]
fn row_to_profile(
    name: String,
    backend: String,
    executable: Option<String>,
    model: Option<String>,
    env_json: String,
    thinking_capable: Option<bool>,
    created_at_ms: i64,
    updated_at_ms: i64,
) -> AgentProfileView {
    let env: Vec<AgentEnvEntry> = serde_json::from_str(&env_json).unwrap_or_default();
    AgentProfileView {
        name,
        backend,
        executable,
        model,
        env,
        thinking_capable,
        created_at_ms,
        updated_at_ms,
    }
}

/// Validates `name`/`backend`/`executable` before a profile is persisted --
/// the same rules `agent_profiles::parse_profile_file` enforces for a TOML
/// profile, since a DB profile must not be able to do anything a TOML one
/// couldn't (reserved-name collision, unknown backend, an executable on a
/// backend that doesn't take one).
fn validate_profile_fields(
    name: &str,
    backend: &str,
    executable: Option<&str>,
) -> std::result::Result<(), AgentProfileSaveError> {
    if RESERVED_AGENT_NAMES.contains(&name) {
        return Err(AgentProfileSaveError::ReservedName(name.to_string()));
    }
    if !PROFILE_BACKENDS.contains(&backend) {
        return Err(AgentProfileSaveError::UnknownBackend(backend.to_string()));
    }
    if is_native_backend(backend) && executable.is_some() {
        return Err(AgentProfileSaveError::ExecutableNotAllowedForNativeBackend(
            backend.to_string(),
        ));
    }
    if backend == RAW_BACKEND && executable.is_none() {
        return Err(AgentProfileSaveError::ExecutableRequiredForRawBackend);
    }
    if backend != RAW_BACKEND && !is_native_backend(backend) && executable.is_some() {
        return Err(
            AgentProfileSaveError::ExecutableNotAllowedForOverridableBackend(backend.to_string()),
        );
    }
    Ok(())
}

/// [`Store::list_agent_profiles`] for a caller that only holds a raw
/// `&Connection` -- e.g. `Store::build_squad_view`'s row-building helpers,
/// which batch-fetch every profile once per view build (mirroring
/// `reviews_by_branch`/`triage_types_by_cell`'s own prefetch-map pattern) so
/// attaching RAL-516's per-row `thinking_capable` never costs an extra query
/// per cell/proof.
///
/// # Errors
/// Propagates any SQLite failure.
pub fn list_agent_profiles_conn(conn: &rusqlite::Connection) -> StoreResult<Vec<AgentProfileView>> {
    let mut stmt = conn.prepare(
        "SELECT name, backend, executable, model, env_json, thinking_capable, created_at_ms, updated_at_ms
         FROM agent_profiles ORDER BY name",
    )?;
    let rows = stmt
        .query_map([], |r| {
            Ok(row_to_profile(
                r.get(0)?,
                r.get(1)?,
                r.get(2)?,
                r.get(3)?,
                r.get(4)?,
                r.get(5)?,
                r.get(6)?,
                r.get(7)?,
            ))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(rows)
}

impl Store {
    /// All stored agent profiles, alphabetical by name.
    ///
    /// # Errors
    /// Propagates any SQLite failure.
    pub fn list_agent_profiles(&self) -> StoreResult<Vec<AgentProfileView>> {
        list_agent_profiles_conn(&self.conn)
    }

    /// Every stored profile that currently selects `backend` -- the blast
    /// radius an admin should see before editing that backend's command
    /// override (interview Q4).
    ///
    /// # Errors
    /// Propagates any SQLite failure.
    pub fn list_agent_profiles_for_backend(
        &self,
        backend: &str,
    ) -> StoreResult<Vec<AgentProfileView>> {
        Ok(self
            .list_agent_profiles()?
            .into_iter()
            .filter(|p| p.backend == backend)
            .collect())
    }

    /// A single stored agent profile by name, `None` if it doesn't exist.
    ///
    /// # Errors
    /// Propagates any SQLite failure.
    pub fn get_agent_profile(&self, name: &str) -> StoreResult<Option<AgentProfileView>> {
        self.conn
            .query_row(
                "SELECT name, backend, executable, model, env_json, thinking_capable, created_at_ms, updated_at_ms
                 FROM agent_profiles WHERE name = ?",
                rusqlite::params![name],
                |r| {
                    Ok(row_to_profile(
                        r.get(0)?,
                        r.get(1)?,
                        r.get(2)?,
                        r.get(3)?,
                        r.get(4)?,
                        r.get(5)?,
                        r.get(6)?,
                        r.get(7)?,
                    ))
                },
            )
            .optional()
            .map_err(Into::into)
    }

    /// Create or replace a stored agent profile. Validates `name`/`backend`/
    /// `executable` and rejects a cyclic `env` `Link` graph *before* touching
    /// the database -- see `agent_profile_env::detect_link_cycle`'s doc
    /// comment and [`validate_profile_fields`].
    ///
    /// # Errors
    /// Any [`AgentProfileSaveError`] variant other than `Store` on a
    /// validation failure; otherwise propagates any SQLite failure.
    pub fn upsert_agent_profile(
        &self,
        name: &str,
        backend: &str,
        executable: Option<&str>,
        model: Option<&str>,
        env: Vec<AgentEnvEntry>,
        thinking_capable: Option<bool>,
    ) -> std::result::Result<(), AgentProfileSaveError> {
        validate_profile_fields(name, backend, executable)?;
        if let Some(cycle) = detect_link_cycle(&env) {
            return Err(AgentProfileSaveError::Cycle(cycle));
        }
        let env_json = serde_json::to_string(&env).unwrap_or_else(|_| "[]".to_string());
        let now = now_ms();
        self.conn.execute(
            "INSERT INTO agent_profiles(name, backend, executable, model, env_json, thinking_capable, created_at_ms, updated_at_ms)
             VALUES(?,?,?,?,?,?,?,?)
             ON CONFLICT(name) DO UPDATE SET
                 backend = excluded.backend,
                 executable = excluded.executable,
                 model = excluded.model,
                 env_json = excluded.env_json,
                 thinking_capable = excluded.thinking_capable,
                 updated_at_ms = excluded.updated_at_ms",
            rusqlite::params![name, backend, executable, model, env_json, thinking_capable, now, now],
        )?;
        crate::rlog!(
            INFO,
            "ralphus [store] agent profile {name:?} saved (backend={backend:?})"
        );
        let _ = self.cartographer_log(crate::cartographer::CartographerEntry {
            level: crate::logging::LogLevel::INFO,
            source: "store",
            message: "agent profile saved",
            scope: Some("agent-profile"),
            squad_id: None,
            guardian_id: None,
            cell_id: None,
            task: None,
            log_path: None,
            payload: serde_json::json!({ "name": name, "backend": backend }),
            admin_only: true,
        });
        Ok(())
    }

    /// Stored squads and reviews that name `name` as their agent -- the
    /// blast-radius report a delete/rename must show before requiring
    /// explicit confirmation (interview Q9). Running invocations already in
    /// flight are unaffected either way; this only reports what is
    /// *durably stored*.
    ///
    /// # Errors
    /// Propagates any SQLite failure.
    pub fn agent_profile_references(&self, name: &str) -> StoreResult<AgentProfileReferences> {
        let mut squad_ids = BTreeSet::new();
        let mut stmt = self.conn.prepare(
            "SELECT squad_id FROM cells WHERE agent = ?
             UNION SELECT squad_id FROM tasks WHERE agent = ?
             UNION SELECT squad_id FROM proofs WHERE agent = ?",
        )?;
        let rows = stmt.query_map(rusqlite::params![name, name, name], |r| {
            r.get::<_, String>(0)
        })?;
        for row in rows {
            squad_ids.insert(row?);
        }

        let mut stmt = self.conn.prepare(
            "SELECT id FROM guardians
             WHERE resolver_agent = ? OR summary_agent = ? OR manual_commands_agent = ? OR chat_agent = ?",
        )?;
        let rows = stmt.query_map(rusqlite::params![name, name, name, name], |r| {
            r.get::<_, String>(0)
        })?;
        let mut guardian_ids = Vec::new();
        for row in rows {
            guardian_ids.push(row?);
        }

        Ok(AgentProfileReferences {
            squad_ids: squad_ids.into_iter().collect(),
            guardian_ids,
        })
    }

    /// Remove a stored agent profile. Returns `false` if no such profile
    /// existed. Callers that must warn about references (stored cells or
    /// reviews naming this profile, interview Q9) should check those before
    /// calling this -- this method itself does not look for referrers.
    ///
    /// # Errors
    /// Propagates any SQLite failure.
    pub fn delete_agent_profile(&self, name: &str) -> StoreResult<bool> {
        let n = self.conn.execute(
            "DELETE FROM agent_profiles WHERE name = ?",
            rusqlite::params![name],
        )?;
        if n > 0 {
            crate::rlog!(INFO, "ralphus [store] agent profile {name:?} deleted");
            let _ = self.cartographer_log(crate::cartographer::CartographerEntry {
                level: crate::logging::LogLevel::INFO,
                source: "store",
                message: "agent profile deleted",
                scope: Some("agent-profile"),
                squad_id: None,
                guardian_id: None,
                cell_id: None,
                task: None,
                log_path: None,
                payload: serde_json::json!({ "name": name }),
                admin_only: true,
            });
        }
        Ok(n > 0)
    }

    /// The stored command override for a built-in backend (`claude-code`,
    /// `codex`, `pi`, ...), `None` if the backend still uses its compiled-in
    /// default.
    ///
    /// # Errors
    /// Propagates any SQLite failure.
    pub fn get_agent_backend_command(&self, backend: &str) -> StoreResult<Option<String>> {
        self.conn
            .query_row(
                "SELECT command FROM agent_backend_commands WHERE backend = ?",
                rusqlite::params![backend],
                |r| r.get(0),
            )
            .optional()
            .map_err(Into::into)
    }

    /// Every stored built-in-backend command override.
    ///
    /// # Errors
    /// Propagates any SQLite failure.
    pub fn list_agent_backend_commands(&self) -> StoreResult<Vec<AgentBackendCommandView>> {
        let mut stmt = self.conn.prepare(
            "SELECT backend, command, updated_at_ms FROM agent_backend_commands ORDER BY backend",
        )?;
        let rows = stmt
            .query_map([], |r| {
                Ok(AgentBackendCommandView {
                    backend: r.get(0)?,
                    command: r.get(1)?,
                    updated_at_ms: r.get(2)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Set (or replace) a built-in backend's command override. `command` may
    /// be a compound/wrapper command line (e.g. `some-complex subcommand --
    /// claude`) -- it is stored and later launched exactly as today's
    /// `RALPHUS_*_COMMAND` env-var override is, via the existing
    /// argv/compound-command parser (interview Q5); no new parsing is added
    /// here.
    ///
    /// # Errors
    /// Propagates any SQLite failure.
    pub fn set_agent_backend_command(&self, backend: &str, command: &str) -> StoreResult<()> {
        let now = now_ms();
        self.conn.execute(
            "INSERT INTO agent_backend_commands(backend, command, updated_at_ms) VALUES(?,?,?)
             ON CONFLICT(backend) DO UPDATE SET command = excluded.command, updated_at_ms = excluded.updated_at_ms",
            rusqlite::params![backend, command, now],
        )?;
        crate::rlog!(
            INFO,
            "ralphus [store] agent backend {backend:?} command override set"
        );
        let _ = self.cartographer_log(crate::cartographer::CartographerEntry {
            level: crate::logging::LogLevel::INFO,
            source: "store",
            message: "agent backend command override set",
            scope: Some("agent-profile"),
            squad_id: None,
            guardian_id: None,
            cell_id: None,
            task: None,
            log_path: None,
            payload: serde_json::json!({ "backend": backend, "command": command }),
            admin_only: true,
        });
        Ok(())
    }

    /// Remove a built-in backend's command override, reverting it to its
    /// compiled-in default. Returns `false` if no override was stored.
    ///
    /// # Errors
    /// Propagates any SQLite failure.
    pub fn reset_agent_backend_command(&self, backend: &str) -> StoreResult<bool> {
        let n = self.conn.execute(
            "DELETE FROM agent_backend_commands WHERE backend = ?",
            rusqlite::params![backend],
        )?;
        if n > 0 {
            crate::rlog!(
                INFO,
                "ralphus [store] agent backend {backend:?} command override reset to default"
            );
            let _ = self.cartographer_log(crate::cartographer::CartographerEntry {
                level: crate::logging::LogLevel::INFO,
                source: "store",
                message: "agent backend command override reset",
                scope: Some("agent-profile"),
                squad_id: None,
                guardian_id: None,
                cell_id: None,
                task: None,
                log_path: None,
                payload: serde_json::json!({ "backend": backend }),
                admin_only: true,
            });
        }
        Ok(n > 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent_profile_env::AgentEnvKind;

    fn set(key: &str, value: &str) -> AgentEnvEntry {
        AgentEnvEntry {
            key: key.to_string(),
            kind: AgentEnvKind::Set,
            value: value.to_string(),
        }
    }
    fn link(key: &str, target: &str) -> AgentEnvEntry {
        AgentEnvEntry {
            key: key.to_string(),
            kind: AgentEnvKind::Link,
            value: target.to_string(),
        }
    }

    #[test]
    fn upsert_then_get_round_trips() {
        let store = Store::open_in_memory().unwrap();
        store
            .upsert_agent_profile(
                "my-profile",
                "claude-code",
                None,
                Some("claude-sonnet-5"),
                vec![set("A", "1"), link("B", "A")],
                None,
            )
            .unwrap();
        let profile = store.get_agent_profile("my-profile").unwrap().unwrap();
        assert_eq!(profile.backend, "claude-code");
        assert_eq!(profile.executable, None);
        assert_eq!(profile.model.as_deref(), Some("claude-sonnet-5"));
        assert_eq!(profile.env, vec![set("A", "1"), link("B", "A")]);
        assert_eq!(profile.thinking_capable, None);
    }

    #[test]
    fn upsert_round_trips_an_explicit_thinking_capable_override() {
        let store = Store::open_in_memory().unwrap();
        store
            .upsert_agent_profile(
                "thinking-on",
                "raw",
                Some("/usr/bin/my-tool"),
                None,
                vec![],
                Some(true),
            )
            .unwrap();
        let profile = store.get_agent_profile("thinking-on").unwrap().unwrap();
        assert_eq!(profile.thinking_capable, Some(true));
        assert!(profile.effective_thinking_capable());

        store
            .upsert_agent_profile("thinking-off", "pi", None, None, vec![], Some(false))
            .unwrap();
        let profile = store.get_agent_profile("thinking-off").unwrap().unwrap();
        assert_eq!(profile.thinking_capable, Some(false));
        assert!(!profile.effective_thinking_capable());
    }

    #[test]
    fn thinking_capable_defaults_to_none_and_inherits_the_backend_default() {
        let store = Store::open_in_memory().unwrap();
        store
            .upsert_agent_profile("p", "pi", None, None, vec![], None)
            .unwrap();
        let profile = store.get_agent_profile("p").unwrap().unwrap();
        assert_eq!(profile.thinking_capable, None);
        assert!(profile.effective_thinking_capable());

        store
            .upsert_agent_profile("q", "claude-code", None, None, vec![], None)
            .unwrap();
        let profile = store.get_agent_profile("q").unwrap().unwrap();
        assert_eq!(profile.thinking_capable, None);
        assert!(!profile.effective_thinking_capable());
    }

    #[test]
    fn upsert_rejects_a_cyclic_env_and_does_not_persist() {
        let store = Store::open_in_memory().unwrap();
        let err = store
            .upsert_agent_profile(
                "cyclic",
                "pi",
                None,
                None,
                vec![link("A", "B"), link("B", "A")],
                None,
            )
            .unwrap_err();
        let AgentProfileSaveError::Cycle(cycle) = err else {
            panic!("expected a Cycle error, got {err:?}");
        };
        assert!(cycle.keys.contains(&"A".to_string()));
        assert!(cycle.keys.contains(&"B".to_string()));
        assert!(store.get_agent_profile("cyclic").unwrap().is_none());
    }

    #[test]
    fn upsert_overwrites_an_existing_profile_by_name() {
        let store = Store::open_in_memory().unwrap();
        store
            .upsert_agent_profile("p", "pi", None, None, vec![set("A", "1")], None)
            .unwrap();
        store
            .upsert_agent_profile("p", "codex", None, Some("gpt-6"), vec![set("A", "2")], None)
            .unwrap();
        let profile = store.get_agent_profile("p").unwrap().unwrap();
        assert_eq!(profile.backend, "codex");
        assert_eq!(profile.model.as_deref(), Some("gpt-6"));
        assert_eq!(profile.env, vec![set("A", "2")]);
    }

    #[test]
    fn upsert_rejects_a_reserved_name() {
        let store = Store::open_in_memory().unwrap();
        let err = store
            .upsert_agent_profile("claude-code", "pi", None, None, vec![], None)
            .unwrap_err();
        assert!(matches!(err, AgentProfileSaveError::ReservedName(_)));
    }

    #[test]
    fn upsert_rejects_an_unknown_backend() {
        let store = Store::open_in_memory().unwrap();
        let err = store
            .upsert_agent_profile("p", "not-a-backend", None, None, vec![], None)
            .unwrap_err();
        assert!(matches!(err, AgentProfileSaveError::UnknownBackend(_)));
    }

    #[test]
    fn upsert_rejects_executable_on_a_native_backend() {
        let store = Store::open_in_memory().unwrap();
        let err = store
            .upsert_agent_profile("p", "claude", Some("/usr/bin/claude"), None, vec![], None)
            .unwrap_err();
        assert!(matches!(
            err,
            AgentProfileSaveError::ExecutableNotAllowedForNativeBackend(_)
        ));
    }

    #[test]
    fn upsert_rejects_executable_on_an_overridable_backend() {
        let store = Store::open_in_memory().unwrap();
        let err = store
            .upsert_agent_profile(
                "p",
                "claude-code",
                Some("/usr/bin/claude"),
                None,
                vec![],
                None,
            )
            .unwrap_err();
        assert!(matches!(
            err,
            AgentProfileSaveError::ExecutableNotAllowedForOverridableBackend(_)
        ));
    }

    #[test]
    fn upsert_requires_executable_on_the_raw_backend() {
        let store = Store::open_in_memory().unwrap();
        let err = store
            .upsert_agent_profile("p", "raw", None, None, vec![], None)
            .unwrap_err();
        assert!(matches!(
            err,
            AgentProfileSaveError::ExecutableRequiredForRawBackend
        ));
    }

    #[test]
    fn upsert_accepts_an_executable_on_the_raw_backend() {
        let store = Store::open_in_memory().unwrap();
        store
            .upsert_agent_profile("p", "raw", Some("/usr/bin/my-tool"), None, vec![], None)
            .unwrap();
        let profile = store.get_agent_profile("p").unwrap().unwrap();
        assert_eq!(profile.executable.as_deref(), Some("/usr/bin/my-tool"));
    }

    #[test]
    fn list_is_alphabetical_and_delete_reports_whether_a_row_existed() {
        let store = Store::open_in_memory().unwrap();
        store
            .upsert_agent_profile("zeta", "pi", None, None, vec![], None)
            .unwrap();
        store
            .upsert_agent_profile("alpha", "pi", None, None, vec![], None)
            .unwrap();
        let names: Vec<String> = store
            .list_agent_profiles()
            .unwrap()
            .into_iter()
            .map(|p| p.name)
            .collect();
        assert_eq!(names, vec!["alpha".to_string(), "zeta".to_string()]);
        assert!(store.delete_agent_profile("alpha").unwrap());
        assert!(!store.delete_agent_profile("alpha").unwrap());
    }

    #[test]
    fn list_for_backend_filters_by_backend() {
        let store = Store::open_in_memory().unwrap();
        store
            .upsert_agent_profile("a", "claude-code", None, None, vec![], None)
            .unwrap();
        store
            .upsert_agent_profile("b", "pi", None, None, vec![], None)
            .unwrap();
        store
            .upsert_agent_profile("c", "claude-code", None, None, vec![], None)
            .unwrap();
        let names: Vec<String> = store
            .list_agent_profiles_for_backend("claude-code")
            .unwrap()
            .into_iter()
            .map(|p| p.name)
            .collect();
        assert_eq!(names, vec!["a".to_string(), "c".to_string()]);
    }

    #[test]
    fn references_reports_no_referrers_for_an_unused_profile() {
        let store = Store::open_in_memory().unwrap();
        store
            .upsert_agent_profile("unused", "pi", None, None, vec![], None)
            .unwrap();
        let refs = store.agent_profile_references("unused").unwrap();
        assert!(refs.is_empty());
    }

    #[test]
    fn references_finds_cells_tasks_and_proofs_by_squad() {
        let store = Store::open_in_memory().unwrap();
        store
            .upsert_agent_profile("my-agent", "pi", None, None, vec![], None)
            .unwrap();
        store
            .conn
            .execute(
                "INSERT INTO squads(id, state, created_at_ms, updated_at_ms) VALUES('sq1', 'Pending', 0, 0)",
                [],
            )
            .unwrap();
        store
            .conn
            .execute(
                "INSERT INTO cells(squad_id, task_idx, idx, sid, agent, state) VALUES('sq1', 0, 0, 's0', 'my-agent', 'Pending')",
                [],
            )
            .unwrap();
        let refs = store.agent_profile_references("my-agent").unwrap();
        assert_eq!(refs.squad_ids, vec!["sq1".to_string()]);
        assert!(refs.guardian_ids.is_empty());
    }

    #[test]
    fn references_finds_guardians_by_any_agent_role_column() {
        let store = Store::open_in_memory().unwrap();
        store
            .upsert_agent_profile("resolver-agent", "pi", None, None, vec![], None)
            .unwrap();
        store
            .conn
            .execute(
                "INSERT INTO guardians(id, name, base_branch, git_root, status, resolver_agent, created_at_ms, updated_at_ms)
                 VALUES('g1', 'g', 'main', '/tmp/repo', 'Collecting', 'resolver-agent', 0, 0)",
                [],
            )
            .unwrap();
        let refs = store.agent_profile_references("resolver-agent").unwrap();
        assert_eq!(refs.guardian_ids, vec!["g1".to_string()]);
        assert!(refs.squad_ids.is_empty());
    }

    #[test]
    fn backend_command_defaults_to_none_then_set_get_reset() {
        let store = Store::open_in_memory().unwrap();
        assert!(
            store
                .get_agent_backend_command("claude-code")
                .unwrap()
                .is_none()
        );
        store
            .set_agent_backend_command("claude-code", "some-complex subcommand -- claude")
            .unwrap();
        assert_eq!(
            store.get_agent_backend_command("claude-code").unwrap(),
            Some("some-complex subcommand -- claude".to_string())
        );
        assert!(store.reset_agent_backend_command("claude-code").unwrap());
        assert!(
            store
                .get_agent_backend_command("claude-code")
                .unwrap()
                .is_none()
        );
        assert!(!store.reset_agent_backend_command("claude-code").unwrap());
    }

    #[test]
    fn set_backend_command_overwrites_on_conflict() {
        let store = Store::open_in_memory().unwrap();
        store
            .set_agent_backend_command("codex", "codex-v1")
            .unwrap();
        store
            .set_agent_backend_command("codex", "codex-v2")
            .unwrap();
        assert_eq!(
            store.get_agent_backend_command("codex").unwrap(),
            Some("codex-v2".to_string())
        );
        let all = store.list_agent_backend_commands().unwrap();
        assert_eq!(all.len(), 1);
    }
}
