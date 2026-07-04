# トラック tracks — the beat belongs to the track, not the player

> **Status:** SHIPPED — Stages 1–3 M1 landed 2026-06-29/30 (clock on the track,
> score on the track, `ClockSource` generalised + first sound out the door). This
> doc states the design as built; `docs/devlog.md` ("The music stack") carries the
> build story and git history keeps the stage-by-stage trackers and review logs.
> Companions: `docs/hyoushigi.md` (the `Tick`/`Timeline` primitive underneath),
> `docs/chameleon.md` (the music application on top), `docs/midi.md` (network/drift
> clock design + the M2–M4 roadmap), `docs/pcm.md` (the render/wire-cue side),
> `docs/shared-state.md` (the `/run` substrate a probe attachment writes).
>
> **Ahead** (per `docs/midi.md`): M2 MIDI-in telemetry, M3 the drift-modeled clock
> (the `Modeled` variant + `apply_estimate`), M4 cross-node + the edge node. Also
> deferred: the cold-start re-arm sweep (a kernel restart resets tracks to stopped).

## The insight

The beat is **exogenous**. Your watch, the family, the pets; my NTP, compute
availability, a good cluster vs a noisy neighbour — none of those clocks are
*ours*. We are *beaten by the world around us*. So the beat does not belong to
the player. It belongs to the **track**, and **a context attaches to a track to
be beaten by it.**

A track is a **clock domain**: a named cadence with a clock source, a score, and
a set of attached contexts. The track persists; contexts and users come and go,
leaving their mark on it and it on them — *a track is a bit like code, or a good
instrument*. A player is whoever is sitting in the chair right now.

This is the substrate under both halves that were once designed separately:
**chameleon** (musicians playing to a beat) and **myaku** (probes sampling on a
cadence) are the *same shape* — a context attached to a clock domain — seen from
the music side and the metrics side. (See *What this subsumes*.)

Because the clock lives on the track, continuity across a rotation chain is free
by construction: musical time never leaves with a retired context, so there is no
playhead-carry, no `track_head` pointer, no horizon race — the machinery those
workarounds existed for is simply absent.

## The track entity

The runtime shape (`TrackState` in `kaijutsu-server/src/beat.rs`):

```
TrackState {
    clock:          ClockSourceKind,   // what drives the beat — see "Clock sources"
    playhead:       Tick,              // musical time, owned here
    beat_count:     u64,               // cadence math
    transport:      Playing | Stopped, // MIDI-style: stop = stop the clock
    score_context:  ContextId,         // the durable score container — see below
    attached:       Map<ContextId, AttachedContext>,
}

Attachment {                           // the persisted binding contract
    wakeup:  Cadence,        // wake THIS context every N beats (its own divisor)
    rotate:  Option<Cadence>,// self-fork page-turn cadence; None = never auto-rotate
    // (role/behavior is the context's own tick rc — see "Behavior on a beat")
}
```

Each `AttachedContext` bundles the `Attachment` with runtime-only materialization
bookkeeping (failure counters, drain water marks) that is never persisted. A
context is attached to **at most one track** at a time; attaching to a new track
detaches from the old, so one track's beat can never inject another's `KJ_*`
facts.

### 1. The track holds copies of its inputs; the score is emergent

The track does two things with what its players produce: it **takes its own copy
of its direct inputs**, and it **keeps a reference back to the producing
context**. The **score is emergent** from that track-held data, not a single
block log players write into and lose on retirement. `Cell` carries a track lane
*and* a separate `played_by` principal — the track-tagged cell is the track's
copy, `played_by` is the back-reference. A retired player's contributions persist
as the track's copies (the score survives the page-turn), while the
back-reference keeps provenance and a live handle to the context while it's
around. *Like code or a good instrument — it persists, the people come and go,
each leaving a mark.*

### 2. The score container is a real per-track score context (option C)

The score's durable home is a **score context** minted when the track is created:
a real `Live` context (`context_type="score"`, label `score-<track>`) whose
Conversation document *is* the track's score, stored set-once on the `tracks`
row. Because it is a **real `ContextId`**, the entire per-context block machinery
is reused unchanged — `reserve_block_id`, `insert_from_snapshot_as`,
`last_block_id`, `block_snapshots`, `max_tick` all take the score context's id,
with no `TrackId` block-store API and no index/RPC ripple — and it embodies the
design's own thesis: the score context is the durable thing producers rotate
around, browsable in the app like any context. It is a **non-producer**: it never
runs an rc lifecycle, takes a turn, or hydrates to a model — viewable, not
hidden.

