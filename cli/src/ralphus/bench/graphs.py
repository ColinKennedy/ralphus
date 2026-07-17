"""Hand-rolled SVG graph generation for RAL-94 benchmark data.

Three graph types, per the acceptance criteria:

* **per-test** — one line, one test, x = commit (vertical labels), y = durable_min.
* **per-file multi-line** — every test in a file/module on one plot, each line
  labeled with a single letter directly on the plot; a legend below maps letters
  to full test names; line color is deterministic (hashed from
  ``relative_path + test_name``), since a file can hold an unbounded number of
  tests and a fixed categorical palette does not scale to that cardinality. The
  letter (not color) is the primary identity channel for this reason.
* **per-file summary** — mean/median/max/min of `durable_min` aggregated across
  a file's tests per commit, computed purely from already-stored stats bundles.
  This is a fixed four-series chart, so it uses the standard fixed categorical
  slots rather than hashing.

No charting library / no new dependency — everything here is plain string-built
SVG, run through the stdlib only.

Output surface (RAL-94 Q4): a developer opens `bench_data/index.html` directly
in a browser — not a raw `.svg` file, not part of `board.html` or the public
docs site. That page links to a per-language landing page
(`bench_data/<language>/index.html`), which links to a per-file/module index
page (`bench_data/<language>/.../index.html`) that embeds that group's SVGs
via `<img>`.
"""

from __future__ import annotations

import argparse
import hashlib
import statistics
import sys
from collections.abc import Callable, Sequence
from dataclasses import dataclass
from pathlib import Path

from ralphus.bench.storage import BenchRecord, Language, TestRecordFile, load_records

__all__ = [
    "FILE_MULTILINE_FILENAME",
    "FILE_SUMMARY_FILENAME",
    "GROUP_INDEX_FILENAME",
    "LANGUAGE_INDEX_FILENAME",
    "ROOT_INDEX_FILENAME",
    "discover_file_groups",
    "generate_all_graphs",
    "main",
    "render_file_multiline_svg",
    "render_file_summary_svg",
    "render_group_index_html",
    "render_language_index_html",
    "render_root_index_html",
    "render_test_svg",
]

# -- chart chrome, from the dataviz skill's reference palette (light surface) --
_SURFACE = "#fcfcfb"
_INK_PRIMARY = "#0b0b0b"
_INK_SECONDARY = "#52514e"
_INK_MUTED = "#898781"
_GRIDLINE = "#e1e0d9"
_BASELINE = "#c3c2b7"
_SERIES_1_BLUE = "#2a78d6"  # single-series default (per-test graph)

# Fixed categorical slots for the always-exactly-4-series summary graph.
_SUMMARY_COLORS = {
    "min": "#2a78d6",  # blue
    "median": "#1baf7a",  # aqua
    "mean": "#4a3aa7",  # violet
    "max": "#e34948",  # red
}

_WIDTH = 900
_HEIGHT = 460
_MARGIN_TOP = 30
_MARGIN_RIGHT = 40
_MARGIN_BOTTOM = 160
_MARGIN_LEFT = 80

FILE_MULTILINE_FILENAME = "__file_multiline__.svg"
FILE_SUMMARY_FILENAME = "__file_summary__.svg"

# RAL-94 Q4: results are opened as a standalone HTML page (SVGs referenced via
# <img>), not by opening raw .svg files directly — these are that HTML surface,
# one per file/module group, one per language, and one repo-wide entry point.
GROUP_INDEX_FILENAME = "index.html"
LANGUAGE_INDEX_FILENAME = "index.html"
ROOT_INDEX_FILENAME = "index.html"


def _escape(text: str) -> str:
    return (
        text.replace("&", "&amp;").replace("<", "&lt;").replace(">", "&gt;").replace('"', "&quot;")
    )


def _hash_color(key: str) -> str:
    """Deterministic hex color for `key`: hash -> hue, fixed sat/lightness.

    Fixed saturation/lightness (rather than hashing straight into RGB) keeps
    every generated color legible against the light chart surface and
    distinguishable from its neighbors, while still being fully deterministic.
    """
    digest = hashlib.sha256(key.encode("utf-8")).digest()
    hue = int.from_bytes(digest[:2], "big") % 360
    return _hsl_to_hex(hue, 0.60, 0.42)


