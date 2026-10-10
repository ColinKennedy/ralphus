#!/usr/bin/env bash
# nextest run-wrapper (RAL-604): runs one test under `strace -f` and, only if
# the test PASSES, records how many processes it exec'd (the test binary itself
# included) to $RALPHUS_PROC_COUNT_DIR/<sanitized id>.count as "<id>\t<count>".
# A failing test writes nothing. Linux + strace only; elsewhere the test just
# runs unwrapped and no data is recorded. Exit status is the test's own.
set -u

dir="${RALPHUS_PROC_COUNT_DIR:-}"
id="${NEXTEST_BINARY_ID:-} ${NEXTEST_TEST_NAME:-}"

if [ -z "$dir" ] || [ -z "${NEXTEST_TEST_NAME:-}" ] || ! command -v strace >/dev/null 2>&1; then
    exec "$@"
fi

trace="$(mktemp)"
strace -f -qq -e trace=execve -o "$trace" -- "$@"
status=$?

if [ "$status" -eq 0 ]; then
    # With -f a call interrupted by another thread prints "<... execve resumed>".
    count="$(grep -cE 'execve\(.*\) += 0$|execve resumed>.*\) += 0$' "$trace" || true)"
    file="$dir/$(printf '%s' "$id" | tr -c 'A-Za-z0-9._-' '_').count"
    printf '%s\t%s\n' "$id" "$count" >"$file"
fi
rm -f "$trace"
exit "$status"
