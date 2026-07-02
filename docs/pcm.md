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

The trait + `AudioRef` + format types live in the **`kaijutsu-audio` crate**
(landed, slice 1), which is **FFI-free**: no `alsa`/`pipewire`/`symphonia`,
no `tokio` (the trait's `at` is a `std::time::Instant`; sinks convert). Its
dependency set is `kaijutsu-cas` (for `ContentHash`) — `kaijutsu-types` is
allowed but not currently pulled (nothing in it needs it yet) — plus `anyhow`
(the trait's `Result`) + `serde` (the wire/record derives); nothing
kernel-ward, never `kaijutsu-hyoushigi`/`-kernel`/`-server`. The FFI deps live only in the
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
dir), pulls a miss over **SFTP against `/v/blobs/<ab>/<hash>`** (the `/v/blobs`
CAS mount + the client fetcher/cache are designed in `docs/slash-v.md` track B —
the mount does **not** exist yet; SFTP already speaks the VFS, so it's zero new
RPC, and sshfs is the prototyping path), and fires on time. Same constraint-remover as MIDI output:
the network delivers *ahead of time*, never just-in-time. Consequence for the
wire type above: `AudioRef::Cas` + the client cache is the primary path even in
the standalone slices; `Encoded` inline bytes shrink to a tiny-sample
convenience. Late-fetch behavior (skip loud vs fire late) is decided at the
first real sink — the fallback machinery is the natural hook.

The **clip payload is designed: `docs/clips.md`** (Shape A —
`application/vnd.kaijutsu.clip+json`: media hash + mime + required label +
source range + gain, versioned record + extension bag), synthesized from the
industry survey in `docs/cue-prior-art.md`. The *code* still lands with its
first consumer (slice 5); the schema, validation rules, tempo-change default,
and growth path (stretch policy → Shape B; late-bound cues = Shape C
resolvers) are locked there so the session starts from a map.

**The seam is a wire cue, and MIDI joins it (decided 2026-07-01).** The
convergence above is realized not as a mime growing on the *in-process*
`RenderTarget::emit` but as a **`RenderCue { mime, payload, lead }` directive
that crosses the wire to an off-box sink** — because the same real-time stance
that lets a sample prefetch under the lead also says MIDI's hardware emit never
needed to be in-process. So this is no longer "the samples half": **MIDI and
samples are one render path**, both mime-keyed cues on the lead, and the
in-process `RenderTarget` trait (`tracks.md`) dissolves into the wire cue as the
sink moves to the app (now) and an edge node (later). The full statement — the
`RenderCue` shape, the compose/render/emit phase split (with `abc→midi` staying
kernel-side for now), and why MIDI becomes sink-dependent — lives in
`docs/midi.md` "Render is a wire cue; the sink owns the hardware." This doc's
slices below are the samples-shaped view of that one seam.

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

- `kaijutsu-app` enables Bevy `"default"` features. **Checklist item RESOLVED
  (2026-07-01):** `AudioPlugin` *is* in the plugin set — `default` → `audio =
  ["bevy_audio", "vorbis"]` → `bevy_audio` (verified against `~/src/bevy`
  Cargo.toml), and `main.rs` uses `DefaultPlugins` disabling only `LogPlugin`,
  so no plugin-group work is needed. **But the only decoder `default` turns on
  is `vorbis` (Ogg) — `wav` is NOT enabled**, so slice 3 MUST add `"wav"` to
  `crates/kaijutsu-app/Cargo.toml`'s bevy features (`["default", "bevy_ui_debug",
  "wav"]`) or a WAV `AudioSource` won't decode. (`AudioPlayer` component +
  `AudioSource` asset + `AudioSink`; remember 0.18's `Message`/`MessageReader`/
  `MessageWriter` rename, per CLAUDE.md.)
- **Symphonia later:** rodio (under `bevy_audio`) has a Symphonia backend; enable
  the corresponding Bevy/rodio format features (FLAC/MP3/OGG/AAC) when we want
  them in `-app`. Same crate (`symphonia`) is the kernel sink's decoder.

## Slices

1. **Seam + types. ✅ landed.** `AudioRenderTarget` trait + `AudioRef`/`AudioFormatHint`
   in the new **FFI-free `kaijutsu-audio` crate**. No platform code, no audio deps —
   the kernel depends on this for orchestration; sinks live in their binaries.
   `AudioFormatHint` carries the `mime()`/`from_mime()`/`from_path_extension()`
   round-trip (the wire MIME + `kj play` extension sniff). 7 tests green.
2. **Wire the directive. ✅ landed.** New `BlockEvents.onPlayAudio @13` in
   `kaijutsu.capnp` (+ wire `AudioRef` union encoded|casHash + `AudioFormatHint`
   enum); the directive rides a new `BlockFlow::PlayAudio` on the in-process
   **FlowBus** (not a client-callback list) — both `rpc.rs` bridges forward it,
   `matches_filter` bypasses it (a directive, not a filterable block). Client
   forwarder `on_play_audio` → `ServerEvent::PlayAudio` in `subscriptions.rs`;
   `kj play <path>` resolves a sample (sniff-before-read, fail loud) and emits it.
3. **App sink. ✅ landed + live-verified.** `crates/kaijutsu-app/src/audio.rs`
   (`AudioOutPlugin` + a `MessageReader<ServerEventMessage>` system → `AudioSource`
   + `AudioPlayer`/`PlaybackSettings::DESPAWN`); added the `wav` feature. **Verified
   live on the runner box (2026-07-01):** `kj play pawlsa-test.wav` → "209850 bytes,
   audio/wav — 2 listener(s)"; BRP `world_query` caught the spawned entity
   (`PlaybackSettings{ mode: Despawn }` — `AudioPlayer<AudioSource>` is a generic,
   so not reflection-registered; query `PlaybackSettings` instead). Audible-sound
   confirmation: Amy (speakers on the Wayland box).
4. **(later) Edge-node agent sink.** A new peripheral binary (the `midi.md`
   "first kernel-owned compute node") attaches over RPC and runs `AlsaPcmOut` —
   Symphonia decode + `pawlsa`'s ALSA PCM loop; headless play with no GUI. The
   `alsa`/`pipewire`/`symphonia` deps live *here*, never in the kernel. Two
   flagged reconciliations for this slice: the **node-agent RPC model** is a
   named prerequisite (see `midi.md` M4 — it exists only by analogy today), and
   the **`alsa` crate version** (pawlsa rides `0.11`; the in-tree M1 target
   rides `0.9` via bevy/cpal).
5. **Fold into the Track — the wire `RenderCue` (see "How it converges" +
   `midi.md` "Render is a wire cue").** This is the unified render seam, and it
   subsumes MIDI. Natural sub-slices, each landing buildable:
   - **5a — the cue seam. ✅ landed.** The play-now `PlayAudio`/`AudioRef` pair
     was replaced outright by a mime-keyed `RenderCue { mime, payload:
     Inline | Cas, lead }` in `kaijutsu-audio`, over the FlowBus/`BlockEvents`
     (`onPlayAudio@13` → `onRenderCue@13`; the `AudioFormatHint` *wire* enum
     dropped — `mime` is free Text, so the wire is content-agnostic). `lead` is
     a relative `Duration` (nanos on the wire; a process-local `Instant` can't
     cross it); the sink schedules at `receipt + lead`. `kj play` stays play-now
     (`lead == ZERO`) with an inline payload; the app sink dispatches by mime
     (inline `audio/*` plays, CAS + non-audio warn+skip = 5c). `AudioFormatHint`
     survives as the kj-play extension→MIME sniff helper (off the wire); the
     FFI-free trait is now `RenderSink::emit(&self, RenderCue)`.
   - **5b — the clip record + validator. ✅ landed.** Shape A
     (`docs/clips.md`) `Clip` record + the content-type-keyed validator in
     `kaijutsu-audio` (pure data, FFI-free). `Clip::parse` is structural (v
     known, label/mime non-empty, media a well-formed hash);
     `Clip::parse_validated(json, &dyn ContentStore)` adds "media present in
     CAS" (fail loud at schedule) — the presence check lives here because
     `ContentStore::exists` is a pure trait boundary, keeping it unit-testable
     against a stub. (Code is truth: `ContentHash` is serde-transparent and
     does NOT validate on deserialize — well-formedness is checked explicitly;
     and a hash is 32 hex chars, not the "64-hex" this doc/clips.md loosely say.)
   - **5c — the MIDI sink + demolition. ✅ landed (2026-07-02, live on zorak).**
     The app is the first MIDI sink: it renders `text/vnd.abc` cues to MIDI (same
     `kaijutsu_abc::midi::events` path — the app already deps `kaijutsu-abc`, so
     render-at-sink beat shipping SMF) and schedules into a local ALSA seq port at
     `receipt + lead`; `kj play *.abc` is the standalone trigger (5c-1). The
     materialize crossing publishes a `RenderCue` per crossed cell (keyed by the
     track's score context) + a `RENDER_FLUSH_MIME` cue on stop/pause, so a real
     musician track plays through the app with **no** in-process target (5c-2).
     With parity proven, the server-side `AlsaMidiOut` + `RenderTarget` trait +
     `kj transport render` verb + the `alsa` server dep were demolished — the
     kernel/server binary links no audio/MIDI FFI (5c-3). **Still ahead in 5c:**
     the *clip/PCM* half — clip cue → parse+validate → resolve `media` from an XDG
     CAS cache, miss → SFTP `/v/blobs/<ab>/<hash>` (the mount + `BlobResolver` are
     `docs/slash-v.md` track B, the active prerequisite), decode, fire honoring
     `lead` (the CAS prefetch under the prepare horizon); and the headless
     edge-node sink (slice 4 / `midi.md` M4) so a kernel with no app can still
     make sound.
   The `abc→midi` render stays kernel-side for now (a relocatable phase — see the
   three-phase split in `midi.md`); an ABC-consuming sampler ("NoteOn → pick
   sample → play") is a later mime on the same seam (`midi.md` "samples-with-MIDI").

## The MIDI parallel — now the plan, not a later refactor (2026-07-01)

The same principle — *the kernel process owns no hardware FFI* — applies to MIDI,
which **already shipped in-process**: `AlsaMidiOut` lives in
`crates/kaijutsu-server/src/render.rs` (M1, loopback-verified on zorak). We
**committed to converging them** (2026-07-01, with the real-time stance in
`docs/midi.md`): MIDI's ALSA-seq emission moves off the server binary onto the
**same wire sink** samples use, so the kernel/server binary stops linking `alsa`
and MIDI and PCM ride one `RenderCue`. The speculation-lead scheduling
(`last_fire_scheduled`/jitter-free `at`) travels with it as a *relative lead* the
sink re-anchors — exactly `midi.md`'s "the node near the gear regenerates fine
timing."

The unlock that de-risked it: **the app is the first MIDI sink, so this did NOT
wait on the M4 edge node.** This **landed 2026-07-02 (slice 5c)**, live on zorak:
the app renders `text/vnd.abc` cues to MIDI into a local ALSA seq port (→
`aconnect` → TiMidity, same box), the materialize crossing publishes the cues
side-by-side with the in-process target, parity was verified on a real musician
track, and then the server-side `AlsaMidiOut` + `RenderTarget` trait + the `alsa`
server dep were demolished. The edge-node agent becomes *just another sink* on
the same protocol — the headless sink for MIDI and PCM alike — still ahead
(slice 4 / M4). Full design + phase split: `docs/midi.md` "Render is a wire cue."

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
- **The clip payload format** — RESOLVED: designed in `docs/clips.md`
  (Shape A), research record in `docs/cue-prior-art.md`. The clip *validator*
  is a voice of the decouple-Act-from-ABC generalization (`docs/issues.md` —
  same content-type-keyed move, input side).
- **Prefetch vs. fire — RESOLVED (timing analysis, 2026-07-02): two-phase.** Do
  not force one `lead` to be both jitter buffer and bulk-I/O window. A cache-cold
  multi-MB SFTP fetch can exceed the fire lead; so a long-lead **prepare/preload**
  horizon (10–30 s, warm the XDG CAS cache when the cell becomes *known*) is
  separate from the short **fire** `lead` (~100 ms, once the sample is verified
  local). Same relative-lead substrate, two horizons — the clip cue / `docs/clips.md`
  grows a `prepare_at` tick (or `prepare_lead`). Snapshot `receipt` at parse-time,
  *before* the fetch, or the fetch latency folds into audio jitter. Full findings:
  `docs/issues.md` → Hyoushigi / Musician.
- **Late-fetch policy — RESOLVED (timing analysis, 2026-07-02): skip-loud.** If the
  sample isn't decoded and local by its deadline, log the underrun and **drop** it —
  never fire-late or time-stretch a transient (a late kick destroys the groove; a
  stretched transient sounds worse). The fallback machinery is the hook. Decode runs
  off a pool, never blocking the scheduling frame; if it isn't ready by
  `lead − safety`, fire the fallback.
- **Routing/volume** — `pawlsa`'s `pw` graph control (default endpoint, node
  volume/mute, links) is deferred to a later surface; first sound just plays to
  the default sink.
- **Timing — RESOLVED (2026-07-02): the substrate is the relative-lead wire cue,
  and it's sound.** First slice plays immediately (`at = None`); scheduled playback
  fires at `receipt + lead`. The timing model was analyzed before building on it
  (`docs/midi.md` "The relative-lead timebase, analyzed" + `docs/issues.md`); the
  one net-new build item is that Bevy's `AudioPlayer` has no scheduling primitive,
  so honoring `lead` for *samples* needs a scheduler the MIDI path (ALSA seq queue)
  gets for free.

## Verification

- **End-to-end (slice 3):** with the app running (`./contrib/kj status`),
  `kj play <wav>` → audible sound; log/`world_query` confirms an `AudioPlayer`
  entity was spawned.
- **Decode unit test:** load one of `pawlsa`'s sample WAVs
  (`~/src/pawlsa-mcp/pawlsa-test.wav` et al.) through the chosen decoder.
- **(later) Edge-node sink:** headless `kj play` on a node with no app attached
  produces sound via ALSA/PipeWire — emitted by the edge-node agent, never the
  kernel binary.
