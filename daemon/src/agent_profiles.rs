//! Daemon-managed agent profiles (RAL-460).
//!
//! An agent profile is a named, reusable agent configuration -- a backend,
//! an optional default model, and a table of environment variables --
//! stored entirely in the daemon's own SQLite store (`agent_profiles` /
//! `agent_profile_env`, see `store.rs`'s schema block). There is no TOML
//! source for this anymore: `[agent.profiles.*]` is no longer read by
//! anything. Profiles are created/edited/removed through `POST`/`DELETE
//! /api/agent-profiles` (the board's admin-only Agent Profiles tab, or
//! `ralphus agent profile ...`), and are global across every project --
//! there is no more per-project override layering.
//!
//! Two rows are always present and `locked` ([`LOCKED_BUILTIN_PROFILES`]):
//! `"claude-code"` and `"codex"`, representing the built-in CLI-forking
//! backends. A locked row's `name`/`backend` can never change and it can
//! never be deleted; the *only* thing an admin can change on it is
//! `executable` (via [`Store::set_locked_agent_profile_executable`]), which
//! replaces the old `RALPHUS_CLAUDE_COMMAND`/`RALPHUS_CODEX_COMMAND`
//! env-var overrides for daemon-run cells (`runner/src/claude_code_backend.rs`,
//! `runner/src/codex_backend.rs`, and the daemon-side resume/reattach paths
//! in `server.rs`/`terminal_relay.rs` now resolve through here instead of
//! reading those env vars directly). `pi`'s `RALPHUS_PI_COMMAND` is
//! untouched -- it is not yet one of the seeded backends.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use ralphus_core::schema::TaskFile;
use ralphus_core::validate::{ErrorKind, ValidationError};
use serde::{Deserialize, Serialize};

use crate::store::{Result as StoreResult, Store, now_ms};

pub const RAW_BACKEND: &str = "raw";

/// Every backend a profile may declare. Mirrors `RESERVED_AGENT_NAMES`
/// (`core/src/schema.rs`) minus nothing -- every reserved name is also a
/// valid profile backend, since a profile is how you *customize* a built-in
/// backend's executable/model/env, not a way to invent a new one.
pub const PROFILE_BACKENDS: &[&str] = &[
    "claude",
    "claude-code",
    "codex",
    "pi",
    "ollama",
    "anthropic",
    "raw",
];

/// The permanent, locked agent-profile rows every daemon seeds on startup
/// (`Store::init_schema`) -- `(name, backend, default executable)`. `name`
/// doubles as the row's fixed `backend`; only `executable` is ever mutable
/// on these rows afterward. `pi` is intentionally not included yet -- its
/// existing `RALPHUS_PI_COMMAND` env-var path is untouched.
pub const LOCKED_BUILTIN_PROFILES: &[(&str, &str, &str)] = &[
    ("claude-code", "claude-code", "claude"),
    ("codex", "codex", "codex"),
];

/// How one `agent_profile_env` row's `value` should be interpreted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EnvValueKind {
    /// `value` is the raw value to set, stored plaintext but redacted in
    /// every API/UI response and registered with `crate::redact` the moment
    /// it is written (see [`Store::upsert_agent_profile`]).
    Literal,
    /// `value` is the *name* of another environment variable to resolve
    /// from at cell-run time (mirrors the old TOML `from_env` indirection) --
    /// re-resolved on every [`resolve_agent`] call, since the target
    /// variable's value can change without the profile itself changing.
    Link,
}

impl EnvValueKind {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Literal => "literal",
            Self::Link => "link",
        }
    }
}

/// One `env` row of a stored agent profile.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AgentProfileEnvVar {
    pub key: String,
    pub kind: EnvValueKind,
    /// Literal: the raw value. Link: the target environment variable's name
    /// (never the resolved value itself).
    pub value: String,
}

/// A stored agent profile, as read back from the `agent_profiles` table --
/// unredacted, for internal (resolution) use only. API responses go through
/// [`AgentProfileView`] instead.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentProfile {
    pub name: String,
    pub backend: String,
    pub executable: Option<String>,
    pub default_model: Option<String>,
    /// Whether this is one of [`LOCKED_BUILTIN_PROFILES`] -- fixed
    /// name/backend, never deletable, only `executable` is mutable.
    pub locked: bool,
    pub env: Vec<AgentProfileEnvVar>,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
}

/// A stored profile's `env` row, redacted for an API/UI response.
#[derive(Debug, Clone, Serialize)]
pub struct AgentProfileEnvVarView {
    pub key: String,
    pub kind: EnvValueKind,
    /// Literal: a masked placeholder (see [`crate::redact`]). Link: the
    /// real target variable name -- that is a pointer, not a secret.
    pub value: String,
    pub redacted: bool,
}

/// [`AgentProfile`], redacted for an API/UI response -- see
/// [`redact_agent_profile`].
#[derive(Debug, Clone, Serialize)]
pub struct AgentProfileView {
    pub name: String,
    pub backend: String,
    pub executable: Option<String>,
    pub default_model: Option<String>,
    pub locked: bool,
    pub env: Vec<AgentProfileEnvVarView>,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
}

