//! `ralphus initialize server` (RAL-501): the interactive, hidden new-
//! installation walkthrough. Deliberately absent from `help_map.rs` (see the
//! `resolved_path` carve-out there), so it never appears in `--help`,
//! `show help-map`, or the help-map-derived MCP tool surface, yet is still
//! directly invocable as `ralphus initialize server [--yes]`. Every answer
//! also has a flag, allowing a complete setup to run without a terminal.
//!
//! `--yes` accepts every stage's default answer instead of prompting, for
//! non-interactive/scripted runs; without it, a non-TTY stdin is rejected
//! the same way `ralphus mcp initialize` rejects one (see `mcp.rs`).
//! Boolean answer flags take `yes` or `no`: `--install-tmux`,
//! `--setup-mcp`, `--register-project`, `--review-auto-submit-pr-stack`,
//! `--require-forks`, `--create-admin`, `--setup-forge-token`, `--submit-sample`.
//! The value flags are `--tmux-program`, repeatable
//! `--mcp-host`, `--agent-logins` (`claude,codex`, `all`, or `none`), `--project-name`, `--project-description`, `--project-is-fork`,
//! `--project-fork-url`, `--project-url`,
//! `--fork-user`, `--fork-url`, `--forge-provider`,
//! `--forge-host`, `--forge-token`, `--admin-name`,
//! `--review-resolver-agent`, `--sample-mode`, and `--sample-agent`.
//!
//! Project registration (`--register-project`) is asked up front, before any
//! other project question, and the actual `register_project`/review-settings/
//! fork daemon calls only happen once every project, review-defaults, and
//! fork-requirements answer has been collected (end of the "Configure fork
//! requirements" step) -- never mid-step-4, so there is no point where only
//! part of a project's settings have been committed. Auto-review thresholds
//! are not customizable here; every project registers with the recommended
//! defaults (bug=3, feature=5, investigation=3, unclassified=5). Likewise a
//! registered project always gets a bundle of recommended review-setting
//! defaults: `auto_fix_pr_errors`, `discourage_tests_during_auto_pull_request_fixes`,
//! and `rebuild_on = [feedback, auto_fix]`; `dual_root_pr` is additionally
//! enabled whenever the project itself is a fork or requires contributor forks.
//! Any fork URL registered here (project-wide or a contributor's) also gets
//! its local git remote (`fork`/`fork-<user>`) created or repointed right
//! away, rather than left to the daemon's lazy, submit-time
//! `ensure_fork_remote` -- so `ralphus check health`'s fork-remote check
//! never has to warn about a remote that is not there yet.
//!
//! Every run ends by writing an answers file (see [`answers`]) recording each
//! setting's value and source; `--answers-file <path>` replays one, with flags
//! overriding it and it overriding the prompts and defaults.

use std::io::{IsTerminal as _, Write as _};
use std::path::PathBuf;

mod answers;

use crate::args::GlobalOpts;
use crate::client::ProjectReviewSettingsPatch;
use crate::health::CheckResult;
use answers::Source;
use ralphus_core::git_remote::{default_upstream_remote, find_remote_for_url};
use ralphus_runner::login_probe::{LoginProbe, LoginState, LoginStatus, StatusRun, login_probes};

const TOTAL_STEPS: u32 = 10;
const WINDOWS_MINIMUM_TMUX_VERSION: (u32, u32, u32) = (3, 3, 8);
const SAMPLE_LABEL_PREFIX: &str = "ralphus initialize server: hello world";

pub(crate) fn validate_forge_host(host: &str) -> Result<(), String> {
    let lower = host.to_ascii_lowercase();
    if host.is_empty()
        || lower.starts_with("http://")
        || lower.starts_with("https://")
        || lower.starts_with("www.")
        || host.contains(['/', ':', '?', '#', '@'])
        || host.chars().any(char::is_whitespace)
    {
        return Err(
            "expected a bare hostname such as gitlab.com (without http(s):// or www.)".to_string(),
        );
    }
    if host.split('.').any(|label| {
        label.is_empty()
            || label.starts_with('-')
            || label.ends_with('-')
            || !label
                .chars()
                .all(|character| character.is_ascii_alphanumeric() || character == '-')
    }) {
        return Err("expected a valid bare hostname such as gitlab.com".to_string());
    }
    Ok(())
}

fn validate_project_fork_url(url: &str) -> Result<(), String> {
    if !url.starts_with("https://") {
        return Err(
            "fork clone URLs must use HTTPS (for example https://github.com/owner/repo.git)"
                .to_string(),
        );
    }
    Ok(())
}

/// One answer exposed through both the terminal walkthrough and the
/// non-interactive command line. Keep this list complete: the parity test
/// makes omissions fail CI.
struct InitializeSetting {
    prompt: &'static str,
    flag: &'static str,
}

const INSTALL_TMUX: InitializeSetting = InitializeSetting {
    prompt: "run the tmux installer",
    flag: "--install-tmux",
};
const TMUX_PROGRAM: InitializeSetting = InitializeSetting {
    prompt: "tmux program path",
    flag: "--tmux-program",
};
const SETUP_MCP: InitializeSetting = InitializeSetting {
    prompt: "set up MCP",
    flag: "--setup-mcp",
};
const MCP_HOST: InitializeSetting = InitializeSetting {
    prompt: "MCP host",
    flag: "--mcp-host",
};
const AGENT_LOGINS: InitializeSetting = InitializeSetting {
    prompt: "agent logins",
    flag: "--agent-logins",
};
const REGISTER_PROJECT: InitializeSetting = InitializeSetting {
    prompt: "register project",
    flag: "--register-project",
};
const PROJECT_NAME: InitializeSetting = InitializeSetting {
    prompt: "project name",
    flag: "--project-name",
};
const PROJECT_DESCRIPTION: InitializeSetting = InitializeSetting {
    prompt: "project description",
    flag: "--project-description",
};
const PROJECT_IS_FORK: InitializeSetting = InitializeSetting {
    prompt: "project is a fork",
    flag: "--project-is-fork",
};
const PROJECT_FORK_URL: InitializeSetting = InitializeSetting {
    prompt: "project fork URL",
    flag: "--project-fork-url",
};
const PROJECT_URL: InitializeSetting = InitializeSetting {
    prompt: "project URL",
    flag: "--project-url",
};
const REVIEW_AUTO_SUBMIT_PR_STACK: InitializeSetting = InitializeSetting {
    prompt: "review auto-submit PR stack",
    flag: "--review-auto-submit-pr-stack",
};
const REVIEW_RESOLVER_AGENT: InitializeSetting = InitializeSetting {
    prompt: "review resolver agent",
    flag: "--review-resolver-agent",
};
const REQUIRE_FORKS: InitializeSetting = InitializeSetting {
    prompt: "require forks",
    flag: "--require-forks",
};
const FORK_USER: InitializeSetting = InitializeSetting {
    prompt: "fork user",
    flag: "--fork-user",
};
const FORK_URL: InitializeSetting = InitializeSetting {
    prompt: "fork URL",
    flag: "--fork-url",
};
const FORGE_HOST: InitializeSetting = InitializeSetting {
    prompt: "forge host",
    flag: "--forge-host",
};
const FORGE_TOKEN: InitializeSetting = InitializeSetting {
    prompt: "forge token",
    flag: "--forge-token",
};
const CREATE_ADMIN: InitializeSetting = InitializeSetting {
    prompt: "create admin",
    flag: "--create-admin",
};
const ADMIN_NAME: InitializeSetting = InitializeSetting {
    prompt: "admin name",
    flag: "--admin-name",
};
const SETUP_FORGE_TOKEN: InitializeSetting = InitializeSetting {
    prompt: "set up forge token",
    flag: "--setup-forge-token",
};
const FORGE_PROVIDER: InitializeSetting = InitializeSetting {
    prompt: "forge provider",
    flag: "--forge-provider",
};
const SUBMIT_SAMPLE: InitializeSetting = InitializeSetting {
    prompt: "submit sample",
    flag: "--submit-sample",
};
const SAMPLE_AGENT: InitializeSetting = InitializeSetting {
    prompt: "sample agent",
    flag: "--sample-agent",
};
const SAMPLE_MODE: InitializeSetting = InitializeSetting {
    prompt: "sample mode",
    flag: "--sample-mode",
};

const INTERACTIVE_SETTINGS: &[&InitializeSetting] = &[
    &INSTALL_TMUX,
    &TMUX_PROGRAM,
    &SETUP_MCP,
    &MCP_HOST,
    &AGENT_LOGINS,
    &REGISTER_PROJECT,
    &PROJECT_NAME,
    &PROJECT_IS_FORK,
    &PROJECT_FORK_URL,
    &PROJECT_URL,
    &PROJECT_DESCRIPTION,
    &REVIEW_AUTO_SUBMIT_PR_STACK,
    &REVIEW_RESOLVER_AGENT,
    &REQUIRE_FORKS,
    &FORK_USER,
    &FORK_URL,
    &CREATE_ADMIN,
    &ADMIN_NAME,
    &SETUP_FORGE_TOKEN,
    &FORGE_PROVIDER,
    &FORGE_HOST,
    &FORGE_TOKEN,
    &SUBMIT_SAMPLE,
    &SAMPLE_MODE,
    &SAMPLE_AGENT,
];

fn interactive_settings_are_valid() -> bool {
    INTERACTIVE_SETTINGS
        .iter()
        .enumerate()
        .all(|(index, setting)| {
            !setting.prompt.is_empty()
                && setting.flag.starts_with("--")
                && INTERACTIVE_SETTINGS[..index]
                    .iter()
                    .all(|earlier| earlier.prompt != setting.prompt && earlier.flag != setting.flag)
        })
}

#[derive(Default)]
pub struct InitializeServerOptions {
    pub yes: bool,
    /// Saved answers to replay (`--answers-file`); flags still win.
    pub answers_file: Option<PathBuf>,
    pub install_tmux: Option<bool>,
    pub tmux_program: Option<String>,
    pub setup_mcp: Option<bool>,
    pub mcp_hosts: Vec<String>,
    /// Backends to log in to (`claude,codex`, `all`, `none`); pre-answers the
    /// agent-logins prompt.
    pub agent_logins: Option<String>,
    pub register_project: Option<bool>,
    pub project_name: Option<String>,
    pub project_is_fork: Option<bool>,
    pub project_fork_url: Option<String>,
    pub project_url: Option<String>,
    pub project_description: Option<String>,
    pub review_auto_submit_pr_stack: Option<bool>,
    pub review_resolver_agent: Option<String>,
    pub require_forks: Option<bool>,
    pub fork_user: Option<String>,
    pub fork_url: Option<String>,
    pub forge_host: Option<String>,
    pub forge_token: Option<String>,
    pub create_admin: Option<bool>,
    pub admin_name: Option<String>,
    pub setup_forge_token: Option<bool>,
    pub forge_provider: Option<String>,
    pub submit_sample: Option<bool>,
    pub sample_mode: Option<String>,
    pub sample_agent: Option<String>,
}

