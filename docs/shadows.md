# Shadows

A shadow is a context that follows another context, its source, and holds
renditions of the source's blocks: speech for a model's text, a description
for an image, an image for a description, or the per-member trace of a swarm's
merged reply. The judge shadow (`docs/council.md`, "Shadow context") and the
description shadow ("What is built") exist. The rest of this document is
direction.

Amy, 2026-11-15: "we will have more use of models where they 'shadow' each
other for accessibility across eg tui and mcp, or do things like tts for the
model's output. so that seems like it could be a 'shadow context' ... images
can get text descriptions via another model. maybe even descriptions get
images and so on ... Some maybe we do on the fly like tts, some on demand,
like maybe tui requests image descriptions as it goes and they sorta get
cached in the shadow context. also as we accomodate swarms, we could do
partial decoding of the members for observability while the main context is
the swarm's merged output."

## What is built

`kj shadow describe <block> [--context <ctx>]` writes a description of an image
block. `kj shadow list [--context <ctx>]` shows a context's shadows.
Both are one mechanism for the judge shadow and the description shadow:

- **The shadow marker.** `context_shadows(context_id, kind)` has one row per
  shadow. A context is a shadow when it has a row and its parent is still its
  senpai. The kinds are `judge` and `describe`. Archiving a source archives all
  its live shadows in the same transaction (`KernelDb::archive_with_shadows`).
