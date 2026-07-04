# Cue prior art — what the industry learned about clip/cue metadata

> **Status:** compressed form of the 2026-07-01 research survey (the full
> ~1000-line version, with inline source URLs and per-format field tables,
> lives in git history). Written to answer one question from Amy: *do we
> need a cue format at all, or just a cue data model on the track?* The
> synthesis landed in `docs/clips.md` (Shape A clip payload, tick-anchored
> no-stretch tempo default, triggers in the transport, late binding as
> `Deferred(Recipe)`, no standalone format). This doc keeps the survey's
> evidence: one paragraph per industry, the convergent field-cluster
> analysis, and the compressed conclusions.

## The industries, one paragraph each

**Theater & show control (QLab, MSC, OSC, ETC Eos).** A cue is a numbered,
labeled record in a *list, not a timeline* — shows are elastic in time,
only order is fixed, and the operator's GO is the clock. QLab's
pre-wait/post-wait/continue triad and Eos's follow (start-relative) vs
hang (end-relative) are the minimal complete chaining vocabulary,
surviving ~40 years because the vocabulary stayed tiny and one engine
evaluates it. MIDI Show Control has lasted since 1991 precisely because of
*how little it says* — trigger + cue name, receiver owns timing; a terse
trigger vocabulary survives decades. Eos's tracking model (cues store
deltas; changes leak forward until an explicit `block` flag) taught the
industry to *make the delta boundary a stored, visible field*. OSC's
hierarchical string addresses beat numeric IDs for human/model authorship.

**Film / video post (OTIO, EDL, AAF, SMPTE timecode).** OTIO is the
strongest precedent for Amy's hypothesis: a *data model + API first* —
the `.otio` JSON is almost incidental serialization, interop happens
through adapters. Its load-bearing designs: `source_range` (the edit
decision) kept separate from the media reference, so relinking never
touches the decision; and `MissingReference` as a first-class named
unresolved state, not a dangling path. EDL's 999-event and 8-char-reel
ceilings still bite people fifty years later — field-width decisions
outlive their hardware by decades — and its reel-name media ref is patient
zero of relinking hell. AAF's richness-without-readability lost mindshare
to OTIO's readable model. SMPTE drop/non-drop is the canonical
representation footgun: the same timecode string means different real
times depending on a flag elsewhere in the file, and timecode has no
concept of tempo at all.

**Broadcast (MOS, CasparCG, SCTE-35/HLS).** MOS's mosID/objID split —
name the authority, then the object within it — survived 25+ years, and
its fail-loud rule (an unknown ID must be answered with `roReq`, never
swallowed) is spec-mandated; but its seven conformance profiles document
its own fragmentation. CasparCG has no persistent cue record at all — the
rundown client owns *when*, Caspar owns *how* — and its terse imperative
protocol (`PLAY 1-10 "AMB"`) plus `BEGIN … COMMIT|DISCARD` atomic batches
is why anything with a socket can drive it. SCTE-35, the most widely
deployed cue format on earth, is ~10 fields: id, time anchor, duration,
in/out polarity, and an opaque typed pointer whose asset decision is
*deferred* to the ad server at play time — broadcast independently
discovered late binding. Its curse: debugging is reading opaque hex — the
anti-pattern a required human label repays.

**Audio production (AES31/ADL, BWF/iXML, CUE sheets, DAW clips).** AES31
upgraded EDL with sample accuracy and an in-media identity key (USID in
the BWF `bext` chunk) — and stalled anyway: a better interchange format
doesn't win without an ecosystem forcing function. BWF's lesson: keep
provenance attached to the *media*, placement attached to the *edit* — a
field recording carries its own sample-accurate placement anchor. CD CUE
sheets prove minimal survives but *underspecified* minimal fragments into
tool dialects. Ableton's warp markers are the industry answer to
"media is wall-time, timeline is beat-time": an explicit stored piecewise
map on the clip, never implicit (the sustained complaint is about
*automatic* transient detection, not the stored-marker model). Reaper's
per-item timebase policy is the honest version — when tempo changes,
declare whether placed audio moves/stretches/stays; store the policy.
Ableton's Session view proves trigger semantics and placement are
separable: follow actions and launch quantization live on clips that have
*no timeline position at all*, and the same clip record committed to the
Arrangement gains an absolute position.

