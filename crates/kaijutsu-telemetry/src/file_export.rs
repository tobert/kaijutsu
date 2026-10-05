//! File export: OTLP JSON lines for traces, metrics, and logs.
//!
//! Each exporter appends one `Export*ServiceRequest` per line, in the OTLP/JSON
//! encoding, to `traces.jsonl`, `metrics.jsonl`, or `logs.jsonl`. The collector's
//! `otlpjsonfile` receiver reads this format. Conversion from SDK data to the
//! request messages and the JSON mapping both come from `opentelemetry-proto`.

use std::fmt;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use opentelemetry_proto::tonic::collector::logs::v1::ExportLogsServiceRequest;
use opentelemetry_proto::tonic::collector::metrics::v1::ExportMetricsServiceRequest;
use opentelemetry_proto::tonic::collector::trace::v1::ExportTraceServiceRequest;
use opentelemetry_proto::transform::common::tonic::ResourceAttributesWithSchema;
use opentelemetry_proto::transform::logs::tonic::group_logs_by_resource_and_scope;
use opentelemetry_proto::transform::trace::tonic::group_spans_by_resource_and_scope;
use opentelemetry_sdk::Resource;
use opentelemetry_sdk::error::OTelSdkResult;
use opentelemetry_sdk::logs::{LogBatch, LogExporter};
use opentelemetry_sdk::metrics::Temporality;
use opentelemetry_sdk::metrics::data::ResourceMetrics;
use opentelemetry_sdk::metrics::exporter::PushMetricExporter;
use opentelemetry_sdk::trace::{SpanData, SpanExporter};

pub(crate) const TRACES_FILE: &str = "traces.jsonl";
pub(crate) const METRICS_FILE: &str = "metrics.jsonl";
pub(crate) const LOGS_FILE: &str = "logs.jsonl";

/// An append-only JSON-lines file shared by one exporter.
///
/// The file opens on first write and reopens after a failed write. A failure
/// logs one warning per sink (to stderr, since the tracing pipeline is the thing
/// being written) and drops that batch; it never panics or blocks.
struct LineSink {
    path: PathBuf,
    file: Mutex<Option<File>>,
    warned: AtomicBool,
}

impl fmt::Debug for LineSink {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LineSink").field("path", &self.path).finish()
    }
}

impl LineSink {
    fn new(dir: &Path, name: &str) -> Self {
        Self {
            path: dir.join(name),
            file: Mutex::new(None),
            warned: AtomicBool::new(false),
        }
    }

    /// Serialize `request` as one line and append it. Returns whether it was written.
    fn write_json<T: serde::Serialize>(&self, request: &T) -> bool {
        let mut line = match to_otlp_json(request) {
            Ok(line) => line,
            Err(e) => {
                self.warn(&format!("cannot serialize batch: {e}"));
                return false;
            }
        };
        line.push(b'\n');
        match self.append(&line) {
            Ok(()) => true,
            Err(e) => {
                self.warn(&format!("cannot write batch: {e}"));
                false
            }
        }
    }

    fn append(&self, line: &[u8]) -> std::io::Result<()> {
        let mut guard = self.file.lock().unwrap_or_else(|p| p.into_inner());
        if guard.is_none() {
            if let Some(parent) = self.path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            *guard = Some(OpenOptions::new().create(true).append(true).open(&self.path)?);
        }
        let file = guard.as_mut().expect("opened above");
        // One write_all of the whole line: a killed process leaves whole lines.
        let result = file.write_all(line).and_then(|()| file.flush());
        if result.is_err() {
            *guard = None;
        }
        result
    }

    fn warn(&self, what: &str) {
        if !self.warned.swap(true, Ordering::Relaxed) {
            eprintln!(
                "kaijutsu-telemetry: OTLP file export to {} failed ({what}); dropping batches, further failures are silent",
                self.path.display()
            );
        }
    }
}

/// JSON object keys whose protobuf type is a 64-bit integer. OTLP/JSON encodes
/// those as strings. `opentelemetry-proto` 0.31 does so for some of these fields
/// and writes a bare number for others (for example `asInt`, histogram `count`
/// and `bucketCounts`), so every request goes through this one mapping. Keys are
/// field names, never attribute keys, which are values under `"key"`.
const INT64_FIELDS: &[&str] = &[
    "timeUnixNano",
    "startTimeUnixNano",
    "endTimeUnixNano",
    "observedTimeUnixNano",
    "asInt",
    "count",
    "zeroCount",
    "bucketCounts",
];