impl std::fmt::Debug for InitializeServerOptions {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("InitializeServerOptions")
            .field("yes", &self.yes)
            .field("answers_file", &self.answers_file)
            .field("install_tmux", &self.install_tmux)
            .field("tmux_program", &self.tmux_program)
            .field("setup_mcp", &self.setup_mcp)
            .field("mcp_hosts", &self.mcp_hosts)
            .field("agent_logins", &self.agent_logins)
            .field("register_project", &self.register_project)
            .field("project_name", &self.project_name)
            .field("project_is_fork", &self.project_is_fork)
            .field("project_fork_url", &self.project_fork_url)
            .field("project_url", &self.project_url)
            .field("project_description", &self.project_description)
            .field(
                "review_auto_submit_pr_stack",
                &self.review_auto_submit_pr_stack,
            )
            .field("review_resolver_agent", &self.review_resolver_agent)
            .field("require_forks", &self.require_forks)
            .field("fork_user", &self.fork_user)
            .field("fork_url", &self.fork_url)
            .field("forge_host", &self.forge_host)
            .field(
                "forge_token",
                &self.forge_token.as_ref().map(|_| "<redacted>"),
            )
            .field("create_admin", &self.create_admin)
            .field("admin_name", &self.admin_name)
            .field("setup_forge_token", &self.setup_forge_token)
            .field("forge_provider", &self.forge_provider)
            .field("submit_sample", &self.submit_sample)
            .field("sample_mode", &self.sample_mode)
            .field("sample_agent", &self.sample_agent)
            .finish()
    }
}

impl InitializeServerOptions {
    fn is_non_interactive(&self) -> bool {
        self.yes
            || self.answers_file.is_some()
            || self.install_tmux.is_some()
            || self.tmux_program.is_some()
            || self.setup_mcp.is_some()
            || !self.mcp_hosts.is_empty()
            || self.agent_logins.is_some()
            || self.register_project.is_some()
            || self.project_name.is_some()
            || self.project_is_fork.is_some()
            || self.project_fork_url.is_some()
            || self.project_url.is_some()
            || self.project_description.is_some()
            || self.review_auto_submit_pr_stack.is_some()
            || self.review_resolver_agent.is_some()
            || self.require_forks.is_some()
            || self.fork_user.is_some()
            || self.fork_url.is_some()
            || self.forge_host.is_some()
            || self.forge_token.is_some()
            || self.create_admin.is_some()
            || self.admin_name.is_some()
            || self.setup_forge_token.is_some()
            || self.forge_provider.is_some()
            || self.submit_sample.is_some()
            || self.sample_mode.is_some()
            || self.sample_agent.is_some()
    }
}

pub fn dispatch(opts: &GlobalOpts, mut setup: InitializeServerOptions) -> i32 {
    debug_assert!(interactive_settings_are_valid());
    answers::reset();
    if let Some(path) = setup.answers_file.clone() {
        if let Err(error) = answers::load_into(&path, &mut setup) {
            println!("error: {error}");
            return 2;
        }
    }
    if !setup.yes && !setup.is_non_interactive() && !std::io::stdin().is_terminal() {
        println!(
            "error: ralphus initialize server needs a terminal to prompt interactively; pass --yes to accept every stage's default non-interactively"
        );
        return 2;
    }

    println!("ralphus interactive server setup");

    let mut step = Step::new(TOTAL_STEPS);

    step.begin("Check tmux/psmux");
    step_tmux(&setup);

    step.begin("Set up MCP hosts (optional)");
    step_mcp(&setup);

    step.begin("Check agent logins (Claude Code, Codex)");
    let logins = step_agent_logins(opts, &setup);

    step.begin("Register this repository as a project (optional)");
    let pending_project = step_project(opts, &setup);

    step.begin("Configure review defaults and auto-review thresholds");
    let review_defaults = match &pending_project {
        Some(_) => step_review_settings(&setup),
        None => {
            println!("  skipped: no project was registered");
            ReviewDefaults::default()
        }
    };

    step.begin("Configure fork requirements");
    let project = match pending_project {
        Some(pending) => {
            let forks = step_forks(&setup);
            finalize_project_registration(opts, pending, review_defaults, forks)
        }
        None => {
            println!("  skipped: no project was registered");
            None
        }
    };

    step.begin("Create an optional default admin user");
    let admin_user = step_admin(opts, &setup);

    step.begin("Configure a forge token for the default admin user");
    step_forge_token(opts, admin_user.as_deref(), &setup);

    step.begin("Run ralphus check health");
    let health_results = step_health(opts);

    step.begin("Submit a sample hello-world task (optional)");
    step_sample(opts, project.as_ref(), &health_results, &logins, &setup);

    let answers_text = answers::render(&mut setup);
    let answers_path = match answers::write(&answers_text) {
        Ok(path) => path,
        Err(error) => {
            println!();
            println!("error: could not save your answers: {error}");
            return 1;
        }
    };
    println!();
    println!(
        "here are your answers (saved to {}):",
        answers_path.display()
    );
    println!("{answers_text}");
    println!(
        "  equivalent command: ralphus initialize server --answers-file {}",
        answers_path.display()
    );
    println!(
        "  note: tmux paths, MCP hosts, and project names/URLs may be specific to this machine; the forge token is not stored"
    );

    println!();
    println!("setup complete.");
    0
}

// ---- step counter --------------------------------------------------------

struct Step {
    total: u32,
    current: u32,
}

impl Step {
    fn new(total: u32) -> Self {
        Self { total, current: 0 }
    }

    fn begin(&mut self, title: &str) {
        self.current += 1;
        println!();
        println!("Step {} of {}: {title}", self.current, self.total);
    }
}

// ---- prompt helpers -------------------------------------------------------

fn prompt(
    setting: &InitializeSetting,
    question: &str,
    default: &str,
    supplied: Option<&String>,
    yes: bool,
) -> String {
    let (value, source) = if let Some(supplied) = supplied {
        (supplied.clone(), answers::supplied_source(setting))
    } else if yes {
        (default.to_string(), Source::Default)
    } else {
        if default.is_empty() {
            print!("{question}: ");
        } else {
            print!("{question} [{default}]: ");
        }
        let _ = std::io::stdout().flush();
        let mut answer = String::new();
        let _ = std::io::stdin().read_line(&mut answer);
        let answer = answer.trim();
        if answer.is_empty() {
            (default.to_string(), Source::Default)
        } else {
            (answer.to_string(), Source::Prompt)
        }
    };
    answers::record_str(setting, &value, source);
    value
}

/// Asks a yes/no question and reports where the answer came from, without
/// recording it (for callers whose answer is not a plain boolean setting).
fn ask_yes_no(question: &str, default: bool, supplied: Option<bool>, yes: bool) -> (bool, Source) {
    if let Some(supplied) = supplied {
        return (supplied, Source::Flag);
    }
    if yes {
        return (default, Source::Default);
    }
    let hint = if default { "Y/n" } else { "y/N" };
    print!("{question} [{hint}] ");
    let _ = std::io::stdout().flush();
    let mut answer = String::new();
    let _ = std::io::stdin().read_line(&mut answer);
    match answer.trim().to_ascii_lowercase().as_str() {
        "y" | "yes" => (true, Source::Prompt),
        "n" | "no" => (false, Source::Prompt),
        _ => (default, Source::Default),
    }
}

fn prompt_yes_no(
    setting: &InitializeSetting,
    question: &str,
    default: bool,
    supplied: Option<bool>,
    yes: bool,
) -> bool {
    let (value, source) = ask_yes_no(question, default, supplied, yes);
    let source = if source == Source::Flag {
        answers::supplied_source(setting)
    } else {
        source
    };
    answers::record_bool(setting, value, source);
    value
}

/// Reads a line for a secret (the forge personal access token) without ever
/// printing it back or handing it to anything that logs. This workspace has
/// no hidden-input crate as a dependency and forbids `unsafe_code`
/// workspace-wide, which rules out a hand-rolled no-echo terminal mode on
/// Windows -- so the terminal echoes the token as it's typed, same as a
/// plain `read_line`. Every caller must be careful never to `println!` the
/// returned value.
fn prompt_secret(_setting: &InitializeSetting, question: &str) -> String {
    print!("{question}: ");
    let _ = std::io::stdout().flush();
    let mut answer = String::new();
    let _ = std::io::stdin().read_line(&mut answer);
    answer.trim().to_string()
}

fn default_user_name() -> String {
    std::env::var("USERNAME")
        .or_else(|_| std::env::var("USER"))
        .unwrap_or_else(|_| "default".to_string())
}

// ---- tmux/psmux -----------------------------------------------------------

fn step_tmux(setup: &InitializeServerOptions) -> bool {
    match ralphus_daemon::tmux::resolve_tmux_program_with_source() {
        Ok((program, source)) => {
            println!("  found a tmux-compatible binary: {program} (source: {source})");
            match tmux_version(&program) {
                Some(version) => {
                    println!("  version: {version}");
                    if cfg!(windows) && !version_at_least(&version, WINDOWS_MINIMUM_TMUX_VERSION) {
                        println!(
                            "  Windows requires psmux {}.{}.{} or later (see docs/dependencies.md); this reports {version}",
                            WINDOWS_MINIMUM_TMUX_VERSION.0,
                            WINDOWS_MINIMUM_TMUX_VERSION.1,
                            WINDOWS_MINIMUM_TMUX_VERSION.2
                        );
                        offer_tmux_alternative(setup)
                    } else {
                        true
                    }
                }
                None => {
                    println!("  warning: could not determine {program}'s version");
                    offer_tmux_alternative(setup)
                }
            }
        }
        Err(error) => {
            println!("  tmux is not available: {error}");
            offer_tmux_alternative(setup)
        }
    }
}

fn tmux_version(program: &str) -> Option<String> {
    let output = std::process::Command::new(program)
        .arg("-V")
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if !text.is_empty() {
        return Some(text);
    }
    let text = String::from_utf8_lossy(&output.stderr).trim().to_string();
    if text.is_empty() { None } else { Some(text) }
}

