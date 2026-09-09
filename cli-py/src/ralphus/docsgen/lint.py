"""Lint: every board tab has a documented, embedded "main" screenshot.

Reads the GUI's own list of tabs — the ``TABS`` JS array in
``librarian/assets/board.html`` or, since RAL-362, one of the
``librarian/assets/board/*.js`` chunks — and, for each one, requires:

1. A mandatory "main" screenshot at
   ``docs/site/pages/screenshots/<tab>-overview.png``.
2. A view page at ``docs/site/pages/views/<tab>.md``.
3. That page's Markdown source actually embeds the main screenshot (an
   image reference containing ``screenshots/<tab>-overview.png``).

Checking only that *some* screenshot file exists on disk isn't enough — the
PNG can go orphaned (page deleted, `<img>` tag dropped) while the file itself
sits untouched in the screenshots directory, which would silently pass a
weaker check. This mirrors what a reader would actually see: no page, or a
page that no longer shows the picture, means the tab isn't documented no
matter what's still sitting in the screenshots folder.

Deliberately dependency-free (stdlib only, no Playwright/MkDocs import) so it
runs in the base CI job — this is the cheap check that runs on every PR; the
actual screenshot regeneration is a separate, deliberately-not-every-PR
workflow (see DOCS_PLAN.local.md).
"""

from __future__ import annotations

import re
import sys
from dataclasses import dataclass, field
from pathlib import Path

__all__ = [
    "BOARD_CHUNKS_DIR",
    "BOARD_HTML",
    "MAIN_SUFFIX",
    "PAGES_DIR",
    "REPO_ROOT",
    "SCREENSHOTS_DIR",
    "VIEWS_DIR",
    "TabProblems",
    "board_tabs",
    "check_tab",
    "main",
]

REPO_ROOT = Path(__file__).resolve().parents[4]
BOARD_HTML = REPO_ROOT / "librarian" / "assets" / "board.html"
# The board's JS lives in global-scope chunks under `board/` — RAL-362 moved
# the tab bar (and its `TABS` array) out of board.html into one of them.
BOARD_CHUNKS_DIR = REPO_ROOT / "librarian" / "assets" / "board"
PAGES_DIR = REPO_ROOT / "docs" / "site" / "pages"
SCREENSHOTS_DIR = PAGES_DIR / "screenshots"
VIEWS_DIR = PAGES_DIR / "views"

# The mandatory "main" screenshot per tab is named "<tab>-overview.png" —
# matches the convention every existing view page already follows.
MAIN_SUFFIX = "overview"

_TABS_RE = re.compile(r"const TABS\s*=\s*\[([^\]]*)\]")
_TAB_NAME_RE = re.compile(r'"([^"]+)"')


def _log(message: str) -> None:
    print(f"ralphus [docsgen] {message}", file=sys.stderr)


def board_tabs() -> list[str]:
    """The GUI's own list of tabs, parsed from the board's `TABS` array.

    The array used to live in board.html; since RAL-362 it lives in a
    board chunk, so both locations are searched.
    """
    sources = [BOARD_HTML, *sorted(BOARD_CHUNKS_DIR.glob("*.js"))]
    for source in sources:
        match = _TABS_RE.search(source.read_text(encoding="utf-8"))
        if match:
            return _TAB_NAME_RE.findall(match.group(1))
    raise RuntimeError(f"could not find `const TABS = [...]` in {[str(s) for s in sources]}")


@dataclass
class TabProblems:
    tab: str
    reasons: list[str] = field(default_factory=list)


def check_tab(tab: str) -> TabProblems:
    """Verify `tab` has a main screenshot AND a view page that embeds it."""
    problems = TabProblems(tab)
    main_name = f"{tab}-{MAIN_SUFFIX}.png"

    if not (SCREENSHOTS_DIR / main_name).is_file():
        problems.reasons.append(f"missing main screenshot docs/site/pages/screenshots/{main_name}")

    page = VIEWS_DIR / f"{tab}.md"
    if not page.is_file():
        problems.reasons.append(f"missing view page docs/site/pages/views/{tab}.md")
        return problems  # nothing left to check without a page to read

    if f"screenshots/{main_name}" not in page.read_text(encoding="utf-8"):
        problems.reasons.append(
            f"docs/site/pages/views/{tab}.md doesn't embed screenshots/{main_name}"
        )
    return problems


def main() -> None:
    tabs = board_tabs()
    failing = [p for tab in tabs if (p := check_tab(tab)).reasons]
    if failing:
        for problem in failing:
            for reason in problem.reasons:
                _log(f"FAIL [{problem.tab}] {reason}")
        _log("regenerate screenshots with: bash scripts/docs-screenshots.sh")
        sys.exit(1)
    _log(f"PASS — every tab has a documented main screenshot ({', '.join(tabs)})")


if __name__ == "__main__":
    main()
