# MIDI — the clock drifts; we model it, we don't chase it

`kaijutsu-audiod` connects to the kernel over SSH and runs the Bevy-free
`kaijutsu-audio-runtime`. It is the sole hardware owner — MIDI in/out, PCM
playback, clock estimation — and the app has no hardware I/O. See
`docs/audio-daemon.md` for the daemon itself. This document is the design
doctrine the daemon implements: the real-time stance, the wire-cue
mechanism, and the one-timebase rules. History of how it got here —
`docs/devlog.md`, "The music stack", "The beat learns to carry its own
clock", and "The hardware gets its own body".
Companions: `docs/tracks.md` (the clock-domain substrate; MIDI is a
`ClockSource` plus a render target), `docs/chameleon.md` (the music
application — "MIDI is a render of the score"), `docs/hyoushigi.md` (the
`Tick`/`Timeline` primitive and the speculation lead this leans on),
`docs/pcm.md` (the samples half: PCM through the same render seam),
`docs/midi-next.md` (device profiles and channel bindings).

## The insight

A clock you don't own **drifts**, and that's the interesting part, not the
annoying part. We do **not** slave pulse-for-pulse to an external MIDI clock and
inherit its jitter. We **observe** the external clock, **model its tempo + phase
+ drift**, and run a *local* clock phase-locked to that model. The network (or
even a jittery USB/WiFi hop) becomes *measurement noise the filter rejects*, not
a realtime stall. Learning the drift is both more interesting and more efficient
than chasing every pulse — it's the same exogenous-beat doctrine as `tracks.md`
("the world beats the track"), now made concrete for a real clock master.

The payoff, stated up front: **on Amy's topology, nothing needs hard-realtime
transport across the network.** Three independent reasons (below) each remove a
realtime constraint, leaving only *local-to-the-hardware* timing — which ALSA on
the node that owns the USB already does well. The stance underneath that payoff
is the next section, and it is the foundation of the whole render story.

## The real-time stance — micro-batch, don't chase

The load-bearing principle under all of this, made explicit: **we take real time
seriously by refusing to chase it.** We don't lock to deadlines and hope; we
**micro-batch** — commit work far enough ahead that we only ever promise what we
can hit *99.99% of the time*. On this instrument that horizon is on the order of
**a few seconds — 16–32 bars of music**, which is exactly the speculation lead
`hyoushigi` already stages content against (`speculate_at = start −
beats_for(lead_time)`). Everything downstream spends that budget: the network
only has to deliver *ahead of time*, never *just in time*; a sink schedules into
its **local** device queue and wire jitter vanishes into the lead.

This is not a MIDI trick — it is the whole real-time story. It licenses samples
(`docs/pcm.md`), drift-modeled clock-in (below), and every render crossing the
wire to an off-box sink. The *only* place we pay hard realtime timing is the
final, sub-lead, local-to-the-hardware scheduling — and that lives on the node
that owns the gear, never on the wire. Say the guarantee out loud and design to
it: **we make only the promises the lead can keep.**

## The latency truth (so we design for the right enemy)

A NoteOn is 3 bytes; on idle wired 1G/2.5G through the Ubiquiti switch transit is
~0.1–0.3 ms RTT. **The network hop is not the enemy.** Three other things are:

