# Kaijutsu Kernel Model

*Last updated: 2026-01-16*

> **This is the authoritative design document for Kaijutsu's kernel model.**

## Implementation Status

| Component | Status |
|-----------|--------|
| Kernel concept | ✅ Designed (this document) |
| Cap'n Proto schema | ✅ Complete |
| Server (kaijutsu-server) | 🚧 Partial |
| Client (kaijutsu-app) | 🚧 Partial |
| kaish integration | 🚧 kaish L0-L4 complete, embedding planned |
| Consent modes | ✅ Implemented |
| Checkpoint system | 📋 Planned |
| Fork/Thread | 📋 Planned |

## kaish Integration

**kaish is the execution engine. Kaijutsu wraps it with collaboration.**

### Interface Ownership

| Interface | Owner | Purpose |
|-----------|-------|---------|
| `kaish.capnp::Kernel` | **kaish** | Execution: parse, eval, tools, VFS, MCP, state, blobs |
| `kaijutsu.capnp::World` | **kaijutsu** | Multi-kernel orchestration |
| `kaijutsu.capnp::Kernel` | **kaijutsu** | Collaboration: consent, fork/thread, checkpoint, messaging |

### Architecture

```
┌─────────────────────────────────────────────────────────────────┐
│                      kaijutsu-server                            │
│                                                                 │
│  kaijutsu.capnp::World                                          │
│  └── listKernels, attachKernel, createKernel                    │
│                                                                 │
│  kaijutsu.capnp::Kernel (collaboration layer)                   │
│  ├── consent: collaborative vs autonomous                       │
│  ├── lifecycle: fork, thread, checkpoint, archive               │
│  ├── messaging: send, mention, subscribe                        │
│  ├── equipment: listEquipment, equip, unequip                   │
│  │                                                              │
│  │  execute() ─────────────────────────────────────────┐        │
│  │                                                     │        │
│  └─────────────────────────────────────────────────────┼────────┤
│                                                        │        │
│  kaish-kernel (embedded, no IPC)                       ▼        │
│  ┌────────────────────────────────────────────────────────────┐ │
│  │  kaish.capnp::Kernel (execution layer)                     │ │
│  │  ├── execute, executeStreaming                             │ │
│  │  ├── getVar, setVar, listVars                              │ │
│  │  ├── listTools, callTool, getToolSchema                    │ │
│  │  ├── mount, unmount, listMounts                            │ │
│  │  ├── registerMcp, listMcpServers                           │ │
│  │  ├── snapshot, restore                                     │ │
│  │  └── readBlob, writeBlob                                   │ │
│  └────────────────────────────────────────────────────────────┘ │
└─────────────────────────────────────────────────────────────────┘
```

### Execution Flow

When a user or AI executes code in kaijutsu:

1. **Execute** — kaijutsu calls `kaish_kernel.execute(code)`
2. **Record** — Output recorded in message DAG
3. **Checkpoint** — If autonomous mode, may trigger checkpoint

```rust
// In kaijutsu-server kernel handler
async fn execute(&self, code: String) -> Result<ExecId> {
    // 1. Execute via embedded kaish
    let exec_id = self.next_exec_id();
    let result = self.kaish.execute(&code).await;

    // 2. Record in DAG
    self.dag.append(Row::tool_result(exec_id, &result));

    // 3. Maybe checkpoint
    if self.consent_mode == Autonomous && self.should_checkpoint() {
        self.checkpoint_auto().await?;
    }

    Ok(exec_id)
}
```

### kaish Standalone Mode

kaish also runs independently (for testing, scripting, other tools):

```bash
kaish                              # Interactive REPL
kaish script.kai                   # Run script
kaish serve --socket=/tmp/k.sock   # RPC server
kaish serve tools.kai --stdio      # MCP server
```

See `~/src/kaish/docs/BUILD.md` for kaish's build plan and layer dependencies.

## Overview

