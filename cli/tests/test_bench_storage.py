"""Tests for RAL-94 per-test, per-commit JSON storage."""

from __future__ import annotations

from pathlib import Path

import pytest

from ralphus.bench.gitinfo import GitState
from ralphus.bench.stats import compute_stats
from ralphus.bench.storage import (
    SkippedRecord,
    append_record,
    append_skipped,
    find_repo_root,
    load_records,
    record_from_stats,
)
from ralphus.bench.storage import test_data_path as compute_test_data_path


def test_load_records_missing_file_returns_none(tmp_path: Path) -> None:
    assert load_records(tmp_path / "nope.json") is None


def test_append_creates_then_accumulates(tmp_path_factory: pytest.TempPathFactory) -> None:
    # A fresh directory per call (not a fixed tmp_path) so this test tolerates
    # being invoked more than once in-process by the RAL-94 bench harness.
    path = tmp_path_factory.mktemp("append_creates") / "my_test.json"
    bundle = compute_stats([1.0, 2.0], durable_min=1.0)
    record = record_from_stats(GitState(commit="aaa", dirty=False), bundle)

    append_record(path, "pkg::my_test", record)
    after_one = load_records(path)
    assert after_one is not None
    assert after_one.test_id == "pkg::my_test"
    assert len(after_one.records) == 1
    assert after_one.records[0].commit == "aaa"

    record2 = record_from_stats(GitState(commit="bbb", dirty=True), bundle)
    append_record(path, "pkg::my_test", record2)
    after_two = load_records(path)
    assert after_two is not None
    assert len(after_two.records) == 2
    assert [r.commit for r in after_two.records] == ["aaa", "bbb"]
    assert after_two.records[1].dirty is True


def test_test_data_path_layout(tmp_path: Path) -> None:
    path = compute_test_data_path(
        "python",
        Path("tests/test_runner.py"),
        "test_foo",
        repo_root=tmp_path,
    )
    assert path == tmp_path / "bench_data" / "python" / "tests" / "test_runner.py" / "test_foo.json"


def test_test_data_path_hashes_long_identifiers(tmp_path: Path) -> None:
    long_name = "test_foo[" + "really-long-parametrize-id-segment-" * 5 + "]"
    path = compute_test_data_path(
        "python",
        Path("tests/test_runner.py"),
        long_name,
        repo_root=tmp_path,
    )
    assert len(path.stem) <= 80
    # Deterministic: the same long name always maps to the same file, so
    # records keep accumulating in one place instead of scattering.
    path2 = compute_test_data_path(
        "python",
        Path("tests/test_runner.py"),
        long_name,
        repo_root=tmp_path,
    )
    assert path == path2


def test_append_skipped_creates_then_accumulates(
    tmp_path_factory: pytest.TempPathFactory,
) -> None:
    path = tmp_path_factory.mktemp("append_skipped") / "my_test.json"

    append_skipped(path, "pkg::my_test", SkippedRecord(commit="aaa", dirty=False))
    after_one = load_records(path)
    assert after_one is not None
    assert after_one.records == []
    assert len(after_one.skipped) == 1
    assert after_one.skipped[0].commit == "aaa"

    append_skipped(path, "pkg::my_test", SkippedRecord(commit="bbb", dirty=True))
    after_two = load_records(path)
    assert after_two is not None
    assert [s.commit for s in after_two.skipped] == ["aaa", "bbb"]


def test_skipped_and_timed_records_coexist_in_one_file(
    tmp_path_factory: pytest.TempPathFactory,
) -> None:
    path = tmp_path_factory.mktemp("skipped_and_timed") / "my_test.json"
    bundle = compute_stats([1.0, 2.0], durable_min=1.0)
    record = record_from_stats(GitState(commit="aaa", dirty=False), bundle)

    append_record(path, "pkg::my_test", record)
    append_skipped(path, "pkg::my_test", SkippedRecord(commit="bbb", dirty=False))

    loaded = load_records(path)
    assert loaded is not None
    assert len(loaded.records) == 1
    assert len(loaded.skipped) == 1


def test_load_records_defaults_skipped_for_legacy_files_without_the_key(tmp_path: Path) -> None:
    path = tmp_path / "legacy.json"
    path.write_text('{"test_id": "pkg::my_test", "records": []}', encoding="utf-8")
    loaded = load_records(path)
    assert loaded is not None
    assert loaded.skipped == []


def test_test_data_path_falls_back_to_filename_when_source_outside_repo_root(
    tmp_path_factory: pytest.TempPathFactory,
) -> None:
    # source_path resolves under a sibling directory, not under repo_root --
    # e.g. pytest invoked against a checkout other than the one repo_root was
    # discovered from. Must not silently discard the bench_data/<language>
    # prefix (a bare `root / absolute_path` join in pathlib does exactly that).
    tmp_path = tmp_path_factory.mktemp("outside_source")
    repo_root = tmp_path / "repo"
    repo_root.mkdir()
    outside_source = tmp_path / "elsewhere" / "test_module.py"
    outside_source.parent.mkdir()
    outside_source.write_text("", encoding="utf-8")

    path = compute_test_data_path(
        "python",
        outside_source,
        "test_foo",
        repo_root=repo_root,
    )
    assert path == repo_root / "bench_data" / "python" / "test_module.py" / "test_foo.json"


def test_find_repo_root_walks_upward(tmp_path_factory: pytest.TempPathFactory) -> None:
    tmp_path = tmp_path_factory.mktemp("repo_root_walk")
    (tmp_path / ".git").mkdir()
    nested = tmp_path / "a" / "b" / "c"
    nested.mkdir(parents=True)
    assert find_repo_root(nested) == tmp_path


def test_find_repo_root_raises_when_no_git_dir(tmp_path_factory: pytest.TempPathFactory) -> None:
    tmp_path = tmp_path_factory.mktemp("no_git_dir")
    lone = tmp_path / "no-git-anywhere-above"
    lone.mkdir()
    with pytest.raises(RuntimeError, match="no git repository"):
        # tmp_path itself has no .git, and pytest's base temp dir tree
        # shouldn't either, so this should walk to the filesystem root and fail.
        find_repo_root(lone)
