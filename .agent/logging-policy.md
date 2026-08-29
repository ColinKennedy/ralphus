# Logging Policy (RAL-79, amended by Cartographer / RAL-98)

Cross-cutting across `daemon/`, `runner/`, and `cli-rs/` — this is the canonical
copy; those folders' `AGENTS.md` link here rather than duplicating it.

There are now two layers, and both fire together — this reconciles the original
stderr-only mandate below with Cartographer, the DB/file-backed structured
event log:

1. **Cartographer** (`daemon/src/cartographer.rs`) is the primary, queryable
   record: every notable event — task/cell lifecycle, proof starts/results,
   status transitions, Guardian review lifecycle — is written as one row with a
   timestamp, human-readable message, source location, entity references
   (squad/cell/guardian/task), and an arbitrary JSON payload, into the
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
daemon-wide lint now prevents an unpaired `rlog!` from being introduced. New
code should prefer emitting a Cartographer record over introducing another
`rlog!`-only call site, especially for anything a user would want to query
later (state transitions, LLM calls, manual actions).

### `rlog!` / Cartographer pairing lint

Run `python scripts/check_rlog_cartographer_pairs.py` from the repository root.
The stdlib-only checker scans every tracked Rust source under `daemon/src/` and
runs as the `rlog / Cartographer pairing lint` CI job. It ignores mentions in
comments and string literals. A real `rlog!` invocation passes when a nearby
(within 80 lines) structured emitter shares a brace-delimited Rust block with
it. Recognized emitters are `Store::cartographer_log`, `Store::log_event*`,
`Note::emit`, and same-file helper functions which wrap one of those emitters.
Statements and match branches may intervene; strict adjacency is not required.

When an infrastructure boundary genuinely cannot reach a `Store`, put this
comment immediately above the call (or after it on the same line):

```rust
// ralphus[ignore-rlog-pair]: provider boundary has no Store; caller records the structured outcome
crate::rlog!(WARNING, "ralphus [remote] provider fallback");
```

The colon and explanation are required. To prevent marker-only bypasses, the
checker rejects explanations shorter than 12 characters or three words. An
exemption documents why a structured row is impossible or already owned at
another layer; it is not a substitute for adding an emitter when a `Store` is
available.

**Runner subprocess → daemon channel:** the runner cannot write Cartographer
rows directly (it has no DB access and stdout is reserved — see below), so it
emits a JSON line to stderr prefixed with `RALPHUS_EVENT: `, mirroring the
existing `RALPHUS_PROOF: PASS/FAIL` marker-parsing pattern. `daemon/src/runner.rs`
reads the child's stderr line-by-line (not just at exit) and forwards any
matching line into Cartographer, enriching `squad_id`/`cell_id`/`task` from
the owning `RunnerSpec` when the event omits them.

All log output still goes to **stderr** only (`eprintln!` in Rust) for the
plain-text sink. The stdout channel carries structured JSON between the
daemon and runner subprocess — do not pollute it with log lines
(Cartographer's runner-side events use the stderr marker above, not stdout).

**The stdout rule is mechanically enforced.** `[workspace.lints.clippy]` in the
root `Cargo.toml` sets `print_stdout = "deny"` — note this must be named
explicitly, because `print_stdout` lives in clippy's `restriction` group and is
*not* covered by the `all = "deny"` beside it. A stray `println!` does not
crash anything; it intermittently corrupts a `CellSpec`/`CellResult` JSON
parse only when something prints while a cell is running, which is exactly
the kind of bug that costs a day. The legitimate stdout writers — CLI output that *is* the
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
`ureq` exporter in `daemon/src/otel.rs` (see [[otel-tracing]]).

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
| `state` | `daemon/src/store.rs` | Every entity state transition: `{entity} {id} {old_state} → {new_state}` (and `output_len` for proof results) |
| `submit` | `daemon/src/store.rs` | `insert_squad`: squad inserted with `state=`, `tasks=` count |
| `recovery` | `daemon/src/store.rs` | Orphaned squads recovered on daemon startup |
| `http` | `daemon/src/server.rs` | Every HTTP request: `{METHOD} {path} → {status}` |
| `scheduler` | `daemon/src/scheduler.rs` | Squad claimed, squad executing (cell+task counts), cell start (agent/model), cell completed (status/tokens), proof starting (kind/agent/model) |
| `runner` | `daemon/src/runner.rs` | Subprocess spawned (pid/squad/cell/agent/model/timeout), cancelled, timed out, result parsed (status/tokens/cost) |
| `spec` | `daemon/src/runner.rs` | System-prompt synthesis: which case applied (user-supplied / addendum / combined), lengths |
| `proof` | `daemon/src/proof.rs` | Command proof starting (cwd/command) and completed (passed) |
| `llm` | `runner/src/execute.rs` | Cell/proof start (squad/cell/agent/model/prompt_len/prompt_hash), system-prompt applied (len/position), done (tokens/cost) or error — at the execute layer |
| `llm-invoke` | `runner/src/agent_backend.rs` | The actual model API call (via `llm_client::run_agent`) start (agent/model/prompt_len/hash), done (elapsed/tokens), or error — at the model API call layer |
| `runner` | `runner/src/main.rs` | Runner invoked (squad/cell/agent/model/proof) |
| `cli` | `cli-rs/src/main.rs` | CLI subcommand invoked with its parsed `Command` |

**Required events** — any new code path that touches these must emit the corresponding log line, and — per the Cartographer amendment above — a matching structured record wherever a `Store` is reachable:
- All entity state transitions (squad, task, cell, proof)
- Every LLM call: before start and after completion (with outcome and duration)
- Every manual user action: CLI subcommand + args, HTTP endpoint hit
- System-prompt synthesis or borrow from task/cell data
- Key scheduler lifecycle: cell claimed, cell completed, proof started
- Subprocess spawn/cancel/timeout
