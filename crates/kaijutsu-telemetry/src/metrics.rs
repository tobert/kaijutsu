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

/// Beat-timing instruments (phase-align, `docs/tracks.md`/`docs/midi.md`) —
/// the empirical tuning loop for the grid/deadband/fold-window knobs
/// (`beat.rs::GRID_RESEED_AFTER_PERIODS`, `timebase.rs::DEFAULT_PHASE_DEADBAND`,
/// `timebase.rs::REF_FOLD_MAX`): if the click and the render still wander,
/// this is where to look before moving a knob blind. Lazily bound to the
/// global meter provider, same shape as [`ContextShellMetrics`].
pub struct BeatMetrics {
    /// `kaijutsu.beat.fired` — beats fired, by `track` (the server-side
    /// scheduler's `fire_due`/`process_track`).
    beats_fired: Counter<u64>,
    /// `kaijutsu.beat.grid_reseed` — grid re-seeds, by `track`: `fire_due` gave
    /// up on bounded catch-up and re-anchored the grid at the actual wakeup
    /// instead (missed beats are missed). Should read ~0 on a healthy
    /// scheduler; sustained nonzero says the beat thread is falling behind.
    grid_reseeds: Counter<u64>,
    /// `kaijutsu.beat.sync_published` — `BeatSync` references published
    /// (server-side, low-rate — see `BEAT_SYNC_EVERY`).
    beat_sync_published: Counter<u64>,
    /// `kaijutsu.metronome.click` — metronome clicks scheduled into the ALSA
    /// queue (app-side).
    metronome_clicks: Counter<u64>,
    /// `kaijutsu.phasor.slew_beats` — the phasor's per-observe raw phase
    /// error magnitude, by `consumer` (`metronome` | `time_well`) and
    /// `outcome` (`stepped` | `deadband`). The tuning signal for
    /// `DEFAULT_PHASE_DEADBAND`: if `outcome=deadband` isn't dominant once a
    /// track is locked, the deadband is too tight for what the wire actually
    /// delivers.
    phasor_slew: Histogram<f64>,
    /// `kaijutsu.render_cue.stale_dropped` — render cues rejected outright by
    /// `midi.rs::backdate_events` as too stale to salvage even partially.
    stale_cues_dropped: Counter<u64>,
}

impl BeatMetrics {
    /// Build the instruments from a meter. Public so tests can bind a meter
    /// backed by an in-memory reader.
    pub fn new(meter: &Meter) -> Self {
        let beats_fired = meter
            .u64_counter("kaijutsu.beat.fired")
            .with_unit("{beat}")
            .with_description("Beats fired by the server-side scheduler, by track")
            .build();
        let grid_reseeds = meter
            .u64_counter("kaijutsu.beat.grid_reseed")
            .with_unit("{reseed}")
            .with_description(
                "Grid re-seeds — fire_due gave up on bounded catch-up and re-anchored at the \
                 actual wakeup, by track",
            )
            .build();
        let beat_sync_published = meter
            .u64_counter("kaijutsu.beat.sync_published")
            .with_unit("{reference}")
            .with_description("BeatSync references published by the server-side scheduler")
            .build();
        let metronome_clicks = meter
            .u64_counter("kaijutsu.metronome.click")
            .with_unit("{click}")
            .with_description("Metronome clicks scheduled into the ALSA queue")
            .build();
        let phasor_slew = meter
            .f64_histogram("kaijutsu.phasor.slew_beats")
            .with_unit("{beat}")
            .with_description(
                "Phasor per-observe raw phase error magnitude, by consumer and outcome",
            )
            .build();
        let stale_cues_dropped = meter
            .u64_counter("kaijutsu.render_cue.stale_dropped")
            .with_unit("{cue}")
            .with_description("Render cues rejected outright as too stale to back-date")
            .build();
        Self {
            beats_fired,
            grid_reseeds,
            beat_sync_published,
            metronome_clicks,
            phasor_slew,
            stale_cues_dropped,
        }
    }

    /// Record one beat fired for `track`.
    pub fn record_beat_fired(&self, track: &str) {
        self.beats_fired.add(1, &[KeyValue::new("track", track.to_owned())]);
    }

