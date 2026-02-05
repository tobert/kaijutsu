# Rig Migration Plan: anthropic-api → rig-core

**Hard cut, no backwards compatibility. Focused scope: core migration + model switching + configurable tools.**

## Scope

1. Replace `anthropic-api` with `rig-core`
2. Context forking to switch models (Claude ↔ Gemini ↔ Ollama)
3. Configurable tool→model wiring (currently: all equipped tools → all models)

## Tool Wiring

```
ToolRegistry.list_with_engines()  →  build_tool_definitions()  →  StreamRequest.with_tools()
                                          ↓
                                  Tools filtered by kernel's ToolConfig
                                  (ToolFilter: All / AllowList / DenyList)
```

**Key files:**
- `rpc.rs` — `build_tool_definitions()` collects engine-backed + filtered tools
- `kernel.rs` — `tool_config: RwLock<ToolConfig>` for per-kernel filtering

## Phase 1: Core rig Integration ✅ COMPLETE

**Commits:**
- `6924143 feat(llm): migrate from anthropic-api to rig-core`
- `b3d3736 fix(llm): proper state machine for streaming block transitions`

The streaming fix adds proper `TextStart`/`TextEnd`/`ThinkingStart`/`ThinkingEnd` events for CRDT block boundaries (identified in Gemini code review).

### 1.1 Dependencies ✅

**Cargo.toml (workspace):**
```toml
[workspace.dependencies]
rig-core = { version = "0.30", features = ["anthropic", "gemini", "openai", "ollama"] }
```

### 1.2 Delete `llm/anthropic.rs` ✅

Removed entirely (543 lines → 0).

### 1.3 Rewrite `llm/mod.rs` ✅

Wrapped rig providers in a unified enum:

```rust
pub enum RigProvider {
    Anthropic(anthropic::Client),
    Gemini(gemini::Client),
    OpenAI(openai::Client),
    Ollama(ollama::Client),
}
```

### 1.4 Adapt `llm/stream.rs` ✅

RigStreamAdapter converts rig SSE streams to kaijutsu StreamEvents with proper block boundary events.

## Phase 2: Context Forking for Model Switching ✅ COMPLETE

### 2.1 Implement RPC fork/thread Stubs ✅ COMPLETE

**Location:** `rpc.rs:936-1108`

**Implementation:**
- `fork(name)` — Creates isolated kernel copy with cloned LlmRegistry, new VFS, new FlowBus
- `thread(name)` — Creates lightweight kernel sharing VFS and FlowBus, cloned LlmRegistry

Both inherit the parent's runtime LLM config (including `setDefaultProvider`/`setDefaultModel` changes) and can modify independently.

### 2.2 Example Flow (Now Supported!)

```
CTX_A (claude-haiku) — fork("sonnet-branch")
    │
    ▼
CTX_B (new kernel + new document)
    └─ Register Anthropic provider with claude-sonnet-4
    └─ Set as default → model switch complete
```

## Phase 3: Tool Configuration Per Context ✅ COMPLETE

### 3.1 Tool Config Lives in Kernel ✅

Each kernel instance (and its forks) has its own tool configuration:

```rust
pub struct Kernel {
    // ... existing fields
    tool_config: RwLock<ToolConfig>,
}

pub struct ToolConfig {
    pub filter: ToolFilter,
}

pub enum ToolFilter {
    All,                        // All registered tools
    AllowList(HashSet<String>), // Only these
    DenyList(HashSet<String>),  // All except these
}
```

### 3.2 Fork Inherits + Can Modify ✅

```rust
impl Kernel {
    pub async fn tool_config(&self) -> ToolConfig { ... }
    pub async fn set_tool_filter(&self, filter: ToolFilter) { ... }
    pub async fn tool_allowed(&self, tool_name: &str) -> bool { ... }
}
```

Fork and thread inherit parent's tool_config but can modify independently.

### 3.3 Configuration in Rhai ✅

**Created:** `assets/defaults/llm.rhai`

```rhai
let default_provider = "anthropic";

let providers = #{
    anthropic: #{
        enabled: true,
        api_key_env: "ANTHROPIC_API_KEY",
        default_model: "claude-haiku-4-5-20251001",
        default_tools: #{ type: "all" },
    },
    gemini: #{
        enabled: false,
        api_key_env: "GEMINI_API_KEY",
        default_model: "gemini-2.0-flash",
        default_tools: #{ type: "all" },
    },
    // ... openai, ollama configs
};
```

### 3.4 Update `build_tool_definitions()` ✅

Now filters tools through kernel's `tool_config` — tools with registered engines are available:

```rust
async fn build_tool_definitions(kernel: &Arc<Kernel>) -> Vec<ToolDefinition> {
    let registry = kernel.tools().read().await;
    let tool_config = kernel.tool_config().await;

    registry.list_with_engines()
        .into_iter()
        .filter(|info| tool_config.allows(&info.name))
        .map(|info| /* ... */)
        .collect()
}
```

