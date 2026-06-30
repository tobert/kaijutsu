# PCM — playing samples through the audio render seam

> **Status:** design direction, captured 2026-06-30 in a co-design session
> (Amy + Claude). Decisions are *directions*, not commitments — code is truth,
> this is where we're aiming. **No code yet**; we execute once a couple more
> `tracks.md` items land (Stage 3 is closing). Companions: `docs/midi.md`
> ("ALSA for MIDI, PipeWire for samples"; output-first), `docs/tracks.md`
> (the `RenderTarget` seam this slots into — Stage 3 §"The render-target seam"),
> `docs/chameleon.md` ("MIDI/sample is a render of the score").

## The insight

Sample playback is **not new machinery** — it is a second render sink alongside
the MIDI-out one that **already landed**: `tracks.md` Stage 3 M1 shipped the
`RenderTarget` trait + `AlsaMidiOut` + `add_render_target` (in
`crates/kaijutsu-server/src/clock.rs`, loopback-verified on zorak). `midi.md`
already split the jobs: **MIDI → ALSA** (in-kernel timestamped queues, no graph
tax), **audio samples → PipeWire** (the audio graph is the right home for PCM).
This doc is the samples half, scoped down to the smallest thing that makes a
sound: *the kernel decides to play a sample, and a sample plays.*

## Decisions (this co-design round)

- **Abstract seam; the kernel process never emits audio.** Define an
  `AudioRenderTarget` seam, but hardware emission lives **outside** the kernel
  process — in a peripheral that speaks RPC. Implement the **app (Bevy) sink
  first** (the dev loop already has a window + speakers); a **headless edge-node
  agent** is the sibling for later (the loft Lenovo of `midi.md` — a node the
  kernel *owns over RPC*, not the kernel binary linking ALSA/PipeWire). The
  kernel orchestrates *what/when*; the node does the physical *emit*.
- **A fixed WAV/PCM first.** The first sound is a known sample file, not a synth.
  Prove the whole pipe end-to-end with zero music-rendering risk.
- **Standalone before the track.** Build a minimal "the kernel makes a sound
  happen" path now, *independent* of the `Track`/`RenderTarget` machinery. Fold
  it into the track render seam once Stage 3 lands (it is the same shape).
- **Don't reinvent decoding.** App uses Bevy's `wav` format feature now; grow
  into more formats via **Symphonia** (rodio's Symphonia backend in `-app`;
  Symphonia directly in the kernel sink). We never hand-roll a WAV parser.

## Where audio lives (why both)

`midi.md`: samples are "bridged locally on whichever node has the speakers." In
the dev loop that node is **the app** (Amy at the GPU box over remote desktop) —
`bevy_audio` is a few lines and the window is already there. For headless play,
the home is a **separate edge-node agent**, *not* the kernel process: the kernel
is the durable orchestrator and should stay free of audio/MIDI FFI, so hardware
I/O is pushed to a peripheral that attaches over RPC. So: one seam, two *external*
sinks (app, edge-node agent). App first because it is cheapest to a working sound;
the edge-node agent reuses the proven ALSA-PCM path from `pawlsa` when we go
headless. The kernel binary never links `alsa`/`pipewire`/`symphonia`.

## The seam

The landed `RenderTarget` (in `crates/kaijutsu-server/src/clock.rs`) consumes a
**committed score cell** + a local instant — perfect for slice 5, wrong for the
standalone first slice, which has a *sample*, not a cell. So the standalone slice
gets its own narrow seam, `AudioRenderTarget`, that converges onto `RenderTarget`
when audio becomes a track render. Same posture as the MIDI one (a render is a
*consumer*, never a producer; it receives resolved bytes + a local instant, never
re-resolves CAS):

```rust
/// One audio sink. Implemented in the app (Bevy) and, later, the kernel (ALSA).
pub trait AudioRenderTarget: Send {
    /// Play one sample. `at == None` means "now" (first slice); a scheduled
    /// instant arrives with the track integration (speculation lead).
    fn play(&self, sample: AudioRef, at: Option<Instant>) -> anyhow::Result<()>;
}

/// What crosses the wire / the seam. Encoded bytes + a format tag, or a CAS
/// ref the sink resolves. Decoding lives at the sink (Bevy decoders / Symphonia)
/// — the wire never carries raw PCM for the first slice.
pub enum AudioRef {
    Encoded { bytes: Vec<u8>, format: AudioFormatHint }, // small samples inline
    Cas { hash: ContentHash, format: AudioFormatHint },  // larger: fetch from CAS
}
```

- **App impl — `BevyAudioOut`.** Turns `AudioRef` into a Bevy `AudioSource`
  (Bevy's `wav` decoder now; Symphonia-backed formats later) and spawns an
  `AudioPlayer`. Lives in `kaijutsu-app`.
- **Edge-node agent impl — `AlsaPcmOut` (later).** Decode with **Symphonia** →
  raw PCM → ALSA PCM out (PipeWire intercepts via its ALSA shim). Reuses
  `pawlsa`'s playback loop. Lives in the node-agent binary — **not** the kernel.

The trait + `AudioRef` + format types live in the **`kaijutsu-audio` crate
(confirmed)**, which is **FFI-free**: no `alsa`/`pipewire`/`symphonia`. Those deps
live only in the consuming binaries (`kaijutsu-app`, the edge-node agent), so the
kernel can depend on `kaijutsu-audio` for orchestration without ever linking a
hardware library.

## Reusable code from `~/src/pawlsa-mcp`

- `src/alsa/playback.rs` — ALSA PCM open + write loop (`alsa` crate 0.11). The
  kernel sink's output stage. (Its hand-rolled `parse_wav` we **replace with
  Symphonia** per the decoding decision.)
