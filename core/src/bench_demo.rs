//! Demo wiring for the RAL-94 Rust benchmark harness — see the "RAL-94
//! benchmark harness" section of `AGENTS.md` for the full design and why
//! Rust uses this explicit per-test opt-in pattern instead of automatic
//! universal inclusion.
//!
//! Proves `#[ralphus_bench]` + the hand-written `ralphus_bench_tests()`
//! collector end to end against real `ralphus-core` logic. Other crates
//! (`daemon`, `librarian`) adopt the same pattern as they opt their own
//! tests in — this module is deliberately not `#[cfg(test)]`-gated, since a
//! `#[cfg(test)]` module is only compiled by `cargo test`'s implicit
//! lib-unittest target, which the external `ralphus-bench-rs` binary never
//! builds; each function here still also runs once under plain `cargo test`
//! via the `#[test]` attribute the macro adds.

use ralphus_bench_macros::ralphus_bench;
use ralphus_bench_types::BenchMeta;

use crate::validate::validate_toml;

const MINIMAL_VALID_TASK: &str = r#"
[[task]]
name = "build"
[[task.session]]
cwd = "/repo"
prompt = "make it build"
"#;

#[ralphus_bench]
fn validate_toml_accepts_minimal_task() {
    let report = validate_toml(MINIMAL_VALID_TASK);
    assert!(report.is_ok());
}

#[ralphus_bench(patience = 3)] // low: validating an empty file is trivial and deterministic (single parse-error branch) — few improving runs needed to find its floor
fn validate_toml_rejects_empty_file() {
    let report = validate_toml("");
    assert!(!report.is_ok());
}

/// Every `#[ralphus_bench]`-tagged test in this crate, for `ralphus-bench-rs`
/// to drive. Update this list by hand when adding or removing a bench test.
#[must_use]
pub fn ralphus_bench_tests() -> Vec<BenchMeta> {
    vec![
        VALIDATE_TOML_ACCEPTS_MINIMAL_TASK_BENCH,
        VALIDATE_TOML_REJECTS_EMPTY_FILE_BENCH,
    ]
}
