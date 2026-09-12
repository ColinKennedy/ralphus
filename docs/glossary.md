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

RAL-362 gave the board two separate tabs over this hierarchy, named for what
they actually show rather than what they historically showed: the
**Squads tab** (`#/squads`, previously misnamed "Tasks") is the squad-centric
dependency-graph/sidebar/details viewer; the **Tasks tab** (`#/tasks`) is a
flat, virtualized table listing every **task** across every squad, one row
per task. Neither viewer is the other's superset — the Squads tab shows the
full graph for one squad at a time, the Tasks tab is cross-squad but
task-grained (no graph, no cell-level browsing except an expanded row).

## Reviews

| Term | Meaning |
|---|---|
| **review** | The user-facing name for a merge review: several branches stacked, rebased, conflict-resolved, and gated by checks. Declared as `[[review]]` in TOML. |
| **guardian** | The *internal* name for the same thing — the stored entity (`guardian-…`), its module (`guardian.rs`, `guardian_merge.rs`), and its API routes (`/api/guardians/…`). Historical: "review" is what users see, "guardian" is what the code says. Expect both. |
| **branch** (in a review) | One contributing feature branch in a review's stack, with its own merge status. Not to be confused with the review's own `review_branch` (the combined head) or its `base_branch`. |
| **stacked rebase** | How a review merges: branch B is rebased onto branch A, which is rebased onto one snapshotted base commit. Linear, ordered, and the reason every worktree in a review must be on one machine. |
| **worktree lease** | Exclusive ownership of one review branch's mutable worktree, held by `run_feedback` for the duration of its resolver call. Different branches' leases may coexist; a queued restack (`restack_from_position`, or the downstream restack `run_feedback` triggers after its own branch's turn) only claims once every branch lease on the guardian is free, and multiple pending restack requests coalesce to the earliest position asked for. `drive_rebase` waits out a held lease before touching that branch's worktree, and rescues any edits it finds still uncommitted there into their own commit rather than letting the rebase discard them. |
| **base branch** | What the stack rebases onto (`main`, typically). Inferred from a worktree's git upstream for a local review; **declared** via `[[review]] upstream` when a remote cell feeds it (RAL-185 D4). Not to be confused with a cell's `?upstream=` cwd query param — see the next entry. |
| **`[[review]]`'s `upstream` key** vs **cell `?upstream=`** | Two unrelated, same-named things (RAL-284). A `[[review]]` block's `upstream` field (`ReviewDef::upstream`) declares the **base branch** (previous entry) a review's stack rebases onto — required when a remote cell feeds the review, since that worktree's git upstream can't be read from this host. A `ralphus:new-worktree/<branch>?upstream=<...>` cwd query param (RAL-100, see **sentinel** below) instead sets that *cell's own worktree's* git tracking upstream at creation time — a different, cell-level, already-shipped concept unrelated to any review. Don't conflate the two just because they share a name. |
| **check gate** | Commands run against the finished combined worktree before a review is considered done. From `[[review]]`'s checks, or a project's `auto_build`. |
| **carry-forward** / **carry refs** | Reusing a previous build's already-resolved conflict commits when a review is rebuilt, so the same conflict isn't resolved twice. |
| **combined worktree** | The read-only worktree at the head of the full stack — what a reviewer reads and what check gates run against. |
| **review branch** (RAL-378) | The branch a review builds one contributing **branch**'s rebased, conflict-resolved commits on — `<task branch>-review`, collision-suffixed `-2`/`-3` and then persisted (`guardian_branches.review_branch_name`) so it never moves under an already-open PR. A combined-worktree review has one, named from the review's own name instead. Distinct from the contributing branch itself (the previous entry's **branch**), which the review never writes to, and from the **PR branch**. Branches registered before RAL-378 keep the internal `guardian/<id>/wt-<branch>` ref. |
| **PR branch** (RAL-378) | The remote branch a review's pull request is opened from. By default it *is* the **review branch**, pushed under its own name — one branch, nothing to reconcile. `[review] separate_pr_branch = true` opts back out, deriving a second, differently-named remote branch from the task branch via `[forge] pull_request_branch_convention` (or `match_pr_branch_name`); those two settings mean nothing in the default mode, since there is no second name to choose. |
| **fork** (RAL-338) | The writable repository a project's review branches are pushed to when the acting user cannot push directly to the project's registered **parent**. Registered per `(project, user)` (`daemon/src/project_forks.rs`), with `user = ""` acting as the project-wide fallback row. Ralphus only *registers* an existing fork; it never creates one through a forge API. Do not use "upstream" or "origin" for this — both are already taken (see **base branch**'s `upstream` entry above, and the git remote convention). |
| **parent** (RAL-338) | A fork-enabled project's *non-fork* repository — the one it's registered against. Every alias in a fork-mode review is pushed to and fetched from the **fork**; only the lowest enabled unmerged branch's PR/MR is filed cross-repository against the parent's base branch. Every later branch stays fork-internal, based on the preceding branch's alias. |
| **promotion** (RAL-338) | Reconcile-first: once a fork-mode review's cross-repository root PR merges, closing the next enabled branch's fork-internal PR and reopening it against the **parent** in its place, since a same-repo base PATCH (the ordinary resync mechanism) can't move a PR across repositories. The superseded PR's row survives (`superseded_by` points at its replacement) so its discussion stays visible in `PrStackView` history. |

## Arbiter and Triage (RAL-318)

The internal-name/user-facing-name split here mirrors **review**/**guardian**
above: users see "Triage", the code says "Arbiter".

| Term | Meaning |
|---|---|
| **Arbiter** | The *internal* name for the daemon subsystem that classifies a Triage-opted-in cell into a registered **triage type** and pools cells toward an automatic review. Exactly one Arbiter exists per daemon — never per-project. Its own module is `daemon/src/arbiter.rs` (classification, the `ralphus check health` round-trip); the type registry and pool bookkeeping live in `daemon/src/triage.rs`. Cartographer notes it logs use `Note::new("arbiter")`. |
| **Triage** | The *user-facing* name for the same subsystem, and the cell-level opt-in: `[[task.cell]] triage = true`, optionally with an inline `triage_type`. What the board's Triage tab and `ralphus triage type …` name. |
| **triage type** | A registered category (`name`/`label`/`description`) a Triage-opted-in cell is classified into. Store-backed and CLI-mutable (`ralphus triage type register/list/get/deregister`) — mirrors the **machine** **provider** registry's pattern, deliberately *not* the **agent profile** pattern (config-file-only, no CLI mutation). The built-in `unclassified` type always exists and can never be deregistered — the permanent fallback for a classification failure (single attempt, no retry) or a submission with no registered types at all. |
| **Triage pool** | Cells opted into Triage, pooled by `(project, triage type)` — or, once a project opts into subproject keying (RAL-346), by `(project, subproject, triage type)` — until that pool's count threshold or one of its cron schedule entries fires — "first to fire wins", and firing resets the pool. New, persisted, cross-submission state (`triage_pool_cells`/`triage_pool_thresholds`/`triage_schedules`) — distinct from `derive_reviews`' existing per-*submission* grouping of explicit `[[review]]` cells, which holds no state between submissions. The `project` half of the key resolves to the registered **project**'s stable name (`crate::triage::pool_key_for_path`) whenever the cell's worktree matches one, falling back to a normalized worktree path only when no registered project matches — never the raw, unnormalized git-reported path (RAL-318 bug 3). A cell whose proof has definitively failed never counts toward a pool's threshold and is never swept into the review a drain produces, though it remains visible forever in the Triage tab's candidate list with a `"failed"` status. |
| **subproject** (RAL-346, keying) | The *broader*, keying/pooling sense: an extra dimension a Triage pool's key can carry (`crate::triage::SubprojectResolution`) so unrelated work in different corners of one monorepo doesn't share the same auto-review threshold — a plain project key `"myproj"` becomes a composite `"myproj::core"` when a cell resolves to subproject `"core"`. **Not the same thing** as the pre-existing `CellDef.subprojects` TOML field (`subprojects = ["packages/foo"]`), which only *scopes a single cell's own edits* to certain directories via a system-prompt addendum (`crate::runner::subproject_system_prompt_addendum`) and has no keying/pooling role of its own — it is, however, the *preferred seed* for this broader sense when non-empty. Every cell has one of three resolution states: **NotApplicable** (the project sets no `[monorepo] subprojects` in its `.ralphus.toml` at all — every single-project repo, forever), **Unresolved** (the project IS a monorepo, but nothing has matched this cell's subproject(s) yet — falls back to plain project-key pooling, identical to pre-RAL-346 behavior), or **Resolved** (a concrete, non-empty list, tagged `inferred: true`/`false` for whether the Arbiter's async description-matching call found it or the cell's own manual `CellDef.subprojects` seeded it directly). Two cells key into the same subproject pool on any overlap between their resolved sets, not exact-set equality — a "shared impact" test. Resolution happens asynchronously (`crate::arbiter::spawn_triage_followup`/`infer_subprojects`, sharing the Arbiter's own `[arbiter] maximum_budget_usd` cap), so it never blocks submission or cell pickup — a still-`Unresolved` cell simply pools by plain project key until (if ever) it resolves. The board shows a purple "Arbiter set this" badge on a cell whose subprojects were inferred (`inferred: true`), reusing the same `--arbiter` provenance badge as an auto-created review's **origin** badge. The Arbiter's cron straggler sweep (`crate::reviews::create_review_from_triage_project_sweep`) still reaches every subproject pool of a project regardless of each one's own threshold. |
| **resolver** (agent/model) | A **review**'s own conflict-resolving `agent`/`model` — the `ReviewDef` schema/wire fields are plain `agent`/`model`, but the CLI's `ralphus review settings --resolver-agent --resolver-model` and `.ralphus.toml`'s `[review] default_resolver_agent` both carry the `resolver-` prefix precisely to disambiguate from the **Arbiter**'s own `agent`/`model`/`maximum_budget_usd` (`[arbiter]` config, CLI-immutable) — resolving conflicts is not classifying work, so neither namespace is prefixed with the other (no `resolver_arbiter`/`arbiter_agent`); they simply never collide. |
| **origin** (guardian) | `GuardianView::origin`: `"explicit"` for a review created from an authored `[[review]]` block (or any other non-Arbiter path), `"arbiter"` for one created by draining a fired Triage pool. Backs the board's Arbiter badge and Reviews sidebar filter. |

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
| **target** | One statically-configured `[machine.targets.<name>]` entry (RAL-355 Phase 2): a `machine` value plus that machine's durable `remote_root` and runner policy. Config-file-only for v1 (`daemon/src/machine_targets.rs`, global scope only — no project-local override, unlike **agent profile**). A target's `machine` field holds the *full* `<scheme>:<uri>` value, never just the **uri** half, to avoid colliding with that already-taken term. |

## Storage and identity

| Term | Meaning |
|---|---|
| **project** | A registered git repository (`ralphus project git`), referenced by name from a task's `project` field. Lets a cell's `cwd` be a placeholder instead of a hardcoded path. |
| **worktree** | A git worktree — one checked-out branch. A cell works in one; a review builds its own per-branch ones. |
| **worktree retirement** | The daily daemon sweep that removes review worktrees idle past a 30-day threshold (`WORKTREE_RETIREMENT_AGE_MS`, `guardian_merge::retire_stale_worktrees`), keeping any held by a non-terminal claim (which raises one durable mailbox escalation instead). An unclaimed worktree first found eligible is not removed immediately: the sweep that discovers it sends one mailbox heads-up (tagged with the review's `guardian:<id>` entity_uri, so anyone watching the review sees it via `ralphus mailbox watch`) and only the *next* daily sweep that still finds it eligible and unclaimed actually removes it. For a machine-backed worktree, retirement runs through the owning provider's `retire` verb (RAL-386, see `docs/machine-providers.md`) instead of a local `git worktree remove`. Each worktree in the view (`GET /api/worktree-retirements`, `ralphus review worktree-retirements` — queryable by any user, admin or not — the board's admin-only ⏳ Retirement tab) is in exactly one state: **scheduled** (too young — `eligible_at_ms` says when it crosses the threshold), **eligible** (old enough — the advance mailbox notice has gone out; the sweep after that removes it), **claimed** (old enough but held by a live task/proof/review claim), **failed** (an attempt ran and was refused — the `error` stays visible until a retry succeeds), **deferred** (RAL-386: a machine provider asked to try again later — not a failure, `error` carries its reason and `retry_at_ms` its display-only hint), **opted_out** (RAL-386: a machine provider, or an operator's static `[machine.targets.*.retirement]` policy, declined to ever retire this worktree automatically — also not a failure), or **retired** (removed). "Eligible" deliberately avoids saying **stale** (already taken by Ark) and "retired" deliberately avoids **done/deployed** (taken by review and squad lifecycle states); **deferred**/**opted_out** are deliberately distinct from **failed** so an operator can tell "nobody is trying" or "try again later" from "something is broken" at a glance. Retired history is durable in `guardian_worktree_retirements` and lives exactly as long as its review's row, since a successful retirement clears the persisted path columns that would otherwise be the only trace. |
| **Ark** | The daemon subsystem that detects old ralphus-owned git worktrees, escalates old reviews, and can explicitly reap eligible checkouts after proving their commits exist on a remote. Preserved local refs live under `refs/ralphus/ark/`. Automatic deletion is gated off. |
| **placeholder** (cwd) | `ralphus:new-worktree/<branch>` — a `cwd` naming a branch rather than a path. Materialized (or provisioned remotely) before the cell runs. |
| **placeholder** (review) | `ralphus:new-review/<key>` — a review id that mints a *fresh* review per submission. The key groups cells within one submission; it never attaches to a previous submission's review. A `[[task.cell]].review` reference to it (or to a plain existing `[[review]].id`) is always a **sentinel** (RAL-269): `<<ralphus:new-review/<key>>>` or `<<review:<id>>>`. |
| **ralphus URI** | RAL-188's addressing scheme for any entity: `ralphus:/SQUAD[label]/TASK[name]/CELL[name]?id=…`. Distinct from both placeholders above despite sharing the `ralphus:` prefix. |
| **ghost** | A short handoff note a cell or review worktree publishes for whoever picks up dependent work next — what it learned, where it struggled, what it left undone. Advisory, not a document store, and deliberately *not* a changelog (the diff already tells you what changed). |
| **Cartographer** | The unified, structured, queryable event log across the whole system. The primary logging mechanism; `rlog!` is the plain-text sink that fires alongside it. |
| **seat** | Secure-dist only: the `user@hostname` a license is locked to, so a copied `ralphus.lic` won't start elsewhere. Deliberately *not* called a **machine** — that word already means "where work runs" (RAL-185), and a seat names a person on a host, not a work destination. A license with no seat runs anywhere. |
| **env override** | One layer of the environment-variable hierarchy applied to a spawned subprocess: `squad < task < cell` for cells, extended by `…< proof scope < that individual step` for proof steps (RAL-150/172/191). A child layer wins per-key over its parents. Seeded from a TOML `environment` table at submit, or set later via the matching `POST …/env` endpoint — the two are indistinguishable once stored. |
| **agent profile** | A named `.ralphus.toml` entry under `[agent.profiles.<name>]` that resolves a task/cell `agent = "<name>"` to a concrete backend, optional executable override, and scoped per-cell environment. The profile is daemon config, not task-TOML schema. |
| **resolved environment** | RAL-324: what one surface's environment variables actually come out as once every layer feeding it is folded together, lowest-precedence first — as opposed to an **env override**, which is a single layer. Read-only: served by the `GET` twin of each `POST .../env` route, rendered by the board's "🔎 Resolved env" popup and `ralphus <noun> env`. Values are masked only for names registered in the Secrets tab. |
| **tombstone** | An env override whose value is `null` rather than a string: "remove this inherited variable entirely", as opposed to *clearing* the override (which restores the inherited value). Only review-branch overrides (RAL-191) have one, because only they layer over an environment inherited from a *different* entity — the branch's source cell. |
| **out of date** (env overrides) | RAL-271: a cosmetic, non-blocking badge on a task/cell/proof step whose own env overrides changed since it last ran/retried or had its status explicitly set. Purely informational — never invalidates a prior proof result. Not to be confused with `--drift` (PR/worktree divergence, RAL-190) or `--stale` (Live View pane liveness, RAL-170) — distinct concepts with their own colors, see `docs/colors.md`. |
| **ticket** | RAL-222: a short-lived (30s), single-use nonce that gates `/api/events` (SSE) in place of the long-lived bearer token, since `EventSource` cannot set an `Authorization` header. Minted via `POST /api/events/ticket`, consumed on first use. Distinct from the informal use of "ticket" for a Jira issue (RAL-…) elsewhere in this repo's docs/commit messages — context disambiguates. |
| **mailbox** | RAL-241: the cross-cutting escalation queue — a `mailbox_client` polls its unread `mailbox_messages` (`urgent`/`high`/`normal` priority) and drains them via `POST /api/mailbox/{client_id}/drain`. Broadcast-only in the poll-only scope: every message is visible to every registered client, with per-`(message_id, client_id)` read state in `mailbox_drains`. Distinct from a **ghost** (an advisory handoff note tied to one owner) — a mailbox message is a broadcast push about something needing attention, not a note left for whoever picks up dependent work next. |
| **scrollback** | The tmux/psmux multiplexer's own in-memory history buffer for a pane, bounded by `history-limit` (`TMUX_HISTORY_LIMIT`, `daemon/src/tmux.rs`) — freed the moment the session ends. What `GET .../pane` (`capture_pane_reply`) reads. Distinct from **transcript** and **terminal log** below; the three used to be conflated before RAL-397 gave each its own name. |
| **transcript** | RAL-397: the durable, on-disk `.raw` file a cell's pane output is continuously teed to for its whole lifetime, via `Tmux::pipe_pane` (`ralphus-runner pipe-sink` as the target). Unbounded by pane **scrollback** depth — it exists specifically so a low `history-limit` doesn't lose anything. `GET .../pane-transcript` reads byte ranges of it directly; raw ANSI codes included, unlike the derived **terminal log** below. |
| **terminal log** | The durable, human-readable, ANSI-stripped `.log` file written once per attempt (RAL-154; `crate::terminal_log::write_attempt`) — as of RAL-397, derived from that attempt's **transcript** rather than a single **scrollback** snapshot. What `GET .../terminal-log-attempts` lists and `GET .../terminal-log-attempts/{n}` reads. A **transcript** is the raw, continuously-growing source; a **terminal log** is the one-time-written, redacted, plain-text record made from it. |

## User identity (placeholder, pre-RAL-252)

There is no multi-user authentication in ralphus today. These names exist as
a seam for RAL-252 to fill in — see `TODO: Replace with user auth once
RAL-252 is done` comments at each one.

| Term | Meaning |
|---|---|
| **user** | A registered placeholder identity (`crate::users`, `users` table): just a name, no password, no session, no permissions beyond `is_admin`'s convenience gating below. Grants nothing on its own — a caller can claim any registered name. **Distinct from a licensing seat** — see **seat** above, which names a person on a host for `secure-dist` locking, not a request identity. |
| **default_user** | The `[daemon]` config scalar (`.ralphus.toml`) naming which registered **user** a request is attributed to when it names none explicitly. |
| **hidden item** | A per-user view preference recording that one **squad** or **review** should be omitted from that user's normal views. It never changes the entity or another user's view. |
| **is_admin** | RAL-332: a boolean flag on a registered **user** gating the board's Machines/Triage/Projects/Users/Secrets tabs and their underlying endpoints, plus admin-only **Cartographer** row visibility. A UI-level convenience gate, not a real security boundary — anyone holding the daemon's shared bearer token can already reach every endpoint it gates directly (no verified login until RAL-252). |
| **UserContext** | The type (`daemon/src/agent_access.rs`) carrying a request's claimed user identity (`id: Option<String>`) through `AgentAccess`. Not a verified identity. |
| **AgentAccess** | The trait deciding which agents a `UserContext` may select (`GET /api/agents`). Only implementation today, `DefaultAgentAccess`, ignores the user and is permissive by design. |

## Monitor watches and notification preferences (RAL-343)

A per-**user** subscription layer over the existing **mailbox** (RAL-241),
not a second notification system. The internal subsystem is **Monitor**;
user-facing actions and relations use **watch** / **watcher**.

| Term | Meaning |
|---|---|
| **Monitor** | The internal subsystem that records watches and emits the bounded set of typed squad/review events eligible for watcher notification. |
| **watch** | One relation binding a **user** to a whole squad or review plus selected notify tiers. Re-watching updates tiers in place. A squad watch cascades to its tasks, cells, and proof steps through `EntityUri::covers`. The legacy SQLite table and user-preference column retain their `follows` / `auto_follow` names for database compatibility. |
| **watcher** | A user who has a watch on the named squad or review. |
| **notify tiers** (on a watch) | The subset of the mailbox's existing `urgent`/`high`/`normal` priority tiers a watch accepts. Distinct from **default notify tiers** (a user-level default). |
| **personal mailbox** | The per-user *view* over the same broadcast mailbox rows, filtered to events covered by that user's watches and tiers. Because both views use the same event-tagged row, being an owner and watcher cannot create duplicate messages. |
| **watch everything I create** | The default-on per-user preference persisted in the compatibility column `users.auto_follow`. It creates watches for future squads and reviews; disabling it does not remove existing watches. |
| **default notify tiers** (user preference) | A per-user preference used when a watch does not specify its own tiers, including automatic creator watches. |

## Scheduling

| Term | Meaning |
|---|---|
| **Pending** vs **Queued** | `Pending` is schedulable now. `Queued` is staged and held (`submit --hold`), needing `/activate`. A submission goes to Pending by default — the predecessor's silently-Queued-forever bug. |
| **soloed** | A task marked so the scheduler dispatches *only* soloed tasks' cells while any is set — everything else in the squad pauses. |
| **queue rank** | The live ordering hint for pending work, seeded from `priority` and owned by the Queue view/CLI thereafter. |
| **gating** | Cross-squad dependency: a squad waiting on another squad's completion. |
| **sentinel** | A `<<…>>` value resolved at squad time rather than authored literally — e.g. `upstream = "<<task:name>>"` rebases this cell's branch onto that dependency's tip. `[[task.cell]].review` is another: `<<review:<id>>>` or `<<ralphus:new-review/<key>>>` (RAL-269) — the bare, unwrapped form is a validation error. The reserved `?upstream=` sentinels on a `ralphus:new-worktree/<branch>` placeholder cwd are `<<default>>` (resolve to the repository's default branch — recommended) and `<<current_branch>>` (resolve to whatever branch the project currently has checked out — riskier, since it can change between runs). These `<<…>>` values are reserved: they are never treated as literal branch names, and any other `<<…>>` value is rejected at validation time. `depends_on` is deliberately NOT a sentinel: it's a bare-string lookup into IDs that already exist in the file, nothing about it is resolved or modified later. Every sentinel, plus the unattended-reply markers and the selector grammars, is tabulated in [`special-syntax.md`](special-syntax.md). |
| **marker** | A word-shaped line (always `RALPHUS_…`-prefixed) that ralphus **parses from an agent's reply or a subprocess's output**: `RALPHUS_PROOF:` (prompt-proof verdict), `RALPHUS_STILL_WORKING:` (continuation escape hatch), `RALPHUS_GHOST:` (handoff note), `RALPHUS_EVENT:` (subprocess stderr → Cartographer), `RALPHUS_TMUX_DONE:` (runner's pane-completion sentinel). Distinct from a **sentinel**, which lives in task-file TOML and resolves at squad time — though both are "machine-parsed words that trigger behavior", see [`special-syntax.md`](special-syntax.md). Only the marker itself is parsed; the framing around it in the assembled prompt is instruction. |
| **restart_on** | A proof step's declaration that another step firing should re-run this cell's proof cursor. Grammar: `task/cell/proof?on=pass\|fail\|both`. |
| **detach** | (RAL-288) Cleanly stopping a still-running cell's live process — without reporting it `Done` or `Failed` — so a real interactive agent session can safely take over the same conversation. The cell stays `Running`, paused, until an explicit **resume automation** call hands it back to unattended execution. |

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
| **read pool** | The daemon's fixed set of threads answering read-only (`GET`) HTTP requests off the accept loop (`daemon/src/server.rs`). `tiny_http` is otherwise one-request-at-a-time, so a `GET` that shells out to git or calls a **forge** would hold up every other request behind it — including a state transition like starting a review's rebase. Mutating methods deliberately stay on the accept loop, so they remain totally ordered with respect to each other. |

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
- **Arbiter** / **Triage** — the same subsystem; don't add a third name (mirrors **review**/**guardian**). "Triage" is user-facing, "Arbiter" is internal.
- **resolver** — a review's own conflict-resolving agent/model (RAL-318 disambiguation). Don't reuse it for the Arbiter's classification agent/model, or vice versa — prefix neither with the other.
- **upstream** / **origin** — already git jargon with their own ambiguity (a git remote name, `@{upstream}` tracking, `[[review]] upstream`'s base-branch meaning). Use **parent** for a fork-enabled project's non-fork repository (RAL-338).

## See also

- [`machine-providers.md`](machine-providers.md) — the provider contract
- [`daemon-api.md`](daemon-api.md) — the wire shapes these names appear in
- [`colors.md`](colors.md) — the semantic colors, which have their own naming rules
- [`fork-workflows.md`](fork-workflows.md) — fork registration, routing, and promotion (RAL-338)
- [`special-syntax.md`](special-syntax.md) — the words, sentinels, and markers that trigger behavior, and what isn't parsed
