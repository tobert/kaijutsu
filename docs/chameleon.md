# Chameleon — models playing to the beat

> **Living document.** This is how the band works *right now* — the
> load-bearing facts a new player or session needs, kept current as the
> instrument grows. Code is truth: when this doc and the kernel disagree, the
> doc is wrong; fix it. Last rework 2026-08-16 (tempo map + jam plan).
> Companions: `docs/tracks.md` (the track substrate), `docs/midi.md` (clock
> doctrine + the wire), `docs/pcm.md` (samples on the same seam),
> `docs/midi-next.md` (device profiles + `kj midi`), `docs/hyoushigi.md` (the
> `Cell`/timeline primitive).

## The instrument in one paragraph

Chameleon is the music application: **contexts play music to a beat the kernel
owns.** The beat belongs to the **track** — a named clock domain with a clock
source, a score, and attached contexts (`docs/tracks.md`) — not to any player.
Players attach, play, and rotate while the track persists. Players exchange
**ABC notation**; the kernel commits it to the track's score; a **sink near the
hardware renders** it to MIDI or samples. Nobody chases a clock — the kernel
models it (see Tempo). The degraded mode is the musical mode: an empty phrase
resolves to **silence** and a dropped player to **`UseLastGood`** (repeat the
lane's last phrase) — vamp insurance, so the system sounds right even when a
player contributes nothing. The house first-loop is Herbie Hancock's *Chameleon*
vamp: B♭ Dorian, B♭m7–E♭7 — a two-chord vamp where repeating the last bar is
musically indistinguishable from the job.

## Tracks & transport (as built)

`kj transport list` shows the live truth: track, state (live/dormant), clock
kind, BPM, phrase length, attachments, playhead, score context. Tracks persist
across restarts; **dormant** = in the DB, nothing re-attached this session. A
track's score lives in a durable **score context** (`score-<track>`) — a real
context that renders like any other but never takes a turn or hydrates to a
model.

The full surface is `kj transport`:

- `attach` / `detach` — the context announces itself on a track (below).
- `play` / `pause` / `stop` — MIDI idiom: stop = stop the clock only; rotation
  is suspended-not-cleared, OODA arm untouched.
- `tempo <bpm>` — set the beat period (system clock).
- `ooda on|off` — arm/disarm one attached context's turn loop, without touching
  the clock.
- `clock system|modeled` — switch the beat driver (see Tempo).
- `rotate` — set/clear the self-fork page-turn cadence.
- `delete` — a rename-aside tombstone; the score context is left untouched and
  a re-attach starts a brand-new track, never resurrects the old one.

**Attaching is the whole trick.** A context becomes a beat participant exactly
when its rc attaches it: `kj transport attach` (no flags) targets the current
context, derives the track from its label, and arms **stopped** + OODA-armed
(no surprise token spend — `play` starts the clock). This used to be a kernel
special-case on `context_type == "musician"`; now it is rc, so any role — a
bassist, a lyricist in time with the music, a probe — is a beat participant
exactly when its `create/` rc attaches it. No kernel edit.

## Tempo — the clock story

Doctrine, from `docs/midi.md`: **a clock you don't own drifts — model it, never
chase it.** The kernel runs a tight *local* clock per track; an external master
is observed and modeled, and only low-rate *references* (never pulses) cross
the wire. And the one timebase: every cue's `at` derives from the **scheduled**
beat grid, never a wakeup wallclock (`docs/midi.md` "The relative-lead timebase,
analyzed"; `docs/pcm.md` "Timing rides the one timebase").

Two drivers, switchable per track with `kj transport clock --track <t>`:

- **`system`** — a local fixed-tempo timer (`SystemClock`, `now + period`).
  Set it with `kj transport tempo --track <t> <bpm>`. Everything starts here.
- **`modeled`** — phase-locked to an observed external MIDI master
  (`ModeledClock`, `docs/midi.md` M3, landed 2026-07-06). An edge observer (the
  app's "ear") learns the master's tempo + phase + drift and ships low-rate
  `ClockEstimate` references; the track free-runs at its last tempo until the
  first reference arrives, then **fires on the master's integer beats** with
  slew-limited corrections, a loud starvation warn if references go quiet, and
  a 5 s stale-drop on the receiving end. The current period carries over on the
  switch; `kj transport tempo` while slaved is an honest manual nudge — the
  master then re-corrects.

**How the rack's tempo reaches the kernel — the map (verified 2026-08-16):**

| Path | Status | Evidence |
|---|---|---|
| Read it off the gear → `kj transport tempo --track <t> <bpm>` | **WORKS TODAY** | system clock, shipped since 2026-06-30 |
| Record a few bars → `kj audio beats <file>` → set the measured BPM | **WORKS TODAY** | Beat This! (ISMIR 2024) via the pure-Rust `beat-this` crate; models in `~/.local/share/kaijutsu/models/beat-this/`; verified live (120 BPM click → `bpm=120.0`) |
| Live MIDI clock-in: rack → app ear → estimator → RPC → `modeled` track | **BUILT — NEEDS A WIRE** | `kaijutsu_audio::clockin::ClockEstimator` (`crates/kaijutsu-audio/src/clockin.rs` — EMA tempo, phase-exact pulse counting, dropout recount, stall flags); app pre-ring tap + `ship_clock_estimates` (`kaijutsu-app/src/midi_in.rs:306`); kernel `BeatRequest::ClockEstimate` (`kaijutsu-server/src/beat.rs:2423`, track resolved by the seat's attachment) → `ModeledClock::apply_estimate` (`clock.rs:154`) |

The missing wire for live clock-in is **operational, not code**: the app must
run on the box that owns the rack's USB (moltar), its session attached to a
`modeled` track, and the master must send MIDI clock on the bus the app hears.
zorak itself cannot hear MIDI today — no reachable `/dev/snd` (`aconnect -l` →
permission denied, `amidi -l` → no sound card, PipeWire down, TiMidity service
inactive). moltar is reachable on the tailnet (sub-ms ping) but its app status
is unverifiable from zorak. The `ear` track (modeled, 338 BPM, dormant) is
evidence the path was exercised at least once — provenance unconfirmed.

Until the live wire is proven, the jam procedure is: **read the tempo off the
gear, set it on the track, and optionally verify with `kj audio beats`** on any
recording of the rack. Measure, don't receive — the modeled lock is a bonus,
not a prerequisite.

## Players — a context_type is an rc bundle

A player's whole behavior is rc (`assets/defaults/rc/musician/`):

- **create/S20-arm.kai** — `kj transport attach`: the entry into the transport.
- **create/S30-hydrate.kai** — hydration window 16 (`kj context hydrate
  --window 16`): the cost guard. Turns hydrate `[0, marker] ∪ last-N`; the
  prefix stays byte-stable for prompt caching; a player's log can grow forever
  at tempo without unbounded per-turn cost.
- **tick/S10-drive.kai** — the OODA hook: fires `kj drive --prompt` with the
  **transport report** each cadence (default: every 8 phrases of 16 beats).
  The kernel seeds `KJ_TICK`, `KJ_PHRASE`, `KJ_TEMPO`, `KJ_HEARD` (the last 8
  phrases of committed notation, all tracks, as a JSON string — the only
  channel that shows a player what was just played, since score blocks are
  hydration-silent), plus `KJ_PULSE`, `KJ_EPOCH_NS`, `KJ_PHRASE_BEATS`
  (`beat.rs::transport_vars`/`heard_json`). `kj drive --prompt` writes the
  report as a real User block, so it hydrates as the fresh turn. Turns are
  therefore **launch-quantized by construction**: they fire on the grid, never
  on demand, and the player composes what *sounds next* — the loop is
  anticipatory, and the vamp covers any slow turn.
- **rotate/S10-rotate.kai** — the page-turn: on the phrase horizon the
  scheduler stops the parent synchronously (Rust — can't race the beat), then
  rc runs `kj fork --preset spawn --switch && kj transport attach &&
  kj transport play`. The child inherits the attachment (track + cadence);
  **fork-lineage IS song form** — each thin fork is a section/movement, drawn
  natively by the time-well. Producer rc edits are horizon-latched: they land
  at the player's next page-turn, never mid-phrase.

Chairs are deeper bundles: `bassist` adds create/S05-chair.md — the voice
(register, groove, note choices), injected into the system prompt. The chair
names the *role*; who sits in it (which model) is a runtime choice. The
original casting — a small local model on bass, Haiku drums, Sonnet keys, Opus
booth, Fable vocals — is the design's first voice, not today's roster. ABC-only
output (no tool calls) is the ideal player UX: the symbolic decisions made the
player role exactly the shape small models are good at.

## The score & the sound

- **Notation is the score; MIDI is a render of it.** Committed cells are
  `text/vnd.abc`; the sink renders ABC→MIDI *at the sink*
  (`kaijutsu-app/src/midi.rs`), scheduling into its local ALSA queue at
  `receipt + lead` — the speculation lead is the jitter buffer, and intra-phrase
  timing is sub-ms off one anchor.
- **Phrases, not bars, in the kernel**: `beats_per_phrase` on the track policy
  (16 or 32 in practice); barlines are a notation/human affordance translated at
  the edge.
- **Fallback**: `UseLastGood` per track = the vamp insurance; an empty track
  resolves to `Skip` (silence until the first good phrase).
- **Samples ride the same seam**: a clip cell
  (`application/vnd.kaijutsu.clip+json`, `kj play --track`) renders like ABC
  through the same `RenderCue` (`docs/pcm.md`). The mime IS the dispatch key.
- **Hearing** (M2, landed 2026-07-06): the app's ear captures incoming MIDI,
  stamps it with ALSA receipt time, and batches it to the kernel as score
  blocks (telemetry, not realtime). Device knowledge lives in profiles
  (`/etc/midi/devices/`, `kj midi list/show/send/identify/panic`); device
  contexts are **side channels** — they tweak the gear while the band plays,
  never on the beat (`docs/midi-next.md`).

## Open items (the honest list)

- **Per-track MIDI channel + per-track flush** — the sink is whole-queue today:
  every cue plays on MIDI channel 0 and stop flushes everything. Two tracks
  sounding at once collide (`docs/midi.md` open questions).
- **`$HEARD` as a real kaish array + push→pull** — still the stopgap JSON
  string.
- **Quantized mailbox flush** — async inbound events digest on the grid
  crossing; wanted at band time, not load-bearing solo.
- **Measured reach k** — turns schedule one phrase ahead; k is unmeasured.
- **Producer/booth loop** — the producer chair (wide-parameter one-shots, chart
  revisions at hydrate boundaries, feedback in the receiver's vocabulary) is
  designed, not built.
- **Knobs as cells** — automation-MIME cells on the timeline; the side-channel
  device contexts are the shipped first step.
- **Cue traps** — cron in musical time (`trap '…' PHRASE%4`); heartbeat vars
  shipped, traps designed.
- **Archive RPC** — closed segments have no archive verb yet.

## Today's jam plan (2026-08-16)

Cast: **Amy's hands + the rack** (moltar — the master clock), **kaijutsu-chan**
(me — arranger/bass), **TiMidity on zorak** (FF4 soundfont — the kaijutsu
voice, IF the zorak audio stack comes up: `systemctl --user start pipewire
wireplumber timidity`, then verify with `aconnect -l`; otherwise the rack
sounds alone and the score plays silent-until-a-sink, which is correct), **the
kernel** (transport).

- **One track**: `jam`, 16-beat phrases, `system` clock to start.
- **Tempo source**: read the clock off the gear (KSP display / rack clock
  module) → `kj transport tempo --track jam <N>`; verify with `kj audio beats`
  on a short recording when we have one; upgrade to `kj transport clock --track
  jam modeled` the moment the app's ear on moltar is confirmed — the wire we
  most want to prove today.
- **Who plays what**: the rack lays down the groove (Amy's hands + its
  sequencer); the bassist context writes the bass — the Chameleon vamp
  (B♭m7–E♭7) if the rack sits in a friendly key, else a two-note drone under
  whatever it is doing; I steer phrases and keep the loop honest; TiMidity
  voices the score.
- **The first loop**: one bar of rack groove + a two-bar bass phrase. Loop =
  rack + bass.
- **First thing when the rack comes up**:
  1. Power the rack; let its clock run. Amy tells me the BPM (or I measure).
  2. Bring up the zorak voice (pipewire + timidity) and check `aconnect -l`.
  3. Create the player: `kj context create --type bassist --name jam` (the
     create rc attaches it to track `jam`, stopped).
  4. `kj transport tempo --track jam <N>`; `kj transport play --track jam`.
  5. I seed the first phrase (`kj drive --prompt` on the bassist), then listen:
     the vamp locks to the rack and we play.
