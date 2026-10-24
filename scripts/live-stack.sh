#!/usr/bin/env bash
# live-stack.sh -- a private ralphus stack for live-testing this checkout.
#
# Builds the daemon, librarian, runner and CLI from the checkout this script
# lives in, then starts a daemon and a board on free ports with their own
# SQLite database, so nothing the host's regular stack (7890/7474,
# ~/.ralphus/tasks.db) is doing is touched. Safe to run from inside a ralphus
# cell: the daemon's startup recovery only resets rows in its own database, and
# it binds its port before doing so.
#
# Usage:
#   live-stack.sh start [--name NAME]   build, start, print the URLs
#   live-stack.sh stop  [--name NAME]   stop exactly the pids `start` recorded
#   live-stack.sh env   [--name NAME]   print the recorded settings (KEY=value)
#   live-stack.sh plan  [--name NAME]   print the ports/paths `start` would use
#
# NAME defaults to this checkout's directory name, so each worktree gets its
# own stack and `stop` never has to guess which one is meant.
#
# State lives in ~/.ralphus/live-stacks/<name>/ (stack.env, tasks.db). Drive
# the stack with `target/debug/ralphus --daemon-url <DAEMON_URL> ...` and open
# <LIBRARIAN_URL> in a browser. Stop it when finished.
#
# Processes are launched detached (PowerShell Start-Process on Windows,
# setsid/nohup elsewhere) rather than as children of this shell: a stack that is
# a child of a killed shell loses its own child spawns on Windows. Stopping is
# always by recorded pid, never by program name.
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
state_home="${USERPROFILE:-${HOME:-.}}"
state_home="${state_home//\\//}"

usage() {
  echo "usage: $0 {start|stop|env|plan} [--name NAME]" >&2
  exit 1
}

