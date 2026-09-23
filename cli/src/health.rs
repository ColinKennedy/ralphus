//! `ralphus check health` -- check that the local setup can actually run
//! tasks, ported from `cli/src/ralphus/health.py`.
//!
//! Checks are grouped into three human-facing sections (RAL-415):
//! - [`CORE`]: daemon reachability, layered configuration provenance, every
//!   registered project's on-disk path/git-repo validity, and every
//!   `.ralphus.toml`-driven setting (timeouts, concurrency, forge/PR
//!   conventions, templates, agent profiles/resolver).
//! - [`HARNESS`]: everything the actual execution harness needs to run a
//!   cell -- `git` (only when a registered project actually uses it),
//!   `tmux`/psmux (live session panes), the runner binary, agent backend
//!   commands (`claude`/`codex`/`pi`/`ollama`), and the optional `gh`/`glab`
//!   forge-auth fallbacks.
//! - [`MACHINE`]: host-level resource/build capabilities that degrade
//!   gracefully rather than blocking task execution -- `nvidia-smi` (GPU
//!   sampling), the opt-in developer toolchain (`cargo`), and opt-in
//!   `--all-remotes` target inventory health.
//!
//! Every [`CheckResult`] states three things: `detail` (the observation --
//! what was found), `impact` (what's at stake if this isn't a clean pass),
//! and `remediation` (the concrete next step, or "No action needed." for a
//! pass). `provenance` additionally names the contributing source (a config
//! file, an env var, a resolution path) when one exists.
//!
//! Only a `fail` makes `check health` exit non-zero; `skip` marks a check
//! that plainly does not apply here (e.g. git validation for a non-Git
//! project) rather than one that was evaluated and passed.
//!
//! Live, cost-incurring agent calls are opt-in: [`check_arbiter`] performs a
//! real completion round-trip against the configured Arbiter model and only
//! runs when `enable_live_agent_checks` is set (`--enable-live-agent-check`).
//! Every other check here is reachability/config-shape only, and runs by
//! default.

use std::path::Path;
use std::time::Duration;

use crate::client::DaemonClient;

pub const CORE: &str = "core";
pub const HARNESS: &str = "harness";
pub const MACHINE: &str = "machine";

const PASS: &str = "pass";
const WARN: &str = "warn";
const FAIL: &str = "fail";
/// A check that was evaluated and found not to apply here -- e.g. git-repo
/// validation for a project registered with a non-Git `vcs` kind. Distinct
/// from `pass` (which means "applies, and is fine") so a human/JSON consumer
/// can tell "nothing to check" apart from "checked, all good".
const SKIP: &str = "skip";

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct CheckResult {
    pub name: String,
    pub status: &'static str,
    /// The observation: what was found (a resolved path, a config value, an
    /// error message).
    pub detail: String,
    pub section: &'static str,
    /// RAL-416: the stable [`ralphus_core::health_catalog`] entry this
    /// result belongs to (e.g. `"git"`, `"project-path"`,
    /// `"remote-ssh-reachable"`) -- distinct from `name`, which is a
    /// free-form, sometimes per-instance display identifier (`"project:foo"`,
    /// a fork's own name). Set via [`CheckResult::with_id`] at the point
    /// each check is assembled into [`run_checks`]'s final list, so a
    /// result can always be joined back to its catalog entry (applicability/
    /// cost tier/requirement level) regardless of how its `name` varies.
    /// Empty for a handful of defensive/internal fallback paths that don't
    /// correspond to a normal catalog entry (e.g. a target-check thread
    /// panic) -- see this module's own parity tests.
    pub id: String,
    /// What's at stake if this check's status isn't a clean `pass` --
    /// always populated (including for a `pass`), so a reader never has to
    /// guess why a check exists.
    pub impact: String,
    /// The concrete, actionable next step. `"No action needed."` for a
    /// clean pass or an inapplicable skip.
    pub remediation: String,
    /// Where the effective value/resolution came from -- a config file
    /// path, an env var name, a resolution source label -- when this check
    /// has one contributing source worth naming. `None` for checks with no
    /// single source (e.g. a live daemon round-trip).
    pub provenance: Option<String>,
}

impl CheckResult {
    fn build(
        name: &str,
        status: &'static str,
        detail: impl Into<String>,
        section: &'static str,
        impact: impl Into<String>,
        remediation: impl Into<String>,
    ) -> Self {
        Self {
            name: name.to_string(),
            status,
            detail: detail.into(),
            section,
            id: String::new(),
            impact: impact.into(),
            remediation: remediation.into(),
            provenance: None,
        }
    }

    fn new(
        name: &str,
        status: &'static str,
        detail: impl Into<String>,
        impact: impl Into<String>,
        remediation: impl Into<String>,
    ) -> Self {
        Self::build(name, status, detail, CORE, impact, remediation)
    }

    fn harness(
        name: &str,
        status: &'static str,
        detail: impl Into<String>,
        impact: impl Into<String>,
        remediation: impl Into<String>,
    ) -> Self {
        Self::build(name, status, detail, HARNESS, impact, remediation)
    }

    fn machine(
        name: &str,
        status: &'static str,
        detail: impl Into<String>,
        impact: impl Into<String>,
        remediation: impl Into<String>,
    ) -> Self {
        Self::build(name, status, detail, MACHINE, impact, remediation)
    }

    #[must_use]
    fn with_provenance(mut self, provenance: impl Into<String>) -> Self {
        self.provenance = Some(provenance.into());
        self
    }