fn version_at_least(version_text: &str, minimum: (u32, u32, u32)) -> bool {
    let Some(token) = version_text.split_whitespace().last() else {
        return false;
    };
    let mut parts = token.split('.').map(|part| {
        part.chars()
            .take_while(char::is_ascii_digit)
            .collect::<String>()
    });
    let major = parts
        .next()
        .and_then(|p| p.parse::<u32>().ok())
        .unwrap_or(0);
    let minor = parts
        .next()
        .and_then(|p| p.parse::<u32>().ok())
        .unwrap_or(0);
    let patch = parts
        .next()
        .and_then(|p| p.parse::<u32>().ok())
        .unwrap_or(0);
    (major, minor, patch) >= minimum
}

fn offer_tmux_alternative(setup: &InitializeServerOptions) -> bool {
    if setup.yes && setup.install_tmux.is_none() && setup.tmux_program.is_none() {
        println!(
            "  skipping tmux install/alternative prompts (--yes); `ralphus check health` will report this below"
        );
        return false;
    }
    if cfg!(windows) {
        println!("  install/upgrade with: winget upgrade --id marlocarlo.psmux");
        if prompt_yes_no(
            &INSTALL_TMUX,
            "  run that command now?",
            true,
            setup.install_tmux,
            setup.yes,
        ) {
            match std::process::Command::new("winget")
                .args(["upgrade", "--id", "marlocarlo.psmux"])
                .status()
            {
                Ok(status) if status.success() => {
                    println!("  installed/upgraded psmux");
                    return true;
                }
                Ok(status) => println!("  winget exited with {status}"),
                Err(error) => println!("  could not run winget: {error}"),
            }
        }
    } else {
        println!(
            "  install tmux with your platform's package manager (e.g. `brew install tmux`, `apt install tmux`)"
        );
    }
    let alternative = prompt(
        &TMUX_PROGRAM,
        "  path to an existing tmux/psmux binary to use instead (blank to skip)",
        "",
        setup.tmux_program.as_ref(),
        setup.yes,
    );
    if alternative.is_empty() {
        return false;
    }
    match tmux_version(&alternative) {
        Some(version) => {
            println!("  {alternative} reports: {version}");
            println!(
                "  to use it for real runs, set RALPHUS_TMUX_CMD={alternative} -- ralphus has no config-file setting for this, only that environment variable"
            );
            true
        }
        None => {
            println!("  could not run `{alternative} -V`; leaving tmux unresolved");
            false
        }
    }
}

// ---- MCP host setup ---------------------------------------------------------

fn step_mcp(setup: &InitializeServerOptions) {
    let hosts = ["claude", "codex", "pi"];
    let detected: Vec<&str> = hosts
        .into_iter()
        .filter(|host| command_available(host))
        .collect();
    if detected.is_empty() {
        println!("  no supported MCP hosts (claude, codex, pi) were detected on PATH; skipping");
        return;
    }
    println!("  detected MCP hosts: {}", detected.join(", "));
    let setup_mcp = setup
        .setup_mcp
        .or_else(|| (!setup.mcp_hosts.is_empty()).then_some(true));
    if !prompt_yes_no(
        &SETUP_MCP,
        "  set up ralphus MCP for any of these hosts?",
        false,
        setup_mcp,
        setup.yes,
    ) {
        println!("  skipped");
        return;
    }
    answers::begin_list(&MCP_HOST);
    for host in detected {
        let selected = setup_mcp.map(|_| setup.mcp_hosts.iter().any(|candidate| candidate == host));
        let (chosen, source) = ask_yes_no(
            &format!("  set up ralphus MCP for {host}?"),
            true,
            selected,
            setup.yes,
        );
        let source = if source == Source::Flag {
            answers::supplied_source(&MCP_HOST)
        } else {
            source
        };
        answers::record_list_item(&MCP_HOST, host, chosen, source);
        if chosen {
            let code = crate::commands::mcp::initialize(host, None, false, true);
            if code == 0 {
                println!("  {host}: done");
            } else {
                println!("  {host}: MCP setup exited with code {code}");
            }
        }
    }
}

fn command_available(program: &str) -> bool {
    std::process::Command::new(program)
        .arg("--version")
        .output()
        .is_ok_and(|output| output.status.success())
}

// ---- agent logins ---------------------------------------------------------------

/// Where one backend's login stands after the step ran; the sample step uses
/// it to pick (and sanity-check) the sample's agent.
#[derive(Debug, Clone, PartialEq, Eq)]
struct AgentLoginReport {
    backend: &'static str,
    /// Names the backend answers to in `--agent-logins` / `--sample-agent`.
    aliases: Vec<&'static str>,
    /// `None` when the backend is not installed or was not probed.
    state: Option<LoginState>,
}

impl AgentLoginReport {
    fn answers_to(&self, name: &str) -> bool {
        self.backend == name || self.aliases.contains(&name)
    }
}

/// The outcome of probing one backend's login.
#[derive(Debug, Clone, PartialEq, Eq)]
enum ProbeOutcome {
    /// The CLI could not be found; carries the reason.
    NotInstalled(String),
    /// The command cannot be probed or logged into (e.g. a compound command).
    Skipped(String),
    Checked {
        /// The resolved program a login would be spawned with.
        program: String,
        status: LoginStatus,
    },
}

/// Everything the login step does to the outside world, so tests can mock the
/// probes, the spawned login, and the prompts.
trait LoginHost {
    fn probe(&self, probe: &dyn LoginProbe) -> ProbeOutcome;
    /// Runs `<program> <args>` with the terminal attached; returns its exit code.
    fn spawn_login(&self, program: &str, args: &[&str]) -> std::io::Result<i32>;
    /// Whether a login flow can be handed the terminal.
    fn is_interactive(&self) -> bool;
    fn is_ssh_session(&self) -> bool;
    /// Prompts with `question` and returns the trimmed answer (empty on EOF).
    fn read_line(&self, question: &str) -> String;
}

/// The real host: resolves each backend's command in the daemon's order
/// (database override, environment variable, default) and runs it locally.
struct RealLoginHost {
    overrides: Vec<(String, String)>,
}

impl RealLoginHost {
    fn load(opts: &GlobalOpts) -> Self {
        let overrides = opts
            .client()
            .list_agent_backend_commands()
            .ok()
            .and_then(|payload| payload["commands"].as_array().cloned())
            .unwrap_or_default()
            .iter()
            .filter_map(|entry| {
                Some((
                    entry["backend"].as_str()?.to_string(),
                    entry["command"].as_str()?.to_string(),
                ))
            })
            .collect();
        Self { overrides }
    }
}

impl LoginHost for RealLoginHost {
    fn probe(&self, probe: &dyn LoginProbe) -> ProbeOutcome {
        let command = self
            .overrides
            .iter()
            .find(|(backend, _)| backend == probe.backend_name())
            .map(|(_, command)| command.clone())
            .or_else(|| std::env::var(probe.command_env_var()).ok())
            .unwrap_or_else(|| probe.default_program().to_string());
        if ralphus_core::shellcmd::is_compound_command(&command) {
            return ProbeOutcome::Skipped(format!("{command} is a compound command"));
        }
        let path = if std::path::Path::new(&command).exists() {
            Some(command.clone())
        } else {
            ralphus_core::process::which(&command)
        };
        let Some(program) = path else {
            return ProbeOutcome::NotInstalled(format!("{command} not found"));
        };
        let status = match ralphus_runner::login_probe::run_status_command(
            &program,
            probe.status_args(),
            ralphus_runner::version_probe::DEFAULT_VERSION_PROBE_TIMEOUT,
        ) {
            StatusRun::SpawnFailed(error) => LoginStatus {
                state: LoginState::Unknown,
                summary: error,
            },
            StatusRun::TimedOut => LoginStatus {
                state: LoginState::Unknown,
                summary: "status probe timed out".to_string(),
            },
            StatusRun::Finished { exit_ok, output } => {
                let env_present =
                    |name: &str| std::env::var_os(name).is_some_and(|value| !value.is_empty());
                probe.parse(&output, exit_ok, &env_present)
            }
        };
        ProbeOutcome::Checked { program, status }
    }

    fn spawn_login(&self, program: &str, args: &[&str]) -> std::io::Result<i32> {
        let status = std::process::Command::new(program).args(args).status()?;
        Ok(status.code().unwrap_or(-1))
    }

    fn is_interactive(&self) -> bool {
        std::io::stdin().is_terminal() && std::io::stdout().is_terminal()
    }

    fn is_ssh_session(&self) -> bool {
        ["SSH_CONNECTION", "SSH_TTY"]
            .iter()
            .any(|name| std::env::var_os(name).is_some_and(|value| !value.is_empty()))
    }

    fn read_line(&self, question: &str) -> String {
        print!("{question} ");
        let _ = std::io::stdout().flush();
        let mut answer = String::new();
        let _ = std::io::stdin().read_line(&mut answer);
        answer.trim().to_string()
    }
}

fn step_agent_logins(opts: &GlobalOpts, setup: &InitializeServerOptions) -> Vec<AgentLoginReport> {
    run_agent_logins(
        &RealLoginHost::load(opts),
        &login_probes(),
        setup.agent_logins.as_deref(),
        setup.yes,
    )
}

fn probe_names(probe: &dyn LoginProbe) -> Vec<&'static str> {
    vec![probe.backend_name(), probe.default_program()]
}

fn config_dir_text(probe: &dyn LoginProbe) -> String {
    probe
        .config_dir()
        .map_or_else(|| "unknown".to_string(), |dir| dir.display().to_string())
}

