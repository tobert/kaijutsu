# The context change feed

A client follows a context through one feed: `ContextObserver`. The kernel
classifies each mutation and ships an ordered batch; `kaijutsu-client` carries
the shared applier (`ContextMirror`), and ACP and the Bevy app consume it.

## Why one feed instead of many events

One feed gets three things a set of per-change events cannot:

1. **Coalescing is native.** A list expresses "here are fifty changes"; a
   per-event method cannot.
2. **Complete mutations.** All events of one accepted mutation arrive in one
   message. Separate mutations, such as appending final output and setting its
   `Done` status, can arrive in separate messages, in acceptance order.
3. **One clock instead of two.** A single monotonic `version` replaces a
   per-context op counter plus a per-subscription delivery counter.

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

**The kernel classifies inside the mutation lock**, where both the before text
and the after text are in hand, and publishes already-classified variants. The
bridge forwards; it never decides.

Classifying at the wire is impossible: by then the change is opaque bytes, and
defining "the text we last sent" per subscription would force the server to keep
one `String` per block per subscriber.

## Acceptance and storage failure

`BlockStore::accept` holds the document guard while preparing a mutation,
committing its durable operation, updating the live-status cache, and
publishing its projected events as one FlowBus group. Metadata comes from that
same guarded state. Each subscription marks its last matching event in the group;
other publishers and new subscriptions cannot interleave with publication.
Compaction snapshots exactly the committed prefix while the guard is held.
Journal counters advance after the database commit. Fork creation persists
its document row and initial snapshot together before exposing the document.

A persistence failure makes the document unusable until restart. The failed
write returns an error and publishes no events; subsequent reads and writes
panic rather than serve uncommitted memory. Restart recovers the durable
prefix. Compaction can fail after the operation committed, so recovery may
include that operation even when its caller received an error.

Lock order is document, then database. A caller must release its database
guard before entering block-store reads or writes.

The bridge finishes a started group before closing its four-millisecond window
or its 512-event target. Overflow discards any incomplete group and terminates
the feed for recovery. Timing events use their own lane and cannot complete an
ordered group. The bridge preserves publish order without sorting; a version
inversion ends the feed. A client accepts equal versions within a delivery,
above the previous delivery's version, and rejects repeats across deliveries.
Snapshot overlap skips all events of a mutation the snapshot already contains.

A refused callback or a callback unanswered for 30 seconds ends the feed. The
server sends a best-effort termination notice with the last acknowledged
version, waits at most one second, then disconnects. The client resubscribes
and fetches a snapshot; retrying an uncertain append could apply it twice.

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

**There is no client-facing RPC for editing block text.** A client follows a
context here and mutates it by asking the kernel to run something. Text reaches
a block through the LLM stream, an MCP block tool, `kj block append`/`edit`, or
a kaish write — all kernel-side, all behind the same capability checks.

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

## Two subscriptions, on purpose

`BlockEvents` carries four members the feed does not replace: `onRenderCue` and
`onBeatSync` (musical-timebase directives, excluded from the feed by the section
above), `exchange` (the MIDI request/reply the kernel calls back on), and
`onSubscriptionTerminated`, their termination signal.
`subscribeBlocks`/`subscribeBlocksFiltered` stay for them.

A client that draws blocks *and* plays sound holds both: a `ContextObserver`
feed per followed context, and a `BlockEvents` subscription for directives. That
is the intended end state, not a transitional wart — one is a change log where
batching is a feature, the other is a directive channel where batching is
forbidden.

The compose input keeps its interaction-rate RPC surface, `editInput` and
`getInputState`. Its text lives in one `Draft` block per context and principal
and rides the ordinary change feed. Chat submission promotes that block. Shell
submission authors a command, then consumes the captured draft revision under
the document guard. Newer typing survives, including edits back to the same
text; a refusal leaves the draft in place. Revision tokens are process-local
and are renewed on reload, so they need no storage or wire migration.

## Open questions

1. **`ContextSwitched`** is published on the block flow but is a shell concern,
   not a block change. Decide whether it joins `ContextEvent` or gets its own
   method.
2. **`BlockId` is a Lamport timestamp** (`{contextId, principalId, seq}`) —
   multi-writer identity that a single sequencer does not need. A
   kernel-assigned UUIDv7 would do. Large blast radius; deliberately deferred.
3. **`retired79 @79 ()` … `retired83 @83 ()`** are placeholder stubs from the KV
   deletion. A flag day is when they could go, if we accept renumbering.
