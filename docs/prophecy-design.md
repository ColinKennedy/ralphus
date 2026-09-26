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
| **Partial** | **The squad → review → PR chain** | `guardians.squad_id` exists but is **nullable**, and a review aggregates branches that may come from different squads. The chain breaks exactly at the seam we want it to hold. | `daemon/src/store.rs:1419` |
| **Missing** | **Cell identity in the agent's own process** | The runner sets `CLAUDE_CONFIG_DIR`, `CODEX_HOME`, `PI_CODING_AGENT_DIR`, `RALPHUS_PI_WORKSPACE_GUARD` — nothing saying who the cell *is*. Blocks the CLI/MCP route only; the marker route doesn't need it. | grep for `RALPHUS_SQUAD`/`CELL`/`TASK`/`GUARDIAN`/`ATTEMPT` across `daemon`, `runner`, `cli`, `mcp` → no hits |

**Read on the "Partial" row:** this — not the absence of a "Memo" object — is
what makes ralphus feel like it has no through line.

**Citation corrections made against the current tree (2026-09-26):** the
daemon-side-attribution row originally cited `daemon/src/runner.rs:2789`,
which lands mid-parameter-list (the `json: &str` line); the function
signature `pub(crate) fn forward_runner_event(` itself starts at line 2784,
so the citation now points there. The squad→review→PR-chain row originally
cited `daemon/src/store.rs:1360`, which falls inside an unrelated table's
column list; the nullable `guardians.squad_id TEXT,` column is actually
declared at line 1419, inside `CREATE TABLE IF NOT EXISTS guardians` (starting
line 1404), so the citation now points there. Every other citation in this
table — `daemon/src/ghost.rs`, `runner/src/execute.rs:884`,
`daemon/src/runner.rs:27`, `daemon/src/remote_runner.rs:468`,
`daemon/src/entity_uri.rs` — still resolves exactly as described, and the
"Missing" row's grep claim (no `RALPHUS_SQUAD`/`CELL`/`TASK`/`GUARDIAN`/`ATTEMPT`
hits, and the runner still sets `CLAUDE_CONFIG_DIR`/`CODEX_HOME`/
`PI_CODING_AGENT_DIR`/`RALPHUS_PI_WORKSPACE_GUARD`) still holds.