/// Probes every backend once, then lets the user pick which logged-out ones to
/// log in to. `selection` is the `--agent-logins` answer; with `--yes` and no
/// answer the step only reports, and it never spawns without a terminal.
fn run_agent_logins(
    host: &dyn LoginHost,
    probes: &[&dyn LoginProbe],
    selection: Option<&str>,
    yes: bool,
) -> Vec<AgentLoginReport> {
    if let Some(selection) = selection {
        answers::record_str(
            &AGENT_LOGINS,
            selection,
            answers::supplied_source(&AGENT_LOGINS),
        );
    }
    let mut reports = Vec::new();
    let mut candidates: Vec<(&dyn LoginProbe, String)> = Vec::new();
    for &probe in probes {
        let name = probe.display_name();
        let state = match host.probe(probe) {
            ProbeOutcome::NotInstalled(reason) => {
                println!("  {name}: not installed ({reason}); skipping");
                None
            }
            ProbeOutcome::Skipped(reason) => {
                println!("  {name}: not probed ({reason})");
                None
            }
            ProbeOutcome::Checked { program, status } => {
                match status.state {
                    LoginState::LoggedIn => {
                        println!("  {name}: OK, logged in ({})", status.summary)
                    }
                    LoginState::LoggedOut => {
                        println!(
                            "  {name}: not logged in (config dir: {}); {name} cells and proofs will fail to authenticate until you log in",
                            config_dir_text(probe)
                        );
                        candidates.push((probe, program));
                    }
                    LoginState::Unknown => {
                        println!("  {name}: login state unknown ({})", status.summary);
                    }
                }
                Some(status.state)
            }
        };
        reports.push(AgentLoginReport {
            backend: probe.backend_name(),
            aliases: probe_names(probe),
            state,
        });
    }
    if candidates.is_empty() {
        return reports;
    }

    let answer = match selection {
        Some(answer) => answer.to_string(),
        None if yes => {
            answers::record_str(&AGENT_LOGINS, "none", Source::Default);
            println!(
                "  skipping logins (--yes); run the commands above yourself, or pass --agent-logins <claude,codex|all|none>"
            );
            for (probe, _) in &candidates {
                println!("    {}: {}", probe.display_name(), probe.login_command());
            }
            return reports;
        }
        None => {
            let names: Vec<&str> = candidates
                .iter()
                .map(|(probe, _)| probe.default_program())
                .collect();
            let typed = host.read_line(&format!(
                "  log in to which? ({}, all, or none) [none]:",
                names.join(", ")
            ));
            let (recorded, source) = match typed.trim() {
                "" => ("none", Source::Default),
                other => (other, Source::Prompt),
            };
            answers::record_str(&AGENT_LOGINS, recorded, source);
            typed
        }
    };
    let chosen = select_logins(&answer, &candidates);
    if chosen.is_empty() {
        println!("  skipped: no agent logins selected");
        return reports;
    }

    for index in chosen {
        let (probe, program) = &candidates[index];
        let outcome = log_in(host, *probe, program, yes);
        if let (Some(state), Some(report)) = (
            outcome,
            reports
                .iter_mut()
                .find(|report| report.backend == probe.backend_name()),
        ) {
            report.state = Some(state);
        }
    }
    reports
}

/// Indices into `candidates` named by a comma-separated `answer` (`all`,
/// `none`, or backend names); unknown names are reported and ignored.
fn select_logins(answer: &str, candidates: &[(&dyn LoginProbe, String)]) -> Vec<usize> {
    let mut chosen = Vec::new();
    for token in answer
        .split(',')
        .map(|token| token.trim().to_ascii_lowercase())
        .filter(|token| !token.is_empty())
    {
        match token.as_str() {
            "none" => return Vec::new(),
            "all" => return (0..candidates.len()).collect(),
            _ => match candidates
                .iter()
                .position(|(probe, _)| probe_names(*probe).contains(&token.as_str()))
            {
                Some(index) => {
                    if !chosen.contains(&index) {
                        chosen.push(index);
                    }
                }
                None => println!("  ignoring \"{token}\": not a logged-out agent backend here"),
            },
        }
    }
    chosen
}

/// Starts (or tells the user how to start) one backend's login, then probes
/// once more. Returns the backend's state afterwards, `None` if nothing ran
/// that could have changed it.
fn log_in(
    host: &dyn LoginHost,
    probe: &dyn LoginProbe,
    program: &str,
    yes: bool,
) -> Option<LoginState> {
    let name = probe.display_name();
    let ssh = host.is_ssh_session();
    let args = probe.login_args(ssh);
    let command = match args {
        Some(args) => format!("{} {}", probe.default_program(), args.join(" ")),
        None => probe.login_command().to_string(),
    };
    println!(
        "  {name}: logging in with `{command}` (config dir: {})",
        config_dir_text(probe)
    );
    let mut ran = false;
    match args {
        Some(args) if host.is_interactive() => match host.spawn_login(program, args) {
            Ok(code) => {
                println!("  `{command}` exited with code {code}");
                ran = true;
            }
            Err(error) => println!("  could not run `{command}`: {error}"),
        },
        _ => {
            if ssh && args.is_none() {
                println!(
                    "  this is an SSH session, so run it where you can finish the browser sign-in"
                );
            }
            println!("  run this yourself: {command}");
        }
    }
    if !ran {
        if yes || !host.is_interactive() {
            return None;
        }
        let answer =
            host.read_line("  press Enter once you have logged in to re-check (or s to skip):");
        if answer.to_ascii_lowercase().starts_with('s') {
            println!("  skipped re-check for {name}");
            return None;
        }
    }
    match host.probe(probe) {
        ProbeOutcome::Checked { status, .. } => {
            match status.state {
                LoginState::LoggedIn => {
                    println!("  {name}: OK, now logged in ({})", status.summary)
                }
                LoginState::LoggedOut => {
                    println!("  {name}: still not logged in; run `{command}` when you are ready");
                }
                LoginState::Unknown => {
                    println!("  {name}: login state unknown ({})", status.summary);
                }
            }
            Some(status.state)
        }
        ProbeOutcome::NotInstalled(reason) | ProbeOutcome::Skipped(reason) => {
            println!("  {name}: could not re-check ({reason})");
            None
        }
    }
}

// ---- project registration --------------------------------------------------

/// Everything collected across "Register this repository as a project",
/// "Configure review defaults and auto-review thresholds", and "Configure
/// fork requirements", before any of it reaches the daemon.
/// [`finalize_project_registration`] is the only place that mutates
/// anything, once all three steps' answers are in hand -- so there is never
/// a point where only part of a project's settings have been committed.
struct PendingProject {
    name: String,
    description: String,
    is_fork: bool,
    fork_url: Option<String>,
    project_url: String,
    cwd: PathBuf,
    target_str: String,
    /// A project is already registered at this exact path -- registration
    /// itself is skipped, but review defaults and fork settings still apply.
    already_registered: bool,
}

/// A project `initialize server` registered (or found already registered) and
/// the git remote of this checkout that points at its non-fork upstream.
struct ProjectSetup {
    name: String,
    upstream_remote: String,
    /// The project was registered as a fork of another project.
    is_fork: bool,
}

fn step_project(opts: &GlobalOpts, setup: &InitializeServerOptions) -> Option<PendingProject> {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let probe = std::process::Command::new("git")
        .args([
            "-C",
            &cwd.to_string_lossy(),
            "rev-parse",
            "--is-inside-work-tree",
        ])
        .output();
    let is_git = matches!(&probe, Ok(out) if out.status.success() && String::from_utf8_lossy(&out.stdout).trim() == "true");
    if !is_git {
        println!(
            "  {} is not inside a git working tree; skipping project registration",
            cwd.display()
        );
        return None;
    }
    if !prompt_yes_no(
        &REGISTER_PROJECT,
        "  register this repository as a project?",
        true,
        setup.register_project,
        setup.yes,
    ) {
        println!("  skipped");
        return None;
    }
    let default_name = cwd
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_else(|| "project".to_string());
    let name = prompt(
        &PROJECT_NAME,
        "  project name",
        &default_name,
        setup.project_name.as_ref(),
        setup.yes,
    );
    let is_fork = prompt_yes_no(
        &PROJECT_IS_FORK,
        "  you are about to register this project: is it a fork of another project?",
        false,
        setup.project_is_fork,
        setup.yes,
    );
    let fork_url = if is_fork {
        let fork_url = prompt(
            &PROJECT_FORK_URL,
            "  fork clone URL (HTTPS required)",
            "",
            setup.project_fork_url.as_ref(),
            setup.yes,
        );
        if let Err(error) = validate_project_fork_url(&fork_url) {
            println!("  invalid fork URL: {error}");
            return None;
        }
        Some(fork_url)
    } else {
        None
    };
    let remotes = git_remotes(&cwd);
    let fork_remote = fork_url
        .as_deref()
        .and_then(|fork| find_remote_for_url(&remotes, fork, &[]))
        .map(str::to_string);
    let default_url = default_upstream_remote(&remotes, fork_url.as_deref())
        .map(|(_, url)| url.clone())
        .unwrap_or_default();
    let project_url = prompt(
        &PROJECT_URL,
        if is_fork {
            "  upstream/origin clone URL for the non-fork project (SSH recommended)"
        } else {
            "  project clone URL (SSH recommended)"
        },
        &default_url,
        setup.project_url.as_ref(),
        setup.yes,
    );
    let exclude: Vec<&str> = fork_remote.as_deref().into_iter().collect();
    let matched_remote = (!project_url.is_empty())
        .then(|| find_remote_for_url(&remotes, &project_url, &exclude))
        .flatten()
        .map(str::to_string);
    let remote_to_add = (!project_url.is_empty() && matched_remote.is_none())
        .then(|| free_remote_name(&remotes, &["origin", "upstream", "ralphus-upstream"]));
    match (&matched_remote, &remote_to_add) {
        (Some(name), _) => println!("  using git remote \"{name}\" for {project_url}"),
        (None, Some(name)) => println!(
            "  no git remote points at {project_url}; will add one named \"{name}\" once you confirm"
        ),
        (None, None) => {}
    }
    let description = prompt(
        &PROJECT_DESCRIPTION,
        "  one-line project description",
        "",
        setup.project_description.as_ref(),
        setup.yes,
    );
    let target =
        ralphus_core::strip_verbatim_prefix(cwd.canonicalize().unwrap_or_else(|_| cwd.clone()));
    let target_str = target.to_string_lossy().to_string();
    let client = opts.client();
    let already_registered = match client.get_project(&name) {
        Ok(existing) if existing["path"].as_str() == Some(&target_str) => true,
        Ok(existing) => {
            println!(
                "  project \"{name}\" is already registered for {}; skipping to avoid changing it",
                existing["path"].as_str().unwrap_or("an unknown path")
            );
            return None;
        }
        Err(error) if error.status_code != Some(404) => {
            println!("  error: could not check existing project registration: {error}");
            return None;
        }
        Err(_) => false,
    };
    Some(PendingProject {
        name,
        description,
        is_fork,
        fork_url,
        project_url,
        cwd,
        target_str,
        already_registered,
    })
}