**Game middleware (Wwise, FMOD).** Wwise's Event is a symbol, resolution
late-bound: audio ships bank updates, game code never changes — the
most-cited reason middleware exists. Its ShortIDs are *name* hashes (not
content hashes): renames break, content swaps are invisible — feature and
bug. Its interactive-music transition matrix resolves quantization
(*immediate / next grid / next bar / next beat / next cue / exit cue*) at
fire time, never storing pending relationships. FMOD's parameter sheets
generalize the deepest idea in the survey: **time is just one axis a cue
can be scheduled against** — a "playback position" can be any game
parameter scrubbing across trigger regions. Both pay a sprawl tax
(RTPC CPU/memory, timeline spaghetti); "Wwise separates content and
action, FMOD keeps them together" is the field's one-line contrast.

**Music programming (Csound, SuperCollider, Tidal, SMF, MusicXML/MEI).**
Csound is fifty years of proof that *time + duration + opaque params
addressed to a named resolver* is a sufficient cue record — and its
positional p-fields are the legendary curse (write-only without the
instrument source): **named fields are worth their bytes**, especially for
model authors. Even Csound pre-sorts into absolute time before playing.
SuperCollider's two-tier split — beats/logical time in the language,
NTP-timetagged bundles sent `latency` ahead to the server — is kaijutsu's
kernel-Tick vs render-target seam, proven since ~1996: *the network
delivers ahead of time, never just-in-time.* Tidal's mini-notation proves
dense text is a great authoring surface for musical events, and its
folder+index sample refs break exactly as you'd expect (a re-sorted folder
silently changes the music — a content hash would have prevented a genre
of Tidal bug); its long-form-arrangement weakness is evidence *for* a
stored timeline. SMF's separated tempo map is the 40-year recurring bug:
ticks are meaningless without tempo context, so any cell that travels
(drift!) must travel *with* its Timebase. MEI's `@startid` — anchoring a
cue to a *notation object* rather than a time coordinate — is a pattern
to remember if clip cells ever want "that phrase" instead of "tick 3840."

**W3C timed media (SMIL, WebVTT, TTML).** SMIL had genuine follow
semantics (syncbase: `begin="song1.begin+2s"`) and died on the open web
not because the model was wrong but because multi-vendor interop never
happened — rich chained timing works when *one engine* owns evaluation,
which is kaijutsu's situation. WebVTT's id + range + opaque-payload shape
is the smallest viable cue record and it won the web; minimalism is the
feature. The WebVTT/TTML coexistence is a controlled experiment in format
culture: strict-schema archival and forgiving authoring formats coexist,
transcoding down to the simple one for delivery — the two-tier outcome to
expect if kaijutsu ever needs interchange.

## The convergent cue data model

Lining up all ~25 systems, six field clusters recur so consistently they
amount to an industry-wide convergent record:

| Cluster | QLab | Eos | EDL/OTIO | SCTE-35 | Ableton clip | Wwise | WebVTT | Csound `i` |
|---|---|---|---|---|---|---|---|---|
| **Identity** | cue number + cue ID | cue number(.part) | event # / schema'd object | splice_event_id | clip name/slot | event name→hash ID | cue id | (implicit) |
| **Target / media ref** | file or *another cue* | channel levels | reel name / MediaReference | UPID / asset URI | sample path | bank+media ID | (external track) | instr number |
| **Temporal anchor + duration** | list position (+pre-wait) | list position | src+rec in/out | pts_time + break_duration | position (beats) + loop | sync point on grid | start --> end | p2 + p3 |
| **Trigger/advance** | GO + follow/continue | GO + follow/hang | n/a (offline) | out/in polarity, auto_return | launch quantize + follow actions | PostEvent + transition rules | time reached | score order |
| **Param envelope** | fade curve, levels | up/down/delay times | effects/metadata | (deferred to ad server) | clip envelopes, warp | RTPC curves | cue settings | p-fields |
| **Label / human** | name, notes, color | label | *FROM CLIP NAME, markers | (none — pain) | name, color | name, notes | payload text | comments |

Three observations:

1. **The temporal anchor and the media's internal range are always two
   different things** in the mature systems: EDL's four timecodes, OTIO's
   `source_range` vs track position, Reaper's `POSITION` vs `SOFFS`,
   Wwise's entry/exit cues. Immature systems conflate them and grow the
   split later. Kaijutsu's cell `Span` is the timeline side; the clip
   payload must carry the *source* side — and never duplicate the
   timeline side.
