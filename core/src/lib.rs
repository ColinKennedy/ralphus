//! `ralphus-core` — shared types and logic used by the daemon and librarian.
//!
//! This crate is deliberately dependency-light and side-effect-free so it can be
//! unit-tested quickly and reused across the Rust executables. Heavier concerns
//! (HTTP, SQLite, process spawning) live in the `daemon` and `librarian` crates.

mod bench_demo;
pub mod schema;
pub mod uri;
pub mod validate;

pub use bench_demo::ralphus_bench_tests;

/// The workspace version, surfaced so every executable reports the same string.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Returns the crate version. Kept as a function (not just the constant) so
/// callers have a stable API even if the source of the version changes later.
#[must_use]
pub fn version() -> &'static str {
    VERSION
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_is_non_empty() {
        assert!(!version().is_empty());
    }

    #[test]
    fn version_matches_constant() {
        assert_eq!(version(), VERSION);
    }
}
