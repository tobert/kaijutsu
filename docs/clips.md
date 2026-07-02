# Clips — placed media on the track

> **Status:** design settled 2026-07-01 (Amy + Claude) — living notes, revised
> freely, per house style. Synthesized from the
> industry survey in `docs/cue-prior-art.md` (seven domains: show control,
> film post, broadcast, audio production, game middleware, music programming,
> W3C timed media). **No code yet** — implementation lands with its first
> consumer (`docs/pcm.md` slice 5, the mime-keyed render seam). Companions:
> `docs/pcm.md` ("How it converges" — the seam this rides), `docs/hyoushigi.md`
> (the `Cell` substrate), `docs/chameleon.md` (vocabulary: clip joins
> track/lane/voice; "cue" stays the trap message).

## The picture in one paragraph

A **clip** is a placed media reference on a track: a committed hyoushigi cell
whose content is a small, human/model-readable symbolic record — "play this
CAS hash at this tick, from this offset, at this gain." The cell owns *where
in musical time*; the payload owns *what media and how to render it*; the
transport owns *when proposals fire* (quantization, follows); the render
target owns *making it sound*. Bytes never ride the track — media lives in
CAS and prefetches to sinks under the speculation lead (SFTP + `/v/blobs`,
client-side XDG cache). Models author clip records as text, the same way they
author ABC.

## Do we expand `Cell`? — No. Here's the mapping.

The survey found every industry re-inventing the same six field clusters.
They map onto what we already have without touching the substrate:

| Convergent cluster | Where it lives in kaijutsu | Status |
|---|---|---|
| identity | `BlockId` at materialization; `Cell.played_by` + `Cell.track` (provenance + lane) | exists |
| temporal anchor + duration | `Cell.span` (`Tick` start + `TickDelta` len) — the *timeline placement* | exists |
| media reference | the **payload**'s `media` hash (see the two-level ref note below) | payload |
| trigger / advance rule | **transport/producers, resolved at fire time** — never the committed record (survey §8.4; the write barrier already forces this) | exists (launch quantization, OODA lead) |
| param envelope | the payload's baked params (`gain`, later fades/env); *live* params are resolver territory (Shape C) | payload |
| human label | the payload's **required** `label` | payload |

Expanding `Cell` would break hyoushigi's founding rule — "content type is
deliberately not one of the three things; a new modality never edits the
substrate" — for fields only one modality needs. The survey's strongest
structural lesson lands the same way: **timeline placement and source range
are separate concerns** (EDL's four timecodes, OTIO's `source_range`, Wwise's
entry/exit cues). `Cell.span` is placement; `src_offset`/`src_len` are source
range; the payload never repeats the tick. `Cell` stays exactly as it is.

**The two-level reference, explicitly:** the cell's `ContentRef` hashes the
*clip record itself* (the immutability anchor + memoization key, like any
cell); the record's `media` field hashes the *sample bytes*. Both are CAS;
they are different objects at different altitudes.

## The payload — Shape A (`application/vnd.kaijutsu.clip+json`)

A small JSON object (models author it; kaish `jq`s it; the app can render it
as text before a clip renderer exists — `from_mime` → `Plain` is correct
until then). Per the OTIO lesson: a per-record version and an extension bag,
unknown keys preserved and ignored.

```jsonc
{
  "v": 1,                       // record version (per-record, OTIO-style)
  "media": "<32-hex ContentHash>", // REQUIRED — the sample bytes, in CAS
                                //   (128-bit BLAKE3 as 32 hex chars — code is truth)
  "mime": "audio/wav",          // REQUIRED — what the sink decodes (AudioFormatHint source)
  "label": "rimshot, dry",      // REQUIRED, non-empty — CAS hashes are opaque;
                                //   the label is how humans and models read the
                                //   score (the anti-SCTE-35-hex lesson)
  "src_offset_ms": 0,           // optional, default 0 — where in the media to start
  "src_len_ms": null,           // optional, default to-end — how much to play
  "gain_db": 0.0,               // optional, default 0.0 — dB, NOT linear (decided)
  "ext": {}                     // optional extension bag — unknown keys survive round-trips
}
```

Decisions the survey told us to make out loud (silent answers breed
complaints — the Reaper/Ableton lesson):

- **Media-internal time is milliseconds (integer).** Source range is
  wall-time-domain, not musical — samples would drag the sample rate into the
  record, floats invite fuzz. Sub-ms trims are out of scope at this altitude.
