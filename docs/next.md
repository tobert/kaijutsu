# Kaijutsu: What's Next

*Last updated: 2026-01-16*

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
| **kaish (execution engine)** | 🚧 L0-L4 complete (lexer, parser, runtime, REPL) |
| kaish embedding | 📋 Planned (blocked on kaish L5-L6) |
| Lease system | 📋 Planned |
| Checkpoint system | 📋 Planned |

## Next Up

### Immediate: Kernel Integration

**Dependency:** kaish (~/src/kaish) provides the execution engine. kaijutsu embeds kaish-kernel.

| kaish Layer | Status | kaijutsu Blocker |
|-------------|--------|------------------|
| L0-L4: Lexer, Parser, Runtime, REPL | ✅ Complete | Can start embedding |
| L5: VFS | 📋 Planned | Needed for file operations |
| L6: Tools | 📋 Planned | Needed for builtins |
| L10: State | 📋 Planned | Needed for persistence |
| L11: RPC | 📋 Planned | Optional (we embed directly) |

**kaijutsu work (parallel with kaish):**

1. **Embed kaish-kernel** — Add kaish as workspace dependency, wire to execute()
2. **Kernel state storage** — SQLite + filesystem per kernel (kaijutsu-side)
3. **VFS mounting** — Coordinate with kaish VFS, attach worktrees
4. **Wire console to kernel** — RPC streaming output via kaish
5. **Lease system** — Who holds the pen, UI indicator (kaijutsu-side)

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
- **kaish:** `~/src/kaish/docs/BUILD.md` — Execution engine build plan and layer dependencies
- **kaish language:** `~/src/kaish/docs/LANGUAGE.md` — Shell language specification
- **Bevy 0.18:** `~/src/bevy` — UI framework source