    /// Tags this result with its stable [`ralphus_core::health_catalog`]
    /// entry id -- see the field doc comment on [`CheckResult::id`].
    #[must_use]
    fn with_id(mut self, id: impl Into<String>) -> Self {
        self.id = id.into();
        self
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
    ralphus_core::process::is_compound_shell_command(value)
}

/// Strips one layer of wrapping quotes from a bare (non-compound) path.
#[must_use]
pub fn unquote_path(value: &str) -> String {
    ralphus_core::process::unquote_path(value)
}

/// Delegates to `GET /api/health/report` (RAL-485): the daemon's cached
/// hourly Free-tier sweep already resolves the effective command (a
/// database override, then an env-var override, then the compiled default --
/// the identical precedence a real cell dispatch uses) and evaluates it --
/// checking a direct command's disk/PATH accessibility and executability
/// (Pi additionally version-checked), or reporting a complex command as
/// `skip` without executing it. Delegating here, rather than
/// re-implementing the same resolution/probing client-side (as this CLI used
/// to, ignorant of any database override), is what keeps `ralphus check
/// health` from ever disagreeing with the daemon's Health/Agents tabs about
/// these three backends.
fn check_agent_command_via_daemon(
    daemon_url: &str,
    id: &'static str,
    display_name: &str,
) -> CheckResult {
    let client = DaemonClient::new(daemon_url);
    let response = match client.health_report() {
        Ok(response) => response,
        Err(e) => {
            return CheckResult::harness(
                display_name,
                FAIL,
                format!("could not reach daemon to check {display_name}: {e}"),
                "Tasks using this backend cannot be validated.",
                "Ensure the daemon is reachable, then re-run this check.",
            );
        }
    };
    let checks = response["checks"].as_array().cloned().unwrap_or_default();
    let Some(check) = checks.iter().find(|c| c["id"].as_str() == Some(id)) else {
        return CheckResult::harness(
            display_name,
            FAIL,
            "the daemon has not completed a health sweep yet",
            "Tasks using this backend cannot be validated until the daemon's sweep completes.",
            "Wait for the daemon's hourly sweep (or POST /api/health/report/refresh), then re-run this check.",
        );
    };
    let detail = check["detail"].as_str().unwrap_or_default().to_string();
    match check["status"].as_str() {
        Some("pass") => CheckResult::harness(
            display_name,
            PASS,
            detail,
            "Tasks using this backend can start.",
            "No action needed.",
        ),
        Some("skip") => CheckResult::harness(
            display_name,
            SKIP,
            detail,
            "The command is a shell pipeline/multi-word invocation, so only a live run can confirm it actually works.",
            "No action needed; verify by running a task with this backend.",
        ),
        _ => CheckResult::harness(
            display_name,
            FAIL,
            detail,
            "Tasks using this backend cannot start.",
            "See the detail above and fix the effective command (a database override, an env-var override, or the compiled default).",
        ),
    }
}

fn is_executable(path: &Path) -> bool {
    ralphus_core::process::is_executable(path)
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
            if is_executable(&candidate) {
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
            "No task can be submitted, monitored, or managed -- and every other check below that talks to the daemon will also fail or be silently skipped.",
            "Start the daemon (`ralphus-daemon`), or fix --daemon-url/$RALPHUS_DAEMON_URL to point at a running one.",
        )],
        Ok(health) => {
            let version = health["version"].as_str().unwrap_or("?");
            let name = health["name"].as_str().unwrap_or("?");
            let mut results = vec![CheckResult::new(
                "daemon",
                PASS,
                format!("reachable ({name} {version})"),
                "The CLI can submit and monitor tasks.",
                "No action needed.",
            )];
            if let Some(warnings) = health["warnings"].as_array() {
                for w in warnings {
                    results.push(CheckResult::new(
                        "daemon",
                        WARN,
                        w.as_str().unwrap_or_default().to_string(),
                        "The daemon itself flagged a condition worth attention; specifics vary by warning.",
                        "See the daemon's own logs/documentation for this warning.",
                    ));
                }
            }
            results
        }
    }
}

/// Whether `git` itself (the binary check, [`check_git`]) is a hard
/// requirement -- only true when at least one registered project actually
/// uses it, or when project registration state couldn't be determined at
/// all (unreachable daemon, nothing registered yet), in which case the
/// safer default is to still require it.
fn check_git(git_required: bool) -> CheckResult {
    match which("git") {
        Some(path) => CheckResult::harness(
            "git",
            PASS,
            path,
            "Guardian reviews, worktree creation, and Git-based project validation all shell out to git.",
            "No action needed.",
        ),
        None if !git_required => CheckResult::harness(
            "git",
            SKIP,
            "not found on PATH, but no registered project uses Git",
            "None today -- every registered project's vcs kind is non-Git, so nothing here depends on a git binary.",
            "If you register a Git-based project later, install git and re-run this check.",
        ),
        None => CheckResult::harness(
            "git",
            FAIL,
            "not found on PATH",
            "Guardian reviews and Git-based project validation cannot run without it.",
            "Install git and ensure it resolves on PATH.",
        ),
    }
}

/// Re-runs the same path/git-repo validation `ralphus project git` does at
/// registration time, for one already-registered project -- skipping the
/// git-specific half entirely when `vcs` names a non-Git kind (RAL-415: a
/// project's on-disk path must still exist regardless of VCS, but only a
/// Git project needs a `.git` checkout).
fn check_project_path(name: &str, path: &str, vcs: &str) -> CheckResult {
    let check_name = format!("project:{name}");
    let p = Path::new(path);
    if !p.is_dir() {
        return CheckResult::new(
            &check_name,
            FAIL,
            format!("{path} does not exist or is not a directory"),
            "Every task/cell routed to this project fails before it can even start.",
            format!(
                "Create {path}, or re-point the registration (ralphus project git --name {name} --path <path>)."
            ),
        );
    }
    if vcs != "git" {
        return CheckResult::new(
            &check_name,
            SKIP,
            format!("{path} exists; vcs=\"{vcs}\" is not Git, so repository validation is skipped"),
            "None -- this project's vcs kind has no git-specific requirement to validate.",
            "No action needed.",
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
            "Reviews and worktree operations for this project cannot run.",
            "Ensure git is installed and runnable from this project's path.",
        ),
        Ok(out) => {
            let stdout = String::from_utf8_lossy(&out.stdout);
            if !out.status.success() || stdout.trim() != "true" {
                CheckResult::new(
                    &check_name,
                    FAIL,
                    format!("{path} is not a git repository"),
                    "This project is registered with vcs=\"git\" but has no working .git checkout; reviews and worktree creation will fail.",
                    format!("Run `git init` in {path}, or fix this project's registered vcs kind."),
                )
            } else {
                CheckResult::new(
                    &check_name,
                    PASS,
                    path,
                    "Reviews and worktree operations can run against this project.",
                    "No action needed.",
                )
            }
        }
    }
}

/// RAL-355: a git project with no registered clone URL fails remote
/// provisioning outright (`worktrees::provision_remote`) the moment any of
/// its cells route to a non-local `machine` -- see
/// `daemon/src/worktrees.rs`. `has_remote_providers` names the cheapest
/// signal `check health` can see today for "remote work is possible here"
/// without a full target inventory (RAL-185 Phase 9, not built yet): at
/// least one machine provider is registered with the daemon at all. Warns
/// rather than fails -- the project is still perfectly usable for local
/// work, and not every registered provider is necessarily used by every
/// project.
fn check_project_clone_url(
    name: &str,
    clone_url: Option<&str>,
    has_remote_providers: bool,
) -> Option<CheckResult> {
    if clone_url.is_some() || !has_remote_providers {
        return None;
    }
    Some(CheckResult::new(
        &format!("project:{name}:clone-url"),
        WARN,
        "no clone URL registered",
        format!("A cell for \"{name}\" routed to a remote machine will fail during provisioning."),
        format!("Register one: ralphus project git --name {name} --path <path> --url <clone-url>"),
    ))
}

/// Per-[`check_projects`] summary: its [`CheckResult`]s, plus the two facts
/// [`check_git`] needs to decide whether the `git` binary itself is
/// actually required here (RAL-415).
struct ProjectsSummary {
    results: Vec<CheckResult>,
    any_project_registered: bool,
    any_git_project: bool,
}

/// Validates every project registered with the daemon. Contributes empty
/// results (and treats git as still required, the conservative default) if
/// the daemon is unreachable ([`check_daemon`] already reports that) or if
/// no projects are registered.
fn check_projects(daemon_url: &str) -> ProjectsSummary {
    let client = DaemonClient::new(daemon_url);
    let Ok(response) = client.list_projects() else {
        return ProjectsSummary {
            results: Vec::new(),
            any_project_registered: false,
            any_git_project: false,
        };
    };
    let has_remote_providers = client
        .list_machines()
        .ok()
        .and_then(|m| m["machines"].as_array().map(|a| !a.is_empty()))
        .unwrap_or(false);
    let projects: Vec<_> = response["projects"].as_array().cloned().unwrap_or_default();
    let any_project_registered = !projects.is_empty();
    let any_git_project = projects
        .iter()
        .any(|p| p["vcs"].as_str().unwrap_or("git") == "git");
    let results = projects
        .iter()
        .flat_map(|p| {
            let Some(name) = p["name"].as_str() else {
                return Vec::new();
            };
            let Some(path) = p["path"].as_str() else {
                return Vec::new();
            };
            let vcs = p["vcs"].as_str().unwrap_or("git");
            let mut results = vec![
                check_project_path(name, path, vcs)
                    .with_id(ralphus_core::health_catalog::ID_PROJECT_PATH),
            ];
            if vcs == "git" {
                results.extend(
                    check_project_clone_url(name, p["clone_url"].as_str(), has_remote_providers)
                        .map(|r| r.with_id(ralphus_core::health_catalog::ID_PROJECT_CLONE_URL)),
                );
            }
            results
        })
        .collect();
    ProjectsSummary {
        results,
        any_project_registered,
        any_git_project,
    }
}

