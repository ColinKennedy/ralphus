# STRESS-004: board payload and latency grow linearly with history; reads collapse at about 1,000 squads

- **Severity:** high. A long-lived instance becomes unusable as squads
  accumulate.
- **Found by:** `ralphus-stress` scenario `volume`
- **Found at:** `035ef1e70` (staging, 2026-10-18)
- **Status:** open

## Numbers

32 closed-loop users per window; release build; 56-CPU host.

| Squads | Projects | `GET /api/tasks` size | Single board GET | Read ok req/s (errors) | Mixed ok req/s (errors) | Daemon RSS |
|---:|---:|---:|---:|---:|---:|---:|
| 133 | 5 | 110 KB | 88 ms | 33 (0%) | 32 (0%) | 35 MB |
| 1,012 | 25 | 1.1 MB | 753 ms | 4 (35%) | 5 (26%) | 114 MB |
| 5,004 | 100 | 5.5 MB | 3.5 s | ~0 (94%) | 1 (76%) | 477 MB |
| 10,007 | 250 | 11 MB | 7.2 s | 1 (73%) | 1 (78%) | 662 MB |

During this scenario the watchdog logged
`store lock UNREACHABLE for …ms`; that is covered by STRESS-005.

## Root cause

- **There is no limit or pagination.** `GET /api/tasks` returns every squad
  ever submitted. `list_squads_conn_limited` (`store.rs:6222-6289`) runs
  with no limit, and each squad costs about 12–13 statements (see
  STRESS-003). Payload size and latency are therefore O(total squads),
  including finished or long-dead ones.
- **`/api/task-index` and `/api/queue` scale the same way.**
  `server.rs:2613`/`2637`; `store.rs:13333-13392`.

## Fix direction

- Paginate or window the board API: by default, active plus the last N
  finished squads. The librarian then asks for more on scroll.
- Keep a summary table or materialized view per squad so the board does not
  re-hydrate every cell and proof.
- Archive old terminal squads out of the hot query.
- Together with STRESS-003's snapshot cache, this should make board cost
  independent of history size.

## Verify

Run `ralphus-stress run … --scenarios volume --volume 1000:25,10000:250`.
The 10,000-squad row's single GET and read ok req/s should be within a small
factor of the 1,000-squad row.
