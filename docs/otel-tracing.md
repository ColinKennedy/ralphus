# OpenTelemetry Tracing

Distributed tracing (RAL-96) across the full user-story path: a `board.html`
button click → the librarian's proxy → the daemon's HTTP handler → the
scheduler → the `ralphus-runner` subprocess → the model backend call — all as
one trace, viewable as a flame/waterfall graph. Entirely opt-in: with no
collector configured, every process makes zero tracing-related network
calls, so the default dev loop (`scripts/build-debug.sh`) is unaffected.

## Viewing a trace (quickstart)

Quick terminology note: OTel here produces **traces** (spans with timing/parent-child structure), not logs — logs are still the separate Cartographer/`rlog!` system. Here's how to view the traces:

**1. Start the collector + Jaeger stack**
```bash
docker compose -f otel/docker-compose.yml up
```
(Requires Docker Desktop running. `-d` to detach if you don't want to keep the terminal open.)

**2. Point the daemon/librarian/runner at it, then launch as usual**

PowerShell:
```powershell
$env:OTEL_EXPORTER_OTLP_ENDPOINT = "http://127.0.0.1:4318"
bash scripts/build-debug.sh
```
Bash:
```bash
export OTEL_EXPORTER_OTLP_ENDPOINT=http://127.0.0.1:4318
bash scripts/build-debug.sh
```
This env var has to be set in the *same shell* that launches `build-debug.sh`, since the runner subprocess inherits it from the daemon. Without it set, every exporter is a no-op — nothing gets sent anywhere, which is the default.

**3. Generate a trace**

Open the board (`http://127.0.0.1:7474`), click **New Task**, submit a TOML task. That one click produces a full trace: browser → librarian → daemon HTTP → scheduler claim → cell execution → (for a `prompt` cell) the runner subprocess → the model call.

**4. View it**

Open **http://127.0.0.1:16686** (Jaeger UI) → pick a service from the dropdown (`ralphus-daemon`, `ralphus-librarian`, or `ralphus-runner`) → **Find Traces**. Click into one to see the waterfall — span names like `librarian.request` → `daemon.http` → `scheduler.cell` → `runner.subprocess` → `llm.cell`, all nested under one trace ID.

## How it works

All three processes (`daemon`, `librarian`, `runner`) are Rust, and all three
create spans the same way: directly through the
`opentelemetry`/`opentelemetry_sdk` crates' manual span API
(`opentelemetry::global::tracer(...).start_with_context(...)`) rather than
the `tracing` crate. `opentelemetry-otlp` (the official exporter) is
deliberately *not* a dependency — its HTTP transport pulls in `reqwest` →
`tokio` → `hyper`, and this workspace keeps no async runtime at all (it is
synchronous by design: `ureq`, `tiny_http`, a thread-per-worker scheduler)
with a deliberately small lock file, because build time is a first-class
constraint here. See AGENTS.md's Logging Policy for the two rules that
replaced the older "never use `tracing`" wording: stdout is reserved for the
daemon↔runner JSON contract (now enforced by `clippy::print_stdout = "deny"`),
and no async runtime may enter the workspace. Instead, each of
`daemon/src/otel.rs`, `librarian/src/otel.rs`, and `runner/src/otel.rs` hand-
rolls its own minimal OTLP/JSON `SpanExporter` over the already-used
synchronous `ureq` client — three standalone copies (not shared through
`ralphus-core`, which is kept dependency-light by design) rather than one
shared implementation, since `runner` in particular shouldn't need to pull
in the whole `daemon` lib (rusqlite, tiny_http, ...) just to reuse ~100 lines
of tracing code.

**Browser (`librarian/assets`).** No `opentelemetry-js` SDK — the board is
plain HTML + vanilla JS chunk files with no build step, and the full browser
SDK is too heavy for that. `newTraceparent()` in `board/20-util.js` hand-rolls the W3C
`traceparent` wire format (`00-<32 hex trace id>-<16 hex span id>-01`,
generated via `crypto.getRandomValues`) and attaches it as a header on every
mutating action (the `post`/`del` fetch helpers, plus the task-submit and
open-terminal calls that build their own `fetch()`), i.e. every "user presses
a button" action that hits the API.

**Known granularity gap.** The runner produces exactly one span per cell
(`llm.cell` or `llm.proof`, built in `runner/src/main.rs`'s
`run_traced`) wrapping the whole `run_cell()` call — there is currently no
nested sub-span around the actual model API call inside
`agent_backend.rs`/`llm_client.rs` the way a `llm-invoke.agent_run` child
span once existed. That narrower call boundary is covered today only by the
plain-text `ralphus [llm-invoke] ...` log lines (see AGENTS.md's Logging
Policy), not by a trace span — closing that gap (adding the nested span) is
follow-up work, not yet done.

## Trace propagation, hop by hop

1. **Browser → librarian.** `board.html` mints a fresh `traceparent` per
   click and sends it as a request header.
2. **Librarian → daemon.** `librarian/src/server.rs`'s `handle_with_trace`
   extracts the incoming header, starts a span, and forwards its own
   `traceparent` to the daemon as a header on the proxied call.
3. **Daemon HTTP → scheduler.** `daemon/src/server.rs`'s `route_with_trace`
   extracts the header and starts an HTTP span. Because a `POST /api/squads`
   only *submits* work — the scheduler claims and executes it later,
   asynchronously, on a different thread, well after the HTTP response has
   already been sent — the daemon persists the request's `traceparent` onto
   the new squad (`squads.trace_context` in SQLite; see
   `Store::set_squad_trace_context`/`squad_trace_context`). This is exactly what
   the W3C `traceparent` string is for: continuing a trace across a boundary
   where you can't just pass a live object.
4. **Scheduler → runner subprocess.** `daemon/src/scheduler.rs` reads the
   squad's stored trace context and builds spans for squad-claim, worktree
   placeholder resolution (`scheduler.resolve_worktrees`, RAL-100 — see
   `daemon/src/worktrees.rs`), cell execution, and proof execution
   (mirroring the existing `scheduler`/`runner`/`proof` log-event lifecycle
   points from AGENTS.md's Logging Policy) — each rebuilt from the
   traceparent *string*, not a shared `Context` object, since cells run on
   separate OS threads.
   `daemon/src/runner.rs` puts its own span's `traceparent` on the
   `CellSpec` JSON sent to the runner on stdin (`trace_context` field,
   `runner/src/spec.rs`) — an optional field on that existing, tested wire
   contract, so it never becomes required and old callers/tests are
   unaffected.
5. **Runner → model backend.** `runner/src/main.rs`'s `run_traced` builds a
   `Context` from the incoming `trace_context` field
   (`otel::context_from_traceparent`) and passes it explicitly into
   `start_span` for the one `llm.cell`/`llm.proof` span around the whole
   `run_cell()` call — explicit `Context` threading throughout, the same
   as every other Rust hop above, rather than an ambient/contextvar-style
   mechanism (Rust has no equivalent idiom to reach for here).

A missing/malformed `traceparent` at any hop degrades to "start a fresh
trace" rather than an error — a broken link produces a disconnected trace,
never a crash.

## Local collector + Jaeger

No collector infrastructure is required for local dev by default. To view a
trace, start the bundled stack (`otel/docker-compose.yml`):

```bash
docker compose -f otel/docker-compose.yml up
```

This runs an OpenTelemetry Collector (receiving OTLP/HTTP on `:4318`) that
forwards to a Jaeger all-in-one instance (UI on `:16686`).

Then, **before** starting the daemon/librarian/runner, point them at the
collector:

```bash
# bash
export OTEL_EXPORTER_OTLP_ENDPOINT=http://127.0.0.1:4318
bash scripts/build-debug.sh
```

```powershell
# PowerShell
$env:OTEL_EXPORTER_OTLP_ENDPOINT = "http://127.0.0.1:4318"
bash scripts/build-debug.sh
```

The runner subprocess inherits the daemon's environment, so setting the
variable once before `build-debug.sh` is enough to enable tracing across all
three processes — no per-process configuration needed.

Open **http://127.0.0.1:16686**, select the `ralphus-daemon`, `ralphus-librarian`,
or `ralphus-runner` service, and find a trace. Submitting a task from the
board is the easiest way to generate one end-to-end: click **New Task**,
submit, and the resulting trace spans the click, the HTTP round trip, the
scheduler claiming and running the cell, and (for a `prompt` cell) the
actual model call.

With `OTEL_EXPORTER_OTLP_ENDPOINT` unset, every exporter call site short-circuits
before any network I/O — this is what keeps tracing out of the default dev
loop.

## Gotchas

- **Port 4318/16686 clash.** If you already run something else on those
  ports, edit `otel/docker-compose.yml`'s port mappings and adjust
  `OTEL_EXPORTER_OTLP_ENDPOINT` to match.
- **Stopping the stack.** `docker compose -f otel/docker-compose.yml down`.
  Traces are not persisted across restarts (Jaeger's in-memory storage) —
  fine for local dev, not meant for long-term retention.
- **Never a console exporter.** Every exporter here (all three hand-rolled
  `ureq`-based ones) writes only to its own HTTP connection to the collector
  — never to stdout (the daemon↔runner JSON contract) or stderr (the
  `ralphus [TYPE] ...` log format / `RALPHUS_EVENT:` marker). A
  `ConsoleSpanExporter` must never be introduced on any of these paths.
