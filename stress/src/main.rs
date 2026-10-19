//! `ralphus-stress`: stress and fault-injection harness for `ralphus-daemon`.
//!
//! ```text
//! ralphus-stress run --daemon-exe <path> [options]   # run the suite, write results
//! ralphus-stress history [--out perf/stress]         # rebuild HISTORY.md
//! ```
//!
//! See `perf/stress/README.md` for what each scenario measures and how to
//! read the results.

mod api;
mod daemon;
mod faults;
mod git;
mod load;
mod logs;
mod report;
mod scenarios;
mod stats;

use faults::Fault;
use scenarios::Config;
use std::path::PathBuf;
use std::time::{Duration, Instant};

const USAGE: &str = "usage:
  ralphus-stress run --daemon-exe <path> [--repo <dir>] [--commit <rev>] [--label <name>]
                     [--out <dir>] [--work <dir>] [--quick] [--scenarios ramp,volume,faults,crash]
                     [--profiles read_heavy,write_heavy,mixed_80_20,browsing_users]
                     [--faults <name,...>] [--max-users N] [--step-secs N] [--fault-secs N]
                     [--volume SQUADS:PROJECTS,...] [--volume-users N] [--victim-users N]
                     [--timeout-ms N] [--keep-sandboxes]
  ralphus-stress history [--out <dir>]";

struct Args {
    cmd: String,
    flags: Vec<(String, Option<String>)>,
}

impl Args {
    fn parse() -> Result<Args, String> {
        let mut it = std::env::args().skip(1).peekable();
        let cmd = it.next().ok_or(USAGE)?;
        let mut flags = Vec::new();
        while let Some(a) = it.next() {
            let name = a
                .strip_prefix("--")
                .ok_or_else(|| format!("unexpected argument {a:?}\n{USAGE}"))?
                .to_string();
            let value = match it.peek() {
                Some(v) if !v.starts_with("--") => it.next(),
                _ => None,
            };
            flags.push((name, value));
        }
        Ok(Args { cmd, flags })
    }

    fn get(&self, name: &str) -> Option<&str> {
        self.flags
            .iter()
            .rev()
            .find(|(n, _)| n == name)
            .and_then(|(_, v)| v.as_deref())
    }

    fn has(&self, name: &str) -> bool {
        self.flags.iter().any(|(n, _)| n == name)
    }

    fn num<T: std::str::FromStr>(&self, name: &str, default: T) -> Result<T, String> {
        match self.get(name) {
            Some(v) => v
                .parse()
                .map_err(|_| format!("--{name}: not a number: {v:?}")),
            None => Ok(default),
        }
    }

    fn list(&self, name: &str) -> Option<Vec<String>> {
        self.get(name).map(|v| {
            v.split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect()
        })
    }
}

fn say(msg: &str) {
    eprintln!("{msg}");
}

fn main() {
    if let Err(e) = real_main() {
        eprintln!("ralphus-stress: {e}");
        std::process::exit(1);
    }
}

fn real_main() -> Result<(), String> {
    let args = Args::parse()?;
    let out = PathBuf::from(args.get("out").unwrap_or("perf/stress"));
    match args.cmd.as_str() {
        "history" => {
            let p = report::history(&out)?;
            say(&format!("wrote {}", p.display()));
            Ok(())
        }
        "run" => run(&args, &out),
        "-h" | "--help" | "help" => {
            say(USAGE);
            Ok(())
        }
        other => Err(format!("unknown command {other:?}\n{USAGE}")),
    }
}

