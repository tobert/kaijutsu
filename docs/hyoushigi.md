# 拍子木 Hyoushigi — kaijutsu's heart of time

> The wooden clappers that mark time in kabuki and sumo — struck to signal *now*,
> and to set the anticipation of what comes next.

Hyoushigi is kaijutsu's timing substrate: the thing that gives every context a sense
of *when*, a memory of *what already happened*, and a machine for *staging what comes
next*. It is the same engine whether a context is writing code, composing MIDI, or
rendering audio — those differ only in how fast their clock ticks and whether that
clock is allowed to wait.

## What it's for

Three workloads kaijutsu cares about are all the same shape — content produced *in
order over time*, where the next thing must be ready before the moment it's needed:

- **Coding** — a turn goes in, a turn comes out, tools fire. Slow, and a human is
  usually waiting, so the clock can stop and wait too.
- **Composing** — a "composer" context aware of an ongoing beat (synced to an external
  MIDI clock), reactive to MIDI and environment events, taking turns that may be
  beat-driven. The beat does **not** wait for a model turn.
- **Audio** — sample-rate content driven by hardware; the DAC never waits.

Coding and audio are the two ends; the composer sits between them and is the reason
hyoushigi exists as one engine rather than three special cases. It needs the LLM-turn
reactivity of a coding context **and** the can't-stop-the-clock discipline of audio,
at once. A design that serves only the easy end (coding) would not carry the composer;
a design that serves the hard end (audio) carries all three. So we design for the hard
end and let the easy end use a dormant subset.

## The core contract

A `Cell` is exactly three things, none content-specific:

1. **a position** — `Span { start, len }` on the timeline. `len == 0` is an instant.
2. **a way to produce content** — `Body`, either `Concrete(ContentRef)` (a literal or a
   crystallized prior result) or `Deferred(Recipe)` (a resolver id + params + a
   `ContextQuery` + a required `fallback`).
3. **a state** — `CellState`, its position in the lifecycle.

Content *type* is deliberately **not** one of the three. It lives inside `ContentRef`,
a slim newtype `{ hash: ContentHash, mime: String }` — a real `kaijutsu-cas`
`ContentHash` (so a malformed hash crashes at construction, not deep in a lookup) plus
an **open-string** MIME, opaque to the core, an open label rather than a closed enum —
so a new content type never edits the substrate, only the resolver that interprets it.
`ContentRef` is the *cell-body contract* (hash + mime, nothing else); `kaijutsu-cas`'s
`CasReference` — which also carries `size_bytes` / `local_path` — is one way to satisfy
it and `From`-converts into a `ContentRef`. The cell never sees a filesystem path.

The one capability is the `Resolver` trait:

```rust
trait Resolver {
    fn id(&self) -> ResolverId;
    fn estimate_cost(&self, params, rctx) -> Duration;       // wall-clock, feeds lead time
    fn compute_basis(&self, params, rctx) -> ContextHash;    // the equivalence class
    fn resolve(&self, params, rctx) -> Result<Resolution>;   // content + emitted cells
}
```

**Everything content-specific is a Resolver. The scheduler never sees anything
narrower.** Adding a modality — text, MIDI, audio, a model turn, a tool call — is a new
`impl Resolver`, never a change to the substrate. A recipe is *data* (resolver id +
params + query), not a closure, so cells persist and round-trip through storage. There
is no freeze/mutate policy: a cell resolves once, its committed content is immutable,
and a cell that wants to behave differently next pass emits a *fresh* cell into the
future — a loop unrolls into distinct committed memories rather than mutating one.

Lead time is derived, not authored:

```
lead_time       = estimate_cost(...) × safety_factor    # wall-clock
speculate_at    = start − beats_for(lead_time)
commit_deadline = start − commit_margin
```

A MIDI resolver reporting tens of ms speculates about a beat ahead; a neural-audio
resolver reporting tens of seconds speculates minutes ahead — automatically, with no
magic numbers.

