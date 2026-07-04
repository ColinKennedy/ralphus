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

```bash
ralphus-daemon serve                          # HTTP API on 127.0.0.1:7890 (+ scheduler)
ralphus-librarian serve [--port 7474]         # web board; RALPHUS_DAEMON_URL points it at the daemon
cd cli && uv run ralphus submit task.toml     # or: validate / status
# or
.\dist\ralphus.exed submit task.toml
```

## Gotchas learned the hard way

- **Session `cwd` must be a real path for the OS the daemon runs on.** On Windows, an MSYS/Git-Bash `/tmp/...` path will not resolve in native-Windows Python — use a Windows path. `cwd` is mandatory and validated.
- **Runner command**: the daemon spawns `RALPHUS_RUNNER_CMD` (default `ralphus-runner`). In dev, point it at the venv script, e.g. `cli/.venv/Scripts/ralphus-runner.exe`.
- **pydantic-ai is the optional `runner` extra**, not a dev dependency. CI does not install it; `pydantic_backend.py` is imported lazily and a mypy override keeps strict checking green without it. The Ollama integration test (`cli/tests/test_ollama_integration.py`) skips unless the extra is installed and Ollama is reachable; run it with `uv run --extra runner pytest -k ollama`.
- **Model selection** is per-session in TOML: `agent = "ollama"` + `model = "qwen3:8b"` for local; `agent = "claude"` (default) uses Anthropic (needs `ANTHROPIC_API_KEY`).
- **Port clash**: the old claudectl also uses 7474/7890; the daemon port is not yet CLI-configurable, so don't run both.
- **Submitting goes to `Pending` (schedulable now)**, not `Queued`. `hold=true` stages as `Queued`; `/activate` promotes it. (The predecessor's silently-`Queued`-forever bug — FINDINGS §2.4.)

## Built (Phases 0–5)

Task pipeline (submit → schedule → run via native pydantic-ai *or* harness backend → command verify → board); dependency-graph scheduling + `{handoff:...}`; cross-run gating; Guardian reviews (stacked cherry-pick merge in a worktree, agent conflict resolution, check gates, Reviews UI); `ralphus` CLI (validate/submit/status/doctor); standalone release builds. See `PLAN.local.md` for the per-item detail.

## What is NOT built yet (see PLAN.local.md)

`brain`/`approval` verify kinds + verify retry policy; Guardian base-branch-shift/auto-rebuild and review cycles + feedback chat; prism ("Open in Prism") desktop handoff; multi-user hardening (auth, per-user attribution, worker pool); editable detail pane; full URL-state routing; the draggable node-graph canvas + Logs modal in the UI; standalone packaging of the pydantic-ai runner.
