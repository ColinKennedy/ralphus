#!/usr/bin/env bash
# build-debug.sh -- the FAST counterpart to build-release.sh.
# Fast local dev loop -- NO dist/. Runs the whole stack from source so
# iterating on the GUI (librarian/assets's board) is quick:
#
#   * daemon + librarian + runner + cli   -> cargo debug builds (incremental;
#                                            seconds each; all four are Rust)
#
# All four binaries are Rust -- there is no Python venv sync step. `cli-py/`
# still exists for `docsgen/` (Playwright screenshots, dev-only, never
# shipped).
#
# Loop: edit a board asset (librarian/assets/board/*.js, board.css,
# board.html) and refresh the browser — the librarian reads board assets from
# disk in dev mode (RALPHUS_BOARD_ASSETS_DIR is exported below), so no rebuild
# is needed. Re-run this script only when Rust code changes.
# Ctrl-C stops both processes. For a distributable standalone build (slow),
# use build-release.sh instead.
#
# Usage: build-debug.sh [--daemon-port N] [--librarian-port N] [--db-path PATH] [--webhook-tunnel]
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
# --webhook-tunnel starts an ngrok tunnel to --daemon-port (ngrok must
# already be on PATH -- https://ngrok.com/download; it's a dev-only tool,
# never a project dependency) and exports RALPHUS_DAEMON_PUBLIC_URL to
# whatever public HTTPS URL ngrok hands back *this run* before the daemon
# starts -- see config.rs's load_daemon_config_with. Solves the chicken-and-
# egg of "the daemon needs public_url at startup, but a free-tier ngrok URL
# is only known once ngrok itself starts, and changes every run" without
# ever touching .ralphus.toml by hand. The tunnel is torn down alongside the
# daemon on exit (same EXIT trap).
#
# Example (regular stack, defaults):      ./build-debug.sh
# Example (second stack, fully isolated): ./build-debug.sh --daemon-port 7891 --librarian-port 7475 --db-path ~/.ralphus/tasks-worktree2.db
# Example (webhook dev, real deliveries): ./build-debug.sh --webhook-tunnel
set -euo pipefail

daemon_port=7890
librarian_port=7474
db_path=""
webhook_tunnel=0

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
    --webhook-tunnel)
      webhook_tunnel=1
      shift
      ;;
    *)
      echo "unknown argument: $1" >&2
      echo "usage: $0 [--daemon-port N] [--librarian-port N] [--db-path PATH] [--webhook-tunnel]" >&2
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
#    any CLI/runner/librarian source edit alike).
echo "== cargo build (debug) daemon + librarian + runner + cli =="
cargo build --package ralphus-daemon --package ralphus-librarian --package ralphus-runner --package ralphus-cli --manifest-path "$root/Cargo.toml"

ext=""; [ -f "$root/target/debug/ralphus-daemon.exe" ] && ext=".exe"

# 2. Point RALPHUS_RUNNER_CMD at the just-built debug runner exe.
export RALPHUS_RUNNER_CMD="$root/target/debug/ralphus-runner${ext}"

# --webhook-tunnel: start ngrok and capture its (fresh-every-run) public
# HTTPS URL into RALPHUS_DAEMON_PUBLIC_URL before the daemon starts, so
# config.rs's load_daemon_config picks it up as this run's [daemon]
# public_url override -- no .ralphus.toml edit, ever. Polls ngrok's own
# local status API (127.0.0.1:4040, loopback-only, no auth) rather than
# scraping ngrok's console output, since the log line format isn't a
# stable contract and this API is (https://ngrok.com/docs/agent/api).
ngrok_pid=""
if [ "$webhook_tunnel" = "1" ]; then
  if ! command -v ngrok >/dev/null 2>&1; then
    echo "error: --webhook-tunnel requires ngrok on PATH (https://ngrok.com/download -- a dev-only tool, not a project dependency)" >&2
    exit 1
  fi
  echo "== starting ngrok tunnel to port ${daemon_port} =="
  ngrok http "$daemon_port" &
  ngrok_pid=$!
  webhook_public_url=""
  attempt=0
  while [ -z "$webhook_public_url" ] && [ "$attempt" -lt 30 ]; do
    webhook_public_url="$(curl -s http://127.0.0.1:4040/api/tunnels 2>/dev/null | sed -n 's/.*"public_url":"\(https:\/\/[^"]*\)".*/\1/p' | head -n1)"
    [ -z "$webhook_public_url" ] && sleep 0.5
    attempt=$((attempt + 1))
  done
  if [ -z "$webhook_public_url" ]; then
    echo "error: ngrok did not report a public https:// tunnel within 15s (check http://127.0.0.1:4040)" >&2
    kill "$ngrok_pid" 2>/dev/null || true
    exit 1
  fi
  echo "   webhook tunnel -> ${webhook_public_url} (ngrok pid ${ngrok_pid})"
  export RALPHUS_DAEMON_PUBLIC_URL="$webhook_public_url"
fi

# 3. Daemon in the background, librarian in the foreground. Ctrl-C (or the
#    librarian exiting) tears the daemon (and any ngrok tunnel) down too.
export RALPHUS_DAEMON_URL="http://127.0.0.1:${daemon_port}"
echo "== starting stack =="
echo "   runner    -> $RALPHUS_RUNNER_CMD"
echo "   cli       -> $root/target/debug/ralphus${ext} (not started; run it yourself, e.g. 'ralphus status')"
echo "   daemon    -> $RALPHUS_DAEMON_URL"
echo "   db        -> ${db_path:-<default: ~/.ralphus/tasks.db>}"
echo "   librarian -> http://127.0.0.1:${librarian_port}"
echo "   board dev mode -> reading librarian/assets from disk (RALPHUS_BOARD_ASSETS_DIR); edits are live on browser refresh"
daemon_args=(serve --port "$daemon_port")
[ -n "$db_path" ] && daemon_args+=(--db "$db_path")
"$root/target/debug/ralphus-daemon${ext}" "${daemon_args[@]}" &
daemon_pid=$!
trap 'kill "$daemon_pid" 2>/dev/null || true; [ -n "$ngrok_pid" ] && kill "$ngrok_pid" 2>/dev/null || true' EXIT

# Dev mode: board assets are read from the checkout on every request, so an
# edit to a chunk/CSS/shell file is one browser-refresh away (no rebuild).
export RALPHUS_BOARD_ASSETS_DIR="$root/librarian/assets"

# Reaching past this line means the librarian exited -- the EXIT trap then
# kills the daemon, so say so out loud: a silent teardown here has previously
# masqueraded as "the daemon crashes after ~2 minutes".
librarian_exit=0
"$root/target/debug/ralphus-librarian${ext}" serve --port "$librarian_port" || librarian_exit=$?
echo "== librarian exited (code ${librarian_exit}); stopping daemon (pid ${daemon_pid}) =="
