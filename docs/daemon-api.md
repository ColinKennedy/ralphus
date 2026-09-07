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
| GET | `/api/config/live-view` | [Live View "Show Debug Messages" default](#get-apiconfiglive-view-ral-232) (RAL-232) |
| GET | `/api/config/templates` | [Simple task form's template picker](#get-apiconfigtemplates) (RAL-297) |
| GET | `/api/agents/catalog` | [Cwd-independent agent+model catalog](#get-apiagentscatalog) (RAL-297) |
| GET | `/api/projects/{name}/branches` | [Local+remote branch names for a git project](#get-apiprojectsnamebranches) (RAL-297) |
| POST | `/api/generate` | [Kick off a Simple-form generation step](#post-apigenerate) (RAL-297) |
| GET | `/api/generate/{id}` | [Poll a generation job](#get-apigenerateid) (RAL-297) |
| GET | `/api/cartographer` | [Structured event log](#get-apicartographer), filtered/paginated; `?entity=` accepts an [entity URI](#entity-uris-ral-155) |
| GET | `/api/cartographer/{id}` | [One event's full detail](#get-apicartographerid) |
| POST | `/api/events/ticket` | [Mint a short-lived SSE ticket](#post-apieventsticket-ral-222) |
| GET | `/api/events` | [SSE push stream](#get-apievents-ral-167) — one event per Cartographer write (RAL-167); requires `?ticket=` (RAL-222) |
| GET | `/api/graph` | [Cross-squad gating graph](#get-apigraph); `?all=1` includes terminal squads |
| GET | `/api/resolve` | [Resolve a ralphus URI](#get-apiresolve-ral-188) to positional coordinates; `?uri=` |
| GET | `/api/ghosts/{owner_uri}` | [Fetch a ghost](#get-apighostsowner_uri) by its owning cell/review URI |
| POST | `/api/ghosts/copy` | [Copy a ghost](#post-apighostscopy) onto another owner, independent of the dependency graph |
| GET | `/api/hidden` | [List the current user's hidden squads and reviews](#hidden-items-ral-328) |
| POST | `/api/hidden/squads/{id}` | [Hide a squad for the current user](#hidden-items-ral-328) |
| DELETE | `/api/hidden/squads/{id}` | [Re-enable a squad for the current user](#hidden-items-ral-328) |
| POST | `/api/hidden/reviews/{id}` | [Hide a review for the current user](#hidden-items-ral-328) |
| DELETE | `/api/hidden/reviews/{id}` | [Re-enable a review for the current user](#hidden-items-ral-328) |

**Squads**
| Method | Path | What |
|---|---|---|
| POST | `/api/squads/validate` | [Validate a TOML batch](#post-apisquadsvalidate) without persisting |
| POST | `/api/squads` | [Submit a TOML batch](#post-apisquads) |
| POST | `/api/clear` | [Bulk-delete](#post-apiclear) tasks and reviews |
| GET | `/api/queue` | Ordered, classified queue of runnable work items |
| POST | `/api/queue/reorder` | Reorder the queue (dependency-repaired) |
| POST | `/api/queue/set-position` | Move item(s) to an absolute/relative position |
| GET | `/api/squads/{id}` | [One squad's full detail](#get-apisquadsid) |
| GET | `/api/squads/{id}/worktrees` | [Per-cell worktree/project/upstream](#get-apisquadsidworktrees) |
| GET | `/api/squads/{id}/logs` | [State-transition audit log](#get-apisquadsidlogs) |
| GET | `/api/squads/{id}/timeline` | [Merged, chronological uber-log-viewer](#get-apisquadsidtimeline) for the whole squad (RAL-155) |
| GET | `/api/squads/{id}/graph` | [Internal cell dependency graph](#get-apisquadsidgraph) |
| POST | `/api/squads/{id}/activate` | [Queued → Pending](#post-apisquadsidactivate) |
| POST | `/api/squads/{id}/cancel/preview` | [Dry-run preview](#post-apisquadsidcancelpreview) of a cascading cancel's impact |
| POST | `/api/squads/{id}/cancel` | [Cancel the squad](#post-apisquadsidcancel), cascading to downstream dependents |
| POST | `/api/squads/{id}/set-status` | Manually override a squad/task/cell/proof's state — for a task/cell/proof, any target state but `pending` first captures its running agent's tmux pane into that cell's ghost and kills it (RAL-163) |
| POST | `/api/squads/{id}/edit` | Edit a squad/task/cell/proof's fields; a squad/task edit resets the whole squad to Pending, a cell edit resets only that cell + its downstream, a proof edit only that step + later steps in its scope |
| POST | `/api/squads/{id}/retry` | Re-run with existing parameters (reset to Pending) |
| POST | `/api/squads/{id}/restart/preview` | [Dry-run preview](#post-apisquadsidrestartpreview) of a whole-squad restart's downstream impact |
| POST | `/api/squads/{id}/restart` | [Restart the whole squad](#post-apisquadsidrestart), cascading dirtiness |
| POST | `/api/squads/{id}/add-dependency` | [Add a cross-squad dependency](#post-apisquadsidadd-dependency) post-submission |
| POST | `/api/squads/{id}/cells/{ti}/{si}/restart/preview` | [Dry-run preview](#post-apisquadsidcellstask_idxcell_idxrestartpreview) of a cell restart's downstream impact |
| POST | `/api/squads/{id}/cells/{ti}/{si}/restart` | [Restart one cell](#post-apisquadsidcellstask_idxcell_idxrestart) + its downstream |
| POST | `/api/squads/{id}/cells/{ti}/{si}/proof/{vi}/restart` | Restart a cell's proof steps from `vi` |
| POST | `/api/squads/{id}/tasks/{ti}/proof/{vi}/restart` | Restart a task's proof steps from `vi` |
| POST | `/api/squads/{id}/tasks/{ti}/restart/preview` | [Dry-run preview](#post-apisquadsidtaskstirestartpreview) of a task restart's downstream impact |
| POST | `/api/squads/{id}/tasks/{ti}/restart` | [Restart a whole task](#post-apisquadsidtaskstirestart) + its downstream (RAL-150) |
| GET | `/api/squads/{id}/env` | [Resolved environment variables](#get-env--resolved-environment-views-ral-324) for the squad, secret values masked (RAL-324) |
| POST | `/api/squads/{id}/env` | [Set/unset persistent environment-variable overrides](#post-apisquadsidenv) on the squad (RAL-150) |
| POST | `/api/squads/{id}/tasks/{ti}/env` | Set/unset env overrides on a task ([hierarchical env overrides](#hierarchical-env-overrides-taskcellproof-layers)) |
| POST | `/api/squads/{id}/tasks/{ti}/proof/env` | Set/unset env overrides on a task's own proof steps |
| POST | `/api/squads/{id}/tasks/{ti}/proof/{vi}/env` | Set/unset env overrides on **one** task-scoped proof step (RAL-191) |
| POST | `/api/squads/{id}/cells/{ti}/{si}/env` | Set/unset env overrides on a cell |
| POST | `/api/squads/{id}/cells/{ti}/{si}/proof/env` | Set/unset env overrides on a cell's own proof steps |
| POST | `/api/squads/{id}/cells/{ti}/{si}/proof/{vi}/env` | Set/unset env overrides on **one** cell-scoped proof step (RAL-191) |
| GET | *(each of the six `.../env` paths above)* | [Resolved environment variables](#get-env--resolved-environment-views-ral-324) for that surface, secret values masked (RAL-324) |
| POST | `/api/squads/{id}/tasks/{ti}/solo` | [Solo a task](#post-apisquadsidtaskstisolo) (RAL-157) — pauses every other task in the squad until un-soloed |
| POST | `/api/squads/{id}/tasks/{ti}/unsolo` | [Un-solo a task](#post-apisquadsidtaskstiunsolo) (RAL-157) — resumes its paused siblings |
| POST | `/api/squads/{id}/cells/{ti}/{si}/open-terminal` | `?mode=open\|readonly`: spawn a resume terminal (`claude --resume`, `codex resume`, or `pi --session`, depending on the cell's agent) **on the daemon host**. `?mode=agent` on a **finished** cell does the same; on a **still-running** cell (RAL-288 Stage 6) it detaches the cell cleanly first, waits for it to genuinely stop, then opens the real agent inside a tmux session that survives closing the terminal — see [below](#post-apisquadsidcellstisiopen-terminalmodeagent) |
| POST | `/api/squads/{id}/cells/{ti}/{si}/terminal-ticket` | [Mint a one-shot ticket for the **remote** Open Agent terminal relay](#post-apisquadsidcellstisiterminal-ticket) (RAL-355 Phase 10) — a WebSocket alternative to `open-terminal?mode=agent` for cells running on a `machine`, since that route only ever spawns a window on the daemon's own desktop |
| POST | `/api/squads/{id}/cells/{ti}/{si}/resume-automation` | Hand a detached cell back to unattended execution, continuing its exact same agent session rather than starting fresh (RAL-288 Stage 6) — see [below](#post-apisquadsidcellstisiresume-automation) |
| POST | `/api/squads/{id}/proofs/{task_idx}/{scope}/{cell_idx}/{proof_idx}/open-terminal` | Same, for a proof step's resolved cell |
| GET | `/api/squads/{id}/cells/{ti}/{si}/debug-events` | [This cell's current-attempt debug stream](#get-apisquadsidcellstisidebug-events-and-its-proofguardian-siblings-ral-296) (RAL-296) |
| GET | `/api/squads/{id}/proofs/{task_idx}/{scope}/{cell_idx}/{proof_idx}/debug-events` | Same, for a proof step's resolved cell |
| DELETE | `/api/squads/{id}` | Permanently delete a squad |

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
| POST | `/api/guardians/{id}/sync-pr` | [Check the forge for a stack reorder, on demand](#post-apiguardiansidsync-pr) |
| POST | `/api/guardians/{id}/branches/{branch_id}/feedback` | Feedback on one branch → resolver re-attempt |
| GET | `/api/guardians/{id}/branches/{branch_id}/messages` | [Per-branch feedback thread](#get-apiguardiansidbranchesbranch_idmessages) |
| GET | `/api/guardians/{id}/base-branches` | Candidate base branches (same remote) |
| POST | `/api/guardians/{id}/base` | Change base branch + rebuild |
| POST | `/api/guardians/{id}/force_start` | Disable not-yet-done branches, merge immediately |
| POST | `/api/guardians/{id}/branches/{branch_id}/dismiss_reenable` | Dismiss the "can re-enable" notice |
| POST | `/api/guardians/{id}/branches/{branch_id}/move` | [Move a branch to another review](#post-apiguardiansidbranchesposmove) (RAL-118) |
| POST | `/api/guardians/{id}/branches/{branch_id}/env` | [Set/unset/clear this review worktree's env overrides](#post-apiguardiansidbranchesbidenv--review-worktree-overrides) (RAL-191) |
| GET | `/api/guardians/{id}/branches/{branch_id}/env` | [Resolved environment variables](#get-env--resolved-environment-views-ral-324) for that review worktree (RAL-324) |
| GET | `/api/guardians/{id}/build-env` | Resolved environment variables for the finalize-time auto-build step (RAL-324) |
| GET | `/api/guardians/{id}/tests-env` | Resolved environment variables for the check gates (tests) — the build step's layer under its own entry point (RAL-324) |
| GET | `/api/guardians/{id}/manual-checks-env` | Resolved environment variables for the manual-checks step (RAL-324) |
| GET | `/api/guardians/{id}/branches/{branch_id}/conflicts` | [Live conflicting-files list](#get-apiguardiansidbranchesbranch_idconflicts) for the board's Reviews UI (RAL-148) |
| POST | `/api/guardians/{id}/branches/{branch_id}/open-terminal` | Spawn a resolver terminal **on the daemon host** |
| GET | `/api/guardians/{id}/branches/{branch_id}/debug-events` | [The resolver's current-attempt debug stream](#get-apisquadsidcellstisidebug-events-and-its-proofguardian-siblings-ral-296) (RAL-296) |
| POST | `/api/guardians/{id}/manual-checks/open-terminal` | Spawn a manual-checks-generation terminal **on the daemon host** (`?mode=open\|agent`) |
| GET | `/api/guardians/{id}/manual-checks/debug-events` | Same, for the manual-checks generation pass |
| POST | `/api/guardians/{id}/merge` | Start/continue the stacked rebase |
| POST | `/api/guardians/{id}/cancel_and_merge` | Cancel an in-progress rebase, start fresh |
| POST | `/api/guardians/{id}/stop` | [Stop a mid-rebase at the next checkpoint](#post-apiguardiansidstop) (RAL-249), leaving it resumable |
| POST | `/api/guardians/{id}/approve` | Approve an in_review guardian |
| POST | `/api/guardians/{id}/cancel` | Cancel a review |
| POST | `/api/guardians/{id}/reopen` | Reopen a cancelled review (→ `collecting`) and immediately try a fresh merge pass if the daemon has capacity |
| POST | `/api/guardians/{id}/run-manual-commands` | Spawn manual-check commands **on the daemon host** |
| POST | `/api/guardians/{id}/run-action-hint` | Spawn a `command`-kind action hint **on the daemon host**; `prompt`-kind is `501` |
| POST | `/api/guardians/{id}/resolve-input` | [Delegate a named check input to the resolver agent](#post-apiguardiansidresolve-input) ("set it for me", RAL-164) |

Four routes above are flagged "on the daemon host": they call
`spawn_in_terminal`/open a GUI terminal window (or, for the read-only
snapshot case below, an external viewer) on whatever machine runs
`ralphus-daemon`, which only makes sense when the daemon and the caller share
a desktop session. The CLI deliberately does **not** call these — see
`docs/cli-reference.md`'s `cell terminal` / `review checks run` / `review
action run` for the headless equivalent (print the resolved command + cwd
instead).

**Structured checks (RAL-164).** `manual_commands` and `action_hints` on
`GuardianView` are both `GuardianCheck[]`, not bare strings —
`{label?, command?, prompt?, cleanup_command?, inputs?}`, where `inputs` is
`{name, message, default, type}[]` naming `{name}` placeholders referenced in
`command`/`cleanup_command`. `type` (RAL-221) is one of `"string"` (default,
unconstrained) or `"int"` (must parse as a base-10 signed integer);
`GuardianView` also carries `input_values`
(`Record<string,string>`, the last value used per input name on this review —
overrides an input's own literal `default`) and `input_resolutions`
(`Record<string,{status, value?}>`, `status` one of `resolving`/`ready`/`failed`,
tracking in-flight/completed `resolve-input` calls). `run-manual-commands` and
`run-action-hint` both accept an extended body: `{index?, inputs?:
Record<string,string>, run_cleanup?: boolean}` — `inputs` substitutes named
placeholders (submitted value wins, then `input_values`, then the input's own
default) and is persisted as the new `input_values` default; `run_cleanup`
opts into chaining the check's `cleanup_command` before the main command.
Every effective value (submitted, else stored, else default) is validated
against its input's declared `type` before substitution — including a value
already sitting in `input_values` from before `type` existed on that input —
and a mismatch fails the whole call with `400 invalid_check_input` naming
which input(s) failed and why, rather than substituting it.

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
| GET | `/api/guardians/{id}/pull-request-stacks` | [List past PR stacks](#get-apiguardiansidpull-request-stacks) submitted for a review, most recent first (RAL-302) |
| GET | `/api/pull-requests` | [Find the PR row](#get-apipull-requests) for a forge PR/MR number; `?forge=&repo=&pr_number=` |
| GET | `/api/pull-requests/index` | [Flat index](#get-apipull-requestsindex) of every PR row across every guardian, annotated with source squad/task/cell (RAL-362, board Tasks tab) |
| GET | `/api/pull-requests/{pr_id}` | One PR row |
| POST | `/api/pull-requests/{pr_id}` | [Mutate the PR mapping](#post-apipull-requestspr_id) (number/url/alias/state) |
| GET | `/api/pull-requests/{pr_id}/comments` | [Live-query the forge](#get-apipull-requestspr_idcomments) for this PR's comments |
| POST | `/api/pull-requests/{pr_id}/action-feedback` | [Pull un-actioned feedback](#post-apipull-requestspr_idaction-feedback) into the worktree |
| GET | `/api/pull-requests/{pr_id}/sync-status` | [Drift check](#get-apipull-requestspr_idsync-status) between the PR branch and the review worktree (RAL-190) |
| POST | `/api/pull-requests/{pr_id}/pull-from-pr` | [Pull PR-branch commits](#post-apipull-requestspr_idpull-from-pr) into the review worktree (RAL-190) |

**Mailbox (RAL-241, poll-only scope)**
| Method | Path | What |
|---|---|---|
| POST | `/api/mailbox/register` | Register a new client, returns `{"client_id": "..."}` |
| GET | `/api/mailbox/{client_id}/messages` | List messages visible to this client; `?unread=true` and `?priority=urgent\|high\|normal` filter |
| POST | `/api/mailbox/{client_id}/drain` | Mark messages read; `{"message_ids": [...]}` or an empty body to drain every unread message |

**Personal watches and notification preferences (RAL-320)**
| Method | Path | What |
|---|---|---|
| GET | `/api/watches` | [List the acting user's watches](#get-apiwatches) |
| POST | `/api/watches` | [Watch (or re-watch) an entity](#post-apiwatches) |
| DELETE | `/api/watches/{entity_uri}` | Stop watching an entity |
| GET | `/api/users/{name}/preferences` | Get a user's `auto_watch`/`default_notify_tiers` preferences |
| POST | `/api/users/{name}/preferences` | Set a user's `auto_watch`/`default_notify_tiers` preferences |
| GET | `/api/mailbox/personal/messages` | The acting user's personal mailbox view, filtered through their watches |
| POST | `/api/mailbox/personal/drain` | Mark personal mailbox messages read for the acting user |

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

## Authentication (RAL-219)

Every route requires a bearer token: `Authorization: Bearer <token>`. A
request with no `Authorization` header, or the wrong token, gets a `401`
using the same error envelope as every other error (`{"error":{"code":
"unauthorized", "message":"missing or invalid bearer token"}}`). This applies
to reads as well as state-changing requests — Cartographer logs, cell/agent
transcripts, and registered project paths are all sensitive, so `GET`s are
gated exactly like `POST`/`DELETE`.

**Where the token lives.** The daemon generates a random token the first time
it starts and persists it to `state_dir()/daemon.token` (`~/.ralphus/
daemon.token`; `0600` permissions on Unix — RAL-230 tracks the equivalent
Windows ACL hardening as separate, not-yet-built follow-up work). A later
restart reuses the same token rather than rotating it, so a client that
already has a copy of the file keeps working across restarts. There is no
login flow and no multi-user identity system here — this is one shared
secret, appropriately scoped for a single-operator local tool (per-user
attribution is tracked separately — see "Notes on future evolution" below).

**How clients authenticate.**
- The `ralphus` CLI and the librarian's `/api/*` proxy both read the same
  token file automatically and attach the header on every request, so local
  use needs no extra configuration.
- A remote or non-browser caller (curl, a script on another machine) reads
  the same file — or is handed the token out of band — and sends the header
  itself. This is a plain HTTP header, not a browser-only/same-origin
  mechanism, so it works identically for any HTTP client; this is the
  explicit reason the mechanism is a standard `Authorization: Bearer` header
  rather than a cookie or a CSRF-style same-origin check.
- `ralphus-daemon stop` (`POST /api/daemon/shutdown`) authenticates the same
  way — it is a route like any other, not a special case.

**`/api/events` (the SSE stream) is not gated by the bearer token
directly** — it bypasses `route()` entirely (see `run_http_loop` in
`server.rs`), so the check above never runs for it. It has its own mechanism
instead (RAL-222): see [`GET /api/events`](#get-apievents-ral-167) below.

### CORS policy (RAL-220)

Both the daemon and the librarian apply an explicit, config-driven CORS
policy at their own HTTP boundary (independently — the librarian's proxy
sits in front of the daemon API, so a browser hitting the librarian's port
must be gated there too, not just at the daemon). A request carrying no
`Origin` header (any non-browser caller — the CLI, `curl`, a remote-machine
provider per RAL-185 — or a same-origin top-level navigation) is unaffected;
CORS is a browser-only mechanism.

A request that *does* carry an `Origin` header is checked against two
allowances:

1. **Default same-origin**: `Origin` matches the request's own `Host` header
   (`http://<host>` or `https://<host>`) — so hitting either server directly
   at its own address always works with zero configuration.
2. **Configured allow-list**: `Origin` exactly matches an entry in
   `[cors].allowed_origins` (`.ralphus.toml`/global config, a list field —
   layers are unioned, global-first then per-project, de-duplicated). There
   is deliberately no wildcard support.

A mismatched `Origin` is **rejected outright** (`403 origin_not_allowed`) —
not merely served without `Access-Control-*` headers. Omitting the headers
alone only stops a browser from *reading* the response; a "simple"
(no-preflight) cross-origin request — e.g. a `POST` whose `Content-Type` is
set to `text/plain` even though the body is JSON — would still execute
server-side. An allowed request gets back `Access-Control-Allow-Origin`
(echoing the exact origin) and `Vary: Origin`; a preflight `OPTIONS` for an
allowed origin additionally gets `Access-Control-Allow-Methods` and
`Access-Control-Allow-Headers`.

CORS is a browser-side control only — it does not gate non-browser callers
(a script, another machine). Combined with the auth-token check above
(RAL-219), CORS constrains what browsers will do while the token constrains
what any caller is allowed to do.

## Squad lifecycle

A *squad* is one submitted TOML batch. Its state machine:

```
Queued ─activate─▶ Pending ─(deps met, scheduler picks up)─▶ Running ─▶ Done
                      │                                          │
                      └──────────────── cancel ─────────────────┴─▶ Cancelled
                                                                   └─▶ Failed
```

**Decision (fixes an old-project bug):** `POST /api/squads` submits directly to
**`Pending`** (schedulable immediately), not `Queued`. In the old project,
submissions landed as `Queued` and silently never ran until a human clicked
"activate" in the UI. `Queued` remains available as an explicit "hold" state for
callers that want to stage a squad without scheduling it (`?hold=true`), activated
later via `POST /api/squads/{id}/activate`.

## Endpoints

### `GET /api/daemon`
Health/version probe. Never requires the DB to be writable.

```json
{ "name": "ralphus-daemon", "version": "0.1.0", "status": "ok", "db": "ok" }
```

### `POST /api/daemon/shutdown`
Kill every process this daemon has spawned — every cell, proof,
review/guardian merge, feedback, change-summary, and manual check
subprocess it started, transitively — then exit the daemon process. This is
what `ralphus-daemon stop [--port N] [--auto-cancel]` calls.

Unconditionally, regardless of the request body: trips every in-flight squad's
cooperative cancellation token (stops its subprocess within the runner's
normal poll interval) and force-kills every `ralphus_`-prefixed tmux.exe
process (covers both squad cells and guardian/review sessions, which share
that naming prefix). Anything left alive after that — e.g. a `command`-kind
proof subprocess, which has no cooperative cancellation today — is still
guaranteed to die: the daemon confines its whole process tree to a Windows
Job Object at startup (`JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`, see
`daemon/src/jobobject.rs`), so terminating the daemon process itself takes
every remaining descendant down with it.

Request (all fields optional; an empty/absent body is equivalent to
`{"auto_cancel": false}`):
```json
{ "auto_cancel": false }
```
- `auto_cancel: false` (default) — DB state is left alone. Squads/guardians
  that were mid-flight stay `running`/`merging`/etc, so the existing
  crash-recovery path (`Store::recover_orphaned_squads` at `serve()` startup,
  `scheduler::recover_interrupted_reviews` at scheduler startup — including
  reapplying any reviewer feedback an unclean shutdown interrupted mid-run)
  resumes them automatically the next time `ralphus-daemon serve` starts.
- `auto_cancel: true` — additionally cascade-cancels every non-terminal squad
  (same logic as [`POST /api/squads/{id}/cancel`](#post-apisquadsidcancel)) and
  every cancellable guardian, so history reflects an intentional stop and
  nothing auto-resumes on the next start.

Response `200` (sent before the daemon actually exits):
```json
{
  "state": "stopping",
  "auto_cancel": false,
  "cancelled_squads": [],
  "cancelled_guardians": []
}
```

### `POST /api/squads/validate`
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

### `POST /api/squads`
Validate (fail-closed) and submit a TOML batch.

Request:
```json
{ "toml": "<raw TOML text>", "hold": false, "label": "optional human label" }
```
`400` with the error envelope (`code: "validation_failed"`, `details` = the
validation errors) if invalid. On success `201`:
```json
{ "squad_id": "squad-000000000001", "state": "pending" }
```

### `POST /api/projects`
Register (or re-register, updating its fields) a project by name (RAL-100).
Lets a task's cell `cwd` use the placeholder
`"ralphus:new-worktree/<branch>?upstream=<upstream>"` instead of a real
filesystem path -- the owning task's `project` field names which registered
project to materialize it under, and the scheduler materializes (or reuses) a
git worktree for `<branch>` before the cell runs. The trailing
`?upstream=<upstream>` is REQUIRED (submit-time validation rejects a
placeholder cwd without one) and names the branch `<branch>`'s worktree
should track (`git branch --set-upstream-to`) -- a local branch (`?upstream=main`)
or a remote-qualified one (`?upstream=origin/main`); it decides what a
review's upstream branch resolves to (unless the review declares its own
`upstream` field, see `[[review]]` below — a declared value always wins)
and drives the resync-on-reuse behavior described below, rather than the
daemon guessing from `HEAD` at materialization time.
Two reserved sentinel values may be used instead of a literal branch name
(RAL-258): `?upstream=<<default>>` (resolve to the repository's default
branch, against the project root, at resolution time — recommended) and
`?upstream=<<current_branch>>` (resolve to whatever branch the project
currently has checked out). These sentinels are reserved and are never
treated as literal branch names; any other `<<...>>` value is rejected at
validation time. The placeholder parser treats everything between
`ralphus:new-worktree/` and the first `?` as one literal branch name, slashes
included. During
materialization the daemon first validates that branch name with
`git check-ref-format --branch`. If a local branch by that exact name already
exists, it is reused. Otherwise, a slash-containing name like `origin/foo` is
checked against the remote-tracking ref `refs/remotes/origin/foo`; when that
ref exists, the daemon creates the new local branch from the remote-tracking
branch -- a real, attached branch checkout, never a detached HEAD. When no
such remote-tracking ref exists, the daemon falls back to creating a literal
local branch named `origin/foo` from the project's current `HEAD` and logs a
warning about that ambiguous fallback. Either way, the branch's tracking
configuration comes from `?upstream=`, not from this fallback logic.

A worktree whose `?upstream=` names a remote-tracking branch is not frozen at
whatever the remote held on first materialization: every later time that
placeholder is resolved (i.e. on each new squad submitted against it), the
daemon fetches that remote branch and rebases the local branch onto it, so
the worktree picks up new pushes over the life of the project. This is a
rebase, not a hard reset -- commits already made in that worktree are
replayed on top of the fetched history rather than discarded. If the rebase
can't complete cleanly (a real conflict, or otherwise-diverged local
history), it is aborted and squad resolution fails with an error naming the
worktree, leaving it exactly as it was for a human to resolve by hand.

`path` remains the daemon host's local checkout. `clone_url` is the
authoritative source a machine provider uses to clone the project on another
machine; Ralphus does not infer it from `path`'s configured Git remotes.
Existing path-only registrations remain valid for local work, but remote Git
provisioning rejects them until a clone URL is registered. The request also
accepts `url` as an alias for `clone_url`. Omitting `clone_url`/`url`
preserves whatever URL is already registered (so an older client that
doesn't know the field yet can't accidentally erase it); to remove a
previously registered URL, set `"clear_clone_url": true` instead (`ralphus
project git --clear-url`) -- `400` if the request sets both a URL and
`clear_clone_url` at once.

Request:
```json
{ "name": "ralphus", "description": "the ralphus repo", "path": "C:/Users/me/ralphus", "clone_url": "git@github.com:owner/ralphus.git", "vcs": "git" }
```
`vcs` defaults to `"git"` (only kind implemented today). `400` if `path` is not
a directory or is not a git working tree. `match_pr_branch_name` (RAL-307,
optional boolean) sets this project's default for whether a newly submitted
PR's branch defaults to the exact worktree/feature branch name; omitted, it
stamps the live global config's value instead (same "frozen at first
registration, not retroactive, not backfilled for a re-registration" shape as
the internal-only `skip_base_updates` stamp). Every new review created under
this project stamps that effective value onto itself at creation time (see
`POST /api/guardians/{id}/settings`'s `match_pr_branch_name`), where it's then
independently editable per-review. Response `201`:
```json
{ "name": "ralphus" }
```
An `http(s)://` `clone_url`/`url` carrying an inline password
(`https://user:pass@host/repo.git`) is accepted, not rejected, but the
response then carries a `warnings` array naming the risk with the password
itself redacted (`https://***@host/repo.git`); the daemon's own log line
about it is redacted the same way, so the warning is never itself a leak
path:
```json
{ "name": "ralphus", "warnings": ["clone URL embeds inline credentials (https://***@host/repo.git); consider an SSH key or credential helper instead"] }
```
An SSH URL (`git@host:org/repo.git`) or an `http(s)://` URL with a bare
username and no password (`https://token@host/repo.git`) never triggers
this -- there is no separate secret sitting in the URL text to warn about.

### `GET /api/projects`
List every registered project.

```json
{ "projects": [ { "name": "ralphus", "description": "...", "path": "...", "clone_url": "git@github.com:owner/ralphus.git", "vcs": "git", "created_at_ms": 0 } ] }
```

### `GET /api/projects/{name}`
A single registered project by its exact name.

```json
{ "name": "ralphus", "description": "the ralphus repo", "path": "C:/Users/me/ralphus", "clone_url": "git@github.com:owner/ralphus.git", "vcs": "git", "created_at_ms": 0 }
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

### `GET /api/projects/{name}/branches`
Local and `origin` remote-tracking branch names for a registered git
project (RAL-297) -- lets a client validate a user-typed upstream branch
before generating a `ralphus:new-worktree/<branch>?upstream=<name>`
placeholder, rather than only discovering a typo much later, at worktree
materialization time. Empty list for a non-git project or a path that no
longer resolves; `404` if no project is registered under that exact name.

```json
{ "branches": ["main", "origin/staging"] }
```

### `GET /api/agents`
List the agents selectable for a project -- built-in backends plus whatever
`.ralphus.toml` custom `[agent.profiles.*]` entries apply there (see the
**agent profile** glossary entry). Backs the board's review-resolver
dropdown; not review-specific, so any future agent picker can read from it
too.

Query params: `cwd` (required, a project/worktree path). The optional
caller-claimed identity is sent in `X-Ralphus-User`, with `[daemon].default_user`
as the fallback. It is passed to `AgentAccess`/`UserContext` but remains inert
until authenticated identity and access policy exist.

`default_agent` is the effective `[review].default_resolver_agent`
(`.ralphus.toml`, global layered under `cwd`'s project config; unset resolves
to `"ollama"`) -- the entry in `agents` whose `id` matches it is the one a
review falls back to when it sets no resolver agent of its own. Not
guaranteed to name an entry in `agents` if misconfigured -- `ralphus check
health` flags that case.

`default_resolver_agent` has four siblings under the same `.ralphus.toml
[review]` table (RAL-342/RAL-338), each the per-project default for a
setting a `[[review]]` block can otherwise declare explicitly -- so a review
the Arbiter creates automatically (Triage, RAL-318, which has no `[[review]]`
block to read from at all) still picks up sensible per-project behavior
instead of falling through to a global, non-project-aware default:

| `.ralphus.toml [review]` key | Mirrors `[[review]]`'s... | Unset resolves to |
| --- | --- | --- |
| `default_resolver_model` | `model` | each backend's own default (`"qwen3:8b"` for ollama, `None` otherwise) |
| `default_machine` | `machine` | the local machine |
| `default_maximum_budget_usd` | `maximum_budget_usd` | unbounded |
| `default_proof_scope` | `proof_scope` | `"each_branch"` |

An explicitly declared `[[review]]` value always wins over its project
default. `default_proof_scope` accepts the legacy `verify_scope` key name too
(pre-dates the Verify→Proof rename, see the glossary). `id`/`name`/`upstream`/
`action` have no project-default equivalent -- see
`daemon/src/config.rs`'s `REVIEW_FIELD_PARITY` for why each is excluded.

```json
{ "agents": [
  { "id": "claude-code", "kind": "builtin", "backend": "claude-code" },
  { "id": "openrouter-deepseek", "kind": "profile", "backend": "claude-code" }
], "default_agent": "ollama" }
```

### `GET /api/agents/catalog`
Cwd-independent agent+model catalog for the board's Simple task form
(RAL-297). Unlike `GET /api/agents`, this takes no `cwd` -- the Simple
tab's agent/model picker is deliberately independent of its project picker
(they're chosen side by side, neither blocking the other). Built-in
backends plus any globally-discoverable `[agent.profiles.*]` entries, each
with its known model list (empty means "any model accepted" -- the board
falls back to free-text model entry).

```json
{ "agents": [
  { "id": "claude-code", "kind": "builtin", "backend": "claude-code", "models": ["sonnet", "opus", "haiku", "fable"] },
  { "id": "ollama", "kind": "builtin", "backend": "ollama", "models": [] }
], "default_agent": "claude" }
```

### `GET /api/config/templates`
The Simple task form's template picker (RAL-297): the effective
`[[templates]]` list (see `docs/simple-task-templates.md` for the schema)
and the `[ui] new_task_default_tab` default. `using_fallback` is `true` when
zero valid templates are configured and `templates` is just the built-in
`hello-world` fallback -- the board disables the picker and shows a tooltip
in that case.

```json
{ "templates": [{ "name": "hello-world", "label": "Hello World", "description": "...", "fields": [], "prompt_template": "{prompt}" }],
  "default_new_task_tab": "simple", "using_fallback": true }
```

### `POST /api/generate`
Kicks off one "generation step" (RAL-297: the Simple form's opt-in
"Generate Proofs"/"Generate Manual Checks" buttons) on a background thread
and returns `202` immediately with a job id -- a single one-shot LLM call
whose prompt asks for a short structured JSON list, never blocking on the
call itself (see `crate::generation`'s module doc comment for why: every
mutating request runs on the daemon's accept loop one at a time, so a
multi-second/minute blocking call there would stall every other write).

Request:
```json
{ "kind": "proof_steps", "cwd": "C:/repo", "agent": "claude-code", "model": "sonnet", "prompt_context": "Add a login form" }
```
`kind` is `"proof_steps"` or `"manual_checks"`. Response `202`:
```json
{ "id": "gen-..." }
```

### `GET /api/generate/{id}`
Poll a generation job started by `POST /api/generate`.

```json
{ "status": "running" }
{ "status": "done", "items": [{ "label": "test", "value": "cargo test" }] }
{ "status": "error", "message": "..." }
```
`404` if `id` names no job this daemon process has ever started.

### Admin flag and admin-only endpoints (RAL-332)

`GET /api/users`, `POST /api/users`, `DELETE /api/users/{name}`,
`POST /api/users/{name}/rename`, `POST /api/users/{name}/admin`,
`POST /api/users/{name}/visit`, everything under `/api/machines`,
everything under `/api/triage`, everything under `/api/secret-env-names`,
and `POST /api/projects` (registering/editing a project) all require the
current placeholder identity (`X-Ralphus-User`, falling back to
`[daemon].default_user`) to be a registered admin (`is_admin: true`).
Non-admin (or unresolved-identity) callers get `403 admin_required`.
`GET /api/projects`, `GET /api/projects/{name}`,
`GET /api/projects/{name}/validate`, and `GET /api/projects/{name}/branches`
are **not** gated -- they back the Simple task form's project/branch pickers
for every user, not just admins.

**This is a UI-level convenience gate, not a real security boundary.**
There is no verified login yet (RAL-252); anyone holding the daemon's shared
bearer token (`crate::token`) can already reach every one of these endpoints
directly. It exists only to keep the board's admin-only tabs consistent with
what the server actually accepts.

**Bootstrap exception:** before any user anywhere has ever been promoted,
every admin-gated endpoint above behaves as if the caller already is one --
otherwise a fresh instance could never register its first user or promote
one, since every path to doing so would itself be admin-gated. This window
closes permanently the instant any user is promoted, from any caller.

**Config-driven alternative:** `[daemon].default_user_is_admin` (a boolean)
applies once at daemon startup, before the bootstrap exception's window
would otherwise need to be used manually. `true` registers `default_user`
if needed and promotes it; `false` demotes it if it currently holds admin.
Config is the source of truth every restart, so this both promotes and
demotes to match it -- a hand-granted admin (via the board's Users tab)
is reverted on the next restart if this field says otherwise. Leaving the
field unset never touches admin status.

### `GET /api/whoami`
The caller's own resolved identity and admin flag -- lets a client (the
board) decide whether to show admin-only UI without needing to already know
its own claimed name.

```json
{ "name": "colin", "is_admin": true }
```
`name` is `null` when no identity resolves at all (no header, no
`default_user`); `is_admin` is `false` in that case too. Never errors.

### `GET /api/users`
List every registered placeholder user (see the **User identity** glossary
section -- this is not authentication; a name grants no permissions on its
own beyond `is_admin`'s convenience gating above). Admin-only (RAL-332).

```json
{ "users": [ { "name": "colin", "created_at_ms": 0, "is_admin": true } ] }
```

### `POST /api/users`
Register a user by name. Idempotent (re-registering an existing name is a
no-op, and never changes an existing `is_admin` value). Admin-only
(RAL-332, bootstrap-exempt).

Request: `{ "name": "colin" }`. `400` if `name` is empty. Response `200`:
```json
{ "name": "colin" }
```

### `DELETE /api/users/{name}`
Remove a registered user by exact name. Admin-only (RAL-332).

```json
{ "deleted": true }
```
`404` if no user is registered under that exact name.

### `POST /api/users/{name}/admin`
Sets or clears `name`'s admin flag. Admin-only (RAL-332, bootstrap-exempt --
this is exactly how the first admin gets promoted on a fresh instance).

Request: `{ "is_admin": true }`. Response `200`:
```json
{ "name": "colin", "is_admin": true }
```
`404` if no user is registered under that exact name.

### `POST /api/users/{name}/visit`
Records that an admin opened "Edit Profile" for `name` -- i.e. viewed the
board's Preferences page scoped to `name` instead of their own identity, by
sending `X-Ralphus-User: <name>` on `GET`/`DELETE /api/hidden*` while that
page is open. Audit-only: this endpoint does not itself read or change
anything for either user. Admin-only (RAL-332). Writes an admin-only
Cartographer row (`source: "users"`, `message: "admin viewed user
profile"`).

Response `200`:
```json
{ "admin": "colin", "target": "alice" }
```
`404` if `name` is not registered.

### Hidden items (RAL-328)

All hidden-item endpoints resolve the current placeholder identity from the
`X-Ralphus-User` request header, falling back to `[daemon].default_user` when
the header is absent. The name must be registered through `POST /api/users`.
This is caller-claimed identity, not authentication.

`GET /api/hidden` returns that user's complete set, newest first:

```json
{
  "hidden": [
    { "kind": "squad", "squad_id": "squad-000000000001", "guardian_id": null, "hidden_at_ms": 0 },
    { "kind": "review", "squad_id": null, "guardian_id": "guardian-000000000001", "hidden_at_ms": 0 }
  ]
}
```

`POST /api/hidden/squads/{id}` and `POST /api/hidden/reviews/{id}` hide an
entity. Repeating the request is a no-op and preserves the first
`hidden_at_ms`; the response is `{ "hidden": true }`. The corresponding
`DELETE` endpoints re-enable it idempotently and return `{ "hidden": false }`.
Deleting a squad or review also deletes every user's preference for it.

These endpoints return `400 current_user_required` when neither identity
source is set, `400 unknown_user` for an unregistered identity, and `404` when
a hide request names an entity that does not exist.

### `GET /api/secret-env-names`
List the user-configurable set of env-var **names** treated as secret
(RAL-281) -- additive to the value-based `from_env` redaction registry
(RAL-264, see `crate::redact`): a name on this list marks that variable's
*resolved value* as secret regardless of whether it was sourced via a
`from_env` agent-profile indirection. Consulted at the point a cell's or
proof step's final env map is merged (`daemon/src/scheduler.rs`), cached
in-memory and invalidated on every mutation below, so a change here takes
effect on the next dispatch without a daemon restart.

```json
{ "names": [ { "name": "ANTHROPIC_API_KEY", "created_at_ms": 0 } ] }
```

Seeded with a small default set the first time the underlying table is
created; never re-seeded afterward, so deleting a default is honored across
restarts.

### `POST /api/secret-env-names`
Register a new secret env-var name.

Request: `{ "name": "STRIPE_SECRET_KEY" }`. `400` if `name` is empty or not a
valid environment-variable identifier (`[A-Za-z_][A-Za-z0-9_]*`, the same
`is_valid_env_key` check `POST /api/squads/{id}/env` uses). `409` if the name
is already registered -- unlike `POST /api/users`, this does **not** upsert.
Response `201`:
```json
{ "name": "STRIPE_SECRET_KEY" }
```

### `POST /api/secret-env-names/{name}/rename`
Rename a registered secret env-var name in place.

Request: `{ "name": "NEW_NAME" }`. Same identifier validation as above (`400`
if invalid). `404` if `{name}` isn't registered; `409` if `NEW_NAME` is
already registered by a different entry. Response `200`:
```json
{ "name": "NEW_NAME" }
```

### `DELETE /api/secret-env-names/{name}`
Remove a registered secret env-var name by exact name.

```json
{ "deleted": true }
```
`404` if no such name is registered.

### `POST /api/machines`
Register (or re-register, updating its fields) a **machine provider** (RAL-185)
— the program the daemon runs to reach machines under one scheme. Lets a task,
cell, proof step or review declare `machine = "<scheme>:<uri>"`; `<uri>` is
opaque and handed to the provider verbatim.

Request:
```json
{ "scheme": "incredibuild", "program": "/opt/ralphus/incredibuild.sh", "description": "build farm", "args": [], "protocol_version": 1 }
```
`args` are prepended before the verb, so one program can back several schemes.
`protocol_version` defaults to the version this daemon implements; a provider
registered against a different one is refused at dispatch rather than invoked.
`400` if `scheme` is empty, unusable as a machine scheme (it must be at least
two characters — a bare drive letter like `C:` is a path, not a machine), the
built-in `local`, or reserved (`ralphus`, which already means the worktree
placeholder and the RAL-188 entity URI). Response `201`:
```json
{ "scheme": "incredibuild" }
```

**Registration is deliberately not declarable in a task file.** A provider entry
names a program the daemon will run, so a TOML that could both *name* and
*define* one would make `POST /api/squads` equivalent to arbitrary code execution.
A task file may only ever reference an already-registered scheme.

### `GET /api/machines`
Every registered provider, plus the built-in schemes that resolve without a
registry row (so a client doesn't render them as missing).

```json
{ "machines": [ { "scheme": "incredibuild", "description": "...", "program": "/opt/ib.sh", "args": [], "protocol_version": 1, "created_at_ms": 0 } ],
  "builtin": ["local"] }
```

### `GET /api/machines/{scheme}`
One registered provider by exact scheme (case-insensitive). `404` if not
registered — built-in schemes are not returned here, only by the list route.

### `DELETE /api/machines/{scheme}`
Deregister a provider. `404` if it was not registered. Response `200`:
```json
{ "deleted": true }
```
Deliberately does **not** check whether any stored squad still references the
scheme: those squads resolved their machines at submit time, so a historical
record should not block cleaning up the registry. A *new* submission naming a
deregistered scheme fails at submit.

### `POST /api/machines/{scheme}/check`
Probe one provider for reachability (RAL-185 Q3) by invoking its `ping` verb.
Explicit and on-demand only — never polled, since a probe spawns the provider
program and a board refreshing every couple of seconds would turn that into
steady load on a build farm. The result is persisted (`last_check_ms` /
`last_check_ok` / `last_check_note` on the provider row) so the Machines tab
shows the last known answer with its timestamp rather than implying live
truth. The built-in `local` scheme always reports reachable without spawning
anything. Response `200`:
```json
{ "ok": true, "note": "loopback provider on windows, uri='probe'" }
```

### `POST /api/machines/cleanup`
Tear down one provisioned workspace (RAL-201) by invoking its `cleanup` verb.
Body names the **full** `machine` value plus the registered `project`, since
Phase 4 lets one provider hold many projects' worth of durable clones, each
with many worktrees — `machine` alone no longer identifies a single
workspace. `branch` scopes to one worktree; omit it to remove the whole
project directory (repository plus every worktree):
```json
{ "machine": "incredibuild:A", "project": "ralphus", "branch": "RAL-169-foo" }
```
**Never called automatically by the daemon** — a workspace is retained after a
squad finishes exactly like a local worktree is, so this is an explicit,
operator-initiated reclaim. `502 provider_error` on failure, with the
provider's own reason verbatim; nothing is discarded on failure since the
daemon keeps no record of the workspace to roll back (`provision` re-derives
it deterministically every time). `400` if `machine` resolves to `local`
(there is nothing to clean up), names an unregistered/unresolvable machine,
`project` is empty, or the named project has no registered clone URL. `404`
if `project` names no registered project.
Response `200`:
```json
{ "ok": true, "removed": "/srv/ralphus/projects/ralphus-a1b2c3d4e5f60708" }
```

See [`docs/machine-providers.md`](machine-providers.md) for the provider
contract (verbs, JSON envelope, versioning) and the publishing model.

### `GET /api/machines/targets/health`
Check every configured `[machine.targets.*]` entry (RAL-355 Phase 9):
`ralphus check health --all-remotes`'s daemon-side counterpart. Always
computed live — never cached or polled, so calling this is itself the
"user-triggered check" the CLI/board surface. Bounded to 8 concurrent
per-target probes. Each target's checks stop at the first failure that would
make every later check fail identically (an unreachable machine skips
straight past `capabilities`/`remote_root`/git checks rather than repeating
the same connectivity failure five times).
```json
{
  "ok": true,
  "any_fail": false,
  "targets": [
    {
      "target": "devbox",
      "machine": "ssh:devbox",
      "checks": [
        { "name": "ssh_reachable", "status": "pass", "detail": "reachable" },
        { "name": "capabilities", "status": "pass", "detail": "os=linux, arch=x86_64, async_exec=true, terminal=false" },
        { "name": "remote_root", "status": "pass", "detail": "create/read/rename/delete all succeeded under \"/srv/ralphus\"" },
        { "name": "git_version", "status": "pass", "detail": "git version 2.43.0" },
        { "name": "git_user.name", "status": "pass", "detail": "Ralphus Bot" },
        { "name": "git_user.email", "status": "warn", "detail": "no global git user.email is set for the remote account -- commits will fail until it is, unless every repository sets it per-repo instead" },
        { "name": "push_credentials", "status": "warn", "detail": "not verified -- no safe, non-mutating way to confirm push authorization exists yet" }
      ]
    }
  ]
}
```
`500 internal_error` only on a config-loading failure (a malformed
`.ralphus.toml`) — an individual target's own failures are reported inside
its own `checks`, never as an overall error, so one unreachable machine
never hides every other target's results.

### `POST /api/clear`
Bulk-delete tasks and reviews (RAL-13).

Request (all fields optional):
```json
{ "states": ["done", "failed"], "keep_temporary": false }
```
With no `states` filter, wipes everything — all squads (and their
cells/tasks/proofs/events) and all guardians (and their branches) — and
resets the id sequences so ids restart at 1. A non-empty `states` list deletes
only squads in those states (with their children) and leaves guardians and the id
sequences untouched. An unknown status is a `400`. Unless `keep_temporary` is
true, on-disk review worktrees for deleted guardians are purged. Response `200`:
```json
{ "squads_deleted": 3, "guardians_deleted": 1, "worktrees_purged": 1 }
```

### `POST /api/squads/{id}/edit`
Edit one node's definition fields. `kind` selects what is addressed and which
other keys are read:

| `kind` | Also required | Editable keys |
|---|---|---|
| `squad` | — | `label` |
| `task` | `task_idx` | `name`, `project`, `model` |
| `cell` | `task_idx`, `cell_idx` | `cwd`, `agent`, `model`, `prompt`, `command`, `auto_compact_threshold`, `maximum_tool_output_tokens`, `system_prompt` |
| `proof` | `task_idx`, `proof_scope`, `cell_idx`, `proof_idx` | `model`, `maximum_tool_output_tokens` |

Every editable key is optional and uses the same three-state convention: the
key **absent** leaves the field untouched, present-but-empty (`""`) **clears**
it back to NULL, and present-and-non-empty **sets** it. There is no way to
distinguish "set to empty string" from "clear" — clearing is the meaning.

`prompt` and `command` stay mutually exclusive: whichever of the two the
caller supplies wins and clears the other; supplying neither leaves both
as they were.

`auto_compact_threshold` and `maximum_tool_output_tokens` are integers and must
be **positive** — `0` and negatives are rejected with `400`, mirroring
`core::validate`'s `check_positive_number` at submit time so an edit cannot
store a value a task file would have been rejected for. A non-numeric value is
likewise a `400`.

`maximum_tool_output_tokens` is additionally rejected with `400` when the agent
that would run the node has no delivery mechanism for it (RAL-333) — accepted
only for `claude-code`/`claude-cli`, `codex`/`codex-cli` and `pi`. A cell is
gated on its effective agent (the `agent` in this same request if given, else
the stored one); a proof step is gated on its stored `agent`, which a task file
cannot set directly -- `[[task.cell.proof]]` has no `agent` key, so the column
is populated from the owning cell/task at submit time, and a later
`kind: "cell"` edit of `agent` does not rewrite it. A custom
`[agent.profiles.*]` name is deferred rather than rejected, the same way `core`
defers it. Clearing the field needs no such check.

Editing resets execution state, scoped as narrowly as the edited node allows:
a `squad` label edit touches nothing, a `task` edit resets the whole squad to
Pending, a `cell` edit resets only that cell and its downstream, and a `proof`
edit resets only that step and later steps in its scope. Any in-flight worker
the reset covers is cancelled and waited out first.

Request:
```json
{ "kind": "cell", "task_idx": 1, "cell_idx": 0, "maximum_tool_output_tokens": "25000" }
```
Response `200` is the full squad view, the same shape
[`GET /api/squads/{id}`](#get-apisquadsid) returns, reflecting the edit and any
state reset it caused.

### `POST /api/squads/{id}/restart/preview`
Dry-run preview of [`POST /api/squads/{id}/restart`](#post-apisquadsidrestart)
(RAL-104): computes the exact same downstream-impact set the real restart
would dirty — every cell/task in the squad, plus every squad transitively
dependent on it — without mutating anything. The librarian shows this before
the user confirms a restart, so the preview and the real restart can never
drift out of sync (both call the same `Store::compute_squad_restart_impact`).
Response `200`:
```json
{
  "cells": [{ "task_idx": 0, "idx": 0, "task_name": "build", "cell_id": "work" }],
  "tasks": [{ "idx": 0, "name": "build" }],
  "dirtied_squads": [{ "id": "squad-000000000002", "label": "downstream squad" }]
}
```

### `POST /api/squads/{id}/restart`
Restart a whole squad and cascade dirtiness downstream (RAL-19): the squad and all
its nodes reset to Pending, and every squad that transitively depends on it is
also reset to Pending so it re-runs once this squad finishes again (cross-squad
gating holds each dependent until its upstreams are Done). Response `200`:
```json
{ "state": "pending", "dirtied": ["squad-000000000002", "squad-000000000003"] }
```

Every restart endpoint below (this one, cell restart, task restart, and
the two proof restarts) accepts an optional JSON body attaching a
human-authored context note to the restart (RAL-174), surfaced to the
restarted cell via the Ghost system (see `GET
/api/ghosts/{owner_uri}`'s `user_note` above):
```json
{ "note": "you were stopped midway through the migration; the schema change is already applied", "apply_to_all": false }
```
Both fields are optional; an empty/missing/malformed body is treated as "no
note" and the restart behaves exactly as it did before this field existed.
`note` is trimmed and capped to `ghost::MAX_CONTENT_CHARS` (4000 chars).
`apply_to_all` (default `false`) controls how far the note reaches: **off**
writes it only onto the exact target(s) this restart directly targets (the
whole squad's cells for a squad restart, the one cell for a cell
restart, the task's own cells for a task restart, the owning cell/task
for a proof restart); **on** additionally writes it onto every cell
downstream of a target within the squad's dependency graph — the same
"children" the restart's own downstream-impact cascade already resets to
Pending. The note itself never accumulates across restarts (it replaces
whatever was there before); only the normal agent-authored ghost content
keeps rolling up as usual.

### `POST /api/squads/{id}/cells/{task_idx}/{cell_idx}/restart/preview`
Dry-run preview of
[`POST /api/squads/{id}/cells/{task_idx}/{cell_idx}/restart`](#post-apisquadsidcellstask_idxcell_idxrestart)
(RAL-104): the target cell plus every cell downstream of it within the
squad, the tasks that own any of those cells, and every squad transitively
dependent on this one — computed without mutating anything, same response
shape as the squad-level preview above. A non-integer index is a `400`.

### `POST /api/squads/{id}/cells/{task_idx}/{cell_idx}/restart`
Restart a single cell: it and every cell downstream of it within the squad
reset to Pending (upstream cells stay Done and are skipped on re-run), the
squad goes back to Pending, and dependent squads are dirtied. Same response shape as
above. A non-integer index is a `400`. Accepts the optional `note`/`apply_to_all`
body documented under [`POST /api/squads/{id}/restart`](#post-apisquadsidrestart)
above (RAL-174) — here the exact target is this one cell.

### `POST /api/squads/{id}/tasks/{task_idx}/restart/preview`
Dry-run preview of
[`POST /api/squads/{id}/tasks/{task_idx}/restart`](#post-apisquadsidtaskstirestart)
(RAL-150): every cell the task owns, plus every cell downstream of any
of them within the squad, the tasks that own any of those cells, and every
squad transitively dependent on this one — same response shape and computation
philosophy as the cell-level preview above (`Store::compute_task_restart_impact`).
A non-integer index is a `400`.

### `POST /api/squads/{id}/tasks/{task_idx}/restart`
Restart a whole task (RAL-150): every cell it owns, plus every cell
downstream of any of them within the squad, resets to Pending; the squad and each
affected task go back to Pending; dependent squads are dirtied. The
task-granularity counterpart of the squad/cell restarts above — same response
shape. A non-integer index is a `400`; an unknown task index is a `404`.
Accepts the optional `note`/`apply_to_all` body documented under
[`POST /api/squads/{id}/restart`](#post-apisquadsidrestart) above (RAL-174) — here
the exact target is the task's own (directly-owned) cells.

### `POST /api/squads/{id}/cells/{ti}/{si}/open-terminal?mode=agent`
On a **finished** cell, spawns a resume terminal (`claude --resume`, `codex
resume`, or `pi --session`, depending on the cell's agent) on the daemon
host — unchanged from before RAL-288 Stage 6.

On a cell that is **still running**, this instead (RAL-288 Stage 6):
1. Requests a clean detach — the cell's live process is stopped without
   reporting it as `Done` or `Failed`; it stays paused (still `Running`),
   not resolved.
2. Blocks (bounded, a few seconds) until the cell's tmux session is
   confirmed genuinely gone. "Detach requested" is not the same guarantee
   as "detach happened" — a second process touching the same conversation
   before the first one has actually released it is what corrupts a
   session, so this wait is not optional.
3. Opens the *real* interactive agent — the actual CLI, not a relay — inside
   a new, named tmux session, so closing the terminal window doesn't kill
   it; reattach later the same way as any other tmux-backed session.

A `409 still_running`-style response (surfaced via whatever the daemon's
error path returns) means the detach didn't complete in time; retry. A
`409 no_claude_session` means the cell has no recorded agent session to
resume from yet. Backend-agnostic — the same flow applies to claude, codex,
and pi. Cell-scoped only, same as before — no equivalent route reaches a
proof step or a review branch resolver.

### `POST /api/squads/{id}/cells/{ti}/{si}/terminal-ticket`
Mints a short-lived, single-use ticket for the **remote** Open Agent terminal
relay (RAL-355 Phase 10) — the WebSocket-based counterpart to
`open-terminal?mode=agent` for cells that run on a `machine`, since that
route only ever spawns a terminal window on the daemon's own desktop, which
makes no sense for a cell that isn't running there. No body.

Eligibility is checked at mint time (and re-checked, independently, when the
WebSocket connection actually opens — see below): the cell must carry a
`machine`, its `agent` must be Claude Code-family, and it must have a
recorded `agent_session_id` to resume. `400 not_remote` /
`400 unsupported_agent` / `409 no_claude_session` cover those three cases in
order. `503 relay_unavailable` means the daemon's terminal-relay listener
itself never came up (its own port was unavailable when the daemon started —
this disables *only* the remote terminal relay, not the rest of the daemon).

On success:
```json
{ "ticket": "<opaque, single-use, short-lived>", "port": 7891, "path": "/terminal" }
```
`port` is the daemon-host port the relay listens on (the main API port + 1,
same host). A client connects a WebSocket to
`ws://<daemon-host>:<port><path>?ticket=<ticket>&squad_id=<id>&task_idx=<ti>&cell_idx=<si>&cols=<n>&lines=<n>`
(`cols`/`lines` optional, default `80`/`24`). The ticket is consumed on the
first connection attempt regardless of outcome — a rejected connection (bad
ticket, cell no longer eligible, relay busy) still burns it; mint a fresh one
to retry. Once connected, the relay sends/receives raw bytes as WebSocket
binary frames — this is a byte-for-byte terminal, not a JSON API, matching
the provider-side `terminal` verb's own raw-passthrough contract (see
`docs/machine-providers.md`). The session ends the moment the connection
closes — there is no reattach; open a fresh ticket + connection to resume
(see `docs/machine-providers.md`'s "Session lifecycle" note).

### `POST /api/squads/{id}/cells/{ti}/{si}/resume-automation`
Hands a detached cell (see above) back to unattended execution, continuing
the *exact same* agent conversation rather than starting fresh — unlike a
generic `restart_cell`. No body.

Marks the cell to resume its own recorded `agent_session_id` on its next
dispatch, then resets it to `pending` the same way `restart_cell` does. A
`409 no_claude_session` means there's no recorded session to resume. A
`409 still_running` means the cell's own tmux session is still alive —
calling this on a genuinely still-running cell would start a second process
racing the live one, so it's rejected rather than attempted.

### `POST /api/squads/{id}/env`
Set (`set`) and/or remove (`unset`) persistent environment-variable overrides
on a squad (RAL-150). Body:
```json
{ "set": { "RALPHUS_RESOLVER_MODEL": "qwen3:8b" }, "unset": ["SOME_OLD_FLAG"] }
```
At least one of `set`/`unset` must be non-empty (`400` otherwise). Every key in
either map/list must be a valid environment-variable identifier
(`[A-Za-z_][A-Za-z0-9_]*`) — checked with `crate::config::is_valid_env_key`,
both to catch typos early and because
`crate::tmux::build_command_line_with_env` relies on the same validation as a
shell-injection backstop for the tmux-wrapped runner path. Every `set` value
must also be free of control characters (`\n`, `\r`, ESC, NUL, ...) — checked
with `crate::config::is_valid_env_value` (`400` otherwise) — since on Windows
a cell's launch command is delivered via `send-keys` typed
keystroke-by-keystroke into a live pty, where an embedded `\n` would act like
pressing Enter mid-command (RAL-227). `set` entries win over `unset` when a
key appears in both. Overrides are **persistent** (not a
one-shot retry parameter): once set, a key stays applied to every
cell/proof-step subprocess this squad spawns — across any number of future
retries/restarts — until explicitly unset. They do not themselves trigger a
re-run; pair this with `.../retry`, `.../restart`, or a task/cell/proof
restart to actually re-execute something under the new values (the CLI's
`ralphus retry <selector> --environment KEY=VAL` does exactly that in one
step). Response `200` is the resulting full override map:
```json
{ "RALPHUS_RESOLVER_MODEL": "qwen3:8b" }
```
Every change is also recorded to Cartographer with the changed key names in
the clear but **values redacted unless the key is in the project's
`[env_overrides].allowlist`** (`.ralphus.toml`) — see `EnvOverridesConfig` in
`daemon/src/config.rs`. The squad detail view (`GET /api/squads/{id}`,
`SquadView.env_overrides`) shows the raw, unredacted current values, since that
view is scoped to whoever already has squad-detail access rather than a shared
audit log.

#### Hierarchical env overrides (task/cell/proof layers)

Env overrides can also be set at task, cell, and proof-step granularity,
each overriding its parent scope's value for the same key:

```
squad  <  task  <  task.proof     <  that step        (a task's own proof steps)
squad  <  task  <  cell  <  cell.proof  <  that step
```

i.e. a cell inherits the squad's and its task's overrides but wins on a
shared key; a cell-scoped proof step additionally inherits+overrides
whatever its owning cell resolved to; and **one individual proof step**
(RAL-191) wins over even the scope-wide proof layer. Same request/response
shape, same validation, persistence, and Cartographer-redaction rules as
`POST /api/squads/{id}/env` above — only the scope and endpoint differ:

| Endpoint | Scope |
|---|---|
| `POST /api/squads/{id}/tasks/{ti}/env` | This task's own overrides — win over the squad's, apply to every cell under this task. |
| `POST /api/squads/{id}/tasks/{ti}/proof/env` | This task's own (task-scoped) proof-step overrides — win over the task's own (and the squad's), for *all* of this task's proof steps. |
| `POST /api/squads/{id}/tasks/{ti}/proof/{vi}/env` | **One** task-scoped proof step's own overrides (RAL-191) — win over everything above. |
| `POST /api/squads/{id}/cells/{ti}/{si}/env` | This cell's own overrides — win over its task's (and the squad's). |
| `POST /api/squads/{id}/cells/{ti}/{si}/proof/env` | This cell's own (cell-scoped) proof-step overrides — win over the cell's own (and its task's/squad's), for *all* of this cell's proof steps. |
| `POST /api/squads/{id}/cells/{ti}/{si}/proof/{vi}/env` | **One** cell-scoped proof step's own overrides (RAL-191) — win over everything above. |

The per-step layer exists because `proof` is an array: two `[[task.proof]]`
blocks setting the same key to different values must not collide, which a
single scope-wide row cannot express.

A non-integer task/cell/proof index is a `400`; an unknown
task/cell/proof step is a `404`. The resolved map for each scope rides
along in the corresponding `TaskView`/`CellView`
(`env_overrides`/`proof_env_overrides` fields) and, for the per-step layer,
`ProofView.env_overrides`, inside the existing `GET /api/squads/{id}` response —
there are no separate GET routes for these.

**TOML-declared environment (RAL-172, extended by RAL-191):** a
`[[task]]`/`[[task.cell]]`/`[[task.proof]]`/`[[task.cell.proof]]`
block may set its own `environment` table (`environment = { KEY = "value" }`)
right in the submitted TOML. `core::validate::validate_toml` enforces the same
identifier rule as above (`[A-Za-z_][A-Za-z0-9_]*`) plus string-only values
before the submission is ever accepted. At submit time (`Store::insert_squad`)
this seeds that task's/cell's/step's own `env_overrides` row — the exact
column the matching `POST .../env` endpoint writes to — so from then on a
TOML-declared value is indistinguishable from one set later via the API,
participates in the same precedence chain, and can be changed or unset the
same way.

#### `GET .../env` — resolved environment views (RAL-324)

Every `POST .../env` route in this document has a read-only `GET` twin on the
**same path** that returns what that surface actually resolves to, rather than
the one layer it owns. Three extra `GET`-only paths round out the set:
`/api/guardians/{id}/build-env`, `/api/guardians/{id}/tests-env` (the check
gates, which run under the build step's own override layer — there is
deliberately no `POST .../tests-env` to edit), and
`/api/guardians/{id}/manual-checks-env`.

```json
{
  "scope": "cell",
  "label": "cell 0/1 of squad-000000000001",
  "layers": ["agent profile", "squad", "task", "cell"],
  "vars": [
    {
      "name": "API_URL",
      "value": "https://staging",
      "redacted": false,
      "source": "cell",
      "layers": [
        {"layer": "squad", "value": "https://prod", "redacted": false, "effective": false},
        {"layer": "cell",  "value": "https://staging", "redacted": false, "effective": true}
      ]
    },
    {"name": "MY_TOKEN", "value": "[REDACTED]", "redacted": true, "source": "squad", "layers": [...]}
  ],
  "redacted_count": 1,
  "note": "Values are masked only for env-var names registered in the Secrets tab. ...",
  "warning": null
}
```

`layers` is the precedence order, lowest first — the same order the scheduler
merges in, so the last layer naming a variable wins. Each `vars` entry carries
every layer that mentioned it, so a shadowed parent value stays visible.

**Redaction.** A value is masked iff its **name** is registered in the Secrets
tab (`GET /api/secret-env-names`, RAL-281), applied through the shared
`ralphus_core::redact::redact_env_value`. There is deliberately no
"looks like a secret" name heuristic: an unregistered credential-looking
variable is shown in full, which is what `note` says out loud. Because the
masking happens in the daemon, the board's popup and `ralphus <noun> env`
receive byte-identical payloads and neither can be used to bypass the other.

**Not included**, by design: the runner process's inherited OS environment,
placeholder expansion (`{{worktree}}` and friends are shown unexpanded — the
scheduler materializes them at dispatch), and any key a layer tombstones away
(it is not part of the resolved environment, so it is not listed as one).
`warning` is set when a layer could not be resolved and was left out — today
only an agent profile whose backend/config does not resolve.

A non-integer index is a `400`; an unknown squad/task/cell/proof step/
guardian/branch is a `404`.

**CLI:** `ralphus squad env <squad_id>`, `ralphus task env <selector> [--scope
task|proof]`, `ralphus cell env <selector> [--scope cell|proof]`, `ralphus
proof env <selector>`, and `ralphus review env <selector> [--scope
build|tests|manual-checks|worktree]`. All five are read-only and build the
same paths above (`cli/src/commands/env.rs`).

**Board:** a "🔎 Resolved env" button on every `environment overrides` section
and on a review's `check gates` section, opening a read-only popup table.

#### `POST /api/guardians/{id}/branches/{bid}/env` — review-worktree overrides

A review worktree is assembled from a cell's work, so by default it runs
under **that cell's resolved environment** (`squad < task < cell`): the
conflict resolver, the dedicated final-proof pass, reviewer-feedback routing,
and this branch's check gates are all spawned with it. Without that, an agent
resolving conflicts would verify the code against a different environment than
the one it was written under.

This endpoint layers per-branch changes on top. Unlike every other layer it
takes **three** operations, because the values are inherited rather than the
branch's own to begin with:

```json
{ "set": { "API_URL": "https://staging" }, "unset": ["DEBUG"], "clear": ["TOKEN"] }
```

| Field | Meaning |
|---|---|
| `set` | Override an inherited value, or add a variable the cell never had. |
| `unset` | **Tombstone** — remove the inherited variable from this worktree's environment entirely. |
| `clear` | Drop this branch's own entry, so the key goes back to inheriting the cell's value. |

At least one of the three is required (`400` otherwise), every key must be a
valid env-var identifier (`400` otherwise), and an unknown branch is a `404`.
Applied in the order `clear` → `unset` → `set`, so the last operation naming a
given key wins deterministically.

The response (and `BranchView.env_overrides` in `GET /api/guardians/{id}`) is a
`{key: value|null}` map, where `null` is a tombstone. `BranchView` also carries
`inherited_env` (what the source cell resolves to, before this layer) and
`resolved_env` (the effective environment the worktree actually runs under), so
a client can show which keys are inherited, overridden, added, or unset without
recomputing the merge.

A branch with no source cell — added manually, or whose cell was deleted
— inherits nothing; its own overrides are the whole environment, and a
tombstone for a never-inherited key is a harmless no-op.

The **combined** review worktree spans every enabled branch at once, rebased
onto the last one in stack order, so anything that runs against it — the
finalize-time build/check-gate step and the manual-checks step (see below) —
uses the last enabled branch's own resolved environment (highest `position`)
instead of one earlier branch's or a
merge of all of them: that branch's code is what's actually checked out at the
worktree's tip. Disabled branches are never candidates, since their commits
are not in the combined worktree either. This is exposed on `GuardianView` as
`combined_env` (`{key: value}`, no per-key provenance — it's already a plain
resolved environment, not an overridable layer itself).

**Remote caveat:** a check gate running on a remote machine still runs without
these overrides — `remote_runner::RunRequest` has no env field, so rather than
half-applying them the remote path is left exactly as it was.

#### `POST /api/guardians/{id}/build-env` / `.../manual-checks-env` — combined-worktree step overrides (RAL-203)

Two more three-operation (`set`/`unset`/`clear`) layers, same request/response
shape and validation as the branch-env endpoint above, each independently
shadowing the guardian-level `combined_env` baseline described above:

| Endpoint | Governs |
|---|---|
| `POST /api/guardians/{id}/build-env` | The finalize-time build/check-gate step run against the combined worktree (`final_checks` in `guardian_merge.rs`) — explicit `checks`, the project's `.ralphus.toml [review] auto_build`, or an AI-inferred build command. |
| `POST /api/guardians/{id}/manual-checks-env` | The manual-checks step: the LLM-suggested commands run via `ralphus review checks run` or the board's "Run all" (`guardian_run_manual_commands` in `server.rs`), and `[[review.action]]` hints, which share the same execution path. |

Setting one never affects the other — a build-only override does not leak into
manual-checks, and vice versa. The response is the same `{key: value|null}`
raw-override-layer shape as branch env; the resolved, effective environment
for each step rides along on `GuardianView` as `build_env_overrides`/
`build_env` and `manual_checks_env_overrides`/`manual_checks_env`
respectively — `build_env`/`manual_checks_env` are `combined_env` with that
section's own overrides applied, mirroring `BranchView.resolved_env`.

**CLI:** `ralphus review build-env <selector> --set KEY=VAL --unset KEY --clear
KEY` and the `manual-checks-env` equivalent. These *override-editing* commands
never print a resolved or overridden environment-variable *value* (only key
names and override/tombstone/inherited status) — see
`redact_guardian_env_values`'s redaction of every map-shaped `env` field in a
`/api/guardians...` response in `cli/src/client.rs`. `ralphus review env`
(RAL-324) is the deliberate exception and the one place values are printed:
its row-shaped payload carries the daemon's own Secrets-tab masking, which is
exactly what the board shows.

### `POST /api/squads/{id}/add-dependency`
Wire up a manual cross-squad dependency after submission (RAL-105), e.g. from the
board's "Add Dependency" right-click menu. Body:
```json
{ "target_id": "squad-000000000001" }
```
Appends `target_id` to `id`'s `[[default]] depends_on` list — the same list
`ralphus submit` populates from TOML — so it is picked up by the existing
whole-squad gating (`Store::list_ready`) with no new scheduling path: `id` will
not be scheduled until `target_id` reaches Done. `target_id` itself is not
modified. A dependency that is already present is a no-op. Returns `200` with
the updated squad. A self-reference or a reference that would create a cycle in
the cross-squad dependency graph is a `409`; an unknown `id`/`target_id` is a
`404`; a malformed body is a `400`.

### `GET /api/guardians/{id}`
A single review's full detail, including its ordered `branches` list.

Each branch also carries `is_empty` (`bool`, RAL-190): `true` when the branch
rebased cleanly but adds **no diff** over the branch beneath it in the stack.
That almost always means its task never committed its work — the review would
otherwise reach `in_review` looking entirely healthy while containing none of
that task's changes, since proof steps check the *code*, not whether it was
committed.

This **fails the merge**: the branch's `merge_status` becomes `failed` and the
guardian's status `merge_failed`. A *task* producing no changes is legal, but a
branch in a review stack is there to contribute something, and approving a
review that carries none of it is worse than stopping. The escape hatch for a
deliberately-empty branch is to disable it, which drops it from the stack while
keeping it visible and re-enableable.

`is_empty` is kept as its own field rather than folded into the failure so a
client can say *why* the merge failed — the board renders it as an `⌀ empty`
badge that takes precedence over the generic conflict badge, since the fix here
is to go look at the task's cell rather than at a diff.

Each branch (`BranchView`) carries two rebase-progress fields (RAL-145):
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

`GuardianView` also carries this review's own agent cost (RAL-193) --
conflict-resolution and verifier LLM calls made by the guardian merge
machinery, deliberately excluding the cost of the tasks/cells that fed
into the review: `maximum_budget_usd` (`f64|null`, from `[[review]]`'s
`maximum_budget_usd`; `null` means no cap), `merge_attempt` (`i64`, bumped
once per rebase/re-merge attempt), `attempt_tokens_in`/`attempt_tokens_out`/
`attempt_cost_usd` (scoped to the current `merge_attempt` only), and
`cumulative_tokens_in`/`cumulative_tokens_out`/`cumulative_cost_usd` (summed
across every rebase/re-merge attempt this review has gone through -- the
value `maximum_budget_usd` is enforced against). Unlike a cell's
`cost_usd` (see the cost-semantics note below), these guardian-level totals
genuinely accumulate: they're computed by summing the `guardian_costs` table
(one row per resolver/verifier call), not read off a single overwritten
column, so no Cartographer-side aggregation is needed to see the full
picture. Once `cumulative_cost_usd` exceeds `maximum_budget_usd`, the daemon
stops making further resolver/verifier calls for this review and fails it --
the same kill-switch behavior as a task/cell `maximum_budget_usd` cap
(RAL-161), just enforced against the review's own cumulative spend rather
than one subprocess's live cost.

### `POST /api/guardians/{id}/settings`
Update per-review opt-out/override settings — only the fields present in the
body are changed, everything else is left as-is. Returns the updated
`GuardianView`. Most fields are documented by their name alone (see
`GuardianSettingsBody` in `daemon/src/server.rs` for the exhaustive list).

`auto_submit_pr_stack` (RAL-317, optional boolean) opts this review into
auto-submitting/growing its PR stack as each branch reaches a terminal
(`done`/`conflict_resolved`) merge state, instead of requiring the manual
`POST .../pull-requests` (or `review pr submit`) call. `null`/omitted inherits
the project/global default (same "stamped from the project's effective value
at review creation, then independently editable per-review" shape as
`match_pr_branch_name` above); `GuardianView.effective_auto_submit_pr_stack`
is the resolved value the trigger actually gates on. A per-branch auto-submit
failure never blocks this review's merge -- it's recorded as a one-shot
`BranchView.auto_submit_error` marker, cleared again the next time that
branch's state is found already covered by an open PR (whether via a fresh
auto-submit or a manual `review pr submit`).

RAL-213: if the review is currently `merging`, the settings write also stops
the in-flight merge and starts a fresh one (the same safe cancel → wait →
reset → restart sequence `cancel_and_merge` uses), so the new setting takes
effect on this build rather than only the next one. This is best-effort — a
restart hiccup is logged, not surfaced as an error, since the settings write
itself has already succeeded by that point. The returned `GuardianView`
reflects the fresh `merging` status when this fires.

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

### `POST /api/guardians/{id}/sync-pr`
Explicit "Sync PR" (RAL-273): check this review's open PRs' *live* base refs
on the forge (GitHub or GitLab) for a reorder made outside ralphus (e.g.
dragging PRs into a new order on the forge's own UI), and apply it if found.
This is the on-demand path alongside the other one that runs without a
user pressing anything: a background poll every 5 minutes scoped to reviews
with an active stack (`in_review`/`merging`). `branches/reorder` and
`branches/arrange` deliberately do *not* also trigger this check -- see the
comment on `guardian_reorder` in `daemon/src/server.rs` -- since it reads the
forge's live PR bases back and racing that against those endpoints' own
in-flight base PATCHes could misread the review's own just-applied local
reorder as external drift and undo it. Both paths converge on the same
detection+apply routine.

Always returns `202` immediately (`{"state":"checking"}`) — detection makes
forge network calls and applying a found reorder runs a full rebase, so the
result shows up asynchronously via `GET /api/guardians/{id}` (branch order,
`status`) the same way any other background rebase does. A `404` is returned
up front if `{id}` doesn't exist.

If a reorder is found: the review is claimed (`in_review` → `merging`, same
CAS the other restack triggers use); if the review was busy with something
else at that moment, RAL-273's "GitHub wins" rule applies -- the in-flight
operation is cancelled and the claim retried for a few seconds before giving
up. When that happens, `GuardianView.notice_kind` is set to
`forge_reorder_interrupted_local` (with `notice_message`/`notice_at_ms`) so
the board can show a one-time bottom-right toast; a plain reorder (nothing
was running) applies silently other than the usual Cartographer log entry.

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
/api/squads/{id}/worktrees`):
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

### `GET /api/guardians/{id}/branches/{branch_id}/messages`
One review branch's read-only feedback thread (RAL-272), oldest first:
```json
{ "messages": [ { "seq": 1, "role": "reviewer", "text": "fix the naming", "at_ms": 1783120106867 } ] }
```
`role` is `reviewer` (human) or `guardian` (triage agent). `at_ms` is when the
message was posted (Unix epoch ms); the board renders it beside each message.
Populated by `POST .../branches/{branch_id}/feedback`: the reviewer's
feedback text is persisted immediately (`role: "reviewer"`), and a short
conversational acknowledgment from the guardian follows in the background
(`role: "guardian"`), generated via `chat_client::call_direct`. The board
shows this thread only once a branch's detail view is expanded and it has at
least one message — otherwise it shows a "No feedback yet" placeholder
pointing at the `feedback` command above.

### `POST /api/guardians/{id}/pull-requests`
Submit one or more PRs/MRs for a review (RAL-117). Body:
```json
{ "prs": [ { "branch_id": "branch-000000000042", "branch_alias": "feature/foo", "title": "...", "description": "..." } ] }
```
Each item in `prs` is either **stacked** (`branch_id` set to one of the
review's stacked branches' stable ids, RAL-122 — not a stack position, which
changes under reorder) or a **whole-stack submission** (`branch_id`
omitted/`null`): a PR for *every* enabled branch that doesn't already have an
open one, each based on the branch below it — never a squashed
all-branches-in-one PR. "Already have an open one" is checked against the
forge's *live* state, not just the locally recorded row — a PR closed or
merged outside ralphus (the GitHub/GitLab UI, `gh pr close`, ...) is detected
and its branch gets a fresh PR instead of being silently skipped forever;
the stale local row is corrected to match. `branch_alias`/`title`/`description` only apply to a
stacked request (they don't make sense across N PRs at once, so they're
ignored on a whole-stack request); an omitted `branch_alias` defaults to the
`[forge] pull_request_branch_convention`-templated name (`"{name}-review"` by
default) — set `use_worktree_branch_name: true` (RAL-307) to default it to the
feature branch's own name instead, bypassing the convention entirely; either
way it's always templated from the feature branch's own name, never the
internal `guardian/guardian-<id>/...` ref. `title`/`description` default to
an LLM-synthesized suggestion from the branch's commits, conforming to the
target repo's PR template when one is found
(`.github/PULL_REQUEST_TEMPLATE.md` or
`.gitlab/merge_request_templates/Default.md`).

`use_worktree_branch_name` (RAL-307, optional boolean, applies to both a
stacked and a whole-stack request) is ignored when `branch_alias` is also set
(an explicit alias always wins verbatim). Omitted, it defers to the review's
own `match_pr_branch_name`/`effective_match_pr_branch_name` setting (see
`POST /api/guardians/{id}/settings`); `true`/`false` here overrides that
setting for this submission only, without changing the review's persisted
default.

Runs in the background (`git push` + a forge API call are both networked);
returns `202 {"status":"submitting"}` immediately. Poll `GET .../pull-requests`
for the resulting rows. PRs are pushed and opened **lowest position first**
so each one's PR base is the previous one's already-pushed alias (`A→B`,
`B→C`, `C→upstream`), whether they arrived as one stacked request or as a
whole-stack submission — the base-chain is seeded from every already-open PR
on the review, not just the ones in the current request, so submitting one
branch at a time (rather than the whole stack in one call) still chains
correctly. On GitHub, a whole-stack submission also registers/grows a
native PR stack (https://docs.github.com/en/rest/pulls/stacks) spanning every
open PR on the review, best-effort — the chained-base PRs themselves are
always correct regardless of whether that registration call succeeds.
`404` if the guardian doesn't exist; `400` for an empty `prs` list.

The resolved `branch_alias` (whether explicit or defaulted) is auto-suffixed
(`-002`, `-003`, ...) when it collides with another PR already recorded for
the same `(forge, repo)` (RAL-190) — a resubmission of the *same* branch is
exempt and keeps reusing its own prior alias. This makes a review worktree
branch reusable directly as its own PR branch (the common case: an explicit
`branch_alias` equal to the worktree's underlying branch name) without
worrying about a name clash with an unrelated review.

Reordering a review's branches (`POST .../branches/reorder` or `.../arrange`)
retargets every affected stacked PR's base in the background: each PR's base
becomes the alias of the nearest-preceding enabled branch that has its own
open PR (or the review's own base branch, if none precedes it). The local
`base_ref` record always updates; the forge PR's base is best-effort PATCHed
too (`GET .../pull-requests` reflects the recorded state either way).

### `GET /api/guardians/{id}/pull-request-stacks`

Read-only history (RAL-302): every PR row ralphus has ever created for this
review, in any state (`open`/`merged`/`closed`/`dropped`), grouped by the
single "submit a stack" call that created it and returned most-recently-
submitted first. This is the same underlying data as `GET .../pull-requests`
(which only ever reflects live rows in practice, since callers filter to
`state == "open"`) except it is never pruned — in particular it still shows a
PR [dropped](#post-apiguardiansidpull-requests) because its linked PR merged
out-of-band while the review was mid-flight (RAL-300), which would otherwise
disappear with no way to see what was previously submitted. Display-only:
there is no endpoint to resubmit/replay a past stack from this history.

Bare JSON array, each entry:
```json
{ "stack_id": "prstack-000000000003", "submitted_at_ms": 1234567890000, "prs": [ { "...": "a normal pull-request row, see above" } ] }
```
`stack_id` groups every PR row created by the same submission call; a row
recorded before this grouping existed falls back to its own PR id, so it
still surfaces as a (single-PR) stack rather than being silently dropped from
history.

### `POST /api/guardians/{id}/pull-requests/unlink`

Guardian-wide "start this review's PR stack over" (RAL-317, `review pr
unlink`): bulk soft-drops every currently `open` PR row for this review
(`state='dropped', dropped_reason='unlinked'`) and clears its registered
GitHub-native PR stack number, so the next submission (auto or manual)
creates a fresh stack instead of trying to append to one whose PRs were just
unlinked. Never hard-deletes -- dropped rows remain visible as history via
`GET .../pull-request-stacks` (`PrStackView`/grouped by `stack_id`), same as
a PR dropped for merging out-of-band (RAL-300). A PR already `merged`/
`closed` is left untouched, not force-dropped. No request body. `404` if the
guardian doesn't exist. `200`:
```json
{ "dropped": 2 }
```

### `POST /api/guardians/{id}/stop`

Halt an in-progress rebase (status `merging`) at its next checkpoint, leaving
the review in the recoverable `merge_stopped` state (RAL-249) — **not**
cancelled. The live merge worker is told to stop (via its cancel token, so any
in-flight conflict-resolution agent is wound down) and the daemon then
atomically flips `merging → merge_stopped`; the review, its branches, and its
worktrees are kept. `POST /api/guardians/{id}/merge` on a `merge_stopped`
review resumes the rebase from the first remaining worktree; `POST
/api/guardians/{id}/cancel` still abandons it.

Returns `{"status":"merge_stopped"}` on success. Because the state flip is
guarded on the review actually being `merging`, a merge that had already
completed before the worker was stopped is left in `in_review` (the endpoint
reports a `store_error`), rather than being mis-labelled as stopped.

### `POST /api/guardians/{id}/reopen`

Reopen a `cancelled` review: flips `cancelled → collecting` and immediately
tries an incremental **staged** merge pass (RAL-265,
`guardian_merge::run_merge_staged`) — the same pass a task completion would
have triggered via the scheduler's `start_reviews` had this review not been
cancelled at the time. This is deliberately not the all-or-nothing pass `POST
/api/guardians/{id}/merge` uses: that path waits for *every* enabled
branch's upstream cell to finish before rebasing anything, so a review
reopened while one branch is still pending would sit idle until that last
cell completes even though earlier branches were already done (and would
already be rebased in, had the review never been cancelled). Reopening
stages that ready prefix in immediately instead of waiting on the last cell
or the periodic maintenance sweep. There is no live merge worker to stop
first — a cancelled review's worker already exited before the `cancelled`
status was written.

`404` if the guardian doesn't exist; `500` (`store_error`) if the guardian
isn't currently `cancelled` (mirrors `cancel_and_merge`'s own error mapping
for an invalid transition). On success: `202 {"status":"merging"}` — a
background staged-merge worker was started (respecting the daemon's normal
worker concurrency cap, so a busy daemon queues rather than blocking this
call). The worker rebases the contiguous prefix of branches whose cells are
already done and returns the review to `collecting` if any branch is still
pending — it only reaches `in_review` once every enabled branch is done.

### `GET /api/pull-requests`
Look up the ralphus PR row for a given forge PR/MR (the PR → worktree
direction), query params `forge` (`github`|`gitlab`), `repo` (URL-encoded), and
`pr_number`. `404` if nothing is recorded for that combination; `400` if any
param is missing.

### `GET /api/pull-requests/index`
RAL-362: a flat, single-query index of every PR row across every guardian —
distinct from `GET /api/pull-requests`, which looks up exactly one row by
forge/repo/pr_number. Backs the board's Tasks tab review/PR badge lane, which
needs every open PR's source task in one request rather than one lookup per
row. No forge calls; each row is annotated with the squad/task/cell its
branch was most recently submitted from (`null` if that source cell has since
been deleted, or the branch was added manually):
```json
[
  {
    "id": "pr-abc123",
    "guardian_id": "g-1",
    "branch_id": "b-1",
    "branch_alias": "feature/foo",
    "forge": "github",
    "repo": "acme/widget",
    "pr_number": 43,
    "pr_url": "https://github.com/acme/widget/pull/43",
    "state": "open",
    "created_at_ms": 1700000000000,
    "updated_at_ms": 1700000001000,
    "source_squad_id": "sq-1",
    "source_task_idx": 0,
    "source_cell_idx": 2
  }
]
```

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
doesn't exist. Refuses (`4xx`, via the same guard `pull-from-pr` exists to
resolve — see below) rather than force-pushing over a PR branch that has
commits the review worktree doesn't, e.g. a reviewer pushed a fix directly to
the open PR branch instead of leaving a comment (RAL-190).

### `GET /api/pull-requests/{pr_id}/sync-status`
Live drift check between the PR's remote `branch_alias` branch and its owning
review worktree (RAL-190) — fetches the remote branch and compares tips via
`git merge-base --is-ancestor` in both directions:
```json
{
  "remote_sha": "abc123...",
  "local_sha": "def456...",
  "last_pushed_sha": "abc123...",
  "in_sync": false,
  "pr_ahead": true,
  "worktree_ahead": false
}
```
`pr_ahead` means the PR branch has commits the review worktree doesn't (a
reviewer pushed directly to it — the board should offer "Pull PR commits");
`worktree_ahead` means the reverse (the review worktree has commits not yet
reflected on the PR branch, e.g. right after resolving feedback — the board
should offer "Push to PR", which submitting/action-feedback already do
automatically). Both can be `false` and `in_sync` `true` when they match
exactly. `502` if the guardian/PR can't be resolved.

### `POST /api/pull-requests/{pr_id}/pull-from-pr`
Fetch the PR branch's commits and rebase them into the owning review
worktree — resolving conflicts through the same agent path a normal stacked
rebase uses — then push the merged result back to the remote and restack
every branch downstream of it in the stack (RAL-190; mirrors the existing
"manual push detected" restack, now for a *remote* push). A PR submitted from
the combined worktree routes to the topmost enabled stacked branch, the same
convention `action-feedback` uses. Runs in the background; returns
`202 {"status":"pulling_pr_commits"}` immediately. `404` if the PR doesn't
exist.

**Auth (RAL-117 Q8):** forge API tokens are read from an environment variable,
never from a config file or the database. See `crate::forge` module docs (and
the `[forge]` config section below) for the full model — the short version is
`RALPHUS_GITHUB_TOKEN` / `RALPHUS_GITLAB_TOKEN`, or a project-specific name via
`[forge].token_env`.

### `GET /api/resolve` (RAL-188)
Translate a **ralphus URI** — the self-describing, name-based addressing form
documented in [`cli-reference.md`](cli-reference.md#the-ralphus-uri-scheme-ral-188)
— into the positional coordinates every other route on this page is built on.

```
GET /api/resolve?uri=ralphus:/SQUAD[my squad]/TASK[ral-178]/CELL[work]?id=squad-000000000151
```

**Why a query parameter and not a path.** The `/` *between* URI segments
cannot sit in a REST path segment without percent-encoding as `%2F`, which
HTTP stacks and proxies routinely normalize or reject. So the URI travels as
a query value and this endpoint hands back coordinates; the positional routes
(`/api/squads/{id}/cells/{ti}/{si}/pane` and friends) are unchanged.

The `uri` value is read as **everything after `uri=` to the end of the query
string**, because a ralphus URI legitimately contains `?` and `&`
(`…?id=squad-1&combined`). Percent-encoding the whole value works identically.
No other query parameter may follow it.

Response (`200`), with absent fields omitted:

```json
{
  "uri": "ralphus:/SQUAD[my squad]/TASK[ral-178]/CELL[work]?id=squad-000000000151",
  "kind": "cell",
  "squad_id": "squad-000000000151",
  "task_idx": 0,
  "cell_idx": 0,
  "combined": false
}
```

| Field | Notes |
|---|---|
| `uri` | The **canonical** URI, rebuilt from the entity's *current* labels and always carrying `?id=`. Resolve a stale or renamed label and you get the fresh form back. |
| `kind` | `squad` \| `task` \| `cell` \| `proof` \| `review` |
| `squad_id`, `task_idx`, `cell_idx`, `proof_idx` | Positional coordinates for the squad family. |
| `proof_scope` | `task` or `cell` — which scope the addressed proof step lives in. |
| `guardian_id`, `branch_id`, `branch` | Review family; `branch_*` only when `?worktree=` was given. `?worktree=` accepts the branch's label (its feature branch name), its stable `branch-...` id, or `~<position>`; the echoed canonical `uri` always uses the label. |
| `combined` | The URI addressed the review's combined worktree (`?combined`) rather than one branch. |

Errors:

| Status | Code | When |
|---|---|---|
| 400 | `bad_request` | No `?uri=` parameter. |
| 400 | `bad_uri` | Malformed URI: unbalanced brackets, unknown segment type or query key, bad percent-escape, a segment sequence that addresses nothing, or `SQUAD[~N]`/`REVIEW[~N]` (neither has a stable position). |
| 404 | `not_found` | The URI is well-formed but names no such squad/review. |
| 409 | `ambiguous_uri` | A label or name segment matches more than one candidate, or none. The message lists them; the caller disambiguates with `?id=`. Never silently resolved. |

### `GET /api/tasks`
The full board state the librarian polls (every ~2s). Returns all squads with
their tasks, cells, and proof steps, plus daemon status. Accepts optional
query params so the board and `ralphus squad list` share one filter/sort
implementation (`daemon/src/server.rs::filter_and_sort_squads`) instead of the
board computing it in JS alone: `status` (comma-separated squad states,
case-insensitive), `name` (case-insensitive substring match on the label),
`sort` (`name` sorts by label/id ascending; anything else, including absent,
keeps the default newest-first order).

`max_concurrent` is the configured global concurrency cap (`0` means no limit).

```json
{
  "daemon": { "running": 1, "max_concurrent": 20, "running_reviews": [], "downtime_active": false },
  "squads": [
    {
      "id": "squad-000000000001",
      "label": null,
      "state": "running",
      "created_at_ms": 1783120106867,
      "started_at_ms": 1783120107200,
      "finished_at_ms": null,
      "tasks": [
        {
          "name": "build",
          "project": "myrepo",
          "agent": null,
          "model": null,
          "state": "running",
          "error": null,
          "soloed": false,
          "env_out_of_date": false,
          "started_at_ms": 1783120107300,
          "finished_at_ms": null,
          "cells": [ { "id": "cell-0", "cwd": "/repo", "agent": "claude", "model": null, "state": "done", "tokens_in": 0, "tokens_out": 0, "cost_usd": 0.0, "maximum_budget_usd": 5.0, "maximum_context": null, "auto_compact_threshold": 80000, "maximum_tool_output_tokens": 40000, "started_at_ms": 1783120107300, "finished_at_ms": 1783120115900, "env_out_of_date": false, "proof": [ { "id": "fmt", "kind": "command", "state": "done", "output": null, "spec": "cargo fmt --check", "model": null, "env_out_of_date": false } ] } ],
          "proof":   [ { "id": "tests", "kind": "command", "state": "pending", "output": null, "spec": "cargo test", "model": null, "env_out_of_date": false } ]
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
cell's `cwd` (e.g. `cwd = "/home/me/myrepo"` -> `"myrepo"`), or the literal
string `"unassigned"` when there's no cell, no `cwd`, or the `cwd` has no
filename component (e.g. `"/"`). This fallback is display-only: it never
writes back to the task's stored `project` value and has no effect on
worktree-placeholder resolution, which still requires an explicit, registered
`project`.

Each `TaskView` also carries raw nullable `agent` and `model` fields: the
task-level values submitted in TOML, before cell inheritance is applied.
These are distinct from each `CellView`'s resolved `agent`/`model` fields;
the board uses the raw task values to explain whether a cell's displayed
resolved value came from the task or was set explicitly on the cell.

`started_at_ms` (RAL-210) is epoch-ms local-machine time of the moment this
cell most recently transitioned to `running`; omitted from the JSON
(rather than `null`) until it has started at least once. A restart
overwrites it in place -- there is no separately-tracked first-start time.

`tokens_in` / `tokens_out` / `cost_usd` are reported by whichever agent
backend ran the cell or proof step, and how complete they are depends on
that backend (RAL-187):

- `cost_usd` is `0.0` whenever the backend reported **no dollar figure at
  all** — it does not mean the work was free. Codex's CLI has no cost field
  anywhere in its output, and the native pydantic backend does not compute
  one, so both always report `0.0`; only Claude Code supplies a real
  `total_cost_usd`. ralphus deliberately does **not** substitute an estimate
  from a price table here, so a consumer never sees an invented number. The
  board renders a zero/absent figure as `N/A` rather than `$0.0000 USD` for
  exactly this reason.
- Token counts update **while a cell is still running**, not only at the
  end: CLI-agent backends forward a running total over the `RALPHUS_EVENT:`
  stderr channel as each agent turn completes, which the daemon persists to
  the cell row (see the Logging Policy in `AGENTS.md`). Granularity is
  therefore per completed turn — both counts read `0` until the first turn
  finishes. Proof steps have no live channel; their counts appear once the
  step completes.
- A cell that **fails mid-run** (cancelled, timed out, or its tmux pane
  died before the runner wrote a result) still reports the tokens it had
  already spent, carried over from the last live snapshot, rather than
  collapsing to `0`.
- The values reflect the cell's **current** run only. A restart overwrites
  them rather than accumulating, so a lifetime total across attempts must be
  derived from Cartographer's event history instead.

`created_at_ms` is when the squad was submitted/queued. `started_at_ms` (on the
squad, each task, and each cell) is when it first entered `running` — `null`
until it does, and can differ from `created_at_ms` when a squad sits `pending`/
`queued` for a while before the scheduler claims it. `finished_at_ms` is when
it last reached a terminal state (`done`/`failed`/`cancelled`) — `null` while
still queued/pending/running. Together these back the Details Pane's "time
running" (live elapsed while non-terminal, frozen `finished_at_ms -
started_at_ms` once terminal) and "started at" (UTC) fields. A squad/task/
cell that is restarted has these cleared back to `null` for the part(s)
genuinely re-executing (see `Store::reset_squad_to_pending`/`restart_cell` in
`daemon/src/store.rs`).

Both *task*-level proof steps (`[[task.proof]]`, on `TaskView.proof`) and
*cell*-level proof steps (`[[task.cell.proof]]`, on `CellView.proof`)
are exposed here, each in task/cell declaration order. A proof entry carries:

- `id` — optional step id from TOML (e.g. `"fmt"`).
- `kind` — one of `command` / `prompt` / `brain` / `approval`.
- `state` — lifecycle state (`pending` / `running` / `done` / `failed` / `cancelled`).
- `output` — captured output once run; `null` before execution.
- `spec` — the step definition: command text for `command` kind, prompt text for
  `prompt` / `brain` kind, or empty string for `approval`.
- `system_prompt` — the read-only effective appended system prompt that the
  agent actually received for this proof step, including ralphus-added hidden
  instructions (proof mode, unattended execution, async retry policy, etc.).
  Omitted for `command` / `brain` / `approval` kinds, and may also be absent on
  historical rows created before August 15, 2026.
- `model` — model override for `prompt`-kind steps; `null` when unset.

`command` and `prompt` proof steps actually run (`pending` → `running` →
`done`/`failed`); `brain`/`approval` steps are accepted but deferred and stay
`pending` forever. A `prompt` step's `output` is the AI's final response
text (used to derive its pass/fail verdict), not command stdout.

Each squad also carries an `env_overrides` field (RAL-150): the squad's persistent
environment-variable overrides, as raw unredacted `{key: value}` pairs (see
[`POST /api/squads/{id}/env`](#post-apisquadsidenv)). Omitted from the JSON
entirely when empty — true for the vast majority of squads.

Each `TaskView` likewise carries `env_overrides` (this task's own overrides,
set via `POST /api/squads/{id}/tasks/{ti}/env`) and `proof_env_overrides`
(this task's own proof-step overrides, set via
`POST /api/squads/{id}/tasks/{ti}/proof/env`); each `CellView` carries the
same pair scoped to the cell (`POST /api/squads/{id}/cells/{ti}/{si}/env`
and `.../proof/env`) -- see
[Hierarchical env overrides](#hierarchical-env-overrides-taskcellproof-layers)
above for how these merge with the squad's. All four are raw unredacted
`{key: value}` pairs, omitted from the JSON when empty, same as the squad's.

Each `TaskView`/`CellView`/`ProofView` also carries `env_out_of_date`
(RAL-271): a cosmetic, non-blocking `boolean` that flips to `true` once the
row's own env overrides (or, for a task/cell, its own proof steps'
`proof_env_overrides`/per-step `env_overrides`) are edited after the row
already exists. Editing a task's `env_overrides` also marks every cell it
owns; editing a cell's `env_overrides` also marks that cell's own proof
steps -- never a sibling task/cell, and never a grandchild two ownership
levels down. It clears back to `false` when the row is reset to `pending` by
a restart/retry, or on any `POST /api/squads/{id}/set-status` call
targeting it (any state, not just a restart). Purely informational -- it has
no effect on scheduling or proof results.

Each `TaskView` also carries an `error` field (RAL-291), mirroring
`CellView.error`'s shape and lifecycle exactly: `null` unless the task
itself failed for a reason with no underlying cell/proof error to point
to. Today the only writer is the RAL-156 no-commits-since-baseline guard,
which sets it to the same message it logs to Cartographer, e.g. `"task
failed: no commits since baseline (baseline a1b2c3d, 2 cells checked)"`. A
task that failed because one of its own cells or proof steps failed leaves
this `null` -- that failure is already visible on the cell's own `error` or
the proof step's `output`. Cleared back to `null` whenever the task is
reset to `pending` by a restart, same as `CellView.error`.

For prompt-driven cells, each `CellView` may also carry `system_prompt`:
the read-only effective appended system prompt the agent actually received,
including ralphus-added hidden instructions (unattended execution, async retry
policy, ghost handoff, and any stored cell/subproject addendum). It is
omitted for command cells, and may also be absent on historical rows created
before August 15, 2026.

Each `CellView` also carries `triage_types` (RAL-318): the cell's resolved
Triage type(s) (inline `triage_type`, or the Arbiter's own classification),
alphabetical, or absent/empty for a cell that never opted into Triage
(`triage = true`). This is resolved synchronously at submit time whether or
not the cell has run yet, so a non-empty list does not by itself mean the
cell is done -- pair it with `state`. The board's cell details pane uses it
to show a "scheduled"/"queued for auto-review" placeholder in place of a
real review link for a Triage cell whose pool hasn't drained into an actual
review yet; once it has, the cell's `reviews` entry (below) takes over.

Each entry in a cell's `reviews` list (`SquadReviewRef`) also carries
`origin`: `"explicit"` (an authored `[[review]]`, or any other non-Triage
creation path) or `"arbiter"` (RAL-318: the review was created automatically
when a Triage pool's count threshold or cron schedule fired). Mirrors
`GuardianView.origin`.

### `GET /api/resources`
Per-task OS resource usage for the board's Resources tab (RAL-11). One entry per
*running* cell that currently has a live `ralphus-runner` subprocess, with its
CPU/RAM/GPU sampled and mapped back to the exact squad/task/cell. Sampling briefly
blocks (~200ms) to compute a CPU delta, so this is polled only while the Resources
tab is open.

```json
{
  "resources": [
    {
      "squad_id": "squad-000000000001",
      "squad_label": null,
      "task_idx": 0,
      "task_name": "build",
      "cell_idx": 0,
      "cell_id": "cell-0",
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
`task_idx`/`cell_idx` are the board's navigation indices (the "Go to task"
button jumps straight to that cell).

### `GET /api/config/live-view` (RAL-232)
The Live View "Show Debug Messages" checkbox's config-driven default: the
effective (global-under-project) `[live_view]` table from `.ralphus.toml`,
following the same global-under-project layering as `[cartographer]`/
`[terminal_logs]`/`[budget]` (`crate::config::load_live_view_config`).

```json
{ "show_debug_messages_default": false }
```

`show_debug_messages_default` (`[live_view]`'s only field so far) is `false`
when unset — the board's Live View panes hide ralphus's own
diagnostic/telemetry lines by default, showing only agent-produced output.
The board fetches this once at page load to initialize each pane's checkbox;
see ["Live View debug-line filtering"](#live-view-debug-line-filtering-ral-232)
below for the full design, including why this is a rendering-only default
that never touches what's persisted.

### `GET /api/squads/{id}`
A single squad's full detail (same shape as one element of `squads` above, plus the
resolved definition fields shown in the details pane).

### `GET /api/squads/{id}/graph`
The squad's internal cell dependency graph (`ralphus graph <squad_id>`), built
from the same `depends_on` resolution `daemon/src/plan.rs::plan()` uses for
scheduling — nodes are cells, not tasks, since the cell is the
schedulable unit:
```json
{
  "nodes": [
    { "id": "t0s0", "task_idx": 0, "cell_idx": 0, "task_name": "build", "cell_id": "compile" }
  ],
  "edges": [ { "from": "t0s0", "to": "t1s0" } ]
}
```
`edges[].from` must complete before `.to` may start. `404` if the squad does not
exist; `500` (`code: "cycle"`) on a dependency cycle — should not happen for an
already-submitted squad (submission itself rejects cycles), but the underlying
`plan()` call is fallible so this stays honest rather than unwrapping. Rendering
(ASCII/DOT) happens entirely client-side; see `cli/src/graphview.rs`.

### `GET /api/graph`
The cross-squad `[[default]] depends_on` gating graph (`ralphus graph --global`):
nodes are squads, edges are `[[default]]` references from one squad to another.
`?all=1` includes terminal (done/failed/cancelled) squads; by default only
active (queued/pending/running) squads are included, and a dependency reference
to an excluded/unresolvable squad produces no edge (best-effort, same philosophy
as within-squad `depends_on` resolution). Shape:
```json
{
  "nodes": [ { "id": "squad-000000000001", "label": null, "state": "pending" } ],
  "edges": [ { "from": "squad-000000000001", "to": "squad-000000000002" } ]
}
```

### `GET /api/squads/{id}/worktrees`
Per-cell git info for the detail pane's read-only rows (CCTL-148; `upstream`
added later): the cell's own worktree (`cwd`), its shared project root, and
the upstream to display. Computed on demand (runs `git` per cell), not on
the hot board path:
```json
[
  { "task_idx": 0, "cell_idx": 0, "worktree": "C:/repo/.git/.ralphus_worktrees/feat", "project": "C:/repo", "upstream": "main" }
]
```
`project`/`upstream` are `null` when `cwd` is not inside a git worktree.
`upstream` is one of two things, per the cell's `upstream = "<<task:...>>"`
sentinel (RAL-50 branch-chaining):
- **Sentinel set**: the *referenced* cell's own worktree branch name (what
  this cell's branch is rebased onto before it runs) — `null` if that
  dependency hasn't materialized a worktree yet (never falls back to the
  tracking ref below in this case, to avoid showing a misleading value).
- **No sentinel**: the worktree's own git upstream tracking branch (typically
  the non-worktree base branch it was forked from, e.g. `main`), or `null` if
  none is configured.

See `daemon/src/reviews.rs::cell_upstream_display`.

### `GET /api/squads/{id}/logs`
The event timeline for a squad: an ordered list of `{ ts, level, source, message }`
plus per-cell and per-proof log references (drives the Logs modal tabs).

### `POST /api/squads/{id}/activate`
Move a held `Queued` squad to `Pending`. Returns the new state.

### `POST /api/squads/{id}/tasks/{ti}/solo`
Solo a task within a squad (RAL-157): while any task in the squad is soloed, the
scheduler only dispatches soloed tasks' not-yet-started cells — every
other task's cells stay `pending` until un-soloed, even once the soloed
task itself finishes (a dependent task must not start racing ahead just
because its soloed upstream completed). A cell already `running` when a
sibling gets soloed is left to finish on its own — there is no per-cell
interrupt in this codebase (cancellation is squad-wide only), so pausing an
in-flight cell's task takes effect starting at that task's *next*
cell, not mid-cell. Multiple tasks in the same squad may be soloed at
once; soloing one does not un-solo another. Idempotent. Solo state is
sticky — it never auto-clears (not on the soloed task's own completion, not
on a squad restart); [`POST /api/squads/{id}/tasks/{ti}/unsolo`](#post-apisquadsidtaskstiunsolo)
is the only way to resume paused siblings. Returns the refreshed `SquadView`
(so `tasks[].soloed` reflects the change in the same round trip). An unknown
squad or task index is a `404`; a non-integer `{ti}` is a `400`.

### `POST /api/squads/{id}/tasks/{ti}/unsolo`
Un-solo a task (RAL-157) — the reverse of
[`POST /api/squads/{id}/tasks/{ti}/solo`](#post-apisquadsidtaskstisolo). Returns
the refreshed `SquadView`. Idempotent; same error responses as `solo`.

### `POST /api/squads/{id}/cancel/preview`
Dry-run preview of [`POST /api/squads/{id}/cancel`](#post-apisquadsidcancel)
(RAL-116): computes the exact same cascade-cancel impact set the real cancel
would affect — this squad plus every squad transitively dependent on it — without
mutating anything. The librarian shows this before the user confirms a
cancel, so the preview and the real cancel can never drift out of sync (both
call the same `Store::cancel_squad`). Response `200`:
```json
{ "squads": [{ "id": "squad-000000000001", "label": null }, { "id": "squad-000000000002", "label": "downstream squad" }] }
```
An unknown `id` is a `404`.

### `POST /api/squads/{id}/cancel`
Cancel a squad **and every squad transitively dependent on it** (RAL-116).
Always available and idempotent regardless of the squad's current state — even
an already-terminal squad (`done`/`failed`/already `cancelled`) is
(re-)cancelled, so it can never be picked up again by another trigger (a
restart, cross-squad gating, etc). Kills any in-flight task/cell/proof
subprocess via the same cooperative cancellation used mid-run (best-effort
termination). `POST /api/squads/{id}/set-status` with `{"kind":"squad","state":"cancelled"}`
routes through this exact same cascading cancel — not a separate DB-only flip
— so both entry points have identical effect. Response `200`:
```json
{ "state": "cancelled", "cancelled": ["squad-000000000001", "squad-000000000002"] }
```
An unknown `id` is a `404`.

### `GET /api/cartographer`
The global Cartographer log (RAL-98): every structured event in the system —
task/cell lifecycle, proof starts/results, status transitions, Guardian
review lifecycle events — as one filterable, paginated, sortable table. This
is the same view used both for "show me everything" and for "show me this
one squad/cell/guardian's history"; the latter is just this endpoint with a
`squad_id`/`cell_id`/`guardian_id` filter applied (it replaces the old
per-squad "events" sub-tab that used to be backed by `GET /api/squads/{id}/logs`).

Query params (all optional): `source`, `scope`, `level`, `squad_id`,
`guardian_id`, `cell_id`, `task` (exact match on task name, RAL-155),
`q` (substring match on message), `since_ms`, `until_ms`, `limit` (default
100, max 1000), `offset`, `sort` (`asc`/`desc`, default `desc` — newest
first) — plus `entity` (RAL-155): a single-string [entity URI](#entity-uris-ral-155)
that addresses a squad/task/cell/proof/guardian uniformly, resolved into
the equivalent `squad_id`/`task`/`cell_id`/`guardian_id` filter fields
server-side (`task_idx`/`cell_idx` are translated to the task's
name/cell's id via a store lookup, since those are what the columns
above actually store). `entity` composes with the other filter fields —
an explicit field always wins over one `entity` would have derived, since
it's the more specific ask. A malformed `entity` string is a `400`.

```json
{
  "rows": [
    {
      "id": 42,
      "at_ms": 1732300000000,
      "level": "info",
      "source": "scheduler",
      "message": "squad squad-000000000001 claimed → running",
      "scope": "squad",
      "squad_id": "squad-000000000001",
      "guardian_id": null,
      "cell_id": null,
      "task": null,
      "log_path": null,
      "payload": {},
      "admin_only": false
    }
  ],
  "total": 128
}
```

`log_path` (RAL-155) is set on rows that reference an on-disk log file — e.g.
a durable terminal-log attempt file (`crate::terminal_log`, RAL-154) — rather
than embedding that file's content in the row itself. `null` for every other
event.

`admin_only` (RAL-332) restricts a row to admin viewers: this endpoint
resolves the caller's identity from `X-Ralphus-User` (falling back to
`default_user`) and silently excludes `admin_only` rows unless that identity
is a registered admin -- no error, no count of what was hidden, the same
shape as a filter that matched nothing. Today only RAL-328's per-user
hide/unhide rows (`source: "hidden"`) and RAL-332's "Edit Profile" audit rows
(`source: "users"`) are marked this way. `GET /api/cartographer/{id}` applies
the same rule and returns `404` (not `403`) for an admin-only row a
non-admin caller asks for by id, indistinguishable from a missing one.

Retention is enforced by two independently configurable caps under
`[cartographer]` in `.ralphus.toml` (`retention_days`, default 30;
`max_rows`, default 50000) — either condition triggers pruning, checked
every 10 minutes by the scheduler.

### `GET /api/cartographer/{id}`
One Cartographer row's full detail, by its `id`. `404` if it does not exist
(e.g. already pruned).

### `POST /api/events/ticket` (RAL-222)
Mints a short-lived (30s), single-use ticket gating [`GET
/api/events`](#get-apievents-ral-167) below. Gated by the ordinary bearer
token like any other route (it's dispatched through `route()`, not
`run_http_loop`'s SSE special-case). Response:

```json
{ "ticket": "<64-char hex string>" }
```

### `GET /api/events` (RAL-167)
A long-lived Server-Sent Events (SSE) stream: the daemon's primary push
mechanism for `board.html`, replacing its old fixed-interval polling. One
event is emitted for every [`GET /api/cartographer`](#get-apicartographer) row
written — i.e. every task/squad/cell/guardian/queue state change, proof
start/result, and Guardian review lifecycle event already flowing through
Cartographer — so this endpoint has no separate instrumentation of its own to
keep in sync. The queue view is a derived, filtered projection over
squad/task state, so a `squad`-kind event also implies "the queue may have
changed."

**Auth (RAL-222).** This endpoint can't carry the bearer token directly — it's
opened by the browser's `EventSource`, which cannot set custom request
headers, and a long-lived credential in a URL query string would leak into
access logs, proxy logs, and browser history. Instead it requires
`?ticket=<nonce>`, a short-lived single-use ticket minted via the bearer-gated
`POST /api/events/ticket` above; the ticket is consumed (and can never be
replayed) the moment a connection is accepted. A missing, unknown, expired, or
already-used ticket gets a `401` before any SSE data is written — the same
error envelope as every other route (`{"error":{"code":"unauthorized",
"message":"missing or invalid events ticket"}}`). Like the bearer token
itself, this check is skipped entirely when the daemon has no token
configured (`Daemon::new`/`serve_with`, used by tests). `board.html` fetches a
ticket immediately before opening each `EventSource` connection (including on
reconnect, since a dropped connection's ticket is already spent) and the
librarian's proxy (`proxy_events_stream`) relays the browser's `?ticket=...`
straight through — the daemon is the only party that mints or validates them.

Each event's `data:` payload is exactly one Cartographer row (same shape as
`GET /api/cartographer`'s `rows[]` entries above); its SSE `event:` name is
one of three kinds, derived from which entity references the row carries:

| `event:` name | When |
|---|---|
| `guardian` | The row carries a `guardian_id` — a review changed (branches, feedback, checks, merge/rebase state, ...). |
| `squad` | The row carries a `squad_id` but no `guardian_id` — a squad/task/cell (and, by extension, the queue view) changed. |
| `other` | Neither — still Cartographer-worthy, but not scoped to one squad or guardian (e.g. daemon-wide startup recovery events). |

A connection with nothing to report for 15s receives an SSE comment line
(`: heartbeat`) instead, so idle proxies/browsers don't decide it's dead. The
librarian proxies this endpoint straight through to the browser byte-for-byte
(see `librarian/src/server.rs::proxy_events_stream`) rather than inventing its
own event model — the daemon is the source of truth for state changes.

This is a fan-out broadcast, not a queryable log: a subscriber only sees
events published *after* it connects (use `GET /api/cartographer` for
history/backfill), and a slow/stalled client's channel (bounded, capacity
256) silently drops events past that bound rather than blocking the rest of
the daemon — `board.html`'s own 60s reconciliation poll (a `tick()` fallback,
not the primary path) covers any resulting gap.

### Entity URIs (RAL-155)
A single-string, index-based way to address any squad/task/cell/proof/
guardian entity — shared, cross-cutting infrastructure used as the
`GET /api/cartographer` `entity=` filter above, and mirrored in the CLI
(`ralphus.entity_uri`, bridging the CLI's human-typed, name-or-index
`ralphus.selector` grammar into this wire format via
`from_resolved_selector`) and the daemon (`daemon/src/entity_uri.rs`, the
authoritative grammar the other two mirror). Grammar:

```text
squad:<squad_id>
task:<squad_id>:<task_idx>
cell:<squad_id>:<task_idx>:<cell_idx>
proof:<squad_id>:<task_idx>:<proof_scope>:<cell_idx>:<proof_idx>
guardian:<guardian_id>
```

`task_idx`/`cell_idx`/`proof_idx` are the same 0-based indices the HTTP
routes already use (`/api/squads/{id}/cells/{ti}/{si}/...`). `proof_scope`
is `"task"` or `"cell"`; `cell_idx` is `-1` for a task-scope proof.
Examples: `task:squad-000000000001:0`, `cell:squad-000000000001:0:1`,
`proof:squad-000000000001:0:cell:1:0`, `guardian:guardian-000000000001`.

### `GET /api/squads/{id}/timeline`
Generates and returns the merged, chronological "uber-log-viewer" for a
whole squad (RAL-155): every Squad/Task/Cell/Proof state transition and
every other Cartographer event scoped to the squad, plus terminal-log excerpts
inlined from any `log_path`-carrying rows, sorted by `(at_ms, id)` ascending
and rendered as one plain-text narrative. As a side effect, the rendered
text is (best-effort) written to a temp file on the daemon's host — a fresh
generation on every call, not a persistent export (RAL-155 Q5) — at a fixed
per-squad path under the OS temp directory. `404` if the squad doesn't exist.

```json
{
  "meta": {
    "squad_id": "squad-000000000001",
    "generated_at_ms": 1732300005000,
    "start_ms": 1732300000000,
    "end_ms": 1732300004000,
    "event_count": 37,
    "terminal_log_count": 3,
    "task_count": 2,
    "cell_count": 4,
    "truncated": false,
    "gaps_possible": false
  },
  "entries": [
    {
      "at_ms": 1732300000000,
      "level": "info",
      "source": "submit",
      "scope": "squad",
      "task": null,
      "cell_id": null,
      "message": "squad inserted",
      "log_path": null,
      "log_excerpt": null
    }
  ],
  "text": "=== ralphus uber-log timeline: squad squad-000000000001 ===\n...",
  "file_path": "C:\\Users\\...\\Temp\\ralphus-timeline-squad-000000000001.log"
}
```

`event_count` is capped at a conservative default (2000 rows) per
generation — `truncated: true` means the squad has more history than fit.
`gaps_possible: true` means the squad's own inaugural `"squad inserted"`
Cartographer row is missing from the returned rows, which reliably indicates
Cartographer's retention pruning has already removed some of this squad's
earliest history — this endpoint is best-effort (RAL-155 Q6), with no
obligation to reconstruct pruned history. The board's "⏱ Timeline" button
(next to "📄 Logs") calls this same endpoint and renders `text` in a modal.

### `GET /api/squads/{id}/cells/{ti}/{si}/debug-events` (and its proof/guardian siblings) (RAL-296)
The same chronologically-merged Cartographer-row-plus-inlined-terminal-log-
excerpt stream `GET /api/squads/{id}/timeline` builds for a whole squad
(`crate::timeline::entity_debug_timeline`, sharing that same merge code),
scoped down to one cell/proof step/guardian branch resolver/guardian
manual-checks run and trimmed to its **most recent attempt only** — a
detach + restart cycle's earlier attempt(s) are not stitched into the same
stream. This is what backs both the Live View pane's "Show Debug Messages"
checkbox and the "Open Terminal Log" attempt-history popup, so the two no
longer disagree with each other. Bare JSON array of `SquadTimelineEntry`
(the same shape as one entry of `GET /api/squads/{id}/timeline`'s
`entries`), ascending by time.

Four equivalent routes, one per entity kind, matching the same
`(squad_id, task, cell_id)` key convention the sibling `GET
.../terminal-log-attempts` routes already use for "browse past attempts":
- `GET /api/squads/{id}/cells/{ti}/{si}/debug-events`
- `GET /api/squads/{id}/proofs/{task_idx}/{scope}/{cell_idx}/{proof_idx}/debug-events`
- `GET /api/guardians/{id}/branches/{branch_id}/debug-events`
- `GET /api/guardians/{id}/manual-checks/debug-events`

```json
[
  {
    "at_ms": 1732300000000,
    "level": "info",
    "source": "runner",
    "scope": "cell",
    "task": "build",
    "cell_id": "worker",
    "message": "tmux session started",
    "log_path": null,
    "log_excerpt": null
  },
  {
    "at_ms": 1732300004000,
    "level": "info",
    "source": "runner",
    "scope": "terminal_log",
    "task": "build",
    "cell_id": "worker",
    "message": "terminal log attempt 0 written (ralphus_squad-000000000001_build_worker)",
    "log_path": "C:\\Users\\...\\terminal_logs\\ralphus_squad-000000000001_build_worker\\0000.log",
    "log_excerpt": "...tail of that attempt's terminal-log content..."
  }
]
```

### `GET /api/ghosts/{owner_uri}`
Fetch one "ghost" (RAL-136) — a short, best-effort handoff note a task
cell or Guardian review worktree published for whoever picks up dependent
work next (open questions, places it struggled, things it noticed but didn't
fix; deliberately **not** a changelog of what's already recoverable from `git
log`). `owner_uri` is the publisher's stable id: `cell:{squad_id}:{task_idx}:
{cell_idx}` for a task cell, `review:{guardian_id}:{branch_id}` (or
`review:{guardian_id}:combined`) for a review worktree. There is at most one
ghost row per owner — a cell/review that publishes again merges onto its
existing note rather than adding a second row. `404` if that owner has never
published one.

Content isn't only the agent's own self-report: when the daemon can determine
the ground-truth pass/fail of a scope's proof/check step(s), it folds an
advisory note onto the same ghost (RAL-152), e.g. "the prior run's 2/2
proof/check step(s) passed -- you internally validated that the code works.
... re-test/re-verify the existing work first rather than assuming it's
broken." This is phrased as a hint, not a guarantee — it can go stale (e.g. a
rebase or conflict resolution since it was written) — and applies wherever a
ghost is written: task cell restarts, proof-only restarts, and Guardian
resolver restarts.

```json
{
  "owner_uri": "cell:squad-000000000001:0:0",
  "kind": "cell",
  "squad_id": "squad-000000000001",
  "guardian_id": null,
  "content": "Left the retry loop untuned -- the 3rd flaky test case needs a longer backoff.",
  "user_note": "you were stopped midway through the migration; the schema change is already applied",
  "revision": "a1b2c3d4",
  "created_at_ms": 1732300000000,
  "updated_at_ms": 1732300000000
}
```

`revision` is an opaque, VCS-agnostic marker (currently a git commit sha when
the publishing worktree is a git repo, `null` otherwise) for best-effort
staleness reasoning — nothing re-validates it automatically. A cell's own
prior ghost, and its direct dependencies' ghosts (one level up in the task
graph only), are already injected into its prompt automatically at cell
start; this endpoint is for explicit lookups beyond that (tooling, the CLI,
one review branch checking another's notes).

`user_note` (RAL-174) is free-form text a human attaches when triggering a
restart via the board's restart popup — see the `note`/`apply_to_all` body
documented on the restart endpoints below. Unlike `content`, it is *not*
merged/accumulated on repeated writes: each restart's note replaces whatever
was there before, while `content` keeps rolling up as usual. `null` when no
one has ever attached a restart note. When present, it's injected into the
restarted cell's prompt as its own distinct line at the very bottom of the
prior-context block (after every agent-authored note, never interleaved with
it).

### `POST /api/ghosts/copy`
Explicitly copy `source_uri`'s ghost onto `target_uri`, independent of the
dependency graph (e.g. seeding a brand-new task cell with a prior
investigation's findings). Merges onto whatever `target_uri` already has,
same as any other ghost write.

```json
{ "source_uri": "cell:squad-000000000001:0:0", "target_uri": "cell:squad-000000000002:1:0" }
```

`target_uri`'s prefix (`cell:`/`review:`) determines the copy's owner
kind — only the two URIs are needed, not every column of the target row.
`400` if `target_uri` doesn't parse as a `cell:`/`review:` URI; `404` if
`source_uri` has no ghost to copy. Response `200` is the resulting `GhostView`
(same shape as [`GET /api/ghosts/{owner_uri}`](#get-apighostsowner_uri)).

### `ralphus history`/`ralphus listen` (RAL-140) — no new daemon endpoints
`ralphus history <cell|proof ID> [--live]` and `ralphus listen <ID>
--until <status>` are CLI-only compositions over the endpoints already
documented above; RAL-140 deliberately adds no new daemon routes or storage
tables of its own:

- **`ralphus history <ID>` (no `--live`)** — a one-shot, non-blocking
  snapshot. If the id's tmux session is currently live, this is the same
  `.../pane` content the board's "Show Live View" reads (see `GET
  .../cells/{ti}/{si}/pane` / `GET .../proofs/{task_idx}/{scope}/{cell_idx}/{proof_idx}/pane`
  above). Once the tmux session is gone, `.../pane` itself may still return a
  persisted last-pane-content snapshot (RAL-102 follow-up —
  `crate::tmux::write_pane_snapshot`/`read_pane_snapshot`, a read-only
  historical record of what the pane last showed) if one was captured, but
  `ralphus history` prefers a more stable, curated record over a raw
  transcript replay — it falls back to whatever was already durably
  persisted for that entity by mechanisms that predate this ticket:
  - A **cell**'s fallback is its RAL-136 ghost — `GET
    /api/ghosts/cell:{squad_id}:{task_idx}:{cell_idx}` (see above). A
    `404` (no ghost ever published) is rendered as "no history recorded yet",
    not an error.
  - A **proof** step's fallback is its already-stored `output` text (part of
    a squad's `GET /api/squads/{id}` response since long before this ticket) —
    ghosts have no per-proof granularity, so there is nothing new to add
    here either.
- **`ralphus history <ID> --live`** — blocks and tails the live `.../pane`
  endpoint every second, diffing each poll against a small cursor
  (`length` + a short trailing-content fingerprint) the CLI persists to
  `~/.ralphus/history_cursors/` so a restarted CLI process resumes from where
  it left off instead of re-printing already-seen output, and so multiple
  independent watchers of the same cell never share (or clobber) a read
  position. Fails immediately with a clear error if nothing is currently
  live for the id (`--wait-until-valid [SECONDS]` opts into waiting instead).
  Once the pane goes inactive, the cell/proof's ghost/output (see above)
  is printed as one final, separately labelled block — it's a short curated
  note, not a continuation of the raw tmux transcript just tailed, so it is
  never diffed against the tailing cursor.
- **`ralphus listen <ID> --until <status>`** — polls the existing `GET
  /api/squads/{id}` (for a squad/task/cell/proof selector) or `GET
  /api/guardians/{id}` (for a review/review-worktree selector) endpoint once
  a second until the resolved entity's status equals the caller-supplied
  `--until` value, then exits. No log/tmux content is involved.

### Live View liveness signal (RAL-170)

Every `GET .../pane` response (`GET .../cells/{ti}/{si}/pane`, `GET
.../proofs/{task_idx}/{scope}/{cell_idx}/{proof_idx}/pane`, `GET
.../guardians/{id}/branches/{branch_id}/pane`, `GET
.../guardians/{id}/manual-checks/pane`) carries a `last_activity_ms` field
alongside `active`/`content`:

```json
{ "active": true, "content": "...", "last_activity_ms": 1739400000123 }
```

`last_activity_ms` is the Unix-epoch-milliseconds time the daemon last
observed *fresh* pane output (strictly more lines than the previous poll)
for this session — `null` once the session has ended or before it has
produced any output yet. The board's Live View ("peek box") uses it to show
an absolute "last activity" timestamp and to distinguish a quiet-but-healthy
long-running command (e.g. a `cargo test` pass inside a Guardian
quality-check) from one that has silently stopped producing output.

**Design decision: tracked continuously, daemon-side, in-memory only —
not on-demand only while a Live View is open.**
`SubprocessRunner::run_via_tmux_attempt` already polls every running
tmux-wrapped session's pane on a fixed 500ms cadence regardless of whether
any client is watching (it needs to, to detect the completion sentinel and
forward `RALPHUS_EVENT:` marker lines) — RAL-170 piggybacks on that existing
poll, storing the timestamp as an in-process `Store` field
(`Store::note_live_activity`/`live_activity_ms`/`clear_live_activity`,
`daemon/src/store.rs`) keyed by the same deterministic `crate::tmux::session_name`
used for task cells, proof steps, and Guardian resolver/manual-check
sessions alike. Two consequences of that choice:

- **Always fresh when read.** Because tracking never depends on a Live View
  being open, there's no "stale from before the view opened" problem to
  solve — the value read by any `GET .../pane` call is whatever the poll
  loop most recently observed, full stop.
- **Deliberately not a DB column.** A SQLite `UPDATE` on every pane-growth
  tick would scale with the number of *concurrently running* sessions —
  fine at "hundreds" (a few hundred writes/sec against the single
  `Store` mutex, comfortably inside SQLite WAL throughput), a real
  contention/write-amplification risk at "thousands" sharing that one
  mutex, for a value nobody needs once the process exits. An in-process
  `HashMap<String, i64>` entry is a pointer-sized insert instead of a WAL
  write, and is removed (`clear_live_activity`) as soon as the owning
  `run_via_tmux` call returns for good — so memory stays bounded by
  currently-running sessions, not lifetime history, and the write cost is
  independent of how many Live Views happen to be open (zero, one, or many
  clients polling the same pane all read the same already-computed value).

If the "thousands of concurrent processes" scale is ever actually reached
and the per-tick `HashMap` insert itself becomes measurable (unlikely — it's
already gated to at most once per 500ms per session by the same poll
interval that drives event forwarding), the next step would be debouncing
the insert further (e.g. only update if the value has advanced by more than
some threshold), not switching to persistent storage.

### Live View debug-line filtering (RAL-232)

Every tmux-wrapped cell/proof/resolver runs its `ralphus-runner` subprocess
directly inside the pane the board's Live View reads, so the pane's raw text
interleaves the agent's own output with ralphus's own diagnostic/telemetry
lines: the `ralphus [TYPE] message ...` convention
([`.agent/logging-policy.md`](../.agent/logging-policy.md)'s log-format table),
`RALPHUS_EVENT:` Cartographer markers (`runner/src/cartographer.rs`), and the
`RALPHUS_TMUX_DONE` tmux-completion sentinel (`daemon/src/runner.rs`). A
per-pane "Show Debug Messages" checkbox lets a viewer choose whether to see
those lines or just the agent's.

**This is a rendering-only concern — nothing about capture or persistence
changes.** `GET .../pane` (`capture_pane_reply`) always returns the full,
unfiltered pane content, exactly as before RAL-232; so do the persisted
last-pane-content snapshot and every durable terminal-log attempt file
(`crate::terminal_log`) and Cartographer row. The filtering
(`stripDebugLines`/`isRalphusDebugLine`) happens entirely in
`librarian/assets/board.html`, applied only to the text already rendered in
a peek box:

- The checkbox's *default* state comes from
  [`GET /api/config/live-view`](#get-apiconfiglive-view-ral-232) — `false`
  (agent-only) unless overridden by `[live_view].show_debug_messages_default`
  in `.ralphus.toml`.
- A viewer can override that default per-pane for the rest of the browser
  session by toggling the checkbox; this is never sent back to the daemon.
- Line classification is intentionally conservative: a line only counts as
  ralphus's own if one of the markers above starts the (trimmed) line, not
  merely appears in it — otherwise an agent reading or grepping ralphus's own
  source (which contains these strings verbatim) would have its own tool
  output misclassified, the same false-positive class
  `daemon/src/runner.rs::pane_shows_done_sentinel` already guards against for
  the completion sentinel specifically. This means the classifier can miss a
  ralphus line that doesn't match one of these prefixes (e.g. a future log
  convention) — see `test/debug-strip.test.mjs` for the classifier's exact
  coverage.

### Personal watches and notification preferences (RAL-320)

A *watch* is one row binding the acting user to an [entity URI](#entity-uris-ral-155)
(squad/task/cell/proof/review/review-worktree) plus the mailbox priority
tiers (`urgent`/`high`/`normal`) that watch cares about. Watching a parent
entity cascades: its notifications also cover every entity nested under it
(watching a squad covers its tasks, cells, and proof steps). All five
endpoints below require an acting user, resolved from `?user=<name>` or
`.ralphus.toml`'s `default_user`; `400 bad_request` if neither is set.

#### `GET /api/watches`
Every watch the acting user owns.

```
GET /api/watches?user=colin
```
```json
{
  "watches": [
    {
      "id": "watch-000000000001",
      "user_name": "colin",
      "entity_uri": "squad:1a2b3c",
      "notify_tiers": ["urgent", "high"],
      "created_at_ms": 1730000000000
    }
  ]
}
```

#### `POST /api/watches`
Watch (or re-watch) an entity on the acting user's behalf. Re-watching the
same `(user, entity_uri)` pair updates its notify tiers in place rather than
creating a duplicate row.

```
POST /api/watches?user=colin
{ "entity_uri": "squad:1a2b3c", "notify_tiers": ["urgent"] }
```
`notify_tiers` is optional; omitted or empty defaults to the acting user's
`default_notify_tiers` preference (see below), falling back to every tier if
that user has no stored preferences. `400 bad_request` if `entity_uri` isn't
a recognized entity URI. Response `201` with the created/updated watch (same
shape as one entry in `GET /api/watches`'s array).

#### `DELETE /api/watches/{entity_uri}`
Stop watching an entity.

```
DELETE /api/watches/squad%3A1a2b3c?user=colin
```
`200 {"deleted": true}`, or `404 not_found` if the acting user wasn't
watching that entity.

#### `GET /api/users/{name}/preferences` / `POST /api/users/{name}/preferences`
A per-user preference pair: `auto_watch` (when `true`, submitting a squad
automatically watches the submitted entity at that user's default notify
tiers) and `default_notify_tiers` (the notify-tier fallback used whenever a
watch — including auto-watch's implicit one — doesn't specify its own tiers
explicitly). `POST` registers `name` first if it isn't already a known user,
same as `POST /api/watches`.

```
POST /api/users/colin/preferences
{ "auto_watch": true, "default_notify_tiers": ["urgent", "high"] }
```
`default_notify_tiers` is optional; omitted or empty means every tier. Both
routes respond with the same `UserView` shape (`GET` is `404 not_found` for
an unregistered user; `POST` never is, since it registers on demand).

#### `GET /api/mailbox/personal/messages` / `POST /api/mailbox/personal/drain`
The acting user's personal mailbox: the same broadcast mailbox stream
(`GET /api/mailbox/{client_id}/messages`) filtered down to messages whose
entity is covered by one of that user's watches and whose priority clears
that watch's notify tiers. Not a separate message store — same rows, a
narrower, per-user read. `GET` accepts the same `?unread=true` and
`?priority=urgent|high|normal` filters as the broadcast mailbox; `POST`
accepts the same `{"message_ids": [...]}` body (or an empty body to drain
every unread message) as `POST /api/mailbox/{client_id}/drain`, scoped to the
acting user.

## Notes on future evolution

- Single-secret bearer-token auth landed in RAL-219 (see
  ["Authentication"](#authentication-ral-219) above). Per-user identity and
  attribution are still **not** in this draft; when multi-user hardening
  begins, a `submitted_by` field on squads (keyed off something richer than one
  shared token) is the expected addition (see `FOLLOW.local.md` #3).
- Live updates are poll-based for now (librarian polls `GET /api/tasks`); an SSE
  or WebSocket channel is a later optimization.
