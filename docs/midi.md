# MIDI — the clock drifts; we model it, we don't chase it

> **Status:** design direction, captured 2026-06-29 in a co-design session
> (Amy + Claude). Decisions are *directions*, not commitments — code is truth,
> this is where we're aiming. No code yet. Companions: `docs/tracks.md` (the
> clock-domain substrate; MIDI is its Stage 3 `ClockSource` + a render target),
> `docs/chameleon.md` (the music application — "MIDI is a render of the score"),
> `docs/hyoushigi.md` (the `Tick`/`Timeline` primitive and the speculation lead
> this leans on), `docs/shared-state.md` (the `/run` substrate a probe writes).
> `tracks.md` is paused on the decisions below.

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
the node that owns the USB already does well.

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
beats_for(lead_time)`); the `tracks.md` Stage-1 rotation-gap argument already
leans on it. So the node that owns the MIDI out receives the *committed* score
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
fine clock locally).** This is the natural shape of `tracks.md` Stage 3's
`ClockSource` trait — **a MIDI clock source is a *proxy* for a clock that lives
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
  alongside app-display and audio-samples. The score stays symbolic (ABC + data);
  the edge node renders committed cells to ALSA MIDI out.

So MIDI input and output are both "a track with a clock source / render target
that happens to live on a node." Which makes the MIDI edge node **the first
kernel-owned compute node** — the resource-offered-wholly-owned-by-the-kernel
fleet idea, deliberately scoped down to one well-defined resource (ALSA MIDI +
a realtime scheduler). Building it prototypes that future, small.

## The topology (Amy's room, 2026-06-29)

- **KeyStep Pro (KSP) — usual clock master**, on a long-range USB3 hub with the
  **1010 Bitbox** mixer (deliberately *not* on MIDI for now — it's the recording
  path). KSP is usually plugged into the **laptop** over USB while jamming.
- Occasionally a Steinway interface or a PC is the master instead; usually KSP.
- **Eurorack** in the loft; **Polyend Poly 2** + other USB-MIDI modules bridge it.
- **zorak** (GPU box) in the basement; **Amy's laptop** in the great room (WiFi).
- **Loft edge node: the 2008 Lenovo workstation laptop** (quad Xeon, 32 GB,
  NVIDIA, gigabit) — already there, repurposed later as the loft MIDI node.
- Wired 1G/2.5G between fixed nodes via a Ubiquiti DMSE switch/router, idle most
  of the time.

**Dev loop for now:** a **virtual MIDI clock on zorak** (software ALSA clock
source) + a virtual MIDI out — no network at all for the first slices. Link the
real KSP / loft node in later.

## Decided (2026-06-29 design round)

- **Model clock drift; don't slave to pulses.** Observe → model tempo/phase/drift
  → regenerate a tight local clock. Learning the drift is the design, not a
  workaround.
- **No MIDI-over-SSH subsystem.** Crypto cost is noise; TCP HOL-blocking is the
  problem. Realtime crosses the wire as RTP-MIDI (UDP + recovery journal) only
  when it must; everything else is the existing RPC control plane.
- **Distribute tempo/intent, not pulses.** The wire carries tempo + transport +
  phase-align points; the node near the gear regenerates fine timing.
- **ALSA for MIDI, PipeWire for samples.** Split by job; no graph tax on MIDI.
- **Output first.** The speculation lead absorbs network latency, so output is the
  low-risk, high-payoff first slice.
- **Input is batched telemetry over the control plane**, not realtime — chunks of
  timestamped events landing as score blocks.
- **Edge node = the loft Lenovo**, repurposed later; not blocking the dev loop.
- **The MIDI edge node is the first kernel-owned compute node** (fleet idea,
  scoped to one resource).

## Staging

- **M1 — Output on zorak, virtual MIDI, system clock. ✅ SHIPPED 2026-06-30**
  (`tracks.md` Stage 3 WIs 1–7, commits `2e3dc6c5`→`508bc0c4`). A track renders its
  committed score (ABC→MIDI) to a local ALSA MIDI out, scheduled with the
  speculation lead. No network, no external clock. The render-target seam
  (`RenderTarget` on the track, fed from the materialize crossing, jitter-free
  scheduled-instant reference, `flush_scheduled_after` on stop/pause) and the one
  real target (`AlsaMidiOut`, relative real-time queue scheduling) both landed; the
  ALSA loopback ran live on zorak. The lead-covers-scheduling property holds by
  construction (cells commit ahead → `at` is in the near future). Canonical record:
  `tracks.md` "Status — M1 landed". The attach surface landed too:
  **`kj transport render --track <t> [--to alsa-midi] [--port <name>]`** opens the seq
  port and registers the target (a `BeatCommand::AddRenderTarget`); live-verified on
  zorak end-to-end (command → beat → NoteOns at a subscribed reader). A
  detach/replace verb is still TODO.
- **M2 — Input telemetry, batched.** Capture a local (virtual then real) MIDI in,
  timestamp with ALSA, batch into a MIDI-in track as score blocks over RPC.
  Snapshot = the track-scoped windowed read.
- **M3 — Drift-modeled clock-in.** Observe the virtual clock (then KSP over USB),
  fit the tempo/phase/drift model, phase-lock a local `ClockSource`. This is the
  real Stage 3 MIDI driver, built remote- and estimate-shaped from the start.
- **M4 — Cross-node + the edge node.** Stand up the loft Lenovo as a kaijutsu
  compute node owning the Eurorack's USB-MIDI; RTP-MIDI for any realtime hop;
  KSP-on-laptop hosts the clock observer and ships the model to the kernel.
- **Later — sense all the clocks; samples-with-MIDI; MIDI 2.0/UMP.** Model the
  drift of every clock we have an app on (multi-clock observation). Sampler nodes
  (MIDI trigger → PipeWire sample). UMP is already in ALSA rawmidi; the symbolic
  score maps onto its per-note pitch / hi-res velocity when wanted — keep the
  `ClockSource`/render seams from assuming MIDI-1.0 bytes forever.

## Open questions (for the implementation sessions)

- **Drift-model shape.** Only the *estimator* is open (the phasor + slew is
  hyoushigi's `Timebase`/local-phasor, reused — see "2. Clock-in"): tempo-EMA +
  phase-correction vs a lightweight Kalman over the observed pulse intervals,
  emitting `Timebase`-shaped corrections. What observability we want (the model's
  residual is a great health signal — a probe could write it to `/run`). Decide
  when M3 starts; M1/M2 don't need it.
- **`ClockSource` trait surface for remote + estimate-driven sources** — settled
  with `tracks.md` Stage 3, but this doc is the second concrete voice for it
  (alongside the system clock): bring both to that trait's design.
- **Render-target seam on the track** — how a track declares "render committed
  cells to ALSA MIDI out on node X" alongside app-display/audio. Where it lives
  relative to the score context (a render is a consumer of the track's score, not
  a producer — likely an attachment shape or a separate render registry).
- **Where the clock observer process lives** when KSP is on the WiFi laptop — a
  kaijutsu node agent on the laptop is fine (it ships a model, not pulses), but
  confirm the laptop runs a node agent vs. moving KSP's USB to a wired node.
- **MIDI-in track conflict with music producers** — `tracks.md` left concurrent
  producers as a loadout policy; a MIDI-in (human) track and a model producer on
  the *same* lane is exactly the concurrent-producer case to think through if Amy
  wants to play *with* the band on one lane rather than a parallel one.
