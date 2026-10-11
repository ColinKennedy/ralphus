"""Tests for RAL-604 per-test process-count tracking."""

from __future__ import annotations

from pathlib import Path

import pytest

from ralphus.proccount.collect import read_run_dir, reduce_runs
from ralphus.proccount.graph import render_html, render_svg
from ralphus.proccount.manifest import TaggedTest, load_manifest, nextest_filter
from ralphus.proccount.storage import CountRecord, load_records, save_records, upsert_record


def test_reduce_runs_takes_lowest_and_drops_tests_missing_from_any_run() -> None:
    runs = [{"a t1": 5, "b t2": 3}, {"a t1": 4}, {"a t1": 6, "b t2": 3}]
    counts, dropped = reduce_runs(runs, ["a t1", "b t2"])
    assert counts == {"a t1": 4}
    assert dropped == ["b t2"]


def test_read_run_dir_parses_wrapper_files(tmp_path: Path) -> None:
    (tmp_path / "x.count").write_text("bin some::test\t7\n", encoding="utf-8")
    (tmp_path / "bad.count").write_text("bin other\tnope\n", encoding="utf-8")
    assert read_run_dir(tmp_path) == {"bin some::test": 7}


def test_manifest_rejects_a_test_with_two_tags(tmp_path: Path) -> None:
    path = tmp_path / "tags.toml"
    path.write_text(
        '[[test]]\nid = "b t"\ntag = "one"\n[[test]]\nid = "b t"\ntag = "two"\n', encoding="utf-8"
    )
    with pytest.raises(ValueError, match="more than one tag"):
        load_manifest(path)


def test_manifest_loads_and_builds_filter(tmp_path: Path) -> None:
    path = tmp_path / "tags.toml"
    path.write_text('[[test]]\nid = "pkg mod::t"\ntag = "x"\n', encoding="utf-8")
    tests = load_manifest(path)
    assert tests == [TaggedTest("pkg", "mod::t", "x")]
    assert nextest_filter(tests) == "(binary_id(=pkg) & test(=mod::t))"


def test_upsert_leaves_existing_tests_untouched_when_a_test_is_added() -> None:
    records = [CountRecord("v0.0.2", "abc", "linux", {"a t1": 4})]
    upsert_record(records, CountRecord("v0.0.2", "def", "linux", {"b t2": 9}))
    assert records[0].counts == {"a t1": 4, "b t2": 9}


def test_save_sorts_versions_numerically(tmp_path: Path) -> None:
    path = tmp_path / "records.json"
    save_records(
        path,
        [CountRecord("v0.0.10", "c", "linux"), CountRecord("v0.0.2", "b", "linux")],
    )
    assert [r.label for r in load_records(path)] == ["v0.0.2", "v0.0.10"]


def test_graph_has_one_polyline_per_test() -> None:
    tests = [TaggedTest("b", "t1", "x"), TaggedTest("b", "t2", "x")]
    records = [
        CountRecord("v0.0.1", "a", "linux", {"b t1": 5, "b t2": 2}),
        CountRecord("v0.0.2", "b", "linux", {"b t1": 3, "b t2": 2}),
    ]
    svg = render_svg(records, tests)
    assert svg.count("<polyline") == 2
    assert "b t1" in render_html(records, tests)
