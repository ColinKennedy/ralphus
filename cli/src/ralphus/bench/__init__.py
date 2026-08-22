"""RAL-94: SVG/HTML graph rendering for benchmark data.

Reads already-stored `durable_min`/stats records and renders them as graphs
(`ralphus.bench.graphs`) — it never runs or times a test itself. Both this
crate's `cli-rs`/`runner`/`daemon`/etc. (Rust, via `bench-harness`/
`bench-macros`) and this package (Python) write into their own subtree of
`bench_data/` in the same record shape this module reads; the two
ecosystems' data never mix.
"""

from __future__ import annotations

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
    "BenchRecord",
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
    "test_data_path",
]
