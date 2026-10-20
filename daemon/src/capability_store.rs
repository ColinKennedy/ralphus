//! RAL-695: the catalog of MCP servers and skills agents may use, and which of
//! them each agent profile has enabled.
//!
//! The catalog is ralphus-owned (`capability_items`): a CLI's own config is
//! only ever a source for the import action, never read at spawn time. An
//! agent profile -- built-in backend names included -- enables catalog items
//! one by one (`agent_capability_enabled`); nothing is enabled by default. A
//! spawned agent receives exactly the enabled items and nothing from the
//! user's personal CLI settings.
//!
//! MCP env and header values are secret-shaped. They use the same Set/Link
//! model as profile env (`agent_profile_env`); API views never carry a `Set`
//! value (see [`mask_entries`]), and every resolved value is registered with
//! `crate::redact` before it can reach a spec, a transcript, Cartographer or
//! the board.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use base64::Engine as _;
use rusqlite::OptionalExtension as _;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::agent_profile_env::{AgentEnvEntry, AgentEnvKind, detect_link_cycle, resolve_agent_env};
use crate::store::{Result as StoreResult, Store, now_ms};
use ralphus_core::schema::RESERVED_AGENT_NAMES;

/// The catalog name reserved for the ralphus MCP server itself.
pub const RALPHUS_MCP_NAME: &str = "ralphus";

/// Largest skill folder the daemon will read into a spec.
const MAX_SKILL_BYTES: u64 = 8 * 1024 * 1024;
const MAX_SKILL_FILES: usize = 400;

/// What a catalog item is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapabilityKind {
    Mcp,
    Skill,
}

impl CapabilityKind {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Mcp => "mcp",
            Self::Skill => "skill",
        }
    }

    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "mcp" => Some(Self::Mcp),
            "skill" => Some(Self::Skill),
            _ => None,
        }
    }
}

/// An MCP server's launch description.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct McpConfig {
    #[serde(default)]
    pub command: Option<String>,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: Vec<AgentEnvEntry>,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub headers: Vec<AgentEnvEntry>,
}

/// A skill folder on the daemon host.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct SkillConfig {
    pub path: String,
}

/// One catalog entry as the API shows it. `Set` env/header values are masked.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CapabilityItemView {
    pub kind: CapabilityKind,
    pub name: String,
    /// Where the entry came from: `manual`, `import:<source>` or `builtin`.
    pub source: String,
    pub mcp: Option<McpConfig>,
    pub skill: Option<SkillConfig>,
    /// `untested`, `ok`, `failing` (MCP) or `found`, `missing` (skill).
    pub status: String,
    pub status_detail: Option<String>,
    pub tool_count: Option<i64>,
    pub checked_at_ms: Option<i64>,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
}

/// A catalog entry with its real values, for probing and resolution.
#[derive(Debug, Clone, PartialEq)]
pub struct CapabilityItem {
    pub kind: CapabilityKind,
    pub name: String,
    pub source: String,
    pub mcp: Option<McpConfig>,
    pub skill: Option<SkillConfig>,
}

/// How the capabilities reach a custom command profile's command.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CapabilityDelivery {
    /// `backend_default`, `extra_args` or `none`.
    pub mode: String,
    #[serde(default)]
    pub args: Vec<String>,
}

impl Default for CapabilityDelivery {
    fn default() -> Self {
        Self {
            mode: "backend_default".to_string(),
            args: Vec::new(),
        }
    }
}

/// A rejected catalog write.
#[derive(Debug)]
pub enum CapabilityError {
    Invalid(String),
    NotFound(String),
    Store(crate::store::StoreError),
}

