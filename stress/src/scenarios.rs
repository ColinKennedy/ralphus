//! Scenario orchestration. Every scenario gets its own sandbox and daemon so
//! one scenario's data volume, held sockets or crash never colours the next.

use crate::api::{self, Api, Capabilities, Op, Rng};
use crate::daemon::{Daemon, Sandbox, slash};
use crate::faults::{self, Fault, InjectorReport, Target};
use crate::load::{self, Background, LoadSpec, Mix, Resources, Sampler};
use crate::logs::{LogCursor, LogScan};
use crate::stats::WindowStats;
use serde::Serialize;
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

pub struct Config {
    pub daemon_exe: PathBuf,
    pub work_dir: PathBuf,
    pub user_steps: Vec<usize>,
    pub browsing_steps: Vec<usize>,
    pub step: Duration,
    pub timeout: Duration,
    pub fault_window: Duration,
    pub victim_users: usize,
    pub volume_levels: Vec<(usize, usize)>,
    pub volume_users: usize,
    pub user_pool: usize,
    pub seed_squads: usize,
    pub seed_projects: usize,
    pub profiles: Vec<String>,
    pub faults: Vec<Fault>,
    pub run_ramp: bool,
    pub run_volume: bool,
    pub run_faults: bool,
    pub run_crash: bool,
    pub keep_sandboxes: bool,
}

/// One live daemon plus everything a scenario needs to drive it.
pub struct Env {
    pub daemon: Daemon,
    pub api: Arc<Api>,
    pub cursor: LogCursor,
    pub startup_s: f64,
    pub projects: usize,
    pub shared_project_repo: bool,
    pub notes: Vec<String>,
}

impl Env {
    pub fn target(&self) -> Target {
        Target {
            addr: format!("127.0.0.1:{}", self.daemon.port)
                .parse()
                .expect("loopback address"),
            token: self.api.token.clone(),
            user: self.api.users.first().cloned(),
            noun: self.api.noun.clone(),
        }
    }
}

pub fn setup(cfg: &Config, name: &str, squads: usize, projects: usize) -> Result<Env, String> {
    let root = cfg.work_dir.join(name);
    let sandbox =
        Sandbox::create(&root).map_err(|e| format!("creating sandbox {}: {e}", root.display()))?;
    let cwd = sandbox.cwd_toml();
    let (daemon, startup) = Daemon::start(&cfg.daemon_exe, sandbox)?;
    let cursor = LogCursor::at_end(daemon.sandbox.log_files());
    let api = api::probe(
        &daemon.base(),
        daemon.token(),
        cfg.user_pool,
        &cwd,
        cfg.timeout,
    )?;
    let mut env = Env {
        daemon,
        api: Arc::new(api),
        cursor,
        startup_s: startup.as_secs_f64(),
        projects: 0,
        shared_project_repo: true,
        notes: Vec::new(),
    };
    add_projects(&mut env, projects)?;
    seed_squads(&env.api, squads)?;
    // Seeding traffic is setup, not measurement.
    let _ = env.cursor.scan();
    Ok(env)
}

fn git_init(dir: &std::path::Path) -> Result<(), String> {
    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    let out = Command::new("git")
        .arg("init")
        .arg("-q")
        .arg(dir)
        .output()
        .map_err(|e| format!("running git init: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "git init {}: {}",
            dir.display(),
            String::from_utf8_lossy(&out.stderr)
        ));
    }
    Ok(())
}

