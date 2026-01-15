# Kaijutsu Architecture

*Last updated: 2026-01-15*

## Vision

**Kaijutsu** (会術 - "The Art of Meeting") is a Bevy-based GUI client for sshwarma. It speaks Cap'n Proto over SSH to provide a rich interface for multi-user AI collaboration.

The way of Kaijutsu: **engineer context for each prompt and anchor models so they can be brilliant.**

```
┌─────────────────────────────────────────────────────────────────────┐
│                       sshwarma server                               │
│  ┌─────────────┐  ┌─────────────┐  ┌─────────────┐  ┌────────────┐ │
│  │   SQLite    │  │  Lua Runtime │  │  Cap'n Proto│  │    MCP     │ │
│  │   World     │  │  (wrap.lua)  │  │   Handlers  │  │   Server   │ │
│  │             │  │  (hooks)     │  │             │  │  (agents)  │ │
│  └──────┬──────┘  └──────┬───────┘  └──────┬──────┘  └─────┬──────┘ │
│         │                │                 │                │       │
│         │          ┌─────┴─────┐           │                │       │
│         │          │   kaish   │           │                │       │
│         │          │  kernels  │ (per room)│                │       │
│         │          └─────┬─────┘           │                │       │
│         └────────────────┴────────┬────────┴────────────────┘       │
│                            ┌──────┴──────┐                          │
│                            │  SSH Server │                          │
│                            │   (russh)   │                          │
│                            └──────┬──────┘                          │
└───────────────────────────────────┼─────────────────────────────────┘
                                    │ SSH + Cap'n Proto
                                    ▼
┌───────────────────────────────────────────────────────────────────────┐
│                        Kaijutsu (Bevy Client)                         │
│  ┌──────────────┐  ┌──────────────┐  ┌──────────────┐               │
│  │   UI Shell   │  │   Context    │  │    RPC       │               │
│  │  (isekai)    │  │    View      │  │   Client     │               │
│  │              │  │  (DAG blocks)│  │  (capnp)     │               │
│  └──────────────┘  └──────────────┘  └──────────────┘               │
└───────────────────────────────────────────────────────────────────────┘
```

## Core Principles

### 1. Thin Client, Fat Server
- **Server**: Agents, context composition, persistence, tool execution, kaish kernels
- **Client**: Rendering, input, state visualization

### 2. Capability-Based RPC
- Cap'n Proto over SSH channels
- Server returns capability objects (Room, Kernel), not IDs
- Streaming for real-time updates

### 3. Context is Everything
- The UI manages cognitive load across multiple concurrent agent conversations
- Nested block structure mirrors the DAG of messages, tool calls, results
- Users drill down to whatever depth they need

### 4. Input is Sacred
- The input bar is always visible, always responsive
- Quake console drops down above it, never displaces it
- Future: controller input, agent-predicted suggestions

### 5. Isekai Aesthetic
- Floating translucent panels with glowing borders
- Video game HUD feel (WoW-inspired)
- Keyboard-driven with gamepad support

## Key Components

### SSH + Cap'n Proto Transport

| Channel | Name      | Direction | Protocol        | Purpose                    |
|---------|-----------|-----------|-----------------|----------------------------|
| 0       | `control` | Bidir     | Simple frames   | Version negotiation, keepalive |
| 1       | `rpc`     | Bidir     | Cap'n Proto RPC | Request/response calls     |
| 2       | `events`  | S→C       | Cap'n Proto     | Subscription streams       |

Benefits:
- SSH handles auth (keys) and encryption
- Native channel multiplexing
- Firewall-friendly (port 22/2222)

### Cap'n Proto Schema

Client-first schema definition: `kaijutsu.capnp`

