# Conversation as Session

> **Status (2026-06-12): implemented, since refined.** The "hydrate once at
> boundary events, append-only thereafter" invariant below now has one
> deliberate exception: a context with a hydration policy
> (`kj context hydrate --window N`) rebuilds a *windowed* wire per turn via
> `ConversationMailbox::rehydrate_windowed` — prefix byte-stable, tail
> sliding (see `docs/chameleon.md`, "RC-driven hydration marker"). The
> fork-side selection/seam design that leans on the same keep-set is
> `docs/fork-filters.md`. The rest of this doc is accurate as the original
> design record.

## Goal

Stop rebuilding the LLM-bound message list from the block log on every turn.
The live **conversation** becomes append-only and authoritative once
hydrated; the **context** (block log, exclusions, edits) remains the durable
multi-writer record but no longer drives wire history per-turn.

See `CLAUDE.md` and the `architecture_context_invariants` memory for the
invariants this implements.

## Current state

`process_llm_stream` in `crates/kaijutsu-server/src/llm_stream.rs` calls
`hydrate_from_blocks` on every prompt and overwrites the per-context cache
(`ConversationCache` in `crates/kaijutsu-server/src/rpc.rs`). The cache exists
but is effectively a per-turn scratch buffer.

Consequences:

- Every prompt re-encodes the whole conversation into `RequestMessage`s.
- `block exclude` / `block edit` quietly affect *live* conversations the
  next turn, which doesn't actually match what the Anthropic API can do
  (history already sent is history sent).
- Big tool outputs balloon the wire payload every turn until they hit the
  provider input cap. This is what surfaced the design gap on Haiku
  (200K input limit).

## Target state

- One **session** per context, in-memory `Vec<LlmMessage>`.
- Session is hydrated *once* from blocks at boundary events: fork, new
  context, cold start, peer attach, eviction. Append-only afterward.
- A per-context **mailbox** owns ingress to the session:
  - Subscribes to `BlockFlow::Inserted` events for the context.
  - Translates each block into a session-append using the same
    role/tool-pairing logic `hydrate_from_blocks` uses today.
  - Holds non-tool-result writers while a tool_use is open, so wire
    history never interleaves unrelated blocks between a tool_use and
    its matching tool_result. Same gate applies to other
    must-travel-together pairs from multi-peer writers.
  - Drops/excludes/edits on already-flushed blocks are no-ops (logged at
    debug). Honest semantics: the API has already seen them.

## Implementation slices

### Slice A — stop per-turn rehydration (this work)

1. **Split `hydrate_from_blocks`** in
   `crates/kaijutsu-kernel/src/llm/mod.rs` into:
   - A per-block translator (`fn translate_block(state, block) -> ()`),
     statefully appending to or extending the session.
   - The current fold function, now expressed as
     `blocks.iter().fold(state, translate_block)`.
   No behavior change yet — pure refactor with the existing tests as
   the contract.

2. **Wire the mailbox.** A new subscriber on per-context `BlockFlow`
   (see `crates/kaijutsu-kernel/src/flows.rs`) consumes `Inserted`
   events and calls `translate_block`. `ExcludedChanged`, `Deleted`,
   `TextOps`, etc. are observed but produce only log events at this
   stage.

3. **Boundary detection in `process_llm_stream`.** Replace the
   unconditional `hydrate_from_blocks` call with: if the session is
   empty (cache miss), hydrate once; otherwise trust the mailbox.
   Fork already produces a new `context_id` so it gets a fresh slot
   for free — confirmed in `kj/fork.rs:120`.

**Out of scope for Slice A — gate left for follow-up.** The original
plan put a tool-pair gate inside the mailbox, queueing non-result
inserts while tool_uses are open. After reading the BlockFlow
shape we walked back: the mailbox-as-translator can't actually keep
the block log coherent — it just hides interleavings from the LLM
stream while leaving them in the durable log, where every future
bootstrap / fork sees them. A real gate sits at insert time (block
writers submit through a per-context queue; the queue holds non-
tool-result writes during open tool_uses). That's a bigger
architectural change touching every block writer; it deserves its
own slice with two concrete consumers (drift, peer tool calls) in
the design phase. Tracked as a follow-up alongside Slice B.

### Slice B — formalize Mailbox as a named type

Promote the BlockFlow subscriber into a `Mailbox` type with explicit
`flush()` / `gate_open()` semantics once we have a second async-event
source (drift integration, peer tool-state notifications) to validate
the shape against. Bring two concrete consumers to the design.

### Slice C — sqlite-backed session storage

Replace `DashMap<ContextId, Mutex<Vec<LlmMessage>>>` with a sqlite-backed
store keyed on `(context_id, message_seq)`. LRU eviction goes away —
cold start re-hydrates from blocks. Orthogonal to the semantic change;
defer until Slice A is settled.

## Tests (Slice A)

- Two prompts in one context send only the delta on the second turn.
- A `kj shell` call between LLM prompts shows up on the next turn
  (mailbox flush path).
- `block exclude` on a block that's already in the session does not
  remove it from the next wire payload (invariant #2).
- Fork-then-prompt after exclude *does* drop the excluded block
  (boundary re-hydrate).
- Cold-start (kernel restart) hydrate path still works.
- Tool-pair atomicity: a tool_use+tool_result pair issued while a
  parallel writer fires unrelated inserts ends up with the pair
  adjacent on the wire, unrelated blocks after.

## Known follow-ups (not in this slice)

- **Provider-side cache expiry as a hydrate trigger.** Anthropic
  prompt-cache TTL expiry isn't modeled today; once per-turn-hydrate
  is gone, long idles may carry messages the provider no longer caches.
  Tracked in tech_debt.md (#10).
- **Eviction-as-destruction.** Once hydrate is rare, evicting an
  in-memory session means a re-hydrate on next touch. Leave LRU as-is
  in Slice A; revisit alongside Slice C.
- **What does "attach" mean for sessions?** A peer reconnecting to a
  context the kernel has in memory should see the live session, not
  re-hydrate. Confirm `tech_debt_peer_reattach_on_reconnect.md` doesn't
  hide a gap here.
