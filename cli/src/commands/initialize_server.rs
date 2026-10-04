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
//! `--setup-mcp`, `--register-project`, `--require-forks`, `--create-admin`,
//! `--submit-sample`. The value flags are `--tmux-program`, repeatable
//! `--mcp-host`, `--project-name`, `--project-description`,
//! `--bug-threshold`, `--feature-threshold`, `--investigation-threshold`,
//! `--unclassified-threshold`, `--fork-user`, `--fork-url`, `--forge-host`,
//! `--forge-token`, `--admin-name`, `--sample-mode`, and `--sample-agent`.

use std::io::{IsTerminal as _, Write as _};
use std::path::PathBuf;

use crate::args::GlobalOpts;
use crate::client::ProjectReviewSettingsPatch;
use crate::commands::misc::CheckArgs;
use crate::health::CheckResult;

const TOTAL_STEPS: u32 = 8;
const WINDOWS_MINIMUM_TMUX_VERSION: (u32, u32, u32) = (3, 3, 8);
const SAMPLE_LABEL: &str = "ralphus initialize server: hello world";

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
const BUG_THRESHOLD: InitializeSetting = InitializeSetting {
    prompt: "bug threshold",
    flag: "--bug-threshold",
};
const FEATURE_THRESHOLD: InitializeSetting = InitializeSetting {
    prompt: "feature threshold",
    flag: "--feature-threshold",
};
const INVESTIGATION_THRESHOLD: InitializeSetting = InitializeSetting {
    prompt: "investigation threshold",
    flag: "--investigation-threshold",
};
const UNCLASSIFIED_THRESHOLD: InitializeSetting = InitializeSetting {
    prompt: "unclassified threshold",
    flag: "--unclassified-threshold",
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
    &REGISTER_PROJECT,
    &PROJECT_NAME,
    &PROJECT_DESCRIPTION,
    &BUG_THRESHOLD,
    &FEATURE_THRESHOLD,
    &INVESTIGATION_THRESHOLD,
    &UNCLASSIFIED_THRESHOLD,
    &REQUIRE_FORKS,
    &FORK_USER,
    &FORK_URL,
    &FORGE_HOST,
    &FORGE_TOKEN,
    &CREATE_ADMIN,
    &ADMIN_NAME,
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
    pub install_tmux: Option<bool>,
    pub tmux_program: Option<String>,
    pub setup_mcp: Option<bool>,
    pub mcp_hosts: Vec<String>,
    pub register_project: Option<bool>,
    pub project_name: Option<String>,
    pub project_description: Option<String>,
    pub bug_threshold: Option<String>,
    pub feature_threshold: Option<String>,
    pub investigation_threshold: Option<String>,
    pub unclassified_threshold: Option<String>,
    pub require_forks: Option<bool>,
    pub fork_user: Option<String>,
    pub fork_url: Option<String>,
    pub forge_host: Option<String>,
    pub forge_token: Option<String>,
    pub create_admin: Option<bool>,
    pub admin_name: Option<String>,
    pub submit_sample: Option<bool>,
    pub sample_mode: Option<String>,
    pub sample_agent: Option<String>,
}

impl std::fmt::Debug for InitializeServerOptions {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("InitializeServerOptions")
            .field("yes", &self.yes)
            .field("install_tmux", &self.install_tmux)
            .field("tmux_program", &self.tmux_program)
            .field("setup_mcp", &self.setup_mcp)
            .field("mcp_hosts", &self.mcp_hosts)
            .field("register_project", &self.register_project)
            .field("project_name", &self.project_name)
            .field("project_description", &self.project_description)
            .field("bug_threshold", &self.bug_threshold)
            .field("feature_threshold", &self.feature_threshold)
            .field("investigation_threshold", &self.investigation_threshold)
            .field("unclassified_threshold", &self.unclassified_threshold)
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
            .field("submit_sample", &self.submit_sample)
            .field("sample_mode", &self.sample_mode)
            .field("sample_agent", &self.sample_agent)
            .finish()
    }
}

