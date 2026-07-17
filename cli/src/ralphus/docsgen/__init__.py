"""Deterministic fixture data + screenshot generation for docs/site/.

Nothing here talks to a real daemon. ``fixtures`` hand-authors JSON payloads
shaped like the daemon's real API responses; ``stub_server`` serves them
locally; ``shots`` drives a headless browser against that stub to produce the
PNGs embedded in the documentation pages.
"""

from __future__ import annotations

__all__: list[str] = []
