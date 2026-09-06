#!/usr/bin/env bash
# docs-screenshots.sh -- regenerate docs/site/pages/screenshots/*.png.
# Separate from docs-build.sh (the fast Markdown->HTML path) on purpose: this
# launches headless Chromium via Playwright and is the only expensive part of
# the docs build. Only run it after a librarian/assets/board.html UI change;
# review and commit the resulting PNG diffs.
set -euo pipefail

_script_dir="$(cd "$(dirname "$0")" && pwd)"
root="$(dirname "$(git -C "$_script_dir" rev-parse --path-format=absolute --git-common-dir)")"
unset _script_dir

echo "== building ralphus-librarian (debug) =="
( cd "$root" && cargo build --package ralphus-librarian )

echo "== syncing docs venv (uv) =="
( cd "$root/cli-py" && uv sync --extra docs >/dev/null )

echo "== installing headless Chromium (Playwright) =="
( cd "$root/cli-py" && uv run playwright install chromium )

echo "== generating screenshots =="
( cd "$root/cli-py" && uv run ralphus-docs-shots )

echo "== done -> $root/docs/site/pages/screenshots/ =="
