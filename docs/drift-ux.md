# Drift UX — making cross-session drift feel like it works

Drift is how content moves between contexts. It has worked, durably, since
roughly February 2026 — about six months before Claude Code shipped
inter-session messaging. That ordering matters for how we read this document:
**cc messaging is not the thing we are catching up to.** It is a useful
contrast case that shows us where our ergonomics are rough, and nothing more.
Where the two designs disagree about substance, we are usually right and they
are usually cheaper.

This doc records the gap between what drift *does* and what drift *feels
like*, ranked, with the slices that close it. Amy's framing, which sets the
bar: this feeds the music making. Players hand each other material mid-piece.
A send that silently does nothing is a dropped note.

## What we have that cc messaging does not

State this first, because every gap below is an ergonomics gap sitting on top
of a genuinely good substrate. None of the fixes should cost us any of this.

- **Delivery is durable.** A drift lands as a first-class `BlockKind::Drift`
  block on the target's CRDT document (`block_store.rs:3020`), journalled and
  broadcast on the FlowBus like any other block. It survives a kernel restart.
  cc messages live and die with the process.
- **Content is never silently lost.** Failed delivery requeues up to
  `MAX_DRIFT_RETRIES = 5`, then moves to a kernel-global dead-letter queue, and
  is eventually written into a lazily-created `lost+found` context
  (`drift.rs:606-680`). A previous bug that *did* lose dead letters is fixed and
  commented as such. This is the CLAUDE.md silent-fallback stance made real.
- **It is many-hands by construction.** A drift block is visible to every
  player attached to that context — human, model, app, sibling. cc messaging is
  point-to-point between two processes.
- **Provenance is first-class.** `source_context` and `source_model` are fields
  on the block (`crdt/src/content.rs:222-224`), not prose in a message body.
- **It carries more than text.** `--summarize` and `pull` LLM-distill a whole
  context rather than shipping a literal string.

## What cc inter-session messaging feels like

The parts worth stealing, in the order a user notices them:

1. **One action sends.** You call the tool, the message is on its way. There is
   no second verb.
2. **It arrives on its own.** "Messages enqueue and drain at the receiver's next
   tool round" — the receiver does not have to go look.
3. **You can see who is there.** A listing shows live peers; the name in the
   listing is the address you send to.
4. **Replying is obvious.** The incoming message carries its sender's address in
   the form you reply with. You copy it back.

Those four map almost exactly onto our four worst gaps.

## The gap list

Verified against code 2026-08-12; absences confirmed two ways each, per the
lesson in `issues.md` that a failed grep and a real absence look identical.

### 1. Sending takes two verbs, and one of them is forgettable

`kj drift push` only appends a `StagedDrift` to an in-memory `Vec`
(`drift.rs:427-458`). Nothing reaches the target document until someone runs
`kj drift flush`. The success message — `staged drift #7 → foo` — reads like
delivery to anyone not steeped in the model.

This is the biggest felt difference and the cheapest to fix. It is also the
one that bites hardest in the music case: staging is invisible, so a player
who forgets `flush` has sent nothing and been told "ok".

Note the staging queue is not a mistake — it is what makes `cancel` possible
and lets a sender batch several drifts into one arrival. The mistake is that
it is the *default*, and that the verb named `push` does not push.

### 2. Nothing wakes the receiver

The block is durable in the destination's document immediately after flush, but
nothing causes that context to take a turn. It surfaces on the destination's
next *natural* turn, when `ConversationMailbox::catch_up` folds in unseen blocks
(`llm/mailbox.rs:143-179`). An idle context sits on an unread drift
indefinitely.

Confirmed absent two ways: no `BlockFlow::Inserted` subscriber is tied to
`BlockKind::Drift`, and the only rc script on the `drift` verb
(`assets/defaults/rc/lib/drift/S40-cache.kai`) does prompt-cache maintenance
only — no notify, no `kj drive`.

The Bevy app does show a 5s arrival toast (`app/src/connection/drift.rs:174`).
That is cosmetic chrome for a human watching, with no analog for a kaish or
MCP session, and it never touches a turn loop.

**This one is a design conversation, not a patch — Amy's ruling, not ours.**
Waking a context on arrival means deciding who may spend tokens on whose
behalf: a drift from a sibling becomes a remote turn trigger. The shared-trust
model permits it, so nothing *stops* us; that is exactly why it should be
chosen rather than inherited from a bug fix. Four shapes, no recommendation:

- **A. Leave it pull-only.** Drift surfaces on the next natural turn, as
  today. Zero surprise spend; an idle context stays unread indefinitely.
- **B. rc opt-in per context.** Arrival runs the target's `drift` rc scripts —
  the socket already exists and does cache work only today. A context that
  wants waking ships a script that calls `kj drive`. Per-context consent,
  no new kernel concept, composes with the existing lifecycle. Cost: the
  behaviour is invisible unless you read the rc tree.
- **C. Sender-declared.** `kj drift push --wake` asks for a turn; default
  stays quiet. Explicit at the call site, but it lets a sender spend a
  receiver's tokens by flag — the wrong direction for consent.
