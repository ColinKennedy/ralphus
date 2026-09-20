"""Unit tests for the board tab/hash route parity checker."""

from __future__ import annotations

import unittest

from scripts.check_board_tab_route_parity import TAB_ROUTES, source_violations


def board_source(routes: dict[str, str] = TAB_ROUTES) -> str:
    """Build minimal routing source that implements every supplied route."""
    sync = "\n".join(f'url = "#/{route}";' for route in routes.values())
    cases: list[str] = []
    for tab, route in routes.items():
        if tab == "squads":
            cases.append(f'if (raw.startsWith("{route}")) return parseSquadsHashBody(raw);')
        elif tab == "tasks":
            cases.append(f'if (raw.startsWith("{route}")) return parseTasksTabHash(raw);')
        else:
            cases.append(f'if (raw.startsWith("{route}")) return {{ tab: "{tab}" }};')
    return f"function syncHash() {{ {sync} }}\nfunction parseHash() {{ {' '.join(cases)} }}"


class BoardTabRouteParityTests(unittest.TestCase):
    def test_all_tabs_and_routes_pass(self) -> None:
        self.assertEqual(source_violations(board_source(), list(TAB_ROUTES)), [])

    def test_tab_without_route_fails(self) -> None:
        violations = source_violations(board_source(), [*TAB_ROUTES, "new-tab"])
        self.assertIn("board tab 'new-tab' has no canonical hash route", violations)

    def test_route_without_tab_fails(self) -> None:
        tabs = [tab for tab in TAB_ROUTES if tab != "agents"]
        violations = source_violations(board_source(), tabs)
        self.assertIn("canonical hash route for 'agents' has no board tab", violations)

    def test_unmapped_parse_route_fails(self) -> None:
        source = board_source().replace(
            "function parseHash() {",
            'function parseHash() { if (raw.startsWith("orphan")) return { tab: "orphan" };',
        )
        violations = source_violations(source, list(TAB_ROUTES))
        self.assertIn(
            "parseHash recognizes route 'orphan' without a matching board tab", violations
        )

    def test_missing_sync_route_fails(self) -> None:
        source = board_source().replace('url = "#/agents";', "")
        violations = source_violations(source, list(TAB_ROUTES))
        self.assertIn("syncHash does not emit #/agents for tab 'agents'", violations)

    def test_unmapped_sync_route_fails(self) -> None:
        source = board_source().replace(
            'function syncHash() {',
            'function syncHash() { url = "#/orphan"; ',
        )
        violations = source_violations(source, list(TAB_ROUTES))
        self.assertIn(
            "syncHash emits route 'orphan' without a matching board tab", violations
        )


if __name__ == "__main__":
    unittest.main()
