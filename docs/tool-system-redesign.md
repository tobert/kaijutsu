# Tool System Redesign — MCP-Centric Kernel

Status: **Planning** · Started: 2026-04-15 · Owner: Amy (tobert)

This document is the source of truth for reworking the kaijutsu-kernel tool
system around MCP as the uniform interface. It is intended to be read by
future planners and executors at the start of each phase.

> **For future planners :** read the whole document first.
> See [§10 Working with this document](#10-working-with-this-document)
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
- **D-34** — Notification blocks are **LLM-visible**. The LLM hydrator
  in `crates/kaijutsu-kernel/src/llm/mod.rs` gets a
  `BlockKind::Notification` arm that routes through
  `format_notification_for_llm` (XML envelope with
  `<notification instance="..." kind="..." level="..."> summary + detail
  </notification>`). Truncation budget 512 chars
  (`NOTIFICATION_DETAIL_HYDRATION_BUDGET`). Rationale: the
  conversation-as-record model applies to tool changes too; the user
  chose fidelity over token savings. Resolves the "separate log buffer?"
  open question in §7 (Notifications) against a dedicated buffer.
- **D-35** — `ServerNotification::ToolsChanged` is diffed **per-tool**.
  The broker holds `Mutex<HashMap<InstanceId, Vec<KernelTool>>>` of
  last-seen tools and diffs against `list_tools(&CallContext::system())`
  on each ToolsChanged (and on register/unregister), emitting one
  `NotificationKind::ToolAdded` or `ToolRemoved` per delta. Implements
  the Phase 2 deliverable literally: "ToolsChanged → tool_added /
  tool_removed notifications" (§8).
- **D-36** — `NotificationPayload` is wired **all the way to the app**
  over capnp in Phase 2 (not deferred to Phase 3). Mirrors the
  `ErrorPayload` precedent: `NotificationPayload` struct in
  `kaijutsu.capnp`, `notificationPayload @29` /
  `hasNotificationPayload @30` on the capnp `BlockSnapshot`, converters
  in `kaijutsu-client/src/rpc.rs` and `kaijutsu-server/src/rpc.rs`.
  Structured metadata reaches the renderer so per-kind styling (color
  keys, potential icons, tool links) is available without a Phase 3
  follow-up.
- **D-37** — `Broker::set_documents(SharedBlockStore)` setter (not
  constructor injection). Called from
  `Kernel::register_builtin_mcp_servers` at bootstrap. `Broker::new()` +
  `Default` stay workable for tests that don't need emission; emission
  is a no-op when `documents` is unset. Keeps the broker constructor
  flexible and the test fixtures minimal.
- **D-38** — `Broker::register_silently()` variant used at kernel
  bootstrap to suppress synthetic `ToolAdded` notifications for the
  three builtin MCP servers. Runtime `register`/`unregister` call the
  emitting variants. Prevents every kernel restart from accumulating
  bootstrap noise when a persistent context happens to exist.
- **D-39** — Coalescer flush via **per-key oneshot timer**.
  `NotificationCoalescer::observe()` returns
  `ObserveOutcome { PassThrough | StartWindow | Coalesced{so_far} }`.
  On `StartWindow`, the broker pump spawns a `sleep(window)` task that
  calls `flush(instance, kind)` and emits a single
  `NotificationKind::Coalesced` summary block. Timers are tracked per
  `(InstanceId, NotifKind)` and aborted on `unregister` so no orphan
  Coalesced blocks fire after teardown (R2 mitigation).
- **D-40** — **Coalescer key extended to `(InstanceId, NotifKind,
  Option<String>)`** (Phase 3). The `Option<String>` is the resource URI
  for `ResourceUpdated`; `None` for all other kinds. Per-URI windows track
  independently so two URIs on the same instance don't coalesce into each
  other. `ObserveOutcome` shape is unchanged. Implementation: composite
  `CoalesceKey { instance, kind, uri }` replaces the 2-tuple HashMap key.
  Test path: `mcp::coalescer::tests::uri_windows_are_independent` +
  `uri_none_and_some_are_independent`.
- **D-41** — **Subscription trigger is an explicit admin MCP tool**, not
  auto-on-read (Phase 3). New virtual server `BuiltinResourcesServer`
  (instance `builtin.resources`) exposes `list` / `read` / `subscribe` /
  `unsubscribe` tools delegating to `Broker::*`. Read stays side-effect-
  free (it emits a root block but does not subscribe). Holds
  `Weak<Broker>` to avoid the Arc cycle (broker owns the instance Arc).
  Registered via `register_silently` at kernel bootstrap. Test path:
  `servers::resources_builtin::tests::builtin_resources_server_subscribe_roundtrip`.
- **D-42** — **`ResourcePayload` is dual-content from day one**, mirroring
  rmcp's `ResourceContents`: `text: Option<String>` + `blob_base64:
  Option<String>` (exactly one populated per read). Binary bodies stay
  base64-encoded end-to-end to avoid encode/decode forks between CRDT,
  RPC, and LLM hydration paths. Resolves §7 Resources (d).
- **D-43** — **On `ResourceUpdated` flush, broker re-reads the URI and
  emits a fresh child `BlockKind::Resource` block parented to the initial
  read**. MCP `resources/updated` carries no contents; re-read gives the
  LLM the current state directly. On re-read failure, emit a single
  `BlockKind::Notification { kind: Log, level: Warn }` under the same
  parent — never a fake Resource block with empty contents. Test path:
  `mcp::broker::tests::failed_reread_emits_log_not_resource`.
- **D-44** — **Subscription lifecycle is bound to `ContextToolBinding`**.
  Broker holds `Mutex<HashMap<ContextId, HashSet<(InstanceId, String)>>>`.
  `clear_binding` / `unregister` walk matching entries and call
  `server.unsubscribe` on each. CRDT replay of a hibernated context does
  **not** re-subscribe (live side effect); past child Resource blocks
  replay as-is. Test path:
  `mcp::broker::tests::clear_binding_unsubscribes_all_uris` +
  `unregister_unsubscribes_bound_contexts`.
- **D-45** — **Per-kind coalescer policy override:
  `NotifKind::ResourceUpdated` uses `max_in_window = 0`**. Every update
  inside a window folds into the flush-emitted summary; there are zero
  pass-throughs. Matches §8 Phase 3 exit criterion #3 literally ("one
  coalesced child block per window, not one per update"). Implementation:
  `CoalescePolicy::per_kind_override: HashMap<NotifKind, usize>` with the
  default constructor inserting `ResourceUpdated => 0`. Log /
  PromptsChanged keep the default cap of 20. Test path:
  `mcp::coalescer::tests::resource_updated_has_no_pass_throughs`.
- **D-46** — **Hook match predicate uses `kaish_glob::glob_match` on
  instance/tool and equality on `ContextId`/`PrincipalId`.**
  `GlobPattern` stays a `String` newtype (no pre-compilation); the
  kernel already depends on `kaish-glob` so no new dep is added. Rejects
  a handwritten `*`/`?` matcher for consistency with every other glob
  site in the repo. `match_context` / `match_principal` use equality,
  not globs (ID types aren't strings).
  Test path: `mcp::broker::tests::hook_match_instance_and_tool_globs`
  + `hook_match_context_and_principal_filters`.
- **D-47** — **Reentrancy cap via `tokio::task_local!` depth counter,
  capped at `MAX_HOOK_DEPTH = 4` (§4.3, D-29).** A `HookDepthGuard`
  decrements on drop so the counter survives panic unwind; the cap is
  checked before increment. Exceed returns
  `McpError::HookRecursionLimit { depth: MAX + 1 }`. Outer `call_tool`
  installs the scope on first entry and reuses it on recursive re-entry
  from Invoke bodies. Test-only `HOOK_DEPTH_OVERRIDE` (`OnceLock<u32>`)
  lets fixtures drive the cap with smaller numbers.
  Test path: `mcp::broker::tests::reentrant_hook_exceeds_depth_cap` +
  positive-control `reentrant_hook_under_cap_succeeds` +
  `hook_depth_resets_across_calls` + drop-guard pin
  `panicking_hook_body_does_not_leak_depth`.
- **D-48** — **`HookAction::Log` emits `tracing::event!`, not a
  block.** LLM-visible audit is achieved by an `Invoke` body that
  writes a block explicitly; the builtin `Log` variant is observability
  only. Matches the "user chooses visibility explicitly" stance
  already used for resource re-reads (D-43).
  Test path: `mcp::broker::tests::log_hook_emits_tracing_event_not_block`
  (uses `tracing::subscriber::set_default` to capture the event).
- **D-49** — **`ShortCircuit` is tracing-attributed via a
  `hook.short_circuit` span event carrying `hook_id = "hook:<id>"`.**
  Event, not a new span, so the parent `broker.call_tool` span remains
  the correlation anchor. The returned `KernelToolResult` is untouched
  — attribution lives in traces, not in the result shape, so the LLM
  doesn't see a synthetic instance name in its result history. Exit
  criterion #3 asserts the event fires with the expected `hook_id`.
  Test path: `mcp::broker::tests::short_circuit_emits_attribution_event`.
- **D-50** — **Named builtin hook registry; admin wire never ships
  `Arc<dyn Hook>`.** A `BuiltinHookRegistry` maps `&'static str` →
  factory `fn() -> Arc<dyn Hook>`. `hook_add` admin RPC takes a tagged
  JSON `action` where `BuiltinInvoke` carries a `name: String`
  referenced against the registry; unknown names reject. Phase 4 ships
  `tracing_audit` + `no_op`. Kaish bodies reject at add time with
  `McpError::Unsupported`. `HookBody::Builtin` carries `{ name, hook }`
  so `hook_inspect` can surface the builtin name without reflecting on
  `Arc<dyn Hook>`; `HookAction::Deny(String)` carries a tracing-only
  reason (outer pipe returns `McpError::Denied { by_hook }` regardless
  per D-28).
  Test path: `mcp::servers::hooks_builtin::tests::hook_add_unknown_builtin_rejects`
  + `hook_add_kaish_rejects` +
  `admin_round_trip_with_builtin_log_hook`.
- **D-51** — **`builtin.hooks` is carved out of hook evaluation.**
  Calls with `params.instance == "builtin.hooks"` bypass
  `evaluate_phase` entirely. Without this, a user who registers a
  PreCall `Deny(match_tool="*")` would lock themselves out of
  `hook_remove` and have no recovery short of a kernel restart. Other
  builtin instances (`builtin.block`, `builtin.file`, `builtin.resources`,
  `builtin.kernel_info`) remain subject to hooks — users may legitimately
  want to audit or gate those. The carve-out is a recovery-path safety
  valve, not a general admin-immunity policy.
  Test path: implicit — the `admin_round_trip_with_builtin_log_hook`
  test registers Deny hooks then still succeeds in issuing `hook_list`
  and `hook_remove`, which would fail without the carve-out.
  [RETIRED: 2026-04-17 — hook persistence makes sqlite surgery + restart
  the recovery path; admin servers uniform under hook evaluation per
  D-53. Carve-out in `Broker::evaluate_phase` deleted. Replacement test:
  `mcp::servers::hooks_builtin::tests::hooks_admin_is_subject_to_hooks`
  asserts a PreCall `Deny(*)` actually blocks `hook_list` on
  `builtin.hooks` (symmetric to Phase 5's
  `bindings_server_subject_to_hooks`). The pre-existing
  `admin_round_trip_with_builtin_log_hook` — which the original note
  described as relying on the carve-out — was in fact self-protecting
  (Log action, no Deny) and passes unchanged after retirement.]
- **D-52** — **Phase 5 reframed from "tool search + late injection" to
  "per-context binding management + persistence."** Phase 1 plumbed
  `ContextToolBinding` but no operator-facing surface curates it;
  every context first-touch-populates with all registered instances
  (`kernel.rs::dispatch_tool_via_broker` / `list_tool_defs_via_broker`).
  Search is UX sugar on top of a curation surface that doesn't exist;
  shipping search first is unanchored. Tool search moved to §9.
  Origin: review conversation 2026-04-17 where the user pointed out
  that "search should be about making that configuration easy" and
  that operational pressure is on curation, not discovery.
- **D-53** — **`builtin.bindings` has no hook-evaluation carve-out.**
  A user who locks themselves out of `bind` / `unbind` with an
  overbroad `PreCall Deny(*)` hook recovers by restarting the kernel
  (hooks are in-memory). Carve-out complexity isn't worth a restart
  we'd accept elsewhere. D-51's identical carve-out on `builtin.hooks`
  is now a candidate for retirement (§9 follow-up); not re-opened
  here. Consistency target: every admin MCP server is subject to
  hook evaluation; kernel restart is the universal escape hatch.
- **D-54** — **Bindings persisted via `KernelDb`; legacy `ToolFilter`
  retired.** Phase 5 adds a `context_bindings` (or equivalent)
  table/shape in `kernel_db.rs` storing `allowed_instances` **and**
  the sticky `name_map` per `ContextId`; first-touch loads from DB,
  fallback to "bind all registered" only when no row exists. The
  legacy `tool_filter` column + `ToolFilter` enum + post-filter in
  `llm_stream.rs::build_tool_definitions` (the one annotated
  "until M5 retires `ToolFilter`") are deleted. Rationale:
  `ContextToolBinding` already expresses allow-list semantics at the
  instance granularity operators actually care about; per-tool deny
  is a hook (D-07) not a filter. DB wipe acceptable per D-16.
- **D-55** — **`kj://kernel/tools` resource on `builtin.bindings`.**
  Discovery surface ("what's available to bind?") is a `BlockKind::
  Resource` read, not a search query — consistent with Phase 3's
  stance that long-form state lives in resources. Subscribable, so
  `register` / `unregister` / MCP-restart (unregister → re-register)
  flow into child Resource blocks via the D-43 coalescer path. The
  payload is instance-grouped (since `bind` operates on instances)
  with per-tool detail under each instance and a `bound: bool` flag
  relative to the calling context so the LLM can tell at read time
  what's already visible versus what a `bind` would add.

  **Namespace:** `kj://kernel/*` is reserved for binding-agnostic
  kernel-wide views (honest about what's installed regardless of the
  calling context's binding or ListTools hooks). `kj://context/*` is
  reserved for per-calling-context views that honor the binding and
  filters. Phase 5 ships only `kj://kernel/tools`; a future
  `kj://context/tools` ("what can I actually call right now, post-
  binding, post-ListTools") is a deliberate follow-up — reading both
  and diffing at runtime is the intended shape for "what would
  binding X give me that I don't already have" queries.
- **D-56** — **`HookPhase::ListTools` added in Phase 5 for tool-level
  visibility filtering.** `ContextToolBinding` is instance-level
  (binding an instance pulls in *all* its tools); without a per-tool
  filter, an operator can't have "`builtin.file` bound but `file_write`
  hidden" — the common persona case (e.g. read-only "planner" or
  "explorer" that want filesystem read without write). Rather than
  re-adding `denied_tools: HashSet<String>` to the binding (which
  would duplicate hook matching), reuse the Phase 4 match-action
  table by adding a new phase. Semantics:
  - Matches on raw `(instance_id, original_tool_name)` — **not** the
    sticky visible name (D-20). Hooks stay stable across resolution.
  - Only `Deny(reason)` and `Log` are admitted as actions;
    `ShortCircuit` and `Invoke` have no coherent list-filter
    semantics (reject at `hook_add` time with `McpError::Unsupported`).
  - `ListTools Deny` = **hidden AND uncallable through this binding**.
    The filter runs inside `Broker::list_visible_tools` before sticky
    resolution; denied tools never enter `name_map`, so a direct
    `call_tool` with the denied name returns
    `McpError::ToolNotFound`. From the binding's perspective the
    tool effectively does not exist.
  - Applies to `list_visible_tools` only, **not** `Broker::list_tools`
    nor the `kj://kernel/tools` Resource (D-55). Discovery is
    binding-agnostic and must be honest about what the kernel hosts;
    personas shape visibility *within* a binding, not kernel-wide.
  Rationale for adding a phase instead of a binding field: hooks
  already carry `match_instance` / `match_tool` globs plus
  `match_context` / `match_principal` equality — all of which are
  useful for persona definitions. Duplicating that machinery on
  `ContextToolBinding` would be waste.

## 7. Open questions per area

Questions that do not need to be answered *now* but should be resolved
before (or during) the phase that touches them. Planners: pick these
up and drive to a concrete choice with the user. Record the decision
by *appending* to §6 (new stable ID) and leaving a `[RESOLVED: <D-XX>]`
note here.

### Broker / identity
- What does a stale tool reference look like across a CRDT replay? Are
  notifications idempotent on replay?

### Bindings (Phase 5)
- Persistence schema: single `context_bindings` table
  `(context_id, allowed_instances, name_map, updated_at)` with JSON
  columns, or normalized rows per `(context_id, visible_name,
  instance_id, original_tool_name)`? JSON is simpler; normalized is
  queryable. Lean: JSON for v1, migrate if we ever need per-tool
  queries.
- When a persisted `allowed_instances` entry refers to an instance no
  longer registered (e.g. external MCP removed from kernel config
  between restarts): silently drop on load, emit a startup-log
  Notification, or surface as a stale-binding Resource? No silent
  fallbacks (CLAUDE.md) argues for at least the log.
- `set_binding` vs `bind`/`unbind` emission semantics: does the
  wholesale setter fire per-instance diffs (computed by comparing
  old-vs-new `allowed_instances`), or does it only fire a single
  ToolsChanged and let the pump diff against `list_tools`? Lean:
  compute the diff in `set_binding` and fire per-instance ToolsChanged
  so the existing pump does the rest.

### Notifications
- Coalescer policy defaults (window, max-in-window) — do we need
  per-kind tuning or is one default enough for v1?
- Should `Log` notifications default to `BlockKind::Notification` or a
  separate log buffer? [RESOLVED: D-34 — LLM-visible Notification blocks]

### Resources
- Subscription lifetime when a context drops — broker unsubscribes, yes;
  what about suspended/hibernated contexts? Do we persist subscriptions
  or require re-subscribe on resume? [RESOLVED: D-44 — subscriptions are
  live side effects; CRDT replay never re-subscribes, re-entry must
  re-issue via `builtin.resources.subscribe`]
- Resource content-type → block rendering: who owns the mapping? First
  cut can be naive (mime → existing BlockKind) but eventually wants the
  block content abstraction from §9. [PARTIAL: D-42 — payload holds both
  `text` and `blob_base64` with mime hint; naive rendering (truncated text
  or `[binary: N bytes]`) lands in Phase 3; richer mime→render is deferred
  to the block content abstraction in §9]
- Subscription replay semantics: subscription is a *side effect* tied
  to the live context, not replayed with CRDT ops. Child blocks produced
  by prior updates are CRDT data and do replay. Formalize this before
  phase 3. [RESOLVED: D-44 — formalized exactly as stated; subscriptions
  die with binding drop, child blocks replay]

### Hooks
- Evaluation order across phases when multiple hooks match: strictly
  priority-sorted, or grouped by some other axis?
  [RESOLVED: D-46 / §4.3 — priority ascending + insertion-order tiebreak.]
- Kaish hook envelope format — full request/result JSON? Subset? How do
  we serialize `CallContext` for the hook script?
- Hook-group atomicity: should a `PreCall` hook be able to guarantee a
  matching `PostCall` hook runs (transactional pairing for audit
  logging)? If yes, introduce `HookGroup` before phase 4.
  [DEFERRED: 2026-04-16 — Phase 4 ships independent phase evaluation
  per §4.3; revisit if an audit-compliance caller actually needs it.]

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

### Phase 2 — Notifications · **code complete, runner verification pending**

Plan: `~/.claude/plans/phase2-tool-notifications.md`

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

### Phase 3 — Resources · **code complete, runner verification pending**

Plan: `~/.claude/plans/we-re-going-to-circle-precious-yeti.md`

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

### Phase 4 — Hooks + admin · **code complete, runner verification pending**

Plan: `~/.claude/plans/let-s-discuss-the-open-twinkly-beaver.md`

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

### Phase 5 — Per-context binding management + persistence · **code complete, runner verification pending**

`builtin.bindings` admin MCP server exposes per-context tool curation
— the capability Phase 1 plumbed but never surfaced. Every context
today first-touch-populates with *every* registered instance
(`kernel.rs::dispatch_tool_via_broker` / `list_tool_defs_via_broker`);
operators have no ergonomic way to say "this context gets
`builtin.file` + `builtin.block` only." Phase 5 fixes that, makes the
resulting binding durable across kernel restart, and wires binding
mutation into the existing per-tool diff pump (D-35) so late injection
is visible on the next LLM turn with no new notification machinery.

Tool search — the original Phase 5 scope — moves to §9 as a follow-up.
Rationale: per-context curation is load-bearing (it's the point of
bindings); search is UX sugar on top and only earns its keep once the
curation surface exists.

Deliverables:
- `builtin.bindings` virtual MCP server (instance `builtin.bindings`):
  - Tools: `bind(instance_id)`, `unbind(instance_id)`, `show` (current
    binding for calling context).
  - Resource: `kj://kernel/tools` — per-instance listing of every
    kernel-registered instance with its tools and a `bound: bool`
    flag relative to the calling context. Subscribable; MCP-restart
    (unregister → re-register) and ordinary register/unregister
    surface as child Resource blocks via the Phase 3 coalescer (D-43).
  - Holds `Weak<Broker>` like `BuiltinResourcesServer` (D-41).
  - **No hook-evaluation carve-out.** Kernel restart is the escape
    hatch (hooks are in-memory), same recovery story as `builtin.hooks`
    deserves. D-51's carve-out on `builtin.hooks` is under separate
    review — not in scope here (§9 follow-up).
- Binding mutation triggers ToolsChanged. `Broker::bind`,
  `Broker::unbind`, and the existing `Broker::set_binding` run the
  per-context diff pump so `tool_added` / `tool_removed` notifications
  fire. Today only register/unregister fire diffs; this extends the
  machinery to binding changes without a new notification kind.
- **Persistent bindings via `KernelDb`.** Bindings (including the
  sticky `name_map` so LLM-visible names survive restart) persist
  alongside context metadata. First-touch loads from DB; only when a
  context has never been bound does the kernel fall back to "bind all
  registered instances." Legacy `tool_filter` column is dropped (DB
  wipe acceptable per D-16).
- **Retire `ToolFilter`.** The `llm_stream.rs::build_tool_definitions`
  post-filter and the `ToolFilter` enum across kaijutsu-types,
  kernel_db, kj, drift, and RPC are deleted. `ContextToolBinding`
  subsumes allow-list semantics at instance granularity; per-tool
  deny becomes a hook (D-07), not a filter. The TODO on
  `llm_stream.rs:33` ("until M5 retires `ToolFilter`") was always
  anticipating this phase.
- Sticky `name_map` test under live mutation and restart: `unbind →
  rebind` (MCP-restart analog) and DB-round-trip preserve the names
  the LLM has already seen.
- **`HookPhase::ListTools`** for tool-level visibility filtering
  within a binding (D-56). Extends `HookTables` with a fourth phase;
  `Broker::list_visible_tools` evaluates it before sticky resolution
  and strips matching tools from the candidate set. `builtin.hooks`
  admin server picks up `ListTools` as a fourth `phase` value with no
  new action types (only `Deny` / `Log`; `ShortCircuit` / `Invoke`
  reject at add time). Precursor for the persona feature
  (planner/coder/explorer/sound engineer archetypes — §9) which
  bundles binding + ListTools hooks into named personas.

Exit criteria:
1. Mid-session `bind(instance)` / `unbind(instance)` changes a live
   context's tool list; LLM sees `tool_added` / `tool_removed` blocks
   on the next turn (existing Phase 2 machinery; exercised
   end-to-end).
2. Sticky resolution (D-20) preserves previously-seen names across
   `unbind → rebind` within a single session, and across kernel
   restart.
3. Bindings survive kernel restart — a context curated to
   `builtin.file` only comes back curated after a kernel restart
   (workspace wipe acceptable; only the curation must persist across
   process restart).
4. `kj://kernel/tools` resource read returns every kernel instance
   with per-tool detail and per-calling-context `bound` flag; a
   `register` / `unregister` produces a subscription update that
   threads as a child block under the original read.
5. `ToolFilter` / `tool_filter` are gone from live code across the
   workspace. Retired names appear only in comments/doc strings that
   describe history (same allowance as D-16 exit wording).
6. An MCP-restart flow round-trip: `unbind(external.gpal)` →
   `bind(external.gpal)` after a simulated restart preserves the
   LLM-visible tool names (sticky test in production path).
7. A `ListTools Deny(match_instance="builtin.file",
   match_tool="file_write")` hook on a bound context hides
   `file_write` from `list_visible_tools`, causes a direct
   `call_tool("file_write", ...)` from that context to return
   `McpError::ToolNotFound`, and leaves `kj://kernel/tools`
   still showing `file_write` as present on the `builtin.file`
   instance (D-56: binding view is filtered; discovery is honest).

### Phase 6+ — Follow-ups (deferred)

See §9.

## 9. Out-of-scope but coherent follow-ups

These are not part of the five phases, but the refactor should produce a
shape that makes them easy. Record decisions here that would constrain
the current work.

- **Personas (binding + filter bundles).** Named archetypes that
  package a `ContextToolBinding` with a set of `ListTools` hooks
  (D-56) into a single "apply persona X to context Y" operation.
  Examples the user has described: `planner` (mostly read-only
  discovery + analysis), `coder` (editing tools), `explorer` (read
  + git), `sound engineer` (sound tools, no editing). Not designed
  yet; Phase 5's `ListTools` hook phase is the mechanism this will
  compose on top of. Likely surface: a `builtin.personas` admin
  server with `list` / `apply` / `define` tools and a `kj://personas`
  resource. Persistence probably alongside bindings in `KernelDb`.
- **Tool search across instances.** `builtin.tool_search` with
  keyword/substring scoring over `(name, description, tags)` as a v1;
  `kaijutsu-index` (ONNX + HNSW) vector search as a v2 once the
  long-standing flake (tech_debt item 7) is addressed. Use case: LLM
  discovers a tool not bound to the calling context and asks the
  operator (or itself via `builtin.bindings.bind`) to pull in the
  owning instance. Moved out of Phase 5 because per-context binding
  management is the prerequisite — search without the curation surface
  is unanchored UX.
- **D-51 carve-out retirement for `builtin.hooks`.** *[SHIPPED
  2026-04-17 alongside hook persistence below.]* The carve-out was
  justified by "no escape hatch short of kernel restart"; kernel
  restart is in fact the accepted escape hatch (hooks are in-memory).
  Simpler, safer model: no carve-out, same recovery. Phase 5 set
  the precedent (no carve-out for `builtin.bindings`); retirement is
  codified in D-51's retirement marker (§6). The real stance the
  retirement codifies: *admin recovery is out-of-band*. Today that's
  restart or `sqlite3 kernel.db "DELETE FROM hooks WHERE ..."`; the
  kernel process doesn't guard itself against its own admin
  configuration — the operator has a shell, and that's enough.
- **Hook persistence.** *[SHIPPED 2026-04-17. Plan:
  `~/.claude/plans/delightful-bubbling-crab.md`.]* `HookTables`
  previously lived in-memory only; kernel restart wiped every
  registered hook. Persistence now gives hooks the same durability
  story bindings got in Phase 5. Normalized `hooks` table in
  `KernelDb` mirrors `HookEntry` fields with the `HookActionWire`
  variant discriminator as a column and variant-specific nullable
  columns (per `feedback_sql_schema.md`, no JSON blobs). `Broker::
  set_db` eagerly hydrates at bootstrap via `row_to_entry` in
  `mcp/hook_persist.rs`; stale-reference rows (unknown builtin name,
  kaish body, shape violation) `tracing::warn!` + skip rather than
  brick the kernel. `hook_add` / `hook_remove` in the admin server
  call `persist_hook_insert` / `persist_hook_delete` after the
  in-memory mutation. Shipped with D-51 retirement so "admin servers
  are not special" holds before and after. End-to-end test:
  `hooks_persist_across_kernel_restart` in `broker_e2e.rs` — install
  a Deny via admin, drop kernel, stand up a new kernel against the
  same DB, verify the hook still fires.
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
- **2026-04-16** (Phase 2 execution — M1–M4 + M6, Amy + Claude Opus 4.7):
  Phase 2 code landed across four milestones. M1: `BlockKind::Notification`
  + `NotificationPayload` rolled out end-to-end (kaijutsu-types, CRDT
  including new `insert_notification_block_as` primitive on
  `SharedBlockStore`, capnp @29/@30 + NotificationKind/LogLevel enums,
  client+server RPC converters, app theme+format, LLM hydrator arm per
  D-34). M2: coalescer rewritten around `ObserveOutcome { PassThrough |
  StartWindow | Coalesced { so_far } }` with `flush()` returning the
  collapsed count (D-39). M3: broker pump per registered instance
  subscribes to `ServerNotification` streams; synthesizes per-tool
  `ToolAdded`/`ToolRemoved` diffs on register/unregister (D-35);
  `set_documents` setter threads `SharedBlockStore` in at bootstrap
  (D-37); `register_silently` suppresses the three builtin bootstrap
  ToolAdded events (D-38); flush timer tracked per
  `(InstanceId, NotifKind)` and aborted on unregister. M4:
  `late_registration_visible_next_turn` test locks "no cache" behavior —
  `build_tool_definitions` enumerates fresh via
  `list_tool_defs_via_broker` each call. Decisions D-34..D-39 recorded.

  Suite adds, organized by the question each test answers:
  - **"Do the types round-trip?"** 15 block/payload tests in
    `kaijutsu-types::block::tests` (kind parse + serde, payload summary
    lines across kinds, JSON + postcard roundtrips, `content_eq` field
    inclusion, `text` constructor leaves notification None, builder
    notification arm, `format_notification_for_llm` envelope + truncation
    + no-payload fallback, `NotificationKind` snake_case serde).
  - **"Does the coalescer shape work?"** 8 tests in
    `mcp::coalescer::tests` (passthrough within cap, `StartWindow` at
    N+1, `Coalesced { so_far }` monotonic, ToolsChanged never coalesces,
    window reset after elapsed, `flush` returns count and clears window,
    `flush` on passthrough-only returns None, flush on untouched key
    returns None).
  - **"Does the broker emit the right blocks?"** 7 tests in
    `mcp::broker::tests` covering exit criteria #1 / #3 / #4, D-35 per-
    tool diff, D-38 silent register, D-39 / R2 flush-timer abort on
    unregister, and ResourceUpdated being silently dropped in Phase 2.
  - **"Does the hydrator actually surface the block to the LLM?"** 3
    tests in `llm::tests::hydration` exercising the `BlockKind::
    Notification` arm through `hydrate_from_blocks` (not just the
    formatter): XML envelope shape, mid-turn flush discipline, System-
    role filter carve-out.
  - **"Is the wire format symmetric?"** 4 capnp roundtrip tests in
    `kaijutsu-client::rpc::tests`: full populated payload, minimal
    (`has_*`-flag / empty-string-sentinel discipline), every
    `NotificationKind` variant (catches ordinal aliasing), every
    `LogLevel` variant.
  - **"Does the full chain work end-to-end without the app?"** 2 tests
    in `kaijutsu-kernel/tests/broker_e2e.rs`:
    `late_registration_visible_next_turn` (exit #2) and
    `server_notification_reaches_llm_hydrator` — stitches register →
    CRDT → `query_blocks` → `hydrate_from_blocks` using only production
    APIs, so a shaky app UI cannot hide kernel-side regressions.

  Workspace tests green excluding the long-standing
  `kaijutsu-index::test_neighbors` flake. Runner verification (M5, exit
  scenarios against a live kaijutsu-runner on the GPU server) pending —
  tracked in §8 Phase 2 status.
- **2026-04-16** (Phase 1 closure, Amy + Claude Opus 4.6): Post-phase
  review caught that `cargo test --workspace` failed to compile —
  `crates/kaijutsu-server/tests/e2e_dispatch.rs` still imported the
  deleted `kaijutsu_kernel::tools::EngineArgs`. The M6 commit had
  only run `cargo test --lib`, hiding integration-test failures.
  Fix: trimmed 5 Tier-0 `EngineArgs::to_argv` tests from
  `e2e_dispatch.rs`; kept 4 Tier-3 shell-dispatch tests. Broker-layer
  invariants had zero direct tests — added 19 new tests (6 in
  `broker.rs`, 4 in `binding.rs`, 5 in `coalescer.rs`, 4 in new
  integration file `crates/kaijutsu-kernel/tests/broker_e2e.rs`)
  covering D-20 sticky resolution, D-27 policy enforcement (timeout /
  concurrency / size cap — the last closes exit criterion #8), D-06
  tool-removed, D-28 is_error→`ExecResult::failure` mapping, §5.3
  ToolsChanged never-coalesces rule. Suite now: 432 kernel lib +
  4 broker_e2e + 45 server tests. A closure-driven
  `MockServer: McpServerLike` fake lives test-only in `broker.rs` and
  the integration file. No live code references to deleted names
  remain; comment-only historical references preserved in
  `mcp/mod.rs` and `servers/external.rs` per doc-line-843 allowance.
- **2026-04-16** (Phase 3 execution — M1–M4 + M6, Amy + Claude Opus 4.7):
  Phase 3 code landed in four milestones plus doc closure. M1: `BlockKind::
  Resource` + `ResourcePayload` rolled out end-to-end (kaijutsu-types
  variant + payload + dual text/blob shape per D-42,
  `format_resource_for_llm` with `RESOURCE_CONTENT_HYDRATION_BUDGET = 2048`
  per D-34, CRDT `insert_resource_block` + kernel `insert_resource_block_as`
  with `Option<&BlockId>` parent primitive, capnp `BlockKind::resource @8`
  + `ResourcePayload` struct + `BlockSnapshot.resourcePayload @31 /
  hasResourcePayload @32`, client+server RPC converters, app theme
  `block_resource` + renderer + border, LLM hydrator `(_, BlockKind::
  Resource)` arm + System-role / empty-content carve-out updates).
  M2: `McpServerLike` gained four resource methods with
  `Err(McpError::Unsupported)` defaults; `KernelResource` / `KernelResourceList`
  / `KernelResourceContents` / `KernelReadResource` newtypes at the broker
  API boundary (D-10); `ExternalMcpServer` delegates via rmcp 1.4.0 `Peer`
  (`list_all_resources`, `read_resource`, `subscribe`, `unsubscribe`) with
  `UnsupportedMethod`→`McpError::Unsupported` special-cased (R8);
  coalescer key extended to `CoalesceKey { instance, kind, uri }` per
  D-40 plus `CoalescePolicy::per_kind_override` with
  `ResourceUpdated => 0` per D-45; `schedule_flush` + `FlushKey` updated
  to carry `Option<String>` URI. M3: broker gained `list_resources` /
  `read_resource` / `subscribe` / `unsubscribe` dispatch methods plus
  `subscriptions: Mutex<HashMap<ContextId, HashSet<(InstanceId, String)>>>`
  and `resource_parents: Mutex<HashMap<(ContextId, InstanceId, String),
  BlockId>>` state; `clear_binding` and `unregister` walk the subscription
  table and call `server.unsubscribe` best-effort on every entry (D-44);
  pump replaces the Phase 2 drop arm with
  `observe → schedule_resource_flush → handle_resource_flush`, which
  re-reads per subscribed context and emits a child `BlockKind::Resource`
  block threaded under the original (D-43). Re-read failure emits a
  single `BlockKind::Notification { kind: Log, level: Warn }` under the
  same parent, never a fake Resource block. `BuiltinResourcesServer`
  (instance `builtin.resources`, D-41) exposes `list` / `read` /
  `subscribe` / `unsubscribe` MCP tools; holds `Weak<Broker>` to avoid
  the Arc cycle; registered via `register_silently` in
  `Kernel::register_builtin_mcp_servers` alongside the three Phase 1
  builtins. `CallContext::system_for_context(ContextId)` helper added for
  broker-internal teardown calls. M4: `resource_updated_threads_child_block_and_llm_sees_it`
  in `tests/broker_e2e.rs` exercises read → subscribe → 25-event burst →
  exactly one coalesced child block → `<resource>` envelope in
  `hydrate_from_blocks` output → `clear_binding` unsubscribes cleanly —
  stitches every Phase 3 decision (D-40, D-41, D-42, D-43, D-44, D-45)
  through production APIs only. Decisions D-40..D-45 recorded.

  Suite adds, organized by the question each test answers:
  - **"Do the types round-trip?"** 14 tests in
    `kaijutsu-types::block::tests` (kind parse + serde + `is_resource`
    helper, payload summary for text and blob variants,
    `resource_block` constructor, child-under-parent constructor variant,
    `text` constructor preserves None default, `content_eq` field
    inclusion, JSON + postcard roundtrips on payload and full snapshot,
    builder `.resource_payload()` arm, `format_resource_for_llm`
    envelope / truncation / blob marker / no-payload fallback).
  - **"Is the wire format symmetric?"** 2 tests in
    `kaijutsu-client::rpc::tests` (full text-variant roundtrip with every
    optional field populated, blob-variant minimal roundtrip exercising
    each `has_*` flag discipline).
  - **"Does the coalescer key extension hold?"** 3 tests in
    `mcp::coalescer::tests` (`uri_windows_are_independent`,
    `uri_none_and_some_are_independent`,
    `resource_updated_has_no_pass_throughs` — locks D-40 per-URI
    independence and D-45 zero-pass-through contract).
  - **"Does the trait default to Unsupported?"** 1 test in
    `mcp::broker::tests` on a bare `BareServer` fake — all four resource
    methods must return `Err(McpError::Unsupported)` via trait defaults.
  - **"Does the broker route resource operations correctly?"** 8 tests
    in `mcp::broker::tests` — `read_resource_emits_block_in_context`
    (exit #1), `resource_updated_emits_child_block_under_parent` (exit #2),
    `resource_updated_burst_coalesces_to_one_child` (exit #3),
    `two_uris_coalesce_independently` (D-40),
    `clear_binding_unsubscribes_all_uris` (exit #4),
    `unregister_unsubscribes_bound_contexts` (D-44 via unregister path),
    `failed_reread_emits_log_not_resource` (D-43 failure path),
    `resource_updated_without_subscription_emits_nothing` (renamed from
    the Phase 2 drop-test; locks "subscription is the trigger, not the
    notification" contract).
  - **"Does the admin server round-trip through the broker?"** 2 tests
    in `mcp::servers::resources_builtin::tests`
    (`builtin_resources_server_subscribe_roundtrip` exercises
    `builtin.resources.subscribe` → `Broker::subscribe` → mock server
    sees the URI → `clear_binding` unsubscribes;
    `builtin_resources_server_unknown_tool_errors` for the
    `ToolNotFound` dispatch).
  - **"Does the full chain work end-to-end without the app?"** 1 test
    in `kaijutsu-kernel/tests/broker_e2e.rs`:
    `resource_updated_threads_child_block_and_llm_sees_it`. Uses only
    production APIs (`Broker::read_resource`, `Broker::subscribe`,
    `SharedBlockStore::query_blocks`, `hydrate_from_blocks`), so if this
    test passes the kernel-side Phase 3 story is real.

  Workspace totals: 1381 tests passing across all crates (baseline was
  ~1341 after Phase 2 closure; net +40 Phase 3 tests, excluding the
  `resource_updated_is_dropped_in_phase_2` test that was renamed in
  place). The long-standing `kaijutsu-index::test_neighbors` /
  `test_save_and_load` flakes remain unfixed (see
  `memory/tech_debt.md`). Runner verification (M5, exit scenarios
  against a live kaijutsu-runner on the GPU server) pending — tracked
  in §8 Phase 3 status. Wire format changed (capnp @31/@32 ordinals,
  new `resource @8` BlockKind variant); per D-16 no migration path —
  persisted contexts from Phase 2 need rebuilding.
- **2026-04-16** (Phase 3 pre-commit review, Amy + Claude Opus 4.7):
  Pre-commit peer review (Gemini 3.1 Pro + validator agent against
  broker.rs / coalescer.rs / resources_builtin.rs) caught three bugs
  in M3, fixed before landing:
  1. **`unregister` subscription-leak race.** `unregister` called
     `teardown_subscriptions_for_instance` *before*
     `self.instances.write().remove(id)`. A concurrent `subscribe` that
     slipped between teardown collecting its list and the instance
     removal recorded a row pointing at a vanished instance. Fix:
     remove from `instances` first (so new subscribes fail
     `InstanceNotFound`); then teardown using the taken Arc; then a
     defensive second sweep catches any subscribe that was already past
     `resolve_instance` when we removed the instance. Regression test:
     `broker::tests::subscribe_after_unregister_errors_and_leaves_no_row`.
  2. **Silent subscribe without prior read.** `Broker::subscribe`
     recorded the subscription in `self.subscriptions` but left
     `resource_parents` empty unless `read_resource` had run first.
     `handle_resource_flush` then silently filtered every update out
     via `parents.get(...).map(...)` returning `None`. The LLM saw
     "subscribed" success and zero updates — contradicts "no silent
     fallbacks" (CLAUDE.md, D-15). Fix: `Broker::subscribe` auto-reads
     when `resource_parents` has no entry for `(ctx, instance, uri)`,
     establishing a parent before registering the subscription.
     Regression test:
     `broker::tests::subscribe_without_prior_read_delivers_updates`.
  3. **N+1 reads on resource flush.** `handle_resource_flush` called
     `server.read_resource(uri, &sys).await` inside the per-context
     loop. N subscribers on the same URI produced N reads per flush —
     N-amplified external pressure, plus potential content divergence
     across subscribers if the resource changed mid-fanout. Fix: single
     read outside the loop, fan the fresh payload out to all targets;
     failure path emits one Log per subscriber under each parent.
     Attribution switches from `CallContext::system_for_context(ctx_id)`
     (per-subscriber) to `CallContext::system()` (broker-internal).
     Regression test:
     `broker::tests::flush_reads_once_for_all_subscribers`.

  `ResourceMock` gained a `read_count: AtomicUsize` to assert fan-out
  in the N+1 regression test. Kernel suite after fixes: 461 lib + 7
  broker_e2e (net +3 over the phase 3 feature commit). Phase 2
  `register_silently_suppresses_synthetic_tool_added` and the Phase 3
  tests all continue to pass. Gemini also flagged two lower-priority
  issues left as follow-ups: (a) `clear_binding` TOCTOU on instance
  re-lookup (best-effort teardown already tolerates mis-routes), and
  (b) `register` overwrite leaks the old pump `JoinHandle` instead of
  aborting — both tracked in `memory/tech_debt.md`.
- **2026-04-16** (Phase 4 execution — M1–M5 + M6, Amy + Claude Opus 4.7):
  Phase 4 code landed across five milestones plus doc closure. M1:
  `Broker::evaluate_phase` helper + `hook_matches` predicate wired into
  `Broker::call_tool` at three pinch points — PreCall before the server
  call, PostCall on `Ok`, OnError on `Err` (with a helper
  `run_on_error_then_err` that converts OnError ShortCircuit into a
  success result, per §4.3 evaluation law). `PhaseOutcome { Continue |
  ShortCircuit | Deny }` keeps the evaluator's return shape small.
  `HookBody::Builtin` refactored to `{ name: String, hook: Arc<dyn Hook> }`
  so admin inspection can surface the builtin name; `HookAction::Deny`
  refactored from `McpError` (not Clone) to `String` reason (tracing-only,
  broker unconditionally returns `McpError::Denied { by_hook }` per D-28).
  M2: OnNotification fires post-coalesce — one hook evaluation per
  emitted block via `build_notification_synth` producing
  `tool = "__notification.<kind>"` and an argument bag of
  `{kind, count, level, detail, tool}`. Wired into both
  `emit_for_bindings` (Log / PromptsChanged / ToolAdded / ToolRemoved /
  Coalesced) and `handle_resource_flush` (resource_updated success +
  D-43 log failure). M3: `crates/kaijutsu-kernel/src/mcp/hooks_builtin.rs`
  ships `BuiltinHookRegistry` mapping `&'static str` to
  `fn() -> Arc<dyn Hook>` factories; seeds `tracing_audit` (emits a
  TRACE event per invocation) and `no_op` (positive/negative test
  controls). Broker gains `builtin_hooks: BuiltinHookRegistry` field
  + accessor. M4: `crates/kaijutsu-kernel/src/mcp/servers/hooks_builtin.rs`
  is the admin MCP server at `builtin.hooks`; four tools delegate to
  `Broker::hooks()` / `builtin_hooks()`. `HookActionWire` tagged enum
  keeps `Arc<dyn Hook>` off the wire (D-50); `Kaish` rejects at add
  time; `hook_list` redacts body detail, `hook_inspect` returns it.
  Bootstrap registers via `register_silently` alongside the three
  Phase 1 builtins and Phase 3's `builtin.resources`. M5: `tokio::task_local!
  HOOK_DEPTH: Cell<u32>` + `HookDepthGuard` with `Drop`. `Broker::call_tool`
  splits into outer wrapper + `call_tool_inner`; outer installs the
  scope on first entry and reuses it on recursive re-entry so depth
  survives `broker.call_tool(...)` from inside an Invoke body. Default
  cap `MAX_HOOK_DEPTH = 4`; test-only `HOOK_DEPTH_OVERRIDE: OnceLock<u32>`
  lets fixtures drive the cap with smaller numbers. Decisions D-46..D-51
  recorded. D-51 is an emergent safety valve: `builtin.hooks` calls bypass
  hook evaluation entirely so a user can't lock themselves out of
  `hook_remove` with a `PreCall Deny *` hook.

  Suite adds, organized by the question each test answers:
  - **"Does PreCall / PostCall / OnError wiring work?"** 9 tests in
    `mcp::broker::tests` (`pre_call_deny_blocks_call`,
    `pre_call_shortcircuit_skips_server`, `post_call_fires_after_success`,
    `on_error_fires_on_server_error_not_post_call`,
    `on_error_shortcircuit_converts_error_to_success`,
    `hook_match_instance_and_tool_globs`,
    `hook_match_context_and_principal_filters`,
    `hook_priority_and_insertion_order_is_deterministic`,
    `log_hook_emits_tracing_event_not_block`,
    `short_circuit_emits_attribution_event`). Exit criteria #2 + #3.
  - **"Does OnNotification fire post-coalesce?"** 4 tests in
    `mcp::broker::tests` (`on_notification_fires_for_log_passthrough`,
    `on_notification_fires_once_per_emission_in_burst` — 5 passthrough +
    1 coalesced summary = 6 fires, `on_notification_deny_skips_emission`,
    `on_notification_fires_for_resource_flush_success_path`).
  - **"Does the builtin registry behave?"** 5 tests in
    `mcp::hooks_builtin::tests` (`registry_lists_known_names`,
    `registry_builds_tracing_audit`, `registry_builds_no_op`,
    `registry_unknown_name_returns_none`,
    `tracing_audit_emits_trace_event`). Exit criterion #1.
  - **"Does the admin surface round-trip?"** 5 tests in
    `mcp::servers::hooks_builtin::tests`
    (`admin_round_trip_with_builtin_log_hook` — exit criterion #4,
    `hook_add_unknown_builtin_rejects`, `hook_add_kaish_rejects`,
    `hook_list_filters_by_phase`, `hook_inspect_returns_body_detail`,
    `hook_remove_missing_is_not_an_error`).
  - **"Does the reentrancy cap enforce and self-heal?"** 4 tests in
    `mcp::broker::tests` (`reentrant_hook_exceeds_depth_cap` — exit
    criterion #5, `reentrant_hook_under_cap_succeeds`,
    `hook_depth_resets_across_calls`,
    `panicking_hook_body_does_not_leak_depth`).

  Workspace totals: 490 kernel lib + 7 broker_e2e (net +29 Phase 4
  tests vs the Phase 3 closure baseline). The long-standing
  `kaijutsu-index` flakes remain unfixed (tech_debt item 7).
  `HookAction::Deny` shape changed from `McpError` to `String`; this is
  the only visible API break vs the Phase 1 stub. Runner verification
  (exit criteria #1–#4 against a live `kaijutsu-runner` on the GPU
  server) pending — tracked in §8 Phase 4 status.
- **2026-04-17** (Phase 5 pre-plan reframe, Amy + Claude Opus 4.7):
  Phase 5 scope reshaped after reviewing what Phase 1 actually shipped
  versus what the doc's original Phase 5 assumed. Finding:
  `ContextToolBinding` is plumbed end-to-end and tested, but no
  operator/LLM-facing surface curates it — every context first-touch-
  populates with all registered instances, so per-context tool
  curation (the original motivation) is invisible. `ToolFilter` is
  semi-alive (post-filters in `llm_stream.rs::build_tool_definitions`
  with a TODO comment that literally says "until M5 retires
  `ToolFilter`"). Decision: drop tool search to §9, refocus Phase 5 on
  **per-context binding management + persistence**. Deliverables:
  `builtin.bindings` admin MCP server with `bind`/`unbind`/`show`
  tools and a `kj://kernel/tools` Resource (D-55), binding
  mutation extended to the per-tool diff pump (D-35 reuse, no new
  notification types), persistent bindings via `KernelDb` with
  `ToolFilter` retired workspace-wide, and sticky `name_map` survival
  across kernel restart + MCP restart. No hook-evaluation carve-out
  (D-53) — kernel restart is the universal escape hatch, consistent
  with the hook-server recovery story; D-51 is flagged for retirement
  review as a §9 follow-up. Decisions D-52..D-55 recorded. Phase 5
  exit criteria rewritten (6 criteria, all end-to-end scenarios
  through production APIs). No code changes this turn.
  Extended same turn after discussing per-tool filter semantics:
  added D-56 (`HookPhase::ListTools` for tool-level visibility
  filtering within a binding) and Phase 5 exit criterion #7.
  Motivation is the forthcoming persona feature — planner / coder /
  explorer / sound engineer archetypes that need "`builtin.file`
  bound but `file_write` hidden"-style filtering. Personas
  themselves moved to §9 as a follow-up that composes on top of
  bindings + ListTools.
- **2026-04-17** (Phase 5 execution — M1–M5 + M6, Amy + Claude Opus 4.7):
  Phase 5 code landed across five milestones plus doc closure. Plan:
  `~/.claude/plans/binary-roaming-unicorn.md`. M1: `HookPhase::ListTools`
  variant + `HookTables::list_tools` field + evaluator arm; `parse_phase`
  / `phase_to_str` / `phase_table_mut` / `hook_list` / `hook_inspect`
  all extended; `validate_action_for_phase` in `hooks_builtin` rejects
  `BuiltinInvoke` / `ShortCircuit` / `Kaish` for `ListTools` at add
  time. **Normalized** `KernelDb` schema per `feedback_sql_schema.md` —
  three tables: `context_bindings` (parent row + updated_at),
  `context_binding_instances` (ordered allow list, `(context_id,
  instance_id)` PK + `order_idx` for Vec semantics + index on
  `instance_id` for ops queries), `context_binding_names`
  (sticky `(visible, instance, original)` map). Transactional
  upsert / join-based get / cascade delete.
  M2: `Broker::set_binding` rewritten to diff old-vs-new
  `(instance, tool_name)` pairs via `binding_visible_tool_pairs`
  (uses cached `tool_snapshots`), write, persist (R1 mitigation —
  identical pair sets emit nothing); `bind(ctx, instance)` +
  `unbind(ctx, &instance)` as thin wrappers; `binding()` hydrates
  from DB on cache miss; `emit_for_bindings` refactored with shared
  `emit_into_context` helper + new `emit_for_context` that skips
  binding filter for mutation-specific emission;
  `apply_list_tools_filter` inside `list_visible_tools` walks
  `list_tools` hook entries and strips `Deny`-matched tools BEFORE
  sticky resolution (D-56 — denied tools never enter `name_map`, so
  they're uncallable as a side effect); `persist_binding` via
  `DbHandle` setter following D-37 precedent.
  M3: `BuiltinBindingsServer` at `builtin.bindings` with `bind` /
  `unbind` / `show` tools plus `kj://kernel/tools` resource
  (instance-grouped JSON payload with per-calling-context `bound`
  flag). Bridge task in `Kernel::register_builtin_mcp_servers`
  forwards kernel-level `KernelNotification::ToolsChanged` (newly
  published from `register_inner` / `unregister`) to the bindings
  server's `notif_tx` as `ServerNotification::ResourceUpdated`, so
  subscribers see updates via the Phase 3 coalescer pipeline. No
  hook-evaluation carve-out — D-53 verified by the
  `bindings_server_subject_to_hooks` unit test.
  M4: `ToolFilter` retirement across the workspace. Deleted: the
  enum in `kaijutsu-types::enums` + re-exports; `llm/config.rs`
  `default_tools` field, `ToolConfig` type, and `ToolFilter`
  re-export; `llm/toml_config.rs` `ToolFilterToml` + conversion (TOML
  `[providers.*.default_tools]` blocks now parse-ignore for
  backwards compat); `drift.rs` `tool_filter` field + `configure_tools`;
  `llm_stream.rs::build_tool_definitions` post-filter (the "until M5
  retires ToolFilter" TODO); `kj/parse.rs::parse_tool_filter_spec` +
  tests; `kj/context.rs` `--tool-filter` CLI flag; `kj/preset.rs`
  `--tool-filter` and `tool_filter` display; `kj/fork.rs` preset
  tool_filter copying; `kernel_db.rs` `tool_filter` columns on both
  `contexts` and `presets`, all INSERT/SELECT/UPDATE references
  including renumbered placeholders, `tool_filter_to_sql` /
  `tool_filter_from_sql` helpers, `update_tool_filter` method, row
  reader column-index realignment; client `ClientToolFilter` enum +
  4 RPC methods + actor `RpcCommand` variants + handle delegations;
  server `tool_filter: None` stubs + 4 RPC impl methods + 2 capnp
  helpers; capnp ordinals @69/@70/@85/@86 stubbed as
  `...Removed @N () -> ();` (wire break per D-16). Workspace
  `cargo check` + `grep -rn 'ToolFilter'` on live code both clean.
  M5: 3 new `broker_e2e.rs` tests covering DB-round-trip scenarios
  the unit-level M2 tests couldn't reach: `binding_persists_across_
  kernel_restart` (exit #3 — drop kernel A with curated binding,
  stand up kernel B against same DB, binding hydrates),
  `list_tools_deny_hides_and_blocks_but_keeps_discovery_honest`
  (exit #7 — three-way D-56 invariant: list omits, name_map empty,
  `kj://kernel/tools` still honest), `kernel_tools_resource_end_to_
  end` (exit #4 — all six builtins appear, per-calling-context
  `bound` flag). Setup extended with `setup_with_db` that threads
  a `DbHandle` via `broker.set_db()` and inserts a matching
  `ContextRow` for the context-bindings FK.

  Suite adds, organized by the question each test answers:
  - **"Do the types + schema compile and roundtrip?"** 5 tests in
    `kernel_db::tests` (`context_binding_roundtrip_preserves_order_and_
    sticky_names`, `context_binding_get_absent_returns_none`,
    `context_binding_upsert_replaces_wholesale`,
    `context_binding_delete_returns_whether_existed`,
    `context_binding_cascades_on_context_delete`); 2 tests in
    `servers::hooks_builtin::tests` (`hook_add_list_tools_rejects_
    invoke_and_shortcircuit`, `hook_add_list_tools_accepts_deny_and_log`).
  - **"Does the broker mutation + filter shape work?"** 7 tests in
    `mcp::broker::tests` — `bind_emits_tool_added_for_newly_visible_tools`,
    `unbind_emits_tool_removed`,
    `set_binding_diff_fires_per_added_and_removed_instance`,
    `set_binding_no_emission_when_pairs_unchanged` (R1 guard),
    `list_tools_deny_strips_tool_from_visible_set`,
    `list_tools_deny_makes_tool_uncallable_via_this_binding`,
    `list_tools_log_does_not_strip`.
  - **"Does the admin server round-trip?"** 7 tests in
    `servers::bindings_builtin::tests` (`bindings_admin_bind_roundtrip`,
    `bindings_admin_unbind_roundtrip`,
    `bindings_admin_show_without_binding_returns_empty`,
    `kernel_tools_resource_read_returns_all_instances`,
    `kernel_tools_resource_bound_flag_reflects_context`,
    `bindings_server_subject_to_hooks` (D-53 confirm),
    `read_resource_rejects_unknown_uri`).
  - **"Does the full chain work end-to-end without the app?"** 3 tests
    in `broker_e2e.rs` (listed above).

  Workspace totals: 499 kernel lib (from pre-M4 511, net -12 from
  `ToolFilter` test removals) + 10 broker_e2e (from 7, net +3) + 45
  server + 54 client + 228 types + other crates. All workspace tests
  pass; long-standing `kaijutsu-index` flakes remain unfixed (tech_debt
  item 7). Wire format broken (capnp @69/@70/@85/@86 stub rename +
  `ToolFilterConfig` now unused); DB schema broken (tool_filter
  columns gone); both acceptable per D-16. Memory recorded:
  `feedback_sql_schema.md` ("prefer normalized SQL schemas over JSON
  blob columns"). Runner verification (exit criteria #1–#7 against a
  live `kaijutsu-runner` on the GPU server) pending — tracked in §8
  Phase 5 status.
- **2026-04-17** (doc update, Amy + Claude Opus 4.7): Added hook
  persistence as a §9 follow-up, paired with the existing D-51
  carve-out retirement bullet. Conversation clarified that "admin
  recovery is out-of-band" is the real stance — kernel restart today
  because hooks are in-memory, sqlite surgery + restart if hooks are
  ever persisted — so the "no carve-out" posture holds in either
  world. Verified current state: no `hooks` table exists in
  `kernel_db.rs` (tables are kernel / workspaces / workspace_paths /
  presets / documents / contexts / context_edges / oplog /
  doc_snapshots / input_oplog / input_doc_snapshots / context_shell /
  context_bindings (+ \_instances / \_names) / context_env). No code
  changes this turn.
- **2026-04-17** (hook persistence + D-51 retirement execution, Amy +
  Claude Opus 4.7): Shipped hook persistence and retired the D-51
  carve-out in a single pass. Plan:
  `~/.claude/plans/delightful-bubbling-crab.md`. Six milestones: M1
  schema + DB methods (normalized `hooks` table with `insertion_idx`
  managed internally via `MAX(insertion_idx) + 1` inside the INSERT;
  `insert_hook` / `delete_hook` / `load_all_hooks` on `KernelDb`);
  M2 broker wiring (new `mcp/hook_persist.rs` module owns the
  `HookEntry`↔`HookRow` conversion with a `RowParseError` taxonomy;
  `Broker::set_db` now eagerly hydrates; `persist_hook_insert` /
  `persist_hook_delete` added); M3 admin server writes persist after
  each in-memory mutation; M4 deleted the `params.instance ==
  "builtin.hooks"` short-circuit in `evaluate_phase`; M5 end-to-end
  `hooks_persist_across_kernel_restart` test in `broker_e2e.rs`
  stitches admin → persist → SQLite → drop kernel → new kernel →
  hydrate → hook fires; M6 doc closure. Also fixed a latent bug in
  `hook_remove`: the admin handler only walked four phase tables,
  skipping `list_tools` — now walks all five.

  Suite adds, organized by the question each test answers:
  - **"Do the DB methods round-trip?"** 4 tests in
    `kernel_db::tests` (`hook_insert_roundtrip_preserves_all_action_variants`,
    `hook_delete_returns_whether_existed`,
    `load_all_hooks_orders_by_phase_priority_then_insertion_idx`,
    `hook_insert_same_id_errors`).
  - **"Does the conversion layer work?"** 4 tests in
    `mcp::hook_persist::tests` (`builtin_invoke_round_trip`,
    `unknown_builtin_fails_to_reconstruct`,
    `shortcircuit_round_trip`, `kaish_row_returns_unsupported`).
  - **"Does the broker wire up correctly?"** 4 tests in
    `mcp::broker::tests` (`hooks_hydrate_on_set_db`,
    `persist_hook_insert_writes_row`, `persist_hook_delete_removes_row`,
    `hydrate_skips_unknown_builtin_and_keeps_valid_rows`).
  - **"Is the admin surface subject to hooks post-retirement?"**
    1 test in `mcp::servers::hooks_builtin::tests`
    (`hooks_admin_is_subject_to_hooks`) — installs a PreCall
    `Deny(*)` directly on the broker, then asserts a `hook_list`
    admin call returns `McpError::Denied` with the expected
    `by_hook` id. Symmetric to Phase 5's
    `bindings_server_subject_to_hooks`.
  - **"Does the full chain work end-to-end?"** 1 test in
    `tests/broker_e2e.rs` (`hooks_persist_across_kernel_restart`):
    admin `hook_add` on kernel A registers a PreCall Deny on
    `builtin.block`, verify `block_list` is Denied, drop kernel A,
    stand up kernel B against same DB, verify `block_list` still
    Denied with the same `by_hook` id — the persisted row
    round-tripped through `set_db`-time hydration.

  Workspace totals: 512 kernel lib + 11 broker_e2e + 45 server + 54
  client + 228 types + others. All workspace tests pass; long-standing
  `kaijutsu-index` flakes remain unfixed (tech_debt item 7). DB schema
  extended (new `hooks` table via `CREATE TABLE IF NOT EXISTS` —
  forward-compatible for existing Phase 5 DBs; no migration needed).
  No wire format changes. D-51 marked `[RETIRED: 2026-04-17]` in §6
  with pointer to the replacement test.
