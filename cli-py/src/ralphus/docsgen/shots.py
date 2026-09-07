"""Generate the deterministic screenshots embedded in docs/site/pages/.

Run via ``uv run ralphus-docs-shots`` (needs the ``docs`` extra plus a
one-time ``uv run playwright install chromium``, and a compiled
``ralphus-librarian`` — see ``librarian_server.py`` — found via
``cargo build -p ralphus-librarian`` or ``$RALPHUS_LIBRARIAN_BIN``). The
normal docs build (``scripts/docs-build.sh``) never calls this — it only
re-renders Markdown against whatever PNGs are already committed, so
regenerating screenshots is an explicit, separate step
(``scripts/docs-screenshots.sh``).
"""

from __future__ import annotations

import sys
from pathlib import Path

from playwright.sync_api import Page, ViewportSize, sync_playwright

from ralphus.docsgen import fixtures
from ralphus.docsgen.librarian_server import librarian_server
from ralphus.docsgen.stub_server import fixture_server

__all__ = ["OUT_DIR", "REPO_ROOT", "SCENARIOS", "VIEWPORT", "main"]

REPO_ROOT = Path(__file__).resolve().parents[4]
OUT_DIR = REPO_ROOT / "docs" / "site" / "pages" / "screenshots"
VIEWPORT: ViewportSize = {"width": 1440, "height": 900}

# Freezes anything that would make two runs of this script produce different
# pixels: CSS transitions/animations/carets, the wall-clock "updated
# HH:MM:SS" label the board writes on every poll, and a stray hover tooltip
# (the same Page is reused across scenarios, so the OS cursor position from
# one scenario's drag can land on a tipped element right after the next
# scenario navigates).
_FREEZE_CSS = """
* { transition: none !important; animation: none !important; caret-color: transparent !important; }
#updated { visibility: hidden !important; }
#board-tip { display: none !important; }
"""


def _log(message: str) -> None:
    print(f"ralphus [docsgen] {message}", file=sys.stderr)


def _goto(page: Page, base_url: str, hash_: str) -> None:
    page.goto(f"{base_url}/{hash_}")
    page.add_style_tag(content=_FREEZE_CSS)


def _shoot(page: Page, name: str) -> None:
    OUT_DIR.mkdir(parents=True, exist_ok=True)
    path = OUT_DIR / f"{name}.png"
    page.screenshot(path=path)
    _log(f"wrote {path.relative_to(REPO_ROOT)}")


def _squads_overview(page: Page) -> None:
    with (
        fixture_server(fixtures.TASKS_ROUTES) as daemon_url,
        librarian_server(daemon_url) as base_url,
    ):
        _goto(page, base_url, "#/squads/squad-000000000004")
        page.wait_for_selector(".squad-item.selected")
        _shoot(page, "squads-overview")


def _squads_session_detail(page: Page) -> None:
    with (
        fixture_server(fixtures.TASKS_ROUTES) as daemon_url,
        librarian_server(daemon_url) as base_url,
    ):
        _goto(page, base_url, "#/squads/squad-000000000004?sel=cell:0:1")
        page.wait_for_selector(".squad-item.selected")
        _shoot(page, "squads-session-detail")


def _tasks_overview(page: Page) -> None:
    with (
        fixture_server(fixtures.TASKS_ROUTES) as daemon_url,
        librarian_server(daemon_url) as base_url,
    ):
        _goto(page, base_url, "#/tasks")
        page.wait_for_selector(".tt-row")
        _shoot(page, "tasks-overview")


def _queue_overview(page: Page) -> None:
    with (
        fixture_server(fixtures.QUEUE_ROUTES) as daemon_url,
        librarian_server(daemon_url) as base_url,
    ):
        _goto(page, base_url, "#/queue")
        # "Ready only" is on by default (hides blocked/excluded rows) and
        # tasks are collapsed by default — turn both off so the full
        # run/task/session hierarchy, readiness badges included, is visible.
        page.evaluate("queueSetReadyOnly(false); queueExpandAll();")
        page.wait_for_selector(f'.q-row.item[data-path="{fixtures.PATH_PURGE}"]')
        _shoot(page, "queue-overview")

        # Drag "provision-database" (a dependency) down past its dependent
        # "run-migrations" and drop it at the very end of the list. The
        # anchoring repair then pushes "run-migrations" down to stay right
        # after it — the "pulled along" highlight is on the dependent this
        # time (see queueAnchoredRepair in librarian/assets/board.html).
        source = page.locator(f'.q-row.item[data-path="{fixtures.PATH_PROVISION}"]')
        last_row = page.locator(f'.q-row.item[data-path="{fixtures.PATH_PURGE}"]')
        src_box = source.bounding_box()
        last_box = last_row.bounding_box()
        assert src_box is not None
        assert last_box is not None
        page.mouse.move(src_box["x"] + src_box["width"] / 2, src_box["y"] + src_box["height"] / 2)
        page.mouse.down()
        page.mouse.move(
            last_box["x"] + last_box["width"] / 2, last_box["y"] + last_box["height"] - 2, steps=8
        )
        page.mouse.up()
        # Move the cursor off the list — the row it dropped on shifted during
        # re-render, and a stray hover would otherwise pop a tooltip over it.
        page.mouse.move(20, 20)
        page.wait_for_selector(".q-row.pulled")
        _shoot(page, "queue-drag-after")


