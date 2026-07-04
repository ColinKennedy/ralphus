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

### `GET /api/tasks`
The full board state the librarian polls (every ~2s). Returns all runs with
their tasks, sessions, and verify steps, plus daemon status.

```json
{
  "daemon": { "running": 1, "max_concurrent": 4 },
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
          "sessions": [ { "id": "session-0", "cwd": "/repo", "agent": "claude", "model": null, "state": "done", "tokens_in": 0, "tokens_out": 0, "cost_usd": 0.0 } ],
          "verify":   [ { "id": "fmt", "kind": "command", "state": "pending" } ]
        }
      ]
    }
  ]
}
```

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
