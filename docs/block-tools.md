# Block Tools Design

*CRDT-native tool interface for models and humans to collaborate on blocks.*

**Status:** Implemented (v2)
**Authors:** Amy, Claude, Gemini
**Last updated:** 2026-01-23

---

## Problem

The server conflates "cells" (UI concept) with "blocks" (kernel primitive). This creates leaky abstractions. The kernel should speak in blocks; the UI renders blocks however it wants.

## Goals

1. **Clean separation** — Kernel → Blocks, UI → Cells (presentation)
2. **CRDT-native** — All mutations are CRDT ops, always syncable
3. **Model-friendly** — Line-based editing (matches training data)
4. **Human-friendly** — Same primitives power editor UI
5. **Streaming-optimized** — Efficient path for model output

---

## Core Concepts

### What is a Block?

A block is the fundamental content primitive. Same Rust type, different semantic kinds:

| Kind | Use | Has Parent | Typical Size |
|------|-----|------------|--------------|
| `text` | Model/user messages | Yes (DAG) | < 1 KB |
| `thinking` | Extended reasoning | Yes | < 10 KB |
| `tool_call` | Tool invocation | Yes | < 1 KB |
| `tool_result` | Tool output | Yes | < 100 KB |

All blocks form a DAG via `parent_id`. This enables:
- Threading (fork a conversation)
- Pruning (collapse old context)
- Including artifacts (code blocks in conversation)

### Block Identity

- **block_id**: Composite of (cellId, agentId, seq)
- **Globally unique** within a kernel

### Block Lifecycle

```
                    ┌──────────┐
       create()     │ pending  │
          ─────────▶│          │
                    └────┬─────┘
                         │ first write
                         ▼
                    ┌──────────┐
                    │ running  │◀─────┐
                    │          │      │ edits (CRDT ops)
                    └────┬─────┘──────┘
                         │
                         │ generation complete
                         ▼
                    ┌──────────┐
                    │  done /  │
                    │  error   │
                    └──────────┘
```

**All blocks use the same lifecycle.** No special "streaming" status.

- **Status enum**: `pending`, `running`, `done`, `error`

**Why no streaming status?** CRDT handles concurrent edits naturally:
- Model appends tokens at end of block
- Human edits earlier in block
- CRDT merges both correctly
- Model doesn't see human edits until next turn (context is frozen)
- KV cache invalidation is a future optimization, ops are always correct

### Persistence Model

```
┌─────────────┐      ┌─────────────┐      ┌─────────────┐
│  Filesystem │ ←──→ │   SQLite    │ ←──→ │   Memory    │
│  (git repo) │      │  (runtime)  │      │  (hot ops)  │
└─────────────┘      └─────────────┘      └─────────────┘
      ▲                                         │
      │              periodic flush             │
      └─────────────────────────────────────────┘
```

- **File blocks**: Loaded lazily on first access, kept in SQLite, LRU eviction
- **Conversation blocks**: Always in SQLite, never evicted (history)
- **Flush to filesystem**: Kernel manages async, whole-file writes to git repo

**Dirty tracking** (synthesized):
```rust
dirty = block.version > block.last_persisted_version
```

---

## Architecture

```
┌─────────────────────────────────────────────────────────────┐
│                     Model / Human / MCP                      │
└─────────────────────────────┬───────────────────────────────┘
                              │ Tool calls
                              ▼
┌─────────────────────────────────────────────────────────────┐
│                    Block Tool Interface                      │
│         block.create, block.append, block.edit, ...         │
└─────────────────────────────┬───────────────────────────────┘
                              │ Semantic operations
                              ▼
┌─────────────────────────────────────────────────────────────┐
│                   Translation Layer                          │
│  • Line offsets → byte offsets                              │
│  • Compare-and-set validation                               │
│  • Append batching (flush on newline/timeout/size)          │
└─────────────────────────────┬───────────────────────────────┘
                              │ CRDT operations
                              ▼
┌─────────────────────────────────────────────────────────────┐
│                    BlockDocument (CRDT)                      │
│  OpLog, versions, merge/sync                                │
└─────────────────────────────┬───────────────────────────────┘
                              │ Op streaming
                              ▼
                    Subscribed clients (sync)
```

---

## Tool Interface

### Block Lifecycle

```
block.create {
  parent_id: BlockId?,      // DAG parent (conversation blocks)
  role: Role,               // user | model | system | tool
  kind: BlockKind,          // text | thinking | tool_call | tool_result | file
  content: String?,         // optional initial content
  metadata: {
    path: String?,          // for file blocks
    language: String?,      // for code
    tool_name: String?,     // for tool_call
    ...
  }
} -> { block_id: BlockId, version: u64 }

block.status {
  block_id: BlockId,
  status: Status,
} -> { version: u64 }
```

### Content Mutation

**Streaming append** (model output hot path):

```
block.append {
  block_id: BlockId,
  text: String,
} -> { version: u64 }
```