/// Register projects up to `target`. Projects must point at a git
/// checkout; one shared repository is tried first and a repository per
/// project is created only if the daemon refuses to share a path.
pub fn add_projects(env: &mut Env, target: usize) -> Result<f64, String> {
    if target <= env.projects {
        return Ok(0.0);
    }
    let t0 = Instant::now();
    let repos = env.daemon.sandbox.repos.clone();
    let shared = repos.join("shared");
    if !shared.exists() {
        git_init(&shared)?;
    }
    let ag = api::agent(env.api.timeout);
    let user = env.api.users.first().map(String::as_str);
    let mut new_cwds = Vec::new();
    for i in env.projects..target {
        let path = if env.shared_project_repo {
            shared.clone()
        } else {
            let p = repos.join(format!("p{i}"));
            git_init(&p)?;
            p
        };
        let body = serde_json::json!({"name": format!("stress-proj-{i}"), "path": slash(&path)})
            .to_string();
        let mut r = env
            .api
            .call(&ag, user, "POST", "/api/projects", Some(&body), false);
        if !r.outcome.is_ok() && env.shared_project_repo && i > 0 {
            env.shared_project_repo = false;
            env.notes.push(format!(
                "daemon refused a second project on a shared repo ({}); using one repo per project",
                r.error.clone().unwrap_or_default()
            ));
            let p = repos.join(format!("p{i}"));
            git_init(&p)?;
            let body = serde_json::json!({"name": format!("stress-proj-{i}"), "path": slash(&p)})
                .to_string();
            r = env
                .api
                .call(&ag, user, "POST", "/api/projects", Some(&body), false);
            new_cwds.push(slash(&p));
        } else {
            new_cwds.push(slash(&path));
        }
        if !r.outcome.is_ok() {
            return Err(format!(
                "registering project {i}: {}",
                r.error.unwrap_or_default()
            ));
        }
    }
    env.projects = target;
    if let Ok(mut c) = env.api.cwds.write() {
        c.retain(|p| p.contains("/repos/"));
        c.extend(new_cwds);
        c.sort();
        c.dedup();
    }
    Ok(t0.elapsed().as_secs_f64())
}

/// Submit (held) squads until `target` exist; returns squads/second.
pub fn seed_squads(api: &Arc<Api>, target: usize) -> Result<f64, String> {
    let have = api.ids.read().map(|v| v.len()).unwrap_or(0);
    if target <= have {
        return Ok(0.0);
    }
    let remaining = Arc::new(AtomicUsize::new(target - have));
    let errors = Arc::new(Mutex::new(Vec::<String>::new()));
    let t0 = Instant::now();
    let mut hs = Vec::new();
    for w in 0..16 {
        let (api, remaining, errors) =
            (Arc::clone(api), Arc::clone(&remaining), Arc::clone(&errors));
        hs.push(thread::spawn(move || {
            let ag = api::agent(api.timeout);
            let mut rng = Rng::new(w as u64 + 99);
            let user = api.user_for(w);
            loop {
                let prev = remaining.fetch_sub(1, Ordering::Relaxed);
                if prev == 0 || prev > usize::MAX / 2 {
                    remaining.store(0, Ordering::Relaxed);
                    return;
                }
                let r = api.submit(&ag, user, &mut rng);
                if !r.outcome.is_ok() {
                    remaining.fetch_add(1, Ordering::Relaxed);
                    if let Ok(mut e) = errors.lock() {
                        e.push(r.error.unwrap_or_else(|| r.outcome.class()));
                        if e.len() > 50 {
                            remaining.store(0, Ordering::Relaxed);
                            return;
                        }
                    }
                }
            }
        }));
    }
    for h in hs {
        let _ = h.join();
    }
    let errs = errors.lock().map(|e| e.clone()).unwrap_or_default();
    if errs.len() > 50 {
        return Err(format!("seeding squads failed repeatedly: {}", errs[0]));
    }
    let made = api
        .ids
        .read()
        .map(|v| v.len())
        .unwrap_or(0)
        .saturating_sub(have);
    Ok(made as f64 / t0.elapsed().as_secs_f64().max(1e-6))
}

/// Daemon health after a scenario: whether `/api/daemon` answers, how fast,
/// and a trimmed copy of what it said (lock-wait/guard-hold/watchdog stats).
#[derive(Serialize, Clone, Debug, Default)]
pub struct Health {
    pub alive: bool,
    pub exit_status: Option<String>,
    pub answered: bool,
    pub latency_ms: Option<f64>,
    pub snapshot: Option<serde_json::Value>,
}

pub fn health(env: &mut Env) -> Health {
    let alive = env.daemon.alive();
    let mut h = Health {
        alive,
        exit_status: if alive {
            None
        } else {
            env.daemon.exit_status()
        },
        ..Health::default()
    };
    if !alive {
        return h;
    }
    let ag = api::agent(Duration::from_secs(10));
    let t0 = Instant::now();
    let r = env.api.call(
        &ag,
        env.api.users.first().map(String::as_str),
        "GET",
        "/api/daemon",
        None,
        true,
    );
    if r.outcome.is_ok() {
        h.answered = true;
        h.latency_ms = Some(t0.elapsed().as_secs_f64() * 1000.0);
        h.snapshot = r
            .body
            .and_then(|b| serde_json::from_str::<serde_json::Value>(&b).ok())
            .map(trim_json);
    }
    h
}

