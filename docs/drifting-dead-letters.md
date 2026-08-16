# Drifting and dead letters

A plan for making kaijutsu's message queues durable, observable, and one thing
instead of three. Written 2026-08-16 from a conversation with Amy; the code
citations are from that day and should be re-checked before trusting a line
number.

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
three fields of `StagedDrift` are drift-specific: `source_ctx`, `source_model`,
`drift_kind` (`drift.rs:100-110`).

`source_ctx: ContextId` is the field a Claude Code session cannot supply, and
it is the whole reason inbound peer messages looked like they needed a second
router. **They do not.** A second router would need context registration and
dead-lettering of its own, and the two would drift apart the day someone fixed
one — the failure mode this codebase keeps re-learning.

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

### 4. The cc inbox melts into the drift queue

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
- Whether staged items and dead letters live in one well-known context or two.
  One context makes the app view simpler; two make "what failed" a query
  instead of a filter.
- Whether a triage agent eventually attaches to lost+found. It costs nothing
  structural once the queue is blocks — it reads blocks and calls `kj drift` —
  so this is a product decision, not an architectural one.