[ $# -ge 1 ] || usage
action="$1"
shift

name="$(basename "$root")"
while [ $# -gt 0 ]; do
  case "$1" in
    --name)
      [ $# -ge 2 ] || usage
      name="$2"
      shift 2
      ;;
    *) usage ;;
  esac
done
name="${name//[^A-Za-z0-9_-]/_}"

stack_dir="$state_home/.ralphus/live-stacks/$name"
env_file="$stack_dir/stack.env"
db_path="$stack_dir/tasks.db"

windows=0
command -v powershell.exe >/dev/null 2>&1 && windows=1

native_path() {
  if [ "$windows" = 1 ]; then cygpath -w "$1"; else printf '%s' "$1"; fi
}

port_in_use() {
  (exec 3<>"/dev/tcp/127.0.0.1/$1") 2>/dev/null
}

# First free port at or above $1. The host stack's ports are never offered.
free_port() {
  local port="$1"
  while port_in_use "$port" || [ "$port" = 7890 ] || [ "$port" = 7474 ]; do
    port=$((port + 1))
  done
  echo "$port"
}

pid_alive() {
  if [ "$windows" = 1 ]; then
    powershell.exe -NoProfile -Command "if (Get-Process -Id $1 -ErrorAction SilentlyContinue) { exit 0 } else { exit 1 }" >/dev/null 2>&1
  else
    kill -0 "$1" 2>/dev/null
  fi
}

pid_stop() {
  if [ "$windows" = 1 ]; then
    powershell.exe -NoProfile -Command "Stop-Process -Id $1 -Force -ErrorAction SilentlyContinue" >/dev/null 2>&1 || true
  else
    kill "$1" 2>/dev/null || true
  fi
}

# launch <exe> [args...] -- start detached, print the pid.
launch() {
  local exe="$1"
  shift
  if [ "$windows" = 1 ]; then
    local arglist="" arg
    for arg in "$@"; do arglist="$arglist \"$arg\""; done
    powershell.exe -NoProfile -Command "(Start-Process -FilePath '$(native_path "$exe")' -ArgumentList '${arglist# }' -PassThru -WindowStyle Hidden).Id" | tr -d '\r'
  else
    setsid nohup "$exe" "$@" >"$stack_dir/$(basename "$exe").log" 2>&1 </dev/null &
    echo $!
  fi
}

wait_for_port() {
  local port="$1" label="$2" tries=0
  until port_in_use "$port"; do
    tries=$((tries + 1))
    if [ "$tries" -gt 120 ]; then
      echo "live-stack: $label did not come up on port $port within 60s" >&2
      return 1
    fi
    sleep 0.5
  done
}

# recorded <KEY> -- the value `start` wrote for KEY (empty when none).
recorded() {
  [ -f "$env_file" ] || return 0
  sed -n "s/^$1=//p" "$env_file"
}

stack_running() {
  local pid
  pid="$(recorded DAEMON_PID)"
  [ -n "$pid" ] && pid_alive "$pid"
}

print_env() {
  cat "$env_file"
}

case "$action" in
  plan)
    echo "NAME=$name"
    echo "STACK_DIR=$stack_dir"
    echo "DB=$db_path"
    echo "DAEMON_PORT=$(free_port 17890)"
    echo "LIBRARIAN_PORT=$(free_port 17474)"
    ;;

  env)
    [ -f "$env_file" ] || {
      echo "live-stack: no stack recorded for '$name'" >&2
      exit 1
    }
    print_env
    ;;

  start)
    if stack_running; then
      echo "live-stack: '$name' is already running" >&2
      print_env
      exit 0
    fi

    echo "== cargo build (debug) daemon + librarian + runner + cli ==" >&2
    cargo build --package ralphus-daemon --package ralphus-librarian --package ralphus-runner --package ralphus-cli --features ralphus-daemon/shell-syntax --manifest-path "$root/Cargo.toml" >&2

    ext=""
    [ -f "$root/target/debug/ralphus-daemon.exe" ] && ext=".exe"

    mkdir -p "$stack_dir"
    daemon_port="$(free_port 17890)"
    librarian_port="$(free_port 17474)"
    daemon_url="http://127.0.0.1:${daemon_port}"
    librarian_url="http://127.0.0.1:${librarian_port}"

    export RALPHUS_RUNNER_CMD="$root/target/debug/ralphus-runner${ext}"
    export RALPHUS_DAEMON_URL="$daemon_url"

    daemon_pid="$(launch "$root/target/debug/ralphus-daemon${ext}" serve --port "$daemon_port" --db "$(native_path "$db_path")")"
    wait_for_port "$daemon_port" "daemon" || {
      pid_stop "$daemon_pid"
      exit 1
    }
    librarian_pid="$(launch "$root/target/debug/ralphus-librarian${ext}" serve --port "$librarian_port")"
    wait_for_port "$librarian_port" "librarian" || {
      pid_stop "$librarian_pid"
      pid_stop "$daemon_pid"
      exit 1
    }

    cat >"$env_file" <<EOF
NAME=$name
DAEMON_URL=$daemon_url
LIBRARIAN_URL=$librarian_url
DAEMON_PORT=$daemon_port
LIBRARIAN_PORT=$librarian_port
DAEMON_PID=$daemon_pid
LIBRARIAN_PID=$librarian_pid
DB=$db_path
CLI=$root/target/debug/ralphus${ext}
EOF
    print_env
    echo "== live stack '$name' is up; stop it with: bash scripts/live-stack.sh stop --name $name ==" >&2
    ;;

  stop)
    [ -f "$env_file" ] || {
      echo "live-stack: no stack recorded for '$name'" >&2
      exit 0
    }
    librarian_pid="$(recorded LIBRARIAN_PID)"
    daemon_pid="$(recorded DAEMON_PID)"
    [ -n "$librarian_pid" ] && pid_stop "$librarian_pid"
    [ -n "$daemon_pid" ] && pid_stop "$daemon_pid"
    rm -f "$env_file"
    echo "live-stack: stopped '$name'" >&2
    ;;

  *) usage ;;
esac