Implementation note: Translation layer batches appends. Flush triggers:
- Newline character
- Silence > 100ms
- Buffer > 50 chars

This reduces OpLog bloat while maintaining liveness.

---

**Line-based editing** (primary interface for models):

```
block.edit {
  block_id: BlockId,
  operations: [
    {
      op: "insert",
      line: u32,              // insert before this line (0-indexed)
      content: String
    },
    {
      op: "delete",
      start_line: u32,
      end_line: u32           // exclusive
    },
    {
      op: "replace",
      start_line: u32,
      end_line: u32,          // exclusive
      content: String,
      expected_text: String?  // CAS: fails if current text doesn't match
    },
  ]
} -> { version: u64 }
```

**Semantics:**
- **Atomic**: If any operation fails (CAS mismatch, out-of-range), entire batch fails
- **expected_text**: Compare-and-set guard. Model proves it read current content before overwriting.
- **Line numbers**: 0-indexed, `end_line` is exclusive (like Python slices)

---

**Character-based editing** (programmatic/refactoring tools, not LLMs):

```
block.splice {
  block_id: BlockId,
  offset: u64,              // byte offset
  delete_count: u64,
  insert: String?,
} -> { version: u64 }
```

---

**Unified diff** (alternative for complex refactors):

```
block.apply_patch {
  block_id: BlockId,
  patch: String,            // standard unified diff format
  dry_run: bool?,           // validate without applying
} -> {
  success: bool,
  errors: [String]?,        // if dry_run or failed
  version: u64?             // if applied
}
```

### Reading

```
block.read {
  block_id: BlockId,
  line_numbers: bool?,      // prefix lines with numbers (default: true)
  range: {
    start: u32,
    end: u32
  }?,                       // line range, exclusive end (default: all)
} -> {
  content: String,
  metadata: Object,
  role: Role,
  kind: BlockKind,
  status: Status,
  version: u64,
  line_count: u32,
}
```

---

**Search within block** (helps models find line numbers):

```
block.search {
  block_id: BlockId,
  query: String,            // regex or literal
  context_lines: u32?,      // lines before/after match (default: 2)
  max_matches: u32?,        // limit results (default: 20)
} -> [{
  line: u32,
  content: String,          // matched line + context
  match_start: u32,         // column of match start
  match_end: u32,
}]
```

---

**List blocks**:

```
block.list {
  parent_id: BlockId?,      // filter by parent (conversation threading)
  kind: BlockKind?,
  status: Status?,
  path_prefix: String?,     // filter file blocks by path
  depth: u32?,              // DAG traversal depth (default: 1)
} -> [{
  block_id: BlockId,
  parent_id: BlockId?,
  role: Role,
  kind: BlockKind,
  status: Status,
  path: String?,            // for file blocks
  summary: String,          // first N chars or line count
  version: u64,
  dirty: bool,              // for file blocks
}]
```

---

**Cross-block search** (grep across kernel):

```
kernel.search {
  query: String,            // regex
  kinds: [BlockKind]?,      // filter by kind (default: [file])
  path_prefix: String?,     // filter by path
  context_lines: u32?,
  max_matches_per_block: u32?,
  max_blocks: u32?,
} -> [{
  block_id: BlockId,
  path: String?,
  matches: [{
    line: u32,
    content: String,
  }]
}]
```

### Sync

For clients, not typically used by models directly:

```
block.subscribe {
  block_ids: [BlockId]?,              // specific blocks, or all if empty
  from_versions: [(BlockId, u64)]?,   // resume from versions
} -> Stream<BlockPatch>

block.sync {
  versions: [(BlockId, u64)],
} -> {
  patches: [BlockPatch],
  new_blocks: [BlockSnapshot]
}
```

**Op streaming**: CRDT ops flow to all subscribed clients in real-time. This enables:
- Multiple humans editing same file (Google Docs style)
- Model and human editing concurrently (CRDT merges)
- Offline clients catching up on reconnect

---

## Translation Layer

### Line → Byte Offset

```rust
fn line_to_byte_offset(content: &str, line: u32) -> Result<u64, EditError> {
    if line == 0 {
        return Ok(0);
    }

    let mut offset = 0;
    for (i, line_content) in content.lines().enumerate() {
        if i as u32 == line {
            return Ok(offset);
        }
        offset += line_content.len() as u64 + 1; // +1 for newline
    }

    // Allow inserting at end
    if line as usize == content.lines().count() {
        Ok(offset)
    } else {
        Err(EditError::LineOutOfRange {
            requested: line,
            max: content.lines().count() as u32
        })
    }
}
```

### Compare-and-Set Validation

```rust
fn validate_expected_text(
    content: &str,
    start_line: u32,
    end_line: u32,
    expected: &str
) -> Result<(), EditError> {
    let actual: String = content
        .lines()
        .skip(start_line as usize)
        .take((end_line - start_line) as usize)
        .collect::<Vec<_>>()
        .join("\n");

    if actual == expected {
        Ok(())
    } else {
        Err(EditError::ContentMismatch {
            expected: expected.to_string(),
            actual,
            start_line,
            end_line,
        })
    }
}
```

