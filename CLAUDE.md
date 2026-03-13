# 会術 Kaijutsu

*"The Art of Meeting"*

An agentic coding system for teams: Bevy 0.18 client + Cap'n Proto over SSH server.

## Before You Touch Code

**Always start by reading these two files** — they are the source of truth:

1. **`kaijutsu.capnp`** — the wire protocol. All RPC methods, capability interfaces, struct shapes
2. **`crates/kaijutsu-types/src/`** — the leaf crate with zero internal deps. All shared types:
   - `ids.rs` — typed UUIDs: `ContextId`, `KernelId`, `PrincipalId`, `SessionId` (UUIDv7, `PrefixResolvable`)
   - `block.rs` — `BlockId` (Copy), `BlockSnapshot`, `BlockHeader`, `BlockStatus`, `Role`
   - `enums.rs` — `BlockKind`, `ToolKind`, `DriftKind`, `ForkKind`, `EdgeKind`, `ToolFilter`, `ConsentMode`
   - `context.rs`, `kernel.rs`, `session.rs`, `principal.rs` — birth certificates

Then read `docs/kernel-model.md` for the conceptual model.

## Architecture

```
┌──────────────────────────────────────────────────────────────────┐
│ Server: ONE shared kernel (会の場所) per server                   │
│  Kernel → VFS, ToolRegistry, LlmRegistry, FlowBus, DriftRouter  │
│  EmbeddedKaish (per-connection) → KaijutsuBackend → BlockStore   │
│  KernelDb (SQLite: contexts, edges, presets, workspaces)         │
│                     SSH + Cap'n Proto                             │
└──────────────────────┬───────────────────────────────────────────┘
        ┌──────────────┼──────────────┐
 kaijutsu-app    kaijutsu-mcp    External (Claude Code, etc.)
 (Bevy client)   (MCP stdio)
```

## Crate Structure

```
kaijutsu/
├── crates/
│   ├── kaijutsu-types/      # Leaf crate — READ FIRST. All shared types
│   ├── kaijutsu-crdt/       # BlockStore, BlockDocument (re-exports types)
│   ├── kaijutsu-kernel/     # Kernel, VFS, tools, LLM, drift, kj builtin
│   ├── kaijutsu-server/     # SSH server, EmbeddedKaish, KaijutsuBackend
│   ├── kaijutsu-client/     # RPC client, ActorHandle (Send+Sync)
│   ├── kaijutsu-app/        # Bevy 0.18 GUI (Vello text, tiling WM)
│   ├── kaijutsu-mcp/        # MCP server (30 tools via rmcp)
│   ├── kaijutsu-index/      # Semantic vector indexing (ONNX + HNSW)
│   └── kaijutsu-telemetry/  # OpenTelemetry (W3C context propagation)
├── kaijutsu.capnp            # READ FIRST. Wire protocol schema
└── docs/                     # kernel-model.md, drift.md, etc.
```

## Related Repos

| Repo | Purpose |
|------|---------|
| `~/src/kaish` | kaish shell (see LANGUAGE.md) |
| `~/src/diamond-types-extended` | Forked CRDT engine (Map/Set/Register/Text) |
| `~/src/bevy` | Bevy 0.18 source |
| `~/src/wt/pr6` | bevy_vello fork (local path dep) |

## Documentation

| Doc | Purpose |
|-----|---------|
| [docs/kernel-model.md](docs/kernel-model.md) | **Authoritative kernel model** |
| [docs/drift.md](docs/drift.md) | Cross-context communication |
| [docs/block-tools.md](docs/block-tools.md) | CRDT block interface |
| [docs/diamond-types-fork.md](docs/diamond-types-fork.md) | Why we forked diamond-types |
| [docs/telemetry.md](docs/telemetry.md) | OTel integration |

## Quick Start

```bash
cargo run -p kaijutsu-server -- add-key ~/.ssh/id_ed25519.pub --nick amy --admin
cargo run -p kaijutsu-server          # Terminal 1
cargo run -p kaijutsu-app             # Terminal 2 (graphical session)
cargo check -p kaijutsu-app           # Fast incremental check (~4s)
```

## Autonomous Development Loop

Bevy client needs a graphical session. Claude works headless via `contrib/kj`:

```bash
# User starts in graphical session:
./contrib/kaijutsu-runner.sh

# Claude uses:
./contrib/kj status|tail|pause|resume|rebuild|restart
```

When Claude runs in a graphical session, use `mcp__bevy_brp__*` tools directly.

## Git Conventions

- Working on main (early development)
- Add files by name, avoid wildcards (parallel sessions)
- Ephemeral markdown files are not usually committed
- Set Co-Authored-By in commit messages

## Bevy 0.18 Quick Reference

| Old (0.14-0.17) | New (0.18) |
|-----------------|------------|
| `#[derive(Event)]` | `#[derive(Message)]` |
| `EventReader<T>` / `EventWriter<T>` | `MessageReader<T>` / `MessageWriter<T>` |
| `events.send(x)` | `messages.write(x)` |
| `app.add_event::<T>()` | `app.add_message::<T>()` |
| `ChildBuilder` | `ChildSpawnerCommands` |
| `BorderColor(color)` | `BorderColor::all(color)` |
| `query.get_single()` | `query.single()` |

Bevy source: `~/src/bevy`, examples at `~/src/bevy/examples/`

## Key Design Decisions

- **No LCD subpixel AA** — OLED/rotated displays break it, Apple dropped it in 2018
- **Vello text rendering** via `bevy_vello` + Parley. `VelloTextAnchor::TopLeft`, Bevy flex layout, `ContentSize` for sizing
- **Never use `BackgroundColor` on entities overlapping Vello text** — Bevy UI renders ON TOP of the Vello canvas texture
- **No children on `UiVelloText` nodes** — Taffy treats them as flex containers, ignoring ContentSize (blocks collapse to ~3px)
- **Contexts ARE documents** — no separate document concept, `ContextId` identifies both
- **Model is immutable on a context** — fork to change it
- **postcard for CRDT serialization** on wire — `#[serde(default)]` on Option fields (positional format, can't skip)
- **Cap'n Proto RPC**: use `pry!()` for sync extraction, `Promise::from_future` for async (`?` doesn't work in Promise methods)
