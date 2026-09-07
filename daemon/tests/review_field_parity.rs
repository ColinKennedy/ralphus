//! RAL-342/RAL-338: every top-level `[[review]]` TOML field must either have
//! a per-project auto-review default (so the Arbiter's `[[review]]`-block-less
//! reviews still get one) or a documented reason it deliberately doesn't --
//! see `daemon/src/config.rs`'s `REVIEW_FIELD_PARITY` doc comment for the
//! full rationale. A real `cargo test`, part of the normal `cargo test
//! --all-targets` run `AGENTS.md` already asks developers to run and
//! `.github/workflows/ci.yml`'s `rust` job already runs -- no separate
//! CI-only script or gate, mirroring `mcp/tests/parity.rs`'s precedent for
//! this exact shape of check.

use std::collections::BTreeSet;

use ralphus_core::validate::REVIEW_KEYS;
use ralphus_daemon::config::{REVIEW_FIELD_PARITY, ReviewConfig, ReviewFieldDefault};

#[test]
fn every_review_key_has_a_parity_entry_and_vice_versa() {
    let review_keys: BTreeSet<&str> = REVIEW_KEYS.iter().copied().collect();
    let parity_keys: BTreeSet<&str> = REVIEW_FIELD_PARITY.iter().map(|(k, _)| *k).collect();

    let missing: Vec<&&str> = review_keys.difference(&parity_keys).collect();
    assert!(
        missing.is_empty(),
        "these `[[review]]` fields (core::validate::REVIEW_KEYS) have no entry in \
         REVIEW_FIELD_PARITY -- add one wiring it to a project default, or a \
         ReviewFieldDefault::NotApplicable with a real reason: {missing:?}"
    );

    let stale: Vec<&&str> = parity_keys.difference(&review_keys).collect();
    assert!(
        stale.is_empty(),
        "these REVIEW_FIELD_PARITY entries name a field that no longer exists on \
         ReviewDef/REVIEW_KEYS (stale or typo'd): {stale:?}"
    );
}

#[test]
fn every_not_applicable_entry_has_a_substantive_reason() {
    let mut bad = Vec::new();
    for (key, disposition) in REVIEW_FIELD_PARITY {
        if let ReviewFieldDefault::NotApplicable(reason) = disposition {
            if reason.trim().len() < 15 {
                bad.push(format!("{key:?}: {reason:?}"));
            }
        }
    }
    assert!(
        bad.is_empty(),
        "every NotApplicable entry needs a real, documented reason (not empty/placeholder \
         text):\n{}",
        bad.join("\n")
    );
}

#[test]
fn every_project_default_getter_is_wired_to_an_actually_unset_field() {
    // Sanity check, not exhaustive: a fresh `ReviewConfig` has nothing set, so
    // every `ProjectDefault` getter must report `false` against it. This
    // mainly guards against a getter accidentally wired to the wrong field
    // (e.g. one that already has a non-`None` fallback baked into its
    // accessor, like `default_resolver_agent()` returning `"ollama"` rather
    // than the raw `Option`).
    let empty = ReviewConfig::default();
    let mut bad = Vec::new();
    for (key, disposition) in REVIEW_FIELD_PARITY {
        if let ReviewFieldDefault::ProjectDefault(getter) = disposition {
            if getter(&empty) {
                bad.push(*key);
            }
        }
    }
    assert!(
        bad.is_empty(),
        "these ProjectDefault getters report `true` against a wholly-unset ReviewConfig -- \
         likely wired to the wrong field: {bad:?}"
    );
}
