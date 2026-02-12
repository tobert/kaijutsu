//! OpenTelemetry integration for kaijutsu.
//!
//! Provides OTel tracing layer setup, W3C Trace Context propagation for
//! distributed tracing across the Cap'n Proto SSH boundary, and a custom
//! sampler with differentiated rates by span category.
//!
//! # Feature flag
//!
//! All OTel dependencies are behind the `telemetry` feature. Without it,
//! the public API compiles to no-ops — `inject_trace_context` returns empty
//! strings and `extract_trace_context` returns a detached span.
//!
//! # Activation
//!
//! OTel export activates when standard OTel environment variables are set:
//!
//! ```bash
//! # Minimal — enables OTLP export to localhost:4317
//! OTEL_EXPORTER_OTLP_ENDPOINT=http://localhost:4317 cargo run -p kaijutsu-server --features telemetry
//!
//! # Full control
//! OTEL_SERVICE_NAME=kaijutsu-server \
//! OTEL_EXPORTER_OTLP_ENDPOINT=http://jaeger:4317 \
//! OTEL_TRACES_EXPORTER=otlp \
//! cargo run -p kaijutsu-server --features telemetry
//! ```
//!
//! Set `OTEL_SDK_DISABLED=true` to explicitly disable even when the endpoint is set.

#[cfg(feature = "telemetry")]
mod otel;

#[cfg(feature = "telemetry")]
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
/// SSH boundary. Without the `telemetry` feature, returns empty strings.
pub fn inject_trace_context() -> (String, String) {
    #[cfg(feature = "telemetry")]
    {
        otel::inject_trace_context_impl()
    }
    #[cfg(not(feature = "telemetry"))]
    {
        (String::new(), String::new())
    }
}

/// Extract W3C Trace Context and create a child span linked to the remote parent.
///
/// Without the `telemetry` feature, returns a disabled span (no-op).
pub fn extract_trace_context(traceparent: &str, tracestate: &str) -> tracing::Span {
    #[cfg(feature = "telemetry")]
    {
        otel::extract_trace_context_impl(traceparent, tracestate)
    }
    #[cfg(not(feature = "telemetry"))]
    {
        let _ = (traceparent, tracestate);
        tracing::Span::none()
    }
}