def _hsl_to_hex(h: int, s: float, l: float) -> str:  # noqa: E741
    c = (1 - abs(2 * l - 1)) * s
    x = c * (1 - abs((h / 60) % 2 - 1))
    m = l - c / 2
    if h < 60:
        r, g, b = c, x, 0.0
    elif h < 120:
        r, g, b = x, c, 0.0
    elif h < 180:
        r, g, b = 0.0, c, x
    elif h < 240:
        r, g, b = 0.0, x, c
    elif h < 300:
        r, g, b = x, 0.0, c
    else:
        r, g, b = c, 0.0, x

    def to255(v: float) -> int:
        return round((v + m) * 255)

    return f"#{to255(r):02x}{to255(g):02x}{to255(b):02x}"


def _short_commit(record: BenchRecord) -> str:
    short = record.commit[:7]
    return f"{short}*" if record.dirty else short


def _fmt_duration(seconds: float) -> str:
    if seconds < 1e-3:
        return f"{seconds * 1e6:.0f}µs"
    if seconds < 1:
        return f"{seconds * 1e3:.1f}ms"
    return f"{seconds:.3f}s"


def _letters() -> list[str]:
    """A, B, ... Z, AA, AB, ... — enough letters for any realistic file size."""
    out = []
    alphabet = "ABCDEFGHIJKLMNOPQRSTUVWXYZ"
    for c in alphabet:
        out.append(c)
    for c1 in alphabet:
        for c2 in alphabet:
            out.append(c1 + c2)
    return out


@dataclass
class _Series:
    label: str
    color: str
    points: list[tuple[str, float]]  # (x-axis label, y value), in commit order


def _plot_area() -> tuple[int, int, int, int]:
    left = _MARGIN_LEFT
    top = _MARGIN_TOP
    right = _WIDTH - _MARGIN_RIGHT
    bottom = _HEIGHT - _MARGIN_BOTTOM
    return left, top, right, bottom


