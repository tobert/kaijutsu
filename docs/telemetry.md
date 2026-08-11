# Telemetry

Kaijutsu uses OpenTelemetry for distributed tracing across the SSH + Cap'n Proto
boundary between client and server.

## Quick Start

OTel is always compiled in. Export activates when standard OTel environment
variables are set:

```bash
# Point at your OTLP collector (gRPC)
export OTEL_EXPORTER_OTLP_ENDPOINT=http://localhost:4317

# Run normally — OTel export activates automatically
cargo run -p kaijutsu-server
cargo run -p kaijutsu-app
cargo run -p kaijutsu-mcp
```

Without `OTEL_EXPORTER_OTLP_ENDPOINT` set, nothing is exported.

## Environment Variables

Standard OTel env vars are respected:

| Variable | Effect |
|----------|--------|
| `OTEL_EXPORTER_OTLP_ENDPOINT` | OTLP gRPC endpoint (enables export) |
| `OTEL_TRACES_EXPORTER` | Exporter type (`otlp`, `none`) |
| `OTEL_SERVICE_NAME` | Overrides the default service name |
| `OTEL_SDK_DISABLED=true` | Force-disable even when endpoint is set |

## What Gets Traced

### Example full trace hierarchy

A tool call from kaijutsu-app produces a connected distributed trace:

```
actor.execute_tool (kaijutsu-app)
  └── rpc_client.execute_tool (kaijutsu-client, injects traceparent)
        └── rpc{method="execute_tool"} (kaijutsu-server, extracts traceparent)
              └── engine.git (kaijutsu-kernel)
```

W3C Trace Context (`traceparent`/`tracestate`) propagates in-band through
Cap'n Proto method params. The client injects context via `inject_trace_context()`,
the server extracts it via `extract_rpc_trace()`.

### Span Naming Convention

| Layer | Pattern | Example | Sample Rate |
|-------|---------|---------|-------------|
| Server RPC | `rpc` with `method` field | `rpc{method="execute"}` | 10% |
| Client RPC | `rpc_client.{method}` | `rpc_client.execute` | 10% (default) |
| Actor | Auto-named from method | `ActorHandle::execute` | 10% (default) |
| Execution engines | `engine.{name}` | `engine.git` | 100% |
| Drift engines | `drift.{op}` | `drift.push` | 100% |
| MCP tools | `mcp.{tool}` | `mcp.block_read` | 10% (default) |
| LLM | Auto-named with `llm.*` fields | `prompt{llm.model, llm.provider}` | 100% |
| Turn outcome | `turn.{op}`, `turn.*` fields | `turn.events_push{turn.stop_reason}` | 100% |
| CRDT sync | `sync.{op}` | `sync.push_ops` | 1% |

### Server RPC

All non-VFS kernel RPC methods in `rpc.rs` are instrumented. Async methods
(those using `Promise::from_future`) use `.instrument(span)` on the future.
Sync methods use `span.entered()` guards.

| Category | Methods |
|----------|---------|
| Execution | `execute`, `execute_tool`, `shell_execute`, `prompt` |
| Context | `create_context`, `join_context`, `list_contexts`, `get_context_id` |
| Fork/Thread | `fork`, `thread`, `cherry_pick_block` |
| CRDT / history | `push_ops`, `get_context_history`, `compact_context` |
| Drift | `drift_queue`, `drift_cancel` (push/pull/merge/flush moved into `kj` dispatch, `kaijutsu-kernel/src/kj/drift.rs`) |
| MCP | `register_mcp`, `unregister_mcp`, `list_mcp_servers`, `call_mcp_tool`, `list_mcp_resources`, `read_mcp_resource` |
| LLM config | `configure_llm`, `get_llm_config`, `set_default_provider`, `set_default_model` |
| Tools | `get_tool_schemas`, `get_tool_filter`, `set_tool_filter` |
| Git | `get_current_branch`, `list_branches`, `switch_branch`, `flush_git`, `register_repo`, `unregister_repo`, `list_repos` |
| VFS | `mount`, `unmount`, `list_mounts` |
| Shell vars | `get_shell_var`, `set_shell_var`, `list_shell_vars`, `get_cwd`, `set_cwd` |
| Blobs | `read_blob`, `write_blob`, `delete_blob`, `list_blobs` |
| Peers | `attach_peer`, `detach_peer`, `list_peers`, `invoke_peer` |
| Config | `get_config`, `list_configs`, `reload_config`, `reset_config` |
| Subscriptions | `subscribe_blocks`, `subscribe_mcp_resources`, `subscribe_mcp_elicitations`, `subscribe_editor`, `subscribe_turn_events` |
| Other | `set_attribution`, `get_command_history` |

**Not instrumented:** VFS filesystem methods (~15 in `impl vfs::Server`) — high volume, low debugging value. Trivial stubs (`whoami`, `get_info`, `interrupt`, `complete`, `detach`).

### Client RPC (46 methods)

