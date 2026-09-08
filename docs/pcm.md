# Samples & clips — media on the mime-keyed render seam

Companions: `docs/midi.md` ("Render is a wire cue" — the phase split; "The
one timebase" — the timing doctrine every cue rides), `docs/tracks.md`
(track/transport), `docs/hyoushigi.md` (the `Cell` substrate),
`docs/chameleon.md` (vocabulary: **clip** = placed media on a track, DAW
sense; "cue" stays chameleon's trap message), `docs/slash-v.md` (the
`/v/cas` mount + client fetcher), `docs/cue-prior-art.md` (the survey the
clip record was synthesized from), `docs/audio-daemon.md` (who owns
hardware now — see below), `docs/devlog.md` ("The music stack — from one
loop to a band on the wire", "The beat learns to carry its own clock" for
how this shipped).

## One seam, as built

MIDI and samples are one render path. The kernel decides *what/when*; a sink
near the hardware does the physical emit. The kernel/server binary links no
`alsa`/`pipewire`/`symphonia` dependency at all — hardware emit lives
entirely in `kaijutsu-audiod` (`docs/audio-daemon.md`).

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
  decoding lives at the sink. `RenderCue::Debug` prints payload byte
  *counts*, never bytes (log-safety, deliberate).
- **Bytes never ride the track, big payloads never ride inline.** `Cas` is
  the primary payload; the sink resolves it from a local XDG CAS cache, miss
  → SFTP `/v/cas/<ab>/<hash>` → re-hash verify (`docs/slash-v.md`). Inline
  is for symbolic content and tiny samples (threshold provisional at 4 KiB
  — revisit if it chafes).
- **Timing rides the one timebase** (`docs/midi.md` — doctrine, not
  folklore). `lead` is *relative* (an `Instant` can't cross the wire);
  `epoch_ns` is the emission wallclock stamp, and the sink **backdates**:
  lead is spent down by the cue's measured age on receipt, past events drop,
  a >5 s-stale cue rejects whole. Cue `at`s derive from the *scheduled* beat
  grid, never a wakeup wallclock.
- **Flush is a cue.** `RENDER_FLUSH_MIME` with an empty payload — transport
  stop/pause tells every sink to drop scheduled-not-played events and
  silence sounding notes. The mime IS the message.

The delivery path: kernel publishes `BlockFlow::RenderCue` on the FlowBus →
both rpc bridges forward → `onRenderCue @9` (`kaijutsu.capnp`) → client
forwarder emits `ServerEvent::RenderCue` → audio runtime. It's a directive,
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
  the ephemeral render is skipped). Mime-agnostic: every crossed cell's own
  mime rides its cue; sinks dispatch and ignore what isn't theirs.
- **Transport stop/pause** — the flush cue, ungated (cheap, must always land).

### Sinks — all inside `kaijutsu-audiod`

`kaijutsu-audiod` is the sole hardware owner; the app has no hardware I/O
(`docs/audio-daemon.md`). Device ownership, capture and Linux service setup
live there; this doc stays the wire/record contract.

- **`kaijutsu-audio-runtime/src/dj/midi.rs`** — `text/vnd.abc`: renders
  ABC→MIDI *at the sink* (`kaijutsu_abc::midi::events`) and schedules into a
  local ALSA seq port at the backdated `receipt + lead`; ALSA's queue owns
  sub-ms timing. Flush drops scheduled events + all-notes-off. (Flush is
  whole-queue, not per-track yet — `docs/issues.md`, "Audio nodes —
  follow-up after daemon extraction".)
- **`kaijutsu-audio-runtime/src/dj/audio.rs` + `src/audio_sched.rs`** —
  `audio.rs` is pure dispatch: it computes each cue's epoch-backdated
  deadline at receipt (same ladder as `midi.rs`, collapsed to
  go/no-go/when), resolves CAS payloads through `CasResolver`, warms the
  cache ahead of time on a `PREPARE_MIME` cue, parses `CLIP_MIME` records
  and applies their source range + gain, then hands everything past the
  skip-loud gate (below) to `audio_sched.rs` — a dedicated thread owning the
  rodio `OutputStream` with a deadline heap (decode-ahead, `Sink`-per-sound
  polyphony, flush drops pending + stops live).

## The clip record — Shape A

**A clip is a placed media reference on a track**: a committed hyoushigi
cell whose content is a small, human/model-readable symbolic record — "play
this CAS hash, from this offset, at this gain." The cell owns *where in
musical time* (`Cell.span`); the payload owns *what media and how to render
it*; the transport owns *when proposals fire* (quantization, follows); the
sink owns *making it sound*. Models author clip records as text, the same
way they author ABC.

Landed (`kaijutsu-audio/src/clip.rs`): `Clip`, `CLIP_MIME =
application/vnd.kaijutsu.clip+json`, `Clip::parse` /
`Clip::parse_validated`, `ClipError` — pure data, FFI-free, tested.

### `Cell` stays untouched — the mapping

The `cue-prior-art.md` survey found every industry re-inventing the same six
field clusters. They map onto what exists without touching the substrate
(expanding `Cell` would break hyoushigi's founding rule — a new modality
never edits the substrate):

| Convergent cluster | Where it lives |
|---|---|
| identity | `BlockId` at materialization; `Cell.played_by` + `Cell.track` |
| temporal anchor + duration | `Cell.span` — the *timeline placement* |
| media reference | the payload's `media` hash |
| trigger / advance rule | transport/producers, resolved at fire time — never the committed record |
| param envelope | the payload's baked params (`gain_db`; fades/env are Shape B); *live* params are Shape C resolver territory |
| human label | the payload's **required** `label` |

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

### Validation — fail loud

`Clip::parse` is structural: `v` known, `label`/`mime` non-empty; `media`
well-formedness is enforced by `ContentHash`'s validating deserialize, so a
malformed hash fails at parse. `Clip::parse_validated(json, &dyn
ContentStore)` adds **media present in CAS** — an absent sample fails at
schedule time, loudly, not two phrases later at prefetch. Unknown `ext`
keys pass through untouched.

### Fallback semantics

Same required `Fallback` as any recipe. Every placed clip carries `Skip` — a
missed resolve is silence (the engine drops the cell without wedging the
lane; a resolve *error* is loud, bypassing fallback entirely). A placed
one-shot must never vamp-repeat. `UseLastGood` on a clip lane — repeating
the lane's last committed *clip record*, media already in every sink's
cache — remains a coherent future option for a lane that wants vamp
insurance (a looping percussion lane, say), but nothing authors it today; it
would come with the verb growing a flag, not as a default.

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

## How a clip plays

1. A producer commits a clip cell through the validator (a model turn or `kj
   play --track`). `parse_validated` runs at commit/schedule: absent media
   fails loud here.
2. The cell crosses the write barrier at the beat; the crossing publishes
   `RenderCue { CLIP_MIME, payload, lead, epoch_ns }` exactly as it does ABC.
3. The sink parses the record, resolves `media` from its XDG cache (warmed
   ahead of the fire cue by a separate `PREPARE_MIME` cue at commit time —
   below), applies source range + gain, and fires at the backdated instant.
4. Transport stop/pause flushes scheduled clips exactly as MIDI. No deriver
   is involved — a clip renders directly; there is no barrier-side sibling.

## `kj play --track` — authoring a clip

Bare `kj play` stays play-now; `--track <t>` commits a clip cell instead:
cas-put the media (or take `--cas`), author the record through the
validator, commit via `schedule_clip_cell` (`hyoushigi/mod.rs`, the sibling
of `schedule_abc_cell` — eager `parse_validated` → armed-track lookup → CAS
store → schedule, but `Fallback::Skip`, never `UseLastGood`). `--at <tick>`
places into the future; omitted, it defaults to ASAP = `playhead + 1`,
computed inside the timeline lock (no TOCTOU). `label` defaults to the file
stem (`--label` overrides; `--cas` with no derivable name requires it).
`kj play --track` is ungated, same as `kj play`/`kj cas` (capabilities are
focus nudges in a shared-trust kernel, not authorization).

## The prepare horizon and skip-loud policy

One `lead` cannot be both a jitter buffer and a bulk-I/O window, so the
prepare signal is its own wire directive at commit time
(`kaijutsu_audio::PREPARE_MIME`): the moment `schedule_clip_cell`
validates+commits a clip cell, the kernel publishes a tiny prepare cue
(payload = the media hash, `lead` ZERO, `epoch_ns` stamped) unconditionally
— a hash, no CAS read, nothing expensive to gate. Every sink warms its XDG
cache from it. The render cue at the crossing is unchanged and carries only
the short *fire* lead.

Late-fetch policy is skip-loud: `decide_deadline(deadline, stamped, now) ->
Fire | DropLate` (`audio_sched.rs`) drops a *musically-placed* cue (its
`RenderCue` stamped `epoch_ns`) whose media lands more than `GRACE` (100 ms
— scheduler wakeup slop, not fetch latency) past its backdated deadline —
logged, naming the hash and how late, and never fired stale; an unstamped
cue (`kj play --cas`'s asap semantics) still fires however late, unchanged.
Each `CasResolver::resolve` attempt is separately bounded by a
`FETCH_TIMEOUT` (10 s — a generous transfer bound, distinct from the
musical deadline above), folding a timeout into the same transport-error
bucket the one-redial retry already handles; `resolve` returns `(bytes,
source)` naming whether it was a cache hit or a fetch, so the happy path
logs which and how long.

The audio scheduler thread (`audio_sched.rs`) holds the rodio `OutputStream`
and a deadline-ordered heap; play-now and scheduled/trimmed/gained playback
are one path, timing lives below any frame loop.

## Distributed listening — later

Local retained PCM input and window export have an execution plan in
`docs/audio-daemon.md`, "Evolution: observe once, retain windows, request
material" — bounded local history exported as immutable objects on
request, not continuous network playback. Design notes carried forward for
when *listening* (playback fanout) goes multi-peer:

- **Every attached listener hears playback on their own output** — shared
  listening = shared context; the kernel never grows an audio stack.
- **Peer capability advertisement** — attach grows a general capabilities
  bag (accepted mimes, latency estimate) so the kernel knows which sinks
  take what.
- **Routing** — today all attached sinks of a context play; open work on
  named multi-machine destinations and a `kj transport route <sink>` verb
  is tracked in `docs/issues.md`, "Audio nodes — follow-up after daemon
  extraction". Volume/routing control reuses `pawlsa`'s PipeWire surface
  when it lands.
- **midi→pcm for dumb sinks** — deferred-PCM-cell vs. budget-excepted-
  deriver shapes; the cell shape is favored (soundfont synthesis is heavy).
- **Out of scope then and now:** continuous streams (no natural tick
  coordinate — clips are objects); seek/rewind (the playhead is
  forward-only; revisiting the past is an export).

## Verification

- **End-to-end:** commit a clip cell on a playing track → the sample sounds
  on the beat through the sink; `parse_validated` rejects an absent-media
  record loudly at commit.
- **Prepare horizon:** a cache-cold multi-MB sample committed ahead of the
  playhead is fetched under the prepare horizon and fires on time; yanking
  the bytes late produces a logged skip, never a late fire.
- **Lead scheduling:** a non-zero-lead sample cue fires at its backdated
  instant with jitter well under a frame (log/tap timestamps) — the audio
  scheduler thread owns the deadline, not any UI frame loop.
- **Headless:** `kj play` on a node with no app produces sound via
  `kaijutsu-audiod`, never the kernel binary.
