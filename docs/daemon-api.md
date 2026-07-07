# ralphus daemon HTTP/JSON API

The daemon is the **only** process that touches the database. The CLI and the
librarian are both clients of this API — there is no shared-file access. The API
is plain HTTP + JSON over localhost (default `http://127.0.0.1:7890`), chosen so
a Python CLI and a browser frontend can both speak it, and so it is trivially
network-exposable when multi-user support arrives.

> Status: **contract draft** for Phase 0. Endpoints are implemented across
> Phase 1 (see `PLAN.local.md`). This document is the source of truth for the
> request/response shapes; implementation must match it.

## Conventions

- All request and response bodies are JSON (`Content-Type: application/json`).
- Timestamps are Unix epoch milliseconds (integer, e.g. `created_at_ms`). This
  avoids a date-formatting dependency in the MVP; a switch to RFC 3339 strings is
  a possible later refinement.
- Errors use a consistent envelope and a non-2xx status:

```json
{ "error": { "code": "validation_failed", "message": "human readable summary", "details": [ ] } }
```

- `code` is a stable machine string; `message` is for humans; `details` is an
  optional array (e.g. per-line validation errors).

## Run lifecycle

A *run* is one submitted TOML batch. Its state machine:

```
Queued ─activate─▶ Pending ─(deps met, scheduler picks up)─▶ Running ─▶ Done
                      │                                          │
                      └──────────────── cancel ─────────────────┴─▶ Cancelled
                                                                   └─▶ Failed
```

**Decision (fixes an old-project bug):** `POST /api/runs` submits directly to
**`Pending`** (schedulable immediately), not `Queued`. In the old project,
submissions landed as `Queued` and silently never ran until a human clicked
"activate" in the UI. `Queued` remains available as an explicit "hold" state for
callers that want to stage a run without scheduling it (`?hold=true`), activated
later via `POST /api/runs/{id}/activate`.

## Endpoints

### `GET /api/daemon`
Health/version probe. Never requires the DB to be writable.

```json
{ "name": "ralphus-daemon", "version": "0.1.0", "status": "ok", "db": "ok" }
```

### `POST /api/runs/validate`
Validate a TOML batch without persisting anything.

Request:
```json
{ "toml": "<raw TOML text>" }
```
Response (200 even when invalid — validity is in the body):
```json
{
  "valid": false,
  "errors":   [ { "line": 12, "kind": "unknown_key", "message": "unknown key \"budgt_usd\"" } ],
  "warnings": [ { "line": 3,  "kind": "forward_ref", "message": "..." } ]
}
```

### `POST /api/runs`
Validate (fail-closed) and submit a TOML batch.

Request:
```json
{ "toml": "<raw TOML text>", "hold": false, "label": "optional human label" }
```
`400` with the error envelope (`code: "validation_failed"`, `details` = the
validation errors) if invalid. On success `201`:
```json
{ "run_id": "run-000000000001", "state": "pending" }
```

### `POST /api/clear`
Bulk-delete tasks and reviews (RAL-13).

Request (all fields optional):
```json
{ "states": ["done", "failed"], "keep_temporary": false }
```
With no `states` filter, wipes everything — all runs (and their
sessions/tasks/verifies/events) and all guardians (and their branches) — and
resets the id sequences so ids restart at 1. A non-empty `states` list deletes
only runs in those states (with their children) and leaves guardians and the id
sequences untouched. An unknown status is a `400`. Unless `keep_temporary` is
true, on-disk review worktrees for deleted guardians are purged. Response `200`:
```json
{ "runs_deleted": 3, "guardians_deleted": 1, "worktrees_purged": 1 }
```

### `POST /api/runs/{id}/restart`
Restart a whole run and cascade dirtiness downstream (RAL-19): the run and all
its nodes reset to Pending, and every run that transitively depends on it is
also reset to Pending so it re-runs once this run finishes again (cross-run
gating holds each dependent until its upstreams are Done). Response `200`:
```json
{ "state": "pending", "dirtied": ["run-000000000002", "run-000000000003"] }
```

### `POST /api/runs/{id}/sessions/{task_idx}/{session_idx}/restart`
Restart a single session: it and every session downstream of it within the run
reset to Pending (upstream sessions stay Done and are skipped on re-run), the
run goes back to Pending, and dependent runs are dirtied. Same response shape as
above. A non-integer index is a `400`.

### `POST /api/guardians/{id}/branches/reorder`
Persist a new branch order for a review (RAL-6/RAL-14). Body is the full ordered
list of branch names:
```json
{ "order": ["feat-a", "feat-b", "feat-c"] }
```
`order` must be a permutation of the review's current branches; names that no
longer exist are ignored, and any existing branch not named keeps its relative
position after the reordered ones. This only rewrites positions — it does not
rebase. The librarian's Save follows it with `POST /api/guardians/{id}/merge` to
re-stack the affected branches in the new order. Returns `200` with the updated
guardian view; a malformed body is a `400`.

