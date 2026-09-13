"""Unit tests for the project-review-settings TOML/UI structural parity lint."""

from __future__ import annotations

import unittest

from scripts.check_project_review_settings_ui_parity import source_violations

CONFIG = '''
pub const REVIEW_FIELD_PARITY: &[(&str, ReviewFieldDefault)] = &[
    (
        "agent",
        ReviewFieldDefault::ProjectDefault(|c| c.default_resolver_agent.is_some()),
    ),
    (
        "upstream",
        ReviewFieldDefault::NotApplicable(
            "for an Arbiter-created review, upstream is derived from the pooled cells' actual base branch",
        ),
    ),
];
'''
SERVER = '''
struct ProjectReviewSettingsBody {
    #[serde(default)]
    default_resolver_agent: Option<String>,
    #[serde(default)]
    default_resolver_model: Option<String>,
}
'''
MODAL = "body.default_resolver_agent = draft.resolverAgent; body.default_resolver_model = draft.resolverModel;"


class ProjectReviewSettingsUiParityTests(unittest.TestCase):
    def test_fully_covered_key_passes(self) -> None:
        self.assertEqual(source_violations(SERVER, CONFIG, MODAL), [])

    def test_missing_settings_body_field_fails(self) -> None:
        # Remove "model"'s mapped field, keeping the "agent" canary field
        # intact so the anchor sanity check doesn't mask the real violation.
        source = SERVER.replace("\n    #[serde(default)]\n    default_resolver_model: Option<String>,", "")
        config = CONFIG.replace(
            '"agent",\n        ReviewFieldDefault::ProjectDefault(|c| c.default_resolver_agent.is_some()),',
            '"agent",\n        ReviewFieldDefault::ProjectDefault(|c| c.default_resolver_agent.is_some()),\n    ),\n    (\n        "model",\n        ReviewFieldDefault::ProjectDefault(|c| c.default_resolver_model.is_some()),',
        )
        violations = source_violations(source, config, MODAL)
        self.assertTrue(violations)
        self.assertIn("missing from ProjectReviewSettingsBody", violations[0])

    def test_missing_modal_reference_fails(self) -> None:
        violations = source_violations(SERVER, CONFIG, "nothing relevant here")
        self.assertTrue(violations)
        self.assertIn("no visible reference", violations[0])

    def test_unmapped_project_default_key_fails(self) -> None:
        # Add a second ProjectDefault entry for an unmapped key, keeping the
        # "agent" canary entry intact.
        config = CONFIG.replace(
            '"agent",\n        ReviewFieldDefault::ProjectDefault(|c| c.default_resolver_agent.is_some()),',
            '"agent",\n        ReviewFieldDefault::ProjectDefault(|c| c.default_resolver_agent.is_some()),\n    ),\n    (\n        "brand_new_field",\n        ReviewFieldDefault::ProjectDefault(|c| c.default_resolver_agent.is_some()),',
        )
        violations = source_violations(SERVER, config, MODAL)
        self.assertTrue(violations)
        self.assertIn("no entry in MAPPING", violations[0])

    def test_not_applicable_keys_are_ignored(self) -> None:
        # "upstream" is `NotApplicable`, not `ProjectDefault` -- it must never
        # be required to have a settings-body field or modal reference.
        self.assertEqual(source_violations(SERVER, CONFIG, MODAL), [])

    def test_missing_anchor_fails(self) -> None:
        self.assertIn("anchor not found", source_violations("struct Else {}", CONFIG, MODAL)[0])


if __name__ == "__main__":
    unittest.main()
