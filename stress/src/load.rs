//! The closed-loop load engine: `users` virtual users, each on its own thread
//! with its own keep-alive connection, issuing requests back to back (or
//! with a think time) for a fixed window.

use crate::api::{Api, Op, Rng, agent};
use crate::stats::{Sample, WindowStats, summarize};
use serde::Serialize;
use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};
use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System};

/// Weighted operation mix, restricted to the ops the daemon supports.
#[derive(Clone, Debug, Serialize)]
pub struct Mix {
    pub name: String,
    pub ops: Vec<(Op, u32)>,
    #[serde(skip)]
    total: u32,
}

impl Mix {
    pub fn new(name: &str, weights: &[(Op, u32)], available: &[Op]) -> Mix {
        let ops: Vec<(Op, u32)> = weights
            .iter()
            .copied()
            .filter(|(op, _)| available.contains(op))
            .collect();
        let total = ops.iter().map(|(_, w)| w).sum();
        Mix {
            name: name.to_string(),
            ops,
            total,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.total == 0
    }

    pub fn pick(&self, rng: &mut Rng) -> Op {
        let mut r = (rng.next() % u64::from(self.total.max(1))) as u32;
        for (op, w) in &self.ops {
            if r < *w {
                return *op;
            }
            r -= w;
        }
        self.ops[0].0
    }
}

pub struct WindowRun {
    pub samples: Vec<Sample>,
    pub examples: BTreeMap<String, String>,
    pub users_started: usize,
    pub users_failed_to_start: usize,
}

impl WindowRun {
    pub fn stats(&self, window: Duration) -> WindowStats {
        summarize(&self.samples, window.as_secs_f64(), &self.examples)
    }
}

pub struct LoadSpec {
    pub users: usize,
    pub duration: Duration,
    pub think: Duration,
    /// Offset added to each VU index when choosing a user identity, so two
    /// concurrent loads use different users.
    pub user_offset: usize,
}

/// Run `spec.users` virtual users against `api` with `mix` until the window
/// ends or `stop` is raised.
pub fn run(api: &Arc<Api>, mix: &Arc<Mix>, spec: &LoadSpec, stop: &Arc<AtomicBool>) -> WindowRun {
    let start = Instant::now();
    let deadline = start + spec.duration;
    let mut handles = Vec::with_capacity(spec.users);
    let mut failed = 0;
    for vu in 0..spec.users {
        let api = Arc::clone(api);
        let mix = Arc::clone(mix);
        let stop = Arc::clone(stop);
        let think = spec.think;
        let user_idx = vu + spec.user_offset;
        let h = thread::Builder::new()
            .name(format!("vu-{vu}"))
            .stack_size(256 * 1024)
            .spawn(move || vu_loop(&api, &mix, user_idx, start, deadline, think, &stop));
        match h {
            Ok(h) => handles.push(h),
            Err(_) => failed += 1,
        }
    }
    let started = handles.len();
    let mut samples = Vec::new();
    let mut examples = BTreeMap::new();
    for h in handles {
        if let Ok((s, ex)) = h.join() {
            samples.extend(s);
            for (k, v) in ex {
                examples.entry(k).or_insert(v);
            }
        }
    }
    WindowRun {
        samples,
        examples,
        users_started: started,
        users_failed_to_start: failed,
    }
}

fn vu_loop(
    api: &Api,
    mix: &Mix,
    user_idx: usize,
    start: Instant,
    deadline: Instant,
    think: Duration,
    stop: &AtomicBool,
) -> (Vec<Sample>, BTreeMap<String, String>) {
    let ag = agent(api.timeout);
    let mut rng = Rng::new(user_idx as u64 + 1 + start.elapsed().as_nanos() as u64);
    let user = api.user_for(user_idx);
    let mut samples = Vec::new();
    let mut examples = BTreeMap::new();
    while Instant::now() < deadline && !stop.load(Ordering::Relaxed) {
        let op = mix.pick(&mut rng);
        let t0 = Instant::now();
        let reply = api.exec(&ag, user, op, &mut rng);
        let lat = t0.elapsed();
        if let Some(err) = reply.error.filter(|_| !reply.outcome.is_ok()) {
            examples.entry(reply.outcome.class()).or_insert(err);
        }
        samples.push(Sample {
            op: op.name(),
            start_us: t0.duration_since(start).as_micros() as u64,
            latency_us: lat.as_micros() as u64,
            outcome: reply.outcome,
            bytes: reply.bytes,
        });
        if !think.is_zero() {
            thread::sleep(think);
        }
    }
    (samples, examples)
}

/// Peak/average CPU and memory of the daemon and of this generator process
/// over a window. CPU is percent of one core (so it can exceed 100).
#[derive(Serialize, Clone, Debug, Default)]
pub struct Resources {
    pub daemon_peak_rss_mb: f64,
    pub daemon_avg_cpu_pct: f64,
    pub daemon_peak_cpu_pct: f64,
    pub generator_avg_cpu_pct: f64,
    pub samples: u32,
}

pub struct Sampler {
    stop: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<Resources>>,
}

impl Sampler {
    pub fn start(daemon_pid: u32) -> Sampler {
        let stop = Arc::new(AtomicBool::new(false));
        let s2 = Arc::clone(&stop);
        let handle = thread::Builder::new()
            .name("resource-sampler".into())
            .spawn(move || sample_loop(daemon_pid, &s2))
            .ok();
        Sampler { stop, handle }
    }

