"""Unit tests for the daemon endpoint <-> CLI leaf parity lint (RAL-410)."""

from __future__ import annotations

import unittest
from unittest import mock

from scripts import check_endpoint_cli_parity as c


SERVER_SOURCE = """
fn route_for_user(daemon: &Daemon, method: &str, segs: &[&str]) -> Response {
    match (method, segs.as_slice()) {
        ("GET", ["api", "daemon"]) => health(daemon),
        // ralphus[ignore-endpoint-cli]: board-only index of every task; the CLI status leaf reads per-squad
        ("GET", ["api", "task-index"]) => task_index(daemon),
        ("POST", ["api", "squads", id, "restart"]) => {
            restart_squad(daemon, id, body)
        }
        (
            "POST",
            ["api", "squads", id, "proofs", task_idx, scope, cell_idx, proof_idx, "open-terminal"],
        ) => {
            open_proof_terminal(daemon, id, task_idx, scope, cell_idx, proof_idx)
        }
        _ => error(ResponseStatus::NotFound, "no such route"),
    }
}
"""

HELP_SOURCE = """
const fn node(name: &'static str, children: &'static [HelpNode]) -> HelpNode {
    HelpNode { name, children }
}

const SQUAD_CHILDREN: &[HelpNode] = &[
    node("show", &[]),
    node("restart", &[]),
];

pub const ROOT: HelpNode = node(
    "ralphus",
    &[
        node("status", &[]),
        node("squad", SQUAD_CHILDREN),
        // ralphus[ignore-endpoint-cli]: prints a locally generated shell script, no daemon call
        node("completion", &[]),
    ],
);

const WATCHER_BACKENDS: &[HelpNode] = &[
    // ralphus[ignore-endpoint-cli]: interactive local launcher, no daemon endpoint
    node("pi", &[]),
];

// ralphus[ignore-endpoint-cli]: interactive end-to-end launch (runtime boot + board watch), no endpoint
pub const QUICK_START: HelpNode = node(
    "quick-start",
    &[node("watcher", WATCHER_BACKENDS)],
);
"""


class RouteArmScanTests(unittest.TestCase):
    def test_extract_route_arms_keys_and_lines(self) -> None:
        eps = c.extract_route_arms(SERVER_SOURCE)
        self.assertEqual(
            [ep.key for ep in eps],
            [
                "GET /api/daemon",
                "GET /api/task-index",
                "POST /api/squads/{id}/restart",
                "POST /api/squads/{id}/proofs/{task_idx}/{scope}/{cell_idx}/{proof_idx}/open-terminal",
            ],
        )
        lines = SERVER_SOURCE.splitlines()
        expected_lines = {
            "GET /api/daemon": next(i for i, ln in enumerate(lines) if '("GET", ["api", "daemon"])' in ln) + 1,
            "GET /api/task-index": next(i for i, ln in enumerate(lines) if '("GET", ["api", "task-index"])' in ln) + 1,
            "POST /api/squads/{id}/restart": next(i for i, ln in enumerate(lines) if '("POST", ["api", "squads", id, "restart"])' in ln) + 1,
            "POST /api/squads/{id}/proofs/{task_idx}/{scope}/{cell_idx}/{proof_idx}/open-terminal": (
                next(i for i, ln in enumerate(lines) if ln.strip() == "(") + 1
            ),
        }
        for ep in eps:
            self.assertEqual(ep.line, expected_lines[ep.key])
        # the marker comment block above the task-index arm is attached
        self.assertEqual(
            c.attach_comments(lines, eps[1].line - 1),
            " ralphus[ignore-endpoint-cli]: board-only index of every task; the CLI status leaf reads per-squad",
        )
        self.assertEqual(
            c.marker_reason(c.attach_comments(lines, eps[1].line - 1)),
            "board-only index of every task; the CLI status leaf reads per-squad",
        )
        self.assertIsNone(c.marker_reason(c.attach_comments(lines, eps[0].line - 1)))

    def test_extract_leaves_mirrors_registered_leaves(self) -> None:
        leaves = c.extract_leaves(HELP_SOURCE)
        self.assertEqual(
            {leaf.path for leaf in leaves},
            {
                ("status",),
                ("squad", "show"),
                ("squad", "restart"),
                ("completion",),
                ("quick-start", "watcher", "pi"),
            },
        )

    def test_substantive_reason_rule(self) -> None:
        self.assertFalse(c.substantive("no"))
        self.assertFalse(c.substantive("internal"))
        self.assertTrue(c.substantive("board-only index; the CLI has no equivalent verb"))