/// Keep top-level keys whose values are small; health payloads can carry
/// long per-thread arrays that would bloat the committed results.
fn trim_json(v: serde_json::Value) -> serde_json::Value {
    match v {
        serde_json::Value::Object(m) => serde_json::Value::Object(
            m.into_iter()
                .filter(|(_, val)| val.to_string().len() <= 4096)
                .collect(),
        ),
        other => other,
    }
}

fn finish_env(cfg: &Config, mut env: Env, had_findings: bool) -> Option<String> {
    env.daemon.kill();
    let root = env.daemon.sandbox.root.clone();
    if had_findings || cfg.keep_sandboxes {
        Some(slash(&root))
    } else {
        // The daemon process has exited; give Windows a moment to release
        // the database file handles before removing the directory.
        thread::sleep(Duration::from_millis(300));
        let _ = std::fs::remove_dir_all(&root);
        None
    }
}

// ---------------------------------------------------------------- profiles

pub fn profile_mix(name: &str, available: &[Op]) -> Option<(Mix, Duration)> {
    use Op::*;
    let read: &[(Op, u32)] = &[
        (Board, 30),
        (SquadGet, 30),
        (Health, 10),
        (Queue, 10),
        (Projects, 10),
        (TaskIndex, 5),
        (Guardians, 5),
    ];
    let write: &[(Op, u32)] = &[(Submit, 50), (EnvSet, 25), (LabelEdit, 20), (Validate, 5)];
    let mixed: &[(Op, u32)] = &[
        (Board, 25),
        (SquadGet, 25),
        (Health, 10),
        (Queue, 8),
        (Projects, 6),
        (TaskIndex, 3),
        (Guardians, 3),
        (Submit, 10),
        (EnvSet, 5),
        (LabelEdit, 5),
    ];
    let (weights, think) = match name {
        "read_heavy" => (read, Duration::ZERO),
        "write_heavy" => (write, Duration::ZERO),
        "mixed_80_20" => (mixed, Duration::ZERO),
        "browsing_users" => (mixed, Duration::from_secs(1)),
        _ => return None,
    };
    let m = Mix::new(name, weights, available);
    if m.is_empty() { None } else { Some((m, think)) }
}

pub const PROFILES: [&str; 4] = ["read_heavy", "write_heavy", "mixed_80_20", "browsing_users"];

// -------------------------------------------------------------------- ramp

#[derive(Serialize, Clone, Debug)]
pub struct Step {
    pub users: usize,
    pub users_started: usize,
    pub users_failed_to_start: usize,
    pub stats: WindowStats,
    pub resources: Resources,
    pub daemon_alive: bool,
}

#[derive(Serialize, Clone, Debug, Default)]
pub struct RampSummary {
    pub peak_ok_rps: f64,
    pub users_at_peak: usize,
    pub p99_ms_at_peak: f64,
    pub error_rate_at_peak: f64,
    pub peak_ok_rps_under_1pct: f64,
    pub users_at_peak_under_1pct: usize,
    /// Highest user count whose step stayed at or under 1% errors.
    pub max_users_under_1pct: usize,
    /// Highest user count (before the first step that broke it) whose step
    /// kept p99 at or under 1 s with at most 1% errors -- capacity at a
    /// usable latency, since errors only appear once requests hit the
    /// client timeout.
    pub max_users_p99_under_1s: usize,
    /// First user count whose step exceeded 1% errors (None: never did).
    pub first_users_over_1pct: Option<usize>,
    pub error_rate_at_first_over: Option<f64>,
    pub max_users_tested: usize,
    pub stop_reason: String,
}

#[derive(Serialize, Clone, Debug)]
pub struct RampResult {
    pub profile: String,
    pub think_ms: u64,
    pub mix: Mix,
    pub startup_s: f64,
    pub steps: Vec<Step>,
    pub summary: RampSummary,
    pub health_after: Health,
    pub logs: LogScan,
    pub sandbox_kept: Option<String>,
    pub notes: Vec<String>,
}

