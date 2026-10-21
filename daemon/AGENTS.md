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
Anything touching `forge.rs`/`pr.rs` (GitHub/GitLab calls, PR/MR submission,
base resync, stacking) must follow
[[../.agent/forge-design-principles|forge-design-principles.md]] — forge
parity + REST-over-CLI, and always folding a PR/MR into the review's
existing stack, with regression tests to match.

## Project display: name, never a raw path (RAL-396)

Whenever a cell/worktree resolves to a registered project, any API response
consumed by the web board must expose that project's *name*
(`Store::project_name_for_path`), not the on-disk path it resolved from. A
cell can run on a remote machine (RAL-185), so its path is not a stable,
UI-facing concept — it can be unknowable or change out from under a running
board. The whole point of registering a project is to give the UI something
stable to key off of instead of a path.

Concretely: resolve worktree → project *server-side* (daemon or librarian),
using `Store::project_name_for_path` (already the resolver used by PR/fork
routing in `pr.rs`) — never ship a raw path to `librarian/assets/board/*.js`
for the UI to resolve or display itself. Fall back to showing the raw path
only when no registered project matches at all (there is no registered
project to protect the path's meaning); once a project association exists,
its name is the only thing the UI shows. `server.rs::squad_worktrees`
(`CellPaths::project`) is the reference implementation.

## Lazy worktree materialization (RAL-612)

Submit claims worktree slots (branch + `w/<short>` + a `creating/<short>`
marker) with no git work; `scheduler.rs` creates them in the background while
ready cells start, and a cell whose worktree is still missing creates it itself
via `worktrees::ensure_claimed_worktree` (single-flight with the background
creator, restart-safe because the marker carries branch and upstream). See
`.agent/gotchas.md` for the ownership rules. Race tests for this live in
`worktrees.rs`'s test module (`concurrent_*`, `*_interrupted_by_restart`,
`*_never_deleted`).

## Store state that is not in SQLite (WS-E.1)

`Store` is the only place SQL lives, but it is not the place non-database state
lives. Worktree leases, restack bookkeeping, run liveness, stall-escalation
debounce and the secret-env-name cache are all in
[`store_memory.rs`](src/store_memory.rs), behind their own small locks, reached
via `Store::memory()` or -- without taking the store lock at all --
`StoreMutex::lock_free_memory()`.

**Do not add a non-SQL collection to `Store`.** Anything on `Store` can only be
reached through the global `StoreMutex`, so putting a `HashMap` there forces
every reader of it to queue behind the scheduler and the guardian-merge workers
for data SQLite never sees. It also blocks the read-path migration: a read
method cannot move to a pooled connection while it still has to reach a map only
the writer lock protects.

The locks in `store_memory.rs` are grouped by the invariants that span them, not
one per field -- read that module's doc comment before adding a field, because
two of the groupings are load-bearing (the restack/worktree-lease interlock in
particular, where separate locks would let a restack and a lease both be held
for one guardian).

## Testing — mocking a forge

Unit tests that need a fake GitHub/GitLab REST API use
[`mock_forge::MockForge`](src/mock_forge.rs) rather than hand-rolling a
`tiny_http` server + thread: `MockForge::start(move |server| ...)` runs the
handler (a `server.recv()` script or a `for req in server.requests()`
dispatcher), `mock.client(kind, repo)` builds the `ForgeClient`, and
`mock.finish()` shuts it down and returns the handler's value. Shutdown is
drop-based, so a handler waiting on a request that never comes fails the test
instead of hanging it, and requests after the handler has exited get a prompt
`500` -- no `recv_timeout` loops or `/done` sentinels needed.

## Testing — Rust integration tests

`cargo nextest run --all-targets` runs everything. Key integration test files in `daemon/tests/`:

