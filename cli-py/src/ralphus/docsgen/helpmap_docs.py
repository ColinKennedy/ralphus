"""Regenerate the machine-readable help-map block in docs/cli-reference.md (RAL-110).

The CLI is now Rust (`cli/`, binary `ralphus`) rather than the Python
`ralphus.__main__`/`ralphus.helpmap` this script originally called into
in-process. Rather than re-deriving the tree-walk logic a second time here,
`generate()` shells out to the real, compiled `ralphus show help-map`
binary and takes its tree verbatim -- the binary is the single source of
truth for its own command surface, so this doc can never silently drift
from what the actual CLI prints, exactly as before, just via a subprocess
boundary instead of a Python import.

Runnable standalone (regenerates the file in place) or with ``--check``
(drift detection for CI: exits 1 without writing if the file would change).
"""

from __future__ import annotations

import argparse
import subprocess
import sys

from ralphus.docsgen.binaries import REPO_ROOT, find_ralphus_binary

__all__ = [
    "BEGIN_MARKER",
    "CLI_REFERENCE",
    "END_MARKER",
    "REPO_ROOT",
    "find_ralphus_binary",
    "generate",
    "main",
    "render_block",
]

CLI_REFERENCE = REPO_ROOT / "docs" / "cli-reference.md"

BEGIN_MARKER = "<!-- BEGIN GENERATED HELP-MAP (RAL-110) -->"
END_MARKER = "<!-- END GENERATED HELP-MAP (RAL-110) -->"

# The line the tree itself always starts with (`ralphus show help-map`
# prints six guidance-note paragraphs first, then the tree) -- used to trim
# the notes off, since this doc's fenced block only ever embedded the bare
# tree (`ralphus.helpmap.generate()`'s old return value), not the notes.
_TREE_START = "- ralphus "


def generate() -> str:
    """The full alphabetized, indented help-map tree, as one string.

    Runs the real `ralphus show help-map` and strips its six leading
    guidance-note paragraphs, keeping only the tree -- matching the old
    in-process `ralphus.helpmap.generate()`'s return value (tree only, no
    trailing newline), since that's what this module's fenced doc block has
    always embedded.
    """
    binary = find_ralphus_binary()
    result = subprocess.run(
        [str(binary), "show", "help-map"],
        capture_output=True,
        text=True,
        check=True,
    )
    output = result.stdout
    start = output.index(_TREE_START)
    return output[start:].rstrip("\n")


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
