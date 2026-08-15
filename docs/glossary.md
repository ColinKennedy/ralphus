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
| **run** | One *submission*. What `ralphus submit x.toml` creates: the whole TOML file's worth of work, with its own id (`run-…`), state, and dependency graph. A run contains tasks. |
| **task** | One unit of work inside a run, named by the author (`[[task]] name = "ral-169"`). Owns a workspace, a project, and — since RAL-185 — a machine. A task contains sessions. |
| **session** | One agent (or command) invocation inside a task (`[[task.session]]`). The thing that actually runs a model or a shell command. Sessions in a task share a workspace and hand off through files on disk. |
| **verify** / **verify step** | A check that runs *after* its owning session (session-scope) or after all of a task's sessions (task-scope). Kinds: `command` (exit code is the verdict), `prompt` (an agent returns `RALPHUS_VERIFY: PASS/FAIL`), `brain` and `approval` (declared, not yet implemented). |

Do **not** reuse *session* for anything else — notably not for connection reuse
or transport (see **channel**).

## Reviews

| Term | Meaning |
|---|---|
| **review** | The user-facing name for a merge review: several branches stacked, rebased, conflict-resolved, and gated by checks. Declared as `[[review]]` in TOML. |
| **guardian** | The *internal* name for the same thing — the stored entity (`guardian-…`), its module (`guardian.rs`, `guardian_merge.rs`), and its API routes (`/api/guardians/…`). Historical: "review" is what users see, "guardian" is what the code says. Expect both. |
| **branch** (in a review) | One contributing feature branch in a review's stack, with its own merge status. Not to be confused with the review's own `review_branch` (the combined head) or its `base_branch`. |
| **stacked rebase** | How a review merges: branch B is rebased onto branch A, which is rebased onto one snapshotted base commit. Linear, ordered, and the reason every worktree in a review must be on one machine. |
| **base branch** | What the stack rebases onto (`main`, typically). Inferred from a worktree's git upstream for a local review; **declared** via `[[review]] base` when a remote session feeds it (RAL-185 D4). |
| **check gate** | Commands run against the finished combined worktree before a review is considered done. From `[[review]]`'s checks, or a project's `auto_build`. |
| **carry-forward** / **carry refs** | Reusing a previous build's already-resolved conflict commits when a review is rebuilt, so the same conflict isn't resolved twice. |
| **combined worktree** | The read-only worktree at the head of the full stack — what a reviewer reads and what check gates run against. |

## Machines and providers (RAL-185)

| Term | Meaning |
|---|---|
| **machine** | Where work runs, declared as `machine = "<scheme>:<uri>"` on a task, session, verify step, or review. Unset means the daemon's own host. |
| **scheme** | The left half of a machine value (`incredibuild`). Names a registered **provider**. |
| **uri** | The right half (`A`, a hostname, a URL). **Opaque** — ralphus never interprets it; it's handed to the provider verbatim. |
| **provider** | A registered executable the daemon runs to reach machines under one scheme. Registered administratively, never declarable in a task file. |
| **verb** | One operation in the provider contract: `provision`, `exec`, `status`, `stream`, `cancel`, `run`, `ping`, `channel`, `cleanup`. |
| **channel** | A provider's *reused transport*: one long-lived process serving many commands instead of a fresh spawn per command. Opt-in per provider (`--channel`), with automatic fallback to per-command spawns. **Deliberately not called a "session"**, which is already taken above. |
| **workspace** | A directory *plus the machine it lives on* (`crate::workspace::Workspace`). Introduced because a bare path answers "which folder" but not "which host". |
| **local** | Reserved machine value meaning the daemon's own host. Also the implicit default. |

## Storage and identity

