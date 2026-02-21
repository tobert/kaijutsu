# kaijutsu-types

Shared identity, kernel, and block types for Kaijutsu — the relational foundation.

This crate defines the **canonical type vocabulary** that all other kaijutsu crates
align to. It has no internal kaijutsu dependencies — a pure leaf crate. This is
未来 (mirai): it defines where we're going, not where we've been.

## Entity-Relationship Model

```
Kernel (KernelId)                     ← 会場 the meeting place
    ├── founded by Principal          ← starts the meeting, not "owner"
    ├── hosts many Contexts           ← conversations within
    └── does NOT fork                 ← only contexts fork

Principal (PrincipalId)               ← anyone who acts
    ├── authenticates via Credential
    ├── founds Kernel
    ├── joins Kernel                  ← participant, peer to founder
    ├── creates Context
    ├── authors Block (BlockId = ContextId + PrincipalId + seq)
    └── opens Session (SessionId)

Context (ContextId)                   ← a conversation, NOT a document
    ├── belongs to a Kernel
    ├── contains Blocks               ← the universal content atom
    ├── forks / threads               ← isolated or parallel work
    └── drifts to other Contexts      ← cross-context knowledge transfer
```

**Design note — kernels don't fork.** The kernel is 会場 (kaijou), the meeting
place where monsters gather. It's singular. The founder starts the meeting;
participants join as peers. Forking creates complexity without benefit at the
kernel level — contexts fork for isolated exploration, and drift handles
cross-context communication. One meeting, many conversations.

**Well-known sentinel:** `PrincipalId::system()` — deterministic UUIDv5 for
kernel-generated blocks (shell output, system messages).

## Key Types

| Type | Purpose |
|------|---------|
| `PrincipalId` | Who (user, model, system) |
| `KernelId` | Which kernel instance |
| `ContextId` | Which context |
| `SessionId` | Which connection session |
| `Principal` | Full identity (id + username + display_name) |
| `Kernel` | Kernel birth certificate (founder + label) |
| `Context` | Context metadata (kernel + lineage + label) |
| `BlockId` | Unique block address (context + agent + seq) |
| `BlockHeader` | Lightweight Copy-able subset for DAG indexing |
| `BlockSnapshot` | Serializable block state |
| `Session` | Session birth certificate (who + where + when) |

## Block Model

### BlockKind + Role

`BlockKind` has 6 variants covering *what* a block is:

| BlockKind | Role | Purpose |
|-----------|------|---------|
| `Text` | User/Model | Content (user message, model response) |
| `Thinking` | Model | Model reasoning (collapsible) |
| `ToolCall` | User/Model | Request to execute something |
| `ToolResult` | Tool | Execution response |
| `Drift` | System | Cross-context transfer |
| `File` | Asset | File content tracked in a context |

`Role` has 5 variants: `User`, `Model`, `System`, `Tool`, `Asset`.

### Companion Enums

Mechanism metadata lives in companion enums on BlockSnapshot:

- **`ToolKind`** on ToolCall/ToolResult — which execution engine:
  - `Shell` — kaish command execution (default)
  - `Mcp` — MCP tool invocation
  - `Builtin` — kernel builtins

- **`DriftKind`** on Drift — how content transferred:
  - `Push` / `Pull` / `Merge` / `Distill` / `Commit`

### BlockHeader

Lightweight Copy struct for DAG indexing and LWW conflict resolution:

| Field | Type | Purpose |
|-------|------|---------|
| `id` | `BlockId` | Identity |
| `parent_id` | `Option<BlockId>` | DAG edge |
| `role` | `Role` | Who |
| `kind` | `BlockKind` | What |
| `status` | `Status` | Lifecycle (Pending/Running/Done/Error) |
| `compacted` | `bool` | Superseded by compaction summary |
| `collapsed` | `bool` | UI collapse state (LWW mutable) |
| `created_at` | `u64` | Wall-clock Unix millis |
| `updated_at` | `u64` | Lamport timestamp for LWW resolution |
| `tool_kind` | `Option<ToolKind>` | Execution engine |
| `exit_code` | `Option<i32>` | Tool exit code |
| `is_error` | `bool` | Error flag |

