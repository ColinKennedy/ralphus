"""OpenTelemetry tracing (RAL-96) — mirrors `daemon/src/otel.rs`'s design.

Exporting is opt-in: :func:`init` only installs a real exporter when
``OTEL_EXPORTER_OTLP_ENDPOINT`` is set, matching the Rust side so both
processes agree on when tracing is actually live. Unlike the Rust side (which
hand-rolls its own OTLP/JSON exporter to keep the banned `tracing` crate out
of the dependency graph — a constraint that is Rust-specific, see RAL-79),
the Python runner uses the official ``opentelemetry-sdk`` and
``opentelemetry-exporter-otlp-proto-http`` packages directly.

Spans are exported over their own HTTP request via the OTLP exporter — never
a ``ConsoleSpanExporter`` — so nothing is written to stdout (reserved for the
``SessionSpec``/``SessionResult`` JSON contract) or stderr (reserved for the
``ralphus [TYPE] ...`` log format and the ``RALPHUS_EVENT:`` marker).

Context propagation uses the SDK's ordinary ambient-context mechanism
(:func:`attach_trace_context` + ``tracer.start_as_current_span``) rather than
threading an explicit ``Context`` object through every call, unlike the Rust
side — the runner is single-threaded per subprocess, so Python's contextvar-
based propagation is both idiomatic and safe here.
"""

from __future__ import annotations

import os
import sys
from collections.abc import Iterator
from contextlib import contextmanager

from opentelemetry import context as otel_context
from opentelemetry import trace
from opentelemetry.sdk.resources import Resource
from opentelemetry.sdk.trace import TracerProvider
from opentelemetry.sdk.trace.export import BatchSpanProcessor
from opentelemetry.trace import Span, SpanKind, Status, StatusCode
from opentelemetry.trace.propagation.tracecontext import TraceContextTextMapPropagator

__all__ = ["attach_trace_context", "init", "mark_error", "mark_ok", "start_span"]

_OTEL_ENDPOINT_VAR = "OTEL_EXPORTER_OTLP_ENDPOINT"
_TRACER_NAME = "ralphus-runner"

# `get_tracer` returns a proxy when called before a real provider is
# installed; it transparently upgrades once `init()` calls
# `set_tracer_provider`, so this module-load-time call is safe even though
# `init()` (if called at all) always runs after this module is imported.
_tracer = trace.get_tracer(_TRACER_NAME)


def init(service_name: str) -> None:
    """Install a global OTLP-exporting tracer provider when
    ``OTEL_EXPORTER_OTLP_ENDPOINT`` is set. A no-op otherwise, so callers can
    unconditionally call this at startup — mirrors `daemon/src/otel.rs::init`.
    """
    endpoint = os.environ.get(_OTEL_ENDPOINT_VAR)
    if not endpoint:
        return
    from opentelemetry.exporter.otlp.proto.http.trace_exporter import OTLPSpanExporter

    provider = TracerProvider(resource=Resource.create({"service.name": service_name}))
    exporter = OTLPSpanExporter(endpoint=f"{endpoint.rstrip('/')}/v1/traces")
    provider.add_span_processor(BatchSpanProcessor(exporter))
    trace.set_tracer_provider(provider)
    print(f"ralphus [otel] exporting traces to {endpoint}", file=sys.stderr)


@contextmanager
def attach_trace_context(traceparent: str | None) -> Iterator[None]:
    """Make ``traceparent`` (the daemon's W3C header, from
    ``SessionSpec.trace_context``) the ambient OTel context for the duration
    of the block, so every :func:`start_span` call inside nests under it
    automatically. A no-op context manager when ``traceparent`` is ``None`` —
    the next `start_span` then just starts a fresh (root) trace.
    """
    if traceparent is None:
        yield
        return
    carrier = {"traceparent": traceparent}
    ctx = TraceContextTextMapPropagator().extract(carrier)
    token = otel_context.attach(ctx)
    try:
        yield
    finally:
        otel_context.detach(token)


@contextmanager
def start_span(name: str, kind: SpanKind = SpanKind.INTERNAL) -> Iterator[Span]:
    """Start a span nested under the ambient context (see
    :func:`attach_trace_context`) via the manual span API
    (``opentelemetry.trace.get_tracer(...).start_as_current_span(...)``),
    same API family as the Rust side (RAL-96).
    """
    with _tracer.start_as_current_span(name, kind=kind) as span:
        yield span


def mark_ok(span: Span) -> None:
    """Record a successful outcome on `span`. Spans left unmarked default to
    an unset status."""
    span.set_status(Status(StatusCode.OK))


def mark_error(span: Span, description: str) -> None:
    """Record a failed outcome on `span`."""
    span.set_status(Status(StatusCode.ERROR, description))