impl std::fmt::Display for CapabilityError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Invalid(m) | Self::NotFound(m) => f.write_str(m),
            Self::Store(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for CapabilityError {}

impl From<rusqlite::Error> for CapabilityError {
    fn from(e: rusqlite::Error) -> Self {
        Self::Store(e.into())
    }
}

impl From<crate::store::StoreError> for CapabilityError {
    fn from(e: crate::store::StoreError) -> Self {
        Self::Store(e)
    }
}

/// Replace every `Set` value with the redaction placeholder so a view never
/// carries a secret. A `Link` value is a variable *name*, not a secret.
#[must_use]
pub fn mask_entries(entries: &[AgentEnvEntry]) -> Vec<AgentEnvEntry> {
    entries
        .iter()
        .map(|e| AgentEnvEntry {
            key: e.key.clone(),
            kind: e.kind,
            value: if e.kind == AgentEnvKind::Set {
                crate::redact::REDACTED.to_string()
            } else {
                e.value.clone()
            },
        })
        .collect()
}

/// Entries an edit submitted, with any masked `Set` value replaced by the
/// stored value of the same key.
fn unmask_entries(incoming: Vec<AgentEnvEntry>, stored: &[AgentEnvEntry]) -> Vec<AgentEnvEntry> {
    incoming
        .into_iter()
        .map(|mut e| {
            if e.kind == AgentEnvKind::Set && e.value == crate::redact::REDACTED {
                let old = stored
                    .iter()
                    .find(|s| s.key == e.key && s.kind == AgentEnvKind::Set);
                if let Some(old) = old {
                    e.value.clone_from(&old.value);
                }
            }
            e
        })
        .collect()
}

fn validate_name(name: &str) -> Result<(), CapabilityError> {
    let ok = !name.is_empty()
        && name.len() <= 64
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        && !name.starts_with('.');
    if ok {
        Ok(())
    } else {
        Err(CapabilityError::Invalid(
            "name must be 1-64 letters, digits, '-', '_' or '.' and not start with '.'".to_string(),
        ))
    }
}

fn validate_mcp(config: &McpConfig) -> Result<(), CapabilityError> {
    let has_command = config
        .command
        .as_deref()
        .is_some_and(|c| !c.trim().is_empty());
    let has_url = config.url.as_deref().is_some_and(|u| !u.trim().is_empty());
    if has_command == has_url {
        return Err(CapabilityError::Invalid(
            "an MCP server needs exactly one of 'command' (stdio) or 'url' (http)".to_string(),
        ));
    }
    for (label, entries) in [("env", &config.env), ("headers", &config.headers)] {
        if entries.iter().any(|e| e.key.trim().is_empty()) {
            return Err(CapabilityError::Invalid(format!(
                "{label} keys must not be empty"
            )));
        }
        if let Some(cycle) = detect_link_cycle(entries) {
            return Err(CapabilityError::Invalid(format!(
                "{label} has a link cycle: {cycle}"
            )));
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn row_to_item(
    kind: String,
    name: String,
    source: String,
    config_json: &str,
    status: String,
    detail: Option<String>,
    tool_count: Option<i64>,
    checked: Option<i64>,
    created: i64,
    updated: i64,
) -> CapabilityItemView {
    let kind = CapabilityKind::parse(&kind).unwrap_or(CapabilityKind::Mcp);
    let (mcp, skill) = match kind {
        CapabilityKind::Mcp => (
            serde_json::from_str::<McpConfig>(config_json)
                .ok()
                .map(|mut c| {
                    c.env = mask_entries(&c.env);
                    c.headers = mask_entries(&c.headers);
                    c
                }),
            None,
        ),
        CapabilityKind::Skill => (None, serde_json::from_str::<SkillConfig>(config_json).ok()),
    };
    CapabilityItemView {
        kind,
        name,
        source,
        mcp,
        skill,
        status,
        status_detail: detail,
        tool_count,
        checked_at_ms: checked,
        created_at_ms: created,
        updated_at_ms: updated,
    }
}

/// The ralphus MCP server's catalog entry. `ralphus-mcp` finds the daemon URL
/// and token itself (`docs/mcp-server.md`), so the entry carries no secret.
#[must_use]
pub fn ralphus_mcp_item() -> CapabilityItem {
    CapabilityItem {
        kind: CapabilityKind::Mcp,
        name: RALPHUS_MCP_NAME.to_string(),
        source: "builtin".to_string(),
        mcp: Some(McpConfig {
            command: Some("ralphus-mcp".to_string()),
            ..McpConfig::default()
        }),
        skill: None,
    }
}

/// What one profile has enabled, ready to resolve into the runner spec's
/// `capabilities` object.
#[derive(Debug, Clone)]
pub struct CapabilityPlan {
    pub profile: String,
    pub items: Vec<CapabilityItem>,
    pub delivery: CapabilityDelivery,
}

impl CapabilityPlan {
    /// The runner-spec `capabilities` object. Always an object: an empty one
    /// tells the runner to deliver nothing and to load nothing from personal
    /// settings.
    ///
    /// Every resolved env/header value is registered for redaction here, so a
    /// failure message or transcript line that echoes one is scrubbed. Skill
    /// folders are read from the daemon host and travel as bytes so a remote
    /// machine can provision from the spec alone. A skill that cannot be read
    /// is skipped and recorded through `store`.
    #[must_use]
    pub fn resolve(&self, store: &Store) -> Value {
        let mut mcp_servers = Vec::new();
        let mut skills = Vec::new();
        let mut secrets = Vec::new();
        for item in &self.items {
            let name = &item.name;
            match item.kind {
                CapabilityKind::Mcp => {
                    let Some(config) = &item.mcp else { continue };
                    let env = resolve_agent_env(&config.env, |k| std::env::var(k).ok());
                    let headers = resolve_agent_env(&config.headers, |k| std::env::var(k).ok());
                    secrets.extend(env.values().cloned());
                    secrets.extend(headers.values().cloned());
                    mcp_servers.push(json!({
                        "name": name,
                        "command": config.command,
                        "args": config.args,
                        "env": env,
                        "url": config.url,
                        "headers": headers,
                    }));
                }
                CapabilityKind::Skill => {
                    let Some(config) = &item.skill else { continue };
                    match read_skill_files(Path::new(&config.path)) {
                        Ok(files) => skills.push(json!({"name": name, "files": files})),
                        Err(reason) => store.log_capability_event(
                            "skill skipped",
                            json!({"skill": name, "profile": self.profile, "reason": reason}),
                        ),
                    }
                }
            }
        }
        crate::redact::register_all(secrets);
        json!({
            "mcp_servers": mcp_servers,
            "skills": skills,
            "delivery": {"mode": self.delivery.mode, "args": self.delivery.args},
        })
    }
}

impl Store {
    /// The catalog, with the ralphus MCP entry present only when
    /// `allow_ralphus_mcp` is set.
    ///
    /// # Errors
    /// Propagates any SQLite failure.
    pub fn list_capability_items(
        &self,
        allow_ralphus_mcp: bool,
    ) -> StoreResult<Vec<CapabilityItemView>> {
        let mut stmt = self.conn.prepare(
            "SELECT kind, name, source, config_json, status, status_detail, tool_count,
                    checked_at_ms, created_at_ms, updated_at_ms
             FROM capability_items ORDER BY kind, name",
        )?;
        let mut out = stmt
            .query_map([], |r| {
                Ok(row_to_item(
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    &r.get::<_, String>(3)?,
                    r.get(4)?,
                    r.get(5)?,
                    r.get(6)?,
                    r.get(7)?,
                    r.get(8)?,
                    r.get(9)?,
                ))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        if allow_ralphus_mcp {
            let item = ralphus_mcp_item();
            out.insert(
                0,
                CapabilityItemView {
                    kind: item.kind,
                    name: item.name,
                    source: item.source,
                    mcp: item.mcp,
                    skill: None,
                    status: "untested".to_string(),
                    status_detail: None,
                    tool_count: None,
                    checked_at_ms: None,
                    created_at_ms: 0,
                    updated_at_ms: 0,
                },
            );
        }
        Ok(out)
    }

    /// One catalog entry with its real values (secrets unmasked).
    ///
    /// # Errors
    /// Propagates any SQLite failure.
    pub fn get_capability_item(
        &self,
        kind: CapabilityKind,
        name: &str,
        allow_ralphus_mcp: bool,
    ) -> StoreResult<Option<CapabilityItem>> {
        if kind == CapabilityKind::Mcp && name == RALPHUS_MCP_NAME {
            return Ok(allow_ralphus_mcp.then(ralphus_mcp_item));
        }
        let row: Option<(String, String)> = self
            .conn
            .query_row(
                "SELECT source, config_json FROM capability_items WHERE kind = ? AND name = ?",
                rusqlite::params![kind.as_str(), name],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        Ok(row.map(|(source, json)| CapabilityItem {
            kind,
            name: name.to_string(),
            source,
            mcp: (kind == CapabilityKind::Mcp)
                .then(|| serde_json::from_str(&json).unwrap_or_default()),
            skill: (kind == CapabilityKind::Skill)
                .then(|| serde_json::from_str(&json).unwrap_or_default()),
        }))
    }

    /// Create or replace a catalog entry. A masked `Set` value in an MCP edit
    /// keeps the stored value for that key.
    ///
    /// # Errors
    /// [`CapabilityError::Invalid`] for a bad name or config; otherwise any
    /// SQLite failure.
    pub fn upsert_capability_item(
        &self,
        kind: CapabilityKind,
        name: &str,
        mcp: Option<McpConfig>,
        skill: Option<SkillConfig>,
        source: &str,
    ) -> Result<(), CapabilityError> {
        validate_name(name)?;
        if kind == CapabilityKind::Mcp && name == RALPHUS_MCP_NAME {
            return Err(CapabilityError::Invalid(format!(
                "'{RALPHUS_MCP_NAME}' is reserved for the ralphus MCP server"
            )));
        }
        let previous = self.get_capability_item(kind, name, false)?;
        let config_json = match kind {
            CapabilityKind::Mcp => {
                let mut config = mcp.ok_or_else(|| {
                    CapabilityError::Invalid("an MCP server needs an 'mcp' config".to_string())
                })?;
                if let Some(old) = previous.as_ref().and_then(|p| p.mcp.as_ref()) {
                    config.env = unmask_entries(config.env, &old.env);
                    config.headers = unmask_entries(config.headers, &old.headers);
                }
                validate_mcp(&config)?;
                serde_json::to_string(&config)
            }
            CapabilityKind::Skill => {
                let config = skill.ok_or_else(|| {
                    CapabilityError::Invalid("a skill needs a 'skill' config".to_string())
                })?;
                if config.path.trim().is_empty() {
                    return Err(CapabilityError::Invalid(
                        "a skill needs a folder 'path'".to_string(),
                    ));
                }
                serde_json::to_string(&config)
            }
        }
        .unwrap_or_else(|_| "{}".to_string());
        let now = now_ms();
        // Editing the config makes an earlier probe result stale.
        self.conn.execute(
            "INSERT INTO capability_items(kind, name, source, config_json, status, created_at_ms, updated_at_ms)
             VALUES(?,?,?,?, 'untested', ?, ?)
             ON CONFLICT(kind, name) DO UPDATE SET
                 source = excluded.source,
                 config_json = excluded.config_json,
                 status = 'untested', status_detail = NULL, tool_count = NULL, checked_at_ms = NULL,
                 updated_at_ms = excluded.updated_at_ms",
            rusqlite::params![kind.as_str(), name, source, config_json, now, now],
        )?;
        self.log_capability_event(
            "capability saved",
            json!({
                "kind": kind.as_str(),
                "name": name,
                "source": source,
                "action": if previous.is_some() { "updated" } else { "created" },
            }),
        );
        Ok(())
    }

    /// Remove a catalog entry and every profile's enablement of it.
    ///
    /// # Errors
    /// Propagates any SQLite failure.
    pub fn delete_capability_item(&self, kind: CapabilityKind, name: &str) -> StoreResult<bool> {
        self.conn.execute(
            "DELETE FROM agent_capability_enabled WHERE kind = ? AND name = ?",
            rusqlite::params![kind.as_str(), name],
        )?;
        let n = self.conn.execute(
            "DELETE FROM capability_items WHERE kind = ? AND name = ?",
            rusqlite::params![kind.as_str(), name],
        )?;
        if n > 0 {
            self.log_capability_event(
                "capability removed",
                json!({"kind": kind.as_str(), "name": name}),
            );
        }
        Ok(n > 0)
    }

    /// Record a probe or folder-check outcome.
    ///
    /// # Errors
    /// Propagates any SQLite failure.
    pub fn set_capability_status(
        &self,
        kind: CapabilityKind,
        name: &str,
        status: &str,
        detail: Option<&str>,
        tool_count: Option<i64>,
    ) -> StoreResult<()> {
        let detail = detail.map(crate::redact::redact_all);
        self.conn.execute(
            "UPDATE capability_items SET status = ?, status_detail = ?, tool_count = ?, checked_at_ms = ?
             WHERE kind = ? AND name = ?",
            rusqlite::params![status, detail, tool_count, now_ms(), kind.as_str(), name],
        )?;
        self.log_capability_event(
            "capability checked",
            json!({
                "kind": kind.as_str(), "name": name, "status": status,
                "tool_count": tool_count, "detail": detail,
            }),
        );
        Ok(())
    }

    /// The catalog items `profile` has enabled, as `(kind, name)`.
    ///
    /// # Errors
    /// Propagates any SQLite failure.
    pub fn list_enabled_capabilities(
        &self,
        profile: &str,
    ) -> StoreResult<Vec<(CapabilityKind, String)>> {
        let mut stmt = self.conn.prepare(
            "SELECT kind, name FROM agent_capability_enabled WHERE profile = ? ORDER BY kind, name",
        )?;
        let rows = stmt
            .query_map(rusqlite::params![profile], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows
            .into_iter()
            .filter_map(|(k, n)| CapabilityKind::parse(&k).map(|k| (k, n)))
            .collect())
    }

    /// Enable or disable one catalog item for `profile` (a built-in backend
    /// name or a stored profile).
    ///
    /// # Errors
    /// [`CapabilityError::NotFound`] for an unknown profile or item.
    pub fn set_capability_enabled(
        &self,
        profile: &str,
        kind: CapabilityKind,
        name: &str,
        enabled: bool,
        allow_ralphus_mcp: bool,
    ) -> Result<(), CapabilityError> {
        self.require_capability_profile(profile)?;
        if enabled {
            if self
                .get_capability_item(kind, name, allow_ralphus_mcp)?
                .is_none()
            {
                return Err(CapabilityError::NotFound(format!(
                    "no {} named {name:?} in the catalog",
                    kind.as_str()
                )));
            }
            self.conn.execute(
                "INSERT OR IGNORE INTO agent_capability_enabled(profile, kind, name) VALUES(?,?,?)",
                rusqlite::params![profile, kind.as_str(), name],
            )?;
        } else {
            self.conn.execute(
                "DELETE FROM agent_capability_enabled WHERE profile = ? AND kind = ? AND name = ?",
                rusqlite::params![profile, kind.as_str(), name],
            )?;
        }
        self.log_capability_event(
            "capability toggled",
            json!({"profile": profile, "kind": kind.as_str(), "name": name, "enabled": enabled}),
        );
        Ok(())
    }

    fn require_capability_profile(&self, profile: &str) -> Result<(), CapabilityError> {
        if RESERVED_AGENT_NAMES.contains(&profile) || self.get_agent_profile(profile)?.is_some() {
            Ok(())
        } else {
            Err(CapabilityError::NotFound(format!(
                "agent profile {profile:?} does not exist"
            )))
        }
    }

    /// The delivery mode for `profile`.
    ///
    /// # Errors
    /// Propagates any SQLite failure.
    pub fn get_capability_delivery(&self, profile: &str) -> StoreResult<CapabilityDelivery> {
        let row: Option<(String, String)> = self
            .conn
            .query_row(
                "SELECT mode, args_json FROM agent_capability_delivery WHERE profile = ?",
                rusqlite::params![profile],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        Ok(
            row.map_or_else(CapabilityDelivery::default, |(mode, args)| {
                CapabilityDelivery {
                    mode,
                    args: serde_json::from_str(&args).unwrap_or_default(),
                }
            }),
        )
    }

    /// Set the delivery mode for a custom command profile.
    ///
    /// # Errors
    /// [`CapabilityError::Invalid`] for an unknown mode or `extra_args`
    /// without arguments; [`CapabilityError::NotFound`] for an unknown profile.
    pub fn set_capability_delivery(
        &self,
        profile: &str,
        delivery: &CapabilityDelivery,
    ) -> Result<(), CapabilityError> {
        self.require_capability_profile(profile)?;
        match delivery.mode.as_str() {
            "backend_default" | "none" => {}
            "extra_args" if !delivery.args.is_empty() => {}
            "extra_args" => {
                return Err(CapabilityError::Invalid(
                    "extra_args delivery needs at least one argument".to_string(),
                ));
            }
            other => {
                return Err(CapabilityError::Invalid(format!(
                    "delivery mode must be backend_default, extra_args or none, not {other:?}"
                )));
            }
        }
        if delivery.mode == "backend_default" {
            self.conn.execute(
                "DELETE FROM agent_capability_delivery WHERE profile = ?",
                rusqlite::params![profile],
            )?;
        } else {
            self.conn.execute(
                "INSERT INTO agent_capability_delivery(profile, mode, args_json) VALUES(?,?,?)
                 ON CONFLICT(profile) DO UPDATE SET mode = excluded.mode, args_json = excluded.args_json",
                rusqlite::params![
                    profile,
                    delivery.mode,
                    serde_json::to_string(&delivery.args).unwrap_or_else(|_| "[]".to_string())
                ],
            )?;
        }
        self.log_capability_event(
            "capability delivery set",
            json!({"profile": profile, "mode": delivery.mode, "args": delivery.args.len()}),
        );
        Ok(())
    }

    /// Drop a deleted profile's enablement and delivery rows.
    pub(crate) fn delete_profile_capabilities(&self, profile: &str) -> StoreResult<()> {
        self.conn.execute(
            "DELETE FROM agent_capability_enabled WHERE profile = ?",
            rusqlite::params![profile],
        )?;
        self.conn.execute(
            "DELETE FROM agent_capability_delivery WHERE profile = ?",
            rusqlite::params![profile],
        )?;
        Ok(())
    }

    /// The enabled catalog items for `profile` with their real values. Cheap
    /// (SQL only); turn it into the wire object with [`CapabilityPlan::resolve`]
    /// *outside* the store lock, because that reads skill folders.
    ///
    /// # Errors
    /// Propagates any SQLite failure.
    pub fn capability_plan(
        &self,
        profile: &str,
        allow_ralphus_mcp: bool,
    ) -> StoreResult<CapabilityPlan> {
        let mut items = Vec::new();
        for (kind, name) in self.list_enabled_capabilities(profile)? {
            if let Some(item) = self.get_capability_item(kind, &name, allow_ralphus_mcp)? {
                items.push(item);
            }
        }
        Ok(CapabilityPlan {
            profile: profile.to_string(),
            items,
            delivery: self.get_capability_delivery(profile)?,
        })
    }

    /// [`Self::capability_plan`] resolved in one step, for callers and tests
    /// that do not mind reading skill folders under the lock.
    ///
    /// # Errors
    /// Propagates any SQLite failure.
    pub fn resolve_capabilities(
        &self,
        profile: &str,
        allow_ralphus_mcp: bool,
    ) -> StoreResult<Value> {
        Ok(self
            .capability_plan(profile, allow_ralphus_mcp)?
            .resolve(self))
    }

    fn log_capability_event(&self, message: &'static str, payload: Value) {
        crate::rlog!(INFO, "ralphus [store] {message}: {payload}");
        // Names, statuses and counts only -- never an env or header value.
        let _ = self.cartographer_log(crate::cartographer::CartographerEntry {
            level: crate::logging::LogLevel::INFO,
            source: "store",
            message,
            scope: Some("capability"),
            squad_id: None,
            guardian_id: None,
            cell_id: None,
            task: None,
            log_path: None,
            payload,
            admin_only: true,
        });
    }
}

/// Every file of a skill folder as `{path, content_base64}`.
///
/// # Errors
/// A message when the folder is missing, has no `SKILL.md`, or is too large.
pub fn read_skill_files(root: &Path) -> Result<Vec<Value>, String> {
    check_skill_folder(root)?;
    let mut files = Vec::new();
    let mut total = 0u64;
    collect_files(root, root, &mut files, &mut total)?;
    Ok(files)
}

/// `Ok` when `root` is a folder holding a `SKILL.md`.
///
/// # Errors
/// A message naming what is missing.
pub fn check_skill_folder(root: &Path) -> Result<(), String> {
    if !root.is_dir() {
        return Err(format!("folder {} does not exist", root.display()));
    }
    if !root.join("SKILL.md").is_file() {
        return Err(format!("{} has no SKILL.md", root.display()));
    }
    Ok(())
}

fn collect_files(
    root: &Path,
    dir: &Path,
    out: &mut Vec<Value>,
    total: &mut u64,
) -> Result<(), String> {
    let mut entries: Vec<PathBuf> = std::fs::read_dir(dir)
        .map_err(|e| format!("cannot read {}: {e}", dir.display()))?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .collect();
    entries.sort();
    for path in entries {
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default();
        if name == ".git" || name == "node_modules" {
            continue;
        }
        if path.is_dir() {
            collect_files(root, &path, out, total)?;
        } else if path.is_file() {
            let bytes =
                std::fs::read(&path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
            *total += bytes.len() as u64;
            if *total > MAX_SKILL_BYTES || out.len() >= MAX_SKILL_FILES {
                return Err(format!(
                    "skill folder {} is larger than the {MAX_SKILL_BYTES}-byte / {MAX_SKILL_FILES}-file limit",
                    root.display()
                ));
            }
            let rel = path
                .strip_prefix(root)
                .unwrap_or(&path)
                .to_string_lossy()
                .replace('\\', "/");
            out.push(json!({
                "path": rel,
                "content_base64": base64::engine::general_purpose::STANDARD.encode(bytes),
            }));
        }
    }
    Ok(())
}

/// A name-keyed map of env entries by key, for tests and the probe.
#[must_use]
pub fn resolved_env(entries: &[AgentEnvEntry]) -> BTreeMap<String, String> {
    resolve_agent_env(entries, |k| std::env::var(k).ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(key: &str, value: &str) -> AgentEnvEntry {
        AgentEnvEntry {
            key: key.to_string(),
            kind: AgentEnvKind::Set,
            value: value.to_string(),
        }
    }

    fn caps_empty(caps: &Value) -> bool {
        caps["mcp_servers"].as_array().is_some_and(Vec::is_empty)
            && caps["skills"].as_array().is_some_and(Vec::is_empty)
    }

    fn stdio(env: Vec<AgentEnvEntry>) -> McpConfig {
        McpConfig {
            command: Some("srv".to_string()),
            args: vec!["--x".to_string()],
            env,
            ..McpConfig::default()
        }
    }

    #[test]
    fn nothing_is_enabled_by_default() {
        let store = Store::open_in_memory().unwrap();
        store
            .upsert_capability_item(
                CapabilityKind::Mcp,
                "fs",
                Some(stdio(vec![])),
                None,
                "manual",
            )
            .unwrap();
        assert!(
            store
                .list_enabled_capabilities("claude-code")
                .unwrap()
                .is_empty()
        );
        assert!(caps_empty(
            &store.resolve_capabilities("claude-code", false).unwrap()
        ));
    }

    #[test]
    fn built_in_backend_can_enable_and_disable() {
        let store = Store::open_in_memory().unwrap();
        store
            .upsert_capability_item(
                CapabilityKind::Mcp,
                "fs",
                Some(stdio(vec![])),
                None,
                "manual",
            )
            .unwrap();
        store
            .set_capability_enabled("codex", CapabilityKind::Mcp, "fs", true, false)
            .unwrap();
        let caps = store.resolve_capabilities("codex", false).unwrap();
        assert_eq!(caps["mcp_servers"][0]["name"], "fs");
        store
            .set_capability_enabled("codex", CapabilityKind::Mcp, "fs", false, false)
            .unwrap();
        assert!(caps_empty(
            &store.resolve_capabilities("codex", false).unwrap()
        ));
    }

    #[test]
    fn unknown_profile_and_item_are_rejected() {
        let store = Store::open_in_memory().unwrap();
        assert!(matches!(
            store.set_capability_enabled("nope", CapabilityKind::Mcp, "fs", true, false),
            Err(CapabilityError::NotFound(_))
        ));
        assert!(matches!(
            store.set_capability_enabled("pi", CapabilityKind::Mcp, "missing", true, false),
            Err(CapabilityError::NotFound(_))
        ));
    }

    #[test]
    fn ralphus_mcp_is_absent_unless_allowed() {
        let store = Store::open_in_memory().unwrap();
        assert!(
            store
                .list_capability_items(false)
                .unwrap()
                .iter()
                .all(|i| i.name != RALPHUS_MCP_NAME)
        );
        assert!(
            store
                .list_capability_items(true)
                .unwrap()
                .iter()
                .any(|i| i.name == RALPHUS_MCP_NAME)
        );
        assert!(matches!(
            store.set_capability_enabled("pi", CapabilityKind::Mcp, RALPHUS_MCP_NAME, true, false),
            Err(CapabilityError::NotFound(_))
        ));
        store
            .set_capability_enabled("pi", CapabilityKind::Mcp, RALPHUS_MCP_NAME, true, true)
            .unwrap();
        assert!(caps_empty(
            &store.resolve_capabilities("pi", false).unwrap()
        ));
        let caps = store.resolve_capabilities("pi", true).unwrap();
        assert_eq!(caps["mcp_servers"][0]["name"], RALPHUS_MCP_NAME);
        assert!(matches!(
            store.upsert_capability_item(
                CapabilityKind::Mcp,
                RALPHUS_MCP_NAME,
                Some(stdio(vec![])),
                None,
                "manual"
            ),
            Err(CapabilityError::Invalid(_))
        ));
    }

    #[test]
    fn views_mask_set_values_and_edits_keep_them() {
        // Built at runtime so no secret-shaped literal is committed to the repo.
        let secret = format!("ghp_probe{}", now_ms());
        let store = Store::open_in_memory().unwrap();
        store
            .upsert_capability_item(
                CapabilityKind::Mcp,
                "gh",
                Some(stdio(vec![set("TOKEN", &secret)])),
                None,
                "manual",
            )
            .unwrap();
        let view = store.list_capability_items(false).unwrap();
        let shown = serde_json::to_string(&view).unwrap();
        assert!(!shown.contains(&secret));
        // Saving the masked view back keeps the stored secret.
        let masked = view[0].mcp.clone().unwrap();
        store
            .upsert_capability_item(CapabilityKind::Mcp, "gh", Some(masked), None, "manual")
            .unwrap();
        let caps = {
            store
                .set_capability_enabled("claude-code", CapabilityKind::Mcp, "gh", true, false)
                .unwrap();
            store.resolve_capabilities("claude-code", false).unwrap()
        };
        assert_eq!(caps["mcp_servers"][0]["env"]["TOKEN"], secret.as_str());
    }

    #[test]
    fn resolved_env_is_registered_for_redaction() {
        crate::redact::with_registry_lock(|| {
            // Built at runtime so no secret-shaped literal is committed to the repo.
            let secret = format!("redact-probe-{}", now_ms());
            let store = Store::open_in_memory().unwrap();
            store
                .upsert_capability_item(
                    CapabilityKind::Mcp,
                    "gh",
                    Some(stdio(vec![set("TOKEN", &secret)])),
                    None,
                    "manual",
                )
                .unwrap();
            store
                .set_capability_enabled("pi", CapabilityKind::Mcp, "gh", true, false)
                .unwrap();
            store.resolve_capabilities("pi", false).unwrap();
            assert!(!crate::redact::redact_all(&format!("oops {secret}")).contains(&secret));
        });
    }

    #[test]
    fn skill_folder_travels_as_bytes_and_missing_is_reported() {
        let dir = std::env::temp_dir().join(format!("ral695-skill-{}", now_ms()));
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::write(dir.join("SKILL.md"), "hello").unwrap();
        std::fs::write(dir.join("sub").join("a.txt"), "a").unwrap();
        let files = read_skill_files(&dir).unwrap();
        assert_eq!(files.len(), 2);
        assert!(files.iter().any(|f| f["path"] == "sub/a.txt"));
        assert!(check_skill_folder(&dir.join("nope")).is_err());
        std::fs::remove_file(dir.join("SKILL.md")).unwrap();
        assert!(check_skill_folder(&dir).unwrap_err().contains("SKILL.md"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn deleting_a_profile_or_item_clears_enablement() {
        let store = Store::open_in_memory().unwrap();
        store
            .upsert_agent_profile("mine", "claude-code", None, None, vec![], None)
            .unwrap();
        store
            .upsert_capability_item(
                CapabilityKind::Mcp,
                "fs",
                Some(stdio(vec![])),
                None,
                "manual",
            )
            .unwrap();
        store
            .set_capability_enabled("mine", CapabilityKind::Mcp, "fs", true, false)
            .unwrap();
        store.delete_agent_profile("mine").unwrap();
        assert!(store.list_enabled_capabilities("mine").unwrap().is_empty());
        store
            .set_capability_enabled("pi", CapabilityKind::Mcp, "fs", true, false)
            .unwrap();
        store
            .delete_capability_item(CapabilityKind::Mcp, "fs")
            .unwrap();
        assert!(store.list_enabled_capabilities("pi").unwrap().is_empty());
    }

    #[test]
    fn delivery_modes_are_validated() {
        let store = Store::open_in_memory().unwrap();
        let bad = CapabilityDelivery {
            mode: "extra_args".to_string(),
            args: vec![],
        };
        assert!(store.set_capability_delivery("pi", &bad).is_err());
        let ok = CapabilityDelivery {
            mode: "extra_args".to_string(),
            args: vec!["--mcp".to_string(), "{mcp_config}".to_string()],
        };
        store.set_capability_delivery("pi", &ok).unwrap();
        assert_eq!(store.get_capability_delivery("pi").unwrap(), ok);
        assert_eq!(
            store.resolve_capabilities("pi", false).unwrap()["delivery"]["mode"],
            "extra_args"
        );
        store
            .set_capability_delivery("pi", &CapabilityDelivery::default())
            .unwrap();
        assert!(caps_empty(
            &store.resolve_capabilities("pi", false).unwrap()
        ));
    }
}