pub fn summarize_ramp(steps: &[Step], stop_reason: String) -> RampSummary {
    let mut s = RampSummary {
        stop_reason,
        ..RampSummary::default()
    };
    let mut slo_broken = false;
    for st in steps {
        if !slo_broken {
            if st.stats.error_rate <= 0.01 && st.stats.latency_ok.p99_ms <= 1000.0 {
                s.max_users_p99_under_1s = st.users;
            } else {
                slo_broken = true;
            }
        }
        s.max_users_tested = s.max_users_tested.max(st.users);
        if st.stats.ok_rps > s.peak_ok_rps {
            s.peak_ok_rps = st.stats.ok_rps;
            s.users_at_peak = st.users;
            s.p99_ms_at_peak = st.stats.latency_ok.p99_ms;
            s.error_rate_at_peak = st.stats.error_rate;
        }
        if st.stats.error_rate <= 0.01 {
            if st.stats.ok_rps > s.peak_ok_rps_under_1pct {
                s.peak_ok_rps_under_1pct = st.stats.ok_rps;
                s.users_at_peak_under_1pct = st.users;
            }
            if s.first_users_over_1pct.is_none() {
                s.max_users_under_1pct = st.users;
            }
        } else if s.first_users_over_1pct.is_none() {
            s.first_users_over_1pct = Some(st.users);
            s.error_rate_at_first_over = Some(st.stats.error_rate);
        }
    }
    s
}

pub fn ramp(cfg: &Config, profile: &str, log: &dyn Fn(&str)) -> Result<RampResult, String> {
    let mut env = setup(
        cfg,
        &format!("ramp-{profile}"),
        cfg.seed_squads,
        cfg.seed_projects,
    )?;
    let (mix, think) = profile_mix(profile, &env.api.available)
        .ok_or_else(|| format!("profile {profile} has no supported ops"))?;
    let mix = Arc::new(mix);
    let steps_cfg = if think.is_zero() {
        &cfg.user_steps
    } else {
        &cfg.browsing_steps
    };
    let mut steps = Vec::new();
    let mut over = 0;
    let mut stop_reason = "completed every configured step".to_string();
    let stop = Arc::new(AtomicBool::new(false));
    for &users in steps_cfg {
        let sampler = Sampler::start(env.daemon.pid);
        let spec = LoadSpec {
            users,
            duration: cfg.step,
            think,
            user_offset: 0,
        };
        let run = load::run(&env.api, &mix, &spec, &stop);
        let resources = sampler.finish();
        let stats = run.stats(cfg.step);
        let alive = env.daemon.alive();
        log(&format!(
            "  [{profile}] users={users:>5} ok_rps={:>8.1} err={:>6.2}% p50={:>7.1}ms p99={:>8.1}ms max={:>8.0}ms stall={:>5.1}s daemon_cpu={:>5.0}% rss={:>6.0}MB",
            stats.ok_rps,
            stats.error_rate * 100.0,
            stats.latency_ok.p50_ms,
            stats.latency_ok.p99_ms,
            stats.latency_ok.max_ms,
            stats.longest_silence_ms / 1000.0,
            resources.daemon_avg_cpu_pct,
            resources.daemon_peak_rss_mb
        ));
        let err = stats.error_rate;
        steps.push(Step {
            users,
            users_started: run.users_started,
            users_failed_to_start: run.users_failed_to_start,
            stats,
            resources,
            daemon_alive: alive,
        });
        if !alive {
            stop_reason = format!("daemon died at {users} users");
            break;
        }
        if err > 0.5 {
            stop_reason = format!("error rate {:.1}% at {users} users", err * 100.0);
            break;
        }
        if err > 0.01 {
            over += 1;
            if over >= 2 {
                stop_reason =
                    format!("error rate above 1% for two consecutive steps (last: {users} users)");
                break;
            }
        } else {
            over = 0;
        }
        // Let closed sockets drain between steps.
        thread::sleep(Duration::from_secs(1));
    }
    let summary = summarize_ramp(&steps, stop_reason);
    let health_after = health(&mut env);
    let logs = env.cursor.scan();
    let findings = !logs.is_clean() || !health_after.answered;
    let notes = env.notes.clone();
    let startup_s = env.startup_s;
    let kept = finish_env(cfg, env, findings);
    Ok(RampResult {
        profile: profile.to_string(),
        think_ms: think.as_millis() as u64,
        mix: (*mix).clone(),
        startup_s,
        steps,
        summary,
        health_after,
        logs,
        sandbox_kept: kept,
        notes,
    })
}

// ------------------------------------------------------------------ volume

