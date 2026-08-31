//! `ralphus check health` -- check that the local setup can actually run
//! tasks, ported from `cli/src/ralphus/health.py`.
//!
//! Checks are grouped into sections:
//! - `core`: things every user needs (daemon reachable, git, every
//!   registered project's on-disk path/git-repo validity, the runner
//!   binary, Ollama for local-model runs, `nvidia-smi` for GPU metrics,
//!   `$RALPHUS_CLAUDE_COMMAND`/`$RALPHUS_CODEX_COMMAND`/`$RALPHUS_PI_COMMAND`
//!   when set). Hard
//!   requirements (`fail`) except `nvidia-smi` (`warn` -- the GPU column
//!   just degrades to N/A without it).
//! - `developer`: `cargo`, needed only to build the Rust binaries from
//!   source. Opt-in (`--enable-developer-checks`) so end users aren't
//!   warned about a tool they don't need.
//!
//! Only a `fail` makes `check health` exit non-zero.

use std::path::Path;
use std::time::Duration;

use crate::client::DaemonClient;

pub const CORE: &str = "core";
pub const DEVELOPER: &str = "developer";

const PASS: &str = "pass";
const WARN: &str = "warn";
const FAIL: &str = "fail";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckResult {
    pub name: String,
    pub status: &'static str,
    pub detail: String,
    pub section: &'static str,
}

impl CheckResult {
    fn new(name: &str, status: &'static str, detail: impl Into<String>) -> Self {
        Self {
            name: name.to_string(),
            status,
            detail: detail.into(),
            section: CORE,
        }
    }

    fn developer(name: &str, status: &'static str, detail: impl Into<String>) -> Self {
        Self {
            name: name.to_string(),
            status,
            detail: detail.into(),
            section: DEVELOPER,
        }
    }

    #[must_use]
    pub fn is_fail(&self) -> bool {
        self.status == FAIL
    }
}

/// RAL-110 Q5 heuristic: is `value` a compound shell command rather than a
/// single bare executable path? A value with no space is always a bare
/// path; a value with a space is still a bare path if entirely wrapped in
/// one matching pair of quotes (a Windows path with spaces).
#[must_use]
pub fn is_compound_shell_command(value: &str) -> bool {
    let stripped = value.trim();
    if !stripped.contains(' ') {
        return false;
    }
    let bytes = stripped.as_bytes();
    if bytes.len() >= 2 {
        let first = bytes[0];
        let last = bytes[bytes.len() - 1];
        if first == last && (first == b'\'' || first == b'"') {
            let inner = &stripped[1..stripped.len() - 1];
            if !inner.contains(first as char) {
                return false;
            }
        }
    }
    true
}

/// Strips one layer of wrapping quotes from a bare (non-compound) path.
#[must_use]
pub fn unquote_path(value: &str) -> String {
    let stripped = value.trim();
    let bytes = stripped.as_bytes();
    if bytes.len() >= 2 {
        let first = bytes[0];
        let last = bytes[bytes.len() - 1];
        if first == last && (first == b'\'' || first == b'"') {
            return stripped[1..stripped.len() - 1].to_string();
        }
    }
    stripped.to_string()
}

fn check_agent_command(name: &str, env_var: &str, default_program: &str) -> CheckResult {
    let Ok(raw) = std::env::var(env_var) else {
        return CheckResult::new(
            name,
            PASS,
            format!("not set (defaults to '{default_program}' on PATH)"),
        );
    };
    if is_compound_shell_command(&raw) {
        return CheckResult::new(
            name,
            PASS,
            format!("compound shell command, not path-checked: {raw}"),
        );
    }
    let path = unquote_path(&raw);
    let p = Path::new(&path);
    if !p.is_file() {
        return CheckResult::new(
            name,
            FAIL,
            format!("{path} does not exist or is not a file"),
        );
    }
    if !is_executable(p) {
        return CheckResult::new(name, FAIL, format!("{path} is not executable"));
    }
    CheckResult::new(name, PASS, path)
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

/// `shutil.which` equivalent: PATH search only (no cwd-first search, unlike
/// `ralphus_runner::shellcmd::find_program`, which deliberately mimics a
/// shell's own resolution order for a different purpose).
fn which(program: &str) -> Option<String> {
    let path_var = std::env::var("PATH").ok()?;
    let mut suffixes = vec![String::new()];
    if ralphus_runner::hostos::is_windows() {
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
                return Some(candidate.to_string_lossy().into_owned());
            }
        }
    }
    None
}

