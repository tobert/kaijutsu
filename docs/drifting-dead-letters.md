# Drifting and dead letters

A plan for making kaijutsu's message queues durable, observable, and one thing
instead of three. Written 2026-08-16 from a conversation with Amy; the code
citations are from that day and should be re-checked before trusting a line
number.

**Status (2026-08-17):** slice 1 shipped (`e30a8deb`) and verified against
HEAD. Slice 3's mechanism shipped in `drift.rs`/`kernel_db.rs` — durable
staging/dead-letter blocks, rehydrate, `ensure_drift_queue_context` — with a
real-restart test suite (`drift::tests::persistence::*`); it is **not yet
wired into a running kernel** (no production code calls
`attach_persistence`/`rehydrate_from_block_log` yet — see `docs/issues.md`
for the exact three-call wiring gap and why it wasn't closed this session).
Slice 3's own residual gap — `drain_dead_letter()` acking before the
lost+found write — **shipped fixed, same day**: see slice 3's status note and
the new "the drain ack path" subsection below.

Slice 2 **shipped, same day**, once a lane with write access to both
`drift.rs` and `kj/drift.rs` picked it up (the morning's lane correctly
stopped short of it for lacking the second file). `StagedDrift.origin:
DriftOrigin` now admits a `Peer` origin alongside `Context`; see slice 2's
status note below for exactly how far delivery goes and where the honest
boundary sits.

## The finding that started it

The Claude Code inbox needed somewhere to put an inbound message. Looking for
the existing queue turned up three mechanisms that are not the same kind of
thing at all:

| | what it is | survives a restart |
|---|---|---|
| `ConversationMailbox` (`llm/mailbox.rs:36-42`) | a **cursor** — `HydrationState` plus `seen: HashSet<BlockId>` over the block log | yes, by construction |
| `DriftRouter.staging` (`drift.rs:161`) | a **queue** — `Vec<StagedDrift>` | **no** |
| `DriftRouter.dead_letter` (`drift.rs:165`) | a **queue** — `Vec<StagedDrift>` | **no** |
| the cc inbox (`cc_inbox.rs`) | a **transport** — socket plus drain task | nothing queued yet |

The mailbox stores nothing. **The block log is the queue and the mailbox is a
cursor over it** — which is why it survives restarts without trying to. That is
the shape the rest of this plan copies.

## Two bugs, both durability

**Staged drift does not survive a restart.** `staging` is an in-memory `Vec`
and the only drift table is `drift_edges` for provenance. A staged item is
content the kernel *accepted and promised to deliver*, so losing it is worse
than losing a dead letter.

**Dead letters do not survive a restart** — same shape, and the mechanism whose
stated guarantee is "content is never silently discarded" (`drift.rs:162-165`)
silently discards it. `dead_letters()` (`:626`) is deliberately non-consuming
so a client can inspect before replaying, which makes the loss sharper: the API
invites you to come back later, and a restart is exactly when you would.

Filed separately in `docs/issues.md`; this plan is the fix.

## Three things that are one thing

`DriftRouter` already is a postmaster that knows exactly one envelope shape.
It holds a context registry, label→`ContextId` lookup with prefix matching, a
staging queue, retry counting, dead-lettering, lost+found, and replay. Only
three fields of `StagedDrift` are drift-specific: `origin` (`source_ctx:
ContextId` at the time this was written — widened to `DriftOrigin` by slice
2, see its status note below), `source_model`, `drift_kind`.

The old `source_ctx: ContextId` was the field a Claude Code session could not
supply, and it was the whole reason inbound peer messages looked like they
needed a second router. **They do not.** A second router would need context
registration and dead-lettering of its own, and the two would drift apart the
day someone fixed one — the failure mode this codebase keeps re-learning.

The name stays. Amy, 2026-08-16: *"I still like the name since it's handling
liminal stuff."*

## Lost+found is identified by a magic string, and that blocks rotation

`adopt_lost_found` (`drift.rs:438`) documents the current scheme: cold start
rebuilds the router from `KernelDb` rows, `lost_found_id` resets to `None`, and
the caller re-claims it by finding the row whose `label == "lost+found"`. That
label is reserved, so `register` conflicts on a second one.

**Rotation is therefore impossible today.** A replacement would need the label
the incumbent holds. The same comment names the hazard if re-claiming is ever
missed: the next dead letter mints a duplicate and orphans the persisted one.

Amy wants rotation — *"if lost+found gets long and I want to rotate rather than
clean up"* — so identity has to stop being the label.

## The slices

Each slice stands alone and leaves the tree green. Do not combine 1 and 3.

### 1. A well-known context registry

A `role → context_id` mapping in `KernelDb`. Roles are a fixed enum, not
arbitrary keys: lost+found, the drift queue, ROOT/director, and whatever later
earns a slot.

**This is not the kernel KV returning.** That was built and demolished on
2026-07-04 with the ruling that kaish's VFS is the shared-state path. A fixed
enum of roles with a foreign key to `contexts` is a schema — no arbitrary keys,
no arbitrary values, one question answered. If it should be browsable, the VFS
projects it rather than owning it.

Rotation becomes an `UPDATE`: the incumbent keeps its history as an ordinary
archived context, the successor takes the role, and nothing has to be renamed
or deleted.

Exit: `lost_found_id` is read from the registry rather than recovered by label
match; the reserved-label special case and the duplicate-minting hazard are
both gone; a rotation test proves the old context keeps its blocks and the new
one receives the next dead letter.

### 2. Widen the origin

`StagedDrift.source_ctx: ContextId` becomes an origin that admits a sender
which is not a kaijutsu context — a context, or a peer with a kind, a display
name, and a reply address.

This is a widening, not a generalization: one field changes so a second caller
fits, and no new abstraction is introduced. Resist extracting a `Postmaster`
trait here. One caller does not reveal an abstraction's shape, and the second
real source is what should drive it.

Exit: drift still routes exactly as before, and an inbound peer message can be
staged without inventing a source context for it.

**Shipped 2026-08-17** — the exit criterion above, exactly, and no further:
`StagedDrift.source_ctx: ContextId` became `origin: DriftOrigin`, an enum of
`Context(ContextId)` (everything that existed before this slice) or
`Peer(PeerOrigin { kind, display_name, reply_address })` — the shape the
exit criterion names, added as one field's type widening rather than a
trait/registry/plugin point, per Amy's explicit instruction to resist that.
`DriftRouter::stage` takes `impl Into<DriftOrigin>` with a blanket
`From<ContextId>`, so every existing call site (`kj/drift.rs`, every test)
kept compiling with a bare `ContextId` — "drift still routes exactly as
before" needed no changes to prove, only new coverage
(`drift::tests::test_stage_peer_origin_without_a_context` and siblings).

**Where the honest boundary actually is**, per the morning lane's own
finding: `kj/drift.rs`'s `drift_flush` reads the origin in the delivery loop
and the lost+found-write loop. Both branch on `DriftOrigin::as_context()`.
A `Context` origin flows through exactly as before. A `Peer` origin cannot
flow through `insert_drift_block_as` (`block_store.rs`, outside this
slice's territory) at all — that function requires a real `ContextId` for
provenance, with no `None`/peer-shaped alternative, and fabricating one
would be exactly the identity-smear mistake `hook_listener.rs` already had
to walk back once (stamping `PrincipalId::system()` in place of real
authorship). So a peer-origin item is **stageable and durable today, but
not yet deliverable**: `drift_flush` treats it as an ordinary delivery
failure (requeues it; it eventually dead-letters, and stays durably queued
rather than lost either way — the same crash-safety Task B gives every
other item). Turning a peer-origin item into an actual delivered block
needs a `block_store.rs` (and likely `kaijutsu-types::BlockSnapshot::drift`)
change, which is the piece that pairs with slice 4 — "the cc inbox melts
into the drift queue" already owns target/delivery resolution for a peer
origin, and is the natural place to design what a peer-authored block's
provenance field actually looks like.

`ContextEdgeRow.source_id`/`target_id` carry a hard SQL foreign key to
`contexts(context_id)` (`kernel_db.rs`) — confirmed while implementing this,
not assumed — so a peer origin categorically cannot get an edge row without
a schema change. Since a peer-origin item never reaches `drift_flush`'s
success branch (the block write it would need always fails first, for the
reason above), no edge is ever attempted for one; this needed no special
case of its own; "an edge from a non-context may simply not be an edge"
held without extra code.

### 3. The queue becomes blocks in a well-known context

Staged and dead-lettered items stop being `Vec`s and become blocks in the
drift-queue context from slice 1.

This is the slice that pays twice. Both durability bugs are fixed by
construction rather than by adding two tables, and observability stops being a
feature to build: the app renders the context, `kj block` queries it, the time
well shows its activity, and replay reads a block before deciding.

The context has no agent attached. A context nobody attaches to is already just
a context — hydration happens on attach — so this needs no new concept. The
drift router is the process that tends it. Amy: *"almost a static agent: the
drift router."*

Exit: a kernel restart mid-flight loses neither staged nor dead-lettered
content, proven by a test that restarts against a real database.

**Mechanism shipped 2026-08-17**, exit criterion met at the `DriftRouter`
level: `attach_persistence`, `rehydrate_from_block_log`,
`ensure_drift_queue_context` in `drift.rs`, `WellKnownRole::DriftQueue` in
`kernel_db.rs`, restart-against-a-real-database tests in
`drift::tests::persistence`. **Not yet wired into a running kernel** — see
`docs/issues.md` for the three-call wiring gap (belongs in
`kaijutsu-server/src/rpc.rs`, next to the existing lost+found re-adoption
code, outside this session's territory). Answers the "one context or two?"
open question below: **one** — a single new `drift-queue` well-known context
(distinct from lost+found) holds both staged and dead-lettered records,
distinguished by a `QueueSlot` tag on each block's content rather than by
which context they live in. Lost+found is unchanged: it stays the
human-facing destination `kj drift flush` writes formatted, delivered dead
letters into. The drift-queue context is closer to internal bookkeeping (its
blocks are `BlockKind::Trace`, model-hidden) than to something a human reads
directly — "what failed" is still answerable via `dead_letters()`/`kj drift`
without needing a second context to query against.

**The drain ack path — shipped fixed 2026-08-17.** `drain_dead_letter()` used
to mark every checked-out item's durable record consumed immediately, before
the caller's write into lost+found ever ran — a crash in that narrow
synchronous window could still lose the item, the exact failure class this
whole slice exists to close (filed in `docs/issues.md`, "Drift drain acks
before the lost+found write..."). It is now a proper two-phase drain, the
same shape [`DriftRouter::drain`]/[`complete`]/[`requeue`] already use for
staging: `drain_dead_letter` checks an item out (flags it
[`in_flight`](StagedDrift::in_flight), leaves it physically in
`dead_letter`, durable record untouched — still `Pending`) rather than
removing it; `ack_dead_letter(id)` is the new second phase, called only
after the caller's lost+found write actually succeeds, and only then marks
the durable record consumed; `restore_dead_letters` (the failure path)
just clears the checked-out flag now, since the record was never touched in
the first place. `kj/drift.rs`'s `drift_flush` calls `ack_dead_letter`
right after its `insert_drift_block_as` write returns `Ok`. The test named
for exactly this crash window is
`drift::tests::persistence::drain_dead_letter_then_crash_before_ack_recovers_on_restart`:
drain, forget everything (no ack, no restore — simulating a kill in the old
unsafe window), reopen the same on-disk file, rehydrate, assert the item is
still a dead letter. A related latent bug found while building this:
`unregister`'s sweep of staging into dead-letter could carry a
staging-domain `in_flight` flag into the dead-letter domain, permanently
hiding the item from `drain_dead_letter` — fixed by clearing both flags on
the move (`drift::tests::test_unregister_dead_letter_is_not_already_in_flight`).

The Claude Code inbox stops being a delivery path and becomes a **source**. Its
transport stays where it belongs — socket, frame codec, descriptor registry,
liveness — in `claude-code-peer`. What it produces is an origin-tagged item
staged like any other.

Target resolution is a per-source concern: Claude Code's is a join through its
own on-disk registry, `from_socket` → `SessionDescriptor.messaging_socket_path`
→ `session_id` → the `cc-<repo>-<sid8>` context the hooks registered. That
join reads Claude Code's own record rather than trusting attribution asserted
in the message, so it needs no new trust decision. `proc_start` already guards
a recycled PID inheriting a stale socket path.

An item whose target cannot be resolved dead-letters like anything else, which
is the whole reason to route it through here.

### 5. Simplify the `kj` surface

Once the queue is blocks, several verbs are answering questions the block tools
already answer. Amy: *"maybe the kj surface can simplify since regular block
tools cover more."* Re-read `kj drift` at that point and delete what has become
a second way to look at a context. Do not pre-decide which — the answer depends
on what slice 3 makes free.

Note the gap that exists today and should not be ported forward: **there is no
way to list dead letters.** `kj drift queue` (`kj/drift.rs:875`) shows staging
only, and the only path to seeing dead letters is `kj drift flush`, which
drains them. Inspection currently requires consumption.

### 6. Observability

`drift.*` spans are already 100% sampled (`telemetry/src/otel.rs:244`), so the
success path is well traced. There are **no metrics at all** — no counters,
gauges, or histograms anywhere in `drift.rs`. Spans describe one operation;
they cannot answer how often delivery fails, how deep staging is now, or how
many retries precede a dead letter.

Add counters for staged, delivered, retried, and dead-lettered, split by origin
once slice 2 lands. `dead_lettered{origin="peer:claude-code"}` versus
`origin="context"` is the split that says whether a bridge is misbehaving or a
context is, and it is the evidence an auto-approve gate would need later.

Amy wants the queue watchable in the app *"like a context... when we have a lot
going on"* — slice 3 delivers that without app work, because it is a context.

## Open questions

- Rotation policy: manual (`kj`) only, or a size/age threshold that rotates on
  its own? Automatic rotation needs its cutoff to come from the context's own
  economics rather than a global constant — the same caution the 3-hour archive
  rule carries.
- ~~Whether staged items and dead letters live in one well-known context or
  two.~~ **Resolved 2026-08-17: one** (the `drift-queue` role, `QueueSlot`
  tag per record) — see slice 3's status note above.
- Whether a triage agent eventually attaches to lost+found. It costs nothing
  structural once the queue is blocks — it reads blocks and calls `kj drift` —
  so this is a product decision, not an architectural one.
