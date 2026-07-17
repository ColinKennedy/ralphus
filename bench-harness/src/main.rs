//! `ralphus-bench-rs` — opt-in entry point for the RAL-94 Rust benchmark
//! harness. Not run as part of `cargo test`; invoke explicitly:
//!
//! ```text
//! cargo run -p ralphus-bench-harness --bin ralphus-bench-rs
//! ```
//!
//! Currently benchmarks `ralphus-core`'s opted-in tests (see
//! `core/src/bench_demo.rs`). Other crates adopt the same
//! `#[ralphus_bench]` attribute + `ralphus_bench_tests()` collector pattern
//! to join in — see the "RAL-94 benchmark harness" section of `AGENTS.md`.

fn main() {
    let tests = ralphus_core::ralphus_bench_tests();
    eprintln!(
        "ralphus [bench] rust harness starting count={}",
        tests.len()
    );

    match ralphus_bench_harness::run_all(&tests) {
        Ok(count) => eprintln!("ralphus [bench] rust harness finished count={count}"),
        Err(err) => {
            eprintln!("ralphus [bench] rust harness failed error={err}");
            std::process::exit(1);
        }
    }
}
