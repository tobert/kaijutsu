# Kaijutsu Kernel Model

*Last updated: 2026-02-06*

> **This is the authoritative design document for Kaijutsu's kernel model.**

## Implementation Status

| Component | Status |
|-----------|--------|
| Kernel concept | ✅ Implemented (`kaijutsu-kernel/src/kernel.rs`) |
| Cap'n Proto schema | ✅ Complete (88 Kernel methods, 6 World methods) |
| Server (kaijutsu-server) | ✅ Functional |
| Client (kaijutsu-client) | ✅ RPC client + ActorHandle (Send+Sync concurrent dispatch) |
| Client (kaijutsu-app) | 🚧 Bevy UI (block rendering, editing, navigation) |
| kaish integration | ✅ EmbeddedKaish (in-process) + KaishProcess (subprocess) |
| KaijutsuBackend | ✅ Maps kaish file I/O to CRDT blocks |
| MCP integration | ✅ McpServerPool with dynamic registration via RPC |
| Consent modes | ✅ Implemented (`control.rs`) |
| Checkpoint system | ✅ Implemented in kernel (not yet exposed via RPC) |
| Fork/Thread (kernel) | ✅ Implemented (`kernel.rs:497-571`) |
| Fork/Thread (RPC) | ✅ Implemented (`rpc.rs:988-1218`) |
| Block tools | ✅ Extensive (9 tools, 104KB implementation) |
| Seat/Context | ✅ Implemented (4-tuple SeatId model) |
| LLM integration | ✅ Per-kernel LLM via `llm.rhai` config (Anthropic, OpenAI, Gemini) |
| FlowBus | ✅ Pub/sub for block, resource, config, progress, elicitation events |
| Drift | ✅ Cross-context communication with LLM distillation |
| Agents | ✅ AgentRegistry with capabilities, status, activity events |
| Git integration | ✅ GitEngine with CRDT-backed worktrees and file watching |
| Tool filtering | ✅ ToolConfig (All/AllowList/DenyList) per kernel |
| complete() | 📋 Stub (returns empty completions) |
| archive() | 📋 Planned (not yet in schema) |

## Architecture

**kaijutsu-kernel owns everything. kaish executes shell commands (embedded or subprocess).**

```
┌─────────────────────────────────────────────────────────────────┐
│                      kaijutsu-server                            │
│                                                                 │
│  kaijutsu.capnp::World                                          │
│  └── listKernels, attachKernel, createKernel, listMySeats       │
│                                                                 │
│  kaijutsu.capnp::Kernel (88 methods)                             │
│  ├── VFS: mount, unmount, listMounts, vfs()                     │
│  ├── State: checkpoints, history, variables                     │
│  ├── Tools: block_*, kernel_search, executeTool                 │
│  ├── MCP: registerMcp, unregisterMcp, listMcpServers, callMcp   │
│  ├── LLM: prompt() with streaming, per-kernel config            │
│  ├── Drift: push, flush, pull, merge, queue, cancel             │
│  ├── Agents: attach, detach, list, capabilities, events         │
│  ├── Git: registerRepo, branches, flush, attribution            │
│  ├── Seats: joinContext, leaveSeat, listContexts                │
│  ├── Lifecycle: fork, thread, detach                            │
│  │                                                              │
│  │  execute() / shellExecute() ──────────────────────┐          │
│  │                                                   │          │
│  └───────────────────────────────────────────────────┼──────────┤
│                                                      ▼          │
│  ┌─────────────────────────────────────────────────────────────┐│
│  │ EmbeddedKaish (in-process, default)                         ││
│  │   └── KaijutsuBackend                                       ││
│  │         ├── File I/O → CRDT BlockStore                      ││
│  │         └── Tool calls → ToolRegistry (block_*, MCP)        ││
│  ├─────────────────────────────────────────────────────────────┤│
│  │ OR: KaishProcess (subprocess, for isolation)                ││
│  │   └── Unix socket + Cap'n Proto IPC                         ││
│  └─────────────────────────────────────────────────────────────┘│
│                                                                 │
│  McpServerPool ──────────────► MCP servers (git, exa, etc.)     │
│    └── McpToolEngine (ExecutionEngine per MCP tool)             │
└─────────────────────────────────────────────────────────────────┘
```

### Ownership

| Component | Owner | Notes |
|-----------|-------|-------|
| VFS (MountTable) | **kaijutsu-kernel** | LocalBackend, MemoryBackend |
| State (history, checkpoints) | **kaijutsu-kernel** | KernelState |
| Tools (block tools, etc.) | **kaijutsu-kernel** | ToolRegistry + ExecutionEngine trait |
| Drift (cross-context) | **kaijutsu-kernel** | DriftRouter (shared via Arc across fork/thread) |
| MCP connections | **kaijutsu-kernel** | McpServerPool (shared across kernels) |
| LLM integration | **kaijutsu-kernel** | LlmRegistry (per-kernel via `llm.rhai`) |
| Agents | **kaijutsu-kernel** | AgentRegistry (capabilities, status, events) |
| Shell execution | **kaijutsu-server** | EmbeddedKaish or KaishProcess |
| Block I/O for kaish | **kaijutsu-server** | KaijutsuBackend (maps files to CRDT blocks) |

