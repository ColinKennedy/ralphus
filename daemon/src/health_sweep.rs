//! RAL-416: the hourly background sweep that runs every Free-tier,
//! daemon-local check from `ralphus_core::health_catalog` and caches the
//! latest results in memory for the read-only report API
//! (`GET /api/health/report`) to serve without re-probing on every request.
//!
//! Deliberately scoped to checks whose probe is self-contained inside the
//! daemon process: binary-on-`PATH`/reachability checks (`git`, `tmux`/
//! psmux, the runner binary, `gh`/`glab`, `nvidia-smi`, Ollama). The CLI's
//! own `.ralphus.toml`-shape checks (`config`, `templates`, `thrash-*`,
//! `pull-request-branch-convention`, ...) need the CLI process's own
//! config-loading context (`ralphus_cli::config::load_config`, keyed off
//! *its* cwd) -- `ralphus-daemon` cannot depend on `ralphus-cli` (the
//! dependency runs the other way: `cli` depends on `daemon`), so
//! duplicating that loader here was judged out of scope for RAL-416. Those
//! checks still run on every `ralphus check health` invocation; they are
//! just not part of this background sweep. This is a deliberate, named
//! scope decision -- see RAL-416's own report -- not a silent gap: every
//! catalog entry this sweep does *not* cover is simply absent from its
//! report rather than reported with a fabricated status.
//!
//! [`CostTier::Free`]/[`Applicability::DaemonLocal`] is enforced structurally
//! (this module only ever constructs [`SweepCheck`]s for entries in
//! [`SWEEP_CATALOG_IDS`], each checked against the catalog by this module's
//! own tests) -- never by filtering a dynamically-assembled list, so an
//! `OnDemand` or `Remote` catalog entry can never accidentally end up here.

use std::path::Path;
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use ralphus_core::health_catalog::{
    ID_CLAUDE_COMMAND, ID_CODEX_COMMAND, ID_GH, ID_GIT, ID_GLAB, ID_NVIDIA_SMI, ID_OLLAMA,
    ID_PI_COMMAND, ID_RUNNER, ID_TMUX,
};
use ralphus_core::process::{is_compound_shell_command, is_executable, unquote_path, which};

const PASS: &str = "pass";
const WARN: &str = "warn";
const FAIL: &str = "fail";

/// Every catalog id this sweep actually probes -- see the module doc
/// comment for why this is a strict subset of every `Free`/`DaemonLocal`
/// catalog entry.
const SWEEP_CATALOG_IDS: &[&str] = &[
    ID_GIT,
    ID_TMUX,
    ID_RUNNER,
    ID_GH,
    ID_GLAB,
    ID_NVIDIA_SMI,
    ID_OLLAMA,
    ID_CLAUDE_COMMAND,
    ID_CODEX_COMMAND,
    ID_PI_COMMAND,
];

/// One check's cached outcome. Deliberately smaller than
/// `cli::health::CheckResult` (no `impact`/`remediation`/`provenance`) --
/// this is a machine-status cache meant to be joined against the catalog's
/// own `impact` text by whatever reads it (the report endpoint/board), not
/// a second copy of the CLI's richer per-run prose.
#[derive(Debug, Clone, serde::Serialize)]
pub struct SweepCheck {
    pub id: &'static str,
    pub status: &'static str,
    pub detail: String,
}

/// The daemon's latest completed sweep pass.
#[derive(Debug, Clone, serde::Serialize)]
pub struct SweepReport {
    pub checked_at_ms: u128,
    pub checks: Vec<SweepCheck>,
}

#[derive(Default)]
struct Inner {
    last: Option<SweepReport>,
}

/// Shared, in-memory latest-sweep cache. Purely derived (re-computed by the
/// very next sweep, same "no source of truth to reconcile" reasoning as
/// `Daemon::active_terminal_sessions`), so a daemon restart simply starts
/// with no report until its first sweep completes -- not worth a SQLite
/// migration for a handful of small strings that are cheap to regenerate.
#[derive(Clone, Default)]
pub struct HealthSweepState(Arc<Mutex<Inner>>);

