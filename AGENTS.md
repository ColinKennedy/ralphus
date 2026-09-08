# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

**ralphus** orchestrates autonomous agent tasks: you submit work as TOML-described tasks, the system runs them with any model (cloud Anthropic or local Ollama), proves the results, and shows everything in a web board. It is a from-scratch successor to `C:\Users\korinkite\Documents\claudectl` (a Rust project whose task interconnection worked but whose *agent provisioning* did not). ralphus rebuilds the part that broke behind a model-agnostic runner that is exercised end-to-end by local models.

Working docs (all git-ignored via the global `*.local.md` rule — they are local-only notes, not committed):
- `PLAN.local.md` — the phased build plan with `- [ ]` / `- [r]` (AI-done, needs human check) / `- [x]` (verified) checkboxes.
- `FINDINGS.local.md` — research on the predecessor (what it was, why it failed, the TOML schema, Guardian mechanics, UI style).
- `FOLLOW.local.md` — deferred decisions to revisit, each with a "reassess when" trigger.
- `OLD_NOTES.local.md` — the original project brief.

## Taxonomy (RAL-239)

The task pipeline's vocabulary is: **squad** → **task** → **cell** → **proof**.
A squad contains tasks; a task contains cells; a cell (or a task directly) ends
in one or more proof steps. `docs/glossary.md` is the canonical definition of
each term and how they nest — check it before coining a new one.

Older code, commit history, and any doc that hasn't been swept yet may still
use the prior names for the same concepts. This table is here so a search for
the old term lands on the current one:

| Old term | Current term | Note |
|---|---|---|
| Run | Squad | One submission (`ralphus submit x.toml`), id prefix `squad-…`. |
| Task | Task | Unchanged — was never part of this rename. |
| Session | Cell | One agent/command invocation inside a task, `[[task.cell]]`. |
| Verify (the step) | Proof | A `command`/`prompt`/`brain`/`approval` check, `[[task.proof]]` / `[[task.cell.proof]]`. |

`ralphus-runner` (the binary/crate) keeps its name — it means "the thing that
runs a cell or proof step," not the Run-submission noun, so it was never part
of what renamed. The Guardian/Review naming split (code says Guardian, users
see Review) is a separate, unrelated naming decision and is untouched by this
rename.

## Agent conduct

Hard rules for any agent working in this repo — full reasoning and the
alternatives to reach for are in
[`.agent/agent-conduct.md`](.agent/agent-conduct.md):

- **Never start/stop/restart the daemon or librarian** (`scripts/build-debug.sh`,
  `ralphus-daemon serve`, ...) unless the user asks in that turn. A dev stack
  is usually already running, and a flaky health probe is not proof it is down.
- **Never run `git stash` in this repo** — the stash stack is shared across
  every worktree hanging off the same `.git`, and concurrent tasks collide on
  it. Use `git diff --stat`, `git show <ref>:path`, or a throwaway
  `git worktree add` instead.
- **Extend agent backends through their existing abstractions** — keep
  backend-specific behavior in the concrete Claude Code, Codex, Pi, and
  native-agent implementations, not daemon-side lookup tables. See
  [`.agent/agent-conduct.md`](.agent/agent-conduct.md).
- **Comments and docstrings describe the current code only** — no "used to",
  "originally", removed-`TODO` narration, or port history.
- **Commit messages omit the `Claude-Session:` trailer**; keep `Co-Authored-By:`.
- **Say what you could not verify.** A partial test run is never reported as
  full coverage.
- **Whenever a new field is added to the task-file schema, ask the user
  whether/how it should be validated** before writing `core/src/validate.rs`
  logic for it — a closed enum, a numeric bound, or deliberately unvalidated
  are all plausible, and guessing wrong either rejects a wanted value or lets
  a typo through.