    /// Record one grid re-seed for `track`.
    pub fn record_grid_reseed(&self, track: &str) {
        self.grid_reseeds.add(1, &[KeyValue::new("track", track.to_owned())]);
    }

    /// Record one published `BeatSync` reference.
    pub fn record_beat_sync_published(&self) {
        self.beat_sync_published.add(1, &[]);
    }

    /// Record one metronome click scheduled.
    pub fn record_metronome_click(&self) {
        self.metronome_clicks.add(1, &[]);
    }

    /// Record one phasor observation's raw phase-error magnitude. `consumer`
    /// is `"metronome"` or `"time_well"`; `deadbanded` picks the `outcome`
    /// attribute (`"deadband"` when the error was too small to act on,
    /// `"stepped"` otherwise). Callers unpack a [`kaijutsu_audio::timebase::Slew`]-
    /// shaped report into these primitives rather than this crate depending on
    /// `kaijutsu-audio` — the `Slew` return value is the hand-off, not a new
    /// telemetry dependency for the audio crate.
    pub fn record_phasor_slew(&self, consumer: &str, error_beats: f64, deadbanded: bool) {
        let outcome = if deadbanded { "deadband" } else { "stepped" };
        self.phasor_slew.record(
            error_beats.abs(),
            &[
                KeyValue::new("consumer", consumer.to_owned()),
                KeyValue::new("outcome", outcome),
            ],
        );
    }

    /// Record one render cue dropped for being too stale to back-date.
    pub fn record_stale_cue_dropped(&self) {
        self.stale_cues_dropped.add(1, &[]);
    }
}

static BEAT_METRICS: LazyLock<BeatMetrics> =
    LazyLock::new(|| BeatMetrics::new(&global::meter("kaijutsu")));

/// Record one beat fired for `track` to the global meter provider. Cheap and
/// safe before OTel is initialized (no-op meter).
pub fn record_beat_fired(track: &str) {
    BEAT_METRICS.record_beat_fired(track);
}

/// Record one grid re-seed for `track` to the global meter provider.
pub fn record_grid_reseed(track: &str) {
    BEAT_METRICS.record_grid_reseed(track);
}

/// Record one published `BeatSync` reference to the global meter provider.
pub fn record_beat_sync_published() {
    BEAT_METRICS.record_beat_sync_published();
}

/// Record one metronome click scheduled to the global meter provider.
pub fn record_metronome_click() {
    BEAT_METRICS.record_metronome_click();
}

/// Record one phasor observation's slew to the global meter provider — see
/// [`BeatMetrics::record_phasor_slew`].
pub fn record_phasor_slew(consumer: &str, error_beats: f64, deadbanded: bool) {
    BEAT_METRICS.record_phasor_slew(consumer, error_beats, deadbanded);
}

