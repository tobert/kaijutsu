# Kaijutsu Remote Console: kaish Integration

*Last updated: 2026-01-15*

## Overview

The remote console in Kaijutsu runs **kaish** (会sh), a purpose-built shell for MCP tool orchestration. Unlike a traditional bash PTY, kaish provides structured I/O that both humans and agents can work with natively.

```
会 (kai) = meeting, gathering, coming together
kaish = kai + sh = the gathering shell
```

## Architecture

### Room Kernel Model

Each **room** has a shared kaish kernel. All users in the room share it.

```
┌──────────────────────────────────────────────────────────────────┐
│                       sshwarma server                             │
│                                                                   │
│  ┌─────────────────────────────────────────────────────────────┐ │
│  │                         Room: lobby                          │ │
│  │  ┌─────────────┐  ┌─────────────┐  ┌─────────────────────┐  │ │
│  │  │ kaish kernel│  │ Message DAG │  │    Equipment        │  │ │
│  │  │  (shared)   │  │  (history)  │  │    (tools)          │  │ │
│  │  │             │  │             │  │                     │  │ │
│  │  │ VFS:        │  │ Rows with   │  │ - claude-opus       │  │ │
│  │  │ /room/src/  │  │ parent_id   │  │ - filesystem        │  │ │
│  │  │ /room/scratch│ │ and type    │  │ - web_search        │  │ │
│  │  └─────────────┘  └─────────────┘  └─────────────────────┘  │ │
│  │                                                              │ │
│  │  Users: amy, bob            Agents: claude-opus              │ │
│  └──────────────────────────────────────────────────────────────┘ │
│                                                                   │
│  ┌──────────────────────────────────────────────────────────────┐ │
│  │                         Room: dev                             │ │
│  │  (separate kernel, separate state)                            │ │
│  └──────────────────────────────────────────────────────────────┘ │
└───────────────────────────────────────────────────────────────────┘
                           │
                     SSH + Cap'n Proto
                           │
┌──────────────────────────┼──────────────────────────────────────┐
│               Kaijutsu (Bevy Client)                             │
│                          │                                       │
│  amy joins room:lobby ───┘                                       │
│  → subscribes to kernel output                                   │
│  → can execute commands                                          │
│  → sees bob's commands too (shared kernel!)                      │
└──────────────────────────────────────────────────────────────────┘
```

### Why Room Kernel (Not Per-User)?

| Per-User Kernel | Per-Room Kernel |
|-----------------|-----------------|
| Private exploration | Shared context |
| Can't see others' work | See everything in the room |
| Multiple kernels per room | One kernel = one truth |
| Complex sync | Simple model |

**Room kernel wins because:**
- The room is the unit of collaboration
- Everyone sees the same VFS state
- Agent tool calls visible to all
- Forking a room forks the kernel too

### Kernel Lifecycle

```
Room created
    │
    ▼
┌─────────────────┐
│ Kernel spawned  │ ← VFS mounted to room's worktrees
│ (idle)          │
└────────┬────────┘
         │
    User joins room
         │
         ▼
┌─────────────────┐
│ Kernel active   │ ← Executing commands, streaming output
│                 │
└────────┬────────┘
         │
    All users leave
         │
         ▼
┌─────────────────┐
│ Kernel hibernates│ ← State preserved, process may sleep
│                  │
└────────┬─────────┘
         │
    Room forked
         │
         ▼
┌─────────────────┐
│ Kernel cloned   │ ← New room gets copy of kernel state
│ (new room)      │   Variables, history, scratch files
└─────────────────┘
```

**Key points:**
- Kernel persists as long as room exists
- `/room/scratch/` persists with room (not wiped on leave)
- Fork = deep clone of kernel state + worktree contents (same branch)
- Empty rooms keep their kernel state

## Virtual Filesystem

```
/
├── bin/                    # Available tools (builtins + MCP)
│   ├── echo
│   ├── ls
│   ├── scatter
│   └── ...
│
├── mcp/                    # MCP servers (read-only listings)
│   ├── claude/
│   │   └── tools/
│   ├── exa/
│   │   └── tools/
│   └── filesystem/
│       └── tools/
│
├── room/                   # Current room context
│   ├── src/                # Mounted repos (each room has own worktrees)
│   │   ├── kaijutsu/       # rw - can modify
│   │   ├── sshwarma/       # ro - reference only
│   │   └── kaish/          # ro - reference only
│   ├── scratch/            # Room-level shared temp (persists!)
│   ├── tools.kai           # Room-defined tools
│   └── .roomrc             # Room startup script
│
└── tmp/                    # Execution-scoped (auto-cleaned per command)
    └── kaish-<exec-id>/
```

