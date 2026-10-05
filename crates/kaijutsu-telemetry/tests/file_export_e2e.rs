//! End to end: `KAIJUTSU_OTEL_FILE_DIR` alone makes `otel_layer` write all three
//! signals. This file holds one test because it sets process environment and
//! installs global providers.

use tracing_subscriber::layer::SubscriberExt;

#[test]
fn file_dir_alone_writes_traces_metrics_and_logs() {
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("nested").join("otel");
    // SAFETY: this test binary has one test and no other thread reads the environment yet.
    unsafe {
        std::env::set_var("KAIJUTSU_OTEL_FILE_DIR", &out);
        std::env::remove_var("OTEL_EXPORTER_OTLP_ENDPOINT");
        std::env::remove_var("OTEL_TRACES_EXPORTER");
        std::env::remove_var("OTEL_SDK_DISABLED");
    }
    assert!(kaijutsu_telemetry::otel_enabled());

    let (layer, guard) = kaijutsu_telemetry::otel_layer("e2e-service");
    let subscriber = tracing_subscriber::registry().with(layer);
    tracing::subscriber::with_default(subscriber, || {
        let span = tracing::info_span!("tool.dispatch", tool = "bash");
        let _entered = span.enter();
        tracing::warn!(beat = 7, "beat missed");
        kaijutsu_telemetry::record_roster_transition("appeared", "bound");
    });
    drop(guard);

    let read = |name: &str| -> Vec<serde_json::Value> {
        std::fs::read_to_string(out.join(name))
            .unwrap_or_else(|e| panic!("{name}: {e}"))
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    };
    let traces = read("traces.jsonl");
    let span = &traces[0]["resourceSpans"][0]["scopeSpans"][0]["spans"][0];
    assert_eq!(span["name"], "tool.dispatch");
    let trace_id = span["traceId"].as_str().unwrap().to_owned();

    let logs = read("logs.jsonl");
    let rec = &logs[0]["resourceLogs"][0]["scopeLogs"][0]["logRecords"][0];
    assert_eq!(rec["body"]["stringValue"], "beat missed");
    assert_eq!(rec["traceId"], trace_id, "the log carries the span's trace id");

    let metrics = read("metrics.jsonl");
    let names: Vec<&str> = metrics
        .iter()
        .flat_map(|l| l["resourceMetrics"][0]["scopeMetrics"].as_array().unwrap())
        .flat_map(|s| s["metrics"].as_array().unwrap())
        .filter_map(|m| m["name"].as_str())
        .collect();
    assert!(names.contains(&"kaijutsu.roster.transition"), "got {names:?}");
}
