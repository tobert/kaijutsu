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

## Runtime owner

The kernel's `TurnState` (`runtime/turn_state.rs`) owns conversation sessions,
image caching, and interrupts. Interactive RPC and headless turn submission
share `runtime/llm_stream.rs`. The kernel worker owns accepted turns through
cancellation and shutdown. Per-turn leases keep queued and running turns
visible until each releases ownership; context interruption signals all of them.
Headless requests use direct runtime admission. Approval delivery also runs
on the kernel worker, with cancellation and joined settlement.
See `docs/kaish-integration.md`.

## Tool pairing at send

`ConversationMailbox::catch_up` discovers new blocks by reading the durable
block log. It is a pull-based cursor, not an insert-event subscriber.
`snapshot()` repairs a clone of the accumulated history; it does not change
the mailbox or the durable blocks.

A pair the gate-resume driver fills in place (`crates/kaijutsu-kernel/src/runtime/approval_resume.rs`,
`act_on_executable_answer`) evicts the context's cached mailbox
(`ConversationCache::evict`), so the next turn hydrates cold from the block
log. Reset preserves the turn lock while any caller holds the session and
takes effect after the active turn releases it. First lookup and idle eviction
are serialized by the session registry; active and waiting turns cannot get
different locks for one context. Idle LRU eviction retains its current cold
hydration policy; changing that semantic boundary is separate work. `catch_up` folds blocks it has not seen; it has no way to notice that a
block it already folded was edited afterward, so a cached mailbox would keep
serving the pre-edit text for the rest of the conversation.

Hydration closes each assistant call batch in one pass over messages. Each
`tool_use` gets a `tool_result` in the immediately following user message,
before ordinary content. Adjacent real results survive; missing results get
explicit interruption errors. Results outside that adjacent reply are
dropped with warnings, including late results for a call already answered
synthetically. Unrelated content remains in order. This preserves the
existing interruption policy; it does not move later results into earlier
conversation turns. Duplicate call or result ids within a batch remain
ambiguous and are refused when sending.

`Provider::stream` validates both directions of the pairing before backend
dispatch. The check covers snapshots and messages appended by the live
agentic loop. It returns `LlmError::InvalidRequest` with the message index
and offending tool ids. The server stops immediately, records a visible
error, and publishes `TurnFlow::Failed`; it does not retry an invalid request.
Remediation remains: exclude the offending blocks, then fork.

This is a conversation projection and a send-time check. Writers can still
interleave blocks in the durable context. Insert-time tool-pair atomicity
remains the separate follow-up described below.

## When an inference stops at the output ceiling

A turn does not end because one inference did. When the provider stops with
`max_tokens` or `length` and the model completed no tool call, the loop writes
a `(System, Notification)` block saying the response stopped at the output
limit of N tokens and to continue from there, sends it as the next user
message — the same shape hydration would produce for that block — and takes
another inference. A driven worker has nobody to say "continue".

The turn does **not** continue when any of these holds, and then it ends as it
did before continuations existed:

- the inference completed a tool call, or made one whose arguments did not
  parse — the loop already carries on with the result, and the model already
  has its turn back;
- the turn has spent `MAX_OUTPUT_CEILING_CONTINUATIONS` continuations
  (`crates/kaijutsu-kernel/src/runtime/llm_stream.rs`, `CeilingStop`), so it
  ends with `TurnStopReason::MaxTokens`, the provider's own reason;
- a beat waits on the turn's output (a score delivery), because extra
  inferences would put slow work on the beat path and the resolver validates
  one block as a whole tune — see `docs/tracks.md`;
- an interrupt is pending, which names the ending it caused.

The decision comes before any durable write: a notice for an inference that
never runs would survive as an instruction to the *next* turn.

The replayed assistant message follows the hydrator's rules
(`llm/hydrate.rs`, `flush_assistant`), so one turn serializes the same way
live and rehydrated. Reasoning rides only with its continuity signature. A
truncated response is replayed only when it has text: reasoning alone is not
an assistant turn, and it is the common truncation — the whole ceiling goes to
reasoning before any text arrives. The notice then follows the previous
message, and the wire merges consecutive user messages.

