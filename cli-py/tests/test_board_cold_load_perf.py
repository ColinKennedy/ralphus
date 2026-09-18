"""Browser-level cold-navigation performance tests (RAL-414).

Proves each board tab's first, clean-session paint completes inside a
documented budget, using realistic large fixtures (650 worktree-retirement
rows, 250 squads x 4 tasks, 300 projects) served by the same hermetic
`stub_server.fixture_server` + real, compiled `ralphus-librarian` binary
combo `docsgen/shots.py` already uses for screenshots -- no real daemon, no
SQLite, no scheduler involved.

Budget: `RALPHUS_BOARD_COLD_LOAD_BUDGET_MS` (integer milliseconds), default
2000 -- mirrors `daemon/src/perf_timing.rs`'s `BOARD_COLD_LOAD_BUDGET_MS`/
`BUDGET_ENV_VAR` name-for-name. That Rust module backs a *separate* test,
`daemon/tests/board_cold_load_perf.rs`, which measures real-daemon
server-side phase timing; this module is the browser-side half: genuine
network + render time inside an actual Chromium tab.

Each test opens a brand-new `browser.new_context()` -- never a shared or
reused context/page -- so there is zero cookie/cache/localStorage carryover
from an earlier test that could quietly make a warm load look cold. The
board is loaded with `?ralphusTiming=1`, which turns on
`window.RalphusTiming` (`librarian/assets/board/85-perf-timing.js`, dormant
unless that query param is present): once a tab navigation resolves,
`RalphusTiming.getLast(tab)` returns either `None` (nothing recorded for
that tab yet) or `{tab, totalMs, fetchMs, renderMs, server, ts}`.

Two things about that breakdown are easy to mistake for bugs in this test
file and are not:

1. `server` (parsed from the response's `Server-Timing` header) is reliably
   `[]` here. Only the real `ralphus-daemon`, run with `RALPHUS_BOARD_TIMING=1`
   in *its own* environment, ever emits that header -- the plain JSON
   `fixture_server` used throughout this module never does and never will.
   `fetchMs`/`renderMs`/`totalMs` are unaffected: they come from the
   browser's own Resource Timing API, independent of any response header.
2. The Squads tab's very first cold load never populates `RalphusTiming` at
   all (`getLast("squads")` is `None`), even when the page genuinely renders
   real data well inside budget. `librarian/assets/board/80-queue.js`'s boot
   sequence calls `tick()` directly for the default (no-hash / `#/squads`)
   case instead of routing through `showTab()`, and only `showTab()`
   (`25-chrome.js`) wraps a navigation with `RalphusTiming.timeNav`. Every
   other tab's first load here goes through `showTab()` -- either because
   its hash is recognized by the boot dispatch in `80-queue.js`, or, for
   `worktree-retirement` (see below), because the test calls `showTab()`
   itself -- and does get a real breakdown.

Because of point 2, this module's pass/fail assertion is always the
wall-clock elapsed time (`time.perf_counter()` around
`page.goto()`/`page.wait_for_selector()`), never `RalphusTiming` itself --
that breakdown is attached to the failure message purely as extra
diagnostic context, falling back to the literal string
"no client timing available" when it's `None`.

`worktree-retirement` has no dedicated URL hash at all: `parseHash()`
(`25-chrome.js`) and the boot dispatch (`80-queue.js`) recognize
`tasks`/`reviews`/`resources`/`queue`/`cartographer`/`projects`/`machines`/
`triage`/`users`/`secrets`/`prefs`, but never `worktree-retirement` -- the
only way in is `showTab('worktree-retirement', true)`, exactly like
`docsgen/shots.py`'s own `_worktree_retirement_overview` scenario. This test
mirrors that scenario's exact recipe: land on `#/tasks` (itself a real,
supported cold navigation), then call `showTab` via `page.evaluate` as the
very next step on that same fresh context, then wait for the tab's real
data marker -- still one continuous cold-session timing window.

Marked `heavy` (large fixtures, a real subprocess, a real Chromium tab) --
run with `-m heavy`, or exclude with `-m "not heavy"`; a plain
`uv run pytest` still runs them like any other test (no silent default
skip). Needs a compiled `ralphus-librarian` (`cargo build -p
ralphus-librarian`, or `$RALPHUS_LIBRARIAN_BIN`) and Chromium (`uv run
playwright install chromium`) -- skips cleanly, not an error, when the
binary can't be found, via the same `RuntimeError` `binaries.find_binary`
already raises for that case.
"""

from __future__ import annotations

import os
import time
from typing import TYPE_CHECKING, Any

import pytest

