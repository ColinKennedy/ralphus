"""Statistics bundle computed from a durable-minimum run's raw samples (RAL-94).

Persisted alongside `durable_min` for every per-commit record, even though only
`durable_min` and the file-level summary graph are required to be visualized —
see RAL-94 acceptance criteria.
"""

from __future__ import annotations

import statistics
from dataclasses import dataclass, field

__all__ = ["StatsBundle", "compute_stats"]


@dataclass
class StatsBundle:
    """Full statistics bundle for one test's samples at one commit."""

    durable_min: float
    max: float
    mean: float
    median: float
    stddev: float
    iqr: float
    outliers: list[float] = field(default_factory=list)
    samples: list[float] = field(default_factory=list)


def compute_stats(samples: list[float], durable_min: float) -> StatsBundle:
    """Compute the full stats bundle from raw samples.

    Outliers are flagged via Tukey's method: values outside 1.5*IQR from Q1/Q3.
    Requires at least two samples (guaranteed by `run_durable_min`, since
    `patience >= 1` always yields at least two invocations).
    """
    if len(samples) < 2:
        raise ValueError(f"compute_stats requires >= 2 samples, got {len(samples)}")

    q1, _, q3 = statistics.quantiles(samples, n=4)
    iqr = q3 - q1
    lower_fence = q1 - 1.5 * iqr
    upper_fence = q3 + 1.5 * iqr
    outliers = sorted(d for d in samples if d < lower_fence or d > upper_fence)

    return StatsBundle(
        durable_min=durable_min,
        max=max(samples),
        mean=statistics.mean(samples),
        median=statistics.median(samples),
        stddev=statistics.stdev(samples),
        iqr=iqr,
        outliers=outliers,
        samples=list(samples),
    )
