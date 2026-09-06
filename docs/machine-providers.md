# Machine providers (RAL-185)

A task, cell, proof step or review may declare which **machine** it runs on:

```toml
[[task]]
name    = "ral-169"
project = "ralphus"
machine = "incredibuild:A"
```

`incredibuild` is a **provider scheme**; `A` is an **opaque URI** the daemon
never interprets. The daemon looks the scheme up in its provider registry to
find an executable, and hands the URI to that executable verbatim. What `A`
means — a build-farm slot, a hostname, a URL, a container tag — is entirely the
provider's business.

> **Status:** implemented — registry, `machine` syntax, validation, remote
> cell/proof execution, and every documented verb (`provision`, `exec`,
> `status`, `stream`, `cancel`, `run`, `read-file`, `write-file`,
> `remove-path`, `ping`, `capabilities`, `channel`, `cleanup`), each
> dispatched genericly through the same registry with no daemon-side
> branching on scheme. Guardian
> reviews now dispatch to a remote review's assigned machine too (RAL-185
> Phase 3/RAL-201) — the merge worktree, stacked rebase, conflict-resolution
> agent, chat/feedback/summary agent invocations, and check gates all route
> through the review's machine. See `REMOTE.local.md` for the full phase
> history and its "Known limitations" section for what still runs local-only
> even on a remote review (build-config resolution, the empty-branch VCS
> check, and a couple of local-fs reads around rebase-state detection).

## The `machine` value

| Form | Meaning |
|---|---|
| *(unset)* | The daemon's own host. Every pre-RAL-185 task file behaves exactly as before. |
| `local` | The daemon's own host, stated explicitly. Case-insensitive. |
| `<scheme>:<uri>` | Resolved through the provider registry. |

Rules for `<scheme>`:

- Letters, digits, `_` and `-` only.
- **At least two characters.** This exists solely so a pasted Windows path
  (`C:\build\wt`) fails with a clear "that's a path, not a machine" error
  instead of parsing as provider `C` and surfacing much later as a mystifying
  "provider C is not registered".
- Matched case-insensitively.

`<uri>` is opaque and only has to be non-empty. It may contain colons, slashes,
anything — `some_provider:https://useful.com/x` passes
`https://useful.com/x` through untouched.

### Inheritance

`machine` inherits exactly like `agent` and `model`:

```
task.machine
  └─ cell.machine             (overrides the task)
       └─ proof.machine       (overrides the cell)
task.proof.machine            (overrides the task)
review.machine                (independent of every task)
```

A review's machine is **not** derived from its contributing tasks. A review may
run on a machine none of its tasks used.

### Built-in schemes

One scheme always resolves with no registry entry:

| Scheme | Meaning |
|---|---|
| `local` | The daemon's own host. |

`ralphus` is additionally **reserved** — it already means the worktree
placeholder (`ralphus:new-worktree/<branch>`) and the entity URI
(`ralphus:/SQUAD[...]`), so registering it as a third thing is rejected.

Attempting to register a provider under either name is rejected.

## Registering a provider

Registration is an **administrative action**, done over the API (or the CLI
wrapping it):

```bash
curl -X POST http://127.0.0.1:7890/api/machines \
  -d '{"scheme":"incredibuild","program":"/opt/ralphus/incredibuild.sh","description":"build farm"}'

curl http://127.0.0.1:7890/api/machines
curl http://127.0.0.1:7890/api/machines/incredibuild
curl -X DELETE http://127.0.0.1:7890/api/machines/incredibuild
```

| Field | Required | Meaning |
|---|---|---|
| `scheme` | yes | Unique provider name, matched against the left half of a `machine` value. |
| `program` | yes | Absolute path to the program the daemon invokes. |
| `description` | no | Shown in listings and error messages. |
| `args` | no | Arguments always prepended before the verb, so one executable can back several schemes. |
| `protocol_version` | no | Contract version this provider implements. Defaults to the current version. |

Re-registering an existing scheme **upserts** it, matching how project
registration behaves.

### Why this is not declarable in TOML

A provider entry names an executable the daemon will run. If a submitted task
file could both *name* and *define* a provider, then `ralphus submit` would be
equivalent to arbitrary code execution by anyone who can write a TOML file.

So a task file may only ever **reference** an already-registered scheme.
Registering one is a separate, explicit administrative action. There is
deliberately no TOML syntax for it, and none should be added.

## The provider contract

A provider is an executable invoked as:

```
<program> [<args>...] <verb> --uri <uri> [verb-specific flags]
```

It reads any payload on stdin and writes a single JSON object to stdout. Every
response carries at least:

```json
{ "ok": true, "protocol_version": 1 }
```

On failure:

```json
{ "ok": false, "protocol_version": 1, "error": "human-readable reason" }
```

### Verbs

