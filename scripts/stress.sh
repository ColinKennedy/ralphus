#!/usr/bin/env bash
# Run the daemon stress suite against the current checkout and record the
# results under perf/stress/ (keyed by the tag on HEAD, else its short hash).
#
#   bash scripts/stress.sh            # full suite (~25-35 min)
#   bash scripts/stress.sh --quick    # smoke pass (~5 min)
#   bash scripts/stress.sh --scenarios faults --faults slowloris
#
# Every extra argument is passed to `ralphus-stress run` (see perf/stress/README.md).
# The daemon under test runs in a throwaway sandbox (own port, database, home
# and config dir), so a running dev stack is never touched.
set -euo pipefail

repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo"

exe=""
case "$(uname -s)" in MINGW*|MSYS*|CYGWIN*) exe=".exe" ;; esac

cargo build --release -p ralphus-daemon -p ralphus-stress

"$repo/target/release/ralphus-stress$exe" run \
  --daemon-exe "$repo/target/release/ralphus-daemon$exe" \
  --repo "$repo" \
  --out "$repo/perf/stress" \
  "$@"
