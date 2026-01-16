# 会術 Kaijutsu

*"The Art of Meeting"*

An agentic coding system for teams with two parts: a Bevy 0.18 client, and a Cap'n Proto over SSH server.

**The way of Kaijutsu:** Engineer context for each prompt and anchor models so they can be brilliant.

## Quick Start

```bash
# Terminal 1: Server
cargo run -p kaijutsu-server

# Terminal 2: Client
cargo run -p kaijutsu-app
```

## Architecture

```
┌─────────────────────────────────────────────────────────────────────┐
│                         Server                                       │
│  ┌─────────────┐  ┌─────────────┐  ┌─────────────┐                 │
│  │   Kernels   │  │    kaish    │  │  Cap'n Proto│                 │
│  │  (state +   │  │ interpreter │  │   Handlers  │                 │
│  │    VFS)     │  │             │  │             │                 │
│  └──────┬──────┘  └──────┬──────┘  └──────┬──────┘                 │
│         └────────────────┴────────────────┘                         │
│                          │                                          │
│                   SSH + Cap'n Proto                                 │
└──────────────────────────┼──────────────────────────────────────────┘
                           │
┌──────────────────────────┼──────────────────────────────────────────┐
│                    Kaijutsu Client (Bevy)                           │
│  ┌──────────────┐  ┌──────────────┐  ┌──────────────┐              │
│  │   UI Shell   │  │   Context    │  │    RPC       │              │
│  │  (isekai)    │  │    View      │  │   Client     │              │
│  └──────────────┘  └──────────────┘  └──────────────┘              │
└─────────────────────────────────────────────────────────────────────┘
```

## Documentation

| Doc | Purpose |
|-----|---------|
| [docs/06-kernel-model.md](docs/06-kernel-model.md) | **Authoritative kernel model — start here** |
| [docs/next.md](docs/next.md) | Current status, what's next |
| [docs/05-lexicon-exploration.md](docs/05-lexicon-exploration.md) | Philosophical background, design decisions |

Legacy docs (01-04) exist but are outdated. Use 06-kernel-model.md as the source of truth.

## Core Concepts

### Kernel

The kernel is the fundamental primitive. Everything is a kernel.

A kernel:
- Owns `/` in its VFS (virtual filesystem)
- Can mount worktrees, repos, other kernels at paths like `/mnt/project`
- Has a lease (who holds "the pen" for mutations)
- Has a consent mode (collaborative vs autonomous)
- Can checkpoint (distill history into summaries)
- Can be forked (heavy copy, isolated) or threaded (light, shared VFS)

See [docs/06-kernel-model.md](docs/06-kernel-model.md) for full details.

### Context Generation

Context isn't stored, it's *generated*. When a context payload is needed (for Claude, for export), kaish walks the kernel state and mounted VFS to emit a fresh payload. Mounts determine what's visible.

### Lease Model

A kernel has one lease holder at a time. Human enters insert mode → acquires lease. AI generates → holds lease. Escape → releases. This prevents interleaving chaos in collaborative sessions.

### Fork vs Thread

| Op | State | VFS | Use case |
|----|-------|-----|----------|
| `fork` | Deep copy | Snapshot | Isolated exploration |
| `thread` | New, linked | Shared refs | Parallel work on same codebase |

### Context View (Not Chat)

The main UI area manages cognitive load, not just chat history.
- Nested DAG blocks (messages → tool calls → results)
- Collapse/expand
- Navigate with `j/k`
- Scale to 5-10 concurrent agents

### Mode System (vim-style)

| Mode | Purpose | Enter | Exit |
|------|---------|-------|------|
| Normal | Navigate, read | `Esc` | - |
| Insert | Type in input | `i` | `Esc` |
| Command | Slash commands | `:` | `Esc` |

## Key Patterns

### RPC (Cap'n Proto)

- Object-capability model over SSH
- `attachKernel()` returns a `Kernel` capability
- Real-time streaming for messages and kernel output

### Remote Console (kaish)

- Quake-style drop-down (`` ` `` to toggle)
- Runs **kaish** (会sh) — the gathering shell
- Connected to the current kernel
- Structured output rendering

## Crate Structure

```
kaijutsu/
├── crates/
│   ├── kaijutsu-client/     # RPC client library
│   ├── kaijutsu-server/     # TCP/SSH server
│   └── kaijutsu-app/        # Bevy GUI
└── docs/
    ├── 06-kernel-model.md   # ✅ Start here
    ├── 05-lexicon-exploration.md
    └── next.md
```

## Related Repos

| Repo | Purpose |
|------|---------|
| `~/src/kaish` | kaish shell (LANGUAGE.md) |
| `~/src/bevy` | Bevy 0.18 source |

## Development

```bash
# Run the client
cargo run -p kaijutsu-app

# Run the server
cargo run -p kaijutsu-server

# Run with debug logging
RUST_LOG=debug cargo run -p kaijutsu-app

# Check Bevy 0.18 examples
cd ~/src/bevy && cargo run --example standard_widgets
```

## Git Conventions

Follow typical open source conventions for commits. This project is still in early phases of development and we are working on main.

- Add files by name, avoid wildcards
- We often work in parallel sessions, be specific in what is added
- We often write ephemeral markdown files, these are not usually committed
- Set a Co-Authored-By in the commit message