- **D. Kernel-level notify without a turn.** Arrival marks the context
  unread and surfaces it in `kj context list` / the app, but never drives.
  Closes the "nobody knows it's there" half without touching spend.

B and D compose, and together they are probably the closest thing to what cc
messaging *feels* like without adopting its spend model. That is an
observation, not a recommendation.

### 3. `push` and `pull` speak different address grammars

Two resolvers, same noun:

- `push` → `DriftRouter::resolve_context`, the in-memory registry: exact label,
  unique label-prefix, unique short-id (`drift.rs:330-338`).
- `pull` / `merge` / `history` → `refs::resolve_context_arg` → `KernelDb`, which
  additionally parses full UUIDs **and** the `.` / `.parent.parent` chain
  (`kj/refs.rs:23-40`).

So `kj drift merge .parent` works and `kj drift push .parent` does not. There is
no reason for this beyond the two paths having been written at different times.

### 4. A received drift is a dead end for reply

Hydration renders the block as `[push from context a1b2c3d4]`
(`llm/hydrate.rs:344-357`) — the **short id only**. If the sending context has a
human-friendly label, the receiver never sees it. There is no thread id, and
nothing in the block tells the receiving model that replying is possible or how.

The short id *is* a valid `push` target, so a model that already knows the drift
verbs can reply. A model that doesn't, can't — it has to infer the mechanism
from its system prompt rather than from the message in front of it. Compare cc,
where the incoming message carries the exact string you reply to.

### 5. No presence, and cc-* contexts never die

Nothing tracks which contexts are live or who is attached. Confirmed two ways:
`presence` in this codebase is MIDI-only (`midi_presence.rs`), and
`ContextState { Live, Staging, Concluded, Archived }`
(`kaijutsu-types/src/enums.rs:126-137`) is a data-lifecycle flag whose `Live`
means "LLM calls enabled", not "someone is here".

Compounding it: the Claude Code hook pipeline registers `cc-*` contexts on
session start and **never deregisters**. `session.end` and `agent.stop` only
insert text blocks (`hook_listener.rs:315-376`). The original commit
(`0c06f51c`, 2026-07-17) says so outright — "context pileup is deliberately
unmanaged for observation". That was the right call for a new pipeline; it has
now been observed for four weeks, and the cost is that `kj context list` is as
much an ossuary as a roster. `last_activity_at` is the only liveness-adjacent
signal, and `register_session` explicitly pushes the staleness check onto the
caller (`lib.rs:1504-1518`) rather than the kernel owning it.

Presence is the gap that most changes what drift *feels* like — you cannot hand
someone material if you cannot tell whether anyone is holding the other end —
but it is also the largest, and it wants the `ContextRegistry` extraction
already filed in `issues.md` to land first.

**This one is not merely cosmetic, and here is the receipt.** Measured against
the live zorak kernel on 2026-08-12:

```
kj context list          289 contexts
  ... of which cc-*      152
  ... of which cc-kaijutsu  60
```

Drift addressing resolves a label *prefix*, and returns `Ambiguous` the moment
more than one label matches (`ids.rs:311-320`). Every cc context for this repo
is `cc-kaijutsu-<suffix>`. So today, on the machine we actually work on:

```
kj drift push cc-kaijutsu "..."   →  ambiguous context prefix 'cc-kaijutsu':
                                     matches [60 candidates]
```

**Scope this precisely — it is prefix addressing that breaks, not all of it.**
Resolution tries exact label first and returns before any ambiguity check
(`ids.rs:283-296`), and falls through to the 8-hex-char short id at step 3. So:

| Form | Today |
|---|---|
| `push cc-kaijutsu-3fc34b49` (exact label) | works |
| `push 05b98e89` (short id) | works |
| `push cc-kaijutsu` (label prefix) | **ambiguous, 60 candidates** |

The loss is real — abbreviating a label is the entire reason to have one, and
it is the form a human or model reaches for first — but a session is still
addressable if you spell it out. Read the gap as "the ergonomic address form
decays to unusable, monotonically", not "cc contexts cannot be reached".

Dead contexts are not inert: they consume the live namespace. That converts
gap #5 from "the roster is untidy" into "the usable address form degrades
without bound", which is a correctness-shaped problem wearing an ergonomics
costume.

Two consequences worth stating plainly:

1. Deregistration is not cleanup-for-tidiness; it is what keeps addressing
   usable. It should move up the slice order accordingly.
2. Any machine whose `kaijutsu-mcp` predates the stable-label fix
   (`554a84d4`) mints a *fresh* orphan per MCP relaunch rather than
   reattaching, so it adds to this count faster. The 60 above were measured
   on zorak only — I have not measured moltar and am not claiming its
   number.

## Slices

Ordered by value over cost. Slice 1 shipped today; the 60-candidate
measurement above re-ranked what follows it — deregistration (was slice 4)
now outranks the addressing and reply polish, because those two make a
namespace nicer to use while deregistration is what keeps it usable at all.