def _render_svg(
    title: str,
    x_labels: Sequence[str],
    series: Sequence[_Series],
    *,
    legend: list[tuple[str, str]] | None = None,
) -> str:
    """Shared line-chart renderer: axes, gridlines, N series, optional legend."""
    left, top, right, bottom = _plot_area()
    all_values = [v for s in series for _, v in s.points]
    y_max = max(all_values) * 1.1 if all_values else 1.0
    y_max = y_max or 1.0

    def x_of(index: int) -> float:
        if len(x_labels) <= 1:
            return (left + right) / 2
        return left + (right - left) * index / (len(x_labels) - 1)

    def y_of(value: float) -> float:
        return bottom - (value / y_max) * (bottom - top)

    parts = [
        f'<svg xmlns="http://www.w3.org/2000/svg" width="{_WIDTH}" height="{_HEIGHT}" '
        f'viewBox="0 0 {_WIDTH} {_HEIGHT}" font-family="system-ui, -apple-system, '
        f"'Segoe UI', sans-serif\">",
        f'<rect x="0" y="0" width="{_WIDTH}" height="{_HEIGHT}" fill="{_SURFACE}"/>',
        f'<text x="{left}" y="18" fill="{_INK_PRIMARY}" font-size="14" '
        f'font-weight="600">{_escape(title)}</text>',
    ]

    # y-axis gridlines + labels (5 ticks)
    ticks = 5
    for i in range(ticks + 1):
        value = y_max * i / ticks
        y = y_of(value)
        parts.append(
            f'<line x1="{left}" y1="{y:.1f}" x2="{right}" y2="{y:.1f}" '
            f'stroke="{_GRIDLINE}" stroke-width="1"/>'
        )
        parts.append(
            f'<text x="{left - 8}" y="{y + 4:.1f}" fill="{_INK_MUTED}" '
            f'font-size="10" text-anchor="end">{_fmt_duration(value)}</text>'
        )

    # axes baseline
    parts.append(
        f'<line x1="{left}" y1="{bottom}" x2="{right}" y2="{bottom}" '
        f'stroke="{_BASELINE}" stroke-width="1"/>'
    )
    parts.append(
        f'<line x1="{left}" y1="{top}" x2="{left}" y2="{bottom}" '
        f'stroke="{_BASELINE}" stroke-width="1"/>'
    )

    # x-axis labels, rotated vertical for readability
    for i, label in enumerate(x_labels):
        x = x_of(i)
        parts.append(
            f'<text x="{x:.1f}" y="{bottom + 8}" fill="{_INK_MUTED}" font-size="9" '
            f'text-anchor="end" transform="rotate(-60 {x:.1f} {bottom + 8})">'
            f"{_escape(label)}</text>"
        )

    # series lines + points
    x_index_by_label = {label: i for i, label in enumerate(x_labels)}
    for s in series:
        path_points = []
        for label, value in s.points:
            i = x_index_by_label[label]
            path_points.append((x_of(i), y_of(value)))
        if not path_points:
            continue
        path_d = "M " + " L ".join(f"{x:.1f} {y:.1f}" for x, y in path_points)
        parts.append(f'<path d="{path_d}" fill="none" stroke="{s.color}" stroke-width="2"/>')
        for x, y in path_points:
            parts.append(f'<circle cx="{x:.1f}" cy="{y:.1f}" r="3" fill="{s.color}"/>')
        # letter/label directly on the plot, at the last point
        last_x, last_y = path_points[-1]
        parts.append(
            f'<text x="{last_x + 6:.1f}" y="{last_y + 3:.1f}" fill="{s.color}" '
            f'font-size="11" font-weight="700">{_escape(s.label)}</text>'
        )

    if legend:
        legend_y = _HEIGHT - 20 * (len(legend) // 2 + 1) - 8
        for i, (label, color) in enumerate(legend):
            col = i % 2
            row = i // 2
            lx = left + col * 420
            ly = legend_y + row * 18
            parts.append(f'<rect x="{lx}" y="{ly - 9}" width="10" height="10" fill="{color}"/>')
            parts.append(
                f'<text x="{lx + 16}" y="{ly}" fill="{_INK_SECONDARY}" font-size="11">'
                f"{_escape(label)}</text>"
            )

    parts.append("</svg>")
    return "\n".join(parts)


def _skip_suffix(skipped_count: int) -> str:
    """Textual skip indicator for a title/legend — never a plotted point (RAL-94 Q5)."""
    return f" ({skipped_count} skipped)" if skipped_count else ""


def render_test_svg(test: TestRecordFile) -> str:
    """Per-test line graph: x = commit, y = durable_min. Single series, no legend.

    Skipped commits (`test.skipped`) are surfaced only as a count in the
    title — they never contribute an x-axis label or a plotted point.
    """
    x_labels = [_short_commit(r) for r in test.records]
    series = [
        _Series(
            label="",
            color=_SERIES_1_BLUE,
            points=[(_short_commit(r), r.durable_min) for r in test.records],
        )
    ]
    title = f"{test.test_id}{_skip_suffix(len(test.skipped))}"
    return _render_svg(title, x_labels, series)


def render_file_multiline_svg(file_label: str, tests: dict[str, TestRecordFile]) -> str:
    """Per-file multi-line graph: one line per test, letters on-plot, legend below."""
    # Union of commits, ordered by first appearance across the file's tests.
    x_labels: list[str] = []
    seen: set[str] = set()
    for test in tests.values():
        for r in test.records:
            key = _short_commit(r)
            if key not in seen:
                seen.add(key)
                x_labels.append(key)

    letters = _letters()
    series = []
    legend = []
    for i, (test_id, test) in enumerate(sorted(tests.items())):
        letter = letters[i] if i < len(letters) else f"#{i}"
        color = _hash_color(f"{file_label}::{test_id}")
        series.append(
            _Series(
                label=letter,
                color=color,
                points=[(_short_commit(r), r.durable_min) for r in test.records],
            )
        )
        legend.append((f"{letter} = {test_id}{_skip_suffix(len(test.skipped))}", color))

    return _render_svg(f"{file_label} (all tests)", x_labels, series, legend=legend)


def render_file_summary_svg(file_label: str, tests: dict[str, TestRecordFile]) -> str:
    """Per-file summary graph: mean/median/max/min of durable_min across tests, per commit."""
    by_commit: dict[str, list[float]] = {}
    order: list[str] = []
    for test in tests.values():
        for r in test.records:
            key = _short_commit(r)
            if key not in by_commit:
                by_commit[key] = []
                order.append(key)
            by_commit[key].append(r.durable_min)

    x_labels = order
    aggregates: dict[str, Callable[[list[float]], float]] = {
        "min": min,
        "median": statistics.median,
        "mean": statistics.mean,
        "max": max,
    }
    series = []
    for name, fn in aggregates.items():
        points = [(label, fn(by_commit[label])) for label in order]
        series.append(_Series(label=name, color=_SUMMARY_COLORS[name], points=points))
    legend = [(name, _SUMMARY_COLORS[name]) for name in aggregates]

    return _render_svg(f"{file_label} (summary)", x_labels, series, legend=legend)


def _html_page(title: str, body: str) -> str:
    """Shared HTML chrome for every generated index page (RAL-94 Q4)."""
    return f"""<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<title>{_escape(title)}</title>
<style>
  body {{
    font-family: system-ui, -apple-system, 'Segoe UI', sans-serif;
    background: {_SURFACE}; color: {_INK_PRIMARY};
    margin: 0; padding: 24px 32px 64px;
  }}
  h1 {{ font-size: 20px; }}
  h2 {{ font-size: 15px; color: {_INK_SECONDARY}; margin-top: 40px; }}
  h3 {{ font-size: 13px; color: {_INK_SECONDARY}; }}
  a {{ color: {_SERIES_1_BLUE}; }}
  ul {{ line-height: 1.8; padding-left: 20px; }}
  img {{ max-width: 100%; border: 1px solid {_GRIDLINE}; background: {_SURFACE}; }}
  .test-block {{ margin-top: 24px; }}
</style>
</head>
<body>
{body}
</body>
</html>
"""


def render_group_index_html(file_label: str, tests: dict[str, TestRecordFile]) -> str:
    """Standalone HTML page for one file/module group: summary + multiline +
    every per-test graph, embedded via `<img>` referencing the sibling SVGs
    `generate_all_graphs` writes alongside this page (RAL-94 Q4)."""
    test_blocks = "\n".join(
        f'<div class="test-block"><h3>{_escape(name)}{_skip_suffix(len(test.skipped))}</h3>'
        f'<img src="{_escape(name)}.svg" alt="{_escape(name)} durable_min trend"></div>'
        for name, test in sorted(tests.items())
    )
    body = f"""<h1>{_escape(file_label)}</h1>
<h2>Summary (min / median / mean / max of durable_min)</h2>
<img src="{FILE_SUMMARY_FILENAME}" alt="file summary graph">
<h2>All tests, combined</h2>
<img src="{FILE_MULTILINE_FILENAME}" alt="file multiline graph">
<h2>Per-test graphs</h2>
{test_blocks}
"""
    return _html_page(file_label, body)


def render_language_index_html(
    language: Language, lang_root: Path, groups: dict[Path, dict[str, TestRecordFile]]
) -> str:
    """Standalone HTML landing page for one language, linking to every file/module
    group's own index page (RAL-94 Q4)."""
    if not groups:
        body = f"<h1>{_escape(language)} benchmarks</h1><p>No bench data recorded yet.</p>"
        return _html_page(f"{language} benchmarks", body)

    items = "\n".join(
        f'<li><a href="{_escape(directory.relative_to(lang_root).as_posix())}/'
        f'{GROUP_INDEX_FILENAME}">{_escape(directory.relative_to(lang_root).as_posix())}</a>'
        f" — {len(tests)} test(s)</li>"
        for directory, tests in sorted(groups.items(), key=lambda kv: kv[0].as_posix())
    )
    body = f"<h1>{_escape(language)} benchmarks</h1>\n<ul>\n{items}\n</ul>"
    return _html_page(f"{language} benchmarks", body)


def render_root_index_html(languages_present: Sequence[Language]) -> str:
    """Repo-wide standalone HTML entry point a developer opens directly to browse
    all RAL-94 benchmark results (RAL-94 Q4)."""
    if not languages_present:
        body = "<h1>RAL-94 benchmark results</h1><p>No bench data recorded yet.</p>"
        return _html_page("RAL-94 benchmark results", body)

    items = "\n".join(
        f'<li><a href="{_escape(lang)}/{LANGUAGE_INDEX_FILENAME}">{_escape(lang)}</a></li>'
        for lang in languages_present
    )
    body = f"<h1>RAL-94 benchmark results</h1>\n<ul>\n{items}\n</ul>"
    return _html_page("RAL-94 benchmark results", body)


def discover_file_groups(lang_root: Path) -> dict[Path, dict[str, TestRecordFile]]:
    """Group per-test JSON files under `lang_root` by their containing directory.

    Each leaf directory under bench_data/<language>/ mirrors one source file, so
    the directory is the "file/module" grouping the per-file graphs need.
    """
    groups: dict[Path, dict[str, TestRecordFile]] = {}
    if not lang_root.exists():
        return groups
    for json_path in lang_root.rglob("*.json"):
        record_file = load_records(json_path)
        if record_file is None:
            continue
        groups.setdefault(json_path.parent, {})[json_path.stem] = record_file
    return groups


def generate_all_graphs(lang_root: Path, language: Language) -> list[Path]:
    """Generate every per-test, per-file-multiline, per-file-summary SVG, and
    each file/module group's standalone HTML index page (RAL-94 Q4).

    Writes graphs alongside their source data, under `lang_root`. Returns the
    list of paths written.
    """
    written: list[Path] = []
    groups = discover_file_groups(lang_root)
    for directory, tests in groups.items():
        file_label = f"{language}: {directory.relative_to(lang_root)}"

        for test_name, test in tests.items():
            svg = render_test_svg(test)
            out = directory / f"{test_name}.svg"
            out.write_text(svg, encoding="utf-8")
            written.append(out)

        multiline_out = directory / FILE_MULTILINE_FILENAME
        multiline_out.write_text(render_file_multiline_svg(file_label, tests), encoding="utf-8")
        written.append(multiline_out)

        summary_out = directory / FILE_SUMMARY_FILENAME
        summary_out.write_text(render_file_summary_svg(file_label, tests), encoding="utf-8")
        written.append(summary_out)

        index_out = directory / GROUP_INDEX_FILENAME
        index_out.write_text(render_group_index_html(file_label, tests), encoding="utf-8")
        written.append(index_out)

    language_index_out = lang_root / LANGUAGE_INDEX_FILENAME
    if groups:
        lang_root.mkdir(parents=True, exist_ok=True)
        language_index_out.write_text(
            render_language_index_html(language, lang_root, groups), encoding="utf-8"
        )
        written.append(language_index_out)

    return written


def main(argv: list[str] | None = None) -> None:
    parser = argparse.ArgumentParser(
        prog="ralphus-bench-graph",
        description="Generate RAL-94 benchmark SVG graphs from stored bench_data/.",
    )
    parser.add_argument(
        "--lang",
        choices=["python", "rust", "all"],
        default="all",
        help="Which ecosystem's data to render graphs for.",
    )
    parser.add_argument(
        "--root",
        type=Path,
        default=None,
        help="Repo root containing bench_data/ (default: discovered from cwd).",
    )
    args = parser.parse_args(argv)

    root = args.root
    if root is None:
        from ralphus.bench.storage import find_repo_root

        root = find_repo_root(Path.cwd())

    languages: list[Language] = ["python", "rust"] if args.lang == "all" else [args.lang]
    total = 0
    languages_present: list[Language] = []
    for language in languages:
        lang_root = root / "bench_data" / language
        written = generate_all_graphs(lang_root, language)
        total += len(written)
        if written:
            languages_present.append(language)
        print(
            f"ralphus [bench] graphs written lang={language} count={len(written)}",
            file=sys.stderr,
        )

    data_root = root / "bench_data"
    data_root.mkdir(parents=True, exist_ok=True)
    root_index_out = data_root / ROOT_INDEX_FILENAME
    root_index_out.write_text(render_root_index_html(languages_present), encoding="utf-8")
    total += 1

    print(
        f"ralphus [bench] graphs done total={total} entry_point={root_index_out}",
        file=sys.stderr,
    )


if __name__ == "__main__":
    main()
