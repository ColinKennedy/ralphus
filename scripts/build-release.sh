#!/usr/bin/env bash
# Build the four ralphus executables into ./dist as copyable standalone binaries.
#
#   daemon    (Rust)   -> dist/ralphus-daemon[.exe]
#   librarian (Rust)   -> dist/ralphus-librarian[.exe]
#   CLI       (Python) -> dist/ralphus/ralphus[.exe]               (one-DIR, PyInstaller)
#   runner    (Python) -> dist/ralphus-runner/ralphus-runner[.exe] (one-DIR, PyInstaller)
#
# The Rust binaries link SQLite in (rusqlite `bundled`) so they need no system
# libraries. The Python CLI and runner are each bundled with their interpreter by
# PyInstaller. The runner drives native model agents (claude/anthropic/ollama)
# through pydantic-ai, so its bundle carries that dependency tree (anthropic +
# openai SDKs, tiktoken encodings); the daemon points RALPHUS_RUNNER_CMD at it.
#
# -----------------------------------------------------------------------------
# WHY --onedir AND NOT --onefile
# -----------------------------------------------------------------------------
# A --onefile exe is a self-extracting archive: at launch its bootloader unpacks
# the interpreter + site-packages to $TEMP/_MEIxxxxxx, sets _MEIPASS2,
# re-executes itself as a child, and the child validates that cache before
# running. That validation can fail with
#
#   [PYI-XXXXX:ERROR] Security validation failure: parent process has different
#   executable!
#
# which was reproduced on two machines when ralphus.exe was launched from a
# sandboxed/reparenting tool layer (see PERMISSIONS_ISSUE.local.md). The Rust
# binaries in the same directory, from the same zip, never showed it -- they
# have no bootloader, no extraction, and no parent-process check.
#
# --onedir removes that whole mechanism: the payload sits next to the exe in
# _internal/, nothing is extracted to $TEMP, no _MEIPASS2, no parent check. It
# also starts faster and stops writing ~45 MB to disk on every cold run.
#
# COST: each app gets its OWN directory with its OWN copy of the interpreter, so
# dist/ is larger than the two onefile exes were, and the exes are no longer
# directly at dist/ralphus[.exe] / dist/ralphus-runner[.exe].
#
# DEPLOYING: copy the whole dist/ tree. Each Python exe must stay next to its
# sibling _internal/ directory -- moving the exe out on its own breaks it.
#   * put dist/ralphus on PATH to get the `ralphus` CLI
#   * point RALPHUS_RUNNER_CMD at the full path to
#     dist/ralphus-runner/ralphus-runner[.exe] (or put that dir on PATH too)
#   * dist/ralphus-daemon[.exe] and dist/ralphus-librarian[.exe] are standalone
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
dist="$root/dist"
mkdir -p "$dist"

# Remove stale --onefile artifacts from a previous build. Without this, an old
# dist/ralphus[.exe] survives beside the new dist/ralphus/ directory and gets
# shipped in the tarball -- and it is exactly the binary whose bootloader check
# this build switched away from.
for stale in ralphus ralphus-runner; do
  for ext in "" ".exe"; do
    if [ -f "$dist/${stale}${ext}" ]; then
      echo "== removing stale onefile artifact dist/${stale}${ext} =="
      rm -f "$dist/${stale}${ext}"
    fi
  done
done

echo "== building Rust executables (release) =="
cargo build --release -p ralphus-daemon -p ralphus-librarian --manifest-path "$root/Cargo.toml"
for bin in ralphus-daemon ralphus-librarian; do
  for ext in "" ".exe"; do
    src="$root/target/release/${bin}${ext}"
    [ -f "$src" ] && cp "$src" "$dist/"
  done
done

# PyInstaller's --version-file only means anything on Windows; on other
# targets the exe has no PE resources to embed it into. $OS=Windows_NT is set
# by the Windows environment itself, so it's present under git-bash too.
version_file_cli=()
version_file_runner=()
if [ "${OS:-}" = "Windows_NT" ]; then
  version_file_cli=(--version-file "$root/scripts/version_info_cli.txt")
  version_file_runner=(--version-file "$root/scripts/version_info_runner.txt")
fi

echo "== building Python CLI (one-dir) =="
# Build from the project venv (editable install) so PyInstaller bundles the
# current source; a fresh `uvx --with .` env can serve a cached wheel instead.
cd "$root/cli"
uv sync >/dev/null
uv run --with pyinstaller \
  pyinstaller --onedir --clean --name ralphus \
  --distpath "$dist" --workpath "$root/target/pyinstaller" --specpath "$root/target/pyinstaller" \
  "${version_file_cli[@]}" \
  "$root/scripts/ralphus_entry.py"

echo "== building Python runner (one-dir, bundles pydantic-ai) =="
# The runner drives native model agents via pydantic-ai, so the standalone exe
# must carry that tree: the anthropic + openai SDKs (openai backs the Ollama
# OpenAI-compatible path) and tiktoken's encoding plugins (loaded dynamically
# through tiktoken_ext, which PyInstaller misses without an explicit hint).
# --recursive-copy-metadata pulls the .dist-info for pydantic-ai-slim AND its
# deps: several (e.g. genai-prices) call importlib.metadata.version() at import
# time and raise PackageNotFoundError -- an ImportError -- without their metadata.
uv sync --extra runner >/dev/null
uv run --extra runner --with pyinstaller \
  pyinstaller --onedir --clean --name ralphus-runner \
  --collect-all pydantic_ai \
  --collect-all anthropic \
  --collect-all openai \
  --collect-all tiktoken \
  --collect-submodules tiktoken_ext \
  --hidden-import tiktoken_ext.openai_public \
  --recursive-copy-metadata pydantic-ai-slim \
  --distpath "$dist" --workpath "$root/target/pyinstaller" --specpath "$root/target/pyinstaller" \
  "${version_file_runner[@]}" \
  "$root/scripts/ralphus_runner_entry.py"

echo "== done; artifacts in $dist =="
ls -la "$dist"
echo
echo "Layout (each Python exe must stay beside its own _internal/ directory):"
echo "  $dist/ralphus-daemon[.exe]"
echo "  $dist/ralphus-librarian[.exe]"
echo "  $dist/ralphus/ralphus[.exe]"
echo "  $dist/ralphus-runner/ralphus-runner[.exe]"