fn check_daemon(daemon_url: &str) -> Vec<CheckResult> {
    let client = DaemonClient::new(daemon_url);
    match client.health() {
        Err(e) => vec![CheckResult::new(
            "daemon",
            FAIL,
            format!("unreachable at {daemon_url}: {e}"),
        )],
        Ok(health) => {
            let version = health["version"].as_str().unwrap_or("?");
            let name = health["name"].as_str().unwrap_or("?");
            let mut results = vec![CheckResult::new(
                "daemon",
                PASS,
                format!("reachable ({name} {version})"),
            )];
            if let Some(warnings) = health["warnings"].as_array() {
                for w in warnings {
                    results.push(CheckResult::new(
                        "daemon",
                        WARN,
                        w.as_str().unwrap_or_default().to_string(),
                    ));
                }
            }
            results
        }
    }
}

fn check_git() -> CheckResult {
    match which("git") {
        None => CheckResult::new(
            "git",
            FAIL,
            "not found on PATH (required for Guardian reviews)",
        ),
        Some(path) => CheckResult::new("git", PASS, path),
    }
}

/// Re-runs the same path/git-repo validation `ralphus project git` does at
/// registration time, for one already-registered project.
fn check_project_path(name: &str, path: &str) -> CheckResult {
    let check_name = format!("project:{name}");
    let p = Path::new(path);
    if !p.is_dir() {
        return CheckResult::new(
            &check_name,
            FAIL,
            format!("{path} does not exist or is not a directory"),
        );
    }
    let output = std::process::Command::new("git")
        .args(["-C", path, "rev-parse", "--is-inside-work-tree"])
        .output();
    match output {
        Err(e) => CheckResult::new(
            &check_name,
            FAIL,
            format!("could not run git in {path}: {e}"),
        ),
        Ok(out) => {
            let stdout = String::from_utf8_lossy(&out.stdout);
            if !out.status.success() || stdout.trim() != "true" {
                CheckResult::new(&check_name, FAIL, format!("{path} is not a git repository"))
            } else {
                CheckResult::new(&check_name, PASS, path)
            }
        }
    }
}

/// Validates every project registered with the daemon. Silently contributes
/// nothing if the daemon is unreachable ([`check_daemon`] already reports
/// that) or if no projects are registered.
fn check_projects(daemon_url: &str) -> Vec<CheckResult> {
    let client = DaemonClient::new(daemon_url);
    let Ok(response) = client.list_projects() else {
        return Vec::new();
    };
    response["projects"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|p| {
            let name = p["name"].as_str()?;
            let path = p["path"].as_str()?;
            Some(check_project_path(name, path))
        })
        .collect()
}

fn check_runner() -> CheckResult {
    let cmd = std::env::var("RALPHUS_RUNNER_CMD").unwrap_or_else(|_| "ralphus-runner".to_string());
    let program = cmd
        .split_whitespace()
        .next()
        .unwrap_or("ralphus-runner")
        .to_string();
    if which(&program).is_none() && !Path::new(&program).exists() {
        return CheckResult::new(
            "runner",
            WARN,
            format!("'{program}' not found (set RALPHUS_RUNNER_CMD)"),
        );
    }
    CheckResult::new("runner", PASS, program)
}

fn check_nvidia_smi() -> CheckResult {
    match which("nvidia-smi") {
        None => CheckResult::new(
            "nvidia-smi",
            WARN,
            "not found on PATH; GPU usage in the resource view will show N/A",
        ),
        Some(path) => CheckResult::new("nvidia-smi", PASS, path),
    }
}

