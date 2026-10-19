//! Result files: `<out>/results/<label>.json` (machine-readable, the source
//! of truth), `<out>/results/<label>.md` (human summary), and
//! `<out>/HISTORY.md`, regenerated from every JSON result so performance can
//! be tracked across tags and commits.

use crate::api::Capabilities;
use crate::git::CommitInfo;
use crate::logs::LogScan;
use crate::scenarios::{CrashResult, FaultResult, RampResult, VolumeResult};
use serde::Serialize;
use std::fmt::Write as _;
use std::fs;
use std::path::Path;

pub const SCHEMA_VERSION: u32 = 1;

#[derive(Serialize, Clone, Debug)]
pub struct Machine {
    pub os: String,
    pub cpu_brand: String,
    pub logical_cpus: usize,
    pub total_memory_gb: f64,
}

pub fn machine() -> Machine {
    use sysinfo::{CpuRefreshKind, MemoryRefreshKind, RefreshKind, System};
    let sys = System::new_with_specifics(
        RefreshKind::nothing()
            .with_cpu(CpuRefreshKind::nothing())
            .with_memory(MemoryRefreshKind::nothing().with_ram()),
    );
    Machine {
        os: System::long_os_version().unwrap_or_default(),
        cpu_brand: sys
            .cpus()
            .first()
            .map(|c| c.brand().trim().to_string())
            .unwrap_or_default(),
        logical_cpus: sys.cpus().len(),
        total_memory_gb: sys.total_memory() as f64 / 1024f64.powi(3),
    }
}

#[derive(Serialize, Clone, Debug)]
pub struct Finding {
    /// high: crash/data loss/panic; medium: degraded behaviour or 5xx;
    /// low: noisy logs, slow requests.
    pub severity: String,
    pub scenario: String,
    pub kind: String,
    pub summary: String,
    pub detail: serde_json::Value,
}

#[derive(Serialize, Clone, Debug)]
pub struct Settings {
    pub user_steps: Vec<usize>,
    pub browsing_steps: Vec<usize>,
    pub step_s: f64,
    pub request_timeout_s: f64,
    pub fault_window_s: f64,
    pub victim_users: usize,
    pub volume_levels: Vec<(usize, usize)>,
    pub volume_users: usize,
    pub user_pool: usize,
    pub seed_squads: usize,
    pub seed_projects: usize,
    pub quick: bool,
}

#[derive(Serialize, Clone, Debug)]
pub struct Report {
    pub schema_version: u32,
    pub label: String,
    pub git: CommitInfo,
    pub run_started: String,
    pub run_seconds: f64,
    pub machine: Machine,
    pub daemon_exe: String,
    pub daemon_startup_s: f64,
    pub capabilities: Option<Capabilities>,
    pub settings: Settings,
    pub ramps: Vec<RampResult>,
    pub volume: Option<VolumeResult>,
    pub faults: Vec<FaultResult>,
    pub crash: Option<CrashResult>,
    pub scenario_errors: Vec<String>,
    pub findings: Vec<Finding>,
}

fn log_findings(out: &mut Vec<Finding>, scenario: &str, logs: &LogScan, sandbox: Option<&String>) {
    for p in &logs.panics {
        out.push(Finding {
            severity: "high".into(),
            scenario: scenario.into(),
            kind: "panic".into(),
            summary: format!(
                "thread '{}' panicked at {}: {}",
                p.thread, p.location, p.message
            ),
            detail: serde_json::json!({ "panic": p, "sandbox": sandbox }),
        });
    }
    if logs.slow_requests > 0 {
        out.push(Finding {
            severity: "low".into(),
            scenario: scenario.into(),
            kind: "slow_requests".into(),
            summary: format!(
                "{} requests logged as SLOW (handler >= 1s)",
                logs.slow_requests
            ),
            detail: serde_json::json!({ "examples": logs.slow_examples }),
        });
    }
    for h in logs.lock_holds.iter().filter(|h| h.max_ms >= 500).take(5) {
        out.push(Finding {
            severity: if h.max_ms >= 1000 { "medium" } else { "low" }.into(),
            scenario: scenario.into(),
            kind: "long_store_lock_hold".into(),
            summary: format!(
                "store lock held up to {} ms at {} ({}x, {} ms total)",
                h.max_ms, h.site, h.count, h.total_ms
            ),
            detail: serde_json::json!(h),
        });
    }
    for l in logs.error_lines.iter().take(10) {
        out.push(Finding {
            severity: "low".into(),
            scenario: scenario.into(),
            kind: "error_log_line".into(),
            summary: format!("{}x {}", l.count, l.pattern),
            detail: serde_json::json!({ "example": l.example, "sandbox": sandbox }),
        });
    }
}

