#!/usr/bin/env bash
# Build the four ralphus executables into ./dist as copyable standalone binaries.
#
#   daemon    (Rust)   -> dist/ralphus-daemon[.exe]
#   librarian (Rust)   -> dist/ralphus-librarian[.exe]
#   CLI       (Python) -> dist/ralphus[.exe]          (one-file, via PyInstaller)
#   runner    (Python) -> dist/ralphus-runner[.exe]   (one-file, via PyInstaller)
#
# The Rust binaries link SQLite in (rusqlite `bundled`) so they need no system
# libraries. The Python CLI and runner are each bundled with their interpreter by
# PyInstaller. The runner drives native model agents (claude/anthropic/ollama)
# through pydantic-ai, so its exe bundles that dependency tree (anthropic + openai
# SDKs, tiktoken encodings); the daemon points RALPHUS_RUNNER_CMD at it.
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
dist="$root/dist"
mkdir -p "$dist"

echo "== building Rust executables (release) =="
cargo build --release -p ralphus-daemon -p ralphus-librarian --manifest-path "$root/Cargo.toml"
for bin in ralphus-daemon ralphus-librarian; do
  for ext in "" ".exe"; do
    src="$root/target/release/${bin}${ext}"
    [ -f "$src" ] && cp "$src" "$dist/"
  done
done

echo "== building Python CLI (one-file) =="
# Build from the project venv (editable install) so PyInstaller bundles the
# current source; a fresh `uvx --with .` env can serve a cached wheel instead.
cd "$root/cli"
uv sync >/dev/null
uv run --with pyinstaller \
  pyinstaller --onefile --clean --name ralphus \
  --distpath "$dist" --workpath "$root/target/pyinstaller" --specpath "$root/target/pyinstaller" \
  "$root/scripts/ralphus_entry.py"

echo "== building Python runner (one-file, bundles pydantic-ai) =="
# The runner drives native model agents via pydantic-ai, so the standalone exe
# must carry that tree: the anthropic + openai SDKs (openai backs the Ollama
# OpenAI-compatible path) and tiktoken's encoding plugins (loaded dynamically
# through tiktoken_ext, which PyInstaller misses without an explicit hint).
# --recursive-copy-metadata pulls the .dist-info for pydantic-ai-slim AND its
# deps: several (e.g. genai-prices) call importlib.metadata.version() at import
# time and raise PackageNotFoundError -- an ImportError -- without their metadata.
uv sync --extra runner >/dev/null
uv run --extra runner --with pyinstaller \
  pyinstaller --onefile --clean --name ralphus-runner \
  --collect-all pydantic_ai \
  --collect-all anthropic \
  --collect-all openai \
  --collect-all tiktoken \
  --collect-submodules tiktoken_ext \
  --hidden-import tiktoken_ext.openai_public \
  --recursive-copy-metadata pydantic-ai-slim \
  --distpath "$dist" --workpath "$root/target/pyinstaller" --specpath "$root/target/pyinstaller" \
  "$root/scripts/ralphus_runner_entry.py"

echo "== done; artifacts in $dist =="
ls -la "$dist"