/// Mirrors the daemon's lazy `ensure_fork_remote` (`project_forks.rs`):
/// creates or repoints the local git remote for a fork right away, so
/// `ralphus check health`'s fork-remote check does not warn about a remote
/// that would otherwise only appear automatically on the first submission
/// through that fork.
fn configure_fork_remote(cwd: &std::path::Path, remote_name: &str, fork_url: &str) {
    let existing = std::process::Command::new("git")
        .args([
            "-C",
            &cwd.to_string_lossy(),
            "config",
            "--get",
            &format!("remote.{remote_name}.url"),
        ])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_string());
    if existing.as_deref() == Some(fork_url) {
        return;
    }
    let subcommand = if existing.is_some() { "set-url" } else { "add" };
    match std::process::Command::new("git")
        .args([
            "-C",
            &cwd.to_string_lossy(),
            "remote",
            subcommand,
            remote_name,
            fork_url,
        ])
        .output()
    {
        Ok(output) if output.status.success() => {
            println!("  git remote \"{remote_name}\" -> {fork_url}");
        }
        Ok(output) => println!(
            "  warning: could not configure git remote \"{remote_name}\": {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ),
        Err(error) => {
            println!("  warning: could not configure git remote \"{remote_name}\": {error}");
        }
    }
}

/// Every configured remote of the repository at `cwd` as `(name, url)`, read
/// from the raw config so a global `url.<x>.insteadOf` rewrite cannot make a
/// remote look different from what was configured.
fn git_remotes(cwd: &std::path::Path) -> Vec<(String, String)> {
    let output = std::process::Command::new("git")
        .args([
            "-C",
            &cwd.to_string_lossy(),
            "config",
            "--get-regexp",
            r"^remote\..*\.url$",
        ])
        .output();
    let Ok(output) = output else {
        return Vec::new();
    };
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| {
            let (key, url) = line.split_once(' ')?;
            let name = key.strip_prefix("remote.")?.strip_suffix(".url")?;
            Some((name.to_string(), url.trim().to_string()))
        })
        .collect()
}

/// The URL `initialize server` offers as the project's upstream: the first
/// remote that is not `fork_url`, preferring `origin`. Empty when none.
fn default_upstream_url(cwd: &std::path::Path, fork_url: Option<&str>) -> String {
    default_upstream_remote(&git_remotes(cwd), fork_url)
        .map(|(_, url)| url.clone())
        .unwrap_or_default()
}

/// The first of `preferred` not already a remote name, else a numbered
/// `ralphus-upstream-N`.
fn free_remote_name(remotes: &[(String, String)], preferred: &[&str]) -> String {
    let taken = |name: &str| remotes.iter().any(|(existing, _)| existing == name);
    preferred
        .iter()
        .find(|name| !taken(name))
        .map(|name| (*name).to_string())
        .unwrap_or_else(|| {
            (2..)
                .map(|n| format!("ralphus-upstream-{n}"))
                .find(|name| !taken(name))
                .expect("an unbounded range yields a free name")
        })
}

fn git_add_remote(cwd: &std::path::Path, name: &str, url: &str) -> Result<(), String> {
    let output = std::process::Command::new("git")
        .args(["-C", &cwd.to_string_lossy(), "remote", "add", name, url])
        .output()
        .map_err(|error| error.to_string())?;
    if output.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&output.stderr).trim().to_string())
    }
}

// ---- review settings / auto-review thresholds ------------------------------

/// Recommended, non-customizable auto-review thresholds applied to every
/// newly registered project (submissions of that triage type queue until
/// this many are pending, then an automatic review fires).
const RECOMMENDED_TRIAGE_THRESHOLDS: [(&str, i64); 4] = [
    ("bug", 3),
    ("feature", 5),
    ("investigation", 3),
    ("unclassified", 5),
];

#[derive(Default)]
struct ReviewDefaults {
    auto_submit_pr_stack: bool,
    resolver_agent: String,
}

fn step_review_settings(setup: &InitializeServerOptions) -> ReviewDefaults {
    println!(
        "  using the recommended auto-review thresholds (bug=3, feature=5, investigation=3, unclassified=5)"
    );
    let auto_submit_pr_stack = prompt_yes_no(
        &REVIEW_AUTO_SUBMIT_PR_STACK,
        "  automatically submit this project's review PR stacks?",
        true,
        setup.review_auto_submit_pr_stack,
        setup.yes,
    );
    let resolver_agent = prompt(
        &REVIEW_RESOLVER_AGENT,
        "  resolver agent for this project's reviews",
        "claude-code",
        setup.review_resolver_agent.as_ref(),
        setup.yes,
    );
    ReviewDefaults {
        auto_submit_pr_stack,
        resolver_agent,
    }
}

// ---- forks ------------------------------------------------------------------

/// A contributor fork collected in "Configure fork requirements", applied
/// only once [`finalize_project_registration`] runs.
struct ForkSettings {
    require_forks: bool,
    contributor: Option<(String, String)>,
}

fn step_forks(setup: &InitializeServerOptions) -> ForkSettings {
    let require_forks = prompt_yes_no(
        &REQUIRE_FORKS,
        "  does this project require contributors to work from forks?",
        false,
        setup.require_forks,
        setup.yes,
    );
    if !require_forks {
        println!("  skipped: forks not required");
        return ForkSettings {
            require_forks: false,
            contributor: None,
        };
    }
    let user = prompt(
        &FORK_USER,
        "  ralphus user these fork credentials belong to",
        &default_user_name(),
        setup.fork_user.as_ref(),
        setup.yes,
    );
    let fork_url = prompt(
        &FORK_URL,
        "  your fork's clone URL",
        "",
        setup.fork_url.as_ref(),
        setup.yes,
    );
    if fork_url.is_empty() {
        println!(
            "  no fork URL given; skipping fork registration (use `ralphus project fork add` later)"
        );
        return ForkSettings {
            require_forks: true,
            contributor: None,
        };
    }
    ForkSettings {
        require_forks: true,
        contributor: Some((user, fork_url)),
    }
}

// ---- finalize: the only place project registration mutates anything -------

fn finalize_project_registration(
    opts: &GlobalOpts,
    pending: PendingProject,
    review: ReviewDefaults,
    forks: ForkSettings,
) -> Option<ProjectSetup> {
    let PendingProject {
        name,
        description,
        is_fork,
        fork_url,
        project_url,
        cwd,
        target_str,
        already_registered,
    } = pending;
    let client = opts.client();

    let remotes = git_remotes(&cwd);
    let fork_remote = fork_url
        .as_deref()
        .and_then(|fork| find_remote_for_url(&remotes, fork, &[]))
        .map(str::to_string);
    let exclude: Vec<&str> = fork_remote.as_deref().into_iter().collect();
    let matched_remote = (!project_url.is_empty())
        .then(|| find_remote_for_url(&remotes, &project_url, &exclude))
        .flatten()
        .map(str::to_string);
    let remote_to_add = (!project_url.is_empty() && matched_remote.is_none())
        .then(|| free_remote_name(&remotes, &["origin", "upstream", "ralphus-upstream"]));
    let upstream_remote = match (&matched_remote, &remote_to_add) {
        (Some(name), _) => name.clone(),
        (None, Some(name)) => name.clone(),
        (None, None) => "origin".to_string(),
    };

    if already_registered {
        println!(
            "  project \"{name}\" is already registered at {target_str}; skipping registration"
        );
    } else {
        let clone_url = (!project_url.is_empty()).then_some(project_url.as_str());
        match client.register_project(
            &name,
            &target_str,
            &description,
            "git",
            clone_url,
            false,
            None,
        ) {
            Ok(payload) => {
                println!("  registered project \"{name}\" -> {target_str}");
                for warning in payload["warnings"].as_array().into_iter().flatten() {
                    if let Some(warning) = warning.as_str() {
                        println!("  warning: {warning}");
                    }
                }
            }
            Err(error) => {
                println!("  error: could not register project: {error}");
                return None;
            }
        }

        if let Some(ref fork_url_val) = fork_url {
            match client.add_project_fork(&name, "", fork_url_val, fork_remote.as_deref(), None) {
                Ok(_) => {
                    println!("  registered the project-wide fork URL");
                    if let Some(ref remote_name) = fork_remote {
                        configure_fork_remote(&cwd, remote_name, fork_url_val);
                    }
                }
                Err(error) => println!("  error registering the project fork URL: {error}"),
            }
        }

        if let Some(remote_name) = &remote_to_add {
            match git_add_remote(&cwd, remote_name, &project_url) {
                Ok(()) => {
                    println!("  added git remote \"{remote_name}\" -> {project_url}");
                }
                Err(error) => {
                    println!("  error: could not add git remote \"{remote_name}\": {error}");
                }
            }
        }
    }

    let pools = client.list_triage_pools().unwrap_or_default();
    for (triage_type, threshold) in RECOMMENDED_TRIAGE_THRESHOLDS {
        let already_configured = pools["pools"].as_array().is_some_and(|pools| {
            pools.iter().any(|pool| {
                pool["project"].as_str() == Some(name.as_str())
                    && pool["triage_type"].as_str() == Some(triage_type)
                    && pool["threshold"].as_i64() == Some(threshold)
            })
        });
        if already_configured {
            println!("  {triage_type} threshold: already configured; skipping");
            continue;
        }
        match client.set_triage_pool_threshold(&name, triage_type, Some(threshold)) {
            Ok(_) => println!("  {triage_type} threshold: {threshold}"),
            Err(error) => println!("  {triage_type} threshold: error setting threshold: {error}"),
        }
    }

    let dual_root_pr = is_fork || forks.require_forks;
    let patch = ProjectReviewSettingsPatch {
        auto_submit_pr_stack: Some(review.auto_submit_pr_stack),
        default_resolver_agent: Some(&review.resolver_agent),
        dual_root_pr: dual_root_pr.then_some(true),
        auto_fix_pr_errors: Some(true),
        discourage_tests_during_auto_pull_request_fixes: Some(true),
        rebuild_on: Some(Some(vec!["feedback".to_string(), "auto_fix".to_string()])),
        ..Default::default()
    };
    match client.set_project_review_settings(&name, &patch) {
        Ok(_) => println!(
            "  review defaults: auto-submit PR stacks = {}; resolver agent = {}; dual-root PRs = {dual_root_pr}; auto-fix PR errors = on; discourage tests during auto-fix = on; rebuild on feedback/auto-fix = on",
            review.auto_submit_pr_stack, review.resolver_agent
        ),
        Err(error) => println!("  error setting review defaults: {error}"),
    }

    if let Some((user, fork_url)) = forks.contributor {
        let contributor_fork_remote =
            find_remote_for_url(&remotes, &fork_url, &[]).map(str::to_string);
        let contributor_remote_to_add = contributor_fork_remote
            .is_none()
            .then(|| free_remote_name(&remotes, &[&format!("fork-{user}"), "fork"]));
        let contributor_remote = match (&contributor_fork_remote, &contributor_remote_to_add) {
            (Some(name), _) => Some(name.clone()),
            (None, Some(name)) => match git_add_remote(&cwd, name, &fork_url) {
                Ok(()) => {
                    println!("  added git remote \"{name}\" -> {fork_url}");
                    Some(name.clone())
                }
                Err(error) => {
                    println!("  error: could not add git remote for {user} fork: {error}");
                    None
                }
            },
            (None, None) => None,
        };

        let existing_fork = client.list_project_forks(&name).ok().and_then(|payload| {
            payload["forks"]
                .as_array()
                .and_then(|forks| {
                    forks
                        .iter()
                        .find(|fork| fork["user"].as_str() == Some(&user))
                })
                .cloned()
        });
        match existing_fork {
            Some(fork) if fork["fork_url"].as_str() == Some(fork_url.as_str()) => {
                println!("  fork for {user} is already registered; skipping");
                if let Some(ref remote_name) = contributor_remote {
                    configure_fork_remote(&cwd, remote_name, &fork_url);
                }
            }
            Some(_) => match client.set_project_fork(
                &name,
                &user,
                Some(&fork_url),
                contributor_remote.as_deref(),
                None,
            ) {
                Ok(_) => {
                    println!("  updated the registered fork for {user}");
                    if let Some(ref remote_name) = contributor_remote {
                        configure_fork_remote(&cwd, remote_name, &fork_url);
                    }
                }
                Err(error) => println!("  error updating fork registration: {error}"),
            },
            None => match client.add_project_fork(
                &name,
                &user,
                &fork_url,
                contributor_remote.as_deref(),
                None,
            ) {
                Ok(_) => {
                    println!("  registered a fork for {user}");
                    if let Some(ref remote_name) = contributor_remote {
                        configure_fork_remote(&cwd, remote_name, &fork_url);
                    }
                }
                Err(error) => println!("  error registering fork: {error}"),
            },
        }
    }

    Some(ProjectSetup {
        name,
        upstream_remote,
        is_fork,
    })
}

