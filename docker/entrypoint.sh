#!/usr/bin/env bash
# Container-mode entrypoint (RAL-225): starts the daemon in the background and
# the librarian in the foreground, mirroring scripts/start-ralphus.sh's
# daemon-then-librarian shape but without any Windows-only path translation --
# this image only ever runs on Linux.
#
# RALPHUS_BIND_ADDR is baked to 0.0.0.0 in the image (see docker/Dockerfile)
# so both listeners are reachable through Docker's published ports; nothing
# else about their behavior changes.
set -euo pipefail

daemon_port="${RALPHUS_DAEMON_PORT:-7890}"
librarian_port="${RALPHUS_LIBRARIAN_PORT:-7474}"

echo "== ralphus container mode =="
echo "   workspace root -> /workspaces"
echo "   runner         -> ${RALPHUS_RUNNER_CMD}"
echo "   daemon         -> http://${RALPHUS_BIND_ADDR}:${daemon_port} (RALPHUS_DAEMON_URL=${RALPHUS_DAEMON_URL})"
echo "   librarian      -> http://${RALPHUS_BIND_ADDR}:${librarian_port}"

ralphus-daemon serve --port "$daemon_port" &
daemon_pid=$!

cleanup() {
  kill "$daemon_pid" 2>/dev/null || true
  wait "$daemon_pid" 2>/dev/null || true
}
trap cleanup EXIT INT TERM

# Give the daemon a moment to bind before the librarian starts proxying to it
# -- a fixed short sleep rather than a health poll, matching this script's
# "thin process supervisor, not a real init system" scope.
sleep 1

ralphus-librarian serve --port "$librarian_port" &
librarian_pid=$!

# Exit as soon as either process dies, so `docker restart`/orchestrator
# health checks see a real failure instead of a container that limps along
# with half the stack gone.
wait -n "$daemon_pid" "$librarian_pid"