## Phase 4: Stream Consumer Refactor ✅ COMPLETE

Updated in Phase 1 commit - rpc.rs now uses RigProvider.

## Files Changed

### Phase 1 (Complete) ✅

**Deleted:**
- `crates/kaijutsu-kernel/src/llm/anthropic.rs` ✅

**Added:**
- `crates/kaijutsu-kernel/src/llm/config.rs` ✅

**Modified:**
- `Cargo.toml` ✅
- `crates/kaijutsu-kernel/Cargo.toml` ✅
- `crates/kaijutsu-kernel/src/llm/mod.rs` ✅
- `crates/kaijutsu-kernel/src/llm/stream.rs` ✅
- `crates/kaijutsu-kernel/src/kernel.rs` ✅
- `crates/kaijutsu-kernel/src/lib.rs` ✅
- `crates/kaijutsu-server/src/rpc.rs` ✅

### Phase 2 (Complete) ✅

**Modified:**
- `crates/kaijutsu-server/src/rpc.rs` — implement `fork()` and `thread()` RPC methods

### Phase 3 (Complete) ✅

**Added:**
- `assets/defaults/llm.rhai` — provider configuration template

**Modified:**
- `crates/kaijutsu-kernel/src/kernel.rs` — add `tool_config: RwLock<ToolConfig>` field + methods
- `crates/kaijutsu-server/src/rpc.rs` — `build_tool_definitions()` uses ToolConfig filter

## Thinking Blocks: VERIFIED ✓

rig-core has full thinking support:
- `streaming.rs:64` — `ThinkingDelta { thinking }`
- `completion.rs:232` — `Content::Thinking { thinking, signature }`
- Full `ThinkingState` tracking

## Abstraction Reduction ✅

**Dropped these kaijutsu types (use rig directly):**
- `Role` → `rig::providers::anthropic::Role`
- `ContentBlock` → `rig::providers::anthropic::Content`
- `Message`, `MessageContent` → `rig::providers::anthropic::Message`
- `LlmProvider` trait → rig client types directly
- `CompletionRequest/Response` → rig types

**Kept (thin adapter):**
- `StreamEvent` enum — maps rig SSE to CRDT operations
- `RigStreamAdapter` — converts provider streams → `StreamEvent`

## Verification ✅

### Phase 1 ✅
1. `cargo test -p kaijutsu-kernel` — 195 tests pass
2. Streaming state machine emits proper Start/End events

### Phase 2 ✅
1. `cargo check -p kaijutsu-server` — compiles
2. Fork/thread RPC methods implemented and compile

### Phase 3 ✅
1. llm.rhai config created
2. Tool filtering works per-kernel via ToolConfig
3. `cargo check --workspace` — passes

## Phase 5: Rhai-Driven LLM Init + RPC Config + Unified Tool Filtering ✅ COMPLETE

All four "Future Work" items are now implemented.

### 5.1 Load llm.rhai at Startup ✅

- **Removed `LlmProvider` trait** — `RigProvider` is the only implementation
- **`LlmRegistry` now stores `Arc<RigProvider>`** (concrete type)
- **Created `llm/rhai_config.rs`** — evaluates `llm.rhai` script, returns `LlmConfig`
- **Created `initialize_llm_registry()`** — builds `LlmRegistry` from Rhai config
- **Removed `ServerState.llm_provider`** — each kernel loads its own LLM config
- **Added `enabled` field to `ProviderConfig`** — skip disabled providers

### 5.2 RPC: Provider/Model Configuration ✅

Cap'n Proto methods @83-@85:
- `getLlmConfig()` → returns `LlmConfigInfo` (default provider, model, provider list)
- `setDefaultProvider(name)` → switch kernel's default LLM provider
- `setDefaultModel(provider, model)` → switch kernel's default model

Client types: `LlmProviderInfo`, `LlmConfigInfo` in `kaijutsu-client/src/rpc.rs`.

### 5.3 RPC: Tool Filter Management ✅

Cap'n Proto methods @86-@87:
- `getToolFilter()` → returns `ToolFilterConfig` (union: all / allowList / denyList)
- `setToolFilter(filter)` → replace kernel's tool filter

Client type: `ClientToolFilter` in `kaijutsu-client/src/rpc.rs`.

### 5.4 Remove `equipped` Flag ✅

- **Removed `equipped: bool` from `ToolInfo`** — tools with engines are available
- **Removed `equip()`/`unequip()`/`list_equipped()`** from `ToolRegistry` and `Kernel`
- **Added `list_with_engines()`** — tools with registered engines (replaces `list_equipped`)
- **Reimplemented equip/unequip RPC** as `ToolConfig` operations (backward compatible)
- **`executeTool` checks `tool_allowed()`** before execution
- **`listEquipment`** populates `equipped` field from `tool_config.allows()`

### Verification ✅

- `cargo check --workspace` — passes
- `cargo test -p kaijutsu-kernel` — 203 tests pass
- `cargo test --workspace` (excluding flaky GPU tests) — 338 tests pass, 0 failures
