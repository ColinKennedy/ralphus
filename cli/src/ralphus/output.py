"""Output helpers shared by every CLI subcommand: the ``--json`` contract,
dependency-free table rendering, and the exit-code convention.

Exit-code convention (also documented in ``ralphus --help``):
    0  ok
    1  domain error    (daemon reached, request rejected)
    2  usage/local error (bad args, file unreadable)
    3  not found       (HTTP 404)
    4  conflict        (HTTP 409, e.g. "already merging")
"""

from __future__ import annotations

import json
from collections.abc import Callable, Iterable, Sequence
from typing import Any

from ralphus.client import DaemonError

__all__ = ["emit", "exit_code_for", "print_kv", "print_table"]


def emit(json_mode: bool, data: Any, human: Callable[[Any], None]) -> None:
    """Print ``data`` as raw JSON if ``json_mode``, else call ``human(data)``.

    Both branches consume the exact same ``data`` value, so a handler builds
    the response object once and only branches on how to render it.
    """
    if json_mode:
        print(json.dumps(data, indent=2, default=str))
    else:
        human(data)


def print_table(headers: Sequence[str], rows: Sequence[Sequence[str]]) -> None:
    """Print a left-aligned, space-padded table (no `rich`/external dependency)."""
    if not rows:
        print("  ".join(headers))
        return
    widths = [max(len(headers[i]), *(len(row[i]) for row in rows)) for i in range(len(headers))]

    def _fmt(row: Sequence[str]) -> str:
        return "  ".join(cell.ljust(width) for cell, width in zip(row, widths, strict=True))

    print(_fmt(headers))
    for row in rows:
        print(_fmt(row))


def print_kv(pairs: Iterable[tuple[str, object]]) -> None:
    """Print ``key : value`` lines, aligning the colons."""
    pairs = list(pairs)
    if not pairs:
        return
    width = max(len(k) for k, _ in pairs)
    for key, value in pairs:
        print(f"{key.ljust(width)} : {value}")


def exit_code_for(exc: DaemonError) -> int:
    """Map a `DaemonError` to the CLI's process exit code (see module docstring)."""
    if exc.status_code == 404:
        return 3
    if exc.status_code == 409:
        return 4
    return 1