### Invariants

- **The commit point is a write barrier.** Behind it, history is immutable and
  append-only; ahead of it, the future is open to rewriting. Emitted cells may append
  to the past (recording a memory) or land in the open future, but may **never** rewrite
  a committed cell. That single rule is what keeps a self-modifying timeline analyzable.
- **A speculation reads committed + past + ambient, never another speculation.**
  Enforced structurally: a resolver is only ever handed a committed view, so an
  uncommitted cell simply has no view to give. Otherwise squashes cascade unpredictably.
- **Crash over silent corruption.** Illegal lifecycle transitions return `Err` and
  leave state untouched. Every `Recipe` carries a *required* `fallback` (`Skip` /
  `UseLastGood` / `Literal`), so a real-time miss with no time to recover can never
  reach undefined behavior — an omitted fallback is impossible by construction.

### The lifecycle and the misprediction mechanic

```
Concrete:  ───────────────────────────► Committed     (born committed)
Deferred:  Pending ─► Speculating ─► Speculated ─► Committed
                          │              │
                          └──► Squashed ◄┘ ──► (re-speculate if time remains)
              any of the above ─► Failed
```

At `speculate_at`, snapshot `basis = compute_basis(...)` and run `resolve`. At
`commit_deadline`, recompute the basis against current context: if it matches, commit
and crystallize to CAS; if it diverged, **squash** — re-speculate if `≥ estimate_cost`
remains, else fire the fallback. The squash is recorded, not hidden: a `Squashed` event
carries both the predicted and the actual context digest, which is the most valuable
output the system produces — it tells you exactly where the anticipation model is wrong,
and `estimate_cost` learns from the measured cost.

## Can the playhead block? — the one axis that matters

The character of a context is set by a single question: **is something external driving
time that won't wait?**

| Context | Clock driver | Can block? | Speculation |
|---|---|---|---|
| coding (human in loop) | agent events; a human waiting | **yes** | dormant — degenerates to optional prefetch |
| composer | external MIDI clock / beat | **no** | load-bearing |
| audio | hardware DAC | **no** | mandatory, large lead times |

When the clock can block, the playhead simply advances as resolves complete, *never*
mispredicts (infinite effective lead time → everything commits on the first try), and
the whole speculative apparatus sits idle. When the clock can't block, the resolver's
content must be ready *before* the playhead arrives or the fallback fires — so it has to
be staged ahead, against a predicted context, and squashed when the prediction breaks.

This is why the composer is the hard, defining case. Its clock can't block (the beat
marches) **and** its resolver is slow and expensive (an LLM turn, or synthesis). That is
precisely the corner where speculation is neither optional nor cheap — and it's the only
corner a coding-only design would never have to face.

### PPQ is resolution, not the blocking axis

A context also picks a **PPQ** (pulses per quarter — the resolution of its clock): ~1
for a coding session whose playhead advances per turn, 24 for sequencer-grade MIDI, very
high for sample-adjacent audio. PPQ is *orthogonal* to whether the clock blocks: an
offline render can be high-PPQ, a real-time control loop can be coarse. Keep the two
knobs separate. The position type is therefore **a logical integer coordinate with a
pluggable wall-clock binding**, not a hardcoded musical tick — `Tick` for position,
`TickDelta` for duration, `Tick + Tick` a compile error, and "musical tick @ N PPQ" just
one binding among several.

#### The `Tick` / `TickDelta` algebra (step 1, spec'd)

This is the point-vs-vector (affine) distinction — the same one `std::time` draws
between `Instant` and `Duration` — applied to logical timeline coordinates. `Tick` is an
absolute position (`Span.start`); `TickDelta` is a duration/offset (`Span.len`). Making
the meaningless operation a *compile error* is the whole point:

```
Tick      + TickDelta → Tick         // a position, offset → a position
Tick      − TickDelta → Tick
Tick      − Tick      → TickDelta     // difference of two positions is a duration
TickDelta ± TickDelta → TickDelta
TickDelta × i64       → TickDelta     // scale a duration (k beats ahead)
Tick      + Tick      → ✗ no impl     // adding two positions is nonsense (Instant+Instant)
```

- **Both newtype over `i64`.** `Tick` is monotone and `Ord` (the write barrier asks "is
  this behind the commit point?"); `TickDelta` is **signed**, because emitted cells "may
  append to the past" so `earlier − later` is legitimately negative. `i64` for both
  avoids underflow ceremony.
- **Lives in `kaijutsu-types`, not the new crate.** `BlockSnapshot` (in `kaijutsu-types`)
  gains a tick coordinate at materialization, so it must be able to *name* `Tick`. Putting
  `Tick` in `kaijutsu-hyoushigi` while that crate depends on the block model would be a
  cycle. The coordinate is foundational; the engine that schedules over it is the new
  crate. This is exactly why step 1 precedes step 2.
- **The binding is a separate domain, with two concrete voices.** `Tick` carries *no*
  wall-clock; mapping `Tick ↔ wall-clock` needs a binding holding `(epoch_tick,
  epoch_wallclock, ppq, tempo)` — the `Timebase`. Two impls: the **kernel** uses a plain
  `Timebase` struct (free-running authority, no Bevy); the **client** uses a Bevy
  `Time<Hyoushigi>` custom context (see below). Step 1 ships only the coordinate algebra
  + a trivial/stub binding — PPQ rides on the binding, never on `Tick`.

#### Bevy coexistence (the client binding rides `Time<T>`)

Bevy already provides the "pluggable wall-clock binding" abstraction, so the disciplined
client follower is **not** a bespoke phasor — it rides Bevy's clock machinery:

- **No namespace clash.** Bevy's `Tick` (`bevy_ecs::change_detection::Tick`) is a `u32`
  *wrapping* change-detection counter and is **not in the prelude**; ours is a monotone
  `i64` position. Different concept entirely. Import ours qualified
  (`hyoushigi::Tick`) in any Bevy module that also touches change-detection.
- **Slew = `Time<Virtual>::set_relative_speed`.** Disciplining a follower toward a kernel
  correction is nudging `relative_speed`, not jumping — exactly the doc's "slews toward
  occasional corrections." `advance_by` / `advance_to` let the clock be driven by an
  external source (MIDI beat, network timebase) instead of the OS clock, and `advance_to`
  *panics on backward time* — the write-barrier "no backdating" stance, enforced by Bevy.
- **Sub-tick phase = `Time<Fixed>` overstep.** `set_timestep_hz` is the PPQ→Hz binding;
  FixedUpdate running zero-or-more times per frame *is* the playhead advancing by completed
  ticks, with `overstep_fraction` as the sub-tick phase for UI interpolation. The client
  phasor reuses this accumulator rather than reinventing it.
- The **coordinate** stays Bevy-free; only the **client binding** depends on Bevy. The
  kernel binding (the authority) never does.

## What may be speculated — and what may not

Speculation runs a resolver *early*, before its content is needed, betting that context
won't change before the commit deadline. That bet has a real price, so the rule is
narrow and explicit:

- **Idempotent, reversible, side-effect-free resolves only.** Prefetching a file into
  the CRDT cache, pre-rendering an audio buffer, pre-resolving a pure transform — all
  safe to discard on a squash.
- **Side-effecting tools are never run speculatively.** A `shell`, `write`, or `git`
  resolve waits for commit; its effect can't be un-happened by a pipeline flush.
- **A speculative model turn costs tokens whether or not it commits.** So lead-time
  derivation and the cost model aren't only latency optimizers — they are budget
  controls. A squash is not free; it is paid for in compute, and the `Squashed` log is
  also the bill.

In a human-driven coding context none of this is on the hot path: the clock blocks, so
hyoushigi just computes each cell when the playhead reaches it. The one opportunistic
win there is prefetch — staging the file a turn is about to read — and it is exactly
that, opportunistic, never required for correctness.

## A context owns a timeline, over one store

Each kaijutsu context (`ContextId`, a node in the fork/drift DAG) owns a timeline: its
CRDT block log *with a temporal structure over it*. Hyoushigi does **not** invent its
own persistence — it is the temporal schema and the live engine; the durable record
stays in kaijutsu's existing machinery.

- **Timeline structure → CRDT blocks.** A committed cell is materialized as a block in
  the context's block log, tagged with its tick position — a *new* coordinate the block
  model doesn't carry today (blocks order via `order_key`, a fractional index; see open
  questions). The block itself is ordinary, so it syncs across participants and survives
  restarts.