fn check_ollama() -> CheckResult {
    let base = std::env::var("RALPHUS_OLLAMA_URL")
        .unwrap_or_else(|_| "http://localhost:11434/v1".to_string());
    let trimmed = base.trim_end_matches('/');
    let root = trimmed.strip_suffix("/v1").unwrap_or(trimmed);
    let tags_url = format!("{root}/api/tags");
    let reachable = ureq::get(&tags_url)
        .timeout(Duration::from_secs(2))
        .call()
        .is_ok();
    if reachable {
        CheckResult::new("ollama", PASS, base)
    } else {
        CheckResult::new(
            "ollama",
            FAIL,
            format!(
                "not reachable at {base} (required to run local models; start it with 'ollama serve')"
            ),
        )
    }
}

fn check_config(cwd: &Path) -> CheckResult {
    let config = crate::config::load_config(cwd, true);
    let mt = config.task.maximum_timeout_seconds;
    if mt < 0 {
        return CheckResult::new(
            "config",
            FAIL,
            format!(
                "task.maximum_timeout_seconds is {mt}; must be >= 0 (0 = unbounded, positive = seconds)"
            ),
        );
    }
    if mt == 0 {
        let src = config
            .sources
            .last()
            .map(|p| format!(" (from {})", p.display()))
            .unwrap_or_default();
        return CheckResult::new(
            "config",
            WARN,
            format!(
                "task.maximum_timeout_seconds is 0 (no timeout){src}; backend sessions may run forever"
            ),
        );
    }
    let src = config
        .sources
        .last()
        .map(|p| format!(" from {}", p.display()))
        .unwrap_or_else(|| " (default)".to_string());
    CheckResult::new(
        "config",
        PASS,
        format!("task.maximum_timeout_seconds={mt}s{src}"),
    )
}

/// Validates `.ralphus.toml`'s `[daemon] max_concurrent`. Mirrors
/// `check_config`'s `0 = unbounded` framing above, but with the opposite
/// polarity for negatives: a negative `max_concurrent` never reaches the
/// daemon's scheduler as-is (`DaemonConfig::max_concurrent` silently falls
/// back to `ralphus_daemon::DEFAULT_MAX_CONCURRENT` per that file's
/// "malformed config never blocks" rule), so this is a `warn`, not a `fail`
/// -- it just tells the user their value was ignored and what took its place.
fn check_max_concurrent(cwd: &Path) -> CheckResult {
    let config = crate::config::load_config(cwd, true);
    let Some(mc) = config.daemon.max_concurrent else {
        return CheckResult::new(
            "daemon-max-concurrent",
            PASS,
            format!(
                "daemon.max_concurrent unset; using default {}",
                ralphus_daemon::DEFAULT_MAX_CONCURRENT
            ),
        );
    };
    let src = config
        .sources
        .last()
        .map(|p| format!(" (from {})", p.display()))
        .unwrap_or_default();
    if mc < 0 {
        return CheckResult::new(
            "daemon-max-concurrent",
            WARN,
            format!(
                "daemon.max_concurrent is {mc}{src}; must be >= 0 (0 = no limit, positive = concurrency cap) -- falling back to default {}",
                ralphus_daemon::DEFAULT_MAX_CONCURRENT
            ),
        );
    }
    if mc == 0 {
        return CheckResult::new(
            "daemon-max-concurrent",
            WARN,
            format!(
                "daemon.max_concurrent is 0{src} (no limit); every ready cell may run at once, which can overwhelm the machine"
            ),
        );
    }
    CheckResult::new(
        "daemon-max-concurrent",
        PASS,
        format!("daemon.max_concurrent={mc}{src}"),
    )
}

/// Reads `[live_view]` as a raw TOML table from `path`, if the file exists,
/// parses, and declares that table -- used by
/// [`check_tool_arg_truncate_chars`] instead of `ralphus_daemon::config`'s
/// typed `LiveViewConfig` loader, since a typed `Option<u32>` deserialize
/// can't tell "key absent" apart from "key present but invalid" (both
/// collapse to `None` under `.unwrap_or_default()`), and this check needs to
/// warn on the latter.
fn read_live_view_table(path: &Path) -> Option<toml::Table> {
    let text = std::fs::read_to_string(path).ok()?;
    let root: toml::Table = text.parse().ok()?;
    root.get("live_view")?.as_table().cloned()
}

