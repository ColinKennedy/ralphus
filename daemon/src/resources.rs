//! Per-task resource sampling (RAL-11).
//!
//! Maps the OS process metrics of each running cell's `ralphus-runner`
//! subprocess (CPU%, resident RAM, GPU memory) back to the exact ralphus
//! squad/task/cell consuming them, for the board's "Resources" tab.
//!
//! CPU/RAM use no extra crates: on Linux we read `/proc/<pid>/{stat,statm}`
//! directly; on Windows we shell out to PowerShell's `Get-Process`. CPU% is a
//! two-sample delta over a short interval. GPU is best-effort via `nvidia-smi`
//! and degrades to `null` ("N/A") whenever the tool is absent or reports nothing
//! for a PID. Any platform we don't handle simply yields `null` for every metric.

use std::collections::HashMap;
use std::time::Duration;

use serde::Serialize;

use crate::procreg::ProcRegistry;
use crate::store::SquadView;

/// Interval between the two CPU samples used to compute a percentage.
const CPU_SAMPLE_INTERVAL: Duration = Duration::from_millis(200);

/// One running task's resource usage, as shown in the board's Resources tab.
#[derive(Debug, Clone, Serialize)]
pub struct ResourceRow {
    /// Owning squad id (for the "jump to task" navigation).
    pub squad_id: String,
    /// Squad label, if any (shown alongside the squad id).
    pub squad_label: Option<String>,
    /// Task position within the squad (matches the board's task index).
    pub task_idx: usize,
    /// Task name.
    pub task_name: String,
    /// Cell position within the task (matches the board's cell index).
    pub cell_idx: usize,
    /// Cell id.
    pub cell_id: String,
    /// The runner subprocess PID being measured.
    pub pid: u32,
    /// CPU usage as a percentage of one core (may exceed 100 for multi-threaded
    /// work). `null` when it could not be sampled.
    pub cpu_percent: Option<f64>,
    /// Resident memory in bytes, or `null` when unavailable.
    pub mem_bytes: Option<u64>,
    /// GPU memory in bytes attributed to this PID, or `null` when GPU metrics are
    /// unavailable (no `nvidia-smi`, no NVIDIA GPU, or nothing for this PID).
    pub gpu_mem_bytes: Option<u64>,
}

/// A single point-in-time reading for one process.
#[derive(Debug, Clone, Copy)]
struct ProcSnap {
    /// Cumulative CPU time consumed, in seconds.
    cpu_secs: Option<f64>,
    /// Resident memory, in bytes.
    mem_bytes: Option<u64>,
}

/// Build the resource rows for every running cell that has a live subprocess,
/// sampling CPU/RAM/GPU. Blocks for [`CPU_SAMPLE_INTERVAL`] while measuring CPU,
/// so callers must not hold the store lock across this call.
#[must_use]
pub fn build(squads: &[SquadView], procs: &ProcRegistry) -> Vec<ResourceRow> {
    let mut rows = collect_running(squads, procs);
    fill_metrics(&mut rows);
    rows
}

/// Gather the running cells that currently have a registered PID. Task and
/// cell indices are the enumerate positions the board uses for navigation.
fn collect_running(squads: &[SquadView], procs: &ProcRegistry) -> Vec<ResourceRow> {
    let mut rows = Vec::new();
    for squad in squads.iter().filter(|s| s.state == "running") {
        for (task_idx, task) in squad.tasks.iter().enumerate() {
            if task.state != "running" {
                continue;
            }
            for (cell_idx, cell) in task.cells.iter().enumerate() {
                if cell.state != "running" {
                    continue;
                }
                let Some(pid) = procs.pid_of(&squad.id, &cell.id) else {
                    continue;
                };
                rows.push(ResourceRow {
                    squad_id: squad.id.clone(),
                    squad_label: squad.label.clone(),
                    task_idx,
                    task_name: task.name.clone(),
                    cell_idx,
                    cell_id: cell.id.clone(),
                    pid,
                    cpu_percent: None,
                    mem_bytes: None,
                    gpu_mem_bytes: None,
                });
            }
        }
    }
    rows
}

