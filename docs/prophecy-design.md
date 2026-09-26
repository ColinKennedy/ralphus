# Prophecy — design record

> Design notes from `PROPHECIES.local.md` (2026-09-25)

## 0. The idea in one paragraph

When an agent learns something the diff can't show — why a path was taken, what
was left behind in a rebase, a hazard noticed but not fixed — it should be able
to say so, durably, at the moment it learns it. Those statements accumulate
across a task's attempts and its review, and surface in the PR so the reasoning
arrives with the code instead of evaporating with the cell.

## 1. Verdict — five decisions

1. **The transport is a marker, not an MCP server.** The daemon reads a cell's
   stderr line by line and already attributes each event to the owning cell
   from its own `RunnerSpec`. A marker-based write needs no token, no daemon
   URL, and no endpoint the agent can reach — and one cell cannot forge
   another's attribution. See §4, §5.
2. **A prophecy is append-only; a ghost is merge-on-write.** They answer
   different questions for different readers on different clocks. Keep both;
   do not widen ghost into this. See §3.
3. **No new parent entity.** Addressing already exists (`EntityUri`) and
   Cartographer already stamps squad/task/cell/guardian per row. One
   correlation key across the squad↔review↔PR seam buys the through line for a
   fraction of the blast radius. See §7.
4. **No new capability for the agent.** Not a daemon URL, not a token. The
   URL is a hardcoded default and the token is a readable file, so withholding
   them protects nothing locally — and exporting them would ship the daemon's
   bearer token to every remote machine. See §5, §6.
5. **The prose goes in the PR body; git gets only a join key.** A one-line
   trailer naming the authoring cell, not the prophecy set. See §8.

## 2. Inventory — what ralphus already has

Four of six pieces are already shipped and load-bearing. This is the single
most useful section: most of the subsystem is assembly, not invention.

| State | Piece | What it gives us | Source |
|---|---|---|---|
| **Shipped** | **Ghost** (RAL-136) — the prototype | An agent writes `RALPHUS_GHOST:` + bullets; the runner parses it into the `ghosts` table. Framed explicitly as "not a changelog — what the diff can't show." This *is* the idea, at cell scope, already working. | `daemon/src/ghost.rs`; parsed at `runner/src/execute.rs:884` |
| **Shipped** | **Cartographer** (RAL-98) — structured storage | Rows carry timestamp, level, source, message, scope, `squad_id`, `guardian_id`, `cell_id`, `task`, `log_path`, free JSON payload. Filterable/paginated over HTTP. | `daemon/src/cartographer.rs` |
| **Shipped** | **An agent→daemon event channel** | `RALPHUS_EVENT: {json}` on stderr, parsed by the daemon. Survives the hard cases: tmux panes (via the `.raw` transcript) and remote hosts through a machine provider. | `daemon/src/runner.rs:27`; `daemon/src/remote_runner.rs:468` |
| **Shipped** | **Daemon-side attribution** | `forward_runner_event` fills a missing `squad_id`/`cell_id`/`task` from the owning `RunnerSpec`. Identity comes from the daemon's bookkeeping, not from what the agent claims. | `daemon/src/runner.rs:2784` |
| **Shipped** | **Uniform addressing** | `squad:<id>`, `task:<id>:<i>`, `cell:<id>:<i>:<j>`, `proof:…`, `guardian:<id>`. A prophecy's owner key is this string. | `daemon/src/entity_uri.rs` (RAL-155) |
| **Partial** | **The squad → review → PR chain** | `guardians.squad_id` exists but is **nullable**, and a review aggregates branches that may come from different squads. The chain breaks exactly at the seam we want it to hold. | `daemon/src/store.rs:1413` |
| **Missing** | **Cell identity in the agent's own process** | The runner sets `CLAUDE_CONFIG_DIR`, `CODEX_HOME`, `PI_CODING_AGENT_DIR`, `RALPHUS_PI_WORKSPACE_GUARD` — nothing saying who the cell *is*. Blocks the CLI/MCP route only; the marker route doesn't need it. | grep for `RALPHUS_SQUAD`/`CELL`/`TASK`/`GUARDIAN`/`ATTEMPT` across `daemon`, `runner`, `cli`, `mcp` → no hits |

**Read on the "Partial" row:** this — not the absence of a "Memo" object — is
what makes ralphus feel like it has no through line.

*(Two citations above were corrected against the current tree: `forward_runner_event`
now starts at `daemon/src/runner.rs:2784`, not `:2789`, and the nullable
`guardians.squad_id` column is now at `daemon/src/store.rs:1413`, not `:1360` —
both files have grown since this table was first drafted. Every other citation
in this table still resolves at the line given.)*