/// Validates `.ralphus.toml`'s `[live_view] tool_arg_truncate_chars`
/// (RAL-303) -- how many characters of a `tool_use` argument value the
/// claude-code backend renders into the Live View tmux pane before
/// truncating with a trailing `…`. Mirrors `check_max_concurrent`'s shape:
/// PASS reports the resolved value and its source; a present-but-invalid
/// value is a WARN, not a FAIL, since `ralphus_daemon::runner::RunnerSpec`
/// construction never blocks on it -- an invalid value is silently ignored
/// in favor of the default, same "malformed config never blocks" rule every
/// other `[live_view]`/`[daemon]` scalar follows.
fn check_tool_arg_truncate_chars(cwd: &Path) -> CheckResult {
    let global_path = ralphus_daemon::config::global_config_path();
    let project_path = ralphus_daemon::config::find_project_config(cwd);
    let global_table = global_path.as_deref().and_then(read_live_view_table);
    let project_table = project_path.as_deref().and_then(read_live_view_table);
    check_tool_arg_truncate_chars_for(
        global_table.as_ref(),
        global_path.as_deref(),
        project_table.as_ref(),
        project_path.as_deref(),
    )
}

/// Pure core of [`check_tool_arg_truncate_chars`], taking already-loaded raw
/// `[live_view]` tables -- split out so tests can exercise it without
/// mutating the real `RALPHUS_CONFIG_HOME`/current-directory environment,
/// same rationale as [`check_pull_request_branch_convention_for`].
/// Per-project wins over global, same layering as `load_live_view_config`.
fn check_tool_arg_truncate_chars_for(
    global_table: Option<&toml::Table>,
    global_path: Option<&Path>,
    project_table: Option<&toml::Table>,
    project_path: Option<&Path>,
) -> CheckResult {
    let (raw, src) = if let Some(v) = project_table.and_then(|t| t.get("tool_arg_truncate_chars")) {
        (Some(v), project_path)
    } else if let Some(v) = global_table.and_then(|t| t.get("tool_arg_truncate_chars")) {
        (Some(v), global_path)
    } else {
        (None, None)
    };

    let Some(raw) = raw else {
        return CheckResult::new(
            "tool-arg-truncate-chars",
            PASS,
            format!(
                "live_view.tool_arg_truncate_chars unset; using default {}",
                ralphus_daemon::config::DEFAULT_TOOL_ARG_TRUNCATE_CHARS
            ),
        );
    };
    let src = src
        .map(|p| format!(" (from {})", p.display()))
        .unwrap_or_default();
    match raw.as_integer() {
        None => CheckResult::new(
            "tool-arg-truncate-chars",
            WARN,
            format!(
                "live_view.tool_arg_truncate_chars is not a number{src} -- falling back to the default {}",
                ralphus_daemon::config::DEFAULT_TOOL_ARG_TRUNCATE_CHARS
            ),
        ),
        Some(n) if n < 0 => CheckResult::new(
            "tool-arg-truncate-chars",
            WARN,
            format!(
                "live_view.tool_arg_truncate_chars is {n}{src}; must be >= 0 -- falling back to the default {}",
                ralphus_daemon::config::DEFAULT_TOOL_ARG_TRUNCATE_CHARS
            ),
        ),
        Some(n) => CheckResult::new(
            "tool-arg-truncate-chars",
            PASS,
            format!("live_view.tool_arg_truncate_chars={n}{src}"),
        ),
    }
}