impl HealthSweepState {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The most recent completed sweep, if any has run yet.
    #[must_use]
    pub fn latest(&self) -> Option<SweepReport> {
        self.0
            .lock()
            .expect("health sweep state mutex poisoned")
            .last
            .clone()
    }

    fn set(&self, report: SweepReport) {
        self.0
            .lock()
            .expect("health sweep state mutex poisoned")
            .last = Some(report);
    }

    /// Runs [`run_sweep`] immediately (rather than waiting for the next
    /// scheduled pass) and caches the result -- backs the board's "check
    /// now" affordance for this daemon's own row. Deliberately the *only*
    /// on-demand re-check this module exposes: it re-runs the same
    /// Free-tier, daemon-local subset the background sweep always runs,
    /// never a remote target (that would be the still-undecided "remote
    /// check-now API shape" -- see RAL-416's own report).
    pub fn refresh_now(&self) -> SweepReport {
        let report = run_sweep();
        self.set(report.clone());
        report
    }
}

fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

fn check_git() -> SweepCheck {
    match which("git") {
        Some(path) => SweepCheck {
            id: ID_GIT,
            status: PASS,
            detail: path,
        },
        None => SweepCheck {
            id: ID_GIT,
            status: FAIL,
            detail: "not found on PATH".to_string(),
        },
    }
}

fn check_tmux() -> SweepCheck {
    match crate::tmux::resolve_tmux_program_with_source() {
        Ok((program, source)) => SweepCheck {
            id: ID_TMUX,
            status: PASS,
            detail: format!("{program} (source: {source})"),
        },
        Err(e) => SweepCheck {
            id: ID_TMUX,
            status: FAIL,
            detail: e.to_string(),
        },
    }
}

fn check_runner() -> SweepCheck {
    let cmd = std::env::var("RALPHUS_RUNNER_CMD").unwrap_or_else(|_| "ralphus-runner".to_string());
    let program = cmd
        .split_whitespace()
        .next()
        .unwrap_or("ralphus-runner")
        .to_string();
    if which(&program).is_none() && !std::path::Path::new(&program).exists() {
        return SweepCheck {
            id: ID_RUNNER,
            status: WARN,
            detail: format!("'{program}' not found (set RALPHUS_RUNNER_CMD)"),
        };
    }
    SweepCheck {
        id: ID_RUNNER,
        status: PASS,
        detail: program,
    }
}

fn check_gh() -> SweepCheck {
    SweepCheck {
        id: ID_GH,
        status: PASS,
        detail: which("gh").unwrap_or_else(|| "not found on PATH".to_string()),
    }
}

fn check_glab() -> SweepCheck {
    SweepCheck {
        id: ID_GLAB,
        status: PASS,
        detail: which("glab").unwrap_or_else(|| "not found on PATH".to_string()),
    }
}

fn check_nvidia_smi() -> SweepCheck {
    match which("nvidia-smi") {
        Some(path) => SweepCheck {
            id: ID_NVIDIA_SMI,
            status: PASS,
            detail: path,
        },
        None => SweepCheck {
            id: ID_NVIDIA_SMI,
            status: WARN,
            detail: "not found on PATH".to_string(),
        },
    }
}

fn check_ollama() -> SweepCheck {
    let base = std::env::var("RALPHUS_OLLAMA_URL")
        .unwrap_or_else(|_| "http://localhost:11434/v1".to_string());
    let trimmed = base.trim_end_matches('/');
    let root = trimmed.strip_suffix("/v1").unwrap_or(trimmed);
    let tags_url = format!("{root}/api/tags");
    let reachable = ureq::get(&tags_url)
        .timeout(Duration::from_secs(2))
        .call()
        .is_ok();
    SweepCheck {
        id: ID_OLLAMA,
        status: if reachable { PASS } else { FAIL },
        detail: if reachable {
            base
        } else {
            format!("not reachable at {base}")
        },
    }
}

