"""Tests for the ASCII/DOT dependency-graph renderers."""

from __future__ import annotations

from ralphus.graphview import render_ascii, render_dot

_NODES = [
    {"id": "t0s0", "label": "build/compile"},
    {"id": "t1s0", "label": "test/session-0"},
]
_EDGES = [{"from": "t0s0", "to": "t1s0"}]


def test_render_ascii_places_dependent_in_a_later_level() -> None:
    out = render_ascii(_NODES, _EDGES)
    lines = out.splitlines()
    level0 = lines.index("level 0:")
    level1 = lines.index("level 1:")
    assert level0 < level1
    assert "t0s0" in out
    assert "t1s0" in out
    # The dependent's line shows its prerequisite.
    dependent_line = next(line_ for line_ in lines if "t1s0" in line_ and "  (" in line_)
    assert "<- t0s0" in dependent_line


def test_render_ascii_independent_nodes_share_level_zero() -> None:
    nodes = [{"id": "a"}, {"id": "b"}]
    out = render_ascii(nodes, [])
    lines = out.splitlines()
    assert lines[0] == "level 0:"
    assert len(lines) == 3  # header + 2 node lines, one level only


def test_render_ascii_node_without_label_omits_parens() -> None:
    out = render_ascii([{"id": "a"}], [])
    assert "(a)" not in out
    assert "  a" in out


def test_render_ascii_cycle_does_not_infinite_loop() -> None:
    # The daemon rejects cycles at submit time; the renderer must still
    # terminate defensively (dumping the residual as one final level) rather
    # than looping forever if one ever reached it.
    nodes = [{"id": "a"}, {"id": "b"}]
    edges = [{"from": "a", "to": "b"}, {"from": "b", "to": "a"}]
    out = render_ascii(nodes, edges)
    assert "a" in out
    assert "b" in out


def test_render_dot_wraps_nodes_and_edges() -> None:
    out = render_dot(_NODES, _EDGES)
    assert out.startswith("digraph {")
    assert out.rstrip().endswith("}")
    assert '"t0s0" [label="build/compile"];' in out
    assert '"t1s0" [label="test/session-0"];' in out
    assert '"t0s0" -> "t1s0";' in out


def test_render_dot_escapes_quotes_in_labels() -> None:
    out = render_dot([{"id": "a", "label": 'say "hi"'}], [])
    assert '\\"hi\\"' in out
