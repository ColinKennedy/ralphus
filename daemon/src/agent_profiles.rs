use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use ralphus_core::schema::TaskFile;
use ralphus_core::validate::{ErrorKind, ValidationError};
use serde::{Deserialize, Serialize};

use crate::agent_profile_env;
use crate::agent_profile_store::AgentProfileView;
use crate::config::{find_project_config, global_config_path};
use crate::store::Store;

pub const RAW_BACKEND: &str = "raw";

pub(crate) const PROFILE_BACKENDS: &[&str] = &[
    "claude",
    "claude-code",
    "codex",
    "pi",
    "ollama",
    "anthropic",
    "raw",
];
use ralphus_core::schema::RESERVED_AGENT_NAMES;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentProfile {
    pub backend: String,
    pub executable: Option<String>,
    pub model: Option<String>,
    pub env: BTreeMap<String, String>,
    /// The resolved values of every `env` entry that was indirection via
    /// `from_env` (as opposed to a literal authored in the config file).
    /// These are treated as secrets for RAL-264: the daemon registers them
    /// with `crate::redact` so the resolved value never lands in durable pane
    /// text / failure `detail`s even when the agent echoes it into its own
    /// terminal (e.g. `$env:ANTHROPIC_AUTH_TOKEN = 'sk-or-v1-...'`). Literal
    /// values are excluded — they are already plaintext in the config file.
    pub secret_values: BTreeSet<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedAgentSelection {
    pub backend: String,
    pub executable: Option<String>,
    pub model: Option<String>,
    pub env: BTreeMap<String, String>,
    pub custom_profile: bool,
    /// The agent-profile `from_env`-resolved secret values in play for this
    /// selection (empty for a built-in backend, which has no `env`).
    /// Propagated from [`AgentProfile::secret_values`]; see its doc comment.
    pub secret_values: BTreeSet<String>,
}

#[derive(Debug, Default, Deserialize)]
struct AgentProfilesFile {
    #[serde(default)]
    agent: Option<AgentTable>,
}

#[derive(Debug, Default, Deserialize)]
struct AgentTable {
    #[serde(default)]
    profiles: BTreeMap<String, RawAgentProfile>,
}

#[derive(Debug, Deserialize)]
struct RawAgentProfile {
    backend: String,
    #[serde(default)]
    executable: Option<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    env: BTreeMap<String, RawEnvValue>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum RawEnvValue {
    Literal(String),
    FromEnv { from_env: String },
}

fn parse_profile_file(path: &Path) -> Result<BTreeMap<String, AgentProfile>, String> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| format!("could not read {}: {e}", path.display()))?;
    let parsed: AgentProfilesFile =
        toml::from_str(&text).map_err(|e| format!("could not parse {}: {e}", path.display()))?;
    let mut out = BTreeMap::new();
    for (name, profile) in parsed.agent.unwrap_or_default().profiles {
        if RESERVED_AGENT_NAMES.contains(&name.as_str()) {
            return Err(format!(
                "{}: agent profile name \"{name}\" collides with a reserved built-in backend name",
                path.display()
            ));
        }
        if !PROFILE_BACKENDS.contains(&profile.backend.as_str()) {
            return Err(format!(
                "{}: agent profile \"{name}\" has unknown backend {:?}; expected one of {}",
                path.display(),
                profile.backend,
                PROFILE_BACKENDS.join(", ")
            ));
        }
        if is_native_backend(&profile.backend) && profile.executable.is_some() {
            return Err(format!(
                "{}: agent profile \"{name}\" sets executable for native backend {:?}; executable is only meaningful for claude-code, codex, pi, or raw",
                path.display(),
                profile.backend
            ));
        }
        if profile.backend == RAW_BACKEND && profile.executable.is_none() {
            return Err(format!(
                "{}: agent profile \"{name}\" uses backend \"raw\" but does not set executable",
                path.display()
            ));
        }
        let mut env = BTreeMap::new();
        // RAL-264: the resolved values of every `from_env`-indirected entry are
        // secret-shaped and must be scrubbed out of any durable pane text.
        // Collected here (where the parse still distinguishes
        // `RawEnvValue::FromEnv` from `RawEnvValue::Literal`) so resolution can
        // hand them to the redaction layer; see [`AgentProfile::secret_values`].
        let mut secret_values = BTreeSet::new();
        for (key, value) in profile.env {
            let resolved = match value {
                RawEnvValue::Literal(s) => s,
                RawEnvValue::FromEnv { from_env } => {
                    let resolved = std::env::var(&from_env).map_err(|_| {
                        format!(
                            "{}: agent profile \"{name}\" requires environment variable {from_env:?}, but it is not set in the daemon process environment. \
                            Set {from_env} in the environment the `ralphus-daemon serve` process runs in (not just your shell), then restart the daemon.",
                            path.display()
                        )
                    })?;
                    secret_values.insert(resolved.clone());
                    resolved
                }
            };
            env.insert(key, resolved);
        }
        out.insert(
            name,
            AgentProfile {
                backend: profile.backend,
                executable: profile.executable,
                model: profile.model,
                env,
                secret_values,
            },
        );
    }
    Ok(out)
}

fn merge_profiles(
    mut global: BTreeMap<String, AgentProfile>,
    local: BTreeMap<String, AgentProfile>,
) -> BTreeMap<String, AgentProfile> {
    for (name, profile) in local {
        global.insert(name, profile);
    }
    global
}

/// Parses `$RALPHUS_CONFIGURATION_PATH` (a `PATH`-separated list of
/// `.ralphus.toml` files, left-to-right, later wins) -- the same env var
/// `cli/src/config.rs` and `runner/src/config.rs` already read for every
/// other config field. Agent profiles didn't honor it, so a profile placed
/// via that established convention (rather than `$RALPHUS_CONFIG_HOME/config.toml`,
/// or a `.ralphus.toml` an ancestor of the resolved project path) silently
/// never loaded.
///
/// Takes the raw env value explicitly (rather than reading `std::env`
/// itself) so callers can test against a fixed value instead of whatever
/// `$RALPHUS_CONFIGURATION_PATH` happens to be set to in the real process
/// environment -- `std::env::set_var`/`remove_var` can't be used to override
/// it in-process, since both are `unsafe fn` and this workspace forbids
/// `unsafe_code` outright. Same rationale as
/// `cli_rs::config::get_candidates`'s `configuration_path_env` parameter.
fn configuration_path_entries(configuration_path_env: Option<&str>) -> Vec<PathBuf> {
    let Some(raw) = configuration_path_env else {
        return Vec::new();
    };
    std::env::split_paths(raw).collect()
}

/// Precedence, lowest to highest: `$RALPHUS_CONFIG_HOME/config.toml` (or its
/// `~/.config/ralphus/` default), then `$RALPHUS_CONFIGURATION_PATH` entries
/// in order, then the project-local `.ralphus.toml` found by walking up from
/// `cwd` -- matching the precedence `runner::config::load_with` already uses
/// for its own fields (configuration-path entries, then the nearest
/// git-root file, win over anything earlier).
fn load_profiles_for_path_with(
    cwd: &Path,
    configuration_path_env: Option<&str>,
) -> Result<BTreeMap<String, AgentProfile>, String> {
    let mut merged = match global_config_path() {
        Some(path) if path.is_file() => parse_profile_file(&path)?,
        _ => BTreeMap::new(),
    };
    for path in configuration_path_entries(configuration_path_env) {
        if path.is_file() {
            merged = merge_profiles(merged, parse_profile_file(&path)?);
        }
    }
    let local = match find_project_config(cwd) {
        Some(path) => parse_profile_file(&path)?,
        None => BTreeMap::new(),
    };
    Ok(merge_profiles(merged, local))
}

pub fn load_profiles_for_path(cwd: &Path) -> Result<BTreeMap<String, AgentProfile>, String> {
    let raw = std::env::var("RALPHUS_CONFIGURATION_PATH").ok();
    load_profiles_for_path_with(cwd, raw.as_deref())
}

pub fn load_profiles_for_current_dir() -> Result<BTreeMap<String, AgentProfile>, String> {
    let cwd =
        std::env::current_dir().map_err(|e| format!("could not read current directory: {e}"))?;
    load_profiles_for_path(&cwd)
}

pub fn is_native_backend(agent: &str) -> bool {
    matches!(agent, "claude" | "anthropic" | "ollama")
}