/// Sample CPU/RAM (two-shot delta) and GPU for every row's PID and fill it in.
fn fill_metrics(rows: &mut [ResourceRow]) {
    if rows.is_empty() {
        return;
    }
    let pids: Vec<u32> = rows.iter().map(|r| r.pid).collect();
    let (cpu, mem) = sample_cpu_mem(&pids);
    let gpu = sample_gpu(&pids);
    for row in rows.iter_mut() {
        row.cpu_percent = cpu.get(&row.pid).copied().flatten();
        row.mem_bytes = mem.get(&row.pid).copied().flatten();
        row.gpu_mem_bytes = gpu.get(&row.pid).copied();
    }
}

/// Two reads of cumulative CPU time, [`CPU_SAMPLE_INTERVAL`] apart, converted to a
/// percentage of one core; RAM is taken from the second read. Returns
/// `(pid -> cpu%, pid -> mem_bytes)`.
type CpuMap = HashMap<u32, Option<f64>>;
type MemMap = HashMap<u32, Option<u64>>;

fn sample_cpu_mem(pids: &[u32]) -> (CpuMap, MemMap) {
    let first = read_procs(pids);
    std::thread::sleep(CPU_SAMPLE_INTERVAL);
    let second = read_procs(pids);
    let secs = CPU_SAMPLE_INTERVAL.as_secs_f64();
    let mut cpu = HashMap::new();
    let mut mem = HashMap::new();
    for &pid in pids {
        let a = first.get(&pid);
        let b = second.get(&pid);
        let percent = match (a.and_then(|s| s.cpu_secs), b.and_then(|s| s.cpu_secs)) {
            (Some(t0), Some(t1)) => Some(((t1 - t0).max(0.0) / secs) * 100.0),
            _ => None,
        };
        cpu.insert(pid, percent);
        // Prefer the latest RAM reading; fall back to the first if the process
        // vanished between samples.
        mem.insert(
            pid,
            b.and_then(|s| s.mem_bytes)
                .or_else(|| a.and_then(|s| s.mem_bytes)),
        );
    }
    (cpu, mem)
}

// ── Linux: read /proc directly (zero dependencies) ────────────────────────────

#[cfg(target_os = "linux")]
fn read_procs(pids: &[u32]) -> HashMap<u32, ProcSnap> {
    let mut out = HashMap::new();
    for &pid in pids {
        let cpu_secs = std::fs::read_to_string(format!("/proc/{pid}/stat"))
            .ok()
            .and_then(|s| parse_linux_stat_cpu_secs(&s));
        let mem_bytes = std::fs::read_to_string(format!("/proc/{pid}/statm"))
            .ok()
            .and_then(|s| parse_linux_statm_rss_bytes(&s));
        if cpu_secs.is_some() || mem_bytes.is_some() {
            out.insert(
                pid,
                ProcSnap {
                    cpu_secs,
                    mem_bytes,
                },
            );
        }
    }
    out
}

/// Cumulative CPU seconds (utime + stime) from a `/proc/<pid>/stat` line.
///
/// The command field (field 2) is wrapped in parentheses and may itself contain
/// spaces or `)`, so we split *after the last* `)`. In the remainder, `state` is
/// the first field; `utime`/`stime` are stat fields 14/15, i.e. indices 11/12
/// here. Ticks are converted with the conventional `USER_HZ` of 100.
#[cfg(any(target_os = "linux", test))]
fn parse_linux_stat_cpu_secs(stat: &str) -> Option<f64> {
    const USER_HZ: f64 = 100.0;
    let after = stat.rsplit_once(')').map(|(_, rest)| rest.trim())?;
    let fields: Vec<&str> = after.split_whitespace().collect();
    let utime: f64 = fields.get(11)?.parse().ok()?;
    let stime: f64 = fields.get(12)?.parse().ok()?;
    Some((utime + stime) / USER_HZ)
}