fn run(args: &Args, out: &std::path::Path) -> Result<(), String> {
    let exe = PathBuf::from(
        args.get("daemon-exe")
            .ok_or(format!("--daemon-exe is required\n{USAGE}"))?,
    );
    if !exe.is_file() {
        return Err(format!("daemon exe not found: {}", exe.display()));
    }
    let repo = PathBuf::from(args.get("repo").unwrap_or("."));
    let mut git = git::describe(&repo, args.get("commit").unwrap_or("HEAD"))?;
    if args.has("commit") {
        // A historical build: the checked-out tree's state says nothing about it.
        git.dirty = false;
    }
    let label = args
        .get("label")
        .map(str::to_string)
        .unwrap_or_else(|| git.label());
    let quick = args.has("quick");
    let max_users: usize = args.num("max-users", if quick { 256 } else { 4096 })?;
    let user_steps: Vec<usize> = [
        1, 2, 4, 8, 16, 32, 64, 128, 256, 512, 1024, 2048, 4096, 8192,
    ]
    .into_iter()
    .filter(|u| *u <= max_users)
    .collect();
    let browsing_steps: Vec<usize> = if quick {
        vec![50, 200]
    } else {
        vec![100, 250, 500, 1000, 2000, 4000]
    }
    .into_iter()
    .filter(|u| *u <= max_users.max(100))
    .collect();
    let volume_levels = match args.get("volume") {
        Some(v) => v
            .split(',')
            .map(|pair| {
                let (s, p) = pair
                    .split_once(':')
                    .ok_or(format!("--volume: expected SQUADS:PROJECTS, got {pair:?}"))?;
                Ok((
                    s.parse().map_err(|_| format!("bad squad count {s:?}"))?,
                    p.parse().map_err(|_| format!("bad project count {p:?}"))?,
                ))
            })
            .collect::<Result<Vec<(usize, usize)>, String>>()?,
        None if quick => vec![(100, 2), (500, 5)],
        None => vec![(100, 5), (1000, 25), (5000, 100), (10000, 250)],
    };
    let scenarios = args.list("scenarios").unwrap_or_else(|| {
        ["ramp", "volume", "faults", "crash"]
            .map(String::from)
            .to_vec()
    });
    let profiles = args
        .list("profiles")
        .unwrap_or_else(|| scenarios::PROFILES.map(String::from).to_vec());
    for p in &profiles {
        if !scenarios::PROFILES.contains(&p.as_str()) {
            return Err(format!(
                "unknown profile {p:?}; known: {}",
                scenarios::PROFILES.join(", ")
            ));
        }
    }
    let fault_list = match args.list("faults") {
        Some(names) => names
            .iter()
            .map(|n| {
                Fault::ALL
                    .into_iter()
                    .find(|f| f.name() == n)
                    .ok_or(format!("unknown fault {n:?}"))
            })
            .collect::<Result<Vec<_>, _>>()?,
        None => Fault::ALL.to_vec(),
    };
    let work_dir = args
        .get("work")
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::temp_dir().join("ralphus-stress").join(&label));
    let step_secs: u64 = args.num("step-secs", if quick { 4 } else { 10 })?;
    let fault_secs: u64 = args.num("fault-secs", if quick { 6 } else { 15 })?;
    let cfg = Config {
        daemon_exe: std::fs::canonicalize(&exe).unwrap_or(exe),
        work_dir: work_dir.clone(),
        user_steps,
        browsing_steps,
        step: Duration::from_secs(step_secs),
        timeout: Duration::from_millis(args.num("timeout-ms", 10_000)?),
        fault_window: Duration::from_secs(fault_secs),
        victim_users: args.num("victim-users", 16)?,
        volume_levels,
        volume_users: args.num("volume-users", 32)?,
        user_pool: args.num("user-pool", 64)?,
        seed_squads: args.num("seed-squads", 200)?,
        seed_projects: args.num("seed-projects", 5)?,
        profiles,
        faults: fault_list,
        run_ramp: scenarios.iter().any(|s| s == "ramp"),
        run_volume: scenarios.iter().any(|s| s == "volume"),
        run_faults: scenarios.iter().any(|s| s == "faults"),
        run_crash: scenarios.iter().any(|s| s == "crash"),
        keep_sandboxes: args.has("keep-sandboxes"),
    };

    let t0 = Instant::now();
    let started = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    say(&format!(
        "ralphus-stress: {label} ({}) daemon={} work={}",
        git.commit,
        cfg.daemon_exe.display(),
        work_dir.display()
    ));
    let mut errors = Vec::new();
    let (capabilities, startup) = match scenarios::capabilities(&cfg) {
        Ok((c, s)) => {
            say(&format!(
                "probe: /api/{} auth={} users={} ops={:?} unavailable={:?}",
                c.noun,
                c.auth,
                c.users,
                c.available_ops,
                c.unavailable_ops.iter().map(|(o, _)| o).collect::<Vec<_>>()
            ));
            (Some(c), s)
        }
        Err(e) => {
            errors.push(format!("probe: {e}"));
            (None, 0.0)
        }
    };

    let mut ramps = Vec::new();
    let mut volume = None;
    let mut fault_results = Vec::new();
    let mut crash = None;
    if capabilities.is_some() {
        if cfg.run_ramp {
            for p in &cfg.profiles {
                say(&format!("ramp: {p}"));
                match scenarios::ramp(&cfg, p, &say) {
                    Ok(r) => {
                        say(&format!(
                            "  -> peak {:.0} ok/s @ {} users; first >1% errors: {:?}; {}",
                            r.summary.peak_ok_rps,
                            r.summary.users_at_peak,
                            r.summary.first_users_over_1pct,
                            r.summary.stop_reason
                        ));
                        ramps.push(r);
                    }
                    Err(e) => errors.push(format!("ramp {p}: {e}")),
                }
            }
        }
        if cfg.run_volume {
            say("volume");
            match scenarios::volume(&cfg, &say) {
                Ok(v) => volume = Some(v),
                Err(e) => errors.push(format!("volume: {e}")),
            }
        }
        if cfg.run_faults {
            say("faults: baseline");
            let baseline = match scenarios::fault(&cfg, None, None) {
                Ok(b) => Some(b),
                Err(e) => {
                    errors.push(format!("fault baseline: {e}"));
                    None
                }
            };
            for f in &cfg.faults {
                say(&format!("faults: {}", f.name()));
                match scenarios::fault(&cfg, Some(*f), baseline.as_ref()) {
                    Ok(r) => {
                        say(&format!(
                            "  -> {} (victim err {:.2}%, p99 {:.1}ms) {}",
                            r.verdict,
                            r.victim_during.error_rate * 100.0,
                            r.victim_during.latency_ok.p99_ms,
                            r.reasons.join("; ")
                        ));
                        fault_results.push(r);
                    }
                    Err(e) => errors.push(format!("fault {}: {e}", f.name())),
                }
            }
            if let Some(b) = baseline {
                fault_results.insert(0, b);
            }
        }
        if cfg.run_crash {
            say("crash/restart");
            match scenarios::crash(&cfg) {
                Ok(c) => {
                    say(&format!("  -> {} {}", c.verdict, c.reasons.join("; ")));
                    crash = Some(c);
                }
                Err(e) => errors.push(format!("crash: {e}")),
            }
        }
    }

    let mut rep = report::Report {
        schema_version: report::SCHEMA_VERSION,
        label,
        git,
        run_started: started,
        run_seconds: t0.elapsed().as_secs_f64(),
        machine: report::machine(),
        daemon_exe: cfg.daemon_exe.display().to_string(),
        daemon_startup_s: startup,
        capabilities,
        settings: report::Settings {
            user_steps: cfg.user_steps.clone(),
            browsing_steps: cfg.browsing_steps.clone(),
            step_s: cfg.step.as_secs_f64(),
            request_timeout_s: cfg.timeout.as_secs_f64(),
            fault_window_s: cfg.fault_window.as_secs_f64(),
            victim_users: cfg.victim_users,
            volume_levels: cfg.volume_levels.clone(),
            volume_users: cfg.volume_users,
            user_pool: cfg.user_pool,
            seed_squads: cfg.seed_squads,
            seed_projects: cfg.seed_projects,
            quick,
        },
        ramps,
        volume,
        faults: fault_results,
        crash,
        scenario_errors: errors,
        findings: Vec::new(),
    };
    rep.collect_findings();
    let (json, md) = report::write(out, &rep)?;
    let hist = report::history(out)?;
    say(&format!(
        "done in {:.0}s: {} findings ({} high)\n  {}\n  {}\n  {}",
        rep.run_seconds,
        rep.findings.len(),
        rep.findings.iter().filter(|f| f.severity == "high").count(),
        json.display(),
        md.display(),
        hist.display()
    ));
    if work_dir
        .read_dir()
        .map(|mut d| d.next().is_none())
        .unwrap_or(false)
    {
        let _ = std::fs::remove_dir(&work_dir);
    }
    Ok(())
}