impl Report {
    pub fn collect_findings(&mut self) {
        let mut f = Vec::new();
        for e in &self.scenario_errors {
            f.push(Finding {
                severity: "high".into(),
                scenario: "setup".into(),
                kind: "scenario_error".into(),
                summary: e.clone(),
                detail: serde_json::Value::Null,
            });
        }
        for r in &self.ramps {
            let sc = format!("ramp:{}", r.profile);
            if !r.health_after.answered {
                f.push(Finding {
                    severity: "high".into(),
                    scenario: sc.clone(),
                    kind: "daemon_unhealthy".into(),
                    summary: format!("daemon did not answer after the ramp (alive={})", r.health_after.alive),
                    detail: serde_json::json!({ "health": r.health_after, "stop_reason": r.summary.stop_reason }),
                });
            }
            if let Some(users) = r.summary.first_users_over_1pct {
                let step = r.steps.iter().find(|s| s.users == users);
                f.push(Finding {
                    severity: "medium".into(),
                    scenario: sc.clone(),
                    kind: "error_threshold".into(),
                    summary: format!(
                        "error rate passed 1% at {users} concurrent users ({:.2}%)",
                        r.summary.error_rate_at_first_over.unwrap_or(0.0) * 100.0
                    ),
                    detail: serde_json::json!({
                        "errors_by_class": step.map(|s| &s.stats.errors_by_class),
                        "error_examples": step.map(|s| &s.stats.error_examples),
                    }),
                });
            }
            log_findings(&mut f, &sc, &r.logs, r.sandbox_kept.as_ref());
        }
        if let Some(v) = &self.volume {
            if let Some(e) = &v.error {
                f.push(Finding {
                    severity: "high".into(),
                    scenario: "volume".into(),
                    kind: "scenario_error".into(),
                    summary: e.clone(),
                    detail: serde_json::Value::Null,
                });
            }
            for l in &v.levels {
                for (name, s) in [("read", &l.read), ("mixed", &l.mixed)] {
                    if s.error_rate > 0.01 {
                        f.push(Finding {
                            severity: "medium".into(),
                            scenario: "volume".into(),
                            kind: "error_threshold".into(),
                            summary: format!(
                                "{name} load at {} squads / {} projects: {:.2}% errors",
                                l.squads,
                                l.projects,
                                s.error_rate * 100.0
                            ),
                            detail: serde_json::json!({ "errors_by_class": s.errors_by_class, "error_examples": s.error_examples }),
                        });
                    }
                }
            }
            log_findings(&mut f, "volume", &v.logs, v.sandbox_kept.as_ref());
        }
        for r in &self.faults {
            let sc = format!("fault:{}", r.fault);
            if r.verdict != "pass" {
                f.push(Finding {
                    severity: if r.verdict == "fail" {
                        "high"
                    } else {
                        "medium"
                    }
                    .into(),
                    scenario: sc.clone(),
                    kind: format!("fault_{}", r.verdict),
                    summary: format!("{}: {}", r.fault, r.reasons.join("; ")),
                    detail: serde_json::json!({
                        "description": r.description,
                        "victim_errors_by_class": r.victim_during.errors_by_class,
                        "victim_error_examples": r.victim_during.error_examples,
                        "injector": r.injector,
                        "health_after": r.health_after,
                        "sandbox": r.sandbox_kept,
                    }),
                });
            }
            log_findings(&mut f, &sc, &r.logs, r.sandbox_kept.as_ref());
        }
        if let Some(c) = &self.crash {
            if c.verdict != "pass" {
                f.push(Finding {
                    severity: "high".into(),
                    scenario: "crash_restart".into(),
                    kind: format!("crash_{}", c.verdict),
                    summary: c.reasons.join("; "),
                    detail: serde_json::json!({ "lost": c.lost, "restart_error": c.restart_error, "sandbox": c.sandbox_kept }),
                });
            }
            log_findings(&mut f, "crash_restart", &c.logs, c.sandbox_kept.as_ref());
        }
        let rank = |s: &str| match s {
            "high" => 0,
            "medium" => 1,
            _ => 2,
        };
        f.sort_by_key(|x| rank(&x.severity));
        self.findings = f;
    }
}