impl InitializeServerOptions {
    fn is_non_interactive(&self) -> bool {
        self.yes
            || self.install_tmux.is_some()
            || self.tmux_program.is_some()
            || self.setup_mcp.is_some()
            || !self.mcp_hosts.is_empty()
            || self.register_project.is_some()
            || self.project_name.is_some()
            || self.project_description.is_some()
            || self.bug_threshold.is_some()
            || self.feature_threshold.is_some()
            || self.investigation_threshold.is_some()
            || self.unclassified_threshold.is_some()
            || self.require_forks.is_some()
            || self.fork_user.is_some()
            || self.fork_url.is_some()
            || self.forge_host.is_some()
            || self.forge_token.is_some()
            || self.create_admin.is_some()
            || self.admin_name.is_some()
            || self.submit_sample.is_some()
            || self.sample_mode.is_some()
            || self.sample_agent.is_some()
    }
}

pub fn dispatch(opts: &GlobalOpts, setup: InitializeServerOptions) -> i32 {
    debug_assert!(interactive_settings_are_valid());
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

    step.begin("Register this repository as a project (optional)");
    let project = step_project(opts, &setup);

    step.begin("Configure review defaults and auto-review thresholds");
    match project.as_deref() {
        Some(project) => step_review_settings(opts, project, &setup),
        None => println!("  skipped: no project was registered"),
    }

    step.begin("Configure fork requirements");
    match project.as_deref() {
        Some(project) => step_forks(opts, project, &setup),
        None => println!("  skipped: no project was registered"),
    }

    step.begin("Create an optional default admin user");
    step_admin(opts, &setup);

    step.begin("Run ralphus check health");
    let health_results = step_health(opts);

    step.begin("Submit a sample hello-world task (optional)");
    step_sample(opts, project.as_deref(), &health_results, &setup);

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
    _setting: &InitializeSetting,
    question: &str,
    default: &str,
    supplied: Option<&String>,
    yes: bool,
) -> String {
    if let Some(supplied) = supplied {
        return supplied.clone();
    }
    if yes {
        return default.to_string();
    }
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
        default.to_string()
    } else {
        answer.to_string()
    }
}