### Repo Mounting

Rooms specify which repos to mount at creation:

```yaml
repos:
  kaijutsu: rw    # Can modify
  sshwarma: ro    # Read-only reference
  kaish: ro       # Read-only reference
branch: feature/context-view
```

**Key properties:**
- Each room gets **its own worktrees** (isolated from other rooms)
- **Worktrees are transparent** - users just see `/room/src/reponame/`
- Many rooms can use the **same branch** without conflict
- **rw vs ro** controls whether the room can modify files
- Git commands work normally inside `/room/src/*/`

### Git Operations

```kaish
> cd /room/src/kaijutsu
> git status
On branch feature/context-view
Changes not staged for commit:
  modified: src/ui/context.rs

> git checkout -b experiment    # Create new branch (still same worktree)
> git add .
> git commit -m "WIP"
> git push origin experiment
```

The worktree is an implementation detail - users think in branches and files.

## Quake Console UX

### Toggle & Position

```
┌─────────────────────────────────────────────────────────────────┐
│ 【会術】 Kaijutsu                              Press ` to toggle │
╠═════════════════════════════════════════════════════════════════╣
│ ┌─ 会sh ────────────────────────────────── room:lobby ── ▼50% ┐ │
│ │ /room/src/api-gateway                                       │ │
│ │ > ls                                                        │ │
│ │ Cargo.toml  src/  tests/  README.md                         │ │
│ │ > exa.web_search query="rust async"                         │ │
│ │ ✓ 3 results ────────────────────────────────────────────── │ │
│ │ │ [0] "Async Rust Book" docs.rs/...                        │ │
│ │ │ [1] "Tokio Tutorial" tokio.rs/...                        │ │
│ │ └───────────────────────────────────────────────────────── │ │
│ │ > _                                                         │ │
│ └─────────────────────────────────────────────────────────────┘ │
│ ══════════════════════════════════════════════════ drag to resize
│                                                                 │
│  Context view continues below...                                │
├─────────────────────────────────────────────────────────────────┤
│ > @opus what do you think about...                          [I] │  ← Input (unmoved)
└─────────────────────────────────────────────────────────────────┘
```

**Input bar stays at bottom.** Console drops down from top, pushing context view down but never touching input.

### Key Bindings

| Key | Action |
|-----|--------|
| `` ` `` | Toggle console |
| `Ctrl+L` | Clear console output |
| `Ctrl+C` | Interrupt current command |
| `Tab` | Autocomplete |
| `Up/Down` | History navigation |
| `Ctrl+1/2/3/4` | Height presets (25/50/75/100%) |

### Height Presets

- **25%**: Quick peek, see a few lines
- **50%**: Default, comfortable working space
- **75%**: Detailed work, lots of output
- **100%**: Full screen (context view hidden)

## Structured Output Rendering

kaish returns structured `$?` results. We render them richly:

### Success

```
> api.get_users limit=5
✓ ok (234ms) ──────────────────────────────────────────────────────
┌─ $.data ─────────────────────────────────────────────────────────┐
│ users: (5)                                                       │
│   [0] { id: 1, name: "alice", role: "admin" }                   │
│   [1] { id: 2, name: "bob", role: "user" }                      │
│   ...                                                            │
│ total: 127                                                       │
└──────────────────────────────────────────────────────────────────┘
```

### Error

```
> api.get_users limit=-1
✗ error ───────────────────────────────────────────────────────────
│ code: 400
│ message: "limit must be positive"
└──────────────────────────────────────────────────────────────────
```

### Progress (scatter/gather)

```
> cat urls.txt | scatter | fetch ${ITEM} | gather progress=true
⠋ scatter/gather ──────────────────────────────────────────────────
│ [████████░░░░░░░░░░░░] 8/20 complete
│ ✓ https://example.com/1 (123ms)
│ ✓ https://example.com/2 (156ms)
│ ⠋ https://example.com/3 (running...)
│ · https://example.com/4 (queued)
└──────────────────────────────────────────────────────────────────
```

## Multi-User Visibility

