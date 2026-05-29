//! OTel internals — tracing layer, W3C propagation, and sampling.

use std::collections::HashMap;

use opentelemetry::trace::{
    Link, SamplingDecision, SamplingResult, SpanContext, SpanId, SpanKind, TraceContextExt,
    TraceFlags, TraceId, TraceState, TracerProvider as _,
};
use opentelemetry::{Context, KeyValue, global};
use opentelemetry_otlp::{MetricExporter, SpanExporter};
use opentelemetry_sdk::Resource;
use opentelemetry_sdk::metrics::SdkMeterProvider;
use opentelemetry_sdk::trace::{Sampler, SdkTracerProvider, ShouldSample, SpanLimits};
use tracing_opentelemetry::OpenTelemetryLayer;

/// Guard that shuts down the OTel tracer provider on drop, flushing pending spans.
/// Also keeps the Tokio runtime alive when one was created for tonic channel setup
/// (e.g. in the Bevy app which doesn't have its own Tokio runtime at init time).
pub struct OtelGuard {
    provider: SdkTracerProvider,
    meter_provider: SdkMeterProvider,
    // Order matters: enter guard must drop before runtime
    _runtime_enter: Option<tokio::runtime::EnterGuard<'static>>,
    _runtime: Option<&'static tokio::runtime::Runtime>,
}

impl Drop for OtelGuard {
    fn drop(&mut self) {
        // Flush metrics before traces so a final periodic export lands.
        if let Err(e) = self.meter_provider.shutdown() {
            eprintln!("OTel meter shutdown error: {e}");
        }
        if let Err(e) = self.provider.shutdown() {
            eprintln!("OTel shutdown error: {e}");
        }
    }
}

/// Build an OpenTelemetry tracing layer and guard.
///
/// The layer plugs into `tracing_subscriber::registry()`. The guard must be
/// held alive for the lifetime of the application to ensure spans are flushed.
pub fn otel_layer<S>(
    service_name: &str,
) -> (
    OpenTelemetryLayer<S, opentelemetry_sdk::trace::SdkTracer>,
    OtelGuard,
)
where
    S: tracing::Subscriber + for<'span> tracing_subscriber::registry::LookupSpan<'span>,
{
    // Tonic needs a Tokio runtime for channel setup and ongoing gRPC exports.
    // The server already has one, but the Bevy app doesn't when main() starts.
    // Create a dedicated runtime, leak it (lives for the process), and enter it
    // so that BatchSpanProcessor's tokio::spawn calls succeed.
    let (span_exporter, metric_exporter, runtime_ref, enter_guard) =
        match tokio::runtime::Handle::try_current() {
            Ok(_handle) => {
                let span = SpanExporter::builder()
                    .with_tonic()
                    .build()
                    .expect("failed to build OTLP span exporter");
                let metric = MetricExporter::builder()
                    .with_tonic()
                    .build()
                    .expect("failed to build OTLP metric exporter");
                (span, metric, None, None)
            }
            Err(_) => {
                let rt: &'static tokio::runtime::Runtime = Box::leak(Box::new(
                    tokio::runtime::Runtime::new().expect("failed to create OTel tokio runtime"),
                ));
                let guard = rt.enter();
                let (span, metric) = rt.block_on(async {
                    let span = SpanExporter::builder()
                        .with_tonic()
                        .build()
                        .expect("failed to build OTLP span exporter");
                    let metric = MetricExporter::builder()
                        .with_tonic()
                        .build()
                        .expect("failed to build OTLP metric exporter");
                    (span, metric)
                });
                (span, metric, Some(rt), Some(guard))
            }
        };

    let resource = Resource::builder()
        .with_service_name(service_name.to_string())
        .build();

    let provider = SdkTracerProvider::builder()
        .with_batch_exporter(span_exporter)
        .with_sampler(KaijutsuSampler)
        .with_resource(resource.clone())
        .with_span_limits(SpanLimits::default())
        .build();

    global::set_tracer_provider(provider.clone());

    // Metrics export through the global meter provider on a periodic reader.
    // Unlike traces, metrics need no tracing-subscriber layer — instruments
    // record straight to the global provider (see `crate::metrics`).
    let meter_provider = SdkMeterProvider::builder()
        .with_periodic_exporter(metric_exporter)
        .with_resource(resource)
        .build();

    global::set_meter_provider(meter_provider.clone());

    let tracer = provider.tracer(service_name.to_string());
    let layer = tracing_opentelemetry::layer().with_tracer(tracer);

    (
        layer,
        OtelGuard {
            provider,
            meter_provider,
            _runtime_enter: enter_guard,
            _runtime: runtime_ref,
        },
    )
}