All `KernelHandle` and `RpcClient` methods in `rpc.rs` have `#[tracing::instrument]`
with `name = "rpc_client.{method}"`. Large args (`code`, `ops`, `content`) are skipped.

### Actor (40 methods)

All `ActorHandle` methods in `actor.rs` have `#[tracing::instrument(skip(self))]`.
Span names auto-derive from the method name (e.g., `ActorHandle::execute_tool`).

### Execution Engines (20 spans)

All `ExecutionEngine::execute()` implementations:

| Engine | Span | File |
|--------|------|------|
| Git | `engine.git` | `git_engine.rs` |
| Rhai | `engine.rhai` | `rhai_engine.rs` |
| MCP tool bridge | `engine.mcp_tool` | `mcp_pool.rs` |
| Whoami | `engine.whoami` | `file_tools/whoami.rs` |
| Read | `engine.read` | `file_tools/read.rs` |
| Write | `engine.write` | `file_tools/write.rs` |
| Edit | `engine.edit` | `file_tools/edit.rs` |
| Glob | `engine.glob` | `file_tools/glob.rs` |
| Grep | `engine.grep` | `file_tools/grep.rs` |
| Block create | `engine.block_create` | `block_tools/engines.rs` |
| Block read | `engine.block_read` | `block_tools/engines.rs` |
| Block list | `engine.block_list` | `block_tools/engines.rs` |
| Block edit | `engine.block_edit` | `block_tools/engines.rs` |
| Block append | `engine.block_append` | `block_tools/engines.rs` |
| Block splice | `engine.block_splice` | `block_tools/engines.rs` |
| Block search | `engine.block_search` | `block_tools/engines.rs` |
| Block status | `engine.block_status` | `block_tools/engines.rs` |
| Kernel search | `engine.kernel_search` | `block_tools/engines.rs` |
| Drift ls | `engine.drift_ls` | `drift.rs` |
| ToolRegistry | `ToolRegistry::execute` | `tools.rs` |

### Drift (10 spans)

| Operation | Span | Description |
|-----------|------|-------------|
| `DriftRouter::register` | `drift.register` | Register context with router |
| `DriftRouter::unregister` | `drift.unregister` | Remove context |
| `DriftRouter::rename` | `drift.rename` | Rename context label |
| `DriftRouter::stage` | Auto-named with fields | Stage content for delivery |
| `DriftRouter::drain` | `drift.drain` | Drain staged items |
| `DriftPushEngine` | `drift.push` | Push content to target context |
| `DriftPullEngine` | `drift.pull` | Pull + distill from source |
| `DriftFlushEngine` | `drift.flush` | Deliver all staged drifts |
| `DriftMergeEngine` | `drift.merge` | Merge fork back to parent |
| `DriftLsEngine` | `engine.drift_ls` | List available contexts |

### MCP Tools (10 tools)

The MCP slim-down cut 16 doc/block/drift-detail tools that duplicated
kernel-side functionality now reached through `kj` (via `shell`/`context_shell`)
— see the removal note at the top of `impl KaijutsuMcp` in
`kaijutsu-mcp/src/lib.rs`. All 10 remaining `#[tool(...)]` methods have
`#[tracing::instrument]`:

| Tool | Span |
|------|------|
| `kaish_exec` | `mcp.kaish_exec` |
| `list_kernel_tools` | `mcp.list_kernel_tools` |
| `shell` | `mcp.shell` |
| `register_session` | `mcp.register_session` |
| `whoami` | `mcp.whoami` |
| `invoke_peer` | `mcp.invoke_peer` |
| `read_input` | `mcp.read_input` |
| `write_input` | `mcp.write_input` |
| `edit_input` | `mcp.edit_input` |
| `submit_input` | `mcp.submit_input` |

### LLM (4 methods)

Provider methods emit spans with `llm.model` and `llm.provider` fields (rig-core
is gone — see `kaijutsu-kernel/src/lib.rs:88`, provider dispatch replaced it).
`gen_ai.*` exists only as **metrics** now (`kaijutsu-telemetry/src/metrics.rs`),
not spans — see Metrics below.

| Method | Fields |
|--------|--------|
| `prompt()` | `llm.model`, `llm.provider` |
| `prompt_with_system()` | `llm.model`, `llm.provider` |
| `stream()` | `llm.provider` |
| `models()` | — |

## Sampling

The `KaijutsuSampler` applies differentiated rates based on span name prefix:

| Category | Rate | Rationale |
|----------|------|-----------|
| `gen_ai.*`, `llm.*` | 100% | Expensive, rare, highest value |
| `engine.*`, `tool.*` | 100% | Critical for debugging |
| `drift.*` | 100% | Cross-context operations |
| `turn.*` | 100% | One span per turn ending — as rare as turns, and the whole story of how one ended |
| `rpc.*` | 10% | High volume |
| `sync.*` | 1% | Very high volume CRDT ops |
| Errors | 100% | Always captured |
| Other | 10% | Default |

Parent-sampled spans always inherit (trace continuity).

