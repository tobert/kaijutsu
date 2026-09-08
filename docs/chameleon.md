# Chameleon — models playing to the beat

> **Living document.** This is how the band works *right now* — the
> load-bearing facts a new player or session needs, kept current as the
> instrument grows. Code is truth: when this doc and the kernel disagree, the
> doc is wrong; fix it. History of how this arrived lives in `docs/devlog.md`
> ("The music stack", "The beat learns to carry its own clock", "The hardware
> gets its own body"). Companions: `docs/tracks.md` (the track substrate),
> `docs/midi.md` (clock doctrine + the wire), `docs/pcm.md` (samples on the
> same seam), `docs/midi-next.md` (device profiles + `kj midi`),
> `docs/hyoushigi.md` (the `Cell`/timeline primitive), `docs/audio-daemon.md`
> (the hardware I/O node — MIDI and PCM I/O live there, not in the app).

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
beat grid, never a wakeup wallclock (`docs/midi.md` "The one timebase";
`docs/pcm.md` "Timing rides the one timebase").

Two drivers, switchable per track with `kj transport clock --track <t>`:

- **`system`** — a local fixed-tempo timer (`SystemClock`, `now + period`).
  Set it with `kj transport tempo --track <t> <bpm>`. Everything starts here.
- **`modeled`** — phase-locked to an observed external MIDI master
  (`ModeledClock`, `docs/midi.md` "Distribute tempo, not pulses"). The node
  owning the master's USB runs the "ear" (`kaijutsu-audio-runtime`'s capture
  thread — `docs/audio-daemon.md`), which learns tempo + phase + drift and
  ships low-rate `ClockEstimate` references; the track free-runs at its last
  tempo until the first reference arrives, then **fires on the master's
  integer beats** with slew-limited corrections, a loud starvation warn if
  references go quiet, and a 5 s stale-drop on the receiving end. The current
  period carries over on the switch; `kj transport tempo` while slaved is an
  honest manual nudge — the master then re-corrects.

**How the rack's tempo reaches the kernel:**

| Path | Evidence |
|---|---|
| Read it off the gear → `kj transport tempo --track <t> <bpm>` | system clock |
| Record a few bars → `kj audio beats <file>` → set the measured BPM | Beat This! (ISMIR 2024) via the pure-Rust `beat-this` crate; models in `~/.local/share/kaijutsu/models/beat-this/`; verified live (120 BPM click → `bpm=120.0`) |
| Live MIDI clock-in: rack → daemon's ear → estimator → RPC → `modeled` track | `kaijutsu_audio::clockin::ClockEstimator` (`crates/kaijutsu-audio/src/clockin.rs` — EMA tempo, phase-exact pulse counting, dropout recount, stall flags); the daemon's capture thread ships estimates (`kaijutsu-audio-runtime/src/runtime.rs`, `report_clock_estimate`); kernel `BeatRequest::ClockEstimate` (`kaijutsu-server/src/beat.rs:2425`, track resolved by the seat's attachment) → `ModeledClock::apply_estimate` (`clock.rs:154`) |

Live clock-in needs `kaijutsu-audiod` running on the box that owns the rack's
USB, its capture context attached to a `modeled` track, and the master
sending MIDI clock on the bus the daemon hears — `kj audio devices --node
audio/<host>` (`docs/audio-daemon.md`) shows whether a node can currently see
the rack. Until that's confirmed for a given room, the jam procedure is:
**read the tempo off the gear, set it on the track, and optionally verify
with `kj audio beats`** on any recording of the rack. Measure, don't receive
— the modeled lock is a bonus, not a prerequisite.

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
  `text/vnd.abc`; `kaijutsu-audiod` renders ABC→MIDI *at the sink*
  (`kaijutsu-audio-runtime/src/dj/midi.rs`), scheduling into its local ALSA
  queue at `receipt + lead` — the speculation lead is the jitter buffer, and
  intra-phrase timing is sub-ms off one anchor.
- **Phrases, not bars, in the kernel**: `beats_per_phrase` on the track policy
  (16 or 32 in practice); barlines are a notation/human affordance translated at
  the edge.
- **Fallback**: `UseLastGood` per track = the vamp insurance; an empty track
  resolves to `Skip` (silence until the first good phrase).
- **Samples ride the same seam**: a clip cell
  (`application/vnd.kaijutsu.clip+json`, `kj play --track`) renders like ABC
  through the same `RenderCue` (`docs/pcm.md`). The mime IS the dispatch key.
- **Hearing**: `kaijutsu-audiod`'s ear captures incoming MIDI, stamps it with
  ALSA receipt time, and batches it to the kernel as score blocks (telemetry,
  not realtime). Device knowledge lives in profiles (`/config/midi/devices/`,
  `kj midi list/show/send/identify/panic`); device contexts are **side
  channels** — they tweak the gear while the band plays, never on the beat
  (`docs/midi-next.md`).

## When a player needs permission (designed, not built)

The beat model has an answer for a turn that is *slow* — the grid fires anyway,
the vamp covers, `UseLastGood` holds the floor. It has no answer for a turn that
is *stopped*, waiting on a human. That gap stops mattering the moment players
run continuously outside the band, which is where this is heading.

The mechanism a stopped turn hits is the approval gate: a `pre_call` hook body
that exits 3 escalates to a ledger ask and **blocks the caller** up to
`gate_wait_timeout` (300 s), then Expires. Three rules, from Amy:

- **Expiry is an error, not silence.** The waiting call should come back as a
  tool error the player can read and act on — retry and block again, or set the
  task aside and pick up other work. A player that stalls with no signal cannot
  route around it.
- **The bound belongs to the ask, not to a constant.** 300 s is tuned for a
  human already at the keyboard. Some asks should wait indefinitely — the useful
  case is a human who has walked away and wants the work still standing when
  they return.
- **Expiry never grants.** A timed-out ask fails closed. It is the one path
  where failing open would be invisible.

Why the ledger rather than per-context state: one board, many contexts. A human
working across a dozen players answers them in one place, which is also what
makes an unbounded wait tolerable rather than lost.

Where a model fits: a seat built like `musician` — narrow loadout, rc-driven,
no human turn — that reads the tool plan and the classifier's signals and
*prepares* the ask, writing a clear description and a recommendation. It does
not decide; a human still answers. A cast slot is keyed by `context_type`, so
casting that seat is a config line, exactly like casting a chair. Full record
and the open pieces: `docs/issues.md`, "The escalation seat: a small model
that prepares the ask".

Nothing here contradicts the beat doctrine — a musician on the grid should
never hold a capability that can escalate in the first place. This is for
players whose work is not quantized.

## Open items (the honest list)

- **Per-track MIDI channel + per-track flush** — the sink is whole-queue today:
  every cue plays on MIDI channel 0 and stop flushes everything. Two tracks
  sounding at once collide (`docs/midi.md`).
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

## Starting a jam

1. Create the track and player: `kj context create --type bassist --name
   <track>` (the create rc attaches it to the track, stopped).
2. Set the tempo: read it off the gear (or a rack's clock module) →
   `kj transport tempo --track <track> <bpm>`; verify with `kj audio beats`
   on a short recording if unsure. Upgrade to `kj transport clock --track
   <track> modeled` once `kj audio devices` confirms the daemon can hear the
   rack's clock.
3. `kj transport play --track <track>`, then seed the first phrase with
   `kj drive --prompt` on the player. The vamp (`UseLastGood`, or the house
   first-loop above) covers until the band locks in.
