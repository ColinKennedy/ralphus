"""Pytest plugin implementing the RAL-94 opt-in benchmark harness.

Registered as a `pytest11` entry point so it is always discoverable, but its
`pytest_pyfunc_call` hook only takes over test execution when `--ralphus-bench`
is passed — a plain `pytest` invocation is completely unaffected, satisfying
the "not run as part of the normal pytest invocation" requirement.

When active, each test function is invoked directly (bypassing pytest's own
single-call default) through `run_durable_min`, in-process and strictly
serially, and the resulting stats bundle is persisted per test, per commit.
"""

from __future__ import annotations

import functools
import re
import sys
from collections.abc import Generator
from pathlib import Path

import pytest

from ralphus.bench.durable_min import DEFAULT_PATIENCE, run_durable_min
from ralphus.bench.gitinfo import GitState, current_git_state
from ralphus.bench.stats import compute_stats
from ralphus.bench.storage import (
    SkippedRecord,
    append_record,
    append_skipped,
    record_from_stats,
    test_data_path,
)

__all__ = [
    "pytest_addoption",
    "pytest_collection_modifyitems",
    "pytest_configure",
    "pytest_pyfunc_call",
    "pytest_runtest_makereport",
]

_UNSAFE_FILENAME_CHARS = re.compile(r"[^A-Za-z0-9_.\-]")


def pytest_addoption(parser: pytest.Parser) -> None:
    group = parser.getgroup("ralphus-bench")
    group.addoption(
        "--ralphus-bench",
        action="store_true",
        default=False,
        help=(
            "Run tests through the RAL-94 durable-minimum benchmark harness "
            "(in-process, serial, repeated invocation) instead of pytest's "
            "normal single call per test. Opt-in only; results are persisted "
            "under bench_data/python/."
        ),
    )


def pytest_configure(config: pytest.Config) -> None:
    config.addinivalue_line(
        "markers",
        "ralphus_bench(patience=N): configure the RAL-94 benchmark harness's "
        "patience (consecutive non-improving runs before stopping) for this "
        "test. Only takes effect when running with --ralphus-bench; default "
        f"patience is {DEFAULT_PATIENCE}.",
    )
    config.addinivalue_line(
        "markers",
        "ollama: marks a test that calls out to a live LLM (e.g. Ollama). "
        "Always deselected from a --ralphus-bench run, regardless of "
        "Ollama's availability — a real model call is slow and its "
        "duration reflects inference/network latency, not this repo's own "
        "performance, so it is not meaningful timing-regression data.",
    )
    config.addinivalue_line(
        "markers",
        "no_bench: marks a test that must never be invoked more than once "
        "in-process. `run_durable_min` always calls a test's body at least "
        "twice (regardless of patience), which breaks a test that hits a "
        "real socket with a real timeout (duration reflects network/OS "
        "latency, not this repo's performance) or whose setup/assertions "
        "assume exactly-once execution against fixture state that persists "
        "across those repeated calls (e.g. a `pytester` sandbox, an "
        "accumulating file). Always deselected from a --ralphus-bench run; "
        "inert otherwise.",
    )


_EXCLUDED_MARKERS = ("ollama", "no_bench")


def pytest_collection_modifyitems(config: pytest.Config, items: list[pytest.Item]) -> None:
    """Deselects `ollama`/`no_bench`-marked tests entirely when active.

    Excluded outright (not run once untimed, not recorded as `skipped`)
    rather than merely left untimed, since a plain pytest run already
    exercises them and this is specifically about what `--ralphus-bench`
    calls.
    """
    if not config.getoption("ralphus_bench"):
        return
    keep = []
    deselected = []
    for item in items:
        if any(item.get_closest_marker(name) is not None for name in _EXCLUDED_MARKERS):
            deselected.append(item)
        else:
            keep.append(item)
    if deselected:
        config.hook.pytest_deselected(items=deselected)
        items[:] = keep


@functools.cache
def _cached_git_state(cwd: Path) -> GitState:
    """Commit/dirty state is constant for the lifetime of one pytest session,
    per working directory.

    Keyed by `cwd` (not a bare no-arg cache) because a benchmarked test can
    itself run a nested pytest session against a different git repo — e.g.
    `test_bench_pytest_plugin.py`'s `pytester`-based tests, which `git init`
    their own sandbox. A single unkeyed cache would freeze whichever repo's
    state was computed first and silently reuse it for every other cwd for
    the rest of the process.
    """
    return current_git_state(cwd)


def _sanitize_filename(name: str) -> str:
    return _UNSAFE_FILENAME_CHARS.sub("_", name)


def _patience_for(item: pytest.Function) -> int:
    marker = item.get_closest_marker("ralphus_bench")
    if marker is not None and "patience" in marker.kwargs:
        return int(marker.kwargs["patience"])
    return DEFAULT_PATIENCE


@pytest.hookimpl(tryfirst=True)
def pytest_pyfunc_call(pyfuncitem: pytest.Function) -> bool | None:
    if not pyfuncitem.config.getoption("ralphus_bench"):
        return None

    patience = _patience_for(pyfuncitem)
    testfunction = pyfuncitem.obj
    argnames = pyfuncitem._fixtureinfo.argnames
    testargs = {name: pyfuncitem.funcargs[name] for name in argnames}

    def call_once() -> None:
        testfunction(**testargs)

    result = run_durable_min(call_once, patience=patience)
    bundle = compute_stats(result.samples, result.durable_min)

    path = test_data_path("python", pyfuncitem.path, _sanitize_filename(pyfuncitem.name))
    record = record_from_stats(_cached_git_state(Path.cwd()), bundle)
    append_record(path, pyfuncitem.nodeid, record)

    print(
        f"ralphus [bench] test benchmarked test={pyfuncitem.nodeid} "
        f"durable_min={result.durable_min:.6f}s samples={len(result.samples)} "
        f"patience={patience}",
        file=sys.stderr,
    )
    return True


@pytest.hookimpl(wrapper=True)
def pytest_runtest_makereport(
    item: pytest.Item, call: pytest.CallInfo[None]
) -> Generator[None, pytest.TestReport, pytest.TestReport]:
    """Persists an explicit `skipped` marker for benched tests (RAL-94 Q5).

    Runs regardless of *why* the test was skipped — a `skip`/`skipif` marker
    (which short-circuits during the setup phase, before `pytest_pyfunc_call`
    ever runs) or a mid-test `pytest.skip()` call (which raises out of
    `call_once()` inside the durable-min loop above, aborting it before any
    timing data is recorded). Checking the report here, after the fact,
    catches both uniformly instead of duplicating skip-detection logic.
    """
    report = yield
    if (
        item.config.getoption("ralphus_bench")
        and isinstance(item, pytest.Function)
        and report.skipped
        and call.when in ("setup", "call")
    ):
        path = test_data_path("python", item.path, _sanitize_filename(item.name))
        git_state = _cached_git_state(Path.cwd())
        skipped = SkippedRecord(commit=git_state.commit, dirty=git_state.dirty)
        append_skipped(path, item.nodeid, skipped)
        print(
            f"ralphus [bench] test skipped test={item.nodeid}",
            file=sys.stderr,
        )
    return report
