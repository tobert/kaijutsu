# Cue prior art — what the industry learned about clip/cue metadata

> **Status:** research report, 2026-07-01, for the upcoming clip-cell payload
> design session (`docs/pcm.md` § "How it converges — the mime-keyed seam").
> Survey of cue/clip data models across seven industries, then a synthesis
> aimed at one question from Amy: *do we need a cue format at all, or just a
> cue data model on the track?*
>
> **Provenance:** the film/post (§2), audio-production (§4),
> game-middleware (§5), music-programming (§6), W3C (§7) sections, and the
> MOS/CasparCG halves of broadcast (§3) are grounded in fetched sources
> (URLs cited inline). The theater/show-control section (§1) and the
> SCTE-35/HLS half of §3 are written from model knowledge of
> well-documented public systems; specifics that are load-bearing for a
> decision should be spot-checked against the linked primary docs before
> the design session.

## Executive summary

- Every industry that schedules media independently re-invents the same
  record: **{identity, target/media-ref, temporal anchor (+duration),
  trigger/advance rule, parameter envelope, human label}**. The field names
  change; the clusters don't. Kaijutsu's hyoushigi `Cell` already carries
  three of the six (identity, anchor via `Span`, body via `ContentRef`) —
  the clip payload only needs to supply the rest.
- **Time models sort into three camps** — wall/timecode (SMPTE, WebVTT,
  SCTE-35), musical/beat (Ableton, Wwise interactive music, Tidal, SMF
  ticks), and event-relative/chained (QLab follow, SMIL syncbase, Eos
  follow/hang). The chained camp is the odd one out: every system that
  committed chaining *into the stored record* pays for it in analyzability;
  every system that resolved chaining *at fire time* (quantize-then-commit)
  stayed clean. Kaijutsu's write barrier already forces the clean choice.
- **Nobody references media by content hash.** Paths (OTIO), 8-char reel
  names (EDL), name-hash IDs (Wwise), folder+index conventions (Tidal).
  The resulting "relinking hell" is post-production's most famous chronic
  pain. Kaijutsu's CAS ref kills the failure class by construction; the one
  cost is human/model opacity, which a mandatory label field repays.
- **The strongest single precedent for Amy's hypothesis is OTIO itself**:
  it is a *data model + API first*; the `.otio` JSON file is almost
  incidental serialization, and its real interop happens through adapters.
  A cue data model on the track, serialized by the codec kaijutsu already
  has, *is* the OTIO shape — a standalone format is only forced by external
  interchange, and then it's an exporter, not a native format.
- Recommendation (§9): start with a **minimal clip payload** (media ref +
  source range + gain + label), let the cell own time entirely, keep all
  trigger semantics in the transport/producers, and treat the Wwise-style
  runtime-resolved cue as a `Deferred(Recipe)` body — machinery hyoushigi
  already ships — rather than new payload fields.

---

## 1. Theater & show control

*(model knowledge; QLab docs at qlab.app/docs, MSC spec is MMA RP-002/1991,
Eos manual at etcconnect.com)*

### QLab (Figure 53)

- **Record shape:** a cue has a **cue number** (user-facing, editable,
  sparse — "10", "10.5", can be blank) *and* a **unique cue ID** (internal,
  stable — OSC and cue targeting use either). Fields: type (Audio, Video,
  Fade, Group, MIDI, Network/OSC, Script, Wait, Target, Arm/Disarm…),
  name, notes, color/flag, **target** (a file for media cues, *another cue*
  for Fade/Start/Stop/GoTo cues), **pre-wait**, action duration,
  **post-wait**, and **continue mode**.