- **Content bytes → CAS.** A cell's `ContentRef` is a `kaijutsu-cas` `ContentHash`. The
  content store and the memoization key are the same thing: identical inputs → identical
  hash → no recompute.

So there is exactly **one** source of truth for "what happened" — the block log + CAS.
The only live, in-RAM part of hyoushigi is the **open future**: pending and speculating
cells ahead of the commit point. The instant a cell commits it crosses the write barrier,
*becomes* a durable block, and leaves the live window.

| kaijutsu | hyoushigi |
|---|---|
| a `ContextId` and its block log | a timeline |
| `BlockSnapshot` | a committed `Cell` (+ tick position, + richer lifecycle) |
| `BlockKind` (Text, ToolCall, Drift, …) | the content type a resolver emits — never seen by the substrate |
| `Status` (Pending / Running / Done / Error) | a lossy projection of a **new** `CellState` enum (Pending → Speculating → Speculated → Committed / Squashed / Failed) — see below |
| `ContentHash` (CAS) | `ContentRef` — content store **and** memoization key |
| a tool dispatch / model turn | a `Resolver::resolve` that emits blocks |
| `fork` | branch the timeline |
| `drift` | offer a child's timeline material to another context; the receiver decides how to absorb it |
| `kaijutsu-abc` `to_midi` | a MIDI resolver's content type |

Be honest about the reuse: this is mostly **new** type surface, not something latent in
the block model. `CellState` is a new enum with a speculative dimension `Status` doesn't
have — `Status` is at best a *lossy projection* of it (Committed→Done, Squashed/Failed→
Error, the speculating states invisible), not a refinement of an existing lifecycle.
Likewise the block model has no tick/`Span` coordinate today (only `order_key`), no
`content_type` companion on `ContentHash`, and no squash/fallback machinery. The genuine
prior art is *structural, not semantic*: blocks are already a synced, restart-surviving
CRDT log, so materializing committed cells **as** blocks is cheap. The temporal schema
over them — positions, lifecycle, the resolver dispatch — is the part we build.

## The composer playhead: beat-aware, turn-reactive

A composer context's playhead is driven by an **external beat** — your MIDI clock — and
its turns are reactive: MIDI input, environment events, and sibling-context messages
arrive and are quantized to the grid. A turn may be *beat-driven* (fire on a beat
boundary) rather than on-demand. Because the beat won't wait for a model turn, the next
beat's content has to be staged ahead of the playhead:

- **A beat is a near-future deadline.** Scheduling a resolve at beat N+k opens the gap
  between *now* and N+k as the window to resolve in — pre-resolve against the predicted
  context, commit if it held, fall back if the turn veered or ran long.
- **Coalescing.** Events landing in one beat window batch and parallelize
  deterministically — the per-context mailbox's *flush-on-next-turn*, generalized to
  *flush-on-next-beat*, with the same atomicity guarantees (a tool_use + tool_result
  pair never split).
- **Structured backpressure.** A beat is a rate limiter with shape — tokens-per-beat,
  calls-per-beat — instead of hammering.
- **A clean, replayable timeline.** A quantized past replays and renders well.