## 3. A prophecy is not a bigger ghost

The tempting cheap move is "raise the ghost cap and keep more of them." That
breaks ghost without delivering a prophecy.

| | Ghost (RAL-136) | Prophecy |
|---|---|---|
| Question it answers | What should the next agent know? | Why does this code look like this? |
| Reader | The next cell's prompt | A human, in the PR |
| Rows | One per owner, folded on rewrite | Append-only, one per insight |
| Written | Once, at end of reply | Continuously, mid-work |
| Across attempts | Merged into one blob | Attempt 1…N stay distinct |
| Size | Hard 4000-char total cap (`MAX_CONTENT_CHARS`) | Per-entry cap, unbounded count |
| Reach | Own restarts + one level of dependents | The whole review stack |
| Lifespan | Cascade-deleted with its squad or guardian | Outlives both; the PR is the terminal home |

Two consequences worth stating:

- **Why not a Cartographer `source`.** Cartographer prunes at 30 days / 50k
  rows (`[cartographer] retention_days`, `max_rows`). A prophecy must survive
  until it reaches a PR. So: its own table — *and* still emit a Cartographer
  row on every write, per the logging policy, so the squad timeline picks it up
  for free.
- **A nice unification, later.** Once a prophecy exists, the **ghost can be
  derived** from the most recent N for that cell. The agent stops having to
  remember a separate end-of-reply marker, and the two systems stop competing
  for the same discipline. See phase 5.

## 4. Transport

| Transport | Covers | Captures | Verdict |
|---|---|---|---|
| `RALPHUS_PROPHECY:` marker | Every backend — ollama, native, tmux, remote | The moment; stderr is read line by line, not at exit | **Primary.** No credential, no endpoint. Attribution is unforgeable. |
| `RunnerResult` field | Every backend, local and remote — it's in the `exec` reply | Everything the cell collected, at exit | **Primary backstop.** A typed contract rather than best-effort stderr. Exactly how `ghost` already crosses the provider boundary. |
| Daemon-side writes | Rebase, merge, auto-fix | Decisions ralphus itself made | **Free.** No agent cooperation needed. |
| MCP tool | claude-code, codex, pi — and only where the daemon is reachable | The moment, with a delivery acknowledgment | Later. Better ergonomics, but the agent must hold a token. |
| CLI write command | Anything that can run a shell command | The moment | Later, with the above — it's what the MCP tool routes through. |

### 4.1 Why the marker wins

I initially had this backwards, and the correction matters: I dismissed the
marker as "an end-of-reply summary, can't record a path." That is true of
`RALPHUS_GHOST:`. It is **not** true of `RALPHUS_EVENT:`, which the daemon
reads **line by line, not just at exit**. So the marker gets mid-work
timestamped capture too, and the MCP tool's only remaining edge is a delivery
acknowledgment plus structured arguments — not worth a credential for
append-only advisory notes.

### 4.2 The MCP finding — banked, not spent

Worth recording because it changes the cost of the *later* phase, not this one:

- `mcp/src/tools.rs:1` builds the tool registry from
  `help_map::registered_leaves()`.
- `mcp/tests/parity.rs` asserts **in both directions** that every
  non-excluded CLI leaf has a tool and every tool has a leaf, with a
  substantive reason required for any exclusion.

So `ralphus prophecy record` would yield the MCP tool, the HTTP endpoint, and a
shell-callable path from one implementation, permanently enforced. **The "tiny
MCP server" idea is not a new server — it is one CLI subcommand.** But every
one of those routes is an authenticated HTTP call, so it waits on §5.

Read-side `ralphus prophecy list | show` is fine to ship early (phase 1) — it
grants an agent read access to prophecies via parity, which is harmless.

### 4.3 The marker's one real weakness

Documented already in `docs/special-syntax.md`: an agent whose *work* prints
one of these markers trips the parser reading its own output. Not hypothetical
here — ralphus agents develop ralphus and read that file.

Mitigation is in-tree precedent: match a **standalone line of exact form**, not
a substring scan. That is exactly what `RALPHUS_TMUX_DONE` does, after the
substring version false-positived. Cheaper to adopt that convention than to
hand out a credential.

### 4.4 Daemon-side writers — start here

`guardian_merge.rs` already writes Cartographer rows at every
conflict-resolution step. Rather than teaching the merge agent a new marker,
have that path write a prophecy directly for decisions it already makes:
conflict resolved, hunk dropped, ours taken. Daemon-side code, needs nobody to
remember anything — and "we had to leave one thing behind in the rebase" is the
single highest-value note on the whole list.

---

