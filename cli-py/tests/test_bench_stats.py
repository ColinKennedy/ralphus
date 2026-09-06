"""Tests for the RAL-94 stats-bundle computation (durable_min + Tukey outliers)."""

from __future__ import annotations

import pytest

from ralphus.bench.stats import compute_stats


def test_basic_bundle_matches_hand_computed_values() -> None:
    bundle = compute_stats([1.0, 2.0, 3.0, 4.0, 5.0], durable_min=1.0)
    assert bundle.durable_min == 1.0
    assert bundle.max == 5.0
    assert bundle.mean == pytest.approx(3.0)
    assert bundle.median == pytest.approx(3.0)
    assert bundle.stddev == pytest.approx(1.5811388300841898)
    assert bundle.iqr == pytest.approx(3.0)
    assert bundle.outliers == []
    assert bundle.samples == [1.0, 2.0, 3.0, 4.0, 5.0]


def test_flags_tukey_outliers() -> None:
    bundle = compute_stats([1.0, 2.0, 2.0, 2.0, 2.0, 2.0, 100.0], durable_min=1.0)
    assert 100.0 in bundle.outliers


def test_requires_at_least_two_samples() -> None:
    with pytest.raises(ValueError, match=">= 2 samples"):
        compute_stats([1.0], durable_min=1.0)