- **Gain is dB** (`0.0` = unity). Consoles, Wwise, and humans speak dB.
- **Tempo-change behavior (the default, stated):** a clip is anchored to its
  `Tick` — a tempo change moves *where the clip starts in wall time* (the
  anchor follows the beat) but never changes its internal playback rate. No
  time-stretch, no repitch in v1. This is Reaper's "timebase: time" per-item
  default, chosen deliberately. The `stretch` field name is **reserved** for
  Shape B (`none | repitch | stretch`, plus warp anchors) and lands with the
  first renderer that needs it — stretch-policy is the survey's named
  first-growth field.
- **Span vs source range precedence:** playback is governed by the source
  range (`src_offset_ms`/`src_len_ms`), in full; `Cell.span.len` is the clip's
  advisory *musical footprint* (what windowed reads and `KJ_HEARD`-style
  queries see), not a truncation gate. No auto-truncate-at-span in v1 —
  stopping sound early is the transport's job (`flush_scheduled_after`), not
  the record's.

### Validation (at schedule time, fail loud)

The clip validator is a voice of the decouple-Act-from-ABC generalization
(content-type-keyed validation; ABC's validator is the sibling): parse the
JSON; `v` known; `label` non-empty; `media` a well-formed hash **and present
in CAS** (an absent sample is caught at schedule, loudly, not at prefetch two
phrases later — crash over silent corruption). Unknown `ext` keys pass
through untouched.

### Fallback semantics

Same required `Fallback` as any recipe. `UseLastGood` on a clip lane repeats
the lane's last committed *clip record* — symbolic, cheap, and the media is
already in every sink's cache from the first play (the vamp insurance
carries over from ABC unchanged). Default for a fresh lane: `Skip` — silence
until the first good clip, matching the locked chameleon default.

## How a clip plays

1. A producer commits a clip cell (model turn via the clip validator, a
   `kj` verb, or — the Shape C future — a resolver whose *output* is a
   Shape A record: TTS, name→hash cue-sheet lookup, switch-like selection.
   Late binding is a `Deferred(Recipe)` body, **zero new payload fields**;
   gated on the reactive `compute_basis` open question in hyoushigi.md).
2. At the materialize crossing the mime-keyed seam (`docs/pcm.md` "How it
   converges") emits a wire `RenderCue { mime, payload, lead }` to every sink
   registered for the clip MIME — a clip cue is just one mime on this seam, the
   ABC/MIDI cue (`docs/midi.md` "Render is a wire cue") its sibling. The `lead`
   rides the speculation lead — the transfer budget; the sink re-anchors it at
   `receipt + lead`.
3. The sink resolves `media` from its local XDG CAS cache, pulling a miss
   over SFTP against `/v/blobs/<ab>/<hash>` (mount + fetcher:
   `docs/slash-v.md` track B); decodes per `mime` (Bevy decoders / Symphonia);
   fires at `at` with the source range and gain applied.
4. Transport stop/pause flushes scheduled audio exactly as MIDI
   (`flush_scheduled_after`), and the derived sibling machinery is **not
   involved** — a clip renders directly; there is no barrier-side deriver.

No derivation, no new `ContentType` variant (that lands with the app's clip
renderer, per the variant-lands-with-its-renderer rule), no trigger fields,
no standalone interchange format (survey §8.5: OTIO won model-first, AES31
stalled format-first; exporters come if interchange ever knocks — and
cross-kernel drift is already covered because the versioned cell/CBOR schema
*is* the wire form).

## Growth path

- **Shape A → B, field-by-field, each with its consuming renderer:**
  `stretch` policy first, then loop braces, fades, clip-local envelopes
  (`env` breakpoints are the *baked* half of the param cluster; the *live*
  half is resolvers), `color`/`notes` for the human cluster.
- **Shape C** is a resolver milestone, not a payload change — its output is
  Shape A. The cue sheet (name → hash/params) is ordinary committed/config
  state the resolver reads; this is where the producer's patch-sheet
  vocabulary (`docs/chameleon.md`, "knobs are cells") meets sample playback.
- **Automation lanes** stay separate cells with an automation MIME on the
  same timeline (chameleon's decision) — a clip's `ext`/`env` never grows
  into a second automation system.

## Sequencing

Nothing here builds ahead of its consumer. `docs/pcm.md` slices 1–4 are
untouched (a fixed WAV through the standalone seam). The clip record's first
consumer is **slice 5** (the mime-keyed emit): land the validator + the
Shape A schema in `kaijutsu-audio` (FFI-free — the record type is pure data),
register the first audio target for the clip MIME, and the prefetch path.
`kj play` grows a `--at`/clip-emitting form when the track integration
lands. The design here is the map; the code waits for the session.