#[derive(Serialize, Clone, Debug)]
pub struct VolumeLevel {
    pub squads: usize,
    pub projects: usize,
    pub seed_squads_per_s: f64,
    pub seed_projects_s: f64,
    pub board_bytes: u64,
    pub board_single_ms_median: f64,
    pub read: WindowStats,
    pub mixed: WindowStats,
    pub resources: Resources,
}

#[derive(Serialize, Clone, Debug)]
pub struct VolumeResult {
    pub users: usize,
    pub levels: Vec<VolumeLevel>,
    pub health_after: Health,
    pub logs: LogScan,
    pub sandbox_kept: Option<String>,
    pub notes: Vec<String>,
    pub error: Option<String>,
}

pub fn volume(cfg: &Config, log: &dyn Fn(&str)) -> Result<VolumeResult, String> {
    let mut env = setup(cfg, "volume", 0, 0)?;
    let stop = Arc::new(AtomicBool::new(false));
    let read_mix = Arc::new(
        profile_mix("read_heavy", &env.api.available)
            .ok_or("no read ops")?
            .0,
    );
    let mixed_mix = Arc::new(
        profile_mix("mixed_80_20", &env.api.available)
            .ok_or("no mixed ops")?
            .0,
    );
    let mut levels = Vec::new();
    let mut error = None;
    for &(squads, projects) in &cfg.volume_levels {
        let seed_projects_s = match add_projects(&mut env, projects) {
            Ok(s) => s,
            Err(e) => {
                error = Some(e);
                break;
            }
        };
        let seed_rate = match seed_squads(&env.api, squads) {
            Ok(r) => r,
            Err(e) => {
                error = Some(e);
                break;
            }
        };
        // Single-request board cost at this volume.
        let ag = api::agent(cfg.timeout);
        let user = env.api.users.first().map(String::as_str);
        let mut times = Vec::new();
        let mut bytes = 0;
        for _ in 0..5 {
            let t0 = Instant::now();
            let r = env.api.call(&ag, user, "GET", "/api/tasks", None, false);
            times.push(t0.elapsed().as_micros() as u64);
            bytes = r.bytes;
        }
        times.sort_unstable();
        let sampler = Sampler::start(env.daemon.pid);
        let spec = |_: ()| LoadSpec {
            users: cfg.volume_users,
            duration: cfg.step,
            think: Duration::ZERO,
            user_offset: 0,
        };
        let read = load::run(&env.api, &read_mix, &spec(()), &stop).stats(cfg.step);
        let mixed = load::run(&env.api, &mixed_mix, &spec(()), &stop).stats(cfg.step);
        let resources = sampler.finish();
        let actual_squads = env.api.ids.read().map(|v| v.len()).unwrap_or(squads);
        log(&format!(
            "  [volume] squads={actual_squads:>6} projects={projects:>4} board={:>7.1}KB single={:>7.1}ms | read ok_rps={:>7.1} err={:.2}% p99={:.1}ms | mixed ok_rps={:>7.1} err={:.2}% p99={:.1}ms",
            bytes as f64 / 1024.0,
            times[2] as f64 / 1000.0,
            read.ok_rps,
            read.error_rate * 100.0,
            read.latency_ok.p99_ms,
            mixed.ok_rps,
            mixed.error_rate * 100.0,
            mixed.latency_ok.p99_ms
        ));
        levels.push(VolumeLevel {
            squads: actual_squads,
            projects,
            seed_squads_per_s: seed_rate,
            seed_projects_s,
            board_bytes: bytes,
            board_single_ms_median: times[2] as f64 / 1000.0,
            read,
            mixed,
            resources,
        });
        if !env.daemon.alive() {
            error = Some("daemon died during the volume scenario".into());
            break;
        }
    }
    let health_after = health(&mut env);
    let logs = env.cursor.scan();
    let notes = env.notes.clone();
    let findings = !logs.is_clean() || !health_after.answered || error.is_some();
    let kept = finish_env(cfg, env, findings);
    Ok(VolumeResult {
        users: cfg.volume_users,
        levels,
        health_after,
        logs,
        sandbox_kept: kept,
        notes,
        error,
    })
}

// ------------------------------------------------------------------ faults