fn stringify_int64(value: &mut serde_json::Value) {
    use serde_json::Value;
    match value {
        Value::Object(map) => {
            for (key, child) in map.iter_mut() {
                if INT64_FIELDS.contains(&key.as_str()) {
                    match child {
                        Value::Number(n) => *child = Value::String(n.to_string()),
                        Value::Array(items) => {
                            for item in items.iter_mut() {
                                if let Value::Number(n) = item {
                                    *item = Value::String(n.to_string());
                                }
                            }
                        }
                        _ => {}
                    }
                }
                stringify_int64(child);
            }
        }
        Value::Array(items) => items.iter_mut().for_each(stringify_int64),
        _ => {}
    }
}

/// Encode `request` as compact OTLP/JSON bytes (no trailing newline).
fn to_otlp_json<T: serde::Serialize>(request: &T) -> serde_json::Result<Vec<u8>> {
    let mut value = serde_json::to_value(request)?;
    stringify_int64(&mut value);
    serde_json::to_vec(&value)
}

/// Span exporter writing `ExportTraceServiceRequest` lines.
#[derive(Debug)]
pub(crate) struct FileSpanExporter {
    sink: LineSink,
    resource: ResourceAttributesWithSchema,
}

impl FileSpanExporter {
    pub(crate) fn new(dir: &Path) -> Self {
        Self {
            sink: LineSink::new(dir, TRACES_FILE),
            resource: ResourceAttributesWithSchema::default(),
        }
    }
}

impl SpanExporter for FileSpanExporter {
    async fn export(&self, batch: Vec<SpanData>) -> OTelSdkResult {
        if batch.is_empty() {
            return Ok(());
        }
        let request = ExportTraceServiceRequest {
            resource_spans: group_spans_by_resource_and_scope(batch, &self.resource),
        };
        self.sink.write_json(&request);
        Ok(())
    }

    fn set_resource(&mut self, resource: &Resource) {
        self.resource = resource.into();
    }
}

/// Log exporter writing `ExportLogsServiceRequest` lines.
#[derive(Debug)]
pub(crate) struct FileLogExporter {
    sink: LineSink,
    resource: ResourceAttributesWithSchema,
}

impl FileLogExporter {
    pub(crate) fn new(dir: &Path) -> Self {
        Self {
            sink: LineSink::new(dir, LOGS_FILE),
            resource: ResourceAttributesWithSchema::default(),
        }
    }
}

impl LogExporter for FileLogExporter {
    async fn export(&self, batch: LogBatch<'_>) -> OTelSdkResult {
        let request = ExportLogsServiceRequest {
            resource_logs: group_logs_by_resource_and_scope(batch, &self.resource),
        };
        if request.resource_logs.iter().all(|r| r.scope_logs.is_empty()) {
            return Ok(());
        }
        self.sink.write_json(&request);
        Ok(())
    }

    fn set_resource(&mut self, resource: &Resource) {
        self.resource = resource.into();
    }
}

/// Metric exporter writing `ExportMetricsServiceRequest` lines.
#[derive(Debug)]
pub(crate) struct FileMetricExporter {
    sink: LineSink,
}

impl FileMetricExporter {
    pub(crate) fn new(dir: &Path) -> Self {
        Self {
            sink: LineSink::new(dir, METRICS_FILE),
        }
    }
}

impl PushMetricExporter for FileMetricExporter {
    async fn export(&self, metrics: &ResourceMetrics) -> OTelSdkResult {
        let request = ExportMetricsServiceRequest::from(metrics);
        if request.resource_metrics.iter().all(|r| r.scope_metrics.is_empty()) {
            return Ok(());
        }
        self.sink.write_json(&request);
        Ok(())
    }

    fn force_flush(&self) -> OTelSdkResult {
        Ok(())
    }

    fn shutdown_with_timeout(&self, _timeout: std::time::Duration) -> OTelSdkResult {
        Ok(())
    }

