# STRESS-007: writes cap at about 70 req/s on one core; submit reads config files under the store lock

- **Severity:** medium. The ceiling is acceptable today but is the next
  limit once reads are fixed. The lock holds add tail latency for everyone.
- **Found by:** `ralphus-stress` ramp `write_heavy`; the long-lock-hold table
- **Found at:** `035ef1e70` (staging, 2026-10-18)
- **Status:** fixed.
  - `stress-fixes-low` (PR #778) took config reads and `.ralphus.toml`
    discovery off the lock: the write peak went from 78 to 165 ok/s.
  - `stress-fixes-followups` cut the rest of the per-submit lock churn:
    - one store lock for all of submit's registry checks instead of about
      eight;
    - no preset-registry load for a file that extends no preset;
    - one commit per materialization progress update;
    - validation answered on the read pool.

    The write peak is 192 ok/s, and the 1%-error point stays at 2048 users.
  - Write batching was considered and not done; see "Fix direction".

## Numbers

`write_heavy` mix: submit 50, env set 25, label edit 20, validate 5.

| Users | ok req/s | p50 | p99 | Daemon CPU |
|---:|---:|---:|---:|---:|
| 1 | 66 | 16 ms | 34 ms | 96% |
| 16 | 71 | 216 ms | 449 ms | 100% |
| 128 | 69 | 1.8 s | 2.2 s | 105% |
| 512 | 69 | 6.8 s | 7.7 s | 103% |
| 1024 | 73 | 4.9 s | 9.9 s | 115%; **58% errors** (timeouts) |

- **Throughput is flat from 1 user up.** Every non-GET goes through one
  write worker (`WRITE_WORKERS = 1`, `daemon/src/server.rs:20524`), and the
  daemon sits at about 100% of a single core. Queue time is the whole
  latency: `POST /api/squads` handler p50 is 20 ms, but queued p99 is 14.4 s.
- **Long store-lock holds in submit** were seen on every fault run's seeding:

  | Site | Max hold |
  |---|---:|
  | `server.rs:7034` (`insert_squad`) | 1.2–1.6 s |
  | `server.rs:6911` | 511 ms |
  | `server.rs:6940` | 509 ms |
  | `server.rs:7510` | 598 ms |
  | `server.rs:9725` | 982 ms |
  | `server.rs:9198` | 663 ms |

## Root cause, from reading the code

| Site | Work done under the store guard |
|---|---|
| `server.rs:6911` | `validate_task_file_profiles` → `resolve_agent_for_path_with` → `load_profiles_for_path_with` (`agent_profiles.rs:403`). It reads `.ralphus.toml` and config files from disk, although the code comment says "DB lookups only". |
| `server.rs:6940` | `apply_profile_model_defaults` → `resolve_agent_for_path_db`, with the same config-file reads. |
| `server.rs:7034` | `insert_squad` (`store.rs:4877-5187`). Inside the open transaction, each task runs `load_worktree_config` and `load_daemon_config()` (`store.rs:4969`, `4975`), which means file reads and TOML parsing. |
| `server.rs:7510` | `enroll_new_squad_in_open_waypoints` (`waypoints.rs:1694`). It computes the squad's subproject and project scope *before* checking whether any waypoint is open. |
| `scheduler_in_downtime()` | Reloads config from disk on every request (`config.rs:1776-1777` → `3244-3262`). |

`server.rs:9198` and `9725` were not examined in this pass.

## Fix direction

- Resolve profile, worktree and daemon config before taking the lock, or
  cache them keyed on file mtime.
- Return early from waypoint enrollment when no waypoint is open.
- Cache the downtime config.
- Longer term:
  - batch writes, so one worker commits several requests per transaction;
  - narrow what needs the global writer lock;
  - or split writes by squad.

**Why write batching was not done.** A write handler takes the store lock
several times per request, and background threads write through the same
connection between those takes. There are two ways to give the batch one
transaction:

- **Keep it open across the handlers.** It would then absorb the background
  threads' writes, and a failed request's savepoint rollback would undo them
  too.
- **Hold the lock for the whole batch.** The handlers' own lock calls would
  then re-enter it, which `crate::store_lock` forbids (a re-entrant take
  parks forever).

Writes are serialized by design ("mutations stay totally ordered",
`WRITE_WORKERS`), and the measured cost is CPU on that one writer, not
fsync: `synchronous=NORMAL` already skips it. The ceiling that remains is
that design choice, now at about 190 ok/s.

## UX risk: low

- **Config changes may lag.** If profile, worktree and daemon config are
  cached, an edit to `.ralphus.toml` or an agent profile might not apply to
  the next submit. Invalidate on file modification time (cheap `stat`
  calls) rather than a TTL, so edits apply immediately.
- **The config can be read before the lock.** Resolving config before
  taking the lock can see config that changes before the insert. That is
  harmless, since the same race exists today between the user editing a
  file and submitting.
- **Batching adds a little latency and needs isolation.** Each click waits
  for its batch window. Keep the window at a few ms or less, so it does not
  show. One invalid write in a batch must fail only itself: use per-item
  savepoints, not one transaction that rolls back everyone's work.

## Verify

The `write_heavy` peak ok req/s should rise. Re-run with
`--scenarios ramp --profiles write_heavy`.