fn prompt_yes_no(
    _setting: &InitializeSetting,
    question: &str,
    default: bool,
    supplied: Option<bool>,
    yes: bool,
) -> bool {
    if let Some(supplied) = supplied {
        return supplied;
    }
    if yes {
        return default;
    }
    let hint = if default { "Y/n" } else { "y/N" };
    print!("{question} [{hint}] ");
    let _ = std::io::stdout().flush();
    let mut answer = String::new();
    let _ = std::io::stdin().read_line(&mut answer);
    match answer.trim().to_ascii_lowercase().as_str() {
        "y" | "yes" => true,
        "n" | "no" => false,
        _ => default,
    }
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
    for host in detected {
        let selected = setup_mcp.map(|_| setup.mcp_hosts.iter().any(|candidate| candidate == host));
        if prompt_yes_no(
            &MCP_HOST,
            &format!("  set up ralphus MCP for {host}?"),
            true,
            selected,
            setup.yes,
        ) {
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

// ---- project registration --------------------------------------------------

fn step_project(opts: &GlobalOpts, setup: &InitializeServerOptions) -> Option<String> {
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
        "  register this repository as a ralphus project?",
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
    match client.get_project(&name) {
        Ok(existing) if existing["path"].as_str() == Some(&target_str) => {
            println!("  already registered project \"{name}\" -> {target_str}; skipping");
            return Some(name);
        }
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
        Err(_) => {}
    }
    match client.register_project(&name, &target_str, &description, "git", None, false, None) {
        Ok(payload) => {
            println!("  registered project \"{name}\" -> {target_str}");
            for warning in payload["warnings"].as_array().into_iter().flatten() {
                if let Some(warning) = warning.as_str() {
                    println!("  warning: {warning}");
                }
            }
            Some(name)
        }
        Err(error) => {
            println!("  error: could not register project: {error}");
            None
        }
    }
}

// ---- review settings / auto-review thresholds ------------------------------

fn step_review_settings(opts: &GlobalOpts, project: &str, setup: &InitializeServerOptions) {
    let client = opts.client();
    match client.get_project_review_settings(project) {
        Ok(settings) => println!("  current review settings: {settings}"),
        Err(error) => println!("  could not read review settings: {error}"),
    }
    println!(
        "  auto-review thresholds: submissions of that triage type queue until this many are pending, then an automatic review fires (blank disables it for that type)"
    );
    let pools = client.list_triage_pools().unwrap_or_default();
    for (setting, triage_type, default_threshold, supplied) in [
        (&BUG_THRESHOLD, "bug", 3_i64, setup.bug_threshold.as_ref()),
        (
            &FEATURE_THRESHOLD,
            "feature",
            5,
            setup.feature_threshold.as_ref(),
        ),
        (
            &INVESTIGATION_THRESHOLD,
            "investigation",
            3,
            setup.investigation_threshold.as_ref(),
        ),
        (
            &UNCLASSIFIED_THRESHOLD,
            "unclassified",
            5,
            setup.unclassified_threshold.as_ref(),
        ),
    ] {
        let answer = prompt(
            setting,
            &format!("    {triage_type} threshold"),
            &default_threshold.to_string(),
            supplied,
            setup.yes,
        );
        let threshold = answer.trim().parse::<i64>().ok();
        let already_configured = pools["pools"].as_array().is_some_and(|pools| {
            pools.iter().any(|pool| {
                pool["project"].as_str() == Some(project)
                    && pool["triage_type"].as_str() == Some(triage_type)
                    && pool["threshold"].as_i64() == threshold
            })
        });
        if already_configured {
            println!("    {triage_type}: already configured; skipping");
            continue;
        }
        match client.set_triage_pool_threshold(project, triage_type, threshold) {
            Ok(_) => println!(
                "    {triage_type}: {}",
                threshold.map_or_else(|| "disabled".to_string(), |t| t.to_string())
            ),
            Err(error) => println!("    {triage_type}: error setting threshold: {error}"),
        }
    }
}

// ---- forks ------------------------------------------------------------------

fn step_forks(opts: &GlobalOpts, project: &str, setup: &InitializeServerOptions) {
    if !prompt_yes_no(
        &REQUIRE_FORKS,
        "  does this project require contributors to work from forks?",
        false,
        setup.require_forks,
        setup.yes,
    ) {
        println!("  skipped: forks not required");
        return;
    }
    let client = opts.client();
    let dual_root_already_enabled = client
        .get_project_review_settings(project)
        .ok()
        .and_then(|settings| settings["effective"]["dual_root_pr"].as_bool())
        .unwrap_or(false);
    if dual_root_already_enabled {
        println!("  dual-root PRs are already enabled for \"{project}\"; skipping");
    } else {
        let patch = ProjectReviewSettingsPatch {
            dual_root_pr: Some(true),
            ..Default::default()
        };
        match client.set_project_review_settings(project, &patch) {
            Ok(_) => println!("  enabled dual-root PRs for \"{project}\""),
            Err(error) => println!("  error enabling dual-root PRs: {error}"),
        }
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
        return;
    }
    let existing_fork = client.list_project_forks(project).ok().and_then(|payload| {
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
        Some(fork) if fork["fork_url"].as_str() == Some(&fork_url) => {
            println!("  fork for {user} is already registered; skipping");
        }
        Some(_) => match client.set_project_fork(project, &user, Some(&fork_url), None, None) {
            Ok(_) => println!("  updated the registered fork for {user}"),
            Err(error) => println!("  error updating fork registration: {error}"),
        },
        None => match client.add_project_fork(project, &user, &fork_url, None, None) {
            Ok(_) => println!("  registered a fork for {user}"),
            Err(error) => println!("  error registering fork: {error}"),
        },
    }
    if setup.yes && setup.forge_host.is_none() && setup.forge_token.is_none() {
        println!(
            "  skipping personal access token prompt (--yes); set one later with `ralphus user set-forge-token`"
        );
        return;
    }
    let host = prompt(
        &FORGE_HOST,
        "  forge host for the personal access token (e.g. github.com)",
        "github.com",
        setup.forge_host.as_ref(),
        setup.yes,
    );
    let token = setup.forge_token.clone().unwrap_or_else(|| {
        prompt_secret(
            &FORGE_TOKEN,
            "  personal access token for that host (never echoed back or logged by ralphus)",
        )
    });
    if token.is_empty() {
        println!("  no token given; skipping (use `ralphus user set-forge-token` later)");
        return;
    }
    let token_already_configured = client
        .list_user_forge_tokens(&user)
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
    match client.set_user_forge_token(&user, &host, &token) {
        Ok(_) => println!("  stored a forge token for {user}@{host}"),
        Err(error) => println!("  error storing forge token: {error}"),
    }
}

// ---- default admin user ------------------------------------------------------

fn step_admin(opts: &GlobalOpts, setup: &InitializeServerOptions) {
    if !prompt_yes_no(
        &CREATE_ADMIN,
        "  create a default admin user?",
        false,
        setup.create_admin,
        setup.yes,
    ) {
        println!("  skipped");
        return;
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
            return;
        }
    }
    if existing_admin != Some(true) {
        match client.set_user_admin(&name, true) {
            Ok(_) => println!("  {name} is now an admin"),
            Err(error) => {
                println!("  error: could not grant admin to {name}: {error}");
                return;
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
    let _ = crate::commands::misc::cmd_check(
        opts,
        CheckArgs {
            enable_developer_checks: false,
            all_remotes: false,
            enable_live_agent_check: false,
        },
    );
    crate::health::run_checks(&opts.daemon_url, &cwd, false, false, false)
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

fn sample_task_toml(project: &str, mode: &str, agent: Option<&str>) -> String {
    let review_id = "ralphus:new-review/ralphus-hello-world";
    let mut toml = format!(
        "[[review]]\nid = \"{review_id}\"\nname = \"Hello world for {project}\"\nproof_scope = \"nothing\"\n\n"
    );
    if let Some(agent) = agent {
        toml.push_str(&format!("agent = \"{agent}\"\n\n"));
    }

    for suffix in ["alpha", "beta", "gamma"] {
        let branch = format!("ralphus-hello-world-{suffix}");
        let file = format!("hello-world-{suffix}.txt");
        toml.push_str(&format!(
            "[[task]]\nname = \"hello-world-{suffix}\"\nproject = \"{project}\"\n\n  [[task.cell]]\n  id = \"write\"\n  cwd = \"<<ralphus:new-worktree/{branch}?upstream=<<default>>>>\"\n  review = \"<<{review_id}>>\"\n"
        ));
        match mode {
            "raw" => toml.push_str(&format!(
                "  mode = \"raw\"\n  command = \"echo Hello from Ralphus task {suffix}.> {file} && git add -- {file} && git commit --message \\\"Add {file}\\\" && git push --set-upstream origin {branch}\"\n\n"
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
    project: Option<&str>,
    health_results: &[CheckResult],
    setup: &InitializeServerOptions,
) {
    let Some(project) = project else {
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
        prompt(
            &SAMPLE_AGENT,
            "  agent backend for the hello-world prompts",
            "claude-code",
            setup.sample_agent.as_ref(),
            setup.yes,
        )
    });
    let toml_text = sample_task_toml(project, &mode, agent.as_deref());
    let client = opts.client();
    let sample_already_submitted = client
        .tasks(None, Some(SAMPLE_LABEL), None)
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
    match client.submit(&toml_text, false, Some(SAMPLE_LABEL)) {
        Ok(payload) => println!("  submitted: {payload}"),
        Err(error) => println!("  error: submission failed: {error}"),
    }
    println!("  saved the submitted TOML to {}", path.display());
    println!("  equivalent command: ralphus submit {}", path.display());
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
        assert_eq!(INTERACTIVE_SETTINGS.len(), 21);
    }

    #[test]
    fn sample_toml_uses_three_parallel_tasks_and_one_explicit_review() {
        let toml = sample_task_toml("example", "agent", Some("claude-code"));
        assert_eq!(toml.matches("[[task]]").count(), 3);
        assert_eq!(toml.matches("review = \"<<ralphus:new-review/").count(), 3);
        assert!(!toml.contains("depends_on"));
        assert!(
            ralphus_core::validate::validate_toml(&toml).is_ok(),
            "{toml}"
        );
    }

    #[test]
    fn raw_sample_toml_has_no_agent_cells() {
        let toml = sample_task_toml("example", "raw", None);
        assert_eq!(toml.matches("mode = \"raw\"").count(), 3);
        assert!(!toml.contains("  agent ="));
        assert!(
            ralphus_core::validate::validate_toml(&toml).is_ok(),
            "{toml}"
        );
    }
}
