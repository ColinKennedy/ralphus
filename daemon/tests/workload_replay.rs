//! WS-B.6 workload replayer: the daemon's real recorded event mix, replayed
//! at its real recorded rates against a seeded store, so every performance
//! number in the plan can be reproduced instead of recalled.
//!
//! # Where the numbers come from
//!
//! [`EVENT_MIX`] and [`MEAN_RATE_PER_SEC`]/[`SUSTAINED_PEAK_PER_SEC`]/
//! [`BURST_PEAK_PER_SEC`] were measured directly from the `cartographer_events`
//! table of the captured 39 MB production database: 54,067 rows across 10.07
//! hours (150 squads, 484 tasks, 899 cells, 144 guardians). They are baked in
//! as constants rather than read from a database at run time so the replay is
//! byte-for-byte reproducible on any machine and in CI.
//!
//! The mix is dominated to an almost comical degree by one call site: 84% of
//! every Cartographer row the daemon wrote in those ten hours came from
//! `source = "guardian"`, and 44,161 of those 45,522 rows -- **81.7% of all
//! write traffic** -- were the single message `"restack deferred: branch
//! worktree leased"`, emitted once per iteration of `drive_rebase`'s 25 ms
//! worktree-lease poll. A 25 ms poll is 40 iterations/sec, which is exactly
//! the 41 writes/sec "peak burst" in the plan's baseline. The peak was never
//! user load; it was one spin loop narrating itself into the database, and
//! each of those rows also published an SSE event that made every connected
//! board re-hydrate.
//!
//! The replay reproduces that shape on purpose. A synthetic uniform mix would
//! be a different workload and would not exercise what actually hurts.
//!
//! # What it asserts
//!
//! That the store keeps up: the achieved write rate tracks the requested rate,
//! every requested row lands, board reads stay responsive alongside the
//! writes, and the store lock's own wait distribution stays bounded. Writes go
//! through a real [`StoreMutex`], so the lock, the WS-B.3 guard watchdog and
//! the wait histogram are all in the measurement.
//!
//! Set `RALPHUS_REPLAY_SECONDS` to lengthen the window -- that is the knob the
//! WS-G.5 soak turns.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use ralphus_daemon::cartographer::{CartographerEntry, CartographerFilter};
use ralphus_daemon::logging::LogLevel;
use ralphus_daemon::store::Store;
use ralphus_daemon::store_lock::StoreMutex;

/// The measured source distribution, in parts per thousand, summing to 1000.
/// Sources below 0.1% in the capture are folded into the tail entries.
const EVENT_MIX: &[(&str, u32)] = &[
    ("guardian", 842),
    ("pr", 82),
    ("store_lock", 47),
    ("ci-watch", 10),
    ("runner", 9),
    ("store", 7),
    ("scheduler", 3),
];

/// The measured level distribution, in parts per thousand.
const LEVEL_MIX: &[(LogLevel, u32)] = &[
    (LogLevel::INFO, 918),
    (LogLevel::WARNING, 52),
    (LogLevel::DEBUG, 30),
];

/// Mean write rate over the captured 10.07 hours.
const MEAN_RATE_PER_SEC: f64 = 1.49;
/// Highest rate sustained over any 10 s window in the capture.
const SUSTAINED_PEAK_PER_SEC: f64 = 31.0;
/// Highest rate in any 1 s window in the capture -- `drive_rebase`'s 25 ms
/// poll loop running flat out.
const BURST_PEAK_PER_SEC: f64 = 41.0;

/// Squads seeded before a replay, so reads have realistic hydration work.
const SEED_SQUADS: usize = 20;

/// Default replay window. Long enough for the pacing measurement to mean
/// something, short enough for CI. `RALPHUS_REPLAY_SECONDS` overrides it.
const DEFAULT_SECONDS: f64 = 5.0;

/// How far below the requested rate the achieved rate may fall before the
/// store is declared unable to keep up. The replayer paces itself by sleeping
/// between writes, so falling short means the writes themselves ran long.
const MIN_PACING_FRACTION: f64 = 0.90;

