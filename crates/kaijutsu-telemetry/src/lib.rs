//! OpenTelemetry integration for kaijutsu.
//!
//! Provides OTel tracing layer setup, W3C Trace Context propagation for
//! distributed tracing across the Cap'n Proto SSH boundary, and a custom
//! sampler with differentiated rates by span category.
//!
//! # Activation
//!
//! OTel export activates when standard OTel environment variables are set:
//!
//! ```bash
//! # Minimal — enables OTLP export to localhost:4317
//! OTEL_EXPORTER_OTLP_ENDPOINT=http://localhost:4317 cargo run -p kaijutsu-server
//!
//! # Full control
//! OTEL_SERVICE_NAME=kaijutsu-server \
//! OTEL_EXPORTER_OTLP_ENDPOINT=http://jaeger:4317 \
//! OTEL_TRACES_EXPORTER=otlp \
//! cargo run -p kaijutsu-server
//! ```
//!
//! Set `OTEL_SDK_DISABLED=true` to explicitly disable even when the endpoint is set.

mod otel;

pub use otel::{otel_layer, OtelGuard};

/// Check whether OTel export should be enabled.
///
/// Returns `true` when standard OTel env vars indicate export is desired:
/// - `OTEL_SDK_DISABLED` is NOT set to `"true"`
/// - AND at least one of:
///   - `OTEL_EXPORTER_OTLP_ENDPOINT` is set
///   - `OTEL_TRACES_EXPORTER` is set (and not `"none"`)
pub fn otel_enabled() -> bool {
    // Explicit disable takes priority
    if std::env::var("OTEL_SDK_DISABLED")
        .map(|v| v.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
    {
        return false;
    }

    // Check for OTLP endpoint
    if std::env::var("OTEL_EXPORTER_OTLP_ENDPOINT").is_ok() {
        return true;
    }

    // Check for traces exporter (anything other than "none")
    if let Ok(exporter) = std::env::var("OTEL_TRACES_EXPORTER") {
        return !exporter.eq_ignore_ascii_case("none");
    }

    false
}

/// Inject W3C Trace Context from the current tracing span.
///
/// Returns `(traceparent, tracestate)` for propagation across the Cap'n Proto
/// SSH boundary.
pub fn inject_trace_context() -> (String, String) {
    otel::inject_trace_context_impl()
}

/// Extract W3C Trace Context and create a child span linked to the remote parent.
pub fn extract_trace_context(traceparent: &str, tracestate: &str) -> tracing::Span {
    otel::extract_trace_context_impl(traceparent, tracestate)
}

/// Create a span under a long-running context trace.
///
/// Constructs a synthetic remote parent with the given trace ID so that all
/// RPC operations touching a context share a single trace. The span name
/// identifies the specific operation (e.g., "join_context", "push_ops").
///
/// Pass `[0u8; 16]` to get a detached span (no context trace linkage).
pub fn context_root_span(trace_id: &[u8; 16], name: &'static str) -> tracing::Span {
    otel::context_root_span_impl(trace_id, name)
}
