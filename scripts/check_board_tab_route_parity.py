"""Ensure every board tab has one canonical hash route and vice versa.

The board's ``TABS`` array remains the source of truth for the tab list; this
checker deliberately imports docsgen's ``board_tabs()`` rather than scanning a
second copy.  It compares that list to the canonical route contract below and
then verifies both ``syncHash()`` and ``parseHash()`` implement each route.
"""

from __future__ import annotations

import importlib
import re
import sys
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parent.parent
DOCSGEN_SRC = REPO_ROOT / "cli-py" / "src"
CHROME = REPO_ROOT / "librarian" / "assets" / "board" / "25-chrome.js"

sys.path.insert(0, str(DOCSGEN_SRC))


# Board tab id -> public hash route.  The two deliberately shorter public
# routes are kept explicit so an internal-id rename cannot silently alter a
# bookmarked URL.
TAB_ROUTES = {
    "squads": "squads",
    "tasks": "tasks",
    "queue": "queue",
    "reviews": "reviews",
    "resources": "resources",
    "cartographer": "logs",
    "projects": "projects",
    "machines": "machines",
    "triage": "triage",
    "users": "users",
    "secrets": "secrets",
    "worktree-retirement": "retirement",
    "health": "health",
    "agents": "agents",
    "prefs": "prefs",
}

_FUNCTION = re.compile(r"function (?P<name>syncHash|parseHash)\([^)]*\)\s*\{")
_HASH_CASE = re.compile(r'raw\.startsWith\("(?P<route>[^"]+)"\)')
_HASH_URL = re.compile(r'["`]#/(?P<route>[a-z][a-z0-9-]*)')


def function_body(source: str, name: str) -> str:
    """Return the brace-delimited body of a named board routing function."""
    match = next((m for m in _FUNCTION.finditer(source) if m.group("name") == name), None)
    if match is None:
        raise ValueError(f"could not find {name}()")
    start = source.find("{", match.start(), match.end())
    depth = 0
    for position in range(start, len(source)):
        if source[position] == "{":
            depth += 1
        elif source[position] == "}":
            depth -= 1
            if depth == 0:
                return source[start + 1 : position]
    raise ValueError(f"could not find closing brace for {name}()")


def parse_case(body: str, route: str) -> str | None:
    """Return parseHash's conditional segment for ``route``, if present."""
    matches = list(_HASH_CASE.finditer(body))
    for index, match in enumerate(matches):
        if match.group("route") == route:
            end = matches[index + 1].start() if index + 1 < len(matches) else len(body)
            return body[match.start() : end]
    return None


def source_violations(source: str, tabs: list[str]) -> list[str]:
    """Return tab/hash parity violations for supplied board source and tab ids."""
    violations: list[str] = []
    tab_set = set(tabs)
    route_tabs = set(TAB_ROUTES)
    for tab in sorted(tab_set - route_tabs):
        violations.append(f"board tab {tab!r} has no canonical hash route")
    for tab in sorted(route_tabs - tab_set):
        violations.append(f"canonical hash route for {tab!r} has no board tab")

    try:
        sync_body = function_body(source, "syncHash")
        parse_body = function_body(source, "parseHash")
    except ValueError as error:
        return [*violations, str(error)]

    expected_routes = set(TAB_ROUTES.values())
    actual_routes = {match.group("route") for match in _HASH_CASE.finditer(parse_body)}
    for route in sorted(actual_routes - expected_routes):
        violations.append(f"parseHash recognizes route {route!r} without a matching board tab")

    emitted_routes = {match.group("route") for match in _HASH_URL.finditer(sync_body)}
    for route in sorted(emitted_routes - expected_routes):
        violations.append(f"syncHash emits route {route!r} without a matching board tab")

    for tab, route in TAB_ROUTES.items():
        if tab not in tab_set:
            continue
        if f"#/{route}" not in sync_body:
            violations.append(f"syncHash does not emit #/{route} for tab {tab!r}")
        case = parse_case(parse_body, route)
        if case is None:
            violations.append(f"parseHash does not recognize #/{route} for tab {tab!r}")
        elif f'tab: "{tab}"' not in case and tab not in {"squads", "tasks"}:
            violations.append(f"parseHash route #/{route} does not select tab {tab!r}")
        elif tab == "squads" and "parseSquadsHashBody" not in case:
            violations.append("parseHash route #/squads does not select the squads tab")
        elif tab == "tasks" and "parseTasksTabHash" not in case:
            violations.append("parseHash route #/tasks does not select the tasks tab")
    return violations


def main() -> int:
    lint = importlib.import_module("ralphus.docsgen.lint")
    tabs = lint.board_tabs()
    violations = source_violations(CHROME.read_text(encoding="utf-8"), tabs)
    for violation in violations:
        print(violation)
    return int(bool(violations))


if __name__ == "__main__":
    sys.exit(main())
