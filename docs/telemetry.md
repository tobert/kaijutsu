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
| CRDT sync | `sync.{op}` | `sync.apply_block_op` | 1% |

### Server RPC (59 methods)

All non-VFS kernel RPC methods in `rpc.rs` are instrumented. Async methods
(those using `Promise::from_future`) use `.instrument(span)` on the future.
Sync methods use `span.entered()` guards.

| Category | Methods |
|----------|---------|
| Execution | `execute`, `execute_tool`, `shell_execute`, `prompt` |
| Context | `create_context`, `join_context`, `list_contexts`, `get_context_id` |
| Fork/Thread | `fork`, `thread`, `fork_from_version`, `cherry_pick_block` |
| Document | `attach_document`, `detach_document`, `push_ops`, `get_document_state`, `get_document_history`, `compact_document` |
| Drift | `drift_push`, `drift_pull`, `drift_merge`, `drift_flush`, `drift_queue`, `drift_cancel` |
| MCP | `register_mcp`, `unregister_mcp`, `list_mcp_servers`, `call_mcp_tool`, `list_mcp_resources`, `read_mcp_resource` |
| LLM config | `configure_llm`, `get_llm_config`, `set_default_provider`, `set_default_model` |
| Tools | `get_tool_schemas`, `get_tool_filter`, `set_tool_filter` |
| Git | `get_current_branch`, `list_branches`, `switch_branch`, `flush_git`, `register_repo`, `unregister_repo`, `list_repos` |
| VFS | `mount`, `unmount`, `list_mounts` |
| Shell vars | `get_shell_var`, `set_shell_var`, `list_shell_vars`, `get_cwd`, `set_cwd` |
| Blobs | `read_blob`, `write_blob`, `delete_blob`, `list_blobs` |
| Agents | `attach_agent`, `detach_agent`, `list_agents`, `set_agent_capabilities`, `invoke_agent` |
| Config | `get_config`, `list_configs`, `reload_config`, `reset_config` |
| Subscriptions | `subscribe_blocks`, `subscribe_agent_events`, `subscribe_mcp_resources`, `subscribe_mcp_elicitations` |
| Other | `set_attribution`, `get_command_history` |

**Special case:** `apply_block_op` uses span name `sync.apply_block_op` (1% sampling).

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

### MCP Tools (25 tools)

All 25 tools in `kaijutsu-mcp/src/lib.rs` have `#[tracing::instrument]`:

| Tool | Span |
|------|------|
| `doc_create` | `mcp.doc_create` |
| `doc_list` | `mcp.doc_list` |
| `doc_delete` | `mcp.doc_delete` |
| `doc_tree` | `mcp.doc_tree` |
| `doc_undo` | `mcp.doc_undo` |
| `block_create` | `mcp.block_create` |
| `block_read` | `mcp.block_read` |
| `block_append` | `mcp.block_append` |
| `block_edit` | `mcp.block_edit` |
| `block_list` | `mcp.block_list` |
| `block_status` | `mcp.block_status` |
| `block_inspect` | `mcp.block_inspect` |
| `block_history` | `mcp.block_history` |
| `block_diff` | `mcp.block_diff` |
| `kernel_search` | `mcp.kernel_search` |
| `drift_ls` | `mcp.drift_ls` |
| `drift_push` | `mcp.drift_push` |
| `drift_pull` | `mcp.drift_pull` |
| `drift_merge` | `mcp.drift_merge` |
| `drift_flush` | `mcp.drift_flush` |
| `drift_queue` | `mcp.drift_queue` |
| `drift_cancel` | `mcp.drift_cancel` |
| `kaish_exec` | `mcp.kaish_exec` |
| `shell` | `mcp.shell` |
| `whoami` | `mcp.whoami` |

### LLM (4 methods)

`RigProvider` methods emit spans with `llm.model` and `llm.provider` fields.
Rig-core 0.30 also emits `gen_ai.*` spans (token counts, response model) that
nest underneath.

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
| `rpc.*` | 10% | High volume |
| `sync.*` | 1% | Very high volume CRDT ops |
| Errors | 100% | Always captured |
| Other | 10% | Default |

Parent-sampled spans always inherit (trace continuity).

## Per-Context Traces

Each context gets a `trace_id` ([u8; 16], UUIDv4) at registration time. Every RPC
operation touching that context creates a span under the context's trace via
`context_root_span()`. This enables querying "show me everything that happened
in context X" in Jaeger/Grafana.

**Instrumented RPC methods:** join_context, push_ops, get_document_state,
shell_execute, drift_push/pull/merge/flush, fork_from_version, cherry_pick_block,
get_document_history, compact_document.

The `trace_id` is exposed on the wire via `ContextHandleInfo.traceId` and parsed
into `ContextInfo.trace_id` on the client side. A reverse index
(`DriftRouter.doc_to_context`) enables document-keyed RPCs to find their context's
trace without an extra lookup.

## Deferred (not yet instrumented)

- **VFS methods** (~15 filesystem ops in `impl vfs::Server`) — high volume, low debugging value
- **BlockStore internals** — CRDT ops, add when sync debugging needed
- **Unimplemented schema methods** — instrument when implemented
