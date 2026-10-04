//! RAL-571: per-backend "is this agent CLI logged in?" probes, run by the
//! daemon's health sweep. Each backend that can be probed implements
//! [`LoginProbe`]: it names the status subcommand to run and parses that
//! subcommand's output into a [`LoginStatus`] -- the parse is pure, so it is
//! the test seam (no real `claude`/`codex` is ever invoked by a unit test).
//! [`login_command`](LoginProbe::login_command) is the human-run fix and is
//! the hook an interactive setup flow can reuse.
//!
//! Both status subcommands are offline: they report whether credentials are
//! present, not whether a token is still valid or unexpired. The account
//! email is deliberately never surfaced.
//!
//! Platform behavior: the status commands run against the config dir the
//! runner copies credentials from (`CLAUDE_CONFIG_DIR`/`CODEX_HOME`, else
//! `~/.claude`/`~/.codex`). On macOS, Claude Code keeps credentials in the
//! Keychain, which `claude auth status` reads but the runner's per-cell
//! isolation does not copy, so a pass there can overstate what an isolated
//! cell sees ([`LoginProbe::platform_caveat`]).

use std::io::Read as _;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::agent_isolation::home_dir;
use crate::claude_code_backend::ClaudeCodeBackend;
use crate::codex_backend::CodexBackend;

/// Whether a backend CLI is authenticated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoginState {
    LoggedIn,
    LoggedOut,
    /// The status command ran but its output was not recognizable (e.g. an
    /// older CLI without the status subcommand).
    Unknown,
}

/// A parsed status-command outcome. `summary` names the auth method and
/// organization -- never an email address.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoginStatus {
    pub state: LoginState,
    pub summary: String,
}

impl LoginStatus {
    fn new(state: LoginState, summary: impl Into<String>) -> Self {
        Self {
            state,
            summary: summary.into(),
        }
    }
}

/// One backend's login probe.
pub trait LoginProbe: Sync {
    /// The agent backend name (`claude-code`, `codex`), matching
    /// `agent_backend_commands.backend`.
    fn backend_name(&self) -> &'static str;
    fn display_name(&self) -> &'static str;
    /// The health-catalog id of this backend's login check.
    fn health_id(&self) -> &'static str;
    /// Env var overriding the program (`RALPHUS_CLAUDE_COMMAND`, ...).
    fn command_env_var(&self) -> &'static str;
    fn default_program(&self) -> &'static str;
    /// Arguments that make the CLI print its login status.
    fn status_args(&self) -> &'static [&'static str];
    /// Env var naming the CLI's config dir (`CLAUDE_CONFIG_DIR`, ...).
    fn config_dir_env(&self) -> &'static str;
    /// Directory under the home dir used when [`Self::config_dir_env`] is unset.
    fn default_config_dirname(&self) -> &'static str;
    /// The command a human runs to log in.
    fn login_command(&self) -> &'static str;
    /// Arguments an interactive setup flow passes to the backend program to
    /// start its login. `None` when the flow cannot run inline in this kind of
    /// session (`ssh_session`: the browser OAuth callback is unreachable), so
    /// the caller should print [`Self::login_command`] for the user instead.
    fn login_args(&self, ssh_session: bool) -> Option<&'static [&'static str]>;
    /// Pure interpretation of a finished status command. `env_present`
    /// reports whether an env var is set to a non-empty value.
    fn parse(&self, output: &str, exit_ok: bool, env_present: &dyn Fn(&str) -> bool)
    -> LoginStatus;
    /// A caveat appended to a logged-in report on platforms where the
    /// status command can see credentials an isolated cell cannot.
    fn platform_caveat(&self) -> Option<&'static str> {
        None
    }

    /// The config dir the runner copies credentials from.
    fn config_dir(&self) -> Option<PathBuf> {
        match std::env::var_os(self.config_dir_env()) {
            Some(dir) if !dir.is_empty() => Some(PathBuf::from(dir)),
            _ => home_dir().map(|home| home.join(self.default_config_dirname())),
        }
    }
}

static CLAUDE_CODE: ClaudeCodeBackend = ClaudeCodeBackend {
    keep_temporary_files: false,
    program_override: None,
};
static CODEX: CodexBackend = CodexBackend {
    keep_temporary_files: false,
    program_override: None,
};

/// Every backend with a login probe. Pi and others join by implementing
/// [`LoginProbe`] and being listed here.
#[must_use]
pub fn login_probes() -> Vec<&'static dyn LoginProbe> {
    vec![&CLAUDE_CODE, &CODEX]
}

