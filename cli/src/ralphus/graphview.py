"""ASCII and Graphviz DOT rendering for `ralphus graph` (CLI_PARITY_PLAN.local.md
Phase 6). The daemon returns only `{nodes, edges}` -- all rendering happens
here, CLI-side, so this is the only place that knows about layout or DOT
syntax.
"""

from __future__ import annotations

from typing import Any

__all__ = ["render_ascii", "render_dot"]


def _levels(node_ids: list[str], edges: list[tuple[str, str]]) -> list[list[str]]:
    """Group nodes into topological levels: level 0 has no prerequisites among
    `node_ids`, level 1's prerequisites are all in level 0, etc. Deterministic
    (nodes within a level are sorted). A residual cycle (shouldn't happen --
    the daemon rejects cycles at submit time) dumps whatever's left as one
    final level rather than looping forever.
    """
    incoming: dict[str, set[str]] = {n: set() for n in node_ids}
    for frm, to in edges:
        if to in incoming:
            incoming[to].add(frm)
    remaining = dict(incoming)
    placed: set[str] = set()
    levels: list[list[str]] = []
    while remaining:
        ready = sorted(n for n, deps in remaining.items() if deps <= placed)
        if not ready:
            ready = sorted(remaining.keys())
        levels.append(ready)
        placed.update(ready)
        for n in ready:
            remaining.pop(n, None)
    return levels


def render_ascii(nodes: list[dict[str, Any]], edges: list[dict[str, Any]]) -> str:
    """Render a layered, indented ASCII view. `nodes` must each have `id`; a
    `label` key (if present) is shown alongside the id.
    """
    labels = {n["id"]: n.get("label", n["id"]) for n in nodes}
    node_ids = list(labels)
    edge_pairs = [(e["from"], e["to"]) for e in edges]
    incoming: dict[str, list[str]] = {n: [] for n in node_ids}
    for frm, to in edge_pairs:
        if to in incoming:
            incoming[to].append(frm)

    lines: list[str] = []
    for i, level in enumerate(_levels(node_ids, edge_pairs)):
        lines.append(f"level {i}:")
        for n in level:
            label = labels[n]
            head = f"  {n}" if label == n else f"  {n}  ({label})"
            deps = incoming.get(n) or []
            tail = f"  <- {', '.join(deps)}" if deps else ""
            lines.append(f"{head}{tail}")
    return "\n".join(lines)


def render_dot(nodes: list[dict[str, Any]], edges: list[dict[str, Any]]) -> str:
    """Render `digraph { ... }` Graphviz source."""
    labels = {n["id"]: n.get("label", n["id"]) for n in nodes}
    lines = ["digraph {"]
    for node_id, label in labels.items():
        escaped = str(label).replace("\\", "\\\\").replace('"', '\\"')
        lines.append(f'  "{node_id}" [label="{escaped}"];')
    for e in edges:
        lines.append(f'  "{e["from"]}" -> "{e["to"]}";')
    lines.append("}")
    return "\n".join(lines)
