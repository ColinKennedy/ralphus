"""Regenerate the machine-readable help-map block in docs/cli-reference.md (RAL-110).

`ralphus.helpmap.generate()` recursively renders the CLI's full command
surface by actually invoking `--verbose --help` at every level (see that
module). This script replaces the fenced block between `BEGIN_MARKER`/
`END_MARKER` in docs/cli-reference.md with a freshly generated tree, so the
doc can never silently drift from what `--help` really prints.

Runnable standalone (regenerates the file in place) or with ``--check``
(drift detection for CI: exits 1 without writing if the file would change).
"""

from __future__ import annotations

import argparse
import sys
from pathlib import Path

from ralphus.helpmap import generate

__all__ = ["BEGIN_MARKER", "CLI_REFERENCE", "END_MARKER", "REPO_ROOT", "main", "render_block"]

REPO_ROOT = Path(__file__).resolve().parents[4]
CLI_REFERENCE = REPO_ROOT / "docs" / "cli-reference.md"

BEGIN_MARKER = "<!-- BEGIN GENERATED HELP-MAP (RAL-110) -->"
END_MARKER = "<!-- END GENERATED HELP-MAP (RAL-110) -->"


def _log(message: str) -> None:
    print(f"ralphus [docsgen] {message}", file=sys.stderr)


def render_block() -> str:
    """The full marker-delimited block to splice into the doc."""
    return f"{BEGIN_MARKER}\n```\n{generate()}\n```\n{END_MARKER}"


def _splice(original: str, block: str) -> str:
    if BEGIN_MARKER not in original or END_MARKER not in original:
        raise RuntimeError(
            f"{CLI_REFERENCE} is missing the {BEGIN_MARKER!r}/{END_MARKER!r} markers"
        )
    before, rest = original.split(BEGIN_MARKER, 1)
    _, after = rest.split(END_MARKER, 1)
    return f"{before}{block}{after}"


def main(argv: list[str] | None = None) -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--check",
        action="store_true",
        help="Exit 1 if the file would change instead of writing it (CI drift check).",
    )
    args = parser.parse_args(argv)

    original = CLI_REFERENCE.read_text(encoding="utf-8")
    updated = _splice(original, render_block())

    if updated == original:
        _log(f"PASS -- {CLI_REFERENCE} help-map is up to date")
        return
    if args.check:
        _log(
            f"FAIL -- {CLI_REFERENCE} help-map is stale; "
            "regenerate with: uv run ralphus-docs-helpmap"
        )
        sys.exit(1)
    CLI_REFERENCE.write_text(updated, encoding="utf-8")
    _log(f"updated {CLI_REFERENCE}")


if __name__ == "__main__":
    main()