A tool call whose arguments do not parse as JSON is recorded with its raw
text and answered with an error tool result naming the byte count and the
parser's position. The loop continues on that result; the turn does not fail,
and the ceiling notice is not added on top of it — one message about one
truncation.

## Input during a turn

Input that arrives while a model turn runs joins that turn; it does not
wait for the turn to end. The turn holding the context's conversation lock
opens an ingress (`runtime/turn_state.rs`, `TurnState::offer_input`). Three
sources offer their block after it is durable and after their rc lifecycle
has run, so the lifecycle's output rides with it:

| Source | Offered by | Extends the final inference | If never delivered |
|---|---|---|---|
| A player's submit | `prompt::submit` | yes | starts the next turn (`prompt::follow_up`) |
| A completion notice | `completion_notice::deliver` | when its notice allows an automatic resume | runs the notice's continuation check (`resume_after_turn`) |
| A drift arrival | `kj drift push` (`deliver_drift`) | no | waits in the log, as a drift into an idle context does |

A running turn accepts input unless an interrupt is pending. A refused
offer is handled as for an idle context: a submit starts a turn, a
completion runs its continuation check, a drift waits.

The turn delivers accepted input at two points, never mid-inference:

- **After a tool round**, behind that round's results. The next request
  already extends the cached prefix there. Any pending input triggers it.
- **After the final inference.** The replayed answer and then the input
  become the next request, and the turn takes one more inference. Only
  input whose source extends the final inference triggers it.

A turn that is stopping, or one a beat waits on, takes no input at either
point.

Delivery (`deliver_live_input`, `ConversationMailbox::live_tail`) reads
the log after the turn's write point, the block its next output follows.
It renders every unseen, hydratable block there as hydration would, then
moves the write point to the last of them. The turn's next blocks follow
the input in the log, so a later hydration sends what the turn sent.
Blocks folded at turn start, and drafts, are skipped. Other writers'
blocks in that span ride along, in the position a later hydration gives
them.

Input counts as delivered once the inference that carried it completes.
Each undelivered input keeps its own entry, so a drift arriving after a
note cannot cost the note its next turn. When the turn ends, undelivered
input is handled by its source's rule in the table: for example, when the
inference carrying it failed, when a beat waited on the turn, or when its
block sits before the write point. Stopping a turn also stops the input it
accepted, as interrupting a queued turn always did. The block stays in the
log for the next turn either way. A turn's ingress remembers what it
delivered after it closes, so an offer that arrives late starts no second
turn.

An unsent draft is never marked seen by the mailbox. Submitting promotes
the draft's own block id, so a draft that was open when a turn hydrated is
still delivered after it is sent.

## Before the session change

`process_llm_stream` in `crates/kaijutsu-server/src/llm_stream.rs` called
`hydrate_from_blocks` on every prompt and overwrote the per-context cache
(`ConversationCache` in `crates/kaijutsu-server/src/rpc.rs`). The cache existed
but was effectively a per-turn scratch buffer.

Consequences:

- Every prompt re-encodes the whole conversation into `RequestMessage`s.
- `stage exclude` / `block edit` quietly affect *live* conversations the
  next turn, which doesn't actually match what the Anthropic API can do
  (history already sent is history sent).
- Big tool outputs balloon the wire payload every turn until they hit the
  provider input cap. This is what surfaced the design gap on Haiku
  (200K input limit).

## Original target

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
- `stage exclude` on a block that's already in the session does not
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
  Tracked in `docs/issues.md`, "Older app and broker debt, carried out of
  auto-memory (2026-09-08)" — "Provider cache expiry is not a hydrate
  boundary". `tech_debt.md` does not exist in this repo.
- **Eviction-as-destruction.** Once hydrate is rare, evicting an
  in-memory session means a re-hydrate on next touch. Leave LRU as-is
  in Slice A; revisit alongside Slice C.
- **What does "attach" mean for sessions?** A peer reconnecting to a
  context the kernel has in memory should see the live session, not
  re-hydrate. Confirm `tech_debt_peer_reattach_on_reconnect.md` doesn't
  hide a gap here.
