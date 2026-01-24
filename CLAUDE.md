# 会術 Kaijutsu

*"The Art of Meeting"*

An agentic coding system for teams with two parts: a Bevy 0.18 client, and a Cap'n Proto over SSH server.

**The way of Kaijutsu:** Everyone editing shared state via CRDT tools. Rhai and kaish use the crdts. Claude does. Gemini
does. Users do via the builtin editor. We share operations via the kernel and reap the collaborative benefits of leaning
into the distributed algorithm to equalize access and handle different networks.

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
│  │  Dashboard / │  │    Block     │  │    RPC       │              │
│  │ Conversation │  │   Rendering  │  │   Client     │              │
│  └──────────────┘  └──────────────┘  └──────────────┘              │
└─────────────────────────────────────────────────────────────────────┘
```

## Documentation

| Doc | Purpose |
|-----|---------|
| [docs/kernel-model.md](docs/kernel-model.md) | **Authoritative kernel model — start here** |
| [docs/block-tools.md](docs/block-tools.md) | CRDT block interface design |
| [docs/diamond-types-fork.md](docs/diamond-types-fork.md) | Why we forked diamond-types |
| [docs/design-notes.md](docs/design-notes.md) | Collected design explorations |

## Core Concepts

### Kernel

The kernel is the fundamental primitive. Everything is a kernel.

A kernel:
- Owns `/` in its VFS (virtual filesystem)
- Can mount worktrees, repos, other kernels at paths like `/mnt/project`
- Has a consent mode (collaborative vs autonomous)
- Can checkpoint (distill history into summaries)
- Can be forked (heavy copy, isolated) or threaded (light, shared VFS)

See [docs/kernel-model.md](docs/kernel-model.md) for full details.

### Context Generation

Context isn't stored, it's *generated*. When a context payload is needed (for Claude, for export), kaish walks the kernel state and mounted VFS to emit a fresh payload. Mounts determine what's visible.

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

## Crate Structure

```
kaijutsu/
├── crates/
│   ├── kaijutsu-crdt/       # CRDT primitives (BlockDocument, DAG)
│   ├── kaijutsu-kernel/     # Kernel state management
│   ├── kaijutsu-client/     # RPC client library
│   ├── kaijutsu-server/     # TCP/SSH server
│   └── kaijutsu-app/        # Bevy GUI
└── docs/
    ├── kernel-model.md      # ✅ Start here
    ├── block-tools.md       # CRDT interface
    ├── diamond-types-fork.md
    └── design-notes.md
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

## Autonomous Development Loop

The Bevy client requires a graphical session (Wayland/X11), but Claude typically runs in a headless terminal. This setup enables autonomous iteration:

```
┌─────────────────────────┐     ┌─────────────────────────┐
│  Graphical Session      │     │  Headless Terminal      │
│  (Moonlight/Konsole)    │     │  (SSH/WezTerm)          │
│                         │     │                         │
│  kaijutsu-runner.sh     │◄────│  Claude edits code      │
│    └─ cargo watch       │     │    └─ ./contrib/kj      │
│        └─ kaijutsu-app  │────►│        └─ BRP tools     │
└─────────────────────────┘     └─────────────────────────┘
      user starts this              Claude works here
```

### Setup

User starts the runner in the graphical session:
```bash
./contrib/kaijutsu-runner.sh        # or --release for optimized builds
```

### Claude's Workflow

1. **Check status**: `./contrib/kj status`
2. **Edit code** with Edit/Write tools → cargo watch auto-rebuilds
3. **Inspect app** via BRP: `mcp__bevy_brp__*` tools
4. **Screenshot**: `mcp__bevy_brp__brp_extras_screenshot`
5. **If build fails**: `./contrib/kj tail` to see errors

### Control Commands

| Command | Effect |
|---------|--------|
| `./contrib/kj status` | Check runner state |
| `./contrib/kj tail` | Follow build output |
| `./contrib/kj pause` | Stop watching (batch edits) |
| `./contrib/kj resume` | Resume watching |
| `./contrib/kj rebuild` | Force clean rebuild |
| `./contrib/kj restart` | Restart cargo watch |

### Output Files

- `/tmp/kj.status` — current state (quick check)
- `/tmp/kaijutsu-runner.typescript` — full output via `script(1)`, captures crashes

## Direct BRP Workflow (Graphical Session)

When Claude runs in a graphical session, use `mcp__bevy_brp__*` tools directly instead of the runner script. The MCP tools are self-documenting.

### When to Use Which Workflow

| Scenario | Workflow |
|----------|----------|
| Claude in graphical session | Direct BRP (`mcp__bevy_brp__*`) |
| Claude SSH'd to headless terminal | `contrib/kj` + runner script |
| Slow/unstable network | `contrib/kj` (cargo watch handles rebuilds) |

## Git Conventions

Follow typical open source conventions for commits. This project is still in early phases of development and we are working on main.

- Add files by name, avoid wildcards
- We often work in parallel sessions, be specific in what is added
- We often write ephemeral markdown files, these are not usually committed
- Set a Co-Authored-By in the commit message

## Bevy 0.18 Quick Reference

### Event → Message rename

| Old (0.14-0.17) | New (0.18) |
|-----------------|------------|
| `#[derive(Event)]` | `#[derive(Message)]` |
| `EventReader<T>` | `MessageReader<T>` |
| `EventWriter<T>` | `MessageWriter<T>` |
| `events.send(x)` | `messages.write(x)` |
| `app.add_event::<T>()` | `app.add_message::<T>()` |

### Other API changes

| Old | New |
|-----|-----|
| `ChildBuilder` | `ChildSpawnerCommands` |
| `BorderColor(color)` | `BorderColor::all(color)` |
| `resolution: (1280., 800.).into()` | `resolution: (1280, 800).into()` |
| `query.get_single()` | `query.single()` |

### Keyboard input

```rust
use bevy::input::keyboard::{Key, KeyboardInput};

fn handle_input(mut keyboard: MessageReader<KeyboardInput>) {
    for event in keyboard.read() {
        if !event.state.is_pressed() { continue; }
        match (&event.logical_key, &event.text) {
            (Key::Enter, _) => { /* ... */ }
            (_, Some(text)) => { /* text input */ }
            _ => {}
        }
    }
}
```

### References

- Bevy source: `~/src/bevy`
- Text input example: `~/src/bevy/examples/input/text_input.rs`
- Message example: `~/src/bevy/examples/ecs/message.rs`