```capnp
interface World {
  whoami @0 () -> (identity :Identity);
  listRooms @1 () -> (rooms :List(RoomInfo));
  joinRoom @2 (name :Text) -> (room :Room);
  createRoom @3 (config :RoomConfig) -> (room :Room);
}

struct RoomConfig {
  name @0 :Text;
  branch @1 :Text;                    # Optional branch stem suggestion
  repos @2 :List(RepoMount);
}

struct RepoMount {
  name @0 :Text;                      # e.g. "kaijutsu"
  url @1 :Text;                       # e.g. "git@github.com:atobey/kaijutsu.git"
  writable @2 :Bool;                  # rw vs ro
}

interface Room {
  getInfo @0 () -> (info :RoomInfo);
  getHistory @1 (limit :UInt32, beforeId :UInt64) -> (rows :List(Row));

  # Messaging
  send @2 (content :Text) -> (row :Row);
  mention @3 (agent :Text, content :Text) -> (row :Row);
  subscribe @4 (callback :RoomEvents);

  # kaish kernel (shared per room)
  getKernel @5 () -> (kernel :KaishKernel);

  # Equipment
  listEquipment @6 () -> (tools :List(ToolInfo));
  equip @7 (tool :Text);
  unequip @8 (tool :Text);

  # Room management
  fork @9 (newName :Text) -> (room :Room);
  leave @10 ();
}

interface KaishKernel {
  execute @0 (code :Text) -> (execId :UInt64);
  interrupt @1 (execId :UInt64);
  complete @2 (partial :Text, cursor :UInt32) -> (completions :List(Completion));
  subscribe @3 (callback :KernelOutput);
  getHistory @4 (limit :UInt32) -> (entries :List(HistoryEntry));
}
```

### Bevy Client Structure

```
kaijutsu/
├── Cargo.toml
├── kaijutsu.capnp           # Schema (client-first)
├── src/
│   ├── main.rs              # App entry, plugin registration
│   ├── connection/
│   │   ├── mod.rs
│   │   ├── ssh.rs           # russh client
│   │   └── rpc.rs           # Cap'n Proto client
│   ├── ui/
│   │   ├── mod.rs
│   │   ├── shell.rs         # Main layout/chrome
│   │   ├── context.rs       # Context view (DAG blocks)
│   │   ├── input.rs         # The sacred input bar
│   │   ├── console.rs       # Quake-style kaish console
│   │   ├── sidebar.rs       # Room list, agents, equipment
│   │   └── theme.rs         # Isekai styling
│   └── state/
│       ├── mod.rs
│       ├── room.rs          # Current room cache
│       └── mode.rs          # UI mode state machine
└── assets/
    ├── fonts/
    └── shaders/
```

## State Model

### Server Authoritative
- All persistent state lives on server (SQLite)
- Client maintains cache for rendering
- Events push updates → eventual consistency

### Room-Scoped Resources
- **kaish kernel**: One per room, shared by all users
- **VFS mounts**: Room's worktrees at `/room/src/`
- **Scratch space**: `/room/scratch/` persists with room
- **Equipment**: Tools available in this room

### Client Local State
- Current room subscription
- UI layout preferences
- Input buffer
- Mode (Normal/Insert/Command)

## Repository & Worktree Model

### Storage Architecture

```
┌─────────────────────────────────────────────────────────────────────┐
│                Physical Storage (hidden from kaish)                  │
│                                                                      │
│  repos/                         worktrees/                          │
│  ├── kaijutsu.git              ├── room-abc-kaijutsu/  ← Room A     │
│  ├── sshwarma.git              ├── room-abc-sshwarma/               │
│  └── kaish.git                 ├── room-def-kaijutsu/  ← Room B     │
│                                ├── room-def-sshwarma/    (same      │
│                                └── ...                    branch,   │
│                                                           diff      │
│  kaish cannot access this      Auto-managed, ephemeral   worktree) │
│  layer directly                                                     │
└─────────────────────────────────────────────────────────────────────┘
                                    │
                               VFS mount
                                    │
┌───────────────────────────────────┼─────────────────────────────────┐
│                Room A: kaijutsu-context-view                        │
│                                   │                                  │
│  /room/src/                       │                                  │
│  ├── kaijutsu/  ──────────────────┘  (rw)                           │
│  ├── sshwarma/  ─────────────────────  (ro)                         │
│  └── kaish/     ─────────────────────  (ro)                         │
│                                                                      │
│  Branch: feature/context-view                                       │
│  Worktrees: room-specific, transparent                              │
└──────────────────────────────────────────────────────────────────────┘
```