// ---- default admin user ------------------------------------------------------

fn step_admin(opts: &GlobalOpts, setup: &InitializeServerOptions) -> Option<String> {
    if !prompt_yes_no(
        &CREATE_ADMIN,
        "  create a default admin user?",
        false,
        setup.create_admin,
        setup.yes,
    ) {
        println!("  skipped");
        return None;
    }
    let name = prompt(
        &ADMIN_NAME,
        "  admin user name",
        "John Smith",
        setup.admin_name.as_ref(),
        setup.yes,
    );
    let client = opts.client();
    let existing_admin = client.list_users().ok().and_then(|payload| {
        payload["users"]
            .as_array()
            .and_then(|users| {
                users
                    .iter()
                    .find(|user| user["name"].as_str() == Some(&name))
            })
            .and_then(|user| user["is_admin"].as_bool())
    });
    if existing_admin == Some(true) {
        println!("  {name} is already an admin; skipping server update");
    } else if existing_admin.is_none() {
        if let Err(error) = client.create_user(&name) {
            println!("  error: could not create user {name}: {error}");
            return None;
        }
    }
    if existing_admin != Some(true) {
        match client.set_user_admin(&name, true) {
            Ok(_) => println!("  {name} is now an admin"),
            Err(error) => {
                println!("  error: could not grant admin to {name}: {error}");
                return None;
            }
        }
    }
    match persist_default_admin(&name) {
        Ok((path, true)) => println!(
            "  wrote default_user/default_user_is_admin to {} (applies on the daemon's next restart)",
            path.display()
        ),
        Ok((path, false)) => println!(
            "  default admin is already persisted in {}; skipping",
            path.display()
        ),
        Err(error) => {
            println!("  warning: could not persist the default admin to the global config: {error}")
        }
    }
    Some(name)
}

// ---- forge token ------------------------------------------------------------

fn step_forge_token(opts: &GlobalOpts, user: Option<&str>, setup: &InitializeServerOptions) {
    let Some(user) = user else {
        println!("  skipped: no default admin user was created");
        return;
    };
    let supplied_token_requests_setup = setup.forge_token.as_ref().map(|_| true);
    if !prompt_yes_no(
        &SETUP_FORGE_TOKEN,
        &format!("  configure a GitHub or GitLab personal access token for {user}?"),
        true,
        setup.setup_forge_token.or(supplied_token_requests_setup),
        setup.yes,
    ) {
        println!("  skipped");
        return;
    }
    let provider = prompt(
        &FORGE_PROVIDER,
        "  forge provider (github or gitlab)",
        "github",
        setup.forge_provider.as_ref(),
        setup.yes,
    );
    let default_host = match provider.as_str() {
        "github" => "github.com",
        "gitlab" => "gitlab.com",
        _ => {
            println!("  invalid forge provider {provider:?}; expected github or gitlab");
            return;
        }
    };
    let host = prompt(
        &FORGE_HOST,
        "  forge host for the personal access token",
        default_host,
        setup.forge_host.as_ref(),
        setup.yes,
    );
    if let Err(error) = validate_forge_host(&host) {
        println!("  invalid forge host {host:?}: {error}");
        return;
    }
    let (token, token_source) = match (&setup.forge_token, answers::forge_token_from_env()) {
        (Some(token), _) => (token.clone(), answers::supplied_source(&FORGE_TOKEN)),
        (None, Some(token)) => (token, Source::Env),
        (None, None) => (
            prompt_secret(
                &FORGE_TOKEN,
                "  personal access token (never echoed back or logged by ralphus)",
            ),
            Source::Prompt,
        ),
    };
    if !token.is_empty() {
        answers::record_forge_token(&FORGE_TOKEN, token_source);
    }
    if token.is_empty() {
        println!("  no token given; skipping (use `ralphus user set-forge-token` later)");
        return;
    }
    let client = opts.client();
    let token_already_configured = client
        .list_user_forge_tokens(user)
        .ok()
        .and_then(|payload| payload["tokens"].as_array().cloned())
        .is_some_and(|tokens| {
            tokens
                .iter()
                .any(|entry| entry["host"].as_str() == Some(&host))
        });
    if token_already_configured {
        println!("  a forge token is already configured for {user}@{host}; skipping");
        return;
    }
    match client.set_user_forge_token(user, &host, &token) {
        Ok(_) => println!("  stored a {provider} forge token for {user}@{host}"),
        Err(error) => println!("  error storing forge token: {error}"),
    }
}

fn persist_default_admin(name: &str) -> Result<(PathBuf, bool), String> {
    let path = ralphus_daemon::config::global_config_path()
        .ok_or_else(|| "could not resolve a home directory for the global config".to_string())?;
    let mut root: toml::Table = if path.exists() {
        let text = std::fs::read_to_string(&path).map_err(|error| error.to_string())?;
        text.parse::<toml::Table>()
            .map_err(|error| error.to_string())?
    } else {
        toml::Table::new()
    };
    if root
        .get("daemon")
        .and_then(toml::Value::as_table)
        .is_some_and(|daemon| {
            daemon.get("default_user").and_then(toml::Value::as_str) == Some(name)
                && daemon
                    .get("default_user_is_admin")
                    .and_then(toml::Value::as_bool)
                    == Some(true)
        })
    {
        return Ok((path, false));
    }
    let daemon_entry = root
        .entry("daemon")
        .or_insert_with(|| toml::Value::Table(toml::Table::new()));
    let toml::Value::Table(daemon_table) = daemon_entry else {
        return Err("[daemon] is not a table in the existing global config".to_string());
    };
    daemon_table.insert(
        "default_user".to_string(),
        toml::Value::String(name.to_string()),
    );
    daemon_table.insert(
        "default_user_is_admin".to_string(),
        toml::Value::Boolean(true),
    );
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    }
    let text = toml::to_string_pretty(&root).map_err(|error| error.to_string())?;
    std::fs::write(&path, text).map_err(|error| error.to_string())?;
    Ok((path, true))
}

// ---- health -------------------------------------------------------------------

fn step_health(opts: &GlobalOpts) -> Vec<CheckResult> {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    println!("  a passing `ralphus check health` is required before ralphus can run real work.");
    let results = crate::health::run_checks(&opts.daemon_url, &cwd, false, false, false);
    let _ = crate::commands::misc::report_health(opts, &cwd, &results);
    results
}

fn health_ok_for_sample(results: &[CheckResult], requires_agent: bool) -> bool {
    let daemon_ok = results
        .iter()
        .any(|r| r.id == ralphus_core::health_catalog::ID_DAEMON && !r.is_fail());
    let agent_ok = [
        ralphus_core::health_catalog::ID_CLAUDE_COMMAND,
        ralphus_core::health_catalog::ID_CODEX_COMMAND,
        ralphus_core::health_catalog::ID_PI_COMMAND,
    ]
    .iter()
    .any(|id| results.iter().any(|r| r.id.as_str() == *id && !r.is_fail()));
    daemon_ok && (!requires_agent || agent_ok)
}

// ---- sample submission ----------------------------------------------------

/// The default branch of `remote`, read from its `HEAD` symref over the
/// network. `None` when the remote is unreachable or reports no symref.
fn remote_default_branch(cwd: &std::path::Path, remote: &str) -> Option<String> {
    let output = std::process::Command::new("git")
        .args([
            "-C",
            &cwd.to_string_lossy(),
            "ls-remote",
            "--symref",
            remote,
            "HEAD",
        ])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    parse_symref_head(&String::from_utf8_lossy(&output.stdout))
}

/// The branch named by a `ref: refs/heads/<branch>\tHEAD` line of
/// `git ls-remote --symref` output.
fn parse_symref_head(output: &str) -> Option<String> {
    output.lines().find_map(|line| {
        let rest = line.strip_prefix("ref: refs/heads/")?;
        let (branch, target) = rest.split_once('\t')?;
        (target.trim() == "HEAD" && !branch.is_empty()).then(|| branch.to_string())
    })
}

/// The `<remote>/<branch>` upstream for a forked project, or `None` when the
/// parent's default branch cannot be found (the daemon's `<<default>>`
/// resolution applies then).
fn fork_upstream(cwd: &std::path::Path, remote: &str) -> Option<String> {
    remote_default_branch(cwd, remote).map(|branch| format!("{remote}/{branch}"))
}