A coding context can borrow the same beat machinery when it runs **autonomous /
headless** (the turn FlowBus is where a tempo wants to live): quantize tool calls to a
near-future beat to get coalescing, backpressure, and deterministic replay. When a human
is in the loop waiting on a single answer, drop the quantization and advance the instant
the resolve completes — holding a 5 ms answer to a 500 ms beat is pure latency unless the
window is buying coalescing or coordination. That choice (quantize vs. advance-now) is a
per-context policy, discussed under open questions.

## The DAG of contexts is a DAG of timelines

kaijutsu contexts already form a fork/drift DAG. Once each context has a timeline, the
DAG *is* a tree of timelines. fork and drift are **not symmetric** here: fork is easy,
drift is the interesting, hard one.

### fork is easy: clone the timeline, optionally stay synced

A fork branches the timeline — it clones the open future and the wall-clock binding into
the child, a speculative future made first-class and durable rather than an in-place
squash-and-retry. Same PPQ, same scale, by construction.

Staying *synced* after the fork is a cheap stretch on machinery we already have: run the
child as a **disciplined follower** of the parent's timebase (the same discipline loop
clients use). The two then share a beat, which is also what an ensemble of sibling
contexts on one clock would want. Land plain fork first; sync is opt-in on top.

### drift is cool, and hard: an offer the receiver resolves

Drift is **not** an automatic merge of a child's committed past into the parent. A drift
**carries data about its hyoushigi** — the drifted material's cells, positions, tempo,
PPQ — and the *receiving* context decides what to do with it. The receiver is in charge,
because only it knows its own grid.

Concretely, the shape we're aiming at: you pop into a context, remember it had a funky
bass line, and say *"drift that bass line over to `abcdef09`."* The offer arrives at
`abcdef09` with its timing intact; that context can **beat-match** it onto its own grid
(this is the cross-PPQ rescale — but now a deliberate musical operation the receiver
performs, not a silent merge), **preview** it in context so the user can actually listen
before committing, and then decide whether to absorb it and how. Accept, reject, place it
here vs. there, re-tempo it — all the receiver's call.

This keeps the crash-over-corruption stance clean: two scales never silently mix on one
line, because the receiver either maps the incoming material explicitly (beat-match) or
declines the offer. Most of this — the preview, the placement UI, the beat-match
resolver — is way down the road and unanswered. The point for now: **fork is settled,
drift is a deliberate receiver-resolved offer, and we don't have to design its interior
yet.**

The non-temporal "associative graph" question is answered by inheritance: kaijutsu's
existing DAG + `kaijutsu-index` (semantic vectors) *is* that graph; hyoushigi is the
temporal projection over it. We don't redesign either here.

## Distribution: the kernel is the time authority

The kernel hosts the authoritative timelines (one per context) and distributes time to
connected clients — the Bevy app, MCP clients — over the FlowBus, the same way state
already syncs. The decision that matters: **the kernel publishes a timebase, not a
stream of raw pulses.**

```
Timebase {
    context: ContextId,
    epoch_tick,            // a known tick…
    epoch_wallclock,       // …pinned to a known kernel wall-clock instant
    ppq, tempo, phase,
    seq,                   // monotonic, for ordering/liveness
}
```

Each client runs a **local phasor** seeded from that timebase, generates its own pulses
locally, and *slews* toward occasional corrections rather than jumping. The wire rate is
thereby decoupled from PPQ: a high-PPQ context costs the same handful of correction
packets as a low-PPQ one. The subject is an ordinary advisory FlowBus topic
(`clock.<context_id>` / `tempo.*`), fire-and-forget, and doubles as a heartbeat — a gap
in corrections is a liveness signal. Cap'n-Proto-over-SSH is TCP, so individual packets
jitter; that is fine for a correction stream and is exactly *why* clients discipline a
local phasor instead of trusting any one pulse's arrival.