class ViolationsTests(unittest.TestCase):
    def setUp(self) -> None:
        # a manifest covering every fixture endpoint/leaf except the
        # deliberately-excluded ones (which carry inline markers)
        self.mapped = {
            "GET /api/daemon": ["status"],
            "POST /api/squads/{id}/restart": ["squad restart"],
            "POST /api/squads/{id}/proofs/{task_idx}/{scope}/{cell_idx}/{proof_idx}/open-terminal": [
                "squad show"
            ],
        }
        self.special = {}

    def violations(self, mapped=None, special=None, server=None, help_=None) -> list[str]:
        with mock.patch.object(c, "ENDPOINT_TO_CLI", mapped if mapped is not None else self.mapped), \
             mock.patch.object(c, "SPECIAL_ROUTES", special if special is not None else self.special):
            return c.violations_for_sources(
                server if server is not None else SERVER_SOURCE,
                help_ if help_ is not None else HELP_SOURCE,
            )

    def test_everything_accounted_for_passes(self) -> None:
        self.assertEqual(self.violations(), [])

    def test_unmapped_endpoint_without_marker_is_a_violation(self) -> None:
        server = SERVER_SOURCE.replace(
            "// ralphus[ignore-endpoint-cli]: board-only index", "// board-only index"
        )
        out = self.violations(server=server)
        self.assertTrue(any("GET /api/task-index" in v and "server.rs:" in v for v in out), out)

    def test_weak_inline_reason_is_a_violation(self) -> None:
        server = SERVER_SOURCE.replace(
            "ralphus[ignore-endpoint-cli]: board-only index of every task; the CLI status leaf reads per-squad",
            "ralphus[ignore-endpoint-cli]: internal",
        )
        out = self.violations(server=server)
        self.assertTrue(any("GET /api/task-index" in v and "not substantive" in v for v in out), out)

    def test_stale_mapping_key_is_a_violation(self) -> None:
        mapped = dict(self.mapped)
        mapped["GET /api/old-route"] = ["status"]
        out = self.violations(mapped=mapped)
        self.assertTrue(any("no such endpoint exists" in v and "GET /api/old-route" in v for v in out), out)

    def test_stale_mapping_value_is_a_violation(self) -> None:
        mapped = dict(self.mapped)
        mapped["GET /api/daemon"] = ["no such leaf"]
        out = self.violations(mapped=mapped)
        self.assertTrue(any("no such leaf exists" in v and "'no such leaf'" in v for v in out), out)

    def test_mapped_and_marked_endpoint_is_a_violation(self) -> None:
        mapped = dict(self.mapped)
        mapped["GET /api/task-index"] = ["status"]
        out = self.violations(mapped=mapped)
        self.assertTrue(any("GET /api/task-index" in v and "both mapped" in v for v in out), out)

    def test_unmapped_leaf_without_marker_is_a_violation(self) -> None:
        help_ = HELP_SOURCE.replace(
            "// ralphus[ignore-endpoint-cli]: prints a locally generated shell script, no daemon call",
            "// prints a locally generated shell script",
        )
        out = self.violations(help_=help_)
        self.assertTrue(any("('completion',)" in v and "help_map.rs:" in v for v in out), out)

    def test_special_route_needs_substantive_builtin_reason(self) -> None:
        self.assertEqual(self.violations(special={"GET /api/events": "SSE needs a live connection; the CLI has no consumer"}), [])
        out = self.violations(special={"GET /api/events": "no"})
        self.assertTrue(any("GET /api/events" in v and "not substantive" in v for v in out), out)

    def test_special_route_conflicts_with_mapping(self) -> None:
        mapped = dict(self.mapped)
        mapped["GET /api/events"] = ["status"]
        out = self.violations(mapped=mapped, special={"GET /api/events": "SSE needs a live connection; the CLI has no consumer"})
        self.assertTrue(any("GET /api/events" in v and "pick one" in v for v in out), out)


class RealSourcesTests(unittest.TestCase):
    def test_real_sources_are_clean(self) -> None:
        """The CI gate itself: every endpoint and leaf in the actual tree is
        accounted for right now."""
        self.assertEqual(
            c.violations_for_sources(
                c.SERVER_RS.read_text(encoding="utf-8"),
                c.HELP_MAP_RS.read_text(encoding="utf-8"),
            ),
            [],
        )

    def test_manifest_is_bidirectional_and_current(self) -> None:
        leaves = c.extract_leaves(c.HELP_MAP_RS.read_text(encoding="utf-8"))
        endpoints = c.extract_route_arms(c.SERVER_RS.read_text(encoding="utf-8"))
        self.assertGreaterEqual(len(endpoints), 200)
        self.assertGreaterEqual(len(leaves), 130)
        self.assertGreaterEqual(len(c.ENDPOINT_TO_CLI), 100)
        self.assertEqual(len({e.key for e in endpoints} | set(c.SPECIAL_ROUTES)), len(endpoints) + len(c.SPECIAL_ROUTES))


if __name__ == "__main__":
    unittest.main()