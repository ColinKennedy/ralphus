# Stress-test follow-ups

Issues found by `ralphus-stress` (see [`../README.md`](../README.md)). Each file
records what was measured, root cause with `file:line` citations (marked
confirmed vs inferred), a fix direction, and how to verify the fix with the
harness. First found on `035ef1e70` (staging, 2026-10-18); see
[`../HISTORY.md`](../HISTORY.md) for whether older commits share them.

## Baseline (before any fix)

Each commit was run sequentially with the full suite on one machine: a
56-CPU Xeon E5-2697 v3 on Windows 10, release builds.

| Build | Read peak | Write peak | Mixed peak | Mixed users at p99 ≤ 1 s | Board GET at ~1k squads |
|---|---:|---:|---:|---:|---:|
| `0a1df37e6` (2026-08-15) | 45/s | 152/s | 39/s | 16 | 341 ms |
| `9190c3fd5` (2026-09-18) | 28/s | 43/s | 24/s | 4 | 514 ms |
| `8f4575796` (2026-10-11) | 19/s | 98/s | 24/s | 4 | 655 ms |
| `cbd049ec5` (2026-10-17) | 19/s | 85/s | 22/s | 4 | 675 ms |
| `035ef1e70` (2026-10-18) | 19/s | 78/s | 20/s | 2 | 715 ms |

Between August and October:

- read and write throughput roughly halved;
- the board's cost at 1,000 squads doubled;
- the number of users the daemon serves with p99 under 1 s fell from 16 to
  2–4.

The read path collapsed between August and mid-September. The write path
fell by October 18.

Caveat: the 2026-10-11, 2026-09-18 and 2026-08-15 runs overlapped
development work on the same machine (`-j 8` builds and some test runs).
That may have taken a few percent off those three rows. The trend is far
larger than that noise.

| Id | Severity | Summary |
|---|---|---|
| [STRESS-001](STRESS-001-oversized-content-length-aborts-daemon.md) | critical | One request with a huge `Content-Length` aborts the daemon — unauthenticated (tiny_http `EqualReader::drop` allocation; no body cap; librarian too) |
| [STRESS-002](STRESS-002-held-request-body-freezes-whole-daemon.md) | critical | One connection holding a request body freezes every request — unauthenticated (body read on the single accept thread, no read timeout) |
| [STRESS-003](STRESS-003-read-path-ceiling-pool-starvation.md) | high | Reads top out at ~20 req/s: 4-connection read pool starved by N+1 board builds; pool wait invisible in metrics |
| [STRESS-004](STRESS-004-board-cost-grows-linearly-with-squads.md) | high | Board payload/latency O(all squads ever): 11 MB / 7 s at 10k squads, reads collapse past ~1k |
| [STRESS-005](STRESS-005-truncate-checkpoint-stalls-writer.md) | high | `TRUNCATE` WAL checkpoint waits out `busy_timeout` behind board readers and stalls the writer ~5.5 s |
| [STRESS-006](STRESS-006-unbounded-queue-work-for-gone-clients.md) | high | Unbounded read queue keeps building responses for disconnected/timed-out clients; herd and abandoned requests stall the daemon for minutes |
| [STRESS-007](STRESS-007-write-ceiling-and-blocking-work-under-store-lock.md) | medium | Writes cap at ~70 req/s on one core; submit reads config files under the store lock |
| [STRESS-008](STRESS-008-acked-submit-fails-after-crash.md) | medium | A 201-acknowledged submit caught mid-materialization by a crash comes back `failed` |
| [STRESS-009](STRESS-009-protocol-edge-cases.md) | low | Protocol hygiene from the malformed corpus (silent closes, no header/URL/body limits, zero-cell task accepted) |

## Plan

### Fix order

1. **STRESS-001 and STRESS-002.** One request can take the daemon down. Both
   fixes are cheap and their UX risk is low if done as described.
2. **STRESS-003 and STRESS-006 together.** They share the board snapshot
   cache and an admission-controlled read queue. This step raises read
   throughput the most, and it carries the second-highest UX risk.
