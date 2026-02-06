# Drift: Cross-Context Communication

*Last updated: 2026-02-06*

> **Drift is how contexts share knowledge without sharing history.**

## What Is Drifting?

A kernel can have multiple **contexts** — parallel conversations with separate documents
but shared state (VFS, tools, CRDT storage). Drift is the mechanism for moving distilled
knowledge between these contexts.

Think of it like this: you fork a conversation to explore a bug. Twenty minutes later,
that fork has found the root cause. Rather than copy-pasting or re-explaining, you
**drift** the findings back — optionally distilled by an LLM into a concise briefing.

```
┌──────────────┐                         ┌──────────────┐
│  Context A   │   drift push "found     │  Context B   │
│  (main)      │◄──── the race condition │  (debug-fork)│
│              │      in session.rs:42"  │              │
│  [drift      │                         │  claude      │
│   block      │   drift pull --summarize│  exploring   │
│   appears]   │────────────────────────►│  auth bug    │
└──────────────┘                         └──────────────┘
```

The transferred content arrives as a `BlockKind::Drift` block in the target document —
a first-class citizen in the conversation DAG, tagged with its origin context and
how it arrived.

## Getting Oriented

If you're an agent landing in a kernel cold, here's the mental model:

**Session vs. Context.** A *session* is your viewport — the SSH connection, the Bevy
window, the MCP channel. A *context* is a conversation with its own document and KV
cache. One session can move between many contexts, fork new ones, and edit across them.

**Default landing.** Every kernel starts with a `main` context. When you connect, that's
where you are unless told otherwise. Use `drift ls` to see what other contexts exist.

**Context ≈ model context window.** A context tracks everything — every message, tool
call, and result — in a CRDT document. When it's time to call an LLM, the context
generates a payload from that document, filtering and formatting for inference. The
document is the durable state; the context window is the ephemeral view.

**All clients auto-connect.** When the RPC layer attaches a kernel, the caller's context
is registered in the `DriftRouter` automatically. You don't need to opt in — you're
already visible to other contexts and can drift to/from them immediately.

**Forking = new conversation + new KV cache.** `fork` creates a deep copy: new document
ID, new context name, independent exploration. The fork remembers its parent (via
`parent_short_id`), which enables `drift merge` to bring findings home. `thread` is
lighter — shared VFS, new document, same kernel.

## Why Drift Instead of Shared Documents?

Contexts intentionally have **separate documents**. This isolation is the feature:

- Each context has its own conversation flow, uncluttered by the other's noise
- Different models can work in different contexts (Claude in one, Gemini in another)
- Fork/thread semantics map to how people think about branching exploration
- Drift is *selective* — you choose what to transfer and when

Shared documents would mean every participant sees every message. Drift means each
context gets a **curated summary** of what the other learned. This is closer to how
human teams work: you don't sit in every meeting, you get a briefing.

## What Drift Blocks Look Like

When a drift arrives, it's a `BlockSnapshot` with `kind: Drift` and these fields:

| Field | Type | Example |
|-------|------|---------|
| `source_context` | `Option<String>` | `"a1b2c3"` (short ID of sender) |
| `source_model` | `Option<String>` | `"claude-opus-4-6"` |
| `drift_kind` | `Option<DriftKind>` | `Push`, `Pull`, `Distill`, `Merge`, `Commit` |
| `content` | `String` | The actual briefing text |
| `author` | `String` | Nick of sender (e.g., `"amy"`, `"claude"`) |
| `created_at` | `u64` | Unix epoch seconds |

The block's role is `System`. It's a first-class DAG citizen — it has a `BlockId`, a
parent in the conversation tree, and participates in CRDT ordering like any other block.

In the UI, drift blocks will be collapsible and visually attributed, like a message
from a named colleague dropping into your conversation with context about where it
came from and how it was produced.

## When to Drift

Drift is the cybernetics of multi-context work: it's the feedback loop that keeps
parallel investigations from diverging into wasted effort.

**Current maturity:** human-directed, agent-hinted. You (an agent or human) decide
when to drift. The maturity path is:

1. **Human-directed** — user says "drift this to context B" *(now)*
2. **Agent-hinted** — agent suggests "this finding is relevant to context B" *(soon)*
3. **Agent-autonomous** — agent auto-drifts on significant findings *(future)*

**When should you drift?** When your findings would save another context significant
rework. Found the root cause of a bug? Drift it. Discovered the right API pattern?
Drift it. Made a design decision that affects shared code? Drift it.

This aligns with how human minds juggle parallel investigation threads — you don't
keep every thread loaded, you context-switch with a mental summary. Drift is that
summary, made explicit and durable.

## Usage

### From kaish (shell mode)

