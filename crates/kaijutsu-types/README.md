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

## Type Mapping: Old → New

| Current Type | Location | Replacement |
|-------------|----------|-------------|
| `ssh::Identity { nick, display_name, is_admin }` | kaijutsu-server | `Principal` |
| `rpc::Identity { username, display_name }` | kaijutsu-kernel | `Principal` |
| `client::Identity { username, display_name }` | kaijutsu-client | `Principal` |
| `BlockId.document_id: String` | kaijutsu-crdt | `BlockId.context_id: ContextId` |
| `BlockId.agent_id: String` | kaijutsu-crdt | `BlockId.agent_id: PrincipalId` |
| `BlockSnapshot.author: String` | kaijutsu-crdt | `BlockSnapshot.author: PrincipalId` |
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

### kaijutsu-client

- [ ] Replace `Identity` struct with `Principal`
- [ ] Store `PrincipalId` from server handshake

### kaijutsu-app

- [ ] Update block rendering to use `PrincipalId` for author display
- [ ] Update constellation to use typed `ContextId` throughout

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
use kaijutsu_types::{Principal, PrincipalId, BlockId, ContextId};

let amy = Principal::new("amy", "Amy Tobey");
let ctx = ContextId::new();
let block = BlockId::new(ctx, amy.id, 1);

// System-generated content
let system_block = BlockId::new(ctx, PrincipalId::system(), 1);
```