pub fn normalize_builtin_agent(agent: &str) -> Option<&'static str> {
    match agent {
        "claude" => Some("claude"),
        "anthropic" => Some("anthropic"),
        "ollama" => Some("ollama"),
        "raw" => Some("raw"),
        "claude-code" | "claude-cli" => Some("claude-code"),
        "codex" | "codex-cli" => Some("codex"),
        "pi" => Some("pi"),
        _ => None,
    }
}

/// A DB-side snapshot of everything [`resolve_agent_for_path_with`] needs to
/// consider RAL-473 database-backed agent profiles/backend-command overrides
/// during resolution, fetched once under a short store lock so the
/// resolution function itself never needs a `Store`/`StoreHandle` (and so it
/// never performs config-file I/O while a lock is held -- see
/// `scheduler.rs::resolve_shared_session_id`'s doc comment for why that
/// ordering matters).
#[derive(Debug, Clone, Default)]
pub struct AgentDbSnapshot {
    /// The stored DB profile named exactly `agent`, if one exists. A DB
    /// profile wins over a same-named legacy TOML profile (with a logged
    /// collision warning) -- see [`resolve_agent_for_path_with`].
    pub profile: Option<AgentProfileView>,
    /// Every stored built-in-backend command override, keyed by backend name
    /// (`claude-code`, `codex`, `pi`). Applies to a resolution that ends up
    /// on one of these backends via *either* a DB profile, a legacy TOML
    /// profile that didn't set its own `executable`, or a bare builtin agent
    /// name -- see [`resolve_agent_for_path_with`].
    pub backend_commands: BTreeMap<String, String>,
}

impl AgentDbSnapshot {
    /// Builds a snapshot for resolving `agent`. Only ever reads (never
    /// mutates) the store; safe to call while holding a store lock, and
    /// cheap enough (two indexed/small-table SQLite reads) to call once per
    /// resolution.
    #[must_use]
    pub fn load(store: &Store, agent: &str) -> Self {
        let profile = store.get_agent_profile(agent).unwrap_or(None);
        let backend_commands = store
            .list_agent_backend_commands()
            .unwrap_or_default()
            .into_iter()
            .map(|c| (c.backend, c.command))
            .collect();
        Self {
            profile,
            backend_commands,
        }
    }
}

pub fn resolve_agent_for_path(agent: &str, cwd: &Path) -> Result<ResolvedAgentSelection, String> {
    let raw = std::env::var("RALPHUS_CONFIGURATION_PATH").ok();
    resolve_agent_for_path_with(agent, cwd, raw.as_deref(), None)
}

/// [`resolve_agent_for_path`] with a database snapshot consulted first (RAL-473).
/// The one-short-lock-then-release pattern [`AgentDbSnapshot::load`] enables
/// is the intended way to call this from a context that only holds a
/// `StoreHandle` (see `scheduler.rs::resolve_agent_selection`).
pub fn resolve_agent_for_path_with_snapshot(
    agent: &str,
    cwd: &Path,
    db: Option<&AgentDbSnapshot>,
) -> Result<ResolvedAgentSelection, String> {
    let raw = std::env::var("RALPHUS_CONFIGURATION_PATH").ok();
    resolve_agent_for_path_with(agent, cwd, raw.as_deref(), db)
}

/// [`resolve_agent_for_path_with_snapshot`] for a caller that already holds a
/// bare `&Store` (no locking of its own required) -- builds the snapshot and
/// resolves in one call.
pub fn resolve_agent_for_path_db(
    agent: &str,
    cwd: &Path,
    store: &Store,
) -> Result<ResolvedAgentSelection, String> {
    let db = AgentDbSnapshot::load(store, agent);
    resolve_agent_for_path_with_snapshot(agent, cwd, Some(&db))
}

/// Backends whose invoked command is a *global* override (interview Q4) --
/// editing `agent_backend_commands` for one of these affects every profile
/// (DB or legacy TOML) that selects it, plus any bare use of the builtin
/// name itself. `raw` is excluded: it has no default command to override,
/// so it keeps a per-profile executable instead (`raw` is also otherwise
/// excluded from [`PROFILE_BACKENDS`] here since it's the one backend that
/// mandates its own executable).
pub(crate) fn command_overridable_backend(backend: &str) -> bool {
    matches!(backend, "claude-code" | "codex" | "pi")
}

/// [`resolve_agent_for_path`] with `$RALPHUS_CONFIGURATION_PATH` passed in
/// explicitly -- see [`configuration_path_entries`] for why. `db`, when
/// supplied, layers in RAL-473 database-backed profiles/backend-command
/// overrides on top of the legacy TOML/builtin resolution below; see
/// [`AgentDbSnapshot`]'s doc comment for the precedence rules.
fn resolve_agent_for_path_with(
    agent: &str,
    cwd: &Path,
    configuration_path_env: Option<&str>,
    db: Option<&AgentDbSnapshot>,
) -> Result<ResolvedAgentSelection, String> {
    if let Some(db_profile) = db.and_then(|db| db.profile.as_ref()) {
        let profiles = load_profiles_for_path_with(cwd, configuration_path_env)?;
        if profiles.contains_key(agent) {
            // ralphus[ignore-rlog-pair]: operator advisory about profile collision; caller logs task outcome if relevant
            crate::rlog!(
                WARNING,
                "ralphus [agent-profiles] agent \"{agent}\" is defined both as a database \
                 profile and a legacy TOML profile; the database profile wins. Remove the \
                 TOML definition (or rename one of the two) to resolve the collision."
            );
        }
        let resolved_env =
            agent_profile_env::resolve_agent_env(&db_profile.env, |name| std::env::var(name).ok());
        let secret_values: BTreeSet<String> =
            agent_profile_env::secret_values(&db_profile.env, |name| std::env::var(name).ok())
                .into_iter()
                .collect();
        // RAL-264: mirror the TOML `from_env` registration below -- a DB
        // profile's `Link`-resolved values are exactly as secret-shaped.
        crate::redact::register_all(secret_values.iter().cloned());
        let executable = if db_profile.backend == RAW_BACKEND {
            db_profile.executable.clone()
        } else {
            db.and_then(|db| db.backend_commands.get(&db_profile.backend).cloned())
        };
        return Ok(ResolvedAgentSelection {
            backend: db_profile.backend.clone(),
            executable,
            model: db_profile.model.clone(),
            env: resolved_env,
            custom_profile: true,
            secret_values,
        });
    }
    let profiles = load_profiles_for_path_with(cwd, configuration_path_env)?;
    if let Some(profile) = profiles.get(agent) {
        // RAL-264: this profile is about to be used to run a cell, so its
        // `from_env`-resolved secret values must be scrubbed everywhere raw
        // pane text gets persisted. Even though the profile may already be
        // registered from a prior resolution (idempotent), registering here
        // guarantees the values are in place before any tmux pane capture.
        crate::redact::register_all(profile.secret_values.iter().cloned());
        // A legacy TOML profile that didn't author its own `executable` for
        // an overridable backend still picks up the global DB command
        // override (RAL-473); an explicit TOML `executable` keeps winning,
        // preserving today's behavior unchanged for anyone still relying on
        // it during the migration window (interview Q8).
        let executable = profile.executable.clone().or_else(|| {
            command_overridable_backend(&profile.backend)
                .then(|| db.and_then(|db| db.backend_commands.get(&profile.backend).cloned()))
                .flatten()
        });
        return Ok(ResolvedAgentSelection {
            backend: profile.backend.clone(),
            executable,
            model: profile.model.clone(),
            env: profile.env.clone(),
            custom_profile: true,
            secret_values: profile.secret_values.clone(),
        });
    }
    if let Some(backend) = normalize_builtin_agent(agent) {
        let executable = command_overridable_backend(backend)
            .then(|| db.and_then(|db| db.backend_commands.get(backend).cloned()))
            .flatten();
        return Ok(ResolvedAgentSelection {
            backend: backend.to_string(),
            executable,
            model: None,
            env: BTreeMap::new(),
            custom_profile: false,
            secret_values: BTreeSet::new(),
        });
    }
    Err(format!(
        "unknown agent \"{agent}\": not a configured agent profile and not a built-in backend"
    ))
}

fn config_cwd_for_cell(
    store: &Store,
    task_project: Option<&str>,
    cell: &ralphus_core::schema::CellDef,
) -> Option<PathBuf> {
    if let Some(cwd) = cell.cwd.as_deref() {
        if ralphus_core::schema::first_worktree_placeholder_in_text(cwd).is_none() {
            return Some(PathBuf::from(cwd));
        }
    }
    task_project
        .and_then(|name| store.resolve_project(name).ok().flatten())
        .map(|p| PathBuf::from(p.path))
}