// ============================================================================
// W3C Trace Context propagation
// ============================================================================

/// Inject the current span's trace context as W3C `traceparent` + `tracestate`.
pub(crate) fn inject_trace_context_impl() -> (String, String) {
    use opentelemetry::propagation::TextMapPropagator;
    use opentelemetry_sdk::propagation::TraceContextPropagator;

    let cx = Context::current();
    let propagator = TraceContextPropagator::new();

    let mut carrier = HashMap::new();
    propagator.inject_context(&cx, &mut carrier);

    let traceparent = carrier.remove("traceparent").unwrap_or_default();
    let tracestate = carrier.remove("tracestate").unwrap_or_default();
    (traceparent, tracestate)
}

/// Extract a remote trace context and return a tracing span linked to it.
pub(crate) fn extract_trace_context_impl(traceparent: &str, tracestate: &str) -> tracing::Span {
    use opentelemetry::propagation::TextMapPropagator;
    use opentelemetry_sdk::propagation::TraceContextPropagator;
    use tracing_opentelemetry::OpenTelemetrySpanExt;

    if traceparent.is_empty() {
        return tracing::info_span!("rpc.request");
    }

    let mut carrier = HashMap::new();
    carrier.insert("traceparent".to_string(), traceparent.to_string());
    if !tracestate.is_empty() {
        carrier.insert("tracestate".to_string(), tracestate.to_string());
    }

    let propagator = TraceContextPropagator::new();
    let cx = propagator.extract(&carrier);

    // Return a tracing span linked to the extracted OTel context
    let span = tracing::info_span!("rpc.request");
    let _ = span.set_parent(cx);
    span
}

// ============================================================================
// Per-context long-running trace
// ============================================================================

/// Create a span under a context's long-running trace.
///
/// Synthesizes a remote parent with the given trace ID so that every RPC
/// touching a context shares a single trace. Returns a detached span if
/// `trace_id` is all zeros.
pub(crate) fn context_root_span_impl(trace_id: &[u8; 16], name: &'static str) -> tracing::Span {
    use tracing_opentelemetry::OpenTelemetrySpanExt;

    if *trace_id == [0u8; 16] {
        return tracing::Span::none();
    }

    let otel_trace_id = TraceId::from_bytes(*trace_id);
    // Derive a stable span ID from the first 8 bytes of the trace ID
    let root_span_id = SpanId::from_bytes(trace_id[0..8].try_into().unwrap());
    let span_context = SpanContext::new(
        otel_trace_id,
        root_span_id,
        TraceFlags::SAMPLED,
        true, // remote
        TraceState::default(),
    );
    let cx = Context::current().with_remote_span_context(span_context);
    let span = tracing::info_span!("context", method = name);
    let _ = span.set_parent(cx);
    span
}

// ============================================================================
// KaijutsuSampler — differentiated sampling by span category
// ============================================================================

/// Custom sampler with differentiated rates by span name prefix.
///
/// | Prefix       | Rate | Rationale                               |
/// |--------------|------|-----------------------------------------|
/// | `gen_ai.*`   | 100% | Expensive LLM calls, highest value      |
/// | `llm.*`      | 100% | Kaijutsu-level LLM spans                |
/// | `engine.*`   | 100% | Tool execution, critical for debugging   |
/// | `tool.*`     | 100% | Tool dispatch                            |
/// | `drift.*`    | 100% | Cross-context operations                 |
/// | `rpc.*`      | 10%  | High volume Cap'n Proto calls            |
/// | `sync.*`     |  1%  | Very high volume CRDT ops                |
/// | errors       | 100% | Always capture failures                  |
/// | other        | 10%  | Default for unclassified spans           |
#[derive(Debug, Clone)]
struct KaijutsuSampler;

