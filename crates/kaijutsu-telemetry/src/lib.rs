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
//! Set `KAIJUTSU_OTEL_FILE_DIR=<dir>` to also (or instead) write traces, metrics,
//! and logs as OTLP JSON lines under `<dir>`. See `docs/telemetry.md`.
//!
//! Set `OTEL_SDK_DISABLED=true` to explicitly disable even when the endpoint is set.

pub mod metrics;
mod file_export;
mod otel;

pub use metrics::{
    TokenCounts, record_beat_fired, record_beat_sync_published, record_clock_offset,
    record_cwd_restore_failed, record_dj_clock_transition, record_dj_cue_dropped,
    record_future_stamp, record_grid_reseed, record_llm_usage, record_metronome_click,
    record_phasor_slew, record_roster_transition, record_stale_cue_dropped,
};
pub use otel::{OtelGuard, otel_layer};

/// Environment variable that turns on OTLP JSON-lines file export.
pub const FILE_DIR_ENV: &str = "KAIJUTSU_OTEL_FILE_DIR";

/// Check whether OTel export should be enabled.
///
/// Returns `true` when `OTEL_SDK_DISABLED` is NOT `"true"` and at least one of:
/// - `OTEL_EXPORTER_OTLP_ENDPOINT` is set (network export)
/// - `OTEL_TRACES_EXPORTER` is set and not `"none"` (network export)
/// - `KAIJUTSU_OTEL_FILE_DIR` is set and not empty (file export)
pub fn otel_enabled() -> bool {
    otel_enabled_from(|name| std::env::var(name).ok())
}

/// Whether network (OTLP gRPC) export is requested, ignoring `OTEL_SDK_DISABLED`.
pub(crate) fn otlp_requested(get: &impl Fn(&str) -> Option<String>) -> bool {
    if get("OTEL_EXPORTER_OTLP_ENDPOINT").is_some() {
        return true;
    }
    get("OTEL_TRACES_EXPORTER").is_some_and(|v| !v.eq_ignore_ascii_case("none"))
}

/// The file export directory, ignoring `OTEL_SDK_DISABLED`.
pub(crate) fn file_dir_requested(
    get: &impl Fn(&str) -> Option<String>,
) -> Option<std::path::PathBuf> {
    get(FILE_DIR_ENV)
        .filter(|v| !v.is_empty())
        .map(std::path::PathBuf::from)
}

pub(crate) fn sdk_disabled(get: &impl Fn(&str) -> Option<String>) -> bool {
    get("OTEL_SDK_DISABLED").is_some_and(|v| v.eq_ignore_ascii_case("true"))
}

fn otel_enabled_from(get: impl Fn(&str) -> Option<String>) -> bool {
    !sdk_disabled(&get) && (otlp_requested(&get) || file_dir_requested(&get).is_some())
}

#[cfg(test)]
mod tests {
    use super::otel_enabled_from;
    use std::collections::HashMap;

    fn enabled(vars: &[(&str, &str)]) -> bool {
        let map: HashMap<String, String> = vars
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        otel_enabled_from(|name| map.get(name).cloned())
    }

    #[test]
    fn nothing_set_is_disabled() {
        assert!(!enabled(&[]));
    }

    #[test]
    fn file_dir_alone_enables() {
        assert!(enabled(&[("KAIJUTSU_OTEL_FILE_DIR", "/tmp/otel")]));
    }

    #[test]
    fn empty_file_dir_does_not_enable() {
        assert!(!enabled(&[("KAIJUTSU_OTEL_FILE_DIR", "")]));
    }

    #[test]
    fn endpoint_alone_still_enables() {
        assert!(enabled(&[("OTEL_EXPORTER_OTLP_ENDPOINT", "http://x:4317")]));
    }

    #[test]
    fn traces_exporter_none_does_not_enable() {
        assert!(!enabled(&[("OTEL_TRACES_EXPORTER", "none")]));
        assert!(enabled(&[("OTEL_TRACES_EXPORTER", "otlp")]));
    }

    #[test]
    fn sdk_disabled_beats_file_dir_and_endpoint() {
        assert!(!enabled(&[
            ("OTEL_SDK_DISABLED", "true"),
            ("KAIJUTSU_OTEL_FILE_DIR", "/tmp/otel"),
        ]));
        assert!(!enabled(&[
            ("OTEL_SDK_DISABLED", "TRUE"),
            ("OTEL_EXPORTER_OTLP_ENDPOINT", "http://x:4317"),
        ]));
    }
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
