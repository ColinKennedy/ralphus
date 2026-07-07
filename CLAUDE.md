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

Three separately-buildable executables. The **daemon owns all state**; the CLI and librarian are clients of its HTTP/JSON API (`docs/daemon-api.md`). The SQLite DB is daemon-private.

| Component | Path | Language | Role |
|---|---|---|---|
| `ralphus-core` | `core/` | Rust lib | Task-file schema + validator + shared types. Dependency-light, heavily unit-tested. |
| `ralphus-daemon` | `daemon/` | Rust bin+lib | SQLite store (WAL), HTTP API, scheduler; spawns the runner per session. |
| `ralphus-librarian` | `librarian/` | Rust bin+lib | Web board; serves static HTML and proxies `/api/*` GETs to the daemon. |
| `ralphus` / `ralphus-runner` | `cli/` | Python + pydantic-ai | CLI (validate/submit/status) and the session runner. |

Data flow: `ralphus submit x.toml` → daemon validates + ingests into SQLite (state **Pending**) → scheduler claims it (up to `max_concurrent`), spawns a worker thread → the worker runs each session by invoking `ralphus-runner` (JSON `SessionSpec` on stdin → `SessionResult` on stdout) → command verifies run → task/run states finalized → librarian polls `/api/tasks` and renders it.

Key module map:
- `core/src/schema.rs` — `TaskFile`/`TaskDef`/`SessionDef`/`VerifyStep`, `ResolvedAgent` inheritance. Session `prompt` XOR `command`.
- `core/src/validate.rs` — raw-`toml::Value` validator: unknown keys, required fields, types, verify one-of, `restart_on` grammar, within-task dep cycles, 1-based line numbers.
- `daemon/src/store.rs` — `Store` (the only place SQL lives), `RunState`/`NodeState`, board views.
- `daemon/src/server.rs` — `route()` (pure, unit-testable) + `serve()` (tiny_http; starts the scheduler thread).
- `daemon/src/scheduler.rs` — claims Pending runs, worker threads run sessions + verifies. Subprocess waits happen OUTSIDE the store lock.
- `daemon/src/runner.rs` — `Runner` trait + `SubprocessRunner` (spawns `RALPHUS_RUNNER_CMD`).
- `daemon/src/verify.rs` — `command` verify execution.
- `cli/src/ralphus/runner/` — `spec.py` (wire contract), `tools.py` (workspace-confined file/shell tools), `execute.py`, `backend.py` (Protocol), `pydantic_backend.py` (native agent), `__main__.py`.
- `cli/src/ralphus/client.py` + `__main__.py` — CLI over the daemon API.
- `librarian/assets/board.html` — the entire dark-theme UI (plain HTML + inline JS, embedded via `include_str!`).

## Build / Test / Lint

Every commit must pass both. **Rust is strict**: `[workspace.lints]` sets `warnings = "deny"`, `unsafe_code = "forbid"`, `clippy::all = "deny"`.

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

## Running it

Two build scripts, two purposes (both in `scripts/`):

| Script | Speed | Output | Use it to |
|---|---|---|---|
| `scripts/build-debug.sh` | seconds (incremental) | runs from source, no `dist/` | iterate — esp. the GUI |
| `scripts/build-release.sh` | minutes | four standalone exes in `dist/` | package / distribute |

**Fast dev loop — `bash scripts/build-debug.sh`.** Debug-builds the daemon +
librarian with `cargo` (incremental, ~seconds) and points `RALPHUS_RUNNER_CMD` at
the **venv runner** (`cli/.venv/Scripts/ralphus-runner.exe`) via `uv sync --extra
runner` — so it never rebuilds the heavy bundled runner exe. It boots the daemon
(`127.0.0.1:7890`) in the background and the librarian board (`127.0.0.1:7474`) in
the foreground; **Ctrl-C stops both**. GUI loop: edit `librarian/assets/board.html`
→ re-run the script → refresh the browser. (board.html is `include_str!`-baked, so
a GUI edit costs one incremental librarian recompile.)

**Release build — `bash scripts/build-release.sh`.** Builds copyable standalone
binaries into `dist/`: `ralphus-daemon`, `ralphus-librarian`, `ralphus` (CLI), and
`ralphus-runner` (PyInstaller one-file bundling the pydantic-ai tree — this is the
slow part). Stop any running daemon/librarian first: they lock their own `dist/`
exes and the copy step will fail with "Device or resource busy".

Run the pieces directly (e.g. against a release build):

```bash
ralphus-daemon serve                          # HTTP API on 127.0.0.1:7890 (+ scheduler)
ralphus-librarian serve [--port 7474]         # web board; RALPHUS_DAEMON_URL points it at the daemon
cd cli && uv run ralphus submit task.toml     # or: validate / status
# or
.\dist\ralphus.exe submit task.toml
```

`scripts/build-release.cmd` builds all four standalone executables into `.\dist`: the two Rust bins via `cargo build --release`, and `ralphus.exe` / `ralphus-runner.exe` via PyInstaller one-file (from `scripts/ralphus_entry.py` / `scripts/ralphus_runner_entry.py`). Both Python builds run against a venv synced with `uv sync --extra runner` first, so both bundle `pydantic-ai` and its dependency tree — the CLI needs it too because `ralphus author` drives pydantic-ai in-process rather than delegating to the runner subprocess. That sync step alone installs ~90 packages and makes the build take noticeably longer (minutes, not seconds) than a plain CLI-only build would.