    fn temporality(&self) -> Temporality {
        Temporality::Cumulative
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use opentelemetry::logs::{AnyValue, LogRecord as _, Logger as _, LoggerProvider as _, Severity};
    use opentelemetry::metrics::MeterProvider as _;
    use opentelemetry::trace::{Span as _, Tracer as _, TracerProvider as _};
    use opentelemetry::KeyValue;
    use opentelemetry_sdk::logs::SdkLoggerProvider;
    use opentelemetry_sdk::metrics::{PeriodicReader, SdkMeterProvider};
    use opentelemetry_sdk::trace::SdkTracerProvider;
    use serde_json::Value;

    fn resource() -> Resource {
        Resource::builder().with_service_name("file-export-test").build()
    }

    fn read_lines(dir: &Path, name: &str) -> Vec<Value> {
        let text = std::fs::read_to_string(dir.join(name))
            .unwrap_or_else(|e| panic!("{name} must exist: {e}"));
        assert!(text.ends_with('\n'), "every line ends with a newline");
        text.lines()
            .map(|l| serde_json::from_str(l).expect("each line is one JSON document"))
            .collect()
    }

    fn string_attr<'a>(attrs: &'a Value, key: &str) -> Option<&'a str> {
        attrs
            .as_array()?
            .iter()
            .find(|kv| kv["key"] == key)
            .and_then(|kv| kv["value"]["stringValue"].as_str())
    }

    #[test]
    fn traces_line_is_otlp_json() {
        let dir = tempfile::tempdir().unwrap();
        let provider = SdkTracerProvider::builder()
            .with_simple_exporter(FileSpanExporter::new(dir.path()))
            .with_resource(resource())
            .build();
        let tracer = provider.tracer("file-export-test");
        let mut span = tracer.start("tool.dispatch");
        span.set_attribute(KeyValue::new("tool.name", "bash"));
        span.set_attribute(KeyValue::new("tool.exit", 3_i64));
        span.add_event("retry", vec![]);
        let ctx = span.span_context().clone();
        span.end();
        provider.shutdown().unwrap();

        let lines = read_lines(dir.path(), TRACES_FILE);
        assert_eq!(lines.len(), 1);
        let rs = &lines[0]["resourceSpans"][0];
        assert_eq!(
            string_attr(&rs["resource"]["attributes"], "service.name"),
            Some("file-export-test")
        );
        let span = &rs["scopeSpans"][0]["spans"][0];
        assert_eq!(span["name"], "tool.dispatch");
        assert_eq!(span["traceId"], ctx.trace_id().to_string(), "trace id is 32 hex chars");
        assert_eq!(span["spanId"], ctx.span_id().to_string(), "span id is 16 hex chars");
        assert_eq!(span["traceId"].as_str().unwrap().len(), 32);
        assert_eq!(span["spanId"].as_str().unwrap().len(), 16);
        let start: u64 = span["startTimeUnixNano"].as_str().expect("nanos are strings").parse().unwrap();
        let end: u64 = span["endTimeUnixNano"].as_str().expect("nanos are strings").parse().unwrap();
        assert!(start > 1_600_000_000_000_000_000 && end >= start);
        assert_eq!(string_attr(&span["attributes"], "tool.name"), Some("bash"));
        let exit = span["attributes"].as_array().unwrap().iter().find(|kv| kv["key"] == "tool.exit").unwrap();
        assert_eq!(exit["value"]["intValue"], "3", "int64 values are strings");
        assert!(span["kind"].is_number(), "enums are integers");
        assert!(span["events"][0]["timeUnixNano"].is_string(), "event nanos are strings");

        // The line parses back into the typed request.
        let text = std::fs::read_to_string(dir.path().join(TRACES_FILE)).unwrap();
        let typed: ExportTraceServiceRequest = serde_json::from_str(text.trim()).unwrap();
        assert_eq!(typed.resource_spans[0].scope_spans[0].spans[0].name, "tool.dispatch");
    }

    #[test]
    fn logs_line_is_otlp_json() {
        let dir = tempfile::tempdir().unwrap();
        let provider = SdkLoggerProvider::builder()
            .with_simple_exporter(FileLogExporter::new(dir.path()))
            .with_resource(resource())
            .build();
        let logger = provider.logger("file-export-test");
        let mut record = logger.create_log_record();
        record.set_severity_number(Severity::Warn);
        record.set_severity_text("WARN");
        record.set_body(AnyValue::from("beat missed"));
        record.set_timestamp(std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_nanos(1_700_000_000_123_456_789));
        record.add_attribute("beat", 42_i64);
        logger.emit(record);
        provider.shutdown().unwrap();

        let lines = read_lines(dir.path(), LOGS_FILE);
        assert_eq!(lines.len(), 1);
        let rl = &lines[0]["resourceLogs"][0];
        assert_eq!(
            string_attr(&rl["resource"]["attributes"], "service.name"),
            Some("file-export-test")
        );
        let rec = &rl["scopeLogs"][0]["logRecords"][0];
        assert_eq!(rec["body"]["stringValue"], "beat missed");
        assert_eq!(rec["timeUnixNano"], "1700000000123456789", "nanos are strings");
        assert_eq!(rec["severityNumber"], 13, "WARN is severity number 13, an integer");
        assert_eq!(rec["severityText"], "WARN");
        let beat = rec["attributes"].as_array().unwrap().iter().find(|kv| kv["key"] == "beat").unwrap();
        assert_eq!(beat["value"]["intValue"], "42");
    }

    #[test]
    fn metrics_line_is_otlp_json() {
        let dir = tempfile::tempdir().unwrap();
        let reader = PeriodicReader::builder(FileMetricExporter::new(dir.path())).build();
        let provider = SdkMeterProvider::builder()
            .with_reader(reader)
            .with_resource(resource())
            .build();
        let counter = provider.meter("file-export-test").u64_counter("kj.test.count").build();
        counter.add(5, &[KeyValue::new("kind", "unit")]);
        provider.shutdown().unwrap();

        let lines = read_lines(dir.path(), METRICS_FILE);
        assert!(!lines.is_empty());
        let rm = &lines[0]["resourceMetrics"][0];
        assert_eq!(
            string_attr(&rm["resource"]["attributes"], "service.name"),
            Some("file-export-test")
        );
        let metric = &rm["scopeMetrics"][0]["metrics"][0];
        assert_eq!(metric["name"], "kj.test.count");
        let point = &metric["sum"]["dataPoints"][0];
        assert_eq!(point["asInt"], "5", "int64 data points are strings");
        assert!(point["timeUnixNano"].is_string(), "nanos are strings");
        assert_eq!(string_attr(&point["attributes"], "kind"), Some("unit"));
        assert_eq!(metric["sum"]["aggregationTemporality"], 2, "cumulative is integer 2");
    }

    #[test]
    fn histogram_counts_are_strings() {
        let dir = tempfile::tempdir().unwrap();
        let reader = PeriodicReader::builder(FileMetricExporter::new(dir.path())).build();
        let provider = SdkMeterProvider::builder().with_reader(reader).build();
        let hist = provider
            .meter("file-export-test")
            .f64_histogram("kj.test.latency")
            .with_boundaries(vec![1.0, 10.0])
            .build();
        hist.record(0.5, &[]);
        hist.record(5.0, &[]);
        hist.record(50.0, &[]);
        provider.shutdown().unwrap();

        let lines = read_lines(dir.path(), METRICS_FILE);
        let point = &lines[0]["resourceMetrics"][0]["scopeMetrics"][0]["metrics"][0]["histogram"]["dataPoints"][0];
        assert_eq!(point["count"], "3");
        assert_eq!(point["bucketCounts"], serde_json::json!(["1", "1", "1"]));
        assert_eq!(point["explicitBounds"], serde_json::json!([1.0, 10.0]));
    }

    #[test]
    fn unwritable_dir_drops_the_batch_without_panic() {
        let dir = tempfile::tempdir().unwrap();
        // A file where the directory should be makes create_dir_all and open fail.
        let blocker = dir.path().join("blocker");
        std::fs::write(&blocker, b"not a directory").unwrap();
        let exporter = FileSpanExporter::new(&blocker.join("otel"));
        let provider = SdkTracerProvider::builder()
            .with_simple_exporter(exporter)
            .build();
        let tracer = provider.tracer("t");
        tracer.start("one").end();
        tracer.start("two").end();
        provider.shutdown().unwrap();
        assert!(!blocker.join("otel").exists());
    }

    #[test]
    fn sink_reports_failure_and_recovers_when_the_dir_appears() {
        let dir = tempfile::tempdir().unwrap();
        let blocker = dir.path().join("late");
        std::fs::write(&blocker, b"file").unwrap();
        let sink = LineSink::new(&blocker, "x.jsonl");
        assert!(!sink.write_json(&serde_json::json!({"a": 1})), "write fails");
        assert!(sink.warned.load(Ordering::Relaxed), "failure warns once");
        std::fs::remove_file(&blocker).unwrap();
        assert!(sink.write_json(&serde_json::json!({"a": 2})), "next batch lands");
        assert_eq!(std::fs::read_to_string(blocker.join("x.jsonl")).unwrap(), "{\"a\":2}\n");
    }
}