fn pct(x: f64) -> String {
    format!("{:.2}%", x * 100.0)
}

pub fn markdown(r: &Report) -> String {
    let mut s = String::new();
    let _ = writeln!(s, "# Stress results: `{}`\n", r.label);
    let _ = writeln!(
        s,
        "- **Commit:** `{}`{} ({}) {}\n- **Tags:** {}\n- **Run:** {} ({:.0}s)\n- **Machine:** {} / {} logical CPUs / {:.0} GB / {}\n- **Daemon startup:** {:.2}s",
        r.git.commit,
        if r.git.dirty { " **(dirty tree)**" } else { "" },
        r.git.commit_date,
        r.git.subject,
        if r.git.tags.is_empty() {
            "none".to_string()
        } else {
            r.git.tags.join(", ")
        },
        r.run_started,
        r.run_seconds,
        r.machine.cpu_brand,
        r.machine.logical_cpus,
        r.machine.total_memory_gb,
        r.machine.os,
        r.daemon_startup_s,
    );
    if let Some(c) = &r.capabilities {
        let ops: Vec<String> = c.available_ops.iter().map(|o| format!("{o:?}")).collect();
        let _ = writeln!(
            s,
            "- **API:** `/api/{}`, auth={}, users={}, ops: {}",
            c.noun,
            c.auth,
            c.users,
            ops.join(", ")
        );
        if !c.unavailable_ops.is_empty() {
            let un: Vec<String> = c
                .unavailable_ops
                .iter()
                .map(|(o, _)| format!("{o:?}"))
                .collect();
            let _ = writeln!(s, "- **Unavailable ops (skipped):** {}", un.join(", "));
        }
    }
    let _ = writeln!(
        s,
        "\nThe load generator runs on the same machine as the daemon, so absolute numbers are only comparable between runs on the same machine.\n"
    );

    let _ = writeln!(s, "## Headline\n");
    let _ = writeln!(
        s,
        "| Profile | Peak ok req/s | Users @ peak | p99 @ peak | Peak ok req/s with ≤1% errors | Max users with ≤1% errors | First users >1% errors | Stopped because |"
    );
    let _ = writeln!(s, "|---|---:|---:|---:|---:|---:|---:|---|");
    for ramp in &r.ramps {
        let m = &ramp.summary;
        let _ = writeln!(
            s,
            "| {}{} | {:.0} | {} | {:.1} ms | {:.0} | {} | {} | {} |",
            ramp.profile,
            if ramp.think_ms > 0 {
                format!(" (think {} ms)", ramp.think_ms)
            } else {
                String::new()
            },
            m.peak_ok_rps,
            m.users_at_peak,
            m.p99_ms_at_peak,
            m.peak_ok_rps_under_1pct,
            m.max_users_under_1pct,
            m.first_users_over_1pct
                .map(|u| format!("{u} ({})", pct(m.error_rate_at_first_over.unwrap_or(0.0))))
                .unwrap_or_else(|| "never".into()),
            m.stop_reason
        );
    }

    for ramp in &r.ramps {
        let _ = writeln!(s, "\n## Ramp: {}\n", ramp.profile);
        let ops: Vec<String> = ramp
            .mix
            .ops
            .iter()
            .map(|(o, w)| format!("{} {}", o.name(), w))
            .collect();
        let _ = writeln!(s, "Mix (weights): {}\n", ops.join(", "));
        let _ = writeln!(
            s,
            "| Users | ok req/s | Errors | p50 | p90 | p99 | max | Daemon CPU avg/peak | Daemon RSS | Top error |"
        );
        let _ = writeln!(s, "|---:|---:|---:|---:|---:|---:|---:|---:|---:|---|");
        for st in &ramp.steps {
            let top = st
                .stats
                .errors_by_class
                .iter()
                .max_by_key(|(_, v)| **v)
                .map(|(k, v)| format!("{k} ×{v}"))
                .unwrap_or_default();
            let l = &st.stats.latency_ok;
            let _ = writeln!(
                s,
                "| {} | {:.0} | {} | {:.1} | {:.1} | {:.1} | {:.0} | {:.0}% / {:.0}% | {:.0} MB | {} |",
                st.users,
                st.stats.ok_rps,
                pct(st.stats.error_rate),
                l.p50_ms,
                l.p90_ms,
                l.p99_ms,
                l.max_ms,
                st.resources.daemon_avg_cpu_pct,
                st.resources.daemon_peak_cpu_pct,
                st.resources.daemon_peak_rss_mb,
                top
            );
        }
    }

    for ramp in &r.ramps {
        if ramp.logs.routes.is_empty() {
            continue;
        }
        let _ = writeln!(
            s,
            "\n### Server-side route timing: {} (whole ramp, from the daemon's request log)\n",
            ramp.profile
        );
        let _ = writeln!(
            s,
            "`queued` = waiting for a read/write worker, `lock_wait` = waiting for the store lock, `handler` = the handler itself (includes lock wait).\n"
        );
        let _ = writeln!(
            s,
            "| Route | Count | non-2xx | handler p50 / p99 / max | queued p50 / p99 | lock_wait p50 / p99 |"
        );
        let _ = writeln!(s, "|---|---:|---:|---:|---:|---:|");
        for t in ramp.logs.routes.iter().take(10) {
            let _ = writeln!(
                s,
                "| `{}` | {} | {} | {:.0} / {:.0} / {:.0} ms | {:.0} / {:.0} ms | {:.0} / {:.0} ms |",
                t.route,
                t.count,
                t.non_2xx,
                t.handler_p50_ms,
                t.handler_p99_ms,
                t.handler_max_ms,
                t.queued_p50_ms,
                t.queued_p99_ms,
                t.lock_wait_p50_ms,
                t.lock_wait_p99_ms
            );
        }
    }

    let mut holds: std::collections::BTreeMap<String, (u64, u64, u64)> = Default::default();
    let all_logs = r
        .ramps
        .iter()
        .map(|x| &x.logs)
        .chain(r.volume.iter().map(|x| &x.logs))
        .chain(r.faults.iter().map(|x| &x.logs))
        .chain(r.crash.iter().map(|x| &x.logs));
    for l in all_logs {
        for h in &l.lock_holds {
            let e = holds.entry(h.site.clone()).or_default();
            e.0 += h.count;
            e.1 = e.1.max(h.max_ms);
            e.2 += h.total_ms;
        }
    }
    if !holds.is_empty() {
        let mut rows: Vec<_> = holds.into_iter().collect();
        rows.sort_by_key(|(_, v)| std::cmp::Reverse(v.2));
        let _ = writeln!(s, "\n## Long store-lock holds (all scenarios)\n");
        let _ = writeln!(
            s,
            "Logged by the daemon when a store guard is held past its warn threshold; the site is where the lock was taken.\n"
        );
        let _ = writeln!(s, "| Site | Count | Max | Total |");
        let _ = writeln!(s, "|---|---:|---:|---:|");
        for (site, (count, max, total)) in rows.iter().take(15) {
            let _ = writeln!(s, "| `{site}` | {count} | {max} ms | {total} ms |");
        }
    }

    if let Some(v) = &r.volume {
        let _ = writeln!(
            s,
            "\n## Data volume ({} closed-loop users per window)\n",
            v.users
        );
        let _ = writeln!(
            s,
            "| Squads | Projects | Seed squads/s | Board size | Board single GET | Read ok req/s | Read err | Read p99 | Mixed ok req/s | Mixed err | Mixed p99 | Daemon RSS |"
        );
        let _ = writeln!(
            s,
            "|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|"
        );
        for l in &v.levels {
            let _ = writeln!(
                s,
                "| {} | {} | {:.0} | {:.0} KB | {:.1} ms | {:.0} | {} | {:.1} ms | {:.0} | {} | {:.1} ms | {:.0} MB |",
                l.squads,
                l.projects,
                l.seed_squads_per_s,
                l.board_bytes as f64 / 1024.0,
                l.board_single_ms_median,
                l.read.ok_rps,
                pct(l.read.error_rate),
                l.read.latency_ok.p99_ms,
                l.mixed.ok_rps,
                pct(l.mixed.error_rate),
                l.mixed.latency_ok.p99_ms,
                l.resources.daemon_peak_rss_mb
            );
        }
        if let Some(e) = &v.error {
            let _ = writeln!(s, "\n**Stopped early:** {e}");
        }
    }

    if !r.faults.is_empty() {
        let _ = writeln!(s, "\n## Fault injection\n");
        let _ = writeln!(
            s,
            "Each fault runs in a fresh daemon while a mixed 80/20 victim load measures what well-behaved clients see; then a 3 s clean window checks recovery.\n"
        );
        let _ = writeln!(
            s,
            "| Fault | Verdict | Victim ok req/s | Victim errors | Victim p99 | After: errors | Injector outcomes | Why |"
        );
        let _ = writeln!(s, "|---|---|---:|---:|---:|---:|---|---|");
        for f in &r.faults {
            let outcomes: Vec<String> = f
                .injector
                .outcomes
                .iter()
                .map(|(k, v)| format!("{k}: {v}"))
                .collect();
            let _ = writeln!(
                s,
                "| {} | **{}** | {:.0} | {} | {:.1} ms | {} | {} | {} |",
                f.fault,
                f.verdict,
                f.victim_during.ok_rps,
                pct(f.victim_during.error_rate),
                f.victim_during.latency_ok.p99_ms,
                pct(f.victim_after.error_rate),
                outcomes.join(", "),
                f.reasons.join("; ")
            );
        }
        if let Some(m) = r.faults.iter().find(|f| f.fault == "malformed_requests") {
            let _ = writeln!(s, "\n### Malformed-request corpus responses\n");
            let _ = writeln!(s, "| Payload | Responses |");
            let _ = writeln!(s, "|---|---|");
            for (name, outcomes) in &m.injector.per_payload {
                let o: Vec<String> = outcomes.iter().map(|(k, v)| format!("{k} ×{v}")).collect();
                let _ = writeln!(s, "| `{name}` | {} |", o.join(", "));
            }
        }
        let notes: Vec<String> = r
            .faults
            .iter()
            .flat_map(|f| {
                f.injector
                    .notes
                    .iter()
                    .map(move |n| format!("- **{}**: {n}", f.fault))
            })
            .collect();
        if !notes.is_empty() {
            let _ = writeln!(s, "\n### Injector notes\n\n{}", notes.join("\n"));
        }
    }

    if let Some(c) = &r.crash {
        let _ = writeln!(s, "\n## Crash / restart durability\n");
        let _ = writeln!(
            s,
            "Hard-killed the daemon {:.1}s into a {}-user submit load, restarted it on the same database.\n\n- **Verdict:** **{}** {}\n- **Restart to first answer:** {}\n- **Acknowledged submits:** {} (present after restart: {}, missing: {}, unreadable: {})\n- **Client error rate across the outage:** {}",
            c.killed_after_s,
            c.victim_users,
            c.verdict,
            c.reasons.join("; "),
            c.restart_s
                .map(|t| format!("{t:.2}s"))
                .unwrap_or_else(|| c.restart_error.clone().unwrap_or_default()),
            c.acked_submits,
            c.verified_present,
            c.lost.len(),
            c.verify_errors,
            pct(c.victim.error_rate)
        );
    }

    let _ = writeln!(s, "\n## Findings\n");
    if r.findings.is_empty() {
        let _ = writeln!(s, "None.");
    }
    for f in &r.findings {
        let _ = writeln!(
            s,
            "- **[{}]** `{}` {}: {}",
            f.severity, f.scenario, f.kind, f.summary
        );
    }
    if !r.scenario_errors.is_empty() {
        let _ = writeln!(s, "\n## Scenario errors\n");
        for e in &r.scenario_errors {
            let _ = writeln!(s, "- {e}");
        }
    }
    s
}