## Gotchas learned the hard way

- **Session `cwd` must be a real path for the OS the daemon runs on.** On Windows, an MSYS/Git-Bash `/tmp/...` path will not resolve in native-Windows Python — use a Windows path. `cwd` is mandatory and validated.
- **Runner command**: the daemon spawns `RALPHUS_RUNNER_CMD` (default `ralphus-runner`). In dev, point it at the venv script, e.g. `cli/.venv/Scripts/ralphus-runner.exe` — that's an editable install, so source edits under `cli/src/ralphus/runner/` take effect immediately with no build step. `dist/ralphus-runner.exe` (built by `scripts/build-release.cmd`) is a frozen PyInstaller snapshot instead; only rebuild it when you're about to hand out a `dist` build and something changed since the last one: runner source, `scripts/ralphus_runner_entry.py`, or the `runner` extra's pinned dependency versions.
- **pydantic-ai is the optional `runner` extra**, not a dev dependency. CI does not install it; `pydantic_backend.py` is imported lazily and a mypy override keeps strict checking green without it. The Ollama integration test (`cli/tests/test_ollama_integration.py`) skips unless the extra is installed and Ollama is reachable; run it with `uv run --extra runner pytest -k ollama`.
- **The full review flow has a live-ollama integration test** — `daemon/tests/reviews_derive.rs::full_flow_validate_submit_run_and_ollama_resolves_conflict` runs validate → submit → run → derive a per-project review of two conflicting branches → rebase, with the conflict resolved by a real ollama agent. It **skips** (prints `SKIP`) unless ollama is up on `127.0.0.1:11434`, the resolver model (`RALPHUS_RESOLVER_MODEL`, default `qwen3:8b`) is pulled, and a `ralphus-runner` is found (`RALPHUS_RUNNER_CMD` or the dev venv). Run it with `cargo test -p ralphus-daemon --test reviews_derive full_flow -- --nocapture`.
- **Monorepo integration test** — `daemon/tests/monorepo.rs` has three always-run pipeline tests (use a `CapturingRunner`; no real subprocess needed) and one live-ollama test (`full_monorepo_flow_with_subproject_sessions`) with the same skip idiom as `reviews_derive.rs`. Run the live test with `cargo test -p ralphus-daemon --test monorepo full_monorepo_flow -- --nocapture`.
- **Model selection** is per-session in TOML: `agent = "ollama"` + `model = "qwen3:8b"` for local; `agent = "claude"` (default) uses Anthropic (needs `ANTHROPIC_API_KEY`).
- **`prompt`-kind verify steps** run for real (`daemon/src/scheduler.rs::run_verifies`), reusing the owning session's resolved backend (`agent`/`model`, with the verify step's own `model` as an override) through the same `Runner`/`ralphus-runner` path as sessions. The runner wraps the verify prompt asking for a final `RALPHUS_VERIFY: PASS`/`FAIL` line and parses it back out (`cli/src/ralphus/runner/execute.py::_parse_verdict`). Small local models don't reliably put the marker alone on its own line as asked (e.g. `qwen3.5` wrote "...confirming RALPHUS_VERIFY: PASS." mid-sentence) — parsing searches the whole output for the marker and trusts the last occurrence, rather than requiring an exact whole-line match. No verdict found = FAIL (fail closed). There's a live-ollama end-to-end test for this: `daemon/tests/prompt_verify.rs` (same skip idiom as `reviews_derive.rs`; model via `RALPHUS_VERIFY_MODEL`, default `qwen3:8b`) plus a lighter Python-level one, `cli/tests/test_verify_ollama_integration.py`.
- **Port clash**: the old claudectl also uses 7474/7890; the daemon port is not yet CLI-configurable, so don't run both.
- **Submitting goes to `Pending` (schedulable now)**, not `Queued`. `hold=true` stages as `Queued`; `/activate` promotes it. (The predecessor's silently-`Queued`-forever bug — FINDINGS §2.4.)

## Built (Phases 0–5)

Task pipeline (submit → schedule → run via native pydantic-ai *or* harness backend → command/prompt verify → board); dependency-graph scheduling + `{handoff:...}`; cross-run gating; Guardian reviews (stacked **rebase** merge in a worktree — each branch rebased onto the prior against one snapshotted base commit, agent conflict resolution, check gates, auto-rebuild when the base branch shifts, feedback chat, Reviews UI); `ralphus` CLI (validate/submit/status/check health); standalone release builds. See `PLAN.local.md` for the per-item detail.

## What is NOT built yet (see PLAN.local.md)

`brain`/`approval` verify kinds + verify retry policy; a verify step's `arguments`/`budget_usd` (parsed but not enforced, same as session-level `args`/`budget_usd`); per-session verify results in the board API (`SessionView` has no `verify` field — only task-level verify is exposed via `GET /api/tasks`/`GET /api/runs/{id}` today); Guardian review cycles (multi-round approve/iterate beyond the single auto-rebuild); prism ("Open in Prism") desktop handoff; multi-user hardening (auth, per-user attribution, worker pool); editable detail pane; full URL-state routing; the draggable node-graph canvas + Logs modal in the UI.

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
