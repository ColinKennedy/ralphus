# Daemon stress tests

`ralphus-stress` (crate `stress/`) answers three questions about
`ralphus-daemon`, per release or commit:

1. **How many requests per second can it serve** with many readers and
   writers at once?
2. **At what load does the error rate pass 1%?**
3. **How does it behave when clients misbehave or the process dies?**

It writes the answers to this folder and keys them by the tag on the commit
under test, or by its short hash when there is no tag. Because the results
are committed, the trend is tracked over time in [`HISTORY.md`](HISTORY.md).

## Running it

```bash
bash scripts/stress.sh                 # full suite against HEAD (~30 min)
bash scripts/stress.sh --quick         # smoke pass (~5 min)
bash scripts/stress-history.sh         # retroactive: 1 day / 1 week / 1 month / 2 months ago
bash scripts/stress-history.sh --ago "3 days,6 weeks"
bash scripts/stress-history.sh --rev v1.2.0 --rev 1a2b3c4 -- --quick
```

In CI, use the **Stress test** workflow
(`.github/workflows/stress.yml`). Trigger it from the Actions tab with
"Run workflow" on any branch. It takes these inputs:

| Input | Values |
|---|---|
| mode | `quick` or `full` |
| scenarios | which scenarios to run |
| history_revs | optional commits/tags to run retroactively |
| extra_args | extra arguments for `ralphus-stress run` |
| runner OS | Linux or Windows |

It uploads `perf/stress/` as an artifact and puts the headline and fault
tables in the job summary. Results are not committed automatically:
download the artifact and commit the `results/*.json` you want tracked in
`HISTORY.md`. Compare numbers only within one runner type.

Or drive the binary directly:

```bash
cargo build --release -p ralphus-daemon -p ralphus-stress
target/release/ralphus-stress run --daemon-exe target/release/ralphus-daemon \
    [--scenarios ramp,volume,faults,crash] [--profiles mixed_80_20] \
    [--faults slowloris,malformed_requests] [--max-users 1024] [--step-secs 10] \
    [--fault-secs 15] [--volume 100:5,1000:25] [--quick] [--keep-sandboxes]
target/release/ralphus-stress history   # rebuild HISTORY.md from results/*.json
```

Run it on a release build, on an otherwise idle machine. The load
generator shares the machine with the daemon, so its own CPU use is recorded
(`generator_avg_cpu_pct`). Compare numbers only between runs on the same
machine; every result records the machine it ran on.

### Isolation

Every scenario starts its own daemon in a throwaway sandbox, so it never
touches a running dev stack:

- its own port and `--db`;
- `USERPROFILE`/`HOME` pointing into the sandbox, which moves the token,
  terminal logs and checkout holds;
- its own `RALPHUS_CONFIG_HOME`;
- a sandbox working directory, so no `.ralphus.toml` is picked up.

Squads are submitted with `hold: true`, and the sandbox config sets an
all-day scheduler downtime window. Nothing is ever claimed or run, so no
agent or runner process is spawned. The tests measure the HTTP/store layer.

Sandboxes are deleted after each scenario unless that scenario produced a
finding. When it did, the sandbox, including the daemon's
stdout/stderr/log files, is kept, and its path is recorded in the result.

## What runs

### Ramp profiles

Closed-loop virtual users (VUs). Each VU is one thread with its own
keep-alive connection, issuing requests back to back. The VU count doubles
each step: 1, 2, 4, … up to 4096 (`--max-users`). Each step lasts
`--step-secs`. A ramp stops once two consecutive steps exceed 1% errors,
once one step exceeds 50%, or when the daemon dies.

| Profile | Mix |
|---|---|
| `read_heavy` | board, squad detail, health, queue, projects, task index, guardians |
| `write_heavy` | submit (held), squad env set, label edit, validate |
| `mixed_80_20` | 80% of the read mix, 20% of the write mix |
| `browsing_users` | the mixed profile with a 1 s think time per user; steps of 100 → 4000 users. Models "how many people can have the board open and clicking". |

Every write except validate goes through the daemon's single write worker
(`WRITE_WORKERS` in `daemon/src/server.rs`). Reads go to the 12-thread read
pool.

### Data volume

A single daemon is grown through (squads, projects) levels:
100/5 → 1000/25 → 5000/100 → 10000/250 (set with `--volume`). At each level
the run records:

- submit throughput while seeding;
- board (`GET /api/tasks`) size and single-request latency;
- a read-heavy window and a mixed window at `--volume-users` (32) VUs.

Projects are registered against real `git init` repositories, as the daemon
requires.

### Fault injection

Each fault gets a fresh daemon. A 16-VU mixed victim load measures what
well-behaved clients see during the fault. A clean 3 s window afterwards
checks that the daemon recovered. A no-fault `baseline` run is the
comparison point.