/// p95 budget for a board read taken concurrently with the replay.
const READ_P95_BUDGET_MS: u128 = 200;

/// M3 budget for the store lock's own wait p95 during a replay.
const LOCK_WAIT_P95_BUDGET_MS: f64 = 200.0;

fn replay_seconds() -> f64 {
    std::env::var("RALPHUS_REPLAY_SECONDS")
        .ok()
        .and_then(|s| s.parse::<f64>().ok())
        .filter(|s| *s > 0.0)
        .unwrap_or(DEFAULT_SECONDS)
}

fn temp_db(tag: &str) -> PathBuf {
    static N: AtomicU32 = AtomicU32::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("ralphus-replay-{tag}-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("mkdir");
    dir.join("tasks.db")
}

/// A deterministic 64-bit LCG. Reproducibility is the whole point of this
/// file, so the event stream must not depend on a thread-local RNG seeded from
/// the clock.
struct Lcg(u64);

impl Lcg {
    fn new(seed: u64) -> Self {
        Self(seed | 1)
    }

    fn next_u32(&mut self) -> u32 {
        // Numerical Recipes' constants; adequate for choosing between seven
        // buckets and cheap enough not to distort the pacing measurement.
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        (self.0 >> 33) as u32
    }

    /// Picks from a parts-per-thousand weighted table.
    fn pick<'t, T>(&mut self, table: &'t [(T, u32)]) -> &'t T {
        let roll = self.next_u32() % 1000;
        let mut acc = 0;
        for (value, weight) in table {
            acc += weight;
            if roll < acc {
                return value;
            }
        }
        // Rounding leaves a few parts per thousand unallocated; the tail
        // catches them rather than the arm above panicking on an edge roll.
        &table[table.len() - 1].0
    }
}

/// One replayed event, shaped like the rows the capture actually contains.
fn write_one(store: &StoreMutex, rng: &mut Lcg, squad_ids: &[String], n: u64) {
    let source = *rng.pick(EVENT_MIX);
    let level = *rng.pick(LEVEL_MIX);
    let squad_id = &squad_ids[(n as usize) % squad_ids.len()];
    // The dominant row in the capture, reproduced verbatim: `drive_rebase`'s
    // poll message accounts for 81.7% of all rows, and `source = "guardian"`
    // rows all carry `scope = "branch"` with a `branch_id`/`owner` payload.
    let (message, scope, payload) = if source == "guardian" {
        (
            "restack deferred: branch worktree leased",
            Some("branch"),
            serde_json::json!({"branch_id": n % 8, "owner": "replay"}),
        )
    } else {
        (
            "replayed event",
            None,
            serde_json::json!({"seq": n, "source": source}),
        )
    };
    let guard = store.lock();
    let _ = guard.cartographer_log(CartographerEntry {
        level,
        source,
        message,
        scope,
        squad_id: Some(squad_id),
        guardian_id: None,
        cell_id: None,
        task: None,
        log_path: None,
        payload,
        admin_only: false,
    });
}

fn seed(store: &StoreMutex) -> Vec<String> {
    let mut ids = Vec::with_capacity(SEED_SQUADS);
    for i in 0..SEED_SQUADS {
        let toml = format!(
            "[[task]]\nname=\"task-{i}\"\n[[task.cell]]\ncwd=\"/repo\"\nprompt=\"replay {i}\"\n"
        );
        let file: ralphus_core::schema::TaskFile =
            toml::from_str(&toml).expect("generated task toml parses");
        let id = store
            .lock()
            .insert_squad(&file, Some(&format!("replay-{i}")), false)
            .expect("seed squad");
        ids.push(id);
    }
    ids
}

struct ReplayResult {
    requested: u64,
    written: u64,
    achieved_rate: f64,
}