- **The description shadow.** One live shadow of kind `describe` exists per
  source context, made by the first request. The source's own performer plays
  it, its parent and senpai are the source, and its `model` slot is a copy of
  the performer's `image` slot (`kj character slot set <character> image
  <backend>/<model> --max-tokens <n>`). A performer with no `image` slot
  gets a refusal that names that verb: no other model stands in. The shadow
  takes no chat turn (`KernelDb::chat_turn_refusal`). A later change to the
  character's slot reaches a new shadow, so archive the old one to pick it up.
- **The rendition link.** `shadow_renditions` links each rendition block, by
  its context, principal, and sequence, to its source block, with `span_start`
  and `span_end` that are NULL for the whole block. Nothing sets a span yet.
- **The index.** `rendition_index` maps the source block's CAS hash (the
  content id), the kind, the maker (`<backend>/<model>` of the shadow's `model`
  slot), and a preset (empty today) to the rendition. The block holds the text.
  A request whose key is in the index, from any context's shadow, calls no
  model: it copies the earlier rendition's text into its own shadow as a new
  block with its own link, and says it reused one.
- **The request.** `describe` takes a `ContentType::Media` block whose CAS
  record names `image/*`; any other block is refused, saying what it is. Video
  waits for a `video` slot. It sends one user message that holds the media, with the instruction
  `/config/kernel/description.md` as the system text (`docs/prompts.md`),
  through the provider's one-shot path, so a council backend sends an object
  when the served model reads the type and the other providers send images. A
  model that would get only a marker is refused before the call, naming the
  model and the type. The call waits at most `DESCRIBE_TIMEOUT`, 120 seconds, and
  is recorded as a turn of role `image` on the shadow.

The rest of what exists:

- **The judge shadow** is a child context performed by the judge character,
  with the watched context as its parent and its senpai. The gate is its only
  writer, it takes its model from the judge character's slots, and it is
  archived with its source (`council/shadow.rs`, `docs/senpai-kohai.md`). It
  keeps its own rotation cycle: a successor's first gate call makes a new one.
- **The score context** is a context with no performer that never takes a
  turn and holds blocks other contexts produce (`docs/tracks.md`, "The score
  container is a real per-track score context").
- **The `DeriverRegistry`** inserts a derived block beside its source at the
  commit barrier, with `parent_id` naming the source. Work heavier than a
  cheap transform is a timeline resolver, never a deriver
  (`docs/hyoushigi.md`).
- **CAS** holds each media object once, with its media type in the CAS record.

The judge shadow and the description shadow share one mechanism. Amy:
"perhaps the score stuff should move to use this later."

## Two questions sort every rendition

**Whose is it?**

- *The producer's.* It is part of the performance: a character's speech, a
  swarm member's trace. It is made while the source streams and is timed
  against it.
- *The viewer's.* It serves one reader: a description for a tui reader, speech
  for a screen reader. A client asks for it, and the kernel keeps it for the
  next reader.

**What is it a function of?**

- *Content only.* Speech for a block's text, or a description of an image,
  depends on the content and on the model and preset that made it. The index
  finds it again by those, so a fork or another context reuses it.
- *The conversation.* A description that knows what the conversation is about,
  or speech that keeps its tone across turns, needs the shadow's own
  conversation. Content-only renditions come first.

## Where renditions live

Each rendition is a block in a shadow context, and an index finds renditions
by content (Amy: "yeah B, with an index"). The alternatives were siblings in
the source context, which fill the producer's log with blocks one viewer
asked for, and an index with no context, which gives speech nowhere to stream
and nothing to browse.

- **A shadow is a child of its source.** Its parent and its senpai are the
  source. One live shadow exists for each source and kind, except that the
  judge shadow rotates (a successor makes a new one).
- **A rendition names its source block,** and optionally a span of it: a
  sentence for speech, a token step for a swarm trace, the whole block for a
  description. The link crosses contexts, which `parent_id` does not today.
- **The index maps a source's content id, the kind, and the model and preset
  to the rendition's content id.** A hit costs a copy of a reference, not a
  model call.

## Slots per media kind

A character says, for each kind of media, which model reads it: an `image`
slot today, and `audio` and `video` slots to come. Nothing is inferred from
what a server says a model can read. Amy, 2026-11-15: "image reading should be
explicitly enabled by setting it on the character, even when pointing at the
same model. no automatic behavior there. then if it points at another model
it's pretty clear too. and if there's only text that's all it can do."

- **The slot names the character's own model:** the model reads that media
  directly in its turns.
- **The slot names another model:** that model makes a description (or a
  transcript, for audio), and the description goes into the turn in place of
  the media, found through the index. A small-window model saves its context
  this way. Amy: "some of the smaller models will quickly blow out their
  context".
- **No slot:** the character reads text only, and media is a marker.
- **The same slot serves viewers.** `kj shadow describe` asks the source
  performer's `image` slot.

Turns do not follow this rule yet: today a turn sends media whenever the
provider can (`docs/issues.md`, "Context shadows").

## Kinds

| Kind | Made | Performer and model | Notes |
|---|---|---|---|
| judge | after each gated call | the judge character | Built. Its rotation cycle is its own. |
| speech | as the source streams | the source's character, with its speech preset | Timed: a resolver with a lead time. |
| describe | when asked (`kj shadow describe`) | the source's performer, on its `image` slot | Built for images. Cached by content. |
| illustration | when a client asks | an illustrator character | An image for a description. |
| swarm trace | with the merged reply | the server, no model call | Each member's choice and agreement at each step. |

**Speech is the character's.** Amy: "character's voice by default, yeah.
viewer override is cool idea that can wait. maybe best to just have the
character have a voice preset." The term is "speech", because "voice" names a
musical voice (`docs/writing.md`, Terms). A character's sheet names a speech
preset: the model, such as Qwen3-TTS on the nagaya, and its voice design
prompt or speaker. A viewer override comes later.

**A swarm trace costs nothing extra.** The nagaya's ensemble combines the
members' logit rows at each step and samples one token, which every member
takes (`~/src/nagaya/docs/lab.md`, "`ensemble`: joined forks"). The server
holds each member's rows, so it can report what each member would have chosen
and how far it agreed. The merged reply is the swarm context's block; the
trace is its rendition, and the app can show any member's view from it.

## Timing

Speech is the kernel's central case: a producer with a lead time whose output
must arrive while it is still useful (`AGENTS.md`). A speech shadow follows
its source at sentence boundaries. Each piece is admitted as a resolver, whose
start and polls do not block, and a piece that misses its moment takes the
declared fallback: it is skipped, or played marked late. A rendition a client
asks for has no moment; it reaches the client through the change feed when
it is ready.

## Lifecycle

- **Archive.** A shadow is archived with its source, as a judge shadow is.
- **Rotate.** A shadow rotates with its source, so the successor's shadows
  continue the predecessor's. Amy: "a feature here might be cleanly rotating
  shadows along with the parent, judge maybe wants a different variant (if it
  only has the user inputs, and does not grow at the same rate, maybe its
  cycle is separate)."
- **Fork.** A fork child starts with no shadows. Its first renditions find the
  parent's through the index.

## Media blocks

`ContentType::Media` is a block whose text is the CAS hash of an image, audio,
or video object. The CAS record is the one owner of the object's real media
type, and the turn path takes it from there. Stored blocks that say `image`
still read as `Media`. On the wire, `contentType` carries
`application/vnd.kaijutsu.media`, which names no particular type.

This is the first step, and it is also the next omni step: an audio block a
model can hear (`docs/chameleon.md`, "Omni models"). A council turn sends any
media part the served model reads as an object; Claude, the OpenAI-compatible
providers, and Gemini send images and write a marker for the rest.

## Transcodes

A transcode is a copy of a media object in another format: a WAV resampled
to 16 kHz, made mono, or encoded for an API's limits. It is not a rendition:
no model makes it and no one reads it as a block.

- **The source stays the truth.** The block names the original's hash, and a
  transcode never replaces it.
- **A transcode is a function of the bytes and a recipe:** the target format,
  rate, and channels, and the transcoder's name and version. Its output is a
  CAS object, and the index maps the source id and the recipe to it, as it
  maps renditions.
- **A transcode can be evicted; a source cannot.** The kernel can make a
  transcode again and cannot make the source again.
- **The seam that needs the format makes it:** a provider for its API, an
  audio sink for its device rate. A transcode is work behind admission and
  never runs on the beat path. An in-process Rust transcoder is ordinary code;
  a subprocess such as `ffmpeg` is a new host execution path and needs a
  design conversation (`AGENTS.md`).
- **The nagaya needs none.** It resamples any `audio/wav` to 16 kHz mono
  itself, and its recipe names the resampler's version
  (`~/src/nagaya/docs/spec.md`, "Video, and audio at any rate"). A turn on
  the nagaya sends the original, so the server's record of what it read stays
  complete.

## Open questions

- How a client learns a media block's type. It does not yet: the wire
  `contentType` is the generic media marker, so the app draws `[media: <hash>]`
  for every media block. Two ways: the client looks the hash up in CAS, or the
  kernel fills the wire `contentType` from the CAS record.
- Where a speech preset lives on the character sheet, and whether its model is
  a slot.
- How a client asks for a rendition: an RPC, since requests come at
  interaction rate. `kj shadow describe` waits for the model and prints the
  description; a client needs the request to return at once and the rendition
  to arrive through the change feed.
- How a client plays speech in time: the app, through audiod, or both.