#[derive(Serialize, Clone, Debug)]
pub struct FaultResult {
    pub fault: String,
    pub description: String,
    pub victim_users: usize,
    pub victim_during: WindowStats,
    pub victim_after: WindowStats,
    pub injector: InjectorReport,
    pub resources: Resources,
    pub health_after: Health,
    pub logs: LogScan,
    pub verdict: String,
    pub reasons: Vec<String>,
    pub sandbox_kept: Option<String>,
}

fn victim_spec(cfg: &Config, d: Duration) -> LoadSpec {
    LoadSpec {
        users: cfg.victim_users,
        duration: d,
        think: Duration::ZERO,
        user_offset: 0,
    }
}

/// `None` runs the no-fault baseline.
pub fn fault(
    cfg: &Config,
    fault: Option<Fault>,
    baseline: Option<&FaultResult>,
) -> Result<FaultResult, String> {
    let name = fault.map(Fault::name).unwrap_or("baseline");
    let mut env = setup(
        cfg,
        &format!("fault-{name}"),
        cfg.seed_squads,
        cfg.seed_projects.min(3),
    )?;
    let mix = Arc::new(
        profile_mix("mixed_80_20", &env.api.available)
            .ok_or("no ops")?
            .0,
    );
    let sampler = Sampler::start(env.daemon.pid);
    let victim = Background::start(&env.api, &mix, victim_spec(cfg, cfg.fault_window));
    let injector = match fault {
        Some(Fault::Malformed) => {
            thread::sleep(Duration::from_secs(1));
            let stop = Arc::new(AtomicBool::new(false));
            let window = cfg.fault_window.saturating_sub(Duration::from_secs(2));
            let target = env.target();
            let daemon = &mut env.daemon;
            let mut check = |payload: &str| -> Option<String> {
                // Give an abort a moment to take the process down.
                thread::sleep(Duration::from_millis(150));
                if daemon.alive() {
                    return None;
                }
                let status = daemon.exit_status().unwrap_or_default();
                let tail = daemon.stderr_tail(6);
                let restart = match daemon.restart() {
                    Ok(d) => format!("restarted in {:.1}s", d.as_secs_f64()),
                    Err(e) => format!("restart failed: {e}"),
                };
                Some(format!(
                    "payload {payload:?} killed the daemon ({status}); stderr tail: {tail}; {restart}"
                ))
            };
            faults::malformed(&target, window, &stop, &mut check)
        }
        Some(f) => {
            thread::sleep(Duration::from_secs(1));
            let stop = Arc::new(AtomicBool::new(false));
            let window = cfg.fault_window.saturating_sub(Duration::from_secs(2));
            faults::inject(f, &env.target(), window, &stop)
        }
        None => InjectorReport::default(),
    };
    let during_run = victim.join().ok_or("victim load thread panicked")?;
    let resources = sampler.finish();
    let during = during_run.stats(cfg.fault_window);
    // Recovery: a short clean window right after the fault ends.
    let stop = Arc::new(AtomicBool::new(false));
    let after_window = Duration::from_secs(3);
    let after = if env.daemon.alive() {
        load::run(&env.api, &mix, &victim_spec(cfg, after_window), &stop).stats(after_window)
    } else {
        WindowStats::default()
    };
    let health_after = health(&mut env);
    let logs = env.cursor.scan();

    let mut reasons = Vec::new();
    let mut verdict = "pass";
    if !health_after.alive {
        verdict = "fail";
        reasons.push(format!(
            "daemon exited ({})",
            health_after.exit_status.clone().unwrap_or_default()
        ));
    } else if !health_after.answered {
        verdict = "fail";
        reasons.push("daemon alive but /api/daemon did not answer within 10s".into());
    }
    if after.requests > 0 && after.error_rate > 0.01 {
        verdict = "fail";
        reasons.push(format!(
            "did not recover: {:.1}% errors in the 3s after the fault",
            after.error_rate * 100.0
        ));
    }
    let mut degrade = |why: String| {
        if verdict == "pass" {
            verdict = "degraded";
        }
        reasons.push(why);
    };
    if during.error_rate > 0.01 {
        degrade(format!(
            "victim error rate {:.2}% during the fault",
            during.error_rate * 100.0
        ));
    }
    if let Some(b) = baseline {
        let base_p99 = b.victim_during.latency_ok.p99_ms;
        let limit = (base_p99 * 5.0).max(base_p99 + 500.0);
        if during.latency_ok.p99_ms > limit {
            degrade(format!(
                "victim p99 {:.0}ms vs baseline {:.0}ms",
                during.latency_ok.p99_ms, base_p99
            ));
        }
        if b.victim_during.ok_rps > 0.0 && during.ok_rps < b.victim_during.ok_rps * 0.5 {
            degrade(format!(
                "victim throughput {:.0} ok/s vs baseline {:.0} ok/s",
                during.ok_rps, b.victim_during.ok_rps
            ));
        }
    }
    for (op, s) in &during.per_op {
        if s.error_rate > 0.05 && s.requests >= 5 {
            degrade(format!(
                "op {op}: {:.1}% errors during the fault",
                s.error_rate * 100.0
            ));
        }
    }
    if !logs.panics.is_empty() {
        degrade(format!(
            "{} distinct panic site(s) in daemon output",
            logs.panics.len()
        ));
    }
    let mut killers = Vec::new();
    for n in &injector.notes {
        if n.contains("killed the daemon") {
            killers.push(n.clone());
        } else if n.contains("server error") || n.contains("no response within") {
            degrade(n.clone());
        }
    }
    if !killers.is_empty() {
        verdict = "fail";
        reasons.extend(killers);
    }
    if let Some(b) = baseline {
        let limit = (b.victim_during.longest_silence_ms * 3.0).max(2000.0);
        if during.longest_silence_ms > limit {
            // Five seconds with nothing completing is the whole server
            // frozen, not merely slow.
            if during.longest_silence_ms >= 5000.0 {
                verdict = "fail";
            } else if verdict == "pass" {
                verdict = "degraded";
            }
            reasons.push(format!(
                "server stalled: no victim request completed for {:.1}s (baseline longest gap {:.1}s)",
                during.longest_silence_ms / 1000.0,
                b.victim_during.longest_silence_ms / 1000.0
            ));
        }
    }
    let findings = verdict != "pass" || !logs.is_clean();
    let kept = finish_env(cfg, env, findings);
    Ok(FaultResult {
        fault: name.to_string(),
        description: fault
            .map(Fault::describe)
            .unwrap_or("no fault: victim load alone")
            .to_string(),
        victim_users: cfg.victim_users,
        victim_during: during,
        victim_after: after,
        injector,
        resources,
        health_after,
        logs,
        verdict: verdict.to_string(),
        reasons,
        sandbox_kept: kept,
    })
}

