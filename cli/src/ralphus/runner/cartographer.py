"""Emit structured events to the daemon's Cartographer log (RAL-98).

The runner subprocess cannot write to stdout — that channel is reserved for
the ``SessionSpec``/``SessionResult`` JSON contract (see the Logging Policy in
``AGENTS.md``) — so structured events are emitted as a single JSON line on
stderr, prefixed with a marker. The daemon's ``runner.rs`` reads the child's
stderr stream line-by-line, detects the marker, and forwards the parsed JSON
into Cartographer. This mirrors the existing ``RALPHUS_VERIFY: PASS/FAIL``
marker-parsing pattern rather than adding a new transport.

Call :func:`emit` alongside (not instead of) the existing human-readable
``print(..., file=sys.stderr)`` calls — Cartographer records are additional
structured detail, not a replacement for the tail-able text log.
"""

from __future__ import annotations

import json
import sys
from typing import Any

__all__ = ["emit"]

_MARKER = "RALPHUS_EVENT: "


def emit(
    source: str,
    message: str,
    *,
    level: str = "info",
    scope: str | None = None,
    run_id: str | None = None,
    session_id: str | None = None,
    task: str | None = None,
    payload: dict[str, Any] | None = None,
) -> None:
    """Write one structured Cartographer event line to stderr.

    ``run_id``/``session_id``/``task`` are optional — when omitted, the
    daemon falls back to the owning session's spec fields.
    """
    event = {
        "source": source,
        "message": message,
        "level": level,
        "scope": scope,
        "run_id": run_id,
        "session_id": session_id,
        "task": task,
        "payload": payload or {},
    }
    print(_MARKER + json.dumps(event), file=sys.stderr)