/// RAL-338: fork registration health, evaluated daemon-side (see
/// `DaemonClient::health_project_forks`'s doc comment for why). Silently
/// contributes nothing if the daemon is unreachable or no forks are
/// registered at all, matching [`check_projects`]'s precedent.
fn check_project_forks(daemon_url: &str) -> Vec<CheckResult> {
    let client = DaemonClient::new(daemon_url);
    let Ok(response) = client.health_project_forks() else {
        return Vec::new();
    };
    response["checks"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|c| {
            let status = match c["status"].as_str() {
                Some(PASS) => PASS,
                Some(WARN) => WARN,
                _ => FAIL,
            };
            let project = c["project"].as_str().unwrap_or_default();
            let user = c["user"].as_str().unwrap_or_default();
            let name = c["name"].as_str().unwrap_or("fork");
            let check_name = if user.is_empty() {
                format!("{name}:{project}")
            } else {
                format!("{name}:{project}:{user}")
            };
            CheckResult::new(
                &check_name,
                status,
                c["detail"].as_str().unwrap_or_default(),
                "A misregistered fork breaks automated PR routing for this project/user.",
                "Re-run this project/user's fork registration, or see the detail above for specifics.",
            )
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
        return CheckResult::harness(
            "runner",
            WARN,
            format!("'{program}' not found (set RALPHUS_RUNNER_CMD)"),
            "The daemon cannot launch cells without a resolvable runner binary; tasks will fail to start.",
            "Build/install ralphus-runner and put it on PATH, or set RALPHUS_RUNNER_CMD to its full path.",
        );
    }
    CheckResult::harness(
        "runner",
        PASS,
        program,
        "The daemon can launch cells.",
        "No action needed.",
    )
}

fn check_nvidia_smi() -> CheckResult {
    match which("nvidia-smi") {
        None => CheckResult::machine(
            "nvidia-smi",
            WARN,
            "not found on PATH",
            "The resource view's GPU column shows N/A instead of live usage; nothing else is affected.",
            "Optional: install NVIDIA drivers/nvidia-smi if you want GPU usage reported.",
        ),
        Some(path) => CheckResult::machine(
            "nvidia-smi",
            PASS,
            path,
            "GPU usage is reported in the resource view.",
            "No action needed.",
        ),
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
        CheckResult::harness(
            "ollama",
            PASS,
            base,
            "Tasks using the ollama agent backend can reach a local model server.",
            "No action needed.",
        )
    } else {
        CheckResult::harness(
            "ollama",
            FAIL,
            format!("not reachable at {base}"),
            "Tasks using the ollama agent backend (and the Arbiter's default resolver, unless reconfigured) cannot run.",
            "Start Ollama (`ollama serve`), or point $RALPHUS_OLLAMA_URL at a reachable server.",
        )
    }
}

/// `daemon/src/forge.rs::resolve_cli_token`'s fallback role, shared by both
/// [`check_gh_for`] and [`check_glab_for`]'s detail text (RAL-415): neither
/// binary is ever required, and a missing one must never fail this check --
/// it just means the fallback path is unavailable if the primary token env
/// var also turns out to be unset.
fn check_gh_for(found: Option<String>) -> CheckResult {
    const ROLE: &str = "optional fallback token source for GitHub auth (`gh auth token`), used only when RALPHUS_GITHUB_TOKEN / [forge].token_env is unset";
    match found {
        Some(path) => CheckResult::harness(
            "gh",
            PASS,
            format!("{path} ({ROLE})"),
            "GitHub PR submission can fall back to a token already cached by `gh auth login`.",
            "No action needed.",
        ),
        None => CheckResult::harness(
            "gh",
            PASS,
            format!("not found on PATH ({ROLE})"),
            "No effect unless RALPHUS_GITHUB_TOKEN/[forge].token_env is also unset -- in that case GitHub PR submission has no token to use.",
            "Optional: install the GitHub CLI (https://cli.github.com) and run `gh auth login`, or set RALPHUS_GITHUB_TOKEN directly.",
        ),
    }
}

fn check_gh() -> CheckResult {
    check_gh_for(which("gh"))
}

fn check_glab_for(found: Option<String>) -> CheckResult {
    const ROLE: &str = "optional fallback token source for GitLab auth (`glab auth status --show-token`), used only when RALPHUS_GITLAB_TOKEN / [forge].token_env is unset";
    match found {
        Some(path) => CheckResult::harness(
            "glab",
            PASS,
            format!("{path} ({ROLE})"),
            "GitLab PR submission can fall back to a token already cached by `glab auth login`.",
            "No action needed.",
        ),
        None => CheckResult::harness(
            "glab",
            PASS,
            format!("not found on PATH ({ROLE})"),
            "No effect unless RALPHUS_GITLAB_TOKEN/[forge].token_env is also unset -- in that case GitLab PR submission has no token to use.",
            "Optional: install the GitLab CLI (https://gitlab.com/gitlab-org/cli) and run `glab auth login`, or set RALPHUS_GITLAB_TOKEN directly.",
        ),
    }
}

fn check_glab() -> CheckResult {
    check_glab_for(which("glab"))
}

/// Resolves tmux/psmux exactly the way the daemon does
/// (`ralphus_daemon::tmux::resolve_tmux_program_with_source`), reporting
/// both the resolved value and which resolution source won (an explicit
/// `RALPHUS_TMUX_CMD` override, the embedded vendored build, or `PATH`) --
/// RAL-415. A missing binary is a hard `fail`: every live cell session
/// depends on it.
fn check_tmux_for(resolved: Result<(String, &'static str), String>) -> CheckResult {
    match resolved {
        Ok((program, source)) => CheckResult::harness(
            "tmux",
            PASS,
            format!("{program} (source: {source})"),
            "Live sessions/panes for running cells depend on this binary.",
            "No action needed.",
        )
        .with_provenance(source),
        Err(e) => CheckResult::harness(
            "tmux",
            FAIL,
            e,
            "Cells cannot start a live, pollable session; task execution fails wherever it depends on tmux/psmux.",
            "Install tmux/psmux and put it on PATH, or set RALPHUS_TMUX_CMD to its full path (see docs/dependencies.md).",
        ),
    }
}

fn check_tmux() -> CheckResult {
    check_tmux_for(
        ralphus_daemon::tmux::resolve_tmux_program_with_source().map_err(|e| e.to_string()),
    )
}

/// Lists every `.ralphus.toml`-shaped file that layers into the CLI-side
/// `task.*`/`[daemon]` settings loader (`crate::config::load_config`), in
/// resolution order (later wins) -- RAL-415: "which files even contribute"
/// is a different, prerequisite question to the per-field provenance the
/// individual `[daemon]`/`[task]` checks below already report.
fn check_config_sources_cli_for(config: &crate::config::Config) -> CheckResult {
    if config.sources.is_empty() {
        return CheckResult::new(
            "config-sources",
            PASS,
            "no .ralphus.toml files contribute to task.*/[daemon] settings; built-in defaults apply",
            "None -- task.maximum_timeout_seconds and [daemon] settings are at their built-in defaults.",
            "If you expect an override to apply, confirm the file exists and is named .ralphus.toml.",
        );
    }
    let listing = config
        .sources
        .iter()
        .enumerate()
        .map(|(i, p)| {
            let label = config
                .source_labels
                .iter()
                .find(|(sp, _)| sp == p)
                .map(|(_, l)| l.as_str())
                .unwrap_or("unknown");
            format!("{}. {} ({label})", i + 1, p.display())
        })
        .collect::<Vec<_>>()
        .join("; ");
    CheckResult::new(
        "config-sources",
        PASS,
        format!("{listing} -- later entries override earlier ones for task.*/[daemon] settings"),
        "Determines the effective task.maximum_timeout_seconds and [daemon] log/concurrency settings.",
        "No action needed; edit the last-listed file to change effective settings.",
    )
    .with_provenance(
        config
            .sources
            .last()
            .map(|p| p.display().to_string())
            .unwrap_or_default(),
    )
}

fn check_config_sources_cli(cwd: &Path) -> CheckResult {
    check_config_sources_cli_for(&crate::config::load_config(cwd, true))
}

/// Lists the global + nearest-per-project `.ralphus.toml` files that layer
/// into `ralphus_daemon::config`'s own loader (used by the `[forge]`/
/// `[live_view]`/`[thrash]`/`[[templates]]`/`[ui]` checks below) -- a
/// distinct resolution chain from [`check_config_sources_cli`]'s
/// `$RALPHUS_CONFIGURATION_PATH`-based one (RAL-415).
fn check_config_sources_project_for(global: Option<&Path>, project: Option<&Path>) -> CheckResult {
    let mut entries = Vec::new();
    if let Some(g) = global {
        entries.push(format!("{}. {} (global)", entries.len() + 1, g.display()));
    }
    if let Some(p) = project {
        entries.push(format!(
            "{}. {} (nearest project)",
            entries.len() + 1,
            p.display()
        ));
    }
    if entries.is_empty() {
        return CheckResult::new(
            "config-sources-project",
            PASS,
            "no global or per-project .ralphus.toml found; built-in defaults apply for [forge]/[live_view]/[thrash]/[[templates]]/[ui]",
            "None -- those settings are at their built-in defaults.",
            "No action needed.",
        );
    }
    CheckResult::new(
        "config-sources-project",
        PASS,
        format!(
            "{} -- project overrides global for [forge]/[live_view]/[thrash]/[[templates]]/[ui] settings",
            entries.join("; ")
        ),
        "Determines the effective forge/live-view/thrash/template/UI settings used by the checks below.",
        "No action needed; edit the project file to override the global one.",
    )
}

fn check_config_sources_project(cwd: &Path) -> CheckResult {
    check_config_sources_project_for(
        ralphus_daemon::config::global_config_path().as_deref(),
        ralphus_daemon::config::find_project_config(cwd).as_deref(),
    )
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
            "Every subprocess timeout computation is undefined with a negative cap; task execution behavior becomes unreliable.",
            "Set task.maximum_timeout_seconds to 0 (unbounded) or a positive number of seconds.",
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
            format!("task.maximum_timeout_seconds is 0 (no timeout){src}"),
            "Backend sessions may run forever if an agent hangs.",
            "Set task.maximum_timeout_seconds to a positive number of seconds if you want a hard cap.",
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
        "Subprocess backends are capped at this wall-clock duration.",
        "No action needed.",
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
            "The daemon caps concurrent cells at the built-in default.",
            "No action needed.",
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
                "daemon.max_concurrent is {mc}{src}; must be >= 0 (0 = no limit, positive = concurrency cap)"
            ),
            "The configured value is ignored; the daemon silently falls back to its built-in default instead.",
            format!(
                "Set daemon.max_concurrent to 0 (no limit) or a positive concurrency cap, or leave it unset (default {})",
                ralphus_daemon::DEFAULT_MAX_CONCURRENT
            ),
        );
    }
    if mc == 0 {
        return CheckResult::new(
            "daemon-max-concurrent",
            WARN,
            format!("daemon.max_concurrent is 0{src} (no limit)"),
            "Every ready cell may run at once, which can overwhelm the machine.",
            "Set daemon.max_concurrent to a positive cap if you want to bound parallelism.",
        );
    }
    CheckResult::new(
        "daemon-max-concurrent",
        PASS,
        format!("daemon.max_concurrent={mc}{src}"),
        "The daemon caps concurrent cells at this value.",
        "No action needed.",
    )
}

