//! `ralphus initialize server` (RAL-501): the interactive, hidden new-
//! installation walkthrough. Deliberately absent from `help_map.rs` (see the
//! `resolved_path` carve-out there), so it never appears in `--help`,
//! `show help-map`, or the help-map-derived MCP tool surface, yet is still
//! directly invocable as `ralphus initialize server [--yes]`.
//!
//! `--yes` accepts every stage's default answer instead of prompting, for
//! non-interactive/scripted runs; without it, a non-TTY stdin is rejected
//! the same way `ralphus mcp initialize` rejects one (see `mcp.rs`).

use std::io::{IsTerminal as _, Write as _};
use std::path::PathBuf;

use crate::args::GlobalOpts;
use crate::client::ProjectReviewSettingsPatch;
use crate::commands::misc::CheckArgs;
use crate::health::CheckResult;

const TOTAL_STEPS: u32 = 8;
const WINDOWS_MINIMUM_TMUX_VERSION: (u32, u32, u32) = (3, 3, 8);

pub fn dispatch(opts: &GlobalOpts, yes: bool) -> i32 {
    if !yes && !std::io::stdin().is_terminal() {
        println!(
            "error: ralphus initialize server needs a terminal to prompt interactively; pass --yes to accept every stage's default non-interactively"
        );
        return 2;
    }

    println!("ralphus interactive server setup");

    let mut step = Step::new(TOTAL_STEPS);

    step.begin("Check tmux/psmux");
    step_tmux(yes);

    step.begin("Set up MCP hosts (optional)");
    step_mcp(yes);

    step.begin("Register this repository as a project (optional)");
    let project = step_project(opts, yes);

    step.begin("Configure review defaults and auto-review thresholds");
    match project.as_deref() {
        Some(project) => step_review_settings(opts, project, yes),
        None => println!("  skipped: no project was registered"),
    }

    step.begin("Configure fork requirements");
    match project.as_deref() {
        Some(project) => step_forks(opts, project, yes),
        None => println!("  skipped: no project was registered"),
    }

    step.begin("Create an optional default admin user");
    step_admin(opts, yes);

    step.begin("Run ralphus check health");
    let health_results = step_health(opts);

    step.begin("Submit a sample hello-world task (optional)");
    step_sample(opts, project.as_deref(), &health_results, yes);

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

fn prompt(question: &str, default: &str, yes: bool) -> String {
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

fn prompt_yes_no(question: &str, default: bool, yes: bool) -> bool {
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
fn prompt_secret(question: &str) -> String {
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

fn step_tmux(yes: bool) -> bool {
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
                        offer_tmux_alternative(yes)
                    } else {
                        true
                    }
                }
                None => {
                    println!("  warning: could not determine {program}'s version");
                    offer_tmux_alternative(yes)
                }
            }
        }
        Err(error) => {
            println!("  tmux is not available: {error}");
            offer_tmux_alternative(yes)
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

fn offer_tmux_alternative(yes: bool) -> bool {
    if yes {
        println!(
            "  skipping tmux install/alternative prompts (--yes); `ralphus check health` will report this below"
        );
        return false;
    }
    if cfg!(windows) {
        println!("  install/upgrade with: winget upgrade --id marlocarlo.psmux");
        if prompt_yes_no("  run that command now?", true, yes) {
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
        "  path to an existing tmux/psmux binary to use instead (blank to skip)",
        "",
        yes,
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

fn step_mcp(yes: bool) {
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
    if !prompt_yes_no("  set up ralphus MCP for any of these hosts?", false, yes) {
        println!("  skipped");
        return;
    }
    for host in detected {
        if prompt_yes_no(&format!("  set up ralphus MCP for {host}?"), true, yes) {
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

fn step_project(opts: &GlobalOpts, yes: bool) -> Option<String> {
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
        "  register this repository as a ralphus project?",
        true,
        yes,
    ) {
        println!("  skipped");
        return None;
    }
    let default_name = cwd
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_else(|| "project".to_string());
    let name = prompt("  project name", &default_name, yes);
    let description = prompt("  one-line project description", "", yes);
    let target =
        ralphus_core::strip_verbatim_prefix(cwd.canonicalize().unwrap_or_else(|_| cwd.clone()));
    let target_str = target.to_string_lossy().to_string();
    let client = opts.client();
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

fn step_review_settings(opts: &GlobalOpts, project: &str, yes: bool) {
    let client = opts.client();
    match client.get_project_review_settings(project) {
        Ok(settings) => println!("  current review settings: {settings}"),
        Err(error) => println!("  could not read review settings: {error}"),
    }
    println!(
        "  auto-review thresholds: submissions of that triage type queue until this many are pending, then an automatic review fires (blank disables it for that type)"
    );
    for (triage_type, default_threshold) in [
        ("bug", 3_i64),
        ("feature", 5),
        ("investigation", 3),
        ("unclassified", 5),
    ] {
        let answer = prompt(
            &format!("    {triage_type} threshold"),
            &default_threshold.to_string(),
            yes,
        );
        let threshold = answer.trim().parse::<i64>().ok();
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

fn step_forks(opts: &GlobalOpts, project: &str, yes: bool) {
    if !prompt_yes_no(
        "  does this project require contributors to work from forks?",
        false,
        yes,
    ) {
        println!("  skipped: forks not required");
        return;
    }
    let client = opts.client();
    let patch = ProjectReviewSettingsPatch {
        dual_root_pr: Some(true),
        ..Default::default()
    };
    match client.set_project_review_settings(project, &patch) {
        Ok(_) => println!("  enabled dual-root PRs for \"{project}\""),
        Err(error) => println!("  error enabling dual-root PRs: {error}"),
    }
    let user = prompt(
        "  ralphus user these fork credentials belong to",
        &default_user_name(),
        yes,
    );
    let fork_url = prompt("  your fork's clone URL", "", yes);
    if fork_url.is_empty() {
        println!(
            "  no fork URL given; skipping fork registration (use `ralphus project fork add` later)"
        );
        return;
    }
    match client.add_project_fork(project, &user, &fork_url, None, None) {
        Ok(_) => println!("  registered a fork for {user}"),
        Err(error) => println!("  error registering fork: {error}"),
    }
    if yes {
        println!(
            "  skipping personal access token prompt (--yes); set one later with `ralphus user set-forge-token`"
        );
        return;
    }
    let host = prompt(
        "  forge host for the personal access token (e.g. github.com)",
        "github.com",
        yes,
    );
    let token = prompt_secret(
        "  personal access token for that host (never echoed back or logged by ralphus)",
    );
    if token.is_empty() {
        println!("  no token given; skipping (use `ralphus user set-forge-token` later)");
        return;
    }
    match client.set_user_forge_token(&user, &host, &token) {
        Ok(_) => println!("  stored a forge token for {user}@{host}"),
        Err(error) => println!("  error storing forge token: {error}"),
    }
}

// ---- default admin user ------------------------------------------------------

fn step_admin(opts: &GlobalOpts, yes: bool) {
    if !prompt_yes_no("  create a default admin user?", false, yes) {
        println!("  skipped");
        return;
    }
    let name = prompt("  admin user name", "John Smith", yes);
    let client = opts.client();
    if let Err(error) = client.create_user(&name) {
        println!("  note: create_user reported {error} (the user may already exist)");
    }
    if let Err(error) = client.set_user_admin(&name, true) {
        println!("  error: could not grant admin to {name}: {error}");
        return;
    }
    println!("  {name} is now an admin");
    match persist_default_admin(&name) {
        Ok(path) => println!(
            "  wrote default_user/default_user_is_admin to {} (applies on the daemon's next restart)",
            path.display()
        ),
        Err(error) => {
            println!("  warning: could not persist the default admin to the global config: {error}")
        }
    }
}

fn persist_default_admin(name: &str) -> Result<PathBuf, String> {
    let path = ralphus_daemon::config::global_config_path()
        .ok_or_else(|| "could not resolve a home directory for the global config".to_string())?;
    let mut root: toml::Table = if path.exists() {
        let text = std::fs::read_to_string(&path).map_err(|error| error.to_string())?;
        text.parse::<toml::Table>()
            .map_err(|error| error.to_string())?
    } else {
        toml::Table::new()
    };
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
    Ok(path)
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

fn health_ok_for_sample(results: &[CheckResult]) -> bool {
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
    daemon_ok && agent_ok
}

// ---- sample submission ----------------------------------------------------

fn sample_task_toml(project: &str, branch: &str, agent: &str) -> String {
    format!(
        r#"[[review]]
id = "ralphus:new-review/{branch}"
name = "hello world"
agent = "{agent}"
skip_auto_build = true

[[task]]
name = "{branch}"
project = "{project}"

  [[task.cell]]
  id = "work"
  agent = "{agent}"
  cwd = "<<ralphus:new-worktree/{branch}?upstream=<<default>>>>"
  review = "<<ralphus:new-review/{branch}>>"
  system_prompt = "Do NOT commit and do NOT push under any circumstances. You are working in a dedicated git worktree of this repository; implement the work exactly as described and keep your changes only within the worktree."
  system_prompt_position = "append"
  prompt = "Create a file named hello-world.txt containing the single line \"Hello, world!\". Do not modify any other files."

  [[task.cell]]
  id = "finalize"
  agent = "{agent}"
  cwd = "<<ralphus:new-worktree/{branch}?upstream=<<default>>>>"
  depends_on = ["work"]
  system_prompt = "ONLY git stage the relevant source files, commit them, and push the commit if a remote exists -- do not make further edits."
  system_prompt_position = "append"
  prompt = "Stage, commit, and push hello-world.txt."
"#
    )
}

fn step_sample(
    opts: &GlobalOpts,
    project: Option<&str>,
    health_results: &[CheckResult],
    yes: bool,
) {
    let Some(project) = project else {
        println!("  skipped: no project was registered");
        return;
    };
    if !health_ok_for_sample(health_results) {
        println!(
            "  skipped: `ralphus check health` reported a failing daemon or no reachable agent -- fix that first, then submit a sample task yourself"
        );
        return;
    }
    if !prompt_yes_no(
        "  submit a sample two-cell hello-world task now?",
        true,
        yes,
    ) {
        println!("  skipped");
        return;
    }
    let agent = prompt("  agent backend for the sample task", "claude-code", yes);
    let branch = "ralphus-hello-world";
    let toml_text = sample_task_toml(project, branch, &agent);
    let path =
        std::env::temp_dir().join(format!("ralphus-hello-world-{}.toml", std::process::id()));
    if let Err(error) = std::fs::write(&path, &toml_text) {
        println!("  error: could not write {}: {error}", path.display());
        return;
    }
    let client = opts.client();
    match client.submit(
        &toml_text,
        false,
        Some("ralphus initialize server: hello world"),
    ) {
        Ok(payload) => println!("  submitted: {payload}"),
        Err(error) => println!("  error: submission failed: {error}"),
    }
    println!("  saved the submitted TOML to {}", path.display());
    println!("  equivalent command: ralphus submit {}", path.display());
}
