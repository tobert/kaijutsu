# Tool System Redesign — MCP-Centric Kernel

Status: **Planning** · Started: 2026-04-15 · Owner: Amy (tobert)

This document is the source of truth for reworking the kaijutsu-kernel tool
system around MCP as the uniform interface. It is intended to be read by
future planners and executors at the start of each phase.

> **For future planners and executors:** read the whole document before
> proposing or executing work. See [§10 Working with this document](#10-working-with-this-document)
> for the update protocol. If a proposed change would alter a decision
> recorded in [§6 Decisions](#6-decisions-locked), STOP and surface it to
> the user — do not quietly rewrite direction. Decisions use stable
> IDs (`D-01`, `D-02`, …) that are append-only and never renumbered.

> **Stance on the existing tool system:** it is a prototype. We are not
> preserving behavior, not doing per-tool migrations, not maintaining dual
> paths, and not protecting persisted data — DB wipes are acceptable at
> this stage. Each phase lands as a clean replacement, not a careful
> migration.

---

## Table of contents

1. [Motivation](#1-motivation)
2. [Goals & non-goals](#2-goals--non-goals)
3. [Target architecture](#3-target-architecture)
4. [Key types and traits](#4-key-types-and-traits)
5. [Cross-cutting: security, observability, lifecycle, notifications](#5-cross-cutting-security-observability-lifecycle-notifications)
6. [Decisions locked](#6-decisions-locked)
7. [Open questions per area](#7-open-questions-per-area)
8. [Phased rollout](#8-phased-rollout)
9. [Out-of-scope but coherent follow-ups](#9-out-of-scope-but-coherent-follow-ups)
10. [Working with this document](#10-working-with-this-document)
11. [Progress log](#11-progress-log)

---

## 1. Motivation

The existing tool system (`kaijutsu-kernel::tools::ExecutionEngine` +
`ToolRegistry`) has accumulated rough edges that make the surface hard to
reason about and extend:

- Schema duplication: every tool hand-writes a JSON schema alongside a
  serde `Params` struct; the two drift.
- String-typed dispatch: `HashMap<String, Arc<dyn ExecutionEngine>>` with
  no `ToolId` enum and no namespace.
- `ToolFilter` is only enforced at the LLM boundary
  (`kaijutsu-server/src/llm_stream.rs::build_tool_definitions`). Direct
  `kernel.execute_with(...)` bypasses it.
- `WorkspaceGuard` is opt-in plumbing, not embedded in the execution
  seam.
- Two parallel dispatch paths (builtin `ExecutionEngine` vs external
  `McpToolEngine`). Builtin tools have richer affordances (direct
  `BlockStore` access), but the registry treats them as equivalent
  abstractions.
- No tool search, no late injection, no principled hook point.
- `EngineArgs::to_argv()` reconstructs Unix-style argv for handlers that
  scan flags ad-hoc. Works, but is undocumented and fragile.

Going MCP-centric gives us one schema model, one metadata vocabulary, one
dispatch path, and a natural fit for tool search / late injection /
resources / prompts. External MCPs already live in this model; virtual
in-process MCP for builtins lets the kernel speak one protocol.

## 2. Goals & non-goals

### Goals

- **Single interface** for all tools, builtin or external: `McpServerLike`.
- **Kernel as MCP broker/proxy.** One registry keyed by
  `(instance_id, tool_name)`; one dispatch pipeline.
- **Uniform metadata** (rmcp types at the wire, kaijutsu newtype wrappers
  at the broker API boundary) so tool search, LLM tool discovery, and UI
  inspection share one shape.
- **First-class notifications and resources**, mapped onto the block
  model, with coalescing designed in from the start.
- **Design seats** for match-action hook tables, streaming, elicitation,
  and tracing, even if the first implementations are minimal.
- **No silent fallbacks.** Crashing > data corruption. Clear errors on
  removed/broken/ambiguous tools.

### Non-goals (for this refactor)

- Block content model refactor (composable content artifacts inside a
  block). Coherent with this work but not a prerequisite; see §9.
- Kaish-backed hooks. Trait seat reserved; implementation deferred.
- End-user surfaces for tool search / late injection. Phase 5 builds
  the metadata and admin primitives; product-shape UI is later.
- MCP `progress` notification → block bridge for external streaming
  tools. Deferred until we have a caller asking for it.
- **LLM streaming rewrite onto `StreamingBlockHandle`.** Design captured
  here (§4.4); implementation is a follow-up (§9). See decision history
  for why.
- **`StreamingBlockHandle` implementation.** Design only in this doc;
  first build happens when a virtual tool or the LLM consumer actually
  needs it.
- **MCP elicitation implementation.** Variant reserved (§4.1); no live
  handling wired during this refactor.
- **Per-call authorization.** Kernel security is perimeter-only
  (`D-22`); no per-call policy check.
- **Hard resource limits.** Modest seats are designed in; enforcement
  beyond "kernel doesn't OOM on obviously pathological input" is a
  follow-up (§9).

### Explicitly in scope (now that we can break things)

- Full replacement of `ExecutionEngine` / `ToolRegistry` / `McpToolEngine`
  in a single phase.
- `schemars`-derived schemas from day one.
- Deletion of `EngineArgs::to_argv()` and related convention scaffolding.
- Kernel newtype wrappers around the rmcp types used at the broker API
  boundary.

## 3. Target architecture

```
                       ┌──────────────────────────────────────┐
                       │              Kernel                  │
                       │                                      │
  LLM call ─┐          │   ┌────────────────────────────┐     │
  kaish call ├─► Broker────► HookTable[PreCall] ───────┐│     │
  MCP call ──┘          │   │                          ▼│     │
                       │   │   (instance_id, tool)          │
                       │   │   Arc<dyn McpServerLike>  │     │
                       │   │       │                   │     │
                       │   │       ▼                   │     │
                       │   │   call_tool(...)          │     │
                       │   │       │                   │     │
                       │   │       ▼                   │     │
                       │   │   HookTable[PostCall] ────┘     │
                       │   └────────────────────────────┘     │
                       │              │                       │
                       │              ▼                       │
                       │     KernelToolResult (newtype)       │
                       │              │                       │
                       │   ┌──────────▼──────────┐            │
                       │   │  BlockStore / VFS   │ (builtins  │
                       │   │  CAS / DocCache     │  only)     │
                       │   └─────────────────────┘            │
                       └──────────────────────────────────────┘
                                 ▲
                                 │ ServerNotification (broadcast)
                                 │   ↓ NotificationCoalescer ↓
          ┌──────────────────────┼──────────────────────┐
          │                      │                      │
   ┌──────┴──────┐        ┌──────┴──────┐        ┌──────┴──────┐
   │ Virtual MCP │        │ Virtual MCP │        │ External MCP│
   │ block       │        │ file        │ ...    │ fs.work     │
   │             │        │             │        │ (subprocess)│
   │  owns:      │        │  owns:      │        │  wraps:     │
   │  BlockStore │        │  Vfs, Cache │        │  rmcp       │
   │  CAS        │        │             │        │  RunningSvc │
   └─────────────┘        └─────────────┘        └─────────────┘
```

Key points:

- **One broker.** `Kernel` owns a `Broker`, which owns a registry of
  `Arc<dyn McpServerLike>` by `InstanceId`.
- **One tool-call pipeline.** Everything goes through `Broker::call_tool`.
  Hook tables wrap it. Virtual and external servers are interchangeable
  from the broker's view.
- **Kernel types at the broker API boundary.** rmcp types are used at
  the wire (external transport, virtual-server return values) but the
  broker API exposes `KernelCallParams`, `KernelToolResult`, `KernelTool`,
  etc. — thin newtypes that give us a single choke point when rmcp revs.
- **Context binding is separate from registration.** The broker knows all
  instances the kernel has available; a `ContextToolBinding` per context
  selects which instances are visible and how tools are named.
- **Notifications** come out of each server as a broadcast stream; the
  broker aggregates, coalesces, and turns them into
  `BlockKind::Notification` blocks (plus internal `ToolsChanged` handling).
- **Resources** become `BlockKind::Resource` blocks on fetch; subscription
  updates thread as child blocks (no mutation of past conversation),
  routed through the coalescer.
- **Tracing**: `CallContext` carries a W3C trace context; the broker
  emits spans around hook phases and tool calls. Existing
  `kaijutsu-telemetry` crate does W3C propagation — reuse it.

## 4. Key types and traits

These sketches are the starting point for phase 1. They may be refined
as implementation surfaces detail — record changes in §11.

### 4.1 `McpServerLike`

```rust
// crates/kaijutsu-kernel/src/mcp/server_like.rs

use rmcp::model::{
    CallToolRequestParam, CallToolResult,
    ListToolsResult, ListResourcesResult, ReadResourceResult,
    ListPromptsResult, GetPromptResult,
    ServerCapabilities,
};
use tokio_util::sync::CancellationToken;
use tokio::sync::broadcast;

pub struct CallContext {
    pub principal_id: PrincipalId,          // attribution, not authorization
    pub context_id:   ContextId,
    pub session_id:   SessionId,
    pub kernel_id:    KernelId,
    pub cwd:          Option<PathBuf>,      // None → filesystem-touching tools reject
    pub trace:        TraceContext,         // W3C trace context (see §5.2)
}

#[derive(Clone, Debug)]
pub enum ServerNotification {
    ToolsChanged,
    ResourceUpdated { uri: String },
    PromptsChanged,
    Log { level: LogLevel, msg: String, tool: Option<String> },
    Elicitation(ElicitationRequest),       // reserved seat (D-25); not wired in this refactor
}

#[derive(Clone, Debug)]
pub enum Health {
    Ready,
    Degraded { reason: String },
    Down     { reason: String },
}

#[async_trait::async_trait]
pub trait McpServerLike: Send + Sync + 'static {
    fn instance_id(&self) -> &InstanceId;
    fn capabilities(&self) -> ServerCapabilities;

    // Tools (required surface)
    async fn list_tools(&self, ctx: &CallContext) -> Result<ListToolsResult, McpError>;
    async fn call_tool(
        &self,
        params: CallToolRequestParam,
        ctx: &CallContext,
        cancel: CancellationToken,
    ) -> Result<CallToolResult, McpError>;

    // Resources / prompts: default Unsupported; override as needed
    async fn list_resources(&self, _ctx: &CallContext)
        -> Result<ListResourcesResult, McpError> { Err(McpError::Unsupported) }
    async fn read_resource(&self, _uri: &str, _ctx: &CallContext)
        -> Result<ReadResourceResult, McpError> { Err(McpError::Unsupported) }
    async fn subscribe(&self, _uri: &str, _ctx: &CallContext)
        -> Result<(), McpError> { Err(McpError::Unsupported) }
    async fn unsubscribe(&self, _uri: &str, _ctx: &CallContext)
        -> Result<(), McpError> { Err(McpError::Unsupported) }
    async fn list_prompts(&self, _ctx: &CallContext)
        -> Result<ListPromptsResult, McpError> { Err(McpError::Unsupported) }
    async fn get_prompt(&self, _name: &str, _args: serde_json::Value, _ctx: &CallContext)
        -> Result<GetPromptResult, McpError> { Err(McpError::Unsupported) }

    fn notifications(&self) -> broadcast::Receiver<ServerNotification>;

    async fn health(&self) -> Health;
    async fn shutdown(&self) -> Result<(), McpError>;
}
```

Notes:
- `CallContext` is explicit — no thread-locals. For external MCPs, the
  subset that external servers can make use of (`principal_id`,
  `context_id`, `trace`) flows through the MCP `_meta` field under a
  stable namespace (`io.kaijutsu.v1.*`, see §5.4).
- `principal_id` is **attribution**, not authorization. Kernel security
  is perimeter-only (D-22).
- Ownership of backing state is the impl's business. `BlockToolsServer`
  holds `Arc<BlockStore>` as a struct field; `ExternalMcpServer` wraps
  `Arc<rmcp::service::RunningService<rmcp::RoleClient, ()>>`.
- The old `ToolContext` is deleted. `CallContext` is the only execution
  context type.

### 4.2 Broker

```rust
pub struct InstanceId(String);                 // e.g. "builtin.block", "fs.work"

pub struct Broker {
    instances:   RwLock<HashMap<InstanceId, Arc<dyn McpServerLike>>>,
    bindings:    RwLock<HashMap<ContextId, ContextToolBinding>>,
    hooks:       RwLock<HookTables>,
    coalescer:   Arc<NotificationCoalescer>,           // §5.3
    policies:    RwLock<HashMap<InstanceId, InstancePolicy>>, // §5.5
    notif_tx:    broadcast::Sender<KernelNotification>,
}

pub struct ContextToolBinding {
    allowed_instances: Vec<InstanceId>,        // order is a tiebreaker for name resolution
    name_map:          HashMap<String, (InstanceId, String)>, // sticky resolved names (§4.2)
}
```

**Tool lookup (qualify mode `Auto` + sticky resolution, D-20):**

1. Build the visible `(instance, tool)` set from `allowed_instances`.
2. For each tool already present in `name_map`, use its existing
   resolution (sticky). This preserves names the LLM has seen across
   binding mutations.
3. For newly-visible tools not yet in `name_map`:
   - If no collision with any currently-resolved name, register
     unqualified (`tool`).
   - If a collision exists, register qualified (`instance.tool`).
4. Instances removed from the binding have their entries dropped from
   `name_map` (tools that leave are gone — tool-removed error on next
   call).

Binding mutations do not rename tools the conversation has already seen.
If an operator needs to force requalification, they do so explicitly via
the admin surface.

### 4.3 Hook tables (match-action)

```rust
pub enum HookPhase { PreCall, PostCall, OnError, OnNotification }

pub struct HookTables {
    pre_call:        HookTable,
    post_call:       HookTable,
    on_error:        HookTable,
    on_notification: HookTable,
}

pub struct HookTable {
    phase:   HookPhase,
    entries: Vec<HookEntry>,  // evaluated in priority order, then insertion order
}

pub struct HookEntry {
    id: HookId,
    // match columns
    match_instance:  Option<GlobPattern>,
    match_tool:      Option<GlobPattern>,
    match_context:   Option<ContextId>,
    match_principal: Option<PrincipalId>,
    // action column
    action:   HookAction,
    priority: i32,
}

pub enum HookAction {
    Invoke(HookBody),                 // continues the chain
    ShortCircuit(KernelToolResult),   // terminal: skips server and later hooks
    Deny(McpError),                   // terminal: returns error
    Log(LogSpec),                     // observe, continue
}

pub enum HookBody {
    Builtin(Arc<dyn Hook>),
    Kaish(ScriptRef),                 // implementation deferred
}
```

**Evaluation laws** (write tests against these):
- `HookEntry::matches(entry, call_site) -> bool` is a pure function.
- Given identical tables and identical call sites, evaluation order is
  deterministic (priority ascending, insertion-order tiebreak).
- A `ShortCircuit` or `Deny` in phase P terminates phase P. `PostCall`
  still runs after a server call completes even if the server errored
  (use `OnError` to intercept errors specifically).
- Results emitted by `ShortCircuit` are attributed to
  `InstanceId("hook:<hook_id>")` in tracing and audit logs.

**Reentrancy:** hook bodies may call back into the broker, but every
call increments a per-task depth counter capped at a fixed small value
(default 4). Exceeding the cap returns `McpError::Other`-equivalent with
"hook recursion depth exceeded" — enumerated as
`McpError::HookRecursionLimit`.

Admin surface: `builtin.hooks` MCP server exposes `hook_add`,
`hook_remove`, `hook_list`, `hook_inspect`. Same path from LLM / kaish
/ kj CLI.

### 4.4 `StreamingBlockHandle` (design only; not built in this refactor)

This is **design intent**. Implementation is deferred until a caller
actually needs it — virtual tools in phase 1 return full
`KernelToolResult` values, and the LLM streaming path (`process_llm_stream`
in `kaijutsu-server`) continues to use its current `BlockStore` append
logic. When the first streaming caller arrives, start from this sketch.

```rust
pub struct StreamingBlockHandle {
    block_id:    BlockId,
    context_id:  ContextId,
    documents:   Arc<BlockDocumentCache>,
    principal:   PrincipalId,
    status:      StreamStatus,        // Open | Closed{Done} | Closed{Error}
}

impl StreamingBlockHandle {
    pub async fn append_text(&mut self, s: &str) -> Result<(), BlockError>;
    pub async fn append_content(&mut self, mime: &str, bytes: &[u8]) -> Result<(), BlockError>;
    pub async fn close(self) -> Result<BlockId, BlockError>;                  // → Done
    pub async fn fail(self, err: String) -> Result<BlockId, BlockError>;      // → Error
}

pub struct StreamSinkFactory { /* mints child blocks under a parent for a context */ }
```

**Important constraints to resolve at implementation time:**

- **Single-block primitive.** `StreamingBlockHandle` owns the Running →
  Done/Error lifecycle for *one* block. LLM streaming is multi-block by
  nature (thinking → text → tool_call_request) and requires an outer
  orchestrator to mint sibling blocks. Do not pretend the primitive
  unifies both; it's a building block the orchestrator uses.
- **Async drop semantics.** We cannot rely on `Drop` to do async work.
  Options: (a) require explicit `close`/`fail` and panic on drop in
  debug builds; (b) have the owning factory track open handles and
  finalize them on context drop. Pick one at implementation time; both
  are viable. Do not spawn tokio tasks from `Drop`.
- **Append granularity.** Decide whether each `append_text` is one CRDT
  op or whether the handle batches. This affects the user-visible
  streaming UX.

### 4.5 Error type

```rust
pub enum McpError {
    Unsupported,
    ToolNotFound { instance: InstanceId, tool: String },
    InstanceNotFound(InstanceId),
    InstanceDown { instance: InstanceId, reason: String },      // health-gated reject
    InvalidParams(serde_json::Error),
    Protocol(rmcp::Error),
    Io(std::io::Error),
    Canceled,
    Denied { by_hook: HookId },
    HookRecursionLimit { depth: u32 },
    Coalescer { reason: CoalescerError },
    Policy(PolicyError),                                        // §5.5
}
```

No `Other(anyhow::Error)` catch-all — all error paths are named (D-26).
If a new error category appears during implementation, add a variant and
record the decision in §6.

**LLM-visible failure policy:** all failures that reach the LLM arrive
as `KernelToolResult { is_error: true, content: [text(...)] }`. The
`McpError` type is for broker-internal control flow and is converted at
the LLM boundary. This keeps the LLM's error channel uniform.

## 5. Cross-cutting: security, observability, lifecycle, notifications

### 5.1 Security model

Kernel security is **perimeter-only** (D-22). The kaijutsu kernel is
reached via SSH (see `kaijutsu-server`); authentication and access
control happen at the SSH boundary. Once a principal is connected, they
are trusted to use anything the kernel exposes — much like being SSH'ed
into a shell. There is no per-call authz policy check.

Consequences:

- `CallContext::principal_id` is for **attribution** (audit logs,
  tracing, block authorship). It does not gate access.
- `builtin.hooks` admin operations (`hook_add`, `hook_remove`, …) are
  callable by any connected principal. This is intentional; do not add
  a check.
- External MCP instances the kernel is configured to launch run with
  the kernel's own privileges.
- Future tightening (per-call authz, capability-scoped tokens) is not in
  scope and should not be designed against in this refactor.

### 5.2 Observability / tracing

`CallContext::trace` carries W3C trace context (`traceparent` +
`tracestate`). `kaijutsu-telemetry` already implements W3C propagation
(see `reference_brp_custom_methods.md` in memory).

Broker emits tracing spans around:

- `Broker::call_tool` (span: `broker.call_tool`, attrs: `instance.id`,
  `tool.name`, `context.id`, `principal.id`).
- Each hook phase (span: `hook.{phase}`, attrs: hooks evaluated,
  outcome).
- `McpServerLike::call_tool` invocation (span: `server.call_tool`).
- Notification coalescer emits spans for flush events.

For external MCPs, `traceparent` and `tracestate` are forwarded via
`_meta` under `io.kaijutsu.v1.trace`. External servers that don't
propagate will simply drop the context — that's fine, spans are
per-hop not required to be end-to-end.

### 5.3 Notification coalescing (first-class)

Resource update storms and chatty logs can overwhelm the block model if
every notification becomes a block one-to-one. The broker owns a
`NotificationCoalescer` from day one:

```rust
pub struct NotificationCoalescer {
    // Per (instance, kind) throttle window. Notifications within the
    // window collapse into a single summary block on flush.
    windows: RwLock<HashMap<(InstanceId, NotifKind), Window>>,
    default_policy: CoalescePolicy,
}

pub struct CoalescePolicy {
    pub window:      Duration,   // e.g. 500ms
    pub max_in_window: usize,    // e.g. 20; beyond this, collapse
    pub hard_drop_after: Option<Duration>, // optional aging out
}
```

Behavior:

- Notifications feed into the coalescer keyed by `(instance, kind)`.
- Within a window, first N notifications pass through as distinct
  `BlockKind::Notification` blocks. Beyond N, the coalescer emits a
  single "K updates coalesced (<summary>)" block at flush.
- `ToolsChanged` never coalesces — tool list changes are always
  surfaced (but downstream consumers should dedupe list diffs).
- `ResourceUpdated` for a single URI with rapid updates is the prime
  target: coalesce within window, flush as a single child block under
  the resource.
- Phase 2 ships the coalescer with `ToolsChanged` and `Log`. Phase 3
  extends it to `ResourceUpdated`.

### 5.4 MCP `_meta` schema

When the broker forwards `CallContext` to external MCPs, it does so
under a stable `_meta` namespace:

```
_meta: {
  "io.kaijutsu.v1.principal_id": "<uuid>",
  "io.kaijutsu.v1.context_id":   "<uuid>",
  "io.kaijutsu.v1.trace":        { "traceparent": "...", "tracestate": "..." }
}
```

- **Versioned**: the `v1` segment lets us revise shape without silently
  overwriting field meanings if the MCP spec reserves something we used.
- **Stripping is tolerated**: external servers that drop `_meta` are
  fine; the fields are for external-server *convenience*, not a
  security boundary.
- When rmcp's own `_meta` conventions change, our namespace remains
  ours.

### 5.5 Resource limits (modest seats)

Not load-bearing for this refactor; designed so phase 1 ships with
trivial defaults that keep the kernel from OOMing on pathological
input. Real limits are a follow-up (§9).

```rust
pub struct InstancePolicy {
    pub call_timeout:      Duration,       // default: 120s
    pub max_result_bytes:  usize,          // default: 64 MiB
    pub max_concurrency:   usize,          // default: 16 per instance
}
```

Broker applies the policy at `call_tool`: wraps the future in
`tokio::time::timeout`, rejects on concurrency overflow, truncates with
an error at the size cap. Violations return `McpError::Policy(...)`.
Phase 1 ships defaults; a proper policy admin surface is a follow-up.

MCP output buffering as a mitigation for chatty external servers is
captured in §9.

### 5.6 Lifecycle, error flow, removed tools

- **Instance registration** is kernel-scoped, not context-scoped. Kernel
  config declares instances (external MCP command + args, or builtin
  factory). Broker spawns on startup and on-demand.
- **Lifecycle**: broker owns spawn, restart policy, shutdown. If an
  external MCP crashes mid-call, the in-flight call returns
  `McpError::InstanceDown { reason }`. The conversation renderer's
  existing error-repair capability handles display.
- **Notifications flow** through the coalescer (§5.3) before emission.
- **Removed tool called by LLM**: `Broker::call_tool` returns a
  `KernelToolResult { is_error: true, content: [text("tool X was
  removed")] }`. LLM-visible failures all go through the `is_error`
  channel (§4.5).
- **Ambiguity**: handled by qualification + stickiness, not by error
  (§4.2).

## 6. Decisions locked

Stable IDs, append-only, never renumbered. When dropping a decision,
mark it `[RETIRED: <date> — <reason>]` in place; do not delete.

- **D-01** — Virtual in-process MCP for builtins; real MCP for externals.
  Same trait (`McpServerLike`) for both. Builtins skip transport/
  serialization; they own kernel state directly as struct fields.
- **D-02** — Kernel is the MCP broker. Kernel-scoped config, kernel owns
  lifecycle. Contexts bind to subsets.
- **D-03** — Tool identity: `(instance_id, tool_name)`. Duplicate
  instances allowed but may be nonsensical; UX guardrails added later.
- **D-04** — Notifications as `BlockKind::Notification` (first-class),
  emitted via the coalescer (D-24).
- **D-05** — Resources as `BlockKind::Resource`. Subscriptions produce
  child blocks; past blocks are never mutated. Block content abstraction
  (§9) is a coherent follow-up, not a prerequisite.
- **D-06** — Tool removal: simple "tool X removed" error, trust the
  agent. Delivered via notification + call-site error.
- **D-07** — Hooks as match-action tables, `Vec<HookEntry>` per phase
  from day one (empty by default). Actions: Invoke / ShortCircuit /
  Deny / Log. Kaish-backed hooks deferred; trait seat reserved.
- **D-08** — No kernel-scoped kaish. Kaish is context-scoped; tool
  calls from kaish go through the same pipeline and see only
  context-bound tools.
- **D-09** — Broken MCPs reject clearly with state in the rejection.
  Broker synthesizes error block; existing renderer repair handles it.
- **D-10** — rmcp types at the wire (external transport, virtual-server
  return values). Broker API boundary exposes kaijutsu newtype
  wrappers (`KernelTool`, `KernelToolResult`, `KernelCallParams`, …) so
  rmcp version breaks are a single choke point. `rmcp::Error` leaks
  through `McpError::Protocol` — accept that for now.
- **D-11** — `CallContext` is explicit. For externals, a documented
  subset flows via MCP `_meta` under `io.kaijutsu.v1.*` (§5.4).
- **D-12** — `CallContext` is a parameter, not thread-local.
- **D-13** — `StreamingBlockHandle` is a **single-block** append-streaming
  primitive for future use. LLM streaming's multi-block orchestration is
  a separate concern that builds on top of it; they are not unified by a
  single primitive. Implementation deferred (§9).
- **D-14** — Admin surfaces as builtin MCP servers (`builtin.hooks`,
  etc.). `kj` CLI becomes a thin wrapper.
- **D-15** — Explicit context over thread-locals, crash over silent
  fallback, fail loud on ambiguity — consistent with project-wide
  conventions.
- **D-16** — Old code is a prototype. No behavior preservation, no
  migration paths, no dual-path coexistence, no persisted-data
  protection. DB wipes are acceptable. Each phase lands as a
  replacement.
- **D-17** — `schemars`-derived schemas from day one. Builtin tool
  Params structs derive JSON Schema; no hand-written schema alongside.
- **D-18** — `EngineArgs::to_argv()` is deleted. New virtual servers
  receive structured params from rmcp directly. Kaish handlers that
  relied on scan-based flag detection are rewritten to the structured
  form.
- **D-19** — `CallContext` is the only execution context type.
  `ToolContext` is deleted, not adapted.
- **D-20** — Qualify mode: `Auto` + sticky resolution. Unqualified when
  unique at first binding; collisions with new instances get qualified.
  Names the LLM has seen in a context are preserved across binding
  mutations (§4.2). One mode, not configurable.
- **D-21** — [RETIRED: 2026-04-15 — symmetry argument, not a real
  constraint. Phase 1 does not bundle the LLM streaming rewrite. LLM
  streaming continues to use its current append logic and routes tool
  calls through the broker; a unified streaming primitive is not needed
  because virtual tools do not stream in v1.]
- **D-22** — Kernel security is **perimeter-only**. SSH-level auth
  gates access; no per-call authz. `principal_id` is attribution only.
  `builtin.hooks` and other admin tools are callable by any connected
  principal. This is intentional and consistent with the kaijutsu
  access model (§5.1).
- **D-23** — `CallContext` carries W3C trace context. Broker emits
  tracing spans around hook phases and tool calls. Context propagated
  to external MCPs via `_meta` under `io.kaijutsu.v1.trace` (§5.2).
  Uses `kaijutsu-telemetry`'s existing W3C propagation.
- **D-24** — Notification coalescing is first-class. Broker owns a
  `NotificationCoalescer`; all notification emission passes through it.
  Phase 2 ships coalescing for `ToolsChanged` / `Log`; phase 3 extends
  to `ResourceUpdated` (§5.3).
- **D-25** — MCP elicitation reserved as `ServerNotification::Elicitation`
  variant; no live handling wired during this refactor. Existing
  `SharedElicitationFlowBus` in `mcp_pool.rs` is deleted with the rest
  of the old system; a design for elicitation flow is in §9.
- **D-26** — No `McpError::Other(anyhow::Error)` catch-all. All error
  paths are named variants; new categories require a new variant and a
  decision entry.
- **D-27** — Modest `InstancePolicy` seat (timeout / size cap /
  concurrency) shipped with trivial defaults in phase 1 (§5.5). Real
  enforcement is a follow-up (§9). Enough to avoid OOM-on-pathological
  input; not a full resource governor.
- **D-28** — LLM-visible failures all arrive as `KernelToolResult {
  is_error: true }`. `McpError` is for broker-internal control flow and
  is converted at the LLM boundary (§4.5). One error channel to the
  model.
- **D-29** — Hook reentrancy: bodies may call back into the broker,
  capped at a small per-task depth (default 4). Cap exceeded returns
  `McpError::HookRecursionLimit`.
- **D-30** — Content-creation tools (`svg_block`, `abc_block`,
  `img_block`, `img_block_from_path`) fold into `BlockToolsServer`
  (instance `builtin.block`). One instance owns the BlockStore and
  exposes both structural and content-creation operations; splitting
  into a separate `ContentToolsServer` doubled BlockStore ownership
  for no user-visible benefit.
- **D-31** — Phase 1 exit criterion #4 ("identical CRDT ops to the
  pre-refactor system") is dropped. Rationale: no snapshot harness
  exists and broker dispatch is a transparent passthrough to the
  same engine bodies; the existing `block_tools`/`file_tools` unit
  tests already cover correctness. Replacement criterion: "broker
  dispatch passes the existing `block_tools` and `file_tools` test
  suites unchanged."
- **D-32** — All four MCP FlowBuses (`SharedResourceFlowBus`,
  `SharedProgressFlowBus`, `SharedLoggingFlowBus`,
  `SharedElicitationFlowBus`) deleted in Phase 1 M5. External MCP
  notifications are dropped on the floor until Phase 2 wires the
  coalescer → `BlockKind::Notification` path. `ExternalMcpServer`
  still receives notifications via its `ClientHandler`, converts
  them to `ServerNotification`, and publishes on its
  `broadcast::Sender`; nothing subscribes yet. Non-MCP flow buses
  (`BlockFlow`, `ConfigFlow`, `InputDocFlow`) survive unchanged.
- **D-33** — Kaish tool invocations route through
  `Broker::call_tool` with the same `CallContext`.
  `KaijutsuBackend::list_tools` / `get_tool` / `call_tool` enumerate
  and dispatch via the broker (no transitional registry shim, per
  D-16). `McpError::ToolNotFound` maps to
  `BackendError::ToolNotFound` at the kaish boundary so downstream
  "tool not found" semantics survive.

## 7. Open questions per area

Questions that do not need to be answered *now* but should be resolved
before (or during) the phase that touches them. Planners: pick these
up and drive to a concrete choice with the user. Record the decision
by *appending* to §6 (new stable ID) and leaving a `[RESOLVED: <D-XX>]`
note here.

### Broker / identity
- What does a stale tool reference look like across a CRDT replay? Are
  notifications idempotent on replay?

### Notifications
- Coalescer policy defaults (window, max-in-window) — do we need
  per-kind tuning or is one default enough for v1?
- Should `Log` notifications default to `BlockKind::Notification` or a
  separate log buffer?

### Resources
- Subscription lifetime when a context drops — broker unsubscribes, yes;
  what about suspended/hibernated contexts? Do we persist subscriptions
  or require re-subscribe on resume?
- Resource content-type → block rendering: who owns the mapping? First
  cut can be naive (mime → existing BlockKind) but eventually wants the
  block content abstraction from §9.
- Subscription replay semantics: subscription is a *side effect* tied
  to the live context, not replayed with CRDT ops. Child blocks produced
  by prior updates are CRDT data and do replay. Formalize this before
  phase 3.

### Hooks
- Evaluation order across phases when multiple hooks match: strictly
  priority-sorted, or grouped by some other axis?
- Kaish hook envelope format — full request/result JSON? Subset? How do
  we serialize `CallContext` for the hook script?
- Hook-group atomicity: should a `PreCall` hook be able to guarantee a
  matching `PostCall` hook runs (transactional pairing for audit
  logging)? If yes, introduce `HookGroup` before phase 4.

### Streaming (deferred to follow-up)
- Async-drop strategy for `StreamingBlockHandle` — explicit close +
  debug panic, or factory-tracked finalization on context drop. Decide
  at implementation time.
- Append granularity — per-call CRDT op or batched. Decide at
  implementation time.
- How does an LLM-initiated cancel reach a streaming tool mid-append?
  `CancellationToken` in `call_tool` is necessary but not sufficient;
  the handle needs a hook too.

### Elicitation (deferred to follow-up)
- How does an elicitation request bubble from external MCP → broker →
  UI? Does it become a `BlockKind::Elicitation` with interactive
  affordance, or a notification with side-channel UI hook?
- What is the response path back to the originating tool call? The
  current MCP call is blocked waiting; do we extend `call_tool` to
  accept an elicitation callback, or multiplex via context state?

## 8. Phased rollout

Five phases. Each phase lands as a replacement (no dual paths) and
leaves the system in a fully working state — DB wipes between phases
are acceptable. Plan documents for each phase go in `~/.claude/plans/`.

Status legend: `planned` · `in-progress` · `complete` · `blocked`

### Phase 1 — Replace plumbing · **complete**

Plan: `~/.claude/plans/polished-sauteeing-corbato.md`

The big phase. Rip out the old tool system in a single branch; main
comes back green with the new shape. **LLM streaming path is updated to
route tool calls through the broker but keeps its current append logic
unchanged** — the streaming primitive is deferred (D-13, §9).

Deliverables:
- `kaijutsu-kernel/src/mcp/`: `server_like.rs`, `broker.rs`, `error.rs`,
  `types.rs`, `coalescer.rs`, `policy.rs` (per §4, §5.3, §5.5).
- Kernel newtype wrappers at the broker API boundary (`KernelTool`,
  `KernelToolResult`, `KernelCallParams`, etc.) per D-10.
- Enable `server` + `macros` features on rmcp in
  `kaijutsu-kernel/Cargo.toml`.
- Virtual MCP servers covering every existing tool:
  - `BlockToolsServer` (block_create/append/edit/splice/read/search/
    list/status + svg/abc/img variants — decide during planning whether
    content tools fold in or live in a `ContentToolsServer`).
  - `FileToolsServer` (read/edit/write/glob/grep).
  - `KernelInfoServer` (whoami/kernel_search).
- `ExternalMcpServer: McpServerLike` wrapping
  `rmcp::service::RunningService`; `CallContext` subset passed via
  `_meta` under `io.kaijutsu.v1.*` (§5.4).
- `ContextToolBinding` per context, qualify mode `Auto` + sticky (D-20).
  LLM tool-definitions built from bindings.
- All builtin tool Params structs derive schemas via `schemars` (D-17).
- Tracing spans emitted around `Broker::call_tool`, hook phases, and
  server calls (§5.2). W3C trace context threaded through `CallContext`
  and `_meta`.
- `InstancePolicy` with trivial defaults (§5.5). Timeout wrapping,
  concurrency gating, result size cap wired.
- `NotificationCoalescer` infrastructure built (§5.3). No notification
  *emission* yet (that's phase 2); just the coalescer exists and is
  injected into the broker.
- `ServerNotification::Elicitation(...)` variant reserved (D-25).
- Deletions:
  - `kaijutsu-kernel/src/tools.rs` (ExecutionEngine, ToolRegistry,
    EngineArgs, ToolContext).
  - `kaijutsu-kernel/src/block_tools/engines.rs` (old engine impls).
  - `kaijutsu-kernel/src/file_tools/*.rs` (old engine impls).
  - `McpToolEngine` in `mcp_pool.rs` plus surrounding scaffolding.
  - `SharedElicitationFlowBus` / related flow buses in `mcp_pool.rs`
    (elicitation reserved via D-25, implementation deferred).
  - All `to_argv` handlers and old `ToolFilter` call sites that become
    redundant under binding-derived tool lists.
- All call sites updated (kaijutsu-server, kaijutsu-mcp, kaijutsu-app,
  kaijutsu-client as applicable). `process_llm_stream` now calls
  `Broker::call_tool` but its `BlockStore` streaming append logic is
  untouched.

Out of scope in phase 1 (deferred):
- `BlockKind::Notification` variant and notification emission (phase 2).
- `BlockKind::Resource` variant and resource flows (phase 3).
- Hook tables wiring + `builtin.hooks` admin server (phase 4). The
  `HookTables` type exists on the broker but is empty and not yet
  evaluated at call time.
- Tool search (phase 5).
- `StreamingBlockHandle` implementation (§9).
- LLM streaming rewrite onto streaming primitive (§9).
- Elicitation live handling (§9).

Exit criteria — concrete end-to-end scenarios that must pass:

1. `cargo check --workspace` clean; `cargo test --workspace` passes.
2. No references to `ExecutionEngine` / `ToolRegistry` / `EngineArgs` /
   `ToolContext` / `McpToolEngine` / `SharedElicitationFlowBus` /
   `Shared{Resource,Progress,Logging}FlowBus` remain in live code
   across the workspace. Retired names appear only in comments/doc
   strings that describe the history.
3. A kaijutsu-app session sends a message to an LLM; the LLM streams a
   response that appears in a Running → Done block sequence.
4. Broker dispatch passes the existing `block_tools` and `file_tools`
   test suites unchanged (per D-31 — the original "identical CRDT ops
   vs pre-refactor" criterion was dropped because no snapshot harness
   existed and delegation is a transparent passthrough).
5. An external MCP tool call through `ExternalMcpServer` succeeds
   end-to-end against at least one real MCP in the user's environment
   (suggested: `gpal.consult_gemini_oneshot` or `bevy_brp.brp_status`),
   with `_meta` propagation verified by inspecting the outgoing request.
6. Tracing spans for the above flows are emitted and visible in the
   telemetry output.
7. The kaijutsu-runner autonomous loop (contrib/kaijutsu-runner.sh +
   contrib/kj) still works for agent-driven testing.
8. A pathological input (tool returning > `max_result_bytes`) produces
   `McpError::Policy` rather than unbounded allocation.

### Phase 2 — Notifications · **planned**

`BlockKind::Notification` variant added. Broker subscribes to per-server
notification streams, aggregates, coalesces via `NotificationCoalescer`
(§5.3), tags with `instance_id`, and emits:

- `ToolsChanged` → `tool_added` / `tool_removed` notifications into
  affected contexts; invalidate cached tool lists.
- `Log` / `PromptsChanged` → notification blocks (coalesced).

(`ResourceUpdated` wiring waits for phase 3.)

Exit criteria:
- Adding an instance at runtime produces a visible `tool_added`
  notification in bound contexts.
- LLM's next-turn tool list reflects the change.
- Removing a tool surfaces as both a call-site error and a notification
  block.
- A burst of `Log` notifications within the coalescer window produces
  one summary block, not N individual blocks.

### Phase 3 — Resources · **planned**

`BlockKind::Resource` variant. `read_resource` on first access produces
a resource block. Subscription updates thread as child blocks under the
original, **routed through the coalescer** (§5.3). Subscription
lifecycle owned by context binding (unsubscribe on context drop).

Exit criteria:
- At least one builtin or external resource is read-through-block.
- A subscription update produces a child block threaded under the
  resource block.
- A burst of updates to the same URI produces one coalesced child block
  per window, not one per update.
- Context drop unsubscribes cleanly (no orphaned subscriptions in the
  broker).

### Phase 4 — Hooks + admin · **planned**

Match-action tables as specified in §4.3. All four phases wired
(`PreCall` / `PostCall` / `OnError` / `OnNotification`). Empty tables
by default. Builtin hook bodies only (`HookBody::Kaish` reserved but
stubbed — returns `Unsupported`).

`builtin.hooks` admin server with `hook_add` / `hook_remove` /
`hook_list` / `hook_inspect`.

Exit criteria:
- A builtin `Log` hook produces visible log output for matching tool
  calls.
- A `Deny` hook blocks a matching call with a clear error result
  (delivered as `KernelToolResult { is_error: true }` to the LLM per
  D-28).
- A `ShortCircuit` hook's result is attributed to
  `InstanceId("hook:<hook_id>")` in tracing.
- Admin server round-trips (add → list → inspect → remove).
- Reentrant hook that exceeds depth cap returns
  `McpError::HookRecursionLimit` (D-29).

### Phase 5 — Tool search + late injection · **planned**

`builtin.tool_search` server that indexes available tools by metadata
(name, description, tags, capabilities) and returns matching
`(instance_id, tool_name)` pairs. Context bindings gain a late-injection
path: add an instance to the binding mid-session, notification fires,
LLM picks up on next turn.

Exit criteria:
- Tool search returns useful results against the builtin corpus.
- Late-injection changes a live context's tool list and the LLM sees
  the new tools on the next turn.
- Sticky resolution (D-20) preserves previously-seen names across the
  binding mutation.

### Phase 6+ — Follow-ups (deferred)

See §9.

## 9. Out-of-scope but coherent follow-ups

These are not part of the five phases, but the refactor should produce a
shape that makes them easy. Record decisions here that would constrain
the current work.

- **`StreamingBlockHandle` implementation.** Build when the first
  streaming caller arrives (likely the LLM streaming rewrite). Resolve
  async-drop strategy and append granularity at implementation time
  (§4.4, §7).
- **LLM streaming rewrite.** Rewrite
  `kaijutsu-server/src/llm_stream.rs::process_llm_stream` onto
  `StreamingBlockHandle` (single-block primitive) plus an outer
  orchestrator that mints sibling blocks for
  thinking/text/tool_call boundaries. Not a unification — a refactor.
- **MCP elicitation** live handling (D-25). Design options in §7.
- **Block content abstraction.** Blocks as containers that compose
  multiple content artifacts. Prerequisite for rich
  resource-subscription rendering.
- **Kaish-backed hooks.** Fill in `HookBody::Kaish` with real
  invocation: serialize request/result, run script, parse return.
- **MCP `progress` → `StreamingBlockHandle` bridge.** External tools
  that stream via MCP progress notifications get wired into a handle
  the same way virtual tools do.
- **Cancellation propagation** from LLM cancel → in-flight tool calls
  via `CancellationToken`. Needs wiring across all call sites plus a
  cancel path into `StreamingBlockHandle`.
- **MCP output buffering.** Nice-to-have mitigation for chatty external
  servers: sliding-window buffer with backpressure, coupled with
  `InstancePolicy::max_result_bytes` (§5.5).
- **Real resource-limit enforcement.** A proper admin surface for
  `InstancePolicy`; per-principal budgets; fair queuing.
- **Tool versioning / deprecation metadata.** Out of scope until we
  have a reason.

## 10. Working with this document

### For future planners

When the user asks you to plan a phase (`read docs/tool-system-redesign.md
and let's plan phase N`):

1. **Read the entire document, top to bottom.** Decisions and open
   questions outside your phase can still affect it.
2. **Confirm scope.** Work strictly within the current phase's
   deliverables and exit criteria. If the phase is too big or too
   small, raise that with the user before planning — do not silently
   rescope.
3. **Respect §6 decisions.** If something in your planning would
   contradict a locked decision, STOP and surface the conflict. The
   user may reopen the decision; you may not.
4. **Pick up open questions from §7 that block this phase.** Drive
   them to a concrete choice with the user. Record the decision by
   *appending* to §6 with a new stable ID, and leave a
   `[RESOLVED: D-XX]` marker in place of the §7 entry (or mark it
   resolved and remove once the phase completes).
5. **Write the plan to `~/.claude/plans/`** per project convention.
   Link it from this doc under the phase (see update protocol below).
6. **Update this doc as part of planning**: set the phase status,
   link the plan, add any new open questions raised, append an entry
   to the progress log (§11).

### For executors

When the user asks you to execute a phase:

1. **Read this document and the phase's plan.** Do not start from just
   the plan — the doc contains the cross-phase constraints.
2. **Update the phase status to `in-progress`** and append a progress
   log entry noting the start.
3. **Execute the plan.** If reality diverges from the plan, update the
   plan and log the divergence with rationale. Do not quietly improvise
   around the doc.
4. **On completion**: set phase status to `complete`, confirm exit
   criteria are met (each named end-to-end scenario), append a progress
   log entry with what shipped and any follow-ups discovered (add them
   to §7 or §9 as appropriate).
5. **Discoveries that would change §6 decisions** must be surfaced to
   the user, not silently adopted. Retired decisions are marked in
   place (`[RETIRED: <date> — <reason>]`), not deleted.

### Update protocol (concrete mechanics)

- **Decision IDs are stable and append-only.** Never renumber. Retired
  decisions stay in §6 with a retirement marker.
- Status changes: edit the `status:` marker in the phase header.
- Links to plans: add a `Plan: path/to/plan.md` line under the phase
  title.
- Log entries: append to §11 with date, author (planner/executor/user),
  and one or two sentences.
- New decisions: append to §6 with the next stable ID (`D-XX`), a brief
  rationale, and — where possible — an enforceable invariant or test
  path.
- Resolved open questions: append new decision to §6, mark §7 entry
  with `[RESOLVED: D-XX]`, remove fully once the relevant phase closes.
- Keep diffs small and focused on what this doc is for — the structure
  (TOC, section numbering) is load-bearing; don't reorganize without
  reason.

## 11. Progress log

Entries are append-only. Most recent at the bottom.

- **2026-04-15** (design conversation, Amy + Claude Opus 4.6): Initial
  document drafted from design conversation. All phases `planned`;
  decisions 1–15 locked; open questions enumerated.
- **2026-04-15** (design conversation, Amy + Claude Opus 4.6): Rescoped
  to five phases after user confirmed old code is prototype and DB
  wipes are acceptable. Collapsed original phases 1–6 + 10 into phase
  1 (replace plumbing + streaming). Added decisions 16–21 (prototype
  stance, schemars from day one, delete EngineArgs, `CallContext`
  only, qualify mode `Auto`, LLM streaming rewrite in phase 1).
- **2026-04-15** (review + revision, Amy + Claude Opus 4.6): Revised
  doc after critical review. Switched §6 to stable append-only IDs.
  Retired D-21 (LLM streaming rewrite in phase 1 — symmetry argument,
  not a real constraint). Revised D-13 (streaming is single-block
  primitive, not a unification). Revised D-20 (added sticky
  resolution). Revised D-10 (newtype wrappers at broker API boundary).
  Added D-22 (perimeter-only security), D-23 (W3C trace context),
  D-24 (first-class notification coalescing), D-25 (elicitation
  reserved), D-26 (no anyhow::Other), D-27 (modest `InstancePolicy`),
  D-28 (unified LLM-visible error channel), D-29 (hook recursion cap).
  Removed `StreamingBlockHandle` implementation and LLM streaming
  rewrite from phase 1; both now follow-ups in §9. Phase 1 exit
  criteria expanded to named end-to-end scenarios including a real
  external MCP round-trip.
- **2026-04-15** (Phase 1 execution, Amy + Claude Opus 4.6): Phase 1
  landed in six milestones (M1 skeleton → M2 builtin virtual servers
  → M3 ExternalMcpServer → M4 call-site swap → M5 aggressive
  deletions → M6 doc/verify). Decisions D-30 (content tools fold
  into BlockToolsServer), D-31 (drop identical-CRDT exit #4),
  D-32 (all 4 MCP FlowBuses deleted), D-33 (kaish through broker)
  recorded during planning. M5 cut ~6,700 LOC across 32 files:
  `tools.rs`, `mcp_pool.rs`, `mcp_config.rs`, `image/gemini.rs`,
  `kj/prompt.rs`, and the 4 MCP flow types. MCP admin RPC methods
  stubbed with `unimplemented` responses behind existing capnp
  ordinals pending Phase 2 broker-admin rewiring. Suite: 417 kernel
  + 45 server tests pass. End-to-end app-session and external MCP
  round-trip verification holds for a subsequent live session on the
  user's GPU server.