### Append Batching

```rust
struct AppendBuffer {
    block_id: BlockId,
    buffer: String,
    last_flush: Instant,
}

impl AppendBuffer {
    fn append(&mut self, text: &str, doc: &mut BlockDocument) {
        self.buffer.push_str(text);

        if self.should_flush() {
            self.flush(doc);
        }
    }

    fn should_flush(&self) -> bool {
        self.buffer.contains('\n')
            || self.buffer.len() > 50
            || self.last_flush.elapsed() > Duration::from_millis(100)
    }

    fn flush(&mut self, doc: &mut BlockDocument) {
        if !self.buffer.is_empty() {
            let offset = doc.len();
            doc.insert(offset, &self.buffer);
            self.buffer.clear();
            self.last_flush = Instant::now();
        }
    }
}
```

---

## Migration Path

### Phase 1: Implement block tools
- Add `block.*` tools to kernel alongside existing `cell.*` tools
- Cell tools internally delegate to block tools
- Both interfaces work

### Phase 2: Migrate consumers
- Update kaish to use block tools
- Update Cap'n Proto schema to expose block interface
- Client uses "cells" for UI, backed by blocks via RPC

### Phase 3: Remove cell abstractions from kernel
- Delete `CellKind`, `CellEntry`, `cell_tools.rs`
- "Cell" becomes purely a client/UI concept

---

## Design Decisions (Resolved)

| Question | Decision | Rationale |
|----------|----------|-----------|
| Line vs char editing | Line-based primary | Matches model training data |
| CAS granularity | `expected_text` per-op | Prevents blind overwrites |
| Batch semantics | Atomic (all or nothing) | Predictable, simple error handling |
| Block identity | (cellId, agentId, seq) tuple | Globally unique within kernel |
| Persistence | SQLite runtime, async flush to FS | Fast ops, safe persistence |
| Dirty tracking | Synthesized from versions | No separate flag needed |
| Cross-block search | Yes, simple first | Index later for scale |
| Concurrent edits | Let CRDT merge | No locking needed, model context frozen per turn |
| Block size limits | None for now | Optimize when we hit real problems |
| Undo/redo | Inverse ops in SQLite | CRDT-native, per-agent history |

## Not Yet Implemented

| Feature | Status |
|---------|--------|
| `block.apply_patch` | Documented but not implemented |
| `block.subscribe` / `block.sync` | CRDT primitives exist, not exposed as tools |
| Cursor position tracking | Module exists (`cursor.rs`), not documented |

---

## Undo/Redo

Undo is just more CRDT ops. Since we persist ops in SQLite, we can query history and apply inverses.

```rust
// Every op has an inverse
impl Op {
    fn inverse(&self) -> Op {
        match self {
            Op::Insert { offset, text } => Op::Delete {
                offset: *offset,
                len: text.len(),
            },
            Op::Delete { offset, len, deleted_text } => Op::Insert {
                offset: *offset,
                text: deleted_text.clone(),
            },
        }
    }
}

// Undo last edit by agent
fn undo(block_id: BlockId, agent_id: AgentId, db: &Db) -> Result<Op> {
    let last_op = db.query_last_op(block_id, agent_id)?;
    let inverse = last_op.inverse();
    db.apply_op(block_id, &inverse)?;
    Ok(inverse)
}
```

**Key insight**: Undo is agent-scoped. "Undo my last edit" not "undo the last edit globally."
This lets multiple agents work on the same block without stepping on each other's undo history.

---

## Open Questions

1. **Undo/redo granularity** — Undo via inverse ops in SQLite. But what's a "unit" of undo?
   - Per-op? (too granular — undo each character)
   - Per-tool-call? (model's `block.edit` is one undo unit)
   - Time-based batching? (ops within 1s = one undo unit)
   - Agent-scoped? (undo my last edit vs undo anyone's last edit)

2. **Block size** — No limits for now. Revisit when we hit a real problem.

3. **Multi-cursor UI** — When multiple humans edit, show their cursors?
   - Pure UI concern, but kernel might need to track cursor positions
   - Or: cursors are ephemeral, not persisted, handled entirely client-side

---

## References

- [kernel-model.md](kernel-model.md) — Kernel architecture
- [diamond-types fork](https://github.com/tobert/diamond-types) — Our CRDT implementation
- Claude Code Edit tool — line-based editing interface
- Figma LiveGraph — multiplayer CRDT architecture

---

## Changelog

**2026-01-23**
- Updated status from "Draft" to "Implemented"
- Removed `file` BlockKind (not implemented in code)
- Fixed Status enum: `active` → `running` to match implementation
- Updated block identity to match actual (cellId, agentId, seq) tuple
- Added "Not Yet Implemented" section for apply_patch, subscribe/sync
- Updated diamond-types reference to fork URL
