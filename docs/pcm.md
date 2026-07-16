# Samples & clips — media on the mime-keyed render seam

> **Status:** as-built + the remaining design; aggressively rewritten
> 2026-07-16, absorbing `docs/clips.md` (merged here whole; it and this doc's
> earlier design generations — including the retired `docs/playback.md` — are
> recoverable from git history). Code is truth: the wire cue, the app sinks,
> and the clip record are **landed**; the open work is the clip *path* —
> producer → track → sink — mapped in "The remaining work" below. Companions:
> `docs/midi.md` ("Render is a wire cue" — the phase split; "The one
> timebase" — the timing doctrine every cue rides), `docs/tracks.md`
> (track/transport), `docs/hyoushigi.md` (the `Cell` substrate),
> `docs/chameleon.md` (vocabulary: **clip** = placed media on a track, DAW
> sense; "cue" stays chameleon's trap message), `docs/slash-v.md` (track B —
> the `/v/cas` mount + client fetcher, landed), `docs/cue-prior-art.md` (the
> seven-industry survey the clip record was synthesized from).

## One seam, as built

MIDI and samples are one render path. The kernel decides *what/when*; a sink
near the hardware does the physical emit. The kernel/server binary links no
`alsa`/`pipewire`/`symphonia` — the in-process `AlsaMidiOut`/`RenderTarget`
generation was demolished 2026-07-02 once the app sink proved parity.

What crosses the wire lives in the FFI-free `kaijutsu-audio` crate
(`src/lib.rs` — no audio deps, no tokio, nothing kernel-ward):

```rust
pub struct RenderCue {
    pub mime: String,        // dispatch key: audio/wav, text/vnd.abc, …clip+json
    pub payload: CuePayload, // Inline(Vec<u8>) | Cas(ContentHash)
    pub lead: Duration,      // sink fires at receipt + lead (ZERO = now)
    pub epoch_ns: u64,       // sender wallclock at emission; 0 = unstamped
}
pub trait RenderSink: Send {
    fn emit(&self, cue: RenderCue) -> anyhow::Result<()>;
}
```

- **Mime-keyed, content-agnostic.** ABC, a clip record, an inline sample —
  the sink dispatches on `mime`. The wire never carries raw decoded PCM;
  decoding lives at the sink (Bevy decoders / Symphonia). `RenderCue::Debug`
  prints payload byte *counts*, never bytes (log-safety, deliberate).
- **Bytes never ride the track, big payloads never ride inline.** `Cas` is
  the primary payload; the sink resolves it from a local XDG CAS cache, miss
  → SFTP `/v/cas/<ab>/<hash>` → re-hash verify (`docs/slash-v.md` track B).
  Inline is for symbolic content and tiny samples (threshold provisional at
  4 KiB — revisit if it chafes).
- **Timing rides the one timebase** (`docs/midi.md` — doctrine, not
  folklore). `lead` is *relative* (an `Instant` can't cross the wire);
  `epoch_ns` is the emission wallclock stamp, and the sink **backdates**:
  lead is spent down by the cue's measured age on receipt, past events drop,
  a >5 s-stale cue rejects whole (`midi.rs::backdate_events`). Cue `at`s
  derive from the *scheduled* beat grid, never a wakeup wallclock.
- **Flush is a cue.** `RENDER_FLUSH_MIME` with an empty payload — transport
  stop/pause tells every sink to drop scheduled-not-played events and
  silence sounding notes. The mime IS the message.
- **`emit(&self)`, not `&mut self`, is deliberate.** The Bevy sink acts
  through `Commands`; the ALSA sink's handle sits behind internal
  mutability. Don't "fix" it.

The delivery path: kernel publishes `BlockFlow::RenderCue` on the FlowBus →
both rpc bridges forward → `onRenderCue @13` (`kaijutsu.capnp`) → client
forwarder emits `ServerEvent::RenderCue` → app systems. It's a directive,
not a block — `matches_filter` bypasses it.

### Producers (kernel side, today)

- **`kj play <path>` / `kj play --cas <hash>`** (`kj/play.rs`) — play-now
  (`lead == ZERO`); mime by extension sniff (`AudioFormatHint`) or from CAS
  metadata. The standalone trigger and the debugging hammer.
- **The materialize crossing** (`kaijutsu-server/src/beat.rs
  publish_render_cues`) — for every cell that crossed the write barrier this
  beat: resolve the source bytes from durable CAS *kernel-side*, compute a
  jitter-free `at` off the scheduled grid (`base + (cell.start − playhead) ×
  period`, clamped ≥ now), stamp ONE `now`/`epoch_ns` pair for the whole
  batch, publish per cell. Subscriber-gated: a headless kernel with no sink
  attached skips the CAS reads entirely (the score is still durable — only
  the ephemeral render is skipped). **ABC-hardwired today**: `cref.mime !=
  ABC_MIME → skip` (beat.rs:1768). Lifting that *is* the clip track path
  (R3 below).
- **Transport stop/pause** — the flush cue, ungated (cheap, must always land).

### Sinks (today the app; later an edge node)

- **`kaijutsu-app/src/midi.rs`** — `text/vnd.abc`: renders ABC→MIDI *at the
  sink* (`kaijutsu_abc::midi::events`) and schedules into a local ALSA seq
  port at the backdated `receipt + lead`; ALSA's queue owns sub-ms timing.
  Flush drops scheduled events + all-notes-off. (Known future work: flush is
  whole-queue, not per-track — issues.md, relative-lead findings.)
- **`kaijutsu-app/src/audio.rs`** — `audio/*`: `Inline` spawns an
  `AudioPlayer` now (play-now parity); `Cas` resolves through `CasResolver`
  off the Bevy main thread and plays on resolve. **This first cut is
  fetch-on-cue, and `lead` is honored only at ZERO** — the prepare horizon
  (R4) and real sample scheduling (R5) are the open half. A `CLIP_MIME` cue
  warn+skips today (R1).
- **Edge-node agent** (headless ALSA, the `midi.md` M4 node) — later, slice
  4 unchanged: Symphonia decode + `pawlsa`'s proven ALSA PCM loop
  (`~/src/pawlsa-mcp/src/alsa/playback.rs`; its `pw` graph-control surface
  is the later routing/volume story). `alsa = 0.11` / `pipewire = 0.9` /
  `symphonia` land in the agent binary, never the kernel. Prerequisite: the
  node-agent RPC model (exists only by analogy).

## Shipped ledger

Details in git history + devlog ("The music stack — from one loop to a band
on the wire"; "The beat learns to carry its own clock").

- **Slices 1–3** (June 30 – July 1): `kaijutsu-audio` crate, wire directive,
  app sink — `kj play <wav>` heard live.
- **5a** (July 2): the play-now `PlayAudio` pair replaced by the mime-keyed
  wire `RenderCue`; `AudioFormatHint` off the wire (mime is free text).
- **5b** (July 2): the Shape A `Clip` record + validator (below) — landed,
  tested, awaiting its consumer.
- **5c** (July 2): the app became the first MIDI sink; parity proven on a
  real musician track; server-side `AlsaMidiOut` + `RenderTarget` trait +
  `kj transport render` + the server `alsa` dep **demolished**. CAS-audio
  fetch-on-cue prefetch landed same day (track B B4).
- **Phase-align** (July 15): `RenderCue.epoch_ns` + sink backdating + the
  stale ladder; the kernel grid went scheduled-periodic. Verified: 400
  click↔bass pairs, mean +0.2 ms, no drift.

## The clip record — Shape A

**A clip is a placed media reference on a track**: a committed hyoushigi
cell whose content is a small, human/model-readable symbolic record — "play
this CAS hash, from this offset, at this gain." The cell owns *where in
musical time* (`Cell.span`); the payload owns *what media and how to render
it*; the transport owns *when proposals fire* (quantization, follows); the
sink owns *making it sound*. Models author clip records as text, the same
way they author ABC.

**Landed** (`kaijutsu-audio/src/clip.rs`): `Clip`, `CLIP_MIME =
application/vnd.kaijutsu.clip+json`, `Clip::parse` /
`Clip::parse_validated`, `ClipError` — pure data, FFI-free, tested. **No
consumer yet**; that's R1–R3.

### `Cell` stays untouched — the mapping

The `cue-prior-art.md` survey found every industry re-inventing the same six
field clusters. They map onto what exists without touching the substrate
(expanding `Cell` would break hyoushigi's founding rule — a new modality
never edits the substrate):

| Convergent cluster | Where it lives | Status |
|---|---|---|
| identity | `BlockId` at materialization; `Cell.played_by` + `Cell.track` | exists |
| temporal anchor + duration | `Cell.span` — the *timeline placement* | exists |
| media reference | the payload's `media` hash | landed |
| trigger / advance rule | transport/producers, resolved at fire time — never the committed record | exists |
| param envelope | the payload's baked params (`gain_db`; fades/env are Shape B); *live* params are Shape C resolver territory | landed |
| human label | the payload's **required** `label` | landed |

The survey's strongest lesson holds: **timeline placement and source range
are separate concerns** (EDL's four timecodes, OTIO's `source_range`).
`Cell.span` is placement; `src_offset_ms`/`src_len_ms` are source range; the
payload never repeats the tick.

**The two-level reference:** the cell's `ContentRef` hashes the *clip record
itself* (immutability anchor + memoization key); the record's `media` field
hashes the *sample bytes*. Both CAS, different objects at different
altitudes.

### The schema (code is truth: `clip.rs`)

```jsonc
{
  "v": 1,                          // record version (per-record, OTIO-style)
  "media": "<32-hex ContentHash>", // REQUIRED — the sample bytes, in CAS
  "mime": "audio/wav",             // REQUIRED — what the sink decodes
  "label": "rimshot, dry",         // REQUIRED, non-empty — hashes are opaque;
                                   //   the label is how the score reads
  "src_offset_ms": 0,              // optional, default 0
  "src_len_ms": null,              // optional, default to-end
  "gain_db": 0.0,                  // optional, default 0.0 — dB, NOT linear
  "ext": {}                        // extension bag — unknown keys survive round-trips
}
```

Decisions stated out loud (silent answers breed complaints — the
Reaper/Ableton lesson):

- **Media-internal time is integer milliseconds.** Source range is
  wall-time-domain, not musical; floats invite fuzz; sub-ms trims are out of
  scope at this altitude.
- **Gain is dB** (`0.0` = unity). Consoles, Wwise, and humans speak dB.
- **Tempo-change default:** a clip is anchored to its `Tick` — a tempo change
  moves *where the clip starts in wall time*, never its internal playback
  rate. No stretch/repitch in v1; the `stretch` field name is **reserved**
  for Shape B.
- **Span vs source range precedence:** playback is governed by the source
  range, in full; `Cell.span.len` is the clip's advisory *musical footprint*
  (windowed reads, `KJ_HEARD`), not a truncation gate. Stopping sound early
  is the transport's job (the flush cue), not the record's.

### Validation — landed, fail loud

`Clip::parse` is structural: `v` known, `label`/`mime` non-empty; `media`
well-formedness is enforced by `ContentHash`'s validating deserialize (CAS
B0), so a malformed hash fails at parse. `Clip::parse_validated(json,
&dyn ContentStore)` adds **media present in CAS** — an absent sample fails
at schedule time, loudly, not two phrases later at prefetch. Unknown `ext`
keys pass through untouched.

### Fallback semantics

Same required `Fallback` as any recipe. `UseLastGood` on a clip lane repeats
the lane's last committed *clip record* — symbolic, cheap, media already in
every sink's cache from the first play (the vamp insurance carries over from
ABC unchanged). Fresh-lane default: `Skip` — silence until the first good
clip, matching the locked chameleon default.

### Growth path

- **Shape A → B, field-by-field, each with its consuming renderer:**
  `stretch` policy first, then loop braces, fades, clip-local envelopes;
  `color`/`notes` for the human cluster.
- **Shape C is a resolver milestone, not a payload change** — its output is
  Shape A (TTS, name→hash cue-sheet lookup, switch-like selection; the cue
  sheet is ordinary committed/config state). Gated on hyoushigi's reactive
  `compute_basis` open question.
- **Automation lanes stay separate cells** with an automation MIME on the
  same timeline — a clip's `ext`/`env` never grows into a second automation
  system.
- **An ABC-consuming sampler** ("NoteOn → pick sample → play") is a later
  mime on this same seam (`docs/midi.md` "samples-with-MIDI").

## How a clip plays — the target end-to-end

1. A producer commits a clip cell *through the validator* (model turn or the
   clip verb — R2). `parse_validated` runs at commit/schedule: absent media
   fails loud here.
2. The cell crosses the write barrier at the beat; the crossing publishes
   `RenderCue { CLIP_MIME, payload, lead, epoch_ns }` exactly as it does ABC
   — once R3 lifts the ABC-hardwire.
3. The sink parses the record, resolves `media` from its XDG cache (warm
   from the prepare horizon — R4), applies source range + gain, and fires at
   the backdated instant (R5).
4. Transport stop/pause flushes scheduled clips exactly as MIDI. No deriver
   is involved — a clip renders directly; there is no barrier-side sibling.

## The remaining work — the map

Ordered; each lands buildable. R1–R3 are the clip path end-to-end; R4–R5
make it musical.

- **R1 — sink clip path** (`kaijutsu-app/src/audio.rs`): handle `CLIP_MIME`
  — parse the record, resolve `media` through the existing `CasResolver`,
  apply `src_offset_ms`/`src_len_ms`/`gain_db`, play. Source range + gain
  need control below `AudioPlayer`'s spawn-and-forget (see decision 3).
- **R2 — producer verb**: commit a clip cell to a track's score — cas-put
  the media (or reference an existing hash), author the record through the
  validator, commit at a tick. Verb shape is decision 1.
- **R3 — crossing mime pass-through** (`beat.rs publish_render_cues`): lift
  the `ABC_MIME` filter so any crossed cell's mime rides the cue; ABC keeps
  its inline pre-resolve, a clip cell's record is small enough to inline
  (the *media* stays CAS). Regression: non-ABC cells must not have been
  silently load-bearing anywhere.
- **R4 — the prepare horizon** (two-phase, resolved 2026-07-02; restate on
  the epoch substrate): don't force one `lead` to be both jitter buffer and
  bulk-I/O window. A long **prepare** horizon (10–30 s — warm the sink's
  cache when the cell becomes *known*) is separate from the short **fire**
  lead (~100 ms, once the sample is verified local). Late-fetch policy is
  **skip-loud** (resolved): log the underrun and drop — never fire-late or
  stretch a transient. Decode runs off a pool; if not ready by `lead −
  safety`, fire the fallback.
- **R5 — sample lead scheduling**: Bevy's `AudioPlayer` has no scheduling
  primitive (the named build risk — issues.md, relative-lead findings), so
  honoring a non-zero `lead` for samples is net-new: delayed-spawn at ~16 ms
  frame granularity, or pierce to the rodio `Sink`. Decision 3.
- **Slice 4 — the edge-node sink** (unchanged, later): headless `kj play` on
  a node with no app produces sound, emitted by the agent binary. Waits on
  the node-agent RPC model (M4).

Adjacent prize (issues.md "Beat-tracking + local-model follow-ups"): once
clips exist, `kj audio beats` runs on clip media and seeds a track's tempo
from a reference recording.

### Decisions open for the implementation session

1. **Producer verb shape** — the thinnest part of the old design ("`kj play`
   grows a `--at`/clip-emitting form" was one sentence). Candidates: a `kj
   clip add <path|--cas> --track <t> --at <tick> --label …` verb vs growing
   `kj play`; either way the flow is cas-put → validate → commit cell. Does
   the musician path author clips in v1, or humans/verbs only?
2. **`prepare_at` tick vs `prepare_lead` duration** on the cue/record — left
   "(or)" in the 2026-07-02 design; pick one, stated on the epoch-stamped
   substrate (a prepare horizon keyed to scheduled wallclock, not receipt).
3. **Delayed-spawn vs rodio piercing** for R5 — note `src_offset`/`src_len`/
   `gain_db` need rodio-level control *regardless* (R1), which leans the
   whole question toward piercing once, for both.
4. **Standing law, carried**: two-phase horizons; skip-loud on late; 4 KiB
   inline threshold (provisional); snapshot `receipt` at parse time, before
   any fetch, so fetch latency never folds into audio jitter.

## Distributed listening — later

Survivors of the retired `playback.md` (2026-06-10 → retired 2026-07-01), to
pick up when listening goes multi-peer:

- **Every attached listener hears playback on their own output** — shared
  listening = shared context. The app sink is the first voice; N peer sinks
  is the generalization; the kernel never grows an audio stack.
- **Peer capability advertisement** — attach grows a general capabilities
  bag (accepted mimes, latency estimate) so the kernel knows which sinks
  take what. Keep the slot generic; rendering surfaces will want it too.
- **A capnp transport surface** (app buttons / spacebar) — also on
  `hyoushigi.md`'s not-yet list.
- **Routing** — default: all attached sinks of a context play; a
  `kj transport route <sink>` verb later. Volume/routing control reuses
  `pawlsa`'s PipeWire surface when it lands.
- **midi→pcm for dumb sinks** — deferred-PCM-cell vs budget-excepted-deriver
  shapes recorded in `docs/issues.md` (Hyoushigi section); the cell shape is
  favored (soundfont synthesis is heavy).
- **Out of scope then and now:** continuous streams (no natural tick
  coordinate — clips are objects); seek/rewind (the playhead is
  forward-only; revisiting the past is an export).

## Verification

- **R1–R3 end-to-end:** commit a clip cell on a playing track → the sample
  sounds on the beat through the app sink; `parse_validated` rejects an
  absent-media record loudly at commit.
- **R4:** a cache-cold multi-MB sample committed ahead of the playhead is
  fetched under the prepare horizon and fires on time; yanking the bytes
  late produces a logged skip, never a late fire.
- **R5:** a non-zero-lead sample cue fires within a frame of its backdated
  instant (BRP/log timestamps), matching the MIDI path's discipline.
- **Slice 4:** headless `kj play` on a node with no app produces sound via
  the agent, never the kernel binary.