impl LoginProbe for ClaudeCodeBackend {
    fn backend_name(&self) -> &'static str {
        "claude-code"
    }
    fn display_name(&self) -> &'static str {
        "Claude Code"
    }
    fn health_id(&self) -> &'static str {
        ralphus_core::health_catalog::ID_CLAUDE_CODE_LOGIN
    }
    fn command_env_var(&self) -> &'static str {
        "RALPHUS_CLAUDE_COMMAND"
    }
    fn default_program(&self) -> &'static str {
        "claude"
    }
    fn status_args(&self) -> &'static [&'static str] {
        &["auth", "status"]
    }
    fn config_dir_env(&self) -> &'static str {
        "CLAUDE_CONFIG_DIR"
    }
    fn default_config_dirname(&self) -> &'static str {
        ".claude"
    }
    fn login_command(&self) -> &'static str {
        "claude auth login"
    }
    fn login_args(&self, ssh_session: bool) -> Option<&'static [&'static str]> {
        (!ssh_session).then_some(&["auth", "login"])
    }
    fn platform_caveat(&self) -> Option<&'static str> {
        cfg!(target_os = "macos").then_some(
            "macOS: credentials may live in the Keychain, which isolated cells do not inherit",
        )
    }

    fn parse(
        &self,
        output: &str,
        _exit_ok: bool,
        _env_present: &dyn Fn(&str) -> bool,
    ) -> LoginStatus {
        let Ok(json) = serde_json::from_str::<Value>(output.trim()) else {
            return LoginStatus::new(
                LoginState::Unknown,
                format!(
                    "unrecognized `claude auth status` output: {}",
                    first_line(output)
                ),
            );
        };
        let text = |key: &str| {
            json.get(key)
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
        };
        match json.get("loggedIn").and_then(Value::as_bool) {
            Some(true) => {
                let method = text("authMethod").unwrap_or("unknown method");
                let mut parts = vec![match text("subscriptionType") {
                    Some(sub) => format!("{method} ({sub})"),
                    None => method.to_string(),
                }];
                if let Some(source) = text("apiKeySource") {
                    parts.push(format!("key from {source} (unverified)"));
                }
                if let Some(org) = text("orgName") {
                    parts.push(org.to_string());
                }
                LoginStatus::new(LoginState::LoggedIn, parts.join(" · "))
            }
            Some(false) => LoginStatus::new(LoginState::LoggedOut, "not logged in"),
            None => LoginStatus::new(
                LoginState::Unknown,
                "`claude auth status` did not report loggedIn",
            ),
        }
    }
}

/// Env vars Codex accepts as an API key but `codex login status` ignores.
const CODEX_ENV_KEYS: [&str; 2] = ["CODEX_API_KEY", "OPENAI_API_KEY"];

impl LoginProbe for CodexBackend {
    fn backend_name(&self) -> &'static str {
        "codex"
    }
    fn display_name(&self) -> &'static str {
        "Codex"
    }
    fn health_id(&self) -> &'static str {
        ralphus_core::health_catalog::ID_CODEX_LOGIN
    }
    fn command_env_var(&self) -> &'static str {
        "RALPHUS_CODEX_COMMAND"
    }
    fn default_program(&self) -> &'static str {
        "codex"
    }
    fn status_args(&self) -> &'static [&'static str] {
        &["login", "status"]
    }
    fn config_dir_env(&self) -> &'static str {
        "CODEX_HOME"
    }
    fn default_config_dirname(&self) -> &'static str {
        ".codex"
    }
    fn login_command(&self) -> &'static str {
        "codex login"
    }
    fn login_args(&self, ssh_session: bool) -> Option<&'static [&'static str]> {
        Some(if ssh_session {
            &["login", "--device-auth"]
        } else {
            &["login"]
        })
    }

    fn parse(
        &self,
        output: &str,
        _exit_ok: bool,
        env_present: &dyn Fn(&str) -> bool,
    ) -> LoginStatus {
        let line = first_line(output);
        let lower = line.to_ascii_lowercase();
        if lower.contains("not logged in") {
            if let Some(name) = CODEX_ENV_KEYS.iter().find(|name| env_present(name)) {
                return LoginStatus::new(
                    LoginState::LoggedIn,
                    format!("env-var key `{name}` present (unverified)"),
                );
            }
            return LoginStatus::new(LoginState::LoggedOut, "not logged in");
        }
        if lower.contains("logged in") {
            let method = line
                .split_once("using ")
                .map_or("unknown method", |(_, rest)| rest.trim());
            return LoginStatus::new(LoginState::LoggedIn, method);
        }
        LoginStatus::new(
            LoginState::Unknown,
            format!("unrecognized `codex login status` output: {line}"),
        )
    }
}

fn first_line(text: &str) -> &str {
    text.trim().lines().next().unwrap_or("")
}

/// A finished (or failed) status-command run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StatusRun {
    SpawnFailed(String),
    TimedOut,
    Finished {
        exit_ok: bool,
        /// stdout, falling back to stderr when stdout is empty (some CLIs
        /// print status text on stderr).
        output: String,
    },
}