The **kernel** is the fundamental primitive in Kaijutsu. Everything is a kernel.

A kernel is a state holder that:
- Owns `/` in its virtual filesystem
- Can mount other VFS (worktrees, repos, other kernels)
- Has a consent mode (collaborative vs autonomous)
- Can checkpoint (distill history into summaries)
- Can be forked (heavy copy) or threaded (light, shared VFS)

## Kernel Structure

```
kernel
├── /                              # kernel owns root
├── /mnt/                          # mount points
│   ├── kaijutsu/                  # mounted worktree (rw)
│   ├── bevy/                      # mounted reference repo (ro)
│   ├── kaish/                     # mounted for cross-reference (ro)
│   └── kernel-research/           # mounted another kernel
│       ├── root/                  # that kernel's VFS
│       ├── state/                 # that kernel's state
│       └── checkpoints/           # that kernel's summaries
├── /scratch/                      # kernel-local ephemeral space
└── state/
    ├── history                    # interaction history (raw)
    ├── checkpoints/               # distilled summaries
    ├── consent_mode               # collaborative | autonomous
    └── context_config             # how to generate payloads
```

## Context Generation

**Key insight:** Context isn't stored, it's *generated*.

When a context payload is needed (for Claude, for another model, for export), kaish walks the kernel state and mounted VFS to emit a fresh payload. This means:

- Mounts determine what's visible to the model
- Checkpoints compress history without losing meaning
- The "now" for the model is always freshly constituted
- Pruning mounts directly shapes what context includes

```
kaish context-emit --format=claude
kaish context-emit --format=openai --include=/mnt/kaijutsu --since=checkpoint:latest
```

## Operations

### Mount / Unmount

Attach or detach VFS from a kernel.

```bash
# Mount a worktree
kaish mount /mnt/kaijutsu ~/src/kaijutsu --rw

# Mount read-only reference
kaish mount /mnt/bevy ~/src/bevy --ro

# Mount another kernel
kaish mount /mnt/research kernel://research-session-42

# Unmount when done
kaish unmount /mnt/bevy
```

Unmounting is natural pruning — reduce scope, focus context.

### Attach / Detach

Connect or disconnect your view to/from a kernel.

```bash
# Attach to a kernel (human)
kaish attach kernel://project-kaijutsu

# Attach (AI agent)
# This happens implicitly when context payload constitutes the model
```

When attached:
- Human gets UI view into the kernel
- AI gets context payload from the kernel
- Both can see each other's presence (if lease permits)

### Fork / Thread

Create new kernels from existing ones.

| Op | State | VFS | Isolation |
|----|-------|-----|-----------|
| `fork` | Deep copy | Snapshot | Full — changes don't propagate |
| `thread` | New, linked | Shared refs | None — changes visible to both |

```bash
# Fork: isolated exploration
kaish fork --name=experiment
# Now in new kernel with copied state, snapshot of mounts

# Thread: parallel view
kaish thread --name=parallel-work
# Now in new kernel, but VFS changes propagate to/from parent
```

**Fork** is like Unix fork — heavy, isolated, for branching explorations.
**Thread** is like pthread — light, shared memory, for parallel work on same codebase.

**Thread details:**
- The "linked" relationship means the child kernel holds references to the parent's VFS mounts, not copies
- Changes to files in `/mnt/project/` are visible to both parent and child immediately
- Each kernel still has its own `state/` (history, lease, checkpoints) — only VFS is shared
- If the parent kernel is archived/killed, threads become orphaned and must either:
  - Adopt the VFS mounts as their own (copy-on-orphan)
  - Be killed along with the parent (cascade delete, configurable)
- Threads are useful for: "I want Claude to explore this while I work on something else in the same repo"

### Checkpoint

Distill history into a summary. Compaction without forgetting.