The track also owns the live `Timeline` (clock + open future + committed log),
keyed by `TrackId` on the kernel. It is armed on track create and disarmed on
track teardown, **not** on context detach — the timeline survives page-turns. A
fork of an attached context never clones it; the child re-attaches and becomes a
co-producer on the same shared timeline.

### 3. Contexts attach with a per-context wakeup cadence

Attachment is not "fire everyone every beat." Each attachment carries its **own
wakeup divisor**, so one track can wake the **musician** every 8 or 16 bars (play
a phrase) and the **conductor** every 64 bars to check in — and the conductor
*can take its time* manipulating the track and the other contexts' inputs between
wakeups. `wakeup` gates only the rc tick behaviour; the per-beat materialize is
the track's own work and runs regardless. Track-level musical knobs (tempo,
`beats_per_phrase`) live on the track's `BeatPolicy`; the wakeup cadence is the
attachment's.

### 4. The context binds; the child inherits the bind at fork

The rotate cadence is part of the binding — but **the context drives the bind,
not the track.** The track stays *ignorant of context lifecycles*; it just holds
the current set of bindings. At fork, the child inherits the bind (the
attachment row copies with the fork) and re-binds — announces itself to the track
— on the way up. A rotation page-turn is: the context self-forks, the child
inherits the binding, the child re-attaches to the **same** track, the parent
detaches. The clock and score never pause. Lifecycle knowledge stays on the
context side (where `fork` already lives); the track stays a passive clock +
score.

### 5. Non-rotating attachments are first-class

A context can attach with `rotate: None` — an **interactive / user-driven**
context that wants the two-way observability of being on a track (woken on a
cadence, reads/writes the track's score and siblings) but is never auto-rotated.
Attaching is the opt-in; rotation is a separate choice. This is also why tracks
are useful **outside music** — "attach a context to a cadence" is a general
capability (a watcher, a heartbeat, a periodic check-in).

## Clock sources — pluggable drivers

A track's clock is a **`ClockSourceKind`** (`kaijutsu-server/src/clock.rs`) — an
enum, deliberately not a `Box<dyn>` (no hot-loop vtable; maps 1:1 to the
persisted `clock_kind` column):

- **`System(SystemClock)`** — a wall-period timer at a tempo. `next_fire(last,
  now)` returns `now + period`; `set_period` is the tempo knob. The only
  constructable variant today.
- **`Modeled`** — the MIDI/drift-modeled clock, uninhabited until M3. Its shape
  is decided in `docs/midi.md`: a `ClockSource` is a *proxy* for a clock that may
  be *remote* and *drift-modeled* — an edge node observes the master, fits a
  tempo/phase/drift model, and ships low-rate *estimates* over the RPC control
  plane; the kernel regenerates a tight local clock phase-locked to that
  estimate. Distribute tempo and intent, never pulses.
- **arbitrary external signals** — solar-power peaks, compute-availability
  cycles, "good cluster / bad cluster." The track is the seam between an
  exogenous beat and whoever's attached: the world beats the track, the track
  beats the players.

**No realtime pulse path into the scheduler; the heap stays.** Every clock is
generative-local — the scheduler's `BinaryHeap<(Instant, TrackId)>`, the
generation/stale-drop logic (a `stop` then `play` within one period never
double-beats; `play` bumps a generation token that invalidates stale heap
entries), and zero-CPU idle all survive every clock kind. A drift clock corrects
an out-of-band estimate; it never streams pulses at the kernel. At M3,
`apply_estimate` returning "fire moved earlier" must bump the generation and push
a fresh heap entry so the scheduler doesn't sleep through a forward phase
correction — the trait is already shaped for it.

**`clock_kind` is MUTABLE and persisted per driver swap.** A track's *identity*
is durable but its clock *source* is circumstantial: you sketch on the system
clock, then plug in the KeyStep Pro and slave the same track to it. So
`clock_kind` updates in `persist_track` whenever the driver changes — the
deliberate opposite of `score_context_id`, which stays set-once because the score
IS the track's durable identity. A restart rebuilds the source from the row and
crashes loud on a kind it can't construct (no silent downgrade to the system
clock).

The speculation-cost `TickClock` is *derived* from the effective period; a tempo
change re-slaves it down to the armed timeline (`Timeline::set_clock`) so the
speculation budget can't go stale against a changed firing period.

**Tracks are independent; there is no 'band' (yet).** Each track is its own clock
domain — no broader ensemble entity owns a shared clock. When tracks need to play
together, what we actually want is to **align bars and beats**, and that
alignment is **rare and intentional**, not continuous: a clock type can align to
a shared reference (both slaved to a MIDI clock, or one phase-locking to another)
at a bar/beat boundary, on demand. Cross-track sync is an occasional, explicit
operation between independent clocks, not an always-on conductor.