/// Validates `.ralphus.toml`'s `[forge] pull_request_branch_convention`
/// (RAL-244) for `cwd`, reusing the daemon's own layered resolution
/// (`ralphus_daemon::config::resolve_forge` -- global config under the
/// nearest per-project `.ralphus.toml`) rather than re-parsing the file
/// client-side, so this can never disagree with what `pr.rs` actually uses
/// at submission time. Pure filesystem parsing with no daemon-process-
/// specific state (unlike `check_agent_profiles`'s `from_env`/`executable`
/// resolution), so -- like `check_config` -- this never needs the daemon to
/// be reachable. An explicitly-set-but-invalid convention is a hard `fail`
/// (RAL-244 interview decision), unlike every other `.ralphus.toml` string
/// field, which silently falls back to its default.
fn check_pull_request_branch_convention(cwd: &Path) -> CheckResult {
    check_pull_request_branch_convention_for(&ralphus_daemon::config::resolve_forge(cwd))
}

/// Pure core of [`check_pull_request_branch_convention`], taking an
/// already-resolved [`ralphus_daemon::config::ForgeConfig`] -- split out so
/// tests can exercise it without mutating the real `RALPHUS_CONFIG_HOME`/
/// `USERPROFILE` environment (this workspace forbids `unsafe`, which
/// `std::env::set_var` requires), same rationale as `config.rs`'s
/// `configuration_path_env` threading.
fn check_pull_request_branch_convention_for(
    forge_cfg: &ralphus_daemon::config::ForgeConfig,
) -> CheckResult {
    let Some(convention) = forge_cfg.pull_request_branch_convention.clone() else {
        return CheckResult::new(
            "pull-request-branch-convention",
            PASS,
            format!(
                "not set (defaults to '{}')",
                ralphus_daemon::config::DEFAULT_PR_BRANCH_CONVENTION
            ),
        );
    };
    match ralphus_daemon::config::validate_pull_request_branch_convention(&convention) {
        Ok(()) => CheckResult::new("pull-request-branch-convention", PASS, convention),
        Err(e) => CheckResult::new("pull-request-branch-convention", FAIL, e),
    }
}

/// Validates `[[templates]]` and `[ui] new_task_default_tab` (RAL-297: the
/// Simple task form's template picker and its default-tab config). Local
/// only (no daemon round-trip) -- reuses `ralphus_daemon::config`'s own
/// resolver/validator functions directly, the same "check health can never
/// disagree with what actually gets used" precedent as
/// [`check_pull_request_branch_convention_for`]. Note these loaders are
/// current-dir-based (the CLI process's own cwd), the same convention
/// `ralphus_daemon::config`'s `[daemon]`/`[cartographer]`/`[env_overrides]`/
/// `[cors]` loaders already use -- there is no per-request-cwd resolver for
/// this config category yet, so a misconfigured malformed individual
/// `[[templates]]` entry is reported by name/message rather than silently
/// dropped, unlike [`crate::config`]'s "malformed config never blocks" rule
/// for the *file as a whole*.
fn check_templates() -> Vec<CheckResult> {
    check_templates_for(
        &ralphus_daemon::config::load_templates_config(),
        &ralphus_daemon::config::load_ui_config(),
    )
}

/// Pure core of [`check_templates`], taking already-loaded config -- split
/// out so tests can exercise it without mutating the real
/// `RALPHUS_CONFIG_HOME`/current-directory environment, same rationale as
/// [`check_pull_request_branch_convention_for`].
fn check_templates_for(
    templates: &[ralphus_daemon::config::TemplateDef],
    ui: &ralphus_daemon::config::UiConfig,
) -> Vec<CheckResult> {
    let mut results = Vec::new();
    if templates.is_empty() {
        results.push(CheckResult::new(
            "templates",
            PASS,
            format!(
                "no [[templates]] configured -- using the built-in \"{}\" fallback",
                ralphus_daemon::config::DEFAULT_TEMPLATE_NAME
            ),
        ));
    } else {
        let errors = ralphus_daemon::config::validate_templates(templates);
        if errors.is_empty() {
            results.push(CheckResult::new(
                "templates",
                PASS,
                format!("{} template(s) configured", templates.len()),
            ));
        } else {
            for e in errors {
                results.push(CheckResult::new("templates", FAIL, e.message));
            }
        }
    }
    match &ui.new_task_default_tab {
        None => results.push(CheckResult::new(
            "new-task-default-tab",
            PASS,
            "not set (defaults to 'simple')",
        )),
        Some(tab) => match ralphus_daemon::config::validate_new_task_default_tab(tab) {
            Ok(()) => results.push(CheckResult::new("new-task-default-tab", PASS, tab.clone())),
            Err(e) => results.push(CheckResult::new("new-task-default-tab", FAIL, e)),
        },
    }
    results
}