// ------------------------------------------------------------------- crash

#[derive(Serialize, Clone, Debug)]
pub struct CrashResult {
    pub victim_users: usize,
    pub killed_after_s: f64,
    pub restart_s: Option<f64>,
    pub restart_error: Option<String>,
    pub victim: WindowStats,
    pub acked_submits: usize,
    pub verified_present: usize,
    /// Acknowledged squads that exist after the restart but in `failed`
    /// (crash recovery fails squads caught mid-materialization).
    pub failed_after_restart: Vec<String>,
    pub lost: Vec<String>,
    pub verify_errors: usize,
    pub health_after: Health,
    pub logs: LogScan,
    pub verdict: String,
    pub reasons: Vec<String>,
    pub sandbox_kept: Option<String>,
}

/// Hard-kill the daemon mid-write-load, restart it on the same database, and
/// check that every submit the client saw acknowledged is still there.
pub fn crash(cfg: &Config) -> Result<CrashResult, String> {
    let mut env = setup(cfg, "crash-restart", 50, 0)?;
    let mix = Arc::new(Mix::new(
        "submit_only",
        &[(Op::Submit, 1)],
        &env.api.available,
    ));
    let before = load::snapshot_ids(&env.api).len();
    let window = cfg.fault_window.max(Duration::from_secs(9));
    let victim = Background::start(&env.api, &mix, victim_spec(cfg, window));
    let kill_at = window / 3;
    thread::sleep(kill_at);
    let (restart_s, restart_error) = match env.daemon.restart() {
        Ok(d) => (Some(d.as_secs_f64()), None),
        Err(e) => {
            victim.stop_now();
            (None, Some(e))
        }
    };
    let run = victim.join().ok_or("victim load thread panicked")?;
    let stats = run.stats(window);
    let ids = load::snapshot_ids(&env.api);
    let acked = ids.len().saturating_sub(before);

    let mut lost = Vec::new();
    let mut present = 0;
    let mut verify_errors = 0;
    let mut failed_after_restart = Vec::new();
    if restart_error.is_none() {
        let ag = api::agent(cfg.timeout);
        let user = env.api.users.first().map(String::as_str);
        for id in &ids {
            let r = env.api.call(
                &ag,
                user,
                "GET",
                &format!("/api/{}/{id}", env.api.noun),
                None,
                true,
            );
            match r.outcome {
                crate::stats::Outcome::Ok(_) => {
                    present += 1;
                    if squad_state(r.body.as_deref()).as_deref() == Some("failed") {
                        failed_after_restart.push(id.clone());
                    }
                }
                crate::stats::Outcome::Http(404) => lost.push(id.clone()),
                _ => verify_errors += 1,
            }
        }
    }
    let health_after = health(&mut env);
    let logs = env.cursor.scan();
    let mut reasons = Vec::new();
    let mut verdict = "pass";
    if let Some(e) = &restart_error {
        verdict = "fail";
        reasons.push(format!("restart failed: {e}"));
    }
    if !lost.is_empty() {
        verdict = "fail";
        reasons.push(format!(
            "{} acknowledged submits missing after restart",
            lost.len()
        ));
    }
    if verify_errors > 0 {
        verdict = "fail";
        reasons.push(format!(
            "{verify_errors} squads could not be read back after restart"
        ));
    }
    if !health_after.answered {
        verdict = "fail";
        reasons.push("daemon not answering after restart".into());
    }
    if verdict == "pass" && !logs.panics.is_empty() {
        verdict = "degraded";
        reasons.push(format!("{} panic site(s) logged", logs.panics.len()));
    }
    if !failed_after_restart.is_empty() {
        if verdict == "pass" {
            verdict = "degraded";
        }
        reasons.push(format!(
            "{} acknowledged submits came back failed after the restart",
            failed_after_restart.len()
        ));
    }
    let findings = verdict != "pass" || !logs.is_clean();
    let kept = finish_env(cfg, env, findings);
    lost.truncate(50);
    failed_after_restart.truncate(50);
    Ok(CrashResult {
        victim_users: cfg.victim_users,
        killed_after_s: kill_at.as_secs_f64(),
        restart_s,
        restart_error,
        victim: stats,
        acked_submits: acked,
        verified_present: present,
        failed_after_restart,
        lost,
        verify_errors,
        health_after,
        logs,
        verdict: verdict.to_string(),
        reasons,
        sandbox_kept: kept,
    })
}

