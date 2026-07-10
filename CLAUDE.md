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

Five Rust workspace members plus a Python project. The **daemon owns all state**; the CLI and librarian are clients of its HTTP/JSON API (`docs/daemon-api.md`). The SQLite DB is daemon-private.

| Component | Path | Language | Role |
|---|---|---|---|
| `ralphus-core` | `core/` | Rust lib | Task-file schema + validator + shared types. Dependency-light, heavily unit-tested. |
| `ralphus-daemon` | `daemon/` | Rust bin+lib | SQLite store (WAL), HTTP API, scheduler; spawns the runner per session. |
| `ralphus-librarian` | `librarian/` | Rust bin+lib | Web board; serves static HTML and proxies `/api/*` GETs to the daemon. |
| `ralphus-auth` | `auth/` | Rust lib | Ed25519 license verification (no-op without `--features secure-dist`). |
| `ralphus-keygen` | `keygen/` | Rust bin | Author-only tool: generate keypair + sign licenses. Never shipped to users. |
| `ralphus` / `ralphus-runner` | `cli/` | Python + pydantic-ai | CLI (validate/submit/status/author) and the session runner. |

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
- `auth/src/lib.rs` — Ed25519 license check (`check_license()`; compiles away without `secure-dist`).
- `keygen/src/main.rs` — keypair generation + license signing CLI.
- `cli/src/ralphus/runner/` — `spec.py` (wire contract), `tools.py` (workspace-confined file/shell tools), `execute.py`, `backend.py` (Protocol), `pydantic_backend.py` (native agent), `harness_backend.py` + `claude_code_backend.py` (external agents), `__main__.py`.
- `cli/src/ralphus/client.py` + `__main__.py` — CLI over the daemon API.
- `cli/src/ralphus/author/` — `core.py` (orchestration loop), `agent.py` (pydantic-ai TOML generator).
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
uvx privata src                     # module-privacy linter; declare intended public API in __all__
uv run pytest                       # run one: uv run pytest -k name
```

CI is `.github/workflows/ci.yml` (a Rust job and a Python job).

## Testing

### Backend — Rust integration tests

`cargo test --all-targets` runs everything. Key integration test files in `daemon/tests/`:

| File | Scope | Notes |
|---|---|---|
| `api_over_http.rs` | HTTP API contract | Two always-run tests; uses `Store::open_in_memory()` |
| `prompt_verify.rs` | Prompt-kind verify execution | Live Ollama; skips if Ollama down |
| `guardian_merge.rs` | Stacked rebase + conflict resolution | Uses `CapturingRunner` (no subprocess) |
| `reviews_derive.rs` | Full review flow end-to-end | Live Ollama; `RALPHUS_RESOLVER_MODEL` (default `qwen3:8b`) |
| `monorepo.rs` | Monorepo pipeline | 3 always-run + 1 live-Ollama test |

Run a specific live-Ollama integration test:
```bash
cargo test -p ralphus-daemon --test reviews_derive full_flow -- --nocapture
cargo test -p ralphus-daemon --test monorepo full_monorepo_flow -- --nocapture
cargo test -p ralphus-daemon --test prompt_verify -- --nocapture
```

All live-Ollama tests **skip** (print `SKIP`) unless Ollama is up on `127.0.0.1:11434` and the required model is pulled. They never fail CI.

### Backend — Python tests

`cd cli && uv run pytest`. Key test files:

| File | Covers |
|---|---|
| `test_runner.py` | Session spec parsing, workspace tools, fake backend (17 tests) |
| `test_client.py` | `DaemonClient` with mock `httpx` transport (8 tests) |
| `test_author.py` | `ralphus author`: intent parsing, token budget, validate loop, dry-run (78 tests) |
| `test_harness_backend.py` | Harness backend (external tool as stand-in) |
| `test_claude_code_backend.py` | Claude Code harness integration |
| `test_ollama_integration.py` | End-to-end Ollama prompt → write file (skips if Ollama down) |
| `test_verify_ollama_integration.py` | Prompt-kind verify with Ollama (same skip idiom) |
| `test_author_ollama_integration.py` | `ralphus author` with qwen3:8b (same skip idiom) |

Ollama integration tests require `uv run --extra runner pytest -k ollama`.

### Frontend — board.html

There are **no automated frontend tests**. The board is plain HTML + vanilla JS embedded via `include_str!` — testing is manual:

1. Run `bash scripts/build-debug.sh` (boots daemon + librarian).
2. Open `http://127.0.0.1:7474` in a browser.
3. Submit a task and watch the board update (polls every 2 s).
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
ralphus doctor

# Query the daemon API directly
curl http://127.0.0.1:7890/api/daemon
curl http://127.0.0.1:7890/api/tasks
curl http://127.0.0.1:7890/api/runs/<id>

# Run a specific Rust unit-test module
cargo test -p ralphus-daemon scheduler::
cargo test -p ralphus-core validate::