## 5. Trust boundary — give the agent no new capability

The tempting version hands the cell a daemon URL and a token so it can `POST` a
prophecy. Refuse it, for the opposite of the obvious reason.

1. **The URL is not a secret.** Hardcoded default `http://127.0.0.1:7890`
   (`cli/src/client.rs:12`). Withholding it protects nothing.
2. **Neither is the token.** Bearer auth *is* enforced on every route
   (RAL-219), but the daemon persists the token as a plain file at
   `state_dir()/daemon.token`, and the CLI reads it from there whenever
   `$RALPHUS_DAEMON_TOKEN` is unset (`cli/src/client.rs:159`). The agent runs
   as the same OS user, so it is a `cat` away.
3. **`ralphus` is already on `PATH`.** A cell can run `ralphus squad cancel`
   today and let the CLI do the token lookup. No env var required.
4. **Caller identity is self-asserted.** The `X-Ralphus-User` header;
   `daemon/src/server.rs:4181` says it outright — *"there is no verified-login
   distinction yet (RAL-252)."* `admin_gated` trusts the header value.

**So the exposure is pre-existing and total.** That is a defensible posture for
a single-user local dev tool and it is not this subsystem's job to fix. But it
does mean a *scoped* prophecy token would be decorative: anything it withheld
is readable anyway. Scoping only becomes real where the cell cannot read the
daemon's filesystem — container mode (RAL-225). Separate ticket, separate
decision (see §11, §13).

**Which is why the marker wins on trust, not just cost.** A stderr line is not
an endpoint: no reachable surface, no credential, and it cannot be pointed at
any other route. And because `forward_runner_event` fills identity from the
owning `RunnerSpec`, the *daemon* decides which cell a prophecy belongs to — a
cell cannot forge another cell's attribution even deliberately.

**On the two remaining env vars.** `RALPHUS_ENTITY_URI` and `RALPHUS_ATTEMPT`
are *information*, not capability — they grant no access, and they would make
any CLI command usable in-cell without the agent guessing ids. Worth doing
eventually, but not needed for a prophecy, and the credential question rides
along with them. Own ticket. `RALPHUS_DAEMON_URL` is cut outright.

---

## 6. Topology — daemon, cell, and review on different machines

Ralphus supports this properly, so it cannot be assumed away.

**The model.** A `machine` value resolves to a provider *executable the daemon
runs locally*; the remote host is reached only through that process's
stdin/stdout and **never initiates a connection back**. There is no reverse
channel, by design. Reviews are machine-aware too: `daemon/src/workspace.rs`
(RAL-185 Phase 3c) carries "a directory *plus the machine it lives on*", and
`guardian_merge` routes git through `ws.git()`, which dispatches to the
provider's `run` verb when remote. So daemon here / cell on a build farm /
review on a third box is a supported topology.

### 6.1 What holds

1. **The marker route is topology-independent.** `remote_runner.rs:468`
   already forwards `RALPHUS_EVENT:` lines off the provider's stderr.
2. **`log_path` and timeline inlining still work.** A remote cell's transcript
   is materialized *locally* on the daemon host from pumped `stream` output
   (`write_pane_snapshot` in `poll_to_completion`). Checked specifically
   because I expected this to break; it does not.
3. **There is a second remote-safe channel, and we should use it.**
   `RunnerResult.ghost` (`daemon/src/runner.rs:925`) already crosses the
   provider boundary inside the `exec` reply. Ghost therefore has *two*
   remote-safe transports today. Mirror both: marker for mid-work streaming,
   plus a `prophecies` field on `RunnerResult` as the at-exit backstop. The
   result field is a typed contract; stderr forwarding is explicitly
   best-effort ("streaming is a convenience, not the result").

### 6.2 Remote makes the credential idea worse, not safer

`env_overrides` is part of the `RunnerSpec` that crosses to the provider on
`exec` (documented in the `exec` verb row of `docs/machine-providers.md`).
Exporting a daemon token or URL would therefore **copy the daemon's bearer
token onto every remote machine** — a credential that currently never leaves
its host — where it would also be useless, since the daemon binds loopback.
Exfiltration with no upside.

**Constraint worth writing down while it is still true:** no ralphus credential
crosses the provider boundary today. Remote auth is the provider's own business
(SSH keys for `ralphus-ssh-provider`). Preserve that.

### 6.3 What genuinely breaks — the commit trailer

`git_hooks::sync_coauthor_hook` uses `std::fs::write` and the **local**
`guardian_merge::git` free function, and is only called from
`execute_worktree_plan` (`daemon/src/worktrees.rs:1204`) — the local
`git worktree add` path. `provision_remote_with_targets` never calls it.

