# 会術 Kaijutsu

Kaijutsu is an cybernetic system for multi-user multi-model multi-context collaboration.

The kaijutsu kernel manages context data, model interactions, workspaces,
and tools. It offers an SSH protocol with Cap'n'Proto over channels.

kaijutsu-app is a Bevy 0.18 user interface that offers a completely programmable and rich
surface for agent interactions, including inline SVG and rendering ABC to standard music
staffs.

Kaijutsu's stdio MCP server offers most of its capabilities and can be called as a hook
from client applications.

## Crate Structure

```
kaijutsu/
├── crates/
│   ├── kaijutsu-types/       # READ FIRST
│   ├── kaijutsu-crdt/        # BlockStore, BlockDocument (re-exports types)
│   ├── kaijutsu-kernel/      # Kernel, VFS, MCP broker, LLM, drift, kj builtin
│   ├── kaijutsu-server/      # SSH server, EmbeddedKaish, KaijutsuBackend
│   ├── kaijutsu-client/      # RPC client, ActorHandle (Send+Sync)
│   ├── kaijutsu-app/         # Bevy GUI
│   ├── kaijutsu-abc/         # ABC music notation
│   ├── kaijutsu-mcp/         # MCP server (rmcp)
│   ├── kaijutsu-cas/         # Content-addressed store (blob persistence)
│   ├── kaijutsu-agent-tools/ # Agent session detection (Claude Code, etc.)
│   ├── kaijutsu-index/       # Semantic vector indexing (ONNX + HNSW)
│   └── kaijutsu-telemetry/   # OpenTelemetry (W3C context propagation)
├── kaijutsu.capnp            # Wire protocol schema
└── docs/                     # design-notes.md, telemetry.md, issues.md, etc.
```

## Autonomous Development Loop

Most testing happens on a Linux server with a real GPU that the user can connect to with remote desktop.

```bash
# user starts this in the Wayland session:
./contrib/kaijutsu-runner.sh

# agents use:
./contrib/kj status|tail|pause|resume|rebuild|restart
```

The Bevy BRP tools work directly. Take screenshots frequently.

## Git Conventions

- Working on main (early development)
- Parallel work on the same repo is common
- Add files by name, avoid wildcards
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

