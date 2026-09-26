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
