# The context change feed

Status: **accepted design, not yet built** (Amy, 2026-08-15 — *"yes change feed,
it's been creeping around my thoughts and is the right move"*).

Authority: `docs/crdt-position-2026-08.md` (one authoritative kernel sequencer,
no client dependency on DTE). This document specifies the wire surface that
replaces the CRDT projection.

## Why one feed instead of many events

Today a client learns about a context through thirteen separate `BlockEvents`
methods, and text arrives as **serialized diamond-types operations**
(`onBlockTextOps @5`, `onBlockTextOpsBatch @17`). Clients therefore link the CRDT
to understand what happened. That is the thing the migration exists to end.

A first design proposed two replacement events, `onBlockTextAppended` and
`onBlockTextReplaced`. Review found it sound in shape and wrong in placement, and
found that a single feed gets three things the pair cannot:

1. **Coalescing becomes native.** `onBlockTextOpsBatch @17` exists only because
   per-event methods cannot express "here are fifty changes". A list can.
2. **Transactional delivery.** A tool's final output text and its `Done` status
   arrive in one message. A client can no longer render a completed tool with
   missing output — a race the current surface allows.
3. **One clock instead of two.** `seqNum` (per-context op counter) and `subSeq`
   (per-subscription delivery counter) collapse into a single monotonic
   `version` the client compares against.

## Shape

```capnp
interface ContextObserver {
  # One delivery carries an ordered batch of changes and the version they
  # bring the client to. Applying the batch in order makes the client's
  # state exactly the context at `version`.
  onContextChanged @0 (contextId :Data, events :List(ContextEvent),
                       version :UInt64);
  onTerminated @1 (reason :SubscriptionEndReason, deliveredVersion :UInt64);
}

struct ContextEvent {
  union {
    blockInserted     @0 :BlockInsert;   # snapshot + afterId
    blockDeleted      @1 :BlockId;
    blockMoved        @2 :BlockMove;
    textAppended      @3 :TextAppend;    # blockId + suffix only
    textReplaced      @4 :TextReplace;   # blockId + full content
    statusChanged     @5 :BlockStatusChange;
    collapsedChanged  @6 :BlockFlagChange;
    excludedChanged   @7 :BlockFlagChange;
    metadataChanged   @8 :BlockMetadataChange;
    outputChanged     @9 :BlockOutputChange;
  }
}
```

`getBlocks` gains a version so a snapshot can be joined to the feed:

```capnp
getBlocks @35 (contextId :Data, query :BlockQuery, trace :TraceContext)
  -> (blocks :List(BlockSnapshot), version :UInt64);
```

## Classification happens at the mutation, not at the wire

This is the correction that matters most, and the first proposal got it wrong.

`BlockFlow::TextOps` carries **opaque CRDT bytes**. A bridge holding those bytes
cannot decide whether a change was an append without linking the CRDT — the exact
dependency being removed. Worse, defining "the text we last sent" *per
subscription* would force the server to keep one `String` per block per
subscriber.

**The kernel classifies inside the mutation lock**, where both the old and new
text are already in hand, and publishes already-classified variants. The bridge
forwards; it never decides.

## Normative rules

Rules use Simplified-Technical-English style. One rule is one sentence. Each term
has one meaning.

### Terms

- **before text** — the block text immediately before the mutation.
- **after text** — the block text immediately after the mutation.
- **append** — a mutation where the after text starts with the before text.
- **suffix** — the characters of the after text that follow the before text.
- **version** — the context's monotonic mutation counter.

### Server

1. The kernel MUST classify a text mutation while the kernel holds the block mutation lock.
2. The kernel MUST classify the mutation as an append if the after text starts with the before text.
3. The kernel MUST classify every other text mutation as a replace.
4. The kernel MUST NOT classify a mutation by the name of the function that made the mutation.
5. The kernel MUST NOT classify a mutation by the name of the tool that made the mutation.
6. The kernel MUST publish `textAppended` with the suffix for an append.
7. The kernel MUST publish `textReplaced` with the after text for a replace.
8. The bridge MUST forward the published classification unchanged.
9. The bridge MUST NOT inspect operation bytes.
10. The server MUST increment the version once for each mutation.
11. The server MUST send events in version order within one delivery.

**Rationale for 4 and 5.** `edit_text_as` makes appends. A merge makes both
kinds. A list of tool names is not a safe rule: an earlier draft listed one tool
and missed four. Content comparison cannot miss a producer.

**The census that proves it** (verified 2026-08-15; keep it as evidence, never as
the rule). Non-append producers reaching *conversation* blocks:

| producer | site |
|---|---|
| MCP `block_edit` | `kernel/src/mcp/servers/block.rs` (~`:877-969`) |
| MCP `block_splice` | `kernel/src/mcp/servers/block.rs:414-421` — a *separate* tool |
| `kj block edit` | `kernel/src/kj/block.rs:858-963` |
| kaish VFS write to `/docs/<ctx>/<block>` | `kernel/src/runtime/kaish_backend.rs:348-382` |
| ~~`pushOps` / `merge_ops`~~ | **deleted 2026-08-15** (`7df29be8`) |

And a producer that *looks* like a splice and is an append: MCP `block_append`
writes through `edit_text_as` at `char_offset = content.chars().count()`
(`block.rs:362`). A provenance rule would misclassify it in the other direction.

The LLM streaming path is 100% `append_text_as` (`server/src/llm_stream.rs:1626,
1696`); the `edit_text_as` calls at `llm_stream.rs:1079,2191` are degenerate
(`pos=0, delete=0`) writes into a just-created empty block.

### Batching

12. The server MAY place many events for many blocks in one delivery.
13. The server MUST preserve kernel publish order across all blocks in a delivery.
14. The server MUST NOT reorder an event of one block ahead of an earlier event of another block.
15. The server MUST emit pending appends for a block before the server emits a replace for that block.

**Rationale for 13 and 14.** The FlowBus guarantees order across topics. Holding
one block's text while forwarding another block's status change would break that
guarantee at the client. A delivery is a window, never a reordering.

### Client

16. The client MUST apply the events of one delivery in order.
17. The client MUST append the suffix to the local text for `textAppended`.
18. The client MUST replace the local text with the received content for `textReplaced`.
19. The client MUST NOT compare character counts to detect a change.
20. The client MUST store the delivered version after the client applies the delivery.

**Rationale for 19.** A replace can keep the character count. A count comparison
reports no change. ACP made this error and rendered corrupted text.

### Recovery

21. The client MUST subscribe before the client fetches a snapshot.
22. The client MUST buffer delivered events until the snapshot is applied.
23. The client MUST fetch the snapshot with `getBlocks`.
24. The client MUST read the version returned by `getBlocks`.
25. The client MUST discard a buffered delivery whose version is not greater than the snapshot version.
26. The client MUST apply the remaining buffered deliveries in version order.
27. The client MUST NOT fetch a snapshot with `getContextSync`.
28. The client MUST re-subscribe and fetch again after `onTerminated`.

**Rationale for 21 and 22.** An event arriving between the fetch and the
subscribe is lost. An event applied before the snapshot corrupts the text.
Subscribing first prevents loss; buffering prevents corruption; the version
resolves the overlap. **Rationale for 24.** Without a snapshot version the client
cannot tell whether a buffered append is already included, and applying it twice
corrupts the text.

## The feed is read-only, and that is the whole client contract

A client **follows** a context here and **mutates** it by asking the kernel to
run something. There is no client-facing RPC for editing block text: `pushOps`
was the only one, and deleting it left none. Text reaches a block through the
LLM stream, an MCP block tool, `kj block append`/`edit`, or a kaish write — all
kernel-side, all behind the same capability checks.

That is a decision, not an omission (Amy, 2026-08-15: *"clients should rely on
stuff in kaish anyways most of the time"*). A parallel authoring RPC would be a
second mutation path to keep in step with the first, and the first is the one
every tool and script already uses. `authorBlock` stays, because authoring a
whole block is a submission rather than an edit.

Practical consequence for anyone writing a client: if you need to change text,
call `executeKj` or a tool. If you need to *see* text change, subscribe here.

## Timing artifacts do NOT ride this feed

`RenderCue` and `BeatSync` stay on their own delivery path.

`docs/midi.md` "The one timebase" is doctrine: wire timing artifacts carry
emission wallclock stamps, sinks back-date, stale timing data is rejected on a
ladder, and missed beats are never replayed. A batching window exists to add
latency in exchange for fewer messages. **Applying that trade to a timing
artifact is exactly the mistake the timebase doctrine forbids.**

29. The server MUST NOT place a timing artifact in a batched delivery.
30. The server MUST NOT delay a timing artifact for batching.

## Deleted by this change

Flag day. Every client is in-repo and rebuilt together.

**`BlockEvents` survives the flag day, reduced.** The table below deletes its
block half, not the interface. Three members have no replacement and must stay:
`onRenderCue` and `onBeatSync` (musical-timebase directives, excluded from the
feed by the section above), `exchange` (the MIDI request/reply the kernel calls
back on), and `onSubscriptionTerminated`, which is still their termination
signal. `subscribeBlocks`/`subscribeBlocksFiltered` therefore stay too.

A client that draws blocks *and* plays sound ends up holding both: a
`ContextObserver` feed per followed context, and a `BlockEvents` subscription
for directives. That is the intended end state, not a transitional wart — one is
a change log where batching is a feature, the other is a directive channel where
batching is forbidden.

| Deleted | Reason |
|---|---|
| `onBlockTextOps @5`, `onBlockTextOpsBatch @17` | replaced by `textAppended` / `textReplaced` |
| ~~`onInputTextOps`~~ | **NOT this flag day** — see below |
| the other per-event `BlockEvents` methods | folded into `ContextEvent` (except the four named above) |
| `getContextSync @36` | returns raw oplog bytes; `getBlocks` + version replaces it |
| `getContextState @34` | already a tombstone, redundant with `getBlocks` |
| `pushOps @37`, `pushInputOps` | clients do not author operations |
| `ops` on `onBlockInserted` | dead weight; the snapshot is what renders |
| `declareEventCapabilities @104` | a flag day needs no negotiation |
| `onSyncReset` | oplog compaction is server maintenance; a client holding materialized state is not affected by it |

**`onInputTextOps` stays, and this is a correction to an earlier draft of this
table.** The compose input is a separate document on a separate API surface
(`editInput`, `getInputState`, `onInputTextOps`, `onInputCleared`) and it is
still CRDT-backed — open question 2 below, deliberately out of scope here.
Deleting its event because it looks like a sibling of `onBlockTextOps` would
break the app's compose box for no gain: nothing in this change replaces it.
It goes when the input document is migrated, which is its own decision and
probably its own shape (a block with a draft status would delete six
endpoints).

## Migration order (Amy, 2026-08-15)

> *"do the kernel, get it and its tests refined, then have subagents chew on the
> clients later. maybe keep kaijutsu-client synced and tested but mcp, app, and
> acp can break while we migrate."*

1. **Kernel + schema + server + `kaijutsu-client`.** These move together and stay
   green. `kaijutsu-client` is the wire's tested consumer — it is how we know the
   design works before any UI depends on it.
2. **`kaijutsu-mcp`, `kaijutsu-app`, `kaijutsu-acp` may break** during step 1.
   They are migrated afterwards, by subagents, one at a time.
3. Breaking them is permitted; leaving them broken is not. The series ends when
   all three are green, and the handoff must say plainly that moltar needs a
   rebuild.

**Known client migration costs — not hand-waved:**

- **ACP relies on `SyncedDocument` for document order**, not only for text. Its
  task-plan rendering needs blocks in DAG order, which the CRDT maintained for
  free. ACP must maintain that ordering itself from `blockInserted` /
  `blockMoved` / `blockDeleted`.

  **Corrected while implementing (2026-08-15).** The sketch above originally
  had `blockInserted` carry a bare `BlockSnapshot`, which cannot support that:
  the wire `BlockSnapshot` has **no ordering key** — no `orderKey`, no `tick`
  that ordinarily applies — so a client receiving one learns that a block
  exists and not where it goes. `getBlocks` hides this because the kernel
  returns blocks already ordered. Hence `BlockInsert { block, afterId,
  hasAfterId }`, mirroring `BlockMove` and the old `onBlockInserted`. The
  alternative — putting `orderKey` on `BlockSnapshot` so any snapshot sorts
  itself — is the tidier answer and a wider change; it stays open.
- **The Bevy app cannot ingest plain text into its CRDT mirror.** `DocumentCache`
  wraps a `SyncedDocument`; appending a raw `String` to a CRDT is not a supported
  operation. That cache is rewritten, not toggled.

## Open questions

1. **`ContextSwitched`** is published on the block flow but is a shell concern,
   not a block change. Decide whether it joins `ContextEvent` or gets its own
   method.
2. **The input document is a parallel API surface** — `editInput`,
   `getInputState`, `onInputTextOps`, `onInputCleared`. It may simply be a block
   with a draft status, which would delete six endpoints. Out of scope here;
   worth deciding before Lane C builds on the current shape.
3. **`BlockId` is a Lamport timestamp** (`{contextId, principalId, seq}`) —
   multi-writer CRDT identity that a single sequencer does not need. A
   kernel-assigned UUIDv7 would do. Large blast radius; deliberately deferred.
4. **`retired79 @79 ()` … `retired83 @83 ()`** are placeholder stubs from the KV
   deletion. A flag day is when they could go, if we accept renumbering.
