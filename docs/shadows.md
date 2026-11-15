# Shadows

A shadow is a context that follows another context, its source, and holds
renditions of the source's blocks: speech for a model's text, a description
for an image, an image for a description, or the per-member trace of a swarm's
merged reply. This document is direction. Only the judge shadow is built
(`docs/council.md`, "Shadow context").

Amy, 2026-11-15: "we will have more use of models where they 'shadow' each
other for accessibility across eg tui and mcp, or do things like tts for the
model's output. so that seems like it could be a 'shadow context' ... images
can get text descriptions via another model. maybe even descriptions get
images and so on ... Some maybe we do on the fly like tts, some on demand,
like maybe tui requests image descriptions as it goes and they sorta get
cached in the shadow context. also as we accomodate swarms, we could do
partial decoding of the members for observability while the main context is
the swarm's merged output."

## What exists

- **The judge shadow** is a child context performed by the judge character,
  with the watched context as its parent and its senpai. The gate is its only
  writer, it takes its model from the judge character's slots, and it is
  archived with its source (`council/shadow.rs`, `docs/senpai-kohai.md`).
- **The score context** is a context with no performer that never takes a
  turn and holds blocks other contexts produce (`docs/tracks.md`, "The score
  container is a real per-track score context").
- **The `DeriverRegistry`** inserts a derived block beside its source at the
  commit barrier, with `parent_id` naming the source. Work heavier than a
  cheap transform is a timeline resolver, never a deriver
  (`docs/hyoushigi.md`).
- **CAS** holds each media object once, with its media type in the CAS record.

A shadow generalizes the judge shadow. Amy: "perhaps the score stuff should
move to use this later."

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
  source. One shadow exists for each source and kind.
- **A rendition names its source block,** and optionally a span of it: a
  sentence for speech, a token step for a swarm trace, the whole block for a
  description. The link crosses contexts, which `parent_id` does not today.
- **The index maps a source's content id, the kind, and the model and preset
  to the rendition's content id.** A hit costs a copy of a reference, not a
  model call.

## Kinds

| Kind | Made | Performer and model | Notes |
|---|---|---|---|
| judge | after each gated call | the judge character | Built. Its rotation cycle is its own. |
| speech | as the source streams | the source's character, with its speech preset | Timed: a resolver with a lead time. |
| description | when a client asks | a describer character | Cached by content. |
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

Every kind needs a block that holds media other than images. Today
`ContentType` is closed: an image block holds a CAS hash and the turn path
takes its real media type from the CAS record, but audio or video has no
content type and maps to `Plain`. The wire already carries `contentType` as
text (`kaijutsu.capnp`).

This is the first step, and it is also the next omni step: an audio block a
model can hear (`docs/chameleon.md`, "Omni models").

## Open questions

- The shape of the media content type (see the proposal in the conversation of
  2026-11-15, recorded here once chosen).
- Where a speech preset lives on the character sheet, and whether its model is
  a slot.
- How a client asks for a rendition: an RPC, since requests come at
  interaction rate.
- How a client plays speech in time: the app, through audiod, or both.