1. **Jitter, not latency.** Feel lives in timing *consistency*. Wired-LAN-idle
   UDP is tight; **WiFi is not** (5–30 ms bursty). So a WiFi node (Amy's laptop)
   must never sit in a realtime clock/note path — it's an observer/control
   surface (the `rotate: None` interactive attachment from `tracks.md`).
2. **Endpoint scheduling jitter** — the hootenanny lesson. A PipeWire graph
   quantum buckets MIDI to graph cycles (1024 samples @48k ≈ 21 ms). People blame
   "the network" for what is actually the local audio graph. → **ALSA for MIDI.**
3. **TCP semantics.** The crypto cost of SSH/RPC is *noise* (AES-GCM/ChaCha20 run
   at GB/s; MIDI is bytes/s — don't bother measuring it). The real cost of
   tunnelling MIDI through the kaijutsu RPC proto or a new SSH subsystem is that
   both ride **TCP**: head-of-line blocking + Nagle, where one retransmit stalls
   everything queued behind it — the wrong failure mode for realtime media. This
   is exactly why the realtime-MIDI standard (RTP-MIDI, RFC 6295) runs over **UDP**
   with an application-level **recovery journal**. **Decision: we do not build a
   MIDI-over-SSH subsystem.** Realtime crosses the wire (when it ever must) as
   RTP-MIDI; everything else is the existing control plane.

## Two planes, two transports

- **Control / score / transport-intent plane → kaijutsu RPC** (Cap'n Proto over
  SSH; "fill up crypto blocks"). Snapshot the bus, publish a phrase, "tempo 120,
  start at the next bar," "slave track *bass* to the KSP clock model," batched
  input telemetry. Tens-of-ms latency is fine. This is the kernel's existing
  language and the existing secure transport — **input rides here** (see below).
- **Realtime clock + note plane → ALSA, local to the hardware.** Cross-node only
  via RTP-MIDI (`rtpmidid` bridges ALSA seq ↔ RTP-MIDI + AppleMIDI session +
  recovery journal). A node never invents a MIDI channel type: it runs a kaijutsu
  **compute-node agent** (control over RPC) *and* owns ALSA MIDI locally
  (realtime). RPC for management, ALSA/RTP-MIDI for pulses.

## The three constraint-removers

**1. Output: the speculation lead *is* the network jitter buffer.** `hyoushigi`
already stages content *ahead* of the playhead (`speculate_at = start −
beats_for(lead_time)`); `tracks.md`'s "Rotation is a gap, not an overlap"
already leans on it. So the node that owns the MIDI out receives the *committed* score
over RPC (non-realtime, loss-tolerant, retryable) and schedules NoteOn events
**locally** into the ALSA sequencer queue against its local clock. The network
only has to deliver **ahead of time**, never **just in time**. The DAC never sees
the network. Output across the basement↔loft hop is solved by construction.

**2. Clock-in: model the drift; regenerate locally.** We never ship 24 PPQN
pulses across the network as the beat. The node that owns the clock master's USB
reads the pulses *locally* (USB is fine), runs a **drift model** (tempo + phase +
rate-of-change), and ships *tempo + phase estimates* to the kernel over RPC at a
low rate. WiFi/RPC jitter is measurement noise the model rejects; the kernel's
track runs a tight **local** clock phase-locked to the estimate.

**This reuses machinery hyoushigi already has — do not reinvent a PLL.**
`hyoushigi.md` already distributes the kernel's *own* clock as a `Timebase`
(`epoch_tick`/`epoch_wallclock`/`tempo`/`phase`) that each client turns into a
**local phasor** which *slews* toward occasional corrections rather than trusting
any one pulse's arrival — explicitly because Cap'n-Proto-over-SSH is TCP and
individual packets jitter ("network jitter never enters an audio callback"). The
MIDI drift-model is **the same phasor + slew, fed from an *external* master's
observed pulses instead of the kernel's**. The only genuinely new part is the
*estimator* that turns noisy observed pulses into a `Timebase`-shaped correction
stream; the regenerate-locally-and-slew half is built. So even KSP-on-the-laptop-over-WiFi is fine: the
laptop hosts the observer, the wire carries a *model*, not a pulse train.

**3. Input notes/CC: batched telemetry, not realtime.** Incoming MIDI (Amy
playing, CC sweeps, the Eurorack's output) is captured locally with ALSA
timestamps and **batched into the kernel as a steady stream of blocks** over the
control plane. It "breaks away from realtime and fills up crypto blocks." No
realtime transport, no jitter budget — just timestamped chunks landing as score.

## Distribute tempo, not pulses; the clock lives near the gear

The one principle to lock: **the wire carries *intent* (tempo, transport
Start/Continue/Stop = MIDI realtime `FA`/`FB`/`FC`, occasional bar/beat
phase-align points); the node near the gear carries *timing* (regenerates the
fine clock locally).** This is the natural shape of `tracks.md`'s
`ClockSource` trait ("Clock sources") — **a MIDI clock source is a *proxy* for a clock that lives
on an edge node**, and it can be *remote* and *drift-modeled*. Design the trait
so a clock source can be remote + estimate-driven and the whole network story
slots in without touching attachments. (RTP-MIDI's own clock-sync covers the rare
case where raw clock genuinely must cross the wire.)

## ALSA vs PipeWire: split by job

- **MIDI → ALSA directly** (`snd_seq`/rawmidi). The ALSA sequencer gives
  in-kernel timestamped queues you schedule events into and a slaveable timer —
  exactly what we want, and it skips the graph tax. No DSP reason to pay
  PipeWire's quantum for pure MIDI.
- **Audio samples → PipeWire** (the `pawlsa` `play_wav`/`play_pcm` path already
  exists). A sampler is "MIDI NoteOn → pick sample → PipeWire play": the trigger
  wants ALSA timing, playback lives in the audio graph. Two subsystems, two jobs,
  bridged locally on whichever node has the speakers.

## Snapshots & publish collapse into the track/score model

No new abstractions — MIDI in/out reduce to `tracks.md` primitives:

- **Snapshot the bus** = a MIDI-in track whose producer attachment turns incoming
  events into timestamped Cells. "Recent bus activity" is the track-scoped
  `KJ_HEARD`-style windowed read Stage 2 already built. The bus monitor is a probe
  attachment writing the score, not new machinery.
- **Publish to the bus** = a **render target.** `chameleon.md` already says "MIDI
  is a render of the score"; MIDI-**out**-to-hardware is just another renderer
  alongside audio-samples, both hosted by `kaijutsu-audiod` — see "Render is a
  wire cue" below for the mechanism.

So MIDI input and output are both "a track with a clock source / render target
that happens to live on a node." Which makes an audio node **a kernel-owned
compute node** — the resource-offered-wholly-owned-by-the-kernel fleet idea,
scoped to one well-defined resource (ALSA MIDI + PCM + a realtime scheduler).

## Render is a wire cue; the sink owns the hardware

The real-time stance says in-process hardware emit buys nothing the lead
doesn't already buy: timing precision comes from scheduling into a *local*
device queue ahead of time, and any sink — including one across the wire —
has a local queue. So hardware emit lives off the kernel entirely, in the
sink: `kaijutsu-audiod`, today. The kernel stays what it always was — a
durable orchestrator with no audio FFI.

**MIDI and samples are one path.** A render is a **mime-keyed symbolic cue**
scheduled on the lead. The committed score stays symbolic (ABC / a clip record,
`docs/pcm.md`); what crosses to the sink is a small cue, never the score:

- **`RenderCue { mime, payload, lead, epoch_ns }`** — `payload` is inline
  symbolic content (`text/vnd.abc`, a clip mime, `RENDER_FLUSH_MIME`) or a CAS
  ref; `lead` is a *relative* `Duration` (a process-local `Instant` can't
  cross the wire); `epoch_ns` is the emission wallclock (see "The one
  timebase" below). The sink schedules at `receipt + lead`, back-dated by the
  cue's age. An ABC/MIDI cue and a clip cue are the same directive with
  different mimes; the sink dispatches by mime.

**Three phases, each its own micro-batch** — the pipeline named so a phase can
move without a rewrite:

1. **Compose** — a producer turn commits an ABC (or clip) cell on the track. The
   score. Micro-batch = the OODA phrase.
2. **Render** — `abc→midi` (or clip→resolved-sample). Near-**pure CPU**: no
   hardware, only a CAS read. It runs at the sink today
   (`kaijutsu-audio-runtime/src/dj/midi.rs`, reusing `kaijutsu_abc::midi::events`)
   — the cue carries the raw ABC text (`text/vnd.abc`), not pre-rendered
   events, so the kernel needs no ABC crate on its own render path.
3. **Emit** — the sink schedules the cue into its local hardware queue at
   `receipt + lead` (ALSA seq for MIDI, PipeWire for samples). Micro-batch =
   the scheduled play-out.

**MIDI is sink-dependent, and that is fine.** A track whose clock is rolling
with no sink attached makes no sound — exactly like samples. That is correct,
not a regression: **the track is preserved** (its committed score is durable,
`KJ_HEARD`-queryable, replayable), so silence-now is never lost work — attach
a sink later and replay. The kernel (a headless systemd service) never needs
an audio stack to keep a band playing into the score.

## The relative-lead timebase, analyzed (2026-07-02)

Before building the PCM path on the wire-cue substrate we analyzed its timing
model (a two-cast review — gemini-pro batch + deepseek — plus the derivation
below). **Verdict: sound, build on it.** But a phrase above needs an honest
correction, and a companion subsystem falls out of it.

**Correction to "wire jitter vanishes into the lead."** The `receipt + lead`
scheme is *not* a jitter buffer: a real jitter buffer decouples the read clock
from the write clock, but here the play-out anchor **is** the (jittered) arrival,
so it *passes arrival jitter through* rather than absorbing it. The blast radius
is bounded, which is why it works: a whole phrase's events schedule into the
local ALSA queue off **one anchor**, so *intra-phrase* timing is sub-millisecond
perfect; jitter lands only at *phrase/cell boundaries* (seconds apart). That is
musically invisible for sustained single-sink material — but audible for PCM
attack transients and for **multi-sink flam** (two sinks on independent streams
fire the same note at `a + d₁` vs `a + d₂`). The clean part still holds: with
`sink_clock = kernel_clock + Θ`, an event intended at kernel-instant `a` fires at
`a + d`; **Θ cancels** (no clock-sync needed), constant latency is free, only
transit *jitter* costs — provided `lead ≥ transit + sink scheduling granularity`.

**Two subsystems that COMPOSE (decided, Amy, 2026-07-02).** The per-cue trigger
path and a *continuous local timebase* are separate renderings of the same kernel
timeline, neither feeding the other:

- **Per-cue trigger** (`RenderCue { lead }`, exists) — fire-and-forget one-shots,
  jitter-sensitive. Owns **sound onset**.
- **Continuous timebase** (the "good-enough shared hyoushigi", to build) — a
  local phasor in the sink that free-runs and *slews* toward low-rate
  `{tick, tempo, phase}` references from the kernel (never hard-resync; a little
  jitter buys resilience). Owns **"where's the beat now"** — metronome, a smooth
  playhead, beat-synced visuals. This is `## Distribute tempo, not pulses`
  applied to *output*.

Divergence between them is **measured, not prevented by construction** — the
metronome slice is the validator (click-on-local-beat vs MIDI-note-on-per-cue,
inter-onset within ~1 ms, watch for drift). Gemini's alternative — *replace* the
per-cue anchor with an absolute **tick** the sink converts through its PLL-smoothed
clock (a true jitter buffer that also keeps audio locked to the visual playhead)
— is retained as the **upgrade path**, reached for only if the metronome test
shows the boundary jitter audibly pulling audio away from the phasor (PCM
transients / multi-sink). Not a prerequisite. PLL failure modes to design against:
starvation drift (reference rate must bound free-run drift < ~1 ms), tempo-step
slew limiting, phase-slew-not-step, reference-jitter outlier rejection. The full
findings list (incl. the `beat.rs:940` random-walk cadence one-liner and the PCM
guardrails) lives in `docs/issues.md` → Hyoushigi / Musician.

## The one timebase — emission stamps, back-dating, and the locked phasor

The metronome slice above exists to *measure* divergence between the per-cue
trigger path and the continuous timebase; it found real divergence twice in
one day (`docs/devlog.md`, "The beat learns to carry its own clock") — a
click burst-and-starve from buffered references folding at one frame-`now`,
then click and bass wandering apart from the boundary-jitter the "relative-lead
timebase" correction above predicted. What those findings hardened into,
stated as doctrine:

- **Every wire timing artifact carries its emission wallclock.** `BeatRef.epochNs`
  and `RenderCue.epochNs` (ns since UNIX_EPOCH; `0` = unstamped, old-peer
  fallback to receipt). `Instant`s still never cross the wire — only wallclock
  stamps do, and only ever as *age*: the sink computes `age = its own wallclock −
  stamp` and back-dates receipt, so the Θ-cancellation of the relative-lead
  scheme survives intact while variable transit latency stops mattering.
- **The kernel's clock is the timebase, and every node models its offset to
  it.** The kernel is the sole sequencer, so its wallclock is what a stamp
  means. A node learns its offset from the ping round trip — the sample with
  the smallest round trip wins, and the applied offset moves only when a new
  estimate lands further away than the current uncertainty, as a step, never
  a slew (musical scheduling runs on `Instant` and is untouched). Stamps are
  minted and aged in that domain, so a box whose host clock is minutes off
  still reads the one timebase. NTP is welcome and not required. A stamp more
  than 250 ms in the future says the receiver and the sender disagree about
  what time it is: it still folds, at receipt — skew never costs a beat — and
  it is counted (`kaijutsu.clock.future_stamps`), never silently floored at
  age zero.
- **Stale timing data is rejected, on a ladder** (Amy, 2026-07-15: throw away
  adjustments when the data is too stale): a reference folds phase only while
  young (`REF_FOLD_MAX`, ~1 s); older-but-plausible it only proves liveness
  (`Touch`); beyond `REF_STALE_MAX` (5 s) it is dropped. A cue older than its
  own lead has missed its onset — the past notes are dropped, never smeared
  into a late chord; a cue stale past the same 5 s bound is rejected whole.
- **Missed beats are missed.** No timing consumer ever replays a backlog — the
  metronome's never-stack-clicks policy, the grid's capped catch-up, the cue
  sink's past-note drop are one stance. Silence recovers; a burst never sounds
  right.
- **The kernel's grid is scheduled-periodic.** Beats re-arm at `scheduled +
  period` (never `actual_wake + period`, which integrates scheduler lateness
  into the musical timeline), with a small capped catch-up and a re-seed past
  it (suspend/stall = the grid moves on). Every per-beat stamp — `BeatSync`,
  `KJ_EPOCH_NS`, the render base — derives from the *scheduled* instant, so the
  timeline the sinks reconstruct is the ideal grid, not the wakeup jitter.
- **Once dialed in, the local clock is the truth between references** (Amy's
  principle — modern system clocks hold phase far better than a jittery
  reference stream corrects it). The receiver phasor runs exact feedforward
  tempo and corrects *phase* only outside a small deadband (~0.02 beats);
  inside it, it takes zero steps and free-runs. References become confirmation,
  not steering. The phasor's residual and every fold land in OTel
  (`kaijutsu.phasor.slew_beats` et al.) — the deadband is tuned from that
  histogram, not from vibes.

This is a deliberate first step onto the 2026-07-02 "upgrade path" (gemini's
absolute-tick-through-PLL): back-dating the anchor buys most of the jitter
buffer without moving cue scheduling into the tick domain. Go further — true
tick-domain scheduling against the phasor — only if the metronome validator
shows residual boundary error after this lands (PCM transients and multi-sink
flam remain the likely triggers, as before).

## The DJ thread — musical dispatch off the frame

A dedicated thread — the **DJ** — owns everything musically time-critical,
end to end: `RenderCue` parse + deadline math + dispatch, ABC→MIDI render +
the ALSA MIDI sink (incl. patch-bay auto-connect), the `LocalBeat` phasor +
click scheduling, and CAS prefetch dispatch. ("Conductor" is reserved for the
human at the instrument; the DJ is the one placing cues on a running clock.)
It lives in `kaijutsu-audio-runtime` (`dj/{core,thread,audio,prefetch,midi}.rs`),
part of `kaijutsu-audiod` — originally built to get cue dispatch off the Bevy
app's frame loop, then it moved with the rest of hardware I/O into the
standalone daemon (`docs/devlog.md`, "The hardware gets its own body"). One
std thread, current-thread tokio, one `select!` over {events, a control
channel, the click-horizon timer, prefetch outcomes}. The decision core is a
pure state machine (`handle_event(now, now_epoch, event) → Vec<DjAction>`)
so the TDD surface needs no threads and no devices.

**The clock is modal, and transitions are first-class.**
The DJ *starts on wallclock, dials into the beat grid, and falls back* — a
regular transition, crossed on every track start/stop, so the fallback path
stays exercised by real use instead of rotting untested:

- **Wallclock** (anchor state): cue placement by the emission-stamp
  back-dating ladder above; clicks silent (no phasor).
- **BeatGrid** (dialed in): entered on the first `Fold`-fresh `BeatSync`.
  Cues carrying an onset-beat stamp are placed at the phasor's predicted
  instant for that beat — phrases and clicks lock to one grid by
  construction, and inter-host wallclock skew cancels. Held through
  `Touch`-aged references (the free-running deadband doctrine); a cue
  without a beat stamp falls through to the wallclock ladder per-cue.
- **Back to Wallclock** on: reference age past `REF_STALE_MAX`, transport
  flush, connection loss (the existing halt triggers), or the **free-run
  cap** (`MAX_FREE_RUN`, ~10 s since the last actual *Fold*): sustained
  `Touch` — every reference late enough to prove liveness but never correct
  phase — must not keep an uncorrected, drifting phasor trusted forever
  (2026-07-18 gemini-pro deliberation). Two mechanical notes from the same
  round: the DJ's sleep is bounded by `next_stale_deadline()` so these
  fallbacks fire *on time*, not at the next click; and the click timer wakes
  as a beat **enters** the horizon (`beat − horizon`), not at the beat —
  waking at the beat would schedule clicks with zero ALSA lead.

Every transition emits telemetry (`kaijutsu.dj.clock_transition`, attrs
`to` + `reason`: fold / stale / flush / disconnect) — the transition rate and
reasons are the health signal for the whole timing path, same stance as the
phasor's slew histogram.

Beat-grid placement rides an additive `RenderCue.onset_beat` field, stamped
by the kernel at emission; a cue carrying one is placed at the phasor's
predicted instant for that beat instead of the wallclock ladder.

## The topology (Amy's room)

- **KeyStep Pro (KSP) — usual clock master**, on a long-range USB3 hub with the
  **1010 Bitbox** mixer (deliberately *not* on MIDI — it's the recording
  path). KSP is usually plugged into the **laptop** over USB while jamming.
- Occasionally a Steinway interface or a PC is the master instead; usually KSP.
- **Eurorack** in the loft; **Polyend Poly 2** + other USB-MIDI modules bridge it.
- **zorak** (GPU box) in the basement, running `kaijutsu-audiod`; **moltar**
  (office/gaming rig) also runs the daemon; **Amy's laptop** in the great
  room is WiFi and stays an observer/control surface, never a realtime node.
- **Loft edge node: the 2008 Lenovo workstation laptop** (quad Xeon, 32 GB,
  NVIDIA, gigabit) — already there, not yet running the daemon.
- Wired 1G/2.5G between fixed nodes via a Ubiquiti DMSE switch/router, idle most
  of the time.

A virtual MIDI clock (software ALSA clock source) plus a virtual MIDI out
still works as a no-hardware dev loop for the estimator and the DJ's clock
modes: `cargo run -p kaijutsu-audio-runtime --example midi_clock -- --bpm 120
--drift 1.0 --jitter-ms 2`.

## Where the pieces live today

A track renders its committed score to a wire cue; `kaijutsu-audiod`
schedules it into local ALSA MIDI (`kj play <file.abc>` is the standalone
trigger). MIDI-in capture, the drift estimator and clock-in all live in the
same daemon (`kaijutsu-audio-runtime`'s `midi_in.rs`, `clockin.rs`,
`dj/midi.rs`), never the server or the app:

- **The ear is the sink's twin.** MIDI-in capture: a ring buffer at the
  device, the daemon's musical timer cuts phrase-aligned batches, and a
  `commitCapture` verb (reverse `RenderCue`, `Inline | Cas` payload) pushes
  them to the kernel, which quantizes to the track grid and commits
  data-only cells. Client push, not kernel pull — the kernel never fetches
  from a client that can disconnect mid-jam. Score first, perception later:
  `KJ_HEARD` stays notation-only, so a captured MIDI window is durable but
  not yet readable by a model (`docs/issues.md`, "Hyoushigi / Musician —
  open remainder").

- **Clock-in.** `kaijutsu-audio/src/clockin.rs`'s estimator: MIDI clock phase
  is a count, not an estimate — pulse *n* is beat *n/24* by definition, so
  only the count→wallclock mapping is learned. Intervals classify by ratio to
  the learned period: ~1× learns, ~integer 2–4× is dropout inference (count
  += k, phase never slips), >4.5× is a loud discontinuity, short is jitter
  (count, don't learn). Start/Continue/Stop/SongPosition follow the MIDI spec
  (position frozen on Stop while tempo keeps learning). `reportClockEstimate`
  ships estimates to the kernel fire-and-forget (~2 Hz); an estimate is inert
  sensor data unless a track is slaved to it, and `kj transport clock <track>
  modeled|system` is the gated authority moment.
- **`ModeledClock`** (`crates/kaijutsu-server/src/clock.rs`) free-runs like
  `SystemClock` until anchored, then fires on the master's integer beats. PLL
  guards: tempo step ≤5%/reference, phase slew ≤0.05 beat with ≥0.5-beat
  seeks stepping outright, starvation (>10 s silent) warns once and
  free-runs. `kj transport tempo` while slaved is an honored manual nudge the
  next reference re-corrects.
- **Cross-node + additional edge nodes** — `docs/audio-daemon.md` is the
  daemon design (SSH connection, presence, ownership, RT priority); open
  follow-ups after the daemon's extraction from the app live in
  `docs/issues.md`, "Audio nodes — follow-up after daemon extraction".
- **Later — sense all the clocks; samples-with-MIDI; MIDI 2.0/UMP.** Model
  the drift of every clock we have a node on (multi-clock observation).
  Sampler nodes (MIDI trigger → PipeWire sample). UMP is already in ALSA
  rawmidi; the symbolic score maps onto its per-note pitch / hi-res velocity
  when wanted — keep the `ClockSource`/render seams from assuming MIDI-1.0
  bytes forever.

## Open questions

- **Observability placement.** `residual_ns` rides every clock estimate and
  is logged, but nothing writes it to `/run` yet for sibling probes to read.
- **Where the clock observer process lives** when KSP is on the WiFi laptop —
  a node running `kaijutsu-audiod` there is fine (it ships a model, not
  pulses), but confirm that vs. moving KSP's USB to a wired node.
- **MIDI-in on the same lane as a model producer.** N producers on one track
  coexist by construction (each committed cell carries `played_by`; ties at a
  tick are allowed; nothing squashes a co-producer's absolute notation).
  Music keeps one *playing* binding per track as **loadout policy**, not
  structure. What's still open is the *musical* policy when Amy plays with
  the band on one lane: does the human's MIDI-in share the model's lane
  (two `played_by`s, one track) or ride a parallel track — and what
  `UseLastGood` should repeat in the mixed case (today it's lane-scoped,
  producer-blind — a decision, not a bug).
- **Per-track channel + per-track render-cue routing** (the moment two tracks
  sound at once). Every cue plays on MIDI **channel 0**, and a
  `RENDER_FLUSH_MIME` cue flushes the sink's *whole* queue, so a second
  simultaneous track collides on the channel and cross-flushes on stop. The
  cue already carries the track's score `context_id`; the sink needs to
  schedule + flush *per context* and assign a channel per track/lane (drums →
  ch 9). Full write-up: `docs/chameleon.md` "Open items". Same routing gap as
  `docs/pcm.md` "Distributed listening" — solve together. The
  channel-assignment vocabulary lives in `docs/midi-next.md` (device
  profiles + bindings): a track binds to a *device.role* resolved through a
  profile, not a raw channel int — build the fix on that.
