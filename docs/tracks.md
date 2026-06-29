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
| `KJ_PULSE` | a per-attachment monotonic counter | the reliable ordering key |
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
