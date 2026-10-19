#!/usr/bin/env bash
# Run the daemon stress suite retroactively against past commits, so the
# performance trend has history from before the harness existed.
#
#   bash scripts/stress-history.sh                       # 1 day/1 week/1 month/2 months ago
#   bash scripts/stress-history.sh --ago "3 days,2 weeks"
#   bash scripts/stress-history.sh --rev v0.4.0 --rev 1a2b3c4 -- --quick
#
# For each commit: a detached `git worktree` is created under the work dir,
# `ralphus-daemon` is built there in release mode (all commits share one
# CARGO_TARGET_DIR so dependencies compile once), the binary is copied out,
# the worktree is removed, and the *current* `ralphus-stress` runs against
# it. The harness probes each daemon's API shape (`/api/squads` vs the older
# `/api/runs`, token auth, user registry), so old commits work unmodified.
# Results land in perf/stress/results/ and HISTORY.md is regenerated.
#
# Arguments after `--` are passed to `ralphus-stress run`.
# RALPHUS_STRESS_WORK overrides the scratch directory.
set -euo pipefail

repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo"

ago="1 day,1 week,1 month,2 months"
revs=()
pass=()
while [ $# -gt 0 ]; do
  case "$1" in
    --ago) ago="$2"; shift 2 ;;
    --rev) revs+=("$2"); ago=""; shift 2 ;;
    --) shift; pass=("$@"); break ;;
    *) echo "unknown argument: $1" >&2; exit 2 ;;
  esac
done

exe=""
case "$(uname -s)" in MINGW*|MSYS*|CYGWIN*) exe=".exe" ;; esac

work="${RALPHUS_STRESS_WORK:-${TMPDIR:-/tmp}/ralphus-stress-history}"
mkdir -p "$work/bin"

if [ -n "$ago" ]; then
  IFS=',' read -r -a spans <<< "$ago"
  for span in "${spans[@]}"; do
    span="$(echo "$span" | sed 's/^ *//; s/ *$//')"
    c="$(git rev-list -1 --first-parent --before="$span ago" HEAD || true)"
    if [ -z "$c" ]; then
      echo "no commit older than '$span ago'; skipping" >&2
      continue
    fi
    revs+=("$c")
  done
fi

cargo build --release -p ralphus-stress
stress="$repo/target/release/ralphus-stress$exe"

failed=()
for rev in "${revs[@]}"; do
  sha="$(git rev-parse --verify "$rev^{commit}")"
  short="${sha:0:12}"
  bin="$work/bin/ralphus-daemon-$short$exe"
  echo "=== $short $(git log -1 --format='%cs %s' "$sha")"
  if [ ! -f "$bin" ]; then
    wt="$work/wt-$short"
    git worktree remove --force "$wt" 2>/dev/null || true
    git worktree add --detach "$wt" "$sha" >/dev/null
    if ! (cd "$wt" && CARGO_TARGET_DIR="$work/target" cargo build --release -p ralphus-daemon); then
      echo "build failed for $short; skipping" >&2
      failed+=("$short (build)")
      git worktree remove --force "$wt" || true
      continue
    fi
    cp "$work/target/release/ralphus-daemon$exe" "$bin"
    git worktree remove --force "$wt"
  fi
  if ! "$stress" run --daemon-exe "$bin" --repo "$repo" --commit "$sha" --out "$repo/perf/stress" \
      --work "$work/sandboxes/$short" ${pass[@]+"${pass[@]}"}; then
    failed+=("$short (run)")
  fi
done

"$stress" history --out "$repo/perf/stress"
if [ ${#failed[@]} -gt 0 ]; then
  echo "failed: ${failed[*]}" >&2
  exit 1
fi
