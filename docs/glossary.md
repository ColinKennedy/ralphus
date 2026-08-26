# Glossary — ralphus's taken words

Words that mean something specific in this codebase. Before coining a new term,
check that it isn't already here meaning something else — reusing a taken word
makes both meanings harder to read, and the collisions are hard to undo once
they're in the schema, the store, and the board.

**When you take a new word, add it here.**

---

## The task pipeline

These four nest, and the nesting is the single most important thing to get
right — most of the collisions worth avoiding are with these.

| Term | Meaning |
|---|---|
| **squad** | One *submission*. What `ralphus submit x.toml` creates: the whole TOML file's worth of work, with its own id (`squad-…`), state, and dependency graph. A squad contains tasks. |
| **task** | One unit of work inside a squad, named by the author (`[[task]] name = "ral-169"`). Owns a workspace, a project, and — since RAL-185 — a machine. A task contains cells. |
| **cell** | One agent (or command) invocation inside a task (`[[task.cell]]`). The thing that actually runs a model or a shell command. Cells in a task share a workspace and hand off through files on disk. |
| **proof** / **proof step** | A check that runs *after* its owning cell (cell-scope) or after all of a task's cells (task-scope). Kinds: `command` (exit code is the verdict), `prompt` (an agent returns `RALPHUS_PROOF: PASS/FAIL`), `brain` and `approval` (declared, not yet implemented). |

Do **not** reuse *cell* for anything else — notably not for connection reuse
or transport (see **channel**).

## Reviews

| Term | Meaning |
|---|---|
| **review** | The user-facing name for a merge review: several branches stacked, rebased, conflict-resolved, and gated by checks. Declared as `[[review]]` in TOML. |
| **guardian** | The *internal* name for the same thing — the stored entity (`guardian-…`), its module (`guardian.rs`, `guardian_merge.rs`), and its API routes (`/api/guardians/…`). Historical: "review" is what users see, "guardian" is what the code says. Expect both. |
| **branch** (in a review) | One contributing feature branch in a review's stack, with its own merge status. Not to be confused with the review's own `review_branch` (the combined head) or its `base_branch`. |
| **stacked rebase** | How a review merges: branch B is rebased onto branch A, which is rebased onto one snapshotted base commit. Linear, ordered, and the reason every worktree in a review must be on one machine. |
| **base branch** | What the stack rebases onto (`main`, typically). Inferred from a worktree's git upstream for a local review; **declared** via `[[review]] base` when a remote cell feeds it (RAL-185 D4). |
| **check gate** | Commands run against the finished combined worktree before a review is considered done. From `[[review]]`'s checks, or a project's `auto_build`. |
| **carry-forward** / **carry refs** | Reusing a previous build's already-resolved conflict commits when a review is rebuilt, so the same conflict isn't resolved twice. |
| **combined worktree** | The read-only worktree at the head of the full stack — what a reviewer reads and what check gates run against. |

## Machines and providers (RAL-185)

| Term | Meaning |
|---|---|
| **machine** | Where work runs, declared as `machine = "<scheme>:<uri>"` on a task, cell, proof step, or review. Unset means the daemon's own host. |
| **scheme** | The left half of a machine value (`incredibuild`). Names a registered **provider**. |
| **uri** | The right half (`A`, a hostname, a URL). **Opaque** — ralphus never interprets it; it's handed to the provider verbatim. |
| **provider** | A registered executable the daemon runs to reach machines under one scheme. Registered administratively, never declarable in a task file. |
| **verb** | One operation in the provider contract: `provision`, `exec`, `status`, `stream`, `cancel`, `run`, `ping`, `channel`, `cleanup`. |
| **channel** | A provider's *reused transport*: one long-lived process serving many commands instead of a fresh spawn per command. Opt-in per provider (`--channel`), with automatic fallback to per-command spawns. **Deliberately not called a "cell"**, which is already taken above. |
| **workspace** | A directory *plus the machine it lives on* (`crate::workspace::Workspace`). Introduced because a bare path answers "which folder" but not "which host". |
| **local** | Reserved machine value meaning the daemon's own host. Also the implicit default. |

## Storage and identity

