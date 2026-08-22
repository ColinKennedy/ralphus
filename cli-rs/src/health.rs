//! `ralphus check health` -- check that the local setup can actually run
//! tasks, ported from `cli/src/ralphus/health.py`.
//!
//! Checks are grouped into sections:
//! - `core`: things every user needs (daemon reachable, git, every
//!   registered project's on-disk path/git-repo validity, the runner
//!   binary, Ollama for local-model runs, `nvidia-smi` for GPU metrics,
//!   `$RALPHUS_CLAUDE_COMMAND`/`$RALPHUS_CODEX_COMMAND` when set). Hard
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
}
