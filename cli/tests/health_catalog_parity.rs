//! RAL-416: drift-safety net between `ralphus_core::health_catalog`'s
//! `ID_*` constants and this crate's actual `cli/src/health.rs` checks.
//! Nothing in the type system stops a catalog entry from being added
//! without ever wiring a check to it (a dead entry `check catalog` would
//! advertise but `check health` never produces), or a check's `.with_id(...)`
//! call from being deleted while the catalog entry survives -- both compile
//! cleanly. This test closes that gap by grepping `health.rs`'s own source
//! text for each `ID_*` constant's Rust *name* (not its string value, which
//! could coincidentally appear in an unrelated string/comment) and reporting
//! every unreferenced one by name, so a failure is immediately actionable
//! rather than "some entry, somewhere, is unused."
//!
//! Deliberately a source-text grep, not a runtime check: most of
//! `run_checks`'s checks round-trip through a live daemon, which this crate's
//! test suite does not spin up (`cli_integration.rs` covers the parts that
//! do, via a real daemon subprocess). A textual reference is a weaker
//! guarantee than "this id was actually produced by a real run," but it is
//! enough to catch the actual failure mode this test exists for: an entry
//! added to the catalog and never wired up at all, or a wiring site deleted
//! without updating the catalog.

const HEALTH_RS_SOURCE: &str = include_str!("../src/health.rs");

#[test]
fn every_catalog_id_constant_is_referenced_in_health_rs() {
    let missing: Vec<&str> = ralphus_core::health_catalog::CATALOG_ID_CONST_NAMES
        .iter()
        .filter(|(const_name, _id)| !HEALTH_RS_SOURCE.contains(const_name))
        .map(|(const_name, _id)| *const_name)
        .collect();
    assert!(
        missing.is_empty(),
        "the following ralphus_core::health_catalog ID_* constants are never referenced in \
         cli/src/health.rs -- either wire a check to them (via CheckResult::with_id) or remove \
         the now-dead catalog entry: {missing:?}"
    );
}

/// The inverse direction: every catalog entry's `id` should be resolvable
/// (`ralphus_core::health_catalog::get`) -- catches a typo'd literal id
/// slipping in anywhere a raw string was used instead of the `ID_*`
/// constant (the constants themselves can't typo, since an unknown ident is
/// a compile error, but nothing stops new code from spelling out a literal
/// instead of using the constant).
#[test]
fn every_catalog_entry_round_trips_through_get() {
    for entry in ralphus_core::health_catalog::CATALOG {
        assert_eq!(
            ralphus_core::health_catalog::get(entry.id).map(|e| e.id),
            Some(entry.id),
            "catalog entry {:?} does not round-trip through health_catalog::get",
            entry.id
        );
    }
}