/// Surfaces `[daemon] opentelemetry = false` in `check health`. This is a
/// deliberate config choice, not a problem to flag, so it's reported at
/// `pass` severity -- the point is just confirming the setting took effect,
/// not warning about it.
fn check_opentelemetry(cwd: &Path) -> CheckResult {
    let config = crate::config::load_config(cwd, true);
    if config.daemon.opentelemetry {
        return CheckResult::new(
            "daemon-opentelemetry",
            PASS,
            "enabled",
            "Cell/scheduler activity is exported as OpenTelemetry traces.",
            "No action needed.",
        );
    }
    let src = config
        .provenance
        .iter()
        .find(|(k, _)| *k == "daemon.opentelemetry")
        .and_then(|(_, v)| v.as_ref())
        .map(|p| format!(" (from {})", p.display()))
        .unwrap_or_default();
    CheckResult::new(
        "daemon-opentelemetry",
        PASS,
        format!("OpenTelemetry has been disabled{src}"),
        "No traces are exported; tracing tools have nothing to show for this daemon.",
        "Set [daemon] opentelemetry = true if you want to export traces.",
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
    let (raw, src_path) =
        if let Some(v) = project_table.and_then(|t| t.get("tool_arg_truncate_chars")) {
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
            "Tool-use argument values in the Live View pane are truncated at the built-in default.",
            "No action needed.",
        );
    };
    let src_str = src_path
        .map(|p| format!(" (from {})", p.display()))
        .unwrap_or_default();
    match raw.as_integer() {
        None => CheckResult::new(
            "tool-arg-truncate-chars",
            WARN,
            format!(
                "live_view.tool_arg_truncate_chars is not a number{src_str} -- falling back to the default {}",
                ralphus_daemon::config::DEFAULT_TOOL_ARG_TRUNCATE_CHARS
            ),
            "The configured value is ignored; Live View truncation silently uses the default instead.",
            "Set live_view.tool_arg_truncate_chars to an integer >= 0.",
        ),
        Some(n) if n < 0 => CheckResult::new(
            "tool-arg-truncate-chars",
            WARN,
            format!(
                "live_view.tool_arg_truncate_chars is {n}{src_str}; must be >= 0 -- falling back to the default {}",
                ralphus_daemon::config::DEFAULT_TOOL_ARG_TRUNCATE_CHARS
            ),
            "The configured value is ignored; Live View truncation silently uses the default instead.",
            "Set live_view.tool_arg_truncate_chars to an integer >= 0.",
        ),
        Some(n) => CheckResult::new(
            "tool-arg-truncate-chars",
            PASS,
            format!("live_view.tool_arg_truncate_chars={n}{src_str}"),
            "Tool-use argument values in the Live View pane are truncated at this length.",
            "No action needed.",
        )
        .with_provenance(
            src_path
                .map(|p| p.display().to_string())
                .unwrap_or_default(),
        ),
    }
}

