#!/usr/bin/env bash
# Build the three ralphus executables into ./dist as copyable standalone binaries.
#
#   daemon    (Rust)   -> dist/ralphus-daemon[.exe]
#   librarian (Rust)   -> dist/ralphus-librarian[.exe]
#   CLI       (Python) -> dist/ralphus[.exe]   (one-file, via PyInstaller)
#
# The Rust binaries link SQLite in (rusqlite `bundled`) so they need no system
# libraries. The Python CLI is bundled with its interpreter by PyInstaller.
# Packaging the pydantic-ai *runner* as a standalone exe is a follow-up (it pulls
# a large dependency tree); for now the runner ships as the Python package.
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

echo "== done; artifacts in $dist =="
ls -la "$dist"
