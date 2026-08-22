# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

**ralphus** orchestrates autonomous agent tasks: you submit work as TOML-described tasks, the system runs them with any model (cloud Anthropic or local Ollama), verifies the results, and shows everything in a web board. It is a from-scratch successor to `C:\Users\korinkite\Documents\claudectl` (a Rust project whose task interconnection worked but whose *agent provisioning* did not). ralphus rebuilds the part that broke behind a model-agnostic runner that is exercised end-to-end by local models.

Working docs (all git-ignored via the global `*.local.md` rule — they are local-only notes, not committed):
- `PLAN.local.md` — the phased build plan with `- [ ]` / `- [r]` (AI-done, needs human check) / `- [x]` (verified) checkboxes.
- `FINDINGS.local.md` — research on the predecessor (what it was, why it failed, the TOML schema, Guardian mechanics, UI style).
- `FOLLOW.local.md` — deferred decisions to revisit, each with a "reassess when" trigger.
- `OLD_NOTES.local.md` — the original project brief.

## Architecture

Eleven Rust workspace members; `cli/` is a Python project kept only for `docsgen/` (doc screenshot generation, dev-only, never shipped — see "The Rust CLI/runner port" below) and a trimmed `bench/` (renders the Rust bench harness's SVG/HTML graphs — see "RAL-94 benchmark harness" below). The **daemon owns all state**; the CLI and librarian are clients of its HTTP/JSON API (`docs/daemon-api.md`). The SQLite DB is daemon-private.

| Component | Path | Language | Role |
|---|---|---|---|
| `ralphus-core` | `core/` | Rust lib | Task-file schema + validator + shared types (incl. `agent_resume`, `uri`). Dependency-light, heavily unit-tested. |
| `ralphus-daemon` | `daemon/` | Rust bin+lib | SQLite store (WAL), HTTP API, scheduler; spawns the runner per session. |
| `ralphus-librarian` | `librarian/` | Rust bin+lib | Web board; serves static HTML and proxies `/api/*` GETs to the daemon. |
| `ralphus-cli` | `cli-rs/` | Rust bin (`ralphus`) | The CLI: validate/submit/status/run/task/session/verify/review/queue/project/machine/agent/show/check/quick-start/... — a thin HTTP client over the daemon's API, ~105 leaf subcommands (see "The Rust CLI/runner port" below). |
| `ralphus-runner` | `runner/` | Rust bin+lib | The session runner: executes one `SessionSpec` (command/prompt/verify), reports a `SessionResult`. Spawned per-session by the daemon over the same stdin/stdout JSON contract the old Python runner used. |
| `ralphus-auth` | `auth/` | Rust lib | Ed25519 license verification (no-op without `--features secure-dist`). |
| `ralphus-keygen` | `keygen/` | Rust bin | Author-only tool: generate keypair + sign licenses. Never shipped to users. |
| `ralphus-ssh-provider` | `ssh-provider/` | Rust bin+lib | Machine provider (RAL-185) reaching any host already SSH-accessible; `exec` verb only (RAL-200), see `docs/machine-providers.md`. |
| `ralphus-bench-types` | `bench-types/` | Rust lib | `BenchMeta` only — kept dependency-free to avoid a cyclic dependency between bench-tagged crates and the harness (RAL-94). |
| `ralphus-bench-macros` | `bench-macros/` | Rust proc-macro | `#[ralphus_bench(patience = N)]` attribute (RAL-94). |
| `ralphus-bench-harness` | `bench-harness/` | Rust lib+bin | Durable-minimum loop, stats/git/storage, `ralphus-bench-rs` opt-in entry point (RAL-94). |

### The Rust CLI/runner port

`ralphus` (the CLI) and `ralphus-runner` were originally Python + pydantic-ai (see the git-ignored `PYTHON_DEPRECATION.local.md` audit for the full history/rationale). Both are now native Rust (`cli-rs/`, `runner/`), and `dist/` is 100% Rust — no PyInstaller, no bundled interpreter, no `_internal/` directory to keep beside an exe. The daemon<->runner wire contract (`SessionSpec`/`SessionResult` JSON on stdin/stdout, `RALPHUS_EVENT:`/`RALPHUS_VERIFY:` stderr markers) is byte-for-byte unchanged; only the language producing/consuming it changed.

- `runner/src/` mirrors the old `cli/src/ralphus/runner/` module-for-module: `spec.rs` (wire contract), `tools.rs` (workspace-confined file/shell tools), `backend.rs` (the `ModelBackend` trait), `claude_code_backend.rs`/`codex_backend.rs`/`harness_backend.rs` (external-CLI-agent + generic-harness backends), `agent_backend.rs` + `llm_client.rs` + `providers.rs` (the hand-rolled Anthropic Messages API / Ollama OpenAI-compatible client and tool-calling loop — the one genuinely new piece; neither Python original used any pydantic *validation* feature, just provider selection and, for the runner, a tool loop), `execute.rs` (system-prompt composition, still-working retry loop, verdict/ghost marker parsing), `shellcmd.rs` (parent-shell detection + quoting, now using the `sysinfo` crate uniformly instead of separate Windows-ctypes/Linux-`/proc` code paths), `cli_agent_common.rs`, `otel.rs`, `cartographer.rs`, `config.rs`.
- `cli-rs/src/` mirrors `cli/src/ralphus/`: `client.rs` (`DaemonClient`, all 77 methods, including the RAL-203 guardian-env redaction chokepoint), `selector.rs` (run/guardian selector grammar + resolution), `entity_uri.rs`, `config.rs`, `agents.rs`, `output.rs`, `tutor.rs`, `graphview.rs`, `health.rs`, and `commands/` (one module per top-level subcommand group: `run.rs`, `task.rs`, `session.rs`, `verify.rs`, `review.rs` — the largest, ~45 leaf commands — `queue.rs`, `project.rs`, `machine.rs`, `agent.rs`, `show.rs`, `misc.rs` for everything else). Follows `daemon/src/lib.rs`'s existing hand-rolled `Command` enum + `parse_args` convention (no `clap` anywhere in this workspace) rather than introducing a new dependency.
- **Known, disclosed gap**: `ralphus author` (the agentic TOML-authoring loop) was deliberately not ported and has no command in `cli-rs` at all. Its Python implementation (`cli/src/ralphus/author/`) has since been deleted outright too — `ralphus author` does not exist in either language. Every other subcommand, including `quick-start` (manager/reviewer × claude-code/codex launch, system-prompt composition, `--read-only` mode, `--append-system-prompt-file` merging) and `show help-map`, is a full 1:1 functional port.
- `cli-rs/src/commands/quick_start.rs` does **not** use tmux/psmux — despite the "interactive" framing, it is a plain foreground `std::process::Command::status()` spawn inheriting this process's own stdio; the spawned `claude`/`codex` process becomes the user's terminal session directly. That mechanism is unrelated to the tmux/psmux machinery `daemon/src/runner.rs`/`daemon/src/tmux.rs` use for tracking a *scheduled task's* session.
- `runner/src/shellcmd.rs` is reused by `cli-rs` (a normal crate dependency) rather than duplicated, since both need "detect the parent shell and quote a compound command for it" for the same reason (`quick-start --command` / the CLI-agent backends' `RALPHUS_CLAUDE_COMMAND`/`RALPHUS_CODEX_COMMAND` handling).
- `core/src/agent_resume.rs` holds the one shared implementation (`is_codex_agent`/`resume_agent_command`/`resume_codex_agent_command`) of the command line that resumes a CLI-agent conversation, imported by both `daemon/src/server.rs` and `cli-rs`'s session/review-terminal commands.
- Test coverage: unit tests throughout both crates (~307 total: 253 in `ralphus-cli`, 54 in `ralphus-runner`) plus `cli-rs/tests/cli_integration.rs` (spawns the real compiled `ralphus` binary against a throwaway local fake-daemon server and asserts on stdout/exit code). This is *not* a line-for-line port of the original Python suite's test lines — the counts don't match 1:1 per module, though every module that still exists on the Python side (none do, for the CLI/runner) has been brought to real functional parity, not just line-count parity.

Data flow: `ralphus submit x.toml` → daemon validates + ingests into SQLite (state **Pending**) → scheduler claims it (up to `max_concurrent`), spawns a worker thread → the worker runs each session by invoking `ralphus-runner` (JSON `SessionSpec` on stdin → `SessionResult` on stdout) → command verifies run → task/run states finalized → librarian polls `/api/tasks` and renders it.

Key module map:
- `core/src/schema.rs` — `TaskFile`/`TaskDef`/`SessionDef`/`VerifyStep`, `ResolvedAgent` inheritance. Session `prompt` XOR `command`.
- `core/src/validate.rs` — raw-`toml::Value` validator: unknown keys, required fields, types, verify one-of, `restart_on` grammar, within-task dep cycles, 1-based line numbers.
- `daemon/src/store.rs` — `Store` (the only place SQL lives), `RunState`/`NodeState`, board views.
- `daemon/src/server.rs` — `route()` (pure, unit-testable) + `serve()` (tiny_http; starts the scheduler thread).
- `daemon/src/scheduler.rs` — claims Pending runs, worker threads run sessions + verifies. Subprocess waits happen OUTSIDE the store lock.
- `daemon/src/runner.rs` — `Runner` trait + `SubprocessRunner` (spawns `RALPHUS_RUNNER_CMD`).
- `daemon/src/verify.rs` — `command` verify execution.
- `daemon/src/plan.rs` — dependency graph (Kahn topological sort).
- `daemon/src/guardian.rs` — Guardian store + state machine (Collecting→Approved→Deployed).
- `daemon/src/guardian_merge.rs` — stacked linear rebase in a worktree, agent conflict resolution.
- `daemon/src/reviews.rs` — review derivation (per-guardian, per-branch merge status).
- `daemon/src/config.rs` — layered config (global + per-project `.ralphus.toml`).
- `daemon/src/cartographer.rs` — Cartographer: the unified, structured, cross-system event log (`Note`/`CartographerEntry` builders, filtered/paginated query incl. `task`/`entity` filters, retention pruning). Rows can carry a `log_path` (RAL-155) pointing at an on-disk log file — e.g. a terminal-log attempt — instead of embedding its content. See "Logging Policy (RAL-79, RAL-98)" below.
- `daemon/src/logging.rs` — the `rlog!` file/stderr sink (RAL-83) that every Cartographer [`Note::emit`] call also writes through.
- `daemon/src/terminal_log.rs` — durable, per-attempt tmux pane transcript capture (RAL-154): flat files under `state_dir()/terminal_logs/<session_name>/<NNNN>.log`, one per tmux attempt (initial run + each `SubprocessRunner::run_via_tmux` reattach), so a restarted/reattached session's full history stays individually readable after the pane dies or the daemon restarts — independent of `crate::tmux`'s single-slot `write_pane_snapshot`/`read_pane_snapshot`, but referenced (by path, via `attempt_path`) from a Cartographer row every time an attempt is (re)written — see `runner.rs::emit_terminal_log_note`. Config: `[terminal_logs]` in `.ralphus.toml` (`max_lines_per_attempt`, `retention_days`, `max_files` — see `docs/daemon-api.md`).
- `daemon/src/entity_uri.rs` — RAL-155: the single-string, index-based `EntityUri` grammar (`run:`/`task:`/`session:`/`verify:`/`guardian:`) that addresses any run/task/session/verify/guardian uniformly; parsed server-side from `GET /api/cartographer`'s `entity=` query param and mirrored in the CLI (`cli-rs/src/entity_uri.rs`).
- `daemon/src/timeline.rs` — RAL-155: `build_run_timeline` merges a whole run's Cartographer rows (state transitions + everything else, already unified — see `cartographer.rs` above) with inlined terminal-log excerpts into one chronological narrative, sorted `(at_ms, id)` ascending. Backs `GET /api/runs/{id}/timeline` and the board's "⏱ Timeline" button.
- `auth/src/lib.rs` — Ed25519 license check (`check_license()`; compiles away without `secure-dist`).
- `keygen/src/main.rs` — keypair generation + license signing CLI.
- `cli/src/ralphus/docsgen/` — `shots.py` (Playwright scenario driver), `stub_server.py` (deterministic `/api/*` JSON stub), `librarian_server.py` (spawns the real, compiled `ralphus-librarian` binary against that stub), `fixtures.py` (canned response data), `lint.py` (screenshot-coverage check), `helpmap_docs.py` (regenerates `docs/cli-reference.md`'s help-map block from `ralphus show help-map`), `binaries.py` (locates the compiled `ralphus`/`ralphus-librarian` exes).
- `cli/src/ralphus/bench/` — renders the Rust bench harness's stored timing data as SVG/HTML graphs: `storage.py` (record read/write), `gitinfo.py`, `stats.py`, `graphs.py`. Does not run or time anything itself — see "RAL-94 benchmark harness" below.
- `core/src/bench_demo.rs` — reference example of the RAL-94 Rust `#[ralphus_bench]` opt-in pattern; see the "RAL-94 benchmark harness" section below.
- `librarian/assets/board.html` — the entire dark-theme UI (plain HTML + inline JS, embedded via `include_str!`).

## Build / Test / Lint

Every commit must pass all checks. **Rust is strict**: `[workspace.lints]` sets `warnings = "deny"`, `unsafe_code = "forbid"`, `clippy::all = "deny"`.

```bash
# Rust (from repo root)
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo test --all-targets            # run one: cargo test -p ralphus-daemon scheduler::

# Python (from cli/, uv-managed)
uv sync --dev
uv run ruff check .
uv run ruff format --check .
uv run mypy                         # strict; covers src + tests
uv run privata src                  # module-privacy linter for production code
uv run privata tests                # module-privacy linter for pytest support modules
uv run deadcode src tests           # unreferenced code linter; configured in cli/pyproject.toml
uv run pytest                       # run one: uv run pytest -k name

# Bench patience comment lint (from repo root, stdlib-only, no uv sync needed)
python scripts/check_bench_patience_comments.py

# Web (from repo root; lints/type-checks librarian/assets/board.html's inline JS)
npm install --no-audit --no-fund
npm run lint                        # eslint, incl. JSDoc-coverage rules (see "Web JSDoc type hints" below)
npm run typecheck                   # tsc --checkJs over the JSDoc-annotated JS; see tsconfig.board.json
npm run knip                        # dead code / unused deps; see knip.config.js
npm test                            # node --test over board.html's pure, DOM-free logic (RAL-186)
```

CI is `.github/workflows/ci.yml` (a Rust job, a Python job, and a web job).

## Web JSDoc type hints (board.html)

`librarian/assets/board.html` is plain HTML with its entire client as one
inline `<script>` — no build step, no TypeScript source files. Type safety
comes from JSDoc annotations checked by `tsc --checkJs` (TypeScript-the-tool,
used purely as a JSDoc type-checker — no `.ts` file is ever compiled or
shipped).

**Rule: any change to `librarian/assets/board.html`'s inline JavaScript —
a new function, an added/changed parameter, a new shared shape used across
functions — must ship with matching JSDoc type hints (`@param`/`@returns`,
and a `@typedef` for any new shared object shape). Before considering the
change done, run:**

```bash
npm install --no-audit --no-fund   # first time / after package.json changes
npm run lint                       # eslint, incl. jsdoc/require-* coverage rules
npm run typecheck                  # tsc --checkJs; errors point at board.html directly
```

**Both must pass clean.** This applies to human and AI-driven changes alike —
neither the Rust "does the HTML contain X" tests nor `cargo`/`uv` checks catch
a JS type regression here; the `web` job in `.github/workflows/ci.yml` is the
only thing that does, and it runs exactly these two commands (plus `npm run
knip` and `npm test` — see "Frontend — board.html" under Testing).

- Every top-level `function name(...) {}` declaration has a JSDoc comment
  with `@param`/`@returns` tags; `jsdoc/require-jsdoc` (in `eslint.config.mjs`)
  enforces this for new code. Small inline arrow-function callbacks (e.g.
  `.map((x) => ...)`) are intentionally exempt — annotating every one would be
  noise, not signal.
- Shared wire shapes (`RunView`, `TaskView`, `SessionView`, `GuardianView`,
  `QueueItem`, `CartographerRow`, ...) are declared once as `@typedef` blocks
  near the top of the `<script>` and referenced by name from function
  signatures — see `docs/daemon-api.md` for the shapes' authoritative source.
- `npm run typecheck` extracts the `<script>` body to `.lint-tmp/board.js`
  (via `scripts/extract-board-js.mjs`, preserving line numbers) and runs
  `tsc -p tsconfig.board.json` via `scripts/typecheck-board.mjs` —
  `tsconfig.board.json` sets `"strict": true`, so this is full TypeScript
  strict mode: `noImplicitAny` (a missing annotation is a hard error) *and*
  `strictNullChecks` (a `T|null`/`T|undefined` value must be narrowed —
  e.g. checked, defaulted with `??`/`||`, or captured into a local `const`
  right after a truthy check — before it's used as a plain `T`). Every
  `.lint-tmp/board.js` path in `tsc`'s diagnostic output is rewritten to
  `librarian/assets/board.html` (line/column numbers already line up 1:1, so
  this is a pure text substitution) — both locally and in CI, an error always
  points at the real file to edit, never the disposable extracted one.
- **Null-narrowing idioms used throughout board.html:**
  - `document.getElementById(id)` for an element that's always present in the
    page's static structure uses the `byId(id)` helper (defined near the top
    of the `<script>`, alongside `esc`/`cvar`), which asserts non-null via a
    `/** @type {HTMLElement} */` cast — same behavior as the original
    unguarded call (still throws if the id is ever missing), just satisfies
    the checker. For an element that's only conditionally rendered, keep the
    existing `const el = document.getElementById(id); if (el) ...` guard
    pattern instead.
  - A value narrowed by a truthy check on one line (`if (x.foo) ...`) does
    **not** stay narrowed inside a nested callback, nor for an *unrelated*
    variable that merely aliases it (e.g. `const y = x; y.foo` after checking
    `x.foo`), nor for a captured outer `let`/mutable-global after any
    intervening function call. Capture the narrowed value into its own local
    `const` right where you need it, rather than re-deriving it inside a
    closure.
  - `Array.prototype.filter(Boolean)` does not narrow `(T|undefined)[]` to
    `T[]` for TypeScript — follow it with `/** @type {T[]} */ (...)`.
- **Gotcha:** `eslint-plugin-jsdoc`'s comment parser does not reliably
  recognize an `@param`/`@returns` tag that shares a physical line with the
  description or with another tag — cram `/** Does X. @param {string} a
  @returns {void} */` onto one line and `jsdoc/require-param`/
  `require-returns` report *incorrect* "missing" errors for the tags after
  the first. Any JSDoc comment carrying a `@param`/`@returns` tag must be
  standard multi-line (one tag per line). `scripts/reformat-jsdoc.mjs` is a
  one-off (but rerunnable) tool that mechanically reformats every
  single-line `/** ... */` block in board.html that needs it — reach for it
  again if a bulk edit reintroduces single-line multi-tag comments. Bare
  `@type`/`@typedef`-only comments with no `@param`/`@returns` are fine to
  leave single-line.

## Knip (dead code / unused deps) for board.html

`knip.config.js` (repo root) registers a custom knip **compiler** for the
`html` extension that lets knip analyze `librarian/assets/board.html`
directly — no separate extracted file and no path-rewritten output, unlike
the ESLint/tsc setup above (`scripts/extract-board-js.mjs` +
`scripts/typecheck-board.mjs`); knip's diagnostics already point at the real
file. `board.html`, `knip.config.js` itself, and
`scripts/reformat-jsdoc.mjs` (a manually-invoked tool, never imported or run
from an npm script) are listed under `entry` so knip doesn't flag them as
unreachable. `project` is left at knip's default glob
(`**/*.{js,mjs,cjs,jsx,ts,tsx,mts,cts}`, gitignore-filtered), so `npm run
knip` also audits every other JS file in this Node package
(`eslint.config.mjs`, `scripts/*.mjs`), not just board.html.

board.html's script is a plain global script (not an ES module — no
`import`/`export`), so out of the box knip's cross-file reachability graph
has nothing to walk inside it — it could only tell whether board.html itself
is reachable, not whether any function *inside* it is dead. The compiler
(`compileHtml()` in `knip.config.js`) goes further, faking a module graph
over the single file so knip also catches **intra-file dead functions**:
every top-level `function`/`async function`/`const ... = (...) =>` gets
rewritten to carry `export` (`includeEntryExports: true` is required
alongside this, since entry-file exports are normally presumed to be public
API and skipped), and every other occurrence of `declaredName(` anywhere in
the raw document — including inside `onclick="..."` strings and multi-layer
indirection like `terminalMenuItem(key, label, \`foo(...)\`, tip)`, neither
of which any real parser (ESLint included — the same reason
`no-unused-vars` stays disabled in `eslint.config.mjs`) can see into — is
collected into a synthetic reference sink appended to the compiled output,
so those string-only-dispatched handlers don't false-positive as unused
(`ignoreExportsUsedInFile: true` is also required, since by default knip
only counts cross-file imports as "used", not same-file references — which
is exactly what the sink produces). A name with zero occurrences anywhere
else in the document is genuinely dead. See the comment block at the top of
`knip.config.js` for the full mechanics and known limitations (it's a
heuristic, not real reference tracking — safe to miss a dead function, e.g.
one dispatched via `window[name]()`, but should not false-positive on any
string-wiring pattern actually used in this codebase). Run it with
`npm run knip`.

## Testing

**Authoring rule — Ollama tests are opt-in, never on by default.** Any test
that calls out to a live Ollama model must be gated off from the normal
`cargo test` / `uv run pytest` run, the same way every existing Ollama test
already is: `#[ignore]` in Rust (see `daemon/tests/`), `pytest.mark.ollama`
in Python (see `cli/pyproject.toml`'s `addopts = "-p pytester -m 'not ollama'"`). Don't add
a new Ollama-backed test that runs unconditionally — CI and the normal dev
loop must never depend on a local model being up. Keep the same
runtime guard too (skip/return early with a clear message if Ollama isn't
reachable on `127.0.0.1:11434` or the required model isn't pulled), so even
an explicit opt-in run degrades gracefully without live infra.

### Backend — Rust integration tests

`cargo test --all-targets` runs everything. Key integration test files in `daemon/tests/`:

| File | Scope | Notes |
|---|---|---|
| `api_over_http.rs` | HTTP API contract | Two always-run tests; uses `Store::open_in_memory()` |
| `prompt_verify.rs` | Prompt-kind verify execution | Both tests are live-Ollama, `#[ignore]`d by default |
| `guardian_merge.rs` | Stacked rebase + conflict resolution | Mostly `CapturingRunner`-based (no subprocess); 1 live-Ollama test, `#[ignore]`d by default |
| `reviews_derive.rs` | Full review flow end-to-end | 1 live-Ollama test, `#[ignore]`d by default; `RALPHUS_RESOLVER_MODEL` (default `qwen3:8b`) |
| `monorepo.rs` | Monorepo pipeline | 3 always-run + 1 live-Ollama test, `#[ignore]`d by default |

**Live-Ollama tests are `#[ignore]`d by default** — a plain `cargo test`/`cargo test --all-targets` never runs them, so CI and the normal dev loop never depend on a local model. Run them explicitly with `--ignored`:
```bash
cargo test -p ralphus-daemon --test reviews_derive full_flow -- --ignored --nocapture
cargo test -p ralphus-daemon --test monorepo full_monorepo_flow -- --ignored --nocapture
cargo test -p ralphus-daemon --test prompt_verify -- --ignored --nocapture
cargo test -p ralphus-daemon --test guardian_merge generate_summary_live_ollama -- --ignored --nocapture
# or, to run every ignored (live-Ollama) test across the workspace at once:
cargo test --all-targets -- --ignored
```

Each still carries its own runtime guard too (prints `SKIP` and returns early) if Ollama isn't up on `127.0.0.1:11434` or the required model isn't pulled — so even an explicit `--ignored` run degrades gracefully without live infra.

### Backend — Python tests

The old Python CLI/runner (and its whole test suite) is gone — `cli/` now
holds only `docsgen/` (doc screenshot generation) and a trimmed `bench/`
(SVG/HTML graph rendering for the Rust bench harness's data; see "RAL-94
benchmark harness" below), neither of which needs live external services.
CLI/runner work belongs in `cli-rs/`/`runner/` and their own Rust test suites
(see "Testing" entries for `ralphus-cli`/`ralphus-runner` elsewhere in this
file), not here.

`cd cli && uv run pytest`. Key test files:

| File | Covers |
|---|---|
| `test_docsgen_helpmap.py` | `helpmap_docs.py`'s marker splice + drift-check logic, and a real (non-mocked) call into the compiled `ralphus show help-map` |
| `test_bench_storage.py` | Bench record read/write, filename hashing |
| `test_bench_graphs.py` | SVG/HTML rendering from stored records |
| `test_bench_gitinfo.py` | Git commit/dirty-state detection |
| `test_bench_stats.py` | Stats-bundle computation (mean/median/stddev/IQR/outliers) |

### Frontend — board.html

The board is plain HTML + vanilla JS embedded via `include_str!`, so most of it
is only testable in a browser. The exception (RAL-186) is its **pure, DOM-free
logic**, which `npm test` (`node --test`, files in `test/`) exercises directly:

```bash
npm test                            # from repo root; also a CI step in the `web` job
```

`test/board-peek-state.mjs` slices the live-view (peek) state machine straight
out of `board.html` — the region between the `// RALPHUS-PEEK-STATE-MACHINE:BEGIN`
/ `:END` markers — and evaluates it standalone. **The tests read the real
shipped source; there is no second copy to drift from.** If that logic moves,
move the markers with it, or the loader fails loudly. To extend this pattern to
another piece of board logic, factor the decision-making part free of DOM/`fetch`/
module-level state (the way `nextPeekPaneState` is: state in, state out) and give
it its own marker region — that separation is what makes it testable at all.

Everything else about the board is still tested manually:

1. Run `bash scripts/build-debug.sh` (boots daemon + librarian).
2. Open `http://127.0.0.1:7474` in a browser.
3. Submit a task and watch the board update (the daemon pushes changes over SSE;
   open Live View boxes refresh on their own 2 s interval).
4. Exercise the tab you changed (Runs, Reviews, etc.).

Frontend dev loop: edit `librarian/assets/board.html` → re-run `bash scripts/build-debug.sh` → refresh browser. One incremental librarian recompile is the cost.

### Common manual-testing commands

```bash
# Validate a task file (offline, no daemon needed)
ralphus validate task.toml
# or
cargo run -p ralphus-daemon -- validate task.toml

# Submit a task
ralphus submit task.toml
ralphus submit task.toml --hold          # stages as Queued, not Pending

# Check system health (daemon reachable, git on PATH, runner available)
ralphus check health

# Query the daemon API directly
curl http://127.0.0.1:7890/api/daemon
curl http://127.0.0.1:7890/api/tasks
curl http://127.0.0.1:7890/api/runs/<id>

# Run a specific Rust unit-test module
cargo test -p ralphus-daemon scheduler::
cargo test -p ralphus-core validate::

# Run a single Python test by name
cd cli && uv run pytest -k test_render_test_svg_includes_commit_labels -s

# Build and run the keygen tool (author-only; not distributed)
cargo run -p ralphus-keygen -- generate
cargo run -p ralphus-keygen -- sign --key ralphus-private.key --name "Name" --expiry 2027-01-01

# Secure-dist build (daemon + librarian refuse to start without ralphus.lic)
cargo build --release --features ralphus-daemon/secure-dist,ralphus-librarian/secure-dist
```

## RAL-94 benchmark harness

A custom, **opt-in** benchmark harness durably times every `#[ralphus_bench]`-
tagged Rust test, one commit at a time, so per-test timing regressions are
visible per commit instead of only "the whole suite got slow" after the fact.
It is never run as part of normal `cargo test` — it is a separate, explicit
invocation, split across two independent steps:

```bash
# Generate data: benchmarks every #[ralphus_bench]-tagged test, writes JSON
cargo run -p ralphus-bench-harness --bin ralphus-bench-rs

# Regenerate the SVG + HTML graphs from already-stored data (no test execution)
cd cli && uv run ralphus-bench-graph --lang rust
```

`ralphus-bench-rs` only *runs tests and writes JSON* to `bench_data/rust/` —
it never touches the SVGs/HTML. `ralphus-bench-graph` (a small standalone
Python tool, `cli/src/ralphus/bench/` — kept specifically for this, stdlib-
only) only *reads* already-stored JSON and (re)renders graphs — it never
executes a test. After running the data-generation command you must re-run
`ralphus-bench-graph` to see updated graphs — it is not triggered
automatically. See [`docs/bench-harness.md`](docs/bench-harness.md) for the
full developer walkthrough.

Implements the "durable minimum" stopping rule: run the test once, in-process;
if it beat the best duration seen so far, remember it and reset `patience`;
otherwise decrement `patience`; stop at `patience == 0`. Every raw sample is
kept and reduced to a full stats bundle (`max`, `mean`, `median`, `stddev`,
`iqr`, Tukey `outliers`) alongside `durable_min`. Results accumulate one
record per commit per test under `bench_data/rust/` — see
`bench-harness/src/storage.rs`. Test-name-derived filename stems are hashed
(not embedded verbatim) once they run long, since Windows' `MAX_PATH` is 260
chars and a test's full path can blow past a safe budget on its own.

A skipped test is recorded explicitly — a `skipped` entry alongside `records`
in that test's JSON file — rather than simply being absent; graph rendering
only ever reads `.records`, so a skip is surfaced as a count and never
plotted as a trend-line point. There's no runtime equivalent for this today:
a crate's hand-maintained `ralphus_bench_tests()` collector simply omits a
test it doesn't want benchmarked, which is already an intentional per-test
opt-in decision rather than a runtime skip.

`cargo test`'s implicit lib-unittest target (the one that runs
`#[cfg(test)] mod tests` inline in `src/`) is the thing that executes the
vast majority of this repo's Rust tests, and Cargo gives no way to swap out
or disable that target's harness — `harness = false` only applies to a
`[[test]]` entry pointing at a file under `tests/`, not to the implicit
inline-unittest target. There is therefore no way for an external
`ralphus-bench-rs` binary to reach into an ordinary `#[test]` function and
re-invoke it in-process, repeatedly, without either linker-level magic
(rejected: this workspace forbids `unsafe_code`) or moving every test out of
`#[cfg(test)] mod tests` (rejected: too large and risky a mechanical
migration to do blind). Instead: a crate opts a test in with
`#[ralphus_bench(patience = N)]` (see `ralphus-bench-macros`), which expands
to the original function, a separately-named `#[test]`-tagged wrapper that
calls it once (so it keeps running under plain `cargo test` too), and a
`const BenchMeta` describing it. The crate then hand-maintains a
`pub fn ralphus_bench_tests() -> Vec<BenchMeta>` collector (see
`core/src/bench_demo.rs` for the reference example) that `ralphus-bench-rs`
calls into. Adding a crate's tests to the harness is therefore a deliberate,
visible, per-test action.

**Authoring rule — patience.** Every time you add a benchmarked test
(`#[ralphus_bench(...)]`), deliberately consider whether the default patience
(10) is appropriate for that test. If you explicitly set a patience value —
even if it happens to equal the default — the line must carry a trailing
inline comment explaining why, e.g.:

```rust
#[ralphus_bench(patience = 3)] // low: validating an empty file is trivial and deterministic — few improving runs needed to find its floor
fn validate_toml_rejects_empty_file() { ... }
```

Tests using the implicit default (no `#[ralphus_bench]` argument at all) need
no comment.

## OpenTelemetry tracing (RAL-96)

A user action (a `board.html` button click) → librarian → daemon → scheduler
→ `ralphus-runner` subprocess is traced end-to-end as one OpenTelemetry
trace, viewable as a flame/waterfall graph. Entirely opt-in — every exporter
is a no-op unless `OTEL_EXPORTER_OTLP_ENDPOINT` is set, so the default dev
loop is unaffected. Rust spans (every hop above, including the runner) use
the `opentelemetry`/`opentelemetry_sdk` crates' manual span API directly (not
the `tracing` crate — see the Logging Policy below); the browser hand-rolls
the W3C `traceparent` format (no `opentelemetry-js`, since `board.html` has
no build step). A local Collector + Jaeger stack for viewing traces lives in
`otel/docker-compose.yml`. See [`docs/otel-tracing.md`](docs/otel-tracing.md)
for the full design and how to run it.

## Running it

Two build scripts, two purposes (both in `scripts/`):

| Script | Speed | Output | Use it to |
|---|---|---|---|
| `scripts/build-debug.sh` | seconds (incremental) | runs from source, no `dist/` | iterate — esp. the GUI |
| `scripts/build-release.sh` | minutes | four standalone exes in `dist/` | package / distribute |

**Fast dev loop — `bash scripts/build-debug.sh`.** Debug-builds all four binaries (daemon, librarian, runner, CLI) with `cargo` (incremental, ~seconds each) and points `RALPHUS_RUNNER_CMD` at the just-built debug `ralphus-runner` exe. It boots the daemon (`127.0.0.1:7890`) in the background and the librarian board (`127.0.0.1:7474`) in the foreground; **Ctrl-C stops both**. Invoke the CLI yourself from another shell, e.g. `target/debug/ralphus status`.

**Release build — `bash scripts/build-release.sh`.** Builds copyable standalone binaries into `dist/`: `ralphus-daemon`, `ralphus-librarian`, `ralphus` (CLI), `ralphus-runner` — all four are a single `cargo build --release` now, no PyInstaller step. Stop any running daemon/librarian/CLI/runner first: they lock their own `dist/` exes and the copy step will fail with "Device or resource busy".

**Testing ralphus using ralphus (multi-instance dev stacks, RAL-164).** `ralphus-daemon serve` accepts `--db <path>` alongside `--port`, and `build-debug.sh`/`.cmd` accept a matching `--db-path`. To keep your regular ralphus instance open while exercising a change in another git worktree, give that worktree's stack its own port *and* its own DB explicitly:

```bash
# worktree A (your regular instance) — unchanged, defaults
bash scripts/build-debug.sh

# worktree B — fully isolated second stack
bash scripts/build-debug.sh --daemon-port 7891 --librarian-port 7475 --db-path ~/.ralphus/tasks-worktree-b.db
```

This is deliberately explicit, not auto-picked: if a script silently chose a port or DB path on `start`, a later `ralphus-daemon stop --port N` (a new shell, a different agent) would have no reliable way to know what to target. Passing a non-default `--daemon-port` without `--db-path` still gets automatic DB isolation (derived as `~/.ralphus/tasks-<port>.db`) — only the *default* port keeps using the plain `~/.ralphus/tasks.db` it always has, so existing setups are unaffected. Point the CLI or a browser at the second instance with `ralphus --daemon-url http://127.0.0.1:7891 ...` / `http://127.0.0.1:7475`, and stop it with `ralphus-daemon stop --port 7891` when done.

Run the pieces directly:

```bash
ralphus-daemon serve                          # HTTP API on 127.0.0.1:7890 (+ scheduler)
ralphus-librarian serve [--port 7474]         # web board; RALPHUS_DAEMON_URL points it at the daemon
ralphus submit task.toml                      # or: validate / status / review / ...
```

`scripts/build-release.cmd` (Windows) builds all four standalone executables into `.\dist` via a single `cargo build --release -p ralphus-daemon -p ralphus-librarian -p ralphus-cli -p ralphus-runner`.

**`dist/` layout — four standalone exes, no sibling directories:**

```
dist/
  ralphus-daemon.exe
  ralphus-librarian.exe
  ralphus.exe
  ralphus-runner.exe
```

Put `dist/` on PATH to get the `ralphus` CLI and have `RALPHUS_RUNNER_CMD` resolve `ralphus-runner` automatically, or point `RALPHUS_RUNNER_CMD` at the full path to `dist/ralphus-runner.exe` explicitly.

**History (pre rust-port):** the CLI and runner used to be PyInstaller `--onedir` bundles (`dist/ralphus/ralphus.exe` + `_internal/`, `dist/ralphus-runner/ralphus-runner.exe` + `_internal/`), which themselves replaced an earlier `--onefile` build after its bootloader's cache validation failed with `Security validation failure: parent process has different executable!` on two machines launched from a sandboxed/reparenting tool layer (see `PERMISSIONS_ISSUE.local.md`). None of that applies anymore — see "The Rust CLI/runner port" above.

## Cryptography / Secure Distribution

See `docs/secure-dist.md` for the full workflow. Summary:

**What it is:** An opt-in build mode (`--features secure-dist`) where `ralphus-daemon` and `ralphus-librarian` refuse to start without a signed `ralphus.lic` file. Standard open builds are completely unaffected — the check compiles away to nothing without the feature flag.

**Crates:**
- `auth/` — `ralphus-auth` lib; exports `check_license()`. Uses **ed25519-dalek** for Ed25519 signature verification, **base64** for signature encoding. Public key is embedded at compile time via `include_bytes!("../public.key")`.
- `keygen/` — `ralphus-keygen` bin (author-only, never distributed). Uses **rand_core::OsRng** for entropy. Subcommands: `generate` (keypair) and `sign` (license file).

**License file format** (`ralphus.lic`, JSON):
```json
{ "holder": "Alice", "seat": "alice@BOX", "expiry": "2027-01-01", "signature": "<base64-ed25519>" }
```
Message signed: `"RALPHUS|<holder>|<seat>|<expiry>"`, where an absent `seat` is the literal `any` and an absent `expiry` is the literal `never`. **`keygen/src/main.rs` builds this string independently of `auth::signing_message` — the two must stay byte-identical, and each has a test pinning the format.** Expiry is compared as a string (`YYYY-MM-DD` lexicographic order). `seat` is a `user@hostname` binding (see the **seat** entry in `docs/glossary.md` — deliberately not called a *machine*, which RAL-185 already took); it is compared case-insensitively against the local `USERNAME`/`USER`/`LOGNAME` plus the `gethostname` crate's hostname. Order of checks is signature → expiry → seat, so a forged file always reports "not authorized" rather than leaking which field was wrong.

**Key files:**
- `auth/public.key` — 32-byte raw Ed25519 public key; baked into the binary at compile time; **committed**.
- `ralphus-private.key` — hex-encoded seed (64 chars); **gitignored**; never distributed; back it up.

**Full workflow:**
```bash
# 1. Generate a keypair (once; overwrites auth/public.key)
cargo run -p ralphus-keygen -- generate

# 2. Rebuild with the new public key baked in
cargo build --release --features ralphus-daemon/secure-dist,ralphus-librarian/secure-dist

# 3. Sign a license for someone
cargo run -p ralphus-keygen -- sign \
  --key ralphus-private.key \
  --name "Alice" \
  --seat alice@HER-BOX \  # or --this-seat; omit both to run anywhere
  --expiry 2027-01-01     # omit for non-expiring license

# 4. Recipient drops ralphus.lic next to the executables
#    (or sets RALPHUS_LICENSE=<path>)
```

Re-keying: delete `ralphus-private.key`, run `generate` again, commit the new `auth/public.key`, rebuild, re-sign all existing recipients — old license files will no longer verify.

## Gotchas learned the hard way

- **Session `cwd` must be a real path for the OS the daemon runs on.** On Windows, an MSYS/Git-Bash `/tmp/...` path will not resolve in native-Windows Python — use a Windows path. `cwd` is mandatory and validated.
- **Runner command**: the daemon spawns `RALPHUS_RUNNER_CMD` (default `ralphus-runner`). In dev, `scripts/build-debug.sh`/`.cmd` build it with `cargo` and point `RALPHUS_RUNNER_CMD` at the fresh `target/debug/ralphus-runner` exe — a normal incremental Rust rebuild, no venv, no editable install. `dist/ralphus-runner.exe` (built by `scripts/build-release.cmd`) is a standalone release exe; only rebuild it when something changed since the last one.
- **The full review flow has a live-Ollama integration test** — `daemon/tests/reviews_derive.rs::full_flow_validate_submit_run_and_ollama_resolves_conflict`. It **skips** unless Ollama is up on `127.0.0.1:11434`, the resolver model (`RALPHUS_RESOLVER_MODEL`, default `qwen3:8b`) is pulled, and a `ralphus-runner` is found.
- **Monorepo integration test** — `daemon/tests/monorepo.rs` has three always-run pipeline tests and one live-Ollama test, `#[ignore]`d by default. Run the live test with `cargo test -p ralphus-daemon --test monorepo full_monorepo_flow -- --ignored --nocapture`.
- **Model selection** is per-session in TOML: `agent = "ollama"` + `model = "qwen3:8b"` for local; `agent = "claude"` (default) uses Anthropic (needs `ANTHROPIC_API_KEY`).
- **`prompt`-kind verify steps** run for real, reusing the owning session's resolved backend through the same `Runner`/`ralphus-runner` path. The runner wraps the verify prompt and parses the final `RALPHUS_VERIFY: PASS`/`FAIL` line. Small local models don't put the marker alone on its own line — parsing searches the whole output and trusts the last occurrence. No verdict found = FAIL (fail closed).
- **Port clash**: the old claudectl also uses 7474/7890; the daemon port is not yet CLI-configurable, so don't run both.
- **Submitting goes to `Pending` (schedulable now)**, not `Queued`. `hold=true` stages as `Queued`; `/activate` promotes it. (The predecessor's silently-`Queued`-forever bug — FINDINGS §2.4.)
- **`keygen` is never shipped.** It is not in the release build scripts and should not be added. It is a workspace member only so `cargo build --all` can catch compile errors in CI.
- **A tmux-wrapped session's pane can vanish mid-run with no clean explanation** on the Windows tmux-alternative this project targets (`psmux`) — see the gitignored `PSMUX_CRASH_NOTES.local.md` if present for background. `SubprocessRunner::run_via_tmux` (`daemon/src/runner.rs`) mitigates this: once a `claude-code`/`codex` session's `agent_session_id` has been captured live (from the `stream-json` init event or Codex's `thread.started` event, forwarded over the `RALPHUS_EVENT:` marker), a pane that goes unreachable for `MISSING_SESSION_STRIKE_LIMIT` consecutive polls triggers a bounded auto-reattach (`claude -p --resume <id>` or `codex exec resume <id>`, up to `SubprocessRunner::MAX_REATTACH_ATTEMPTS` times) in a fresh tmux session under the *same* deterministic name — so the board's "Show Live View"/"Open Terminal Log" buttons transparently start working again with no UI-side change. The overall `timeout_sec` budget is shared across every attempt, never reset by a reattach. Every stage (session lost / reattach attempt / giving up) is logged via both `rlog!` and a `tmux-reattach`-scoped Cartographer note carrying attempt/elapsed/reason/last-error, so a real occurrence is fully diagnosable from Cartographer alone. Only exercised for `agent = "claude-code"`/`"claude-cli"`/`"codex"`/`"codex-cli"`; other agents still fail outright on a lost pane, exactly as before.

## Built (Phases 0–5)

Task pipeline (submit → schedule → run via native agent *or* harness backend → command/prompt verify → board); dependency-graph scheduling + `{handoff:...}`; cross-run gating; Guardian reviews (stacked **rebase** merge in a worktree — each branch rebased onto the prior against one snapshotted base commit, agent conflict resolution, check gates, auto-rebuild when the base branch shifts, feedback chat, Reviews UI); `ralphus` CLI (validate/submit/status/check health/review/...); harness backend (external agents like `claude-code`, `codex`); standalone release builds; secure-distribution licensing (`ralphus-auth` + `ralphus-keygen`). `ralphus author` (agentic TOML generation) was built during this phase but has since been retired entirely — see "The Rust CLI/runner port" above. See `PLAN.local.md` for per-item detail.

## What is NOT built yet (see PLAN.local.md)

`brain`/`approval` verify kinds + verify retry policy; a verify step's `arguments`/`budget_usd` (parsed but not enforced); per-session verify results in the board API (`SessionView` has no `verify` field — only task-level verify is exposed today); Guardian review cycles (multi-round approve/iterate beyond the single auto-rebuild); detached daemon lifecycle (`ralphus daemon start/stop/status` with PID file); prism ("Open in Prism") desktop handoff; multi-user hardening (auth, per-user attribution, worker pool); editable detail pane; full URL-state routing; the draggable node-graph canvas + Logs modal in the UI.

## UI Tooltip Rule (RAL-40)

Every new UI element in `librarian/assets/board.html` **must ship with a tooltip** using the `data-tip="..."` attribute. The tooltip engine is a lightweight JS+CSS system already wired into the page (see the `// ---- Tooltip engine (RAL-40) ----` block in the `<script>` tag and the `#board-tip` CSS rule). See also the "Web JSDoc type hints (board.html)" section above — the other mandatory checklist item (`npm run lint && npm run typecheck`) for any `board.html` change.

**Implementation pattern:** add `data-tip="..."` to any HTML element — static or dynamically generated in a JS template string. The engine uses event delegation on `mouseover`/`mouseout` and renders a fixed-position dark-themed popover near the cursor. Multi-line content: use `\n` in the attribute value.

**Required tooltip content:**
1. **Why** — the purpose of the element (what it does and why it matters).
2. **Who / when** — the scenario in which someone would use it.
3. **Caveats / warnings** — irreversible or destructive actions **must** include the phrase "This cannot be undone." even if a `confirm()` dialog also exists.

**Example:**
```html
<!-- Static HTML -->
<button data-tip="Delete this run and all its data permanently.\nThis cannot be undone." ...>🗑 Delete</button>

<!-- JS template string -->
items.push(`<div data-tip="Cancel this run — stops all running sessions." onclick="...">■ Cancel</div>`);
```

**Do not use the native `title` attribute** for new tooltips — it renders with browser default styling and ignores the dark theme. The `title` attribute can remain on existing splitter elements (they already use `data-tip`) but should not be added to new elements.

## Read-Only Quick-Start Safety List (RAL-194)

Each `HelpNode` in `cli-rs/src/help_map.rs`'s command tree (`ROOT` and its children) carries a `read_only_safe: bool` field — the allowlist a `--read-only` quick-start session (manager/reviewer) is told it may call. It drives the `(read-only-safe)` tag shown in the injected help-map (see `READ_ONLY_NOTE` in that same file) and the model is instructed to only invoke tagged commands while mutating ones stay off-limits.

**Whenever you add a new `ralphus` CLI subcommand, decide whether it is safe to run under `--read-only`, and if so, set its `HelpNode`'s `read_only_safe` to `true`.** A command belongs on the list only if it performs no mutation under *any* of its own flags — it queries the daemon or local files and prints, never writes (a command that only prints an action for a human to run themselves, like `review checks run`, still counts as non-mutating). Everything else — including any new `set-status`/`restart`/`edit`/`create`/`delete`/`cancel`/`merge`/`approve`/`feedback`/`register`/`remove`/`git`, `submit`, or `clear`-shaped command — must be left `false` (unsafe-by-default). Don't default to leaving a new command off the list out of habit; make the call explicitly, the same way the Bench Patience rule below asks you to deliberately decide on `patience` for every new benchmarked test.

## Vocabulary

**ralphus has taken words.** `run`, `task`, `session`, `verify`, `review`,
`guardian`, `agent`, `backend`, `provider`, `machine`, `channel`, `ghost`,
`project`, `worktree` and others all mean something specific here, and several
of them nest in a way that matters (a run contains tasks, which contain
sessions, which contain verify steps).

**Before coining a term — for a concept, a struct, a field, a doc — check
[`docs/glossary.md`](docs/glossary.md).** Reusing a taken word makes both
meanings harder to read, and a collision is painful to undo once it has reached
the schema, the store, and the board. That document also lists the words already
carrying too much weight to take, with the alternative to use instead.

**When you take a new word, add it to the glossary.**

## Design / UI colors

**Any time you choose a color for the web board (`librarian/assets/board.html`) or
any UI, consult [`docs/colors.md`](docs/colors.md) and use the documented CSS
variable or rule.** That document is the single source of truth for what each color
means (status colors, `--accent` for selection, `--teal` for dependency-driven
"pulled along" movement, `--ignored` amber reserved for caution, etc.).

Rules:
- Never hardcode a hex value or invent an ad-hoc color. Always use `var(--name)`
  (or an `rgba()` tint of a documented hue).
- If no existing semantic role fits, add a new variable in `board.html` (and its
  light-theme override if needed), document it in `docs/colors.md`, *then* use it.
- Every new UI element also needs a `data-tip` tooltip — see the "UI Tooltip Rule
  (RAL-40)" section in `CLAUDE.md`.

## Logging Policy (RAL-79, amended by Cartographer / RAL-98)

There are now two layers, and both fire together — this reconciles the original
stderr-only mandate below with Cartographer, the DB/file-backed structured
event log:

1. **Cartographer** (`daemon/src/cartographer.rs`) is the primary, queryable
   record: every notable event — task/session lifecycle, verify starts/results,
   status transitions, Guardian review lifecycle — is written as one row with a
   timestamp, human-readable message, source location, entity references
   (run/session/guardian/task), and an arbitrary JSON payload, into the
   `cartographer_events` SQLite table. Query it via `GET /api/cartographer`
   (filterable, paginated, sortable) or the board's Cartographer tab. Retention
   is capped by `[cartographer]` in `.ralphus.toml` (`retention_days`,
   `max_rows`; defaults 30 / 50,000) — either cap triggers pruning.
2. **The plain-text sink** (stderr, or a file when `[daemon].log_path` is set)
   is still written for every Cartographer record, so `tail`-based debugging
   keeps working exactly as before. `crate::cartographer::Note::emit(&store,
   message, payload)` writes both in one call: the human-readable
   `ralphus [source] message` line via the same sink `rlog!` uses, plus the
   structured row. Call sites that already hold a `Store`/lock use
   `store.cartographer_log(CartographerEntry { .. })` directly alongside their
   existing `rlog!` call instead.

**Cartographer formally replaces `rlog!` as the primary logging mechanism.**
`rlog!` itself is unchanged (see below) and many call sites now emit both — a
full mechanical conversion of every remaining `rlog!` site is tracked as
ongoing/follow-up work rather than a hard gate on new code; new code should
prefer emitting a Cartographer record over introducing another `rlog!`-only
call site, especially for anything a user would want to query later (state
transitions, LLM calls, manual actions).

**Runner subprocess → daemon channel:** the Python runner cannot write
Cartographer rows directly (it has no DB access and stdout is reserved — see
below), so it emits a JSON line to stderr prefixed with `RALPHUS_EVENT: ` (via
`ralphus.runner.cartographer.emit(...)`), mirroring the existing
`RALPHUS_VERIFY: PASS/FAIL` marker-parsing pattern. `daemon/src/runner.rs`
reads the child's stderr line-by-line (not just at exit) and forwards any
matching line into Cartographer, enriching `run_id`/`session_id`/`task` from
the owning `RunnerSpec` when the event omits them.

All log output still goes to **stderr** only (`eprintln!` in Rust; `print(..., file=sys.stderr)` in Python) for the plain-text sink. The stdout channel carries structured JSON between the daemon and runner subprocess — do not pollute it with log lines (Cartographer's runner-side events use the stderr marker above, not stdout).

**The stdout rule is mechanically enforced.** `[workspace.lints.clippy]` in the
root `Cargo.toml` sets `print_stdout = "deny"` — note this must be named
explicitly, because `print_stdout` lives in clippy's `restriction` group and is
*not* covered by the `all = "deny"` beside it. A stray `println!` does not
crash anything; it intermittently corrupts a `SessionSpec`/`SessionResult` JSON
parse only when something prints mid-session, which is exactly the kind of bug
that costs a day. The legitimate stdout writers — CLI output that *is* the
program's product (`daemon/src/main.rs`, `librarian/src/main.rs`,
`validate_file`) and test `SKIP:` notices — each carry a local
`#[allow(clippy::print_stdout)]` with a reason. `print_stderr` is deliberately
**not** enabled.

**No async runtime.** Do not add `tokio`, `reqwest`, `hyper`, or a dependency
that pulls one in. The workspace is synchronous by design (`ureq`,
`tiny_http`, a thread-per-worker scheduler) and the lock file is deliberately
small (~197 crates); the root `Cargo.toml`'s profile note is explicit that
build time is a first-class constraint. This — not log-channel safety — is the
real reason `opentelemetry-otlp` was rejected in favor of the hand-rolled
`ureq` exporter in `daemon/src/otel.rs`.

**On the `tracing` crate (superseded).** Earlier revisions of this policy said
"Never use the `tracing` crate," citing RAL-79. That prohibition has been
retired, because RAL-79 does not actually support it: its text reads *"Do NOT
introduce the `tracing` crate right now … Tracing is a future concern"* —
time-bounded scope control on a ticket about adding `eprintln!` coverage, which
also asked for output "on stderr/stdout" and so did not treat stdout as
reserved at all. `tracing` is moreover a *facade*: it emits nothing unless a
subscriber is registered in our own binary, so a transitive `tracing` is inert
(`log` 0.4.x already sits in our tree on exactly those terms). The one real
hazard was always `tracing_subscriber::fmt()`, whose default writer is
**stdout** — installing a fmt subscriber without `.with_writer(io::stderr)`
would write log lines straight into the runner JSON channel. That specific
hazard is now covered by the two rules above. If you do introduce a subscriber,
it must write to stderr. Judge any candidate dependency on async-runtime weight,
not on whether `tracing` appears in its tree.

**Log format:** `ralphus [TYPE] message key=value …`

| TYPE | Where emitted | What to log |
|---|---|---|
| `state` | `daemon/src/store.rs` | Every entity state transition: `{entity} {id} {old_state} → {new_state}` (and `output_len` for verify results) |
| `submit` | `daemon/src/store.rs` | `insert_run`: run inserted with `state=`, `tasks=` count |
| `recovery` | `daemon/src/store.rs` | Orphaned runs recovered on daemon startup |
| `http` | `daemon/src/server.rs` | Every HTTP request: `{METHOD} {path} → {status}` |
| `scheduler` | `daemon/src/scheduler.rs` | Run claimed, run executing (session+task counts), session start (agent/model), session completed (status/tokens), verify starting (kind/agent/model) |
| `runner` | `daemon/src/runner.rs` | Subprocess spawned (pid/run/session/agent/model/timeout), cancelled, timed out, result parsed (status/tokens/cost) |
| `spec` | `daemon/src/runner.rs` | System-prompt synthesis: which case applied (user-supplied / addendum / combined), lengths |
| `verify` | `daemon/src/verify.rs` | Command verify starting (cwd/command) and completed (passed) |
| `llm` | `runner/src/execute.rs` | Session/verify start (run/session/agent/model/prompt_len/prompt_hash), system-prompt applied (len/position), done (tokens/cost) or error — at the execute layer |
| `llm-invoke` | `runner/src/agent_backend.rs` | The actual model API call (via `llm_client::run_agent`) start (agent/model/prompt_len/hash), done (elapsed/tokens), or error — at the model API call layer |
| `runner` | `runner/src/main.rs` | Runner invoked (run/session/agent/model/verify) |
| `cli` | `cli-rs/src/main.rs` | CLI subcommand invoked with its parsed `Command` |

**Required events** — any new code path that touches these must emit the corresponding log line, and — per the Cartographer amendment above — a matching structured record wherever a `Store` is reachable:
- All entity state transitions (run, task, session, verify)
- Every LLM call: before start and after completion (with outcome and duration)
- Every manual user action: CLI subcommand + args, HTTP endpoint hit
- System-prompt synthesis or borrow from task/session data
- Key scheduler lifecycle: session claimed, session completed, verify started
- Subprocess spawn/cancel/timeout

## Bench Patience Comment Rule (RAL-94 / RAL-95)

`ralphus_bench` is the patience-annotation mechanism (RAL-94) for marking a
Rust test's expected runtime budget — `#[ralphus_bench(patience = ...)]`.
The authoring rule has two halves:

1. **Deliberately consider patience** whenever you add or touch a test.
   Don't default to leaving it unset out of habit — decide whether the test
   needs a non-default patience budget.
2. **Comment whenever a value is explicitly set.** If you write
   `patience = ...`, that line must carry a justification comment (`//`)
   explaining *why* that budget is needed (e.g. spins up a subprocess, hits
   a real Ollama model, rebuilds a worktree) — either as a trailing comment
   on the same line, or as a comment on the line immediately above.

Only the second half is mechanically checkable — "did you think about it" is
a review-time discipline, not something a script can verify. That's what
`scripts/check_bench_patience_comments.py` enforces: it scans every tracked
`*.rs` file for `#[ralphus_bench(patience = ...)]` (and, harmlessly, every
tracked `*.py` file for the now-retired Python equivalent — no `*.py` file
carries that pattern anymore since the Python side of RAL-94 was reduced to
`cli/src/ralphus/bench/`'s graph renderer, which never times any test), and
fails if a match lacks a same-line trailing comment or a comment on the line
above. It's stdlib-only (no `uv sync` needed) and runs as its own CI job
(`bench-patience`) on every PR:

```bash
python scripts/check_bench_patience_comments.py
```

A clean run prints nothing and exits 0; violations print one
`<path>:<line>: patience value set without an inline comment explaining why`
line per offending match and exit nonzero.
