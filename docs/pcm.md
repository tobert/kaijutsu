# PCM — playing samples through the audio render seam

> **Status:** design direction, captured 2026-06-30 in a co-design session
> (Amy + Claude); refreshed 2026-07-01 (harmonization pass). Decisions are
> *directions*, not commitments — code is truth, this is where we're aiming.
> **No code yet**, and nothing blocks it: `tracks.md` Stage 3 M1 (the
> `RenderTarget` seam + `AlsaMidiOut`) landed 2026-06-30, which was this doc's
> only prerequisite. Companions: `docs/midi.md` ("ALSA for MIDI, PipeWire for
> samples"; output-first), `docs/tracks.md` (the `RenderTarget` seam this slots
> into — Stage 3 §"The render-target seam"), `docs/chameleon.md` ("MIDI/sample
> is a render of the score"). This doc also absorbed the surviving ideas from
> `docs/playback.md` (retired 2026-07-01 — see "Distributed listening", below).

## The insight

Sample playback is **not new machinery** — it is a second render sink alongside
the MIDI-out one that **already landed**: `tracks.md` Stage 3 M1 shipped the
`RenderTarget` trait + `AlsaMidiOut` + `add_render_target` (in
`crates/kaijutsu-server/src/render.rs`, loopback-verified on zorak). `midi.md`
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
headless. The kernel **crate** never links `alsa`/`pipewire`/`symphonia` — and after
the M4 extraction ("The MIDI parallel", below) the server binary won't either;
today `AlsaMidiOut`'s ALSA FFI lives in `kaijutsu-server`, acknowledged there.

## The seam

The landed `RenderTarget` (in `crates/kaijutsu-server/src/render.rs`) consumes a
**committed score cell** + a local instant — perfect for slice 5, wrong for the
standalone first slice, which has a *sample*, not a cell. So the standalone slice
gets its own narrow seam, `AudioRenderTarget`, that converges onto `RenderTarget`
when audio becomes a track render. Same posture as the MIDI one (a render is a
*consumer*, never a producer; it receives resolved bytes + a local instant, never
re-resolves CAS):

```rust
/// One audio sink. Implemented in the app (Bevy) and, later, the edge-node agent (ALSA).
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

- **`play(&self)` vs `emit(&mut self)` is deliberate, not an oversight.** The
  Bevy sink spawns entities via `Commands` (world access, not sink-struct
  mutation) and the edge-node agent's ALSA handle lives behind internal
  mutability — unlike `AlsaMidiOut`, which mutates its seq connection inline.
  Don't "fix" the asymmetry.
- **`AudioFormatHint` values align with Symphonia's codec set** (`Wav | Flac |
  Mp3 | Ogg | Aac` to start); the wire MIME derives from it (`audio/wav`, …).

The trait + `AudioRef` + format types live in the **`kaijutsu-audio` crate
(confirmed — no longer an open question)**, which is **FFI-free**: no
`alsa`/`pipewire`/`symphonia`. Its dependency set is `kaijutsu-cas` (for
`ContentHash`) + `kaijutsu-types`, nothing kernel-ward — never
`kaijutsu-hyoushigi`/`-kernel`/`-server`. The FFI deps live only in the
consuming binaries (`kaijutsu-app`, the edge-node agent), so the kernel can
depend on `kaijutsu-audio` for orchestration without ever linking a hardware
library.

### How it converges — the mime-keyed seam (decided 2026-07-01)

The slice-5 fold-in is decided, and it is small: **the track's render seam
becomes mime-keyed over symbolic content; bytes never ride the track.**
`RenderTarget::emit(abc: &str, at)` grows a mime — `emit(content: &str, mime:
&str, at)` — and targets register by MIME: `AlsaMidiOut` takes `text/vnd.abc`
and behaves exactly as today; an audio target takes a **clip** MIME (a small
symbolic cue: CAS hash + placement/params — "play `<hash>` at this tick, this
gain"), parses it, resolves the hashes it names, and fires at `at`. A committed
sample is therefore a *clip cell* — notation, like ABC — never inline audio
bytes. The score stays symbolic ("hearing is symbolic," `docs/chameleon.md`),
and this is the render-side twin of the mime-keyed `DeriverRegistry` and the
decouple-Act-from-ABC axis: one generalization, both sides of the barrier.
(Vocabulary: **"clip"** = a placed media reference on a track, DAW sense —
"cue" stays reserved for chameleon's trap messages.)

**Sample bytes move out-of-band, prefetched under the speculation lead.** Cells
commit ahead of the playhead, so `emit` fires with `at` in the near future —
that lead is the transfer budget. A sink checks a client-side CAS cache (XDG
dir), pulls a miss over **SFTP against `/v/blobs/<hash>`** (the VFS already
mounts CAS and SFTP already speaks the VFS — zero new RPC; sshfs is the
prototyping path), and fires on time. Same constraint-remover as MIDI output:
the network delivers *ahead of time*, never just-in-time. Consequence for the
wire type above: `AudioRef::Cas` + the client cache is the primary path even in
the standalone slices; `Encoded` inline bytes shrink to a tiny-sample
convenience. Late-fetch behavior (skip loud vs fire late) is decided at the
first real sink — the fallback machinery is the natural hook.

The **clip payload format** is deliberately unspecified until its first
consumer (the lands-with-its-consumer rule). Prior art to mine when that
session comes: **OpenTimelineIO** (media-reference indirection, JSON),
**Csound score / TidalCycles mini-notation** (musical-time textual events with
sample refs + params), **WebVTT** (the minimal id + time-range + payload cue
shape), **SMIL/TTML** (the W3C XML timing vocabulary), **QLab / MIDI Show
Control / MOS** (trigger + rundown semantics). None carry CAS refs or musical
`Tick` natively — expect a small data MIME that borrows vocabulary, not an
adopted format.

## Reusable code from `~/src/pawlsa-mcp`

- `src/alsa/playback.rs` — ALSA PCM open + write loop (`alsa` crate 0.11). The
  edge-node sink's output stage. (Its hand-rolled `parse_wav` we **replace with
  Symphonia** per the decoding decision. It's a blocking loop — confirm
  threading when extracting; it runs beside the agent's RPC runtime.)
- `src/pw/mod.rs` — `spawn_pw_thread`/`PwHandle`: link create/destroy, node
  volume/mute, device profile/route via SPA pods (`pipewire` crate 0.9). **Not
  needed for the first sound**; this is the later routing/volume control surface.
- Deps to pull when the edge-node sink lands: `alsa = "0.11"`, `pipewire = "0.9"`,
  `symphonia` (decode). These land in the agent binary, never the kernel.

## The wire (standalone first)

The kernel→app push channel already exists: `BlockEvents` (server→client
subscription) carries `KernelOutputEvent` via the `on_output` callback, consumed
by `BlockEventsForwarder` in `crates/kaijutsu-client/src/subscriptions.rs`.

- **Add an audio-play directive** as a sibling event on that channel — a new
  `BlockEvents` callback method, **not** `KernelOutput` (that interface is keyed
  by `execId` and carries shell stdout/stderr; wrong semantic domain) — carrying
  `AudioRef`: a `Cas` ref as the primary form (the client cache fetches — see
  "How it converges"), `Encoded` inline only for tiny samples. Schema in
  `kaijutsu.capnp`.
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
  `MessageReader`/`MessageWriter` rename, per CLAUDE.md.) Slice-3 checklist
  item: **verify `AudioPlugin` is actually in the app's plugin set** (it rides
  `DefaultPlugins` when `bevy_audio` is enabled, but neither doc nor code has
  confirmed the app's plugin group includes it — check `main.rs` first).
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
   `alsa`/`pipewire`/`symphonia` deps live *here*, never in the kernel. Two
   flagged reconciliations for this slice: the **node-agent RPC model** is a
   named prerequisite (see `midi.md` M4 — it exists only by analogy today), and
   the **`alsa` crate version** (pawlsa rides `0.11`; the in-tree M1 target
   rides `0.9` via bevy/cpal).
5. **(later) Fold into the Track — the mime-keyed emit (see "How it
   converges").** Add the mime parameter to `RenderTarget::emit` and register
   targets by MIME. An audio target consumes **clip cells** (CAS ref +
   placement), prefetches bytes under the speculation lead
   (`at = Some(instant)`), and fires locally. An ABC-consuming sampler
   ("NoteOn → pick sample → play") is simply a second target on the same seam —
   `midi.md` "samples-with-MIDI".

## The MIDI parallel (a consequence, not yet committed)

The same principle — *the kernel process owns no hardware FFI* — applies to MIDI,
which **already shipped in-process**: `AlsaMidiOut` lives in
`crates/kaijutsu-server/src/render.rs` (M1, loopback-verified on zorak). Under
this lean, that ALSA-seq emission is a candidate to **extract into the same
edge-node agent** so the kernel/server binary stops linking `alsa`. The
speculation-lead scheduling (`last_fire_scheduled`/jitter-free `at`) travels with
it — which is exactly `midi.md`'s "the node near the gear regenerates fine
timing." This is a refactor of landed code; tracked in `docs/issues.md`, sequenced
with the M4 edge-node work, not part of the PCM slices above.

## Distributed listening — later (absorbs `playback.md`, retired 2026-07-01)

`docs/playback.md` (2026-06-10, git history) designed multi-listener playback
before the track/`RenderTarget` architecture existed. Its *mechanism* decisions
are superseded: scheduling rides the materialize crossing and kernel-push render
targets now, not sink-side block subscription + CAS prefetch; and its
pause=mute / stop=clock-freeze **verb remap never happened** — `tracks.md`
Stage 1 decided transport differently (`stop` stops the track's clock with
rotation suspended; both `stop` and `pause` flush render targets). A sink-side
mute would be *new, presentation-only* state, not a remap of `pause`. What
survives, to pick up when listening goes multi-peer:

- **Every attached listener hears playback on their own output** — shared
  listening = shared context. The app sink (slice 3) is the first voice; N
  peer sinks is the generalization, and the kernel (headless systemd service)
  still never needs an audio stack.
- **Peer capability advertisement.** `PeerConfig` is nick-only today; attach
  grows a *general* capabilities bag (accepted formats, optional latency
  estimate) so the kernel knows which sinks take `audio/midi` vs PCM. Keep the
  slot generic — rendering surfaces will want it too.
- **A capnp transport surface** (app buttons / spacebar) — subscribable
  transport state beyond `kj transport`. Also on `hyoushigi.md`'s not-yet list.
- **Routing.** Default: all attached sinks of a context play. A
  `kj transport route <sink>` verb for targeted output (room speakers vs
  headphones) comes later.
- **midi→pcm for dumb sinks.** Smart sinks synth `audio/midi` themselves;
  PCM-only sinks need a midi→pcm step — the deferred-PCM-cell vs
  budget-excepted-deriver shapes are recorded in `docs/issues.md` (Hyoushigi
  section). Soundfont synthesis is heavy, so the cell shape is favored.
- **The metronome slice.** A 拍子木 click in the app on the beat remains the
  cheapest audible clock-sync test harness when peer sinks land.
- **Out of scope then and now:** continuous streams (no natural tick
  coordinate — clips are still objects); seek/rewind (the playhead is
  forward-only; revisiting the past is an export).

## Open questions (for the implementation session)

- **Inline vs CAS on the wire** — the lean is DECIDED ("How it converges"):
  `Cas` primary + a client-side XDG CAS cache; `Encoded` only for tiny samples.
  Residual: the actual size threshold number.
- **The clip payload format** — deferred to its first consumer; prior-art
  shortlist in "How it converges". Its design session pairs with the
  decouple-Act-from-ABC generalization (`docs/issues.md` — same
  content-type-keyed move, input side).
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
- **(later) Edge-node sink:** headless `kj play` on a node with no app attached
  produces sound via ALSA/PipeWire — emitted by the edge-node agent, never the
  kernel binary.