2. **The param cluster bifurcates**: baked values on the record (QLab
   fades, Reaper item volume) vs *bindings to live context* (RTPCs, FMOD
   parameters, SMIL syncbase). Both are legitimate; the second is what
   makes game middleware feel alive, and in kaijutsu it maps to
   `Deferred(Recipe)` — a resolver reading committed/ambient context —
   not to payload fields.
3. **Every system that skipped the human-label cluster regrets it**
   (SCTE-35 debugging is reading hex). For model-authored cells this
   cluster is *load-bearing*, not cosmetic: the label is how the next
   model turn (and the human) reads the score.

## Compressed conclusions (the ones clips.md relies on)

**Timing.** Three camps: wall/timecode (unambiguous, musically mute —
exporter targets, not a model to build on; their pathology is
representational ambiguity like drop-frame and PTS wrap), musical/beat
(every one keeps a beat coordinate plus a *separately stored* tempo
map — exactly the `Tick` + `Timebase` split; their recurring bug is the
two halves separating), and chained/event-relative (powerful for elastic
shows, but the stored form doesn't tell you when anything happens). The
clean split the survey converges on: **chaining/quantization are
fire-time semantics; committed records hold resolved absolute
positions.** Kaijutsu's write barrier already enforces this. Worth
stealing from the beat camp: warp markers as stored data, Reaper's
declared per-clip tempo policy (silent defaults breed complaints), and
Wwise's quantize-then-commit sync points.

**Media reference.** No system in the survey references media by content
hash — the spectrum runs reel names → paths → folder+index → name-hash →
authority+ID → in-media USID, each with its signature failure mode, and
relinking hell is post-production's most famous chronic pain. Kaijutsu's
CAS ref kills the failure class by construction. The costs and their
repayments: hash opacity is repaid by a mandatory human/model-facing
label (Wwise's authoring-in-names, running-in-IDs); mutable intent ("the
current kick sample") is a *different concept* needing a name layer —
that's a `Deferred` body resolving name→hash at commit, no new reference
type. And answer dangling references, never swallow them: MOS `roReq`,
OTIO `MissingReference`, kaijutsu's required `Fallback` are the same
family — an unresolvable ref is a first-class, visible state.

**Triggers.** Sorted by where the trigger lives, the systems that aged
best keep it *in the transport/protocol* (MSC GO, AMCP PLAY, OSC bundles,
PostEvent) with the record staying inert data; in-record chaining works
only while the vocabulary stays tiny (QLab/Eos) and becomes unanalyzable
as it grows (SMIL). For kaijutsu the assignment is nearly forced: the
committed clip cell holds only resolved placement; follow/continue/
quantize live in producers and the beat scheduler (the musician's OODA
loop scheduling a phrase ahead *is* a follow action evaluated at
authoring time); the one trigger-ish field that belongs in the data model
is a **quantization hint on the proposed cell**, consumed at commit and
inert afterward. CasparCG's atomic batch is the precedent for multi-cell
commits — kaijutsu's mailbox/timeline lock already plays this role.

**Do we need a format at all?** No. OTIO's real product is the data
model + API; Ableton/Reaper/Wwise never published their clip formats as
standards — the world interoperates through *exports*; and AES31 is the
cautionary converse: a technically excellent standalone format that
stalled because nothing forced adoption — building the format *first*
buys nothing. What would force a format: (1) external interchange —
answered by an exporter/adapter (OTIO, SMF, stems), not a native format;
(2) cross-kernel drift — already answered, since drift offers carry
hyoushigi data (cells + tempo + PPQ, honoring the SMF tempo-map lesson)
and at-rest serialization is versioned CBOR with fail-loud decoding;
(3) archival — the CRDT log + CAS is the archive, and outside-posterity
export is case 1 again. The only discipline the format-less path demands
is OTIO's: version the record per-object and keep an extension bag so
unknown fields survive round-trips.

**Recommendation (landed in clips.md).** Ship the minimal
WebVTT-grade clip payload (Shape A: media hash + mime + required label +
source offset/length + gain) with versioned-record + extension-bag
discipline; grow toward the DAW-grade shape field-by-field as renderers
demand (stretch policy first — it's the one A silently answers by
omission); treat the Wwise-grade symbolic cue as a `Deferred(Recipe)`
resolver milestone, not a payload design — its output is Shape A, so the
payload schema and the late-binding question are independent. Keep every
trigger/quantize/follow concept out of the committed record.
