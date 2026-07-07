#!/usr/bin/env bash
# build-debug.sh -- the FAST counterpart to build-release.sh.
# Fast local dev loop -- NO PyInstaller, NO dist/. Runs the whole stack from
# source so iterating on the GUI (librarian/assets/board.html) is quick:
#
#   * daemon + librarian   -> cargo debug builds (incremental; seconds)
#   * runner               -> the venv script via uv (RALPHUS_RUNNER_CMD),
#                             so a GUI/daemon edit NEVER rebuilds the heavy
#                             standalone runner exe.
#
# Loop: edit board.html -> re-run this script -> refresh the browser.
# Ctrl-C stops both processes. For a distributable standalone build (slow),
# use build-release.sh instead.
set -euo pipefail

# Resolve the main repo root via git's --git-common-dir so this works correctly
# when invoked from a linked worktree (dirname "$0" alone would give the worktree
# root, not the main checkout, causing RALPHUS_RUNNER_CMD to point into the worktree).
_script_dir="$(cd "$(dirname "$0")" && pwd)"
root="$(dirname "$(git -C "$_script_dir" rev-parse --path-format=absolute --git-common-dir)")"
unset _script_dir

# 1. Runner: use the venv script (fast; no bundling). Sync the runner extra so
#    native model agents work; this is a near-no-op once the venv is warm.
echo "== syncing runner venv (uv) =="
( cd "$root/cli" && uv sync --extra runner >/dev/null )
runner="$root/cli/.venv/Scripts/ralphus-runner.exe"   # Windows
[ -f "$runner" ] || runner="$root/cli/.venv/bin/ralphus-runner"  # POSIX
export RALPHUS_RUNNER_CMD="$runner"

# 2. Build the Rust bins in debug (fast incremental rebuild picks up board.html).
echo "== cargo build (debug) daemon + librarian =="
cargo build -p ralphus-daemon -p ralphus-librarian --manifest-path "$root/Cargo.toml"

ext=""; [ -f "$root/target/debug/ralphus-daemon.exe" ] && ext=".exe"

# 3. Daemon in the background, librarian in the foreground. Ctrl-C (or the
#    librarian exiting) tears the daemon down too.
export RALPHUS_DAEMON_URL="http://127.0.0.1:7890"
echo "== starting stack =="
echo "   runner -> $RALPHUS_RUNNER_CMD"
echo "   daemon -> $RALPHUS_DAEMON_URL"
"$root/target/debug/ralphus-daemon${ext}" serve &
daemon_pid=$!
trap 'kill "$daemon_pid" 2>/dev/null || true' EXIT

"$root/target/debug/ralphus-librarian${ext}" serve
