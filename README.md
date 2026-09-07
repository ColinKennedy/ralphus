# ralphus

Reliable, harness-agnostic orchestration of autonomous agent tasks.

ralphus submits work as TOML-described tasks, runs them with any model (cloud or
local), verifies the results, and combines dependent branches into a reviewable
whole — with a web UI that multiple people can watch and drive at once.

It is a from-scratch successor to an earlier Rust project whose interconnection
worked but whose *agent provisioning* did not. ralphus keeps the durable backend
and the strong TOML/validation and review model, and rebuilds the part that
broke — actually launching an agent and having it do real work — behind a
model-agnostic runner that can be exercised end-to-end by local models.

## Architecture

Four standalone, copyable executables, all Rust:

| Executable | Crate | Role |
| ---------- | ----- | ---- |
| `ralphus-daemon` | `daemon/` | Authoritative task store (SQLite), scheduler, HTTP/JSON API. |
| `ralphus-librarian` | `librarian/` | Web UI over the daemon's API (plain HTML/JS, no build step). |
| `ralphus` | `cli/` | CLI: validate/submit/status/... — a thin HTTP client over the daemon's API. |
| `ralphus-runner` | `runner/` | Executes one cell/proof step; spawned per-cell by the daemon. |

The daemon owns all state; the CLI and librarian are clients of its
[HTTP/JSON API](docs/daemon-api.md). Seven more supporting crates
(`core`, `auth`, `keygen`, `ssh-provider`, `mcp`, and the `bench-*` trio) round
out the 11-member Rust workspace — see the Architecture table in `AGENTS.md`
for the full breakdown. `cli/` is a separate Python project kept only for doc
screenshot generation and bench-graph rendering; it is never shipped. See
`PLAN.local.md` for the build plan, `FINDINGS.local.md` for research on the
predecessor, and `FOLLOW.local.md` for decisions to revisit. Everything this
repo depends on to build or run — Rust toolchain, git, tmux/psmux, optional
agent CLIs, and more — is inventoried in [`docs/dependencies.md`](docs/dependencies.md).

## Repository layout

```
core/        Rust: shared schema, validation, and DTO types
daemon/      Rust: store + scheduler + HTTP API      (bin: ralphus-daemon)
librarian/   Rust: web UI server                     (bin: ralphus-librarian)
cli/      Rust: the `ralphus` CLI                 (bin: ralphus)
runner/      Rust: the cell/proof runner             (bin: ralphus-runner)
cli/         Python: docsgen + bench-graph tooling — dev-only, never shipped
docs/        API contract and design docs
```

## Development

Rust (strict — warnings are errors):

```bash
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo nextest run --workspace
```

Python (dev-only tooling — doc screenshots and bench-graph rendering, not the
shipped CLI; from `cli/`, managed with [uv](https://docs.astral.sh/uv/)):

```bash
uv sync --dev
uv run ruff check .
uv run ruff format --check .
uv run mypy
uv run privata src
uv run privata tests
uv run deadcode src tests
uv run pytest
```

Web (lints/type-checks `librarian/assets/board.html`'s inline JS; from repo
root, Node 22+):

```bash
npm install --no-audit --no-fund
npm run lint
npm run typecheck
npm run knip
npm test
```

## Quick start

Build the binaries (or use `cargo run`), then:

```bash
# 1. Start the daemon (HTTP API + scheduler) on 127.0.0.1:7890
ralphus-daemon serve

# 2. Start the web board on http://127.0.0.1:7474 (in another terminal)
ralphus-librarian serve            # open http://127.0.0.1:7474

# 3. Submit a task from the CLI (in another terminal)
ralphus validate task.toml         # optional: check it first
ralphus submit task.toml --label demo
ralphus status                     # list squads; `ralphus status <squad-id>` for detail
ralphus check health                # check the local setup
```

A minimal `task.toml` (a deterministic command cell — no model needed):

```toml
[[task]]
name = "hello"
[[task.cell]]
cwd = "/absolute/path/to/a/dir"     # must be a real directory
command = "echo hello > out.txt"
[[task.cell.proof]]
command = "test -f out.txt"          # fmt/lint/test-style gate
```

For an AI cell, replace `command` with a `prompt` and pick a model — local:

```toml
[[task.cell]]
cwd = "/absolute/path/to/repo"
agent = "ollama"
model = "qwen3:8b"
prompt = "Create hello.txt containing: hi"
```

The daemon spawns the runner via `RALPHUS_RUNNER_CMD` (default `ralphus-runner`
resolved from `PATH`); in a dev checkout built with `scripts/build-debug.sh`
this is already pointed at the just-built debug `ralphus-runner` exe for you.
`ollama` needs a reachable Ollama server (default `http://localhost:11434/v1`);
`claude-code`/`codex`/`pi` need the corresponding CLI installed. See
[`docs/dependencies.md`](docs/dependencies.md) for the full breakdown of which
agent backends need what.

## Status

MVP complete through Phase 5 (see `PLAN.local.md`:
`- [ ]` todo · `- [r]` done, needs verification · `- [x]` verified). The task
pipeline, dependency scheduling, Guardian reviews, CLI, and web board all work
end to end, including runs driven by a local Ollama model.

## License

Proprietary and exclusive — see [LICENSE](LICENSE).