- **A remote worktree receives no `prepare-commit-msg` hook at all.**
- So RAL-445's `Co-authored-by:` is **already silently absent from every remote
  commit today**, and a `Ralphus-Cell:` trailer would inherit that hole.
- Mechanical fix: port the module from `&Path` to `&Workspace` and use the
  existing `ws.write_file()` / `ws.git()`.
- File as a RAL-445 bug independent of this subsystem (§13).

Same shape, smaller: `ghost::current_revision` (`daemon/src/ghost.rs:202`)
calls the local `git`, so on a remote `cwd` it returns `None` and the revision
marker silently vanishes. Needs `ws.git()` too.

---

## 7. The through line — a correlation key, not a new entity

The instinct that ralphus has no through line is about half right, and the
wrong half matters for cost.

**Already there:**

- Addressing — `EntityUri` (§2).
- Four entity references stamped on every Cartographer row.
- `build_squad_timeline` (`daemon/src/timeline.rs`) already merges a squad's
  whole history chronologically with terminal-log excerpts inlined.

**Genuinely absent:** a durable link across the **squad ↔ review ↔ PR** seam
(`guardians.squad_id` nullable; a review aggregates branches from possibly
different squads).

**Recommendation.** A new parent object — Memo, Book, whatever — means every
existing table grows a foreign key and every existing query learns about it.
That is an enormous blast radius for a feature whose value is "keep a note." A
correlation id stamped on a prophecy and carried across that seam buys the same
queryability for a fraction of the change. If it later earns promotion to a
real entity, nothing here blocks that.

---

## 8. Where a prophecy lands

### 8.1 The PR body — deterministic, never LLM-rewritten

`pr.rs::resolve_title_description` already composes a PR body:
`synthesize_pr_text` asks a model (honoring the repo's own PR template via
`fetch_pr_template`), with `fallback_pr_description` as the deterministic path.

- A prophecy folds in as a `<details>` block appended **after** the synthesized
  description.
- Appended **deterministically**. A summarizer in that position would quietly
  launder away the specifics that make the note worth keeping. The note the
  agent wrote is the note the human reads.
- Mark entries published so a resubmit does not duplicate them.

### 8.2 The commit trailer — a join key, nothing more

Nothing large goes into a commit message. A git trailer is the `Key: Value`
block at the bottom of a commit message — where `Co-authored-by:` already lives
— and git parses it natively, so it is queryable:

```
Ralphus-Cell: cell:squad-000000000012:0:1
```

1. **On every commit the cell authors** — not the earliest, not the tip. A
   trailer rides along with its own commit through a rebase, so nothing needs
   updating when `guardian_merge.rs` restacks, and there is no "which commit"
   decision available to get wrong. "Earliest commit of the PR" is not a stable
   identity in a stack.
2. **It names the author, not the prophecy set.** A commit is authored at time
   T, but a prophecy for that cell keeps arriving afterward — that is the whole
   point. A trailer listing prophecy URIs is stale the moment it is written and
   would need commit amends to stay current. The entity URI is stable and known
   at commit time; join on it at read time.
3. **Keep the attempt number out**, so the same cell across attempts 1 and 3
   yields one trailer rather than two (`--if-exists addIfDifferent` keys on the
   exact key+value pair).
4. **The hook already exists.** RAL-445's `daemon/src/git_hooks.rs` installs a
   `prepare-commit-msg` hook running
   `git interpret-trailers --in-place --if-exists addIfDifferent --trailer "Co-authored-by: …"`,
   synced on every squad worktree materialization and at project registration.
   Hooks are repo-common, so one install covers every worktree; a `MARKER`
   guard keeps it from clobbering a hand-authored hook; `[commits]
   add_coauthor` opts a project out. Adding `Ralphus-Cell:` is a second
   `--trailer` flag — and since the hook inherits the cell's environment, it
   reads `$RALPHUS_ENTITY_URI` for free once that is exported.
5. **Two pre-existing RAL-445 gaps block the trailer half** — see §13.

---

## 9. Shape of one prophecy

Small enough to be cheap, typed enough to be filterable, not so typed the agent
stalls.

| Field | Type | Notes |
|---|---|---|
| `entity_uri` | text | Owner: `cell:…` / `guardian:…`. Reuses `EntityUri`. |
| `attempt` | int | Keeps attempts 1…N distinct. The reason this table is append-only. |
| `kind` | enum? | Proposal: closed set `discovery` \| `decision` \| `hazard` \| `deferred`. **Validation is an open question — see §11.1.** |
| `body` | text | The note. Per-entry cap. |
| `revision` | text? | Opaque VCS marker, best-effort, `None` when unavailable (mirrors ghost). Needs `ws.git()` to work remotely (§6.3). |
| `created_at_ms` | int | |
| `published_at_ms` | int? | Set when folded into a PR body. |
| `pr_id` | text? | Which PR it landed in. |

