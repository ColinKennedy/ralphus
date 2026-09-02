//! ralphus configuration loader, ported from `cli/src/ralphus/config.py`.
//! Reads `.ralphus.toml` files from `$RALPHUS_CONFIGURATION_PATH`
//! (`path`-separated list) and the git-repo-root `.ralphus.toml`, later
//! sources overriding earlier ones. Only stdlib-equivalent `toml` parsing
//! (the workspace's shared `toml` crate) -- no other dependency.

use std::path::{Path, PathBuf};

const DEFAULT_TIMEOUT_SEC: i64 = 1800;
const VALID_LOG_LEVELS: [&str; 5] = ["error", "warn", "info", "debug", "trace"];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskConfig {
    pub maximum_timeout_seconds: i64,
}

impl Default for TaskConfig {
    fn default() -> Self {
        Self {
            maximum_timeout_seconds: DEFAULT_TIMEOUT_SEC,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DaemonConfig {
    pub log_path: Option<String>,
    pub log_level: Option<String>,
    pub keep_temporary_files: bool,
    /// The daemon's global concurrency cap. `None` means unset (the daemon
    /// falls back to its own default); `Some(0)` means "no limit". Negative
    /// values are invalid -- `ralphus check health` warns about them.
    pub max_concurrent: Option<i64>,
}

#[derive(Debug, Clone, Default)]
pub struct Config {
    pub task: TaskConfig,
    pub daemon: DaemonConfig,
    pub sources: Vec<PathBuf>,
    pub source_labels: Vec<(PathBuf, String)>,
    /// One entry per trackable field; value is the source file that last set
    /// it, or `None` if it's still at its default.
    pub provenance: Vec<(&'static str, Option<PathBuf>)>,
}

impl Config {
    /// Wall-clock timeout for backend subprocesses, or `None` for no limit.
    /// `maximum_timeout_seconds <= 0` means unbounded; negative values are
    /// flagged by `ralphus check health`, not clamped here.
    #[must_use]
    pub fn subprocess_timeout(&self) -> Option<f64> {
        if self.task.maximum_timeout_seconds <= 0 {
            None
        } else {
            Some(self.task.maximum_timeout_seconds as f64)
        }
    }
}

#[derive(Debug, Clone)]
pub struct ConfigFileIssues {
    pub path: PathBuf,
    pub label: String,
    pub syntax_error: Option<String>,
    pub issues: Vec<String>,
}

fn find_git_root(start: &Path) -> Option<PathBuf> {
    let mut current = start.canonicalize().ok()?;
    loop {
        if current.join(".git").exists() {
            return Some(current);
        }
        current = current.parent()?.to_path_buf();
    }
}

/// `configuration_path_env` is threaded in explicitly (rather than read from
/// `std::env` here) so tests are hermetic against whatever
/// `$RALPHUS_CONFIGURATION_PATH` happens to be set to in the real process
/// environment (a real, common dev-machine setting) -- same rationale as
/// `runner/src/config.rs` and `runner/src/providers.rs`.
fn get_candidates(
    cwd: &Path,
    include_local: bool,
    configuration_path_env: Option<&str>,
) -> Vec<(PathBuf, String)> {
    let mut candidates: Vec<(PathBuf, String)> = Vec::new();
    if let Some(env_val) = configuration_path_env {
        for part in std::env::split_paths(env_val) {
            if !part.as_os_str().is_empty() {
                candidates.push((part, "environment variable".to_string()));
            }
        }
    }
    if include_local {
        if let Some(git_root) = find_git_root(cwd) {
            let git_config = git_root.join(".ralphus.toml");
            if !candidates.iter().any(|(p, _)| p == &git_config) {
                candidates.push((git_config, "local".to_string()));
            }
        }
    }
    candidates
}

fn type_name(value: &toml::Value) -> &'static str {
    match value {
        toml::Value::Boolean(_) => "boolean",
        toml::Value::Integer(_) => "integer",
        toml::Value::Float(_) => "float",
        toml::Value::String(_) => "string",
        toml::Value::Array(_) => "array",
        toml::Value::Table(_) => "table",
        toml::Value::Datetime(_) => "datetime",
    }
}

fn validate_raw(raw: &toml::Table) -> Vec<String> {
    let mut issues = Vec::new();
    const KNOWN_TOP: [&str; 6] = ["task", "daemon", "review", "defaults", "agent", "ark"];
    for key in raw.keys() {
        if !KNOWN_TOP.contains(&key.as_str()) {
            issues.push(format!("key \"{key}\" is unknown"));
        }
    }

    if let Some(task_raw) = raw.get("task") {
        match task_raw.as_table() {
            None => issues.push(format!(
                "key \"task\" expects a \"table\" but got a \"{}\" type",
                type_name(task_raw)
            )),
            Some(table) => {
                for k in table.keys() {
                    if k != "maximum_timeout_seconds" {
                        issues.push(format!("key \"task.{k}\" is unknown"));
                    }
                }
                if let Some(mt) = table.get("maximum_timeout_seconds") {
                    match mt.as_integer() {
                        None => issues.push(format!(
                            "key \"task.maximum_timeout_seconds\" expects \"integer\" but got a \"{}\" type",
                            type_name(mt)
                        )),
                        Some(v) if v < 0 => issues.push(format!(
                            "key \"task.maximum_timeout_seconds\" got invalid value {v}. Expected >= 0 (0 = unbounded, positive = cap in seconds)"
                        )),
                        Some(_) => {}
                    }
                }
            }
        }
    }

    if let Some(daemon_raw) = raw.get("daemon") {
        match daemon_raw.as_table() {
            None => issues.push(format!(
                "key \"daemon\" expects a \"table\" but got a \"{}\" type",
                type_name(daemon_raw)
            )),
            Some(table) => {
                const KNOWN_DAEMON: [&str; 4] = [
                    "log_path",
                    "log_level",
                    "keep_temporary_files",
                    "max_concurrent",
                ];
                for k in table.keys() {
                    if !KNOWN_DAEMON.contains(&k.as_str()) {
                        issues.push(format!("key \"daemon.{k}\" is unknown"));
                    }
                }
                if let Some(mc) = table.get("max_concurrent") {
                    if mc.as_integer().is_none() {
                        issues.push(format!(
                            "key \"daemon.max_concurrent\" expects \"integer\" but got a \"{}\" type",
                            type_name(mc)
                        ));
                    }
                }
                if let Some(lp) = table.get("log_path") {
                    if lp.as_str().is_none() {
                        issues.push(format!(
                            "key \"daemon.log_path\" expects \"string\" but got a \"{}\" type",
                            type_name(lp)
                        ));
                    }
                }
                if let Some(ll) = table.get("log_level") {
                    match ll.as_str() {
                        None => issues.push(format!(
                            "key \"daemon.log_level\" expects \"string\" but got a \"{}\" type",
                            type_name(ll)
                        )),
                        Some(v) if !VALID_LOG_LEVELS.contains(&v) => {
                            let valid = VALID_LOG_LEVELS
                                .iter()
                                .map(|v| format!("\"{v}\""))
                                .collect::<Vec<_>>()
                                .join(", ");
                            issues.push(format!(
                                "key \"daemon.log_level\" got invalid value \"{v}\". Expected one of [{valid}]"
                            ));
                        }
                        Some(_) => {}
                    }
                }
                if let Some(ktf) = table.get("keep_temporary_files") {
                    if ktf.as_bool().is_none() {
                        issues.push(format!(
                            "key \"daemon.keep_temporary_files\" expects \"boolean\" but got a \"{}\" type",
                            type_name(ktf)
                        ));
                    }
                }
            }
        }
    }

    if let Some(ark_raw) = raw.get("ark") {
        match ark_raw.as_table() {
            None => issues.push(format!(
                "key \"ark\" expects a \"table\" but got a \"{}\" type",
                type_name(ark_raw)
            )),
            Some(table) => {
                const KNOWN_ARK: [&str; 3] =
                    ["sweep_interval_days", "stale_after_days", "max_worktrees"];
                for key in table.keys() {
                    if !KNOWN_ARK.contains(&key.as_str()) {
                        issues.push(format!("key \"ark.{key}\" is unknown"));
                    }
                }
                for key in KNOWN_ARK {
                    if let Some(value) = table.get(key) {
                        match value.as_integer() {
                            None => issues.push(format!(
                                "key \"ark.{key}\" expects \"integer\" but got a \"{}\" type",
                                type_name(value)
                            )),
                            Some(value) if value <= 0 => issues.push(format!(
                                "key \"ark.{key}\" got invalid value {value}. Expected > 0"
                            )),
                            Some(_) => {}
                        }
                    }
                }
            }
        }
    }

    for section_name in ["review", "defaults"] {
        let Some(section_raw) = raw.get(section_name) else {
            continue;
        };
        match section_raw.as_table() {
            None => issues.push(format!(
                "key \"{section_name}\" expects a \"table\" but got a \"{}\" type",
                type_name(section_raw)
            )),
            Some(table) => {
                const KNOWN_REVIEW: [&str; 2] = ["skip_worktrees", "checks"];
                for k in table.keys() {
                    if !KNOWN_REVIEW.contains(&k.as_str()) {
                        issues.push(format!("key \"{section_name}.{k}\" is unknown"));
                    }
                }
                if let Some(sw) = table.get("skip_worktrees") {
                    if sw.as_bool().is_none() {
                        issues.push(format!(
                            "key \"{section_name}.skip_worktrees\" expects \"boolean\" but got a \"{}\" type",
                            type_name(sw)
                        ));
                    }
                }
                if let Some(checks) = table.get("checks") {
                    match checks.as_array() {
                        None => issues.push(format!(
                            "key \"{section_name}.checks\" expects \"array\" but got a \"{}\" type",
                            type_name(checks)
                        )),
                        Some(items) => {
                            for (i, item) in items.iter().enumerate() {
                                if item.as_str().is_none() {
                                    issues.push(format!(
                                        "key \"{section_name}.checks[{i}]\" expects \"string\" but got a \"{}\" type",
                                        type_name(item)
                                    ));
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    validate_agent_section(raw, &mut issues);

    issues
}

/// Validate `[agent.profiles.<name>]` (RAL-agent-profile-backend-resolution).
/// This is a structural lint only -- offline, and without the daemon's
/// registry, `core` (and this CLI-side checker) can't confirm a profile's
/// `env.*.from_env` variable is actually set, or (for non-reserved-name
/// checks) resolve which backends exist beyond the ones baked into
/// `ralphus_core::schema::RESERVED_AGENT_NAMES`. The authoritative parse +
/// business rules (backend enum, `raw` requiring `executable`, native
/// backends rejecting it) live in `daemon::agent_profiles::parse_profile_file`
/// and run at submit time -- see that module's doc comment on
/// `RESERVED_AGENT_NAMES` for why the split exists.
fn validate_agent_section(raw: &toml::Table, issues: &mut Vec<String>) {
    let Some(agent_raw) = raw.get("agent") else {
        return;
    };
    let Some(agent_table) = agent_raw.as_table() else {
        issues.push(format!(
            "key \"agent\" expects a \"table\" but got a \"{}\" type",
            type_name(agent_raw)
        ));
        return;
    };

    const KNOWN_AGENT: [&str; 1] = ["profiles"];
    for k in agent_table.keys() {
        if !KNOWN_AGENT.contains(&k.as_str()) {
            issues.push(format!("key \"agent.{k}\" is unknown"));
        }
    }

    let Some(profiles_raw) = agent_table.get("profiles") else {
        return;
    };
    let Some(profiles_table) = profiles_raw.as_table() else {
        issues.push(format!(
            "key \"agent.profiles\" expects a \"table\" but got a \"{}\" type",
            type_name(profiles_raw)
        ));
        return;
    };

    const KNOWN_PROFILE: [&str; 3] = ["backend", "executable", "env"];
    for (name, profile_raw) in profiles_table {
        let path = format!("agent.profiles.{name}");
        if ralphus_core::schema::RESERVED_AGENT_NAMES.contains(&name.as_str()) {
            issues.push(format!(
                "key \"{path}\" collides with a reserved built-in backend name"
            ));
        }
        let Some(profile_table) = profile_raw.as_table() else {
            issues.push(format!(
                "key \"{path}\" expects a \"table\" but got a \"{}\" type",
                type_name(profile_raw)
            ));
            continue;
        };
        for k in profile_table.keys() {
            if !KNOWN_PROFILE.contains(&k.as_str()) {
                issues.push(format!("key \"{path}.{k}\" is unknown"));
            }
        }
        match profile_table.get("backend") {
            None => issues.push(format!("key \"{path}.backend\" is required")),
            Some(b) if b.as_str().is_none() => issues.push(format!(
                "key \"{path}.backend\" expects \"string\" but got a \"{}\" type",
                type_name(b)
            )),
            Some(_) => {}
        }
        if let Some(exe) = profile_table.get("executable") {
            if exe.as_str().is_none() {
                issues.push(format!(
                    "key \"{path}.executable\" expects \"string\" but got a \"{}\" type",
                    type_name(exe)
                ));
            }
        }
        if let Some(env_raw) = profile_table.get("env") {
            match env_raw.as_table() {
                None => issues.push(format!(
                    "key \"{path}.env\" expects \"table\" but got a \"{}\" type",
                    type_name(env_raw)
                )),
                Some(env_table) => {
                    for (env_key, env_value) in env_table {
                        let env_path = format!("{path}.env.{env_key}");
                        let valid = env_value.as_str().is_some()
                            || env_value.as_table().is_some_and(|t| {
                                t.get("from_env").is_some_and(toml::Value::is_str)
                            });
                        if !valid {
                            issues.push(format!(
                                "key \"{env_path}\" expects a string or {{ from_env = \"VAR\" }}, got a \"{}\" type",
                                type_name(env_value)
                            ));
                        }
                    }
                }
            }
        }
    }
}

fn apply_task(base: &TaskConfig, raw: &toml::Table) -> TaskConfig {
    let Some(section) = raw.get("task").and_then(toml::Value::as_table) else {
        return base.clone();
    };
    match section
        .get("maximum_timeout_seconds")
        .and_then(toml::Value::as_integer)
    {
        Some(mt) => TaskConfig {
            maximum_timeout_seconds: mt,
        },
        None => base.clone(),
    }
}

fn apply_daemon(base: &DaemonConfig, raw: &toml::Table) -> DaemonConfig {
    let Some(section) = raw.get("daemon").and_then(toml::Value::as_table) else {
        return base.clone();
    };
    DaemonConfig {
        log_path: section
            .get("log_path")
            .and_then(toml::Value::as_str)
            .map(str::to_string)
            .or_else(|| base.log_path.clone()),
        log_level: section
            .get("log_level")
            .and_then(toml::Value::as_str)
            .map(str::to_string)
            .or_else(|| base.log_level.clone()),
        keep_temporary_files: section
            .get("keep_temporary_files")
            .and_then(toml::Value::as_bool)
            .unwrap_or(base.keep_temporary_files),
        max_concurrent: section
            .get("max_concurrent")
            .and_then(toml::Value::as_integer)
            .or(base.max_concurrent),
    }
}

/// Parses and validates each candidate config file; returns only files with
/// problems (missing files are silently skipped, same as [`load_config`]).
#[must_use]
pub fn validate_config_files(cwd: &Path, include_local: bool) -> Vec<ConfigFileIssues> {
    let env = std::env::var("RALPHUS_CONFIGURATION_PATH").ok();
    validate_config_files_with(cwd, include_local, env.as_deref())
}

fn validate_config_files_with(
    cwd: &Path,
    include_local: bool,
    configuration_path_env: Option<&str>,
) -> Vec<ConfigFileIssues> {
    let mut results = Vec::new();
    for (path, label) in get_candidates(cwd, include_local, configuration_path_env) {
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        match text.parse::<toml::Table>() {
            Ok(raw) => {
                let issues = validate_raw(&raw);
                if !issues.is_empty() {
                    results.push(ConfigFileIssues {
                        path,
                        label,
                        syntax_error: None,
                        issues,
                    });
                }
            }
            Err(e) => results.push(ConfigFileIssues {
                path,
                label,
                syntax_error: Some(e.to_string()),
                issues: Vec::new(),
            }),
        }
    }
    results
}

/// Loads and merges all applicable `.ralphus.toml` files. Resolution order
/// (later wins): `$RALPHUS_CONFIGURATION_PATH` entries left-to-right, then
/// the git-repo-root file (unless `include_local` is false).
#[must_use]
pub fn load_config(cwd: &Path, include_local: bool) -> Config {
    let env = std::env::var("RALPHUS_CONFIGURATION_PATH").ok();
    load_config_with(cwd, include_local, env.as_deref())
}

fn load_config_with(
    cwd: &Path,
    include_local: bool,
    configuration_path_env: Option<&str>,
) -> Config {
    let mut task = TaskConfig::default();
    let mut daemon = DaemonConfig::default();
    let mut sources = Vec::new();
    let mut source_labels = Vec::new();
    let mut provenance: Vec<(&'static str, Option<PathBuf>)> = vec![
        ("task.maximum_timeout_seconds", None),
        ("daemon.log_path", None),
        ("daemon.log_level", None),
        ("daemon.keep_temporary_files", None),
        ("daemon.max_concurrent", None),
    ];

    for (path, label) in get_candidates(cwd, include_local, configuration_path_env) {
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(raw) = text.parse::<toml::Table>() else {
            continue;
        };

        let new_task = apply_task(&task, &raw);
        if new_task != task {
            set_provenance(&mut provenance, "task.maximum_timeout_seconds", &path);
        }
        task = new_task;

        let new_daemon = apply_daemon(&daemon, &raw);
        if new_daemon.log_path != daemon.log_path {
            set_provenance(&mut provenance, "daemon.log_path", &path);
        }
        if new_daemon.log_level != daemon.log_level {
            set_provenance(&mut provenance, "daemon.log_level", &path);
        }
        if new_daemon.keep_temporary_files != daemon.keep_temporary_files {
            set_provenance(&mut provenance, "daemon.keep_temporary_files", &path);
        }
        if new_daemon.max_concurrent != daemon.max_concurrent {
            set_provenance(&mut provenance, "daemon.max_concurrent", &path);
        }
        daemon = new_daemon;

        sources.push(path.clone());
        source_labels.push((path, label));
    }

    Config {
        task,
        daemon,
        sources,
        source_labels,
        provenance,
    }
}

fn set_provenance(provenance: &mut [(&'static str, Option<PathBuf>)], field: &str, path: &Path) {
    if let Some(entry) = provenance.iter_mut().find(|(k, _)| *k == field) {
        entry.1 = Some(path.to_path_buf());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "ralphus-cli-config-test-{name}-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn defaults_when_no_env_and_no_git_root() {
        let dir = scratch_dir("defaults");
        let config = load_config_with(&dir, true, None);
        assert_eq!(config.task.maximum_timeout_seconds, DEFAULT_TIMEOUT_SEC);
        assert!(!config.daemon.keep_temporary_files);
        assert!(config.sources.is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn subprocess_timeout_unbounded_when_non_positive() {
        let config = Config {
            task: TaskConfig {
                maximum_timeout_seconds: 0,
            },
            ..Config::default()
        };
        assert_eq!(config.subprocess_timeout(), None);
    }

    #[test]
    fn validate_raw_flags_unknown_keys_and_bad_types() {
        let raw: toml::Table = "bogus = 1\n[task]\nmaximum_timeout_seconds = \"nope\"\n"
            .parse()
            .unwrap();
        let issues = validate_raw(&raw);
        assert!(issues.iter().any(|i| i.contains("\"bogus\" is unknown")));
        assert!(
            issues
                .iter()
                .any(|i| i.contains("task.maximum_timeout_seconds"))
        );
    }

    #[test]
    fn validate_raw_accepts_well_formed_agent_profile() {
        let raw: toml::Table = r#"
[agent.profiles.openrouter-deepseek]
backend = "claude-code"

[agent.profiles.openrouter-deepseek.env]
ANTHROPIC_BASE_URL = "https://openrouter.ai/api"
ANTHROPIC_AUTH_TOKEN = { from_env = "RALPHUS_OPENROUTER_DEEKSEEK_AUTH_TOKEN" }
"#
        .parse()
        .unwrap();
        let issues = validate_raw(&raw);
        assert!(issues.is_empty(), "{issues:?}");
    }

    #[test]
    fn validate_raw_flags_agent_profile_issues() {
        let raw: toml::Table = r#"
[agent.profiles.claude]
backend = "codex"

[agent.profiles.custom]
executable = "my-runner"

[agent.profiles.custom.env]
BAD = 5
"#
        .parse()
        .unwrap();
        let issues = validate_raw(&raw);
        assert!(
            issues
                .iter()
                .any(|i| i.contains("agent.profiles.claude") && i.contains("reserved")),
            "{issues:?}"
        );
        assert!(
            issues
                .iter()
                .any(|i| i.contains("agent.profiles.custom.backend") && i.contains("required")),
            "{issues:?}"
        );
        assert!(
            issues
                .iter()
                .any(|i| i.contains("agent.profiles.custom.env.BAD")),
            "{issues:?}"
        );
    }

    #[test]
    fn validate_raw_flags_invalid_log_level() {
        let raw: toml::Table = "[daemon]\nlog_level = \"verbose\"\n".parse().unwrap();
        let issues = validate_raw(&raw);
        assert!(
            issues
                .iter()
                .any(|i| i.contains("invalid value \"verbose\""))
        );
    }

    #[test]
    fn validate_raw_accepts_well_formed_file() {
        let raw: toml::Table =
            "[task]\nmaximum_timeout_seconds = 60\n[daemon]\nkeep_temporary_files = true\n"
                .parse()
                .unwrap();
        assert!(validate_raw(&raw).is_empty());
    }

    #[test]
    fn validate_raw_checks_ark_policy() {
        let valid: toml::Table =
            "[ark]\nsweep_interval_days=1\nstale_after_days=90\nmax_worktrees=100"
                .parse()
                .unwrap();
        assert!(validate_raw(&valid).is_empty());

        let invalid: toml::Table =
            "[ark]\nsweep_interval_days=0\nstale_after_days=\"old\"\nunknown=true"
                .parse()
                .unwrap();
        let issues = validate_raw(&invalid).join("\n");
        assert!(issues.contains("sweep_interval_days"));
        assert!(issues.contains("stale_after_days"));
        assert!(issues.contains("ark.unknown"));
    }

    #[test]
    fn validate_raw_flags_non_integer_max_concurrent() {
        let raw: toml::Table = "[daemon]\nmax_concurrent = \"nope\"\n".parse().unwrap();
        let issues = validate_raw(&raw);
        assert!(
            issues.iter().any(|i| i.contains("daemon.max_concurrent")),
            "{issues:?}"
        );
    }

    #[test]
    fn validate_raw_accepts_zero_max_concurrent() {
        let raw: toml::Table = "[daemon]\nmax_concurrent = 0\n".parse().unwrap();
        assert!(validate_raw(&raw).is_empty());
    }

    #[test]
    fn apply_daemon_parses_max_concurrent() {
        let base = DaemonConfig::default();
        let raw: toml::Table = "[daemon]\nmax_concurrent = 24\n".parse().unwrap();
        assert_eq!(apply_daemon(&base, &raw).max_concurrent, Some(24));
    }

    #[test]
    fn apply_daemon_only_overrides_present_fields() {
        let base = DaemonConfig {
            log_path: Some("old.log".to_string()),
            log_level: None,
            keep_temporary_files: false,
            max_concurrent: None,
        };
        let raw: toml::Table = "[daemon]\nlog_level = \"debug\"\n".parse().unwrap();
        let merged = apply_daemon(&base, &raw);
        assert_eq!(merged.log_path.as_deref(), Some("old.log"));
        assert_eq!(merged.log_level.as_deref(), Some("debug"));
    }

    #[test]
    fn load_config_applies_explicit_configuration_path_file_and_records_provenance() {
        let dir = scratch_dir("explicit");
        let cfg_path = dir.join("override.toml");
        std::fs::write(&cfg_path, "[daemon]\nkeep_temporary_files = true\n").unwrap();

        let candidates = vec![(cfg_path.clone(), "environment variable".to_string())];
        // Exercise the merge/provenance logic directly against a known file
        // list instead of the real $RALPHUS_CONFIGURATION_PATH (a real,
        // common dev-machine setting -- see the runner crate's `config.rs`
        // for the same hermetic-testing rationale).
        let mut task = TaskConfig::default();
        let mut daemon = DaemonConfig::default();
        let mut provenance: Vec<(&'static str, Option<PathBuf>)> =
            vec![("daemon.keep_temporary_files", None)];
        for (path, _label) in &candidates {
            let text = std::fs::read_to_string(path).unwrap();
            let raw: toml::Table = text.parse().unwrap();
            task = apply_task(&task, &raw);
            let new_daemon = apply_daemon(&daemon, &raw);
            if new_daemon.keep_temporary_files != daemon.keep_temporary_files {
                set_provenance(&mut provenance, "daemon.keep_temporary_files", path);
            }
            daemon = new_daemon;
        }
        assert!(daemon.keep_temporary_files);
        assert_eq!(provenance[0].1.as_deref(), Some(cfg_path.as_path()));
        let _ = task;
        std::fs::remove_dir_all(&dir).ok();
    }
}
