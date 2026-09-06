# daemon/

Owns all state (SQLite, WAL) and the scheduler; the CLI and librarian are
clients of its HTTP/JSON API (`docs/daemon-api.md`). See the root
`AGENTS.md`'s Architecture table and key-module map for the file-by-file
layout (`store.rs`, `server.rs`, `scheduler.rs`, `guardian.rs`,
`cartographer.rs`, ...).

Logging conventions (Cartographer + stderr sink) that this crate is the
primary owner of are documented at [[../.agent/logging-policy|logging-policy.md]]
rather than here, since they're shared with `runner/` and `cli/`.
Daemon-specific runtime gotchas (port clash, live-Ollama test gating,
`Pending` vs `Queued` on submit) are in [[../.agent/gotchas|gotchas.md]].

## Tmux cell process-tree confinement (RAL-321)

On Windows, `tmux.rs`'s `confine` module assigns each cell's `new-session`
client `Child` to a Windows Job Object (`JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`)
immediately after spawn, keyed by session name in a process-global registry.
Job membership is inherited automatically by any child the client process
spawns (the psmux server, then the shell, then the runner/agent CLI), so
`Tmux::kill_session` dropping that job kills the whole real process tree, not
just the top `tmux.exe`/client box — `force_kill_tmux_processes` remains only
as a last-resort fallback for the server process itself. This relies on the
assignment completing before the client has a chance to spawn the psmux
server (the same accepted race `daemon/src/proof.rs`'s `ProcessTree` already
documents for check-gate commands); it's confirmed to hold in practice by
`runner.rs`'s `live_tmux_cancel_kills_the_real_process_tree` test, which
starts a real tmux-wrapped cell, cancels it, and polls for the real OS PID to
die.

**No equivalent confinement exists for the tmux path on Unix** — real tmux
uses `setsid()` for its server, which defeats process-group inheritance from
the client, so a Unix fix would need a different mechanism (e.g. resolving
and signalling the session's pane PIDs directly) and is not implemented.
`runner/src/tools.rs::run_bash`'s non-tmux timeout-kill path does have a
working Unix implementation (`process_group(0)` + `killpg`), since that path
doesn't go through tmux/psmux at all.

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
