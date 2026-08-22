#!/usr/bin/env bash
# Launch a prebuilt ralphus bundle from its extracted root directory.
#
# Usage:
#   ./start-ralphus.sh [--daemon-port N] [--librarian-port N] [--db-path PATH] [--daemon-only]
set -euo pipefail

daemon_port=7890
librarian_port=7474
db_path=""
daemon_only=0

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
    --daemon-only)
      daemon_only=1
      shift
      ;;
    *)
      echo "unknown argument: $1" >&2
      echo "usage: $0 [--daemon-port N] [--librarian-port N] [--db-path PATH] [--daemon-only]" >&2
      exit 1
      ;;
  esac
done

root="$(cd "$(dirname "$0")" && pwd)"
if [[ -z "$db_path" && "$daemon_port" != "7890" ]]; then
  state_home="${USERPROFILE:-${HOME:-.}}"
  db_path="${state_home}/.ralphus/tasks-${daemon_port}.db"
fi

to_win_path() {
  if command -v cygpath >/dev/null 2>&1; then
    cygpath -w "$1"
  else
    printf '%s\n' "$1"
  fi
}

runner_path="$root/bin/ralphus-runner/ralphus-runner.exe"
tmux_path="$root/tmux/tmux.exe"
daemon_exe="$root/bin/ralphus-daemon.exe"
librarian_exe="$root/bin/ralphus-librarian.exe"

for required in "$daemon_exe" "$librarian_exe" "$runner_path" "$tmux_path"; do
  if [[ ! -f "$required" ]]; then
    echo "required file missing: $required" >&2
    exit 1
  fi
done

export RALPHUS_RUNNER_CMD="$(to_win_path "$runner_path")"
export RALPHUS_TMUX_CMD="$(to_win_path "$tmux_path")"
export RALPHUS_DAEMON_URL="http://127.0.0.1:${daemon_port}"

echo "== starting ralphus bundle =="
echo "   runner    -> $RALPHUS_RUNNER_CMD"
echo "   tmux      -> $RALPHUS_TMUX_CMD"
echo "   daemon    -> $RALPHUS_DAEMON_URL"
if [[ -n "$db_path" ]]; then
  echo "   db        -> $db_path"
else
  echo "   db        -> <default: ~/.ralphus/tasks.db>"
fi
if [[ "$daemon_only" -eq 0 ]]; then
  echo "   librarian -> http://127.0.0.1:${librarian_port}"
fi

daemon_args=(serve --port "$daemon_port")
if [[ -n "$db_path" ]]; then
  daemon_args+=(--db "$(to_win_path "$db_path")")
fi
"$daemon_exe" "${daemon_args[@]}" &
daemon_pid=$!

cleanup() {
  kill "$daemon_pid" 2>/dev/null || true
}

if ! powershell.exe -NoProfile -Command \
  "\$deadline = (Get-Date).AddSeconds(30); while ((Get-Date) -lt \$deadline) { try { Invoke-RestMethod -Uri '$RALPHUS_DAEMON_URL/api/daemon' -TimeoutSec 2 | Out-Null; exit 0 } catch { Start-Sleep -Milliseconds 250 } }; exit 1"
then
  echo "daemon did not become ready within 30 seconds" >&2
  cleanup
  exit 1
fi

if [[ "$daemon_only" -eq 1 ]]; then
  echo "daemon is ready on $RALPHUS_DAEMON_URL"
  exit 0
fi

trap cleanup EXIT
"$librarian_exe" serve --port "$librarian_port"