- **Never run `cargo test --all-targets` / `cargo nextest run --all-targets`
  when ralphus itself is the one driving the work** (a ralphus cell/proof
  step developing ralphus, i.e. dogfooding) — that cell's own dev daemon/
  librarian exe is locked from underneath it, so `--all-targets` doesn't just
  fail, it often hangs. Use `cargo nextest run -p ralphus-daemon --lib` (and
  the same for `ralphus-librarian`) instead. See `.agent/gotchas.md`.

## Architecture

Eleven Rust workspace members; `cli-py/` is a Python project kept only for `docsgen/` (doc screenshot generation, dev-only, never shipped) and a trimmed `bench/` (renders the Rust bench harness's SVG/HTML graphs) — see `cli-py/AGENTS.md`. The **daemon owns all state**; the CLI and librarian are clients of its HTTP/JSON API (`docs/daemon-api.md`). The SQLite DB is daemon-private.

| Component | Path | Language | Role | Details |
|---|---|---|---|---|
| `ralphus-core` | `core/` | Rust lib | Task-file schema + validator + shared types (incl. `agent_resume`, `uri`). Dependency-light, heavily unit-tested. | |
| `ralphus-daemon` | `daemon/` | Rust bin+lib | SQLite store (WAL), HTTP API, scheduler; spawns the runner per cell. | `daemon/AGENTS.md` |
| `ralphus-librarian` | `librarian/` | Rust bin+lib | Web board; serves static HTML and proxies `/api/*` GETs to the daemon. | `librarian/AGENTS.md` |
| `ralphus-cli` | `cli/` | Rust bin (`ralphus`) | The CLI: validate/submit/status/squad/task/cell/proof/review/queue/project/machine/agent/show/check/quick-start/... — a thin HTTP client over the daemon's API, ~105 leaf subcommands. | `cli/AGENTS.md` |
| `ralphus-mcp` | `mcp/` | Rust bin+lib (`ralphus-mcp`) | MCP server (RAL-301) exposing the same ~105-command surface as MCP tools, over stdio JSON-RPC — talks to the daemon's HTTP API directly via `ralphus-cli`'s `DaemonClient` (reused as a library), no dependency on the compiled `ralphus` binary. Tool listing/schemas are derived from `cli/src/help_map.rs`'s tree; `--read-only` preserves the CLI's read-only-safe/mutating split. `mcp/tests/parity.rs` enforces bidirectional CLI↔tool parity (every non-excluded leaf has a tool and vice versa) as a normal `cargo test`, with a documented-reason-required exclusion list in `mcp/src/exclusions.rs` for the handful of commands (`cell open-agent`, `quick-start` backends) that spawn an interactive TTY session and have no single-request/response shape. | |
| `ralphus-runner` | `runner/` | Rust bin+lib | The cell runner: executes one `CellSpec` (command/prompt/proof), reports a `CellResult`. Spawned per-cell by the daemon over the same stdin/stdout JSON contract the old Python runner used. | see `.agent/cli-runner-port.md` |
| `ralphus-auth` | `auth/` | Rust lib | Ed25519 license verification (no-op without `--features secure-dist`). | `auth/AGENTS.md` |
| `ralphus-keygen` | `keygen/` | Rust bin | Author-only tool: generate keypair + sign licenses. Never shipped to users. | `keygen/AGENTS.md` |
| `ralphus-ssh-provider` | `ssh-provider/` | Rust bin+lib | Machine provider (RAL-185) reaching any host already SSH-accessible; `exec` verb only (RAL-200), see `docs/machine-providers.md`. | |
| `ralphus-bench-types` | `bench-types/` | Rust lib | `BenchMeta` only — kept dependency-free to avoid a cyclic dependency between bench-tagged crates and the harness (RAL-94). | `bench-harness/AGENTS.md` |
| `ralphus-bench-macros` | `bench-macros/` | Rust proc-macro | `#[ralphus_bench(patience = N)]` attribute (RAL-94). | `bench-harness/AGENTS.md` |
| `ralphus-bench-harness` | `bench-harness/` | Rust lib+bin | Durable-minimum loop, stats/git/storage, `ralphus-bench-rs` opt-in entry point (RAL-94). | `bench-harness/AGENTS.md` |

`cli` and `runner` were ported from Python to Rust as one project (module-for-module mirrors, shared `shellcmd.rs`/`agent_resume.rs`, the disclosed `ralphus author` gap) — see [`.agent/cli-runner-port.md`](.agent/cli-runner-port.md) for the full detail; it's cross-cutting so it isn't owned by either crate's own `AGENTS.md`.

### Key module map

- `core/src/schema.rs` — `TaskFile`/`TaskDef`/`CellDef`/`ProofStep`, `ResolvedAgent` inheritance. Cell `prompt` XOR `command`. Also owns `RESERVED_AGENT_NAMES` (the built-in agent backend names + aliases, e.g. `claude-code`/`codex`/`ollama`) — **whenever a new agent backend/harness is added, update this list too** (`daemon/src/agent_profiles.rs` reuses it as the reserved set custom `[agent.profiles.*]` names can't collide with, and `core/src/validate.rs`'s `system_prompt` check uses it to decide which agent names it can classify offline vs. must defer to the daemon).
- `core/src/validate.rs` — raw-`toml::Value` validator: unknown keys, required fields, types, proof one-of, `restart_on` grammar, within-task dep cycles, 1-based line numbers.
- `daemon/src/store.rs` — `Store` (the only place SQL lives), `SquadState`/`NodeState`, board views.
- `daemon/src/server.rs` — `route()` (pure, unit-testable) + `serve()` (tiny_http; starts the scheduler thread).
- `daemon/src/scheduler.rs` — claims Pending squads, worker threads run cells + proofs. Subprocess waits happen OUTSIDE the store lock.
- `daemon/src/runner.rs` — `Runner` trait + `SubprocessRunner` (spawns `RALPHUS_RUNNER_CMD`).
- `daemon/src/proof.rs` — `command` proof execution.
- `daemon/src/plan.rs` — dependency graph (Kahn topological sort).
- `daemon/src/guardian.rs` — Guardian store + state machine (Collecting→Approved→Deployed).
- `daemon/src/guardian_merge.rs` — stacked linear rebase in a worktree, agent conflict resolution.
- `daemon/src/reviews.rs` — review derivation (per-guardian, per-branch merge status).
- `daemon/src/project_forks.rs` — per-project, per-user fork registration; fork-aware PR/MR routing and reconcile-first promotion live in `pr.rs`/`forge.rs` (RAL-338, see [`docs/fork-workflows.md`](docs/fork-workflows.md)).
- `daemon/src/config.rs` — layered config (global + per-project `.ralphus.toml`).
- `daemon/src/cartographer.rs` — Cartographer: the unified, structured, cross-system event log — see [`.agent/logging-policy.md`](.agent/logging-policy.md).
- `daemon/src/logging.rs` — the `rlog!` file/stderr sink (RAL-83); see [`.agent/logging-policy.md`](.agent/logging-policy.md).
- `daemon/src/terminal_log.rs` — durable, per-attempt tmux pane transcript capture (RAL-154); see the tmux/psmux gotcha in [`.agent/gotchas.md`](.agent/gotchas.md).
- `daemon/src/entity_uri.rs` — RAL-155: the single-string, index-based `EntityUri` grammar, mirrored in `cli/src/entity_uri.rs`.
- `daemon/src/timeline.rs` — RAL-155: `build_squad_timeline` merges a whole squad's Cartographer rows with inlined terminal-log excerpts. Backs `GET /api/squads/{id}/timeline` and the board's "⏱ Timeline" button.
- `auth/src/lib.rs` — Ed25519 license check (`check_license()`; compiles away without `secure-dist`) — see `auth/AGENTS.md`.
- `keygen/src/main.rs` — keypair generation + license signing CLI — see `keygen/AGENTS.md`.
- `librarian/assets/` — the entire dark-theme UI: `board.html` shell + `board/*.js` global-scope chunks + `board.css` + vendored xterm files; `build.rs` bakes them into the exe, dev mode reads them from disk — see `librarian/AGENTS.md`.

## Build / Test / Lint

Every commit must pass all checks. **Rust is strict**: `[workspace.lints]` sets `warnings = "deny"`, `unsafe_code = "forbid"`, `clippy::all = "deny"`.

```bash
# Rust (from repo root)
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo nextest run --workspace        # run one: cargo nextest run -p ralphus-daemon scheduler::
# ...but while the dev daemon/librarian are running, their exes are locked and
# can't be relinked: use `cargo nextest run -p ralphus-daemon --lib`
# for those two packages. See .agent/gotchas.md.

# Python (from cli-py/, uv-managed) — see cli-py/AGENTS.md for the test-file table
uv sync --dev && uv run ruff check . && uv run ruff format --check . && uv run mypy && uv run pytest

# Bench patience comment lint (from repo root, stdlib-only, no uv sync needed)
python scripts/check_bench_patience_comments.py

# Web (from repo root; lints/type-checks the librarian board chunks) — see librarian/AGENTS.md
npm install --no-audit --no-fund && npm run lint && npm run typecheck && npm run knip && npm test
```

CI is `.github/workflows/ci.yml` (a Rust job, a Python job, and a web job).

Component-specific build/test detail (Rust integration tests, Python test
table, the board chunks' JSDoc/lint/knip/frontend-test rules, bench harness
commands, running the dev stack) lives in each folder's own `AGENTS.md` —
see the Documentation Map below.

## Running it

`bash scripts/build-debug.sh` for the fast incremental dev loop (daemon +
librarian), `bash scripts/build-release.sh` for standalone `dist/` exes, or
invoke `ralphus-daemon serve` / `ralphus-librarian serve` / `ralphus ...`
directly once built. Multi-instance dev stacks, the container execution mode
(RAL-225), and `dist/` layout are all in [`scripts/AGENTS.md`](scripts/AGENTS.md);
building the `docs/site/` HTML documentation is `bash scripts/docs-build.sh`
(fast path) or `scripts/docs-screenshots.sh` (regenerates screenshots) — see
[`docs/docs-site.md`](docs/docs-site.md).

Common one-off testing/debugging commands (validate a task file, curl the
daemon API, run a single test) are in
[`.agent/manual-testing-commands.md`](.agent/manual-testing-commands.md).

## Cryptography / Secure Distribution

Opt-in `--features secure-dist` build mode where the daemon/librarian refuse
to start without a signed `ralphus.lic`. Full workflow, key files, and
license format: [`auth/AGENTS.md`](auth/AGENTS.md) (and `keygen/AGENTS.md`
for the signing tool). See also [`docs/secure-dist.md`](docs/secure-dist.md).

## Vocabulary

**ralphus has taken words.** `squad`, `task`, `cell`, `proof`, `review`,
`guardian`, `agent`, `backend`, `provider`, `machine`, `channel`, `ghost`,
`project`, `worktree` and others all mean something specific here, and several
of them nest in a way that matters (a squad contains tasks, which contain
cells, which contain proof steps).

**Before coining a term — for a concept, a struct, a field, a doc — check
[`docs/glossary.md`](docs/glossary.md).** Reusing a taken word makes both
meanings harder to read, and a collision is painful to undo once it has reached
the schema, the store, and the board. That document also lists the words already
carrying too much weight to take, with the alternative to use instead.

**When you take a new word, add it to the glossary.**

## Design / UI colors

Any color choice in the web board or any UI must use a
documented CSS variable from [`docs/colors.md`](docs/colors.md) — never a
hardcoded hex value. Full rules and the tooltip requirement that pairs with
every new UI element: [`librarian/AGENTS.md`](librarian/AGENTS.md).

## Logging Policy (RAL-79, amended by Cartographer / RAL-98)

Every notable event (state transitions, LLM calls, manual actions) must be
logged to stderr **and** written as a structured Cartographer row; stdout is
reserved for the daemon↔runner JSON wire contract and is mechanically
enforced via `clippy::print_stdout = "deny"`. This is cross-cutting across
`daemon/`, `runner/`, and `cli/` — full policy, the log-format table, and
the `tracing`-crate ruling: [`.agent/logging-policy.md`](.agent/logging-policy.md).

## OpenTelemetry tracing (RAL-96)

Opt-in, end-to-end trace of a user action through librarian → daemon →
scheduler → runner subprocess, viewable as a flame/waterfall graph. No-op
unless `OTEL_EXPORTER_OTLP_ENDPOINT` is set. Full design:
[`.agent/otel-tracing.md`](.agent/otel-tracing.md) and
[`docs/otel-tracing.md`](docs/otel-tracing.md).

## Gotchas learned the hard way

Cross-component pitfalls (bind-address exposure, cell `cwd` path format, the
tmux/psmux reattach mitigation, port clashes, submit-goes-to-Pending, ...)
are collected in [`.agent/gotchas.md`](.agent/gotchas.md) — read it before
debugging anything that looks like "this used to work."

## Status

What's built (Phases 0–5) and what's explicitly not yet built (proof
retry policy, multi-round Guardian cycles, detached daemon lifecycle, the
draggable node-graph canvas, ...): [`.agent/roadmap.md`](.agent/roadmap.md).

## Documentation Map

Full detail has moved out of this file into per-component docs so an agent
only loads what's relevant to the folder it's touching. Subfolder `AGENTS.md`
files (each paired with a `CLAUDE.md` containing `@AGENTS.md`):

- [`daemon/AGENTS.md`](daemon/AGENTS.md) — store/scheduler/API detail, Rust integration test table
- [`librarian/AGENTS.md`](librarian/AGENTS.md) — board chunks JSDoc/lint/knip/tooltip/color rules
- [`cli/AGENTS.md`](cli/AGENTS.md) — CLI module map, Read-Only Quick-Start Safety List
- [`mcp/AGENTS.md`](mcp/AGENTS.md) — MCP server module map, tool-generation/exclusion/parity-check detail
- [`cli-py/AGENTS.md`](cli-py/AGENTS.md) — Python docsgen/bench-graph project, its test table
- [`auth/AGENTS.md`](auth/AGENTS.md) — secure-dist license format and signing workflow
- [`keygen/AGENTS.md`](keygen/AGENTS.md) — pointer to `auth/AGENTS.md`
- [`bench-harness/AGENTS.md`](bench-harness/AGENTS.md) — RAL-94 harness design, patience-comment rule
- [`bench-macros/AGENTS.md`](bench-macros/AGENTS.md) — pointer to `bench-harness/AGENTS.md`
- [`bench-types/AGENTS.md`](bench-types/AGENTS.md) — pointer to `bench-harness/AGENTS.md`
- [`scripts/AGENTS.md`](scripts/AGENTS.md) — build-debug/build-release/container-mode/docs-site usage

`.agent/` files for content that doesn't belong to one component:

- [`.agent/agent-conduct.md`](.agent/agent-conduct.md) — hard rules for agents working here (daemon, stash, comments, commits)
- [`.agent/logging-policy.md`](.agent/logging-policy.md) — stderr+Cartographer logging rules, log-type table
- [`.agent/otel-tracing.md`](.agent/otel-tracing.md) — opt-in end-to-end OpenTelemetry tracing design
- [`.agent/gotchas.md`](.agent/gotchas.md) — cross-component pitfalls learned the hard way
- [`.agent/roadmap.md`](.agent/roadmap.md) — what's built vs. not-yet-built by phase
- [`.agent/cli-runner-port.md`](.agent/cli-runner-port.md) — Python-to-Rust CLI/runner port detail
- [`.agent/manual-testing-commands.md`](.agent/manual-testing-commands.md) — copy-paste commands for ad-hoc manual testing