pub fn validate_task_file_profiles(
    store: &Store,
    raw_toml: &str,
    file: &TaskFile,
) -> Vec<ValidationError> {
    let raw = std::env::var("RALPHUS_CONFIGURATION_PATH").ok();
    validate_task_file_profiles_with(store, raw_toml, file, raw.as_deref())
}

/// [`validate_task_file_profiles`] with `$RALPHUS_CONFIGURATION_PATH` passed
/// in explicitly -- see [`configuration_path_entries`] for why.
fn validate_task_file_profiles_with(
    store: &Store,
    _raw_toml: &str,
    file: &TaskFile,
    configuration_path_env: Option<&str>,
) -> Vec<ValidationError> {
    let mut errors = Vec::new();
    for (task_idx, task) in file.task.iter().enumerate() {
        for (cell_idx, cell) in task.cell.iter().enumerate() {
            // `agent` may be a literal name or a candidate list (RAL-4xx) --
            // every named candidate must resolve to a real builtin/profile,
            // even though only one of them will actually be picked at
            // submit time (`Store::insert_squad_with_id`'s availability
            // walk). This just catches typos/unknown names up front.
            let spec = cell
                .agent
                .clone()
                .or_else(|| task.agent.clone())
                .unwrap_or_else(|| {
                    ralphus_core::schema::AgentSpec::Single(
                        ralphus_core::schema::DEFAULT_AGENT.to_string(),
                    )
                });
            let names = spec.names();
            let Some(cwd) = config_cwd_for_cell(store, task.project.as_deref(), cell) else {
                continue;
            };
            let mut selections = Vec::with_capacity(names.len());
            let mut resolution_failed = false;
            for (candidate_idx, name) in names.iter().enumerate() {
                let db = AgentDbSnapshot::load(store, name);
                match resolve_agent_for_path_with(name, &cwd, configuration_path_env, Some(&db)) {
                    Ok(s) => selections.push(s),
                    Err(message) => {
                        let path = if names.len() > 1 {
                            format!("task[{task_idx}].cell[{cell_idx}].agent[{candidate_idx}]")
                        } else {
                            format!("task[{task_idx}].cell[{cell_idx}].agent")
                        };
                        errors.push(ValidationError {
                            path,
                            kind: ErrorKind::InvalidValue,
                            message,
                            line: None,
                        });
                        resolution_failed = true;
                        break;
                    }
                }
            }
            if resolution_failed {
                continue;
            }

            // Capability gating, run against every resolved candidate: since
            // which one wins isn't known until submit-time availability
            // resolution, a field is only allowed if it would be valid no
            // matter which candidate is picked (mirrors `core::validate`'s
            // "all must support" policy for the list form; a
            // RESERVED_AGENT_NAMES agent is already rejected there, so this
            // additionally covers a custom profile, whose backend `core`
            // cannot see).
            for (name, selection) in names.iter().zip(selections.iter()) {
                // `core`'s offline validator only rejects system_prompt for
                // agent names it recognizes itself (RESERVED_AGENT_NAMES) --
                // it defers on any custom profile, since it can't see the
                // profile's backend. Now that the profile is resolved, check
                // its backend the same way.
                if selection.custom_profile
                    && (cell.system_prompt.is_some() || cell.system_prompt_position.is_some())
                    && !ralphus_core::schema::agent_supports_system_prompt(&selection.backend)
                {
                    let key = if cell.system_prompt.is_some() {
                        "system_prompt"
                    } else {
                        "system_prompt_position"
                    };
                    errors.push(ValidationError {
                        path: format!("task[{task_idx}].cell[{cell_idx}].{key}"),
                        kind: ErrorKind::InvalidValue,
                        message: format!(
                            "agent profile \"{name}\" resolves to backend \"{}\", which does \
                             not support system_prompt/system_prompt_position (only \
                             claude-code, codex, and pi backends do)",
                            selection.backend
                        ),
                        line: None,
                    });
                }
                // RAL-304: same deferral shape as system_prompt above --
                // `core` can't classify a custom profile's backend itself,
                // so it only rejects `maximum_context`/`auto_compact_threshold`
                // for a RESERVED_AGENT_NAMES agent; check the resolved
                // backend here. Either field cascades from the task, so both
                // are checked at their effective (cell-or-task) value, not
                // just the cell's own. The two fields are checked
                // independently, not as a pair -- claude-code accepts
                // auto_compact_threshold but not maximum_context (see
                // `agent_supports_auto_compact_threshold`'s doc comment for
                // why).
                let has_maximum_context =
                    cell.maximum_context.is_some() || task.maximum_context.is_some();
                let has_auto_compact_threshold =
                    cell.auto_compact_threshold.is_some() || task.auto_compact_threshold.is_some();
                if selection.custom_profile
                    && has_maximum_context
                    && !ralphus_core::schema::agent_supports_maximum_context(&selection.backend)
                {
                    errors.push(ValidationError {
                        path: format!("task[{task_idx}].cell[{cell_idx}].maximum_context"),
                        kind: ErrorKind::InvalidValue,
                        message: format!(
                            "agent profile \"{name}\" resolves to backend \"{}\", which does \
                             not support maximum_context (only codex and pi backends do)",
                            selection.backend
                        ),
                        line: None,
                    });
                }
                if selection.custom_profile
                    && has_auto_compact_threshold
                    && !ralphus_core::schema::agent_supports_auto_compact_threshold(
                        &selection.backend,
                    )
                {
                    errors.push(ValidationError {
                        path: format!("task[{task_idx}].cell[{cell_idx}].auto_compact_threshold"),
                        kind: ErrorKind::InvalidValue,
                        message: format!(
                            "agent profile \"{name}\" resolves to backend \"{}\", which does \
                             not support auto_compact_threshold (only codex, pi, and \
                             claude-code backends do)",
                            selection.backend
                        ),
                        line: None,
                    });
                }
                // RAL-333: same deferral shape as maximum_context/
                // auto_compact_threshold above -- `core` can't classify a
                // custom profile's backend itself, so it only rejects
                // `maximum_tool_output_tokens` for a RESERVED_AGENT_NAMES
                // agent; check the resolved backend here. Cascades from the
                // task, so it's checked at its effective (cell-or-task)
                // value.
                let has_maximum_tool_output_tokens = cell.maximum_tool_output_tokens.is_some()
                    || task.maximum_tool_output_tokens.is_some();
                if selection.custom_profile
                    && has_maximum_tool_output_tokens
                    && !ralphus_core::schema::agent_supports_maximum_tool_output_tokens(
                        &selection.backend,
                    )
                {
                    errors.push(ValidationError {
                        path: format!(
                            "task[{task_idx}].cell[{cell_idx}].maximum_tool_output_tokens"
                        ),
                        kind: ErrorKind::InvalidValue,
                        message: format!(
                            "agent profile \"{name}\" resolves to backend \"{}\", which does \
                             not support maximum_tool_output_tokens (only codex, pi, and \
                             claude-code backends do)",
                            selection.backend
                        ),
                        line: None,
                    });
                }
            }
        }
    }

    // `[[review]].agent` gets the same treatment as a cell's `agent`, but a
    // review has no `cwd`/`project` of its own -- it's inferred from
    // whichever cells opt in via `cell.review = "<<review:<id>>>"` (a review
    // can span several projects, materializing one guardian per project).
    // Resolve against every distinct project a matching cell resolves to.
    for (review_idx, review) in file.review.iter().enumerate() {
        let (Some(review_id), Some(agent)) = (review.id.as_deref(), review.agent.as_deref()) else {
            continue;
        };
        let mut seen_cwds = BTreeSet::new();
        for task in &file.task {
            for cell in &task.cell {
                let cell_review_id = cell
                    .review
                    .as_deref()
                    .and_then(ralphus_core::schema::parse_cell_review_sentinel);
                if cell_review_id != Some(review_id) {
                    continue;
                }
                let Some(cwd) = config_cwd_for_cell(store, task.project.as_deref(), cell) else {
                    continue;
                };
                if !seen_cwds.insert(cwd.clone()) {
                    continue;
                }
                let db = AgentDbSnapshot::load(store, agent);
                if let Err(message) =
                    resolve_agent_for_path_with(agent, &cwd, configuration_path_env, Some(&db))
                {
                    errors.push(ValidationError {
                        path: format!("review[{review_idx}].agent"),
                        kind: ErrorKind::InvalidValue,
                        message: format!("{message} (for project {})", cwd.display()),
                        line: None,
                    });
                }
            }
        }
    }

    errors
}