fn resolved_agent_command(command: &str) -> Option<String> {
    let path = unquote_path(command);
    let resolved = if path.contains(['/', '\\']) {
        path
    } else {
        which(&path)?
    };
    is_executable(Path::new(&resolved)).then_some(resolved)
}

fn check_agent_command(id: &'static str, env_var: &str, default_program: &str) -> SweepCheck {
    let (command, source) = match std::env::var(env_var) {
        Ok(command) => (command, format!("${env_var}")),
        Err(_) => (default_program.to_string(), "default".to_string()),
    };
    if is_compound_shell_command(&command) {
        return SweepCheck {
            id,
            status: PASS,
            detail: format!("compound shell command, not executable-checked: {command}"),
        };
    }
    match resolved_agent_command(&command) {
        Some(path) => SweepCheck {
            id,
            status: PASS,
            detail: format!("{path} ({source})"),
        },
        None => SweepCheck {
            id,
            status: FAIL,
            detail: format!("{source} command '{command}' does not resolve to an executable file"),
        },
    }
}

fn parse_version(output: &str) -> Option<(u64, u64, u64)> {
    output
        .split(|character: char| !character.is_ascii_digit() && character != '.')
        .find_map(|candidate| {
            let mut components = candidate.split('.');
            Some((
                components.next()?.parse().ok()?,
                components.next()?.parse().ok()?,
                components.next()?.parse().ok()?,
            ))
        })
}

#[must_use]
fn supported_pi_version(version: (u64, u64, u64)) -> bool {
    version >= (0, 85, 1)
}

fn check_pi_command() -> SweepCheck {
    let (command, source) = match std::env::var("RALPHUS_PI_COMMAND") {
        Ok(command) => (command, "$RALPHUS_PI_COMMAND".to_string()),
        Err(_) => ("pi".to_string(), "default".to_string()),
    };
    if is_compound_shell_command(&command) {
        return SweepCheck {
            id: ID_PI_COMMAND,
            status: PASS,
            detail: format!(
                "compound shell command, not executable- or version-checked: {command}"
            ),
        };
    }
    let Some(path) = resolved_agent_command(&command) else {
        return SweepCheck {
            id: ID_PI_COMMAND,
            status: FAIL,
            detail: format!("{source} command '{command}' does not resolve to an executable file"),
        };
    };
    match Command::new(&path).arg("--version").output() {
        Err(error) => SweepCheck {
            id: ID_PI_COMMAND,
            status: FAIL,
            detail: format!("could not run {path} --version: {error}"),
        },
        Ok(output) if !output.status.success() => SweepCheck {
            id: ID_PI_COMMAND,
            status: FAIL,
            detail: format!("{path} --version exited with {}", output.status),
        },
        Ok(output) => {
            let text = format!(
                "{}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            match parse_version(&text) {
                Some(version) if supported_pi_version(version) => SweepCheck {
                    id: ID_PI_COMMAND,
                    status: PASS,
                    detail: format!(
                        "{path} version {}.{}.{} ({source})",
                        version.0, version.1, version.2
                    ),
                },
                Some(version) => SweepCheck {
                    id: ID_PI_COMMAND,
                    status: FAIL,
                    detail: format!(
                        "{path} version {}.{}.{} is older than required 0.85.1",
                        version.0, version.1, version.2
                    ),
                },
                None => SweepCheck {
                    id: ID_PI_COMMAND,
                    status: FAIL,
                    detail: format!(
                        "could not parse a Pi version from {path} --version output: {}",
                        text.trim()
                    ),
                },
            }
        }
    }
}