### BlockSnapshot

Full serializable block state. Extends BlockHeader with non-Copy fields:

- `content: String` — primary text
- `tool_name`, `tool_input`, `tool_call_id` — tool metadata
- `display_hint` — per-viewer rendering hint
- `source_context`, `source_model`, `drift_kind` — drift provenance
- `file_path` — logical path for File blocks
- `order_key` — fractional index for sibling ordering (set by BlockStore)

Named constructors: `text()`, `thinking()`, `tool_call()`, `tool_result()`,
`tool_result_with_hint()`, `drift()`, `file()`. Also `BlockSnapshotBuilder`.

`is_shell()` convenience: `tool_kind == Some(Shell) && kind.is_tool()`.

## Per-Crate Migration Status

### kaijutsu-crdt — done

- [x] Depends on kaijutsu-types, re-exports all block/ID/enum types
- [x] `block.rs` is a thin re-export module
- [x] `ids.rs` re-exports + shim functions for legacy compat
- [x] `BlockId` uses `ContextId` + `PrincipalId`
- [x] `BlockSnapshot.author` removed — use `author()` method
- [x] `ShellCommand`/`ShellOutput` unified into `ToolCall`/`ToolResult` + `ToolKind`
- [x] New `BlockStore` + `BlockContent` (per-block DTE architecture)

### kaijutsu-kernel — pending

- [ ] Replace `Identity` struct with `Principal`
- [ ] Replace `KernelState.id: String` with `KernelId`, add `founder: PrincipalId`
- [ ] Update agent registration to use `PrincipalId`
- [ ] Migrate from `BlockDocument` to `BlockStore`

### kaijutsu-server — pending

- [ ] Replace `ssh::Identity` with `Principal`
- [ ] Map SSH fingerprint → `PrincipalId` at connection time
- [ ] Replace hardcoded `"server"` agent_id with `PrincipalId::system()`
- [ ] Update `shell_execute` for `ToolCall`/`ToolResult` + `ToolKind::Shell`

### kaijutsu-client — pending

- [ ] Replace `Identity` struct with `Principal`
- [ ] Store `PrincipalId` from server handshake

### kaijutsu-app — pending

- [ ] Use `PrincipalId` for author display
- [ ] Replace `BlockKind::ShellCommand`/`ShellOutput` match arms with `is_shell()`
- [ ] Constellation uses typed `ContextId` throughout

### kaijutsu-mcp — pending

- [ ] Update tool implementations to pass `PrincipalId` for authorship

## Usage

```rust
use kaijutsu_types::{Principal, PrincipalId, Kernel, Context, KernelId};
use kaijutsu_types::{BlockId, BlockSnapshot, ContextId, Role, ToolKind};

// Amy starts a kernel
let amy = Principal::new("amy", "Amy Tobey");
let kernel = Kernel::new(amy.id, Some("team-dev".into()));

// Create a context within the kernel
let ctx = Context::new(kernel.id, Some("default".into()), None, amy.id);

// User typed a shell command (author is amy.id via BlockId)
let cmd = BlockSnapshot::tool_call(
    BlockId::new(ctx.id, amy.id, 1),
    None,
    ToolKind::Shell,
    "shell",
    serde_json::json!({"command": "ls -la"}),
    Role::User,
);
assert!(cmd.is_shell());
assert_eq!(cmd.author(), amy.id);

// Track a file in the context
let file = BlockSnapshot::file(
    BlockId::new(ctx.id, amy.id, 2),
    None,
    "/src/main.rs",
    "fn main() {}",
);
assert_eq!(file.role, Role::Asset);
assert_eq!(file.file_path, Some("/src/main.rs".to_string()));

// System-generated content
let system_block = BlockId::new(ctx.id, PrincipalId::system(), 1);
```