| File | Scope | Notes |
|---|---|---|
| `api_over_http.rs` | HTTP API contract | Two always-run tests; uses `Store::open_in_memory()` |
| `prompt_verify.rs` | Prompt-kind proof execution | Both tests are live-Ollama, `#[ignore]`d by default |
| `guardian_merge.rs` | Stacked rebase + conflict resolution | Mostly `CapturingRunner`-based (no subprocess); 1 live-Ollama test, `#[ignore]`d by default |
| `reviews_derive.rs` | Full review flow end-to-end | 1 live-Ollama test, `#[ignore]`d by default; `RALPHUS_RESOLVER_MODEL` (default `qwen3:8b`) |
| `monorepo.rs` | Monorepo pipeline | 3 always-run + 1 live-Ollama test, `#[ignore]`d by default |
| `board_cold_load_perf.rs` | RAL-414: board endpoint cold-load budget vs. realistic fixtures | 4 always-run smoke tests + 4 `#[ignore]`d heavy tests; the heavy tests run every PR in the `perf-tests` CI job (`.github/workflows/ci.yml`), not the `rust` job's default `Test` step |
| `review_input_races.rs` | Commits reaching a review from several inputs at once (feedback, PR auto-fix, reviewer pushes, review-worktree commits, upstream base moves) must all end up on every PR branch | Real git with a bare upstream/PR remote and a file-backed store; the base moves by *pushing upstream*, and the daemon's own `poll_base_branch_freshness_once` + `review_maintenance` (the `run_loop` calls, on a 0.1 s cadence) do the rebase. Only the resolver LLM is a deterministic stand-in, held mid-edit to force each race open. Forge races run against `FakeGitHub`, a stateful fake of the GitHub **and GitLab** REST endpoints (the parametrised forge tests run once per forge; `with_forge_kind(.., "gitlab")`; it also models workflow runs/pipelines and their cancellation, a native-stack base-change restriction, injected 5xx failures, and several repositories for fork routing): `origin` is `ssh://localhost/acme/w.git` (a `core.sshCommand` shim runs real git on the bare upstream) and `.ralphus.toml`'s `[forge] api_base` points the daemon's own client resolution at the fake. Relies on nextest's process-per-test isolation, and is excluded from every default nextest run by `.config/nextest.toml`'s `default-filter`; run it with `-P review-races --features race-suite` (`cargo nextest run -P review-races -p ralphus-daemon --features race-suite [-E "test(name)"]`, plus `--run-ignored only` for `soak_*`). CI runs it only from `.github/workflows/race-soak-nightly.yml` (ubuntu + windows), triggered by pushing to the `race-suite` branch (`git push <remote> HEAD:race-suite --force`); there is no cron because a schedule only fires from the default branch, which doesn't carry this file. Never on pull requests. Hermetic: the first test to touch the filesystem re-executes the binary with `RALPHUS_CONFIG_HOME` and the working directory pointed at an empty temp dir (`ensure_hermetic_environment`), so a developer's own `~/.config/ralphus/config.toml` and `.ralphus.toml` never leak in. The seeded stress test is `#[ignore]`d and runs in the nightly workflow; the ~15-minute 8-branch soak (`soak_*`) is `#[ignore]`d and runs there too; run it locally with `-P review-races --features race-suite --run-ignored only -E "test(/^soak_/)"`. Every other `soak_*` test (20-branch stack, every release order of three held writers, conflicting writers) runs there too. `soak_randomly_seeded_time_boxed` is a no-op unless `RALPHUS_RACE_NIGHTLY=1`; `.github/workflows/race-soak-nightly.yml` sets it, picks a fresh seed, prints it (`RACE SOAK SEED`) and replays with `RALPHUS_RACE_SEED=<n>`. Every test that calls `assert_no_unexpected_agent_calls` also fails if the review logged a "review-only commits not carried" or fork-point carry WARNING |
| `board_contention.rs` | WS-B.4: board reads vs. four concurrent writers over real HTTP | 1 `#[ignore]`d test; runs in the `perf-tests` CI job. File-backed store — `open_in_memory`'s `cache=shared` raises `SQLITE_LOCKED` under concurrent read/write |
| `store_write_throughput.rs` | WS-B.5: the writer's sustained commit rate, batched and unbatched | 2 `#[ignore]`d tests; runs in the `perf-tests` CI job |
| `workload_replay.rs` | WS-B.6: the captured production event mix replayed at its recorded rates | 3 `#[ignore]`d tests; runs in the `perf-tests` CI job. `RALPHUS_REPLAY_SECONDS` lengthens the window (the WS-G.5 soak knob) |
| `submit_lock_hold.rs` | WS-D.8: the submit path's worst store-lock *hold* (`GET /api/daemon` → `guard_hold.max_ms`) | 1 always-run test. Asserts hold duration, not request latency: a submit may legitimately be slow, but it must not make every other request slow |
| `simple_tab_submit.rs` | The board "+ New Squad" Simple tab: runs the real `ntSimpleBuildToml` under Node (`test/emit-simple-task-toml.mjs`) and feeds every generated TOML through the validator and `POST /api/squads` | 2 always-run tests; no model call. Needs `node` (present on CI runners; skips locally if absent, fails if `CI` is set) |
| `read_path_ratchet.rs` | WS-E.4: how many `GET` handlers (and which background workers) read from the read pool rather than the writer lock | 4 always-run tests. A source scan, because which path a handler takes is not visible to a latency test until it happens to land behind a slow writer. Raise `MIN_POOLED_GET_HANDLERS` as handlers migrate; never lower it. Also pins the per-tick review-maintenance workers in `MAINTENANCE_WORKERS_OFF_THE_LOCK` (`pr.rs`/`ci_watch.rs`/`waypoints.rs`, incl. the waypoint survey sweep) to hydrating reviews through the read pool |

