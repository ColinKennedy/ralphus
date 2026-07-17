"""Tests for RAL-94 SVG graph generation."""

from __future__ import annotations

from pathlib import Path

from ralphus.bench.graphs import (
    FILE_MULTILINE_FILENAME,
    FILE_SUMMARY_FILENAME,
    GROUP_INDEX_FILENAME,
    LANGUAGE_INDEX_FILENAME,
    _hash_color,
    discover_file_groups,
    generate_all_graphs,
    render_file_multiline_svg,
    render_file_summary_svg,
    render_root_index_html,
    render_test_svg,
)
from ralphus.bench.storage import BenchRecord
from ralphus.bench.storage import TestRecordFile as RecordFile


def _record(commit: str, durable_min: float, *, dirty: bool = False) -> BenchRecord:
    return BenchRecord(
        commit=commit,
        dirty=dirty,
        durable_min=durable_min,
        max=durable_min * 2,
        mean=durable_min * 1.5,
        median=durable_min * 1.4,
        stddev=0.001,
        iqr=0.0005,
        outliers=[],
        samples=[durable_min, durable_min * 2],
    )


def test_hash_color_is_deterministic() -> None:
    a = _hash_color("core/src/schema.rs::my_test")
    b = _hash_color("core/src/schema.rs::my_test")
    assert a == b
    assert a.startswith("#")
    assert len(a) == 7


def test_hash_color_differs_for_different_keys() -> None:
    assert _hash_color("a") != _hash_color("b")


def test_render_test_svg_includes_commit_labels_and_dirty_marker() -> None:
    test = RecordFile(
        test_id="tests/test_x.py::test_foo",
        records=[_record("aaaaaaa1234", 0.01), _record("bbbbbbb5678", 0.02, dirty=True)],
    )
    svg = render_test_svg(test)
    assert "<svg" in svg
    assert "aaaaaaa" in svg
    assert "bbbbbbb*" in svg  # dirty commit gets a trailing '*'


def test_render_test_svg_omits_missing_commits() -> None:
    # A test with only one record simply has a one-point line; no error.
    test = RecordFile(test_id="t", records=[_record("only1", 0.01)])
    svg = render_test_svg(test)
    assert "<svg" in svg


def test_render_test_svg_surfaces_skip_count_but_never_plots_it() -> None:
    from ralphus.bench.storage import SkippedRecord

    test = RecordFile(
        test_id="tests/test_x.py::test_foo",
        records=[_record("aaaaaaa1234", 0.01)],
        skipped=[SkippedRecord(commit="ccccccc9999", dirty=False)],
    )
    svg = render_test_svg(test)
    assert "(1 skipped)" in svg
    # The skipped commit never appears as a plotted x-axis label.
    assert "ccccccc" not in svg


def test_render_file_multiline_svg_uses_letters_and_legend() -> None:
    tests = {
        "test_a": RecordFile(test_id="test_a", records=[_record("c1", 0.01)]),
        "test_b": RecordFile(test_id="test_b", records=[_record("c1", 0.02)]),
    }
    svg = render_file_multiline_svg("mod.py", tests)
    assert ">A<" in svg
    assert ">B<" in svg
    assert "A = test_a" in svg
    assert "B = test_b" in svg


def test_render_file_summary_svg_has_four_series() -> None:
    tests = {
        "test_a": RecordFile(test_id="test_a", records=[_record("c1", 0.01), _record("c2", 0.03)]),
        "test_b": RecordFile(test_id="test_b", records=[_record("c1", 0.02), _record("c2", 0.01)]),
    }
    svg = render_file_summary_svg("mod.py", tests)
    for label in ("min", "median", "mean", "max"):
        assert f">{label}<" in svg


def test_discover_file_groups_and_generate_all_graphs(tmp_path: Path) -> None:
    lang_root = tmp_path / "bench_data" / "python"
    file_dir = lang_root / "tests" / "test_module.py"
    # exist_ok: tolerates being invoked more than once in-process against the
    # same tmp_path by the RAL-94 bench harness itself.
    file_dir.mkdir(parents=True, exist_ok=True)

    from ralphus.bench.storage import append_record

    record = _record("c1", 0.01)
    append_record(file_dir / "test_a.json", "tests/test_module.py::test_a", record)
    append_record(file_dir / "test_b.json", "tests/test_module.py::test_b", record)

    groups = discover_file_groups(lang_root)
    assert file_dir in groups
    assert set(groups[file_dir].keys()) == {"test_a", "test_b"}

    written = generate_all_graphs(lang_root, "python")
    written_names = {p.name for p in written}
    assert "test_a.svg" in written_names
    assert "test_b.svg" in written_names
    assert FILE_MULTILINE_FILENAME in written_names
    assert FILE_SUMMARY_FILENAME in written_names
    assert GROUP_INDEX_FILENAME in written_names
    assert LANGUAGE_INDEX_FILENAME in written_names

    for path in written:
        assert path.exists()
        content = path.read_text(encoding="utf-8")
        if path.suffix == ".svg":
            assert content.startswith("<svg")
        else:
            assert content.startswith("<!doctype html>")

    group_index = (file_dir / GROUP_INDEX_FILENAME).read_text(encoding="utf-8")
    assert "test_a.svg" in group_index
    assert "test_b.svg" in group_index
    assert FILE_SUMMARY_FILENAME in group_index
    assert FILE_MULTILINE_FILENAME in group_index

    language_index = (lang_root / LANGUAGE_INDEX_FILENAME).read_text(encoding="utf-8")
    assert "tests/test_module.py" in language_index.replace("\\", "/")


def test_discover_file_groups_empty_root_returns_empty(tmp_path: Path) -> None:
    assert discover_file_groups(tmp_path / "does-not-exist") == {}


def test_generate_all_graphs_empty_lang_root_writes_nothing(tmp_path: Path) -> None:
    written = generate_all_graphs(tmp_path / "bench_data" / "python", "python")
    assert written == []


def test_render_root_index_html_links_present_languages() -> None:
    html = render_root_index_html(["python", "rust"])
    assert html.startswith("<!doctype html>")
    assert 'href="python/index.html"' in html
    assert 'href="rust/index.html"' in html


def test_render_root_index_html_handles_no_data() -> None:
    html = render_root_index_html([])
    assert "No bench data" in html
