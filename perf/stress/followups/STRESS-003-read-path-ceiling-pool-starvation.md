# STRESS-003: reads top out at ~20 req/s: board builds starve the 4-connection read pool

- **Severity:** high. This sets the daemon's overall throughput ceiling.
- **Found by:** `ralphus-stress` ramps `read_heavy`, `mixed_80_20` and
  `browsing_users`
- **Found at:** `035ef1e70` (staging, 2026-10-18); `cbd049ec5` shows the
  same numbers
- **Status:** open

## Numbers

Release build; 56-CPU Xeon E5-2697 v3; Windows 10; about 200 held squads;
5 projects; generator on the same host.

| Profile | Peak ok req/s | Max users ≤1% errors | p99 at that load | First users >1% errors |
|---|---:|---:|---:|---:|
| read_heavy | 21 | 64 | 7.2 s | 128 (29%) |
| mixed_80_20 | 21 | 64 | 7.8 s | 128 (46%) |
| browsing_users (1 s think time) | 22 | 100 | 7.3 s | 250 (53%) |

- Throughput is flat from 2 to 256 users while daemon CPU sits at about
  370–400%. Errors start only once latency reaches the generator's 10 s
  timeout.
- From the daemon's own request log (read_heavy ramp): `GET /api/tasks`
  handler p50 is 1512 ms and its queue wait reaches p99 18.8 s.
  `GET /api/daemon`, a near-trivial health route, shows a 1221 ms handler
  p50. `lock_wait` is 0 everywhere.
- A lone, sequential `GET /api/tasks` at the same data size takes about
  56–90 ms.

## Root cause

1. **The pool is starved.** `store_pool.rs:100` sets `POOL_SIZE = 4`, but
   12 read workers (`READ_WORKERS`, `server.rs:20505`) compete for those 4
   connections. These routes all hold a pooled connection for their whole
   build:

   | Route | Where |
   |---|---|
   | `/api/tasks` | `read_board_snapshot`, `server.rs:395-401`; route `server.rs:2570` |
   | `/api/task-index` | `server.rs:2613` |
   | `/api/queue` | `server.rs:14174` |
   | `/api/projects` | `server.rs:4779` |
   | `/api/guardians` | `server.rs:15005` |
   | `/api/daemon` | `read_running_count`, `server.rs:364-369` |

   `/api/daemon` (`health()`, `server.rs:2433-2462`) needs one
   `SELECT COUNT(*)`, so its ~1.2 s is time spent waiting for a free
   connection. `GET /api/squads/{id}` uses the writer lock instead of the
   pool, and it is the only read that stayed fast (13–200 ms).
2. **The pool wait is invisible.** `StorePool::acquire()`
   (`store_pool.rs:195-209`) blocks on a condvar with no timeout and no
   timing. `lock_wait` measures only the global `StoreMutex`
   (`store_lock.rs:134-138`), and `queued` stops when a worker picks the
   job up (`server.rs:20646`). Pool wait therefore shows up as "handler"
   time, which is why every log line said `lock_wait 0ms`.
3. **Each board build is N+1.** `list_squads_conn_limited`
   (`store.rs:6222-6289`) calls `build_squad_view` per squad, roughly 12–13
   statements each.
   - All of them use plain `conn.prepare`; there is no `prepare_cached` in
     the crate.
   - Agent profiles are re-read for every squad (`store.rs:6389`).
   - Prompt text is fully hydrated and then thrown away by
     `strip_prompt_text` (`server.rs:2548`).
   - `/api/task-index` builds the whole `SquadView` before compacting it
     (`server.rs:2637`).
   - At 200 squads that is about 2,500 statement compiles per request.
4. **Inferred, not verified:** under concurrency each build gets about 4×
   more expensive in CPU. Candidates are SQLite's global memory-status
   mutex (the bundled `libsqlite3-sys` build does not set
   `SQLITE_DEFAULT_MEMSTATUS=0`), allocator contention, and WAL growth while
   long readers pin snapshots. Profile one worker at 8 users to confirm.

## Fix direction, by expected payoff

1. Cache the board snapshot. Concurrent `/api/tasks` and `/api/task-index`
   requests should share one build, keyed on a writer generation counter or
   a short TTL. The ETag is already a hash of the body (`server.rs:20430`).
2. Rewrite the board read as about 10 whole-table queries grouped in memory.
   Use `prepare_cached`, load profiles once per list, give task-index a lean
   query, and stop selecting prompt text.
3. Take `/api/daemon` off the pool (it shouldn't depend on read capacity),
   and size `POOL_SIZE` to match `READ_WORKERS`.
4. Record pool-acquire wait time in the request log and in `/api/daemon`.

## Verify

Re-run `bash scripts/stress.sh --scenarios ramp,volume`. The read_heavy peak
ok req/s and its "max users ≤1% errors" should move, and `GET /api/daemon`
should stay in single-digit milliseconds under load.
