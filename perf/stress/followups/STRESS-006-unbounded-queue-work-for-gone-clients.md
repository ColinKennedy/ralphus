# STRESS-006: unbounded request queue keeps building responses for clients that already left

- **Severity:** high. Cheap requests can push the daemon into minutes of
  backlog, and it does not recover on its own.
- **Found by:** `ralphus-stress` faults `abandon_response` and
  `thundering_herd`; the post-ramp health checks
- **Found at:** `035ef1e70` (staging, 2026-10-18); `cbd049ec5` behaves the
  same
- **Status:** fixed. `stress-fixes-medium` (PR #780) bounded the read
  queue, so `thundering_herd` sheds load with fast 503s.
  `stress-fixes-followups` skips queued requests whose client has hung up,
  including `Connection: close` ones (tiny_http patch 5). It also gives
  writes their own queue bound, with the refusal made before the write runs.
  `abandon_response` went from 17–25% victim errors at a 5–9 s p99 to 1.7%
  at 679 ms. The few errors left in both faults are fast 503s.

## Evidence

| Scenario | What happened |
|---|---|
| `abandon_response`: 4 clients send a full `GET /api/tasks` or submit and close without reading, 6,000 connections | Victim load had **45% errors**, and **nothing completed for 11.7 s**. In the 3 s after the fault: **100% errors**. `/api/daemon` did not answer within 10 s. |
| `thundering_herd`: 2,000 new connections, one board GET each, at once | Only 149 of 2,000 got an answer within 30 s. Victim load had 37% errors and an 8.6 s stall. After the fault: 84% errors, health unanswered. |
| End of the `mixed_80_20` and `browsing_users` ramps | `/api/daemon` did not answer within 10 s after load stopped (high finding `daemon_unhealthy`). The daemon was still working off requests whose clients had timed out. |
| Daemon request log, read_heavy ramp | `GET /api/tasks` queued **p99 18.8 s**; in mixed_80_20, `GET /api/projects` queued p99 26.8 s. The generator's timeout is 10 s, so this was work for clients that had already gone. |

## Root cause

- **The queue is unbounded.** The accept loop (`run_http_loop`,
  `daemon/src/server.rs:21067`) pushes every GET onto the `ReadPool`
  channel (`ReadPool`, `server.rs:20576`; workers `server.rs:20589-20600`).
  That channel has no bound and no admission control.
- **Nothing is ever cancelled.** A queued job has no deadline and nothing
  checks whether the client is still connected, so it runs to completion
  even if the client gave up long ago.
- **Combined with the cost of a board build, small inputs become minutes of
  work.** One board build costs 1–2 s under load (STRESS-003). A burst of N
  cheap board GETs, even ones whose sockets are already closed, therefore
  costs N/4 × 1.5 s of pool time.

## Fix direction

- **Bound the read queue.** When it is full, answer `503` with
  `Retry-After` right away. A fast 503 is better than a 20 s queue followed
  by a dead socket.
- **Stamp each job with `accepted_at`** (already carried in `PendingRequest`,
  `server.rs:21208`) and drop jobs older than a deadline (for example 10 s)
  before running the handler.
- **Optionally peek the socket for EOF before building an expensive
  response.** tiny_http doesn't expose the socket, so this may need a
  different server layer (see STRESS-002).
- **STRESS-003's snapshot cache would collapse a herd into a single build.**

## UX risk: medium

The trade-off is visible, fast failures instead of slow success. Under
overload users see "busy" instead of a 10–20 s spinner, and recovery is
immediate.

- **Board reads cope already.** A non-OK GET keeps the last render and
  retries on the next refresh (`librarian/assets/board/20-util.js:781`). But
  every open tab retries on every SSE event with **no backoff**, so a
  saturated daemon gets a retry storm from N tabs. To avoid it:
  - add jittered exponential backoff on `503`, honoring `Retry-After`;
  - show a "daemon busy, retrying" banner instead of silently keeping stale
    data.
- **Writes are the real hazard.** If the write queue is bounded, or queued
  writes are dropped once old, a user's Cancel, Submit or Merge fails
  visibly. Worse, a queued write that a client is still waiting on can be
  dropped: the CLI and MCP have timeouts longer than 10 s, and large
  submits take a while. To avoid it:
  - **Apply the bound and the age-based drop to GETs only.** Writes keep
    queueing, or get a much larger bound.
  - If writes ever get a bound, it must be checked **before** the write
    runs, never after, so a client never sees a 503 for something that
    happened. The client should show "daemon busy, not applied — retry".
- **The deadline must not undercut legitimate slow reads.** Big-history
  boards take seconds today (STRESS-004). Pick the deadline from the
  client's timeout, not from the ideal latency, until 003 and 004 land.
- **Health endpoints.** Exempt `/api/daemon` (or give it its own lane) so
  liveness checks and the board's banner can tell "busy" from "down".

## Verify

Run `ralphus-stress run … --scenarios faults --faults abandon_response,thundering_herd`.
Both should pass: no stall over 2 s, and errors under 1% in the recovery
window.