### Execution Flow

When a user or AI runs shell code:

1. **Execute** — EmbeddedKaish runs code in-process (or KaishProcess for isolation)
2. **File I/O** — KaijutsuBackend routes to CRDT blocks (collaborative editing)
3. **Tool calls** — Route through ToolRegistry (block tools, MCP tools)
4. **Record** — Output recorded in message DAG
5. **Checkpoint** — If autonomous mode, may trigger checkpoint

```rust
// In kaijutsu-server kernel handler
async fn shell_execute(&self, code: String) -> Result<ExecResult> {
    // 1. Execute via embedded kaish (backed by KaijutsuBackend)
    let result = self.embedded_kaish.execute(&code).await?;

    // 2. Create shell output block in conversation
    let block_id = self.blocks.create_block(
        &conv_id,
        BlockKind::ShellOutput,
        Some(&result.stdout),
    )?;

    // 3. Maybe checkpoint
    if self.consent_mode == Autonomous && self.should_checkpoint() {
        self.checkpoint_auto().await?;
    }

    Ok(result)
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

**Design philosophy:** Everyone edits shared state via CRDT tools. kaish scripts,
Rhai programs, Claude, Gemini, and users in the editor all use the same block
operations. The distributed algorithm equalizes access regardless of network
conditions or participant type.

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
# Attach to a kernel (user)
kaish attach kernel://project-kaijutsu

# Attach (AI agent)
# This happens implicitly when context payload constitutes the model
```

When attached:
- User gets UI view into the kernel
- AI gets context payload from the kernel
- Both can see each other's presence via seats

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
- Each kernel still has its own `state/` (history, checkpoints) — only VFS is shared
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
# User: "yes" or approves in UI

# Automatic (in autonomous mode)
# Kernel self-checkpoints based on heuristics
```

**Checkpoint contents:**
- Summary text (user or AI authored)
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

- Checkpoints require user consent
- AI suggests, user approves
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
- "Auto-checkpoint if no user input for 50 interactions"
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
cat /mnt/research/state/seats   # Who's active there
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

## Block Tools

The kernel exposes a rich set of CRDT-native block tools for content manipulation.
Tools are registered as `ExecutionEngine` implementations and available via `executeTool()`.

| Tool | Purpose |
|------|---------|
| `block_create` | Create new block with role, kind, optional content |
| `block_append` | Streaming-optimized text append (batched) |
| `block_edit` | Line-based editing with CAS validation |
| `block_splice` | Character-level splice for programmatic edits |
| `block_read` | Read block content with line numbers |
| `block_search` | Regex search within a block |
| `block_list` | List blocks with filters (kind, status, parent) |
| `block_status` | Set block status (pending/running/done/error) |
| `kernel_search` | Cross-block grep across kernel |
| `drift` | Cross-context communication (push, pull, merge) |
| `git` | Context-aware git with VFS-resolved paths |
| `context` | Context management (create, list, switch) |

See [block-tools.md](block-tools.md) for the block interface specification.

## Drift: Cross-Context Communication

Drift is how contexts within a kernel share knowledge without sharing conversation
history. Each context has its own document, and drift transfers distilled content
between them as first-class `BlockKind::Drift` blocks.

```
Context A (main)           Context B (debug-fork)
    │                           │
    │  fork ───────────────────►│
    │                           │  ... Claude explores bug ...
    │                           │
    │◄── drift push "found it" ─│
    │  [Drift block injected]   │
    │                           │
    │◄── drift merge ───────────│  (LLM distills fork into parent)
    │  [Merge summary block]    │
```

**Key concepts:**
- `DriftRouter` — context registry + staging queue, shared via `Arc<RwLock>` across fork/thread
- `DriftEngine` — `ExecutionEngine` for the `drift` kaish command
- `DriftKind` — Push, Pull, Distill, Merge, Commit (provenance tracking)
- LLM distillation — optional summarization before transfer (uses context's configured model)

**Why drift instead of shared documents?** Isolation is the feature. Each context has
its own conversation flow. Drift provides *selective, curated* transfer — closer to
how human teams work (briefings, not sitting in every meeting).

See [drift.md](drift.md) for the full design document.

## Seat & Context Model

Seats provide presence tracking for multi-participant collaboration.

**SeatId** is a 4-tuple:
```
(nick, instance, kernel, context)
("amy", "laptop", "kaijutsu-dev", "planning")
```

| Field | Purpose |
|-------|---------|
| `nick` | Display name ("amy", "refactor-bot") |
| `instance` | Device or model ("laptop", "haiku") |
| `kernel` | Which kernel |
| `context` | Sub-context within kernel |

**SeatStatus**: `active`, `idle`, `away`

RPC methods: `listContexts`, `joinContext`, `leaveSeat`, `listMySeats` (on World)

## Cap'n Proto Interface

The actual schema lives in `kaijutsu.capnp`. Key interfaces:

```capnp
interface World {
  whoami @0 () -> (identity :Identity);
  listKernels @1 () -> (kernels :List(KernelInfo));
  attachKernel @2 (id :Text) -> (kernel :Kernel);
  createKernel @3 (config :KernelConfig) -> (kernel :Kernel);
  listMySeats @4 () -> (seats :List(SeatInfo));
}

