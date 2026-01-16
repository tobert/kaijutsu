# Kaijutsu: What's Next

*Last updated: 2026-01-16 (kaish fe506f2 — L6 Tools complete)*

## Current State

### Completed

**Phase 1: Bootstrap**
- Bevy 0.18 shell with isekai theme
- Mode system (Normal/Insert/Command)
- Sacred input bar, sidebar, context view
- j/k navigation, collapsible DAG blocks

**Phase 2: Server Connection**
- Workspace restructure: client lib, server, app crates
- Cap'n Proto RPC over TCP (SSH skeleton exists)
- Integration tests passing
- UI wired: slash commands, messages, connection state

**Phase 3: Quake Console (UI)**
- Toggle with backtick, height presets
- Local echo working
- Frame overlay with 9-slice

**Design: Kernel Model**
- Established kernel as the fundamental primitive
- Defined VFS mounting, fork/thread, lease, checkpoint, consent modes
- See [docs/06-kernel-model.md](./06-kernel-model.md) for full specification

**Schema Migration**
- Cap'n Proto schema rewritten (Kernel-native interfaces)
- Server, client, app code updated
- Flattened model (Kernel IS the thing, no indirection)
- All tests passing

### Implementation Status

| Component | Status |
|-----------|--------|
| Kernel model design | ✅ Complete |
| Cap'n Proto schema | ✅ Complete |
| Server kernel handlers | ✅ Basic impl |
| Client kernel API | ✅ Basic impl |
| Server kernel storage | 📋 Planned |
| Client kernel UI | 📋 Planned |
| **kaish (execution engine)** | ✅ L0-L6 solid (305 tests) |
| kaish embedding | 🚧 Ready to start |
| Lease system | 📋 Planned |
| Checkpoint system | 📋 Planned |

## Next Up

### Immediate: Kernel Integration

**Dependency:** kaish (~/src/kaish) provides the execution engine. kaijutsu embeds kaish-kernel.

#### Interface Ownership

| Interface | Owner | Purpose |
|-----------|-------|---------|
| `kaish.capnp::Kernel` | **kaish** | Execution: parse, eval, tools, VFS, MCP, state, blobs |
| `kaijutsu.capnp::World` | **kaijutsu** | Multi-kernel orchestration |
| `kaijutsu.capnp::Kernel` | **kaijutsu** | Collaboration: lease, consent, fork/thread, checkpoint, messaging |

kaijutsu's `Kernel.execute()` delegates to the embedded kaish kernel. kaijutsu adds collaboration on top (lease, consent, checkpoint, messaging).

#### kaish Layer Dependencies

| kaish Layer | Status | kaijutsu Blocker |
|-------------|--------|------------------|
| L0-L4: Lexer, Parser, Interpreter, REPL | ✅ Complete (257 kernel tests) | **Unblocked** |
| L5: VFS | ✅ Complete (29 tests) | **Unblocked** |
| L6: Tools | ✅ Complete (33 tests, wired to REPL) | **Unblocked** |
| L7: Job Scheduler | 📋 Next | Needed for pipelines, background jobs |
| L10: State | 📋 Planned | Needed for persistence |
| L11: RPC | 📋 Planned | Optional (we embed directly) |
| L14: context-emit | 📋 Planned | Needed for AI context generation |

**kaijutsu work (unblocked):**

1. **Embed kaish-kernel** — Add kaish as workspace dependency, wire `execute()` through
   ```rust
   // kaijutsu-server wraps kaish-kernel
   let kaish = kaish_kernel::Kernel::new();
   let client = kaish_kernel::EmbeddedClient::new(kaish);
   // Kernel.execute() → client.execute() → kaish interpreter
   ```
2. **Wire console to kernel** — RPC streaming output via embedded kaish
3. **Lease system** — Who holds the pen, UI indicator (kaijutsu-side)
4. **Kernel state storage** — SQLite + filesystem per kernel (kaijutsu-side, or defer to kaish L10)

**Parallel with kaish development:**

5. **VFS mounting** — kaish L5 complete; LocalFs at `/mnt/local`, MemoryFs at `/scratch`
6. **Pipelines** — Wait for kaish L7 Job Scheduler for `cmd1 | cmd2` support
7. **Context generation** — Use kaish L14 `context-emit` when available

### Phase 4: Kernel Operations

1. **Fork/Thread** — Create new kernels from existing
2. **Checkpoint** — Distill history into summaries
3. **Consent modes** — Collaborative vs autonomous
4. **Context generation** — `kaish context-emit` for fresh payloads

### Phase 5: Polish

- Rich structured output rendering
- History navigation
- Interrupt (Ctrl+C)
- Drag-to-resize console
- Kernel discovery/listing UI

## Quick Start

```bash
# Terminal 1: Server
cargo run -p kaijutsu-server

# Terminal 2: Client
cargo run -p kaijutsu-app
```

## Crate Structure

```
kaijutsu/
├── crates/
│   ├── kaijutsu-client/     # RPC client library
│   ├── kaijutsu-server/     # TCP/SSH server
│   └── kaijutsu-app/        # Bevy GUI
└── docs/
    ├── 06-kernel-model.md   # ✅ Authoritative kernel design
    ├── 05-lexicon-exploration.md
    └── next.md              # This file
```

## Key Reading

- **Start here:** [docs/06-kernel-model.md](./06-kernel-model.md) — Full kernel model specification (includes kaish integration)
- **Background:** [docs/05-lexicon-exploration.md](./05-lexicon-exploration.md) — Design philosophy and decisions
- **kaish BUILD:** `~/src/kaish/docs/BUILD.md` — Execution engine build plan and layer dependencies
- **kaish ARCHITECTURE:** `~/src/kaish/docs/ARCHITECTURE.md` — Interface ownership, embedding pattern
- **kaish LANGUAGE:** `~/src/kaish/docs/LANGUAGE.md` — Shell language specification
- **Bevy 0.18:** `~/src/bevy` — UI framework source