At low PPQ the kernel may also emit literal pulses for clients that just want a tick to
react to (a UI flash, a mailbox flush). **Audio is the exception: it is always
hardware-clocked, never network-clocked.** The DAC's clock drives the callback; the
kernel timebase only aligns *musical* position, and the audio path slews loosely to it.
Network jitter never enters an audio callback.

### Clients run hyoushigi too

The app may attach to several contexts at once (the constellation view), each needing its
own disciplined clock, so the client crate carries hyoushigi as well and exposes the same
read/observe/schedule API and per-`ContextId` registry. Only the *role* differs:

- **Authoritative** (kernel): free-running, drives tempo, commits cells into CRDT/CAS.
- **Disciplined** (client): a follower seeded from a `Timebase` and slewed to stay
  locked. The discipline loop lives inside the type, so a client just reads `now()` /
  phase and trusts the lock.

A client may hold its own local open future — ephemeral speculative cells it owns (the
next beat's UI animation, a pre-rolled buffer) — without touching the write barrier.
Anything that must become durable shared memory is proposed to the kernel and commits
there, like an optimistic CRDT edit. On reconnect the follower re-disciplines from a
fresh timebase and unconfirmed local speculations **squash** — reusing the misprediction
machinery, mirroring how `ActorHandle` re-registers subscriptions after a reconnect.

## Visualizing past sessions

Because the timeline structure is durable CRDT blocks with positions, a context's
hyoushigi can be *rendered* — in the Bevy UI a session becomes a literal timeline you can
scrub and inspect. It is also a forcing function: the committed timeline must stay
queryable and well-ordered, which is exactly what writing it into the block log buys.

## Settled vs. open

**Settled:**

- The three-part cell contract; `Resolver` as the one capability; content-type
  agnosticism (a new modality is a downstream `impl Resolver`, zero substrate change).
- The committed/speculative split, the write barrier, no-reading-speculations, required
  `fallback`, crash-over-corruption.
- Position is an integer logical coordinate; wall-clock is a separate domain bound at the
  driver/PPQ boundary. PPQ (resolution) and can-the-clock-block are independent knobs. The
  `Tick`/`TickDelta` algebra is spec'd (affine point/vector, both `i64`, in `kaijutsu-types`
  so blocks can name it); the wall-clock binding has two voices — a plain kernel `Timebase`
  and a Bevy `Time<Hyoushigi>` client context that slews via `set_relative_speed` and reuses
  `Time<Fixed>` overstep for sub-tick phase. Bevy's own `Tick` (u32, change-detection, not
  in prelude) does not collide.
- **One store:** timeline structure → CRDT blocks, content → CAS. No parallel history.
- **Per-context timeline.** fork branches the timeline (same scale, optionally a synced
  disciplined follower); drift is a **receiver-resolved offer** carrying hyoushigi data,
  never an automatic merge. The associative graph is delegated to the existing DAG +
  `kaijutsu-index`.
- The kernel is the **time authority**, publishing a per-context **timebase** (not raw
  pulses); clients run **disciplined** followers. Audio is hardware-clocked.
- Hyoushigi is **symmetric across kernel and client** — same crate, same API and
  per-`ContextId` registry; role (authoritative vs. disciplined) is the only difference.
- **Speculation is narrow:** idempotent/reversible resolves only; side-effecting tools
  wait for commit; a wrong speculation costs real compute and is logged as such.
