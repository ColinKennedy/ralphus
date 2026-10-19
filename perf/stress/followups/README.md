# Stress-test follow-ups

Issues found by `ralphus-stress` (see [`../README.md`](../README.md)). Each file
records what was measured, root cause with `file:line` citations (marked
confirmed vs inferred), a fix direction, and how to verify the fix with the
harness. First found on `035ef1e70` (staging, 2026-10-18); see
[`../HISTORY.md`](../HISTORY.md) for whether older commits share them.

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

Suggested order: 001 and 002 first (one-request outages), then 003 + 006
together (they share the board snapshot cache and an admission-controlled
read queue), then 005, 004, 007, 008, 009.
