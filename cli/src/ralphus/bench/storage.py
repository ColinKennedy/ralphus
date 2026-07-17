"""Per-test, per-commit JSON storage for benchmark records (RAL-94).

One file per test, accumulating one record per commit run. Python and Rust
results live under separate top-level directories (`bench_data/python/` and
`bench_data/rust/`) so the two ecosystems' data is never intermixed; the
directory layout otherwise mirrors the test's source path.
"""

from __future__ import annotations

import hashlib
import json
from dataclasses import asdict, dataclass, field
from pathlib import Path
from typing import Literal

from ralphus.bench.gitinfo import GitState
from ralphus.bench.stats import StatsBundle

__all__ = [
    "BenchRecord",
    "Language",
    "SkippedRecord",
    "TestRecordFile",
    "append_record",
    "append_skipped",
    "find_repo_root",
    "load_records",
    "record_from_stats",
    "test_data_path",
]

Language = Literal["python", "rust"]

_DATA_ROOT_NAME = "bench_data"

# Windows MAX_PATH is 260 chars; parametrized pytest test names (e.g.
# `test_foo[really-long-parameter-id-...]`) can blow past a safe filename
# budget on their own. Keep individual filename stems well under that ceiling
# by hashing long identifiers instead of embedding them verbatim (RAL-94 Q3).
_MAX_FILENAME_STEM_LEN = 80


def _short_identifier(name: str) -> str:
    """Bounds a test-name-derived filename stem, hashing it if it's long.

    Short names pass through unchanged (keeps filenames human-readable for
    the common case). Names over the length budget are truncated and suffixed
    with a content hash so two long-but-differently-tailed names never collide.
    """
    if len(name) <= _MAX_FILENAME_STEM_LEN:
        return name
    digest = hashlib.sha256(name.encode("utf-8")).hexdigest()[:16]
    keep = _MAX_FILENAME_STEM_LEN - len(digest) - 1
    return f"{name[:keep]}_{digest}"


@dataclass
class BenchRecord:
    """One commit's worth of bench data for a single test."""

    commit: str
    dirty: bool
    durable_min: float
    max: float
    mean: float
    median: float
    stddev: float
    iqr: float
    outliers: list[float] = field(default_factory=list)
    samples: list[float] = field(default_factory=list)


@dataclass
class SkippedRecord:
    """One commit where the test was skipped rather than timed (RAL-94 Q5).

    Kept in a list separate from `records` (rather than folded into
    `BenchRecord` with optional timing fields) precisely so a skipped commit
    can never accidentally end up plotted as a trend-line point — graph
    rendering only ever reads `records`.
    """

    commit: str
    dirty: bool


@dataclass
class TestRecordFile:
    """The full accumulated history for one test, across commits."""

    test_id: str
    records: list[BenchRecord] = field(default_factory=list)
    skipped: list[SkippedRecord] = field(default_factory=list)


def record_from_stats(git_state: GitState, stats: StatsBundle) -> BenchRecord:
    """Build a BenchRecord from a git state + stats bundle pair."""
    return BenchRecord(
        commit=git_state.commit,
        dirty=git_state.dirty,
        durable_min=stats.durable_min,
        max=stats.max,
        mean=stats.mean,
        median=stats.median,
        stddev=stats.stddev,
        iqr=stats.iqr,
        outliers=list(stats.outliers),
        samples=list(stats.samples),
    )


def find_repo_root(start: Path) -> Path:
    current = start.resolve()
    while True:
        if (current / ".git").exists():
            return current
        parent = current.parent
        if parent == current:
            raise RuntimeError(f"no git repository found above {start}")
        current = parent


def test_data_path(
    language: Language,
    source_path: Path,
    test_name: str,
    *,
    repo_root: Path | None = None,
) -> Path:
    """Path to the JSON file accumulating one test's records across commits.

    `source_path` may be absolute or relative; it is stored relative to the
    repo root so the layout is portable across checkouts. Layout:
    `<repo_root>/bench_data/<language>/<source_path>/<test_name>.json`.
    """
    root = repo_root if repo_root is not None else find_repo_root(Path.cwd())
    source_path = source_path.resolve() if source_path.is_absolute() else source_path
    try:
        relative = source_path.relative_to(root) if source_path.is_absolute() else source_path
    except ValueError:
        # source_path lives outside `root` (e.g. pytest invoked against a
        # different checkout than the one `root` was discovered from).
        # `root / relative` would silently discard everything left of an
        # absolute `relative` (pathlib: joining an absolute path replaces the
        # whole path), corrupting the bench_data/<language> prefix — so fall
        # back to just the file's own name instead of the full absolute path.
        relative = Path(source_path.name)
    return root / _DATA_ROOT_NAME / language / relative / f"{_short_identifier(test_name)}.json"


def load_records(path: Path) -> TestRecordFile | None:
    """Load an existing per-test record file, or None if it does not exist yet."""
    if not path.exists():
        return None
    raw = json.loads(path.read_text(encoding="utf-8"))
    return TestRecordFile(
        test_id=raw["test_id"],
        records=[BenchRecord(**r) for r in raw["records"]],
        # `skipped` was added after the first release of this format; older
        # files on disk simply don't have the key.
        skipped=[SkippedRecord(**s) for s in raw.get("skipped", [])],
    )


def _write(path: Path, record_file: TestRecordFile) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(asdict(record_file), indent=2) + "\n", encoding="utf-8")


def append_record(path: Path, test_id: str, record: BenchRecord) -> TestRecordFile:
    """Append one commit's record to a test's data file, creating it if needed."""
    existing = load_records(path)
    if existing is None:
        existing = TestRecordFile(test_id=test_id)
    existing.records.append(record)
    _write(path, existing)
    return existing


def append_skipped(path: Path, test_id: str, skipped: SkippedRecord) -> TestRecordFile:
    """Record that a test was skipped at this commit, creating the file if needed.

    Stored explicitly (rather than the test simply having no entry for this
    commit) so a skip is distinguishable from "this test didn't exist yet" —
    see RAL-94 Q5. Never contributes a point to the trend line: only
    `.records` is read by graph rendering.
    """
    existing = load_records(path)
    if existing is None:
        existing = TestRecordFile(test_id=test_id)
    existing.skipped.append(skipped)
    _write(path, existing)
    return existing