**Prefix collision caveat:** the 100% namespaces are **dot-qualified**
(`drift.`, `engine.`, `tool.`, `gen_ai.`, `llm.`, `turn.`). Auto-named actor/method spans
like `drift_queue` must NOT match `drift.` — before the dot they were swept to
100% and the app's 5s idle drift poll dominated trace volume. See
`sampling_rate()` and its regression test in `kaijutsu-telemetry`.

## Metrics

OTel metrics are always compiled in and export through the **global meter
provider** installed by `otel_layer` — there is no tracing-subscriber layer for
metrics (unlike traces/logs). Activation is the same `OTEL_EXPORTER_OTLP_*`
gating as traces. A periodic reader pushes over OTLP gRPC.

Metrics are **not sampled** — instead, keep cardinality bounded by attribute
choice (no context/trace ids as metric attributes).

### Instruments

| Metric | Type | Attributes | Source |
|--------|------|------------|--------|
| `gen_ai.client.token.usage` | histogram `{token}` | `gen_ai.system`, `gen_ai.request.model`, `gen_ai.token.type` (input/output/cache_read/cache_creation) | `record_llm_usage()` at `StreamEvent::Done` |
| `gen_ai.client.operation.count` | counter `{operation}` | `gen_ai.system`, `gen_ai.request.model` | same |
| `kaijutsu.beat.fired` | counter `{beat}` | `track` | `beat.rs::process_track` (server) |
| `kaijutsu.beat.grid_reseed` | counter `{reseed}` | `track` | `beat.rs::fire_due`'s re-seed branch — the grid gave up on bounded catch-up |
| `kaijutsu.beat.sync_published` | counter `{reference}` | — | `beat.rs::publish_beat_sync` post-gate |
| `kaijutsu.metronome.click` | counter `{click}` | — | `metronome.rs::click_on_beat` (app) |
| `kaijutsu.phasor.slew_beats` | histogram `{beat}` | `consumer` (metronome/time_well), `outcome` (stepped/deadband) | `metronome.rs`/`live.rs` `Fold` arm — the phase-align tuning loop for `DEFAULT_PHASE_DEADBAND`/`REF_FOLD_MAX` |
| `kaijutsu.render_cue.stale_dropped` | counter `{cue}` | — | `midi.rs::backdate_events` reject branch (app) |

Naming follows OTel GenAI semantic conventions (`gen_ai.*`). This is
**intentionally distinct** from the kaijutsu `llm.*` span fields — spans and
metrics live in different namespaces so the metrics line up with standard
dashboards and the collector's spanmetrics output. `record_llm_usage()` takes a
`TokenCounts` so the recorder stays decoupled from any provider's usage struct.

> Cache-token accounting (`cache_read` / `cache_creation`) isn't carried on
> `StreamEvent::Done` yet — only `input` / `output` are recorded today. Extend
> when `Done` carries the richer `Usage`.

### RED metrics from spans (collector spanmetrics)

RED metrics (rate / errors / duration) are derived at the **collector** from
existing spans via the `spanmetrics` connector — no app instrumentation. This is
free and retroactive to all spans, but it counts only **sampled** spans:
`engine.`/`drift.`/`llm.` at 100% are accurate; `rpc` at 10% is a ×10 estimate.
Drive "is the kernel busy" dashboards off the 100% namespaces, not raw `rpc`
counts. (Connector config is collector-side — see the deploy notes.)

## Logs

Existing `tracing` events are bridged to OTLP log records via
`opentelemetry-appender-tracing`. The bridge is added to each binary's registry
by `otel_layer` alongside the trace layer (as a `Vec<Box<dyn Layer>>` so both
sit at one level). Records carry the active trace/span id for correlation.

The fmt → stderr/journald logging is unchanged; OTLP logs are additive and
respect the same `EnvFilter`. The bridge **excludes `opentelemetry*` targets**
so the exporter's own internal logs can't feed back into the exporter and storm
on a persistent export failure.

## Per-Context Traces

Each context gets a `trace_id` ([u8; 16], UUIDv4) at registration time. Every RPC
operation touching that context creates a span under the context's trace via
`context_root_span()`. This enables querying "show me everything that happened
in context X" in Jaeger/Grafana.

**Instrumented RPC methods:** join_context, push_ops, shell_execute,
cherry_pick_block, get_context_history, compact_context, drift_queue,
drift_cancel. (`get_context_history` and `compact_context` carried stale
`"get_document_history"` / `"compact_document"` span-name strings from before
the document→context rename until 2026-08-11 — if you are correlating traces
older than that, the span names differ from the method names.)

The `trace_id` is exposed on the wire via `ContextHandleInfo.traceId` and parsed
into `ContextInfo.trace_id` on the client side. A reverse index
(`DriftRouter.doc_to_context`) enables document-keyed RPCs to find their context's
trace without an extra lookup.

## Deferred (not yet instrumented)

- **VFS methods** (~15 filesystem ops in `impl vfs::Server`) — high volume, low debugging value
- **BlockStore internals** — CRDT ops, add when sync debugging needed
- **Unimplemented schema methods** — instrument when implemented
