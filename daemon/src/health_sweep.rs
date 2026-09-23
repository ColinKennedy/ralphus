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

use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use ralphus_core::health_catalog::{
    ID_CLAUDE_COMMAND, ID_CODEX_COMMAND, ID_GH, ID_GIT, ID_GLAB, ID_NVIDIA_SMI, ID_OLLAMA,
    ID_PI_COMMAND, ID_RUNNER, ID_TMUX,
};
use ralphus_core::process::which;
use ralphus_runner::cli_agent_common::{self, BackendCommandHealth};
use ralphus_runner::pi_backend;

use crate::store::Store;
use crate::store_lock::StoreHandle;

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
    /// now" affordance for this daemon's own row (both the Health tab's own
    /// button and, RAL-485, the Agents tab's Refresh/Save/Reset actions, so
    /// every surface stays on the same evaluation). Deliberately the *only*
    /// on-demand re-check this module exposes: it re-runs the same
    /// Free-tier, daemon-local subset the background sweep always runs,
    /// never a remote target (that would be the still-undecided "remote
    /// check-now API shape" -- see RAL-416's own report).
    pub fn refresh_now(&self, store: &StoreHandle) -> SweepReport {
        let report = run_sweep(store);
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

/// RAL-485: this backend's currently-stored `agent_backend_commands`
/// override, if any -- the highest-precedence source in the resolution order
/// a real cell dispatch already uses (`agent_profiles::resolve_agent_for_path_with`):
/// database override, then daemon environment override, then compiled
/// default. A short, read-only store access -- callers hold the lock only
/// long enough to read this, never across the slower probing below.
fn backend_command_override(store: &Store, backend: &str) -> Option<String> {
    store
        .list_agent_backend_commands()
        .unwrap_or_default()
        .into_iter()
        .find(|c| c.backend == backend)
        .map(|c| c.command)
}

/// The three overridable backends' current `agent_backend_commands` rows,
/// snapshotted under one short store lock so the (potentially slow, up to
/// five seconds for Pi's `--version` probe) checks below never run while
/// holding it. Mirrors `agent_profiles::AgentDbSnapshot::load`'s "read once,
/// then release" shape.
struct AgentCommandOverrides {
    claude_code: Option<String>,
    codex: Option<String>,
    pi: Option<String>,
}

impl AgentCommandOverrides {
    fn load(store: &Store) -> Self {
        Self {
            claude_code: backend_command_override(store, "claude-code"),
            codex: backend_command_override(store, "codex"),
            pi: backend_command_override(store, "pi"),
        }
    }
}

/// Resolves one backend's effective command and a human-readable source
/// label, in precedence order: database override (already snapshotted into
/// `db_override`), then `env_var`, then `default_program` -- the identical
/// order a real cell dispatch resolves through
/// (`agent_profiles::resolve_agent_for_path_with`), so this check reports
/// exactly what a real cell would invoke.
fn resolve_effective_command(
    db_override: Option<&str>,
    env_var: &str,
    default_program: &str,
) -> (String, String) {
    if let Some(command) = db_override {
        return (command.to_string(), "database override".to_string());
    }
    match std::env::var(env_var) {
        Ok(command) => (command, format!("${env_var}")),
        Err(_) => (default_program.to_string(), "default".to_string()),
    }
}

/// Diagnoses one already-resolved backend command (RAL-485): a compound
/// (shell-routed) command is reported `skip` and displayed as-is, never
/// executed; a direct command is checked for disk/PATH accessibility and
/// executability, with Pi additionally version-checked against the
/// supported minimum. `pub(crate)` and taking an already-resolved command
/// (not a backend id/env var) so `agent_profiles::check_db_profiles_health`
/// can reuse the identical evaluation for a database-stored backend command
/// override -- previously a second, divergent implementation there took the
/// first whitespace-delimited token of the command and resolved only that,
/// silently mis-evaluating any command with arguments.
pub(crate) fn diagnose_backend_command(backend: &str, command: &str) -> BackendCommandHealth {
    if backend == "pi" {
        pi_backend::diagnose_pi_command(command)
    } else {
        cli_agent_common::diagnose_command(command)
    }
}

fn check_backend_command(
    id: &'static str,
    backend: &str,
    db_override: Option<&str>,
    env_var: &str,
    default_program: &str,
) -> SweepCheck {
    let (command, source) = resolve_effective_command(db_override, env_var, default_program);
    let health = diagnose_backend_command(backend, &command);
    SweepCheck {
        id,
        status: health.status,
        detail: format!("{} (source: {source}, command: {command})", health.detail),
    }
}

/// Runs every check in [`SWEEP_CATALOG_IDS`] and returns the resulting
/// report. Takes a [`StoreHandle`] only to snapshot the current
/// `agent_backend_commands` overrides under one short lock
/// ([`AgentCommandOverrides::load`]) -- released well before any of the
/// slower external probing below (a Pi `--version` probe alone can take up
/// to five seconds), so this never holds the store lock across a live
/// subprocess/network check the way a naive `store.lock()`-for-the-whole-call
/// would.
#[must_use]
pub fn run_sweep(store: &StoreHandle) -> SweepReport {
    let overrides = AgentCommandOverrides::load(&store.lock());
    let checks = vec![
        check_git(),
        check_tmux(),
        check_runner(),
        check_gh(),
        check_glab(),
        check_nvidia_smi(),
        check_ollama(),
        check_backend_command(
            ID_CLAUDE_COMMAND,
            "claude-code",
            overrides.claude_code.as_deref(),
            "RALPHUS_CLAUDE_COMMAND",
            "claude",
        ),
        check_backend_command(
            ID_CODEX_COMMAND,
            "codex",
            overrides.codex.as_deref(),
            "RALPHUS_CODEX_COMMAND",
            "codex",
        ),
        check_backend_command(
            ID_PI_COMMAND,
            "pi",
            overrides.pi.as_deref(),
            "RALPHUS_PI_COMMAND",
            "pi",
        ),
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
pub fn spawn_health_sweep(state: HealthSweepState, store: StoreHandle) {
    state.set(run_sweep(&store));
    std::thread::spawn(move || {
        loop {
            let cfg = crate::config::load_health_sweep_config();
            std::thread::sleep(cfg.poll_interval());
            if !cfg.enabled() {
                continue;
            }
            state.set(run_sweep(&store));
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use ralphus_core::health_catalog;

    fn test_store_handle() -> StoreHandle {
        std::sync::Arc::new(crate::store_lock::StoreMutex::new(
            Store::open_in_memory().expect("open in-memory store"),
        ))
    }

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
        let report = run_sweep(&test_store_handle());
        let ids: Vec<&str> = report.checks.iter().map(|c| c.id).collect();
        for id in SWEEP_CATALOG_IDS {
            assert!(ids.contains(id), "missing check for {id}");
        }
    }

    // ── RAL-485: precedence, skip semantics, and the DB-override wiring ────

    #[test]
    fn compound_agent_commands_are_reported_as_skip_without_execution() {
        let result = check_backend_command(
            ID_CLAUDE_COMMAND,
            "claude-code",
            None,
            "RALPHUS_TEST_MISSING_AGENT_COMMAND_RAL485",
            "wrapper claude",
        );
        assert_eq!(result.status, "skip");
        assert!(result.detail.contains("wrapper claude"), "{result:?}");
    }

    #[test]
    fn database_override_takes_precedence_over_env_and_default() {
        let (command, source) = resolve_effective_command(
            Some("db-claude"),
            "RALPHUS_TEST_ENV_RAL485_PRECEDENCE",
            "claude",
        );
        assert_eq!(command, "db-claude");
        assert_eq!(source, "database override");
    }

    #[test]
    fn default_program_is_used_when_neither_database_nor_env_override_is_set() {
        let (command, source) = resolve_effective_command(
            None,
            "RALPHUS_TEST_ENV_RAL485_ALMOST_CERTAINLY_UNSET",
            "claude",
        );
        assert_eq!(command, "claude");
        assert_eq!(source, "default");
    }

    #[test]
    fn database_override_reaches_the_check_via_agent_command_overrides() {
        let store = Store::open_in_memory().expect("open in-memory store");
        store
            .set_agent_backend_command("claude-code", "rez-env foo -- claude")
            .expect("set backend command override");
        let overrides = AgentCommandOverrides::load(&store);
        assert_eq!(
            overrides.claude_code.as_deref(),
            Some("rez-env foo -- claude")
        );
        let result = check_backend_command(
            ID_CLAUDE_COMMAND,
            "claude-code",
            overrides.claude_code.as_deref(),
            "RALPHUS_CLAUDE_COMMAND",
            "claude",
        );
        // A compound database override is skipped, never executed -- and the
        // effective command shown is the override, not the env/default.
        assert_eq!(result.status, "skip");
        assert!(
            result.detail.contains("rez-env foo -- claude"),
            "{result:?}"
        );
        assert!(result.detail.contains("database override"), "{result:?}");
    }

    #[test]
    fn diagnose_backend_command_routes_pi_through_the_pi_specific_probe() {
        // A nonexistent program: both paths fail, but only the Pi one's
        // failure text can ever mention a version -- this just proves the
        // dispatch reaches `pi_backend::diagnose_pi_command`, not that a
        // real version check ran (no real `pi` binary in a test env).
        let health = diagnose_backend_command("pi", "definitely-not-a-real-pi-ral485");
        assert_eq!(health.status, "fail");
        assert_eq!(health.effective_command, "definitely-not-a-real-pi-ral485");
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