    pub fn finish(mut self) -> Resources {
        self.stop.store(true, Ordering::Relaxed);
        self.handle
            .take()
            .and_then(|h| h.join().ok())
            .unwrap_or_default()
    }
}

fn sample_loop(daemon_pid: u32, stop: &AtomicBool) -> Resources {
    let mut sys = System::new();
    let dpid = Pid::from_u32(daemon_pid);
    let me = Pid::from_u32(std::process::id());
    let kind = ProcessRefreshKind::nothing().with_cpu().with_memory();
    let pids = [dpid, me];
    let mut r = Resources::default();
    let (mut d_cpu_sum, mut g_cpu_sum) = (0.0, 0.0);
    sys.refresh_processes_specifics(ProcessesToUpdate::Some(&pids), true, kind);
    while !stop.load(Ordering::Relaxed) {
        thread::sleep(Duration::from_millis(500));
        sys.refresh_processes_specifics(ProcessesToUpdate::Some(&pids), true, kind);
        if let Some(p) = sys.process(dpid) {
            let cpu = f64::from(p.cpu_usage());
            r.daemon_peak_rss_mb = r
                .daemon_peak_rss_mb
                .max(p.memory() as f64 / (1024.0 * 1024.0));
            r.daemon_peak_cpu_pct = r.daemon_peak_cpu_pct.max(cpu);
            d_cpu_sum += cpu;
        }
        if let Some(p) = sys.process(me) {
            g_cpu_sum += f64::from(p.cpu_usage());
        }
        r.samples += 1;
    }
    if r.samples > 0 {
        r.daemon_avg_cpu_pct = d_cpu_sum / f64::from(r.samples);
        r.generator_avg_cpu_pct = g_cpu_sum / f64::from(r.samples);
    }
    r
}

/// Shared handle for loads that run alongside something else (fault
/// injection): start it, do the other thing, then collect.
pub struct Background {
    stop: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<WindowRun>>,
}

impl Background {
    pub fn start(api: &Arc<Api>, mix: &Arc<Mix>, spec: LoadSpec) -> Background {
        let stop = Arc::new(AtomicBool::new(false));
        let (api, mix, s2) = (Arc::clone(api), Arc::clone(mix), Arc::clone(&stop));
        let handle = thread::Builder::new()
            .name("victim-load".into())
            .spawn(move || run(&api, &mix, &spec, &s2))
            .ok();
        Background { stop, handle }
    }

    pub fn stop_now(&self) {
        self.stop.store(true, Ordering::Relaxed);
    }

    pub fn join(mut self) -> Option<WindowRun> {
        self.handle.take().and_then(|h| h.join().ok())
    }
}

/// Collect id values a load produced, guarded so a poisoned lock never
/// aborts the report.
pub fn snapshot_ids(api: &Api) -> Vec<String> {
    api.ids.read().map(|v| v.clone()).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mix_drops_unavailable_ops_and_picks_by_weight() {
        let m = Mix::new(
            "t",
            &[(Op::Board, 3), (Op::Submit, 1), (Op::Guardians, 5)],
            &[Op::Board, Op::Submit],
        );
        assert_eq!(m.ops.len(), 2);
        let mut rng = Rng::new(42);
        let mut boards = 0;
        for _ in 0..4000 {
            if m.pick(&mut rng) == Op::Board {
                boards += 1;
            }
        }
        assert!((2700..3300).contains(&boards), "{boards}");
    }
}