from ralphus.docsgen import fixtures
from ralphus.docsgen.binaries import find_librarian_binary
from ralphus.docsgen.librarian_server import librarian_server
from ralphus.docsgen.stub_server import fixture_server

if TYPE_CHECKING:
    from playwright.sync_api import Browser, BrowserContext

_playwright_sync_api = pytest.importorskip("playwright.sync_api")
sync_playwright = _playwright_sync_api.sync_playwright

pytestmark = pytest.mark.heavy

#: Mirrors `daemon/src/perf_timing.rs`'s `BOARD_COLD_LOAD_BUDGET_MS`/`BUDGET_ENV_VAR`.
BUDGET_MS = int(os.environ.get("RALPHUS_BOARD_COLD_LOAD_BUDGET_MS", "2000"))

# ---------------------------------------------------------------------------
# Load-scale fixtures -- built once at import time (pure/deterministic, see
# `fixtures.py`), then wired into one `Routes` dict per tab below.
# ---------------------------------------------------------------------------

_RETIREMENT_ROWS: tuple[fixtures.Json, ...] = fixtures.many_worktree_retirement_rows(650)
_HEAVY_SQUADS: tuple[fixtures.Json, ...] = fixtures.many_squads_with_tasks(
    num_squads=250, tasks_per_squad=4
)
_TASK_INDEX_SQUADS: tuple[fixtures.Json, ...] = fixtures.task_index_squads_from(_HEAVY_SQUADS)
_HEAVY_PROJECTS: tuple[fixtures.Json, ...] = fixtures.many_projects(300)


def _daemon_status(running: int = 0) -> fixtures.Json:
    return {"running": running, "max_concurrent": 12, "running_reviews": []}


def _empty_board() -> fixtures.Json:
    return {"daemon": _daemon_status(), "squads": []}


#: Squads tab (default, `#/`) -- the rich `/api/tasks` `SquadView` shape at 250x4 scale.
SQUADS_ROUTES: fixtures.Routes = {
    "/api/tasks": {"daemon": _daemon_status(), "squads": list(_HEAVY_SQUADS)},
    "/api/guardians": [],
    "/api/resources": {"resources": []},
    "/api/queue": {"items": []},
}

#: Tasks tab (`#/tasks`) -- the compact `/api/task-index` `TaskIndexBoard` shape,
#: derived from the SAME 250x4 fixture as SQUADS_ROUTES (see
#: `fixtures.task_index_squads_from`'s docstring for why the wire shapes differ).
TASKS_ROUTES: fixtures.Routes = {
    "/api/tasks": _empty_board(),
    "/api/task-index": {"daemon": _daemon_status(), "squads": list(_TASK_INDEX_SQUADS)},
    "/api/pull-requests/index": [],
    "/api/projects": {"projects": []},
    "/api/guardians": [],
    "/api/resources": {"resources": []},
    "/api/queue": {"items": []},
}

#: Retirements tab (`showTab('worktree-retirement', true)` -- no dedicated hash) -- 650 rows.
WORKTREE_RETIREMENT_ROUTES: fixtures.Routes = {
    "/api/tasks": _empty_board(),
    "/api/task-index": {"daemon": _daemon_status(), "squads": []},
    "/api/pull-requests/index": [],
    "/api/guardians": [],
    "/api/resources": {"resources": []},
    "/api/queue": {"items": []},
    "/api/worktree-retirements": {"entries": list(_RETIREMENT_ROWS)},
}

#: Projects tab (`#/projects`) -- 300 rows, plus one `/validate` route per
#: project so the tab's own re-validate-on-load pass (`pollProjects`) resolves
#: the same way it would against a real daemon, instead of every row failing
#: validation just because this stub never heard of `/validate`.
PROJECTS_ROUTES: fixtures.Routes = {
    "/api/tasks": _empty_board(),
    "/api/guardians": [],
    "/api/resources": {"resources": []},
    "/api/queue": {"items": []},
    "/api/projects": {"projects": list(_HEAVY_PROJECTS)},
    **{f"/api/projects/{p['name']}/validate": {"valid": True} for p in _HEAVY_PROJECTS},
}


def _skip_if_no_librarian() -> None:
    try:
        find_librarian_binary()
    except RuntimeError as e:
        pytest.skip(str(e))


