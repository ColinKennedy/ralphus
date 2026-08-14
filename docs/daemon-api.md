# ralphus daemon HTTP/JSON API

The daemon is the **only** process that touches the database. The CLI and the
librarian are both clients of this API — there is no shared-file access. The API
is plain HTTP + JSON over localhost (default `http://127.0.0.1:7890`), chosen so
a Python CLI and a browser frontend can both speak it, and so it is trivially
network-exposable when multi-user support arrives.

> Status: **live reference**. All ~50 routes the daemon serves are listed in
> the [endpoint index](#endpoint-index) below; the sections that follow it
> document the ones worth explaining in depth (non-obvious request/response
> shapes, side effects, or gotchas) rather than repeating every route's full
> JSON schema. `daemon/src/server.rs::route()` is the ground truth for exact
> shapes if a route isn't detailed here. See also `docs/cli-reference.md` for
> the `ralphus` CLI surface over this API.

## Endpoint index

All routes, grouped by resource. `#` links jump to a detailed section below
where one exists.

**Daemon / board**
| Method | Path | What |
|---|---|---|
| GET | `/api/daemon` | [Health/version probe](#get-apidaemon) |
| POST | `/api/daemon/shutdown` | [Kill every spawned process and exit](#post-apidaemonshutdown) (`ralphus-daemon stop`) |
| GET | `/api/tasks` | [Board state](#get-apitasks); `?status=&name=&sort=` filter/sort |
| GET | `/api/resources` | [Per-task CPU/RAM/GPU](#get-apiresources) |
| GET | `/api/cartographer` | [Structured event log](#get-apicartographer), filtered/paginated |
| GET | `/api/cartographer/{id}` | [One event's full detail](#get-apicartographerid) |
| GET | `/api/graph` | [Cross-run gating graph](#get-apigraph); `?all=1` includes terminal runs |
| GET | `/api/ghosts/{owner_uri}` | [Fetch a ghost](#get-apighostsowner_uri) by its owning session/review URI |
| POST | `/api/ghosts/copy` | [Copy a ghost](#post-apighostscopy) onto another owner, independent of the dependency graph |

**Runs**
| Method | Path | What |
|---|---|---|
| POST | `/api/runs/validate` | [Validate a TOML batch](#post-apirunsvalidate) without persisting |
| POST | `/api/runs` | [Submit a TOML batch](#post-apiruns) |
| POST | `/api/clear` | [Bulk-delete](#post-apiclear) tasks and reviews |
| GET | `/api/queue` | Ordered, classified queue of runnable work items |
| POST | `/api/queue/reorder` | Reorder the queue (dependency-repaired) |
| POST | `/api/queue/set-position` | Move item(s) to an absolute/relative position |
| GET | `/api/runs/{id}` | [One run's full detail](#get-apirunsid) |
| GET | `/api/runs/{id}/worktrees` | [Per-session worktree/project/upstream](#get-apirunsidworktrees) |
| GET | `/api/runs/{id}/logs` | [State-transition audit log](#get-apirunsidlogs) |
| GET | `/api/runs/{id}/graph` | [Internal session dependency graph](#get-apirunsidgraph) |
| POST | `/api/runs/{id}/activate` | [Queued → Pending](#post-apirunsidactivate) |
| POST | `/api/runs/{id}/cancel/preview` | [Dry-run preview](#post-apirunsidcancelpreview) of a cascading cancel's impact |
| POST | `/api/runs/{id}/cancel` | [Cancel the run](#post-apirunsidcancel), cascading to downstream dependents |
| POST | `/api/runs/{id}/set-status` | Manually override a run/task/session/verify's state — for a task/session/verify, any target state but `pending` first captures its running agent's tmux pane into that session's ghost and kills it (RAL-163) |
| POST | `/api/runs/{id}/edit` | Edit a run/task/session's fields; a run/task edit resets the whole run to Pending, a session edit resets only that session + its downstream |
| POST | `/api/runs/{id}/retry` | Re-run with existing parameters (reset to Pending) |
| POST | `/api/runs/{id}/restart/preview` | [Dry-run preview](#post-apirunsidrestartpreview) of a whole-run restart's downstream impact |
| POST | `/api/runs/{id}/restart` | [Restart the whole run](#post-apirunsidrestart), cascading dirtiness |
| POST | `/api/runs/{id}/add-dependency` | [Add a cross-run dependency](#post-apirunsidadd-dependency) post-submission |
| POST | `/api/runs/{id}/sessions/{ti}/{si}/restart/preview` | [Dry-run preview](#post-apirunsidsessionstask_idxsession_idxrestartpreview) of a session restart's downstream impact |
| POST | `/api/runs/{id}/sessions/{ti}/{si}/restart` | [Restart one session](#post-apirunsidsessionstask_idxsession_idxrestart) + its downstream |
| POST | `/api/runs/{id}/sessions/{ti}/{si}/verify/{vi}/restart` | Restart a session's verify steps from `vi` |
| POST | `/api/runs/{id}/tasks/{ti}/verify/{vi}/restart` | Restart a task's verify steps from `vi` |
| POST | `/api/runs/{id}/tasks/{ti}/restart/preview` | [Dry-run preview](#post-apirunsidtaskstirestartpreview) of a task restart's downstream impact |
| POST | `/api/runs/{id}/tasks/{ti}/restart` | [Restart a whole task](#post-apirunsidtaskstirestart) + its downstream (RAL-150) |
| POST | `/api/runs/{id}/env` | [Set/unset persistent environment-variable overrides](#post-apirunsidenv) on the run (RAL-150) |
| POST | `/api/runs/{id}/tasks/{ti}/env` | Set/unset env overrides on a task ([hierarchical env overrides](#hierarchical-env-overrides-tasksessionverify-layers)) |
| POST | `/api/runs/{id}/tasks/{ti}/verify/env` | Set/unset env overrides on a task's own verify steps |
| POST | `/api/runs/{id}/sessions/{ti}/{si}/env` | Set/unset env overrides on a session |
| POST | `/api/runs/{id}/sessions/{ti}/{si}/verify/env` | Set/unset env overrides on a session's own verify steps |
| POST | `/api/runs/{id}/tasks/{ti}/solo` | [Solo a task](#post-apirunsidtaskstisolo) (RAL-157) — pauses every other task in the run until un-soloed |
| POST | `/api/runs/{id}/tasks/{ti}/unsolo` | [Un-solo a task](#post-apirunsidtaskstiunsolo) (RAL-157) — resumes its paused siblings |
| POST | `/api/runs/{id}/sessions/{ti}/{si}/open-terminal` | Spawn a resume terminal (`claude --resume` or `codex exec resume`, depending on which agent the session ran under) **on the daemon host** (`?mode=readonly\|open`) |
| POST | `/api/runs/{id}/verifies/{ti}/{scope}/{si}/{vi}/open-terminal` | Same, for a verify step's resolved session |
| DELETE | `/api/runs/{id}` | Permanently delete a run |

**Guardians (reviews)**
| Method | Path | What |
|---|---|---|
| GET | `/api/guardians` | List all reviews (returns a **bare JSON array**, not `{guardians:[...]}`) |
| POST | `/api/guardians` | Create a review |
| GET | `/api/guardians/{id}` | [One review's full detail](#get-apiguardiansid) |
| GET | `/api/guardians/{id}/logs` | State-transition audit log (bare array) |
| POST | `/api/guardians/{id}/rename` | Rename |
| POST | `/api/guardians/{id}/settings` | Update opt-out settings (only present fields change) |
| POST | `/api/guardians/{id}/squash` | [Per-project commit squashing](#post-apiguardiansidsquash) |
| DELETE | `/api/guardians/{id}` | Delete + purge worktrees/branches |
| POST | `/api/guardians/{id}/branches` | Add a branch |
| POST | `/api/guardians/{id}/branches/reorder` | [Reorder branches](#post-apiguardiansidbranchesreorder) (does **not** rebase) |
| POST | `/api/guardians/{id}/branches/arrange` | [Reorder + rebase atomically](#post-apiguardiansidbranchesarrange) |
| POST | `/api/guardians/{id}/branches/{branch_id}/feedback` | Feedback on one branch → resolver re-attempt |
| GET | `/api/guardians/{id}/messages` | [Global feedback thread](#get-apiguardiansidmessages) |
| POST | `/api/guardians/{id}/chat` | [Post to the feedback thread](#post-apiguardiansidchat) |
| POST | `/api/guardians/{id}/chat/fork` | Fork the thread at a message seq |
| GET | `/api/guardians/{id}/base-branches` | Candidate base branches (same remote) |
| POST | `/api/guardians/{id}/base` | Change base branch + rebuild |
| POST | `/api/guardians/{id}/force_start` | Disable not-yet-done branches, merge immediately |
| POST | `/api/guardians/{id}/branches/{branch_id}/dismiss_reenable` | Dismiss the "can re-enable" notice |
| POST | `/api/guardians/{id}/branches/{branch_id}/move` | [Move a branch to another review](#post-apiguardiansidbranchesposmove) (RAL-118) |
| GET | `/api/guardians/{id}/branches/{branch_id}/conflicts` | [Live conflicting-files list](#get-apiguardiansidbranchesbranch_idconflicts) for the board's Reviews UI (RAL-148) |
| POST | `/api/guardians/{id}/branches/{branch_id}/open-terminal` | Spawn a resolver terminal **on the daemon host** |
| POST | `/api/guardians/{id}/manual-checks/open-terminal` | Spawn a manual-checks-generation terminal **on the daemon host** (`?mode=open\|agent`) |
| POST | `/api/guardians/{id}/merge` | Start/continue the stacked rebase |
| POST | `/api/guardians/{id}/cancel_and_merge` | Cancel an in-progress rebase, start fresh |
| POST | `/api/guardians/{id}/approve` | Approve an in_review guardian |
| POST | `/api/guardians/{id}/cancel` | Cancel a review |
| POST | `/api/guardians/{id}/run-manual-commands` | Spawn manual-check commands **on the daemon host** |
| POST | `/api/guardians/{id}/run-action-hint` | Spawn a `command`-kind action hint **on the daemon host**; `prompt`-kind is `501` |
| POST | `/api/guardians/{id}/resolve-input` | [Delegate a named check input to the resolver agent](#post-apiguardiansidresolve-input) ("set it for me", RAL-164) |

Four routes above are flagged "on the daemon host": they call
`spawn_in_terminal`/open a GUI terminal window (or, for the read-only
snapshot case below, an external viewer) on whatever machine runs
`ralphus-daemon`, which only makes sense when the daemon and the caller share
a desktop session. The CLI deliberately does **not** call these — see
`docs/cli-reference.md`'s `session terminal` / `review checks run` / `review
action run` for the headless equivalent (print the resolved command + cwd
instead).

**Structured checks (RAL-164).** `manual_commands` and `action_hints` on
`GuardianView` are both `GuardianCheck[]`, not bare strings —
`{label?, command?, prompt?, cleanup_command?, inputs?}`, where `inputs` is
`{name, message, default}[]` naming `{name}` placeholders referenced in
`command`/`cleanup_command`. `GuardianView` also carries `input_values`
(`Record<string,string>`, the last value used per input name on this review —
overrides an input's own literal `default`) and `input_resolutions`
(`Record<string,{status, value?}>`, `status` one of `resolving`/`ready`/`failed`,
tracking in-flight/completed `resolve-input` calls). `run-manual-commands` and
`run-action-hint` both accept an extended body: `{index?, inputs?:
Record<string,string>, run_cleanup?: boolean}` — `inputs` substitutes named
placeholders (submitted value wins, then `input_values`, then the input's own
default) and is persisted as the new `input_values` default; `run_cleanup`
opts into chaining the check's `cleanup_command` before the main command.

**Read-only terminal-log viewer (RAL-153).** When `open-terminal`'s target
tmux session has already ended, the daemon degrades to a read-only path
instead of attaching: it copies the session's persisted last-pane snapshot
(`crate::tmux::read_pane_snapshot`) to a disposable per-click temp file and
opens that copy with the user's preferred external viewer — `$VISUAL`, then
`$EDITOR`, then the OS's own default handler for the file type
(`start`/`open`/`xdg-open`) — rather than spawning a terminal window running
`Get-Content`. The copy lives under a dedicated OS-temp subdirectory (not
`pane_snapshots/` under `state_dir()`, which remains the durable record) and
is pruned after an hour; `409` if the session never ran under tmux or
produced no pane output.

**Pull requests (RAL-117)**
| Method | Path | What |
|---|---|---|
| POST | `/api/guardians/{id}/pull-requests` | [Submit PR(s)](#post-apiguardiansidpull-requests) for stacked/combined worktree(s) |
| GET | `/api/guardians/{id}/pull-requests` | List every PR submitted for a review (bare array) |
| GET | `/api/pull-requests` | [Find the PR row](#get-apipull-requests) for a forge PR/MR number; `?forge=&repo=&pr_number=` |
| GET | `/api/pull-requests/{pr_id}` | One PR row |
| POST | `/api/pull-requests/{pr_id}` | [Mutate the PR mapping](#post-apipull-requestspr_id) (number/url/alias/state) |
| GET | `/api/pull-requests/{pr_id}/comments` | [Live-query the forge](#get-apipull-requestspr_idcomments) for this PR's comments |
| POST | `/api/pull-requests/{pr_id}/action-feedback` | [Pull un-actioned feedback](#post-apipull-requestspr_idaction-feedback) into the worktree |

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

### `POST /api/daemon/shutdown`
Kill every process this daemon has spawned — every session, verify,
review/guardian merge, feedback chat, change-summary, and manual check
subprocess it started, transitively — then exit the daemon process. This is
what `ralphus-daemon stop [--port N] [--auto-cancel]` calls.

Unconditionally, regardless of the request body: trips every in-flight run's
cooperative cancellation token (stops its subprocess within the runner's
normal poll interval) and force-kills every `ralphus_`-prefixed tmux.exe
process (covers both run sessions and guardian/review sessions, which share
that naming prefix). Anything left alive after that — e.g. a `command`-kind
verify subprocess, which has no cooperative cancellation today — is still
guaranteed to die: the daemon confines its whole process tree to a Windows
Job Object at startup (`JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`, see
`daemon/src/jobobject.rs`), so terminating the daemon process itself takes
every remaining descendant down with it.

Request (all fields optional; an empty/absent body is equivalent to
`{"auto_cancel": false}`):
```json
{ "auto_cancel": false }
```
- `auto_cancel: false` (default) — DB state is left alone. Runs/guardians
  that were mid-flight stay `running`/`merging`/etc, so the existing
  crash-recovery path (`Store::recover_orphaned_runs` /
  `recover_orphaned_merges`, run at every `serve()` startup) resumes them
  automatically the next time `ralphus-daemon serve` starts.
- `auto_cancel: true` — additionally cascade-cancels every non-terminal run
  (same logic as [`POST /api/runs/{id}/cancel`](#post-apirunsidcancel)) and
  every cancellable guardian, so history reflects an intentional stop and
  nothing auto-resumes on the next start.

Response `200` (sent before the daemon actually exits):
```json
{
  "state": "stopping",
  "auto_cancel": false,
  "cancelled_runs": [],
  "cancelled_guardians": []
}
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

### `POST /api/projects`
Register (or re-register, updating its fields) a project by name (RAL-100).
Lets a task's session `cwd` use the placeholder `"ralphus:new-worktree/<branch>"`
instead of a real filesystem path -- the owning task's `project` field names
which registered project to materialize it under, and the scheduler
materializes (or reuses) a git worktree for `<branch>` before the session runs.

Request:
```json
{ "name": "ralphus", "description": "the ralphus repo", "path": "C:/Users/me/ralphus", "vcs": "git" }
```
`vcs` defaults to `"git"` (only kind implemented today). `400` if `path` is not
a directory or is not a git working tree. Response `201`:
```json
{ "name": "ralphus" }
```

### `GET /api/projects`
List every registered project.

```json
{ "projects": [ { "name": "ralphus", "description": "...", "path": "...", "vcs": "git", "created_at_ms": 0 } ] }
```

### `GET /api/projects/{name}`
A single registered project by its exact name.

```json
{ "name": "ralphus", "description": "the ralphus repo", "path": "C:/Users/me/ralphus", "vcs": "git", "created_at_ms": 0 }
```
`404` if no project is registered under that exact name (this is an exact
lookup, unlike the fuzzy `resolve_project` matching used internally when a
task's `project` field is resolved against the registry).

### `GET /api/projects/{name}/validate`
Re-check a registered project's on-disk path/vcs kind without writing
anything (RAL-101) -- lets a client (e.g. the librarian's Projects tab) flag
a row whose git repository has since moved, been deleted, or stopped being a
repo, without re-registering it. Runs the exact same checks as `POST
/api/projects`.

```json
{ "valid": true }
{ "valid": false, "message": "path \"C:/gone\" does not exist or is not a directory" }
```
`404` if no project is registered under that exact name.

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

### `POST /api/runs/{id}/restart/preview`
Dry-run preview of [`POST /api/runs/{id}/restart`](#post-apirunsidrestart)
(RAL-104): computes the exact same downstream-impact set the real restart
would dirty — every session/task in the run, plus every run transitively
dependent on it — without mutating anything. The librarian shows this before
the user confirms a restart, so the preview and the real restart can never
drift out of sync (both call the same `Store::compute_run_restart_impact`).
Response `200`:
```json
{
  "sessions": [{ "task_idx": 0, "idx": 0, "task_name": "build", "session_id": "work" }],
  "tasks": [{ "idx": 0, "name": "build" }],
  "dirtied_runs": [{ "id": "run-000000000002", "label": "downstream run" }]
}
```

### `POST /api/runs/{id}/restart`
Restart a whole run and cascade dirtiness downstream (RAL-19): the run and all
its nodes reset to Pending, and every run that transitively depends on it is
also reset to Pending so it re-runs once this run finishes again (cross-run
gating holds each dependent until its upstreams are Done). Response `200`:
```json
{ "state": "pending", "dirtied": ["run-000000000002", "run-000000000003"] }
```

### `POST /api/runs/{id}/sessions/{task_idx}/{session_idx}/restart/preview`
Dry-run preview of
[`POST /api/runs/{id}/sessions/{task_idx}/{session_idx}/restart`](#post-apirunsidsessionstask_idxsession_idxrestart)
(RAL-104): the target session plus every session downstream of it within the
run, the tasks that own any of those sessions, and every run transitively
dependent on this one — computed without mutating anything, same response
shape as the run-level preview above. A non-integer index is a `400`.

### `POST /api/runs/{id}/sessions/{task_idx}/{session_idx}/restart`
Restart a single session: it and every session downstream of it within the run
reset to Pending (upstream sessions stay Done and are skipped on re-run), the
run goes back to Pending, and dependent runs are dirtied. Same response shape as
above. A non-integer index is a `400`.

### `POST /api/runs/{id}/tasks/{task_idx}/restart/preview`
Dry-run preview of
[`POST /api/runs/{id}/tasks/{task_idx}/restart`](#post-apirunsidtaskstirestart)
(RAL-150): every session the task owns, plus every session downstream of any
of them within the run, the tasks that own any of those sessions, and every
run transitively dependent on this one — same response shape and computation
philosophy as the session-level preview above (`Store::compute_task_restart_impact`).
A non-integer index is a `400`.

### `POST /api/runs/{id}/tasks/{task_idx}/restart`
Restart a whole task (RAL-150): every session it owns, plus every session
downstream of any of them within the run, resets to Pending; the run and each
affected task go back to Pending; dependent runs are dirtied. The
task-granularity counterpart of the run/session restarts above — same response
shape. A non-integer index is a `400`; an unknown task index is a `404`.

### `POST /api/runs/{id}/env`
Set (`set`) and/or remove (`unset`) persistent environment-variable overrides
on a run (RAL-150). Body:
```json
{ "set": { "RALPHUS_RESOLVER_MODEL": "qwen3:8b" }, "unset": ["SOME_OLD_FLAG"] }
```
At least one of `set`/`unset` must be non-empty (`400` otherwise). Every key in
either map/list must be a valid environment-variable identifier
(`[A-Za-z_][A-Za-z0-9_]*`) — checked with `crate::config::is_valid_env_key`,
both to catch typos early and because
`crate::tmux::build_command_line_with_env` relies on the same validation as a
shell-injection backstop for the tmux-wrapped runner path. `set` entries win
over `unset` when a key appears in both. Overrides are **persistent** (not a
one-shot retry parameter): once set, a key stays applied to every
session/verify-step subprocess this run spawns — across any number of future
retries/restarts — until explicitly unset. They do not themselves trigger a
re-run; pair this with `.../retry`, `.../restart`, or a task/session/verify
restart to actually re-execute something under the new values (the CLI's
`ralphus retry <selector> --environment KEY=VAL` does exactly that in one
step). Response `200` is the resulting full override map:
```json
{ "RALPHUS_RESOLVER_MODEL": "qwen3:8b" }
```
Every change is also recorded to Cartographer with the changed key names in
the clear but **values redacted unless the key is in the project's
`[env_overrides].allowlist`** (`.ralphus.toml`) — see `EnvOverridesConfig` in
`daemon/src/config.rs`. The run detail view (`GET /api/runs/{id}`,
`RunView.env_overrides`) shows the raw, unredacted current values, since that
view is scoped to whoever already has run-detail access rather than a shared
audit log.

#### Hierarchical env overrides (task/session/verify layers)

Env overrides can also be set at task, session, and verify-step granularity,
each overriding its parent scope's value for the same key:

```
run  <  task  <  task.verify        (a task's own verify steps)
run  <  task  <  session  <  session.verify   (a session's own verify steps)
```

i.e. a session inherits the run's and its task's overrides but wins on a
shared key; a session-scoped verify step additionally inherits+overrides
whatever its owning session resolved to. Same request/response shape, same
validation, persistence, and Cartographer-redaction rules as
`POST /api/runs/{id}/env` above — only the scope and endpoint differ:

| Endpoint | Scope |
|---|---|
| `POST /api/runs/{id}/tasks/{ti}/env` | This task's own overrides — win over the run's, apply to every session under this task. |
| `POST /api/runs/{id}/tasks/{ti}/verify/env` | This task's own (task-scoped) verify-step overrides — win over the task's own (and the run's), only for this task's verify steps. |
| `POST /api/runs/{id}/sessions/{ti}/{si}/env` | This session's own overrides — win over its task's (and the run's). |
| `POST /api/runs/{id}/sessions/{ti}/{si}/verify/env` | This session's own (session-scoped) verify-step overrides — win over the session's own (and its task's/run's), only for this session's verify steps. |

A non-integer task/session index is a `400`; an unknown task/session is a
`404`. The resolved map for each scope rides along in the corresponding
`TaskView`/`SessionView` (`env_overrides`/`verify_env_overrides` fields) inside
the existing `GET /api/runs/{id}` response — there are no separate GET routes
for these.

### `POST /api/runs/{id}/add-dependency`
Wire up a manual cross-run dependency after submission (RAL-105), e.g. from the
board's "Add Dependency" right-click menu. Body:
```json
{ "target_id": "run-000000000001" }
```
Appends `target_id` to `id`'s `[[default]] depends_on` list — the same list
`ralphus submit` populates from TOML — so it is picked up by the existing
whole-run gating (`Store::list_ready`) with no new scheduling path: `id` will
not be scheduled until `target_id` reaches Done. `target_id` itself is not
modified. A dependency that is already present is a no-op. Returns `200` with
the updated run. A self-reference or a reference that would create a cycle in
the cross-run dependency graph is a `409`; an unknown `id`/`target_id` is a
`404`; a malformed body is a `400`.

### `GET /api/guardians/{id}`
A single review's full detail, including its ordered `branches` list. Each
branch (`BranchView`) carries two rebase-progress fields (RAL-145):
`rebase_commands_done` / `rebase_commands_total` (`i64`, both `null` unless
populated). They're read live, straight from git's own interactive-rebase
todo-list bookkeeping in that branch's worktree (`rebase-merge/done` and
`rebase-merge/git-rebase-todo` — the same files `git status` summarizes as
"Last commands done" / "Next commands to do") — no separate bookkeeping is
kept by the merge engine itself. Populated only for the branch currently
`merge_status: "in_progress"` that also has a `worktree` (every other branch
always reports `null`/`null`), and even then only when a rebase is actually
paused/running there — a transient gap right after `--continue`/`--skip`
(files briefly absent) also reads as `null`/`null` rather than a stale or
spurious `0`/`0`.

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

### `POST /api/guardians/{id}/branches/arrange`
Atomically reorder branches, optionally toggle their enabled state, **and**
kick off the rebase — one call instead of `branches/reorder` followed by
`merge`. Added for CLI parity (`ralphus review reorder` /
`review branch enable|disable`) to close the read-modify-write window the
two-call sequence had: a second writer's `reorder` landing between the first
caller's `reorder` and `merge` was silently clobbered. Body:
```json
{ "order": ["feat-a", "feat-b", "feat-c"], "enabled": { "feat-b": false } }
```
`enabled` is optional; unknown branch names in it are ignored (same as
`branches/reorder`). Returns whatever `POST /api/guardians/{id}/merge` returns
(the merge-start Reply), after the reorder/enable changes are already
persisted — so the arrangement is visible via `GET /api/guardians/{id}`
regardless of whether the merge itself then succeeds.

### `POST /api/guardians/{id}/branches/{branch_id}/move`
Move a branch out of this review and into another (RAL-118: "compose a review
from worktrees belonging to other reviews"). `branch_id` is the branch's
stable id (RAL-122, e.g. `branch-000000000042`) — not its stack position,
which changes under reorder and isn't a valid addressing key. Body:
```json
{ "to_guardian_id": "guardian-000000000002" }
```
This is a **move, not a copy** (see "Future: copy mode" below): the branch leaves `{id}`'s stack entirely and is
appended to the end of `to_guardian_id`'s stack. `{id}`'s remaining branches
are renumbered to stay contiguous, but the moved branch's `branch_id` itself
is unchanged by the move — the same id continues to address it in its new
review. The moved branch's merge state is reset to
`pending` (its stale `review_branch`/`worktree`/conflict fields are cleared) —
it needs a fresh rebase against its new stack before a PR can be submitted for
it. A `moved_from_guardian_id` field on the branch records the *original*
owning review and survives further moves, so a branch moved more than once
still identifies where it truly started.

Blocked with a `409 invalid_transition` while either `{id}` or
`to_guardian_id` has a merge/rebase in flight (status `merging`) — let it
finish, or cancel it, before retrying. Also `409` if `{id}` and
`to_guardian_id` are the same, or if they are not in the same git repository.
A missing `id`/`to_guardian_id`/`branch_id` is a `404`; a malformed body
is a `400`.

On success, this endpoint also kicks off a rebuild on both reviews (source
first, best-effort, only if it still has branches left; then the
destination) so the stacked rebase in each reflects the move, then returns
whatever `POST /api/guardians/{to_guardian_id}/merge` returns (the
destination's merge-start Reply) — the same convention
`branches/{branch_id}/move`'s sibling `POST .../base` uses for its own rebuild
kickoff. The move itself is already persisted by the time that reply is
returned, regardless of whether the rebuild succeeds.

**Future: copy mode.** RAL-118 Q3 scoped v1 to *move* only and asked for a
sketch of what *copy* — the same work landing in two reviews, each rebased
against its own chain — would need on top of this:
- **Data model.** `moved_from_guardian_id` is a single nullable column: one
  branch, one (optional) origin. Copy is inherently one-to-many (one source
  branch → N destination guardians), which a scalar column can't represent.
  It would need a join table, e.g.
  `guardian_branch_copies(source_guardian_id, source_position, dest_guardian_id, dest_position, created_at_ms)`,
  so every copy relationship for a branch can be listed/traversed in either
  direction.
- **Mechanically, the rebuild side is nearly free.** Two `guardian_branches`
  rows can already name the same underlying feature branch today (no
  uniqueness constraint on `branch` — see the schema in `store.rs`), and
  `run_merge` already checks out each stack's `guardian/<id>/wt-<branch>`
  worktree from that shared feature branch fresh, namespaced per guardian id.
  So each destination naturally rebases the *same* source commits onto its
  *own* stack independently, with no cross-guardian coupling required — this
  part is not the "distinct, more complex feature."
- **The actual complexity is downstream, in PR submission and shipping.**
  Nothing today stops two independent stacks built from the same source
  branch from each being approved and each having a PR opened — i.e. the same
  commits shipping twice, through two different PRs, possibly with diverging
  conflict resolutions if the branches around them differ. A real copy
  feature needs an explicit answer to that before it ships: e.g. a "shadow"/
  linked-PR concept that surfaces "this branch is also copied into review X"
  in the PR description, or a hard gate that blocks approving/merging one
  copy while a sibling copy is still open, or simply an explicit
  user-acknowledged warning at copy time and no automated gate at all (v1
  simplicity, deferred correctness). This product decision, not the git
  mechanics above, is why copy was deferred out of RAL-118 v1.
- **Carry-forward already isolates cleanly per destination** (protection refs
  and worktree branches are namespaced `refs/ralphus/carry/<guardian id>/...`
  / `guardian/<guardian id>/wt-<branch>`), so conflict resolutions made while
  building one copy do not leak into or corrupt the other's rebuild — this is
  the same isolation `move_guardian_branch`'s source-side rebuild relies on
  (see `guardian_merge::purge_worktrees`).

### `POST /api/guardians/{id}/resolve-input`
"Set it for me" (RAL-164): asks the resolver agent to propose a value for one
named `CheckInput` (see "Structured checks (RAL-164)" above) declared by a
check in `manual_commands`/`action_hints` (searched in that order, first
match wins — the request only needs the input's name, not which check it
belongs to). Body:
```json
{ "input_name": "port" }
```
Returns `202` immediately; the caller polls the normal guardian view (this is
not a dedicated polling endpoint) and reads
`input_resolutions[input_name]` — `{"status": "resolving"}` while the
resolver agent call is in flight, then `{"status": "ready", "value": "..."}`
on success or `{"status": "failed"}` on error. On success the value is also
folded into `input_values[input_name]`, so it becomes the new default the
next time that input is shown, exactly as if a human had submitted it.

Spam-proofed **server-side**, not just by disabling a button client-side: a
concurrent duplicate request for the same `(guardian_id, input_name)` pair —
a double-click, a second browser tab, a direct API call — races an atomic
SQL upsert (`Store::claim_guardian_input_resolution`) and loses, getting
`409 already_in_progress`. `400 unknown_input` if no check on this review
declares an input with that name. A daemon restart while a resolution is
`resolving` resets it to `failed` on the next startup (crash recovery,
mirrors merge recovery) so the UI never shows a permanently-stuck spinner.

### `GET /api/guardians/{id}/branches/{branch_id}/conflicts`
The live list of files still carrying unresolved `<<<<<<<` merge-conflict
markers in one review branch's worktree (RAL-148), for the Reviews UI's
auto-refreshing conflicting-files panel. `branch_id` is the branch's stable
id (RAL-122), not its stack position. Computed on demand — runs `git diff
--name-only --diff-filter=U` in the branch's worktree, the same call
`guardian_merge::conflicted_files` already makes mid-rebase — rather than
persisted, so it is not on the hot board path (same convention as `GET
/api/runs/{id}/worktrees`):
```json
{ "files": ["src/foo.rs", "src/bar.rs"], "rebase_in_progress": true }
```
- `files` — paths relative to the worktree root. Empty (not an error) once
  the branch has no worktree yet, the worktree directory no longer exists
  (e.g. purged by a base-branch-shift rebuild), or every conflict in it has
  been resolved and staged — a poller can treat an empty list as "nothing to
  show" without special-casing those cases.
- `rebase_in_progress` — whether the worktree is currently mid-`git rebase`.
  Can be `false` while `files` is still non-empty (e.g. between rebase steps,
  or after a resolver commits an intermediate fix), so the board should not
  infer "no active rebase" means the conflicts are stale.

`404` if `id` or `branch_id` doesn't address a real guardian/branch. Never
`409`/`5xx` for "no conflicts right now" — that is the ordinary `files: []`
response above.

### `POST /api/guardians/{id}/squash`
Toggle per-commit squashing for one git project within a review (RAL-91). Body:
```json
{ "project": "C:/repo", "enabled": true }
```
`project` must be one of the review's `projects` (a `400` otherwise). When
enabled, that project's task branches are each collapsed to a single squashed
commit in the review worktree during the next stacked rebase; the individual task
commits on the feature branches are left untouched. Scope is per git project — a
review spanning several projects honours each project's setting independently.
The change is persisted and applied on the next `merge`/rebuild (like the other
per-review opt-out toggles). Returns `200` with the updated guardian view, whose
`squash_projects` array lists the project roots with squash enabled.

### `GET /api/guardians/{id}/messages`
The review's global feedback thread (RAL-22), oldest first:
```json
{ "messages": [ { "seq": 1, "role": "reviewer", "text": "fix the naming", "at_ms": 1783120106867 } ] }
```
`role` is `reviewer` (human) or `guardian` (triage agent). `at_ms` is when the
message was posted (Unix epoch ms); the board renders it beside each message.

### `POST /api/guardians/{id}/chat`
Post a reviewer message to the global feedback thread. Body `{ "text": "..." }`
(empty is a `400`). The message is persisted immediately; the triage agent then
replies in the background — it works in the combined (all-branches-rebased)
review worktree, decides which branch(es) each request applies to, and appends
its reply to the thread. Returns `202 {"status":"triaging"}`.

### `POST /api/guardians/{id}/pull-requests`
Submit one or more PRs/MRs for a review (RAL-117). Body:
```json
{ "prs": [ { "branch_id": "branch-000000000042", "branch_alias": "feature/foo", "title": "...", "description": "..." } ] }
```
Each item in `prs` is either **stacked** (`branch_id` set to one of the
review's stacked branches' stable ids, RAL-122 — not a stack position, which
changes under reorder) or **combined** (`branch_id` omitted/`null`, submitting
the all-branches-in-one combined review worktree). `branch_alias`/`title`/
`description` are all optional: `branch_alias` defaults to the feature branch's
own name (stacked) or a sanitized form of the review's name (combined) — never
the internal `guardian/guardian-<id>/...` ref. `title`/`description` default to
an LLM-synthesized suggestion from the branch's commits, conforming to the
target repo's PR template when one is found (`.github/PULL_REQUEST_TEMPLATE.md`
or `.gitlab/merge_request_templates/Default.md`).

Runs in the background (`git push` + a forge API call are both networked);
returns `202 {"status":"submitting"}` immediately. Poll `GET .../pull-requests`
for the resulting rows. Stacked requests are pushed and opened **lowest
position first** so each one's PR base is the previous one's already-pushed
alias (`A→B`, `B→C`, `C→upstream`); a combined request always targets the
review's own base branch. `404` if the guardian doesn't exist; `400` for an
empty `prs` list.

### `GET /api/pull-requests`
Look up the ralphus PR row for a given forge PR/MR (the PR → worktree
direction), query params `forge` (`github`|`gitlab`), `repo` (URL-encoded), and
`pr_number`. `404` if nothing is recorded for that combination; `400` if any
param is missing.

### `POST /api/pull-requests/{pr_id}`
Mutate the recorded PR mapping. Body (all fields optional; only present ones
change):
```json
{ "pr_number": 43, "pr_url": "https://github.com/acme/widget/pull/43", "branch_alias": "feature/foo", "state": "closed" }
```
Exists because PR numbers are not permanently stable — a PR closed and
reopened gets a new number, and this is the only way to update the recorded
mapping after the fact (the CLI's `review pr update` wraps this). Returns the
updated PR row.

### `GET /api/pull-requests/{pr_id}/comments`
Live-queries the forge for this PR's comments/notes (GitHub issue comments;
GitLab MR notes, with system-generated notes filtered out) and returns them
annotated with whether each has already been actioned into the worktree:
```json
[ { "external_id": "123", "author": "reviewer1", "body": "please rename this", "created_at": "2026-01-01T00:00:00Z", "actioned": false } ]
```
`409` if the PR has no recorded number yet; `502` on a forge API error.

### `POST /api/pull-requests/{pr_id}/action-feedback`
Fetch this PR's un-actioned comments, aggregate them into one feedback string,
and apply them into the owning review worktree via the same path a manual
reviewer's freeform feedback takes (`POST /api/guardians/{id}/branches/{branch_id}/feedback`)
— so it re-triggers the exact same downstream restack of dependent stacked
branches. A PR submitted from the combined worktree routes feedback to the
topmost enabled stacked branch (the combined worktree itself is read-only).
Once applied, the resulting branch is pushed back to the remote under this
PR's recorded alias so the open PR/MR reflects the fix. Runs in the background;
returns `202 {"status":"actioning_feedback"}` immediately. `404` if the PR
doesn't exist.

**Auth (RAL-117 Q8):** forge API tokens are read from an environment variable,
never from a config file or the database. See `crate::forge` module docs (and
the `[forge]` config section below) for the full model — the short version is
`RALPHUS_GITHUB_TOKEN` / `RALPHUS_GITLAB_TOKEN`, or a project-specific name via
`[forge].token_env`.

### `GET /api/tasks`
The full board state the librarian polls (every ~2s). Returns all runs with
their tasks, sessions, and verify steps, plus daemon status. Accepts optional
query params so the board and `ralphus run list` share one filter/sort
implementation (`daemon/src/server.rs::filter_and_sort_runs`) instead of the
board computing it in JS alone: `status` (comma-separated run states,
case-insensitive), `name` (case-insensitive substring match on the label),
`sort` (`name` sorts by label/id ascending; anything else, including absent,
keeps the default newest-first order).

```json
{
  "daemon": { "running": 1, "max_concurrent": 12, "running_reviews": [], "downtime_active": false },
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
          "soloed": false,
          "sessions": [ { "id": "session-0", "cwd": "/repo", "agent": "claude", "model": null, "state": "done", "tokens_in": 0, "tokens_out": 0, "cost_usd": 0.0, "maximum_budget_usd": 5.0, "verify": [ { "id": "fmt", "kind": "command", "state": "done", "output": null, "spec": "cargo fmt --check", "model": null } ] } ],
          "verify":   [ { "id": "tests", "kind": "command", "state": "pending", "output": null, "spec": "cargo test", "model": null } ]
        }
      ]
    }
  ]
}
```
`TaskView.project` is always a non-null, non-empty string (RAL-141), so a
project filter facet always has real data to group by. It is the registered
project name when the task's TOML sets `project` (used to resolve the
`ralphus:new-worktree/<branch>` placeholder via `POST /api/projects` -- see
above); otherwise it falls back to the basename of the task's first
session's `cwd` (e.g. `cwd = "/home/me/myrepo"` -> `"myrepo"`), or the literal
string `"unassigned"` when there's no session, no `cwd`, or the `cwd` has no
filename component (e.g. `"/"`). This fallback is display-only: it never
writes back to the task's stored `project` value and has no effect on
worktree-placeholder resolution, which still requires an explicit, registered
`project`.

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

Each run also carries an `env_overrides` field (RAL-150): the run's persistent
environment-variable overrides, as raw unredacted `{key: value}` pairs (see
[`POST /api/runs/{id}/env`](#post-apirunsidenv)). Omitted from the JSON
entirely when empty — true for the vast majority of runs.

Each `TaskView` likewise carries `env_overrides` (this task's own overrides,
set via `POST /api/runs/{id}/tasks/{ti}/env`) and `verify_env_overrides`
(this task's own verify-step overrides, set via
`POST /api/runs/{id}/tasks/{ti}/verify/env`); each `SessionView` carries the
same pair scoped to the session (`POST /api/runs/{id}/sessions/{ti}/{si}/env`
and `.../verify/env`) -- see
[Hierarchical env overrides](#hierarchical-env-overrides-tasksessionverify-layers)
above for how these merge with the run's. All four are raw unredacted
`{key: value}` pairs, omitted from the JSON when empty, same as the run's.

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

### `GET /api/runs/{id}/graph`
The run's internal session dependency graph (`ralphus graph <run_id>`), built
from the same `depends_on` resolution `daemon/src/plan.rs::plan()` uses for
scheduling — nodes are sessions, not tasks, since the session is the
schedulable unit:
```json
{
  "nodes": [
    { "id": "t0s0", "task_idx": 0, "session_idx": 0, "task_name": "build", "session_id": "compile" }
  ],
  "edges": [ { "from": "t0s0", "to": "t1s0" } ]
}
```
`edges[].from` must complete before `.to` may start. `404` if the run does not
exist; `500` (`code: "cycle"`) on a dependency cycle — should not happen for an
already-submitted run (submission itself rejects cycles), but the underlying
`plan()` call is fallible so this stays honest rather than unwrapping. Rendering
(ASCII/DOT) happens entirely client-side; see `cli/src/ralphus/graphview.py`.

### `GET /api/graph`
The cross-run `[[default]] depends_on` gating graph (`ralphus graph --global`):
nodes are runs, edges are `[[default]]` references from one run to another.
`?all=1` includes terminal (done/failed/cancelled) runs; by default only
active (queued/pending/running) runs are included, and a dependency reference
to an excluded/unresolvable run produces no edge (best-effort, same philosophy
as within-run `depends_on` resolution). Shape:
```json
{
  "nodes": [ { "id": "run-000000000001", "label": null, "state": "pending" } ],
  "edges": [ { "from": "run-000000000001", "to": "run-000000000002" } ]
}
```

### `GET /api/runs/{id}/worktrees`
Per-session git info for the detail pane's read-only rows (CCTL-148; `upstream`
added later): the session's own worktree (`cwd`), its shared project root, and
the upstream to display. Computed on demand (runs `git` per session), not on
the hot board path:
```json
[
  { "task_idx": 0, "session_idx": 0, "worktree": "C:/repo/.git/.ralphus_worktrees/feat", "project": "C:/repo", "upstream": "main" }
]
```
`project`/`upstream` are `null` when `cwd` is not inside a git worktree.
`upstream` is one of two things, per the session's `upstream = "<<task:...>>"`
sentinel (RAL-50 branch-chaining):
- **Sentinel set**: the *referenced* session's own worktree branch name (what
  this session's branch is rebased onto before it runs) — `null` if that
  dependency hasn't materialized a worktree yet (never falls back to the
  tracking ref below in this case, to avoid showing a misleading value).
- **No sentinel**: the worktree's own git upstream tracking branch (typically
  the non-worktree base branch it was forked from, e.g. `main`), or `null` if
  none is configured.

See `daemon/src/reviews.rs::session_upstream_display`.

### `GET /api/runs/{id}/logs`
The event timeline for a run: an ordered list of `{ ts, level, source, message }`
plus per-session and per-verifier log references (drives the Logs modal tabs).

### `POST /api/runs/{id}/activate`
Move a held `Queued` run to `Pending`. Returns the new state.

### `POST /api/runs/{id}/tasks/{ti}/solo`
Solo a task within a run (RAL-157): while any task in the run is soloed, the
scheduler only dispatches soloed tasks' not-yet-started sessions — every
other task's sessions stay `pending` until un-soloed, even once the soloed
task itself finishes (a dependent task must not start racing ahead just
because its soloed upstream completed). A session already `running` when a
sibling gets soloed is left to finish on its own — there is no per-session
interrupt in this codebase (cancellation is run-wide only), so pausing an
in-flight session's task takes effect starting at that task's *next*
session, not mid-session. Multiple tasks in the same run may be soloed at
once; soloing one does not un-solo another. Idempotent. Solo state is
sticky — it never auto-clears (not on the soloed task's own completion, not
on a run restart); [`POST /api/runs/{id}/tasks/{ti}/unsolo`](#post-apirunsidtaskstiunsolo)
is the only way to resume paused siblings. Returns the refreshed `RunView`
(so `tasks[].soloed` reflects the change in the same round trip). An unknown
run or task index is a `404`; a non-integer `{ti}` is a `400`.

### `POST /api/runs/{id}/tasks/{ti}/unsolo`
Un-solo a task (RAL-157) — the reverse of
[`POST /api/runs/{id}/tasks/{ti}/solo`](#post-apirunsidtaskstisolo). Returns
the refreshed `RunView`. Idempotent; same error responses as `solo`.

### `POST /api/runs/{id}/cancel/preview`
Dry-run preview of [`POST /api/runs/{id}/cancel`](#post-apirunsidcancel)
(RAL-116): computes the exact same cascade-cancel impact set the real cancel
would affect — this run plus every run transitively dependent on it — without
mutating anything. The librarian shows this before the user confirms a
cancel, so the preview and the real cancel can never drift out of sync (both
call the same `Store::cancel_run`). Response `200`:
```json
{ "runs": [{ "id": "run-000000000001", "label": null }, { "id": "run-000000000002", "label": "downstream run" }] }
```
An unknown `id` is a `404`.

### `POST /api/runs/{id}/cancel`
Cancel a run **and every run transitively dependent on it** (RAL-116).
Always available and idempotent regardless of the run's current state — even
an already-terminal run (`done`/`failed`/already `cancelled`) is
(re-)cancelled, so it can never be picked up again by another trigger (a
restart, cross-run gating, etc). Kills any in-flight task/session/verify
subprocess via the same cooperative cancellation used mid-run (best-effort
termination). `POST /api/runs/{id}/set-status` with `{"kind":"run","state":"cancelled"}`
routes through this exact same cascading cancel — not a separate DB-only flip
— so both entry points have identical effect. Response `200`:
```json
{ "state": "cancelled", "cancelled": ["run-000000000001", "run-000000000002"] }
```
An unknown `id` is a `404`.

### `GET /api/cartographer`
The global Cartographer log (RAL-98): every structured event in the system —
task/session lifecycle, verify starts/results, status transitions, Guardian
review lifecycle events — as one filterable, paginated, sortable table. This
is the same view used both for "show me everything" and for "show me this
one run/session/guardian's history"; the latter is just this endpoint with a
`run_id`/`session_id`/`guardian_id` filter applied (it replaces the old
per-run "events" sub-tab that used to be backed by `GET /api/runs/{id}/logs`).

Query params (all optional): `source`, `scope`, `level`, `run_id`,
`guardian_id`, `session_id`, `q` (substring match on message), `since_ms`,
`until_ms`, `limit` (default 100, max 1000), `offset`, `sort` (`asc`/`desc`,
default `desc` — newest first).

```json
{
  "rows": [
    {
      "id": 42,
      "at_ms": 1732300000000,
      "level": "info",
      "source": "scheduler",
      "message": "run run-000000000001 claimed → running",
      "scope": "run",
      "run_id": "run-000000000001",
      "guardian_id": null,
      "session_id": null,
      "task": null,
      "payload": {}
    }
  ],
  "total": 128
}
```

Retention is enforced by two independently configurable caps under
`[cartographer]` in `.ralphus.toml` (`retention_days`, default 30;
`max_rows`, default 50000) — either condition triggers pruning, checked
every 10 minutes by the scheduler.

### `GET /api/cartographer/{id}`
One Cartographer row's full detail, by its `id`. `404` if it does not exist
(e.g. already pruned).

### `GET /api/ghosts/{owner_uri}`
Fetch one "ghost" (RAL-136) — a short, best-effort handoff note a task
session or Guardian review worktree published for whoever picks up dependent
work next (open questions, places it struggled, things it noticed but didn't
fix; deliberately **not** a changelog of what's already recoverable from `git
log`). `owner_uri` is the publisher's stable id: `session:{run_id}:{task_idx}:
{session_idx}` for a task session, `review:{guardian_id}:{branch_id}` (or
`review:{guardian_id}:combined`) for a review worktree. There is at most one
ghost row per owner — a session/review that publishes again merges onto its
existing note rather than adding a second row. `404` if that owner has never
published one.

Content isn't only the agent's own self-report: when the daemon can determine
the ground-truth pass/fail of a scope's verify/check step(s), it folds an
advisory note onto the same ghost (RAL-152), e.g. "the prior run's 2/2
verify/check step(s) passed -- you internally validated that the code works.
... re-test/re-verify the existing work first rather than assuming it's
broken." This is phrased as a hint, not a guarantee — it can go stale (e.g. a
rebase or conflict resolution since it was written) — and applies wherever a
ghost is written: task session restarts, verify-only restarts, and Guardian
resolver restarts.

```json
{
  "owner_uri": "session:run-000000000001:0:0",
  "kind": "session",
  "run_id": "run-000000000001",
  "guardian_id": null,
  "content": "Left the retry loop untuned -- the 3rd flaky test case needs a longer backoff.",
  "revision": "a1b2c3d4",
  "created_at_ms": 1732300000000,
  "updated_at_ms": 1732300000000
}
```

`revision` is an opaque, VCS-agnostic marker (currently a git commit sha when
the publishing worktree is a git repo, `null` otherwise) for best-effort
staleness reasoning — nothing re-validates it automatically. A session's own
prior ghost, and its direct dependencies' ghosts (one level up in the task
graph only), are already injected into its prompt automatically at session
start; this endpoint is for explicit lookups beyond that (tooling, the CLI,
one review branch checking another's notes).

### `POST /api/ghosts/copy`
Explicitly copy `source_uri`'s ghost onto `target_uri`, independent of the
dependency graph (e.g. seeding a brand-new task session with a prior
investigation's findings). Merges onto whatever `target_uri` already has,
same as any other ghost write.

```json
{ "source_uri": "session:run-000000000001:0:0", "target_uri": "session:run-000000000002:1:0" }
```

`target_uri`'s prefix (`session:`/`review:`) determines the copy's owner
kind — only the two URIs are needed, not every column of the target row.
`400` if `target_uri` doesn't parse as a `session:`/`review:` URI; `404` if
`source_uri` has no ghost to copy. Response `200` is the resulting `GhostView`
(same shape as [`GET /api/ghosts/{owner_uri}`](#get-apighostsowner_uri)).

### `ralphus history`/`ralphus listen` (RAL-140) — no new daemon endpoints
`ralphus history <session|verify ID> [--live]` and `ralphus listen <ID>
--until <status>` are CLI-only compositions over the endpoints already
documented above; RAL-140 deliberately adds no new daemon routes or storage
tables of its own:

- **`ralphus history <ID>` (no `--live`)** — a one-shot, non-blocking
  snapshot. If the id's tmux session is currently live, this is the same
  `.../pane` content the board's "Show Live View" reads (see `GET
  .../sessions/{ti}/{si}/pane` / `GET .../verifies/{ti}/{scope}/{si}/{vi}/pane`
  above). Once the tmux session is gone, `.../pane` itself may still return a
  persisted last-pane-content snapshot (RAL-102 follow-up —
  `crate::tmux::write_pane_snapshot`/`read_pane_snapshot`, a read-only
  historical record of what the pane last showed) if one was captured, but
  `ralphus history` prefers a more stable, curated record over a raw
  transcript replay — it falls back to whatever was already durably
  persisted for that entity by mechanisms that predate this ticket:
  - A **session**'s fallback is its RAL-136 ghost — `GET
    /api/ghosts/session:{run_id}:{task_idx}:{session_idx}` (see above). A
    `404` (no ghost ever published) is rendered as "no history recorded yet",
    not an error.
  - A **verify** step's fallback is its already-stored `output` text (part of
    a run's `GET /api/runs/{id}` response since long before this ticket) —
    ghosts have no per-verify granularity, so there is nothing new to add
    here either.
- **`ralphus history <ID> --live`** — blocks and tails the live `.../pane`
  endpoint every second, diffing each poll against a small cursor
  (`length` + a short trailing-content fingerprint) the CLI persists to
  `~/.ralphus/history_cursors/` so a restarted CLI process resumes from where
  it left off instead of re-printing already-seen output, and so multiple
  independent watchers of the same session never share (or clobber) a read
  position. Fails immediately with a clear error if nothing is currently
  live for the id (`--wait-until-valid [SECONDS]` opts into waiting instead).
  Once the pane goes inactive, the session/verify's ghost/output (see above)
  is printed as one final, separately labelled block — it's a short curated
  note, not a continuation of the raw tmux transcript just tailed, so it is
  never diffed against the tailing cursor.
- **`ralphus listen <ID> --until <status>`** — polls the existing `GET
  /api/runs/{id}` (for a run/task/session/verify selector) or `GET
  /api/guardians/{id}` (for a review/review-worktree selector) endpoint once
  a second until the resolved entity's status equals the caller-supplied
  `--until` value, then exits. No log/tmux content is involved.

## Notes on future evolution

- Auth/identity and per-user attribution are **not** in this draft; when
  multi-user hardening begins, an `Authorization` header + a `submitted_by`
  field on runs are the expected additions (see `FOLLOW.local.md` #3).
- Live updates are poll-based for now (librarian polls `GET /api/tasks`); an SSE
  or WebSocket channel is a later optimization.
