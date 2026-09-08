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
use ralphus_daemon::config::{
    REVIEW_CONFIG_KEYS, REVIEW_FIELD_PARITY, ReviewConfig, ReviewFieldDefault,
};

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

/// RAL-369: a second, plain name-matching parity check over the same two
/// sources of truth as the tests above -- `ralphus_core::validate::REVIEW_KEYS`
/// (`[[review]]`'s TOML fields) and `ralphus_daemon::config::REVIEW_CONFIG_KEYS`
/// (`ReviewConfig`'s own TOML key names, in declaration order). Unlike
/// `REVIEW_FIELD_PARITY` above, this doesn't require a `fn(&ReviewConfig) ->
/// bool` getter per field -- deliberately, since a field like `checks` has
/// union semantics (a `Vec<String>`, not a scalar override) that a single
/// boolean getter can't honestly represent. This table only proves the two
/// field-*name* lists stay in lockstep; the later ticket in this batch that
/// wires `checks`' union behavior has its own, separate test coverage for
/// that.
///
/// Identity pairs (the `[[review]]` name and the `ReviewConfig` name match)
/// and renamed pairs (the project-config side carries a `default_`/
/// `resolver` qualifier the per-review side doesn't need) are both listed
/// here uniformly as `(review_key, review_config_key)`.
const PAIRS: &[(&str, &str)] = &[
    ("skip_worktrees", "skip_worktrees"),
    ("skip_base_updates", "skip_base_updates"),
    ("match_pr_branch_name", "match_pr_branch_name"),
    ("checks", "checks"),
    ("auto_build", "auto_build"),
    ("summary_format", "summary_format"),
    ("auto_submit_pr_stack", "auto_submit_pr_stack"),
    ("proof_scope", "default_proof_scope"),
    ("proof_skip_auto_clean", "proof_skip_auto_clean"),
    ("agent", "default_resolver_agent"),
    ("model", "default_resolver_model"),
    ("machine", "default_machine"),
    ("maximum_budget_usd", "default_maximum_budget_usd"),
];

/// The `[[review]]` fields that legitimately have no `REVIEW_CONFIG_KEYS`
/// counterpart at all -- see this same file's `REVIEW_FIELD_PARITY`
/// `NotApplicable` entries for the full reasoning behind each; kept short
/// here since this table only needs a reason substantive enough to prove
/// it's a deliberate call, not a placeholder.
const REVIEW_DEF_ONLY_EXCLUSIONS: &[(&str, &str)] = &[
    (
        "id",
        "identity assigned at creation time, not a project-wide default",
    ),
    (
        "name",
        "derived per-review from id/pool key; a fixed default would collide",
    ),
    (
        "upstream",
        "derived from the pooled cells' actual base branch, never static",
    ),
    (
        "action",
        "a bespoke per-review manual-test button, not a scalar setting",
    ),
];

#[test]
fn every_review_key_is_covered_by_a_pair_or_exclusion() {
    let review_keys: BTreeSet<&str> = REVIEW_KEYS.iter().copied().collect();
    let paired: BTreeSet<&str> = PAIRS.iter().map(|(k, _)| *k).collect();
    let excluded: BTreeSet<&str> = REVIEW_DEF_ONLY_EXCLUSIONS.iter().map(|(k, _)| *k).collect();

    let overlap: Vec<&&str> = paired.intersection(&excluded).collect();
    assert!(
        overlap.is_empty(),
        "these `[[review]]` fields are both paired to a project default AND excluded -- pick \
         one: {overlap:?}"
    );

    let covered: BTreeSet<&str> = paired.union(&excluded).copied().collect();
    let uncovered: Vec<&&str> = review_keys.difference(&covered).collect();
    assert!(
        uncovered.is_empty(),
        "these `[[review]]` fields (core::validate::REVIEW_KEYS) have no entry in PAIRS or \
         REVIEW_DEF_ONLY_EXCLUSIONS: {uncovered:?}"
    );

    let orphaned: Vec<&&str> = covered.difference(&review_keys).collect();
    assert!(
        orphaned.is_empty(),
        "these PAIRS/REVIEW_DEF_ONLY_EXCLUSIONS entries name a `[[review]]` field that no \
         longer exists in REVIEW_KEYS (stale or typo'd): {orphaned:?}"
    );
}

#[test]
fn every_review_config_key_is_covered_by_exactly_one_pair() {
    let config_keys: BTreeSet<&str> = REVIEW_CONFIG_KEYS.iter().copied().collect();

    let mut seen = BTreeSet::new();
    let mut duplicates = Vec::new();
    for (_, config_key) in PAIRS {
        if !seen.insert(*config_key) {
            duplicates.push(*config_key);
        }
    }
    assert!(
        duplicates.is_empty(),
        "these REVIEW_CONFIG_KEYS names appear more than once as a PAIRS right-hand side: \
         {duplicates:?}"
    );

    let uncovered: Vec<&&str> = config_keys.difference(&seen).collect();
    assert!(
        uncovered.is_empty(),
        "these REVIEW_CONFIG_KEYS entries (daemon::config::REVIEW_CONFIG_KEYS) have no PAIRS \
         entry pointing at them: {uncovered:?}"
    );

    let orphaned: Vec<&&str> = seen.difference(&config_keys).collect();
    assert!(
        orphaned.is_empty(),
        "these PAIRS entries name a project-config field that no longer exists in \
         REVIEW_CONFIG_KEYS (stale or typo'd): {orphaned:?}"
    );
}

#[test]
fn review_def_only_exclusions_are_exactly_id_name_upstream_action() {
    let names: BTreeSet<&str> = REVIEW_DEF_ONLY_EXCLUSIONS.iter().map(|(k, _)| *k).collect();
    let expected: BTreeSet<&str> = ["id", "name", "upstream", "action"].into_iter().collect();
    assert_eq!(
        names, expected,
        "REVIEW_DEF_ONLY_EXCLUSIONS must be exactly {{id, name, upstream, action}} -- every \
         other `[[review]]` field must map to a real project default in PAIRS"
    );
}

#[test]
fn every_review_def_only_exclusion_has_a_substantive_reason() {
    let mut bad = Vec::new();
    for (key, reason) in REVIEW_DEF_ONLY_EXCLUSIONS {
        if reason.trim().len() < 15 {
            bad.push(format!("{key:?}: {reason:?}"));
        }
    }
    assert!(
        bad.is_empty(),
        "every REVIEW_DEF_ONLY_EXCLUSIONS entry needs a real, documented reason (not \
         empty/placeholder text):\n{}",
        bad.join("\n")
    );
}
