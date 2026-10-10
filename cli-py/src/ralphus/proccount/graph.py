"""Hand-rolled SVG/HTML line graph: one line per test, x = label, y = process count."""

from __future__ import annotations

import html
from pathlib import Path

from ralphus.proccount.manifest import TaggedTest
from ralphus.proccount.storage import CountRecord

__all__ = ["render_html", "render_svg", "write_graphs"]

_PALETTE = ["#2a6ebb", "#c8501e", "#1f8a5b", "#8a4fbf", "#b08a00", "#c2185b", "#00838f", "#5d6b2f"]
_DASHES = ["", "6 3", "2 3", "8 3 2 3"]
_W, _H = 960, 420
_LEFT, _RIGHT, _TOP, _BOTTOM = 56, 24, 24, 56


def _series(records: list[CountRecord], tags: dict[str, str]) -> dict[str, list[tuple[int, int]]]:
    """Test id -> [(x index, count)], for tests with at least one measurement."""
    ids = sorted(
        {test_id for r in records for test_id in r.counts},
        key=lambda t: (tags.get(t, ""), t),
    )
    return {
        test_id: [(i, r.counts[test_id]) for i, r in enumerate(records) if test_id in r.counts]
        for test_id in ids
    }


def render_svg(records: list[CountRecord], tests: list[TaggedTest]) -> str:
    tags = {t.test_id: t.tag for t in tests}
    series = _series(records, tags)
    tag_names = sorted({tags.get(t, "untagged") for t in series})
    max_y = max(max((c for pts in series.values() for _, c in pts), default=1), 1)
    plot_w, plot_h = _W - _LEFT - _RIGHT, _H - _TOP - _BOTTOM

    def x_of(i: int) -> float:
        return _LEFT + (plot_w / 2 if len(records) <= 1 else plot_w * i / (len(records) - 1))

    def y_of(v: int) -> float:
        return _TOP + plot_h - plot_h * v / max_y

    out = [
        f'<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 {_W} {_H}" width="{_W}" '
        f'height="{_H}" font-family="sans-serif" font-size="11">',
        f'<rect width="{_W}" height="{_H}" fill="#fcfcfb"/>',
    ]
    for v in range(0, max_y + 1, max(1, max_y // 5)):
        out.append(
            f'<line x1="{_LEFT}" x2="{_W - _RIGHT}" y1="{y_of(v):.1f}" y2="{y_of(v):.1f}" '
            'stroke="#e3e2de"/>'
            f'<text x="{_LEFT - 6}" y="{y_of(v) + 4:.1f}" text-anchor="end" '
            f'fill="#52514e">{v}</text>'
        )
    for i, record in enumerate(records):
        out.append(
            f'<text x="{x_of(i):.1f}" y="{_H - _BOTTOM + 18}" text-anchor="middle" '
            f'fill="#52514e">{html.escape(record.label)}</text>'
        )
    mid = _TOP + plot_h / 2
    out.append(
        f'<text x="14" y="{mid:.0f}" fill="#52514e" text-anchor="middle" '
        f'transform="rotate(-90 14 {mid:.0f})">processes per test</text>'
    )
    within_tag: dict[str, int] = {}
    for test_id, points in series.items():
        tag = tags.get(test_id, "untagged")
        color = _PALETTE[tag_names.index(tag) % len(_PALETTE)]
        nth = within_tag.get(tag, 0)
        within_tag[tag] = nth + 1
        dash = _DASHES[nth % len(_DASHES)]
        dash_attr = f' stroke-dasharray="{dash}"' if dash else ""
        coords = " ".join(f"{x_of(i):.1f},{y_of(c):.1f}" for i, c in points)
        title = f"<title>{html.escape(tag)}: {html.escape(test_id)}</title>"
        out.append(
            f'<g><polyline points="{coords}" fill="none" stroke="{color}" '
            f'stroke-width="2"{dash_attr}/>{title}'
        )
        out.extend(
            f'<circle cx="{x_of(i):.1f}" cy="{y_of(c):.1f}" r="3" fill="{color}"/>'
            for i, c in points
        )
        out.append("</g>")
    if not series:
        out.append(
            f'<text x="{_W / 2}" y="{_H / 2}" text-anchor="middle" '
            'fill="#52514e">no data yet</text>'
        )
    out.append("</svg>")
    return "\n".join(out) + "\n"


def render_html(records: list[CountRecord], tests: list[TaggedTest]) -> str:
    tags = {t.test_id: t.tag for t in tests}
    rows = "\n".join(
        f"<tr><td>{html.escape(tags.get(test_id, 'untagged'))}</td>"
        f"<td><code>{html.escape(test_id)}</code></td>"
        + "".join(f"<td>{dict(points).get(i, '')}</td>" for i in range(len(records)))
        + "</tr>"
        for test_id, points in _series(records, tags).items()
    )
    heads = "".join(f"<th>{html.escape(r.label)}</th>" for r in records)
    return (
        "<!doctype html><meta charset=utf-8><title>Process counts per test</title>"
        '<body style="font-family:sans-serif;background:#fcfcfb;color:#0b0b0b">'
        "<h1>Process counts per test</h1>"
        "<p>Lowest of three runs per test; x = git tag. See <code>docs/proc-counts.md</code>.</p>"
        f"{render_svg(records, tests)}"
        f'<table border="1" cellpadding="4"><tr><th>tag</th><th>test</th>{heads}</tr>{rows}</table>'
    )


def write_graphs(directory: Path, records: list[CountRecord], tests: list[TaggedTest]) -> None:
    directory.mkdir(parents=True, exist_ok=True)
    (directory / "graph.svg").write_text(render_svg(records, tests), encoding="utf-8")
    (directory / "index.html").write_text(render_html(records, tests), encoding="utf-8")
