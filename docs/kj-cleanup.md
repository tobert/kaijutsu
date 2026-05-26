# kj / MCP cleanup

We've drifted into a hybrid (kj-rich, MCP-narrow) without fully committing to
it. This doc locks the target shape so parallel work (kaish-clap, this repo's
MCP slim-down) doesn't diverge.

## Direction (locked)

**Hybrid, with `context_shell` as the rich entry point.** Kaish is not an MCP
client — tool-surface footprint matters too much in agent context. kj stays
the rich kernel-verb surface; MCP exposes a narrow set of escape hatches and
session/peer/turn primitives. The LLM uses both; the kj prompt uses kaish only.

Counter-evidence that would re-open this: per-verb MCP policies needed (hooks
that fire on `fork` but not `drift push`), or schema-driven typed args at the
MCP boundary becoming load-bearing for model UX.

## Surface split

### Stays MCP (no kj equivalent)

Session/peer/turn concerns — not context-verbs.

- `register_session`, `whoami`
- `invoke_peer`
- `read_input`, `write_input`, `edit_input`, `submit_input`
- `context_shell`, `kaish_exec` — the entry points themselves
- `list_kernel_tools`, `tool_search` — discovery

### Moves to kj (delete the MCP duplicate after audit)

These already mostly exist on kj. Audit gates the deletion.

- `block_create`, `block_read`, `block_append`, `block_edit`, `block_list`,
  `block_status`, `block_exclude`, `block_inspect`, `block_history`, `block_diff`
  → `kj block ...` (mostly already there)
- `doc_list`, `doc_tree` → `kj doc list|tree` (new)
- `kernel_search` → `kj search`
- `stage_commit` → `kj stage commit`

### Lifts from "only via kj shell today" to first-class kj (no MCP, no change)

Already kj-native — listing for completeness:

`context`, `fork`, `drift`, `cas`, `attach`, `rc`, `workspace`, `preset`,
`cache`, `synth`, `prompt`, `stage`

## `context_shell` target shape

Current return is the ToolResult block's `content` as a bare `String`. That
flattens three things we need to flow:

1. **`KjResult::data`** — the structured JSON the dispatcher already
   produces (array of identifiers for list commands, object for inspect, etc.;
   conventions in `kj/mod.rs:90-100`). Today: dropped at the MCP boundary.
2. **Exit code** — `KjResult::Err` and shell `exit_code != 0` are
   indistinguishable from success in the current return. Agents can't detect
   failure without text-matching.
3. **Block id** — useful for follow-ups (`block_read`, etc.); today the only
   surface that exposes it is the timeout error message.

### New return: JSON object

```json
{
  "stdout": "human-readable text",
  "data": <structured JSON or null>,
  "exit_code": 0,
  "block_id": "...",
  "content_type": "text/plain" | "text/markdown" | "application/json" | ...,
  "ephemeral": false,
  "elapsed_ms": 1234
}
```

Wire this through `execute_and_poll_shell` — return the ToolResult block snapshot
rather than just `content`, then assemble the JSON.

### Completion: investigate first, fix second

Code at `kaijutsu-mcp/src/lib.rs:575` already subscribes to
`BlockStatusChanged` events with a 500ms store-check fallback. The
orchestration.md:239 "doesn't observe completion" claim may be stale OR there's
a real bug (parent_id mismatch, event not emitted for `kj` results, etc.).
Reproduce before patching. If genuinely broken, fix the event/match path; do
not extend the timeout.

### Open: timeout semantics

300s wall-clock cap stays — what changes is whether timeout returns the
partial state (block_id, current status, accumulated content) as a structured
result or as the current text error. Recommend structured — agent can decide
to poll `block_read` or move on.

## Completion architecture

Depends on parallel kaish-clap session. Once kaish builtins use clap_derive,
kj subcommands do too. Then:

- `kj __complete --line "..." --point N` exposes clap's completion machinery
- Tiny bash/zsh/fish wrappers delegate to `kj __complete` (cargo/gh pattern)
- kaish REPL gains a `Tool::complete` hook calling the same path; LSP/MCP can
  also call it

Until kaish-clap lands, no completion work happens here — we'd be building
infrastructure we throw away.

## Migration sequence

Numbered for ordering, not for atomic PRs. Each step lands independently.

1. **This doc lands.** Decisions captured; parallel session can read it.
2. **context_shell structured return + completion investigation.** TDD —
   integration tests lock in the JSON shape and the block_status wait. Doesn't
   change any other MCP tool.
3. **Parity audit.** Read-only inventory: for each "moves to kj" tool, confirm
   the kj equivalent exists and matches feature-for-feature. Outputs a
   checklist; no code changes. Gates step 5.
4. **(Parallel session lands kaish-clap.)** Watch for `Tool` trait stability.
5. **kj uses clap_derive.** Lifts the new MCP-deprecated verbs (`doc`,
   `search`, `stage commit`) as new kj subcommands.
6. **MCP slim-down.** Delete the duplicate `block_*` / `doc_*` / `kernel_search`
   / `stage_commit` MCP tools. Update orchestration.md and any agent-facing
   docs.
7. **Completion ships.** `kj __complete` + shell wrappers + kaish `Tool::complete`
   hook.

## Decisions for Amy

- **Return JSON shape** above — accept or revise?
- **Timeout-on-partial-result** — return structured partial or keep current
  text-error?
- **`kj search` vs keep `kernel_search` as MCP-only** — search is a discovery
  primitive; arguments for keeping it MCP-side (it's something an agent does
  *before* it knows about kj). My lean: move it. Open to push-back.
- **`kj doc` namespace** — `kj doc list` / `kj doc tree`, or fold under
  existing `kj context`? Docs and contexts are 1:1 in practice, so the
  separate namespace might be overhead.