fn sample_task_toml(
    project: &str,
    remote: &str,
    mode: &str,
    agent: Option<&str>,
    explicit_upstream: Option<&str>,
) -> String {
    let review_id = "ralphus:new-review/ralphus-hello-world";
    let cell_upstream = explicit_upstream.unwrap_or("<<default>>");
    let mut toml = format!(
        "[[review]]\nid = \"{review_id}\"\nname = \"Hello world for {project}\"\nproof_scope = \"nothing\"\n"
    );
    if let Some(upstream) = explicit_upstream {
        toml.push_str(&format!("upstream = \"{upstream}\"\n"));
    }
    toml.push('\n');
    if let Some(agent) = agent {
        toml.push_str(&format!("agent = \"{agent}\"\n\n"));
    }

    for suffix in ["alpha", "beta", "gamma"] {
        let branch = format!("ralphus-hello-world-{suffix}");
        let file = format!("hello-world-{suffix}.txt");
        toml.push_str(&format!(
            "[[task]]\nname = \"hello-world-{suffix}\"\nproject = \"{project}\"\n\n  [[task.cell]]\n  id = \"write\"\n  cwd = \"<<ralphus:new-worktree/{branch}?upstream={cell_upstream}>>\"\n  review = \"<<{review_id}>>\"\n"
        ));
        match mode {
            "raw" => toml.push_str(&format!(
                "  mode = \"raw\"\n  command = \"echo Hello from Ralphus task {suffix}.> {file} && git add -- {file} && git commit --message \\\"Add {file}\\\" && git push --set-upstream {remote} {branch}\"\n\n"
            )),
            "agent" => toml.push_str(&format!(
                "  agent = \"{}\"\n  system_prompt = \"Work only in the dedicated git worktree. Create exactly the requested file, stage only that file, commit it, and push the branch.\"\n  system_prompt_position = \"append\"\n  prompt = \"Create {file} containing the single line \\\"Hello from Ralphus task {suffix}.\\\". Do not modify any other files. Then stage, commit, and push that file.\"\n\n",
                agent.expect("agent mode supplies an agent")
            )),
            _ => unreachable!("sample mode is parsed before TOML generation"),
        }
    }
    toml
}

fn step_sample(
    opts: &GlobalOpts,
    project: Option<&ProjectSetup>,
    health_results: &[CheckResult],
    logins: &[AgentLoginReport],
    setup: &InitializeServerOptions,
) {
    let Some(ProjectSetup {
        name: project,
        upstream_remote,
        is_fork,
    }) = project
    else {
        println!("  skipped: no project was registered");
        return;
    };
    if !prompt_yes_no(
        &SUBMIT_SAMPLE,
        &format!("  submit a three-task hello-world squad for {project} now?"),
        true,
        setup.submit_sample,
        setup.yes,
    ) {
        println!("  skipped");
        return;
    }
    let mode = sample_mode(setup);
    if !health_ok_for_sample(health_results, mode == "agent") {
        let requirement = if mode == "agent" {
            "a failing daemon or no reachable agent"
        } else {
            "a failing daemon"
        };
        println!(
            "  skipped: `ralphus check health` reported {requirement} -- fix that first, then submit a sample task yourself"
        );
        return;
    }
    let agent = (mode == "agent").then(|| {
        let agent = prompt(
            &SAMPLE_AGENT,
            "  agent backend for the hello-world prompts",
            default_sample_agent(logins),
            setup.sample_agent.as_ref(),
            setup.yes,
        );
        if logged_out(logins, &agent) {
            println!(
                "  warning: {agent} is not logged in, so the sample cells will likely fail to authenticate"
            );
        }
        agent
    });
    let explicit_upstream = if *is_fork {
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let found = fork_upstream(&cwd, upstream_remote);
        match &found {
            Some(upstream) => println!("  fork project: basing the sample on {upstream}"),
            None => println!(
                "  fork project: could not read {upstream_remote}'s default branch; using the default upstream"
            ),
        }
        found
    } else {
        None
    };
    let toml_text = sample_task_toml(
        project,
        upstream_remote,
        &mode,
        agent.as_deref(),
        explicit_upstream.as_deref(),
    );
    let client = opts.client();
    let label = sample_label(&mode);
    let sample_already_submitted = client
        .tasks(None, Some(&label), None)
        .ok()
        .and_then(|payload| payload["squads"].as_array().cloned())
        .is_some_and(|squads| !squads.is_empty());
    if sample_already_submitted {
        println!("  sample hello-world task was already submitted; skipping");
        return;
    }
    let path =
        std::env::temp_dir().join(format!("ralphus-hello-world-{}.toml", std::process::id()));
    if let Err(error) = std::fs::write(&path, &toml_text) {
        println!("  error: could not write {}: {error}", path.display());
        return;
    }
    match client.submit(&toml_text, false, Some(&label)) {
        Ok(payload) => println!("  submitted: {payload}"),
        Err(error) => println!("  error: submission failed: {error}"),
    }
    println!("  saved the submitted TOML to {}", path.display());
    println!("  equivalent command: ralphus submit {}", path.display());
}

/// The first backend confirmed logged in, else `claude-code`.
fn default_sample_agent(logins: &[AgentLoginReport]) -> &'static str {
    logins
        .iter()
        .find(|report| report.state == Some(LoginState::LoggedIn))
        .map_or("claude-code", |report| report.backend)
}

fn logged_out(logins: &[AgentLoginReport], agent: &str) -> bool {
    logins
        .iter()
        .any(|report| report.answers_to(agent) && report.state == Some(LoginState::LoggedOut))
}

fn sample_label(mode: &str) -> String {
    format!("{SAMPLE_LABEL_PREFIX} ({mode})")
}

