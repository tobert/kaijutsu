# Rig Migration Plan: anthropic-api → rig-core

**Hard cut, no backwards compatibility. Focused scope: core migration + model switching + configurable tools.**

## Scope

1. Replace `anthropic-api` with `rig-core`
2. Context forking to switch models (Claude ↔ Gemini ↔ Ollama)
3. Configurable tool→model wiring (currently: all equipped tools → all models)

## Current Tool Wiring

```
ToolRegistry.list_equipped()  →  build_tool_definitions()  →  StreamRequest.with_tools()
                                      ↓
                              Equipped tools filtered by kernel's ToolConfig
                              (equipped flag + tool filter)
```

**Key files:**
- `rpc.rs:3836-3860` — `build_tool_definitions()` collects equipped + filtered tools
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
- `fork(name)` — Creates isolated kernel copy with fresh LlmRegistry, new VFS, new FlowBus
- `thread(name)` — Creates lightweight kernel sharing VFS and FlowBus with parent

Both return a new `Kernel` capability that can have different providers registered.

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

Now filters tools through both `equipped` status AND kernel's `tool_config`:

```rust
async fn build_tool_definitions(kernel: &Arc<Kernel>) -> Vec<ToolDefinition> {
    let registry = kernel.tools().read().await;
    let tool_config = kernel.tool_config().await;

    registry.list_equipped()
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

## Future Work

- [ ] Load llm.rhai at server startup to initialize providers
- [ ] RPC to set default provider/model on a kernel
- [ ] RPC to modify tool filter at runtime
- [ ] Remove global `equipped` flag from ToolInfo (replace with ToolConfig only)
