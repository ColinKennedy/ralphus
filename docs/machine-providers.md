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
> `remove-path`, `ping`, `channel`, `cleanup`), each dispatched genericly
> through the same registry with no daemon-side branching on scheme. Guardian
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
| `run` | `--uri` | [`RunRequest`](../daemon/src/remote_runner.rs) (`cwd: String`, `program: String`, `args: Vec<String>`) | Run one VCS command in a workspace. Reply `{"exit_code": 0, "stdout": "..."}`. |
| `read-file` | `--uri` | [`FileRequest`](../daemon/src/remote_runner.rs) (`path: String`, `content: null`, `recursive: false`) | Return a file's contents. Reply `{"stdout": "<file content>"}`. A missing/unreadable file is **not** an error at the daemon layer — callers treat it as "absent" — but the provider still replies however it normally would (`ok: false` is fine; the daemon maps any failure to "absent"). |
| `write-file` | `--uri` | [`FileRequest`](../daemon/src/remote_runner.rs) (`path: String`, `content: "<text to write>"`, `recursive: false`) | Write `content` to `path`, creating parent directories as needed. Reply is the standard envelope. |
| `remove-path` | `--uri` | [`FileRequest`](../daemon/src/remote_runner.rs) (`path: String`, `content: null`, `recursive: bool`) | Delete a file, or a directory tree when `recursive` is `true`. A path that does not exist is success, not an error. |
| `ping` | `--uri` | none | Confirm the machine is reachable and ready, doing no work. Reply `{"ok": true, "detail": "..."}`. Called on demand from the board's Machines tab, never polled. |
| `channel` | `--uri` | newline-delimited `RunRequest`s | **Optional.** Serve many requests from one process: read newline-delimited JSON `run` requests on stdin, write one newline-delimited JSON response each, until stdin closes. |
| `cleanup` | `--uri` | none | Tear the workspace down (see "The `cleanup` verb and its retention policy" below). |

The cell spec arrives on stdin for `exec`; the provision request arrives on
stdin for `provision`; the file/run requests above arrive on stdin for their
own verb. Handle-scoped verbs (`status`/`stream`/`cancel`) and `ping`/`cleanup`
take no stdin payload.

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
inspecting it:

```bash
curl -X POST http://127.0.0.1:7890/api/machines/cleanup \
  -d '{"machine": "incredibuild:A"}'
# or
ralphus machine cleanup incredibuild:A
```

**Retention on failure: nothing is discarded.** The daemon keeps no record of
a provisioned workspace to roll back or retry against — `provision` is
idempotent and re-derives the same workspace deterministically every time
(from `squad_id`/`cell_id`/the requested branch), so there is nothing to
"forget" on a failed cleanup. If your `cleanup` implementation fails partway
through (permissions, a process still holding the directory open, a dead
machine), leave the workspace exactly as it was and reply `{"ok": false,
"error": "..."}` — the daemon surfaces that reason verbatim to the caller
rather than swallowing it, so a human can retry or investigate.

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

> **Status: `exec` only.** `provision`/`stream`/`status`/`cancel`/`cleanup`
> daemon-side dispatch is a separate ticket (RAL-201) — check its status
> before assuming those verbs are wired up. This provider's `exec` runs
> **synchronously**: it blocks until the remote cell finishes and replies
> with `result` directly, so nothing on the daemon side needs to poll
> `status`/`stream`/`cancel` for it regardless of RAL-201. It also implements
> `ping`, since the daemon already dispatches that verb independently (the
> board's Machines tab "Check" button) — everything else replies with an
> explicit "not implemented, see RAL-201" error rather than a bare "unknown
> verb".

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

## See also

- `daemon/src/machines.rs` — the registry, resolution and its tests.
- `core/src/schema.rs` — `parse_machine`, the inheritance helpers.
- `docs/daemon-api.md` — the `/api/machines` endpoint shapes.
- `ssh-provider/` — the SSH provider (RAL-200): `src/uri.rs` (uri parsing),
  `src/transport.rs` (per-OS source-transfer command construction),
  `src/ssh.rs` (non-interactive `ssh` invocation + failure interpretation),
  `src/exec.rs` (the `exec` verb orchestration).
- `REMOTE.local.md` — the phased implementation plan and open questions.