/// Masks every literal `env` value so a `GET`/`POST` response never carries
/// a secret in the clear -- link values pass through unmasked, since a
/// target variable *name* is not itself sensitive (mirrors RAL-264's
/// existing value-based, not name-based, secret model).
#[must_use]
pub fn redact_agent_profile(p: &AgentProfile) -> AgentProfileView {
    AgentProfileView {
        name: p.name.clone(),
        backend: p.backend.clone(),
        executable: p.executable.clone(),
        default_model: p.default_model.clone(),
        locked: p.locked,
        env: p
            .env
            .iter()
            .map(|v| match v.kind {
                EnvValueKind::Literal => AgentProfileEnvVarView {
                    key: v.key.clone(),
                    kind: v.kind,
                    value: crate::redact::REDACTED.to_string(),
                    redacted: true,
                },
                EnvValueKind::Link => AgentProfileEnvVarView {
                    key: v.key.clone(),
                    kind: v.kind,
                    value: v.value.clone(),
                    redacted: false,
                },
            })
            .collect(),
        created_at_ms: p.created_at_ms,
        updated_at_ms: p.updated_at_ms,
    }
}

/// What a `agent = "..."` name resolves to -- a built-in backend, or a
/// stored profile (locked or custom).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedAgentSelection {
    pub backend: String,
    pub executable: Option<String>,
    pub model: Option<String>,
    pub env: BTreeMap<String, String>,
    /// True for a genuinely custom (non-locked) profile -- false for a bare
    /// built-in name and for a locked row, since both are backends `core`'s
    /// own offline validator already recognizes via `RESERVED_AGENT_NAMES`.
    pub custom_profile: bool,
    /// The resolved values of every `env` entry (literal or link) --
    /// registered with `crate::redact` so none of them land in durable pane
    /// text / failure `detail`s.
    pub secret_values: BTreeSet<String>,
}

/// Whether `agent` is a backend ralphus talks to directly (no external CLI
/// process to fork).
#[must_use]
pub fn is_native_backend(agent: &str) -> bool {
    matches!(agent, "claude" | "anthropic" | "ollama")
}

/// Normalizes a built-in backend name/alias, or `None` if `agent` names
/// neither a built-in nor (necessarily) a stored profile.
#[must_use]
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

/// Resolves `agent` against the daemon's own agent-profile store, falling
/// back to a bare built-in backend name/alias. Profiles are global now
/// (no more per-project `.ralphus.toml` layering), so no `cwd` is needed.
///
/// # Errors
/// Returns an error string when `agent` names neither a stored profile nor
/// a built-in backend, or when a `link`-kind env entry's target variable is
/// not set in the daemon process's own environment.
/// [`resolve_agent`] for a call site with no `Store` handle in reach (see
/// `guardian_merge::resolve_resolver_agent_from_config`'s doc comment for
/// the one place this is used) -- resolves a bare built-in backend
/// name/alias only. A name that would resolve to a stored **custom**
/// profile is indistinguishable from an unknown name here, since profiles
/// live only in the store now and there is no TOML fallback left to check
/// instead; this is a narrow, documented capability gap on that one
/// best-effort call site, not a general resolution path.
///
/// # Errors
/// Returns an error string when `agent` names neither a built-in backend
/// nor its alias.
pub fn resolve_builtin_agent_only(agent: &str) -> Result<ResolvedAgentSelection, String> {
    let Some(backend) = normalize_builtin_agent(agent) else {
        return Err(format!(
            "unknown agent \"{agent}\": not a built-in backend (custom agent profiles cannot be \
             resolved from this call site -- see resolve_builtin_agent_only's doc comment)"
        ));
    };
    Ok(ResolvedAgentSelection {
        backend: backend.to_string(),
        executable: None,
        model: None,
        env: BTreeMap::new(),
        custom_profile: false,
        secret_values: BTreeSet::new(),
    })
}

pub fn resolve_agent(agent: &str, store: &Store) -> Result<ResolvedAgentSelection, String> {
    let profile = store.get_agent_profile(agent).map_err(|e| e.to_string())?;
    let Some(profile) = profile else {
        let Some(backend) = normalize_builtin_agent(agent) else {
            return Err(format!(
                "unknown agent \"{agent}\": not a configured agent profile and not a built-in backend"
            ));
        };
        return Ok(ResolvedAgentSelection {
            backend: backend.to_string(),
            executable: None,
            model: None,
            env: BTreeMap::new(),
            custom_profile: false,
            secret_values: BTreeSet::new(),
        });
    };
    let mut env = BTreeMap::new();
    let mut secret_values = BTreeSet::new();
    for var in &profile.env {
        let resolved = match var.kind {
            EnvValueKind::Literal => var.value.clone(),
            EnvValueKind::Link => std::env::var(&var.value).map_err(|_| {
                format!(
                    "agent profile \"{agent}\" requires environment variable {:?}, but it is not set in the daemon process environment. \
                    Set {} in the environment the `ralphus-daemon serve` process runs in (not just your shell), then restart the daemon.",
                    var.value, var.value
                )
            })?,
        };
        secret_values.insert(resolved.clone());
        env.insert(var.key.clone(), resolved);
    }
    // RAL-264, extended: every resolved env value (literal or link) is
    // treated as secret-shaped now, not just `link` resolutions -- see the
    // module doc's "Secrets" decision. Also registered at write time
    // (`Store::upsert_agent_profile`); registering again here is
    // idempotent and covers a `link` target whose value changed since.
    crate::redact::register_all(secret_values.iter().cloned());
    Ok(ResolvedAgentSelection {
        backend: profile.backend,
        executable: profile.executable,
        model: profile.default_model,
        env,
        custom_profile: !profile.locked,
        secret_values,
    })
}

