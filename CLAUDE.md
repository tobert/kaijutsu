# 会術 Kaijutsu

*"The Art of Meeting"*

An agentic coding system for teams with two parts: a Bevy 0.18 client, and a Cap'n Proto over SSH server.

**The way of Kaijutsu:** Everyone editing shared state via CRDT tools. Rhai and kaish use the crdts. Claude does. Gemini
does. Users do via the builtin editor. We share operations via the kernel and reap the collaborative benefits of leaning
into the distributed algorithm to equalize access and handle different networks.

## Quick Start

```bash
# First time: add your SSH key
cargo run -p kaijutsu-server -- add-key ~/.ssh/id_ed25519.pub --nick amy --admin

# Terminal 1: Server
cargo run -p kaijutsu-server

# Terminal 2: Client
cargo run -p kaijutsu-app
```

## SSH Authentication

The server uses SQLite-backed public key authentication. Keys must be registered before connecting.

### Key Management

```bash
# Add a key (with nick and admin flag)
kaijutsu-server add-key ~/.ssh/id_ed25519.pub --nick amy --admin

# Import from authorized_keys (first key becomes admin if DB empty)
kaijutsu-server import ~/.ssh/authorized_keys

# List users and keys
kaijutsu-server list-users
kaijutsu-server list-keys [nick]

# Rename a user
kaijutsu-server set-nick old-nick new-nick
```

### Identity

Each key maps to a user with:
- **nick**: Short identifier used in RPC (e.g., "amy", "claude")
- **display_name**: Full name (defaults to key comment)
- **is_admin**: Admin privileges flag

Nick is auto-generated from fingerprint tail if not specified. Use `set-nick` to rename.

### Database Location

`~/.local/share/kaijutsu/auth.db` (XDG compliant)

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
| Chat | LLM prompts | `i` | `Esc` |
| Shell | kaish commands | `s` | `Esc` |
| Command | Slash commands | `:` | `Esc` |
| Visual | Selection | `v` | `Esc` |

## Key Patterns

### RPC (Cap'n Proto)

- Object-capability model over SSH
- `attachKernel()` returns a `Kernel` capability
- Real-time streaming for messages and kernel output

### MCP Integration

Dynamic MCP server registration via RPC:
- `registerMcp(name, command, args, env)` — spawn and connect to MCP server
- `unregisterMcp(name)` — disconnect and stop server
- `listMcpServers()` — list connected servers and their tools
- `callMcpTool(server, tool, args)` — invoke MCP tool

MCP tools are automatically registered as ExecutionEngines with qualified names like `git:status`.

### kaish Integration

Two execution modes:
- **EmbeddedKaish** — in-process interpreter, routes file I/O through CRDT blocks via `KaijutsuBackend`
- **KaishProcess** — subprocess with Unix socket IPC (for isolation)

`KaijutsuBackend` maps kaish file operations to blocks:
- `/docs/{doc_id}/{block_key}` — read/write block content
- `/docs/{doc_id}/_meta` — document metadata
- Tool calls route through kernel's ToolRegistry

## Crate Structure

```
kaijutsu/
├── crates/
│   ├── kaijutsu-crdt/       # CRDT primitives (BlockDocument, DAG)
│   ├── kaijutsu-kernel/     # Kernel, VFS, ToolRegistry, McpServerPool, FlowBus
│   ├── kaijutsu-client/     # RPC client library
│   ├── kaijutsu-server/     # SSH server, EmbeddedKaish, KaijutsuBackend
│   └── kaijutsu-app/        # Bevy GUI, kaish syntax validation
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

## Text Rendering Decisions

### No LCD Subpixel AA

**Do not pursue LCD subpixel antialiasing.**

- OLED displays (Pentile, QD-OLED) have non-standard subpixel layouts that break RGB assumptions
- Rotated monitors turn RGB into BGR or worse
- Apple removed subpixel AA entirely in macOS Mojave (2018)
- The "ClearType look" comes from stem darkening + gamma correction, not subpixels

### MTSDF Over Direct Vector

Dense code text (10k+ glyphs at 4K) is too ALU-heavy for per-pixel Bézier evaluation on integrated GPUs. MTSDF provides:

- Constant-time fragment shader regardless of glyph complexity
- Scalable rendering at any zoom level
- Support for effects (glow, weight variation) without re-rasterizing

### Core Quality Techniques

| Technique | Implementation |
|-----------|---------------|
| **Stem darkening** | `stem_darkening` uniform (0.15 default) shifts SDF bias inversely proportional to font size. The #1 technique for ClearType-quality at 12-16px. |
| **Shader hinting** | Gradient-based stroke detection (astiopin/webgl_fonts). Sharpens horizontal strokes, softens vertical for balanced weight. |
| **Semantic weighting** | `importance` field on glyphs (0.0 = faded, 0.5 = normal, 1.0 = bold). Enables cursor proximity emphasis and agent activity highlighting. |
| **Pixel alignment** | CPU-side baseline snapping + x-height grid fitting in `MsdfTextBuffer::update_glyphs()`. |

### TAA Investigation

Bevy 0.18 has TAA in `bevy_anti_alias::taa`. Key components:

- `TemporalAntiAliasing` component enables TAA on cameras
- `TemporalJitter` applies Halton(2,3) sequence offsets (8 samples)
- Requires `DepthPrepass` + `MotionVectorPrepass`
- Text could potentially use TAA for temporal super-resolution on static text

**Note:** TAA is designed for 3D scenes. Integration with 2D text overlay would require investigation.
