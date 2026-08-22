#!/usr/bin/env bash
# run-container.sh -- selects the container execution mode (RAL-225): the
# whole daemon+librarian+runner stack, and therefore every locally-executed
# agent subprocess, runs inside one hardened container instead of as bare
# host subprocesses. See docs/container-mode.md before relying on this for
# real isolation -- it documents exactly what the container boundary does
# and does not confine.
#
# This is a thin wrapper around `docker compose`; scripts/build-debug.sh and
# scripts/start-ralphus.sh (bare subprocess, no container) remain the
# default and are unaffected by this script's existence.
#
# Usage:
#   RALPHUS_WORKSPACE_ROOT=/path/to/your/checkouts bash scripts/run-container.sh [--daemon-port N] [--librarian-port N] [docker compose args...]
#
# RALPHUS_WORKSPACE_ROOT is the ONLY host directory the container can read or
# write -- point it at a parent directory of whatever project checkouts your
# task files' `cwd`s live under, and use container-side paths (e.g.
# `/workspaces/my-project`) in those task files when submitting against this
# instance.
set -euo pipefail

: "${RALPHUS_WORKSPACE_ROOT:?usage: RALPHUS_WORKSPACE_ROOT=/path/to/your/checkouts bash scripts/run-container.sh}"

daemon_port=7890
librarian_port=7474
compose_args=()

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
      compose_args+=("$1")
      shift
      ;;
  esac
done

root="$(cd "$(dirname "$0")/.." && pwd)"

export RALPHUS_WORKSPACE_ROOT
export RALPHUS_DAEMON_PORT="$daemon_port"
export RALPHUS_LIBRARIAN_PORT="$librarian_port"

echo "== ralphus container mode =="
echo "   workspace root -> $RALPHUS_WORKSPACE_ROOT (mounted read-write at /workspaces in the container)"
echo "   daemon         -> http://127.0.0.1:${daemon_port}"
echo "   librarian      -> http://127.0.0.1:${librarian_port}"

exec docker compose -f "$root/docker/docker-compose.yml" up --build "${compose_args[@]}"
