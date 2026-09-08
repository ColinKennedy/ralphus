"""Unit tests for the review settings/TOML structural parity lint."""

from __future__ import annotations

import unittest

from scripts.check_review_settings_parity import source_violations

VALIDATE = '''
pub const REVIEW_KEYS: &[&str] = &[
    "id",
    "skip_auto_build",
];
'''
SERVER = '''
struct GuardianSettingsBody {
    skip_auto_build: Option<bool>,
}
'''


class ReviewSettingsParityTests(unittest.TestCase):
    def test_missing_mapping_fails(self) -> None:
        source = SERVER.replace(
            "skip_auto_build: Option<bool>,",
            "skip_auto_build: Option<bool>,\n    new_setting: Option<bool>,",
        )
        self.assertIn("no [[review]] mapping", source_violations(source, VALIDATE)[0])

    def test_inline_exemption_passes(self) -> None:
        source = '''
struct GuardianSettingsBody {
    // ralphus[ignore-review-parity]: endpoint-only migration control, not a review setting
    endpoint_only: Option<bool>,
    skip_auto_build: Option<bool>,
}
'''
        self.assertEqual(source_violations(source, VALIDATE), [])

    def test_unsubstantive_exemption_fails(self) -> None:
        source = '''
struct GuardianSettingsBody {
    // ralphus[ignore-review-parity]: no
    endpoint_only: Option<bool>,
    skip_auto_build: Option<bool>,
}
'''
        self.assertIn("substantive", source_violations(source, VALIDATE)[0])

    def test_missing_anchor_fails(self) -> None:
        self.assertIn("anchor not found", source_violations("struct Else {}", VALIDATE)[0])


if __name__ == "__main__":
    unittest.main()