### Key Properties

| Concept | User-Facing | Description |
|---------|-------------|-------------|
| **Branch** | ✅ Yes | What you're working on, shown in git commands |
| **Worktree** | ❌ Hidden | Implementation detail, per-room isolation |
| **Repo mount** | ✅ Yes | Which repos appear in `/room/src/` |
| **rw/ro** | ✅ Yes | Whether room can modify the repo |

### Multi-Repo Rooms

Rooms can mount multiple repositories for cross-cutting work:

```
Room: 会術-fullstack
├── /room/src/
│   ├── kaijutsu/     ← rw, feature/schema-v2
│   ├── sshwarma/     ← rw, feature/schema-v2
│   └── kaish/        ← ro, main (reference only)
```

### Worktree Isolation

- Every room gets its own worktrees
- Many rooms can use the same branch without conflict
- Each room's worktree is independent (uncommitted changes isolated)
- Worktree management is transparent to users

```
Room A: feature-x-dev     Room B: feature-x-review
├── kaijutsu @ feature-x  ├── kaijutsu @ feature-x
│   (worktree: room-A)    │   (worktree: room-B)
│   has uncommitted WIP   │   clean checkout
```

## Room Lifecycle

```
┌──────────────────────────────────────────────────────────────────┐
│                           Room                                    │
│  ┌────────────────┐  ┌────────────────┐  ┌────────────────────┐ │
│  │  kaish kernel  │  │   Message DAG  │  │    Equipment       │ │
│  │  (shared)      │  │   (history)    │  │    (tools)         │ │
│  │                │  │                │  │                    │ │
│  │  VFS:          │  │  parent_id →   │  │  - claude-opus     │ │
│  │  /room/src/    │  │  type: chat    │  │  - filesystem      │ │
│  │  /room/scratch │  │  type: tool    │  │  - web_search      │ │
│  └────────────────┘  └────────────────┘  └────────────────────┘ │
│                                                                  │
│  Repos: kaijutsu (rw), sshwarma (ro)                            │
│  Branch: feature/context-view                                    │
│  Users: amy, bob        Agents: claude-opus                      │
└──────────────────────────────────────────────────────────────────┘
```

### Operations

| Operation | What Happens |
|-----------|--------------|
| **Create** | Specify repos + optional branch stem → worktrees created |
| **Join** | Subscribe to room events, kernel output, see VFS |
| **Leave** | Unsubscribe, room persists, kernel hibernates if empty |
| **Fork** | Copy worktree contents + kernel state → new room, same branch |

### Fork Behavior

```
Room A: kaijutsu-dev
├── kaijutsu @ main (rw)
├── sshwarma @ main (ro)
├── kernel state
└── /room/scratch/ contents

        │ fork("experiment")
        ▼

Room B: kaijutsu-dev-experiment
├── kaijutsu @ main (rw)        ← SAME branch
├── sshwarma @ main (ro)
├── kernel state (copied)       ← Cloned from Room A
└── /room/scratch/ (copied)     ← NEW worktrees with copied contents
```

- Fork copies worktree contents, not branch
- Both rooms on same branch until someone commits differently
- No branch collision because worktrees are isolated
- User can `git checkout -b experiment` in forked room if needed

### Git in Rooms

Users see normal git, worktrees are invisible:

```kaish
> cd /room/src/kaijutsu
> git status
On branch feature/context-view
Changes not staged for commit:
  modified: src/ui/context.rs

> git checkout -b experiment     # Creates new branch
> git commit -m "WIP experiment"
> git push origin experiment
```

## References

- [PLAN-ssh-capnp.md](~/src/sshwarma/PLAN-ssh-capnp.md) - Transport feasibility
- [PLAN-thin-client-arch.md](~/src/sshwarma/PLAN-thin-client-arch.md) - Server architecture
- [LANGUAGE.md](~/src/mcpsh/LANGUAGE.md) - kaish shell specification
