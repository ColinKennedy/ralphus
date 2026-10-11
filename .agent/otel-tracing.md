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

## Daemon health signals (RAL-596)

Separate from the per-action traces above, `daemon/src/daemon_health.rs` emits
daemon-wide health spans (`daemon.heartbeat`, `daemon.store_lock.*`,
`daemon.loop.*`, `daemon.tmux.*`, `daemon.resource.low_disk`,
`daemon.startup`/`startup_failed`/`panic`). Per-review, per-squad and per-cell
failures stay out of OTel, and there is no Cartographer→OTel bridge. Callers
only `try_send` onto a bounded channel drained by one emitter thread (the
exporter is a blocking HTTP call, so it must never run on the store-lock
path); a full queue drops. `emit` is a no-op unless OTel is enabled. Table of
spans, thresholds and the 15-minute disk/tmux probes:
[`docs/otel-tracing.md`](../docs/otel-tracing.md).
