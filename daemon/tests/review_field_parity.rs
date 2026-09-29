//! Prevent authored review settings and project defaults from silently drifting.

use std::collections::BTreeSet;

use ralphus_core::validate::REVIEW_KEYS;
use ralphus_daemon::config::REVIEW_CONFIG_KEYS;

const PAIRS: &[(&str, &str)] = &[
    ("agent", "default_resolver_agent"),
    ("model", "default_resolver_model"),
    ("machine", "default_machine"),
    ("maximum_budget_usd", "default_maximum_budget_usd"),
    ("proof_scope", "default_proof_scope"),
    ("auto_submit_pr_stack", "auto_submit_pr_stack"),
    ("match_pr_branch_name", "match_pr_branch_name"),
    ("skip_worktrees", "skip_worktrees"),
    ("skip_base_updates", "skip_base_updates"),
    ("proof_skip_auto_clean", "proof_skip_auto_clean"),
    ("checks", "checks"),
    ("auto_build", "auto_build"),
    ("summary_format", "summary_format"),
];

const REVIEW_DEF_ONLY_EXCLUSIONS: &[(&str, &str)] = &[
    ("id", "identity is assigned for each individual review"),
    ("name", "a shared default label would collide between reviews"),
    ("upstream", "the contributing branches determine the correct upstream"),
    ("action", "manual action buttons are bespoke to one review"),
    ("skip_auto_clean", "legacy spelling accepted only for backwards compatibility"),
    ("auto_pr_feedback", "feedback handling remains an explicit per-review choice"),
    ("base_shift_maximum_rebuilds", "outside this review-default parity contract"),
    ("separate_pr_branch", "outside this review-default parity contract"),
    ("dual_root_pr", "outside this review-default parity contract"),
    ("skip_auto_build", "an opt-out has no project default counterpart"),
    ("auto_fix_pr_errors", "outside this review-default parity contract"),
    ("auto_fix_prompt_template", "outside this review-default parity contract"),
    ("discourage_tests_during_auto_pull_request_fixes", "outside this review-default parity contract"),
    ("auto_cancel_outdated_pr_pipelines", "outside this review-default parity contract"),
];

#[test]
fn every_review_key_is_covered_by_a_pair_or_exclusion() {
    let review_keys: BTreeSet<_> = REVIEW_KEYS.iter().copied().collect();
    let paired: BTreeSet<_> = PAIRS.iter().map(|(review, _)| *review).collect();
    let excluded: BTreeSet<_> = REVIEW_DEF_ONLY_EXCLUSIONS.iter().map(|(review, _)| *review).collect();
    assert_eq!(paired.len(), PAIRS.len(), "duplicate review keys in PAIRS");
    assert_eq!(excluded.len(), REVIEW_DEF_ONLY_EXCLUSIONS.len(), "duplicate review keys in exclusions");
    assert!(paired.is_disjoint(&excluded), "a key cannot be paired and excluded");
    let covered: BTreeSet<_> = paired.union(&excluded).copied().collect();
    assert_eq!(review_keys, covered, "every authored review key must be paired or deliberately excluded");
    assert!(REVIEW_DEF_ONLY_EXCLUSIONS.iter().all(|(_, reason)| reason.trim().len() >= 15));
}

#[test]
fn every_project_default_key_is_covered_by_exactly_one_pair() {
    let config_keys: BTreeSet<_> = REVIEW_CONFIG_KEYS.iter().copied().collect();
    let paired: BTreeSet<_> = PAIRS.iter().map(|(_, config)| *config).collect();
    assert_eq!(paired.len(), PAIRS.len(), "duplicate project config keys in PAIRS");
    assert_eq!(config_keys, paired, "every project default must have one authored counterpart");
}