- `src/pw/mod.rs` — `spawn_pw_thread`/`PwHandle`: link create/destroy, node
  volume/mute, device profile/route via SPA pods (`pipewire` crate 0.9). **Not
  needed for the first sound**; this is the later routing/volume control surface.
- Deps to pull when the kernel sink lands: `alsa = "0.11"`, `pipewire = "0.9"`,
  `symphonia` (decode).

## The wire (standalone first)

The kernel→app push channel already exists: `BlockEvents` (server→client
subscription) carries `KernelOutputEvent` via the `on_output` callback, consumed
by `BlockEventsForwarder` in `crates/kaijutsu-client/src/subscriptions.rs`.

- **Add an audio-play directive** as a sibling event on that channel (a new
  `BlockEvents` callback method, or a new `OutputEvent`-style variant), carrying
  `AudioRef` — inline `Encoded` bytes for small samples, a `Cas` ref for anything
  larger (the kernel already has CAS for assets). Schema in `kaijutsu.capnp`.
- **Trigger:** a `kj` subcommand (e.g. `kj play <path|asset>`) — kernel resolves
  the sample (VFS path or CAS asset) and calls the active `AudioRenderTarget`.
  With the app as the home, "call the target" == "emit the directive over the
  client channel"; the app's `BevyAudioOut` is the target on the far side.
- **App side:** the forwarder pushes the directive into the Bevy world over the
  existing client→app bridge; a Bevy system (`MessageReader<PlayAudio>` — Bevy
  0.18 `Message`, not `Event`) loads the bytes as an `AudioSource` and spawns an
  `AudioPlayer`.

## Bevy 0.18 audio specifics

- `kaijutsu-app` already enables Bevy `"default"` features, which include
  `bevy_audio`. **Add the `wav` format feature** for WAV decode. (`AudioPlayer`
  component + `AudioSource` asset + `AudioSink`; remember 0.18's `Message`/
  `MessageReader`/`MessageWriter` rename, per CLAUDE.md.)
- **Symphonia later:** rodio (under `bevy_audio`) has a Symphonia backend; enable
  the corresponding Bevy/rodio format features (FLAC/MP3/OGG/AAC) when we want
  them in `-app`. Same crate (`symphonia`) is the kernel sink's decoder.

## Slices

1. **Seam + types.** `AudioRenderTarget` trait + `AudioRef`/`AudioFormatHint` in
   the new **FFI-free `kaijutsu-audio` crate**. No platform code, no audio deps —
   the kernel depends on this for orchestration; sinks live in their binaries.
2. **Wire the directive.** New `BlockEvents` audio-play callback in
   `kaijutsu.capnp`; client forwarder plumbing in `subscriptions.rs`; `kj play`
   command that resolves a sample and emits it.
3. **App sink.** `BevyAudioOut` + the Bevy system; add the `wav` feature.
   **Verify a WAV plays end-to-end:** `kj play <wav>` → sound.
4. **(later) Edge-node agent sink.** A new peripheral binary (the `midi.md`
   "first kernel-owned compute node") attaches over RPC and runs `AlsaPcmOut` —
   Symphonia decode + `pawlsa`'s ALSA PCM loop; headless play with no GUI. The
   `alsa`/`pipewire`/`symphonia` deps live *here*, never in the kernel.
5. **(later) Fold into the Track.** Audio becomes a `RenderTarget` on the track
   (Stage 3), consuming committed score cells, scheduled against the speculation
   lead (`at = Some(instant)`). ABC→sample mapping / a sampler ("NoteOn → pick
   sample → play") is its own later cut — `midi.md` "samples-with-MIDI".

## The MIDI parallel (a consequence, not yet committed)

The same principle — *the kernel process owns no hardware FFI* — applies to MIDI,
which **already shipped in-process**: `AlsaMidiOut` lives in
`crates/kaijutsu-server/src/clock.rs` (M1, loopback-verified on zorak). Under
this lean, that ALSA-seq emission is a candidate to **extract into the same
edge-node agent** so the kernel/server binary stops linking `alsa`. The
speculation-lead scheduling (`last_fire_scheduled`/jitter-free `at`) travels with
it — which is exactly `midi.md`'s "the node near the gear regenerates fine
timing." This is a refactor of landed code; tracked in `docs/issues.md`, sequenced
with the M4 edge-node work, not part of the PCM slices above.

## Open questions (for the implementation session)

- **Inline vs CAS on the wire** — size threshold for `Encoded` vs `Cas`. Lean
  CAS ref for anything non-trivial; the kernel already stores assets in CAS.
- **Shared-types home** — new `kaijutsu-audio` crate vs `kaijutsu-types`
  (recommend the new crate to fence FFI deps).
- **Routing/volume** — `pawlsa`'s `pw` graph control (default endpoint, node
  volume/mute, links) is deferred to a later surface; first sound just plays to
  the default sink.
- **Timing** — first slice plays immediately (`at = None`); scheduled playback
  against the speculation lead arrives with the track integration (slice 5).

## Verification

- **End-to-end (slice 3):** with the app running (`./contrib/kj status`),
  `kj play <wav>` → audible sound; log/`world_query` confirms an `AudioPlayer`
  entity was spawned.
- **Decode unit test:** load one of `pawlsa`'s sample WAVs
  (`~/src/pawlsa-mcp/pawlsa-test.wav` et al.) through the chosen decoder.
- **(later) Kernel sink:** headless `kj play` on a node with no app attached
  produces sound via ALSA/PipeWire.
</content>
</invoke>
