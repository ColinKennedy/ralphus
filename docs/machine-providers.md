# Machine providers (RAL-185)

A task, session, verify step or review may declare which **machine** it runs on:

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
> session/verify execution, `provision`, `stream` (Live View), and `cancel`.
> Guardian reviews still run on the daemon's own host, so a remote session may
> not opt into one yet (rejected at submit). See `REMOTE.local.md` Phase 3.

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
  └─ session.machine          (overrides the task)
       └─ verify.machine      (overrides the session)
task.verify.machine           (overrides the task)
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
(`ralphus:/RUN[...]`), so registering it as a third thing is rejected.

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

| Verb | Flags | Purpose |
|---|---|---|
| `provision` | `--uri` | Ensure a workspace exists; reply with `{"workspace": "<abs path on this machine>"}`. Must be idempotent — a re-run after a daemon restart reuses the existing workspace. |
| `exec` | `--uri` | Run one session/verify. Reply with **either** `{"result": {...}}` (ran synchronously) **or** `{"handle": "..."}` (started asynchronously). |
| `status` | `--uri --handle` | For an async handle: `{"state": "running"}`, or `{"state": "done"\|"failed", "result": {...}}`. |
| `stream` | `--uri --handle [--since N]` | `{"output": "...", "next": N}` — output since the cursor. Optional; omitting it costs Live View, not execution. |
| `cancel` | `--uri --handle` | Stop the work behind a handle. |
| `run` | `--uri` | Run one VCS command in a workspace. Request on stdin: `{"cwd": "...", "program": "git", "args": ["rev-parse", "HEAD"]}`. Reply `{"exit_code": 0, "stdout": "..."}`. |
| `ping` | `--uri` | Confirm the machine is reachable and ready, doing no work. Reply `{"ok": true, "detail": "..."}`. Called on demand from the board's Machines tab, never polled. |
| `channel` | `--uri` | **Optional.** Serve many requests from one process: read newline-delimited JSON `run` requests on stdin, write one newline-delimited JSON response each, until stdin closes. |
| `cleanup` | `--uri` | Tear the workspace down. |

The session spec arrives on stdin for `exec`; the provision request arrives on
stdin for `provision`. Handle-scoped verbs take no stdin payload.

**Synchronous vs async `exec`.** A provider that can only block returns
`result` and is done — no `status`/`stream`/`cancel` needed. A provider that
returns a `handle` gets live output and mid-run cancellation, at the cost of
implementing three more verbs. Both are first-class; pick per provider.

### `provision` and non-git projects

The provision payload's `source` is deliberately **not** git-shaped:

```json
{"project": "ralphus",
 "source": {"kind": "git", "url": "git@host:acme/repo.git", "branch": "RAL-169-foo"},
 "run_id": "...", "session_id": "work"}
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

(*Channel*, not *session*: a session is already a ralphus concept — a task
contains sessions, which contain verify steps — so the word is kept clear of
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
debugging a failed run unable to tell "the build broke" from "the build box is
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
  that drops them leaves Cartographer blind for every remote session.

  The daemon scrapes that marker from the stderr of **every** verb it invokes,
  not just `exec` — which matters most for an async provider. Its session's
  events are produced long after `exec` returned a handle, so the only place
  left to surface them is the stderr of the later `status`/`stream` calls. Echo
  them there. `examples/providers/loopback.py`'s `cmd_stream` shows the whole
  pattern in five lines.
- **Forward `llm-invoke` usage events specifically.** The live cost-cap kill
  reads token/cost usage from them. A provider that drops them silently
  disables budget enforcement — the run still completes, just uncapped.
- **Honor `trace_context`.** The session spec carries a W3C `traceparent`
  (RAL-96). A provider that starts its work without propagating it into the
  remote environment gets a disconnected trace rather than one end-to-end
  waterfall — visible, but only if you go looking.

## Publishing

**The daemon never publishes on a machine's behalf.** Deciding what to
`git add`, what commit message to write, and whether the work is even in a
committable state is judgment, not orchestration — a generic
`git add -A && commit && push` would sweep up build artifacts and scratch files,
and would contradict the per-session control task files already exercise
(`system_prompt = "Do NOT commit and do NOT push under any circumstances."`).

So the split is:

| Operation | Who |
|---|---|
| Publish (`add` / `commit` / `push`) | **the task's own session**, authored in its prompt or command |
| Fetch a known branch onto another machine | **the daemon** — the branch name and remote are already known, so nothing is left to judgment |

The contract is **between tasks**: once a task is Done, one of its sessions has
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
echo '{"run_id":"r1","session_id":"s0","cwd":"."}'   | python examples/providers/loopback.py exec --uri sandbox
python examples/providers/loopback.py status --handle <handle>
python examples/providers/loopback.py stream --handle <handle> --since 0
python examples/providers/loopback.py cancel --handle <handle>
```

## See also

- `daemon/src/machines.rs` — the registry, resolution and its tests.
- `core/src/schema.rs` — `parse_machine`, the inheritance helpers.
- `docs/daemon-api.md` — the `/api/machines` endpoint shapes.
- `REMOTE.local.md` — the phased implementation plan and open questions.