pub fn validate_task_file_profiles(
    store: &Store,
    _raw_toml: &str,
    file: &TaskFile,
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
            let mut selections = Vec::with_capacity(names.len());
            let mut resolution_failed = false;
            for (candidate_idx, name) in names.iter().enumerate() {
                match resolve_agent(name, store) {
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

    // `[[review]].agent` gets the same treatment as a cell's `agent`.
    // Profiles are global now, so (unlike before) this no longer needs to
    // find a matching cell's cwd first -- every review that sets `agent`
    // is checked directly.
    for (review_idx, review) in file.review.iter().enumerate() {
        let Some(agent) = review.agent.as_deref() else {
            continue;
        };
        if let Err(message) = resolve_agent(agent, store) {
            errors.push(ValidationError {
                path: format!("review[{review_idx}].agent"),
                kind: ErrorKind::InvalidValue,
                message,
                line: None,
            });
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
            if let Ok(selection) = resolve_agent(agent, store) {
                cell.model = selection.model;
            }
        }
    }

    for review in &mut file.review {
        if review.model.is_some() {
            continue;
        }
        let Some(agent) = review.agent.as_deref() else {
            continue;
        };
        if let Ok(selection) = resolve_agent(agent, store) {
            review.model = selection.model;
        }
    }
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
/// `agent`.
///
/// Every candidate name has already been confirmed to exist by
/// [`validate_task_file_profiles`] (the caller runs that first and rejects
/// the submission outright on any unknown name), so a resolution failure
/// here is only ever "not installed/configured on this host," never a typo.
pub fn resolve_agent_candidate_lists(
    store: &Store,
    file: &mut TaskFile,
    runner: &dyn crate::runner::Runner,
) -> Vec<ValidationError> {
    let mut errors = Vec::new();
    for (task_idx, task) in file.task.iter_mut().enumerate() {
        if let Some(ralphus_core::schema::AgentSpec::Candidates(candidates)) = &task.agent {
            match pick_available_candidate(runner, candidates, store) {
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
            match pick_available_candidate(runner, candidates, store) {
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
    runner: &dyn crate::runner::Runner,
    candidates: &[ralphus_core::schema::AgentCandidate],
    store: &Store,
) -> Option<ralphus_core::schema::AgentCandidate> {
    candidates
        .iter()
        .find(|c| {
            resolve_agent(&c.agent, store).is_ok_and(|selection| {
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
/// Runs entirely inside the daemon process (`Store::list_agent_profiles`,
/// `std::env::var`/[`resolve_executable`]) so the result reflects the
/// daemon's actual environment/PATH -- not whatever shell happened to run
/// the `ralphus` CLI, which can silently differ from the environment
/// `ralphus-daemon serve` was started in.
#[derive(Debug, Clone, Serialize)]
pub struct ProfileHealthResult {
    pub name: String,
    /// `"pass"`, `"fail"`, or `"skip"` (a multi-word `executable` -- see
    /// [`is_single_word_command`] -- is not one resolvable PATH token, so it
    /// is deliberately not checked rather than reported as a false
    /// failure).
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

/// Whether `s` is a single bare token (no whitespace) -- the only shape
/// [`resolve_executable`]'s PATH/PATHEXT lookup can meaningfully check. A
/// multi-word value (e.g. `"wsl.exe claude"`) names a command *line*, not
/// one resolvable program, so [`check_profiles_health`] skips it rather
/// than reporting a false failure.
#[must_use]
fn is_single_word_command(s: &str) -> bool {
    let s = s.trim();
    !s.is_empty() && !s.chars().any(char::is_whitespace)
}

/// Health-checks every stored agent profile's `executable`. Flat over the
/// whole (global) profile store now -- no more per-project-root repetition,
/// since profiles no longer have per-project TOML layers.
#[must_use]
pub fn check_profiles_health(store: &Store) -> Vec<ProfileHealthResult> {
    let profiles = match store.list_agent_profiles() {
        Ok(p) => p,
        Err(e) => {
            return vec![ProfileHealthResult {
                name: "agent-profiles".to_string(),
                status: "fail",
                detail: e.to_string(),
            }];
        }
    };
    let mut results = Vec::with_capacity(profiles.len());
    for profile in profiles {
        let check_name = format!("agent-profile:{}", profile.name);
        // A `link`-kind env entry whose target variable isn't set in the
        // daemon process's own environment makes this profile unusable --
        // `resolve_agent` already does exactly this check (and, as a
        // beneficial side effect, registers any resolved secret with
        // `crate::redact`), so reuse it rather than re-walking `env` here.
        if let Err(e) = resolve_agent(&profile.name, store) {
            results.push(ProfileHealthResult {
                name: check_name,
                status: "fail",
                detail: e,
            });
            continue;
        }
        let Some(executable) = profile.executable.as_deref().filter(|e| !e.is_empty()) else {
            results.push(ProfileHealthResult {
                name: check_name,
                status: "pass",
                detail: format!("backend={}", profile.backend),
            });
            continue;
        };
        if !is_single_word_command(executable) {
            results.push(ProfileHealthResult {
                name: check_name,
                status: "skip",
                detail: format!(
                    "backend={} executable={executable:?} is a multi-word command; not checked \
                     against PATH (only a single bare program name can be resolved this way)",
                    profile.backend
                ),
            });
            continue;
        }
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

/// Outcome of [`Store::set_locked_agent_profile_executable`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SetLockedExecutableOutcome {
    Updated,
    NotFound,
    /// The named row exists but is not locked -- use
    /// [`Store::upsert_agent_profile`] for a custom profile instead.
    NotLocked,
}

/// Outcome of [`Store::delete_agent_profile`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeleteAgentProfileOutcome {
    Deleted,
    NotFound,
    /// A locked (built-in-backend) row can never be deleted.
    Locked,
}

impl Store {
    /// Every stored agent profile (locked built-ins first, then custom
    /// profiles alphabetically), each with its full `env` table.
    ///
    /// # Errors
    /// Propagates any SQLite failure.
    pub fn list_agent_profiles(&self) -> StoreResult<Vec<AgentProfile>> {
        let mut stmt = self.conn.prepare(
            "SELECT name, backend, executable, default_model, locked, created_at_ms, updated_at_ms
             FROM agent_profiles ORDER BY locked DESC, name",
        )?;
        let mut profiles: Vec<AgentProfile> = stmt
            .query_map([], |r| {
                Ok(AgentProfile {
                    name: r.get(0)?,
                    backend: r.get(1)?,
                    executable: r.get(2)?,
                    default_model: r.get(3)?,
                    locked: r.get::<_, i64>(4)? != 0,
                    env: Vec::new(),
                    created_at_ms: r.get(5)?,
                    updated_at_ms: r.get(6)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        let mut env_stmt = self.conn.prepare(
            "SELECT profile_name, key, kind, value FROM agent_profile_env ORDER BY profile_name, key",
        )?;
        let rows = env_stmt.query_map([], |r| {
            let profile_name: String = r.get(0)?;
            let key: String = r.get(1)?;
            let kind_raw: String = r.get(2)?;
            let value: String = r.get(3)?;
            Ok((profile_name, key, kind_raw, value))
        })?;
        let mut env_by_profile: BTreeMap<String, Vec<AgentProfileEnvVar>> = BTreeMap::new();
        for row in rows {
            let (profile_name, key, kind_raw, value) = row?;
            let kind = if kind_raw == "link" {
                EnvValueKind::Link
            } else {
                EnvValueKind::Literal
            };
            env_by_profile
                .entry(profile_name)
                .or_default()
                .push(AgentProfileEnvVar { key, kind, value });
        }
        for profile in &mut profiles {
            if let Some(env) = env_by_profile.remove(&profile.name) {
                profile.env = env;
            }
        }
        Ok(profiles)
    }

    /// One profile by name (case-insensitive), or `None`.
    ///
    /// # Errors
    /// Propagates any SQLite failure.
    pub fn get_agent_profile(&self, name: &str) -> StoreResult<Option<AgentProfile>> {
        Ok(self
            .list_agent_profiles()?
            .into_iter()
            .find(|p| p.name.eq_ignore_ascii_case(name.trim())))
    }

    /// Create or update a **custom** agent profile (upsert on `name`).
    /// Refuses to touch a locked (built-in-backend) row -- returns
    /// `Ok(false)` in that case; see
    /// [`Store::set_locked_agent_profile_executable`] for the one thing a
    /// locked row can still be changed through. Callers are expected to
    /// have already validated `backend`/`executable`/reserved-name
    /// collisions (mirrors `register_machine`'s split in `server.rs`: the
    /// route validates, the store just writes).
    ///
    /// # Errors
    /// Propagates any SQLite failure.
    pub fn upsert_agent_profile(
        &self,
        name: &str,
        backend: &str,
        executable: Option<&str>,
        default_model: Option<&str>,
        env: &[AgentProfileEnvVar],
    ) -> StoreResult<bool> {
        let name = name.trim();
        if let Some(existing) = self.get_agent_profile(name)? {
            if existing.locked {
                return Ok(false);
            }
        }
        let now = now_ms();
        self.conn.execute(
            "INSERT INTO agent_profiles(name, backend, executable, default_model, locked, created_at_ms, updated_at_ms)
             VALUES(?,?,?,?,0,?,?)
             ON CONFLICT(name) DO UPDATE SET
                backend=excluded.backend,
                executable=excluded.executable,
                default_model=excluded.default_model,
                updated_at_ms=excluded.updated_at_ms",
            rusqlite::params![name, backend, executable, default_model, now, now],
        )?;
        self.conn.execute(
            "DELETE FROM agent_profile_env WHERE profile_name = ?",
            rusqlite::params![name],
        )?;
        for var in env {
            self.conn.execute(
                "INSERT INTO agent_profile_env(profile_name, key, kind, value) VALUES(?,?,?,?)",
                rusqlite::params![name, var.key, var.kind.as_str(), var.value],
            )?;
            // RAL-264, extended: register at write time too (not just at
            // resolution) so a secret is scrubbed from durable pane text
            // from the moment it's saved, even before the profile is ever
            // used. `link`-kind values are re-registered again at
            // resolution time, since the target variable can change later.
            if var.kind == EnvValueKind::Literal {
                crate::redact::register_all(std::iter::once(var.value.clone()));
            } else if let Ok(resolved) = std::env::var(&var.value) {
                crate::redact::register_all(std::iter::once(resolved));
            }
        }
        crate::rlog!(
            INFO,
            "ralphus [store] agent profile {name:?} registered backend={backend}"
        );
        let _ = self.cartographer_log(crate::cartographer::CartographerEntry {
            level: crate::logging::LogLevel::INFO,
            source: "store",
            message: "agent profile registered",
            scope: Some("agent"),
            squad_id: None,
            guardian_id: None,
            cell_id: None,
            task: None,
            log_path: None,
            payload: serde_json::json!({"name": name, "backend": backend}),
            admin_only: false,
        });
        Ok(true)
    }

    /// Changes a **locked** profile row's `executable` -- the only field a
    /// locked (built-in-backend) row can ever have changed on it.
    ///
    /// # Errors
    /// Propagates any SQLite failure.
    pub fn set_locked_agent_profile_executable(
        &self,
        name: &str,
        executable: &str,
    ) -> StoreResult<SetLockedExecutableOutcome> {
        let name = name.trim();
        let Some(existing) = self.get_agent_profile(name)? else {
            return Ok(SetLockedExecutableOutcome::NotFound);
        };
        if !existing.locked {
            return Ok(SetLockedExecutableOutcome::NotLocked);
        }
        self.conn.execute(
            "UPDATE agent_profiles SET executable=?, updated_at_ms=? WHERE name=?",
            rusqlite::params![executable, now_ms(), name],
        )?;
        crate::rlog!(
            INFO,
            "ralphus [store] built-in agent profile {name:?} executable set to {executable:?}"
        );
        let _ = self.cartographer_log(crate::cartographer::CartographerEntry {
            level: crate::logging::LogLevel::INFO,
            source: "store",
            message: "built-in agent profile executable changed",
            scope: Some("agent"),
            squad_id: None,
            guardian_id: None,
            cell_id: None,
            task: None,
            log_path: None,
            payload: serde_json::json!({"name": name, "executable": executable}),
            admin_only: false,
        });
        Ok(SetLockedExecutableOutcome::Updated)
    }

    /// Remove a **custom** agent profile. A locked row is never deleted --
    /// returns [`DeleteAgentProfileOutcome::Locked`] instead.
    ///
    /// Deliberately does **not** check whether any stored squad still
    /// references the profile name: that squad already resolved its agent
    /// when it was submitted, and a historical squad's record should not
    /// block cleaning up the registry. A *new* submission naming a removed
    /// profile fails validation with an "unknown agent" error.
    ///
    /// # Errors
    /// Propagates any SQLite failure.
    pub fn delete_agent_profile(&self, name: &str) -> StoreResult<DeleteAgentProfileOutcome> {
        let name = name.trim();
        let Some(existing) = self.get_agent_profile(name)? else {
            return Ok(DeleteAgentProfileOutcome::NotFound);
        };
        if existing.locked {
            return Ok(DeleteAgentProfileOutcome::Locked);
        }
        self.conn.execute(
            "DELETE FROM agent_profiles WHERE name = ?",
            rusqlite::params![name],
        )?;
        crate::rlog!(INFO, "ralphus [store] agent profile {name:?} removed");
        let _ = self.cartographer_log(crate::cartographer::CartographerEntry {
            level: crate::logging::LogLevel::INFO,
            source: "store",
            message: "agent profile removed",
            scope: Some("agent"),
            squad_id: None,
            guardian_id: None,
            cell_id: None,
            task: None,
            log_path: None,
            payload: serde_json::json!({"name": name}),
            admin_only: false,
        });
        Ok(DeleteAgentProfileOutcome::Deleted)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> Store {
        Store::open_in_memory().expect("open store")
    }

    #[test]
    fn builtin_agent_aliases_normalize() {
        assert_eq!(normalize_builtin_agent("claude-cli"), Some("claude-code"));
        assert_eq!(normalize_builtin_agent("codex-cli"), Some("codex"));
        assert_eq!(normalize_builtin_agent("unknown"), None);
    }

    #[test]
    fn locked_builtin_profiles_are_seeded_on_open() {
        let store = store();
        let claude_code = store
            .get_agent_profile("claude-code")
            .expect("query")
            .expect("seeded");
        assert!(claude_code.locked);
        assert_eq!(claude_code.executable.as_deref(), Some("claude"));
        let codex = store
            .get_agent_profile("codex")
            .expect("query")
            .expect("seeded");
        assert!(codex.locked);
        assert_eq!(codex.executable.as_deref(), Some("codex"));
    }

    #[test]
    fn locked_profile_cannot_be_upserted_or_deleted() {
        let store = store();
        let committed = store
            .upsert_agent_profile("claude-code", "claude-code", Some("my-claude"), None, &[])
            .expect("upsert call");
        assert!(!committed, "a locked row must refuse the general upsert");
        // Untouched.
        let claude_code = store
            .get_agent_profile("claude-code")
            .expect("query")
            .expect("still present");
        assert_eq!(claude_code.executable.as_deref(), Some("claude"));
        assert_eq!(
            store.delete_agent_profile("claude-code").expect("delete"),
            DeleteAgentProfileOutcome::Locked
        );
    }

    #[test]
    fn locked_profile_executable_can_be_changed_through_dedicated_method() {
        let store = store();
        assert_eq!(
            store
                .set_locked_agent_profile_executable("codex", "my-codex-fork")
                .expect("set"),
            SetLockedExecutableOutcome::Updated
        );
        let codex = store
            .get_agent_profile("codex")
            .expect("query")
            .expect("present");
        assert_eq!(codex.executable.as_deref(), Some("my-codex-fork"));
    }

    #[test]
    fn set_locked_executable_refuses_a_non_locked_row() {
        let store = store();
        store
            .upsert_agent_profile("my-custom", "claude-code", None, None, &[])
            .expect("upsert");
        assert_eq!(
            store
                .set_locked_agent_profile_executable("my-custom", "x")
                .expect("set"),
            SetLockedExecutableOutcome::NotLocked
        );
    }

    #[test]
    fn custom_profile_round_trips_through_upsert_get_delete() {
        let store = store();
        let env = vec![
            AgentProfileEnvVar {
                key: "FEATURE_FLAG".to_string(),
                kind: EnvValueKind::Literal,
                value: "enabled".to_string(),
            },
            AgentProfileEnvVar {
                key: "PATH_COPY".to_string(),
                kind: EnvValueKind::Link,
                value: "PATH".to_string(),
            },
        ];
        let committed = store
            .upsert_agent_profile("my-openrouter", "claude-code", None, Some("gpt-5"), &env)
            .expect("upsert");
        assert!(committed);
        let profile = store
            .get_agent_profile("my-openrouter")
            .expect("query")
            .expect("present");
        assert!(!profile.locked);
        assert_eq!(profile.backend, "claude-code");
        assert_eq!(profile.default_model.as_deref(), Some("gpt-5"));
        assert_eq!(profile.env.len(), 2);

        assert_eq!(
            store.delete_agent_profile("my-openrouter").expect("delete"),
            DeleteAgentProfileOutcome::Deleted
        );
        assert!(
            store
                .get_agent_profile("my-openrouter")
                .expect("query")
                .is_none()
        );
    }

    #[test]
    fn redact_agent_profile_masks_literal_but_not_link_values() {
        let profile = AgentProfile {
            name: "custom".to_string(),
            backend: "pi".to_string(),
            executable: None,
            default_model: None,
            locked: false,
            env: vec![
                AgentProfileEnvVar {
                    key: "API_KEY".to_string(),
                    kind: EnvValueKind::Literal,
                    value: "sk-secret".to_string(),
                },
                AgentProfileEnvVar {
                    key: "TOKEN".to_string(),
                    kind: EnvValueKind::Link,
                    value: "MY_TOKEN_VAR".to_string(),
                },
            ],
            created_at_ms: 0,
            updated_at_ms: 0,
        };
        let view = redact_agent_profile(&profile);
        let literal = view.env.iter().find(|v| v.key == "API_KEY").expect("row");
        assert!(literal.redacted);
        assert_ne!(literal.value, "sk-secret");
        let link = view.env.iter().find(|v| v.key == "TOKEN").expect("row");
        assert!(!link.redacted);
        assert_eq!(link.value, "MY_TOKEN_VAR");
    }

    #[test]
    fn resolve_agent_finds_locked_builtin_with_customized_executable() {
        let store = store();
        store
            .set_locked_agent_profile_executable("claude-code", "my-claude-fork")
            .expect("set");
        let selection = resolve_agent("claude-code", &store).expect("resolve");
        assert_eq!(selection.backend, "claude-code");
        assert_eq!(selection.executable.as_deref(), Some("my-claude-fork"));
        assert!(
            !selection.custom_profile,
            "a locked row must not be treated as a custom profile"
        );
    }

    #[test]
    fn resolve_agent_finds_custom_profile_and_falls_back_to_builtin() {
        let store = store();
        store
            .upsert_agent_profile("my-ollama", "ollama", None, Some("qwen3:8b"), &[])
            .expect("upsert");
        let selection = resolve_agent("my-ollama", &store).expect("resolve");
        assert_eq!(selection.backend, "ollama");
        assert_eq!(selection.model.as_deref(), Some("qwen3:8b"));
        assert!(selection.custom_profile);

        let builtin = resolve_agent("ollama", &store).expect("resolve builtin");
        assert!(!builtin.custom_profile);
    }

    #[test]
    fn resolve_agent_resolves_link_env_from_process_environment() {
        let store = store();
        store
            .upsert_agent_profile(
                "shared",
                "raw",
                Some("my-raw-runner"),
                None,
                &[AgentProfileEnvVar {
                    key: "PATH_COPY".to_string(),
                    kind: EnvValueKind::Link,
                    value: "PATH".to_string(),
                }],
            )
            .expect("upsert");
        let selection = resolve_agent("shared", &store).expect("resolve");
        assert_eq!(
            selection.env.get("PATH_COPY").map(String::as_str),
            std::env::var("PATH").ok().as_deref()
        );
        assert!(
            selection
                .secret_values
                .contains(std::env::var("PATH").ok().unwrap_or_default().as_str())
        );
    }

    #[test]
    fn resolve_agent_rejects_unknown_name() {
        let store = store();
        let err = resolve_agent("not-a-real-profile", &store).expect_err("should fail");
        assert!(err.contains("not-a-real-profile"));
    }

    #[test]
    fn profile_default_model_applies_unless_task_or_cell_declares_one() {
        let store = store();
        store
            .upsert_agent_profile(
                "deepseek",
                "pi",
                None,
                Some("openrouter/deepseek/deepseek-v4-flash-0731"),
                &[],
            )
            .expect("upsert");
        let mut file: TaskFile = toml::from_str(
            "[[task]]\nname=\"t\"\nagent=\"deepseek\"\n[[task.cell]]\nid=\"default\"\nprompt=\"p\"\n[[task.cell]]\nid=\"override\"\nmodel=\"openrouter/other\"\nprompt=\"p\"\n"
        )
        .expect("parse task file");

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

    fn task_file_with_agent_and_system_prompt(agent: &str) -> TaskFile {
        let src = format!(
            "[[task]]\nname=\"t\"\nproject=\"unused\"\n[[task.cell]]\nid=\"work\"\nagent=\"{agent}\"\nprompt=\"p\"\nsystem_prompt=\"be terse\"\nsystem_prompt_position=\"append\"\n"
        );
        toml::from_str(&src).expect("parse task file")
    }

    #[test]
    fn validate_task_file_profiles_rejects_system_prompt_for_non_supporting_profile_backend() {
        let store = store();
        store
            .upsert_agent_profile("custom-ollama", "ollama", None, None, &[])
            .expect("upsert");
        let file = task_file_with_agent_and_system_prompt("custom-ollama");
        let errors = validate_task_file_profiles(&store, "", &file);

        assert!(
            errors
                .iter()
                .any(|e| e.path.contains("system_prompt") && e.message.contains("ollama")),
            "{errors:?}"
        );
    }

    #[test]
    fn validate_task_file_profiles_allows_system_prompt_for_claude_code_profile_backend() {
        let store = store();
        store
            .upsert_agent_profile("custom-claude", "claude-code", None, None, &[])
            .expect("upsert");
        let file = task_file_with_agent_and_system_prompt("custom-claude");
        let errors = validate_task_file_profiles(&store, "", &file);

        assert!(
            errors.iter().all(|e| !e.path.contains("system_prompt")),
            "{errors:?}"
        );
    }

    fn task_file_with_agent_and_maximum_context(agent: &str) -> TaskFile {
        let src = format!(
            "[[task]]\nname=\"t\"\nproject=\"unused\"\n[[task.cell]]\nid=\"work\"\nagent=\"{agent}\"\nprompt=\"p\"\nmaximum_context=100000\n"
        );
        toml::from_str(&src).expect("parse task file")
    }

    #[test]
    fn validate_task_file_profiles_rejects_maximum_context_for_non_supporting_profile_backend() {
        let store = store();
        store
            .upsert_agent_profile("custom-ollama", "ollama", None, None, &[])
            .expect("upsert");
        let file = task_file_with_agent_and_maximum_context("custom-ollama");
        let errors = validate_task_file_profiles(&store, "", &file);

        assert!(
            errors
                .iter()
                .any(|e| e.path.contains("maximum_context") && e.message.contains("ollama")),
            "{errors:?}"
        );
    }

    #[test]
    fn validate_task_file_profiles_rejects_maximum_context_for_claude_code_profile_backend() {
        let store = store();
        store
            .upsert_agent_profile("custom-claude", "claude-code", None, None, &[])
            .expect("upsert");
        let file = task_file_with_agent_and_maximum_context("custom-claude");
        let errors = validate_task_file_profiles(&store, "", &file);

        assert!(
            errors
                .iter()
                .any(|e| e.path.contains("maximum_context") && e.message.contains("claude-code")),
            "{errors:?}"
        );
    }

    #[test]
    fn validate_task_file_profiles_allows_maximum_context_for_pi_profile_backend() {
        let store = store();
        store
            .upsert_agent_profile("custom-pi", "pi", None, None, &[])
            .expect("upsert");
        let file = task_file_with_agent_and_maximum_context("custom-pi");
        let errors = validate_task_file_profiles(&store, "", &file);

        assert!(
            errors.iter().all(|e| !e.path.contains("maximum_context")),
            "{errors:?}"
        );
    }

    fn task_file_with_review_agent(review_agent: &str) -> TaskFile {
        let src = format!(
            "[[review]]\nid=\"r\"\nagent=\"{review_agent}\"\n\
             [[task]]\nname=\"t\"\nproject=\"unused\"\n\
             [[task.cell]]\nid=\"work\"\nreview=\"<<review:r>>\"\nprompt=\"p\"\n"
        );
        toml::from_str(&src).expect("parse task file")
    }

    #[test]
    fn validate_task_file_profiles_rejects_unresolvable_review_agent() {
        let store = store();
        let file = task_file_with_review_agent("not-a-real-profile");
        let errors = validate_task_file_profiles(&store, "", &file);

        assert!(
            errors
                .iter()
                .any(|e| e.path == "review[0].agent" && e.message.contains("not-a-real-profile")),
            "{errors:?}"
        );
    }

    #[test]
    fn validate_task_file_profiles_allows_resolvable_review_agent() {
        let store = store();
        store
            .upsert_agent_profile("openrouter-deepseek", "claude-code", None, None, &[])
            .expect("upsert");
        let file = task_file_with_review_agent("openrouter-deepseek");
        let errors = validate_task_file_profiles(&store, "", &file);

        assert!(
            errors.iter().all(|e| !e.path.starts_with("review[")),
            "{errors:?}"
        );
    }

    #[test]
    fn validate_task_file_profiles_rejects_when_any_candidate_in_a_list_lacks_system_prompt_support()
     {
        let store = store();
        store
            .upsert_agent_profile("custom-claude", "claude-code", None, None, &[])
            .expect("upsert");
        store
            .upsert_agent_profile("custom-ollama", "ollama", None, None, &[])
            .expect("upsert");
        let file: TaskFile = toml::from_str(
            "[[task]]\nname=\"t\"\n[[task.cell]]\nprompt=\"p\"\n\
             system_prompt=\"be terse\"\n\
             agent = [{ agent = \"custom-claude\" }, { agent = \"custom-ollama\" }]\n",
        )
        .expect("parse task file");

        let errors = validate_task_file_profiles(&store, "", &file);

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
        let store = store();
        store
            .upsert_agent_profile("custom-claude", "claude-code", None, None, &[])
            .expect("upsert");
        store
            .upsert_agent_profile("custom-codex", "codex", None, None, &[])
            .expect("upsert");
        let file: TaskFile = toml::from_str(
            "[[task]]\nname=\"t\"\n[[task.cell]]\nprompt=\"p\"\n\
             system_prompt=\"be terse\"\n\
             agent = [{ agent = \"custom-claude\" }, { agent = \"custom-codex\" }]\n",
        )
        .expect("parse task file");

        let errors = validate_task_file_profiles(&store, "", &file);

        assert!(
            errors.iter().all(|e| !e.path.contains("system_prompt")),
            "{errors:?}"
        );
    }

    #[test]
    fn validate_task_file_profiles_rejects_an_unknown_name_inside_a_candidate_list() {
        let store = store();
        let file: TaskFile = toml::from_str(
            "[[task]]\nname=\"t\"\n[[task.cell]]\nprompt=\"p\"\n\
             agent = [{ agent = \"codex\" }, { agent = \"not-a-real-profile\" }]\n",
        )
        .expect("parse task file");

        let errors = validate_task_file_profiles(&store, "", &file);

        assert!(
            errors
                .iter()
                .any(|e| e.path.contains("agent[1]") && e.message.contains("not-a-real-profile")),
            "{errors:?}"
        );
    }

    /// Test-only [`crate::runner::Runner`] that reports a fixed set of agent
    /// names as available, without spawning a real `ralphus-runner`
    /// subprocess.
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
        let store = store();
        let mut file: TaskFile = toml::from_str(concat!(
            "[[task]]\nname=\"t\"\n[[task.cell]]\nprompt=\"p\"\n",
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
        let store = store();
        let mut file: TaskFile = toml::from_str(concat!(
            "[[task]]\nname=\"t\"\n[[task.cell]]\nprompt=\"p\"\n",
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
        let store = store();
        let mut file: TaskFile = toml::from_str(concat!(
            "[[task]]\nname=\"t\"\n",
            "agent = [{ agent = \"claude-code\" }, { agent = \"codex\" }]\n",
            "[[task.cell]]\nprompt=\"p\"\n",
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
        let store = store();
        let mut file: TaskFile = toml::from_str(
            "[[task]]\nname=\"t\"\n[[task.cell]]\nprompt=\"p\"\nagent=\"claude-code\"\n",
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
    fn check_profiles_health_fails_a_profile_with_an_unresolved_link_env_var() {
        let store = store();
        store
            .upsert_agent_profile(
                "deepseek",
                "anthropic",
                None,
                None,
                &[AgentProfileEnvVar {
                    key: "ANTHROPIC_AUTH_TOKEN".to_string(),
                    kind: EnvValueKind::Link,
                    value: "RALPHUS_AGENT_PROFILES_HEALTH_TEST_VAR_UNSET".to_string(),
                }],
            )
            .expect("upsert");
        let results = check_profiles_health(&store);
        let deepseek = results
            .iter()
            .find(|r| r.name == "agent-profile:deepseek")
            .expect("row present");
        assert_eq!(deepseek.status, "fail");
        assert!(
            deepseek
                .detail
                .contains("RALPHUS_AGENT_PROFILES_HEALTH_TEST_VAR_UNSET")
        );
    }

    #[test]
    fn check_profiles_health_skips_multi_word_executables() {
        let store = store();
        store
            .set_locked_agent_profile_executable("claude-code", "wsl.exe claude")
            .expect("set");
        let results = check_profiles_health(&store);
        let claude_code = results
            .iter()
            .find(|r| r.name == "agent-profile:claude-code")
            .expect("row present");
        assert_eq!(claude_code.status, "skip");
    }

    #[test]
    fn check_profiles_health_checks_single_word_executables() {
        let store = store();
        // "codex" is seeded by default -- almost certainly not literally on
        // PATH in a test sandbox, so this exercises the pass/fail branch
        // without asserting which of the two it lands on.
        let results = check_profiles_health(&store);
        let codex = results
            .iter()
            .find(|r| r.name == "agent-profile:codex")
            .expect("row present");
        assert!(codex.status == "pass" || codex.status == "fail");
    }
}
