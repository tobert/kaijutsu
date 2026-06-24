# 会術 Kaijutsu

Kaijutsu is a cybernetic system for multi-user multi-model multi-context collaboration.
It is an **instrument you play, not a harness that drives you** — you play it, a model
plays it, anyone with a connected app plays it too; many hands on one keyboard. The
kernel is the instrument's *body*: it holds context data, model interactions, workspaces,
and tools, and supplies what a turn needs without playing the turn itself. It speaks SSH
with Cap'n Proto over channels. (Named for humans in `docs/instrument-design.md`;
embodied — never preached — in the model-facing rc stances.)

## Stance

The kernel restates the cybernetic / 改善 / TDD posture in its own rc lifecycle:
`/etc/rc/coder/create/S00-stance.kai` reaches the model via the system-prompt slot for
every context with `context_type=coder`. rc scripts at `/etc/rc` are **CRDT-owned** — the
kernel is the sole owner (no host file, no write-through); embedded defaults under
`assets/defaults/rc/` seed the CRDT once on a fresh kernel. There is no host file to
`vim`: edit a live script with `kj rc edit <path> --content <body>`, and `kj rc reset
<path>` restores one to its embedded default. Change the shipped default by editing
`assets/defaults/rc/` (the in-repo seed). See `docs/config-crdt-ownership.md`.

## Crates

`kaijutsu-types` first — the shared types every other crate depends on. Then
`kaijutsu-crdt` (BlockStore/BlockDocument), `kaijutsu-kernel` (Kernel, VFS, MCP broker,
LLM, drift, `kj` builtin), `kaijutsu-server` (SSH server, EmbeddedKaish),
`kaijutsu-client` (RPC client, Send+Sync ActorHandle), `kaijutsu-app` (Bevy 0.18 GUI;
inline SVG + ABC→staff rendering). Others: abc, mcp, cas, agent-tools, index, telemetry,
hyoushigi, viz. Wire schema: `kaijutsu.capnp`. The stdio MCP server (`kaijutsu-mcp`)
exposes most kernel capabilities and can be called as a hook from client applications.

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

## Working Notes

Two committed markdown files carry durable work between sessions; keep them current
**as you go**, not at the end:

- **`docs/issues.md`** (committed) — the open-work backlog and side-quest valve. Record
  out-of-scope work here before moving on; **delete an entry when it ships** (melt the
  story into the devlog if it's worth keeping). Code is truth; this tracks what's *not*
  in the code yet.
- **`docs/devlog.md`** (committed) — a durable narrative from the agent's perspective.
  Write your story there.

## Git Conventions

- Working on main (early development); parallel work on the same repo is common
- Add files by name, avoid wildcards; ephemeral markdown is usually not committed
- Set Co-Authored-By in commit messages

## Bevy 0.18 Quick Reference

Trust this table over training memory — Bevy 0.18 renamed the event system and is newer than most model training.

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
