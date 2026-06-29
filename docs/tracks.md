# トラック tracks — the beat belongs to the track, not the player

> **Status:** design direction, captured 2026-06-29 in a co-design session
> (Amy + Claude). Decisions are *directions*, not commitments — code is truth,
> this is where we're aiming. The implementation is future work; a fresh
> Claude + Amy should start the surgery from this doc. Companions:
> `docs/hyoushigi.md` (the low-level `Tick`/`Timeline` primitive this builds on),
> `docs/chameleon.md` (the music application that consumes tracks),
> `docs/shared-state.md` (the `/run` output substrate a probe attachment writes).

## The insight

The beat is **exogenous**. Your watch, the family, the pets; my NTP, compute
availability, a good cluster vs a noisy neighbour — none of those clocks are
*ours*. We are *beaten by the world around us*. So the beat should not belong to
the player. It belongs to the **track**, and **a context attaches to a track to
be beaten by it.**

A track is a **clock domain**: a named cadence with a clock source, a score, and
a set of attached contexts. The track persists; contexts and users come and go,
leaving their mark on it and it on them — *a track is a bit like code, or a good
instrument*. A player is whoever is sitting in the chair right now.

This is the substrate under both halves we'd been designing separately:
**chameleon** (musicians playing to a beat) and **myaku** (probes sampling on a
cadence) are the *same shape* — a context attached to a clock domain — seen from
the music side and the metrics side. (See *What this subsumes*.)

## Where the beat lives today (and why we're moving it)

Today everything that clocks is per-**context**:

- the timeline + playhead is `kernel.timeline(context_id)` (`beat.rs`);
- the scheduler heap entries are keyed by `ContextId`;
- `armed: HashMap<ContextId, BeatState>` holds the policy, and **`track` is just a
  field on `BeatState`** — a label copied across forks, with no entity of its own.

So the track is currently an *emergent grouping* of contexts that share a label.
That forced two workarounds that this model retires:

- **the playhead carry** (`beat_state.playhead_tick`, shipped 2026-06-29): musical
  time is made continuous across a rotation chain by *copying the tick number*
  context→context, precisely because the clock keeps leaving with the retired
  context. If the clock lives on the track, **it never leaves — continuity is
  free, no carry.**
- **`track_head`** (the per-track "who's the live tip" pointer we were about to
  add for `stop --track`/`play --track`): unnecessary once the track *is* the
  entity and knows its own attached contexts.

## The track entity

```
Track {
    id:          TrackId,             // the durable lane identity (already exists)
    clock:       ClockSource,         // what drives the beat — see "Clock sources"
    playhead:    Tick,                // musical time, owned here (was per-context)
    beat_count:  u64,                 // for cadence math
    transport:   Playing | Stopped,   // MIDI-style: stop = stop the clock
    score:       Timeline,            // the track's COPIES of its inputs + played_by
                                      //   back-refs; the score is emergent (entity #1)
    attached:    Map<ContextId, Attachment>, // the track's passive view of current
                                      //   bindings — the CONTEXT binds itself (#3)
}

Attachment {
    wakeup:  Cadence,        // wake THIS context every N beats/bars (its own divisor)
    rotate:  Option<Cadence>,// self-fork page-turn cadence; None = never auto-rotate
    // (role/behavior is the context's own tick rc — see "Behavior on a beat")
}
```

### 1. The track holds copies of its inputs; the score is emergent

The track does two things with what its players produce: it **takes its own copy
of its direct inputs**, and it **keeps a reference back to the producing context**.
The **score is emergent** from that track-held data (plus the contexts'), not a
single block log players write into and lose on retirement. `Cell` already carries
a track lane *and* a separate `played_by` principal — so the track-tagged cell is
the track's copy, and `played_by`/provenance is the back-reference; the mechanism
mostly exists. A retired player's contributions persist as the track's copies (the
score survives the page-turn), while the back-reference keeps provenance and a live
handle to the context while it's around. *Like code or a good instrument — it
persists, the people come and go, each leaving a mark.*

So "the track owns the score" means it owns the **copies + provenance**, and the
score is the emergent ordered view over them — *not* that contexts have no data of
their own. This is the larger part of the surgery (today the `Timeline` and its
cells are reached per-context). It **can be staged** — see *Staging*.

### 2. Contexts attach with a per-context wakeup cadence

Attachment is not "fire everyone every beat." Each attachment carries its **own
wakeup divisor**, so one track can wake:

- the **musician** every 8 or 16 bars (play a phrase), and
- the **conductor** every 64 bars to check in — and the conductor *can take its
  time* manipulating the track and the other contexts' inputs between wakeups.

This generalises today's per-context `ooda_every` cadence and moves its ownership
onto the track (the track knows when to wake each attached context).

### 3. The context binds; the child inherits the bind at fork

The rotate cadence is part of the binding — but **the context drives the bind, not
the track.** The track stays *ignorant of context lifecycles*; it just holds the
current set of bindings. **At fork, the child inherits the bind** — it travels with
the fork exactly as `beat_state.track` does today — and the child re-binds
(announces itself to the track) on the way up. So a rotation page-turn is: the
context self-forks, the child inherits the binding, the child re-binds/re-arms, and
the track's binding set updates because the *child told it* — the track never had
to watch for the fork. The clock and score never pause, so there's no race, no
playhead carry, no `track_head`. Lifecycle knowledge stays on the context side
(where `fork` already lives); the track stays a passive clock + score.

### 4. Non-rotating attachments are first-class

A context can attach with `rotate: None` — an **interactive / user-driven**
context that wants the two-way observability and the other goodies of being on a
track (it's woken on a cadence, it can read/write the track's score and siblings),
but is never auto-rotated. Attaching is the opt-in; rotation is a separate choice.
This is also why tracks are useful **outside music** — "attach a context to a
cadence" is a general capability (a watcher, a heartbeat, a periodic check-in).

## Clock sources — pluggable drivers

A track's clock is driven by a **`ClockSource`**, and the point is that you can
**plug in different drivers and experiment**:

- **v1: the system clock** — a wall-period timer at a tempo (what `beat.rs`
  already does). Start here.
- **MIDI clock** — for sure happening. We'll want some tracks on the system clock
  and some on MIDI timing, running side by side. *(Possible reuse: the app may
  already have time-matching / sync algorithms — worth checking before writing new
  ones. To investigate during implementation.)*
- **arbitrary external signals** — solar-power peaks, compute-availability cycles,
  "good cluster / bad cluster." The track is the seam between an exogenous beat and
  whoever's attached: the world beats the track, the track beats the players.

**Start simple, design for more.** v1 is one wall-clock driver; the seam is a
trait so MIDI and external signals slot in without touching the attachments.

**Tracks are independent; there is no 'band' (yet).** Each track is its own clock
domain — we deliberately do *not* add a broader band/ensemble entity that owns a
shared clock. When tracks need to play together, what we actually want is to
**align bars and beats**, and that alignment is **rare and intentional**, not
continuous: a clock type can *align to a shared reference* — both slaved to a MIDI
clock, or one clock type that phase-locks to another — at a bar/beat boundary, on
demand. So cross-track sync is an occasional, explicit operation between
independent clocks, not an always-on conductor. (It's also why the next point is
free: separate clock domains pause independently.)

The "metrics must keep sampling while the music is paused" problem that made
`myaku` argue for its own scheduler **dissolves here**: that's just two tracks —
a system-clock metrics track you never stop, and a musical track you do. Pausing
one clock domain doesn't touch the other.

## Behavior on a beat

The track owns *when*; the attached **context owns *what*.** When a track wakes an
attached context, it runs that context's **tick behaviour** — an rc lifecycle —
and injects the fire coordinates as `KJ_` env vars (the existing
kernel-injection convention, cf. `KJ_PARENT_BLOCK_COUNT`):

| var | source | job |
|-----|--------|-----|
| `KJ_TICK` | the track's playhead | musical position (frozen off-beat by design) |
| `KJ_PULSE` | a per-attachment monotonic counter | ordering key *within a run* (resets on restart; see Stage-1 follow-ups) |
| `KJ_EPOCH_NS` | wall clock, shared across contexts woken the same tick | human "when" + cross-context join |

So a **musician**'s tick behaviour produces ABC into the track's score; a
**probe**'s tick behaviour runs a kaish script that writes `/run` (the output
substrate, `shared-state.md`). Same mechanism, different rc. *Being a "musician"
= being attached to a beat track with the musician tick behaviour* — which is
exactly the `context_type`-is-an-rc-bundle decomposition (`chameleon.md`):
**arming is attaching.**

