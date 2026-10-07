#!/usr/bin/env bash
# Run every `ralphus initialize <exercise>` live, locally and fully remote
# (--remote: every cell and review on a strict loopback machine), each against
# its own throwaway daemon that --stop shuts down afterwards.
#
# Usage: bash scripts/check-initialize-exercises.sh [--bin-dir DIR] [--state-root DIR]
#                                                   [--only NAME[,NAME...]] [--local-only|--remote-only]
#
# --bin-dir defaults to target/debug and must hold ralphus, ralphus-daemon and
# ralphus-runner (build them first: `cargo build -p ralphus-cli -p
# ralphus-daemon -p ralphus-runner`). Needs git and Python 3 on PATH (the
# loopback machine provider is examples/providers/loopback.py). Exits non-zero
# if any exercise fails, printing that exercise's output and daemon log tail.
#
# Run it from your own terminal, never from inside a ralphus cell or feedback
# pass: each daemon's startup reap kills every ralphus_ tmux server on the
# machine, including the one hosting the caller (.agent/agent-conduct.md).
set -u

repo_root=$(cd "$(dirname "$0")/.." && pwd)
bin_dir="$repo_root/target/debug"
state_root=""
only=""
modes="local remote"
while [ $# -gt 0 ]; do
    case "$1" in
        --bin-dir) bin_dir=$2; shift 2 ;;
        --state-root) state_root=$2; shift 2 ;;
        --only) only=$2; shift 2 ;;
        --local-only) modes="local"; shift ;;
        --remote-only) modes="remote"; shift ;;
        -h|--help) sed -n '2,13p' "$0"; exit 0 ;;
        *) echo "unknown argument: $1" >&2; exit 2 ;;
    esac
done

exe=""
case "$(uname -s)" in MINGW*|MSYS*|CYGWIN*) exe=".exe" ;; esac
cli="$bin_dir/ralphus$exe"
for binary in ralphus ralphus-daemon ralphus-runner; do
    if [ ! -x "$bin_dir/$binary$exe" ]; then
        echo "missing $bin_dir/$binary$exe -- build it first" >&2
        exit 2
    fi
done
if [ -z "$state_root" ]; then
    state_root=$(mktemp -d "${TMPDIR:-/tmp}/ralphus-exercises.XXXXXX")
fi
mkdir -p "$state_root"

exercises="machine mailbox triage review waypoint"
# `followup` is exempt from the exercise rules (cli/src/commands/initialize/
# AGENTS.md), so it is not in EXERCISES, but it still runs here, locally only.
exempt="followup"
if [ -n "$only" ]; then
    exercises=$(echo "$only" | tr ',' ' ')
else
    exercises="$exercises $exempt"
fi

# Exercises resolve the loopback provider from the checkout they run in.
cd "$repo_root" || exit 2

failed=""
for name in $exercises; do
    for mode in $modes; do
        flag=""
        [ "$mode" = remote ] && flag="--remote"
        # `machine` always runs remote; its local pass would repeat the same run.
        if [ "$name" = machine ] && [ "$mode" = local ]; then
            continue
        fi
        # `followup` drives its review merge as a git fast-forward in the
        # daemon's own checkout, so it has no remote variant.
        if [ "$name" = followup ] && [ "$mode" = remote ]; then
            continue
        fi
        dir="$state_root/$name-$mode"
        rm -rf "$dir"
        echo "=== initialize $name ${flag:-(local)}"
        start=$(date +%s)
        # --stop shuts the exercise's daemon down; timeout is the backstop.
        timeout 600 "$cli" initialize "$name" $flag --stop --state-dir "$dir" >"$dir.out" 2>&1
        code=$?
        elapsed=$(( $(date +%s) - start ))
        if [ "$code" -eq 0 ]; then
            echo "    ok (${elapsed}s)"
        else
            echo "    FAILED exit=$code (${elapsed}s)"
            sed 's/^/    | /' "$dir.out"
            if [ -f "$dir/daemon.log" ]; then
                echo "    -- daemon.log (last 40 lines) --"
                tail -n 40 "$dir/daemon.log" | sed 's/^/    | /'
            fi
            failed="$failed $name/$mode"
        fi
    done
done

if [ -n "$failed" ]; then
    echo "failed exercises:$failed (state kept under $state_root)"
    exit 1
fi
echo "all exercises passed (state under $state_root)"
