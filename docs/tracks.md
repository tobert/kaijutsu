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
    score:       Timeline,            // the committed cells — OWNED BY THE TRACK
    attached:    Map<ContextId, Attachment>,
}

Attachment {
    wakeup:  Cadence,        // wake THIS context every N beats/bars (its own divisor)
    rotate:  Option<Cadence>,// self-fork page-turn cadence; None = never auto-rotate
    // (role/behavior is the context's own tick rc — see "Behavior on a beat")
}
```

### 1. The track owns the score

The committed cells — the *score* — belong to the track, not the ephemeral
player. The score is the thing that persists while players rotate through the
chair; `Cell` already carries a track lane, and the lane is already "the durable
fork-lineage identity" (`chameleon.md`). Moving the `Timeline` itself onto the
track makes that real: **contexts produce *into* the track's score**, they don't
each own a private one. *Like code or a good instrument — it persists, the people
come and go, each leaving a mark.*

This is the largest part of the surgery (the `Timeline` is per-context
everywhere today). It **can be staged** — see *Staging*.

### 2. Contexts attach with a per-context wakeup cadence

Attachment is not "fire everyone every beat." Each attachment carries its **own
wakeup divisor**, so one track can wake:

- the **musician** every 8 or 16 bars (play a phrase), and
- the **conductor** every 64 bars to check in — and the conductor *can take its
  time* manipulating the track and the other contexts' inputs between wakeups.

This generalises today's per-context `ooda_every` cadence and moves its ownership
onto the track (the track knows when to wake each attached context).

### 3. Rotation cadence is per-attachment, owned by the track

The self-fork page-turn cadence (today `rotate_every_phrases` on `BeatState`)
becomes a field of the **attachment**, owned by the track. Rotation = the track
**rebinds** the attachment from the retiring context to its fresh fork — the
clock and score never pause, so there is no race, no carry, no `track_head`.

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

- **stop = stop the clock** for the lane. Rotation (the page-turn automation) is
  *suspended*, not cleared — the attachment's `rotate` cadence is remembered.
- **play = start broadcast + start the clock.** Whatever automation was in place
  resumes — including rotation. (An explicit `kj transport rotate off` stays the
  separate "permanently stop turning pages" knob.)
- the **horizon race** that plagued the per-context design can't occur: a
  page-turn just rebinds the attachment on a clock that's either running or
  stopped; there's no second per-context entity to arm late.

Surface convention: name the lane directly (`--track <name>`); kaish does
context→track lookups on the fly, so `kj` stays crisp.

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
   attachment set (wakeup + rotate cadence). Contexts attach/detach. This alone
   retires the playhead carry and `track_head`, gives native `stop/play --track`,
   and kills the horizon race. The score can stay per-context-but-track-tagged for
   this stage.
2. **Move the score (`Timeline`) onto the track.** Contexts produce into the
   track's score; retired players' marks persist on the lane. Larger, touches
   every `Timeline` consumer.
3. **Generalise the clock source.** Land the `ClockSource` trait + MIDI driver +
   the external-signal seam.

The playhead-carry code (shipped 2026-06-29) is a stage-0 stepping stone: it
encodes the invariant (musical time is continuous across the chain) and its tests
describe the behaviour we want; when stage 1 lands, "the clock stayed on the
track" replaces "copy the number," same observable, less machinery.

## Open questions (for the implementation session)

- **Attachment multiplicity & roles.** Music keeps one *playing* attachment per
  track (rotation swaps it); metrics has N. Is "the player" vs "an observer"
  (e.g. the conductor, a user) a flag on the attachment, or just its wakeup/rotate
  shape? How do multiple producing attachments share one score without clobbering?
- **Score ownership vs context conversation.** The track owns the score; a context
  still has its own conversation/hydration. Where's the seam (does a musician's
  conversation *window* onto the track score, à la `$HEARD`)?
- **Clock source trait shape** + whether the app's existing time-sync algorithms
  are the right substrate for the MIDI driver.
- **Transport scope above the track** — a "band" (all tracks) stop/play, and the
  "cascading control planes" that implies. Deferred deliberately.
- **Migration / compatibility** — the `beat_state` table, `BeatCommand`, and
  `kj transport` all assume per-context; map each onto the track entity.