/// Apply a configured profile's default model to cells and reviews that did
/// not declare a task-, cell-, or review-level model of their own.
///
/// This happens after profile validation and before persistence, so the
/// selected model is durable in the cell/review row and reaches the backend
/// exactly as if it had been authored in the task file. An authored model
/// always wins over the profile default.
pub fn apply_profile_model_defaults(store: &Store, file: &mut TaskFile) {
    for task in &mut file.task {
        for cell in &mut task.cell {
            if cell.model.is_some() || task.model.is_some() {
                continue;
            }
            // A candidate list (RAL-4xx) carries its own per-entry model,
            // and a list-form scope can't also set a sibling `model` (see
            // `core::validate::check_agent_field`) -- so profile-default
            // backfill only makes sense for a literal `agent`. Skip a
            // candidate list entirely rather than guessing which entry's
            // model to fill in.
            let agent = match cell.agent.as_ref().or(task.agent.as_ref()) {
                Some(ralphus_core::schema::AgentSpec::Single(s)) => s.as_str(),
                Some(ralphus_core::schema::AgentSpec::Candidates(_)) => continue,
                None => ralphus_core::schema::DEFAULT_AGENT,
            };
            let Some(cwd) = config_cwd_for_cell(store, task.project.as_deref(), cell) else {
                continue;
            };
            if let Ok(selection) = resolve_agent_for_path_db(agent, &cwd, store) {
                cell.model = selection.model;
            }
        }
    }

    for review in &mut file.review {
        if review.model.is_some() {
            continue;
        }
        let (Some(review_id), Some(agent)) = (review.id.as_deref(), review.agent.as_deref()) else {
            continue;
        };
        let matching_cwd = file.task.iter().find_map(|task| {
            task.cell.iter().find_map(|cell| {
                (cell
                    .review
                    .as_deref()
                    .and_then(ralphus_core::schema::parse_cell_review_sentinel)
                    == Some(review_id))
                .then(|| config_cwd_for_cell(store, task.project.as_deref(), cell))
                .flatten()
            })
        });
        if let Some(cwd) = matching_cwd {
            if let Ok(selection) = resolve_agent_for_path_db(agent, &cwd, store) {
                review.model = selection.model;
            }
        }
    }
}

/// Cell/task cwd used to resolve an `agent` value that has no cell of its
/// own to read a `cwd` from -- a cell-less task's own `agent` candidate
/// list. Mirrors [`config_cwd_for_cell`]'s own project-path fallback, since
/// there's no cell here to check for an inline `cwd` first. Only matters for
/// resolving a *custom profile* candidate name (a builtin resolves
/// regardless of cwd, see [`resolve_agent_for_path_with`]) -- falling back
/// to the daemon's own working directory when even the project can't be
/// resolved just means a profile-named candidate in that edge case won't be
/// found, which is the correct outcome when there's no project to look one
/// up in.
fn task_level_cwd(store: &Store, task: &ralphus_core::schema::TaskDef) -> PathBuf {
    task.cell
        .first()
        .and_then(|cell| config_cwd_for_cell(store, task.project.as_deref(), cell))
        .or_else(|| {
            task.project
                .as_deref()
                .and_then(|name| store.resolve_project(name).ok().flatten())
                .map(|p| PathBuf::from(p.path))
        })
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")))
}

/// Walk every task/cell whose `agent` is a candidate list (RAL-4xx) and
/// collapse it to the first available candidate, mutating `file` in place.
/// Called once, at submit time, before persistence -- mirrors
/// [`apply_profile_model_defaults`]'s mutate-before-`insert_squad` shape,
/// and, like `TaskDef::machine` (RAL-185), is resolved exactly once so a
/// restart never re-walks it: by the time `Store::insert_squad_with_id`
/// runs, every `agent` field is guaranteed a literal (or absent).
///
/// "Available" is the same free preflight check the scheduler itself runs
/// before dispatching a cell (`Runner::preflight_agent`, which spawns
/// `ralphus-runner preflight`) -- not a live/billed reachability probe, so a
/// bad API key or rejected model name still only surfaces once the picked
/// candidate actually runs, same as it already does today for a literal
/// `agent`. Only checked on the daemon's own host: resolution happens once,
/// at submit time, before any worktree exists to know which remote machine
/// (RAL-185) a cell will actually run on.
///
/// Every candidate name has already been confirmed to exist by
/// [`validate_task_file_profiles`] (the caller runs that first and rejects
/// the submission outright on any unknown name), so a resolution failure
/// here is only ever "not installed/configured on this host," never a typo.
///
/// Takes `runner` rather than constructing a
/// [`crate::runner::SubprocessRunner`] itself, mirroring
/// `guardian_merge::preflight_resolver_agent`'s injected-`&dyn Runner` shape
/// -- lets a test substitute a fake without spawning a real subprocess.
pub fn resolve_agent_candidate_lists(
    store: &Store,
    file: &mut TaskFile,
    runner: &dyn crate::runner::Runner,
) -> Vec<ValidationError> {
    let mut errors = Vec::new();
    for (task_idx, task) in file.task.iter_mut().enumerate() {
        if let Some(ralphus_core::schema::AgentSpec::Candidates(candidates)) = &task.agent {
            let cwd = task_level_cwd(store, task);
            match pick_available_candidate(store, runner, candidates, &cwd) {
                Some(winner) => {
                    task.model = winner.model.clone();
                    task.agent = Some(ralphus_core::schema::AgentSpec::Single(winner.agent));
                }
                None => errors.push(no_candidate_available_error(
                    format!("task[{task_idx}].agent"),
                    candidates,
                )),
            }
        }
        for (cell_idx, cell) in task.cell.iter_mut().enumerate() {
            let Some(ralphus_core::schema::AgentSpec::Candidates(candidates)) = &cell.agent else {
                continue;
            };
            let Some(cwd) = config_cwd_for_cell(store, task.project.as_deref(), cell) else {
                continue;
            };
            match pick_available_candidate(store, runner, candidates, &cwd) {
                Some(winner) => {
                    cell.model = winner.model.clone();
                    cell.agent = Some(ralphus_core::schema::AgentSpec::Single(winner.agent));
                }
                None => errors.push(no_candidate_available_error(
                    format!("task[{task_idx}].cell[{cell_idx}].agent"),
                    candidates,
                )),
            }
        }
    }
    errors
}

/// First candidate (in list order) whose resolved backend passes the
/// scheduler's own preflight check.
fn pick_available_candidate(
    store: &Store,
    runner: &dyn crate::runner::Runner,
    candidates: &[ralphus_core::schema::AgentCandidate],
    cwd: &Path,
) -> Option<ralphus_core::schema::AgentCandidate> {
    candidates
        .iter()
        .find(|c| {
            resolve_agent_for_path_db(&c.agent, cwd, store).is_ok_and(|selection| {
                runner
                    .preflight_agent(&selection.backend, selection.executable.as_deref(), None)
                    .is_ok()
            })
        })
        .cloned()
}

fn no_candidate_available_error(
    path: String,
    candidates: &[ralphus_core::schema::AgentCandidate],
) -> ValidationError {
    let tried: Vec<&str> = candidates.iter().map(|c| c.agent.as_str()).collect();
    ValidationError {
        path,
        kind: ErrorKind::InvalidValue,
        message: format!(
            "no candidate agent in this list is available on this machine (tried: {})",
            tried.join(", ")
        ),
        line: None,
    }
}

/// One agent-profile health finding, as returned by `GET
/// /api/health/agent-profiles` and rendered by `ralphus check health`.
/// Runs entirely inside the daemon process (`Store::list_projects` for the
/// project roots, `std::env::var`/[`resolve_executable`] for resolution) so
/// the result reflects the daemon's actual environment/PATH -- not whatever
/// shell happened to run the `ralphus` CLI, which can silently differ from
/// the environment `ralphus-daemon serve` was started in.
#[derive(Debug, Clone, Serialize)]
pub struct ProfileHealthResult {
    pub name: String,
    pub status: &'static str,
    pub detail: String,
}

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::metadata(path)
        .map(|m| m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(not(unix))]
fn is_executable(_path: &Path) -> bool {
    true
}