```bash
# Manual checkpoint
kaish checkpoint "Established kernel model"

# AI-suggested (in collaborative mode, requires consent)
# Claude: "We've reached a decision point. Checkpoint?"
# Human: "yes" or approves in UI

# Automatic (in autonomous mode)
# Kernel self-checkpoints based on heuristics
```

**Checkpoint contents:**
- Summary text (human or AI authored)
- Timestamp
- Reference to what was compacted
- Optionally: archived raw history

**Compaction flow:**
```
Before checkpoint:
├── 200 interactions
├── 15 tool call traces
└── ~80k tokens

After checkpoint:
├── checkpoint: "Established kernel model..."
├── 20 recent interactions
└── ~12k tokens
```

### GC (Garbage Collection)

Remove orphaned/unreferenced state.

```bash
kaish gc --dry-run  # Show what would be cleaned
kaish gc            # Actually clean
```

## Consent Mode

Kernels operate in one of two modes:

### Collaborative (default)

- Checkpoints require human consent
- AI suggests, human approves
- Good for: pair programming, supervised work

### Autonomous

- Kernel can self-checkpoint
- AI manages its own lifecycle
- Good for: background research, long-running agents, unsupervised tasks

```bash
kaish config consent_mode=collaborative
kaish config consent_mode=autonomous
```

Hybrid rules are possible:
- "Auto-checkpoint if no human input for 50 interactions"
- "Auto-checkpoint on fork/thread creation"
- "Require consent for checkpoints that prune > 100 interactions"

## Kernel-to-Kernel Attachment

Kernels can mount other kernels, enabling composition and federation.

### Mount (Read-Only Visibility)

```bash
kaish mount /mnt/research kernel://research-session

# Now can read:
ls /mnt/research/root/          # Their VFS
cat /mnt/research/checkpoints/  # Their summaries
cat /mnt/research/state/lease   # Who's active there
```

Every kernel exposes itself as a mountable filesystem. This enables:
- Research kernel mounting multiple project kernels
- Meta-kernel overseeing a team's work
- AI reading another AI's context

### Attach (Bidirectional Participation)

Heavier than mount — active coordination:

```bash
kaish attach kernel://shared-session --mode=participant
```

When attached bidirectionally:
- Both kernels aware of each other's presence
- Lease coordination spans both
- Checkpoint events can propagate
- Like joining a shared document

## Kernel Lifecycle

```
┌──────────────────┐
│   create         │  ← kaish new --name=project-x
└────────┬─────────┘
         │
         ▼
┌──────────────────┐
│   mount VFS      │  ← kaish mount /mnt/src ~/src/project
└────────┬─────────┘
         │
         ▼
┌──────────────────┐
│   work           │  ← interactions, tool calls, exploration
│   (accumulate    │
│    state)        │
└────────┬─────────┘
         │
    ┌────┴────┐
    │         │
    ▼         ▼
┌────────┐ ┌────────┐
│ fork   │ │ thread │  ← branch exploration or parallel work
└────────┘ └────────┘
         │
         ▼
┌──────────────────┐
│   checkpoint     │  ← distill, compress, prune
└────────┬─────────┘
         │
         ▼
┌──────────────────┐
│   unmount        │  ← kaish unmount /mnt/old-reference
│   (prune scope)  │
└────────┬─────────┘
         │
    ┌────┴────┐
    │         │
    ▼         ▼
┌────────┐ ┌────────┐
│archive │ │ kill   │  ← preserve for later or delete
└────────┘ └────────┘
```

## Cap'n Proto Interface (Draft)