| Term | Meaning |
|---|---|
| **project** | A registered git repository (`ralphus project git`), referenced by name from a task's `project` field. Lets a session's `cwd` be a placeholder instead of a hardcoded path. |
| **worktree** | A git worktree — one checked-out branch. A session works in one; a review builds its own per-branch ones. |
| **placeholder** (cwd) | `ralphus:new-worktree/<branch>` — a `cwd` naming a branch rather than a path. Materialized (or provisioned remotely) before the session runs. |
| **placeholder** (review) | `ralphus:new-review/<key>` — a review id that mints a *fresh* review per submission. The key groups sessions within one submission; it never attaches to a previous submission's review. |
| **ralphus URI** | RAL-188's addressing scheme for any entity: `ralphus:/RUN[label]/TASK[name]/SESSION[name]?id=…`. Distinct from both placeholders above despite sharing the `ralphus:` prefix. |
| **ghost** | A short handoff note a session or review worktree publishes for whoever picks up dependent work next — what it learned, where it struggled, what it left undone. Advisory, not a document store, and deliberately *not* a changelog (the diff already tells you what changed). |
| **Cartographer** | The unified, structured, queryable event log across the whole system. The primary logging mechanism; `rlog!` is the plain-text sink that fires alongside it. |
| **seat** | Secure-dist only: the `user@hostname` a license is locked to, so a copied `ralphus.lic` won't start elsewhere. Deliberately *not* called a **machine** — that word already means "where work runs" (RAL-185), and a seat names a person on a host, not a work destination. A license with no seat runs anywhere. |
| **env override** | One layer of the environment-variable hierarchy applied to a spawned subprocess: `run < task < session` for sessions, extended by `…< verify scope < that individual step` for verify steps (RAL-150/172/191). A child layer wins per-key over its parents. Seeded from a TOML `environment` table at submit, or set later via the matching `POST …/env` endpoint — the two are indistinguishable once stored. |
| **tombstone** | An env override whose value is `null` rather than a string: "remove this inherited variable entirely", as opposed to *clearing* the override (which restores the inherited value). Only review-branch overrides (RAL-191) have one, because only they layer over an environment inherited from a *different* entity — the branch's source session. |

## Scheduling

| Term | Meaning |
|---|---|
| **Pending** vs **Queued** | `Pending` is schedulable now. `Queued` is staged and held (`submit --hold`), needing `/activate`. A submission goes to Pending by default — the predecessor's silently-Queued-forever bug. |
| **soloed** | A task marked so the scheduler dispatches *only* soloed tasks' sessions while any is set — everything else in the run pauses. |
| **queue rank** | The live ordering hint for pending work, seeded from `priority` and owned by the Queue view/CLI thereafter. |
| **gating** | Cross-run dependency: a run waiting on another run's completion. |
| **sentinel** | A `<<…>>` value resolved at run time rather than authored literally — e.g. `upstream = "<<task:name>>"` rebases this session's branch onto that dependency's tip. |
| **restart_on** | A verify step's declaration that another step firing should re-run this session's verify cursor. Grammar: `task/session/verify?on=pass\|fail\|both`. |

## Components

| Term | Meaning |
|---|---|
| **daemon** | `ralphus-daemon`. Owns *all* state; the SQLite DB is daemon-private. Everything else is a client of its HTTP API. |
| **librarian** | `ralphus-librarian`. Serves the web board and proxies `/api/*` GETs to the daemon. |
| **board** | The web UI itself (`librarian/assets/board.html`). |
| **runner** | `ralphus-runner`, the Python subprocess the daemon spawns per session. Speaks `SessionSpec` in / `SessionResult` out over stdio. |
| **backend** | Inside the runner: the adapter for one agent kind (`pydantic`, `claude-code`, `codex`, `harness`). Not to be confused with **provider** (a machine) or **agent** (the model program). |
| **agent** | The model-running program a session uses (`claude`, `ollama`, `claude-code`, `codex`). Set per session/task in TOML. |
| **forge** | GitHub or GitLab, reached over their REST APIs for pull/merge requests. Never the `gh` CLI. |
| **harness** | The backend that drives an *external* agent CLI as a subprocess. |
| **prism** | A desktop handoff ("Open in Prism"). Declared, not built. |

## Benchmarking (RAL-94)

| Term | Meaning |
|---|---|
| **durable minimum** | The stopping rule: re-run a test until `patience` consecutive runs fail to beat the best time. |
| **patience** | How many non-improving runs to tolerate before stopping. Setting it explicitly requires a comment saying why (enforced by `scripts/check_bench_patience_comments.py`). |

## Words to avoid taking

Already carrying weight; pick something else:

- **session** — the pipeline concept. Use **channel** for transport reuse.
- **run** — a submission. Use "invocation" or "call" for a single execution.
- **agent** — the model program. Use **provider** for a machine, **backend** for a runner adapter.
- **branch** — ambiguous between a review's contributing branch and a git branch generally. Say which.
- **review** / **guardian** — the same thing; don't add a third name.
- **verify** — the pipeline step. Use "check" for a review's gates, which is what they're already called.
- **machine** — where work runs (RAL-185). Use **seat** for a licensing identity (`user@hostname`).

## See also

- [`machine-providers.md`](machine-providers.md) — the provider contract
- [`daemon-api.md`](daemon-api.md) — the wire shapes these names appear in
- [`colors.md`](colors.md) — the semantic colors, which have their own naming rules