The **death certificate** (a woken context that crashed/timed out/OOM'd — only
the parent sees it) is recorded by the track on the attachment, because the track
is the parent that wakes it. (This is myaku's `status` file, generalised.)

## Transport — MIDI idioms, now native

Because the clock lives on the track, transport is a single operation on one
clock domain (no fan-out walk, no `halted_tracks` set, no `track_head`):

```
kj transport stop  --track bass     # stop the track's clock; automation preserved
kj transport play  --track bass     # start broadcast + clock; rotation resumes
kj transport tempo --track bass 120
```

- **stop = stop the clock** for the track. Rotation (the page-turn automation) is
  *suspended*, not cleared — the binding's `rotate` cadence is remembered.
- **play = start broadcast + start the clock.** Whatever automation was in place
  resumes — including rotation. (An explicit `kj transport rotate off` stays the
  separate "permanently stop turning pages" knob.)
- the **horizon race** that plagued the per-context design can't occur: a
  page-turn just has the child inherit + re-bind on a clock that's either running
  or stopped; there's no second per-context entity to arm late.

Surface convention: name the track directly (`--track <name>`); kaish does
context→track lookups on the fly, so `kj` stays crisp. (*Vocabulary:* "track" is
the DAW-sense clock domain / durable identity; "lane" stays reserved for automation
*inside* a track and "voice" for ABC's `V:` field, per `chameleon.md`.)

## What this subsumes — myaku dissolves

`myaku` (the pulse facility) existed because the beat was welded to the
musician's transport, forcing "one executor, two trigger front-ends" so metrics
could keep sampling while music paused. With the beat on the track that whole
tension is **two tracks**. So myaku splits cleanly and stops being a facility:

- its **cadence, fire-coordinate injection, and death certificate** → are what a
  **track** does (this doc).
- its **`/run` output substrate (`now`/`history`/`status`) and `pulse_emit`
  helper** → are what a probe *attachment writes*, and belong to
  **`shared-state.md`** (the VFS-is-the-namespace thesis).

A probe is then just: *a context attached to a system-clock track, whose tick
behaviour writes `/run`.* `myaku.md` is retired with a pointer here and to
`shared-state.md` (its detailed `/run` layout + `pulse_emit` design lives in git
history until it's migrated into `shared-state.md`).

## Staging the surgery

This is real surgery — the clock/playhead/heap move up a level and contexts gain
attach/detach. Stage it:

1. **Move the clock first.** The track owns the clock + playhead + transport + the
   binding set (wakeup + rotate cadence). Contexts bind/unbind. This alone retires
   the playhead carry and `track_head`, gives native `stop/play --track`, and kills
   the horizon race. The score can stay per-context-but-track-tagged for this stage.
2. **Give the track its own score (copies + back-reference).** The track takes
   copies of its direct inputs and keeps a `played_by`/provenance back-reference to
   the producing context; the score is the emergent view. Retired players' copies
   persist on the track. Larger — touches the cell/`Timeline` data path.
3. **Generalise the clock source.** Land the `ClockSource` trait + a MIDI driver +
   the external-signal seam, plus the rare/intentional cross-track bar/beat
   alignment between independent clocks. **The MIDI driver's shape is decided in
   `docs/midi.md` (2026-06-29):** a `ClockSource` is a *proxy* for a clock that may
   be *remote* and *drift-modeled* (observe → model tempo/phase/drift → regenerate
   a tight local clock; distribute tempo/intent, not pulses). Design the trait
   remote- and estimate-shaped from the start; MIDI is the second concrete voice
   for it alongside the system clock. MIDI-out is a *render target* on the track
   (a score consumer), not a clock concern.

The playhead-carry code (shipped 2026-06-29) is a stage-0 stepping stone: it
encodes the invariant (musical time is continuous across the chain) and its tests
describe the behaviour we want; when stage 1 lands, "the clock stayed on the
track" replaces "copy the number," same observable, less machinery.

## Decided (2026-06-29 design round)

- **No 'band' entity (yet); cross-track sync is rare + intentional.** Tracks are
  independent clock domains. Aligning bars/beats between tracks is an occasional,
  explicit operation via alignable clock types (MIDI the prime mover), not an
  always-on conductor. (Resolves the band-clock-vs-track-clock collision: there is
  no band clock; there is no shared `Timeline`.) See *Clock sources*.
- **Data locality: the track holds copies of its inputs + a back-reference; the
  score is emergent.** Not one block log contexts write into and lose on
  retirement. `Cell`'s `track` (the copy) + `played_by` (the back-ref) already
  carry the shape. See entity #1.
- **The context binds; the child inherits the bind at fork.** The track stays
  ignorant of context lifecycles. See entity #3.

## Open questions (for the implementation session)

- **Concurrent producers into one track.** Music keeps one *playing* binding per
  track (rotation swaps it); a user/observer can bind read-mostly. The copies +
  back-reference model makes several contexts contributing copies *tractable* (each
  copy carries its `played_by`), but the conflict story — two producers, same beats
  — is unspecified. Music sidesteps it (one at a time); decide if/when a track wants
  concurrent producers.
- **Score vs context conversation seam.** The score is the track's; a context still
  has its own conversation/hydration. Does a musician's conversation *window* onto
  the track's score (the `$HEARD`/hydration-marker machinery re-pointed at the
  track), and how does that interact with the copies model?
- **Clock-source trait shape** + whether the app's existing time-sync algorithms
  are the right substrate for the MIDI driver and for cross-track bar/beat alignment.
- **Migration / compatibility** — the `beat_state` table, `BeatCommand`, and
  `kj transport` all assume per-context; map each onto the track entity.

---

# Stage 1 implementation — move the clock onto the Track

> **Status:** locked 2026-06-29 (Amy + Claude), in progress. This is the *living*
> implementation tracker — a fresh session continues from here. **No backwards
> compat; on `main`.** Decisions below are firmer than the design sketch above
> because they're checked against current code; where code disagrees with the
> sketch, code wins and the divergence is noted. Tick a box and add a one-line
> note when an item lands; record fresh decisions under *Decisions made in-flight*.

## Where the code actually is (verified 2026-06-29)

The doc above is the design; these are the corrections that survived a code sweep,
and the plan is built on them:

- The beat/clock lives in **`kaijutsu-server/src/beat.rs`** (2856 lines), *not* the
  kernel. `BeatScheduler` holds `heap: BinaryHeap<(Instant, ContextId)>` (`:265`)
  and `armed: HashMap<ContextId, BeatState>` (`:266`).
- **The live playhead is NOT on `BeatState`.** It lives on the per-context
  `Timeline` (`crates/kaijutsu-hyoushigi/src/engine.rs:156`), read live each beat
  (`advance_to(playhead + STEP)`, `beat.rs:594`). `beat_state.playhead_tick` is only
  the **SQLite recovery copy** the carry reads/writes. So "move the playhead to the
  track" = move it off the per-context `Timeline`.
- The fire-coordinate env vars are **un-prefixed** today: `$TICK/$PHRASE/$TEMPO/
  $HEARD/$ROTATE_EVERY` (`beat.rs:139-156, 846-878`). The doc's `KJ_TICK/KJ_PULSE/
  KJ_EPOCH_NS` **do not exist yet**. `KJ_PARENT_BLOCK_COUNT` is real but **fork-only**
  (`kj/lifecycle.rs:452`), and is the precedent for the `KJ_` kernel-injection
  convention we adopt.
- **`track_head`, `halted_tracks`, `myaku` are docs-only** — zero code. Nothing to
  delete there; `track_head` is retired before it was ever built.
- **No `Track` entity, no `Attachment`, no `ClockSource`, no wakeup divisor** exist.
  `TrackId` is a `String` newtype (`kaijutsu-types/src/track.rs:21`) carried as a
  label on `BeatState` (`beat.rs:84`), `Cell` (`cell.rs:126`), and the block
  (`block.rs:1334`). `ooda_every` is a single `BeatPolicy` field (`hyoushigi/
  mod.rs:47`).
- `BeatCommand` + `BeatPolicy` + `BeatAck` live in
  `kaijutsu-kernel/src/hyoushigi/mod.rs:34-155`; `kj transport` in
  `kaijutsu-kernel/src/kj/transport.rs`; `beat_state` DDL + `PersistedBeatState` +
  CRUD + fork-copy in `kaijutsu-kernel/src/kernel_db.rs:615-681, 1466-1505,
  3207-3336`.

## The core data move

| Today (per-context) | Stage 1 (per-track) |
|---|---|
| `heap: BinaryHeap<(Instant, ContextId)>` | `heap: BinaryHeap<(Instant, TrackId)>` — **one entry per track** |
| `armed: HashMap<ContextId, BeatState>` | `tracks: HashMap<TrackId, TrackState>` |
| live playhead on per-context `Timeline`; `beat_state.playhead_tick` recovery copy | playhead **owned on `TrackState`**; per-context `Timeline.playhead` slaved to it each beat (Stage-1 bridge) |
| `beat_state` row keyed by `context_id` | new `tracks` (PK `track_id`) + `attachments` (track_id, context_id) tables; **drop `beat_state`** |

```
TrackState {                                  // runtime, in the scheduler (beat.rs)
    clock:       ClockState,                  // period + next-fire (system clock; the ClockSource trait is Stage 3)
    playhead:    Tick,                        // moved off the per-context Timeline
    beat_count:  u64,
    transport:   Playing | Stopped,
    policy:      BeatPolicy,                   // period/beats_per_phrase/ooda_every — reused as-is
    attached:    HashMap<ContextId, Attachment>,
    materialize_failures, failure_water,       // carried over from BeatState
}
Attachment { wakeup: Cadence, rotate: Option<Cadence>, ooda_armed: bool, pulse: u64 }
```

## Work items (TDD throughout — tests that can and will fail)

- [ ] **1. Types** (`kaijutsu-types` + `kernel/hyoushigi/mod.rs`). Add `Attachment`
  + `Cadence` (a beat-divisor newtype) beside `BeatPolicy`. Reshape `BeatCommand`:
  `Arm{…}` → **`Attach{track, ctx, attachment, policy_if_new}`** + **`Detach{track,
  ctx}`**; `Play/Pause/Stop/SetTempo/SetOoda/SetRotate` re-key `ContextId` → **`TrackId`**.
  *Tests:* command round-trip; attach-creates-track-once; divisor math.
- [ ] **2. Scheduler re-key** (`beat.rs`, the bulk). `armed` → `tracks`; heap →
  `(Instant, TrackId)`. `fire_due`/`process_one`: a track beats once → advance its
  own playhead + `beat_count` → for each attached ctx whose `wakeup` divisor is due,
  seed that ctx's `Timeline.playhead` from the track playhead, materialize, then
  `fire_lifecycle(tick)`; rotate-due → `fire_rotate`. *Tests:* pause freezes /
  resume at +1 (preserve existing behaviour); two attachments with different wakeup
  divisors fire independently; stopped track = no heap entry.
- [ ] **3. Retire the playhead-carry** (`beat.rs:295-435, 551-571`). Delete the
  `from_log.max(carried)` seed dance, the persist-playhead-before-stop, and the
  rotate-horizon defer. Track playhead persists once in its `tracks` row, never
  leaves. **Preserve the carry's tests** re-pointed at "the clock stayed on the
  track — continuity is free" (the carry is a stage-0 stepping stone whose tests
  describe the target).
- [ ] **4. Fork inheritance + re-bind** (`kernel_db.rs:1466-1505` + fork rc). At
  fork, copy the child's `attachments` row like `beat_state` is copied today. Child
  re-binds via an `Attach` from create/fork rc — the track never watches forks.
  Rotation = child attaches to the **same** track (clock never pauses) + parent
  detaches. No persist-before-stop, no horizon race. *Test:* rotation swaps the
  playing binding with zero playhead discontinuity.
- [ ] **5. Persistence** (`kernel_db.rs`). Drop `beat_state`; add
  `tracks(track_id PK, period_ms, beats_per_phrase, ooda_every, playhead_tick,
  transport)` + `attachments(track_id, context_id, wakeup_every,
  rotate_every_phrases, ooda_armed)`. Cold-start re-arm sweep stays **deferred** (as
  today: restart resets to stopped), but the shape supports it. *Test:* CRUD +
  fork-copy of an attachment row.
- [ ] **6. Transport surface** (`kj/transport.rs`). `arm` → `attach --track <name>
  [--wakeup N] [--rotate N]`; add `detach`; `play/pause/stop/tempo/rotate` take
  `--track <name>` (kaish does ctx→track lookup so `kj` stays crisp). *Test:* `stop
  --track` halts one domain, leaves a sibling track running.
- [ ] **7. Fire-coordinate env vars** (`beat.rs:139-156, 846-878` +
  `assets/defaults/rc/musician/`). Rename to **`KJ_TICK/KJ_PHRASE/KJ_TEMPO/KJ_HEARD/
  KJ_ROTATE_EVERY`**; add **`KJ_PULSE`** (per-attachment monotonic counter on
  `Attachment.pulse`) + **`KJ_EPOCH_NS`** (one wall-clock reading shared across all
  contexts woken on the same beat). Update `tick/S10-drive.kai` +
  `rotate/S10-rotate.kai`. *Test:* two contexts woken on one beat see identical
  `KJ_EPOCH_NS` and distinct monotonic `KJ_PULSE`.
- [ ] **8. Docs** — flip Stage 1 boxes here as they land; devlog entry; update the
  `hyoushigi.md` direction note when the clock has actually moved.

## Decisions made in-flight

- **Env vars adopt the `KJ_` prefix** (matching `KJ_PARENT_BLOCK_COUNT`) and gain
  `KJ_PULSE` + `KJ_EPOCH_NS` (chosen 2026-06-29). No back-compat alias for the
  un-prefixed names; the musician rc scripts move with them.
- **Stage-1 bridge: the per-context `Timeline` still owns committed cells**; its
  playhead is slaved to the track playhead each beat. This is the explicit seam
  Stage 2 removes — keep it visible so the later cut is clean.
- **Concurrent producers:** Stage 1 keeps "one *playing* binding per track; rotation
  swaps it." A non-rotating read-mostly attachment is allowed but produces no score
  yet (defers the design's open question).
- **`Attachment.wakeup` subsumes `BeatPolicy.ooda_every`** (refinement, 2026-06-29).
  The clean split that fell out of writing the spine: `BeatPolicy` is now purely
  *track-level* musical knobs (`period` = tempo, `beats_per_phrase` = phrase length —
  properties of the clock domain); the former `ooda_every` moves onto the per-context
  `Attachment` as `wakeup: Cadence` (the divisor that fires the context's tick rc).
  `ooda_armed` stays as the on/off gate. The **per-beat materialize is the track's own
  work** (runs every beat for each producing attachment, independent of `wakeup`);
  `wakeup` gates *only* the rc tick behaviour. So one track can wake a musician every
  128 beats and a conductor every 1024 with no new machinery. `Cadence` is a divisor
  newtype reused for both `wakeup` (beats) and `rotate` (phrases), unit documented per
  field.
- **Transport `Stop` = stop the clock only** (MIDI idiom, per design). The old
  `Stop(ContextId)` also disarmed OODA ("clean stopped state"); the new `Stop(TrackId)`
  only halts the clock — rotation is *suspended/remembered*, per-attachment `ooda_armed`
  is untouched. Re-arming OODA is the separate `SetOoda` knob. The old `Disarm(ContextId)`
  folds into **`Detach`** (a context unbinds; rotation's parent-side + archive both use it).
- **`TrackState` shape (from reading `beat.rs`):** the per-context materialization
  bookkeeping — `cursor`, `materialize_failures`, `failure_water` — cannot sit flat on
  the track once a track has >1 attached context. It moves into a per-context bundle:
  `track.attached: HashMap<ContextId, AttachedContext>` where
  `AttachedContext { attachment: Attachment, cursor, materialize_failures, failure_water }`.
  The persisted `Attachment` (wakeup/rotate/ooda_armed/pulse) is the *binding contract*;
  the materialization fields are *runtime-only*, never persisted. `TrackState` itself:
  `{ policy: BeatPolicy, playing: bool, playhead: Tick, beat_count: u64, attached }`.
- **Rotation never overlaps producers.** At a rotate horizon the track *synchronously*
  detaches the retiring context (remove from `attached` + `disarm_timeline`; its
  committed blocks persist in the store, so the app still shows them) and fires `rotate`;
  the child forks and re-`Attach`es to the **same** track, seeded to the track's *current*
  playhead. The clock never pauses → continuity is free and there is never a beat with two
  producing bindings. This realizes "one playing binding; rotation swaps it" concretely.
- **The seed/carry logic collapses (item 3 falls out of item 1).** No fork-copy of a
  playhead number, no persist-playhead-before-stop, no horizon-race persist to get right.
  But seeding still needs care (sharpened by the gemini review below) — there are **two**
  playheads at attach:
  - **TRACK playhead** (seeded only when *creating* the track's live state):
    `max(get_track(track_id).playhead_tick, max_tick(attaching_ctx))`, fatal on DB error,
    else `ZERO`. NOT the attaching context's `max_tick` alone — a rotation child has no
    blocks (`max_tick`=0) and would rewind the whole lane on a restart re-create. The
    durable `tracks` row + the lane's committed blocks are the track's memory.
  - **Per-context Timeline playhead** (every attach): `max(max_tick(ctx), track.playhead)`.
    `max_tick(ctx)` stops a context with its own committed history (cold-restart re-attach)
    from re-seeding behind its log (`DuplicateBlock`); `track.playhead` puts a fresh child
    at current musical time so the next beat is one `advance_to` step, not a giant catch-up
    from zero.

## Stage 1 review findings (gemini batch, 2026-06-29)

A holistic two-prompt gemini review (whole files: tracks.md + hyoushigi.md + the spine +
beat.rs) ahead of locking the surgery. It **confirmed** the clean parts (the
`wakeup`/`ooda_every` decomposition; cursor + `materialize_failures` + `failure_water`
*must* be per-attachment, not flat on the track; `KJ_EPOCH_NS` latched once per beat;
`KJ_PULSE` per-attachment monotonic; `Stop` clock-only). It surfaced the two seed
corrections folded in above, plus one decision:

- **Rotation handoff = GAP, not OVERLAP (deliberate).** An exogenous clock that doesn't
  pause during a page-turn must pick one: the parent detaches synchronously (a few
  *producerless* beats while the child boots + re-binds — a GAP) or the parent lingers
  until the child binds (two producers double-scheduling into the score — an OVERLAP).
  Stage 1 takes the **gap** (synchronous parent-detach): never two producers, a few rest
  beats during boot. This matches the doc's own "the band played a beat while the bass
  player was unplugging." An **atomic `Swap`/`Rotate` transport command** that lets the
  track briefly suspend its clock across the handoff (closing the gap) is a real future
  option — deferred. `KJ_PULSE`/`KJ_TICK` continuity is unaffected either way (the track
  holds the numbers).

  **Why the gap is safe in practice (Amy, 2026-06-29):** the gap is in *live production*,
  not in *playback*. Hyoushigi stages content **ahead of the playhead** (lead-time
  derivation — `speculate_at = start − beats_for(lead_time)`): the retiring parent already
  speculated/committed the handoff beats' notation *before* it detached, so the committed
  score covers the rotation window. Whatever consumes the score downstream (ABC→MIDI, then
  samples/synth) reads **continuous** notation across the page-turn — the producer gap
  never reaches the DAC. So the rotation gap is a gap in *who's composing next*, not in
  *what's playing now*; the speculation lead is exactly what absorbs it. (This is the
  musician case from `hyoushigi.md` — the clock can't block, so content is staged ahead;
  rotation is just another reason the lead exists.) This makes the atomic `Swap` a polish
  item, not a correctness requirement, for any track whose lead ≥ the child's boot time.

## Stage 1 review follow-ups (landed code, 2026-06-29)

A second kaibo pass on the **landed** code (gemini-batch ×2 + a deepseek agent, whole
files, no diff) confirmed the design (slew guard, the two seeds, borrow/iteration
safety, fork-copy ordering, single-writer persistence all verified correct) and found
a cluster of restart/handoff gaps. **Fixed in the follow-up commit:**

- **`detach` persists the track playhead** — the rotate-horizon handoff is the durable
  record a 0-block child inherits across a crash (the beat path deliberately does not
  persist every beat; `detach` is rare). Without it a crash *in the rotation gap* would
  re-seed the track from a stale row and rewind the lane.
- **`attach` pulls a lagging track up to a joining context's committed frontier** — a
  restart where a *thin* context (no blocks) created the track at a stale tick, then a
  *thick* musician (committed history at tick N) attaches, no longer freezes the
  musician's Timeline behind the track (the forward-only slew guard would otherwise pin
  it for N beats while its OODA kept firing). This also makes the "silent pinned context
  ahead of its track" state unreachable via `attach`.
- **`attach` enforces one-track-per-context** (the `context_track` index is 1:1): moving
  a context to a new track detaches it from the old (live) and deletes the stale
  persisted attachment row, so one track's beat can't inject another's `KJ_*` facts and
  a restart re-attach can't hit the ambiguous `many` case. `detach` guards the timeline
  disarm so a (future) multi-lane context isn't silently killed.
- **`attach` reads the context's `max_tick` once** (was twice) — no half-created
  track-without-attachment state if the second read failed.

**Also fixed (follow-up #2):**

- **stop/pause → play within one beat period no longer double-beats.** Each `TrackState`
  carries a monotonic `generation` bumped by every `play`; heap entries carry the
  generation they were enlisted under, and `fire_due` drops a popped entry whose
  generation is stale. So the pre-stop entry (re-pushed by the last beat) is invalidated
  by `play`'s bump instead of processing alongside `play`'s fresh entry. Normal beats
  re-push under the same generation. (Test: `stop_then_play_within_one_period_beats_once`.)

**Deferred (tracked here, not blocking Stage 1):**

- **`KJ_PULSE` / `beat_count` reset on a kernel restart** (not persisted). This is now
  *documented as the contract*, not a silent gap: a restart re-hydrates the context's
  conversation fresh, so a model never carries a stale pulse across the boundary — the
  reset is consistent with the conversation lifecycle. Full cross-restart durability
  lands with the **cold-start re-arm sweep** (already deferred), which is the right place
  to persist these counters holistically rather than bolting a column on now.
- **stop/pause → play within one beat period double-beats** (a stale heap entry isn't
  dropped before `play` re-enlists the track). Pre-existing shape; real transport ops are
  seconds apart with a beat between (which drains the stale entry), so it's a test-only
  artifact today. Fix = a per-track **generation token** on heap entries (drop a popped
  entry whose generation is stale). Documented at the `play()` call site.
- **Track-scoped `max_tick`** — the track playhead seed uses the *context*'s committed
  high-water, which over-inflates a cross-track re-attach (a context that played track A
  to tick 500 then attaches to a fresh track B seeds B at 500). Harmless in Stage 1
  (a musician is 1:1 with its lane for life) and forward-only (never a rewind); the
  proper fix is a track-scoped tick query, which **is Stage 2** (the track owns its
  score). 
- **Rotation gap is unbounded** if the child's boot/turn is queued behind other work; the
  speculation lead covers the *notation*, not the *production* gap. The atomic `Swap`
  transport command (suspend the clock across the handoff) is the eventual closer.

---

# Stage 2 implementation — the track owns its score

> **Status:** design locked 2026-06-29 (Amy + Claude), not yet coded. This is the
> *living* implementation tracker — a fresh session continues from here, exactly as
> Stage 1 did. **No backwards compat; on `main`.** The two scoping decisions are made
> (below); the work items are TDD and unstarted. Tick a box + add a one-line note when
> an item lands; record fresh decisions under *Decisions made in-flight*.

## The two scoping decisions (made 2026-06-29)

1. **Minimal track-keyed store first** — move the `Timeline` (clock + open future +
   committed log) onto the **track**, and add a **track-scoped block store/query** that
   feeds materialize + `KJ_HEARD` now. The first-class, app-synced/scrubbable track
   *Document* (the "track is like a good instrument" render) is a **later cut** — Stage 2
   lands the kernel-side value (continuity-by-construction, the *real* band-view HEARD,
   one materialize cursor) without the app surface.
2. **Design concurrent producers now — by removing the single-producer assumption, not by
   adding machinery.** See *The concurrent-producer model* — this turns out to be the
   same work as the minimal store, done without baking in "one producer."

## Where the code actually is (verified 2026-06-29, post-Stage-1)

Stage 1 moved the *clock* onto `TrackState` (`beat.rs:60-94`: playhead, beat_count,
transport, heap, generation). The **score is still per-context**:

- **`Kernel.timelines: DashMap<ContextId, SharedTimeline>`** (`kernel.rs:711`) with
  `arm_timeline`/`timeline`/`disarm_timeline` (`kernel.rs:705-740`) — **the central cut
  point.** The score container is keyed by `ContextId`.
- The `Timeline` (`hyoushigi/src/engine.rs:153-612`) holds `playhead`, `future: Vec<Scheduled>`
  (open future), **`committed: Vec<Cell>`** (the durable score, in-RAM stand-in for the
  block log), `cas`, `squashes`, **`failures`**. Public API: `schedule(cell)` (input,
  `:239`), `advance_to`/`pump` (drive), `committed()`/`squashes()`/`failures()` (read),
  `seed_playhead` (virgin re-arm), `materialize(cell, block_id)` (→ `BlockSnapshot`).
- **The data is already track-tagged; only the container is per-context.** `Cell.track`
  (lane identity, required) + `Cell.played_by` (producer principal) — `cell.rs:113-173`,
  in `crates/kaijutsu-hyoushigi/src/cell.rs` (**not** `kaijutsu-types`). `commit()`
  (`engine.rs:493-531`) already stamps every emitted sibling with the committing cell's
  `track` + `played_by` and asserts loudly on divergence. `BlockSnapshot.track` +
  `BlockId.principal_id` carry the same two axes to the block layer (`block.rs:1322-1334`).
- **The Stage-1 bridge** (`beat.rs:778-787`, inside `materialize_one`): each beat slews
  the per-context timeline's playhead to the track playhead. **This is the seam Stage 2
  deletes** (the explicit Stage-1 note, lines 392-394).
- **Per-context materialization bookkeeping** rides each `AttachedContext`
  (`beat.rs:101-124`): `cursor: MaterializeCursor`, `materialize_failures`,
  `failure_water` — it exists *because each context materializes into its own timeline +
  own block store*. Becomes track-scoped (one cursor over the track's score).
- **`KJ_HEARD` is a per-context lie today.** `transport_env` reads
  `self.documents.block_snapshots(ctx)` (`beat.rs:1018`, keyed by `ContextId`) — a single
  context's log — though the comments (`beat.rs:213-214`) already *describe* a cross-track
  band view. The aspiration becomes true once the read is track-scoped.
- **No track-owned or cross-context-by-track aggregation exists** anywhere
  (`block_snapshots(ctx)`/`last_block_id(ctx)` are per-context, `block_store.rs:2097, 824`).
  This is what Stage 2 builds. (It also closes the deferred Stage-1 "track-scoped
  `max_tick`" item above — the seed can finally ask the *track's* high-water, not the
  context's.)

## The core data move

| Today (per-context) | Stage 2 (per-track) |
|---|---|
| `Kernel.timelines: DashMap<ContextId, SharedTimeline>` | `tracks` own the `Timeline` — keyed by `TrackId` (held on/beside `TrackState`) |
| `arm_timeline(ctx)` / `timeline(ctx)` / `disarm_timeline(ctx)` | track-keyed: arm on track-create, read by `TrackId`, disarm on track teardown (not on detach) |
| `schedule_abc_cell(kernel, ctx, …)` routes input to the ctx's timeline | routes to the **track's** timeline (cell already carries `track`+`played_by`) |
| per-`AttachedContext` `cursor`/`materialize_failures`/`failure_water` | **one track-scoped** cursor + failure ledger (the `Timeline.failures()` is already per-timeline → now per-track) |
| materialized blocks land in the **producing context's** block store | land in the **track-scoped** block store (a cell carries no `ContextId`, only `played_by`) |
| `KJ_HEARD` ← `block_snapshots(ctx)` | `KJ_HEARD` ← **track-scoped** read (the real band view, spanning the rotation chain) |
| Stage-1 bridge slew (`beat.rs:778-787`) | **deleted** — the playhead lives on the track; nothing to slave |

## The concurrent-producer model (the load-bearing design)

The store is built so **N producing contexts can contribute to one track's score**, and
this needs *almost no new machinery* — hyoushigi's existing invariants already make it
correct. Music keeps "one producing binding at a time" as a **loadout policy** (two bass
phrases on one beat clash), *not* a structural assumption — the kernel never forbids
concurrency; the music rc just doesn't spawn it (Stance: ergonomic nudge, not enforcement).

- **No conflict at the same tick.** Two producers committing at the same `Span` produce
  two distinct cells, each with its own `played_by`; the shared-coordinate doctrine
  already allows ties (`BlockId` is the unique row). Both copies persist with provenance —
  the doc's "copies + back-reference make several contexts contributing *tractable*."
- **The write barrier already holds per-cell**, not per-track-tick. A committing at tick N
  never rewrites B's committed cell at tick N — different cells. N producers don't threaten
  "never rewrite a committed cell."
- **Speculation isolation is already structural** — a resolver only ever gets the
  *committed* view, so A can't read B's uncommitted speculation; A *can* read B's committed
  cells (the payoff — A composes against what B actually landed).
- **Misprediction handles it for free** — B committing inside A's speculate→commit window
  shifts A's `compute_basis` → A squashes and re-speculates against B's contribution; the
  `Squashed` ledger records it.
- **The genuinely new mechanisms (small, but NOT zero — the deepseek review corrected the
  "almost no machinery" claim):**
  1. **The per-track `Timeline`'s lock is the sequencer** — the per-track analog of the
     per-context mailbox-as-sequencer (preserves "the kernel is the sole timeline
     sequencer" with N producers; they queue at the track's lock).
  2. **`FailureEvent` gains `played_by`, and failure-draining becomes per-context.** This
     is the one *correctness* addition the review found: `FailureEvent` (`engine.rs:84-93`)
     carries no producer identity today, and `drain_failures` (`beat.rs:919-990`) drains
     *all* new failures into the *draining* context's conversation and advances one
     `failure_water`. With N producers on one shared track timeline that **misattributes**
     — producer B's resolve failures surface in producer A's conversation and B never sees
     its own. So concurrent producers require: stamp `FailureEvent.played_by` from the
     failing cell at construction (`engine.rs:396-401`), and make `drain_failures` filter
     to the draining context's producer. (See WI 3.)
- **`UseLastGood`/`last_committed_content_in` stays *lane*-scoped** (the track's last
  committed cell, regardless of producer) — for music the listener hears the lane continue;
  a `played_by`-scoped "repeat MY last" is a future refinement if a use case needs it. This
  is a *behavioral change* from the single-producer baseline (A's fallback may now repeat
  B's last phrase) — a decision, not a bug.

## Work items (TDD throughout — tests that can and will fail)

- [x] **1. Move the `Timeline` registry onto the track.** `Kernel.timelines`
  (`kernel.rs:711`) → `TrackId`-keyed (or hang the `SharedTimeline` on `TrackState`).
  `arm_timeline`/`disarm_timeline` become **track**-keyed (`arm_track_timeline(track_id)` on
  track create / first attach; `disarm_track_timeline(track_id)` on **track teardown**,
  *not* on context detach). **`attach` must stop calling `arm_timeline(context_id)`**
  (`beat.rs:434`) — that per-context arm is a Stage-1 artifact; a score context just joins
  the track's `attached` set, the (already-armed) track timeline is the score target.
  `timeline(ctx)` either goes away or becomes a convenience that does ctx→track→timeline.
  Per-context timelines for **non-track** contexts (coders) are unaffected. **Drop timeline
  cloning on fork (gemini):** `insert_forked_context` clones the per-context timeline today —
  with the timeline on the track, a fork of an attached context must NOT clone it; the child
  re-attaches and becomes a **co-producer on the existing shared track timeline** (cloning
  would give it a disconnected open future, breaking one-timeline-per-track). *Tests:* a
  track's timeline survives a detach; two tracks have independent timelines; rotation
  (parent detach → child attach, same track) keeps one continuous timeline (no re-arm, no
  seed dance); a freshly-attached score context creates **no** per-context timeline; a fork
  of an attached context shares (does not clone) the track timeline.
- [x] **2. Re-point score input.** `schedule_abc_cell` (`mod.rs:320`) + its caller
  `on_turn_completed` (`beat.rs:1106-1168`) schedule into the **track's** timeline by the
  ctx→track index; cell keeps `track`+`played_by` so provenance survives. *Tests:* a cell
  scheduled by ctx A then committed *after* A detaches still lands in the track score
  (rotation hand-off); two producers' cells coexist in one track's committed log,
  distinguished by `played_by`.
- [x] **3. Track-scoped materialize, but per-context error surfacing.** The score
  **materialize** runs **once per track** (one cursor over the track's committed cells →
  the track-scoped block store), retiring the per-`AttachedContext` `cursor`. **But error
  surfacing stays per-context** — the deepseek review's key structural correction: the
  `failures: Vec<FailureEvent>` *ledger* lives on the shared (per-track) timeline, yet
  *draining* it (the `failure_water` cursor + inserting Error blocks, and the poison-skip
  Error path `beat.rs:869-901`) must land in the **producing** context's conversation so a
  model reads its *own* failures next turn. Concretely: (a) add `played_by` to
  `FailureEvent` (`engine.rs:84-93`), stamped from the failing cell (`engine.rs:396-401`);
  (b) materialize once per track; (c) a per-context loop drains failures **filtered by
  `played_by`**, advancing a per-context water mark. *Needs:* a `ContextId → PrincipalId`
  (the context's producer) mapping reachable from the drain — simplest is to store the
  producer `played_by` on `AttachedContext` at attach. **Hoist materialize out of the
  per-context loop (gemini):** `process_track` today loops `for each attached ctx →
  materialize_one`; with a track-scoped cursor that would **double/triple-emit** per attached
  context. Restructure: run `materialize_track(track_id)` **exactly once per beat**, then
  iterate `attached` only for per-context cadence (rotate / phrase / ooda wake) + the
  per-context failure drain. *Tests:* two producers materialize through one cursor with no
  double-emit; a track with 3 attachments materializes each committed cell **once** per beat;
  producer B's resolve failure surfaces in **B's** conversation, never A's; poison-skip is
  attributed to the cell's `played_by`.
- [x] **4. The per-track score context (DECIDED: option C — do this FIRST, it gates 1–3).**
  Create a durable **score context** when a track is created; its Conversation document holds
  the materialized score. Because it's a **real `ContextId`**, *all* the per-context block APIs
  are reused unchanged — `materialize_committed` keeps taking a `ContextId`, just the **score
  context's**, not the producer's: `reserve_block_id(score_ctx, played_by)` (per-`(score_ctx,
  principal)` seqs — the per-`(track,principal)` the review wanted, real and restart-seeded by
  the existing block-log seeding), `insert_from_snapshot_as(score_ctx, …)`,
  `last_block_id(score_ctx)` as the `after` anchor, `block_snapshots(score_ctx)` /
  `max_tick(score_ctx)`. **No `TrackId` block-store API, no `kaijutsu-index`/RPC ripple** — the
  (C) payoff (the per-`ContextId` index trait at `lib.rs:74` already works for the score ctx).
  Store the score `ContextId` on `TrackState` + the persisted `tracks` row. The score context
  must be a **non-producer** (no rc lifecycle, no turn, no LLM hydration — a kind/flag). The
  **Error-block insert stays the PRODUCER's `ContextId`** (errors go to the producer's
  conversation, per WI 3), NOT the score context. Closes the deferred **track-scoped
  `max_tick`**: `attach`'s seed reads `max_tick(score_ctx)` (`beat.rs:393-397, 415`) instead of
  the producer's. *Tests:* creating a track creates its score context (+ KernelDb row);
  materialized blocks land in the score context, not the producer's; two producers' cells share
  the score context distinguished by `played_by`/`BlockId`; the score context is flagged
  non-producer (never armed for a turn); legacy `track == None` blocks match no track.
- [x] **5. Re-point `KJ_HEARD` at the track score** (`beat.rs:1013-1021`). The band view
  becomes real — a producer hears the whole lane (all producers, across rotations) within
  the window. *Test:* after a rotation, the child's `KJ_HEARD` includes the retired
  parent's committed phrases; two concurrent producers each see the other's committed cells.
- [x] **6. Delete the Stage-1 bridge** (`beat.rs:778-787`) and the per-context seed/slew
  it serves. The playhead lives only on the track. *Test:* the existing continuity tests
  (re-pointed from Stage 1's "the clock stayed on the track") pass with the bridge gone.
- [x] **7. Persistence — `UseLastGood` survives a restart (approach b, chosen 2026-06-29).**
  Rather than a separate cell-log table, the committed log is **reconstructed from the score
  context's materialized ABC blocks on (re-)arm** (`reconstruct_score_cells` in `beat.rs`): the
  ABC *source* blocks ARE the committed Concrete cells (the MIDI sibling is derived, never
  committed), and a content-derived `ContentRef::of(content)` hash matches the bytes already in
  durable CAS. `engine.rs` gained `rehydrate_committed(cells)` (virgin-only; crash over
  corrupting a live log); `attach`'s create block rehydrates the track timeline and starts the
  materialize cursor **past** the restored cells so they're never re-emitted. gemini called
  reconstruction "lossy" (drops `CellState`/`Recipe`/`Span`), but those are all trivially
  reconstructable for *committed Concrete* cells, which is exactly what `UseLastGood` needs — so
  (b) avoids the new schema + blob-growth of (a). **Open future / squashes / in-flight
  speculations still do NOT survive restart** (consistent with conversation re-hydration) — the
  documented contract. *Test:* `attach_rehydrates_committed_from_persisted_score` — attach off a
  persisted `tracks` row recovers the score context, rehydrates the prior ABC phrase into
  `committed`, excludes the MIDI sibling, and a beat does not re-materialize it (cursor past it).
- [x] **8. Docs** — devlog entry landed ("Tracks Stage 2 — the score moves onto the
  track", 2026-06-29); these boxes flipped; `hyoushigi.md` already carries a forward
  "Direction (2026-06-29)" note pointing here for the stage 1–2 move.
- [x] **9. Test audit (the review's risk finding).** Done: the `beat.rs` tests were
  migrated to the track model (marker tests pre-arm the *track* timeline + read the score
  context; failure/poison tests schedule onto the track timeline); `second_context` became a
  real two-producers-share-one-score test; and `two_producers_failures_route_to_their_own_conversations`
  pins the concurrent-producer mechanism. Note: the at-risk `hyoushigi/mod.rs` tests stayed
  green *unchanged* — `materialize_committed` kept its `ContextId` signature (the re-point
  routes the score ctx from `beat.rs`), so those isolation tests still exercise the bridge
  correctly. Original risk note kept below for the record.
  <details><summary>(original at-risk list)</summary>
  The suite was deeply per-context
  coupled, so many tests would **keep passing while exercising the wrong path** after the
  cut — at-risk: `hyoushigi/mod.rs` tests `bridge_materializes_committed_cell_into_block_and_cas`
  (`:877`), `materialize_after_reload_mints_fresh_seq_no_duplicate` (`:945`),
  `score_cell_commits_abc_and_derives_midi_sibling` (`:1100`),
  `materialize_after_restart_does_not_collide` (`:1275`), `partial_insert_resumes_per_artifact`
  (`:1655`), `arm_then_lookup_returns_some…` (`:1700`), `disarm_removes_the_timeline`
  (`:1749`). For each, add a **track-scoped** variant that exercises the new path. Add the
  **concurrent-producer** test: two contexts schedule into one track timeline; both cells
  materialize once into the track store; both appear in track-scoped `KJ_HEARD`; a failure
  from one producer surfaces **only** in that producer's Error blocks. **Specific silent-pass
  trap (gemini):** any test asserting on `block_snapshots(ctx).len()` to confirm a note
  materialized will break/mislead once the score lands in the *track* store — grep them out
  and re-point at the track-scoped query.
  </details>

## Status — increments landed (2026-06-29)

- **Increment 1 (`9aa280f4`):** `TrackId`-keyed timeline registry on the Kernel.
- **Increment 2 (`9aa280f4`):** the per-track **score context** (option C) — DB column +
  set-once persist + `ensure_score_context` mint/recover, wired into `attach`.
- **Prereq (`e446bbe4`):** `FailureEvent.played_by`.
- **Increment 3 — the breaking re-point — DONE + green (this commit):** WI 1, 2, 3, 4, 5, 6 all
  landed. `schedule_abc_cell` + materialize route to the track timeline + score context;
  materialize is **hoisted once-per-beat** out of the per-context loop; the cursor +
  failure-water live on `TrackState`; the failure ledger drains **per producer** (routed by
  `played_by`, single-producer fallback, orphan→score ctx); `KJ_HEARD` reads the score context
  (the real band view); the Stage-1 per-context bridge slew is **deleted** (the track timeline's
  `advance_to` is the legit pump); `attach` arms the track timeline (no per-ctx arm) and `detach`
  no longer disarms it. 37 beat + 1266 kernel tests green; full workspace builds.
- **WI 7 (`UseLastGood` restart) + WI 8 (docs) + WI 9 (test audit) — DONE.** WI 7 took approach
  (b): reconstruct the committed log from the score context's ABC blocks on arm + a virgin
  `rehydrate_committed`, cursor started past the restored cells. Devlog entry landed; beat tests
  migrated; concurrent-producer + rehydrate tests added.
- **All Stage 2 work items (1–9) are DONE.** Still ahead: **Stage 3** (`ClockSource` trait + MIDI
  driver + cross-track bar/beat alignment) and a **real-kernel live-verify** (rebuild + restart;
  the `tracks` table picks up `score_context_id` via the additive ALTER — should migrate clean).

## Decisions made in-flight

- **WI 1–4 are ONE coherent cut, not independently green (found 2026-06-29, first code
  pass).** `materialize_committed` (`mod.rs:586`) uses `context_id` for *three* block-store
  calls — `last_block_id` (`:597`), `reserve_block_id` (`:685`), `insert_from_snapshot_as`
  (`:718`). The moment the timeline becomes track-keyed (WI 1), a materialized cell has no
  `ContextId` to write to (it carries only `track` + `played_by`), so the block store MUST be
  track-keyed in the same change (WI 4). Schedule (WI 2) and the materialize hoist (WI 3) ride
  along. **Real dependency order: WI 4 (track-keyed block container) → WI 1 (timeline
  registry) → WI 2/3 (re-point) → WI 5/6.** The 8-item numbering is a checklist, not a landing
  order.
- **WI 4 container — DECIDED 2026-06-29 (Amy): (C) a real per-track "score context."** When a
  track is created, create a durable backing context whose Conversation document **is** the
  track's score; producers materialize into it. *Why (C) over the alternatives:* it reuses the
  entire per-context block machinery via a **real `ContextId`** (so `reserve_block_id`/
  `insert_from_snapshot_as`/`last_block_id`/`block_snapshots`/`max_tick` and per-`(ctx,principal)`
  seqs all work unchanged — no TrackId block-store API, no `kaijutsu-index`/RPC ripple), it
  satisfies `handle_implies_row` (no synthetic id), and it embodies the design's own thesis —
  *the track persists, producers come and go* — the score context is the durable thing they
  rotate around, and it's browsable as the track's score. (Rejected: (A) synthetic ContextId —
  fake-id leak + `handle_implies_row` violation; (B) parallel `track_documents` store — most new
  code + the index/RPC ripple.) **Consequence:** the score context is *not a conversational
  producer* — it must NOT run rc lifecycle / take turns / hydrate to an LLM; it is a score
  holder (a new `context_type`/kind or a "no-OODA, no-hydrate" flag). `TrackState` (and the
  persisted `tracks` row) gains the score `ContextId` so materialize/HEARD/seed resolve it.
  This partly delivers the deferred "first-class track Document" for free (the score context
  renders like any context) — fine, just not required; keep app surface out of Stage 2.
- **No literal timeline-clone on fork (corrects the gemini finding).** There is no
  timeline-cloning code in `insert_forked_context`/`lifecycle.rs`; the fork child gets its own
  per-context timeline because its `attach` calls `arm_timeline(context_id)` (`beat.rs:434`).
  So "drop the fork clone" = the same fix as "attach stops arming a per-context timeline" — arm
  once per track, attach joins. (WI 1.)
- **Score contexts are APP-VIEWABLE (Amy, 2026-06-29), not hidden.** They're real `Live`
  contexts (`context_type="score"`, label `score-<track>`) that appear in `kj context list` and
  the app — the "first-class Document for free" upside of (C). "Non-producer" means only that
  they never take a turn or hydrate to a model, NOT that they're hidden. (No app-side work is in
  scope now; viewability is a property the kernel-side mint preserves.)
- **Foundation step (additive, safe): the Kernel gains a `TrackId`-keyed timeline registry**
  (`arm_track_timeline`/`track_timeline`/`disarm_track_timeline`) ALONGSIDE the per-context one,
  so coders keep their per-context timelines and the re-point can proceed incrementally before
  the per-context score path is deleted.

## Open questions carried into Stage 2 coding

- **The first-class track Document** (app-synced, scrubbable in the time-well) is
  deliberately out of this cut. Where the minimal track-keyed store and that Document meet
  (is the store *the* Document's backing, or a separate kernel structure the Document later
  reads?) is decided when the render lands, not now — but item 4 should not paint it into a
  corner (prefer a shape a Document can later wrap).
- **Score ↔ conversation seam.** Stage 2 keeps the model's *turn* (its ABC output block)
  in the producing context's conversation and moves the committed *score* to the track;
  `KJ_HEARD` is the context's window onto the track score. Whether the hydration-marker
  machinery should also re-point at the track (so a re-hydrated musician reads the track
  score directly, not just via `KJ_HEARD`) is left for when it bites.
- **Concurrent-producer ordering at the exact same tick** is by insertion order at the
  track lock (shared-coordinate ties; `order_key` successor-derived). Deterministic per-run,
  **not across runs** (depends which producer takes the lock first); `BlockId` is the unique
  row so correctness holds regardless. If a use case needs deterministic cross-producer
  ordering, revisit then.

## Stage 2 review findings (deepseek, 2026-06-29 — pre-coding)

A holistic whole-file review (engine.rs/cell.rs/beat.rs/hyoushigi mod.rs + the docs, no diff)
ahead of coding. It **validated both scoping decisions** and tested the concurrent-producer
claim (a)-(d) individually against the code: all four hold — the per-cell write barrier
(`engine.rs:480-495`, append-only `commit`), speculation isolation (`CommittedCtx` borrows
only the committed slice, `engine.rs:617-621`), ties-by-`played_by` (→ `BlockId.principal_id`,
`mod.rs:685`), and squash-on-co-commit are all structurally sound. It also confirmed the
clean parts: the per-track lock as a natural extension of the existing per-context lock; the
gap-not-overlap rotation with the track timeline surviving detach; the `HeardEntry` struct
already carrying `track` so the band view is structurally ready; the materialize cursor logic
being *simpler* with one cursor per track.

The findings (folded into the claim + work items above):

1. **🔴 `FailureEvent` has no producer attribution** — the one correctness *bug*. Folded into
   the concurrent-producer model + WI 3 (add `played_by`; per-context filtered draining).
2. **Materialize-once-per-track vs. per-context error surfacing** — folded into WI 3 (the
   "one failure ledger per track" language was misleading; the ledger is per-track, the
   *draining* is per-context).
3. **block-store API surface understated** — folded into WI 4 (enumerated the TrackId-keyed
   variants + the `kaijutsu-index`/RPC ripple; Error insert stays per-context).
4. **`arm/disarm_timeline` + `attach` per-ctx arm** — folded into WI 1.
5. **`seed_playhead` virgin-only vs. re-derivation** — folded into WI 7 (rehydration ctor).
6. **`attach` seed still `max_tick(ctx)`** — folded into WI 4 (→ `max_tick_track`).
7. **Test suite silently passes on the wrong path** — became WI 9 (test audit).

**Document, not fix (sound, but worth stating so the design doesn't mislead):**

- **The squash / "misprediction handles it for free" path is structurally correct but
  *dormant* for today's only resolver — and that is *correct musical behavior*, not a gap.**
  Both reviewers (deepseek + gemini, independently) flagged this: `CasCommitResolver::compute_basis`
  (`mod.rs:402-408`) hashes only the recipe's `hash` param — it never reads the committed view —
  so a co-producer's commit can't change its basis and it never squashes under concurrency.
  **Gemini's reframe:** for absolute notation that is exactly right — if two players land a note
  at the same tick you want *both* to commit and play (a chord/clash), not for one to cancel and
  re-evaluate the other. So claim (d)'s machinery is ready but should NOT be expected to fire for
  concurrent `cas_commit` producers; both committing at one tick is the feature. (Squash only
  re-activates for a future resolver whose basis depends on committed state.) **Action: the doc
  must not imply concurrent producers squash each other — they coexist.**

## Stage 2 review findings (gemini-pro batch, 2026-06-29 — pre-coding, second voice)

A gemini-pro batch pass (succeeded on batch capacity while interactive gemini was 503-overloaded
— *batch is the resilient path for gemini-pro*). It **independently endorsed the design**
("locked and solid; proceed exactly as planned"), confirmed the parts deepseek did (per-cell
barrier, the synchronous `parking_lot::Mutex` sequencer with **no `.await` under the lock →
deadlock-impossible**, lane-scoped `UseLastGood` with `fire_fallback` correctly authoring repeats
as `PrincipalId::beat()` so no forged provenance), and **converged** on the squash-dormancy above.
It found **three new items** beyond deepseek's set (folded into the work items):

1. **🟠 Restart re-derivation from blocks is LOSSY → persist the Cell log (sharpens WI 7).**
   `materialize` (→ `BlockSnapshot`) drops `CellState`, the `Recipe`, and full `Span`. If a track
   re-arms with an empty `committed` Vec (or an imperfect `BlockSnapshot → Cell` round-trip),
   `last_committed_content_in` finds nothing and the **next `UseLastGood` fallback wedges the
   track**. Resolution: do NOT reverse-engineer `Vec<Cell>` from blocks — persist the timeline's
   own append-only Cell log (or at least `Concrete` cells with full `Span` + `played_by`) to a
   track-scoped row, loaded at arm. (Folded into WI 7 below.)
2. **🟠 Hoist materialize OUT of the per-context loop (sharpens WI 3).** `process_track` today
   loops `for each attached ctx → materialize_one`. Making the cursor track-scoped *without*
   restructuring would **double/triple-emit** per attached context per beat. Run
   `materialize_track` exactly ONCE per beat, then iterate `attached` only for per-context cadence
   (rotate / phrase / ooda wake). (Folded into WI 3.)
3. **🔴 Drop timeline cloning on fork (new — WI 1).** `insert_forked_context` clones the
   per-context timeline today. With the timeline on the track, a fork of an attached context must
   NOT clone it — a fork becomes a **new co-producer joining the existing shared track timeline**.
   Cloning would give the child a disconnected open future, violating one-timeline-per-track.
   (Folded into WI 1.)

Plus it sharpened **WI 4**: the CRDT `after` anchor (`last_block_id`) and `reserve_block_id` must
*both* go track-scoped, and the per-`(TrackId, PrincipalId)` sequence must be **seeded from the
track-scoped log on cold start** or interleaved producers collide `BlockId`s / scramble
`order_key` across restarts. And **WI 5/9**: re-pointing `KJ_HEARD` removes the score from the
*context's* block store, so any test asserting on `block_snapshots(ctx).len()` silently breaks —
and the score↔conversation hydration seam (open question) must be settled before a re-hydrated
musician can read its own past performance (note: the score blocks are already `ephemeral`/
hydration-silent today, so the model's memory already flows *only* via `KJ_HEARD` — re-pointing
HEARD at the track keeps that intact; the seam question is whether hydration should *also* read
the track store directly).