# Run a single Python test by name
cd cli && uv run pytest -k test_author_dry_run -s

# Build and run the keygen tool (author-only; not distributed)
cargo run -p ralphus-keygen -- generate
cargo run -p ralphus-keygen -- sign --key ralphus-private.key --name "Name" --expiry 2027-01-01

# Secure-dist build (daemon + librarian refuse to start without ralphus.lic)
cargo build --release --features ralphus-daemon/secure-dist,ralphus-librarian/secure-dist
```

## Running it

Two build scripts, two purposes (both in `scripts/`):

| Script | Speed | Output | Use it to |
|---|---|---|---|
| `scripts/build-debug.sh` | seconds (incremental) | runs from source, no `dist/` | iterate — esp. the GUI |
| `scripts/build-release.sh` | minutes | four standalone exes in `dist/` | package / distribute |

**Fast dev loop — `bash scripts/build-debug.sh`.** Debug-builds the daemon + librarian with `cargo` (incremental, ~seconds) and points `RALPHUS_RUNNER_CMD` at the **venv runner** (`cli/.venv/Scripts/ralphus-runner.exe`) via `uv sync --extra runner` — so it never rebuilds the heavy bundled runner exe. It boots the daemon (`127.0.0.1:7890`) in the background and the librarian board (`127.0.0.1:7474`) in the foreground; **Ctrl-C stops both**.

**Release build — `bash scripts/build-release.sh`.** Builds copyable standalone binaries into `dist/`: `ralphus-daemon`, `ralphus-librarian`, `ralphus` (CLI), and `ralphus-runner` (PyInstaller one-file bundling the pydantic-ai tree — this is the slow part). Stop any running daemon/librarian first: they lock their own `dist/` exes and the copy step will fail with "Device or resource busy".

Run the pieces directly:

```bash
ralphus-daemon serve                          # HTTP API on 127.0.0.1:7890 (+ scheduler)
ralphus-librarian serve [--port 7474]         # web board; RALPHUS_DAEMON_URL points it at the daemon
cd cli && uv run ralphus submit task.toml     # or: validate / status / author
.\dist\ralphus.exe submit task.toml           # Windows release build
```

`scripts/build-release.cmd` (Windows) builds all four standalone executables into `.\dist`: the two Rust bins via `cargo build --release`, and `ralphus.exe` / `ralphus-runner.exe` via PyInstaller one-file. Both Python builds run against a venv synced with `uv sync --extra runner` — that sync installs ~90 packages and makes the build take minutes.

## Cryptography / Secure Distribution

See `docs/secure-dist.md` for the full workflow. Summary:

**What it is:** An opt-in build mode (`--features secure-dist`) where `ralphus-daemon` and `ralphus-librarian` refuse to start without a signed `ralphus.lic` file. Standard open builds are completely unaffected — the check compiles away to nothing without the feature flag.

**Crates:**
- `auth/` — `ralphus-auth` lib; exports `check_license()`. Uses **ed25519-dalek** for Ed25519 signature verification, **base64** for signature encoding. Public key is embedded at compile time via `include_bytes!("../public.key")`.
- `keygen/` — `ralphus-keygen` bin (author-only, never distributed). Uses **rand_core::OsRng** for entropy. Subcommands: `generate` (keypair) and `sign` (license file).

**License file format** (`ralphus.lic`, JSON):
```json
{ "holder": "Alice", "expiry": "2027-01-01", "signature": "<base64-ed25519>" }
```
Message signed: `"RALPHUS|<holder>|<expiry>"` (or `"RALPHUS|<holder>|never"` if no expiry). Expiry is compared as a string (`YYYY-MM-DD` lexicographic order).

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
  --expiry 2027-01-01    # omit for non-expiring license

# 4. Recipient drops ralphus.lic next to the executables
#    (or sets RALPHUS_LICENSE=<path>)
```

Re-keying: delete `ralphus-private.key`, run `generate` again, commit the new `auth/public.key`, rebuild, re-sign all existing recipients — old license files will no longer verify.

## Gotchas learned the hard way

