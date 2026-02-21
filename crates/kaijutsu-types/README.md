# kaijutsu-types

Shared identity and block types for Kaijutsu — the relational foundation.

This crate defines the **typed identity model** that replaces string-based IDs
scattered across the codebase. It has no internal kaijutsu dependencies — a pure
leaf crate.

## Why This Crate Exists

The identity model accumulated organically: three separate `Identity` structs with
inconsistent field names, string-based IDs where UUIDs should be, a `display_name`
bug that silently drops real names, and a hardcoded `"server"` agent_id on every block.

`kaijutsu-types` provides one vocabulary, one set of types, with full serde and
postcard support.

## Entity-Relationship Model

```
Principal (PrincipalId)  ──── authenticates via ──── Credential
    │                                                 (SshKey today)
    ├── owns ──── Kernel (KernelId)
    │                 │
    │            contains ──── Context (ContextId)
    │                              │
    │                         contains ──── Block (BlockId)
    │
    └── opens ──── Session (SessionId)
```

**Well-known sentinel:** `PrincipalId::system()` — deterministic UUIDv5 for
kernel-generated blocks (shell output, system messages).

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

The `Role` on the block already captures *who* initiated it (user typed `ls` vs
model requested shell execution). `ToolKind` captures *which engine*. No separate
BlockKind variants needed.

**Migration notes for `match block.kind` arms:**
- `BlockKind::ShellCommand` → `BlockKind::ToolCall` where `tool_kind == Some(ToolKind::Shell)`
- `BlockKind::ShellOutput` → `BlockKind::ToolResult` where `tool_kind == Some(ToolKind::Shell)`
- UI rendering that keys off `ShellCommand`/`ShellOutput` (the `$ ` prefix, no-border
  style, display_hint formatting) should use `BlockSnapshot::is_shell()` instead
- The `is_shell()` convenience method checks `tool_kind == Some(Shell) && kind.is_tool()`

## Type Mapping: Old → New

| Current Type | Location | Replacement |
|-------------|----------|-------------|
| `ssh::Identity { nick, display_name, is_admin }` | kaijutsu-server | `Principal` |
| `rpc::Identity { username, display_name }` | kaijutsu-kernel | `Principal` |
| `client::Identity { username, display_name }` | kaijutsu-client | `Principal` |
| `BlockId.document_id: String` | kaijutsu-crdt | `BlockId.context_id: ContextId` |
| `BlockId.agent_id: String` | kaijutsu-crdt | `BlockId.agent_id: PrincipalId` |
| `BlockSnapshot.author: String` | kaijutsu-crdt | `BlockSnapshot.author: PrincipalId` |
| `BlockKind::ShellCommand` | kaijutsu-crdt | `BlockKind::ToolCall` + `ToolKind::Shell` |
| `BlockKind::ShellOutput` | kaijutsu-crdt | `BlockKind::ToolResult` + `ToolKind::Shell` |
| `KernelState.id: String` | kaijutsu-kernel | `KernelId` |
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
- [ ] Replace `KernelState.id: String` with `KernelId`
- [ ] Replace `ContextManager.kernel_id: String` with `KernelId`
- [ ] Update agent registration to use `PrincipalId`

### kaijutsu-server

- [ ] Replace `ssh::Identity` with `Principal`
- [ ] Add `uuid` column to `auth.db` (SQLite migration)
- [ ] Map SSH fingerprint → `PrincipalId` at connection time
- [ ] Replace hardcoded `"server"` agent_id with `PrincipalId::system()`
- [ ] Update `shell_execute` to create `ToolCall`/`ToolResult` blocks with `ToolKind::Shell`
- [ ] Remove `ShellCommand`/`ShellOutput` block creation paths

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

The Cap'n Proto schema will need:

```capnp
struct Identity {
  principalId @0 :Data;       # 16-byte PrincipalId (new)
  username @1 :Text;
  displayName @2 :Text;
}
```

`BlockId` on wire becomes three fields:
- `contextId @N :Data;` (16 bytes)
- `agentId @N :Data;` (16 bytes)
- `seq @N :UInt64;`

Block snapshot gains `toolKind` field, loses `shellCommand`/`shellOutput` kind values.

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
use kaijutsu_types::{Principal, PrincipalId, BlockId, ContextId, ToolKind};
use kaijutsu_types::{BlockSnapshot, Role};

let amy = Principal::new("amy", "Amy Tobey");
let ctx = ContextId::new();

// User typed a shell command
let cmd = BlockSnapshot::tool_call(
    BlockId::new(ctx, amy.id, 1),
    None,
    ToolKind::Shell,
    "shell",
    serde_json::json!({"command": "ls -la"}),
    Role::User,
    amy.id,
);
assert!(cmd.is_shell());

// Model requested an MCP tool
let tool = BlockSnapshot::tool_call(
    BlockId::new(ctx, amy.id, 2),
    None,
    ToolKind::Mcp,
    "read_file",
    serde_json::json!({"path": "/etc/hosts"}),
    Role::Model,
    amy.id,
);
assert!(!tool.is_shell());

// System-generated content
let system_block = BlockId::new(ctx, PrincipalId::system(), 1);
```