fn sample_mode(setup: &InitializeServerOptions) -> String {
    loop {
        let mode = prompt(
            &SAMPLE_MODE,
            "  run the hello-world tasks with an agent or raw commands",
            "agent",
            setup.sample_mode.as_ref(),
            setup.yes,
        );
        if matches!(mode.as_str(), "agent" | "raw") {
            return mode;
        }
        println!("  please enter `agent` or `raw`");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interactive_settings_have_unique_prompt_and_flag_contracts() {
        assert!(interactive_settings_are_valid());
        assert_eq!(INTERACTIVE_SETTINGS.len(), 25);
    }

    #[test]
    fn sample_toml_uses_three_parallel_tasks_and_one_explicit_review() {
        let toml = sample_task_toml("example", "origin", "agent", Some("claude-code"), None);
        assert_eq!(toml.matches("?upstream=<<default>>>>").count(), 3);
        assert!(!toml.contains("\nupstream = "));
        assert_eq!(toml.matches("[[task]]").count(), 3);
        assert_eq!(toml.matches("review = \"<<ralphus:new-review/").count(), 3);
        assert!(!toml.contains("depends_on"));
        assert!(
            ralphus_core::validate::validate_toml(&toml).is_ok(),
            "{toml}"
        );
    }

    #[test]
    fn upstream_is_found_by_url_under_a_non_origin_remote_name() {
        let dir = std::env::temp_dir().join(format!("ral583-remotes-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let git = |args: &[&str]| {
            let status = std::process::Command::new("git")
                .arg("-C")
                .arg(&dir)
                .args(args)
                .status()
                .unwrap();
            assert!(status.success(), "git {args:?}");
        };
        git(&["init", "-q"]);
        git(&["remote", "add", "origin", "git@github.com:me/app.git"]);
        git(&["remote", "add", "main-repo", "git@github.com:acme/app.git"]);
        let remotes = git_remotes(&dir);
        assert_eq!(remotes.len(), 2);
        assert_eq!(
            find_remote_for_url(&remotes, "https://github.com/acme/app", &[]),
            Some("main-repo")
        );
        assert_eq!(
            default_upstream_url(&dir, Some("https://github.com/me/app.git")),
            "git@github.com:acme/app.git"
        );
        assert_eq!(
            free_remote_name(&remotes, &["origin", "upstream"]),
            "upstream"
        );
        git_add_remote(&dir, "upstream", "git@github.com:x/y.git").unwrap();
        assert_eq!(git_remotes(&dir).len(), 3);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn fork_sample_toml_pins_cells_and_review_to_the_upstream_remote() {
        let toml = sample_task_toml("example", "main-repo", "raw", None, Some("main-repo/trunk"));
        assert_eq!(toml.matches("?upstream=main-repo/trunk>>").count(), 3);
        assert!(toml.contains("\nupstream = \"main-repo/trunk\"\n"));
        assert!(!toml.contains("<<default>>"));
        assert!(
            ralphus_core::validate::validate_toml(&toml).is_ok(),
            "{toml}"
        );
    }

    #[test]
    fn symref_head_is_parsed_from_ls_remote_output() {
        let out = "ref: refs/heads/trunk\tHEAD\n0123abc\tHEAD\n";
        assert_eq!(parse_symref_head(out).as_deref(), Some("trunk"));
        assert_eq!(parse_symref_head("0123abc\tHEAD\n"), None);
    }

    #[test]
    fn fork_upstream_reads_the_default_branch_of_the_matched_remote() {
        let base = std::env::temp_dir().join(format!("ral584-fork-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let parent = base.join("parent.git");
        let work = base.join("work");
        std::fs::create_dir_all(&work).unwrap();
        let git = |dir: &std::path::Path, args: &[&str]| {
            let status = std::process::Command::new("git")
                .arg("-C")
                .arg(dir)
                .args(args)
                .status()
                .unwrap();
            assert!(status.success(), "git {args:?}");
        };
        std::fs::create_dir_all(&parent).unwrap();
        git(&parent, &["init", "-q", "--bare", "--initial-branch=trunk"]);
        git(&work, &["init", "-q"]);
        git(
            &work,
            &["remote", "add", "main-repo", &parent.to_string_lossy()],
        );
        git(
            &work,
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@example.com",
                "commit",
                "-q",
                "--allow-empty",
                "-m",
                "init",
            ],
        );
        git(&work, &["push", "-q", "main-repo", "HEAD:refs/heads/trunk"]);
        assert_eq!(
            fork_upstream(&work, "main-repo").as_deref(),
            Some("main-repo/trunk")
        );
        assert_eq!(fork_upstream(&work, "no-such-remote"), None);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn raw_sample_toml_has_no_agent_cells() {
        let toml = sample_task_toml("example", "upstream", "raw", None, None);
        assert_eq!(toml.matches("mode = \"raw\"").count(), 3);
        assert_eq!(toml.matches("git push --set-upstream upstream ").count(), 3);
        assert!(!toml.contains("  agent ="));
        assert!(
            ralphus_core::validate::validate_toml(&toml).is_ok(),
            "{toml}"
        );
    }

    use std::cell::RefCell;
    use std::collections::HashMap;

    /// Scripted [`LoginHost`]: each backend's probes pop from a queue (the
    /// last entry repeats); spawns and prompts are recorded.
    struct MockHost {
        probes: RefCell<HashMap<&'static str, Vec<ProbeOutcome>>>,
        interactive: bool,
        ssh: bool,
        answers: RefCell<Vec<String>>,
        questions: RefCell<Vec<String>>,
        spawns: RefCell<Vec<(String, Vec<String>)>>,
        probe_calls: RefCell<Vec<&'static str>>,
        /// Logs the backend in as a side effect of a spawn.
        spawn_fixes: bool,
    }

    fn checked(state: LoginState) -> ProbeOutcome {
        ProbeOutcome::Checked {
            program: "prog".to_string(),
            status: LoginStatus {
                state,
                summary: "summary".to_string(),
            },
        }
    }

    fn host(claude: Vec<ProbeOutcome>, codex: Vec<ProbeOutcome>) -> MockHost {
        MockHost {
            probes: RefCell::new(HashMap::from([("claude-code", claude), ("codex", codex)])),
            interactive: true,
            ssh: false,
            answers: RefCell::new(Vec::new()),
            questions: RefCell::new(Vec::new()),
            spawns: RefCell::new(Vec::new()),
            probe_calls: RefCell::new(Vec::new()),
            spawn_fixes: false,
        }
    }

    impl LoginHost for MockHost {
        fn probe(&self, probe: &dyn LoginProbe) -> ProbeOutcome {
            self.probe_calls.borrow_mut().push(probe.backend_name());
            let mut probes = self.probes.borrow_mut();
            let queue = probes.get_mut(probe.backend_name()).expect("scripted");
            if queue.len() > 1 {
                queue.remove(0)
            } else {
                queue[0].clone()
            }
        }
        fn spawn_login(&self, program: &str, args: &[&str]) -> std::io::Result<i32> {
            self.spawns.borrow_mut().push((
                program.to_string(),
                args.iter().map(ToString::to_string).collect(),
            ));
            if self.spawn_fixes {
                for queue in self.probes.borrow_mut().values_mut() {
                    if queue.first() == Some(&checked(LoginState::LoggedOut)) {
                        *queue = vec![checked(LoginState::LoggedIn)];
                    }
                }
            }
            Ok(0)
        }
        fn is_interactive(&self) -> bool {
            self.interactive
        }
        fn is_ssh_session(&self) -> bool {
            self.ssh
        }
        fn read_line(&self, question: &str) -> String {
            self.questions.borrow_mut().push(question.to_string());
            let mut answers = self.answers.borrow_mut();
            if answers.is_empty() {
                String::new()
            } else {
                answers.remove(0)
            }
        }
    }

    fn run(host: &MockHost, selection: Option<&str>, yes: bool) -> Vec<AgentLoginReport> {
        run_agent_logins(host, &login_probes(), selection, yes)
    }

    fn states(reports: &[AgentLoginReport]) -> Vec<Option<LoginState>> {
        reports.iter().map(|report| report.state).collect()
    }

    #[test]
    fn logged_in_backends_report_ok_without_prompting() {
        let host = host(
            vec![checked(LoginState::LoggedIn)],
            vec![checked(LoginState::LoggedIn)],
        );
        let reports = run(&host, None, false);
        assert!(host.questions.borrow().is_empty());
        assert!(host.spawns.borrow().is_empty());
        assert_eq!(
            states(&reports),
            [Some(LoginState::LoggedIn), Some(LoginState::LoggedIn)]
        );
    }

    #[test]
    fn uninstalled_backends_are_skipped_without_prompting() {
        let missing = ProbeOutcome::NotInstalled("nope not found".to_string());
        let host = host(vec![missing.clone()], vec![missing]);
        let reports = run(&host, None, false);
        assert!(host.questions.borrow().is_empty());
        assert_eq!(states(&reports), [None, None]);
    }

    #[test]
    fn logged_out_then_fixed_by_spawning_each_backends_login() {
        for (answer, backend, args) in [
            ("claude", "claude-code", vec!["auth", "login"]),
            ("codex", "codex", vec!["login"]),
        ] {
            let mut host = host(
                vec![checked(LoginState::LoggedOut)],
                vec![checked(LoginState::LoggedOut)],
            );
            host.spawn_fixes = true;
            let reports = run(&host, Some(answer), false);
            assert_eq!(
                *host.spawns.borrow(),
                [(
                    "prog".to_string(),
                    args.iter().map(ToString::to_string).collect::<Vec<_>>()
                )]
            );
            let report = reports.iter().find(|r| r.backend == backend).unwrap();
            assert_eq!(report.state, Some(LoginState::LoggedIn), "{backend}");
            // The other backend was skipped and stays logged out.
            let other = reports.iter().find(|r| r.backend != backend).unwrap();
            assert_eq!(other.state, Some(LoginState::LoggedOut));
        }
    }

    #[test]
    fn both_logged_out_are_chosen_from_one_prompt_and_both_fixed() {
        let mut host = host(
            vec![checked(LoginState::LoggedOut)],
            vec![checked(LoginState::LoggedOut)],
        );
        host.spawn_fixes = true;
        host.answers.borrow_mut().push("claude, codex".to_string());
        let reports = run(&host, None, false);
        assert_eq!(host.questions.borrow().len(), 1);
        assert_eq!(host.spawns.borrow().len(), 2);
        assert_eq!(
            states(&reports),
            [Some(LoginState::LoggedIn), Some(LoginState::LoggedIn)]
        );
    }

    #[test]
    fn all_selects_every_logged_out_backend() {
        let host = host(
            vec![checked(LoginState::LoggedOut)],
            vec![checked(LoginState::LoggedOut)],
        );
        host.answers.borrow_mut().push("all".to_string());
        run(&host, None, false);
        assert_eq!(host.spawns.borrow().len(), 2);
    }

    #[test]
    fn skipping_spawns_nothing_and_leaves_state_logged_out() {
        for answer in ["none", ""] {
            let host = host(
                vec![checked(LoginState::LoggedOut)],
                vec![checked(LoginState::LoggedOut)],
            );
            host.answers.borrow_mut().push(answer.to_string());
            let reports = run(&host, None, false);
            assert!(host.spawns.borrow().is_empty());
            assert_eq!(
                states(&reports),
                [Some(LoginState::LoggedOut), Some(LoginState::LoggedOut)]
            );
        }
    }

    #[test]
    fn logged_out_but_still_logged_out_after_the_login_reports_it() {
        let host = host(
            vec![checked(LoginState::LoggedOut)],
            vec![checked(LoginState::LoggedIn)],
        );
        let reports = run(&host, Some("claude"), false);
        assert_eq!(host.spawns.borrow().len(), 1);
        assert_eq!(reports[0].state, Some(LoginState::LoggedOut));
    }

    #[test]
    fn yes_without_a_selection_reports_only() {
        let host = host(
            vec![checked(LoginState::LoggedOut)],
            vec![checked(LoginState::LoggedOut)],
        );
        let reports = run(&host, None, true);
        assert!(host.questions.borrow().is_empty());
        assert!(host.spawns.borrow().is_empty());
        assert_eq!(host.probe_calls.borrow().len(), 2, "probed once each");
        assert_eq!(reports[0].state, Some(LoginState::LoggedOut));
    }

    #[test]
    fn without_a_terminal_the_command_is_printed_not_spawned_or_waited_on() {
        let mut host = host(
            vec![checked(LoginState::LoggedOut)],
            vec![checked(LoginState::LoggedIn)],
        );
        host.interactive = false;
        let reports = run(&host, Some("all"), false);
        assert!(host.spawns.borrow().is_empty());
        assert!(host.questions.borrow().is_empty());
        assert_eq!(reports[0].state, Some(LoginState::LoggedOut));
    }

    #[test]
    fn claude_over_ssh_prints_the_command_and_rechecks_after_enter() {
        let mut host = host(
            vec![
                checked(LoginState::LoggedOut),
                checked(LoginState::LoggedIn),
            ],
            vec![checked(LoginState::LoggedIn)],
        );
        host.ssh = true;
        host.answers.borrow_mut().push(String::new());
        let reports = run(&host, Some("claude"), false);
        assert!(host.spawns.borrow().is_empty());
        assert_eq!(host.questions.borrow().len(), 1);
        assert_eq!(reports[0].state, Some(LoginState::LoggedIn));
    }

    #[test]
    fn claude_over_ssh_can_skip_the_recheck() {
        let mut host = host(
            vec![checked(LoginState::LoggedOut)],
            vec![checked(LoginState::LoggedIn)],
        );
        host.ssh = true;
        host.answers.borrow_mut().push("s".to_string());
        let reports = run(&host, Some("claude"), false);
        assert_eq!(host.probe_calls.borrow().len(), 2, "no re-probe");
        assert_eq!(reports[0].state, Some(LoginState::LoggedOut));
    }

    #[test]
    fn codex_uses_device_auth_over_ssh() {
        let mut host = host(
            vec![checked(LoginState::LoggedIn)],
            vec![checked(LoginState::LoggedOut)],
        );
        host.ssh = true;
        run(&host, Some("codex"), false);
        assert_eq!(
            host.spawns.borrow()[0].1,
            ["login".to_string(), "--device-auth".to_string()]
        );
    }

    #[test]
    fn sample_agent_defaults_to_a_logged_in_backend_and_warns_on_logged_out() {
        let reports = vec![
            AgentLoginReport {
                backend: "claude-code",
                aliases: vec!["claude-code", "claude"],
                state: Some(LoginState::LoggedOut),
            },
            AgentLoginReport {
                backend: "codex",
                aliases: vec!["codex"],
                state: Some(LoginState::LoggedIn),
            },
        ];
        assert_eq!(default_sample_agent(&reports), "codex");
        assert!(logged_out(&reports, "claude-code"));
        assert!(logged_out(&reports, "claude"));
        assert!(!logged_out(&reports, "codex"));
        assert!(!logged_out(&reports, "ollama"));
        assert_eq!(default_sample_agent(&[]), "claude-code");
    }

    #[test]
    fn sample_labels_are_idempotent_per_mode() {
        assert_ne!(sample_label("agent"), sample_label("raw"));
    }
}
