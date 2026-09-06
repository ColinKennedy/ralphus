#!/usr/bin/env bash
# docs-build.sh -- FAST docs build: Markdown -> HTML only, no screenshots.
# Renders docs/site/pages/*.md (MkDocs + Material) into docs/site/_site/
# using whatever PNGs are already committed under
# docs/site/pages/screenshots/. Never touches Playwright, so this is cheap
# to re-run on every doc edit.
#
# To regenerate the screenshots themselves (only needed after a board.html
# UI change), run docs-screenshots.sh instead -- a separate, explicit,
# heavier step.
set -euo pipefail

_script_dir="$(cd "$(dirname "$0")" && pwd)"
root="$(dirname "$(git -C "$_script_dir" rev-parse --path-format=absolute --git-common-dir)")"
unset _script_dir

echo "== syncing docs venv (uv) =="
( cd "$root/cli-py" && uv sync --extra docs >/dev/null )

echo "== screenshot coverage lint =="
( cd "$root/cli-py" && uv run ralphus-docs-lint )

echo "== mkdocs build =="
( cd "$root/cli-py" && uv run mkdocs build --config-file "$root/docs/site/mkdocs.yml" )

echo "== done -> $root/docs/site/_site/index.html =="
