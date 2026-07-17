# OpenTelemetry Tracing

Distributed tracing (RAL-96) across the full user-story path: a `board.html`
button click → the librarian's proxy → the daemon's HTTP handler → the
scheduler → the `ralphus-runner` Python subprocess → the model backend call —
all as one trace, viewable as a flame/waterfall graph. Entirely opt-in: with
no collector configured, every process makes zero tracing-related network
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

Open the board (`http://127.0.0.1:7474`), click **New Task**, submit a TOML task. That one click produces a full trace: browser → librarian → daemon HTTP → scheduler claim → session execution → (for a `prompt` session) the Python runner subprocess → the model call.

**4. View it**

Open **http://127.0.0.1:16686** (Jaeger UI) → pick a service from the dropdown (`ralphus-daemon`, `ralphus-librarian`, or `ralphus-runner`) → **Find Traces**. Click into one to see the waterfall — span names like `librarian.request` → `daemon.http` → `scheduler.session` → `runner.subprocess` → `llm.session` → `llm-invoke.agent_run`, all nested under one trace ID.

## How it works

**Rust (`daemon`, `librarian`).** Spans are created directly through the
`opentelemetry`/`opentelemetry_sdk` crates' manual span API
(`opentelemetry::global::tracer(...).start_with_context(...)`) — never the
`tracing` crate. AGENTS.md's Logging Policy (RAL-79) bans `tracing`
workspace-wide because stdout is reserved for the daemon↔runner JSON
contract, and `tracing`'s ecosystem defaults (subscribers, layers) too easily
leak onto it. `opentelemetry-otlp` (the official exporter) is deliberately
*not* a dependency either — its HTTP transport pulls in `reqwest` → `tokio` →
`tracing` transitively. Instead, `daemon/src/otel.rs` (duplicated, with a
different service name, as `librarian/src/otel.rs` — `ralphus-core` is kept
dependency-light by design, so this isn't shared through it) hand-rolls a
minimal OTLP/JSON `SpanExporter` over the already-used synchronous `ureq`
client.

**Python (`ralphus-runner`).** No such constraint applies to Python, so
`ralphus/runner/otel.py` uses the official `opentelemetry-sdk` and
`opentelemetry-exporter-otlp-proto-http` packages directly. Context
propagation uses the SDK's ordinary contextvar-based ambient context
(`otel.attach_trace_context(...)` + `tracer.start_as_current_span(...)`)
rather than threading an explicit `Context` object through every call —
the runner is single-threaded per subprocess, so this is both idiomatic and
safe, unlike the Rust daemon's multi-threaded scheduler (see below).

**Browser (`board.html`).** No `opentelemetry-js` SDK — the board is plain
HTML with inline vanilla JS and no build step, and the full browser SDK is
too heavy for that. `newTraceparent()` in `board.html` hand-rolls the W3C
`traceparent` wire format (`00-<32 hex trace id>-<16 hex span id>-01`,
generated via `crypto.getRandomValues`) and attaches it as a header on every
mutating action (the `post`/`del` fetch helpers, plus the task-submit and
open-terminal calls that build their own `fetch()`), i.e. every "user presses
a button" action that hits the API.

## Trace propagation, hop by hop

1. **Browser → librarian.** `board.html` mints a fresh `traceparent` per
   click and sends it as a request header.
2. **Librarian → daemon.** `librarian/src/server.rs`'s `handle_with_trace`
   extracts the incoming header, starts a span, and forwards its own
   `traceparent` to the daemon as a header on the proxied call.
3. **Daemon HTTP → scheduler.** `daemon/src/server.rs`'s `route_with_trace`
   extracts the header and starts an HTTP span. Because a `POST /api/runs`
   only *submits* work — the scheduler claims and executes it later,
   asynchronously, on a different thread, well after the HTTP response has
   already been sent — the daemon persists the request's `traceparent` onto
   the new run (`runs.trace_context` in SQLite; see
   `Store::set_run_trace_context`/`run_trace_context`). This is exactly what
   the W3C `traceparent` string is for: continuing a trace across a boundary
   where you can't just pass a live object.
4. **Scheduler → runner subprocess.** `daemon/src/scheduler.rs` reads the
   run's stored trace context and builds spans for run-claim, worktree
   placeholder resolution (`scheduler.resolve_worktrees`, RAL-100 — see
   `daemon/src/worktrees.rs`), session execution, and verify execution
   (mirroring the existing `scheduler`/`runner`/`verify` log-event lifecycle
   points from AGENTS.md's Logging Policy) — each rebuilt from the
   traceparent *string*, not a shared `Context` object, since sessions run on
   separate OS threads.
   `daemon/src/runner.rs` puts its own span's `traceparent` on the
   `SessionSpec` JSON sent to the runner on stdin (`trace_context` field,
   `cli/src/ralphus/runner/spec.py`) — a new, optional field on that existing,
   tested wire contract, so it never becomes required and old callers/tests
   are unaffected.
5. **Runner → model backend.** `ralphus/runner/__main__.py` attaches the
   incoming trace context as the ambient OTel context for the whole
   `run_session()` call; `execute.py` and `pydantic_backend.py` each start a
   nested span around their existing `llm`/`llm-invoke` log points, so the
   Python side's spans land as children of the Rust `runner.subprocess` span
   without needing any request the daemon side gets to make.

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
scheduler claiming and running the session, and (for a `prompt` session) the
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
- **Never a console exporter.** Every exporter here (Rust's hand-rolled
  `ureq`-based one, Python's `OTLPSpanExporter`) writes only to its own HTTP
  connection to the collector — never to stdout (the daemon↔runner JSON
  contract) or stderr (the `ralphus [TYPE] ...` log format /
  `RALPHUS_EVENT:` marker). A `ConsoleSpanExporter` must never be introduced
  on any of these paths.