interface Kernel {
  # 88 methods (@0-@87) — key categories shown below.
  # Full schema lives in kaijutsu.capnp.

  # kaish execution
  execute @1 (code :Text) -> (execId :UInt64);
  shellExecute @25 (code :Text, cellId :Text) -> (blockId :BlockId);

  # Tools
  executeTool @16 (call :ToolCall) -> (result :ToolResult);
  getToolSchemas @17 () -> (schemas :List(ToolSchema));

  # Lifecycle
  fork @9 (name :Text) -> (kernel :Kernel);
  thread @10 (name :Text) -> (kernel :Kernel);
  detach @11 ();

  # Block CRDT sync
  pushOps @34 (documentId :Text, ops :Data) -> (ackVersion :UInt64);
  getDocumentState @35 (documentId :Text) -> (state :DocumentState);
  subscribeBlocks @19 (callback :BlockEvents);

  # Drift (cross-context communication)
  driftPush @76 (targetCtx :Text, content :Text, summarize :Bool);
  driftFlush @77 () -> (count :UInt32);
  driftPull @78 (sourceCtx :Text, prompt :Text) -> (blockId :BlockId);
  driftMerge @79 (sourceCtx :Text) -> (blockId :BlockId);
  driftQueue @80 () -> (staged :List(StagedDrift));
  driftCancel @81 (stagedId :UInt64) -> (success :Bool);
  listAllContexts @82 () -> (contexts :List(ContextInfo));

  # LLM (per-kernel config)
  prompt @21 (request :LlmRequest) -> (promptId :Text);
  getLlmConfig @83 () -> (config :LlmConfigInfo);
  setDefaultProvider @84 (provider :Text);
  setDefaultModel @85 (provider :Text, model :Text);

  # MCP
  registerMcp @38 (...) -> (info :McpServerInfo);
  callMcpTool @41 (server :Text, tool :Text, arguments :Text);

  # Tool filtering
  getToolFilter @86 () -> (filter :ToolFilter);
  setToolFilter @87 (filter :ToolFilter);
}
```

**Note:** Only a representative subset shown. `complete()` remains a stub.
See `kaijutsu.capnp` for the full schema.

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
| Fork vs Thread | Maps to how users think about branching (isolated experiment) vs parallel work (same codebase). |
| Checkpoint = distillation | Compress without forgetting. Summaries carry forward. Raw history can be archived. |
| Consent modes | Collaborative work needs user approval. Autonomous agents need freedom. |

### Open Questions

1. **Cross-kernel drift:** The server has its own `DriftRouter` for cross-kernel communication — how does it compose with per-kernel routers?
2. **Kernel discovery:** How do you find kernels? Tags? Search? Hierarchy?
3. **Garbage collection:** When is it safe to delete archived history?
4. **Drift policies:** When should contexts auto-push findings? On checkpoint? On significant tool results?
5. **kaijutsu-mcp:** How do external agents (Claude Code, opencode, Gemini CLI) participate as drift contexts?

## References

- [drift.md](./drift.md) — Cross-context communication design
- [block-tools.md](./block-tools.md) — Block CRDT interface specification
- [design-notes.md](./design-notes.md) — Design explorations and background

---

## Changelog

**2026-02-06**
- Added Drift section documenting cross-context communication
- Updated Implementation Status (88 RPC methods, fork/thread implemented, agents, git, tool filtering)
- Updated Cap'n Proto schema to show drift, LLM config, tool filter, agents
- Fixed stale references: removed "lease" (use seats), removed "equip/unequip" (use ToolConfig)
- Updated ownership table with drift, agents, per-kernel LLM
- Added drift.md and context/git/drift to block tools table
- Updated Open Questions for drift era

**2026-01-24**
- Added MCP integration (McpServerPool, McpToolEngine, RPC methods)
- Added EmbeddedKaish and KaijutsuBackend for in-process kaish with CRDT block I/O
- Added FlowBus for pub/sub of block events
- Updated architecture diagram to show both execution modes and MCP

**2026-01-23**
- Updated Implementation Status to reflect actual state (checkpoint, block tools, seats all implemented)
- Added Block Tools section documenting the 9 CRDT-native tools
- Added Seat & Context Model section with SeatId 4-tuple
- Rewrote Cap'n Proto Interface to match actual schema (25 methods)
- Noted fork/thread kernel implementation is complete, only RPC layer is stubbed
- **Architecture rewrite**: kaijutsu-kernel owns VFS, state, tools, LLM; kaish is subprocess for shell only