/// Delegates to `GET /api/health/agent-profiles`, which runs entirely
/// inside the daemon process -- `from_env`/`executable` resolution has to
/// happen there, since the daemon may have been started with a different
/// environment/PATH than whatever shell is running this CLI (a background
/// service, a stale terminal, ...). Checking client-side would silently
/// verify the wrong process.
fn check_agent_profiles(daemon_url: &str, cwd: &Path) -> Vec<CheckResult> {
    let client = DaemonClient::new(daemon_url);
    let response = match client.health_agent_profiles(cwd) {
        Ok(response) => response,
        Err(e) => {
            return vec![CheckResult::new(
                "agent-profiles",
                FAIL,
                format!("could not reach daemon to check agent profiles: {e}"),
            )];
        }
    };
    response["profiles"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|p| {
            let status = if p["status"].as_str() == Some(PASS) {
                PASS
            } else {
                FAIL
            };
            CheckResult::new(
                p["name"].as_str().unwrap_or("agent-profile"),
                status,
                p["detail"].as_str().unwrap_or_default(),
            )
        })
        .collect()
}

/// Validates that `.ralphus.toml`'s `[review].default_resolver_agent`
/// (RAL-?, `ralphus_daemon::config::ReviewConfig::default_resolver_agent`)
/// names a real, currently-selectable agent -- a built-in backend or a
/// configured `[agent.profiles.*]` entry -- for `cwd`. Delegates to
/// `GET /api/agents`, the same daemon-side resolution the board's
/// review-resolver dropdown uses, so this can never disagree with what a
/// review actually falls back to (mirrors `check_agent_profiles`'s
/// rationale for staying daemon-side rather than re-parsing `.ralphus.toml`
/// client-side).
fn check_default_resolver_agent(daemon_url: &str, cwd: &Path) -> CheckResult {
    let client = DaemonClient::new(daemon_url);
    let response = match client.list_agents(cwd) {
        Ok(response) => response,
        Err(e) => {
            return CheckResult::new(
                "default-resolver-agent",
                FAIL,
                format!("could not reach daemon to check the default resolver agent: {e}"),
            );
        }
    };
    let default_agent = response["default_agent"].as_str().unwrap_or("ollama");
    let known = response["agents"]
        .as_array()
        .into_iter()
        .flatten()
        .any(|a| a["id"].as_str() == Some(default_agent));
    if known {
        CheckResult::new("default-resolver-agent", PASS, default_agent)
    } else {
        CheckResult::new(
            "default-resolver-agent",
            FAIL,
            format!(
                "[review].default_resolver_agent = \"{default_agent}\" does not match any \
                 built-in backend or configured [agent.profiles.*] entry for this project"
            ),
        )
    }
}

fn check_cargo() -> CheckResult {
    match which("cargo") {
        None => CheckResult::developer(
            "cargo",
            FAIL,
            "cargo not found on PATH (required to build the Rust binaries; install it via https://rustup.rs)",
        ),
        Some(path) => CheckResult::developer("cargo", PASS, path),
    }
}

