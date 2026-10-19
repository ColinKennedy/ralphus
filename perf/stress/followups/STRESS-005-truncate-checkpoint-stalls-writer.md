# STRESS-005: TRUNCATE WAL checkpoint blocks the writer for about 5.5 s while board reads run

- **Severity:** high. Every write stalls for `busy_timeout` and the
  watchdog reports the store lock UNREACHABLE.
- **Found by:** `ralphus-stress` scenarios `volume` and `abandon_response`;
  the long-lock-hold table
- **Found at:** `035ef1e70` (staging, 2026-10-18)
- **Status:** open

## Evidence

Long store-lock holds across the whole HEAD run:

| Site | Count | Max |
|---|---:|---:|
| `daemon/src/server.rs:7344` (`run_submit_followup`'s `checkpoint` closure, one Cartographer insert) | 49 | 5551 ms |
| `daemon/src/server.rs:7198` (`report_materialization_phase`, one UPDATE plus one insert) | 22 | 5611 ms |

Both are trivial statements. The volume sandbox's `daemon.log` shows the
cause on consecutive lines:

```
01:55:56 ralphus [store] slow wal checkpoint mode=TRUNCATE took 5494ms
01:55:56 ralphus [store_lock] store guard held for 5493ms (acquired at daemon\src\server.rs:7198) -- blocking work ran under the store lock
01:55:56 ralphus [store_lock] waited 5494ms for the store lock; previously acquired at daemon\src\server.rs:7198 (5494ms ago)
01:55:56 ralphus [store_lock] waited 4750ms for the store lock; previously acquired at daemon\src\server.rs:6911 (4ms ago)
```

The watchdog also fired:
`ralphus [watchdog] store lock UNREACHABLE for …ms (… consecutive failed check(s))`.

## Root cause

- **The checkpoint escalates.** The checkpoint thread
  (`spawn_wal_checkpoint_thread`, `daemon/src/store.rs:1563`) runs `PASSIVE`
  normally but switches to `TRUNCATE` once the `-wal` file passes a size
  threshold (`WalCheckpointMode::for_wal_size`, around `store.rs:1622`).
- **TRUNCATE waits for readers.** A `TRUNCATE` checkpoint must wait for
  every reader to leave the WAL. It takes SQLite's write lock and then calls
  the busy handler for up to `store_pool::BUSY_TIMEOUT` (5 s,
  `store_pool.rs:35`).
- **The board reads keep it waiting.** Board reads run back to back for
  1–7 s on all 4 pooled connections (STRESS-003/004), so the checkpoint
  waits the full 5 s. During that time the writer connection, which holds
  the global `StoreMutex`, cannot commit. Every write and every
  `store.lock()` caller queues behind it.
- **The checkpoint is on its own connection, but that doesn't help.** The
  file comment above `spawn_wal_checkpoint_thread` explains it was moved
  off the writer's path. A blocking checkpoint mode still blocks the writer
  through SQLite's own write lock.

## Fix direction

- **Never escalate to a blocking mode while readers are active.** For
  example, retry `PASSIVE` and allow the WAL to grow, or give the
  checkpoint connection a short `busy_timeout` (tens of ms) so `TRUNCATE`
  gives up quickly instead of holding the write lock for 5 s.
- **Or use `journal_size_limit`** and accept a larger WAL between quiet
  periods.
- **Fixing STRESS-003/004 also shrinks the window.** Shorter board read
  transactions mean shorter waits.

## Regression test

The setup is a file-backed store with a long-running reader on the pool and
a WAL above the TRUNCATE threshold. Assert that a write through the store
lock completes in well under `BUSY_TIMEOUT`. `daemon/tests/board_contention.rs`
already has the writer-versus-reader HTTP harness.
