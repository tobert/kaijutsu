//! Domain metric instruments for kaijutsu.
//!
//! Metrics record through the **global meter provider** installed by
//! [`crate::otel_layer`] — there is no tracing-subscriber layer for metrics.
//! Before OTel is initialized the global meter is a no-op, so calling these
//! recorders without an endpoint configured is harmless.
//!
//! Naming follows the OpenTelemetry GenAI semantic conventions
//! (`gen_ai.client.*`, attributes `gen_ai.system` / `gen_ai.request.model`)
//! so the metrics line up with standard dashboards and the collector's
//! spanmetrics output. Note this is intentionally distinct from the kaijutsu
//! `llm.*` span fields — spans and metrics use different namespaces.

use std::sync::LazyLock;

use opentelemetry::KeyValue;
use opentelemetry::global;
use opentelemetry::metrics::{Counter, Histogram, Meter};

/// LLM token-usage and operation-count instruments.
pub struct LlmMetrics {
    /// `gen_ai.client.token.usage` — tokens per operation, split by
    /// `gen_ai.token.type` (input / output / cache_read / cache_creation).
    token_usage: Histogram<u64>,
    /// `gen_ai.client.operation.count` — completed LLM operations.
    operation_count: Counter<u64>,
}

impl LlmMetrics {
    /// Build the instruments from a meter. Public so tests can bind a meter
    /// backed by an in-memory reader.
    pub fn new(meter: &Meter) -> Self {
        let token_usage = meter
            .u64_histogram("gen_ai.client.token.usage")
            .with_unit("{token}")
            .with_description("Tokens used in an LLM operation, by token type")
            .build();
        let operation_count = meter
            .u64_counter("gen_ai.client.operation.count")
            .with_unit("{operation}")
            .with_description("Completed LLM operations")
            .build();
        Self {
            token_usage,
            operation_count,
        }
    }

    /// Record one completed LLM operation and its token usage.
    ///
    /// Token types are recorded as separate measurements keyed by
    /// `gen_ai.token.type`; zero-valued types are skipped to avoid empty
    /// time series. `cache_read` / `cache_creation` carry the provider-specific
    /// (e.g. Anthropic) cache accounting and are reported alongside `input`,
    /// not folded into it.
    pub fn record(&self, provider: &str, model: &str, tokens: TokenCounts) {
        let op_attrs = [
            KeyValue::new("gen_ai.system", provider.to_owned()),
            KeyValue::new("gen_ai.request.model", model.to_owned()),
        ];
        self.operation_count.add(1, &op_attrs);

        for (kind, n) in [
            ("input", tokens.input),
            ("output", tokens.output),
            ("cache_read", tokens.cache_read),
            ("cache_creation", tokens.cache_creation),
            ("reasoning", tokens.reasoning),
        ] {
            if n == 0 {
                continue;
            }
            self.token_usage.record(
                n,
                &[
                    KeyValue::new("gen_ai.system", provider.to_owned()),
                    KeyValue::new("gen_ai.request.model", model.to_owned()),
                    KeyValue::new("gen_ai.token.type", kind),
                ],
            );
        }
    }
}

/// Token counts for a single LLM operation.
///
/// Decouples the recorder from any provider's usage struct: callers map their
/// own usage type into this and the kernel's LLM types stay out of telemetry.
#[derive(Debug, Clone, Copy, Default)]
pub struct TokenCounts {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_creation: u64,
    /// Chain-of-thought tokens (DeepSeek reasoner / thinking modes),
    /// billed as output but reported separately so dashboards can split
    /// reasoning spend from answer spend.
    pub reasoning: u64,
}

/// Context-shell instruments, lazily bound to the global meter provider.
pub struct ContextShellMetrics {
    /// `kaijutsu.context_shell.cwd_restore_failed` — a context cwd that no
    /// longer resolves to a directory in the shell's backend: either dropped on
    /// restore (fell back to the default landing dir) or skipped on a
    /// switch-time save (last good value preserved).
    cwd_restore_failed: Counter<u64>,
}

impl ContextShellMetrics {
    /// Build the instruments from a meter. Public so tests can bind a meter
    /// backed by an in-memory reader.
    pub fn new(meter: &Meter) -> Self {
        let cwd_restore_failed = meter
            .u64_counter("kaijutsu.context_shell.cwd_restore_failed")
            .with_unit("{restore}")
            .with_description(
                "Persisted context cwd that no longer resolves in the backend on restore",
            )
            .build();
        Self { cwd_restore_failed }
    }

