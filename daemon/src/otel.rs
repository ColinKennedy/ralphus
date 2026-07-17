//! OpenTelemetry tracing (RAL-96) — manual span API only.
//!
//! RAL-79 bans the `tracing` crate (and therefore `tracing-opentelemetry`)
//! workspace-wide: stdout is reserved for the daemon<->runner JSON contract and
//! `tracing`'s ecosystem defaults too easily leak onto it. Every span here is
//! created directly through `opentelemetry::global::tracer(...)` /
//! `start_with_context`, per RAL-96's acceptance criteria — never `tracing`.
//!
//! `opentelemetry-otlp` (the official exporter crate) is deliberately NOT a
//! dependency: its HTTP transport pulls `reqwest` -> `tokio` -> the `tracing`
//! crate transitively, which would violate RAL-79 despite never being called
//! directly (verified against `Cargo.lock`). Instead, [`UreqOtlpJsonExporter`]
//! below hand-rolls the OTLP/JSON wire format over the already-used
//! synchronous `ureq` client, so export traffic goes out over its own request
//! with zero risk of touching stdout/stderr.
//!
//! Exporting is entirely opt-in: [`init`] only installs a real exporter when
//! `OTEL_EXPORTER_OTLP_ENDPOINT` is set in the environment. With no endpoint
//! configured, `opentelemetry::global::tracer(...)` returns the built-in no-op
//! tracer and this module makes zero network calls — so the default dev loop
//! (`scripts/build-debug.sh`) is unaffected unless a collector is explicitly
//! configured (see `docs/otel-tracing.md`).

use std::collections::HashMap;
use std::time::SystemTime;

use opentelemetry::propagation::TextMapPropagator;
use opentelemetry::trace::{SpanKind, Status, TraceContextExt as _, Tracer as _};
use opentelemetry::{Context, KeyValue, Value, global};
use opentelemetry_sdk::Resource;
use opentelemetry_sdk::error::{OTelSdkError, OTelSdkResult};
use opentelemetry_sdk::propagation::TraceContextPropagator;
use opentelemetry_sdk::trace::{SdkTracerProvider, SpanData, SpanExporter};

/// The tracer name every daemon span is created under, and the OTLP
/// instrumentation scope name spans are reported with.
const TRACER_NAME: &str = "ralphus-daemon";

/// Standard OTel env var naming the collector's OTLP/HTTP endpoint, e.g.
/// `http://127.0.0.1:4318`. Traces are only exported when this is set.
const OTEL_ENDPOINT_VAR: &str = "OTEL_EXPORTER_OTLP_ENDPOINT";

/// Install a global OTLP-exporting tracer provider and W3C propagator when
/// `OTEL_EXPORTER_OTLP_ENDPOINT` is set. A no-op otherwise, so callers can
/// unconditionally call this at startup.
///
/// Returns the provider so the caller can [`shutdown`] it before exit to
/// flush any buffered spans; `None` when tracing was not enabled.
pub fn init(service_name: &'static str) -> Option<SdkTracerProvider> {
    let endpoint = std::env::var(OTEL_ENDPOINT_VAR).ok()?;
    global::set_text_map_propagator(TraceContextPropagator::new());
    let resource = Resource::builder().with_service_name(service_name).build();
    let traces_url = format!("{}/v1/traces", endpoint.trim_end_matches('/'));
    let exporter = UreqOtlpJsonExporter::new(traces_url, &resource);
    let provider = SdkTracerProvider::builder()
        .with_simple_exporter(exporter)
        .build();
    global::set_tracer_provider(provider.clone());
    crate::rlog!(INFO, "ralphus [otel] exporting traces to {endpoint}");
    Some(provider)
}

/// Flush and shut down a tracer provider returned by [`init`]. Best-effort —
/// shutdown errors are logged through the daemon's own sink, never stdout.
pub fn shutdown(provider: Option<SdkTracerProvider>) {
    if let Some(provider) = provider {
        if let Err(e) = provider.shutdown() {
            crate::rlog!(WARNING, "ralphus [otel] shutdown: {e}");
        }
    }
}

/// The daemon's tracer, fetched through the global API per RAL-96's
/// acceptance criteria (`opentelemetry::global::tracer(...)`). Returns a no-op
/// tracer when [`init`] was never called or found no endpoint configured.
fn tracer() -> global::BoxedTracer {
    global::tracer(TRACER_NAME)
}