### `GET /api/guardians/{id}/messages`
The review's global feedback thread (RAL-22), oldest first:
```json
{ "messages": [ { "role": "reviewer", "text": "fix the naming", "at_ms": 0 } ] }
```
`role` is `reviewer` (human) or `guardian` (triage agent).

### `POST /api/guardians/{id}/chat`
Post a reviewer message to the global feedback thread. Body `{ "text": "..." }`
(empty is a `400`). The message is persisted immediately; the triage agent then
replies in the background — it works in the combined (all-branches-rebased)
review worktree, decides which branch(es) each request applies to, and appends
its reply to the thread. Returns `202 {"status":"triaging"}`.

### `GET /api/tasks`
The full board state the librarian polls (every ~2s). Returns all runs with
their tasks, sessions, and verify steps, plus daemon status.

```json
{
  "daemon": { "running": 1, "max_concurrent": 12 },
  "runs": [
    {
      "id": "run-000000000001",
      "label": null,
      "state": "running",
      "created_at_ms": 1783120106867,
      "tasks": [
        {
          "name": "build",
          "project": "myrepo",
          "state": "running",
          "sessions": [ { "id": "session-0", "cwd": "/repo", "agent": "claude", "model": null, "state": "done", "tokens_in": 0, "tokens_out": 0, "cost_usd": 0.0, "verify": [ { "id": "fmt", "kind": "command", "state": "done", "output": null, "spec": "cargo fmt --check", "model": null } ] } ],
          "verify":   [ { "id": "tests", "kind": "command", "state": "pending", "output": null, "spec": "cargo test", "model": null } ]
        }
      ]
    }
  ]
}
```
Both *task*-level verify steps (`[[task.verify]]`, on `TaskView.verify`) and
*session*-level verify steps (`[[task.session.verify]]`, on `SessionView.verify`)
are exposed here, each in task/session declaration order. A verify entry carries:

- `id` — optional step id from TOML (e.g. `"fmt"`).
- `kind` — one of `command` / `prompt` / `brain` / `approval`.
- `state` — lifecycle state (`pending` / `running` / `done` / `failed` / `cancelled`).
- `output` — captured output once run; `null` before execution.
- `spec` — the step definition: command text for `command` kind, prompt text for
  `prompt` / `brain` kind, or empty string for `approval`.
- `model` — model override for `prompt`-kind steps; `null` when unset.

`command` and `prompt` verify steps actually run (`pending` → `running` →
`done`/`failed`); `brain`/`approval` steps are accepted but deferred and stay
`pending` forever. A `prompt` step's `output` is the AI's final response
text (used to derive its pass/fail verdict), not command stdout.

### `GET /api/resources`
Per-task OS resource usage for the board's Resources tab (RAL-11). One entry per
*running* session that currently has a live `ralphus-runner` subprocess, with its
CPU/RAM/GPU sampled and mapped back to the exact run/task/session. Sampling briefly
blocks (~200ms) to compute a CPU delta, so this is polled only while the Resources
tab is open.

```json
{
  "resources": [
    {
      "run_id": "run-000000000001",
      "run_label": null,
      "task_idx": 0,
      "task_name": "build",
      "session_idx": 0,
      "session_id": "session-0",
      "pid": 48213,
      "cpu_percent": 12.5,
      "mem_bytes": 104857600,
      "gpu_mem_bytes": null
    }
  ]
}
```

`cpu_percent` is a percentage of one core (can exceed 100 for multi-threaded work).
`mem_bytes` is resident memory. `gpu_mem_bytes` is best-effort via `nvidia-smi` and
is `null` ("N/A" in the UI) whenever GPU metrics are unavailable (no `nvidia-smi`,
no NVIDIA GPU, or nothing attributed to that PID) — `ralphus check health` warns
when `nvidia-smi` is missing. CPU/RAM sampling uses no extra crates (`/proc` on
Linux, PowerShell `Get-Process` on Windows); unsupported platforms report `null`.
`task_idx`/`session_idx` are the board's navigation indices (the "Go to task"
button jumps straight to that session).

### `GET /api/runs/{id}`
A single run's full detail (same shape as one element of `runs` above, plus the
resolved definition fields shown in the details pane).

### `GET /api/runs/{id}/logs`
The event timeline for a run: an ordered list of `{ ts, level, source, message }`
plus per-session and per-verifier log references (drives the Logs modal tabs).

### `POST /api/runs/{id}/activate`
Move a held `Queued` run to `Pending`. Returns the new state.

### `POST /api/runs/{id}/cancel`
Cancel a `Queued`/`Pending`/`Running` run. Best-effort termination of any live
session; the run transitions to `Cancelled`.

## Notes on future evolution

- Auth/identity and per-user attribution are **not** in this draft; when
  multi-user hardening begins, an `Authorization` header + a `submitted_by`
  field on runs are the expected additions (see `FOLLOW.local.md` #3).
- Live updates are poll-based for now (librarian polls `GET /api/tasks`); an SSE
  or WebSocket channel is a later optimization.
