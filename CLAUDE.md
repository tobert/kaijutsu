# 会術 Kaijutsu

*"The Art of Meeting"*

Bevy 0.18 client for sshwarma. Cap'n Proto over SSH. Isekai UI aesthetic.

**The way of Kaijutsu:** Engineer context for each prompt and anchor models so they can be brilliant.

## Quick Start

```bash
cargo run  # Launch the client (MVP: just UI shell)
```

## Architecture

```
sshwarma server                     Kaijutsu client (this repo)
├── Rooms (message DAG)             ├── Bevy 0.18 UI
├── kaish kernels (per room)        ├── Context view (DAG blocks)
├── Equipment (tools)               ├── Quake console (kaish client)
├── Lua hooks (wrap)                ├── Sacred input bar
│   └── Cap'n Proto ◄──────────────►├── Cap'n Proto over SSH
└── SQLite                          └── Isekai styling
```

## Documentation

| Doc | Purpose |
|-----|---------|
| [docs/01-architecture.md](docs/01-architecture.md) | System design, room lifecycle, Cap'n Proto schema |
| [docs/02-ui-system.md](docs/02-ui-system.md) | Bevy UI patterns, modes, layout |
| [docs/03-bootstrap.md](docs/03-bootstrap.md) | MVP implementation plan |
| [docs/04-kaish-console.md](docs/04-kaish-console.md) | Remote kaish console (Quake-style) |

## Core Concepts

### Input is Sacred
The input bar never moves. Everything else adapts around it.
- Quake console drops down *above* it
- Context view scrolls
- Sidebar can collapse
- Input stays fixed at bottom

### Context View (Not Chat)
The main area manages cognitive load, not just chat history.
- Nested DAG blocks (messages → tool calls → results)
- Collapse/expand
- Navigate with `j/k`
- Scale to 5-10 concurrent agents

### Room Kernel
Each room has a shared kaish kernel.
- All users see all commands
- VFS mounted to room's worktrees
- `/room/scratch/` persists with room
- Fork = clone kernel + copy worktree contents (same branch)

### Mode System (vim-style)
| Mode | Purpose | Enter | Exit |
|------|---------|-------|------|
| Normal | Navigate, read | `Esc` | - |
| Insert | Type in input | `i` | `Esc` |
| Command | Slash commands | `:` | `Esc` |

## Key Patterns

### RPC (Cap'n Proto)
- Object-capability model over SSH
- `joinRoom()` returns a `Room` capability
- `room.getKernel()` returns shared kaish kernel
- Real-time streaming for messages and kernel output

### Remote Console (kaish)
- Quake-style drop-down (`` ` `` to toggle)
- Runs **kaish** (会sh), not bash
- Shared per room (everyone sees everything)
- Structured output rendering

### Equipment
- Tools available in a room
- Managed by sshwarma
- Surface nicely in sidebar

## Related Repos

| Repo | Purpose |
|------|---------|
| `~/src/sshwarma` | Server (PLAN-*.md files) |
| `~/src/mcpsh` | kaish shell (LANGUAGE.md) |
| `~/src/bevy` | Bevy 0.18 source |

## Development

```bash
# Run the client
cargo run

# Run with debug logging
RUST_LOG=debug cargo run

# Check Bevy 0.18 examples
cd ~/src/bevy && cargo run --example standard_widgets
```

## Git Conventions

**Add files by name, not with `-A` or `.`**

```bash
# Good - explicit about what we're committing
git add src/ui/context.rs src/main.rs

# Avoid - might grab unintended files
git add -A
git add .
```

This keeps commits focused and avoids accidentally staging debug files, scratch work, or other cruft.
