//! RAL-416: drift-safety net between `ralphus_core::health_catalog`'s
//! `remote-*` entries and `daemon/src/health_targets.rs`'s actual per-target
//! checks -- the daemon-side counterpart to
//! `cli/tests/health_catalog_parity.rs` (see that file's doc comment for the
//! full rationale: a source-text grep for each `ID_REMOTE_*` constant's own
//! Rust name, so an added-but-never-wired or wired-then-deleted entry is
//! reported by name).

const HEALTH_TARGETS_RS_SOURCE: &str = include_str!("../src/health_targets.rs");

#[test]
fn every_remote_catalog_id_constant_is_referenced_in_health_targets_rs() {
    let missing: Vec<&str> = ralphus_core::health_catalog::CATALOG_ID_CONST_NAMES
        .iter()
        .filter(|(_const_name, id)| id.starts_with("remote-"))
        .filter(|(const_name, _id)| !HEALTH_TARGETS_RS_SOURCE.contains(const_name))
        .map(|(const_name, _id)| *const_name)
        .collect();
    assert!(
        missing.is_empty(),
        "the following ralphus_core::health_catalog remote-* ID_* constants are never \
         referenced in daemon/src/health_targets.rs -- either wire a TargetCheck to them or \
         remove the now-dead catalog entry: {missing:?}"
    );
}

/// Every `TargetCheck` this module can actually produce carries an `id`
/// that resolves in the catalog as a `Remote`/`OnDemand` entry -- the
/// hourly Free-tier sweep (`crate::health_sweep`) must never accidentally
/// gain a remote probe, and the catalog's own `no_remote_entry_is_free`
/// test only proves the catalog's metadata is self-consistent, not that
/// this module's real output matches it.
#[test]
fn every_remote_catalog_entry_is_ondemand_and_remote() {
    for (_, id) in ralphus_core::health_catalog::CATALOG_ID_CONST_NAMES
        .iter()
        .filter(|(_, id)| id.starts_with("remote-"))
    {
        let entry = ralphus_core::health_catalog::get(id)
            .unwrap_or_else(|| panic!("{id} is not in the catalog"));
        assert_eq!(
            entry.cost_tier,
            ralphus_core::health_catalog::CostTier::OnDemand,
            "{id} must be OnDemand"
        );
        assert_eq!(
            entry.applicability,
            ralphus_core::health_catalog::Applicability::Remote,
            "{id} must be Remote"
        );
    }
}
