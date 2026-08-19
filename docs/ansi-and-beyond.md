# ANSI and beyond: ingest transforms, provenance, and styled glyphs

Living design doc, born 2026-08-19 from a conversation with Amy about ANSI
styling in the app. The title is the scope: ANSI is the first *ingest
transform*, and the machinery here — strip to a projection, keep the original
bytes as provenance, render spans through the glyph instance buffer — is a
pattern, not a one-off. Sibling design note: `docs/issues.md` "Text effects on
the surface: the instance buffer IS the map (2026-08-18)".

## Why strip

Escape bytes in block content are hostile to every consumer:

- **Models** see either an invisible control character or `\x1b[31m` noise.
  Readable-ish when concentrating, but it burns tokens — and worse, it poisons
  byte-offset arithmetic: an edit range computed over text containing
  invisible codes lands crooked in ways nobody notices until it does.
- **Search, edit, exclusion** all operate on block content byte ranges; codes
  make every range subtly wrong.
- **Every client** would otherwise parse independently and drift.

ANSI escape codes are the terminal world's storage-engine ops. Kaijutsu
doctrine already says players consume *projected facts, not encodings* — so
the kernel strips at ingestion and stores the projection.

## The pattern: provenance, not round-trip

The tempting design is a lossless span map that can round-trip back to the
original bytes. That's the wrong burden to place on the parser: losslessness
means faithfully encoding every OSC, every private-mode sequence, every
malformed fragment, and the 80/20 fidelity we actually want becomes a
liability because the lost 20% breaks the guarantee.

Instead: **keep the original bytes verbatim, and release the span map from
round-trip duty entirely.**

- **Original bytes** → immutable provenance in a kernel-side sqlite table.
  Byte-for-byte what arrived, pre-strip. Never edited, never hydrated, never
  rides the wire. The round trip is trivially perfect because it's a copy.
- **Stripped text** → the block content. The one live document that edits,
  exclusions, search, hydration, and rendering all share. One truth for
  offsets.
- **Span map** → derived metadata, tagged with the parser version that
  produced it. Deterministic, allowed to be 80/20, and **re-derivable**:
  parser improves → replay provenance through it → spans update through the
  ordinary sequenced mutation path.

If a block is edited later, its provenance row goes stale — fine; it records
what *arrived*, not what the document *is*.

The generalization: any lossy ingest transform stores
`(original blob, transform id + version, derived content + metadata)`. The
next transform adds a row kind, not a table. This is why the doc is called
"and beyond".

## What travels where

Blocks have no table of their own — block state serializes into
`oplog.payload` and `doc_snapshots.state`, and every block field rides the
change feed to every client. So the split is:

**On the block** (`BlockSnapshot` in kaijutsu-types, both `#[serde(default)]`
so at-rest payloads deserialize clean):

```rust
/// One styled span over the *stripped* content, byte-offset addressed.
/// Semantic, not resolved: `Indexed(1)` means "ANSI red", themed at draw.
pub style_spans: Vec<StyleSpan>,        // empty for ordinary blocks
pub provenance: Option<ProvenanceTag>,  // { transform: "ansi-strip", version }
```

Spans are small, needed at render time, and reach the app by the feed it
already follows. The tag is deliberately tiny: the affordance that says "an
original exists, you can ask for it" without shipping it anywhere.

**In the kernel** (`kernel_db.rs`), map-shaped as one row per
(block, transform):

```sql
CREATE TABLE IF NOT EXISTS block_provenance (
    context_id    BLOB    NOT NULL,
    principal_id  BLOB    NOT NULL,
    block_seq     INTEGER NOT NULL,       -- the three BlockId components
    transform     TEXT    NOT NULL,       -- 'ansi-strip'
    version       INTEGER NOT NULL,       -- parser version at ingestion
    original      BLOB    NOT NULL,       -- exactly as captured, pre-strip
    created_at    INTEGER NOT NULL DEFAULT (CAST((unixepoch('subsec') * 1000) AS INTEGER)),
    PRIMARY KEY (context_id, principal_id, block_seq, transform),
    FOREIGN KEY (context_id) REFERENCES documents(document_id) ON DELETE CASCADE
) WITHOUT ROWID;
```