- **Session `cwd` must be a real path for the OS the daemon runs on.** On Windows, an MSYS/Git-Bash `/tmp/...` path will not resolve in native-Windows Python — use a Windows path. `cwd` is mandatory and validated.
- **Runner command**: the daemon spawns `RALPHUS_RUNNER_CMD` (default `ralphus-runner`). In dev, point it at the venv script, e.g. `cli/.venv/Scripts/ralphus-runner.exe` — that's an editable install, so source edits under `cli/src/ralphus/runner/` take effect immediately with no build step. `dist/ralphus-runner.exe` (built by `scripts/build-release.cmd`) is a frozen PyInstaller snapshot; only rebuild it when something changed since the last one.
- **pydantic-ai is the optional `runner` extra**, not a dev dependency. CI does not install it; `pydantic_backend.py` is imported lazily and a mypy override keeps strict checking green without it.
- **The full review flow has a live-Ollama integration test** — `daemon/tests/reviews_derive.rs::full_flow_validate_submit_run_and_ollama_resolves_conflict`. It **skips** unless Ollama is up on `127.0.0.1:11434`, the resolver model (`RALPHUS_RESOLVER_MODEL`, default `qwen3:8b`) is pulled, and a `ralphus-runner` is found.
- **Monorepo integration test** — `daemon/tests/monorepo.rs` has three always-run pipeline tests and one live-Ollama test. Run the live test with `cargo test -p ralphus-daemon --test monorepo full_monorepo_flow -- --nocapture`.
- **Model selection** is per-session in TOML: `agent = "ollama"` + `model = "qwen3:8b"` for local; `agent = "claude"` (default) uses Anthropic (needs `ANTHROPIC_API_KEY`).
- **`prompt`-kind verify steps** run for real, reusing the owning session's resolved backend through the same `Runner`/`ralphus-runner` path. The runner wraps the verify prompt and parses the final `RALPHUS_VERIFY: PASS`/`FAIL` line. Small local models don't put the marker alone on its own line — parsing searches the whole output and trusts the last occurrence. No verdict found = FAIL (fail closed).
- **Port clash**: the old claudectl also uses 7474/7890; the daemon port is not yet CLI-configurable, so don't run both.
- **Submitting goes to `Pending` (schedulable now)**, not `Queued`. `hold=true` stages as `Queued`; `/activate` promotes it. (The predecessor's silently-`Queued`-forever bug — FINDINGS §2.4.)
- **`keygen` is never shipped.** It is not in the release build scripts and should not be added. It is a workspace member only so `cargo build --all` can catch compile errors in CI.

## Built (Phases 0–5 + authoring)

Task pipeline (submit → schedule → run via native pydantic-ai *or* harness backend → command/prompt verify → board); dependency-graph scheduling + `{handoff:...}`; cross-run gating; Guardian reviews (stacked **rebase** merge in a worktree — each branch rebased onto the prior against one snapshotted base commit, agent conflict resolution, check gates, auto-rebuild when the base branch shifts, feedback chat, Reviews UI); `ralphus` CLI (validate/submit/status/doctor); `ralphus author` (agentic TOML generation with intent parsing, token budgeting, review gating); harness backend (external agents like `claude-code`, `aider`); standalone release builds; secure-distribution licensing (`ralphus-auth` + `ralphus-keygen`). See `PLAN.local.md` for per-item detail.

## What is NOT built yet (see PLAN.local.md)

`brain`/`approval` verify kinds + verify retry policy; a verify step's `arguments`/`budget_usd` (parsed but not enforced); per-session verify results in the board API (`SessionView` has no `verify` field — only task-level verify is exposed today); Guardian review cycles (multi-round approve/iterate beyond the single auto-rebuild); detached daemon lifecycle (`ralphus daemon start/stop/status` with PID file); prism ("Open in Prism") desktop handoff; multi-user hardening (auth, per-user attribution, worker pool); editable detail pane; full URL-state routing; the draggable node-graph canvas + Logs modal in the UI.

## UI Tooltip Rule (RAL-40)

Every new UI element in `librarian/assets/board.html` **must ship with a tooltip** using the `data-tip="..."` attribute. The tooltip engine is a lightweight JS+CSS system already wired into the page (see the `// ---- Tooltip engine (RAL-40) ----` block in the `<script>` tag and the `#board-tip` CSS rule).

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

## Logging Policy (RAL-79)

All log output goes to **stderr** only (`eprintln!` in Rust; `print(..., file=sys.stderr)` in Python). Never use the `tracing` crate. The stdout channel carries structured JSON between the daemon and runner subprocess — do not pollute it with log lines.

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
| `llm` | `cli/src/ralphus/runner/execute.py` | Session/verify start (run/session/agent/model/prompt_len/prompt_hash), system-prompt applied (len/position), done (tokens/cost), error — all at the execute layer |
| `llm-invoke` | `cli/src/ralphus/runner/pydantic_backend.py` | Actual `agent.run_sync` start (prompt_len/hash), done (elapsed/tokens), error — at the model API call layer |
| `runner` | `cli/src/ralphus/runner/__main__.py` | Runner invoked (run/session/agent/model/verify) |
| `cli` | `cli/src/ralphus/__main__.py` | CLI subcommand invoked with its parsed args |

**Required events** — any new code path that touches these must emit the corresponding log line:
- All entity state transitions (run, task, session, verify)
- Every LLM call: before start and after completion (with outcome and duration)
- Every manual user action: CLI subcommand + args, HTTP endpoint hit
- System-prompt synthesis or borrow from task/session data
- Key scheduler lifecycle: session claimed, session completed, verify started
- Subprocess spawn/cancel/timeout