/// Runs every check in [`SWEEP_CATALOG_IDS`] and returns the resulting
/// report -- does not itself touch [`HealthSweepState`], so tests can call
/// this without needing a state handle.
#[must_use]
pub fn run_sweep() -> SweepReport {
    let checks = vec![
        check_git(),
        check_tmux(),
        check_runner(),
        check_gh(),
        check_glab(),
        check_nvidia_smi(),
        check_ollama(),
        check_agent_command(ID_CLAUDE_COMMAND, "RALPHUS_CLAUDE_COMMAND", "claude"),
        check_agent_command(ID_CODEX_COMMAND, "RALPHUS_CODEX_COMMAND", "codex"),
        check_pi_command(),
    ];
    debug_assert_eq!(
        checks.len(),
        SWEEP_CATALOG_IDS.len(),
        "run_sweep's check list and SWEEP_CATALOG_IDS have drifted apart"
    );
    SweepReport {
        checked_at_ms: now_ms(),
        checks,
    }
}

/// Spawn the background loop that runs [`run_sweep`] every
/// `[health].poll_interval_secs` (default one hour) and caches the result
/// into `state`. The interval and on/off switch are read fresh from
/// `crate::config::load_health_sweep_config` at the top of every cycle, same
/// "load config fresh where needed" style as
/// [`crate::pr::spawn_pr_base_drift_poller`]. Runs once at startup too (same
/// rationale as every other startup-plus-interval sweep in
/// `crate::scheduler`), so a freshly (re)started daemon has a report
/// available immediately rather than waiting a full interval.
pub fn spawn_health_sweep(state: HealthSweepState) {
    state.set(run_sweep());
    std::thread::spawn(move || {
        loop {
            let cfg = crate::config::load_health_sweep_config();
            std::thread::sleep(cfg.poll_interval());
            if !cfg.enabled() {
                continue;
            }
            state.set(run_sweep());
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use ralphus_core::health_catalog;

    #[test]
    fn every_swept_id_is_free_and_daemon_local_in_the_catalog() {
        for id in SWEEP_CATALOG_IDS {
            let entry = health_catalog::get(id)
                .unwrap_or_else(|| panic!("{id} is swept but not in the catalog"));
            assert_eq!(
                entry.cost_tier,
                health_catalog::CostTier::Free,
                "{id} is swept but not Free"
            );
            assert_eq!(
                entry.applicability,
                health_catalog::Applicability::DaemonLocal,
                "{id} is swept but not DaemonLocal"
            );
        }
    }

    #[test]
    fn run_sweep_produces_one_check_per_swept_id() {
        let report = run_sweep();
        let ids: Vec<&str> = report.checks.iter().map(|c| c.id).collect();
        for id in SWEEP_CATALOG_IDS {
            assert!(ids.contains(id), "missing check for {id}");
        }
    }

    #[test]
    fn parse_version_accepts_surrounding_pi_output() {
        assert_eq!(parse_version("pi coding agent v0.85.1\n"), Some((0, 85, 1)));
        assert_eq!(parse_version("version 1.2.3-beta"), Some((1, 2, 3)));
        assert_eq!(parse_version("no version here"), None);
    }

    #[test]
    fn pi_version_comparison_requires_0851() {
        assert!(supported_pi_version((0, 85, 1)));
        assert!(supported_pi_version((0, 86, 0)));
        assert!(supported_pi_version((1, 0, 0)));
        assert!(!supported_pi_version((0, 85, 0)));
        assert!(!supported_pi_version((0, 84, 9)));
    }

    #[test]
    fn compound_agent_commands_are_reported_without_execution() {
        let result = check_agent_command(
            ID_CLAUDE_COMMAND,
            "RALPHUS_TEST_MISSING_AGENT_COMMAND_RAL484",
            "wrapper claude",
        );
        assert_eq!(result.status, PASS);
        assert!(result.detail.contains("wrapper claude"));
    }

    #[test]
    fn state_starts_empty_and_reports_the_latest_set_sweep() {
        let state = HealthSweepState::new();
        assert!(state.latest().is_none());
        state.set(SweepReport {
            checked_at_ms: 123,
            checks: vec![SweepCheck {
                id: ID_GIT,
                status: PASS,
                detail: "ok".to_string(),
            }],
        });
        let latest = state.latest().expect("a report was set");
        assert_eq!(latest.checked_at_ms, 123);
        assert_eq!(latest.checks[0].id, ID_GIT);
    }
}