**Live-Ollama tests are `#[ignore]`d by default** — a plain `cargo nextest run`/`cargo nextest run --all-targets` never runs them, so CI and the normal dev loop never depend on a local model. Run them explicitly with `--ignored`:
```bash
cargo nextest run -p ralphus-daemon --test reviews_derive full_flow -- --ignored --nocapture
cargo nextest run -p ralphus-daemon --test monorepo full_monorepo_flow -- --ignored --nocapture
cargo nextest run -p ralphus-daemon --test prompt_verify -- --ignored --nocapture
cargo nextest run -p ralphus-daemon --test guardian_merge generate_summary_live_ollama -- --ignored --nocapture
# or, to run every ignored (live-Ollama) test across the workspace at once:
cargo nextest run --all-targets -- --ignored
```

Each still carries its own runtime guard too (prints `SKIP` and returns early) if Ollama isn't up on `127.0.0.1:11434` or the required model isn't pulled — so even an explicit `--ignored` run degrades gracefully without live infra.

**Authoring rule — Ollama tests are opt-in, never on by default.** Any test
that calls out to a live Ollama model must be `#[ignore]`d, the same way
every existing Ollama test already is (see the table above). Don't add a new
Ollama-backed test that runs unconditionally. Keep the same runtime guard too
(skip/return early with a clear message if Ollama isn't reachable on
`127.0.0.1:11434` or the required model isn't pulled), so even an explicit
opt-in run degrades gracefully without live infra.

## Test scratch directories (RAL-690)

Use `ralphus-test-support` (`TestDir`, `scratch`) for test fixtures, never
`std::env::temp_dir()`. `RALPHUS_TEST_TMP` relocates them (point it at a
non-Temp drive on Windows); `RALPHUS_KEEP_TEST_TMP=1` keeps them. Details:
`.agent/gotchas.md`.

Why it matters: on Windows the profile service scans all of `%TEMP%` at every
logon, so leftovers there cost minutes per boot. To pay down an existing
backlog run `scripts/sweep-temp.ps1 -DryRun`, then without `-DryRun` (it only
deletes allow-listed ralphus directories). Mechanism and numbers: the
"`%TEMP%` size is logon time" entry in `.agent/gotchas.md`.