| Term | Meaning |
|---|---|
| **project** | A registered git repository (`ralphus project git`), referenced by name from a task's `project` field. Lets a cell's `cwd` be a placeholder instead of a hardcoded path. |
| **worktree** | A git worktree — one checked-out branch. A cell works in one; a review builds its own per-branch ones. |
| **placeholder** (cwd) | `ralphus:new-worktree/<branch>` — a `cwd` naming a branch rather than a path. Materialized (or provisioned remotely) before the cell runs. |
| **placeholder** (review) | `ralphus:new-review/<key>` — a review id that mints a *fresh* review per submission. The key groups cells within one submission; it never attaches to a previous submission's review. A `[[task.cell]].review` reference to it (or to a plain existing `[[review]].id`) is always a **sentinel** (RAL-269): `<<ralphus:new-review/<key>>>` or `<<review:<id>>>`. |
| **ralphus URI** | RAL-188's addressing scheme for any entity: `ralphus:/SQUAD[label]/TASK[name]/CELL[name]?id=…`. Distinct from both placeholders above despite sharing the `ralphus:` prefix. |
| **ghost** | A short handoff note a cell or review worktree publishes for whoever picks up dependent work next — what it learned, where it struggled, what it left undone. Advisory, not a document store, and deliberately *not* a changelog (the diff already tells you what changed). |
| **Cartographer** | The unified, structured, queryable event log across the whole system. The primary logging mechanism; `rlog!` is the plain-text sink that fires alongside it. |
| **seat** | Secure-dist only: the `user@hostname` a license is locked to, so a copied `ralphus.lic` won't start elsewhere. Deliberately *not* called a **machine** — that word already means "where work runs" (RAL-185), and a seat names a person on a host, not a work destination. A license with no seat runs anywhere. |
| **env override** | One layer of the environment-variable hierarchy applied to a spawned subprocess: `squad < task < cell` for cells, extended by `…< proof scope < that individual step` for proof steps (RAL-150/172/191). A child layer wins per-key over its parents. Seeded from a TOML `environment` table at submit, or set later via the matching `POST …/env` endpoint — the two are indistinguishable once stored. |
| **agent profile** | A named `.ralphus.toml` entry under `[agent.profiles.<name>]` that resolves a task/cell `agent = "<name>"` to a concrete backend, optional executable override, and scoped per-cell environment. The profile is daemon config, not task-TOML schema. |
| **tombstone** | An env override whose value is `null` rather than a string: "remove this inherited variable entirely", as opposed to *clearing* the override (which restores the inherited value). Only review-branch overrides (RAL-191) have one, because only they layer over an environment inherited from a *different* entity — the branch's source cell. |
| **out of date** (env overrides) | RAL-271: a cosmetic, non-blocking badge on a task/cell/proof step whose own env overrides changed since it last ran/retried or had its status explicitly set. Purely informational — never invalidates a prior proof result. Not to be confused with `--drift` (PR/worktree divergence, RAL-190) or `--stale` (Live View pane liveness, RAL-170) — distinct concepts with their own colors, see `docs/colors.md`. |
| **ticket** | RAL-222: a short-lived (30s), single-use nonce that gates `/api/events` (SSE) in place of the long-lived bearer token, since `EventSource` cannot set an `Authorization` header. Minted via `POST /api/events/ticket`, consumed on first use. Distinct from the informal use of "ticket" for a Jira issue (RAL-…) elsewhere in this repo's docs/commit messages — context disambiguates. |
| **mailbox** | RAL-241: the cross-cutting escalation queue — a `mailbox_client` polls its unread `mailbox_messages` (`urgent`/`high`/`normal` priority) and drains them via `POST /api/mailbox/{client_id}/drain`. Broadcast-only in the poll-only scope: every message is visible to every registered client, with per-`(message_id, client_id)` read state in `mailbox_drains`. Distinct from a **ghost** (an advisory handoff note tied to one owner) — a mailbox message is a broadcast push about something needing attention, not a note left for whoever picks up dependent work next. |

## User identity (placeholder, pre-RAL-252)

There is no multi-user authentication in ralphus today. These names exist as
a seam for RAL-252 to fill in — see `TODO: Replace with user auth once
RAL-252 is done` comments at each one.

| Term | Meaning |
|---|---|
| **user** | A registered placeholder identity (`crate::users`, `users` table): just a name, no password, no session, no permissions. Grants nothing on its own — a caller can claim any registered name. **Distinct from a licensing seat** — see **seat** above, which names a person on a host for `secure-dist` locking, not a request identity. |
| **default_user** | The `[daemon]` config scalar (`.ralphus.toml`) naming which registered **user** a request is attributed to when it names none explicitly. |
| **UserContext** | The type (`daemon/src/agent_access.rs`) carrying a request's claimed user identity (`id: Option<String>`) through `AgentAccess`. Not a verified identity. |
| **AgentAccess** | The trait deciding which agents a `UserContext` may select (`GET /api/agents`). Only implementation today, `DefaultAgentAccess`, ignores the user and is permissive by design. |

## Scheduling