/// `shutil.which` equivalent, evaluated against the daemon process's own
/// `PATH`/`PATHEXT` -- deliberately separate from `cli_rs::health`'s
/// identically-shaped helper, since that one runs in the CLI process and
/// would check the wrong environment for this purpose.
///
/// `pub(crate)`: also used by [`crate::runner::SubprocessRunner`]'s
/// `preflight_runner_executable` (RAL-377), which needs the exact same
/// PATH/PATHEXT-aware resolution to check the `ralphus-runner` executable
/// itself before the scheduler marks a cell in-progress.
pub(crate) fn resolve_executable(program: &str) -> Result<String, String> {
    let path = Path::new(program);
    if path.components().count() > 1 {
        if !path.is_file() {
            return Err(format!("{program} does not exist or is not a file"));
        }
        if !is_executable(path) {
            return Err(format!("{program} is not executable"));
        }
        return Ok(program.to_string());
    }
    let Ok(path_var) = std::env::var("PATH") else {
        return Err(format!("{program} is not resolvable: PATH is not set"));
    };
    let mut suffixes = vec![String::new()];
    if cfg!(target_os = "windows") {
        let pathext =
            std::env::var("PATHEXT").unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".to_string());
        suffixes.extend(
            pathext
                .split(';')
                .filter(|s| !s.is_empty())
                .map(str::to_string),
        );
    }
    for dir in std::env::split_paths(&path_var) {
        for suffix in &suffixes {
            let candidate = dir.join(format!("{program}{suffix}"));
            if candidate.is_file() {
                return Ok(candidate.to_string_lossy().into_owned());
            }
        }
    }
    Err(format!("{program:?} is not resolvable on PATH"))
}

fn check_profiles_for_root(root: &Path, suffix: &str) -> Vec<ProfileHealthResult> {
    let profiles = match load_profiles_for_path(root) {
        Ok(profiles) => profiles,
        Err(e) => {
            return vec![ProfileHealthResult {
                name: format!("agent-profiles{suffix}"),
                status: "fail",
                detail: e,
            }];
        }
    };
    let mut results = Vec::new();
    for (name, profile) in profiles {
        let check_name = format!("agent-profile:{name}{suffix}");
        let Some(executable) = profile.executable.as_deref() else {
            results.push(ProfileHealthResult {
                name: check_name,
                status: "pass",
                detail: format!("backend={}", profile.backend),
            });
            continue;
        };
        match resolve_executable(executable) {
            Ok(resolved) => results.push(ProfileHealthResult {
                name: check_name,
                status: "pass",
                detail: format!(
                    "backend={} executable={executable} -> {resolved}",
                    profile.backend
                ),
            }),
            Err(reason) => results.push(ProfileHealthResult {
                name: check_name,
                status: "fail",
                detail: format!(
                    "backend={} {reason} in the daemon process's PATH. Install it (or fix the \
                    path) where `ralphus-daemon serve` runs, then restart the daemon.",
                    profile.backend
                ),
            }),
        }
    }
    results
}

/// Health-checks every RAL-473 database-backed agent profile and backend
/// command override, independent of any project root (DB profiles are
/// global-only, see [`AgentDbSnapshot`]'s doc comment). A backend-command
/// override for `claude-code`/`codex`/`pi` is checked once per backend
/// (never per-profile) since it's a shared, global setting -- matching how
/// [`resolve_agent_for_path_with`] applies it.
fn check_db_profiles_health(store: &Store) -> Vec<ProfileHealthResult> {
    let mut results = Vec::new();
    let commands = store.list_agent_backend_commands().unwrap_or_default();
    for cmd in &commands {
        let check_name = format!("agent-backend-command:{}", cmd.backend);
        // RAL-485: evaluated through the same `diagnose_backend_command`
        // every other surface (the Health/Agents tabs' claude-command/
        // codex-command/pi-command checks) uses, rather than a naive
        // `command.split_whitespace().next()` guess at the executable --
        // that guess mis-evaluated any command carrying arguments (it
        // resolved and executability-checked only the first word) and never
        // recognized a genuinely complex, shell-routed command as anything
        // other than "the first token", silently testing the wrong thing.
        let health = crate::health_sweep::diagnose_backend_command(&cmd.backend, &cmd.command);
        results.push(ProfileHealthResult {
            name: check_name,
            status: health.status,
            detail: format!("command={:?} {}", cmd.command, health.detail),
        });
    }
    let profiles = store.list_agent_profiles().unwrap_or_default();
    for profile in &profiles {
        let check_name = format!("agent-profile:{} (database)", profile.name);
        let Some(executable) = profile.executable.as_deref() else {
            results.push(ProfileHealthResult {
                name: check_name,
                status: "pass",
                detail: format!("backend={}", profile.backend),
            });
            continue;
        };
        match resolve_executable(executable) {
            Ok(resolved) => results.push(ProfileHealthResult {
                name: check_name,
                status: "pass",
                detail: format!(
                    "backend={} executable={executable} -> {resolved}",
                    profile.backend
                ),
            }),
            Err(reason) => results.push(ProfileHealthResult {
                name: check_name,
                status: "fail",
                detail: format!(
                    "backend={} {reason} in the daemon process's PATH. Install it (or fix the \
                    path) where `ralphus-daemon serve` runs, then restart the daemon.",
                    profile.backend
                ),
            }),
        }
    }
    results
}