/// Runs `<program> <args>` bounded by `timeout`, capturing its output.
#[must_use]
pub fn run_status_command(program: &str, args: &[&str], timeout: Duration) -> StatusRun {
    let mut child = match Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(child) => child,
        Err(error) => {
            return StatusRun::SpawnFailed(format!("could not run {program}: {error}"));
        }
    };
    let mut stdout_pipe = child.stdout.take();
    let mut stderr_pipe = child.stderr.take();
    let stdout_thread = std::thread::spawn(move || {
        let mut buf = String::new();
        if let Some(pipe) = stdout_pipe.as_mut() {
            let _ = pipe.read_to_string(&mut buf);
        }
        buf
    });
    let stderr_thread = std::thread::spawn(move || {
        let mut buf = String::new();
        if let Some(pipe) = stderr_pipe.as_mut() {
            let _ = pipe.read_to_string(&mut buf);
        }
        buf
    });
    let start = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {}
            Err(error) => {
                return StatusRun::SpawnFailed(format!("{program}: {error}"));
            }
        }
        if start.elapsed() >= timeout {
            let _ = child.kill();
            let _ = child.wait();
            return StatusRun::TimedOut;
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    let stdout = stdout_thread.join().unwrap_or_default();
    let stderr = stderr_thread.join().unwrap_or_default();
    StatusRun::Finished {
        exit_ok: status.success(),
        output: if stdout.trim().is_empty() {
            stderr
        } else {
            stdout
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn no_env(_: &str) -> bool {
        false
    }

    const CLAUDE_LOGGED_IN: &str = r#"{"loggedIn": true, "authMethod": "claude.ai", "apiProvider": "firstParty", "configDirectory": "/home/x/.claude", "email": "someone@example.com", "orgId": "abc", "orgName": "Acme", "subscriptionType": "max"}"#;

    #[test]
    fn claude_logged_in_reports_method_and_org_but_never_email() {
        let status = CLAUDE_CODE.parse(CLAUDE_LOGGED_IN, true, &no_env);
        assert_eq!(status.state, LoginState::LoggedIn);
        assert_eq!(status.summary, "claude.ai (max) · Acme");
        assert!(!status.summary.contains("someone@example.com"));
        assert!(!status.summary.contains('@'));
    }

    #[test]
    fn claude_logged_out_is_logged_out() {
        let out = r#"{"loggedIn": false, "authMethod": "none"}"#;
        let status = CLAUDE_CODE.parse(out, false, &no_env);
        assert_eq!(status.state, LoginState::LoggedOut);
    }

    #[test]
    fn claude_env_var_api_key_is_logged_in_with_its_source() {
        let out =
            r#"{"loggedIn": true, "authMethod": "api_key", "apiKeySource": "ANTHROPIC_API_KEY"}"#;
        let status = CLAUDE_CODE.parse(out, true, &no_env);
        assert_eq!(status.state, LoginState::LoggedIn);
        assert_eq!(
            status.summary,
            "api_key · key from ANTHROPIC_API_KEY (unverified)"
        );
    }

    #[test]
    fn claude_unrecognized_output_is_unknown() {
        let status = CLAUDE_CODE.parse("error: unknown command 'auth'", false, &no_env);
        assert_eq!(status.state, LoginState::Unknown);
        let status = CLAUDE_CODE.parse(r#"{"other": 1}"#, true, &no_env);
        assert_eq!(status.state, LoginState::Unknown);
    }

    #[test]
    fn codex_logged_in_reports_method() {
        let status = CODEX.parse("Logged in using ChatGPT\n", true, &no_env);
        assert_eq!(status.state, LoginState::LoggedIn);
        assert_eq!(status.summary, "ChatGPT");
    }

    #[test]
    fn codex_logged_out_is_logged_out() {
        let status = CODEX.parse("Not logged in\n", false, &no_env);
        assert_eq!(status.state, LoginState::LoggedOut);
    }

    #[test]
    fn codex_env_var_key_counts_when_status_says_logged_out() {
        let env = |name: &str| name == "OPENAI_API_KEY";
        let status = CODEX.parse("Not logged in", false, &env);
        assert_eq!(status.state, LoginState::LoggedIn);
        assert_eq!(
            status.summary,
            "env-var key `OPENAI_API_KEY` present (unverified)"
        );
    }

    #[test]
    fn codex_unrecognized_output_is_unknown() {
        let status = CODEX.parse("error: unrecognized subcommand 'status'", false, &no_env);
        assert_eq!(status.state, LoginState::Unknown);
    }

    #[test]
    fn missing_binary_is_a_spawn_failure() {
        let run = run_status_command(
            "ralphus-definitely-not-a-real-binary-ral571",
            &["auth", "status"],
            Duration::from_secs(1),
        );
        assert!(matches!(run, StatusRun::SpawnFailed(_)), "{run:?}");
    }

    #[test]
    fn probes_cover_both_backends_with_distinct_names() {
        let names: Vec<_> = login_probes().iter().map(|p| p.backend_name()).collect();
        assert_eq!(names, ["claude-code", "codex"]);
    }
}
