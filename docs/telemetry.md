# Telemetry

Kaijutsu uses OpenTelemetry for distributed tracing across the SSH + Cap'n Proto
boundary between client and server.

## Quick Start

All three binaries support OTel export behind the `telemetry` feature flag.
Export activates when standard OTel environment variables are set:

```bash
# Point at your OTLP collector (gRPC)
export OTEL_EXPORTER_OTLP_ENDPOINT=http://localhost:4317

# Run with telemetry enabled
cargo run -p kaijutsu-server --features telemetry
cargo run -p kaijutsu-app --features telemetry
cargo run -p kaijutsu-mcp --features telemetry
```

Without the `telemetry` feature, no OTel deps are compiled in. Without
`OTEL_EXPORTER_OTLP_ENDPOINT` set, nothing is exported even with the feature.

## Environment Variables

Standard OTel env vars are respected:

| Variable | Effect |
|----------|--------|
| `OTEL_EXPORTER_OTLP_ENDPOINT` | OTLP gRPC endpoint (enables export) |
| `OTEL_TRACES_EXPORTER` | Exporter type (`otlp`, `none`) |
| `OTEL_SERVICE_NAME` | Overrides the default service name |
| `OTEL_SDK_DISABLED=true` | Force-disable even when endpoint is set |

## What Gets Traced

### Distributed traces (client → server)

W3C Trace Context (`traceparent`/`tracestate`) is propagated in-band through
Cap'n Proto method params. A tool call from kaijutsu-app produces a connected trace:

```
client::actor::execute_tool (kaijutsu-app)
  └── rpc.execute_tool (kaijutsu-server, linked via traceparent)
        └── engine.execute (tool=git__status)
```

### Instrumented RPC methods

| Method | Span name | Side |
|--------|-----------|------|
| `execute` | `rpc.execute` | Server |
| `executeTool` | `rpc.execute_tool` | Server |
| `shellExecute` | `rpc.shell_execute` | Server |
| `prompt` | `rpc.prompt` | Server |
| `callMcpTool` | `rpc.call_mcp_tool` | Server |
| `pushOps` | `rpc.push_ops` | Server |
| `getDocumentState` | `rpc.get_document_state` | Server |
| `driftPush` | `rpc.drift_push` | Server |
| `driftFlush` | `rpc.drift_flush` | Server |
| `driftPull` | `rpc.drift_pull` | Server |
| `driftMerge` | `rpc.drift_merge` | Server |

### LLM spans

`RigProvider::prompt()` and `prompt_with_system()` emit spans with
`llm.model` and `llm.provider` fields. Rig-core 0.30 also emits `gen_ai.*`
spans (token counts, response model) that nest underneath.

### Drift operations

`DriftPushEngine`, `DriftPullEngine`, `DriftFlushEngine`, `DriftMergeEngine`
emit `drift.push`, `drift.pull`, `drift.flush`, `drift.merge` spans.

### MCP tool surface

The `kaijutsu-mcp` server instruments its key tool methods with `mcp.*` spans.

## Sampling

The `KaijutsuSampler` applies differentiated rates:

| Category | Rate | Rationale |
|----------|------|-----------|
| `gen_ai.*`, `llm.*` | 100% | Expensive, rare, highest value |
| `engine.*`, `tool.*` | 100% | Critical for debugging |
| `drift.*` | 100% | Cross-context operations |
| `rpc.*` | 10% | High volume |
| `sync.*` | 1% | Very high volume CRDT ops |
| Errors | 100% | Always captured |
| Other | 10% | Default |

Parent-sampled spans always inherit (trace continuity).

## Without the Feature

When compiled without `--features telemetry`:

- No OTel dependencies are pulled in
- `inject_trace_context()` returns empty strings (zero-cost)
- `extract_trace_context()` returns a disabled span
- `#[instrument]` spans still work with any tracing subscriber (file logging, etc.)