| Fault | What it does |
|---|---|
| `abrupt_disconnect` | half a request, then close; looped from 8 clients |
| `slowloris` | 400 connections trickle one header line per second and never finish |
| `idle_connections` | 1000 connections opened and held without sending a byte |
| `truncated_body_hold` | POSTs promise a 1 MiB body, send 64 bytes, then hold the socket open. Aimed at the single write worker. |
| `truncated_body_close` | POSTs promise 5000 bytes, send 100, then close |
| `abandon_response` | a full board GET or submit POST, closed before the response is read |
| `malformed_requests` | about 35 hostile payloads: bad framing, huge headers and URLs, invalid JSON/TOML/UTF-8, CL+TE smuggling shapes, traversal and injection ids, pipelining, `Expect: 100-continue`, … Per-payload responses are recorded. |
| `oversized_body` | well-framed 16, 32 and 64 MiB submits |
| `connection_flood` | connect and close immediately, as fast as possible |
| `bad_auth_flood` | 16 clients hammer the board with a wrong bearer token |
| `thundering_herd` | 2000 new connections released at the same instant, one board GET each |

Each injector caps the connections it opens at 6000. Every closed client
socket parks an ephemeral port in TIME_WAIT, and running the generator out
of ports would read as "the daemon refused connections".

A fault's verdict is one of three:

- **fail**: the daemon died; or it stopped answering `/api/daemon`; or the
  3 s recovery window still had more than 1% errors; or no victim request
  completed for 5 s or more (the whole server stalled).
- **degraded**: during the fault, the victim had one of these:
  - more than 1% errors;
  - a p99 above 5× the baseline (and at least 500 ms over it);
  - throughput below half the baseline;
  - any op with more than 5% errors.

  A daemon panic, a 5xx answer to a malformed payload, a complete request
  that went unanswered for 10 s, or a stall longer than max(2 s, 3× the
  baseline's longest gap) also count.
- When a malformed payload kills the daemon, the harness pins the crash on
  that payload, records the exit status and stderr tail, restarts the
  daemon, and finishes the rest of the corpus. That run's verdict is
  **fail**.
- **pass**: none of the above.

### Crash and restart

The daemon is hard-killed (TerminateProcess/SIGKILL) a third of the way
into a 16-user submit-only load. It is then restarted on the same database.
The run records how long the restart took to answer. Every submit the
clients saw acknowledged (201) must still be readable afterwards. One that
is missing is data loss and fails the check. One that comes back in state
`failed` (crash recovery fails squads caught mid-materialization) marks the
check as degraded.

## Reading results

`results/<commit-date>_<label>.json` is the source of truth. It records
every step's throughput, error classes with an example message,
p50/p90/p99/max latency, per-op breakdown, daemon CPU and RSS, a trimmed
`/api/daemon` snapshot after each scenario, and the findings.
`results/<commit-date>_<label>.md` is the human summary of the same run.

Definitions:

- **ok req/s**: successful requests that finished inside the window, per
  second. Errors never inflate it.
- **error**: anything other than a 2xx/3xx: a 4xx or 5xx status, a timeout
  (10 s by default), a refused or reset connection, a protocol error, or a
  submit without an id.
- **Peak with ≤1% errors**: the best step whose error rate stayed at or
  under 1%. This is the "hard max" number to track.
- **First users >1% errors**: the concurrency where the error budget broke.
- **Max users with p99 ≤ 1 s**: the highest concurrency (before the first
  step that broke it) with p99 at or under 1 s and ≤1% errors. Read this
  together with the 1% point: requests only start to *fail* once they reach
  the 10 s client timeout, so the 1% point is often far past where the
  daemon stopped being usable.
- **Longest stall**: the longest stretch of a window in which no request
  completed successfully. A stall that is shorter than the client timeout
  shows up here even when no request fails.

Each result also includes server-side timing scraped from the daemon's own
output:

- per-route `handler`/`queued`/`lock_wait` percentiles from its request
  log. All writes are logged, but GETs only when they are `SLOW` (≥ 1 s);
- long store-lock holds, grouped by the `file:line` that took the lock.

Findings come from several sources:

- daemon output: panics (thread, `file:line:col`, message), `SLOW:`
  request lines, and error-looking log lines;
- scenario verdicts;
- data-loss checks;
- error-threshold crossings.

Triaged follow-up work for findings lives in [`followups/`](followups/).

## Adding to it

- **A new endpoint:** add an `Op` in `stress/src/api.rs` and weight it into
  a profile in `stress/src/scenarios.rs::profile_mix`. The capability probe
  drops ops an older daemon does not support. Each result records which ops
  ran, so cross-version comparisons stay honest.
- **A new fault:** add a `Fault` variant in `stress/src/faults.rs`.
- **Schema changes:** bump `report::SCHEMA_VERSION`. `HISTORY.md` is built
  from the raw JSON and tolerates missing fields.