**Slice 1 — make `push` push. SHIPPED 2026-08-12.** `push` delivers on the
spot; staging moved behind `--stage`. `flush` is unchanged for staged items and
the dead-letter drain.

The rationale is not just ergonomics: a `push` that reported `staged drift #1 →
dst` and delivered nothing is squarely in the **silent-fallback defect class**
CLAUDE.md rejects — the caller was told the operation succeeded, and the thing
they asked for had not happened. Naming it that way is what moved it to the
front of the queue.

A failed immediate delivery does *not* silently fall back to staging either: it
stages the content (so it is never lost) and returns `KjResult::Err` saying so,
naming the staged id and the retry command. Loud fallback, not silent.

Tests pinning it: `drift_push_delivers_immediately`,
`drift_push_stage_defers_delivery`, `drift_push_delivery_failure_stages_loudly`
(`kj/drift.rs`). The delivery body was extracted into `deliver_drift`, which is
a first bite at the "orchestration bloat" entry in `issues.md` — `pull`,
`merge` and `flush` still inline their own copies and can migrate when next
touched.

**Slice 2 — stop the bleeding on the namespace.** Deregister (conclude) a
`cc-*` context on `session.end`. This is the small half of gap #5 and does not
need the `ContextRegistry` extraction: the hook already fires and currently
only writes a text block. Pair it with a one-shot sweep for the 152 already
resident. Concluding is reversible-ish and non-destructive — archived contexts
keep their blocks; they just stop competing for the name.

**Slice 3 — one address grammar. SHIPPED 2026-08-12 (`46878b28`).** `push`
resolves through `refs::resolve_context_arg` like everything else, so `.`,
`.parent` chains and full UUIDs work on it; the router stays as a fallback.
Structural refs are also the one address form immune to the label collisions
in gap #5 — a fork can reach its parent without naming anything.

**Slice 4 — make reply obvious. NOT cheap; needs a decision.** The intent
stands: a receiver should see `[push from lead-kaijutsu (a1b2c3d4)]` rather
than a bare short id. Costing it revealed the estimate in the first draft of
this doc was wrong, and *why* it is wrong is the useful part.

Two implementations, and they are not equivalent:

- **Stamp the label on the block at delivery.** Mechanically parallel to
  `source_model`, which means ~65 references across 14 files plus the wire
  schema and the app renderers — not a papercut fix. Worse, it stamps a
  **mutable** value: `stabilize_context_label` renames `cc-*` contexts after
  their first real session id arrives, so a stamped label can go stale. A
  stale label is not merely cosmetic — it is a *wrong address* displayed as
  a right one, which is the failure class this whole lane exists to remove.
- **Resolve the label at hydration.** Always current by construction, and no
  schema change. But `translate_block` has no DB access today, and it is
  called from several paths (`llm/mod.rs`, `mailbox.rs` ×3). It wants a
  label snapshot built once by the caller and passed in — deliberately *not*
  a live DB handle held across hydration, which would invite the lock
  ordering we already have filed pain about.

Resolution-at-hydration is the correct design. It is a small design pass, not
a mechanical edit, so it is not being done on a guess. Note that gap #4 also
matters less now that slice 3 landed: a receiver can reply with `.parent`
where the drift came from its parent, without needing any label at all.

**Slice 5 — real presence.** A kernel-side "who is attached right now" view,
distinct from "not yet archived". Wants the `ContextRegistry` extraction first.
Design pass before code.

**Slice 6 — arrival wakes a turn.** See gap #2's four shapes; Amy's ruling,
not scheduled.

## Open questions for Amy

1. **Should an arriving drift be able to trigger a turn?** The four shapes are
   in gap #2 above. This is the one that touches the turn loop and the spend
   posture at once, so it is yours to rule on.
2. **May a `session.end` hook conclude its context?** Slice 2 depends on it.
   Concluding is non-destructive — blocks are kept, the label stops competing —
   but it changes what a cc session leaves behind, and the original pipeline
   left pileup unmanaged *deliberately* ("for observation", `0c06f51c`). Four
   weeks of observation now say the cost is a 60-way ambiguous prefix. Asking
   rather than assuming, because reversing that decision is yours.
3. **Do we sweep the 152 existing cc-\* contexts?** A one-shot conclude of
   everything with no activity in N days would restore the namespace
   immediately. Same non-destructive argument, larger blast radius, and it
   touches contexts other sessions may still consider theirs.
4. **Is `lost+found` discoverable enough?** It is created lazily and nothing
   points at it. If drift starts carrying musical material, a silent
   `lost+found` is a dropped phrase nobody goes looking for.

Resolved while drafting: *does `push` delivering immediately break an existing
caller?* No — no rc script, orchestration path, or non-test caller invokes
`kj drift push` anywhere in-repo. Only help docs and tests, all updated.

## What we are not doing

Redesigning drift toward cc's model. Point-to-point ephemeral messaging between
two processes is a weaker primitive than a durable block on a shared document,
and the many-hands stance is the whole project. We are closing an ergonomics
gap, not adopting an architecture.
