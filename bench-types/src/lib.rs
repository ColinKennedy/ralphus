//! `BenchMeta` — the one type a `#[ralphus_bench]`-tagged crate (e.g.
//! `ralphus-core`) and the `ralphus-bench-harness` runner both need.
//!
//! It lives in its own dependency-free crate specifically to avoid a cyclic
//! workspace dependency: bench-tagged crates depend on this crate to name
//! the type their `ralphus_bench_tests()` collector returns, while
//! `ralphus-bench-harness`'s binary depends on those same crates to call
//! that collector — those two edges can't both point at one crate without
//! forming a cycle, so the type is split out here instead.

/// One bench-registered test: metadata plus the function pointer to invoke.
/// Built by the `#[ralphus_bench]` macro expansion — see `ralphus-bench-macros`.
#[derive(Clone, Copy)]
pub struct BenchMeta {
    pub name: &'static str,
    pub file: &'static str,
    pub patience: u32,
    pub run: fn(),
}