/// Resident memory in bytes from a `/proc/<pid>/statm` line (field 2 = resident
/// pages, at the conventional 4 KiB page size).
#[cfg(any(target_os = "linux", test))]
fn parse_linux_statm_rss_bytes(statm: &str) -> Option<u64> {
    const PAGE_SIZE: u64 = 4096;
    let resident_pages: u64 = statm.split_whitespace().nth(1)?.parse().ok()?;
    Some(resident_pages * PAGE_SIZE)
}

// ── Windows: shell out to PowerShell's Get-Process (zero dependencies) ─────────

#[cfg(target_os = "windows")]
fn read_procs(pids: &[u32]) -> HashMap<u32, ProcSnap> {
    if pids.is_empty() {
        return HashMap::new();
    }
    let ids = pids
        .iter()
        .map(u32::to_string)
        .collect::<Vec<_>>()
        .join(",");
    // One line per still-alive PID: "pid,cpu_seconds,working_set_bytes". `$_.CPU`
    // is null very early in a process's life; it then formats as empty and parses
    // back to a missing CPU sample (percentage stays null for that tick).
    let script = format!(
        "Get-Process -Id {ids} -ErrorAction SilentlyContinue | ForEach-Object {{ '{{0}},{{1}},{{2}}' -f $_.Id, [double]$_.CPU, [int64]$_.WorkingSet64 }}"
    );
    let output = std::process::Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-Command", &script])
        .output();
    match output {
        Ok(o) if o.status.success() => parse_windows_procs(&String::from_utf8_lossy(&o.stdout)),
        _ => HashMap::new(),
    }
}

/// Parse the `pid,cpu_seconds,working_set_bytes` lines emitted by the Windows
/// `Get-Process` reader. A PID that fails to parse is skipped; a blank CPU or
/// memory field becomes a missing (`None`) reading rather than an error.
#[cfg(any(target_os = "windows", test))]
fn parse_windows_procs(text: &str) -> HashMap<u32, ProcSnap> {
    let mut out = HashMap::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let mut parts = line.split(',');
        let Some(pid) = parts.next().and_then(|s| s.trim().parse::<u32>().ok()) else {
            continue;
        };
        let cpu_secs = parts.next().and_then(|s| s.trim().parse::<f64>().ok());
        let mem_bytes = parts.next().and_then(|s| s.trim().parse::<u64>().ok());
        out.insert(
            pid,
            ProcSnap {
                cpu_secs,
                mem_bytes,
            },
        );
    }
    out
}

// ── Fallback: unknown OS yields nothing (all metrics degrade to null) ─────────

#[cfg(not(any(target_os = "linux", target_os = "windows")))]
fn read_procs(_pids: &[u32]) -> HashMap<u32, ProcSnap> {
    HashMap::new()
}

// ── GPU: best-effort via nvidia-smi ───────────────────────────────────────────

/// GPU memory per PID via `nvidia-smi`. Any failure (tool absent, no NVIDIA GPU,
/// non-zero exit) yields an empty map, so every row's GPU degrades to `null`.
fn sample_gpu(pids: &[u32]) -> HashMap<u32, u64> {
    if pids.is_empty() {
        return HashMap::new();
    }
    let output = std::process::Command::new("nvidia-smi")
        .args([
            "--query-compute-apps=pid,used_memory",
            "--format=csv,noheader,nounits",
        ])
        .output();
    let by_pid = match output {
        Ok(o) if o.status.success() => parse_nvidia_smi(&String::from_utf8_lossy(&o.stdout)),
        _ => return HashMap::new(),
    };
    // Only keep GPU usage for PIDs we actually track.
    let wanted: std::collections::HashSet<u32> = pids.iter().copied().collect();
    by_pid
        .into_iter()
        .filter(|(pid, _)| wanted.contains(pid))
        .collect()
}

