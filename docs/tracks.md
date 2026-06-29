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
   alignment between independent clocks.

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
