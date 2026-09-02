use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use ralphus_core::schema::TaskFile;
use ralphus_core::validate::{ErrorKind, ValidationError};
use serde::{Deserialize, Serialize};

use crate::config::{find_project_config, global_config_path};
use crate::store::Store;

pub const RAW_BACKEND: &str = "raw";

const PROFILE_BACKENDS: &[&str] = &[
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
/// `cli-rs/src/config.rs` and `runner/src/config.rs` already read for every
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

pub fn resolve_agent_for_path(agent: &str, cwd: &Path) -> Result<ResolvedAgentSelection, String> {
    let raw = std::env::var("RALPHUS_CONFIGURATION_PATH").ok();
    resolve_agent_for_path_with(agent, cwd, raw.as_deref())
}

/// [`resolve_agent_for_path`] with `$RALPHUS_CONFIGURATION_PATH` passed in
/// explicitly -- see [`configuration_path_entries`] for why.
fn resolve_agent_for_path_with(
    agent: &str,
    cwd: &Path,
    configuration_path_env: Option<&str>,
) -> Result<ResolvedAgentSelection, String> {
    let profiles = load_profiles_for_path_with(cwd, configuration_path_env)?;
    if let Some(profile) = profiles.get(agent) {
        // RAL-264: this profile is about to be used to run a cell, so its
        // `from_env`-resolved secret values must be scrubbed everywhere raw
        // pane text gets persisted. Even though the profile may already be
        // registered from a prior resolution (idempotent), registering here
        // guarantees the values are in place before any tmux pane capture.
        crate::redact::register_all(profile.secret_values.iter().cloned());
        return Ok(ResolvedAgentSelection {
            backend: profile.backend.clone(),
            executable: profile.executable.clone(),
            env: profile.env.clone(),
            custom_profile: true,
            secret_values: profile.secret_values.clone(),
        });
    }
    if let Some(backend) = normalize_builtin_agent(agent) {
        return Ok(ResolvedAgentSelection {
            backend: backend.to_string(),
            executable: None,
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
    task: &ralphus_core::schema::TaskDef,
    cell: &ralphus_core::schema::CellDef,
) -> Option<PathBuf> {
    if let Some(cwd) = cell.cwd.as_deref() {
        if ralphus_core::schema::first_worktree_placeholder_in_text(cwd).is_none() {
            return Some(PathBuf::from(cwd));
        }
    }
    task.project
        .as_deref()
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
            let agent = cell
                .agent
                .as_deref()
                .or(task.agent.as_deref())
                .unwrap_or(ralphus_core::schema::DEFAULT_AGENT);
            let Some(cwd) = config_cwd_for_cell(store, task, cell) else {
                continue;
            };
            let selection = match resolve_agent_for_path_with(agent, &cwd, configuration_path_env) {
                Ok(s) => s,
                Err(message) => {
                    errors.push(ValidationError {
                        path: format!("task[{task_idx}].cell[{cell_idx}].agent"),
                        kind: ErrorKind::InvalidValue,
                        message,
                        line: None,
                    });
                    continue;
                }
            };
            if selection.custom_profile
                && cell.model.clone().or_else(|| task.model.clone()).is_some()
            {
                errors.push(ValidationError {
                    path: format!("task[{task_idx}].cell[{cell_idx}].model"),
                    kind: ErrorKind::ConflictingKeys,
                    // Conservative v1 rule: if you're using a custom agent profile, you can't
                    // also set `model`. We can relax this later once there's a concrete
                    // multi-model-per-profile use case with clear semantics.
                    message: "if you're using a custom agent profile, you can't also set `model`"
                        .to_string(),
                    line: None,
                });
            }
            // `core`'s offline validator only rejects system_prompt for agent
            // names it recognizes itself (RESERVED_AGENT_NAMES) -- it defers on
            // any custom profile, since it can't see the profile's backend. Now
            // that the profile is resolved, check its backend the same way.
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
                        "agent profile \"{agent}\" resolves to backend \"{}\", which does not \
                         support system_prompt/system_prompt_position (only claude-code, \
                         codex, and pi backends do)",
                        selection.backend
                    ),
                    line: None,
                });
            }
            // RAL-304: same deferral shape as system_prompt above -- `core`
            // can't classify a custom profile's backend itself, so it only
            // rejects `maximum_context`/`auto_compact_threshold` for a
            // RESERVED_AGENT_NAMES agent; check the resolved backend here.
            // Either field cascades from the task, so both are checked at
            // their effective (cell-or-task) value, not just the cell's own.
            // The two fields are checked independently, not as a pair --
            // claude-code accepts auto_compact_threshold but not
            // maximum_context (see `agent_supports_auto_compact_threshold`'s
            // doc comment for why).
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
                        "agent profile \"{agent}\" resolves to backend \"{}\", which does not \
                         support maximum_context (only codex and pi backends do)",
                        selection.backend
                    ),
                    line: None,
                });
            }
            if selection.custom_profile
                && has_auto_compact_threshold
                && !ralphus_core::schema::agent_supports_auto_compact_threshold(&selection.backend)
            {
                errors.push(ValidationError {
                    path: format!("task[{task_idx}].cell[{cell_idx}].auto_compact_threshold"),
                    kind: ErrorKind::InvalidValue,
                    message: format!(
                        "agent profile \"{agent}\" resolves to backend \"{}\", which does not \
                         support auto_compact_threshold (only codex, pi, and claude-code \
                         backends do)",
                        selection.backend
                    ),
                    line: None,
                });
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
                let Some(cwd) = config_cwd_for_cell(store, task, cell) else {
                    continue;
                };
                if !seen_cwds.insert(cwd.clone()) {
                    continue;
                }
                if let Err(message) =
                    resolve_agent_for_path_with(agent, &cwd, configuration_path_env)
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
fn resolve_executable(program: &str) -> Result<String, String> {
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

/// Health-checks every agent profile the daemon can see for `cwd` plus
/// every registered project's `.ralphus.toml` (deduped by config path) --
/// mirrors the root discovery `validate_task_file_profiles` uses at submit
/// time, so `ralphus check health` and `ralphus submit` agree on which
/// profiles are in scope.
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
}
