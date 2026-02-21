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

Context (ContextId)                   ← a conversation, a document
    ├── belongs to a Kernel
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
| `ContextId` | Which context (= document) |
| `SessionId` | Which connection session |
| `Principal` | Full identity (id + username + display_name) |
| `Kernel` | Kernel birth certificate (founder + label) |
| `Context` | Context metadata (kernel + lineage + label) |
| `BlockId` | Unique block address (context + agent + seq) |
| `BlockHeader` | Lightweight Copy-able subset for DAG indexing |
| `BlockSnapshot` | Serializable block state |
| `Session` | Session birth certificate (who + where + when) |

## BlockKind + ToolKind + DriftKind

`BlockKind` is deliberately small — 5 variants covering *what* a block is:

| BlockKind | Purpose |
|-----------|---------|
| `Text` | Content (user message, model response) |
| `Thinking` | Model reasoning (collapsible) |
| `ToolCall` | Request to execute something |
| `ToolResult` | Execution response |
| `Drift` | Cross-context transfer |

Mechanism metadata lives in companion enums on BlockSnapshot:

- **`ToolKind`** on ToolCall/ToolResult — which execution engine:
  - `Shell` — kaish command execution (default)
  - `Mcp` — MCP tool invocation
  - `Builtin` — kernel builtins

- **`DriftKind`** on Drift — how content transferred:
  - `Push` / `Pull` / `Merge` / `Distill` / `Commit`

### Shell → ToolCall migration

The current codebase has `BlockKind::ShellCommand` and `BlockKind::ShellOutput`
as separate variants. In kaijutsu-types, these are unified:

| Old (kaijutsu-crdt) | New (kaijutsu-types) |
|---------------------|---------------------|
| `BlockKind::ShellCommand` | `BlockKind::ToolCall` + `ToolKind::Shell` + `Role::User` |
| `BlockKind::ShellOutput` | `BlockKind::ToolResult` + `ToolKind::Shell` |

`BlockSnapshot::is_shell()` is the convenience check for rendering.

## Type Mapping: Old → New

| Current Type | Location | Replacement |
|-------------|----------|-------------|
| `ssh::Identity` | kaijutsu-server | `Principal` |
| `rpc::Identity` | kaijutsu-kernel | `Principal` |
| `client::Identity` | kaijutsu-client | `Principal` |
| `BlockId.document_id: String` | kaijutsu-crdt | `BlockId.context_id: ContextId` |
| `BlockId.agent_id: String` | kaijutsu-crdt | `BlockId.agent_id: PrincipalId` |
| `BlockSnapshot.author: String` | kaijutsu-crdt | removed — use `author()` method (`id.agent_id`) |
| `BlockKind::ShellCommand` | kaijutsu-crdt | `BlockKind::ToolCall` + `ToolKind::Shell` |
| `BlockKind::ShellOutput` | kaijutsu-crdt | `BlockKind::ToolResult` + `ToolKind::Shell` |
| `KernelState.id: String` | kaijutsu-kernel | `KernelId` + `Kernel` (metadata) |
| `ContextManager.kernel_id: String` | kaijutsu-kernel | `KernelId` |
| `ContextHandle.document_id: String` | kaijutsu-kernel | `ContextId` |
| `source_context: Option<String>` | kaijutsu-crdt | `source_context: Option<ContextId>` |

## Per-Crate Migration Checklist

### kaijutsu-crdt

- [ ] Add `kaijutsu-types` dependency
- [ ] Re-export types from `kaijutsu-types` (or alias)
- [ ] Keep `ids.rs` with `from_document_id()` and `recover()` as migration shims
- [ ] Migrate `block.rs` `BlockId` to use `ContextId` + `PrincipalId`
- [ ] Migrate `BlockSnapshot.author` from `String` to `PrincipalId`
- [ ] Remove `ShellCommand`/`ShellOutput` from `BlockKind`, add `ToolKind` field
- [ ] Update `BlockDocument` to use typed `BlockId`
- [ ] Delete old `KernelId`/`ContextId` once consumers are migrated

### kaijutsu-kernel

- [ ] Replace `Identity` struct with `Principal`
- [ ] Replace `KernelState.id: String` with `KernelId`, add `founder: PrincipalId`
- [ ] Replace `ContextManager.kernel_id: String` with `KernelId`
- [ ] Update agent registration to use `PrincipalId`
- [ ] Remove kernel fork/thread — only context fork/thread remains
- [ ] Rename runtime `Kernel` struct (e.g. `KernelEngine`) to avoid conflict with `kaijutsu_types::Kernel`

### kaijutsu-server

- [ ] Replace `ssh::Identity` with `Principal`
- [ ] Add `uuid` column to `auth.db` (SQLite migration)
- [ ] Map SSH fingerprint → `PrincipalId` at connection time
- [ ] Replace hardcoded `"server"` agent_id with `PrincipalId::system()`
- [ ] Update `shell_execute` to create `ToolCall`/`ToolResult` blocks with `ToolKind::Shell`
- [ ] `attach_kernel` populates `Kernel` metadata with authenticated principal as founder
- [ ] Remove kernel fork/thread RPC methods from schema

### kaijutsu-client

- [ ] Replace `Identity` struct with `Principal`
- [ ] Store `PrincipalId` from server handshake

### kaijutsu-app

- [ ] Update block rendering to use `PrincipalId` for author display
- [ ] Update constellation to use typed `ContextId` throughout
- [ ] Replace `BlockKind::ShellCommand`/`ShellOutput` match arms with `is_shell()` checks
- [ ] Shell rendering ($ prefix, no border, display_hint) keys off `is_shell()`

### kaijutsu-mcp

- [ ] Update tool implementations to pass `PrincipalId` for authorship
- [ ] Replace string-based identity in remote state

## Wire Format Changes (kaijutsu.capnp)

```capnp
struct Identity {
  principalId @0 :Data;       # 16-byte PrincipalId
  username @1 :Text;
  displayName @2 :Text;
}
```

`BlockId` on wire becomes three fields:
- `contextId @N :Data;` (16 bytes)
- `agentId @N :Data;` (16 bytes)
- `seq @N :UInt64;`

Block snapshot gains `toolKind` field, loses `shellCommand`/`shellOutput` kind values.

Remove `fork` and `thread` from `Kernel` interface — contexts handle isolation.

## Database Schema Changes

### auth.db

Add a `uuid BLOB` column to the users table. Backfill existing users with
`PrincipalId::new()`. The SSH fingerprint → principal mapping becomes:
fingerprint → row → `PrincipalId`.

### Kernel data (BlockDocument snapshots)

**Clean-break strategy:** Since the kernel data format is still in flux,
the simplest migration is to wipe kernel data and start fresh. The auth.db
is the only persistent state that needs a proper schema migration.

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

// Model requested an MCP tool
let tool = BlockSnapshot::tool_call(
    BlockId::new(ctx.id, amy.id, 2),
    None,
    ToolKind::Mcp,
    "read_file",
    serde_json::json!({"path": "/etc/hosts"}),
    Role::Model,
);
assert!(!tool.is_shell());

// System-generated content
let system_block = BlockId::new(ctx.id, PrincipalId::system(), 1);
```