```bash
# List contexts in this kernel
drift ls

# Push a finding to another context
drift push a1b2c3 "The auth bug is a race condition in session.rs:42"

# Push with LLM distillation (summarizes your whole context first)
drift push a1b2c3 --summarize

# Review what's queued
drift queue

# Deliver staged drifts
drift flush

# Pull from another context (LLM distills it for you)
drift pull a1b2c3

# Pull with a focused question
drift pull a1b2c3 "what was decided about caching?"

# Merge a forked context back to its parent
drift merge a1b2c3
```

**How delivery works:** The sender *stages* content, then *flushes* it. On flush, the
content is injected as a `BlockKind::Drift` block into the target's document. There's
no notification system yet — the block simply appears in the target document like any
other message. Recipients see it when they next read their document.

**Push vs. Pull:**
- **Push** = "I have something for you" — source initiates, stages content for target
- **Pull** = "I want something from you" — destination initiates, reads + distills from source

Agents will typically `push` + `flush` in sequence, since there's rarely a reason
to batch review when operating programmatically.

### From RPC (Cap'n Proto)

All drift operations are exposed via the Kernel interface:

| RPC Method | Description |
|------------|-------------|
| `driftPush` | Stage content or distilled summary |
| `driftFlush` | Deliver staged drifts |
| `driftQueue` | View staging queue |
| `driftCancel` | Cancel a staged drift |
| `driftPull` | Read + distill from source context |
| `driftMerge` | Merge fork into parent |
| `listAllContexts` | List all registered contexts |
| `getContextId` | Get caller's short ID |

### From the client library

```rust
// Via ActorHandle (Send+Sync, concurrent)
let actor = spawn_actor(config, kernel_id, context_name, instance, None);

// Push content to another context
let staged_id = actor.drift_push("a1b2c3", "found the bug", false).await?;

// Flush to deliver
let count = actor.drift_flush().await?;

// Pull with distillation
let block_id = actor.drift_pull("a1b2c3", Some("what about auth?")).await?;

// List all contexts
let contexts = actor.list_all_contexts().await?;
```

## DriftKind

How content arrived — stored on the drift block for provenance:

| Kind | Meaning |
|------|---------|
| `Push` | Direct content transfer ("here's what I found") |
| `Pull` | Source was read and LLM-distilled into caller's context |
| `Distill` | Like Push, but LLM-summarized before staging |
| `Merge` | Fork summarized back into parent (like git merge for conversations) |
| `Commit` | Conversation → git commit message (utility, not cross-context) |

## Staging Model

Drift uses a two-phase staging pattern:

1. **Stage** — `drift push` or `drift push --summarize` adds to the queue
2. **Flush** — `drift flush` delivers all staged drifts by injecting `BlockKind::Drift`
   blocks into target documents

This lets you review what's queued before delivery (`drift queue`) and cancel
mistakes (`drift cancel <id>`). Pull and merge bypass staging — they're immediate
because the caller explicitly requested the content.

Agents will typically push and flush in immediate sequence. The staging model is
most useful for humans who want to review before sending, or for batching multiple
drifts to different contexts into a single delivery.

## LLM Distillation

Drift includes built-in LLM-powered summarization for cross-context transfer:

**System prompt** (used for `--summarize`, `pull`, and `merge`):
> Summarize this conversation for transfer to another context. Be concise.
> Preserve: key findings, decisions made, code references, and open questions.
> Format as a briefing, not a transcript. Use bullet points for clarity.
> Keep it under 500 words.

The distillation process:
1. Read all blocks from source context's document
2. Format as a labeled transcript (truncating blocks > 2KB)
3. Optionally add a directed focus prompt ("what about caching?")
4. Send to the context's configured LLM provider
5. The summary becomes the drift block content

This means **different contexts can use different models** and drift still works —
Claude distills for Claude, Gemini distills for Gemini, and the summaries bridge
the gap.

Future: distillation prompts will be scriptable via Rhai, so teams can customize
the summarization style per-context or per-drift-kind.

## Architecture

### Components

```
┌─────────────────────────────────────────────────────────────┐
│                         Kernel                               │
│                                                              │
│  SharedDriftRouter = Arc<RwLock<DriftRouter>>                │
│  ├── contexts: HashMap<short_id, ContextHandle>              │
│  ├── staging: Vec<StagedDrift>  (push → stage → flush)       │
│  └── context_to_short: HashMap<name, short_id>               │
│                                                              │
│  DriftEngine (ExecutionEngine for `drift` command)           │
│  ├── drift ls       — list contexts                          │
│  ├── drift push     — stage content for target               │
│  ├── drift pull     — read + distill from source             │
│  ├── drift merge    — summarize fork into parent             │
│  ├── drift flush    — deliver staged drifts                  │
│  ├── drift queue    — inspect staging queue                  │
│  └── drift cancel   — remove from queue                      │
│                                                              │
│  SharedBlockStore (all contexts' documents live here)        │
└─────────────────────────────────────────────────────────────┘
```

