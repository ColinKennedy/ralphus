//! RAL-416: the hourly background sweep that runs every Free-tier,
//! daemon-local check from `ralphus_core::health_catalog` and caches the
//! latest results in memory for the read-only report API
//! (`GET /api/health/report`) to serve without re-probing on every request.
//!
//! Deliberately scoped to checks whose probe is self-contained inside the
//! daemon process: binary-on-`PATH`/reachability checks (`git`, `tmux`/
//! psmux, the runner binary, `gh`/`glab`, ripgrep, `nvidia-smi`, Ollama). The CLI's
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
    ID_CLAUDE_CODE_LOGIN, ID_CLAUDE_COMMAND, ID_CODEX_COMMAND, ID_CODEX_LOGIN, ID_GH, ID_GIT,
    ID_GLAB, ID_NVIDIA_SMI, ID_OLLAMA, ID_PI_COMMAND, ID_RG, ID_RUNNER, ID_TMUX,
};
use ralphus_core::process::which;
use ralphus_runner::cli_agent_common::{self, BackendCommandHealth};
use ralphus_runner::login_probe::{self, LoginProbe, LoginState, StatusRun, login_probes};
use ralphus_runner::pi_backend;
use ralphus_runner::version_probe::{self, VersionProbe};

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
    ID_RG,
    ID_NVIDIA_SMI,
    ID_OLLAMA,
    ID_CLAUDE_COMMAND,
    ID_CODEX_COMMAND,
    ID_PI_COMMAND,
    ID_CLAUDE_CODE_LOGIN,
    ID_CODEX_LOGIN,
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

/// RAL-546: git PATH resolution plus `git --version`, via the shared
/// [`version_probe`] helper. `fail`s on `NotFound` only -- git is
/// load-bearing, same as the pre-RAL-546 PATH-only check -- but a resolved
/// git whose version probe itself fails still `pass`es, since the binary is
/// present and usable even if `--version` parsing broke.
fn check_git() -> SweepCheck {
    let probe = version_probe::probe_version(
        "git",
        &["--version"],
        version_probe::DEFAULT_VERSION_PROBE_TIMEOUT,
        |out| version_probe::first_token_after_prefix(out, "git version "),
    );
    SweepCheck {
        id: ID_GIT,
        status: if matches!(probe, VersionProbe::NotFound) {
            FAIL
        } else {
            PASS
        },
        detail: probe.detail("git"),
    }
}

