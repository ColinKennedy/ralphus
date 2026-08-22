#!/usr/bin/env bash
# build-debug.sh -- the FAST counterpart to build-release.sh.
# Fast local dev loop -- NO dist/. Runs the whole stack from source so
# iterating on the GUI (librarian/assets/board.html) is quick:
#
#   * daemon + librarian + runner + cli   -> cargo debug builds (incremental;
#                                            seconds each; all four are Rust)
#
# All four binaries are Rust -- there is no Python venv sync step. `cli/`
# still exists for `docsgen/` (Playwright screenshots, dev-only, never
# shipped).
#
# Loop: edit board.html -> re-run this script -> refresh the browser.
# Ctrl-C stops both processes. For a distributable standalone build (slow),
# use build-release.sh instead.
#
# Usage: build-debug.sh [--daemon-port N] [--librarian-port N] [--db-path PATH]
# Defaults to 7890/7474. Pass different ports to run a second stack alongside
# the regular one. RAL-164: --daemon-port + --db-path together give FULL
# isolation (separate port AND separate SQLite DB) -- the right way to keep
# your regular ralphus instance open while testing ralphus in another git
# worktree. Without --db-path, a non-default --daemon-port still gets its own
# DB automatically (derived as ~/.ralphus/tasks-<port>.db); the *default*
# port keeps using the plain ~/.ralphus/tasks.db it always has, so existing
# setups are unaffected. Ports/paths are never silently invented beyond this
# per-port default -- write down whatever you pass so a later
# `ralphus-daemon stop --port N` targets the right instance.
#
# Example (regular stack, defaults):     ./build-debug.sh
# Example (second stack, fully isolated): ./build-debug.sh --daemon-port 7891 --librarian-port 7475 --db-path ~/.ralphus/tasks-worktree2.db
set -euo pipefail

daemon_port=7890
librarian_port=7474
db_path=""

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
    --db-path)
      db_path="$2"
      shift 2
      ;;
    *)
      echo "unknown argument: $1" >&2
      echo "usage: $0 [--daemon-port N] [--librarian-port N] [--db-path PATH]" >&2
      exit 1
      ;;
  esac
done

# Only auto-derive a per-port DB when the daemon port was actually changed
# from the default -- the default port keeps its existing DB path untouched.
if [ -z "$db_path" ] && [ "$daemon_port" != "7890" ]; then
  state_home="${USERPROFILE:-${HOME:-.}}"
  db_path="${state_home}/.ralphus/tasks-${daemon_port}.db"
fi

# Root is whichever checkout this script lives in (main or a worktree) --
# mirrors build-debug.cmd's `%~dp0..` resolution, so a review worktree builds
# and runs its own binaries instead of the main checkout's.
root="$(cd "$(dirname "$0")/.." && pwd)"

# 1. Build all four Rust bins in debug (fast incremental rebuild picks up
#    board.html and any CLI/runner source edit alike).
echo "== cargo build (debug) daemon + librarian + runner + cli =="
cargo build -p ralphus-daemon -p ralphus-librarian -p ralphus-runner -p ralphus-cli --manifest-path "$root/Cargo.toml"

ext=""; [ -f "$root/target/debug/ralphus-daemon.exe" ] && ext=".exe"

# 2. Point RALPHUS_RUNNER_CMD at the just-built debug runner exe.
export RALPHUS_RUNNER_CMD="$root/target/debug/ralphus-runner${ext}"

# 3. Daemon in the background, librarian in the foreground. Ctrl-C (or the
#    librarian exiting) tears the daemon down too.
export RALPHUS_DAEMON_URL="http://127.0.0.1:${daemon_port}"
echo "== starting stack =="
echo "   runner    -> $RALPHUS_RUNNER_CMD"
echo "   cli       -> $root/target/debug/ralphus${ext} (not started; run it yourself, e.g. 'ralphus status')"
echo "   daemon    -> $RALPHUS_DAEMON_URL"
echo "   db        -> ${db_path:-<default: ~/.ralphus/tasks.db>}"
echo "   librarian -> http://127.0.0.1:${librarian_port}"
daemon_args=(serve --port "$daemon_port")
[ -n "$db_path" ] && daemon_args+=(--db "$db_path")
"$root/target/debug/ralphus-daemon${ext}" "${daemon_args[@]}" &
daemon_pid=$!
trap 'kill "$daemon_pid" 2>/dev/null || true' EXIT

"$root/target/debug/ralphus-librarian${ext}" serve --port "$librarian_port"
