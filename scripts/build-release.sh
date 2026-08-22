#!/usr/bin/env bash
# Build the four ralphus executables into ./dist as copyable standalone
# binaries -- all four are Rust:
#
#   daemon    -> dist/ralphus-daemon[.exe]
#   librarian -> dist/ralphus-librarian[.exe]
#   CLI       -> dist/ralphus[.exe]
#   runner    -> dist/ralphus-runner[.exe]
#
# All four link SQLite in where needed (rusqlite `bundled`) and need no
# system libraries or bundled interpreter -- a plain `cargo build --release`
# produces one self-contained exe per binary, no _internal/ directory to
# keep each exe beside. `cli/` still exists for `docsgen/` (Playwright
# screenshots, dev-only, never shipped) -- see AGENTS.md.
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
dist="$root/dist"
mkdir -p "$dist"

# Remove stale PyInstaller/onefile artifacts from a previous build so a
# leftover dist/ralphus/ (dir) or dist/ralphus[.exe] (old onefile) never
# gets shipped alongside the new plain exe.
rm -rf "$dist/ralphus" "$dist/ralphus-runner"
for stale in ralphus ralphus-runner ralphus-daemon ralphus-librarian; do
  for ext in "" ".exe"; do
    [ -f "$dist/${stale}${ext}" ] && rm -f "$dist/${stale}${ext}"
  done
done

echo "== building Rust executables (release) =="
cargo build --release -p ralphus-daemon -p ralphus-librarian -p ralphus-cli -p ralphus-runner --manifest-path "$root/Cargo.toml"
for bin in ralphus-daemon ralphus-librarian ralphus ralphus-runner; do
  for ext in "" ".exe"; do
    src="$root/target/release/${bin}${ext}"
    [ -f "$src" ] && cp "$src" "$dist/"
  done
done

echo "== done; artifacts in $dist =="
ls -la "$dist"
echo
echo "Layout (each binary is a standalone exe -- no sibling directory needed):"
echo "  $dist/ralphus-daemon[.exe]"
echo "  $dist/ralphus-librarian[.exe]"
echo "  $dist/ralphus[.exe]"
echo "  $dist/ralphus-runner[.exe]"
echo
echo "Put $dist on PATH to get the 'ralphus' CLI and have RALPHUS_RUNNER_CMD"
echo "resolve 'ralphus-runner' automatically; or point RALPHUS_RUNNER_CMD at"
echo "the full path to $dist/ralphus-runner[.exe] explicitly."
