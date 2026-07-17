//! RAL-94 in-process, serial, durable-minimum benchmark harness for Rust
//! tests explicitly opted in via `#[ralphus_bench_macros::ralphus_bench]`.
//!
//! Not part of `cargo test` — see the `ralphus-bench-rs` binary in this
//! crate for the opt-in entry point. See the "RAL-94 benchmark harness"
//! section of `AGENTS.md` for why Rust uses explicit per-test opt-in rather
//! than Python's zero-annotation universal inclusion.

mod durable_min;
mod gitinfo;
mod stats;
mod storage;

pub use durable_min::{DEFAULT_PATIENCE, DurableMinResult, run_durable_min};
pub use gitinfo::{GitError, GitState, current_git_state};
pub use ralphus_bench_types::BenchMeta;
pub use stats::{StatsBundle, compute_stats};
pub use storage::{
    BenchRecord, StorageError, TestRecordFile, append_record, find_repo_root, load_records,
    record_from_stats, test_data_path,
};

use std::path::{Path, PathBuf};
use std::time::Instant;

/// A suite that ran unusually long, surfaced so a slow-degrading harness
/// stays visible instead of silently eating CI time (see RAL-94 Risks).
const SLOW_SUITE_WARNING_SECS: f64 = 300.0;

/// Runs every provided bench test through the durable-minimum loop,
/// persisting each result under `bench_data/rust/`. Returns the number of
/// tests run.
pub fn run_all(tests: &[BenchMeta]) -> Result<usize, StorageError> {
    let cwd = std::env::current_dir().map_err(|source| StorageError::Read {
        path: PathBuf::from("."),
        source,
    })?;
    let repo_root = find_repo_root(&cwd)?;
    let git_state = current_git_state(&repo_root)?;
    let start = Instant::now();

    for test in tests {
        eprintln!(
            "ralphus [bench] test starting name={} patience={}",
            test.name, test.patience
        );
        let test_start = Instant::now();
        let result = run_durable_min(test.run, test.patience);
        let bundle = compute_stats(&result.samples, result.durable_min);
        let record = record_from_stats(&git_state, &bundle);

        let path = test_data_path("rust", Path::new(test.file), test.name, &repo_root);
        append_record(&path, test.name, record)?;

        eprintln!(
            "ralphus [bench] test done name={} durable_min={:.6}s samples={} elapsed={:.2}s",
            test.name,
            result.durable_min,
            result.samples.len(),
            test_start.elapsed().as_secs_f64()
        );
    }

    let total_elapsed = start.elapsed().as_secs_f64();
    if total_elapsed > SLOW_SUITE_WARNING_SECS {
        eprintln!(
            "ralphus [bench] WARN suite took {:.1}s across {} tests \
             — consider lowering per-test patience or trimming scope",
            total_elapsed,
            tests.len()
        );
    }
    eprintln!(
        "ralphus [bench] done total={} elapsed={total_elapsed:.2}s",
        tests.len()
    );
    Ok(tests.len())
}