Sqlite, not CAS: shell output under kaish's output limits is small and
unique (dedup value ~nil), and cascade-with-context is what we want.

**Retrieval** is a `kj` verb, per the "kj is good enough for admin-like
stuff" rule — occasional and deliberate, no wire method:

```
kj block original <block-id>     # dump the stored bytes
kj block reproject <block-id>    # run the CURRENT parser over the original;
                                 # re-emit spans as a normal sequenced edit
```

**Ingestion atomicity**: clean text, spans, and the provenance row commit in
the same transaction — commit first, publish second. Streaming starts with
buffer-until-done for the provenance bytes (text still commits per flush);
upgrade to append-per-flush only if crash-loss of provenance ever bites.

## Parsing: vte, scope, safety invariants

**Don't write the lexer.** Use `vte` — alacritty's parser, a `no_std`
implementation of the canonical DEC ANSI state machine (vt100.net), fuzzed
for years, incremental by design, with *no execution semantics*: it hands
back callbacks for text / CSI / OSC / ESC and already bounds parameter
counts. What we write is the boring part: SGR integer → style enum, span
assembly. (If kaijutsu ever embeds a real terminal, it'll be alacritty —
same parser lineage, no divergence.)

**SGR scope, 80/20**: fg/bg (16-color indexed, 256, truecolor), bold, dim,
italic, underline, inverse, strikethrough, blink. Indexed colors stay
*semantic* in the span (`Indexed(n)`, not resolved RGB) so themes apply to
terminal output.

**Cursor and screen-control codes: preserve, don't render.** They land in
provenance, are consumed correctly by the parser (sequence boundaries known,
payloads never leak into text), and produce nothing in the projection. This
kills the hidden-text deception class by construction: no cursor motion means
no overwrite, so what the model reads and what the human sees cannot be made
to diverge that way.

**Standing safety invariants** (stated, not merely tested):

1. We never *respond* to sequences (DSR/DA answerback injection — dead; we
   are not a terminal).
2. We never *execute* OSC (OSC 52 clipboard-write is a real exfil vector in
   terminals; here it is consumed inert into provenance).
3. Anything that replays raw provenance bytes to a real tty is an explicit
   deliberate act, never a default path. Stripping at ingestion protects
   every downstream consumer that forgets to think about it.

## Test ladder

Cheapest to heaviest; the chunk-boundary property is the one that will catch
*our* bugs (kaish output streams, sequences straddle flushes):

1. **Totality fuzzing** — cargo-fuzz over arbitrary bytes: never panics,
   memory O(input), time linear. With vte underneath, this mostly exercises
   our span assembly.
2. **Chunk-boundary property** — parse split at every position (and random
   splits) ≡ one-shot parse, byte-identical output.
3. **Projection properties** — stripped text = input minus recognized
   sequences (differential vs a naive stripper on well-formed input); span
   offsets land on UTF-8 char boundaries of the stripped text;
   `strip(strip(x)) == strip(x)`.
4. **Real-world goldens** — captured `cargo test`, `git diff --color`,
   `ls --color`, fastfetch; insta-snapshotted token streams. These pin the
   80% and document what "close enough" means.
5. **Differential SGR attribution** — corpus through a reference
   implementation (`termwiz` or the `vt100` crate), compare color/attribute
   assignment on the SGR subset.
6. **Standing CI invariant** — for any unedited block with a provenance row:
   `strip(original) == (content, style_spans)`. Runnable against a real
   kernel.db, not just fixtures.

## Rendering: spans → instance buffer → style table

The conversation surface already draws every glyph through
`msdf_surface.wgsl` every frame, from a per-glyph instance buffer
(`GlyphInstance`: doc position, quad, UV, color, importance). Static ANSI
color could ride the existing color attribute with zero shader work — but
the better version adds one indirection:

- Glyphs carry a small **style index** into a style/effect table in GPU
  memory (storage buffer). The shader resolves index → params.
- **ANSI colors stay semantic on the GPU too**: a glyph says "ANSI red"; the
  palette lives in the table and maps through the scene palette. A retheme
  is a ~1KB buffer write, not a window rebuild.
- **Animated effects are table entries**: rainbow = effect kind +
  `hue(doc_pos, time)` (time is already a uniform); ANSI blink and the
  chrome chase ride the same slot. The rainbow-on-input revival is just a
  span with effect index N.
- **The consistency split is a rule**: the instance buffer changes only on
  window rebuild (structure — which glyphs, where); the style table changes
  at animation/theme rate (parameters). Per-glyph *dynamic* data never
  migrates into the instance buffer — that's the rebuild trap the surface
  rewrite escaped.

Async CPU-side updates are safe by platform shape: Bevy's extract snapshots
main-world state once per frame and wgpu queue writes land before that
frame's draws — "next-frame consistency", never a torn mid-frame read.

ANSI needs two things that aren't glyph-fragment work: **backgrounds** and
**underline/strikethrough** are geometry, built at assembly time from the
span map into the existing quad/chrome lanes. Bold maps to the existing
`importance` (MSDF weight).

## Policy in rc, mechanism in the kernel

Could the strip/transform phase be kaish/rc composition? Split the question:

**The streaming strip itself: no — it's a chatty path.** Per-flush parsing
runs at interaction rate, and the same rule that keeps compose keystrokes on
RPC ("routing a chatty path through a shell dispatch per event is a cost
with no benefit") keeps it out of script dispatch. There's a second,
stronger reason: the provenance row records `transform + version`, and the
CI invariant `strip(original) == (content, spans)` needs the transform to be
a *named, versioned* thing. A user-edited rc script can't honestly sign a
version. So the parser is a kernel builtin: `ansi-strip@N`.

**Which transforms bind to which flows: yes — that's rc's job.** rc doesn't
implement the filesystem; it decides what mounts where. Same here: rc (or
`/etc/config`) declares "shell output in coder contexts passes through
ansi-strip", per context type, per flow. Policy is composition; mechanism is
versioned.

**Ad-hoc composition: yes — the transform is also a kaish verb.** `ansi-strip`
usable in a kaish pipeline for experiments (`fastfetch | ansi-strip --spans`),
and `kj block reproject` is the same mechanism invoked deliberately. One
implementation, three access paths (ingest hook, kaish verb, kj admin verb).

## The RC boot aesthetic

The flip side of stripping is *emitting*: rc scripts producing beautiful
ANSI console output — old-school boot systems isekai'd into the world of AI.
Good news: the classic look (`[ OK ]` in green, columns, banners) is almost
pure SGR plus spaces; sysvinit never needed cursor addressing. SGR-only
rendering covers the aesthetic fully, and the pipeline is exactly the
composition above: rc emits ANSI → kernel strips to spans + provenance →
app renders it in glory → the model reads clean text. Every player gets
their native projection of the same fact.

A kaish palette/formatting helper for rc scripts (named colors, OK/FAIL
markers, column alignment) is the eventual ergonomic layer; not needed for
slice 1.

## Open questions

- **`\r`-overwrite progress bars** (cargo, pip): not ANSI but same family.
  Preserve-don't-render means stripped text keeps every intermediate frame —
  noisy for hydration. A later 80/20 collapse ("keep the final frame of a
  `\r` run") is probably right; decide when it bites.
- **Provenance growth**: no eviction story yet. Cascade-with-context bounds
  it; if a long-lived context's provenance ever matters, add an age/size
  sweep. Watch, don't build.
- **Span edits**: block edits invalidate span offsets past the edit point.
  Slice 1 answer: an edit to a styled block drops its spans (content is
  still correct, just unstyled); reprojection can restore them for
  provenance-backed blocks. Smarter span rebasing only if we ever care.