/// Record one stale render cue dropped to the global meter provider.
pub fn record_stale_cue_dropped() {
    BEAT_METRICS.record_stale_cue_dropped();
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

    // ========================================================================
    // BeatMetrics (phase-align Slice 4)
    // ========================================================================

    /// Sum of a `u64` counter's data points whose `attr_key` attribute equals
    /// `attr_val`, across all exported metrics named `metric`.
    fn counter_sum(
        rm: &[opentelemetry_sdk::metrics::data::ResourceMetrics],
        metric: &str,
        attr_key: &str,
        attr_val: &str,
    ) -> u64 {
        let mut total = 0;
        for r in rm {
            for sm in r.scope_metrics() {
                for m in sm.metrics() {
                    if m.name() != metric {
                        continue;
                    }
                    let AggregatedMetrics::U64(MetricData::Sum(s)) = m.data() else {
                        continue;
                    };
                    for dp in s.data_points() {
                        let matches = dp
                            .attributes()
                            .any(|kv| kv.key.as_str() == attr_key && kv.value.as_str() == attr_val);
                        if matches {
                            total += dp.value();
                        }
                    }
                }
            }
        }
        total
    }

    /// Total count of a `u64` counter's data points (no attribute filter) —
    /// for the attribute-free instruments (`beat_sync_published`,
    /// `metronome_clicks`, `stale_cues_dropped`).
    fn counter_total(rm: &[opentelemetry_sdk::metrics::data::ResourceMetrics], metric: &str) -> u64 {
        let mut total = 0;
        for r in rm {
            for sm in r.scope_metrics() {
                for m in sm.metrics() {
                    if m.name() != metric {
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

    /// Number of `f64` histogram data points recorded under `metric` whose
    /// `attr_key` attribute equals `attr_val` — one row per distinct
    /// `(consumer, outcome)` attribute set, so this is "did the right bucket
    /// get a row" rather than a sum (a sum of phase errors isn't meaningful).
    fn histogram_row_count(
        rm: &[opentelemetry_sdk::metrics::data::ResourceMetrics],
        metric: &str,
        attr_key: &str,
        attr_val: &str,
    ) -> u64 {
        let mut count = 0;
        for r in rm {
            for sm in r.scope_metrics() {
                for m in sm.metrics() {
                    if m.name() != metric {
                        continue;
                    }
                    let AggregatedMetrics::F64(MetricData::Histogram(h)) = m.data() else {
                        continue;
                    };
                    for dp in h.data_points() {
                        let matches = dp
                            .attributes()
                            .any(|kv| kv.key.as_str() == attr_key && kv.value.as_str() == attr_val);
                        if matches {
                            count += dp.count();
                        }
                    }
                }
            }
        }
        count
    }

    /// Every `BeatMetrics` instrument produces the expected exported row(s):
    /// counters attributed by `track`, the attribute-free counters, and the
    /// phasor-slew histogram split by `consumer`/`outcome`. One test per
    /// instrument would be a lot of near-identical ceremony for a set of
    /// thin record_* wrappers over the same `Counter`/`Histogram` shape
    /// already exercised above (`records_token_usage_and_operation_count`);
    /// this pins them all in one pass instead.
    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn records_beat_metrics_rows_per_instrument() {
        let exporter = InMemoryMetricExporter::default();
        let provider = SdkMeterProvider::builder()
            .with_periodic_exporter(exporter.clone())
            .build();
        let metrics = BeatMetrics::new(&provider.meter("test"));

        metrics.record_beat_fired("groove");
        metrics.record_beat_fired("groove");
        metrics.record_beat_fired("bassline-b");
        metrics.record_grid_reseed("groove");
        metrics.record_beat_sync_published();
        metrics.record_beat_sync_published();
        metrics.record_metronome_click();
        metrics.record_phasor_slew("metronome", 0.01, true); // deadband
        metrics.record_phasor_slew("metronome", 0.05, false); // stepped
        metrics.record_phasor_slew("time_well", -0.5, false); // stepped, negative error
        metrics.record_stale_cue_dropped();

        provider.force_flush().expect("flush");
        let rm = exporter.get_finished_metrics().expect("metrics exported");

        assert_eq!(counter_sum(&rm, "kaijutsu.beat.fired", "track", "groove"), 2);
        assert_eq!(counter_sum(&rm, "kaijutsu.beat.fired", "track", "bassline-b"), 1);
        assert_eq!(counter_sum(&rm, "kaijutsu.beat.grid_reseed", "track", "groove"), 1);
        assert_eq!(counter_total(&rm, "kaijutsu.beat.sync_published"), 2);
        assert_eq!(counter_total(&rm, "kaijutsu.metronome.click"), 1);
        assert_eq!(counter_total(&rm, "kaijutsu.render_cue.stale_dropped"), 1);
        assert_eq!(
            histogram_row_count(&rm, "kaijutsu.phasor.slew_beats", "outcome", "deadband"),
            1,
            "one deadbanded observation recorded"
        );
        assert_eq!(
            histogram_row_count(&rm, "kaijutsu.phasor.slew_beats", "outcome", "stepped"),
            2,
            "two stepped observations recorded"
        );
        assert_eq!(
            histogram_row_count(&rm, "kaijutsu.phasor.slew_beats", "consumer", "time_well"),
            1,
            "the time_well-attributed observation is discoverable by consumer too"
        );
    }
}