/// Reads `[thrash]` as a raw TOML table from `path`, if the file exists and
/// parses -- same "raw table, not the typed loader" reasoning as
/// [`read_live_view_table`]: a typed `Option<u32>` can't tell "key absent"
/// apart from "key present but invalid", and this check needs to warn on the
/// latter.
fn read_thrash_table(path: &Path) -> Option<toml::Table> {
    let text = std::fs::read_to_string(path).ok()?;
    let root: toml::Table = text.parse().ok()?;
    root.get("thrash")?.as_table().cloned()
}

/// Validates one `[thrash]` scalar field (RAL-339's `max_compactions` (N) or
/// `min_turn_gap` (M)) for `cwd`, mirroring
/// [`check_tool_arg_truncate_chars_for`]'s shape: PASS reports the resolved
/// value and its source; a present-but-invalid value is a WARN, not a FAIL,
/// since `ralphus_daemon::config::ThrashConfig::max_compactions`/
/// `min_turn_gap` never block on it -- an invalid value is silently ignored
/// in favor of the default, the same "malformed config never blocks" rule
/// every other `.ralphus.toml` scalar follows.
fn check_thrash_field_for(
    global_table: Option<&toml::Table>,
    global_path: Option<&Path>,
    project_table: Option<&toml::Table>,
    project_path: Option<&Path>,
    field: &str,
    check_name: &str,
    default: u32,
) -> CheckResult {
    let (raw, src_path) = if let Some(v) = project_table.and_then(|t| t.get(field)) {
        (Some(v), project_path)
    } else if let Some(v) = global_table.and_then(|t| t.get(field)) {
        (Some(v), global_path)
    } else {
        (None, None)
    };

    let Some(raw) = raw else {
        return CheckResult::new(
            check_name,
            PASS,
            format!("thrash.{field} unset; using default {default}"),
            "Thrash detection uses the built-in default for this field.",
            "No action needed.",
        );
    };
    let src_str = src_path
        .map(|p| format!(" (from {})", p.display()))
        .unwrap_or_default();
    match raw.as_integer() {
        None => CheckResult::new(
            check_name,
            WARN,
            format!(
                "thrash.{field} is not a number{src_str} -- falling back to the default {default}"
            ),
            "The configured value is ignored; thrash detection silently uses the default instead.",
            format!("Set thrash.{field} to a non-negative integer."),
        ),
        Some(n) if n < 0 => CheckResult::new(
            check_name,
            WARN,
            format!(
                "thrash.{field} is {n}{src_str}; must be >= 0 -- falling back to the default {default}"
            ),
            "The configured value is ignored; thrash detection silently uses the default instead.",
            format!("Set thrash.{field} to a non-negative integer."),
        ),
        Some(n) => CheckResult::new(
            check_name,
            PASS,
            format!("thrash.{field}={n}{src_str}"),
            "Thrash detection uses this configured value.",
            "No action needed.",
        )
        .with_provenance(
            src_path
                .map(|p| p.display().to_string())
                .unwrap_or_default(),
        ),
    }
}

/// Validates `.ralphus.toml`'s `[thrash] max_compactions` (N, RAL-339) --
/// how many autocompactions must occur in one run before thrash detection
/// can fire at all. See `runner/src/thrash.rs`'s `ThrashTracker` for the
/// rule this feeds.
fn check_thrash_max_compactions(cwd: &Path) -> CheckResult {
    let global_path = ralphus_daemon::config::global_config_path();
    let project_path = ralphus_daemon::config::find_project_config(cwd);
    let global_table = global_path.as_deref().and_then(read_thrash_table);
    let project_table = project_path.as_deref().and_then(read_thrash_table);
    check_thrash_field_for(
        global_table.as_ref(),
        global_path.as_deref(),
        project_table.as_ref(),
        project_path.as_deref(),
        "max_compactions",
        "thrash-max-compactions",
        ralphus_daemon::config::DEFAULT_THRASH_MAX_COMPACTIONS,
    )
}

/// Validates `.ralphus.toml`'s `[thrash] min_turn_gap` (M, RAL-339) -- the
/// previous-compaction gap, in assistant turns, below which a compaction
/// at/after `max_compactions` counts as thrash.
fn check_thrash_min_turn_gap(cwd: &Path) -> CheckResult {
    let global_path = ralphus_daemon::config::global_config_path();
    let project_path = ralphus_daemon::config::find_project_config(cwd);
    let global_table = global_path.as_deref().and_then(read_thrash_table);
    let project_table = project_path.as_deref().and_then(read_thrash_table);
    check_thrash_field_for(
        global_table.as_ref(),
        global_path.as_deref(),
        project_table.as_ref(),
        project_path.as_deref(),
        "min_turn_gap",
        "thrash-min-turn-gap",
        ralphus_daemon::config::DEFAULT_THRASH_MIN_TURN_GAP,
    )
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
            "PR branches are named using the built-in default convention.",
            "No action needed.",
        );
    };
    match ralphus_daemon::config::validate_pull_request_branch_convention(&convention) {
        Ok(()) => CheckResult::new(
            "pull-request-branch-convention",
            PASS,
            convention,
            "PR branches are named using this convention.",
            "No action needed.",
        ),
        Err(e) => CheckResult::new(
            "pull-request-branch-convention",
            FAIL,
            e,
            "PR submission fails outright: an invalid convention has no safe default to fall back to.",
            "Fix [forge].pull_request_branch_convention so it contains the required {name} placeholder.",
        ),
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
            "The Simple task form's template picker offers only the built-in fallback.",
            "No action needed; add [[templates]] entries to offer more.",
        ));
    } else {
        let errors = ralphus_daemon::config::validate_templates(templates);
        if errors.is_empty() {
            results.push(CheckResult::new(
                "templates",
                PASS,
                format!("{} template(s) configured", templates.len()),
                "The Simple task form's template picker offers these templates.",
                "No action needed.",
            ));
        } else {
            for e in errors {
                results.push(CheckResult::new(
                    "templates",
                    FAIL,
                    e.message,
                    "A malformed template entry breaks the Simple task form's template picker.",
                    "Fix the [[templates]] entry named in the detail above.",
                ));
            }
        }
    }
    let mut results: Vec<CheckResult> = results
        .into_iter()
        .map(|r| r.with_id(ralphus_core::health_catalog::ID_TEMPLATES))
        .collect();
    match &ui.new_task_default_tab {
        None => results.push(
            CheckResult::new(
                "new-task-default-tab",
                PASS,
                "not set (defaults to 'simple')",
                "The Simple task form opens on the 'simple' tab by default.",
                "No action needed.",
            )
            .with_id(ralphus_core::health_catalog::ID_NEW_TASK_DEFAULT_TAB),
        ),
        Some(tab) => match ralphus_daemon::config::validate_new_task_default_tab(tab) {
            Ok(()) => results.push(
                CheckResult::new(
                    "new-task-default-tab",
                    PASS,
                    tab.clone(),
                    "The Simple task form opens on this tab by default.",
                    "No action needed.",
                )
                .with_id(ralphus_core::health_catalog::ID_NEW_TASK_DEFAULT_TAB),
            ),
            Err(e) => results.push(
                CheckResult::new(
                    "new-task-default-tab",
                    FAIL,
                    e,
                    "[ui].new_task_default_tab names an unknown tab; the New Task form's default-tab setting is broken.",
                    "Set [ui].new_task_default_tab to a known tab name.",
                )
                .with_id(ralphus_core::health_catalog::ID_NEW_TASK_DEFAULT_TAB),
            ),
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
                "Custom [agent.profiles.*] entries cannot be validated; a misconfigured profile could fail silently at task-submission time instead.",
                "Ensure the daemon is reachable, then re-run this check.",
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
                "A broken agent profile fails any task that selects it, at submission time.",
                "Fix the profile in .ralphus.toml's [agent.profiles.*] per the detail above.",
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
                "Review resolution cannot be validated; a misconfigured resolver agent could fail silently at review time instead.",
                "Ensure the daemon is reachable, then re-run this check.",
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
        CheckResult::new(
            "default-resolver-agent",
            PASS,
            default_agent,
            "Reviews without an explicit resolver fall back to this agent.",
            "No action needed.",
        )
    } else {
        CheckResult::new(
            "default-resolver-agent",
            FAIL,
            format!(
                "[review].default_resolver_agent = \"{default_agent}\" does not match any \
                 built-in backend or configured [agent.profiles.*] entry for this project"
            ),
            "Reviews without an explicit resolver fail outright instead of falling back to a working agent.",
            "Fix [review].default_resolver_agent to name a built-in backend or a configured [agent.profiles.*] entry.",
        )
    }
}

