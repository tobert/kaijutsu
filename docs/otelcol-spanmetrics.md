# Collector spanmetrics — RED metrics from spans (hand-off)

This is a **collector-side** change (`/etc/otelcol/config.yaml`, root-owned,
mounted into the `otel-collector` quadlet container). It needs root to apply and
a collector restart — it is not part of the kaijutsu build.

## What it does

The `spanmetrics` connector derives RED metrics (request **R**ate, **E**rror
rate, **D**uration histogram) from the spans the collector already receives —
keyed by span name + service. Zero app instrumentation, retroactive to every
span.

## Apply

Add a `connectors:` block, wire it as an exporter on the traces pipeline and a
receiver on the metrics pipeline:

```yaml
connectors:
  spanmetrics:
    histogram:
      explicit:
        buckets: [1ms, 5ms, 10ms, 25ms, 50ms, 100ms, 250ms, 500ms, 1s, 5s]
    dimensions:
      - name: rpc.method        # surfaces the per-method dimension on rpc spans
    metrics_flush_interval: 15s

service:
  pipelines:
    traces:
      receivers: [otlp, otlp/tls]
      processors: [batch]
      exporters: [otlp/backend, file/traces, spanmetrics]   # + spanmetrics
    metrics:
      receivers: [otlp, otlp/tls, spanmetrics]              # + spanmetrics
      processors: [batch]
      exporters: [otlp/backend, file/metrics]
```

Then: `sudo systemctl restart otel-collector` (or `podman restart otel-collector`).

## Sampling caveat (important)

Sampling happens **in-app** (the SDK `KaijutsuSampler` drops spans before
export), so the collector only ever sees *sampled* spans. spanmetrics therefore
counts post-sample:

- `engine.` / `drift.` / `llm.` / `gen_ai.` spans are 100% sampled → counts are accurate.
- `rpc` spans are 10% sampled → rates are ×10 estimates.

Drive "is the kernel busy?" dashboards off the 100% namespaces and the app-side
`gen_ai.client.operation.count`, not raw `rpc` counts. If accurate `rpc` RED is
needed later, either lift `rpc` sampling or move that counting app-side.