def _reviews(page: Page) -> None:
    with (
        fixture_server(fixtures.REVIEWS_ROUTES) as daemon_url,
        librarian_server(daemon_url) as base_url,
    ):
        _goto(page, base_url, f"#/reviews/{fixtures.REVIEWS_GUARDIAN['id']}")
        page.wait_for_selector(".branch-row")
        _shoot(page, "reviews-overview")

        # The feedback thread only renders once its branch's merge-detail
        # panel is expanded (RAL-272) — click that branch's toggle first.
        page.locator(
            f'[data-click="toggleBranch"][data-branch-id="{fixtures.REVIEWS_ROLLOUT_BRANCH_ID}"]'
        ).click()
        chat = page.locator(".chat-msg").first
        chat.wait_for()
        chat.scroll_into_view_if_needed()
        _shoot(page, "reviews-chat")

        page.locator('button[data-click="toggleManualMenu"]').click()
        page.wait_for_timeout(50)
        _shoot(page, "reviews-manual-checks")


def _resources_overview(page: Page) -> None:
    with (
        fixture_server(fixtures.RESOURCES_ROUTES) as daemon_url,
        librarian_server(daemon_url) as base_url,
    ):
        _goto(page, base_url, "#/resources")
        page.wait_for_selector(".res-table")
        _shoot(page, "resources-overview")


def _cartographer_overview(page: Page) -> None:
    with (
        fixture_server(fixtures.CARTOGRAPHER_ROUTES) as daemon_url,
        librarian_server(daemon_url) as base_url,
    ):
        _goto(page, base_url, "#/cartographer")
        page.wait_for_selector("#cartographer-body table")
        _shoot(page, "cartographer-overview")


def _projects_overview(page: Page) -> None:
    with (
        fixture_server(fixtures.PROJECTS_ROUTES) as daemon_url,
        librarian_server(daemon_url) as base_url,
    ):
        _goto(page, base_url, "#/projects")
        page.wait_for_selector("#projects .proj-table")
        _shoot(page, "projects-overview")


def _machines_overview(page: Page) -> None:
    # The Machines tab has no dedicated URL hash (unlike tasks/queue/reviews/
    # resources/cartographer/projects) — land on the default tab, then switch
    # with the same `showTab` the tab button's onclick calls.
    with (
        fixture_server(fixtures.MACHINES_ROUTES) as daemon_url,
        librarian_server(daemon_url) as base_url,
    ):
        _goto(page, base_url, "#/tasks")
        page.evaluate("showTab('machines', true)")
        page.wait_for_selector("#machines .proj-table")
        _shoot(page, "machines-overview")


def _users_overview(page: Page) -> None:
    # Same no-dedicated-hash situation as Machines above.
    with (
        fixture_server(fixtures.USERS_ROUTES) as daemon_url,
        librarian_server(daemon_url) as base_url,
    ):
        _goto(page, base_url, "#/tasks")
        page.evaluate("showTab('users', true)")
        page.wait_for_selector("#users .proj-table")
        _shoot(page, "users-overview")


def _secrets_overview(page: Page) -> None:
    # Same no-dedicated-hash situation as Machines/Users above.
    with (
        fixture_server(fixtures.SECRETS_ROUTES) as daemon_url,
        librarian_server(daemon_url) as base_url,
    ):
        _goto(page, base_url, "#/tasks")
        page.evaluate("showTab('secrets', true)")
        page.wait_for_selector("#secrets .proj-table")
        _shoot(page, "secrets-overview")


def _triage_overview(page: Page) -> None:
    with (
        fixture_server(fixtures.TRIAGE_ROUTES) as daemon_url,
        librarian_server(daemon_url) as base_url,
    ):
        _goto(page, base_url, "#/triage")
        page.wait_for_selector("#triage .proj-table")
        _shoot(page, "triage-overview")


def _prefs_overview(page: Page) -> None:
    with (
        fixture_server(fixtures.PREFS_ROUTES) as daemon_url,
        librarian_server(daemon_url) as base_url,
    ):
        _goto(page, base_url, "#/prefs")
        page.wait_for_selector("#hidden-items .proj-table")
        _shoot(page, "prefs-overview")


SCENARIOS = (
    _squads_overview,
    _squads_session_detail,
    _tasks_overview,
    _queue_overview,
    _reviews,
    _resources_overview,
    _cartographer_overview,
    _projects_overview,
    _machines_overview,
    _users_overview,
    _secrets_overview,
    _triage_overview,
    _prefs_overview,
)


def main() -> None:
    with sync_playwright() as p:
        browser = p.chromium.launch(headless=True)
        context = browser.new_context(viewport=VIEWPORT, locale="en-US", timezone_id="UTC")
        page = context.new_page()
        for scenario in SCENARIOS:
            _log(f"running {scenario.__name__}")
            scenario(page)
        context.close()
        browser.close()
    _log(f"done — screenshots in {OUT_DIR}")


if __name__ == "__main__":
    main()
