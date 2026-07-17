"""The "durable minimum" repeated-invocation algorithm (RAL-94).

Runs a zero-argument callable in-process, strictly serially, repeatedly, keeping
the best (lowest) wall-clock duration seen and bailing out once `patience`
consecutive non-improving runs have been observed. This is the sole execution
loop for a benchmarked test — there is no separate warmup or collection pass,
so every duration it observes is retained by the caller for the stats bundle.
"""

from __future__ import annotations

import time
from collections.abc import Callable
from dataclasses import dataclass, field

__all__ = ["DEFAULT_PATIENCE", "DurableMinResult", "run_durable_min"]

DEFAULT_PATIENCE = 10


@dataclass
class DurableMinResult:
    """Outcome of one durable-minimum run: the best duration plus every sample."""

    durable_min: float
    samples: list[float] = field(default_factory=list)


def run_durable_min(
    fn: Callable[[], None],
    *,
    patience: int = DEFAULT_PATIENCE,
    clock: Callable[[], float] = time.perf_counter,
) -> DurableMinResult:
    """Repeatedly call `fn`, tracking the durable minimum duration.

    best = None; remaining = patience.
    Each iteration calls fn() once and times it. A new best resets `remaining`
    to `patience`; a non-improving run decrements it. Stops when `remaining`
    reaches 0. `patience` must be >= 1 (a value of 1 means: stop as soon as one
    run fails to improve on the best seen so far).
    """
    if patience < 1:
        raise ValueError(f"patience must be >= 1, got {patience}")

    best: float | None = None
    remaining = patience
    samples: list[float] = []

    while True:
        start = clock()
        fn()
        duration = clock() - start
        samples.append(duration)

        if best is None or duration < best:
            best = duration
            remaining = patience
        else:
            remaining -= 1

        if remaining == 0:
            break

    assert best is not None
    return DurableMinResult(durable_min=best, samples=samples)
