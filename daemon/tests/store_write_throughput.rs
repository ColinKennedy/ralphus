//! WS-B.5 write throughput: how many store writes per second the daemon's
//! single writer connection actually sustains, asserted against a floor.
//!
//! This is a `#[test]`, not a `ralphus-bench-harness` benchmark, on purpose.
//! The harness is in-process, serial and single-threaded and has **no
//! pass/fail** -- it reports a durable minimum for a human to read. What the
//! performance plan needs is a gate: a number CI refuses to let regress. The
//! floors below are ratchets; when a configuration change raises the measured
//! rate, raise the floor with it.
//!
//! The workload is [`Store::cartographer_log`], chosen because it is the
//! highest-volume write in production (54,040 rows against 150 squads in the
//! captured database) and is one `INSERT` with no read-modify-write, so the
//! number it produces is the writer's commit cost and nothing else.
//!
//! The store is file-backed: the whole point is to measure `journal_mode=WAL`
//! plus whatever `synchronous` the writer is configured with, and an
//! in-memory database has no fsync to measure.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Instant;

use ralphus_daemon::cartographer::CartographerEntry;
use ralphus_daemon::logging::LogLevel;
use ralphus_daemon::store::Store;

/// Writes per measured run. Large enough that per-write cost dominates
/// setup, small enough that even the un-tuned `synchronous=FULL` floor
/// finishes in a couple of seconds.
const WRITES: usize = 2_000;

/// Minimum sustained single-threaded write rate, in writes/sec.
///
/// Measured here at 404/sec, which is an fsync per commit and nothing else:
/// `synchronous` defaults to `FULL`. Raising this floor is WS-C's job --
/// `synchronous=NORMAL` is what unlocks the plan's 5,000/sec M10 target, and
/// no amount of work elsewhere in the daemon moves this number while every
/// commit waits on the disk. The floor sits well under the measurement so a
/// slower CI disk does not turn a real gate into a flaky one.
const MIN_WRITES_PER_SEC: f64 = 250.0;

/// Minimum rate when the same writes are wrapped in one explicit transaction.
///
/// Batching amortizes the one commit over the whole batch, which is why it is
/// ~40x the unbatched rate even under `synchronous=FULL` -- and why it is the
/// number WS-F.3's batched inbox is reaching for. Gating it here means the
/// benefit is measured before the restructuring that exploits it, not claimed
/// afterwards. Measured at 17,343/sec against the real schema, well under the
/// 166,669/sec the plan's simplified-schema estimate suggested.
const MIN_BATCHED_WRITES_PER_SEC: f64 = 10_000.0;

fn temp_db(tag: &str) -> PathBuf {
    static N: AtomicU32 = AtomicU32::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "ralphus-writebench-{tag}-{}-{n}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("mkdir");
    dir.join("tasks.db")
}

fn entry<'a>(message: &'a str, payload: &'a serde_json::Value) -> CartographerEntry<'a> {
    CartographerEntry {
        level: LogLevel::INFO,
        source: "write_throughput",
        message,
        scope: None,
        squad_id: Some("squad-000000000001"),
        guardian_id: None,
        cell_id: None,
        task: None,
        log_path: None,
        payload: payload.clone(),
        admin_only: false,
    }
}

/// Runs `WRITES` `cartographer_log` calls and returns the achieved rate.
fn measure(store: &Store) -> f64 {
    // A payload with some real shape to it: the production rows carry JSON,
    // and `payload.to_string()` is part of the per-write cost.
    let payload = serde_json::json!({"phase": "measure", "detail": "one representative row"});
    let started = Instant::now();
    for i in 0..WRITES {
        let message = format!("throughput sample {i}");
        store
            .cartographer_log(entry(&message, &payload))
            .expect("cartographer write");
    }
    let elapsed = started.elapsed();
    (WRITES as f64) / elapsed.as_secs_f64()
}

#[test]
// Wall-clock throughput, so this belongs in the `perf-tests` job with
// `--test-threads 1` rather than sharing cores with the workspace suite --
// same arrangement as `board_cold_load_perf.rs` and `board_contention.rs`.
#[ignore = "perf budget; run via the perf-tests job (--run-ignored only --test-threads 1)"]
fn writer_sustains_the_throughput_floor() {
    let db = temp_db("unbatched");
    let store = Store::open(&db).expect("open store");

    // A warm-up run: the first writes pay for schema pages entering the cache
    // and for the WAL file being created, which is setup cost, not write cost.
    let warm = measure(&store);
    let rate = measure(&store);
    eprintln!("write throughput: {rate:.0}/sec unbatched (warm-up run {warm:.0}/sec)");

    assert!(
        rate >= MIN_WRITES_PER_SEC,
        "single-threaded write throughput {rate:.0}/sec is below the \
         {MIN_WRITES_PER_SEC:.0}/sec floor -- check the writer's `synchronous` \
         pragma first, since an fsync per commit caps this at a few hundred \
         per second regardless of anything else"
    );

    drop(store);
    let _ = std::fs::remove_dir_all(db.parent().expect("db dir"));
}

#[test]
#[ignore = "perf budget; run via the perf-tests job (--run-ignored only --test-threads 1)"]
fn batching_writes_into_one_transaction_amortizes_the_commit() {
    let db = temp_db("batched");
    let store = Store::open(&db).expect("open store");

    // Warm-up, for the same reason as above.
    let _ = measure(&store);

    let payload = serde_json::json!({"phase": "measure", "detail": "one representative row"});
    let started = Instant::now();
    store
        .transaction(|s| {
            for i in 0..WRITES {
                let message = format!("batched sample {i}");
                s.cartographer_log(entry(&message, &payload))?;
            }
            Ok(())
        })
        .expect("batched write");
    let rate = (WRITES as f64) / started.elapsed().as_secs_f64();
    eprintln!("write throughput: {rate:.0}/sec in one transaction");

    assert!(
        rate >= MIN_BATCHED_WRITES_PER_SEC,
        "batched write throughput {rate:.0}/sec is below the \
         {MIN_BATCHED_WRITES_PER_SEC:.0}/sec floor"
    );

    drop(store);
    let _ = std::fs::remove_dir_all(db.parent().expect("db dir"));
}