/// RAL-546: resolves the tmux-compatible binary the same way a real cell
/// dispatch would (`resolve_tmux_program_with_source`), then additionally
/// probes its version with `-V` -- real tmux and the vendored psmux both
/// self-report their first line as `"tmux <version>"`. Skips the version
/// probe for a compound `RALPHUS_TMUX_CMD` override (shell syntax, not a
/// directly-invokable program) the same way [`cli_agent_common::diagnose_command`]
/// skips a compound agent command; the embedded-psmux and PATH cases are
/// always a single invokable program.
fn check_tmux() -> SweepCheck {
    match crate::tmux::resolve_tmux_program_with_source() {
        Ok((program, source)) => {
            if cli_agent_common::is_compound_command(&program) {
                return SweepCheck {
                    id: ID_TMUX,
                    status: PASS,
                    detail: format!("{program} (source: {source})"),
                };
            }
            let probe = version_probe::probe_version_at(
                &program,
                &["-V"],
                version_probe::DEFAULT_VERSION_PROBE_TIMEOUT,
                |out| version_probe::first_token_after_prefix(out, "tmux "),
            );
            let detail = if let VersionProbe::Ok { version, .. } = &probe {
                format!("{program} (source: {source}, version {version})")
            } else {
                format!(
                    "{program} (source: {source}); version probe: {}",
                    probe.detail(&program)
                )
            };
            SweepCheck {
                id: ID_TMUX,
                status: PASS,
                detail,
            }
        }
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

/// RAL-546: `gh` PATH resolution plus `gh --version`, via the shared
/// [`version_probe`] helper. Always `pass`es, found or not -- `gh` is
/// optional, the same "never a hard fail" rule ripgrep's checks follow.
fn check_gh() -> SweepCheck {
    let probe = version_probe::probe_version(
        "gh",
        &["--version"],
        version_probe::DEFAULT_VERSION_PROBE_TIMEOUT,
        |out| version_probe::first_token_after_prefix(out, "gh version "),
    );
    SweepCheck {
        id: ID_GH,
        status: PASS,
        detail: probe.detail("gh"),
    }
}

/// RAL-546: `glab` PATH resolution plus `glab --version`, via the shared
/// [`version_probe`] helper. Always `pass`es, found or not -- `glab` is
/// optional, the same "never a hard fail" rule ripgrep's checks follow.
fn check_glab() -> SweepCheck {
    let probe = version_probe::probe_version(
        "glab",
        &["--version"],
        version_probe::DEFAULT_VERSION_PROBE_TIMEOUT,
        |out| version_probe::first_token_after_prefix(out, "glab "),
    );
    SweepCheck {
        id: ID_GLAB,
        status: PASS,
        detail: probe.detail("glab"),
    }
}

/// RAL-522: ripgrep PATH resolution plus `rg --version` on the resolved
/// executable, via the same shared probe the CLI's `check health` runs
/// (`ralphus_runner::ripgrep`) so the two surfaces can never disagree. A
/// `warn`, never a `fail` (agents fall back to `grep`), with a distinct
/// detail per failure shape.
fn check_rg() -> SweepCheck {
    let probe = ralphus_runner::ripgrep::probe_version();
    SweepCheck {
        id: ID_RG,
        status: probe.status(),
        detail: probe.detail(),
    }
}

/// RAL-546: `nvidia-smi` PATH resolution plus `nvidia-smi --version`, via
/// the shared [`version_probe`] helper, so the detail carries the
/// NVIDIA-SMI, driver, and CUDA versions. A `warn` when it's missing or
/// broken, never a `fail`: only the resource view's GPU column needs it.
fn check_nvidia_smi() -> SweepCheck {
    let probe = version_probe::probe_version(
        "nvidia-smi",
        &["--version"],
        version_probe::DEFAULT_VERSION_PROBE_TIMEOUT,
        version_probe::nvidia_smi_version,
    );
    SweepCheck {
        id: ID_NVIDIA_SMI,
        status: probe.status(),
        detail: probe.detail("nvidia-smi"),
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
    /// One entry per [`login_probes`] backend, in the same order.
    login: Vec<Option<String>>,
}

impl AgentCommandOverrides {
    fn load(store: &Store) -> Self {
        Self {
            login: login_probes()
                .iter()
                .map(|probe| backend_command_override(store, probe.backend_name()))
                .collect(),
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
/// supported minimum, and Claude Code/Codex additionally version-probed
/// (RAL-546) via the shared [`version_probe`] helper. `pub(crate)` and
/// taking an already-resolved command (not a backend id/env var) so
/// `agent_profiles::check_db_profiles_health` can reuse the identical
/// evaluation for a database-stored backend command override -- previously
/// a second, divergent implementation there took the first
/// whitespace-delimited token of the command and resolved only that,
/// silently mis-evaluating any command with arguments.
pub(crate) fn diagnose_backend_command(backend: &str, command: &str) -> BackendCommandHealth {
    match backend {
        "pi" => pi_backend::diagnose_pi_command(command),
        // Claude Code's banner has changed shape across releases; accept the
        // first semver-shaped token rather than depending on its wording.
        "claude-code" => {
            cli_agent_common::diagnose_command_with_version(command, &["--version"], |out| {
                version_probe::first_version_token(out)
            })
        }
        // Codex likewise has emitted both prefixed and unprefixed banners.
        "codex" => {
            cli_agent_common::diagnose_command_with_version(command, &["--version"], |out| {
                version_probe::first_version_token(out)
            })
        }
        _ => cli_agent_common::diagnose_command(command),
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

const SKIP: &str = "skip";

/// RAL-571: is the backend's CLI logged in? Resolves the command exactly as
/// [`check_backend_command`] does (database override, env var, default) and
/// runs the backend's own status subcommand against the config dir the
/// runner copies credentials from. A missing binary or a compound command is
/// `skip` (the command check owns reporting those); logged out, an
/// unrecognized status output, or a timeout is `warn`.
fn check_login(probe: &dyn LoginProbe, db_override: Option<&str>) -> SweepCheck {
    let id = probe.health_id();
    let (command, source) = resolve_effective_command(
        db_override,
        probe.command_env_var(),
        probe.default_program(),
    );
    let name = probe.display_name();
    let skip = |detail: String| SweepCheck {
        id,
        status: SKIP,
        detail,
    };
    if cli_agent_common::is_compound_command(&command) {
        return skip(format!(
            "{name} command is a compound command ({command}); login not probed"
        ));
    }
    let path = if std::path::Path::new(&command).exists() {
        Some(command.clone())
    } else {
        which(&command)
    };
    let Some(path) = path else {
        return skip(format!(
            "{command} not found (source: {source}); {name} is not installed"
        ));
    };
    let run = login_probe::run_status_command(
        &path,
        probe.status_args(),
        version_probe::DEFAULT_VERSION_PROBE_TIMEOUT,
    );
    let config = probe
        .config_dir()
        .map_or_else(|| "unknown".to_string(), |dir| dir.display().to_string());
    let provenance = format!("config {config} · via {path} (source: {source})");
    let remedy = probe.login_command();
    match run {
        StatusRun::SpawnFailed(error) => SweepCheck {
            id,
            status: WARN,
            detail: format!("cannot determine login: {error}; {provenance}"),
        },
        StatusRun::TimedOut => SweepCheck {
            id,
            status: WARN,
            detail: format!("status probe timed out; {provenance}"),
        },
        StatusRun::Finished { exit_ok, output } => {
            let env_present =
                |name: &str| std::env::var_os(name).is_some_and(|value| !value.is_empty());
            let status = probe.parse(&output, exit_ok, &env_present);
            match status.state {
                LoginState::LoggedIn => {
                    let caveat = probe
                        .platform_caveat()
                        .map_or_else(String::new, |c| format!(" · {c}"));
                    SweepCheck {
                        id,
                        status: PASS,
                        detail: format!("{} · {provenance}{caveat}", status.summary),
                    }
                }
                LoginState::LoggedOut => SweepCheck {
                    id,
                    status: WARN,
                    detail: format!("{name} is not logged in (run `{remedy}`); {provenance}"),
                },
                LoginState::Unknown => SweepCheck {
                    id,
                    status: WARN,
                    detail: format!("cannot determine login: {}; {provenance}", status.summary),
                },
            }
        }
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
    // allow-lock-io: see the doc comment above -- one short DB read, released
    // before any of the slower external probing below.
    let overrides = AgentCommandOverrides::load(&store.lock());
    let mut checks = vec![
        check_git(),
        check_tmux(),
        check_runner(),
        check_gh(),
        check_glab(),
        check_rg(),
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
    checks.extend(
        login_probes()
            .into_iter()
            .zip(&overrides.login)
            .map(|(probe, db_override)| check_login(probe, db_override.as_deref())),
    );
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
    fn login_check_skips_when_the_binary_is_missing() {
        for probe in login_probes() {
            let result = check_login(probe, Some("ralphus-no-such-binary-ral571"));
            assert_eq!(result.status, "skip", "{result:?}");
            assert_eq!(result.id, probe.health_id());
        }
    }

    #[test]
    fn login_check_skips_compound_commands_without_running_them() {
        let probe = login_probes()[0];
        let result = check_login(probe, Some("wrapper claude"));
        assert_eq!(result.status, "skip", "{result:?}");
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
    fn ripgrep_check_is_a_warn_at_worst_and_names_the_resolved_path() {
        // RAL-522: the ripgrep check is never a hard fail (agents fall back
        // to grep), and when PATH resolution finds an rg, the detail names
        // that resolved executable whatever the invocation outcome was.
        let check = check_rg();
        assert_ne!(check.status, FAIL, "{check:?}");
        match which("rg") {
            Some(path) => assert!(
                check.detail.contains(&path),
                "detail {:?} should name the resolved path {path:?}",
                check.detail
            ),
            None => assert_eq!(check.status, WARN, "{check:?}"),
        }
    }

    // ── RAL-546: version-probed git/tmux/gh/glab and claude-code/codex ─────

    #[test]
    fn git_check_never_fails_when_git_is_actually_on_path() {
        // This test suite itself runs from a git worktree, so git must be on
        // PATH -- if it weren't, the whole dev loop would already be broken.
        let check = check_git();
        assert_eq!(check.status, PASS, "{check:?}");
        assert!(check.detail.contains("version"), "{check:?}");
    }

    #[test]
    fn tmux_check_names_a_source_whatever_the_resolution_outcome() {
        // Can't assume a real tmux/psmux is installed in every test
        // environment. When resolution succeeds the detail must say which
        // source it came from; when it fails (no tmux anywhere) the detail is
        // the resolver's error, which has no single source to name.
        let check = check_tmux();
        if check.status == FAIL {
            assert!(
                check.detail.contains("tmux"),
                "expected the resolver error, got {check:?}"
            );
        } else {
            assert!(
                check.detail.contains("source:"),
                "expected a source label, got {check:?}"
            );
        }
    }

    #[test]
    fn gh_and_glab_checks_never_fail_even_when_not_installed() {
        // RAL-546: gh/glab are optional -- absence is a `pass` with a "not
        // found" detail, never a `fail` that would sink the whole sweep.
        let gh = check_gh();
        assert_eq!(gh.status, PASS, "{gh:?}");
        let glab = check_glab();
        assert_eq!(glab.status, PASS, "{glab:?}");
    }

    #[test]
    fn diagnose_backend_command_routes_claude_code_through_the_version_probe() {
        // No real `claude` binary in a test env, but the dispatch must still
        // reach `diagnose_command_with_version` (not the plain, version-less
        // `diagnose_command`) for this backend id.
        let health = diagnose_backend_command("claude-code", "definitely-not-a-real-claude-ral546");
        assert_eq!(health.status, "fail");
        assert_eq!(
            health.effective_command,
            "definitely-not-a-real-claude-ral546"
        );
        assert_eq!(health.version, None);
    }

    #[test]
    fn diagnose_backend_command_routes_codex_through_the_version_probe() {
        let health = diagnose_backend_command("codex", "definitely-not-a-real-codex-ral546");
        assert_eq!(health.status, "fail");
        assert_eq!(
            health.effective_command,
            "definitely-not-a-real-codex-ral546"
        );
        assert_eq!(health.version, None);
    }

    #[test]
    fn diagnose_backend_command_still_skips_a_compound_claude_code_override_without_probing() {
        // A compound (shell-routed) override must stay `skip`, not get
        // routed into the version probe's direct `Command::new` invocation.
        let health = diagnose_backend_command("claude-code", "rez-env foo -- claude");
        assert_eq!(health.status, "skip");
        assert_eq!(health.effective_command, "rez-env foo -- claude");
        assert_eq!(health.version, None);
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
