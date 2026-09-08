# OpenTelemetry tracing (RAL-96)

Cross-cutting across `daemon/`, `runner/`, and `librarian/assets` (the board) —
kept here rather than in one crate's `AGENTS.md`.

A user action (a board button click) → librarian → daemon → scheduler
→ `ralphus-runner` subprocess is traced end-to-end as one OpenTelemetry
trace, viewable as a flame/waterfall graph. Entirely opt-in — every exporter
is a no-op unless `OTEL_EXPORTER_OTLP_ENDPOINT` is set, so the default dev
loop is unaffected. Rust spans (every hop above, including the runner) use
the `opentelemetry`/`opentelemetry_sdk` crates' manual span API directly (not
the `tracing` crate — see [[logging-policy]]); the browser hand-rolls
the W3C `traceparent` format (no `opentelemetry-js`, since the board has
no build step). A local Collector + Jaeger stack for viewing traces lives in
`otel/docker-compose.yml`. See [`docs/otel-tracing.md`](../docs/otel-tracing.md)
for the full design and how to run it.