/// RAL-318: a live completion round-trip against the configured Arbiter
/// agent/model, via `POST /api/health/arbiter`. Unlike every other check in
/// this module (config shape/reachability only), this is a genuinely live,
/// cost-incurring call -- gated behind `enable_live_agent_checks`
/// (`--enable-live-agent-check`, RAL-415) so plain `ralphus check health`
/// never silently spends model budget; the endpoint itself still respects
/// the Arbiter's `maximum_budget_usd` cap when it does run.
fn check_arbiter(daemon_url: &str) -> CheckResult {
    let client = DaemonClient::new(daemon_url);
    match client.health_arbiter() {
        Ok(response) => {
            let agent = response["agent"].as_str().unwrap_or_default();
            if response["status"].as_str() == Some(PASS) {
                CheckResult::new(
                    "arbiter",
                    PASS,
                    format!(
                        "{agent} responded: {}",
                        response["reply"].as_str().unwrap_or_default()
                    ),
                    "The Arbiter can complete a live round-trip against its configured agent/model.",
                    "No action needed.",
                )
            } else {
                CheckResult::new(
                    "arbiter",
                    FAIL,
                    format!(
                        "{agent}: {}",
                        response["detail"].as_str().unwrap_or("no reply")
                    ),
                    "Reviews/tasks that depend on the Arbiter will fail the same way.",
                    "See the detail above for the underlying agent/model error, and fix the Arbiter's configuration or credentials.",
                )
            }
        }
        Err(e) => CheckResult::new(
            "arbiter",
            FAIL,
            format!("could not reach daemon to check the Arbiter: {e}"),
            "Reviews/tasks that depend on the Arbiter cannot be validated.",
            "Ensure the daemon is reachable, then re-run with --enable-live-agent-check.",
        ),
    }
}

fn check_cargo() -> CheckResult {
    match which("cargo") {
        None => CheckResult::machine(
            "cargo",
            FAIL,
            "cargo not found on PATH",
            "The Rust binaries in this workspace cannot be built from source.",
            "Install it via https://rustup.rs.",
        ),
        Some(path) => CheckResult::machine(
            "cargo",
            PASS,
            path,
            "The Rust binaries in this workspace can be built from source.",
            "No action needed.",
        ),
    }
}

/// `--all-remotes`: every configured `[machine.targets.*]` entry's health,
/// via `GET /api/machines/targets/health` (RAL-355 Phase 9). The CLI never
/// opens its own SSH connections -- it only ever asks the daemon, which is
/// the one process with the provider programs/credentials to do the actual
/// checking.
fn check_remote_targets(daemon_url: &str) -> Vec<CheckResult> {
    let client = DaemonClient::new(daemon_url);
    let response = match client.health_remote_targets() {
        Ok(response) => response,
        Err(e) => {
            return vec![CheckResult::machine(
                "remote-targets",
                FAIL,
                format!("could not reach daemon to check remote targets: {e}"),
                "Remote-machine cell routing cannot be validated.",
                "Ensure the daemon is reachable, then re-run with --all-remotes.",
            )];
        }
    };
    let targets = response["targets"].as_array().cloned().unwrap_or_default();
    if targets.is_empty() {
        return vec![CheckResult::machine(
            "remote-targets",
            PASS,
            "no [machine.targets.*] configured",
            "All work runs locally; no remote machine routing is configured.",
            "No action needed.",
        )];
    }
    targets
        .iter()
        .flat_map(|t| {
            let target_name = t["target"].as_str().unwrap_or("?");
            let machine = t["machine"].as_str().unwrap_or("?");
            t["checks"]
                .as_array()
                .cloned()
                .unwrap_or_default()
                .into_iter()
                .map(move |c| {
                    let status = match c["status"].as_str() {
                        Some("pass") => PASS,
                        Some("warn") => WARN,
                        _ => FAIL,
                    };
                    let check_name = c["name"].as_str().unwrap_or("?");
                    CheckResult::machine(
                        &format!("{target_name}:{check_name}"),
                        status,
                        format!("[{machine}] {}", c["detail"].as_str().unwrap_or_default()),
                        "A broken remote target fails any cell routed to it.",
                        "See the detail above and this target's machine-provider configuration.",
                    )
                    .with_id(remote_target_catalog_id(check_name))
                })
        })
        .collect()
}

/// Maps a daemon-reported remote-target sub-check name
/// (`daemon::health_targets::TargetCheck::name`) to its stable
/// [`ralphus_core::health_catalog`] entry id. `"panic"` (the defensive
/// fallback `daemon::health_targets::check_targets` reports if a check
/// thread itself panics) is treated as the same connectivity-class failure
/// as an unreachable SSH connection, since neither tells you anything more
/// specific than "this target could not be checked."
fn remote_target_catalog_id(check_name: &str) -> &'static str {
    use ralphus_core::health_catalog::*;
    match check_name {
        "resolve" => ID_REMOTE_RESOLVE,
        "ssh_reachable" | "panic" => ID_REMOTE_SSH_REACHABLE,
        "capabilities" => ID_REMOTE_CAPABILITIES,
        "remote_root" => ID_REMOTE_ROOT,
        "git_version" => ID_REMOTE_GIT_VERSION,
        "git_user.name" => ID_REMOTE_GIT_IDENTITY_NAME,
        "git_user.email" => ID_REMOTE_GIT_IDENTITY_EMAIL,
        "push_credentials" => ID_REMOTE_PUSH_CREDENTIALS,
        "runner" => ID_REMOTE_RUNNER,
        _ => "",
    }
}