- **`ContentRef` = a slim newtype, not a parallel content-type system.** It is
  `{ hash: ContentHash, mime: String }` — the existing `kaijutsu-cas` hash newtype plus
  an open MIME — and `CasReference` `From`-converts into it (dropping `size_bytes` /
  `local_path`, which a cell body doesn't carry). Content typing is **two tiers, one a
  projection of the other**, exactly as the `Image` block already works: the open-string
  MIME (in `ContentRef` / the CAS sidecar) is canonical and resolver-interpreted; the
  closed `ContentType` enum on the block is a *render-dispatch hint* derived from it via
  `ContentType::from_mime`, unknown → `Plain`. The hyoushigi core only ever holds the
  open string, so adding a modality is still zero substrate change; a new closed-enum
  variant lands only when its *renderer* does (a UI change). The boundary is asymmetric:
  **crash on a malformed hash** (structural — bad hash = corruption), **be lenient on an
  unknown MIME** (open by design — never an error, just `Plain`). This subsumes the old
  "hash newtype at the CAS boundary" item.
- **Byte homes, one rule.** The `ContentRef` hash is *always* the immutability anchor +
  memoization key (the "crystallize to CAS at commit" barrier). Where the *rendered*
  bytes also live is materialization detail: text-ish content (model turn, pure
  transform) inlines in `block.content` and lets `from_mime` pick the text renderer;
  binary/large content (MIDI, audio, images) lives in CAS only, with `block.content`
  holding an optional human caption and `output` unchanged for structured shell
  rendering.
- **Beat policy is derived from two bits, with one declared knob.** `!clock_can_block`
  (composer, audio) → **quantize-and-stage**, mandatory. `clock_can_block && has_waiter`
  (a human/sync RPC blocked on one answer) → **advance-now** — never hold a 5 ms answer
  to a 500 ms beat. `clock_can_block && !has_waiter` (autonomous/headless coding) → a
  **declared** policy that *defaults to advance-now*, opt into quantize when you want the
  coalescing / backpressure / deterministic-replay a tempo buys. Only that last quadrant
  is genuinely policy; the rest is automatic from the blocking axis. (Beat *period* and
  sibling-shared clocks remain open — see below.)
- **The write barrier holds because the kernel is the sole *timeline sequencer*.** kaijutsu
  already separates two layers: each block owns its own diamond-types content CRDT (file
  text, message text, tool input — character-level, multi-writer), while block *membership
  and ordering* are structural. The barrier governs only the **structure layer of
  `Conversation` documents** — which cell exists at which `Tick`. It does **not** gate block
  content, and does not touch `Code` / `Text` / `Config` documents: a code file or a future
  musicxml doc stays a fully independent multi-writer CRDT, because timelines layer onto
  `Conversation` docs only. Committed-past immutability comes from **crystallizing the
  cell's content to CAS** at commit (a content-addressed snapshot), so the live content
  document may keep evolving without violating the barrier. And the sequencer is continuous
  with the existing per-context **mailbox** — already the atomicity gate that serializes
  block insertion and keeps tool_use/tool_result paired — it merely also assigns the `Tick`.
  So no CRDT capability in actual use is lost: content co-editing and cross-context sync are
  untouched. (Enforcement — rejecting or re-sequencing client-proposed blocks that backdate
  behind the commit point — lands when multi-writer timelines do; the single-writer first
  proof needs none of it. Rejected alternatives: per-author/per-lane barriers, which leave
  global state at a tick undefined and force vector-clock-aware cells; conflict-as-squash,
  where a late packet could squash seconds of paid generation.)
- **`order_key` and `Tick` coexist cleanly.** `order_key` stays the CRDT's sibling-ordering
  index; `Tick` is the kernel's semantic coordinate; at materialization the kernel emits an
  `order_key` that appends so CRDT order matches `Tick` order. (DAG parentage is a third,
  orthogonal axis — *across* contexts, not within one timeline.)

**Open / deferred:**

- **drift's interior** — the receiver-side beat-match (cross-PPQ rescale as a deliberate
  musical op), the in-context preview/listen-before-merge, and the accept/place/re-tempo
  UI. fork (incl. optional sync) is settled; drift's interior is wide open and deferred.
- **The equivalence-class projection** (`compute_basis`) for the first real resolvers —
  too strict thrashes, too loose commits stale content. Unwritten until a resolver exists.
- **Beat cadence (the residue of beat policy).** The *policy* — quantize vs. advance-now
  — is settled above; what stays open is the tuning: the beat *period* per context, and
  whether sibling contexts can share one clock for ensemble drive. Both want the real
  composer to tune against, and the cadence wants confirming against the autonomous-turn
  FlowBus and the mailbox flush boundary.