- **Time model:** event-relative chaining. Pre-wait = delay after trigger
  before action; post-wait interacts with continue mode:
  **auto-continue** fires the next cue *post-wait after this one starts*;
  **auto-follow** fires the next cue *when this one completes* (post-wait
  is replaced by the action's actual duration). Groups add "start first and
  enter," "start all," "timeline" modes. No global timeline — the show is
  a *list*, advanced by GO.
- **Media ref:** file path (macOS bookmarks/aliases under the hood);
  broken targets show as warnings. Fade cues reference *cues*, not media —
  first-class cue-to-cue targeting.
- **Params:** fade curves, audio levels matrix per cue, integrated
  light dashboards in QLab 5; parameters live *on the cue*, not in a
  separate automation lane.
- **Trigger semantics:** operator GO is the primary clock; wall-clock,
  timecode (LTC/MTC), MIDI, OSC, and hotkey triggers are opt-in per cue.
- **Lessons:** practitioners praise the pre-wait/post-wait/continue triad
  as the minimal complete chaining vocabulary — it expresses "bang-bang
  sequences," overlaps, and timed beds with three fields. The curse:
  cue-number-vs-cue-ID duality confuses OSC control, and deeply nested
  groups become hard to reason about (the "what will GO actually do"
  problem). The list-not-timeline stance is *why* theater loves it: shows
  are elastic in time; only the order is fixed.

### MIDI Show Control (MSC)

- **Record shape:** a SysEx message, not a stored record:
  `F0 7F <device_ID> 02 <command_format> <command> <data> F7`.
  Command format = target discipline (lighting, sound, machinery, video,
  pyro…); commands: GO, STOP, RESUME, TIMED_GO, LOAD, SET, FIRE, ALL_OFF,
  RESTORE, RESET, GO_OFF. Data = **cue number as ASCII decimal string**
  (dots allowed, e.g. "23.4"), optionally cue list and cue path, delimited.
- **Time model:** none in the message (except TIMED_GO's time field); MSC
  assumes the *receiver* owns cue timing — it only transports "go do cue N
  now."
- **Media ref:** none. Cue numbers are opaque names resolved by the
  receiving console. Maximum indirection: the message names an intention.
- **Lessons:** its longevity (1991→today, still on every lighting console)
  comes from *how little it says* — trigger + name, nothing else. The
  curse: no acknowledgment, no state query (that's MSC's companion "MIDI
  Machine Control" and vendor extensions), so show networks bolt feedback
  on ad hoc. A terse trigger vocabulary survives decades; a rich one
  wouldn't have.

### OSC show control

- Shape: address pattern + typed args (`/cue/10.5/start`,
  `/eos/cue/1/25/fire`). QLab publishes a full OSC dictionary; Eos,
  disguise, and most media servers do too. OSC **bundles carry NTP
  timetags** for scheduled delivery — the transport itself supports
  "execute at T," which almost no show-control user exploits (they fire
  "now"). Lesson: hierarchical string addresses beat numeric IDs for
  human/model authorship — directly relevant to model-authored cells.

### ETC Eos (lighting cue stacks)

- **Record shape:** cue = number (decimal subdivisions: 10, 10.1, 10.15),
  optional **parts** (simultaneous sub-cues with independent timing),
  up/down transition times, per-category discrete times, **delay**,
  **follow** (fire next cue N seconds after this one *starts*) vs
  **hang** (N seconds after this one *completes*) — exactly QLab's
  continue/follow split under different names — plus **block** (stop
  tracked changes propagating through this cue), **assert** (re-play
  tracked values), **mark** (pre-position moving lights in an earlier cue),
  label, scene markers.
- **Time model:** manual-GO-advanced list with per-cue relative offsets;
  the *tracking* model (a cue stores only *changes*; values track forward
  until blocked) is the distinctive part.
- **Lessons:** tracking-vs-cue-only is lighting's version of
  delta-vs-keyframe, and every programmer has a war story about an
  unblocked change leaking into the next act — the industry answer was
  the explicit `block` flag, i.e. *make the delta boundary a stored,
  visible field*. Follow/hang as two distinct stored fields (start-relative
  vs end-relative) has survived ~40 years of console generations; it is
  the minimal chaining pair.

## 2. Film / video post *(fetched & cited)*

### OpenTimelineIO

Built at Pixar to kill the N×M converter problem between NLEs (Avid,
Premiere, FCP, in-house tools each speaking proprietary formats); used on
Coco, Incredibles 2, Toy Story 4; donated to the Academy Software
Foundation 2019. ([docs](https://opentimelineio.readthedocs.io/en/latest/tutorials/otio-timeline-structure.html),
[file-format spec](https://opentimelineio.readthedocs.io/en/latest/tutorials/otio-file-format-specification.html))

- **Record shape:** JSON tree of typed objects tagged
  `"OTIO_SCHEMA": "Clip.5"` etc. (per-object schema versioning).
  `Timeline` → `Stack` → `Track` → {`Clip`, `Gap`, `Transition`, nested
  `Track`/`Stack`}. Every item carries a **namespaced free-form `metadata`
  dict**, `markers` (color + `marked_range` + comment), and `effects`
  (thin: `effect_name` + metadata bag — deliberately *not* a universal
  effects DSL).
- **Time model:** `RationalTime {value, rate}` and
  `TimeRange {start_time, duration}` everywhere. Two distinct spaces per
  clip: `available_range` (what media exists) vs `source_range` (the
  trimmed segment used) — the edit decision is separate from the media
  description. Timecode is only an adapter-boundary interpretation.
- **Media ref — the load-bearing design:** `MediaReference` is a base
  class: `ExternalReference {target_url, available_range}`,
  `MissingReference` (a *first-class named unresolved state*, not a
  dangling path), `ImageSequenceReference` (url base + prefix/suffix +
  frame policy incl. `missing_frame_policy`). Site-specific **Media
  Linker** plugins resolve `MissingReference`s locally after import;
  relinking touches `media_reference` while `source_range` — the decision
  — is untouched.
- **Lessons:** (1) OTIO is a *library/data model first*; the file is its
  serialization and the ecosystem value is adapters — precedent for
  "data model, not format." (2) Adoption is asymmetric: pipeline tools and
  Resolve speak it natively; Avid/Premiere still route through AAF/XML
  adapters, which remain the interop bottleneck (GitHub issues #1701,
  #482: AAF export crashing Media Composer, nested-scope round-trip
  failures). A common model doesn't erase last-mile adapters.

### EDL / CMX 3600

- One event per ASCII line:
  `001 TAPE1 V C 00:00:32:00 00:00:35:16 01:00:00:00 01:00:03:16` —
  event number (**hard 3-digit / 999-event limit**), reel (**8 chars
  max**), track, transition (C/D/W + frames), then *four timecodes*:
  source in/out and record in/out. `FCM:` header sets drop/non-drop for
  the whole file. Everything richer lives in `*` comment lines
  (`*FROM CLIP NAME:` …) parsed heuristically per vendor.
- **Lessons:** the 999-event and 8-char-reel ceilings are 1970s hardware
  constraints still biting people in Premiere/Resolve forums fifty years
  later — **field-width decisions outlive their hardware by decades**.
  And the reel-name media ref (no path, no id, no hash) is patient zero of
  relinking hell. ([niwa.nu EDL guide](https://www.niwa.nu/2013/05/how-to-read-an-edl/),
  [edlmax reference](https://www.edlmax.com/EdlMaxHelp/Edl/Edl_Overview.htm))

### AAF

- Object graph, not a list: `CompositionMob` (the edit) → `MasterMob`
  (source aggregation/insulation) → `SourceMob` (essence description),
  each with `MobSlot`s (Timeline / Static / Event varieties); extensible
  metadata dictionary; binary MS Structured Storage container; essence
  embedded or external. Richest effects/params model of the three (nested
  compositions, keyframed parameter graphs).
- **Lessons:** the completeness is why it lost mindshare — opaque binary,
  vendor-inconsistent population of the Mob indirection layers, so the
  in-principle-correct relinking model still breaks in practice (REDUSER /
  Blackmagic forum relink threads). OTIO explicitly positioned against it:
  richness without readability breeds brittle implementations.
  ([AAF object spec](https://static.amwa.tv/ms-01-aaf-object-spec.pdf))

### SMPTE timecode

- `HH:MM:SS:FF`; **drop-frame** drops frame *labels* (not frames) to track
  wall clock at 29.97; **non-drop** counts frames and drifts ~3.6 s/hour
  off the wall clock. The same string means different real times depending
  on a flag elsewhere in the file — a representation footgun cited as a
  "massive post-production headache."
- **Lessons for kaijutsu:** timecode has **no concept of tempo/beat** —
  "fire on beat 3 of bar 12 across tempo changes" is inexpressible without
  a bolted-on tempo map; and never allow two interpretations of one
  numeric string to coexist implicitly (kaijutsu's `Tick` + explicit
  `Timebase` binding already draws this line correctly).

## 3. Broadcast

*(MOS and CasparCG verified against fetched sources; SCTE-35/HLS from
model knowledge — spec at scte.org, RFC 8216 + Apple interstitials docs)*

### MOS rundowns *(verified against the MOS 2.8.5 spec)*

- **Record shape:** XML, three-level hierarchy: Running Order (`roID`,
  `roSlug`) → **Story** (`storyID`, `storySlug`, `mosAbstract`) →
  **Item** (`itemID`, `itemSlug`; item order is significant — "sent in
  the intended order they will be played") → object reference. A
  `mosObj` carries `objID` ("absolutely unique within the scope of the
  Media Object Server"), `objSlug` (human-readable, ≤128 chars),
  `objDur`, `objTB` (timebase, e.g. 59.94), `objRev`, `objType`,
  `objGroup`, `objAir`, `status`.
- **Time model:** frame-based durations (`objDur` against `objTB`); no
  wall-clock trigger field in the base protocol — sequencing is purely
  **ordinal**, actual air timing lives in the external automation layer.
- **Media ref:** no path/URL/hash — a `(mosID, objID)` composite key,
  where `mosID` is a fully qualified authority name
  (`<family>.<machine>.<location>.<enterprise>.mos`). MOS never carries
  media, only metadata/refs.
- **Params:** `mosExternalMetadata` = `mosScope` (OBJECT/STORY/PLAYLIST —
  controls propagation) + `mosSchema` (a URI) + `mosPayload` (arbitrary
  well-formed XML). MOS ferries, never interprets — the extension-bag
  pattern, standardized.
- **Trigger/advance:** structural verbs (`roCreate`, `roElementAction`
  INSERT/REPLACE/MOVE/SWAP/DELETE, `roStorySend`, `roReadyToAir`) on one
  port; live control on another — **`roCtrl`** carries PLAY / EXECUTE /
  PAUSE / STOP / SIGNAL, the protocol's actual GO. **Fail-loud by spec:**
  a device receiving a `roElementAction` naming an unknown
  roID/storyID/itemID must send `roReq` back to the newsroom system —
  dangling references are answered, never swallowed.
- **Lessons:** the mosID/objID split — *name the authority, then the
  object within it* — survived 25+ years of newsroom churn. The pain
  signal: MOS ships **seven conformance profiles (0–7)** because vendors
  kept shipping divergent partial implementations; "MOS compatible" is
  untrustworthy without checking profile numbers — a spec that needs
  profile declarations is documenting its own fragmentation.
  ([MOS protocol](https://mosprotocol.com/),
  [mosromgr docs](https://mosromgr.readthedocs.io/en/stable/))

### CasparCG (playout) *(verified against the AMCP wiki)*

- AMCP text protocol: `PLAY 1-10 "AMB" LOOP`, `LOADBG` (+ `AUTO` for
  follow-on playout), `CG ADD` for template graphics — addressing is
  **channel-layer** (`1-10` = channel 1 layer 10), media by
  server-relative name without extension, frame-relative `SEEK`/`LENGTH`.
  Template data payloads are format-sniffed by first character (`<` =
  XML, `{` = JSON), and `DATA STORE` lets a cue reference a stored
  dataset by key instead of inlining it.
- **No persistent cue record at all** — server state is a live
  addressable channel/layer stack; the "cue list" concept does not exist
  server-side. The rundown client (e.g. a MOS gateway) owns *when*,
  Caspar owns *how*.
- **Atomicity:** `BEGIN … COMMIT|DISCARD` batches AMCP commands into one
  atomic application — multi-layer cues change together or not at all.
- **Lessons:** the terse imperative verb + coordinate + name shape is
  extremely automatable — Caspar has run 24/7 broadcast playout across
  European broadcasters since 2006 (SVT origin) largely because anything
  that can open a socket can drive it. Model-authored cues want this
  property.
  ([AMCP protocol](https://github.com/CasparCG/help/wiki/AMCP-Protocol),
  [casparcg-server](https://github.com/svt/casparcg-server))

### SCTE-35 / HLS interstitials

- **SCTE-35 splice_insert fields:** `splice_event_id`,
  `out_of_network_indicator`, `splice_time` (**PTS, 90 kHz ticks**, 33-bit),
  `break_duration` + `auto_return`, `avail_num`/`avails_expected`,
  encryption/CRC. The richer `time_signal` + segmentation descriptor path
  adds `segmentation_type_id` (program/chapter/ad start-end…) and
  **UPIDs** — typed external content identifiers (Ad-ID, EIDR, UUID, URI…).
- **HLS surface:** `EXT-X-DATERANGE` with `ID`, `START-DATE` (wall clock,
  ISO 8601), `DURATION`/`PLANNED-DURATION`, `SCTE35-OUT`/`-IN` (the binary
  payload, hex), and for interstitials `X-ASSET-URI`/`X-ASSET-LIST`,
  `X-RESUME-OFFSET`, `X-RESTRICT`. A cue is *an annotation on the media
  timeline* pointing at replacement content.
- **Lessons:** the most widely deployed cue format on earth is ~10 fields:
  id, time anchor, duration, in/out polarity, and an opaque typed pointer
  to "what goes here." It works at planetary scale because it is terse and
  because the *asset decision is deferred* (the ad server resolves the
  UPID at play time) — broadcast independently discovered late binding,
  same as Wwise. The curse: 33-bit PTS wraps (~26.5 h), and wall-clock
  `START-DATE` vs media-time PTS duality causes real alignment bugs.

## 4. Audio production

### AES31 / ADL

- Plain-text audio decision list in **EDML** (Edit Decision Markup
  Language — XML-style tags with parenthesis-enclosed keywords; `.adl`
  files; AES31-4 (2024) adds a formal XML-Schema mapping of the same
  content). Sections for list structure, edit time markers, ADL event
  entries, and media-source reference lists. Time is **TCF** (Timecode
  Character Format): `HHiMMiSSiFFissss` — hours:minutes:seconds:frames
  *plus a sample count*, with indicator fields for frame rate / film
  format / sample rate. Events reference mono BWF files via Media File
  Locators (URLs) plus the **USID** identity mirrored in the BWF `bext`
  `OriginatorReference` — an *identity carried inside the media file
  itself*, not just a path. Fades/crossfades are edit-event attributes;
  there is no general envelope model.
  ([AES31 overview](https://en.wikipedia.org/wiki/AES31),
  [WaveLab AES-31 docs](https://archive.steinberg.help/wavelab_pro/v10/en/wavelab/topics/audio_montage/audio_montage_aes_31_export_import_c.html))
- **Lesson:** AES31 is EDL's shape upgraded with (a) sample accuracy and
  (b) an in-file identity key — the industry's closest approach to
  content-anchored referencing before hashes. Adoption stalled anyway:
  Pro Tools never implemented it; WaveLab is effectively the reference
  implementation; a cottage industry of translators (AATranslator,
  Vordio) papers over the gap. A *better interchange format* doesn't win
  without a dominant vendor or ecosystem forcing function — directly
  relevant to §8.5.

### BWF `bext` + iXML

- **`bext` chunk:** Description(256), Originator(32),
  OriginatorReference(32, the USID), OriginationDate/Time, **TimeReference
  — a 64-bit sample count since midnight** (the sample-accurate timecode
  anchor every DAW uses to auto-place field recordings), UMID, loudness
  fields. **iXML chunk:** scene/take/tape, circled-take, note, speed block
  (sample rate, TC rate), track list with channel names.
- **Lesson:** *the media file carries its own placement anchor and
  provenance.* A sound-report workflow drops a thousand takes on a
  timeline correctly because each file knows its own when/what. iXML
  (public-domain, ~2005, Gallery UK) won near-universal field-recorder
  adoption precisely because it was free, solved a real multi-vendor
  mess, and rides *inside* the audio file — nothing to lose track of,
  no session file to go stale against the media. For kaijutsu: CAS blobs
  could carry a metadata sidecar the same way, but the cell already owns
  placement — the BWF lesson is really "keep provenance attached to the
  *media*, placement attached to the *edit*."
  ([bext fields](https://wavinfo.readthedocs.io/en/latest/scopes/bext.html),
  [iXML](https://en.wikipedia.org/wiki/IXML))

### CD CUE sheets

- `FILE "x.wav" WAVE` / `TRACK 01 AUDIO` / `INDEX 01 03:42:57` —
  MM:SS:FF at **75 frames/second** (the Red Book sector rate), INDEX 00 =
  pregap start, INDEX 01 = audible track start, plus
  PERFORMER/TITLE/ISRC/FLAGS and free-form `REM` lines repurposed by
  convention (`REM GENRE`, `REM REPLAYGAIN_*`). Thirty years old, still
  the de facto gapless-album/DJ-mix annotation format.
  ([Hydrogenaudio cue sheet](https://wiki.hydrogenaudio.org/index.php?title=Cue_sheet))
- **Lesson:** a cue format with a handful of keywords refuses to die
  because it's human-writable in any text editor and does exactly one
  job. But the spec is loosely enforced: rippers disagree about where
  gaps attach (previous track's tail vs next track's pregap), so the
  minimal format fragmented into tool-specific dialects anyway. Minimal
  survives; *underspecified* minimal fragments.

### DAW clip models

- **Ableton Live** (the richest beat-time clip record in the survey):
  a clip = media ref + **warp markers** (an explicit piecewise map between
  *sample time* and *beat time* — elastic audio as stored data, with warp
  modes Beats/Tones/Texture/Re-Pitch/Complex choosing the stretch
  algorithm; markers can be **saved back into the sample's sidecar** so
  the mapping travels with the media, not just the project), loop brace +
  start offset (in beats), **launch mode** + **launch quantization**
  (per-clip: None/Global/1 bar…1/32 — fire requests snap to the grid),
  **follow actions** (two candidate actions A/B with percentage chance
  weights — Next/Previous/First/Last/Any/Other/Jump/Stop — gated either
  **Linked** to the clip's end / N loop repetitions or **Unlinked** on a
  fixed bars:beats:16ths timer independent of clip length), clip
  envelopes (per-parameter breakpoint automation, linkable or unlinked
  with independent loop length — polymeter as a side effect), legato,
  color, name. Session view = clips in a launch grid (no timeline
  position); Arrangement view = same clip record *plus* a timeline
  position. ([manual: warping](https://www.ableton.com/en/manual/audio-clips-tempo-and-warping/),
  [launching clips](https://www.ableton.com/en/manual/launching-clips/))
- **Bitwig:** same two-surface model; distinctive additions are per-note
  expression envelopes and **operators** on notes/steps (chance, repeat,
  occurrence "play only on 1st/2nd loop pass", alternate) — probabilistic
  micro-follow-actions *inside* the clip.
- **Reaper** (`.rpp` is readable text; fields below confirmed against a
  real project file): `<ITEM` block with `POSITION` (timeline seconds),
  `LENGTH`, `SOFFS` (source offset into media — the timeline/source
  coordinate split as two plain fields), `PLAYRATE` (+ preserve-pitch
  flag), `FADEIN`/`FADEOUT` (shape, length, curve), `VOLPAN`, `IGUID`
  (stable item GUID), `NAME`, **takes** (nested per-take data for
  comping), and a nested `<SOURCE WAVE FILE "path.wav"` chunk. Fades live
  on the item; general automation envelopes are *track/FX-scoped*, not
  clip-scoped. Core placement is wall-clock seconds — musical time is an
  overlay (per-item timebase setting, stretch markers, project tempo
  envelope), so the tempo-change semantics are a stored, per-item policy
  rather than a global assumption.
  ([example .rpp](https://raw.githubusercontent.com/cpmpercussion/Studio1-Demo/master/reaper/reaper-test.RPP),
  [Cockos forum on ITEM chunks](https://forums.cockos.com/showthread.php?t=201344))
- **Lessons:** (1) Ableton's warp markers are the industry's answer to
  "media is wall-time, timeline is beat-time" — the mapping is *explicit
  stored data on the clip*, never implicit; and the sustained practitioner
  complaint is about *automatic* transient detection ("auto-warp eats the
  attack"), not about the stored-marker model — auto-detect is a draft,
  user-correctable markers are the record. (2) Reaper's per-item timebase
  policy is the honest version of the same problem: when tempo changes,
  something must be declared about what happens to placed audio (move?
  stretch? stay?). Kaijutsu's clip cell will face exactly this and should
  store the policy, not assume it. (3) Follow actions prove chaining can
  live *outside* the timeline record (Session clips have no position at
  all) — trigger semantics and placement are separable concerns; Bitwig
  sharpens this by making auto-advance an *optional, explicit* Next
  Action rather than an always-on per-clip property. (4) Reaper's plain-
  text `.rpp` grew a whole third-party tooling ecosystem (`rppp`, Python
  `rpp`) purely because it's grep/diff/scriptable — human-readable
  serialization is an ecosystem feature, which bodes well for a textual,
  model-authored clip payload.

## 5. Game audio middleware *(verified against Audiokinetic / FMOD docs)*

### Wwise

- **Events are the API surface:** game code posts
  `PostEvent("Play_Footstep", gameObject)` — an Event is a *named
  container of Actions* (Play/Stop/Pause/Resume/Set Volume/Set State/…)
  targeting objects in the Actor-Mixer or Interactive Music hierarchies.
  Two Event kinds: "Action" Events (flat action list) and **Dialogue
  Events** (decision trees keyed on State Groups). The Event names
  *behavior*, not *content* — per the docs, "you can create the events,
  integrate them into the game and then build and fine-tune the contents
  of the Event without ever having to re-integrate it into the game":
  **the cue is a symbol; resolution is late-bound.**
- **IDs:** every project object gets a 128-bit **GUID** at creation; at
  build time a 32-bit **ShortID** is derived — for Events, Game Syncs,
  and SoundBanks it's the **FNV hash of the object's name**; for other
  objects a 30-bit FNV hash of the GUID bytes (reference implementation
  ships in `AkFNVHash.h`). Runtime resolves the hash against loaded
  **SoundBanks** (an Init.bnk with busses/game-syncs must load first;
  content banks after), never file paths; a `.strings.bank` for
  human-readable lookup is optional/debug-only. Note: a *name* hash, not
  a *content* hash — renames break, content swaps are invisible.
- **Game syncs (the parameter model):** **RTPCs** — continuous **Game
  Parameters** (speed, health, RPM) mapped through designer-drawn
  point-curves onto any Wwise property (volume, pitch, filter, sends —
  on objects, busses, attenuations, effects); **States** — global
  mutually-exclusive modes (Combat/Explore) applying property offsets;
  **Switches** — per-game-object selectors (surface type) choosing a
  container child; RTPCs can even drive Switch changes, collapsing
  continuous into discrete. The cue record stays tiny because parameters
  are *bindings to live context*, not baked values. The docs themselves
  warn RTPC sprawl "can consume a significant amount of memory and CPU."
- **Interactive music hierarchy:** **Music Segments** carry tempo + time
  signature and are annotated with **Cues** — built-in **Entry/Exit
  Cues** plus user-defined Custom Cues — with **Pre-entry/Post-exit**
  regions explicitly reserved for transition material. Music
  Switch/Playlist Containers sequence segments; a **Transition Matrix**
  declares (source, destination) rules with a sync point — *immediate /
  next grid / next bar / next beat / next cue / exit cue* — optionally
  bridged by a transition segment: **quantized late-binding between
  musical chunks**. **Stingers** are phrases bound to a **Trigger** game
  sync, quantized onto the playing segment's grid; one stinger per
  trigger per hierarchy level, most-specific object wins.
- **Lessons:** universally praised: the Event/implementation indirection
  decouples iteration (audio ships bank updates; code never changes) —
  the most-cited reason middleware exists. Cursed: real onboarding cost
  ("hours of training before successfully navigating and generating
  sound banks") and RTPC/State/Switch sprawl as a CPU/memory tax.
  "Wwise separates content and action, while FMOD keeps them together"
  is the field's one-line contrast.
  ([Managing Events](https://www.audiokinetic.com/en/library/edge/?source=Help&id=managing_events_overview),
  [Interactive music](https://www.audiokinetic.com/en/library/edge/?source=Help&id=understanding_interactive_music),
  [javierzumer FMOD/Wwise comparison](https://javierzumer.com/blog/2022/6/28/differences-between-fmod-amp-wwise-part-1))

### FMOD Studio

- **Event = a mini-DAW timeline + parameter sheets.** An Event has Audio/
  Return/Master/Automation **tracks** and **sheets**: **Action Sheets**
  (concurrent or consecutive playlists fired at Event start) and
  **Parameter Sheets** — a ruler over a *parameter's value range* where
  the "playback position" is the parameter value, scrubbing across
  trigger regions. **Instruments** are the placed content units: Single,
  Multi, Event (nested), Scatterer, Programmer (game-fed audio),
  Command (Start Event / Set Parameter), Snapshot, Plug-in. Events
  instance freely (`EventInstance` per campfire).
- **Time model:** the default **Timeline parameter** is literally wall
  time ("advances at a rate of one per second"). Musical behavior layers
  on via **logic markers**: tempo markers (define bar/beat quantization;
  120 BPM 4/4 default), destination markers/regions, **loop regions**,
  **magnet regions**, sustain points, **transition markers/regions**
  (quantizable to bar/beat intervals, with an offset mode —
  None/Relative/Inverted — so a jump can land at *the same relative
  position within the bar*, and optional **transition timelines** for
  crossfade material). FMOD builds Wwise's segment/cue system out of
  generic timeline geometry instead of a dedicated music-object type.
- **Media ref:** string path + **GUID**, resolved through built
  **Banks**: `getEvent("event:/UI/Cancel")`; the emitted
  **`.strings.bank`** (name/path/GUID of every event) is explicitly
  optional — "without it, you can only look up events via their GUIDs."
  Banks split metadata (event/mixer definitions) from sample data; a
  Master Bank is always resident, content banks load/unload for memory.
- **Params/automation:** parameters are typed — Timeline, User
  (Continuous float / Discrete int / Labeled enum), and **Built-in**
  (auto-computed from 3D listener–emitter geometry: Distance, Direction,
  Elevation, Cone Angle, Speed…). Any parameter drives **automation
  curves** on nearly any property, multiple parameters combining
  additively; **Modulators** (Random, AHDSR) add per-instance variation;
  **Snapshots** capture scoped bus-mix states (Overriding vs Blending,
  with priority ordering and an intensity dial, itself modulatable).
- **Lessons:** praised for iteration speed (live-connect to a running
  game) and DAW-familiar approachability; the trade-off practitioners
  name is that content-and-behavior-in-one-Event is less flexible for
  large parallel teams than Wwise's split, and complex event logic
  becomes opaque timeline spaghetti — visual authoring doesn't scale to
  deeply conditional cues. The parameter-sheet idea generalizes: **time
  is just one axis a cue can be scheduled against.**
  ([FMOD Studio manual: events](https://www.fmod.com/docs/2.02/studio/authoring-events.html),
  [parameters](https://www.fmod.com/docs/2.02/studio/parameters.html),
  [banks](https://www.fmod.com/docs/2.02/studio/getting-events-into-your-game.html))

## 6. Music programming / computational

### Csound score

- `i 1 0 4 0.5 440` — instrument (p1, numeric or named; fractional p1
  like `10.1` addresses a named *instance*), start in beats (p2),
  duration (p3), then **positional p-fields** whose meaning is defined
  solely by the receiving instrument. `f` statements build function
  tables (which double as envelope/automation data); `t` maps beats to
  seconds (without it, beat == second). Three preprocessing passes before
  performance: **carry** (blank/`.` fields inherit from the previous line;
  `+` in p2 = previous p2+p3), **tempo**, **sort** (events reordered into
  absolute time order). ([i statement](https://csound.com/docs/manual/i.html),
  [t statement](https://csound.com/docs/manual/t.html))
- **Lessons:** fifty years of proof that *time + duration + opaque params
  addressed to a named resolver* is a sufficient cue record. The curse is
  legendary: positional p-fields are write-only — `p7` means nothing
  without the instrument source — and Csound 7's own manual now pushes
  `passign`/named locals, an admission that positional fields don't scale.
  **Named fields are worth their bytes**, especially for model authors.
  Note also that Csound *pre-sorts into absolute time* before playing —
  even the score-file ancestor abandoned relative order at runtime.

### SuperCollider

- **Patterns:** `Pbind(\instrument, \sine, \dur, 0.25, \freq, Pseq(...))`
  — an *event stream generator*; events are key-value dictionaries with
  well-known defaulted keys, materialized lazily against a clock
  (`TempoClock` in beats). The stored thing is the *generator*, not the
  events.
- **OSC timetag bundles:** client schedules ahead by sending the server
  bundles stamped with **NTP-format future timetags** (the client leads by
  a configured `latency`); the server fires sample-accurately. This is
  kaijutsu's speculation-lead/prefetch pattern, verbatim, from 1996-2002
  era design: *the network delivers ahead of time, never just-in-time.*
- **Lesson:** the two-tier split — musical scheduling in the language
  (beats, **logical time** that advances only at scheduled wake-ups) and
  wall-clock execution in the server (NTP timetags, outgoing bundles
  stamped logical-time-plus-`latency`) — maps exactly onto kaijutsu's
  kernel-Tick vs render-target `at: Instant` seam, and is the proven
  answer to network/execution jitter. Prior art that the seam is right.
  ([ServerTiming](https://doc.sccode.org/Guides/ServerTiming.html),
  [OSC 1.0 spec](https://opensoundcontrol.stanford.edu/spec-1_0.html))

### TidalCycles / Strudel

- Patterns are **functions of cycle time** (rational), not stored event
  lists: `sound "bd sn hh*2"` queried over an arc yields events.
  Samples referenced by **folder name + index** into a sample library
  (`"bd:3"` = 4th file in the `bd` folder) — convention over identity;
  params applied by combinators (`# gain 0.8 # speed "1 2"`), themselves
  patterns.
- **Lessons:** mini-notation is the proof that *dense text is a great
  authoring surface for musical events* — directly encouraging for
  model-authored clip cells. The folder+index media ref is the loosest in
  the survey and breaks exactly as you'd expect (a re-sorted sample folder
  silently changes the music) — a content hash would have prevented an
  entire genre of Tidal bug. Two more: the parameter-merge algebra
  (`#` takes *structure from the left, values from the right*; `|+`/`|*`
  combine arithmetically) is a portable idea for applying param patterns
  over placed clips; and long-form arrangement is Tidal's known weak spot
  (Strudel discussion #629 — multi-section pieces need a bolted-on
  `arrange`), which is evidence *for* kaijutsu's stored-timeline choice:
  cyclic pattern-functions and committed timelines are complementary, not
  rivals. ([mini-notation](https://tidalcycles.org/docs/reference/mini_notation/),
  [Strudel #629](https://github.com/tidalcycles/strudel/discussions/629))

### SMF (Standard MIDI File)

- Events carry **delta ticks** (variable-length, relative to previous
  event) at a header-declared PPQ; tempo is a *separate* meta-event track
  (`FF 51`, µs per quarter). Relevant meta events: **Cue Point (`FF 07`,
  free text)**, Marker (`FF 06`), Lyric (`FF 05`) — i.e., MIDI's own "cue"
  is just *text at a tick*.
- **Lessons:** the classic gotchas are (1) the separated tempo map —
  ticks are meaningless without the tempo track, and merging/excerpting
  SMF files while forgetting tempo context is a 40-year-old recurring
  bug; (2) *relative* deltas themselves — the canonical conversion bug is
  applying only the preceding tempo to a delta segment when a tempo
  change lands mid-segment (the fix, per the MIDI.org forums: convert to
  absolute tick position *first*, then apply tempo piecewise); (3)
  format-1 timing is only computable by merging all tracks. Kaijutsu
  equivalence: absolute `Tick` (not deltas) is the right side of this
  history, and a `Tick` is meaningless without its track's `Timebase` —
  any clip cell that travels (drift!) must travel *with* tempo context.
  Hyoushigi's drift-carries-hyoushigi-data rule already honors this.
  ([SMF spec](https://midimusic.github.io/tech/midispec.html),
  [MIDI.org tempo-change thread](https://midi.org/community/midi-specifications/calculation-of-the-delta-time-when-changing-the-tempo))

### MIDI 2.0 / UMP

- Relevant deltas only: 16/32-bit parameter resolution, **Jitter
  Reduction timestamps** (sender stamps intra-packet timing so receivers
  can de-jitter), per-note controllers. No new cue concept; it validates
  the "distribute a clock model, not raw pulses" stance (JR timestamps are
  a micro version of it).

### MusicXML / MEI

- MusicXML: notation-first; `<sound tempo="120" dynamics="106"/>` inside
  `<direction>` embeds performance hints; time is symbolic
  (`<divisions>` per quarter + per-note `<duration>`); instruments bind
  via MIDI program numbers. MEI's performance module aligns notation to
  recordings via `<recording>`/`<clip>` with `@begin`/`@end` and
  `@startid` anchoring a timespan to a *specific notated event*, plus
  `<when>` for named timepoints — the most developed notation↔media
  alignment machinery in the survey.
  ([MusicXML `<sound>`](https://www.w3.org/2021/06/musicxml40/musicxml-reference/elements/sound/),
  [MEI facsimiles/recordings](https://music-encoding.org/guidelines/v4/content/facsimilesrecordings.html))
- **Lesson:** notation formats treat media/timing hooks as annotations
  bolted onto the symbolic score — the symbolic layer stays canonical.
  Same stance as kaijutsu ("hearing is symbolic"); the hook layer is where
  clip cells live. MEI's `@startid` — anchoring a cue to a *notation
  object* rather than a time coordinate — is a pattern to remember if
  clip cells ever want to reference "that phrase" instead of "tick 3840."

## 7. W3C timed media *(fetched & cited)*

### SMIL

- Timing is an attribute set (`begin/dur/end/repeatCount/fill/restart`) on
  any element, inside `<par>`/`<seq>`/`<excl>` containers. Crucially,
  `begin` accepts **syncbase values** (`begin="song1.begin+2s"`,
  `end="video1.end-1s"`) and **event values** (`begin="button.click+3s"`,
  `foo.repeat(2)`) — genuine chained/follow semantics, cascading
  transitively off other elements' lifecycles.
  ([SMIL 3.0 Timing](https://www.w3.org/TR/SMIL3/smil-timing.html))
- **Lessons:** the only W3C system with real follow semantics — and it
  died on the open web not because the model was wrong but because
  multi-vendor interop never happened; it survives in single-implementation
  profiles (DAISY, 3GPP, signage). Rich chained timing is implementable
  when *one engine* owns evaluation — which is kaijutsu's situation
  (the kernel is the sole sequencer).

### WebVTT

- Cue = optional id, `HH:MM:SS.mmm --> HH:MM:SS.mmm`, inline settings
  (`position/line/align/size/region`), payload until blank line. Flat
  interval list, zero chaining; `kind=metadata` tracks carry arbitrary
  (often JSON) timed payloads — the web's generic cue track.
  ([spec](https://www.w3.org/TR/webvtt1/))
- **Lessons:** minimalism *is* the feature — human-writable, forgiving
  parser, "anyone can be a sender." The id + range + opaque-payload shape
  is the smallest viable cue record and it won the web.

### TTML

- SMIL-like `begin/end/dur` + `timeContainer="par|seq"` on any element,
  but **no** cross-tree syncbase/event referencing; heavy referential
  styling; XML validation. Broadcasters mandate it (QC, archival,
  styling precision); the web rejected it for the same strictness.
  ([TTML2](https://www.w3.org/TR/ttml2/),
  [Balisage: WebVTT vs TTML](https://www.balisage.net/Proceedings/vol10/html/Tai01/BalisageVol10-Tai01.html))
- **Lesson:** the WebVTT/TTML split is a controlled experiment in format
  culture: the strict-schema archival format and the forgiving authoring
  format *coexist*, with transcoding down to the simple one for delivery.
  If kaijutsu ever needs an interchange format, expect this two-tier
  outcome (rich internal model, simple export).

---

# Synthesis

## 8.1 The convergent cue data model

Lining up all ~25 systems, six field clusters recur so consistently that
they amount to an industry-wide convergent record:

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
   payload must carry the *source* side (offset/length into the media) —
   and never duplicate the timeline side.
2. **The param cluster bifurcates**: baked values on the record (QLab
   fades, Reaper item volume) vs *bindings to live context* (RTPCs,
   FMOD parameters, SMIL syncbase). Both are legitimate; the second one
   is what makes game middleware feel alive, and in kaijutsu it maps to
   `Deferred(Recipe)` — a resolver reading committed/ambient context —
   not to payload fields.
3. **Every system that skipped the human-label cluster regrets it**
   (SCTE-35 debugging is reading hex). For model-authored cells this
   cluster is *load-bearing*, not cosmetic: the label is how the next
   model turn (and the human) reads the score.

## 8.2 Timing-model lessons

- **Wall/timecode camp** (SMPTE, EDL, WebVTT, TTML, SCTE-35 PTS, BWF
  TimeReference): unambiguous, interchange-friendly, and *musically
  mute* — cannot express tempo-relative placement at all. Their pathology
  is representational ambiguity (drop/non-drop, PTS wrap, DATERANGE
  wall-clock vs PTS media-time). Kaijutsu note: these are the systems a
  future *exporter* targets; they are not a model to build on.
- **Musical/beat camp** (Ableton, Wwise music, Tidal, SMF, Csound-with-`t`,
  SuperCollider TempoClock): every one of them keeps **an integer or
  rational beat coordinate plus a separately stored tempo map/binding** —
  exactly the `Tick` + `Timebase` split. Their recurring bug is the two
  halves separating (SMF tempo-track loss, Tidal `cps` mismatch). What the
  best of them add on top, and what's worth stealing:
  - **Ableton warp markers:** an explicit, stored, piecewise sample-time↔
    beat-time map on the clip. Any beat-placed *audio* needs one eventually
    — even a single-point version ("this sample's beat-1 is at 0.37 s").
  - **Reaper's per-item timebase policy:** a declared per-clip answer to
    "tempo changed — do I move/stretch/stay?" Store the policy; silent
    defaults are the DAW-forum complaint generator.
  - **Wwise sync points:** quantization vocabulary (*immediate / next
    grid / next bar / next beat / next cue / exit cue*) as *transition-time*
    semantics, resolved at fire time — never stored as a pending
    relationship in the record.
- **Chained/event-relative camp** (QLab follow/continue, Eos follow/hang,
  SMIL syncbase, Ableton follow actions): powerful for elastic, operator-
  paced shows; the cost is that the *stored* form doesn't tell you when
  anything happens — analyzability, export, and preview all require
  evaluating the chain. SMIL is the cautionary tale of maximal chaining;
  QLab/Eos survive because the chain vocabulary is tiny (two fields) and
  a single engine evaluates it.

**The clean split the survey converges on:** chaining/quantization are
*fire-time* semantics; committed records hold *resolved* absolute
positions. Ableton demonstrates it within one product: a Session clip
(no position, launch-quantized, follow actions) versus the *same clip
record* committed to the Arrangement (absolute beat position). Kaijutsu's
write barrier already enforces the Arrangement half; the Session half —
elastic, event-relative firing — belongs to producers/transport (the OODA
loop scheduling "one phrase ahead" *is* a follow action evaluated at
authoring time).

## 8.3 Media-reference lessons

The survey's starkest pattern: **no system references media by content
hash.** The spectrum runs:

| Mechanism | System | Failure mode |
|---|---|---|
| 8-char reel name | EDL | relinking hell, the original |
| File path/URL | QLab, Reaper, Ableton, OTIO ExternalReference | moves/renames break shows & sessions |
| Folder + index convention | TidalCycles | re-sorted folder silently changes the music |
| Name-hash ID into banks | Wwise | rename breaks; content swap is *invisible* (feature and bug) |
| Authority + object ID | MOS (mosID/objID), SCTE-35 UPID | authority unavailable = unresolvable |
| In-media identity (USID/UMID + TimeReference) | BWF/AES31 | best-in-class provenance; niche adoption |

Lessons for the CAS-ref clip cell:

1. **Content addressing eliminates the dominant failure class** of forty
   years of these systems. Nobody did it because their media mutated in
   place (renders, re-conforms) and CAS requires the storage discipline
   kaijutsu already has. This is a genuine structural advantage — keep it.
2. **OTIO's deeper lesson is not the URL — it's the indirection + the
   named unresolved state.** `MediaReference` as a polymorphic slot, and
   `MissingReference` as a first-class value rather than a dangling
   string, is what makes relinking *tractable*. Kaijutsu's analogue
   already exists: `Fallback` is required on every `Recipe`, and a CAS
   miss at fire time should be an explicit, recorded state (skip-loud per
   pcm.md), never a silent path-style failure.
3. **The cost of hashes is opacity** — to humans *and to the models
   authoring cells*. Wwise's name→ID hash shows the mitigation: authoring
   happens in names, runtime happens in IDs. The clip cell should carry a
   human/model-facing label (sample name/provenance) *alongside* the hash,
   with the hash canonical. BWF shows where richer provenance lives: as
   metadata attached to the *media* (a CAS sidecar), not fattening every
   cell that references it.
4. **Mutable intent needs a name layer.** A hash pins bytes forever; "the
   current kick sample" is a *different concept* (Wwise's whole event
   indirection; SCTE-35's defer-to-ad-server). When kaijutsu wants late
   binding, that's a `Deferred` body resolving a name→hash at commit —
   two existing mechanisms, no new reference type.
5. **Answer dangling references, never swallow them.** MOS requires a
   device that receives an action naming an unknown roID/storyID/itemID
   to send `roReq` back — the spec-mandated version of fail-loud. Same
   family as OTIO's `MissingReference` and kaijutsu's required
   `Fallback`: an unresolvable ref is a first-class, *visible* state.

## 8.4 Trigger-semantics lessons: record vs transport

Sorting every trigger mechanism in the survey by *where it lives*:

- **In the stored record:** QLab pre/post-wait + continue mode, Eos
  follow/hang/block, Ableton follow actions + launch quantization, SMIL
  syncbase. Works when one engine owns evaluation and the vocabulary is
  tiny; becomes unanalyzable as it grows (SMIL).
- **In the transport/protocol:** MSC GO, AMCP PLAY, OSC bundles,
  PostEvent. The record stays inert data; liveness comes from outside.
  These are the systems that aged best.
- **Resolved at fire time, then gone:** Wwise transition sync points,
  Ableton launch quantization (the *clip* doesn't remember which bar it
  launched on — the arrangement recording does).

For kaijutsu the assignment is nearly forced by existing invariants:

- The **committed clip cell** holds only resolved placement (`Span` at a
  `Tick`) — the write barrier already forbids "this cell fires when that
  one ends" as stored, unresolved dependency.
- **Follow/continue/quantize live in producers and the beat scheduler**:
  the musician's OODA loop scheduling a phrase ahead is QLab auto-follow;
  arm/stop and `kj transport play` are MSC GO; a future "launch this clip
  at the next bar" verb is Wwise's *next bar* sync point — a
  quantize-then-commit operation on a *proposed* cell, not a field on a
  committed one.
- The one trigger-ish field that *does* belong in the data model:
  a **quantization hint on the uncommitted/proposed cell** (grid unit to
  snap to), consumed at commit and inert afterward. Cheap, honest, and
  matches how every beat-time system in the survey behaves.
- One transport-side pattern worth naming: CasparCG's
  `BEGIN … COMMIT|DISCARD` batches cue mutations atomically. Kaijutsu's
  per-context mailbox / per-track timeline lock already plays this role
  for block insertion; if a producer ever commits *several* clip cells
  that must land together (a multi-lane hit), the batch-atomic verb is
  the precedent — an existing gate to route through, not new machinery.

## 8.5 Do we need a format at all?

Amy's hypothesis: a cue **data model on the track** — fields and semantics
in our own cell records — with no standalone interchange format. The
survey supports it, with precedents on both sides:

**For:** OTIO's real product is the data model + API; the file format is
its serialization and most users never hand-write one. Ableton/Reaper/
Wwise never published their clip formats as standards — the model lives
in the tool, and the world interoperates through *exports* (stems, MIDI,
EDL/AAF/OTIO adapters). WebVTT/CUE-sheet survival shows simple formats
win *when the payload must cross organizational boundaries in files* —
which kaijutsu's clip cells don't: they live in a CRDT log, sync by CRDT,
and their bytes live in CAS. And AES31/ADL is the cautionary converse: a
technically excellent standalone interchange format that stalled because
no ecosystem forced adoption — building the format *first* buys nothing.

**What would force a format** (in rough order of likelihood):

1. **External interchange** — exporting a session to a DAW/NLE, or
   ingesting one. Answer: an *exporter/adapter* (OTIO, SMF, stems), not a
   native format. The internal model just needs to be lossless enough to
   project from — which the convergent-field analysis above is the
   checklist for.
2. **Cross-kernel drift** — a clip cell drifting to another kernel is the
   moment cell records cross a trust/version boundary as data. But
   kaijutsu already has the answer machinery: drift offers carry hyoushigi
   data (cells + tempo + PPQ — honoring the SMF tempo-map lesson), and
   at-rest serialization is already versioned CBOR with fail-loud
   decoding. The "format" is the versioned schema of the cell record —
   which exists the moment the data model does. No *second* format needed.
3. **Archival** — the CRDT log + CAS is the archive; an export for
   outside-the-kernel posterity is case 1 again (and the TTML/WebVTT
   lesson predicts it: rich internal, simple export).

So: **design the data model; the format is its serialization, and
interchange is adapters.** The only discipline the format-less path
demands is the one OTIO teaches — version the record per-object (the
`"OTIO_SCHEMA": "Clip.5"` move; kaijutsu's CBOR codec version byte is the
same instinct) and keep an extension bag so unknown fields survive
round-trips.

## 9. Candidate clip-cell shapes for kaijutsu

All three candidates assume the settled division of labor: the **cell**
owns identity (BlockId/provenance), timeline placement (`Span{start,len}`
in `Tick`s), and lifecycle; the **payload** (the clip MIME body) owns the
media reference and everything about *how* to render it. The payload never
repeats the tick. Payload is a small human/model-readable text record
(JSON-ish or key:value — serialization secondary), MIME something like
`application/vnd.kaijutsu.clip+json`.

### Shape A — the WebVTT-grade minimal clip (recommended first)

```
media:      ContentHash            # canonical; CAS ref
mime:       string                 # audio/wav, audio/flac… (the AudioFormatHint source)
label:      string                 # REQUIRED — human/model-facing name/provenance
src_offset: media-time (ms or samples)   # optional, default 0 — where in the media to start
src_len:    media-time             # optional, default "to end" (cell Span may truncate)
gain:       float (dB or linear — pick one, say so)   # optional, default unity
```

- **Honors:** WebVTT/CUE-sheet minimalism (smallest viable record wins);
  the EDL/OTIO source-range-vs-placement split; the CAS advantage; the
  mandatory-label lesson (anti-SCTE-35-hex).
- **Ignores, deliberately:** tempo-elasticity (a tempo change after
  placement changes where the clip *starts* but not its internal rate —
  Reaper's "timebase: time" default), fades, loops, envelopes.
- **Trade-off:** three sessions from now someone wants a fade and a loop.
  That's fine — the extension-bag discipline (§8.5) makes A grow into B
  without a migration, and A matches pcm.md's first consumer ("play
  `<hash>` at this tick, this gain") exactly.

### Shape B — the DAW-grade clip

Everything in A, plus:

```
rate:        float                 # playback rate; pitch-preserve flag alongside
stretch:     none | repitch | stretch      # Reaper's per-item timebase policy, stored
beat_anchor: optional (media-time, tick-delta) pairs   # degenerate warp markers;
                                   # one pair = "media's downbeat is here"
loop:        optional {src_start, src_len}  # loop brace within the media
fade_in/out: optional {len: TickDelta or media-time, shape}
env:         optional [{param, points: [(TickDelta, value)]}]   # clip-local breakpoints
color/notes: optional              # the human cluster, full strength
```

- **Honors:** Ableton's warp-as-stored-data and Reaper's declared tempo
  policy (§8.2) — the two lessons wall-time media on a beat timeline
  cannot dodge forever; clip envelopes as the baked half of the param
  cluster.
- **Ignores:** late binding/live params (still no RTPC analogue — right,
  because that's resolver territory), follow semantics (right, per §8.4).
- **Trade-off:** front-loads schema nobody consumes yet, violating the
  lands-with-its-consumer rule; envelope points in `TickDelta` vs
  media-time is a real decision being made before a renderer exists to
  test it. B is the *destination*, not the first move.

### Shape C — the Wwise-grade symbolic cue (mostly already built)

The insight from §5 restated: a late-bound cue = a name + parameters,
resolved against live context at fire time. In kaijutsu that is not new
payload fields — it is a **`Deferred(Recipe)` cell body**: resolver id +
params + `ContextQuery` + required `fallback`, resolving at the commit
deadline to a Shape-A/B concrete payload (name→hash binding, switch-like
selection on committed state, RTPC-like param reads via the basis
mechanism). The "cue sheet" — which names map to which samples/params —
is ordinary committed/config state the resolver reads.

- **Honors:** the event-indirection lesson (design iterates without
  touching triggers), SCTE-35's defer-the-asset-decision, MissingReference
  via the required `fallback`.
- **Ignores:** nothing — but it *costs* a resolver implementation and a
  basis design (`compute_basis` for reactive resolvers is explicitly open
  in hyoushigi.md), so it's gated on that open question.
- **Trade-off:** zero new record surface (its output is Shape A), which is
  the strongest possible argument that the payload schema and the
  late-binding question are independent — A does not foreclose C.

### Recommendation

Ship **A** as the clip payload with the OTIO-style versioned-record +
extension-bag discipline; grow toward **B** field-by-field as renderers
demand (stretch-policy first — it's the one A silently answers by
omission, and the survey says silent answers here breed complaints);
treat **C** as a resolver milestone, not a payload design. Keep every
trigger/quantize/follow concept out of the committed record and in the
transport/producer layer, with at most a quantize hint on proposed cells.
And per §8.5: no standalone format — the data model on the track, its
versioned CBOR/text serialization, and future exporters (OTIO, SMF) when
interchange actually knocks.

---

## Appendix: source notes

Fetched (see inline links): OTIO readthedocs (timeline structure, file
format spec, media linker), OTIO GitHub issues #1701/#482, niwa.nu and
edlmax EDL references, AMWA AAF object spec, Wikipedia (EDL, AAF, SMPTE
timecode, AES31, iXML), 3Play/Sonix drop-frame explainers,
REDUSER/Blackmagic relink threads, W3C SMIL 3.0 Timing, W3C WebVTT, MDN
WebVTT, W3C TTML2, Balisage "WebVTT vs TTML," CSS-Tricks SMIL
retrospective, WHATWG HTML-vs-SMIL wiki, EBU-TT/SMPTE-TT compliance
overviews, Steinberg WaveLab AES-31 docs, AES31-4-2024 announcement, Avid
DUC AES-31 thread, wavinfo bext/iXML docs, digitizationguidelines.gov BWF
embedding guideline, Hydrogenaudio cue-sheet wiki, Ableton manual
(warping / clip view / launching clips) + Live 11 follow-actions note +
Ableton forum warp threads, Bitwig user guide (launcher clips), a real
`.rpp` project file + Cockos forum ITEM-chunk thread, Csound manual
(i/f/t statements, named instruments), doc.sccode.org (Pbind,
ServerTiming), OSC 1.0 spec, scsynth.org threads, tidalcycles.org
mini-notation reference, strudel.cc docs + Strudel discussion #629,
midimusic.github.io SMF spec, somascape SMF reference, MIDI.org forum
threads (delta-time/tempo), MIDI 2.0 references, MusicXML 4 `<sound>`
reference, MEI v4 guidelines (facsimiles/recordings), MOS 2.8.5 spec
(mosprotocol.com) + mosromgr docs, CasparCG AMCP wiki + casparcg-server
repo + GitHub issue #1685, Audiokinetic Wwise 2025.1 docs (Managing
Events, States/Switches/RTPCs, Interactive Music, Music Segments,
Transitions, Stingers, SoundBanks, glossary — incl. `AkFNVHash.h`), FMOD
Studio 2.02 manual (authoring events, instruments, parameters, snapshots,
banks), javierzumer + thegameaudioco Wwise/FMOD comparisons.

From model knowledge (primary docs to spot-check load-bearing specifics):
QLab manual (qlab.app/docs), MSC = MMA RP-002, ETC Eos operations manual,
SCTE-35 2023 standard + RFC 8216 / Apple HLS interstitials.