/// Runs the core health checks; adds the developer section when opted in.
#[must_use]
pub fn run_checks(daemon_url: &str, cwd: &Path, enable_developer_checks: bool) -> Vec<CheckResult> {
    let mut results = check_daemon(daemon_url);
    results.push(check_git());
    results.extend(check_projects(daemon_url));
    results.push(check_runner());
    results.push(check_ollama());
    results.push(check_nvidia_smi());
    results.push(check_config(cwd));
    results.push(check_max_concurrent(cwd));
    results.push(check_tool_arg_truncate_chars(cwd));
    results.push(check_pull_request_branch_convention(cwd));
    results.extend(check_templates());
    results.extend(check_agent_profiles(daemon_url, cwd));
    results.push(check_default_resolver_agent(daemon_url, cwd));
    results.push(check_agent_command(
        "claude-command",
        "RALPHUS_CLAUDE_COMMAND",
        "claude",
    ));
    results.push(check_agent_command(
        "codex-command",
        "RALPHUS_CODEX_COMMAND",
        "codex",
    ));
    results.push(check_agent_command(
        "pi-command",
        "RALPHUS_PI_COMMAND",
        "pi",
    ));
    if enable_developer_checks {
        results.push(check_cargo());
    }
    results
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_compound_shell_command_detects_multiword_unquoted() {
        assert!(is_compound_shell_command("cd foo bar ; ./claude"));
        assert!(!is_compound_shell_command("claude"));
        assert!(!is_compound_shell_command(
            "\"C:\\Program Files\\claude\\claude.exe\""
        ));
    }

    #[test]
    fn unquote_path_strips_one_layer() {
        assert_eq!(unquote_path("\"C:\\a b\\c.exe\""), "C:\\a b\\c.exe");
        assert_eq!(unquote_path("plain"), "plain");
    }

    #[test]
    fn check_result_is_fail_only_for_fail_status() {
        let fail = CheckResult::new("x", FAIL, "boom");
        let warn = CheckResult::new("x", WARN, "meh");
        assert!(fail.is_fail());
        assert!(!warn.is_fail());
    }

    #[test]
    fn check_agent_command_passes_when_unset() {
        // Use a var name guaranteed not to be set in any real environment.
        let result = check_agent_command("x-command", "RALPHUS_TEST_UNSET_AGENT_VAR_XYZ", "claude");
        assert_eq!(result.status, PASS);
        assert!(result.detail.contains("not set"));
    }

    #[test]
    fn which_finds_a_program_known_to_exist_on_path() {
        // `git` is a hard requirement elsewhere in this suite's own dev
        // environment expectations (health check itself requires it).
        assert!(which("git").is_some() || std::env::var("PATH").is_err());
    }

    #[test]
    fn check_pull_request_branch_convention_passes_when_unset() {
        let result = check_pull_request_branch_convention_for(
            &ralphus_daemon::config::ForgeConfig::default(),
        );
        assert_eq!(result.status, PASS);
        assert!(result.detail.contains("{name}-review"));
    }

    #[test]
    fn check_pull_request_branch_convention_passes_when_valid() {
        let cfg = ralphus_daemon::config::ForgeConfig {
            pull_request_branch_convention: Some("review-{name}".to_string()),
            ..ralphus_daemon::config::ForgeConfig::default()
        };
        let result = check_pull_request_branch_convention_for(&cfg);
        assert_eq!(result.status, PASS);
        assert_eq!(result.detail, "review-{name}");
    }

    #[test]
    fn check_pull_request_branch_convention_fails_when_missing_placeholder() {
        let cfg = ralphus_daemon::config::ForgeConfig {
            pull_request_branch_convention: Some("static-branch".to_string()),
            ..ralphus_daemon::config::ForgeConfig::default()
        };
        let result = check_pull_request_branch_convention_for(&cfg);
        assert_eq!(result.status, FAIL);
        assert!(result.detail.contains("{name}"));
    }

    // ── check_tool_arg_truncate_chars (RAL-303) ───────────────────────────

    #[test]
    fn check_tool_arg_truncate_chars_passes_unset() {
        let result = check_tool_arg_truncate_chars_for(None, None, None, None);
        assert_eq!(result.status, PASS);
        assert!(result.detail.contains("unset"));
        assert!(
            result
                .detail
                .contains(&ralphus_daemon::config::DEFAULT_TOOL_ARG_TRUNCATE_CHARS.to_string())
        );
    }

    #[test]
    fn check_tool_arg_truncate_chars_passes_with_a_valid_value() {
        let table: toml::Table = "tool_arg_truncate_chars = 400".parse().unwrap();
        let path = Path::new("/tmp/.ralphus.toml");
        let result = check_tool_arg_truncate_chars_for(None, None, Some(&table), Some(path));
        assert_eq!(result.status, PASS);
        assert!(result.detail.contains("tool_arg_truncate_chars=400"));
        assert!(result.detail.contains("/tmp/.ralphus.toml"));
    }

    #[test]
    fn check_tool_arg_truncate_chars_warns_on_negative_value() {
        let table: toml::Table = "tool_arg_truncate_chars = -5".parse().unwrap();
        let result = check_tool_arg_truncate_chars_for(None, None, Some(&table), None);
        assert_eq!(result.status, WARN);
        assert!(result.detail.contains("-5"));
        assert!(
            result
                .detail
                .contains(&ralphus_daemon::config::DEFAULT_TOOL_ARG_TRUNCATE_CHARS.to_string())
        );
    }

    #[test]
    fn check_tool_arg_truncate_chars_warns_on_non_numeric_value() {
        let table: toml::Table = "tool_arg_truncate_chars = \"lots\"".parse().unwrap();
        let result = check_tool_arg_truncate_chars_for(None, None, Some(&table), None);
        assert_eq!(result.status, WARN);
        assert!(result.detail.contains("not a number"));
    }

    #[test]
    fn check_tool_arg_truncate_chars_prefers_project_over_global() {
        let global: toml::Table = "tool_arg_truncate_chars = 100".parse().unwrap();
        let project: toml::Table = "tool_arg_truncate_chars = 500".parse().unwrap();
        let global_path = Path::new("/global/config.toml");
        let project_path = Path::new("/project/.ralphus.toml");
        let result = check_tool_arg_truncate_chars_for(
            Some(&global),
            Some(global_path),
            Some(&project),
            Some(project_path),
        );
        assert_eq!(result.status, PASS);
        assert!(result.detail.contains("tool_arg_truncate_chars=500"));
        assert!(result.detail.contains("/project/.ralphus.toml"));
    }

    // ── check_templates (RAL-297) ─────────────────────────────────────────

    #[test]
    fn check_templates_passes_with_no_templates_configured() {
        let results = check_templates_for(&[], &ralphus_daemon::config::UiConfig::default());
        assert!(results.iter().all(|r| r.status == PASS));
        assert!(
            results
                .iter()
                .any(|r| r.name == "templates" && r.detail.contains("hello-world"))
        );
    }

    #[test]
    fn check_templates_passes_with_a_valid_template() {
        let templates = vec![ralphus_daemon::config::TemplateDef {
            name: "standard".to_string(),
            prompt_template: Some("{prompt}".to_string()),
            ..ralphus_daemon::config::TemplateDef::default()
        }];
        let results = check_templates_for(&templates, &ralphus_daemon::config::UiConfig::default());
        let templates_result = results.iter().find(|r| r.name == "templates").unwrap();
        assert_eq!(templates_result.status, PASS);
    }

    #[test]
    fn check_templates_fails_on_a_malformed_entry() {
        let templates = vec![ralphus_daemon::config::TemplateDef {
            name: "broken".to_string(),
            prompt_template: None,
            ..ralphus_daemon::config::TemplateDef::default()
        }];
        let results = check_templates_for(&templates, &ralphus_daemon::config::UiConfig::default());
        let templates_result = results.iter().find(|r| r.name == "templates").unwrap();
        assert_eq!(templates_result.status, FAIL);
        assert!(templates_result.detail.contains("prompt_template"));
    }

    #[test]
    fn check_templates_validates_new_task_default_tab() {
        let ok = ralphus_daemon::config::UiConfig {
            new_task_default_tab: Some("paste".to_string()),
        };
        let results = check_templates_for(&[], &ok);
        let tab_result = results
            .iter()
            .find(|r| r.name == "new-task-default-tab")
            .unwrap();
        assert_eq!(tab_result.status, PASS);

        let bad = ralphus_daemon::config::UiConfig {
            new_task_default_tab: Some("bogus".to_string()),
        };
        let results = check_templates_for(&[], &bad);
        let tab_result = results
            .iter()
            .find(|r| r.name == "new-task-default-tab")
            .unwrap();
        assert_eq!(tab_result.status, FAIL);
    }
}