### ContextHandle

Each registered context maps a short hex ID (6 chars from UUID) to:

| Field | Type | Purpose |
|-------|------|---------|
| `short_id` | `String` | Unique address (e.g., `"a1b2c3"`) |
| `context_name` | `String` | Human name (e.g., `"main"`, `"debug-auth"`) |
| `document_id` | `String` | Primary document in the shared BlockStore |
| `pwd` | `Option<String>` | Working directory in VFS (for git operations) |
| `provider` | `Option<String>` | LLM provider name (e.g., `"anthropic"`) |
| `model` | `Option<String>` | Model name (e.g., `"claude-opus-4-6"`) |
| `parent_short_id` | `Option<String>` | Parent context (set on fork/thread, enables merge) |
| `created_at` | `u64` | Creation timestamp (Unix epoch seconds) |

## Fork/Thread and Drift

Fork and thread create new contexts. The `SharedDriftRouter` is shared via
`Arc::clone` across parent and child, so both immediately see each other:

```rust
// In kernel.fork():
let drift = Arc::clone(&self.drift);  // shared router

// In kernel.thread():
let drift = Arc::clone(&self.drift);  // shared router
```

**Fork lineage** is tracked via `parent_short_id` on the `ContextHandle`. This
enables `drift merge` — summarize a forked exploration and inject the findings
back into the parent context.

```
Parent (main)          Fork (debug-auth)
    │                       │
    │  fork ───────────────►│
    │                       │  ... explores, finds root cause ...
    │                       │
    │◄── drift merge ───────│  (LLM summarizes fork, injects into parent)
    │                       │
    ▼  [drift block with    │
       merge summary]       │
```

## Error Handling

Drift errors are loud and educational — they tell you what went wrong and what
your options are:

| Operation | Error condition | What happens |
|-----------|----------------|--------------|
| `drift push <ctx>` | Target context doesn't exist | Error with list of available contexts |
| `drift merge <ctx>` | Context has no `parent_short_id` | Error explaining that merge requires fork lineage |
| `drift flush` | Nothing staged | No-op (zero blocks delivered, not an error) |
| `drift pull <ctx>` | Source document is empty | Error or empty summary (no content to distill) |
| `drift cancel <id>` | ID doesn't exist in staging | Error with current queue contents |

## Implementation Status

| Feature | Status |
|---------|--------|
| DriftRouter (context registry + staging) | ✅ Implemented |
| DriftEngine (kaish `drift` command) | ✅ Implemented |
| RPC handlers (push/flush/pull/merge/queue/cancel) | ✅ Implemented |
| Client library API | ✅ Implemented |
| ActorHandle (Send+Sync wrapper) | ✅ Implemented |
| E2e tests (push→queue→flush through EmbeddedKaish) | ✅ Implemented |
| LLM distillation (summarize for transfer) | ✅ Implemented |
| BlockKind::Drift (first-class CRDT block type) | ✅ Implemented |
| Fork lineage tracking (parent_short_id) | ✅ Implemented |
| UI rendering of drift blocks | 🚧 Renders as text, no provenance chrome |
| MCP exposure of drift operations | 📋 Planned (kaijutsu-mcp) |
| Automated drift (agent-initiated) | 📋 Planned |

## Future

For full multi-context drifting (5-10 concurrent agents, each in their own context,
freely drifting findings between each other), we'll need:

1. **kaijutsu-mcp** — MCP server exposing drift operations to external agents
   (Claude Code, opencode, Gemini CLI) so they can participate as contexts
2. **Context lifecycle management** — automated creation/cleanup of forked contexts
3. **Drift policies** — rules for when to auto-push (e.g., on checkpoint, on
   significant finding, on tool result above a quality threshold)
4. **UI for drift provenance** — visual attribution showing where content came
   from, which model distilled it, and the fork lineage
5. **Cross-kernel drift** — the server has a `DriftRouter` too, enabling drift
   between kernels (not just contexts within a kernel)

## References

- Implementation: `crates/kaijutsu-kernel/src/drift.rs` (1522 lines)
- CRDT types: `crates/kaijutsu-crdt/src/block.rs` (BlockKind::Drift, DriftKind)
- RPC handlers: `crates/kaijutsu-server/src/rpc.rs` (lines 3949-4393)
- Client API: `crates/kaijutsu-client/src/actor.rs` (ActorHandle drift methods)
- E2e tests: `crates/kaijutsu-server/tests/e2e_dispatch.rs` (Tier 1-2)
- Block tools spec: [block-tools.md](block-tools.md)
- Kernel model: [kernel-model.md](kernel-model.md)
