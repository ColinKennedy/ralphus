# Per-test process counts (RAL-604)

Process spawning is expensive and easy to regress unnoticed. This tracks, for a
**tagged subset** of tests, how many processes each test spawns, and graphs it
over git tags so a change in one test's line is a real change in process
behaviour. Graph only: nothing alerts or fails CI on an increase.

## What is measured

`scripts/proc-count-wrap.sh` is a nextest run-wrapper (profile `proc-count` in
`.config/nextest.toml`). It runs one test under `strace -f -e trace=execve` and
counts successful `execve`s — the test binary itself plus every process it
launches, transitively. That is the total number of processes made to complete
that test's work. A test that fails writes nothing. Linux + `strace` only.

`ralphus-proccount collect` runs the tagged tests **three times** and keeps the
**lowest** count per test. A test that failed in any run gets no data.

## Tags

`proc_counts/tags.toml` lists the tracked tests: `id = "<nextest binary id>
<test name>"` and exactly **one** `tag` each (a duplicate id is rejected).
Every section of work gets a tag (e.g. `proof-command`, `cell-command`; a
sub-feature of a larger area gets its own single tag). Distinct behaviours are
separate tests (a prompt cell, a remediation cell, a `mode=raw` proof...).
**When you add or change behaviour that spawns processes, add a tagged test for
it.** Each test has its own line, so adding one never moves an existing line.

## Storage and graph

- `proc_counts/records.json` — list of JSON blobs
  `{label, commit, platform, counts: {test id: n}}`, one per git-tag label.
  Re-running a label updates only the tests it measured.
- `proc_counts/graph.svg`, `proc_counts/index.html` — line graph (one line per
  test, x = label), checked in so trends are visible in the repo.

## Running

```bash
cd cli-py
uv run ralphus-proccount collect                 # label = nearest v* tag
uv run ralphus-proccount collect --label v0.0.1 --ref <sha>   # backfill
uv run ralphus-proccount graph                   # re-render only
```

`.github/workflows/proc-counts.yml` runs it on a daily cron (and on manual
dispatch with `label`/`ref` inputs for backfill) on a GitHub runner, then commits
`proc_counts/`. It never runs against a developer's live stack. Do not run
`collect` from inside a ralphus cell: it builds and runs the workspace tests.

Separate from `bench_data/` (the RAL-94 timing harness), which is untouched.