/// A started span's context. Wraps the `Context` carrying the span so a
/// callee can start its own child (`start_span(name, &active.cx, ...)`) or
/// propagate `active.cx` across a thread/process boundary via
/// [`traceparent_from_context`]. Ends the span on drop — so every call site
/// gets `.end()` for free, even on an early `return`.
#[must_use = "dropping this immediately ends the span"]
pub struct ActiveSpan {
    /// The context carrying this span — pass to children or propagate onward.
    pub cx: Context,
}

impl ActiveSpan {
    /// Record the span's outcome. Call before drop when the caller knows
    /// success/failure; spans left unmarked default to `Status::Unset`.
    pub fn set_status(&self, status: Status) {
        self.cx.span().set_status(status);
    }

    /// Attach an attribute (e.g. `run_id`, `session_id`) to the span.
    pub fn set_attribute(&self, key: &'static str, value: impl Into<Value>) {
        self.cx.span().set_attribute(KeyValue::new(key, value));
    }
}

impl Drop for ActiveSpan {
    fn drop(&mut self) {
        self.cx.span().end();
    }
}

/// Start a span named `name` as a child of `parent`, using the daemon's
/// tracer via the global manual-span API (`opentelemetry::global::tracer(...)`
/// `.start_with_context(...)`, per RAL-96 — never the `tracing` crate).
///
/// No `#[must_use]` here: the returned [`ActiveSpan`] already carries one.
pub fn start_span(name: &'static str, parent: &Context, kind: SpanKind) -> ActiveSpan {
    let t = tracer();
    let span = t
        .span_builder(name)
        .with_kind(kind)
        .start_with_context(&t, parent);
    ActiveSpan {
        cx: parent.with_span(span),
    }
}

/// Build a [`Context`] from an incoming W3C `traceparent` header value (see
/// <https://www.w3.org/TR/trace-context/>). Returns a fresh (root) context
/// when `traceparent` is `None` or malformed — a broken header degrades to
/// "start a new trace" rather than an error.
#[must_use]
pub fn context_from_traceparent(traceparent: Option<&str>) -> Context {
    let Some(tp) = traceparent else {
        return Context::new();
    };
    let mut carrier = HashMap::new();
    carrier.insert("traceparent".to_string(), tp.to_string());
    TraceContextPropagator::new().extract_with_context(&Context::new(), &carrier)
}

/// Serialize `cx`'s span context back into a W3C `traceparent` header value,
/// for forwarding across the next hop (subprocess spawn, outbound HTTP call).
/// `None` when `cx` carries no valid span context.
#[must_use]
pub fn traceparent_from_context(cx: &Context) -> Option<String> {
    let mut carrier = HashMap::new();
    TraceContextPropagator::new().inject_context(cx, &mut carrier);
    carrier.remove("traceparent")
}

// ── OTLP/JSON export over `ureq` (no `opentelemetry-otlp`, see module docs) ──

/// Exports spans as OTLP/JSON (the collector's `/v1/traces` HTTP+JSON
/// endpoint) using `ureq` — synchronous, no tokio, no `tracing` crate in the
/// dependency graph. `trace_id`/`span_id` are hex strings per the OTLP/JSON
/// wire format's special-cased ID encoding (unlike ordinary protobuf `bytes`
/// fields, which use base64).
#[derive(Debug)]
struct UreqOtlpJsonExporter {
    traces_url: String,
    resource_attributes: Vec<serde_json::Value>,
}

impl UreqOtlpJsonExporter {
    fn new(traces_url: String, resource: &Resource) -> Self {
        let resource_attributes = resource
            .iter()
            .map(|(k, v)| attribute_json(k.as_str(), v))
            .collect();
        Self {
            traces_url,
            resource_attributes,
        }
    }
}

impl SpanExporter for UreqOtlpJsonExporter {
    async fn export(&self, batch: Vec<SpanData>) -> OTelSdkResult {
        if batch.is_empty() {
            return Ok(());
        }
        let spans: Vec<serde_json::Value> = batch.iter().map(span_json).collect();
        let body = serde_json::json!({
            "resourceSpans": [{
                "resource": { "attributes": self.resource_attributes },
                "scopeSpans": [{
                    "scope": { "name": TRACER_NAME },
                    "spans": spans,
                }],
            }],
        });
        ureq::post(&self.traces_url)
            .set("Content-Type", "application/json")
            .send_string(&body.to_string())
            .map(|_| ())
            .map_err(|e| OTelSdkError::InternalFailure(e.to_string()))
    }
}

