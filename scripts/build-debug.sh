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
#
# Usage: build-debug.sh [--daemon-port N] [--librarian-port N]
# Defaults to 7890/7474. Pass different ports to run a second stack alongside
# the regular one -- but both instances still share the same SQLite DB
# (~/.ralphus/tasks.db) unless USERPROFILE/HOME is also overridden, so this
# is for a second UI/API endpoint onto the same data, not full isolation.
#
# Example (regular stack, defaults):     ./build-debug.sh
# Example (second stack, side-by-side):  ./build-debug.sh --daemon-port 7891 --librarian-port 7475
set -euo pipefail

daemon_port=7890
librarian_port=7474

while [[ $# -gt 0 ]]; do
  case "$1" in
    --daemon-port)
      daemon_port="$2"
      shift 2
      ;;
    --librarian-port)
      librarian_port="$2"
      shift 2
      ;;
    *)
      echo "unknown argument: $1" >&2
      echo "usage: $0 [--daemon-port N] [--librarian-port N]" >&2
      exit 1
      ;;
  esac
done

# Root is whichever checkout this script lives in (main or a worktree) --
# mirrors build-debug.cmd's `%~dp0..` resolution, so a review worktree builds
# and runs its own binaries instead of the main checkout's.
root="$(cd "$(dirname "$0")/.." && pwd)"

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
export RALPHUS_DAEMON_URL="http://127.0.0.1:${daemon_port}"
echo "== starting stack =="
echo "   runner    -> $RALPHUS_RUNNER_CMD"
echo "   daemon    -> $RALPHUS_DAEMON_URL"
echo "   librarian -> http://127.0.0.1:${librarian_port}"
"$root/target/debug/ralphus-daemon${ext}" serve --port "$daemon_port" &
daemon_pid=$!
trap 'kill "$daemon_pid" 2>/dev/null || true' EXIT

"$root/target/debug/ralphus-librarian${ext}" serve --port "$librarian_port"