/// Parse `nvidia-smi --query-compute-apps=pid,used_memory` CSV (memory in MiB)
/// into `pid -> bytes`. Rows whose PID or memory is non-numeric (e.g. `[N/A]`)
/// are skipped rather than erroring.
fn parse_nvidia_smi(text: &str) -> HashMap<u32, u64> {
    let mut out = HashMap::new();
    for line in text.lines() {
        let mut parts = line.split(',');
        let pid = parts.next().and_then(|s| s.trim().parse::<u32>().ok());
        let mib = parts.next().and_then(|s| s.trim().parse::<u64>().ok());
        if let (Some(pid), Some(mib)) = (pid, mib) {
            out.insert(pid, mib * 1024 * 1024);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Exercises the real OS sampling path (the /proc reader on Linux, the
    /// PowerShell reader on Windows) against this test process. On platforms we
    /// don't implement, sampling is allowed to yield nothing.
    #[test]
    fn samples_the_current_process_ram() {
        let me = std::process::id();
        let (cpu, mem) = sample_cpu_mem(&[me]);
        assert!(cpu.contains_key(&me));
        assert!(mem.contains_key(&me));
        #[cfg(any(target_os = "linux", target_os = "windows"))]
        assert!(
            mem[&me].is_some_and(|b| b > 0),
            "the current process should report a non-zero RSS"
        );
    }

    #[test]
    fn linux_stat_cpu_handles_comm_with_spaces_and_parens() {
        // comm = "(weird ) name)"; utime/stime are stat fields 14/15.
        // Build: pid (comm) state ppid pgrp session tty tpgid flags minflt
        //        cminflt majflt cmajflt utime stime ...
        let stat = "1234 ((weird ) name)) R 1 1 1 0 -1 0 0 0 0 0 250 150 0 0";
        // fields after last ')': state=R, then ... utime=250 (idx11), stime=150 (idx12)
        let secs = parse_linux_stat_cpu_secs(stat).unwrap();
        assert!((secs - 4.0).abs() < 1e-9, "got {secs}"); // (250+150)/100
    }

    #[test]
    fn linux_stat_cpu_rejects_garbage() {
        assert_eq!(parse_linux_stat_cpu_secs("no parens here"), None);
        assert_eq!(parse_linux_stat_cpu_secs("1 (x) R 1 2 3"), None); // too few fields
    }

    #[test]
    fn linux_statm_rss_is_pages_times_page_size() {
        // size resident shared text lib data dt
        assert_eq!(
            parse_linux_statm_rss_bytes("1000 42 10 1 0 20 0"),
            Some(42 * 4096)
        );
        assert_eq!(parse_linux_statm_rss_bytes(""), None);
        assert_eq!(parse_linux_statm_rss_bytes("1000"), None);
    }

    #[test]
    fn windows_procs_parses_and_tolerates_blanks() {
        let text = "4242,12.5,10485760\n99,,2048\nbad,1,2\n";
        let m = parse_windows_procs(text);
        assert_eq!(m.len(), 2, "the non-numeric pid line is skipped");
        assert_eq!(m[&4242].cpu_secs, Some(12.5));
        assert_eq!(m[&4242].mem_bytes, Some(10_485_760));
        // Blank CPU -> missing reading, memory still parsed.
        assert_eq!(m[&99].cpu_secs, None);
        assert_eq!(m[&99].mem_bytes, Some(2048));
    }

    #[test]
    fn nvidia_smi_parses_mib_to_bytes_and_skips_na() {
        let text = "1234, 512\n5678, 1024\n9999, [N/A]\n";
        let m = parse_nvidia_smi(text);
        assert_eq!(m[&1234], 512 * 1024 * 1024);
        assert_eq!(m[&5678], 1024 * 1024 * 1024);
        assert!(!m.contains_key(&9999), "the [N/A] row is skipped");
    }

    #[test]
    fn nvidia_smi_empty_output_is_empty_map() {
        assert!(parse_nvidia_smi("").is_empty());
    }
}