Since the kernel is shared, everyone sees everything:

```
┌─ 会sh ────────────────────────────────────── room:lobby ─────────┐
│ /room/src                                                        │
│ amy> ls                                                          │
│ Cargo.toml  src/  README.md                                      │
│ bob> cat README.md                                               │  ← bob's command
│ # API Gateway                                                    │     visible to amy
│ This service handles...                                          │
│ amy> _                                                           │
└──────────────────────────────────────────────────────────────────┘
```

Each command shows who ran it. This enables:
- Pair programming
- Watching agents work
- Learning from teammates

## Autocomplete

```bash
会sh> exa.<TAB>
  web_search    "Search the web"
  news_search   "Search news articles"

会sh> ls /room/src/<TAB>
  api-gateway/
  user-service/
  shared-types/

会sh> ${?.<TAB>
  code      # Exit code
  ok        # Success bool
  err       # Error message
  out       # Raw stdout
  data      # Parsed JSON
```

### Completion Sources

1. **Builtins** - `echo`, `ls`, `scatter`, etc.
2. **MCP tools** - From connected servers
3. **Paths** - VFS-aware path completion
4. **Variables** - `${?.<TAB>}` shows result fields
5. **History** - Previous commands matching prefix
6. **Room tools** - From `/room/tools.kai`

## Cap'n Proto Interface

```capnp
interface Room {
  # ... existing room methods ...

  # kaish kernel (shared per room)
  getKernel @5 () -> (kernel :KaishKernel);
}

interface KaishKernel {
  execute @0 (code :Text) -> (execId :UInt64);
  interrupt @1 (execId :UInt64);
  complete @2 (partial :Text, cursor :UInt32) -> (completions :List(Completion));
  subscribe @3 (callback :KernelOutput);
  getHistory @4 (limit :UInt32) -> (entries :List(HistoryEntry));
}

interface KernelOutput {
  onOutput @0 (execId :UInt64, user :Text, stream :Stream, data :Data);
  onResult @1 (execId :UInt64, user :Text, result :KaishResult);
  onPrompt @2 (cwd :Text);
}

struct KaishResult {
  code @0 :Int32;
  ok @1 :Bool;
  err @2 :Text;
  out @3 :Text;
  data @4 :AnyPointer;  # JSON-like structured data
}

struct HistoryEntry {
  execId @0 :UInt64;
  user @1 :Text;
  code @2 :Text;
  timestamp @3 :Int64;
}

struct Completion {
  text @0 :Text;
  display @1 :Text;
  description @2 :Text;
  kind @3 :CompletionKind;
}

enum CompletionKind {
  builtin @0;
  mcpTool @1;
  path @2;
  variable @3;
  history @4;
  roomTool @5;
}
```

## Implementation Phases

### Phase 1: Basic Console
- [ ] Quake-style toggle animation
- [ ] Connect to room kernel via Cap'n Proto
- [ ] Basic text input/output
- [ ] Show user who ran each command

### Phase 2: Rich Rendering
- [ ] Structured `$?` result display
- [ ] Syntax highlighting for kaish
- [ ] Progress indicators
- [ ] Collapsible output sections

### Phase 3: Completion
- [ ] Tab completion UI
- [ ] Completion from kernel
- [ ] History navigation
- [ ] Up/Down through previous commands

### Phase 4: Polish
- [ ] Height presets + drag resize
- [ ] Clear/interrupt commands
- [ ] Scroll through long output
- [ ] Copy output to clipboard

## kaish Crate Structure

```
kaish/                      # Standalone crate
├── Cargo.toml
├── src/
│   ├── lib.rs             # Library for embedding
│   ├── main.rs            # kaish binary (scripts, REPL)
│   ├── parse/             # Parser
│   ├── eval/              # Evaluator
│   ├── vfs/               # Virtual filesystem
│   ├── builtin/           # Built-in tools
│   └── mcp/               # MCP client integration
└── examples/
    └── embed.rs           # Embedding example
```

sshwarma embeds `kaish` crate, spawns one kernel per room.

## References

- [LANGUAGE.md](~/src/mcpsh/LANGUAGE.md) - kaish language specification
- [Jupyter Messaging](https://jupyter-client.readthedocs.io/en/latest/messaging.html) - Protocol inspiration
- [docs/01-architecture.md](./01-architecture.md) - Overall system architecture