fn span_json(s: &SpanData) -> serde_json::Value {
    serde_json::json!({
        "traceId": s.span_context.trace_id().to_string(),
        "spanId": s.span_context.span_id().to_string(),
        "parentSpanId": s.parent_span_id.to_string(),
        "name": s.name.as_ref(),
        "kind": span_kind_code(&s.span_kind),
        "startTimeUnixNano": unix_nanos(s.start_time),
        "endTimeUnixNano": unix_nanos(s.end_time),
        "attributes": s.attributes.iter().map(|kv| attribute_json(kv.key.as_str(), &kv.value)).collect::<Vec<_>>(),
        "status": status_json(&s.status),
    })
}

fn unix_nanos(t: SystemTime) -> String {
    t.duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
        .to_string()
}

fn span_kind_code(kind: &SpanKind) -> i32 {
    match kind {
        SpanKind::Internal => 1,
        SpanKind::Server => 2,
        SpanKind::Client => 3,
        SpanKind::Producer => 4,
        SpanKind::Consumer => 5,
    }
}

fn status_json(status: &Status) -> serde_json::Value {
    match status {
        Status::Unset => serde_json::json!({ "code": 0 }),
        Status::Ok => serde_json::json!({ "code": 1 }),
        Status::Error { description } => {
            serde_json::json!({ "code": 2, "message": description.as_ref() })
        }
    }
}

fn attribute_json(key: &str, value: &Value) -> serde_json::Value {
    let any = match value {
        Value::Bool(b) => serde_json::json!({ "boolValue": b }),
        Value::I64(i) => serde_json::json!({ "intValue": i.to_string() }),
        Value::F64(f) => serde_json::json!({ "doubleValue": f }),
        Value::String(s) => serde_json::json!({ "stringValue": s.as_ref() }),
        other => serde_json::json!({ "stringValue": format!("{other:?}") }),
    };
    serde_json::json!({ "key": key, "value": any })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn traceparent_roundtrips_through_context() {
        let header = "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01";
        let cx = context_from_traceparent(Some(header));
        assert!(cx.has_active_span());
        let out = traceparent_from_context(&cx).expect("traceparent");
        assert!(out.starts_with("00-4bf92f3577b34da6a3ce929d0e0e4736-"));
    }

    #[test]
    fn missing_traceparent_yields_context_with_no_span() {
        let cx = context_from_traceparent(None);
        assert!(!cx.has_active_span());
        assert_eq!(traceparent_from_context(&cx), None);
    }

    #[test]
    fn malformed_traceparent_degrades_to_no_span() {
        let cx = context_from_traceparent(Some("not-a-traceparent"));
        assert!(!cx.has_active_span());
    }

    #[test]
    fn start_span_child_shares_trace_id_with_parent() {
        let root = context_from_traceparent(Some(
            "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01",
        ));
        let child = start_span("test.child", &root, SpanKind::Internal);
        let root_tp = traceparent_from_context(&root).unwrap();
        let child_tp = traceparent_from_context(&child.cx).unwrap();
        // Same trace id (the middle hex segment), different span id.
        assert_eq!(
            root_tp.split('-').nth(1),
            child_tp.split('-').nth(1),
            "child span must stay on the parent's trace"
        );
    }

    #[test]
    fn span_json_maps_kind_and_status() {
        assert_eq!(span_kind_code(&SpanKind::Server), 2);
        assert_eq!(span_kind_code(&SpanKind::Client), 3);
        assert_eq!(status_json(&Status::Ok)["code"], 1);
        assert_eq!(status_json(&Status::Unset)["code"], 0);
        let err = status_json(&Status::error("boom"));
        assert_eq!(err["code"], 2);
        assert_eq!(err["message"], "boom");
    }

    #[test]
    fn attribute_json_encodes_common_types() {
        assert_eq!(
            attribute_json("k", &Value::from("v"))["value"]["stringValue"],
            "v"
        );
        assert_eq!(
            attribute_json("k", &Value::from(42_i64))["value"]["intValue"],
            "42"
        );
        assert_eq!(
            attribute_json("k", &Value::from(true))["value"]["boolValue"],
            true
        );
    }
}
