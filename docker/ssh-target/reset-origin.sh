#!/bin/sh
# Resets the fixture's bare Git origin to its pristine seeded state
# (RAL-355). Extracted out of entrypoint.sh so a destructive integration-test
# suite can invoke this on demand -- `docker compose exec target
# /usr/local/bin/reset-origin.sh --force` -- without tearing down the
# `ssh-target destroy` volumes it does not need to touch (the remote root,
# the pinned host key).
#
# Default (no argument, also entrypoint.sh's own boot-time call): a no-op if
# the origin already exists, so a normal container restart keeps whatever
# history earlier tasks pushed. `--force`: always wipe and reseed, for a
# clean origin between test suites.
set -eu

origin=/srv/git/ralphus-test.git
force=0
if [ "${1:-}" = "--force" ]; then
    force=1
fi

if [ -d "$origin" ] && [ "$force" -eq 0 ]; then
    exit 0
fi

rm -rf "$origin"
seed=/tmp/ralphus-origin-seed
rm -rf "$seed"
install -d -m 0755 -o ralphus -g ralphus "$seed"
runuser -u ralphus -- git init --initial-branch=main "$seed"
runuser -u ralphus -- git -C "$seed" config user.name "Ralphus Docker Test"
runuser -u ralphus -- git -C "$seed" config user.email "ralphus-docker@example.invalid"
printf '%s\n' '# Ralphus SSH target fixture' >"$seed/README.md"
chown ralphus:ralphus "$seed/README.md"
runuser -u ralphus -- git -C "$seed" add README.md
runuser -u ralphus -- git -C "$seed" commit -m "seed fixture origin"
runuser -u ralphus -- git init --bare --initial-branch=main "$origin"
runuser -u ralphus -- git -C "$seed" remote add origin "$origin"
runuser -u ralphus -- git -C "$seed" push origin main
rm -rf "$seed"
chown -R ralphus:ralphus /srv/git