/// Runs the core health checks; adds the developer/machine-toolchain check
/// when opted in (`--enable-developer-checks`), the remote-target inventory
/// when opted in (`--all-remotes`), and the live Arbiter round-trip when
/// opted in (`--enable-live-agent-check`, RAL-415 -- every other check here
/// is reachability/config-shape only and always runs).
#[must_use]
pub fn run_checks(
    daemon_url: &str,
    cwd: &Path,
    enable_developer_checks: bool,
    enable_remote_checks: bool,
    enable_live_agent_checks: bool,
) -> Vec<CheckResult> {
    use ralphus_core::health_catalog::*;

    let mut results: Vec<CheckResult> = check_daemon(daemon_url)
        .into_iter()
        .map(|r| r.with_id(ID_DAEMON))
        .collect();
    results.push(check_config_sources_cli(cwd).with_id(ID_CONFIG_SOURCES));
    results.push(check_config_sources_project(cwd).with_id(ID_CONFIG_SOURCES_PROJECT));

    let projects = check_projects(daemon_url);
    let git_required = !projects.any_project_registered || projects.any_git_project;
    results.push(check_git(git_required).with_id(ID_GIT));
    results.extend(projects.results);
    results.extend(
        check_project_forks(daemon_url)
            .into_iter()
            .map(|r| r.with_id(ID_PROJECT_FORK)),
    );

    results.push(check_runner().with_id(ID_RUNNER));
    results.push(check_ollama().with_id(ID_OLLAMA));
    results.push(check_tmux().with_id(ID_TMUX));
    results.push(check_gh().with_id(ID_GH));
    results.push(check_glab().with_id(ID_GLAB));
    results.push(
        check_agent_command_via_daemon(daemon_url, ID_CLAUDE_COMMAND, "claude-command")
            .with_id(ID_CLAUDE_COMMAND),
    );
    results.push(
        check_agent_command_via_daemon(daemon_url, ID_CODEX_COMMAND, "codex-command")
            .with_id(ID_CODEX_COMMAND),
    );
    results.push(
        check_agent_command_via_daemon(daemon_url, ID_PI_COMMAND, "pi-command")
            .with_id(ID_PI_COMMAND),
    );

    results.push(check_config(cwd).with_id(ID_CONFIG));
    results.push(check_max_concurrent(cwd).with_id(ID_DAEMON_MAX_CONCURRENT));
    results.push(check_opentelemetry(cwd));
    results.push(check_tool_arg_truncate_chars(cwd).with_id(ID_TOOL_ARG_TRUNCATE_CHARS));
    results.push(check_thrash_max_compactions(cwd).with_id(ID_THRASH_MAX_COMPACTIONS));
    results.push(check_thrash_min_turn_gap(cwd).with_id(ID_THRASH_MIN_TURN_GAP));
    results
        .push(check_pull_request_branch_convention(cwd).with_id(ID_PULL_REQUEST_BRANCH_CONVENTION));
    results.extend(check_templates());
    results.extend(
        check_agent_profiles(daemon_url, cwd)
            .into_iter()
            .map(|r| r.with_id(ID_AGENT_PROFILES)),
    );
    results.push(check_default_resolver_agent(daemon_url, cwd).with_id(ID_DEFAULT_RESOLVER_AGENT));
    if enable_live_agent_checks {
        results.push(check_arbiter(daemon_url).with_id(ID_ARBITER));
    }

    results.push(check_nvidia_smi().with_id(ID_NVIDIA_SMI));
    if enable_developer_checks {
        results.push(check_cargo().with_id(ID_CARGO));
    }
    if enable_remote_checks {
        results.extend(check_remote_targets(daemon_url));
    }
    results
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

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
        let fail = CheckResult::new("x", FAIL, "boom", "impact", "remediation");
        let warn = CheckResult::new("x", WARN, "meh", "impact", "remediation");
        assert!(fail.is_fail());
        assert!(!warn.is_fail());
    }

    #[test]
    fn check_result_states_observation_impact_and_remediation() {
        let result = CheckResult::new("x", FAIL, "observed thing", "impact text", "fix text");
        assert_eq!(result.detail, "observed thing");
        assert_eq!(result.impact, "impact text");
        assert_eq!(result.remediation, "fix text");
    }

    #[test]
    fn check_project_clone_url_warns_when_missing_and_remote_providers_exist() {
        let result = check_project_clone_url("proj", None, true).expect("should warn");
        assert_eq!(result.status, WARN);
        assert_eq!(result.name, "project:proj:clone-url");
        assert!(result.detail.contains("no clone URL"), "{}", result.detail);
    }

    #[test]
    fn check_project_clone_url_is_silent_without_remote_providers() {
        // No machine provider is registered at all, so there is nothing a
        // missing clone URL would break yet -- not worth warning about.
        assert!(check_project_clone_url("proj", None, false).is_none());
    }

    #[test]
    fn check_project_clone_url_is_silent_once_a_url_is_registered() {
        assert!(
            check_project_clone_url("proj", Some("git@example.invalid:team/proj.git"), true)
                .is_none()
        );
    }

    #[test]
    fn check_agent_command_via_daemon_fails_when_the_daemon_is_unreachable() {
        let result = check_agent_command_via_daemon(
            "http://127.0.0.1:1",
            ralphus_core::health_catalog::ID_CLAUDE_COMMAND,
            "claude-command",
        );
        assert_eq!(result.status, FAIL);
        assert_eq!(result.section, HARNESS);
        assert!(
            result.detail.contains("could not reach daemon"),
            "{}",
            result.detail
        );
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
        assert_eq!(result.provenance.as_deref(), Some("/tmp/.ralphus.toml"));
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

    // ── check_thrash_field_for (RAL-339) ──────────────────────────────────

    #[test]
    fn check_thrash_field_passes_unset() {
        let result = check_thrash_field_for(
            None,
            None,
            None,
            None,
            "max_compactions",
            "thrash-max-compactions",
            3,
        );
        assert_eq!(result.status, PASS);
        assert!(result.detail.contains("unset"));
        assert!(result.detail.contains('3'));
    }

    #[test]
    fn check_thrash_field_passes_with_a_valid_value() {
        let table: toml::Table = "max_compactions = 5".parse().unwrap();
        let path = Path::new("/tmp/.ralphus.toml");
        let result = check_thrash_field_for(
            None,
            None,
            Some(&table),
            Some(path),
            "max_compactions",
            "thrash-max-compactions",
            3,
        );
        assert_eq!(result.status, PASS);
        assert!(result.detail.contains("thrash.max_compactions=5"));
        assert!(result.detail.contains("/tmp/.ralphus.toml"));
    }

    #[test]
    fn check_thrash_field_warns_on_negative_value() {
        let table: toml::Table = "min_turn_gap = -1".parse().unwrap();
        let result = check_thrash_field_for(
            None,
            None,
            Some(&table),
            None,
            "min_turn_gap",
            "thrash-min-turn-gap",
            2,
        );
        assert_eq!(result.status, WARN);
        assert!(result.detail.contains("-1"));
        assert!(result.detail.contains('2'));
    }

    #[test]
    fn check_thrash_field_warns_on_non_numeric_value() {
        let table: toml::Table = "min_turn_gap = \"nope\"".parse().unwrap();
        let result = check_thrash_field_for(
            None,
            None,
            Some(&table),
            None,
            "min_turn_gap",
            "thrash-min-turn-gap",
            2,
        );
        assert_eq!(result.status, WARN);
        assert!(result.detail.contains("not a number"));
    }

    #[test]
    fn check_thrash_field_prefers_project_over_global() {
        let global: toml::Table = "max_compactions = 10".parse().unwrap();
        let project: toml::Table = "max_compactions = 4".parse().unwrap();
        let global_path = Path::new("/global/config.toml");
        let project_path = Path::new("/project/.ralphus.toml");
        let result = check_thrash_field_for(
            Some(&global),
            Some(global_path),
            Some(&project),
            Some(project_path),
            "max_compactions",
            "thrash-max-compactions",
            3,
        );
        assert_eq!(result.status, PASS);
        assert!(result.detail.contains("thrash.max_compactions=4"));
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

    // ── vcs-aware project checks (RAL-415) ────────────────────────────────

    #[test]
    fn check_project_path_skips_git_validation_for_non_git_vcs() {
        let dir = std::env::temp_dir().join(format!(
            "ralphus-health-test-nongit-{}-{}",
            std::process::id(),
            line!()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let result = check_project_path("proj", &dir.to_string_lossy(), "none");
        assert_eq!(result.status, SKIP);
        assert!(result.detail.contains("vcs=\"none\""));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn check_project_path_fails_for_missing_directory_regardless_of_vcs() {
        let result = check_project_path("proj", "/definitely/does/not/exist/ralphus-xyz", "none");
        assert_eq!(result.status, FAIL);
    }

    #[test]
    fn check_project_path_validates_git_repo_for_git_vcs() {
        // The worktree this test runs in is itself a git repository.
        let cwd = std::env::current_dir().unwrap();
        let result = check_project_path("proj", &cwd.to_string_lossy(), "git");
        assert_eq!(result.status, PASS);
    }

    #[test]
    fn check_project_path_fails_non_repo_directory_for_git_vcs() {
        let dir = std::env::temp_dir().join(format!(
            "ralphus-health-test-notrepo-{}-{}",
            std::process::id(),
            line!()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let result = check_project_path("proj", &dir.to_string_lossy(), "git");
        assert_eq!(result.status, FAIL);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn check_git_skips_when_not_required_and_absent() {
        let result = check_git(false);
        if which("git").is_some() {
            assert_eq!(result.status, PASS);
        } else {
            assert_eq!(result.status, SKIP);
        }
    }

    #[test]
    fn check_git_fails_when_required_and_absent() {
        let result = check_git(true);
        if which("git").is_some() {
            assert_eq!(result.status, PASS);
        } else {
            assert_eq!(result.status, FAIL);
        }
    }

    // ── config-sources (RAL-415 layered config provenance) ────────────────

    #[test]
    fn check_config_sources_cli_passes_with_no_sources() {
        let result = check_config_sources_cli_for(&crate::config::Config::default());
        assert_eq!(result.status, PASS);
        assert!(result.detail.contains("no .ralphus.toml"));
    }

    #[test]
    fn check_config_sources_cli_lists_every_source_in_precedence_order() {
        let config = crate::config::Config {
            sources: vec![
                PathBuf::from("/env/a.toml"),
                PathBuf::from("/repo/.ralphus.toml"),
            ],
            source_labels: vec![
                (
                    PathBuf::from("/env/a.toml"),
                    "environment variable".to_string(),
                ),
                (PathBuf::from("/repo/.ralphus.toml"), "local".to_string()),
            ],
            ..crate::config::Config::default()
        };
        let result = check_config_sources_cli_for(&config);
        assert_eq!(result.status, PASS);
        assert!(
            result
                .detail
                .contains("1. /env/a.toml (environment variable)")
        );
        assert!(result.detail.contains("2. /repo/.ralphus.toml (local)"));
        assert_eq!(result.provenance.as_deref(), Some("/repo/.ralphus.toml"));
    }

    #[test]
    fn check_config_sources_project_passes_with_no_files() {
        let result = check_config_sources_project_for(None, None);
        assert_eq!(result.status, PASS);
        assert!(result.detail.contains("no global or per-project"));
    }

    #[test]
    fn check_config_sources_project_lists_global_then_project() {
        let global = Path::new("/global/config.toml");
        let project = Path::new("/repo/.ralphus.toml");
        let result = check_config_sources_project_for(Some(global), Some(project));
        assert_eq!(result.status, PASS);
        assert!(result.detail.contains("1. /global/config.toml (global)"));
        assert!(
            result
                .detail
                .contains("2. /repo/.ralphus.toml (nearest project)")
        );
    }

    // ── optional forge CLI fallbacks (RAL-415) ─────────────────────────────

    #[test]
    fn check_gh_never_fails_when_missing() {
        let result = check_gh_for(None);
        assert_ne!(result.status, FAIL);
        assert!(result.detail.contains("fallback"));
        assert_eq!(result.section, HARNESS);
    }

    #[test]
    fn check_gh_reports_path_when_found() {
        let result = check_gh_for(Some("/usr/bin/gh".to_string()));
        assert_ne!(result.status, FAIL);
        assert!(result.detail.contains("/usr/bin/gh"));
    }

    #[test]
    fn check_glab_never_fails_when_missing() {
        let result = check_glab_for(None);
        assert_ne!(result.status, FAIL);
        assert!(result.detail.contains("fallback"));
        assert_eq!(result.section, HARNESS);
    }

    #[test]
    fn check_glab_reports_path_when_found() {
        let result = check_glab_for(Some("/usr/bin/glab".to_string()));
        assert_ne!(result.status, FAIL);
        assert!(result.detail.contains("/usr/bin/glab"));
    }

    // ── tmux resolution (RAL-415) ───────────────────────────────────────

    #[test]
    fn check_tmux_reports_resolved_value_and_source() {
        let result = check_tmux_for(Ok((
            "tmux".to_string(),
            ralphus_daemon::tmux::TMUX_SOURCE_ENV_OVERRIDE,
        )));
        assert_eq!(result.status, PASS);
        assert!(result.detail.contains("tmux"));
        assert!(
            result
                .detail
                .contains(ralphus_daemon::tmux::TMUX_SOURCE_ENV_OVERRIDE)
        );
        assert_eq!(
            result.provenance.as_deref(),
            Some(ralphus_daemon::tmux::TMUX_SOURCE_ENV_OVERRIDE)
        );
    }

    #[test]
    fn check_tmux_reports_embedded_and_path_sources() {
        let embedded = check_tmux_for(Ok((
            "/tmp/tmux.exe".to_string(),
            ralphus_daemon::tmux::TMUX_SOURCE_EMBEDDED,
        )));
        assert!(
            embedded
                .detail
                .contains(ralphus_daemon::tmux::TMUX_SOURCE_EMBEDDED)
        );

        let on_path = check_tmux_for(Ok((
            "tmux".to_string(),
            ralphus_daemon::tmux::TMUX_SOURCE_PATH,
        )));
        assert!(
            on_path
                .detail
                .contains(ralphus_daemon::tmux::TMUX_SOURCE_PATH)
        );
    }

    #[test]
    fn check_tmux_fails_with_remediation_when_unresolved() {
        let result = check_tmux_for(Err("no tmux-compatible binary found".to_string()));
        assert_eq!(result.status, FAIL);
        assert!(result.remediation.contains("RALPHUS_TMUX_CMD"));
        assert_eq!(result.section, HARNESS);
    }
}
