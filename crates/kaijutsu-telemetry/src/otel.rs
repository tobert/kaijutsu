//! OTel internals — tracing layer, W3C propagation, and sampling.

use std::collections::HashMap;

use opentelemetry::trace::{
    Link, SamplingDecision, SamplingResult, SpanContext, SpanId, SpanKind, TraceContextExt,
    TraceFlags, TracerProvider as _, TraceId, TraceState,
};
use opentelemetry::{global, Context, KeyValue};
use opentelemetry_otlp::SpanExporter;
use opentelemetry_sdk::trace::{SdkTracerProvider, Sampler, ShouldSample, SpanLimits};
use opentelemetry_sdk::Resource;
use tracing_opentelemetry::OpenTelemetryLayer;

/// Guard that shuts down the OTel tracer provider on drop, flushing pending spans.
/// Also keeps the Tokio runtime alive when one was created for tonic channel setup
/// (e.g. in the Bevy app which doesn't have its own Tokio runtime at init time).
pub struct OtelGuard {
    provider: SdkTracerProvider,
    // Order matters: enter guard must drop before runtime
    _runtime_enter: Option<tokio::runtime::EnterGuard<'static>>,
    _runtime: Option<&'static tokio::runtime::Runtime>,
}

impl Drop for OtelGuard {
    fn drop(&mut self) {
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
) -> (OpenTelemetryLayer<S, opentelemetry_sdk::trace::SdkTracer>, OtelGuard)
where
    S: tracing::Subscriber + for<'span> tracing_subscriber::registry::LookupSpan<'span>,
{
    // Tonic needs a Tokio runtime for channel setup and ongoing gRPC exports.
    // The server already has one, but the Bevy app doesn't when main() starts.
    // Create a dedicated runtime, leak it (lives for the process), and enter it
    // so that BatchSpanProcessor's tokio::spawn calls succeed.
    let (exporter, runtime_ref, enter_guard) = match tokio::runtime::Handle::try_current() {
        Ok(_handle) => {
            let exp = SpanExporter::builder()
                .with_tonic()
                .build()
                .expect("failed to build OTLP exporter");
            (exp, None, None)
        }
        Err(_) => {
            let rt: &'static tokio::runtime::Runtime =
                Box::leak(Box::new(tokio::runtime::Runtime::new()
                    .expect("failed to create OTel tokio runtime")));
            let guard = rt.enter();
            let exp = rt.block_on(async {
                SpanExporter::builder()
                    .with_tonic()
                    .build()
                    .expect("failed to build OTLP exporter")
            });
            (exp, Some(rt), Some(guard))
        }
    };

    let resource = Resource::builder()
        .with_service_name(service_name.to_string())
        .build();

    let provider = SdkTracerProvider::builder()
        .with_batch_exporter(exporter)
        .with_sampler(KaijutsuSampler)
        .with_resource(resource)
        .with_span_limits(SpanLimits::default())
        .build();

    global::set_tracer_provider(provider.clone());

    let tracer = provider.tracer("kaijutsu");
    let layer = tracing_opentelemetry::layer().with_tracer(tracer);

    (layer, OtelGuard { provider, _runtime_enter: enter_guard, _runtime: runtime_ref })
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
    span.set_parent(cx);
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
    span.set_parent(cx);
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

        // Determine rate by span name prefix
        let rate = if name.starts_with("gen_ai")
            || name.starts_with("llm")
            || name.starts_with("engine")
            || name.starts_with("tool")
            || name.starts_with("drift")
        {
            1.0 // 100%
        } else if name.starts_with("rpc") {
            0.1 // 10%
        } else if name.starts_with("sync") {
            0.01 // 1%
        } else {
            0.1 // 10% default
        };

        // Delegate to trace-id ratio sampler for deterministic decisions
        Sampler::TraceIdRatioBased(rate).should_sample(
            parent_context,
            trace_id,
            name,
            span_kind,
            attributes,
            links,
        )
    }
}