/// Replays events at `rate_per_sec` for `seconds`, pacing with a sleep so the
/// *requested* rate is what the capture recorded rather than "as fast as
/// possible". Returns how many it managed.
fn replay(
    store: &StoreMutex,
    squad_ids: &[String],
    rate_per_sec: f64,
    seconds: f64,
) -> ReplayResult {
    let requested = (rate_per_sec * seconds).round() as u64;
    let interval = Duration::from_secs_f64(1.0 / rate_per_sec);
    let mut rng = Lcg::new(0x5eed_1234);
    let started = Instant::now();
    let mut written = 0u64;
    for n in 0..requested {
        write_one(store, &mut rng, squad_ids, n);
        written += 1;
        // Pace against the start, not against the previous write: sleeping a
        // fixed interval after each write would accumulate the write cost into
        // the schedule and understate the achieved rate.
        let due = Duration::from_secs_f64((n + 1) as f64 / rate_per_sec);
        if let Some(slack) = due.checked_sub(started.elapsed()) {
            thread::sleep(slack.min(interval));
        }
    }
    ReplayResult {
        requested,
        written,
        achieved_rate: (written as f64) / started.elapsed().as_secs_f64(),
    }
}

fn percentile(samples: &mut [u128], fraction: f64) -> u128 {
    if samples.is_empty() {
        return 0;
    }
    samples.sort_unstable();
    let idx = (((samples.len() as f64) * fraction).ceil() as usize)
        .saturating_sub(1)
        .min(samples.len() - 1);
    samples[idx]
}

/// The total Cartographer row count. `cartographer_query` reports `total`
/// independently of `limit`, so a one-row page is the cheapest count available
/// without adding a store method for the tests' benefit alone.
fn carto_rows(store: &StoreMutex) -> i64 {
    store
        .lock()
        .cartographer_query(&CartographerFilter::recent(1))
        .expect("count cartographer rows")
        .total
}

#[test]
// Wall-clock pacing, so this belongs in the `perf-tests` job with
// `--test-threads 1` -- same reasoning as `board_contention.rs`.
#[ignore = "perf budget; run via the perf-tests job (--run-ignored only --test-threads 1)"]
fn the_captured_event_mix_replays_at_its_recorded_peak() {
    let db = temp_db("peak");
    let store = StoreMutex::new(Store::open(&db).expect("open store"));
    let squad_ids = seed(&store);
    let before = carto_rows(&store);

    let seconds = replay_seconds();
    let result = replay(&store, &squad_ids, BURST_PEAK_PER_SEC, seconds);
    let after = carto_rows(&store);

    eprintln!(
        "replay burst-peak: requested {} at {BURST_PEAK_PER_SEC}/sec over {seconds}s, \
         achieved {:.1}/sec, rows +{}",
        result.requested,
        result.achieved_rate,
        after - before
    );

    assert_eq!(
        result.written, result.requested,
        "the replayer dropped writes"
    );
    assert_eq!(
        after - before,
        result.requested as i64,
        "requested {} rows but the store holds {} more -- writes were lost",
        result.requested,
        after - before
    );
    assert!(
        result.achieved_rate >= BURST_PEAK_PER_SEC * MIN_PACING_FRACTION,
        "achieved {:.1}/sec against a requested {BURST_PEAK_PER_SEC}/sec -- the \
         store cannot keep up with its own recorded peak burst",
        result.achieved_rate
    );

    drop(store);
    let _ = std::fs::remove_dir_all(db.parent().expect("db dir"));
}