    /// Record one persisted-cwd restore that failed to resolve.
    pub fn record_cwd_restore_failed(&self) {
        self.cwd_restore_failed.add(1, &[]);
    }
}

static CONTEXT_SHELL_METRICS: LazyLock<ContextShellMetrics> =
    LazyLock::new(|| ContextShellMetrics::new(&global::meter("kaijutsu")));

/// Record one failed context-cwd restore to the global meter provider. Cheap
/// and safe before OTel is initialized (no-op meter), like [`record_llm_usage`].
pub fn record_cwd_restore_failed() {
    CONTEXT_SHELL_METRICS.record_cwd_restore_failed();
}

/// Process-wide LLM instruments, lazily bound to the global meter provider.
///
/// Initialized on first use — which in practice is the first LLM call, well
/// after `otel_layer` has installed the real provider, so the instruments bind
/// to the exporting meter rather than the startup no-op.
static LLM_METRICS: LazyLock<LlmMetrics> =
    LazyLock::new(|| LlmMetrics::new(&global::meter("kaijutsu")));

/// Record a completed LLM operation's token usage to the global meter provider.
pub fn record_llm_usage(provider: &str, model: &str, tokens: TokenCounts) {
    LLM_METRICS.record(provider, model, tokens);
}

#[cfg(test)]
mod tests {
    use super::*;
    use opentelemetry::metrics::MeterProvider;
    use opentelemetry_sdk::metrics::SdkMeterProvider;
    use opentelemetry_sdk::metrics::InMemoryMetricExporter;
    use opentelemetry_sdk::metrics::data::{AggregatedMetrics, MetricData};

    /// Sum the `gen_ai.token.type` values recorded to the token-usage histogram
    /// for a given token type, across all exported data points.
    fn token_sum(rm: &[opentelemetry_sdk::metrics::data::ResourceMetrics], kind: &str) -> u64 {
        let mut total = 0;
        for r in rm {
            for sm in r.scope_metrics() {
                for m in sm.metrics() {
                    if m.name() != "gen_ai.client.token.usage" {
                        continue;
                    }
                    let AggregatedMetrics::U64(MetricData::Histogram(h)) = m.data() else {
                        continue;
                    };
                    for dp in h.data_points() {
                        let is_kind = dp
                            .attributes()
                            .any(|kv| kv.key.as_str() == "gen_ai.token.type" && kv.value.as_str() == kind);
                        if is_kind {
                            total += dp.sum();
                        }
                    }
                }
            }
        }
        total
    }

    fn operation_count(rm: &[opentelemetry_sdk::metrics::data::ResourceMetrics]) -> u64 {
        let mut total = 0;
        for r in rm {
            for sm in r.scope_metrics() {
                for m in sm.metrics() {
                    if m.name() != "gen_ai.client.operation.count" {
                        continue;
                    }
                    let AggregatedMetrics::U64(MetricData::Sum(s)) = m.data() else {
                        continue;
                    };
                    for dp in s.data_points() {
                        total += dp.value();
                    }
                }
            }
        }
        total
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn records_token_usage_and_operation_count() {
        let exporter = InMemoryMetricExporter::default();
        let provider = SdkMeterProvider::builder()
            .with_periodic_exporter(exporter.clone())
            .build();
        let metrics = LlmMetrics::new(&provider.meter("test"));

        metrics.record(
            "anthropic",
            "claude-opus-4-8",
            TokenCounts { input: 100, output: 50, cache_read: 20, cache_creation: 10, reasoning: 0 },
        );
        metrics.record(
            "deepseek",
            "deepseek-v4-pro",
            TokenCounts { input: 200, output: 60, cache_read: 0, cache_creation: 0, reasoning: 40 },
        );

        provider.force_flush().expect("flush");
        let rm = exporter.get_finished_metrics().expect("metrics exported");

        // Two operations recorded.
        assert_eq!(operation_count(&rm), 2, "operation count");
        // Token sums per type — distinct totals catch input/output swaps.
        assert_eq!(token_sum(&rm, "input"), 300, "input tokens");
        assert_eq!(token_sum(&rm, "output"), 110, "output tokens");
        assert_eq!(token_sum(&rm, "cache_read"), 20, "cache_read tokens");
        assert_eq!(token_sum(&rm, "cache_creation"), 10, "cache_creation tokens");
        assert_eq!(token_sum(&rm, "reasoning"), 40, "reasoning tokens");
    }
}
