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

Three separately-buildable, copyable executables:

| Executable    | Language               | Role |
| ------------- | ---------------------- | ---- |
| `ralphus`     | Python + pydantic-ai   | CLI: validate & submit tasks; run agent sessions (runner). |
| daemon        | Rust                   | Authoritative task store (SQLite), scheduler, HTTP/JSON API. |
| librarian     | Rust + plain HTML/JS   | Web UI over the daemon's API. Fast to iterate; no build step. |

The daemon owns all state; the CLI and librarian are clients of its
[HTTP/JSON API](docs/daemon-api.md). See `PLAN.local.md` for the build plan,
`FINDINGS.local.md` for research on the predecessor, and `FOLLOW.local.md` for
decisions to revisit.

## Repository layout

```
core/        Rust: shared schema, validation, and DTO types
daemon/      Rust: store + scheduler + HTTP API  (bin: ralphus-daemon)
librarian/   Rust: web UI server                 (bin: ralphus-librarian)
cli/         Python: the `ralphus` CLI + runner
docs/        API contract and design docs
```

## Development

Rust (strict — warnings are errors):

```bash
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo test --all-targets
```

Python (from `cli/`, managed with [uv](https://docs.astral.sh/uv/)):

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
ralphus status                     # list runs; `ralphus status <run-id>` for detail
ralphus check health                # check the local setup
```

A minimal `task.toml` (a deterministic command session — no model needed):

```toml
[[task]]
name = "hello"
[[task.session]]
cwd = "/absolute/path/to/a/dir"     # must be a real directory
command = "echo hello > out.txt"
[[task.verify]]
command = "test -f out.txt"          # fmt/lint/test-style gate
```

For an AI session, replace `command` with a `prompt` and pick a model — local:

```toml
[[task.session]]
cwd = "/absolute/path/to/repo"
agent = "ollama"
model = "qwen3:8b"
prompt = "Create hello.txt containing: hi"
```

The daemon spawns the runner via `RALPHUS_RUNNER_CMD` (default `ralphus-runner`);
in a dev checkout point it at `cli/.venv/Scripts/ralphus-runner` (install the
runner extra with `uv sync --extra runner` for AI sessions).

## Status

MVP complete through Phase 5 (see `PLAN.local.md`:
`- [ ]` todo · `- [r]` done, needs verification · `- [x]` verified). The task
pipeline, dependency scheduling, Guardian reviews, CLI, and web board all work
end to end, including runs driven by a local Ollama model.

## License

Proprietary and exclusive — see [LICENSE](LICENSE).
