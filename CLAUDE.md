# 会術 Kaijutsu

Kaijutsu is an cybernetic system for multi-user multi-model multi-context collaboration.

The kaijutsu kernel holds context data, model interactions, workspaces, and
tools — the shared body that people and models play together. It offers an SSH
protocol with Cap'n'Proto over channels.

kaijutsu-app is a Bevy 0.18 user interface that offers a completely programmable and rich
surface for agent interactions, including inline SVG and rendering ABC to standard music
staffs.

Kaijutsu's stdio MCP server offers most of its capabilities and can be called as a hook
from client applications.

## Stance

The kernel restates the cybernetic / 改善 / TDD coding posture in its
own rc lifecycle: `/etc/rc/coder/create/S00-stance.md` reaches the
model via the system-prompt slot for every context with
`context_type=coder`. rc scripts at `/etc/rc` are **CRDT-owned** (the
kernel is the sole owner — no host file, no write-through); the embedded
defaults under `assets/defaults/rc/` seed the CRDT once on a fresh kernel.
Edit a live script with `kj rc edit /etc/rc/coder/create/S00-stance.md`
(there is no host file to `vim`); `kj rc reset <path>` restores one script
to its embedded default. Change the shipped default by editing
`assets/defaults/rc/` (the in-repo seed). See `docs/config-crdt-ownership.md`.

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

## Conversation vs Context

**Context** is the durable side: CRDT block log, exclusions, edits, conversation metadata. Multi-writer. Holds more than the live conversation knows about.

**Conversation** is the live session: an append-only message sequence shipped to the LLM. Hydrated from context once at boundary events (fork, new, cold start, attach) and append-only thereafter.

`block exclude` / `block edit` operate on the context and only take effect at the next hydrate boundary — typically fork. To remediate a poisoned conversation (giant tool output, bad turn): exclude in context, then fork. Async events between turns (shell output, drift, MCP calls from sibling agents) queue in a per-context mailbox and flush on the next turn. The mailbox is also the atomicity gate that keeps tool_use+tool_result pairs (and other must-travel-together blocks) from being split by unrelated writers.

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