That independence is also why the "metrics must keep sampling while the music is
paused" problem dissolves: that's just two tracks — a system-clock metrics track
you never stop, and a musical track you do. Pausing one clock domain doesn't
touch the other.

## Behavior on a beat

The track owns *when*; the attached **context owns *what*.** When a track wakes
an attached context, it runs that context's **tick behaviour** — an rc lifecycle
— and injects the fire coordinates as `KJ_` env vars (the kernel-injection
convention, cf. `KJ_PARENT_BLOCK_COUNT`):

| var | source | job |
|-----|--------|-----|
| `KJ_TICK` | the track's playhead | musical position (frozen off-beat by design) |
| `KJ_PHRASE` / `KJ_TEMPO` / `KJ_ROTATE_EVERY` | track policy + attachment | cadence facts |
| `KJ_HEARD` | the track's score context | recent committed notation — the **band view**, spanning all producers across rotations |
| `KJ_PULSE` | per-attachment monotonic counter | ordering key *within a run* (resets on restart — the documented contract, consistent with conversation re-hydration) |
| `KJ_EPOCH_NS` | wall clock, latched once per beat | human "when" + cross-context join — identical for every context woken on the same beat |

A **musician**'s tick behaviour produces ABC into the track's score; a
**probe**'s tick behaviour runs a kaish script that writes `/run`
(`shared-state.md`). Same mechanism, different rc. *Being a "musician" = being
attached to a beat track with the musician tick behaviour* — the
`context_type`-is-an-rc-bundle decomposition (`chameleon.md`): **arming is
attaching.**

The **death certificate** (a woken context that crashed/timed out — only the
parent sees it) is recorded by the track on the attachment, because the track is
the parent that wakes it.

## Concurrent producers — one open future, per-producer failures

The score is built so **N producing contexts can contribute to one track**, and
hyoushigi's existing invariants carry most of it. Music keeps "one *playing*
binding at a time" as a **loadout policy** (two bass phrases on one beat clash),
*not* a structural assumption — the kernel never forbids concurrency; the music
rc just doesn't spawn it (an ergonomic nudge, not enforcement).

- **The per-track `Timeline`'s lock is the sequencer** — synchronous, no `.await`
  ever held under it. N producers queue at the track's lock.
- **No conflict at the same tick.** Two producers committing at the same span
  produce two distinct cells, each with its own `played_by`; ties are allowed
  (`BlockId` is the unique row). Both copies persist with provenance. Concurrent
  producers **coexist — they do not squash each other**: `cas_commit`'s basis
  never reads the committed view, so two players landing a note at the same tick
  is a chord (or a clash), which is correct musical behavior. Squash only
  activates for a future resolver whose basis depends on committed state.
- **Failures route per producer.** The failure ledger lives on the shared track
  timeline, but each `FailureEvent` is stamped with the failing cell's
  `played_by`, and draining is per-context — producer B's resolve failures
  surface in **B's** conversation, never A's. Error blocks land in the
  *producer's* conversation, not the score context.
- **`UseLastGood` stays lane-scoped** (the track's last committed cell,
  regardless of producer) — the listener hears the lane continue. A's fallback
  may repeat B's last phrase; that is a decision, not a bug.
- Ordering of same-tick cells is insertion order at the track lock —
  deterministic per-run, not across runs; `BlockId` uniqueness keeps correctness
  regardless.

## Rotation is a gap, not an overlap

An exogenous clock that doesn't pause during a page-turn must pick one: the
parent detaches synchronously (a few *producerless* beats while the child boots —
a GAP) or the parent lingers until the child binds (two producers
double-scheduling — an OVERLAP). Rotation takes the **gap**: at the rotate
horizon the track synchronously detaches the retiring context and fires `rotate`;
the child forks, attaches to the same track, and is seeded at the track's
*current* playhead. There is never a beat with two producing bindings.

