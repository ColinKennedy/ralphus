#!/usr/bin/env bash
# Prepare the live remote checks in this directory against a running daemon:
# two git fixtures (each a clone plus a bare origin), registered as projects
# `fx` and `fx2`, and the loopback machine provider registered as `loopback`.
#
# Usage: bash examples/remote/setup.sh <work-dir>
#
# The daemon must already have a `[machine.targets.*]` entry for
# `loopback:lb` in its global config ($RALPHUS_CONFIG_HOME/config.toml):
#
#   [machine.targets.lb]
#   machine = "loopback:lb"
#   remote_root = "<work-dir>/remote-root"
#   allow_ephemeral_remote_root = true   # only if <work-dir> is under a temp dir
#
# and, for the sharpest test, RALPHUS_LOOPBACK_STRICT=1 in its environment.
# Point the CLI at that daemon with RALPHUS_DAEMON_URL as usual.
set -euo pipefail

work=${1:?usage: setup.sh <work-dir>}
repo_root=$(cd "$(dirname "$0")/../.." && pwd)
python=python3
command -v python3 >/dev/null 2>&1 || python=python
mkdir -p "$work"
work=$(cd "$work" && pwd)

fixture() {
    local name=$1 checks=$2
    local repo="$work/$name" origin="$work/$name-origin.git"
    rm -rf "$repo" "$origin"
    git init --quiet --bare --initial-branch main "$origin"
    git init --quiet --initial-branch main "$repo"
    git -C "$repo" config user.email live@example.invalid
    git -C "$repo" config user.name "Live Check"
    printf '# %s\n' "$name" > "$repo/README.md"
    printf 'one\ntwo\nthree\n' > "$repo/shared.txt"
    cat > "$repo/check.py" <<'PY'
import os, sys
# Fails when CHECK_FAIL is set; prints LIVE_ENV so env propagation is visible.
print("check sees LIVE_ENV=" + os.environ.get("LIVE_ENV", "<unset>"))
sys.exit(1 if os.environ.get("CHECK_FAIL") else 0)
PY
    if [ "$checks" = yes ]; then
        printf '[review]\nchecks = ["python check.py"]\n' > "$repo/.ralphus.toml"
    fi
    git -C "$repo" add -A
    git -C "$repo" commit --quiet -m seed
    git -C "$repo" remote add origin "$origin"
    git -C "$repo" push --quiet -u origin main
    ralphus project git --name "$name" --path "$repo" --url "$origin" --description "live remote check fixture"
}

# `fx` carries a review check gate; `fx2` has none, so its review runs its
# `[[review.prepare]]` steps instead (explicit check gates take precedence).
fixture fx yes
fixture fx2 no
ralphus machine register --scheme loopback --program "$python" \
    --arg "$repo_root/examples/providers/loopback.py" --description "live remote checks"
ralphus machine check loopback
echo "ready: submit examples/remote/*.toml with ralphus submit"
