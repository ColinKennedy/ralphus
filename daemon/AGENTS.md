# daemon/

Owns all state (SQLite, WAL) and the scheduler; the CLI and librarian are
clients of its HTTP/JSON API (`docs/daemon-api.md`). See the root
`AGENTS.md`'s Architecture table and key-module map for the file-by-file
layout (`store.rs`, `server.rs`, `scheduler.rs`, `guardian.rs`,
`cartographer.rs`, ...).

Logging conventions (Cartographer + stderr sink) that this crate is the
primary owner of are documented at [[../.agent/logging-policy|logging-policy.md]]
rather than here, since they're shared with `runner/` and `cli-rs/`.
Daemon-specific runtime gotchas (port clash, live-Ollama test gating,
`Pending` vs `Queued` on submit) are in [[../.agent/gotchas|gotchas.md]].

## Testing — Rust integration tests

`cargo test --all-targets` runs everything. Key integration test files in `daemon/tests/`:

| File | Scope | Notes |
|---|---|---|
| `api_over_http.rs` | HTTP API contract | Two always-run tests; uses `Store::open_in_memory()` |
| `prompt_verify.rs` | Prompt-kind proof execution | Both tests are live-Ollama, `#[ignore]`d by default |
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

**Authoring rule — Ollama tests are opt-in, never on by default.** Any test
that calls out to a live Ollama model must be `#[ignore]`d, the same way
every existing Ollama test already is (see the table above). Don't add a new
Ollama-backed test that runs unconditionally. Keep the same runtime guard too
(skip/return early with a clear message if Ollama isn't reachable on
`127.0.0.1:11434` or the required model isn't pulled), so even an explicit
opt-in run degrades gracefully without live infra.