The gap is safe because it is in *live production*, not in *playback*: hyoushigi
stages content **ahead of the playhead** (the speculation lead), so the retiring
parent already committed the handoff beats' notation before detaching. Whatever
consumes the score reads continuous notation across the page-turn — the producer
gap never reaches the DAC. The gap is in *who's composing next*, not in *what's
playing now*. (An atomic `Swap` transport command that briefly suspends the clock
across the handoff remains a polish option for tracks whose lead is shorter than
a child's boot time — deferred.)

`detach` persists the track playhead (the durable record a zero-block child
inherits across a crash), and the attach seed reads the *score context's*
high-water — the durable `tracks` row plus the score's committed blocks are the
track's memory, so a fresh child can never rewind the lane.

## Transport — MIDI idioms, native

Because the clock lives on the track, transport is a single operation on one
clock domain:

```
kj transport attach --track bass [--wakeup N] [--rotate N]   # bind; track created stopped if new
kj transport detach [--track bass]                           # unbind; track persists
kj transport play|pause|stop|tempo --track bass [...]
kj transport ooda|rotate --track bass on|off                 # separate knobs
```

- **stop = stop the clock**, nothing else. Rotation is *suspended*, not cleared —
  the binding's `rotate` cadence is remembered; per-attachment OODA arming is
  untouched (`ooda off` is the separate knob).
- **play = start the clock**; whatever automation was in place resumes,
  including rotation.
- A new track arms **stopped** (no surprise token spend); `play` starts it.
- Name the track directly (`--track <name>`); when omitted, the verb resolves the
  track from the calling context's attachment. (*Vocabulary:* "track" is the
  DAW-sense clock domain / durable identity; "lane" stays reserved for automation
  *inside* a track and "voice" for ABC's `V:` field, per `chameleon.md`.)

## Restart contract

On re-attach after a restart, the committed score is **reconstructed from the
score context's materialized ABC blocks** (`reconstruct_score_cells` →
`Timeline::rehydrate_committed`, virgin-only — crash over corrupting a live log):
the ABC source blocks ARE the committed cells (the MIDI sibling is derived, never
committed), and the materialize cursor starts *past* the restored cells so they
are never re-emitted. Persisted tempo/phrase/clock-kind are recovered from the
`tracks` row — a restart never silently reverts to default policy. The **open
future, squashes, and in-flight speculations do not survive restart**, and
`KJ_PULSE`/`beat_count` reset — consistent with conversation re-hydration; full
cross-restart counter durability lands with the deferred cold-start re-arm sweep.

## Render — a wire cue, not an in-process sink

Rendering the score to sound is **not the track's job**. A mime-keyed
`RenderCue { mime, payload, lead }` is published at the materialize crossing and
consumed by an off-box sink — the app first, which renders `text/vnd.abc` → MIDI
and schedules into its local ALSA seq port at `receipt + lead`. The speculation
lead IS the jitter buffer. See `docs/pcm.md` (5c) and `docs/midi.md`; `kj play
<file.abc>` is the standalone trigger. (An in-process first cut —
`RenderTarget`/`AlsaMidiOut`/`kj transport render` — shipped with Stage 3 M1 and
was demolished 2026-07-02 when render moved to the wire; git history holds that
design record. The seam's *home on the track* — render fed from the materialize
crossing, as a score consumer, never a producer — survived the move exactly.)

The ABC crate exposes the per-event stream a renderer needs:
`kaijutsu-abc/src/midi.rs::events(tune, params) -> Vec<MidiEvent>`, with
`generate` as the SMF-framing wrapper over the same writer.

## What this subsumes — myaku dissolves

`myaku` (the pulse facility) existed because the beat was welded to the
musician's transport, forcing "one executor, two trigger front-ends" so metrics
could keep sampling while music paused. With the beat on the track that whole
tension is **two tracks**, and myaku stops being a facility:

- its **cadence, fire-coordinate injection, and death certificate** are what a
  **track** does (this doc);
- its **`/run` output substrate and `pulse_emit` helper** are what a probe
  *attachment writes* — `shared-state.md`.

A probe is just: *a context attached to a system-clock track, whose tick
behaviour writes `/run`.* `myaku.md` is retired; its detailed `/run` layout lives
in git history until migrated into `shared-state.md`.

## Where the code is

- `crates/kaijutsu-server/src/beat.rs` — `BeatScheduler`, `TrackState`, attach/
  detach/rotation, materialize crossing, `KJ_*` injection, score reconstruction.
- `crates/kaijutsu-server/src/clock.rs` — `ClockSourceKind` + `SystemClock`.
- `crates/kaijutsu-hyoushigi/src/engine.rs` — the `Timeline` (open future,
  committed log, `rehydrate_committed`, `set_clock`); `cell.rs` — `Cell` with
  `track` + `played_by`.
- `crates/kaijutsu-kernel/src/hyoushigi/mod.rs` — `Attachment`, `Cadence`,
  `BeatCommand`, `BeatPolicy`, materialize.
- `crates/kaijutsu-kernel/src/kj/transport.rs` — the `kj transport` surface;
  `kj/play.rs` — `kj play`.
- `crates/kaijutsu-kernel/src/kernel_db.rs` — the `tracks` + `attachments`
  tables (`score_context_id` set-once, `clock_kind` mutable).
- `crates/kaijutsu-abc/src/midi.rs` — `events()` / `generate()`.
- `assets/defaults/rc/musician/` — the musician tick/rotate rc bundle.