#[test]
#[ignore = "perf budget; run via the perf-tests job (--run-ignored only --test-threads 1)"]
fn board_reads_stay_responsive_through_a_replay() {
    let db = temp_db("reads");
    let store = std::sync::Arc::new(StoreMutex::new(Store::open(&db).expect("open store")));
    let squad_ids = seed(&store);
    let seconds = replay_seconds();

    // The replay runs at the sustained peak rather than the 1 s burst: this
    // test is about what the board experiences during a busy stretch, and the
    // 10 s sustained figure is the one that lasts long enough to be felt.
    let reads_done = std::sync::Arc::new(AtomicU64::new(0));
    let reader = {
        let store = std::sync::Arc::clone(&store);
        let reads_done = std::sync::Arc::clone(&reads_done);
        let deadline = Instant::now() + Duration::from_secs_f64(seconds);
        thread::spawn(move || {
            let mut samples: Vec<u128> = Vec::new();
            while Instant::now() < deadline {
                let started = Instant::now();
                let squads = store.lock().list_squads().expect("list squads");
                samples.push(started.elapsed().as_millis());
                reads_done.fetch_add(1, Ordering::Relaxed);
                assert!(!squads.is_empty(), "seeded squads vanished mid-replay");
            }
            samples
        })
    };

    let result = replay(&store, &squad_ids, SUSTAINED_PEAK_PER_SEC, seconds);
    let mut samples = reader.join().expect("reader thread");

    let p50 = percentile(&mut samples, 0.50);
    let p95 = percentile(&mut samples, 0.95);
    let max = samples.last().copied().unwrap_or(0);
    let wait = ralphus_daemon::store_lock::store_lock_wait_snapshot();
    eprintln!(
        "replay sustained-peak: {:.1}/sec writes, {} board reads, \
         read p50={p50}ms p95={p95}ms max={max}ms, \
         lock_wait p50={}ms p95={}ms max={}ms",
        result.achieved_rate,
        samples.len(),
        wait.p50_ms,
        wait.p95_ms,
        wait.max_ms
    );

    assert!(
        samples.len() >= 10,
        "only {} board reads completed; the read side was starved",
        samples.len()
    );
    assert!(
        p95 <= READ_P95_BUDGET_MS,
        "board read p95 {p95}ms (p50 {p50}ms, max {max}ms) during a \
         {SUSTAINED_PEAK_PER_SEC}/sec replay exceeds the {READ_P95_BUDGET_MS}ms budget"
    );
    assert!(
        wait.p95_ms <= LOCK_WAIT_P95_BUDGET_MS,
        "store-lock wait p95 {}ms during a {SUSTAINED_PEAK_PER_SEC}/sec replay \
         exceeds the {LOCK_WAIT_P95_BUDGET_MS}ms budget",
        wait.p95_ms
    );

    // Close the store before removing its directory: the reader thread has
    // joined, so this is the last `Arc` and dropping it drops the connection.
    drop(std::sync::Arc::into_inner(store));
    let _ = std::fs::remove_dir_all(db.parent().expect("db dir"));
}

#[test]
#[ignore = "perf budget; run via the perf-tests job (--run-ignored only --test-threads 1)"]
fn the_mean_rate_is_far_below_what_the_store_sustains() {
    let db = temp_db("mean");
    let store = StoreMutex::new(Store::open(&db).expect("open store"));
    let squad_ids = seed(&store);

    // One second of the *mean* recorded load, timed, then compared against how
    // long the same writes take unpaced. The ratio is the store's headroom at
    // the load it actually sees, and it is the number that makes the plan's
    // central point: the daemon hangs at a small fraction of its capacity.
    let count = (MEAN_RATE_PER_SEC * 10.0).round() as u64;
    let mut rng = Lcg::new(0xfeed_5678);
    let started = Instant::now();
    for n in 0..count {
        write_one(&store, &mut rng, &squad_ids, n);
    }
    let unpaced_rate = (count as f64) / started.elapsed().as_secs_f64();
    let headroom = unpaced_rate / MEAN_RATE_PER_SEC;
    eprintln!(
        "replay mean-load headroom: store sustains {unpaced_rate:.0}/sec vs a \
         recorded mean of {MEAN_RATE_PER_SEC}/sec ({headroom:.0}x)"
    );

    assert!(
        headroom >= 20.0,
        "the store sustains only {unpaced_rate:.0}/sec against a recorded mean \
         load of {MEAN_RATE_PER_SEC}/sec ({headroom:.0}x headroom) -- at that \
         margin the daemon's slowness really is write capacity, which would \
         invalidate the plan's diagnosis"
    );

    drop(store);
    let _ = std::fs::remove_dir_all(db.parent().expect("db dir"));
}