/// The `state` of a squad-detail response, wherever this daemon version puts
/// it (top level, or under `squad`/`run`).
fn squad_state(body: Option<&str>) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(body?).ok()?;
    [&v["state"], &v["squad"]["state"], &v["run"]["state"]]
        .into_iter()
        .find_map(|s| s.as_str().map(str::to_string))
}

/// Probe-only pass, so the report records what the daemon supports even if
/// every scenario was skipped.
pub fn capabilities(cfg: &Config) -> Result<(Capabilities, f64), String> {
    let env = setup(cfg, "probe", 0, 0)?;
    let caps = env.api.capabilities();
    let startup = env.startup_s;
    finish_env(cfg, env, false);
    Ok((caps, startup))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn step(users: usize, ok_rps: f64, err: f64) -> Step {
        Step {
            users,
            users_started: users,
            users_failed_to_start: 0,
            stats: WindowStats {
                ok_rps,
                error_rate: err,
                ..WindowStats::default()
            },
            resources: Resources::default(),
            daemon_alive: true,
        }
    }

    #[test]
    fn ramp_summary_finds_peak_and_one_percent_point() {
        let steps = vec![
            step(1, 100.0, 0.0),
            step(2, 180.0, 0.0),
            step(4, 300.0, 0.005),
            step(8, 320.0, 0.03),
            step(16, 250.0, 0.2),
        ];
        let s = summarize_ramp(&steps, "x".into());
        assert_eq!(s.users_at_peak, 8);
        assert_eq!(s.users_at_peak_under_1pct, 4);
        assert_eq!(s.max_users_under_1pct, 4);
        assert_eq!(s.max_users_p99_under_1s, 4);
        assert_eq!(s.first_users_over_1pct, Some(8));
        assert_eq!(s.max_users_tested, 16);
    }

    #[test]
    fn every_profile_resolves_with_all_ops() {
        for p in PROFILES {
            assert!(profile_mix(p, &Op::ALL).is_some(), "{p}");
        }
        assert!(profile_mix("nope", &Op::ALL).is_none());
    }
}