3. **STRESS-005.** No visible UX change.
4. **STRESS-004.** This changes how the board behaves and needs UI work
   alongside it, so do it after the cache from step 2 has bought headroom.
5. **STRESS-007, then STRESS-008, then STRESS-009.**

After each step, re-run
`bash scripts/stress.sh --scenarios ramp,faults` (add `volume` for 003/004),
and replace the estimates below with measured numbers.

### Expected gains

These are reasoned from the measurements, not measured.

| Fix | Rough effect |
|---|---|
| 001, 002, 006 | Throughput unchanged. The one-request crash and freeze, the 9–12 s stalls and the slow recovery after a spike all go away. |
| 003 | Reads go from about 20 to roughly 200–1,000 req/s. Today the daemon uses at most about 4 of 56 cores, and a lone board build takes 56–90 ms against 1.5–2.2 s under load. |
| 007 | Writes go from about 70 to about 150/s. The August 2026 build already did about 155/s on this machine. |
| Mixed 80/20, all of the above | About 300–700 req/s (20k–40k req/min), up from about 1,250 req/min. That is several hundred to about 1,000 board users clicking once a second, with p99 under 1 s. |

Confidence varies by item:

- **High** for the outage fixes. Their causes are confirmed in the code.
- **Medium** for the read multiplier. The pool size and query fan-out are
  confirmed, but the extra slowdown under concurrency is still inferred.

### UX risk

These are the ways each fix could make the board less responsive, more
brittle or more confusing. Each ticket's "UX risk" section has the details
and how to avoid them.

| Fix | UX risk | Main hazard |
|---|---|---|
| STRESS-004: paginate the board | **High** | The board assumes it holds every squad: search, filters, counts, bulk select, hovercards and deep links would all quietly cover only the loaded page. |
| STRESS-003: snapshot cache, query rewrite | **Medium** | A stale board when a change doesn't invalidate the cache (in-memory state, time-derived fields), or a field silently changing in the rewrite. |
| STRESS-006: bounded queue with fast 503s | **Medium** | Visible "busy" failures instead of slow success; retry storms from tabs without backoff; dropped writes that a client is still waiting on. |
| STRESS-002: read timeout, auth before body | **Medium if done naively** | A socket-level timeout would also kill SSE and long-poll streams, silently stopping live updates; slow uploads could get 408s. |
| STRESS-008: resume materialization | Low–medium | Squads that showed as failed come back after a restart; duplicate branches if a phase is not idempotent. |
| STRESS-007: config caching, write batching | Low | A config edit is not picked up immediately; a few extra ms per click. |
| STRESS-001: body cap | Low | A huge but legitimate task file is rejected. |
| STRESS-009: protocol limits | Low | Long URLs or zero-cell task files that used to work are refused. |
| STRESS-005: checkpoint mode | ~None | The WAL file grows larger between quiet periods. |

### How the board consumes the API today

The UX risks above follow from these facts about the board's fetch layer
(`librarian/assets/board/`):

- **Refreshes are push-driven.** The board refreshes on SSE events, one per
  Cartographer write, coalesced in `70-sse.js:565`, with a 60 s fallback
  poll.
- **Conditional GETs don't save server work.** Each refresh is a
  conditional GET with an ETag (`20-util.js:774`), but the daemon still
  builds the whole board to answer `304`.
- **Failures are treated as transient.** Any non-OK response becomes
  `{changed: true, data: null}`, which keeps the last render and retries on
  the next event (`20-util.js:781`). There is no backoff and no fetch
  timeout, so an overloaded daemon looks like stale data or a spinner today,
  not an error.
- **Mutations force the next refresh to refetch.** `post` and `del` drop
  every cached ETag and cancel the in-flight `/api/tasks` request
  (`20-util.js:823-845`), so the refresh after a click refetches.
- **The board response is not per-user.** `GET /api/tasks` depends only on
  the query string (`daemon/src/server.rs:1095`).
- **Board UI assumes data is already loaded.** Hovercards and other derived
  UI render only from data already in memory, by design ("Lazy by default"
  in `librarian/AGENTS.md`).