pub fn write(out: &Path, r: &Report) -> Result<(std::path::PathBuf, std::path::PathBuf), String> {
    let dir = out.join("results");
    fs::create_dir_all(&dir).map_err(|e| format!("creating {}: {e}", dir.display()))?;
    let stem = format!(
        "{}_{}",
        r.git.commit_date.get(..10).unwrap_or("unknown"),
        sanitize(&r.label)
    );
    let json = dir.join(format!("{stem}.json"));
    let md = dir.join(format!("{stem}.md"));
    let text = serde_json::to_string_pretty(r).map_err(|e| e.to_string())?;
    fs::write(&json, text + "\n").map_err(|e| format!("writing {}: {e}", json.display()))?;
    fs::write(&md, markdown(r)).map_err(|e| format!("writing {}: {e}", md.display()))?;
    Ok((json, md))
}

fn sanitize(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

fn ramp_field(v: &serde_json::Value, profile: &str, field: &str) -> Option<serde_json::Value> {
    v["ramps"]
        .as_array()?
        .iter()
        .find(|r| r["profile"] == profile)
        .map(|r| r["summary"][field].clone())
}

fn fmt_num(v: Option<serde_json::Value>) -> String {
    match v {
        Some(serde_json::Value::Number(n)) => format!("{:.0}", n.as_f64().unwrap_or(0.0)),
        Some(serde_json::Value::Null) => "never".into(),
        _ => "–".into(),
    }
}

/// Rebuild `HISTORY.md` from every `results/*.json`, oldest commit first.
pub fn history(out: &Path) -> Result<std::path::PathBuf, String> {
    let dir = out.join("results");
    let mut rows: Vec<serde_json::Value> = Vec::new();
    if let Ok(entries) = fs::read_dir(&dir) {
        for e in entries.flatten() {
            let p = e.path();
            if p.extension().is_some_and(|x| x == "json") {
                if let Some(v) = fs::read_to_string(&p)
                    .ok()
                    .and_then(|t| serde_json::from_str(&t).ok())
                {
                    rows.push(v);
                }
            }
        }
    }
    rows.sort_by(|a, b| {
        let ka = (
            a["git"]["commit_date"].as_str().unwrap_or(""),
            a["run_started"].as_str().unwrap_or(""),
        );
        let kb = (
            b["git"]["commit_date"].as_str().unwrap_or(""),
            b["run_started"].as_str().unwrap_or(""),
        );
        ka.cmp(&kb)
    });
    let mut s = String::new();
    let _ = writeln!(s, "# Stress-test history\n");
    let _ = writeln!(
        s,
        "Generated by `ralphus-stress history` from `results/*.json`; do not edit by hand. Numbers are only comparable between runs on the same machine (see each result's machine line). \"Peak\" is successful requests/second at the best step with ≤1% errors; \"1% users\" is the first concurrent-user count whose error rate exceeded 1% (\"never\" = not reached within the tested range).\n"
    );
    let _ = writeln!(
        s,
        "| Label | Commit date | Run | Machine | Read peak | Read 1% users | Mixed peak | Mixed 1% users | Write peak | Write 1% users | Browsing max users ≤1% | Faults pass/degraded/fail | Crash | Findings (high/med/low) |"
    );
    let _ = writeln!(
        s,
        "|---|---|---|---|---:|---:|---:|---:|---:|---:|---:|---|---|---|"
    );
    for v in &rows {
        let faults = v["faults"].as_array().cloned().unwrap_or_default();
        let count = |verdict: &str| faults.iter().filter(|f| f["verdict"] == verdict).count();
        let findings = v["findings"].as_array().cloned().unwrap_or_default();
        let sev = |x: &str| findings.iter().filter(|f| f["severity"] == x).count();
        let file = format!(
            "results/{}_{}.md",
            v["git"]["commit_date"]
                .as_str()
                .unwrap_or("")
                .get(..10)
                .unwrap_or("unknown"),
            sanitize(v["label"].as_str().unwrap_or(""))
        );
        let _ = writeln!(
            s,
            "| [`{}`]({file}) | {} | {} | {} ({} cpus) | {} | {} | {} | {} | {} | {} | {} | {}/{}/{} | {} | {}/{}/{} |",
            v["label"].as_str().unwrap_or("?"),
            v["git"]["commit_date"]
                .as_str()
                .unwrap_or("")
                .get(..10)
                .unwrap_or(""),
            v["run_started"]
                .as_str()
                .unwrap_or("")
                .get(..10)
                .unwrap_or(""),
            v["machine"]["cpu_brand"].as_str().unwrap_or("?"),
            v["machine"]["logical_cpus"],
            fmt_num(ramp_field(v, "read_heavy", "peak_ok_rps_under_1pct")),
            fmt_num(ramp_field(v, "read_heavy", "first_users_over_1pct")),
            fmt_num(ramp_field(v, "mixed_80_20", "peak_ok_rps_under_1pct")),
            fmt_num(ramp_field(v, "mixed_80_20", "first_users_over_1pct")),
            fmt_num(ramp_field(v, "write_heavy", "peak_ok_rps_under_1pct")),
            fmt_num(ramp_field(v, "write_heavy", "first_users_over_1pct")),
            fmt_num(ramp_field(v, "browsing_users", "max_users_under_1pct")),
            count("pass"),
            count("degraded"),
            count("fail"),
            v["crash"]["verdict"].as_str().unwrap_or("–"),
            sev("high"),
            sev("medium"),
            sev("low"),
        );
    }
    let path = out.join("HISTORY.md");
    fs::write(&path, s).map_err(|e| format!("writing {}: {e}", path.display()))?;
    Ok(path)
}