Deliberately **not** in v1: a confidence score (§12).

---

## 10. Build phases

Each phase is independently shippable and useful alone — stop after two and
something was still gained.

### Phase 1 — store, plus the writers that need no agent

- [x] Append-only table keyed by `entity_uri` + `attempt`
- [x] Cartographer row emitted on every write (logging policy)
- [x] Read-side `ralphus prophecy list | show`
- [x] Populate from the rebase/conflict decisions `guardian_merge.rs` already
      makes (§4.4)
- [x] Glossary entry for **prophecy** in `docs/glossary.md`

*Why first:* zero agent involvement, immediate value, and it proves the storage
model against real traffic before any prompt work exists.

### Phase 2 — transport

- [x] Scan the agent's streaming output for a standalone `RALPHUS_PROPHECY:`
      line; match exact-form lines only, per the `RALPHUS_TMUX_DONE` precedent
      (§4.3)
- [x] Forward it as a runner event; let `forward_runner_event` attribute it
- [x] Add a `prophecies` field to `RunnerResult` so the at-exit set crosses the
      provider boundary as a typed contract, mirroring `ghost` (§6.1)
- [x] `docs/special-syntax.md` entry for the marker

*No credential, no endpoint, no new env. Works remotely unchanged.*

### Phase 3 — teach the agents

- [ ] System-prompt fragment alongside the existing ghost fragment in
      `runner/src/execute.rs`

*This is where the discipline is won or lost. Ghost is the evidence it works.*

### Phase 4 — fold into the PR

- [ ] Deterministic `<details>` block after the synthesized description (§8.1)
- [ ] `published_at_ms` / `pr_id` so a resubmit does not duplicate
- [ ] `Ralphus-Cell:` trailer on the existing RAL-445 hook (§8.2) — **blocked
      on the two gaps in §13**

### Phase 5 — derive the ghost (optional)

- [ ] Build the ghost from recent prophecies instead of a separate marker, so
      there is one discipline to teach rather than two (§3)

*Only once the rest is proven.*

### Phase 6 — the CLI write path (only if the marker disappoints)

- [ ] `ralphus prophecy record`; inherit the MCP tool via parity (§4.2)
- [ ] Export `RALPHUS_ENTITY_URI` / `RALPHUS_ATTEMPT`

*Blocked on the credential question, which is RAL-252 / RAL-225 territory, not
this subsystem's. Do not start here.*

### Status as of this commit

This task's scope was Phase 1 only. What landed here:

- The `prophecies` table (`daemon/src/prophecy.rs`, `daemon/src/store.rs`),
  keyed by `entity_uri` + `attempt`, matching the §9 field shape.
- A Cartographer row on every `Store::record_prophecy` call.
- `GET /api/prophecies` / `GET /api/prophecies/{id}` plus
  `ralphus prophecy list` / `ralphus prophecy show` (CLI, and — by MCP parity
  — the corresponding MCP tools).
- Three daemon-side call sites in `daemon/src/guardian_merge.rs` that record
  a prophecy at rebase/conflict-resolution decisions, with no agent
  cooperation. Caveat: §4.4 and the table above describe the decisions as
  "conflict resolved, hunk dropped, ours taken" — `guardian_merge.rs` has no
  literal `--ours`/"ours taken" merge strategy to hook into (confirmed by
  grep), so the writers fire on the conflict-resolution and give-up paths
  that actually exist in the code, not on a literal "ours taken" branch.
- The glossary entry for **prophecy** in `docs/glossary.md`.
- `kind` is implemented as an open string, not the closed enum §11.1
  proposes — that question was left unresolved by the design doc and is not
  this task's call to make; the validation site carries a
  `// TODO(prophecy-kind-enum):` comment instead of guessing.

Phase 2 (transport) was already landed by the sibling task scoped to §4 —
present in the working tree at the time of this commit and cited above
without re-verifying its authorship: the `RALPHUS_PROPHECY:` marker constant
and standalone-line scanner (`daemon/src/runner.rs`), forwarding through
`forward_runner_event`, the `RunnerResult.prophecies` backstop field, and a
`docs/special-syntax.md` entry. This task did not touch any of that code and
takes no credit for it beyond confirming it is present.

Phases 3–6 are not started. Nothing in this commit implements a system-prompt
fragment, PR folding, ghost derivation, or a CLI/MCP write path.
