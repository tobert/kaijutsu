# MCP → kj parity audit

Gating the MCP slim-down step in docs/kj-cleanup.md (migration step 3).

Each "FULL PARITY" line is safe to delete from MCP after audit passes. PARTIAL/MISSING requires implementation before deletion.

## Block tools

- [x] **block_list** — FULL PARITY via `kj block list [--kind|--role|--status|--context] [--json]`
- [x] **block_inspect** — FULL PARITY via `kj block inspect <block-id> [--json]`
- [x] **block_status** — FULL PARITY via `kj block status <block-id> <new-status>`
- [x] **block_create** — FULL PARITY via `kj block create --role --kind [--content] [--parent] [--after] [--context]`
- [x] **block_read** — FULL PARITY via `kj block read <block-id> [--no-line-numbers] [--range start:end]`
- [x] **block_append** — FULL PARITY via `kj block append <block-id> --text "..."`
- [x] **block_edit** — FULL PARITY via `kj block edit <block-id> {insert|delete|replace}` (MCP's multi-op batch becomes one-op-per-invocation; CAS preserved on replace)
- [x] **block_exclude** — FULL PARITY via `kj stage exclude <block-id>` / `kj stage include <block-id>`
- [x] **block_history** — FULL PARITY via `kj block history <block-id>`
- [x] **block_diff** — FULL PARITY via `kj block diff <block-id> [--original "..."]`

**parity: 10/10 — all block_* MCP tools deletable**

## Document tools

The doc namespace is a separate primitive from context — see CLARIFICATION
in docs/kj-cleanup.md (Code, Text, Config docs exist without contexts).
`kj doc` is the storage-layer surface; `kj context` stays as the
conversation-management surface. Not duplication.

- [x] **doc_list** — FULL PARITY via `kj doc list [--kind <k>] [--json]`. Shows non-conversation docs that `kj context list` hides; attaches context metadata when present.
- [x] **doc_tree** — FULL PARITY via `kj doc tree <id> [--max-depth N] [--expand-tools]`. Inlined `format_dag_tree` from `kaijutsu-mcp/src/tree.rs` (factor to `kaijutsu-crdt` later).
- [x] **doc_create** — FULL PARITY via `kj doc create [--kind <k>] [--language <l>] [--id <hex>]`. For Conversation kind, prefer `kj context create` (also registers metadata).
- [x] **doc_delete** — FULL PARITY via `kj doc delete <id> [--confirm <nonce>]`. Latch-gated — same destructive-op pattern as `kj context archive`.

**parity: 4/4 — all doc_* MCP tools deletable**

## Search

- [x] **kernel_search** — FULL PARITY via `kj search <pattern> [--all|--context] [--kind|--role] [--context-lines N] [--max-matches N] [--json]`

**parity: 1/1**

## Stage (management of liminal staging state)

- [x] **stage_commit** — FULL PARITY via `kj stage commit` (also: `kj stage include|exclude|status`)

**parity: 1/1**

## Summary

**Overall parity: 16/16 tools (100%) safe to delete after this PR series lands**

All MCP tools in scope for the slim-down now have kj equivalents:

- 10 block_* tools via the new `kj block` clap namespace
- 4 doc_* tools via the new `kj doc` clap namespace (own primitive,
  not duplication — see preamble under Document tools)
- `stage_commit` → `kj stage commit`
- `kernel_search` → `kj search`

The next step in `docs/kj-cleanup.md` (MCP slim-down PR) is unblocked.
Recommended deletion order:
1. Block tools (10) — kj block has been tested and used in this session
2. doc_* (4)
3. kernel_search (1)
4. stage_commit (1) — least risk, already a thin wrapper around `kj stage commit`

## Gaps to close before MCP slim-down (prioritized by delete-blocking)

### Blocking slim-down of block_read (1 tool, 1 gap)

1. **`kj block read` with content + formatting**
   - Accept `<block-id>` and optional `--range start:end` to extract line range
   - Accept `--lines` / `-n` to include line numbers in output
   - Return formatted text suitable for editing (same as MCP `block_read`)
   - Dependencies: existing `block_inspect` can be extended; reuse `line_count`, `extract_lines_with_numbers` helpers from MCP

### Blocking slim-down of block_create, block_append, block_edit (3 tools, requires content-mutation surface)

2. **`kj block create` — spawn a new block in a context**
   - Signature: `kj block create [--role user|model|system|tool] [--kind text|thinking|...] [--parent <block-id>] [--after <block-id>] [<content-arg-or-stdin>]`
   - Dependencies: store API exists (`insert_block_as`); needs CLI wrapper

3. **`kj block append` — stream content into a block**
   - Signature: `kj block append <block-id> <text-arg-or-stdin>`
   - Use case: kaish pipes, MCP tools that stream results
   - Dependencies: store API exists (`append_text_as`); needs CLI wrapper

4. **`kj block edit` — line-based in-place edits**
   - Signature: `kj block edit <block-id> --op insert:10:text | delete:10:20 | replace:10:20:text [--expected "text" for CAS]`
   - Dependencies: store API exists (`edit_text_as`); needs CLI wrapper + operation parsing

### Blocking slim-down of block_history, block_diff (2 tools, requires timeline/diff verbs)

5. **`kj block history` — timeline of a block**
   - Signature: `kj block history <block-id> [--json]`
   - Return: created_at, author, version, content_length, status evolution
   - Dependencies: snapshot metadata available; needs formatting layer

6. **`kj block diff` — unified diff against original text**
   - Signature: `kj block diff <block-id> [--original <file-or-text>]`
   - If no original: show current content summary
   - Return: unified diff
   - Dependencies: unified diff algorithm (can reuse MCP impl); needs CLI wrapper

### Blocking slim-down of doc_list, doc_tree, doc_create, doc_delete (4 tools, new doc namespace)

7. **`kj doc` namespace** — context-document aliasing
   - Decision needed (from kj-cleanup.md "Decisions for Amy"): **`kj doc list|tree|create|delete` vs fold under `kj context`?**
   - Current state: documents 1:1 with contexts; separate namespace might be overhead
   - Placeholder: assume `kj doc` subcommands (lowest-effort path)

   a. **`kj doc list [--context <ref>] [--json]`**
      - Alias for `kj context list` with document kind metadata (conversation/code/text/git)
      - Dependencies: metadata already in `KernelDb`; needs CLI wrapper

   b. **`kj doc tree <doc-id> [--max-depth N] [--expand-tools] [--json]`**
      - Conversational DAG visualization (already in MCP as tree.rs)
      - Dependencies: `format_dag_tree` is in MCP; can be moved to kernel or duplicated in kj

   c. **`kj doc create <id> --kind conversation|code|text|git [--language <lang>]`**
      - Explicit document creation (currently implicit via `kj context create`)
      - Dependencies: store API exists; needs CLI wrapper

   d. **`kj doc delete <doc-id>`**
      - Alias for `kj context remove` / `kj context archive`
      - Dependencies: store API exists; needs CLI wrapper

### Blocking slim-down of kernel_search (1 tool, requires search verb)

8. **`kj search` — regex search across blocks**
   - Signature: `kj search <regex> [--kind text|thinking|...] [--role user|model|...] [--context <ref>] [--context-lines N] [--max-matches N] [--json]`
   - Return: matches with surrounding context (same as MCP `kernel_search`)
   - Dependencies: regex matching + filtering already in MCP; needs kj CLI wrapper

---

## Implementation order for step 5 (kj uses clap_derive)

After kaish-clap lands and `KjDispatcher` gains `clap_derive` trait implementations, land these in parallel:

**Phase 1: Content mutation (enables block_create, block_append, block_edit deletion)**
- `kj block create` (lowest complexity; just wraps store API)
- `kj block append` (low complexity; wraps store API)
- `kj block edit` (medium complexity; needs operation parser)

**Phase 2: Introspection (enables block_history, block_diff deletion)**
- `kj block history` (low complexity; format snapshot metadata)
- `kj block diff` (medium complexity; diff algorithm already exists in MCP)

**Phase 3: Document metadata (enables doc_* deletion)**
- `kj doc create`, `kj doc delete` (low; wrap context lifecycle)
- `kj doc list` (low; alias for context list with doc metadata)
- `kj doc tree` (high; needs tree.rs migration or duplication)

**Phase 4: Discovery (enables kernel_search deletion)**
- `kj search` (medium; regex + filter wrapper around existing algorithms)

**Phase 5: Content reading (enables block_read deletion)**
- `kj block read` (low; wraps snapshot content + formatting)

---

## Implementation notes for each gap

### `kj block read` (blocks block_read deletion)

```
File: kaijutsu-kernel/src/kj/block.rs
Add to dispatch_block match:
  "read" => self.block_read(&argv[1..]),

Implementation:
- Parse <block-id> (required)
- Parse --range start:end (optional, overrides default show-all)
- Parse --lines / -n flag (default: true per MCP)
- Fetch block via blocks.block_snapshots(ctx_id)
- Format content with line_count + extract_lines_with_numbers helpers (copy from MCP lib.rs)
- Return KjResult::ok(formatted_content, ContentType::Plain)
```

### `kj block create`, `append`, `edit` (block mutations)

```
Dependencies: store.insert_block_as, append_text_as, edit_text_as (all async)
Block: store API is sync; kj block.rs is sync. Need either:
  - Make store API callable from sync context (unlikely, CRDTs are async)
  - Move mutations to kaish shell entry (context_shell) only (current pattern)

Decision: For now, keep mutations MCP-only. Revisit if sync store layer emerges.
```

### `kj block history`, `kj block diff`

```
Copy from MCP:
- BlockHistoryRequest → block_history() formatting
- BlockDiffRequest → block_diff() formatting
- Unified diff algorithm in lib.rs:1739+

File: kaijutsu-kernel/src/kj/block.rs
Add to dispatch_block match:
  "history" => self.block_history(&argv[1..]),
  "diff" => self.block_diff(&argv[1..]),
```

### `kj doc` namespace (decision point)

If accepting as separate namespace:

```
File: kaijutsu-kernel/src/kj/mod.rs
Add to dispatch() match:
  "doc" => self.dispatch_doc(&argv[1..], caller).await,

File: kaijutsu-kernel/src/kj/doc.rs (new)
Implement dispatch_doc, doc_list, doc_tree, doc_create, doc_delete.

Alternatively: fold under context via `kj context show <id> --tree` (lower overhead).
```

### `kj search`

```
File: kaijutsu-kernel/src/kj/mod.rs
Add to dispatch() match:
  "search" => self.dispatch_search(&argv[1..], caller).await,

File: kaijutsu-kernel/src/kj/search.rs (new)
Implement dispatch_search with regex + filters. Reuse algorithm from MCP kernel_search.
```

---

## Audit metadata

- **Date**: 2026-05-26
- **Scope**: MCP tools in `kaijutsu-mcp/src/lib.rs` vs kj subcommands in `kaijutsu-kernel/src/kj/`
- **Files checked**:
  - `/home/atobey/src/kaijutsu/crates/kaijutsu-mcp/src/lib.rs` (2298+ lines, 26 tools)
  - `/home/atobey/src/kaijutsu/crates/kaijutsu-mcp/src/models.rs` (request types)
  - `/home/atobey/src/kaijutsu/crates/kaijutsu-kernel/src/kj/mod.rs` (dispatcher)
  - `/home/atobey/src/kaijutsu/crates/kaijutsu-kernel/src/kj/block.rs` (block subcommands)
  - `/home/atobey/src/kaijutsu/crates/kaijutsu-kernel/src/kj/stage.rs` (stage subcommands)
  - `/home/atobey/src/kaijutsu/crates/kaijutsu-kernel/docs/help/kj.md` (command reference)