| Term | Meaning |
|---|---|
| **Pending** vs **Queued** | `Pending` is schedulable now. `Queued` is staged and held (`submit --hold`), needing `/activate`. A submission goes to Pending by default — the predecessor's silently-Queued-forever bug. |
| **soloed** | A task marked so the scheduler dispatches *only* soloed tasks' cells while any is set — everything else in the squad pauses. |
| **queue rank** | The live ordering hint for pending work, seeded from `priority` and owned by the Queue view/CLI thereafter. |
| **gating** | Cross-squad dependency: a squad waiting on another squad's completion. |
| **sentinel** | A `<<…>>` value resolved at squad time rather than authored literally — e.g. `upstream = "<<task:name>>"` rebases this cell's branch onto that dependency's tip. `[[task.cell]].review` is another: `<<review:<id>>>` or `<<ralphus:new-review/<key>>>` (RAL-269) — the bare, unwrapped form is a validation error. The reserved `?upstream=` sentinels on a `ralphus:new-worktree/<branch>` placeholder cwd are `<<default>>` (resolve to the repository's default branch — recommended) and `<<current_branch>>` (resolve to whatever branch the project currently has checked out — riskier, since it can change between runs). These `<<…>>` values are reserved: they are never treated as literal branch names, and any other `<<…>>` value is rejected at validation time. `depends_on` is deliberately NOT a sentinel: it's a bare-string lookup into IDs that already exist in the file, nothing about it is resolved or modified later. |
| **restart_on** | A proof step's declaration that another step firing should re-run this cell's proof cursor. Grammar: `task/cell/proof?on=pass\|fail\|both`. |

## Components

| Term | Meaning |
|---|---|
| **daemon** | `ralphus-daemon`. Owns *all* state; the SQLite DB is daemon-private. Everything else is a client of its HTTP API. |
| **librarian** | `ralphus-librarian`. Serves the web board and proxies `/api/*` GETs to the daemon. |
| **board** | The web UI itself (`librarian/assets/board.html`). |
| **runner** | `ralphus-runner`, the subprocess the daemon spawns per cell. Speaks `CellSpec` in / `CellResult` out over stdio. |
| **backend** | Inside the runner: the adapter for one agent kind (`claude`, `anthropic`, `ollama`, `claude-code`, `codex`, `raw`). Also the field name inside an agent profile that selects which adapter to use. Not to be confused with **provider** (a machine) or **agent** (the task/cell selector). |
| **agent** | The model-running program a cell uses (`claude`, `ollama`, `claude-code`, `codex`). Set per cell/task in TOML. |
| **executable** | In an agent profile, the optional program/path override for subprocess-spawning backends (`claude-code`, `codex`, `raw`). Never meaningful for native API backends (`claude`, `anthropic`, `ollama`). |
| **forge** | GitHub or GitLab, reached over their REST APIs for pull/merge requests. Never the `gh` CLI. |
| **harness** | Internal name for the generic external-CLI `ModelBackend` (`HarnessBackend`, `runner/src/harness_backend.rs`) that drives any executable as a subprocess with no protocol-specific stream parsing. Only reachable as a backend under the reserved name **raw** — not itself a valid `backend`/`agent` value. |
| **raw** | The explicit generic external-executable backend (implemented by the **harness** `ModelBackend`). Equivalent to the old implicit fallback, but now only valid when a profile also supplies `executable`. |
| **prism** | A desktop handoff ("Open in Prism"). Declared, not built. |

## Benchmarking (RAL-94)

| Term | Meaning |
|---|---|
| **durable minimum** | The stopping rule: re-run a test until `patience` consecutive runs fail to beat the best time. |
| **patience** | How many non-improving runs to tolerate before stopping. Setting it explicitly requires a comment saying why (enforced by `scripts/check_bench_patience_comments.py`). |

## Words to avoid taking

Already carrying weight; pick something else:

- **cell** — the pipeline concept (one agent/command invocation inside a task). Use **channel** for transport reuse.
- **squad** — a submission. Use "invocation" or "call" for a single execution.
- **agent** — the model program. Use **provider** for a machine, **backend** for a runner adapter.
- **branch** — ambiguous between a review's contributing branch and a git branch generally. Say which.
- **review** / **guardian** — the same thing; don't add a third name.
- **proof** — the pipeline step. Use "check" for a review's gates, which is what they're already called.
- **machine** — where work runs (RAL-185). Use **seat** for a licensing identity (`user@hostname`).

## See also

- [`machine-providers.md`](machine-providers.md) — the provider contract
- [`daemon-api.md`](daemon-api.md) — the wire shapes these names appear in
- [`colors.md`](colors.md) — the semantic colors, which have their own naming rules
