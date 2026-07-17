"""Tests for the RAL-94 durable-minimum repeated-invocation algorithm."""

from __future__ import annotations

from collections.abc import Iterator

import pytest

from ralphus.bench.durable_min import run_durable_min


def _fake_clock(durations: list[float]) -> Iterator[float]:
    """Yields timestamps two-at-a-time (start, end) per duration, since
    `run_durable_min` calls the clock once before and once after each
    invocation (`start = clock(); ...; d = clock() - start`)."""
    total = 0.0
    for d in durations:
        yield total
        total += d
        yield total


def test_stops_after_patience_non_improving_runs() -> None:
    # durations: 5, 4, 3 (new best), 3.5, 3.2 -> patience=2 stops once two
    # non-improving runs (3.5 then 3.2) follow the best.
    clock = _fake_clock([5.0, 4.0, 3.0, 3.5, 3.2])
    result = run_durable_min(lambda: None, patience=2, clock=lambda: next(clock))
    assert result.samples == pytest.approx([5.0, 4.0, 3.0, 3.5, 3.2])
    assert result.durable_min == pytest.approx(3.0)


def test_patience_resets_on_each_new_best() -> None:
    clock = _fake_clock([5.0, 4.0, 3.0, 3.0])
    result = run_durable_min(lambda: None, patience=1, clock=lambda: next(clock))
    assert result.samples == pytest.approx([5.0, 4.0, 3.0, 3.0])
    assert result.durable_min == pytest.approx(3.0)


def test_patience_one_stops_after_first_non_improvement() -> None:
    clock = _fake_clock([3.0, 5.0])
    calls = 0

    def call_once() -> None:
        nonlocal calls
        calls += 1

    result = run_durable_min(call_once, patience=1, clock=lambda: next(clock))
    assert len(result.samples) == 2
    assert calls == 2


def test_rejects_patience_below_one() -> None:
    with pytest.raises(ValueError, match="patience must be >= 1"):
        run_durable_min(lambda: None, patience=0)


def test_default_patience_runs_real_clock() -> None:
    calls = 0

    def call_once() -> None:
        nonlocal calls
        calls += 1

    result = run_durable_min(call_once)
    # No control over real wall-clock jitter, but the loop must always
    # terminate and durable_min must be the true minimum of what it saw.
    assert result.durable_min == min(result.samples)
    assert calls == len(result.samples)
