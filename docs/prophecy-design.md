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
