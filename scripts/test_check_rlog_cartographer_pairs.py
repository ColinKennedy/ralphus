"""Unit tests for the daemon rlog / Cartographer pairing lint."""

from __future__ import annotations

import unittest

from scripts.check_rlog_cartographer_pairs import source_violations


class RlogCartographerLintTests(unittest.TestCase):
    def test_allows_intervening_statements_and_wrapper_emit(self) -> None:
        source = """
fn record(store: &Store) {
    store.cartographer_log(entry());
}
fn work(store: &Store) {
    crate::rlog!(INFO, "starting");
    do_something();
    record(store);
}
"""
        self.assertEqual(source_violations(source), [])

    def test_rejects_emit_in_another_function(self) -> None:
        source = """
fn work() {
    crate::rlog!(INFO, "unpaired");
}
fn unrelated(store: &Store) {
    store.cartographer_log(entry());
}
"""
        self.assertEqual(source_violations(source)[0][0], 3)

    def test_requires_a_substantive_exemption_reason(self) -> None:
        weak = """
fn work() {
    // ralphus[ignore-rlog-pair]: no
    crate::rlog!(INFO, "unpaired");
}
"""
        explained = weak.replace(
            ": no", ": helper has no Store; caller records the structured outcome"
        )
        self.assertIn("substantive reason", source_violations(weak)[0][1])
        self.assertEqual(source_violations(explained), [])

    def test_ignores_mentions_in_comments_and_literals(self) -> None:
        source = '''
fn docs() {
    // crate::rlog!(INFO, "comment");
    let _ = "crate::rlog!(INFO, string)";
    let _ = '{';
}
'''
        self.assertEqual(source_violations(source), [])


if __name__ == "__main__":
    unittest.main()