impl ShouldSample for KaijutsuSampler {
    fn should_sample(
        &self,
        parent_context: Option<&Context>,
        trace_id: TraceId,
        name: &str,
        span_kind: &SpanKind,
        attributes: &[KeyValue],
        links: &[Link],
    ) -> SamplingResult {
        // If parent is sampled, always sample (maintain trace continuity)
        if let Some(cx) = parent_context {
            let parent_span = cx.span();
            let parent_ctx = parent_span.span_context();
            if parent_ctx.is_sampled() {
                return SamplingResult {
                    decision: SamplingDecision::RecordAndSample,
                    attributes: vec![],
                    trace_state: parent_ctx.trace_state().clone(),
                };
            }
        }

        // Check for error attributes — always sample errors
        let is_error = attributes.iter().any(|kv| {
            (kv.key.as_str() == "otel.status_code" && kv.value.as_str() == "ERROR")
                || (kv.key.as_str() == "error" && kv.value.as_str() == "true")
        });

        if is_error {
            return SamplingResult {
                decision: SamplingDecision::RecordAndSample,
                attributes: vec![],
                trace_state: TraceState::default(),
            };
        }

        // Delegate to trace-id ratio sampler for deterministic decisions
        Sampler::TraceIdRatioBased(sampling_rate(name)).should_sample(
            parent_context,
            trace_id,
            name,
            span_kind,
            attributes,
            links,
        )
    }
}

/// Sampling rate for a span, selected by its name.
///
/// The high-value namespaces are **dot-qualified** (`drift.`, `engine.`, …) so
/// that RPC/actor method spans never collide with them. The actor layer
/// auto-names spans from the method (`drift_queue`, `drift_push`, …); without
/// the dot, `starts_with("drift")` swept those into the 100% bucket and the
/// app's 5s idle drift poll was fully sampled — ~10x its sibling
/// `list_contexts`. The `rpc` family stays a bare prefix on purpose so it
/// covers `rpc`, `rpc.request`, and `rpc_client.*` alike.
fn sampling_rate(name: &str) -> f64 {
    if name.starts_with("gen_ai.")
        || name.starts_with("llm.")
        || name.starts_with("engine.")
        || name.starts_with("tool.")
        || name.starts_with("drift.")
    {
        1.0 // 100% — high-value, low-volume namespaces
    } else if name.starts_with("rpc") {
        0.1 // 10% — rpc, rpc.request, rpc_client.* (high-volume Cap'n Proto)
    } else if name.starts_with("sync") {
        0.01 // 1% — very high-volume CRDT ops
    } else {
        0.1 // 10% default
    }
}

#[cfg(test)]
mod tests {
    use super::sampling_rate;

    /// Regression: the auto-named actor/method span `drift_queue` (fired every
    /// 5s by the app's idle drift poll) must be sampled at the default rate,
    /// NOT mistaken for a `drift.{op}` engine span. This is the prefix
    /// collision that made an idle kernel look busy.
    #[test]
    fn method_spans_do_not_collide_with_engine_namespaces() {
        assert_eq!(sampling_rate("drift_queue"), 0.1);
        assert_eq!(sampling_rate("drift_push"), 0.1);
        assert_eq!(sampling_rate("drift_flush"), 0.1);
    }

    /// The dotted engine-style namespaces still sample at 100%.
    #[test]
    fn engine_style_namespaces_sample_full() {
        assert_eq!(sampling_rate("drift.push"), 1.0);
        assert_eq!(sampling_rate("drift.register"), 1.0);
        assert_eq!(sampling_rate("engine.git"), 1.0);
        assert_eq!(sampling_rate("engine.read"), 1.0);
        assert_eq!(sampling_rate("tool.dispatch"), 1.0);
        assert_eq!(sampling_rate("gen_ai.chat"), 1.0);
        assert_eq!(sampling_rate("llm.prompt"), 1.0);
    }

    /// The rpc family — bare `rpc`, `rpc.request`, `rpc_client.*` — and other
    /// unclassified method spans sample at 10%.
    #[test]
    fn rpc_family_and_methods_sampled_low() {
        assert_eq!(sampling_rate("rpc"), 0.1);
        assert_eq!(sampling_rate("rpc.request"), 0.1);
        assert_eq!(sampling_rate("rpc_client.drift_queue"), 0.1);
        assert_eq!(sampling_rate("list_contexts"), 0.1);
    }

    #[test]
    fn sync_sampled_lowest() {
        assert_eq!(sampling_rate("sync.push_ops"), 0.01);
    }
}