@pytest.fixture(scope="module")
def browser() -> Any:
    """One real, headless Chromium instance shared by every test in this
    module -- each test still opens its own fresh `new_context()` (see the
    module docstring), so nothing about *what's measured* is shared, only the
    (comparatively slow) browser-process startup cost.
    """
    _skip_if_no_librarian()
    with sync_playwright() as p:
        # `--disable-dev-shm-usage`: GitHub Actions' `ubuntu-latest` runners
        # ship a tiny (64MB) `/dev/shm`, which Chromium uses for renderer
        # shared memory by default. These fixtures render hundreds/thousands
        # of DOM rows per tab -- enough that a renderer can exhaust that
        # 64MB and wedge instead of cleanly crashing, which reads back as
        # `page.goto()` hanging until Playwright's own navigation timeout
        # rather than a fast, obvious failure. This flag makes Chromium fall
        # back to `/tmp` for that shared memory instead.
        b = p.chromium.launch(headless=True, args=["--disable-dev-shm-usage"])
        try:
            yield b
        finally:
            b.close()


def _failure_message(
    tab: str, fixture_size: int, elapsed_ms: float, timing: fixtures.Json | None
) -> str:
    breakdown = timing if timing is not None else "no client timing available"
    return (
        f"board tab {tab!r} cold navigation exceeded its budget -- "
        f"fixture_size={fixture_size} budget_ms={BUDGET_MS} observed_ms={elapsed_ms:.1f} "
        f"timing={breakdown}"
    )


def _cold_nav(
    context: BrowserContext,
    base_url: str,
    hash_: str,
    ready_selector: str,
    tab: str,
    *,
    post_goto_js: str | None = None,
) -> tuple[float, fixtures.Json | None]:
    """One cold navigation on a fresh context: times `page.goto()` through
    `page.wait_for_selector(ready_selector)`, optionally running
    `post_goto_js` (the `worktree-retirement` `showTab()` workaround, see the
    module docstring) in between, and returns
    `(elapsed_ms, RalphusTiming.getLast(tab))`.
    """
    page = context.new_page()
    start = time.perf_counter()
    page.goto(f"{base_url}/?ralphusTiming=1{hash_}")
    if post_goto_js is not None:
        page.evaluate(post_goto_js)
    page.wait_for_selector(ready_selector)
    elapsed_ms = (time.perf_counter() - start) * 1000
    timing = page.evaluate(
        "(t) => (window.RalphusTiming ? window.RalphusTiming.getLast(t) : null)", tab
    )
    return elapsed_ms, timing


def test_squads_tab_cold_load_under_budget(browser: Browser) -> None:
    fixture_size = len(_HEAVY_SQUADS)
    with (
        fixture_server(SQUADS_ROUTES) as daemon_url,
        librarian_server(daemon_url) as base_url,
    ):
        context = browser.new_context()
        try:
            elapsed_ms, timing = _cold_nav(context, base_url, "#/", "#squads .squad-item", "squads")
        finally:
            context.close()
    assert elapsed_ms < BUDGET_MS, _failure_message("squads", fixture_size, elapsed_ms, timing)


def test_tasks_tab_cold_load_under_budget(browser: Browser) -> None:
    fixture_size = len(_TASK_INDEX_SQUADS)
    with (
        fixture_server(TASKS_ROUTES) as daemon_url,
        librarian_server(daemon_url) as base_url,
    ):
        context = browser.new_context()
        try:
            elapsed_ms, timing = _cold_nav(context, base_url, "#/tasks", ".tt-row", "tasks")
        finally:
            context.close()
    assert elapsed_ms < BUDGET_MS, _failure_message("tasks", fixture_size, elapsed_ms, timing)


def test_worktree_retirement_tab_cold_load_under_budget(browser: Browser) -> None:
    fixture_size = len(_RETIREMENT_ROWS)
    with (
        fixture_server(WORKTREE_RETIREMENT_ROUTES) as daemon_url,
        librarian_server(daemon_url) as base_url,
    ):
        context = browser.new_context()
        try:
            elapsed_ms, timing = _cold_nav(
                context,
                base_url,
                "#/tasks",
                "#worktree-retirement .proj-table",
                "worktree-retirement",
                post_goto_js="showTab('worktree-retirement', true)",
            )
        finally:
            context.close()
    assert elapsed_ms < BUDGET_MS, _failure_message(
        "worktree-retirement", fixture_size, elapsed_ms, timing
    )


def test_projects_tab_cold_load_under_budget(browser: Browser) -> None:
    fixture_size = len(_HEAVY_PROJECTS)
    with (
        fixture_server(PROJECTS_ROUTES) as daemon_url,
        librarian_server(daemon_url) as base_url,
    ):
        context = browser.new_context()
        try:
            elapsed_ms, timing = _cold_nav(
                context, base_url, "#/projects", "#projects .proj-table", "projects"
            )
        finally:
            context.close()
    assert elapsed_ms < BUDGET_MS, _failure_message("projects", fixture_size, elapsed_ms, timing)