## Sequencing

The first proof is deliberately **not** the easy (coding) end. A coding context @ ~1 PPQ
blocks, so it would build the whole speculative apparatus and never run it — shipping a
speculation engine that has never speculated. So the first proof is a **composer-lite**
context: the smallest thing whose clock *can't block*, so speculation, squash, and
fallback are exercised by their first user.

> **Status (landed):** steps 1–3 and the headless core of 4 are implemented and
> green. The build order was reordered from the list below — the speculative
> engine (4-core) was proven against an in-memory committed log *before* wiring
> real block materialization (3), so the novel squash/fallback logic got TDD
> feedback first. Done: `Tick`/`TickDelta` in `kaijutsu-types` (+ `trybuild`
> compile-fail guard); the `kaijutsu-hyoushigi` crate (`Cell`/`Body`/`Recipe`/
> `Fallback`/`CellState`/`Resolver`/`ContentRef`); the in-memory `Timeline` with
> lead-time derivation, the speculate→commit-or-squash→fallback loop, and a
> `SquashEvent` ledger (predicted vs. actual basis) — exercised by clean-commit,
> squash→fallback, and squash→re-speculate→commit tests; a `tick: Option<Tick>`
> field on `BlockSnapshot` through the **full capnp wire** (schema + both
> conversion directions + roundtrip test) and all struct-literal sites; the
> `Cell → BlockSnapshot` materialization (mime → `ContentType::from_mime`, text
> inline vs. binary-as-CAS-hash byte homes); and the **internal beat** —
> `tick_at`/`pump` drive the playhead from a wall-clock reading without waiting
> for resolves (the actual interval timer is the kernel/client integrator's job,
> keeping the crate runtime-agnostic). **Not yet:** wiring hyoushigi into the
> kernel at all (it's a standalone island — no live code constructs a ticked
> block, so every block on the wire has `tick = None`); the UI timeline render;
> and steps 5–6. **At integration:** attach `hyoushigi.tick` as a span attribute
> on the `engine.block_create`/`block_append` (and materialize→insert) spans, so
> a block's timeline position shows up in traces — there is no producer to
> instrument until then.

1. **Generalize position first** — land the `Tick` / `TickDelta` split as the
   logical-coordinate-with-pluggable-binding generalization, per the spec'd algebra under
   "PPQ is resolution." Both `i64` newtypes in `kaijutsu-types`; the binding is stubbed.
   TDD: assert the arithmetic, plus a compile-fail check (`trybuild`) that `Tick + Tick`
   is rejected. Per-context PPQ rides on the binding, not the coordinate.
2. **Create the crate** `crates/kaijutsu-hyoushigi`, depending on `kaijutsu-cas` for
   content refs and on the CRDT block model for materialization. (New surface — the doc no
   longer assumes anything is latent in the block model; see "the reuse is structural.")
3. **Plumb materialization** — committed cells → blocks. This is where the open data-model
   questions get answered, not deferred: `order_key`-vs-`tick`, and `content`/`output`/
   `ContentType` vs. `ContentRef`. Single-writer to start, so the write-barrier-vs-CRDT
   problem is sidestepped (and gated before any collaborative timeline).
4. **First proof — composer-lite.** A context driven by a minimal *internal* beat (no
   external MIDI yet) whose playhead can't block, one `Resolver`, and a real
   speculate→commit-or-squash→fallback loop against a first `compute_basis`. This forces
   the hard core to actually run. Render the resulting timeline in the UI.
5. **Real composer** — discipline the beat to an *external* MIDI clock, add the composer
   `context_type` (rc scripts, tool policy), and a MIDI resolver via `kaijutsu-abc`.
6. **Then the fast end** — audio drivers (hardware-clocked), and the distributed timebase
   for clients that need to follow a context's beat.
