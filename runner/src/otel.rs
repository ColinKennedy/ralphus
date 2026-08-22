//! OpenTelemetry tracing for the runner subprocess. Deliberately a standalone
//! copy of the same hand-rolled OTLP/JSON-over-`ureq` approach as
//! `daemon/src/otel.rs` (and `librarian/src/otel.rs`) rather than a shared
//! dependency on the `ralphus-daemon` crate -- pulling in the whole daemon
//! lib (rusqlite, tiny_http, ...) just to reuse ~100 lines of tracing code
//! would work against this crate's build-weight/binary-size goals.
//! `opentelemetry-otlp` stays out of the dependency tree for the same reason
//! documented in the root `Cargo.toml`: it pulls `reqwest`->`tokio`->`hyper`
//! into a workspace that is synchronous by design.

use std::collections::HashMap;
use std::time::SystemTime;

use opentelemetry::propagation::TextMapPropagator;
use opentelemetry::trace::{SpanKind, Status, TraceContextExt as _, Tracer as _};
use opentelemetry::{Context, KeyValue, Value, global};
use opentelemetry_sdk::Resource;
use opentelemetry_sdk::error::{OTelSdkError, OTelSdkResult};
use opentelemetry_sdk::propagation::TraceContextPropagator;
use opentelemetry_sdk::trace::{SdkTracerProvider, SpanData, SpanExporter};

const TRACER_NAME: &str = "ralphus-runner";
const OTEL_ENDPOINT_VAR: &str = "OTEL_EXPORTER_OTLP_ENDPOINT";

/// Installs a global OTLP-exporting tracer provider when
/// `OTEL_EXPORTER_OTLP_ENDPOINT` is set; a no-op otherwise -- the same gate
/// `daemon/src/otel.rs`/`librarian/src/otel.rs` use, so every process agrees
/// whether tracing is live.
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
    Some(provider)
}

pub fn shutdown(provider: Option<SdkTracerProvider>) {
    if let Some(provider) = provider {
        let _ = provider.shutdown();
    }
}

fn tracer() -> global::BoxedTracer {
    global::tracer(TRACER_NAME)
}

#[must_use = "dropping this immediately ends the span"]
pub struct ActiveSpan {
    pub cx: Context,
}

impl ActiveSpan {
    pub fn set_status(&self, status: Status) {
        self.cx.span().set_status(status);
    }

    pub fn set_attribute(&self, key: &'static str, value: impl Into<Value>) {
        self.cx.span().set_attribute(KeyValue::new(key, value));
    }
}

impl Drop for ActiveSpan {
    fn drop(&mut self) {
        self.cx.span().end();
    }
}

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

/// Builds a [`Context`] from an incoming W3C `traceparent` (the daemon's
/// `RunnerSpec::trace_context`), so this process's spans nest under the
/// daemon's trace instead of starting a disconnected one. A missing/malformed
/// header degrades to a fresh root context.
#[must_use]
pub fn context_from_traceparent(traceparent: Option<&str>) -> Context {
    let Some(tp) = traceparent else {
        return Context::new();
    };
    let mut carrier = HashMap::new();
    carrier.insert("traceparent".to_string(), tp.to_string());
    TraceContextPropagator::new().extract_with_context(&Context::new(), &carrier)
}

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
    fn missing_traceparent_yields_root_context() {
        let cx = context_from_traceparent(None);
        assert!(!cx.has_active_span());
    }

    #[test]
    fn malformed_traceparent_degrades_to_no_span() {
        let cx = context_from_traceparent(Some("garbage"));
        assert!(!cx.has_active_span());
    }

    #[test]
    fn valid_traceparent_yields_active_span() {
        let cx = context_from_traceparent(Some(
            "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01",
        ));
        assert!(cx.has_active_span());
    }
}