```capnp
interface World {
  whoami @0 () -> (identity :Identity);
  listKernels @1 () -> (kernels :List(KernelInfo));
  attachKernel @2 (id :Text) -> (kernel :Kernel);
  createKernel @3 (config :KernelConfig) -> (kernel :Kernel);
}

struct KernelConfig {
  name @0 :Text;
  consentMode @1 :ConsentMode;
  mounts @2 :List(MountSpec);
}

enum ConsentMode {
  collaborative @0;
  autonomous @1;
}

struct MountSpec {
  path @0 :Text;           # e.g. "/mnt/kaijutsu"
  source @1 :Text;         # e.g. "~/src/kaijutsu" or "kernel://other"
  writable @2 :Bool;
}

interface Kernel {
  getInfo @0 () -> (info :KernelInfo);

  # VFS
  mount @1 (spec :MountSpec);
  unmount @2 (path :Text);
  listMounts @3 () -> (mounts :List(MountInfo));

  # Lifecycle
  fork @4 (name :Text) -> (kernel :Kernel);
  thread @5 (name :Text) -> (kernel :Kernel);
  checkpoint @6 (summary :Text);
  archive @7 ();

  # kaish execution
  execute @8 (code :Text) -> (execId :UInt64);
  interrupt @9 (execId :UInt64);
  complete @10 (partial :Text, cursor :UInt32) -> (completions :List(Completion));
  subscribeOutput @11 (callback :KernelOutput);

  # History & context
  getHistory @12 (limit :UInt32) -> (entries :List(HistoryEntry));
  getCheckpoints @13 () -> (checkpoints :List(CheckpointInfo));
  emitContext @14 (config :ContextConfig) -> (payload :Data);
}

struct CheckpointInfo {
  id @0 :UInt64;
  summary @1 :Text;
  timestamp @2 :Int64;
  compactedCount @3 :UInt32;  # how many interactions were compacted
}

struct ContextConfig {
  format @0 :Text;         # "claude", "openai", "raw"
  includePaths @1 :List(Text);
  sinceCheckpoint @2 :UInt64;  # 0 = include all
  maxTokens @3 :UInt32;
}
```

## Storage & Persistence

Kernel state lives **server-side** (in kaijutsu-server). The client (kaijutsu-app) is a thin view.

```
Server storage (SQLite + filesystem)
├── kernels/
│   ├── <kernel-id>/
│   │   ├── state.db          # history, checkpoints, config
│   │   └── scratch/          # kernel-local ephemeral files
│   └── ...
├── worktrees/                 # git worktrees for VFS mounts
│   ├── <worktree-id>/
│   └── ...
└── archives/                  # archived kernel snapshots
    └── <kernel-id>-<timestamp>.tar.zst
```

**Implication for AI:** When Claude "attaches" to a kernel, this happens implicitly:
1. The server generates a context payload from kernel state + mounted VFS
2. This payload constitutes Claude's "now" for that interaction
3. Claude's outputs become new history entries in the kernel

There is no persistent "Claude process" — each interaction is a fresh emergence from kernel state.

## Notes for Future Agents

### Mental Model

Think of a kernel like a development environment that:
- Has a filesystem (the VFS with mounts)
- Has memory (history, checkpoints)
- Can be cloned (fork) or viewed in parallel (thread)
- Can be summarized (checkpoint) or archived

### Key Design Decisions

| Decision | Rationale |
|----------|-----------|
| Kernel owns `/` | Unix philosophy. Everything is a file. Kernels expose themselves as VFS. |
| Context is generated, not stored | Fresh context on each interaction. Mounts shape what's visible. Enables pruning. |
| Fork vs Thread | Maps to how humans think about branching (isolated experiment) vs parallel work (same codebase). |
| Checkpoint = distillation | Compress without forgetting. Summaries carry forward. Raw history can be archived. |
| Consent modes | Collaborative work needs human approval. Autonomous agents need freedom. |

### Open Questions

1. **Cross-kernel transactions:** Can a checkpoint span multiple kernels?
2. **Kernel discovery:** How do you find kernels? Tags? Search? Hierarchy?
3. **Garbage collection:** When is it safe to delete archived history?
4. **Multi-model:** Can a kernel have multiple AI models attached with different roles?

## References

- [docs/05-lexicon-exploration.md](./05-lexicon-exploration.md) — Philosophical dialogue that led to this model
