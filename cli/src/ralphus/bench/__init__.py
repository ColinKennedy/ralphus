"""RAL-94: opt-in, in-process, serial benchmark harness for Python tests.

Not part of the normal `pytest` invocation — see `ralphus.bench.pytest_plugin`
for the `--ralphus-bench` opt-in flag and `ralphus.bench.graphs` for the SVG
graph generator. Rust tests are tracked separately (see `bench-macros` /
`bench-harness` in the Cargo workspace); the two ecosystems' data never mix.
"""

from __future__ import annotations

from ralphus.bench.durable_min import DEFAULT_PATIENCE, DurableMinResult, run_durable_min
from ralphus.bench.gitinfo import GitState, current_git_state
from ralphus.bench.stats import StatsBundle, compute_stats
from ralphus.bench.storage import (
    BenchRecord,
    Language,
    TestRecordFile,
    append_record,
    find_repo_root,
    load_records,
    record_from_stats,
    test_data_path,
)

__all__ = [
    "DEFAULT_PATIENCE",
    "BenchRecord",
    "DurableMinResult",
    "GitState",
    "Language",
    "StatsBundle",
    "TestRecordFile",
    "append_record",
    "compute_stats",
    "current_git_state",
    "find_repo_root",
    "load_records",
    "record_from_stats",
    "run_durable_min",
    "test_data_path",
]