/// Health-checks every agent profile the daemon can see for `cwd` plus
/// every registered project's `.ralphus.toml` (deduped by config path), plus
/// every RAL-473 database-backed profile/backend-command override -- mirrors
/// the root discovery `validate_task_file_profiles` uses at submit time, so
/// `ralphus check health` and `ralphus submit` agree on which profiles are
/// in scope.
pub fn check_profiles_health(store: &Store, cwd: &Path) -> Vec<ProfileHealthResult> {
    let mut results = check_profiles_for_root(cwd, "");
    let mut seen_configs = BTreeSet::new();
    if let Some(path) = find_project_config(cwd) {
        seen_configs.insert(path);
    }
    let projects = store.list_projects().unwrap_or_default();
    for project in projects {
        let project_path = PathBuf::from(&project.path);
        let Some(config_path) = find_project_config(&project_path) else {
            continue;
        };
        if !seen_configs.insert(config_path.clone()) {
            continue;
        }
        let suffix = format!(" ({})", config_path.display());
        results.extend(check_profiles_for_root(&project_path, &suffix));
    }
    results.extend(check_db_profiles_health(store));
    results
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;

    fn tempdir(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "ralphus-agent-profiles-{label}-{}-{}",
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
    fn builtin_agent_aliases_normalize() {
        assert_eq!(normalize_builtin_agent("claude-cli"), Some("claude-code"));
        assert_eq!(normalize_builtin_agent("codex-cli"), Some("codex"));
        assert_eq!(normalize_builtin_agent("unknown"), None);
    }

    #[test]
    fn parse_profile_file_resolves_from_env_indirection() {
        let project_root = tempdir("project-root");
        fs::write(
            project_root.join(".ralphus.toml"),
            r#"
[agent.profiles.shared]
backend = "raw"
executable = "my-raw-runner"

[agent.profiles.shared.env]
PATH_COPY = { from_env = "PATH" }
"#,
        )
        .expect("write project config");

        let profiles =
            parse_profile_file(&project_root.join(".ralphus.toml")).expect("parse profiles");
        let shared = profiles.get("shared").expect("shared profile");
        assert_eq!(shared.backend, "raw");
        assert_eq!(shared.executable.as_deref(), Some("my-raw-runner"));
        assert_eq!(
            shared.env.get("PATH_COPY").map(String::as_str),
            std::env::var("PATH").ok().as_deref()
        );
        // RAL-264: the `from_env`-resolved value is tracked as a secret value
        // (scrubbed from durable pane text), even though it isn't itself an
        // API key — `from_env` is the marker for "could be sensitive".
        assert!(
            shared
                .secret_values
                .contains(std::env::var("PATH").ok().unwrap_or_default().as_str())
        );
    }

    #[test]
    fn profile_default_model_applies_unless_task_or_cell_declares_one() {
        let project_root = tempdir("profile-default-model");
        fs::write(
            project_root.join(".ralphus.toml"),
            r#"
[agent.profiles.deepseek]
backend = "pi"
model = "openrouter/deepseek/deepseek-v4-flash-0731"
"#,
        )
        .expect("write project config");
        let cwd = project_root.to_string_lossy().replace('\\', "/");
        let mut file: TaskFile = toml::from_str(&format!(
            "[[task]]\nname=\"t\"\nagent=\"deepseek\"\n[[task.cell]]\nid=\"default\"\ncwd=\"{cwd}\"\nprompt=\"p\"\n[[task.cell]]\nid=\"override\"\ncwd=\"{cwd}\"\nmodel=\"openrouter/other\"\nprompt=\"p\"\n"
        ))
        .expect("parse task file");
        let store = Store::open_in_memory().expect("open store");

        apply_profile_model_defaults(&store, &mut file);

        assert_eq!(
            file.task[0].cell[0].model.as_deref(),
            Some("openrouter/deepseek/deepseek-v4-flash-0731")
        );
        assert_eq!(
            file.task[0].cell[1].model.as_deref(),
            Some("openrouter/other")
        );
    }

    #[test]
    fn literal_env_values_are_not_treated_as_secrets() {
        let root = tempdir("literal-not-secret");
        fs::write(
            root.join(".ralphus.toml"),
            r#"
[agent.profiles.custom]
backend = "raw"
executable = "my-raw-runner"

[agent.profiles.custom.env]
FEATURE_FLAG = "enabled"
"#,
        )
        .expect("write project config");

        let profiles = parse_profile_file(&root.join(".ralphus.toml")).expect("parse profiles");
        let custom = profiles.get("custom").expect("custom profile");
        assert_eq!(
            custom.env.get("FEATURE_FLAG").map(String::as_str),
            Some("enabled")
        );
        // A literal authored in the config file is already plaintext there, so
        // it is not promoted to the redaction set (RAL-264).
        assert!(custom.secret_values.is_empty());
    }

    #[test]
    fn load_profiles_for_path_with_reads_configuration_path_entries() {
        let config_dir = tempdir("configuration-path-source");
        let config_file = config_dir.join(".ralphus.toml");
        fs::write(
            &config_file,
            r#"
[agent.profiles.from-configuration-path]
backend = "claude-code"
"#,
        )
        .expect("write configuration-path file");

        // `cwd` is unrelated to `config_dir` -- no ancestor `.ralphus.toml` and
        // no `$RALPHUS_CONFIG_HOME`, so the only way this profile can be found
        // is through the `$RALPHUS_CONFIGURATION_PATH` entry.
        let cwd = tempdir("configuration-path-unrelated-cwd");
        let profiles = load_profiles_for_path_with(&cwd, Some(config_file.to_str().unwrap()))
            .expect("load profiles");
        assert!(profiles.contains_key("from-configuration-path"));
    }

    #[test]
    fn load_profiles_for_path_with_project_local_wins_over_configuration_path() {
        let config_dir = tempdir("configuration-path-loser");
        let config_file = config_dir.join(".ralphus.toml");
        fs::write(
            &config_file,
            r#"
[agent.profiles.shared]
backend = "codex"
"#,
        )
        .expect("write configuration-path file");

        let project_root = tempdir("configuration-path-project-local-winner");
        fs::write(
            project_root.join(".ralphus.toml"),
            r#"
[agent.profiles.shared]
backend = "claude-code"
"#,
        )
        .expect("write project config");

        let profiles =
            load_profiles_for_path_with(&project_root, Some(config_file.to_str().unwrap()))
                .expect("load profiles");
        assert_eq!(
            profiles.get("shared").map(|p| p.backend.as_str()),
            Some("claude-code")
        );
    }

    #[test]
    fn merge_profiles_prefers_project_layer_on_name_collision() {
        let mut global = BTreeMap::new();
        global.insert(
            "shared".to_string(),
            AgentProfile {
                backend: "codex".to_string(),
                executable: Some("codex-global".to_string()),
                model: None,
                env: BTreeMap::from([("GLOBAL_ONLY".to_string(), "1".to_string())]),
                secret_values: BTreeSet::new(),
            },
        );
        let mut project = BTreeMap::new();
        project.insert(
            "shared".to_string(),
            AgentProfile {
                backend: "raw".to_string(),
                executable: Some("project-runner".to_string()),
                model: None,
                env: BTreeMap::from([("PROJECT_ONLY".to_string(), "1".to_string())]),
                secret_values: BTreeSet::new(),
            },
        );

        let merged = merge_profiles(global, project);
        let shared = merged.get("shared").expect("shared profile");
        assert_eq!(shared.backend, "raw");
        assert_eq!(shared.executable.as_deref(), Some("project-runner"));
        assert_eq!(
            shared.env.get("PROJECT_ONLY").map(String::as_str),
            Some("1")
        );
        assert!(!shared.env.contains_key("GLOBAL_ONLY"));
    }

    fn task_file_with_agent_and_system_prompt(cwd: &Path, agent: &str) -> TaskFile {
        let cwd = cwd.to_string_lossy().replace('\\', "/");
        let src = format!(
            "[[task]]\nname=\"t\"\nproject=\"unused\"\n[[task.cell]]\nid=\"work\"\ncwd=\"{cwd}\"\nagent=\"{agent}\"\nprompt=\"p\"\nsystem_prompt=\"be terse\"\nsystem_prompt_position=\"append\"\n"
        );
        toml::from_str(&src).expect("parse task file")
    }

    #[test]
    fn validate_task_file_profiles_rejects_system_prompt_for_non_supporting_profile_backend() {
        let project_root = tempdir("system-prompt-ollama-profile");
        fs::write(
            project_root.join(".ralphus.toml"),
            r#"
[agent.profiles.custom-ollama]
backend = "ollama"
"#,
        )
        .expect("write project config");

        let store = Store::open_in_memory().expect("open store");
        let file = task_file_with_agent_and_system_prompt(&project_root, "custom-ollama");
        let errors = validate_task_file_profiles_with(&store, "", &file, None);

        assert!(
            errors
                .iter()
                .any(|e| e.path.contains("system_prompt") && e.message.contains("ollama")),
            "{errors:?}"
        );
    }

    #[test]
    fn validate_task_file_profiles_allows_system_prompt_for_claude_code_profile_backend() {
        let project_root = tempdir("system-prompt-claude-code-profile");
        fs::write(
            project_root.join(".ralphus.toml"),
            r#"
[agent.profiles.custom-claude]
backend = "claude-code"
"#,
        )
        .expect("write project config");

        let store = Store::open_in_memory().expect("open store");
        let file = task_file_with_agent_and_system_prompt(&project_root, "custom-claude");
        let errors = validate_task_file_profiles_with(&store, "", &file, None);

        assert!(
            errors.iter().all(|e| !e.path.contains("system_prompt")),
            "{errors:?}"
        );
    }

    fn task_file_with_agent_and_maximum_context(cwd: &Path, agent: &str) -> TaskFile {
        let cwd = cwd.to_string_lossy().replace('\\', "/");
        let src = format!(
            "[[task]]\nname=\"t\"\nproject=\"unused\"\n[[task.cell]]\nid=\"work\"\ncwd=\"{cwd}\"\nagent=\"{agent}\"\nprompt=\"p\"\nmaximum_context=100000\n"
        );
        toml::from_str(&src).expect("parse task file")
    }

    #[test]
    fn validate_task_file_profiles_rejects_maximum_context_for_non_supporting_profile_backend() {
        let project_root = tempdir("maximum-context-ollama-profile");
        fs::write(
            project_root.join(".ralphus.toml"),
            r#"
[agent.profiles.custom-ollama]
backend = "ollama"
"#,
        )
        .expect("write project config");

        let store = Store::open_in_memory().expect("open store");
        let file = task_file_with_agent_and_maximum_context(&project_root, "custom-ollama");
        let errors = validate_task_file_profiles_with(&store, "", &file, None);

        assert!(
            errors
                .iter()
                .any(|e| e.path.contains("maximum_context") && e.message.contains("ollama")),
            "{errors:?}"
        );
    }

    #[test]
    fn validate_task_file_profiles_rejects_maximum_context_for_claude_code_profile_backend() {
        let project_root = tempdir("maximum-context-claude-code-profile");
        fs::write(
            project_root.join(".ralphus.toml"),
            r#"
[agent.profiles.custom-claude]
backend = "claude-code"
"#,
        )
        .expect("write project config");

        let store = Store::open_in_memory().expect("open store");
        let file = task_file_with_agent_and_maximum_context(&project_root, "custom-claude");
        let errors = validate_task_file_profiles_with(&store, "", &file, None);

        assert!(
            errors
                .iter()
                .any(|e| e.path.contains("maximum_context") && e.message.contains("claude-code")),
            "{errors:?}"
        );
    }

    #[test]
    fn validate_task_file_profiles_allows_maximum_context_for_pi_profile_backend() {
        let project_root = tempdir("maximum-context-pi-profile");
        fs::write(
            project_root.join(".ralphus.toml"),
            r#"
[agent.profiles.custom-pi]
backend = "pi"
"#,
        )
        .expect("write project config");

        let store = Store::open_in_memory().expect("open store");
        let file = task_file_with_agent_and_maximum_context(&project_root, "custom-pi");
        let errors = validate_task_file_profiles_with(&store, "", &file, None);

        assert!(
            errors.iter().all(|e| !e.path.contains("maximum_context")),
            "{errors:?}"
        );
    }

    fn task_file_with_agent_and_auto_compact_threshold(cwd: &Path, agent: &str) -> TaskFile {
        let cwd = cwd.to_string_lossy().replace('\\', "/");
        let src = format!(
            "[[task]]\nname=\"t\"\nproject=\"unused\"\n[[task.cell]]\nid=\"work\"\ncwd=\"{cwd}\"\nagent=\"{agent}\"\nprompt=\"p\"\nauto_compact_threshold=80000\n"
        );
        toml::from_str(&src).expect("parse task file")
    }

    #[test]
    fn validate_task_file_profiles_allows_auto_compact_threshold_for_claude_code_profile_backend() {
        let project_root = tempdir("auto-compact-threshold-claude-code-profile");
        fs::write(
            project_root.join(".ralphus.toml"),
            r#"
[agent.profiles.custom-claude]
backend = "claude-code"
"#,
        )
        .expect("write project config");

        let store = Store::open_in_memory().expect("open store");
        let file = task_file_with_agent_and_auto_compact_threshold(&project_root, "custom-claude");
        let errors = validate_task_file_profiles_with(&store, "", &file, None);

        assert!(
            errors
                .iter()
                .all(|e| !e.path.contains("auto_compact_threshold")),
            "{errors:?}"
        );
    }

    #[test]
    fn validate_task_file_profiles_rejects_auto_compact_threshold_for_non_supporting_profile_backend()
     {
        let project_root = tempdir("auto-compact-threshold-ollama-profile");
        fs::write(
            project_root.join(".ralphus.toml"),
            r#"
[agent.profiles.custom-ollama]
backend = "ollama"
"#,
        )
        .expect("write project config");

        let store = Store::open_in_memory().expect("open store");
        let file = task_file_with_agent_and_auto_compact_threshold(&project_root, "custom-ollama");
        let errors = validate_task_file_profiles_with(&store, "", &file, None);

        assert!(
            errors
                .iter()
                .any(|e| e.path.contains("auto_compact_threshold") && e.message.contains("ollama")),
            "{errors:?}"
        );
    }

    fn task_file_with_agent_and_maximum_tool_output_tokens(cwd: &Path, agent: &str) -> TaskFile {
        let cwd = cwd.to_string_lossy().replace('\\', "/");
        let src = format!(
            "[[task]]\nname=\"t\"\nproject=\"unused\"\n[[task.cell]]\nid=\"work\"\ncwd=\"{cwd}\"\nagent=\"{agent}\"\nprompt=\"p\"\nmaximum_tool_output_tokens=40000\n"
        );
        toml::from_str(&src).expect("parse task file")
    }

    #[test]
    fn validate_task_file_profiles_allows_maximum_tool_output_tokens_for_claude_code_profile_backend()
     {
        let project_root = tempdir("tool-output-max-tokens-claude-code-profile");
        fs::write(
            project_root.join(".ralphus.toml"),
            r#"
[agent.profiles.custom-claude]
backend = "claude-code"
"#,
        )
        .expect("write project config");

        let store = Store::open_in_memory().expect("open store");
        let file =
            task_file_with_agent_and_maximum_tool_output_tokens(&project_root, "custom-claude");
        let errors = validate_task_file_profiles_with(&store, "", &file, None);

        assert!(
            errors
                .iter()
                .all(|e| !e.path.contains("maximum_tool_output_tokens")),
            "{errors:?}"
        );
    }

    #[test]
    fn validate_task_file_profiles_rejects_maximum_tool_output_tokens_for_non_supporting_profile_backend()
     {
        let project_root = tempdir("tool-output-max-tokens-ollama-profile");
        fs::write(
            project_root.join(".ralphus.toml"),
            r#"
[agent.profiles.custom-ollama]
backend = "ollama"
"#,
        )
        .expect("write project config");

        let store = Store::open_in_memory().expect("open store");
        let file =
            task_file_with_agent_and_maximum_tool_output_tokens(&project_root, "custom-ollama");
        let errors = validate_task_file_profiles_with(&store, "", &file, None);

        assert!(
            errors
                .iter()
                .any(|e| e.path.contains("maximum_tool_output_tokens")
                    && e.message.contains("ollama")),
            "{errors:?}"
        );
    }

    #[test]
    fn validate_task_file_profiles_allows_maximum_tool_output_tokens_for_pi_profile_backend() {
        let project_root = tempdir("tool-output-max-tokens-pi-profile");
        fs::write(
            project_root.join(".ralphus.toml"),
            r#"
[agent.profiles.custom-pi]
backend = "pi"
"#,
        )
        .expect("write project config");

        let store = Store::open_in_memory().expect("open store");
        let file = task_file_with_agent_and_maximum_tool_output_tokens(&project_root, "custom-pi");
        let errors = validate_task_file_profiles_with(&store, "", &file, None);

        assert!(
            errors
                .iter()
                .all(|e| !e.path.contains("maximum_tool_output_tokens")),
            "{errors:?}"
        );
    }

    fn task_file_with_review_agent(cwd: &Path, review_agent: &str) -> TaskFile {
        let cwd = cwd.to_string_lossy().replace('\\', "/");
        let src = format!(
            "[[review]]\nid=\"r\"\nagent=\"{review_agent}\"\n\
             [[task]]\nname=\"t\"\nproject=\"unused\"\n\
             [[task.cell]]\nid=\"work\"\ncwd=\"{cwd}\"\nreview=\"<<review:r>>\"\nprompt=\"p\"\n"
        );
        toml::from_str(&src).expect("parse task file")
    }

    #[test]
    fn validate_task_file_profiles_rejects_unresolvable_review_agent() {
        let project_root = tempdir("review-agent-unresolvable");
        let store = Store::open_in_memory().expect("open store");
        let file = task_file_with_review_agent(&project_root, "not-a-real-profile");
        let errors = validate_task_file_profiles_with(&store, "", &file, None);

        assert!(
            errors
                .iter()
                .any(|e| e.path == "review[0].agent" && e.message.contains("not-a-real-profile")),
            "{errors:?}"
        );
    }

    #[test]
    fn validate_task_file_profiles_allows_resolvable_review_agent() {
        let project_root = tempdir("review-agent-resolvable");
        fs::write(
            project_root.join(".ralphus.toml"),
            r#"
[agent.profiles.openrouter-deepseek]
backend = "claude-code"
"#,
        )
        .expect("write project config");

        let store = Store::open_in_memory().expect("open store");
        let file = task_file_with_review_agent(&project_root, "openrouter-deepseek");
        let errors = validate_task_file_profiles_with(&store, "", &file, None);

        assert!(
            errors.iter().all(|e| !e.path.starts_with("review[")),
            "{errors:?}"
        );
    }

    #[test]
    fn validate_task_file_profiles_rejects_when_any_candidate_in_a_list_lacks_system_prompt_support()
     {
        let project_root = tempdir("candidate-list-system-prompt-mixed");
        fs::write(
            project_root.join(".ralphus.toml"),
            r#"
[agent.profiles.custom-claude]
backend = "claude-code"

[agent.profiles.custom-ollama]
backend = "ollama"
"#,
        )
        .expect("write project config");
        let cwd = project_root.to_string_lossy().replace('\\', "/");
        let store = Store::open_in_memory().expect("open store");
        let file: TaskFile = toml::from_str(&format!(
            "[[task]]\nname=\"t\"\n[[task.cell]]\ncwd=\"{cwd}\"\nprompt=\"p\"\n\
             system_prompt=\"be terse\"\n\
             agent = [{{ agent = \"custom-claude\" }}, {{ agent = \"custom-ollama\" }}]\n"
        ))
        .expect("parse task file");

        let errors = validate_task_file_profiles_with(&store, "", &file, None);

        assert!(
            errors
                .iter()
                .any(|e| e.path.contains("system_prompt") && e.message.contains("ollama")),
            "{errors:?}"
        );
    }

    #[test]
    fn validate_task_file_profiles_accepts_a_candidate_list_when_every_entry_supports_system_prompt()
     {
        let project_root = tempdir("candidate-list-system-prompt-all-ok");
        fs::write(
            project_root.join(".ralphus.toml"),
            r#"
[agent.profiles.custom-claude]
backend = "claude-code"

[agent.profiles.custom-codex]
backend = "codex"
"#,
        )
        .expect("write project config");
        let cwd = project_root.to_string_lossy().replace('\\', "/");
        let store = Store::open_in_memory().expect("open store");
        let file: TaskFile = toml::from_str(&format!(
            "[[task]]\nname=\"t\"\n[[task.cell]]\ncwd=\"{cwd}\"\nprompt=\"p\"\n\
             system_prompt=\"be terse\"\n\
             agent = [{{ agent = \"custom-claude\" }}, {{ agent = \"custom-codex\" }}]\n"
        ))
        .expect("parse task file");

        let errors = validate_task_file_profiles_with(&store, "", &file, None);

        assert!(
            errors.iter().all(|e| !e.path.contains("system_prompt")),
            "{errors:?}"
        );
    }

    #[test]
    fn validate_task_file_profiles_rejects_an_unknown_name_inside_a_candidate_list() {
        let project_root = tempdir("candidate-list-unknown-name");
        let cwd = project_root.to_string_lossy().replace('\\', "/");
        let store = Store::open_in_memory().expect("open store");
        let file: TaskFile = toml::from_str(&format!(
            "[[task]]\nname=\"t\"\n[[task.cell]]\ncwd=\"{cwd}\"\nprompt=\"p\"\n\
             agent = [{{ agent = \"codex\" }}, {{ agent = \"not-a-real-profile\" }}]\n"
        ))
        .expect("parse task file");

        let errors = validate_task_file_profiles_with(&store, "", &file, None);

        assert!(
            errors
                .iter()
                .any(|e| e.path.contains("agent[1]") && e.message.contains("not-a-real-profile")),
            "{errors:?}"
        );
    }

    /// Test-only [`crate::runner::Runner`] that reports a fixed set of agent
    /// names as available, without spawning a real `ralphus-runner`
    /// subprocess (mirrors `guardian_merge::preflight_resolver_agent`'s
    /// injected-`&dyn Runner` seam).
    struct FakeAvailabilityRunner {
        available: Vec<&'static str>,
    }

    impl crate::runner::Runner for FakeAvailabilityRunner {
        fn run(&self, _spec: &crate::runner::RunnerSpec) -> crate::runner::RunnerResult {
            unimplemented!("not exercised by these tests")
        }

        fn preflight_agent(
            &self,
            agent: &str,
            _executable: Option<&str>,
            _machine: Option<&str>,
        ) -> Result<(), String> {
            if self.available.contains(&agent) {
                Ok(())
            } else {
                Err(format!("{agent} is not available"))
            }
        }
    }

    #[test]
    fn resolve_agent_candidate_lists_picks_the_first_available_and_collapses_to_single() {
        let store = Store::open_in_memory().expect("open store");
        let mut file: TaskFile = toml::from_str(concat!(
            "[[task]]\nname=\"t\"\n[[task.cell]]\ncwd=\"/r\"\nprompt=\"p\"\n",
            "agent = [\n",
            "    { agent = \"claude-code\", model = \"opus\" },\n",
            "    { agent = \"codex\", model = \"gpt-5\" },\n",
            "]\n",
        ))
        .expect("parse task file");
        let runner = FakeAvailabilityRunner {
            available: vec!["codex"],
        };

        let errors = resolve_agent_candidate_lists(&store, &mut file, &runner);

        assert!(errors.is_empty(), "{errors:?}");
        assert_eq!(
            file.task[0].cell[0].agent,
            Some(ralphus_core::schema::AgentSpec::Single("codex".to_string()))
        );
        assert_eq!(file.task[0].cell[0].model.as_deref(), Some("gpt-5"));
    }

    #[test]
    fn resolve_agent_candidate_lists_errors_naming_every_tried_candidate_when_none_are_available() {
        let store = Store::open_in_memory().expect("open store");
        let mut file: TaskFile = toml::from_str(concat!(
            "[[task]]\nname=\"t\"\n[[task.cell]]\ncwd=\"/r\"\nprompt=\"p\"\n",
            "agent = [{ agent = \"claude-code\" }, { agent = \"codex\" }]\n",
        ))
        .expect("parse task file");
        let runner = FakeAvailabilityRunner { available: vec![] };

        let errors = resolve_agent_candidate_lists(&store, &mut file, &runner);

        assert!(
            errors.iter().any(|e| e.path.ends_with(".agent")
                && e.message.contains("claude-code")
                && e.message.contains("codex")),
            "{errors:?}"
        );
    }

    #[test]
    fn resolve_agent_candidate_lists_collapses_a_task_level_list_with_no_cell_override() {
        let store = Store::open_in_memory().expect("open store");
        let mut file: TaskFile = toml::from_str(concat!(
            "[[task]]\nname=\"t\"\n",
            "agent = [{ agent = \"claude-code\" }, { agent = \"codex\" }]\n",
            "[[task.cell]]\ncwd=\"/r\"\nprompt=\"p\"\n",
        ))
        .expect("parse task file");
        let runner = FakeAvailabilityRunner {
            available: vec!["codex"],
        };

        let errors = resolve_agent_candidate_lists(&store, &mut file, &runner);

        assert!(errors.is_empty(), "{errors:?}");
        assert_eq!(
            file.task[0].agent,
            Some(ralphus_core::schema::AgentSpec::Single("codex".to_string()))
        );
        // The cell has no `agent` of its own -- untouched, still inherits.
        assert_eq!(file.task[0].cell[0].agent, None);
    }

    #[test]
    fn resolve_agent_candidate_lists_leaves_a_literal_agent_completely_untouched() {
        let store = Store::open_in_memory().expect("open store");
        let mut file: TaskFile = toml::from_str(
            "[[task]]\nname=\"t\"\n[[task.cell]]\ncwd=\"/r\"\nprompt=\"p\"\nagent=\"claude-code\"\n",
        )
        .expect("parse task file");
        // No candidate would resolve -- proves a literal `agent` never goes
        // through the availability walk at all.
        let runner = FakeAvailabilityRunner { available: vec![] };

        let errors = resolve_agent_candidate_lists(&store, &mut file, &runner);

        assert!(errors.is_empty(), "{errors:?}");
        assert_eq!(
            file.task[0].cell[0].agent,
            Some(ralphus_core::schema::AgentSpec::Single(
                "claude-code".to_string()
            ))
        );
    }

    #[test]
    fn parse_profile_file_rejects_reserved_name_and_native_executable() {
        let root = tempdir("invalid-root");
        let config = root.join(".ralphus.toml");
        fs::write(
            &config,
            r#"
[agent.profiles.claude]
backend = "codex"
executable = "codex"
"#,
        )
        .expect("write config");
        let err = parse_profile_file(&config).expect_err("reserved name error");
        assert!(err.contains("collides with a reserved built-in backend name"));

        fs::write(
            &config,
            r#"
[agent.profiles.custom]
backend = "ollama"
executable = "should-not-be-here"
"#,
        )
        .expect("rewrite config");
        let err = parse_profile_file(&config).expect_err("native executable error");
        assert!(err.contains("executable is only meaningful for claude-code, codex, pi, or raw"));
    }

    /// RAL-485: a database-stored backend-command override that carries
    /// arguments must be evaluated as a whole (compound-command detection),
    /// not by resolving only its first whitespace-delimited token --
    /// previously, a command like `rez-env foo -- claude` was silently
    /// checked as `resolve_executable("rez-env")`, which could pass or fail
    /// for reasons unrelated to whether `claude` itself was ever reachable.
    #[test]
    fn check_db_profiles_health_reports_a_compound_backend_command_as_skip() {
        let store = Store::open_in_memory().expect("open store");
        store
            .set_agent_backend_command("claude-code", "rez-env foo -- claude")
            .expect("set backend command override");

        let results = check_db_profiles_health(&store);

        let entry = results
            .iter()
            .find(|r| r.name == "agent-backend-command:claude-code")
            .expect("claude-code backend-command health entry");
        assert_eq!(entry.status, "skip", "{entry:?}");
        assert!(entry.detail.contains("rez-env foo -- claude"), "{entry:?}");
    }
}