Every request/response struct named below is defined in
`daemon/src/remote_runner.rs` (line numbers as of RAL-201; search the file for
the struct name if they've since drifted — each carries `#[derive(Serialize)]`
or `#[derive(Deserialize)]` matching the direction it's used in).

| Verb | Flags | Request struct | Purpose |
|---|---|---|---|
| `provision` | `--uri` | [`ProvisionRequest`](../daemon/src/remote_runner.rs) (`project: String`, `source: WorkspaceSource { kind, url?, branch? }`, `squad_id: String`, `cell_id: String`) | Ensure a workspace exists; reply `{"workspace": "<abs path on this machine>"}`. Must be idempotent — a re-run after a daemon restart reuses the existing workspace (no daemon-side handle backs this; see "Restart safety" below). |
| `exec` | `--uri` | The full cell spec (`RunnerSpec` in `daemon/src/runner.rs`: `squad_id`, `task`, `cell_id`, `cwd`, `prompt`/`command`, `agent`, `model`, `system_prompt`, `timeout_sec`, `budget_tokens`, `maximum_budget_usd`, `proof`, `trace_context`, `env_overrides`, ...) | Run one cell/proof. Reply with **either** `{"result": {...}}` (ran synchronously) **or** `{"handle": "..."}` (started asynchronously). `result`'s shape is `RunnerResult` (`daemon/src/runner.rs`): `status` (`"done"`/`"failed"`), `tokens_in`, `tokens_out`, `cost_usd`, `summary`, `error?`, `proofed?`, `agent_session_id?`, `ghost?`. |
| `status` | `--uri --handle` | none | For an async handle: `{"state": "running"}`, or `{"state": "done"\|"failed", "result": {...}}` (`result` is the same `RunnerResult` shape as `exec`). |
| `stream` | `--uri --handle [--since N]` | none | `{"output": "...", "next": N}` — output since the cursor. Optional; omitting it costs Live View, not execution. |
| `cancel` | `--uri --handle` | none | Stop the work behind a handle. Reply is the standard `{"ok": true, "protocol_version": 1}` envelope — no extra fields. |
| `job-cleanup` | `--uri --handle` | none | Explicitly delete one terminal job's retained state. This is separate from worktree `cleanup`; providers may retain failed-job diagnostics according to their policy. |
| `run` | `--uri` | [`RunRequest`](../daemon/src/remote_runner.rs) (`cwd: String`, `program: String`, `args: Vec<String>`) | Run one VCS command in a workspace. Reply `{"exit_code": 0, "stdout": "..."}`. |
| `read-file` | `--uri` | [`FileRequest`](../daemon/src/remote_runner.rs) (`path: String`, `content: null`, `recursive: false`) | Return a file's contents. Reply `{"stdout": "<file content>"}`. A missing/unreadable file is **not** an error at the daemon layer — callers treat it as "absent" — but the provider still replies however it normally would (`ok: false` is fine; the daemon maps any failure to "absent"). |
| `write-file` | `--uri` | [`FileRequest`](../daemon/src/remote_runner.rs) (`path: String`, `content: "<text to write>"`, `recursive: false`) | Write `content` to `path`, creating parent directories as needed. Reply is the standard envelope. |
| `remove-path` | `--uri` | [`FileRequest`](../daemon/src/remote_runner.rs) (`path: String`, `content: null`, `recursive: bool`) | Delete a file, or a directory tree when `recursive` is `true`. A path that does not exist is success, not an error. |
| `ping` | `--uri` | none | Confirm the machine is reachable and ready, doing no work. Reply `{"ok": true, "detail": "..."}`. Called on demand from the board's Machines tab, never polled. |
| `capabilities` | `--uri` | none | **Optional.** Report what this provider/machine pairing supports. Reply `{"capabilities": {...}}` — see [`Capabilities`](../daemon/src/remote_runner.rs) (`os?`, `arch?`, `supported_ops: [String]`, `async_exec: bool`, `terminal: bool`, `runner_version?`). Every field is best-effort (`None`/omitted means "unknown", never "no"). A provider that does not implement this verb is not a failure — the daemon reads that the same way as "no capability information available", not an error. Must never have side effects (no runner upload, no workspace mutation) even when answering `runner_version` or `async_exec` would otherwise tempt one. |
| `channel` | `--uri` | newline-delimited `RunRequest`s | **Optional.** Serve many requests from one process: read newline-delimited JSON `run` requests on stdin, write one newline-delimited JSON response each, until stdin closes. |
| `cleanup` | `--uri` | [`CleanupRequest`](../daemon/src/remote_runner.rs) (`project: String`, `clone_url: String`, `branch?: String`, `remote_root?: String`) | Tear one workspace down — one worktree when `branch` is given, the whole project directory (repository plus every worktree) when it is omitted. Reply `{"removed": "<abs path on this machine>"}`. See "The `cleanup` verb and its retention policy" below. |
| `terminal` | `--uri --command --cols --lines` | none (raw byte stream, not JSON) | **The one verb that is not JSON request/response.** Runs `--command` on the target under an allocated pty (e.g. `ssh -tt`), with `--cols`/`--lines` setting the pty's *initial* size only (no live resize forwarding — see below). From the moment the pty connects, this process's own stdin/stdout **are** the terminal byte stream: read stdin, write it to the pty; read the pty, write it to stdout; until either side closes. Success/failure is this process's own exit code, not a trailing JSON line — see "The `terminal` verb" below. |

The cell spec arrives on stdin for `exec`; the provision request arrives on
stdin for `provision`; the file/run/cleanup requests above arrive on stdin
for their own verb. Handle-scoped verbs (`status`/`stream`/`cancel`/
`job-cleanup`) and `ping` take no stdin payload. `terminal` takes no stdin
payload either — its whole request is the four flags above.

**Synchronous vs async `exec`.** A provider that can only block returns
`result` and is done — no `status`/`stream`/`cancel` needed. A provider that
returns a `handle` gets live output and mid-run cancellation, at the cost of
implementing three more verbs. Both are first-class; pick per provider.

### The `cleanup` verb and its retention policy

`cleanup` tears a provisioned workspace down. It is **never called
automatically** by the daemon — a local cell's worktree
(`.git/.ralphus_worktrees/<branch>`) is never auto-deleted either, so a remote
workspace keeps the same property rather than being reclaimed the instant a
squad ends. An operator reclaims one explicitly, once they are actually done
inspecting it. Since RAL-355 Phase 4 a single machine can hold many projects
and many worktrees per project (`<remote_root>/projects/<name>-<hash>/
worktrees/<branch>-<suffix>`), so `cleanup` targets one project's registered
name plus its clone URL, optionally scoped to a single `branch`:

```bash
# Remove one branch's worktree only:
curl -X POST http://127.0.0.1:7890/api/machines/cleanup \
  -d '{"machine": "incredibuild:A", "project": "ralphus", "branch": "RAL-169-foo"}'
# or
ralphus machine cleanup incredibuild:A --project ralphus --branch RAL-169-foo

# Remove the whole project directory (repository plus every worktree):
ralphus machine cleanup incredibuild:A --project ralphus
```

**Retention on failure: nothing is discarded.** The daemon keeps no record of
a provisioned workspace to roll back or retry against — `provision` is
idempotent and re-derives the same workspace deterministically every time
(from the project's name/clone URL and the requested branch), so there is
nothing to "forget" on a failed cleanup. If your `cleanup` implementation
fails partway through (permissions, a process still holding the directory
open, a dead machine), leave the workspace exactly as it was and reply
`{"ok": false, "error": "..."}` — the daemon surfaces that reason verbatim to
the caller rather than swallowing it, so a human can retry or investigate. On
success, reply `{"removed": "<abs path>"}` so the operator sees exactly what
was torn down.

### The `terminal` verb

`terminal` (RAL-355 Phase 10) exists for exactly one purpose: a remote Open
Agent terminal relay, so a browser or CLI client can attach an interactive
terminal to a **remote** cell's resumed Claude Code session. It is the one
verb in the whole contract that is not JSON request/response — see the note
in the verb table above. A trailing JSON envelope would land in the middle of
a human's terminal session as garbled text, so there is nothing to parse:
success or failure is reported through this process's own exit code after
the byte-relay ends, not through anything on stdout.

The daemon never calls `terminal` directly through `ProviderRunner::invoke` —
it spawns the provider program with `terminal --uri <uri> --command <cmd>
--cols <n> --lines <n>` and piped stdio (`ProviderRunner::spawn_terminal`),
then relays bytes between that child's stdin/stdout and a WebSocket
connection itself. `--command` is the already-built resume command (e.g.
`'claude' --resume '<session-id>' --dangerously-skip-permissions`, POSIX-quoted
— see `ralphus_core::agent_resume::resume_agent_command_posix`), not
something the provider constructs.

**Claude Code only, this release.** The daemon's ticket-mint route
(`POST /api/squads/{id}/cells/{task}/{cell}/terminal-ticket`) refuses any
cell whose `agent` isn't Claude Code-family, and any cell that has no
recorded `agent_session_id` yet (nothing to resume). Codex/Pi remote
terminals are deferred until their resume paths are actually exercised
remotely, matching this codebase's existing precedent of not guessing at an
unexercised harness's shape.

**No live resize forwarding.** `--cols`/`--lines` set the pty's *initial*
size only, via `COLUMNS`/`LINES` on the remote command. A later browser
resize does not propagate — the ssh-provider implementation's own `ssh`
child has no local pty of its own to detect the change and forward it via
`SIGWINCH`, and taking on a local pty-allocation dependency for that is out
of scope for this first version. This is a disclosed limitation, not a bug.

**Session lifecycle: dies on disconnect.** There is no reattach. A closed
WebSocket connection (tab closed, network drop, CLI client exited) kills the
spawned `terminal` child and the `ssh -tt` underneath it — ending the remote
session outright. To resume, a human runs the existing headless Resume
Automation, or opens a fresh terminal relay connection (same as local Open
Agent's own resume path already works).

### Failure classification

Every remote call site here still returns a plain error message
(`Result<_, String>`) — a provider does not need to structure its errors any
particular way. But once an error reaches the daemon, `daemon/src/scheduler.rs`
classifies a *failed remote cell's* message into a
[`RemoteFailureKind`](../daemon/src/remote_failure.rs) (`unreachable`,
`prerequisite_failed`, `provision_failed`, `launch_failed`, `lost`,
`cancelled`, `timed_out`, `invalid_result`, or `unknown`) via message-text
heuristics, and records it as `failure_kind` on the cell's "cell completed"
Cartographer event — so a human or a future retry policy can tell "the
machine was unreachable" from "the workspace failed to provision" from "this
was cancelled" without parsing prose. Local cell failures never get a
`failure_kind` (`null`) — the classification only models remote
infrastructure failure modes, not local agent/proof outcomes. This is
heuristic, not a wire-protocol requirement: writing a clear, specific error
message (as every example above already does) is what makes classification
land correctly, not a schema a provider must conform to.

### Restart safety

Two different things survive a daemon restart, by two different mechanisms:

- **A provisioned workspace** has no daemon-side record at all. `provision`
  being idempotent (re-deriving the same path from `squad_id`/`cell_id`/the
  branch every time) *is* the restart-safety mechanism — see
  [`crate::worktrees::ensure_worktree`]'s identical local-only property.
- **An in-flight async `exec` handle** *is* recorded — in the
  `remote_exec_handles` SQLite table — the moment `exec` returns one, and
  cleared once that `exec` finishes by any outcome. On startup, before the
  scheduler resumes anything, the daemon calls `cancel` on every handle still
  in that table (a crash mid-poll is the only way one survives to see a
  restart) and clears the row — this stops the *old* attempt on the provider
  before a fresh one is dispatched, so a restart never leaves two copies of
  the same work running remotely at once. Implement `cancel` for real if your
  provider supports async `exec`: an unimplemented/no-op `cancel` means the
  old attempt keeps running (and, if it holds a paid resource, keeps costing)
  after the daemon has already moved on.

### `provision` and non-git projects

The provision payload's `source` is deliberately **not** git-shaped:

```json
{"project": "ralphus",
 "source": {"kind": "git", "url": "git@host:acme/repo.git", "branch": "RAL-169-foo"},
 "squad_id": "...", "cell_id": "work"}
```

`kind` comes from the registered project's own `vcs` column. `url` and
`branch` are populated only when the VCS has them — a Perforce or
plain-directory project gets `kind` alone and the provider does whatever that
source system needs. The daemon never runs `git` on a provider's behalf.

**There is no `publish` verb, by design.** See "Publishing" below.

**`run` is the hot one.** A one-branch review merge issues ~23 of them
(measured, not estimated). Per-call cost matters here more than anywhere else in
the contract — and the transport is entirely yours: a fresh SSH invocation, an
`ssh -o ControlMaster` multiplexed one, or a message into a persistent
**channel** you keep open. The contract deliberately says nothing about it, so a
LAN farm and a machine across a slow link can make different choices without
ralphus changing.

(*Channel*, not *cell*: a cell is already a ralphus concept — a task
contains cells, which contain proof steps — so the word is kept clear of
transport. See [`glossary.md`](glossary.md).)

### The `channel` verb

Implement it only if reaching your machines is expensive. It buys one spawn and
one connection handshake for a whole merge instead of ~23 of each.

Register with it advertised:

```bash
ralphus machine register --scheme incredibuild     --program /opt/ralphus/incredibuild.sh --channel
```

Requests and responses are strictly **one-to-one and in order** — the daemon
issues a merge's commands sequentially, so there is no pipelining to reconcile
and no request ids to match up.

**Getting this wrong degrades rather than breaks.** If the channel cannot be
opened, dies, returns unparseable JSON, or stops answering, the daemon logs it
and falls back to a one-shot spawn for that command. The fallback is
semantically identical, only slower. A *command* that runs and fails still
fails — that arrives as a normal reply, and is not a transport problem.

Worth knowing before reaching for it: on a Linux or macOS daemon host, SSH's own
`ControlMaster` multiplexing gets you the same amortisation with **no code at
all** — set `ControlMaster auto` / `ControlPersist` in the provider's ssh config
and keep the one-shot shape. `channel` earns its keep where that is unavailable,
which notably includes a **Windows daemon host**, since Windows OpenSSH does not
implement connection multiplexing.

`args` arrives **already split**. Never re-join it into a shell string: a branch
name containing a space, or a path containing a quote, would then be mis-parsed
on the far side. Passing the vector straight to your exec call makes that class
of bug impossible.

**`ping` is deliberately separate from every other verb.** A machine being down
is a different fact from work failing on it; conflating them leaves someone
debugging a failed squad unable to tell "the build broke" from "the build box is
unplugged". Keep it cheap — it is called to paint a status chip, not to do
anything.

### Versioning

Every response must include `protocol_version`. The daemon refuses to invoke a
provider registered against a version it does not implement, and says so
explicitly, rather than guessing at a mismatched protocol.

### A provider that only implements `provision` is not usable

"Return JSON saying the source was accepted" is roughly 5% of the contract, and
a provider that stops there cannot run anything. Accepting work is the easy
part; the daemon also needs to start it, watch it, report its token/cost usage,
cancel it, and clean it up. Budget for the whole verb set.

Three requirements are easy to miss, and the first two fail *silently*:

- **Forward `RALPHUS_EVENT:` lines.** The runner emits structured events on
  stderr behind that marker (see the Logging Policy in `AGENTS.md`). A provider
  that drops them leaves Cartographer blind for every remote cell.

  The daemon scrapes that marker from the stderr of **every** verb it invokes,
  not just `exec` — which matters most for an async provider. Its cell's
  events are produced long after `exec` returned a handle, so the only place
  left to surface them is the stderr of the later `status`/`stream` calls. Echo
  them there. `examples/providers/loopback.py`'s `cmd_stream` shows the whole
  pattern in five lines.
- **Forward `llm-invoke` usage events specifically.** The live cost-cap kill
  reads token/cost usage from them — the daemon compares the latest snapshot
  against `maximum_budget_usd` on every `status`/`stream` poll and cancels the
  handle the moment it's exceeded, the same as it does for a local cell. A
  provider that drops these events silently disables budget enforcement for
  its cells — the squad still completes, just uncapped, since there is
  nothing for the daemon to compare against.
- **Honor `trace_context`.** The cell spec carries a W3C `traceparent`
  (RAL-96). A provider that starts its work without propagating it into the
  remote environment gets a disconnected trace rather than one end-to-end
  waterfall — visible, but only if you go looking.

### Conformance tiers (RAL-355 Phase 12)

Five tiers, each strictly larger than the last. A provider implements
whichever tier its use case needs — the daemon degrades gracefully at every
verb boundary rather than requiring the whole set.

1. **Minimum, synchronous execution**: `ping`, `provision`, and `exec`
   returning a final `result` (never a `handle`). Usable, but blocking —
   no Live View, no mid-run cancellation, no daemon-restart reconciliation
   of in-flight work.
2. **Preferred, controllable execution**: tier 1 plus `status`, `stream`,
   `cancel`, `run`, and `cleanup`, with `exec` returning a `handle` instead
   of blocking. This is what buys Live View, mid-run cancellation, and
   surviving a daemon restart without losing track of in-flight work.
3. **File operations, for remote Review parity**: `read-file`, `write-file`,
   and `remove-path`. Required for a review whose worktree lives on this
   machine — Guardian's stacked-rebase merge logic hand-writes `.git`
   worktree link files and reads conflict markers back out of files, which
   `run` alone (structured VCS commands only) cannot do.
4. **Optional performance**: `channel`, for connection reuse across the ~23
   `run` calls one branch's merge issues. A provider that omits it is spawned
   per command instead and works identically, just with more per-call
   overhead — the daemon falls back automatically if a channel cannot be
   opened or stops answering.
5. **Terminal capability, for remote Open Agent**: the `terminal` verb (RAL-355
   Phase 10) — the one verb in the whole contract that isn't JSON
   request/response (see "The `terminal` verb" above). The daemon still
   brokers the actual client-facing relay itself, over its own WebSocket
   listener; a provider's job is only to turn `terminal --uri --command
   --cols --lines` into a byte-for-byte pty tunnel on its own stdin/stdout. A
   provider reports whether it implements this tier via `terminal: bool` in
   `capabilities` (`ssh-provider` reports `true`; a provider with no
   interactive-terminal story reports `false`, or omits the field entirely,
   both read the same as "not supported").

`capabilities`'s own `supported_ops`/`async_exec` fields are how a provider
reports which of tiers 1–4 it actually implements, so an operator (or a
future daemon-side check) can see a gap at registration/health time instead
of discovering it mid-Cell.

### Conformance test suite

`daemon/tests/provider_conformance.rs` exercises tiers 1–3 generically,
through the same `ProviderRunner` client the daemon itself uses to talk to
any provider — so it proves the same thing the daemon would otherwise only
discover the hard way, mid-Cell, if a provider got a verb's shape wrong.
Run against two providers:

- `examples/providers/loopback.py` (this doc's own worked example) —
  unconditionally, on every `cargo test`.
- The real, compiled `ralphus-ssh-provider` binary against the SSH Docker
  fixture — opt-in, `#[ignore]`d by default:
  ```powershell
  $env:RALPHUS_SSH_DOCKER_TEST = '1'
  $env:RALPHUS_SSH_CONFIG_FILE = (Resolve-Path .docker-ssh-target/ssh_config)
  cargo test -p ralphus-daemon --test provider_conformance -- --ignored --nocapture
  ```

Checks `ping`, `capabilities` (tolerating its absence), `provision`
(idempotent, using a non-VCS source so neither target needs a real
repository), a `write-file`/`read-file`/`remove-path` round-trip under the
provisioned workspace, `exec` terminating with a recognizable
`"done"`/`"failed"` status (a missing `ralphus-runner` install is a
conformant `"failed"` result, not a broken provider — this suite proves the
*envelope* is well-formed, not that a real agent ran), and that `cleanup`
returns a well-formed response either way. Writing a new provider? Point
this suite at it (swap `ProviderRunner::new`'s program/args) before
registering it with a real daemon.

## Publishing

**The daemon never publishes on a machine's behalf.** Deciding what to
`git add`, what commit message to write, and whether the work is even in a
committable state is judgment, not orchestration — a generic
`git add -A && commit && push` would sweep up build artifacts and scratch files,
and would contradict the per-cell control task files already exercise
(`system_prompt = "Do NOT commit and do NOT push under any circumstances."`).

So the split is:

| Operation | Who |
|---|---|
| Publish (`add` / `commit` / `push`) | **the task's own cell**, authored in its prompt or command |
| Fetch a known branch onto another machine | **the daemon** — the branch name and remote are already known, so nothing is left to judgment |

The contract is **between tasks**: once a task is Done, one of its cells has
already published whatever downstream work needs. The existing dependency graph
supplies the ordering barrier — a downstream task starts only after its upstream
tasks complete.

**If you author a task that runs on a remote machine and feeds a review, that
task must push before it completes.** This is the one thing a remote task must
do differently from a local one.

A consequence worth noting: a remote machine needs push credentials only if its
task is authored to push. A read-only task needs none.

## A worked example

[`examples/providers/loopback.py`](../examples/providers/loopback.py) implements
every verb, but its "remote machine" is the local host — so you can exercise the
entire remote path without a second machine, an SSH key, or a build farm:

```bash
ralphus machine register --scheme loopback     --program "python /path/to/examples/providers/loopback.py"
ralphus machine list
```

Then point a task at `machine = "loopback:sandbox"`. Everything that makes it
*local* is confined to two functions (`_provision_workspace`, `_run_locally`);
the dispatch, JSON envelope, and handle lifecycle are what a real SSH or
build-farm provider keeps.

You can also drive it by hand, exactly as the daemon does:

```bash
echo '{"project":"demo","source":{"kind":"git","url":"...","branch":"x"}}'   | python examples/providers/loopback.py provision --uri sandbox
echo '{"squad_id":"r1","cell_id":"s0","cwd":"."}'   | python examples/providers/loopback.py exec --uri sandbox
python examples/providers/loopback.py status --handle <handle>
python examples/providers/loopback.py stream --handle <handle> --since 0
python examples/providers/loopback.py cancel --handle <handle>
```

## The SSH provider (RAL-200)

`ssh-provider/` ships a standalone provider, `ralphus-ssh-provider`, that
reaches any host you already have SSH access to — no agent to install, no
port to open, no second daemon to keep alive. It is the first real (not
throwaway-example) provider in the repo.

> **Status: `exec`, `status`, `stream`, `cancel`, `job-cleanup`, `ping`, and
> `provision` are implemented.** Configured targets use durable asynchronous
> execution; legacy invocations without a matching target policy preserve the
> synchronous `exec` result path. Project/worktree `cleanup`, `run`, and
> `channel` remain explicit unsupported-verb errors.
> `provision` (RAL-355 Phase 4) durably clones/fetches a project and creates
> a `git worktree` per task branch under the machine's configured
> `remote_root` (see the "Configuration" and "target" glossary entry) —
> everything else replies with an explicit "not implemented, see RAL-201"
> error rather than a bare "unknown verb".

### The `<uri>` forms

| Form | Meaning |
|---|---|
| `ssh:user@hostname` | Explicit user. |
| `ssh:hostname` | Falls back to the default user — whatever a bare `ssh hostname` would use. |
| `ssh:my-alias` | A `~/.ssh/config` `Host` alias. Opaque to this provider; `ssh` resolves it, including any `User`/`IdentityFile` lines. |

### What `exec` actually does

The daemon hands this provider the same cell spec it would hand a local
runner, `cwd` included — but that `cwd` is a path on the **daemon's own
host**. `exec`:

1. Derives a deterministic remote workspace directory from the local `cwd`
   (so repeated calls against the same cell reuse it rather than
   recreating it from scratch).
2. Syncs the local `cwd`'s contents there — `rsync` when available (Linux/macOS
   daemon hosts, opportunistically), or a `tar | ssh tar -x` stream everywhere
   else (the mandatory fallback: Windows ships OpenSSH + bsdtar in
   `System32`, but not `rsync`). Build/vendor directories (`.git`, `target`,
   `node_modules`, `.venv`, `__pycache__`, …) are excluded by default.
3. Rewrites the spec's `cwd` to the remote path and pipes it into `ralphus-runner`
   on the remote host over one non-interactive `ssh` invocation, forwarding
   `RALPHUS_EVENT:` stderr lines (**including `llm-invoke` usage events**) onto
   its own stderr *as they arrive*, not buffered until the cell ends — this
   is what keeps the daemon's live cost-cap kill able to act mid-run rather
   than only after the whole remote cell has already finished.
4. Parses the remote runner's stdout as the cell's `CellResult` and
   returns it verbatim as `result`, regardless of the remote command's own
   exit code (`ralphus-runner` exits non-zero for a *failed cell* just as
   validly as it exits zero for a done one — the JSON on stdout is the source
   of truth, not the process exit code).

**The remote host is assumed to have a POSIX-like shell** (`sh`/`bash`) for
the extraction/invocation commands this provider constructs — the common case
for an SSH-reachable dev/build box. A Windows *daemon host* is fully
supported (that's the transport-selection split above); a Windows *remote
target* is not exercised by this provider today.

### What `provision` actually does (RAL-355 Phase 4)

Unlike `exec`'s ephemeral-shaped, hash-of-local-path workspace (derived
under `RALPHUS_SSH_REMOTE_BASE`, recreated per cell), `provision` maintains
a **durable, deterministic** clone that persists and is reused across every
task that touches the same project on the same machine — the daemon's own
local worktree pattern (`daemon/src/worktrees.rs::ensure_worktree`), mirrored
onto the remote boundary. `remote_root` comes from the request payload (the
daemon resolves it from the matching `[machine.targets.*]` entry before
dispatch — see `daemon/src/machine_targets.rs`), never from an environment
variable on this provider's own process the way `exec`'s
`RALPHUS_SSH_REMOTE_BASE` does; a machine with no configured target refuses
the request before it ever reaches this provider.

One remote `sh` invocation, holding an `flock` on a per-project lock file for
its whole duration (so two concurrent `provision` calls for the same project
never race), does all of the following:

1. Derives the deterministic project directory:
   `<remote_root>/projects/<project-name>-<url-hash>/` — the URL hash means a
   changed `clone_url` provisions under a *new* identity rather than silently
   repointing an old clone (see `ssh-provider/src/layout.rs`).
2. If `<project-dir>/repository` doesn't exist yet: clones the URL there
   (`git clone --origin origin`) and writes an advisory `metadata.json`
   alongside it.
3. If it already exists: verifies `git remote get-url origin` still matches
   the requested URL exactly — refusing to adopt a directory whose origin
   doesn't match (a hash collision, or manual tampering) rather than fetching
   into what might be the wrong repository — then fetches.
4. Creates `<project-dir>/worktrees/<branch>-<suffix>/` via
   `git worktree add` off the shared clone (reusing an existing worktree for
   that branch rather than recreating it), tracking the resolved
   `upstream`/base reference the daemon supplied. The suffix keeps two
   branches that would otherwise sanitize to the same directory name (e.g.
   `feature/x` and `bugfix/x`) from colliding.
5. Replies with the absolute worktree path as `workspace`.

Every project name, URL, branch, and upstream value is passed through
`transport::shell_quote_single` before it reaches the remote shell — never
interpolated raw. This is the one verb in the whole provider where getting
that wrong would matter most, since `provision` is the verb a task file's
own `project`/branch-shaped fields could otherwise reach a shell through.

Only `kind == "git"` is supported; a non-git project's `provision` request is
refused with a clear "not supported" error rather than attempting something
undefined. There is no immutable base object ID pinning yet (the daemon
sends a ref name, not a resolved SHA) — deliberately deferred in favor of
matching the local path's own ref-based behavior for the first working
version; see `REMOTE_IMPROVEMENTS.local.md`'s Phase 4 notes if that
robustness increment is picked up later.

### Runner installation policy

Each configured target defaults to an already-installed runner and verifies
the command with `--version` before provisioning or execution:

```toml
[machine.targets.buildbox]
machine = "ssh:buildbox"
remote_root = "/srv/ralphus"

[machine.targets.buildbox.runner]
mode = "installed"
command = "ralphus-runner"
```

Upload mode is explicit and maps remote target triples to daemon-host artifact
paths. The provider probes the remote OS and architecture before selecting an
artifact; it never copies a runner found beside the daemon and fails when no
matching mapping exists.

```toml
[machine.targets.buildbox.runner]
mode = "upload"

[machine.targets.buildbox.runner.artifacts]
x86_64-unknown-linux-musl = "C:/ralphus-artifacts/linux/ralphus-runner"
x86_64-pc-windows-msvc = "C:/ralphus-artifacts/windows/ralphus-runner.exe"
```

Uploaded runners live at
`<remote_root>/runners/<target-triple>/<sha256>/ralphus-runner`. The provider
uploads to a protected temporary file, verifies SHA-256 on the target, marks it
executable, and atomically renames it. Content-addressed paths keep old or
currently-running versions intact. `scripts/build-remote-runner.ps1` produces
the Windows MSVC artifact and checksum; `scripts/build-remote-runner.sh`
produces the static Linux musl artifact and checksum. Static linking remains
OS- and architecture-specific—it does not make either artifact portable to the
other target.

### Per-agent executable overrides

An agent profile's explicit executable override (`[agent.profiles.*]`, or a
cell's own `executable` field) resolves to a path on the **daemon's own**
filesystem. Forwarding that verbatim to a remote machine is essentially
never correct — `/Users/alice/.claude/local/claude` almost certainly doesn't
exist on `buildbox`. So before dispatch, `daemon/src/remote_runner.rs`'s
`ProviderRunner::resolve_remote_executable` refuses a path-shaped override
(anything containing `/` or `\`) for a remote cell unless the target
configures a deliberate replacement, keyed by agent name:

```toml
[machine.targets.buildbox.agents]
claude-code = "claude"
codex = "/opt/tools/codex-wrapper"
```

A bare command name (no `/`/`\`) is always forwarded unchanged — the remote
runner resolves it on the remote account's own `PATH`, or falls back to its
usual `RALPHUS_CLAUDE_COMMAND`-style environment default, exactly as a local
cell would. `[machine.targets.<name>.agents]` is optional; most targets
never need it.

### Durable asynchronous jobs

For configured targets, `exec` stores a protected spec and durable state below
`<remote_root>/jobs/<squad-hash>/<cell-hash>/<opaque-handle>/`, launches the
runner in a new POSIX session, then returns the handle. State includes creation
time, supervisor PID plus Linux `/proc` start time, output high-water mark,
combined runner output, and an atomically published final result. Repeating the
same in-flight squad/cell dispatch returns its existing handle; a replacement
is created only after the recorded process identity is no longer alive.

`stream` returns complete output lines in chunks no larger than 64 KiB and a
numeric byte cursor, so repeating a cursor is idempotent and polling cannot load
an unbounded transcript into provider memory. It forwards `RALPHUS_EVENT:`
lines on provider stderr while keeping the provider's single JSON reply on
stdout. `status` reads only durable target state and therefore works from a new
provider process after restart.

The daemon sends resolved cell/proof and agent-profile environment values in
an explicit `execution_environment` object on provider stdin. It does not add
them to the local provider process environment. The SSH provider removes that
object from the stored runner spec, writes a separate mode-`0600`-equivalent
shell environment file over SSH stdin, sources it while preserving the remote
account's ordinary environment, and deletes it before starting the runner.
Cancellation, abandoned startup, and lost-job reconciliation also remove that
transient file; a daemon crash can therefore leave it only temporarily in the
protected job directory until one of those recovery paths runs.

Resolved secret values never enter provider/SSH arguments or remote shell
command text. Stream chunks, provider errors/results, and every string nested
in a structured runner event pass through the same registered-value and
credential-pattern redaction used by local pane snapshots before they can
reach Cartographer, durable pane text, or returned diagnostics.

On Linux, `cancel` sends TERM and then KILL to the runner's entire process
group, verifies the recorded PID/start-time identity is gone, and records the
request time, requester, and outcome before publishing a terminal cancelled
result. Windows-target process-tree cancellation is not implemented by this
POSIX SSH backend.

Job-state cleanup is intentionally distinct from workspace cleanup and is an
explicit provider operation:

```bash
ralphus-ssh-provider job-cleanup --uri buildbox --handle <handle>
```

It refuses a starting/running job and preserves `lost` or corrupt state for
diagnosis. Recursive deletion is limited to the validated, normalized path for
that handle beneath the configured remote root.

### Non-interactive auth, by construction

Every `ssh` invocation this provider makes carries `BatchMode=yes`,
`StrictHostKeyChecking=yes`, `PasswordAuthentication=no`, and
`KbdInteractiveAuthentication=no` — unconditionally, with no configuration
knob to turn them off. A password or host-key prompt fails the connection
immediately instead of hanging forever, with a message telling the operator
exactly what to do:

```bash
# Before registering this machine:
ssh-keyscan -H <host> >> ~/.ssh/known_hosts   # verify the fingerprint out-of-band
ssh-copy-id <user>@<host>                     # or otherwise install your public key
```

### Configuration (environment variables)

| Variable | Default | Meaning |
|---|---|---|
| `RALPHUS_SSH_REMOTE_BASE` | `~/.ralphus/ssh-workspaces` | Remote parent directory workspaces are created under. |
| `RALPHUS_SSH_EXCLUDE` | *(none)* | Comma-separated patterns **added to** the default build/vendor exclude list — additive, not a replacement. |
| `RALPHUS_SSH_CONNECT_TIMEOUT_SECS` | `15` | `ssh -o ConnectTimeout=`. |
| `RALPHUS_SSH_REMOTE_RUNNER_CMD` | `ralphus-runner` | The command run on the remote host, mirroring the daemon's own `RALPHUS_RUNNER_CMD`. |
| `RALPHUS_SSH_CONFIG_FILE` | OpenSSH default | Optional client configuration passed with `ssh -F`; useful for isolated targets and daemon service accounts. |

### Registering it

Same generic mechanism every provider uses — no new daemon-side plumbing
(there isn't a TOML-declarable or auto-bootstrapped path; see "Why this is
not declarable in TOML" above):

```bash
ralphus machine register --scheme ssh --program /path/to/ralphus-ssh-provider \
    --description "reaches any host already reachable via ssh (RAL-200)"
ralphus machine list
```

`ssh` collides with neither the built-in `local` scheme nor the reserved
`ralphus` word, so registration and re-registration (which upserts, same as
every other provider) both just work — `daemon/src/machines.rs`'s
`the_ssh_scheme_is_registrable_and_re_registration_upserts` test pins exactly
this.

Then point a task at it:

```toml
[[task]]
name    = "ral-200-demo"
project = "ralphus"
machine = "ssh:alice@build-box"
  [[task.cell]]
  cwd    = "/home/alice/work/ralphus-checkout"
  prompt = "..."
```

### Testing it

- Unit tests (`ssh-provider/src/*.rs`) cover URI parsing, per-OS transport
  selection and exact command construction, and failure-message
  construction — all without a live remote host.
- `ssh-provider/tests/exec_live_ssh.rs` is an opt-in, `#[ignore]`d-by-default
  integration test that asserts `RALPHUS_EVENT:`/`llm-invoke` markers survive
  a real `ssh` round trip. It skips gracefully (prints `SKIP:`) unless
  `RALPHUS_SSH_LIVE_TEST_TARGET` is set and reachable — run it against your
  own machine (any dev box with OpenSSH server enabled and key-based auth to
  itself already satisfies it):
  ```bash
  RALPHUS_SSH_LIVE_TEST_TARGET=127.0.0.1 cargo test -p ralphus-ssh-provider \
      --test exec_live_ssh -- --ignored --nocapture
  ```
- `ssh-provider/tests/docker_ssh_target.rs` exercises durable async startup,
  duplicate suppression, cursor streaming, restart-safe status, Linux
  descendant cancellation, post-cancel Git integrity, and job cleanup against
  the isolated Docker SSH fixture.

## See also

- `daemon/src/machines.rs` — the registry, resolution and its tests.
- `core/src/schema.rs` — `parse_machine`, the inheritance helpers.
- `docs/daemon-api.md` — the `/api/machines` endpoint shapes.
- `ssh-provider/` — the SSH provider (RAL-200): `src/uri.rs` (uri parsing),
  `src/transport.rs` (per-OS source-transfer command construction),
  `src/ssh.rs` (non-interactive `ssh` invocation + failure interpretation),
  `src/exec.rs` (the legacy synchronous `exec` path), `src/job.rs` (durable
  asynchronous job lifecycle), `src/layout.rs` (RAL-355
  Phase 2/4: deterministic remote storage paths), `src/provision.rs`
  (RAL-355 Phase 4: the `provision` verb orchestration).
- `daemon/src/machine_targets.rs` — RAL-355 Phase 2: `[machine.targets.*]`
  config (`remote_root`, runner policy) a `provision` request is resolved
  against.
- `REMOTE.local.md` — RAL-185's original phased implementation plan
  (machine provider registry foundation) and open questions.
- `REMOTE_IMPROVEMENTS.local.md` — RAL-355's phased plan for what's built on
  top of that foundation: authoritative project clone URLs, durable remote
  storage, persistent Git provisioning, async remote execution, and more.
