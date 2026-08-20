# Tech-debt audit: ANSI ingest + block provenance + conversation surface

Territory: the work merged 2026-08-19 on moltar — `84505811`, `167f41b0`,
`9f5b5128`, `a2eb2632`, `ab185532`, `d6264828`, `2c531cec`, `7b17adfe`,
`bc02e244`, `1fb5ff25`, `1461740f`, `1f20eda6`.

Read whole: `crates/kaijutsu-ansi/` (src, tests, fuzz, corpus),
`kaijutsu-kernel/src/ansi_ingest.rs`, the six hook sites, the
`block_provenance` table + accessors + `kj block original|reproject`,
`kaijutsu-types/src/block.rs` span section, `kaijutsu.capnp`, the client
decode, `kaijutsu-app/src/text/ansi.rs`, `text/msdf/layout_bridge.rs`,
`text/msdf/surface_renderer.rs`, `assets/shaders/msdf_surface.wgsl`,
`view/surface/*.rs`, `docs/ansi-and-beyond.md`, relevant `docs/issues.md`.

`cargo check --workspace --all-targets` is clean.

Overall: this is high-quality work. The parser crate is genuinely well
tested, the provenance table is correctly keyed, and the six-site
consolidation is real. The debt is concentrated in three places — a legacy
renderer that is gone from the code but not from the prose, a design doc
whose superseded sketches were never deleted, and a couple of claims
("one policy", "the CI invariant") that the code does not actually back.

---

## BUGS

### B1. The `provenance` tag survives an edit and then lies

`crates/kaijutsu-kernel/src/blocks/content.rs:384-393` — `edit_text` clears
`style_spans` and deliberately keeps `provenance`.
`crates/kaijutsu-types/src/block.rs:1661-1664` documents the tag as: *"Set
when `content` is an ingest-transform projection and the kernel holds the
byte-exact original."* After any edit that is false — the content is no
longer the projection — yet the tag stays.

Same file contradicts itself: `block.rs:1425-1431` states the weaker,
correct claim (spans drop on edit, provenance is for reprojection).

Consequence, and this is the part that makes it a bug rather than a
doc nit: **nothing in the durable state distinguishes an edited tagged
block from an unedited one.** `docs/ansi-and-beyond.md:219-221` specifies
the standing invariant as *"for any unedited block with a provenance
row"* — there is no way to evaluate "unedited". Empty `style_spans` is not
a discriminator, because a legitimately-projected cursor-motion-only block
also has empty spans (`ansi_ingest.rs:180-187`,
`escape_sequences_without_styling_still_project`).

`kj block reproject` copes (it compares text and refuses,
`kj/block.rs:1089-1099`), so nothing is corrupted today. But the invariant
the code repeatedly cites cannot be mechanically checked against a real
kernel.db as written.

- Action: pick one. Either clear `provenance` in `edit_text` alongside the
  spans (then tag ⇒ projection holds, and the invariant is checkable), or
  weaken `block.rs:1661-1664` to "an ingest transform produced this block's
  content at insert; the original is still stored" and give the invariant a
  real discriminator (an `edited` flag, or compare against
  `updated_at`/version). Reconcile `block.rs:1425` and `:1661` either way.
- Size **M**. Risk: clearing the tag on edit loses the affordance that
  `kj block original` still has bytes worth reading — probably why it was
  kept. The doc fix is the cheap half; do that first.

### B2. `background_exec` re-derives the ingest predicate, and gets a different answer

`crates/kaijutsu-kernel/src/background_exec.rs:825-830` decides "does this
need the transform?" with an inline `chunk.contains(&0x1b)`.
`ansi_ingest::project` (`ansi_ingest.rs:74-83`) decides it with **two**
guards: the `ESC` memchr *and* "escape bytes present but the projection is
byte-identical with no spans → still a no-op, because a tag whose original
equals its content teaches nobody anything."

`background_exec` has only the first guard. A background process that emits
a lone stray `\x1b` and nothing else gets a `provenance` tag, a
`block_provenance` row and a `SpansChanged` event that
`ansi_ingest::project` exists specifically to suppress.

This is also the literal drift the module doc says cannot happen
(`ansi_ingest.rs:15-17`: *"Keeping the policy in one function is the point.
The rules below are subtle enough that six independent copies would drift
within a month."*). It drifted in the same commit.

- Action: give `AnsiDrain::finish` the second guard — `if spans.is_empty()
  && self.parser.text().as_bytes() == raw { return None }` — or better,
  export the predicate from `ansi_ingest` (e.g. `fn is_noop_projection(text,
  spans, raw) -> bool`) and call it from both. Also swap the literal `0x1b`
  for `ansi_ingest::ESC` (make it `pub(crate)`).
- Size **S**. Risk: none; it strictly reduces the set of tagged blocks.

---

## 1. Two mechanisms for one thing / incomplete deletion

### 1.1 "Six sites, one policy" is five sites and one re-derivation — **HIGH**

`crates/kaijutsu-kernel/src/ansi_ingest.rs:1` and
`docs/ansi-and-beyond.md:133-141` both claim every hook site is three lines
(`project` → write clean text → `record`). Verified call sites:

| site | uses `project()` | uses `record()` |
|---|---|---|
| `kaijutsu-server/src/rpc.rs:8407` (interactive shell) | yes | `:8424` |
| `kaijutsu-server/src/rpc.rs:8708` (`kj` capture) | yes | `:8718` |
| `kaijutsu-server/src/llm_stream.rs:1078` (inline tool result) | yes | `:1101` |
| `kaijutsu-server/src/llm_stream.rs:2214` (agentic tool result) | yes | `:2237` |
| `kaijutsu-kernel/src/kj/lifecycle.rs:812` (rc scripts) | yes | `:832` |
| `kaijutsu-kernel/src/background_exec.rs` (background procs) | **no** — `AnsiDrain`, `:825` | `:976` |

The streaming site genuinely cannot call `project()` (it needs the
incremental parser), and that is fine — but the doc and the module comment
state a stronger property than holds, and B2 above is the first cost.

- Action: rewrite `ansi_ingest.rs:1` and `docs/ansi-and-beyond.md:133-141`
  to "one policy, two shapes: `project`/`record` for whole-buffer sites,
  `AnsiDrain` for the one streaming site — and here is the predicate they
  share", and actually share the predicate (B2).
- Size **S** (prose) + **S** (the shared predicate). Risk: none.

### 1.2 The five whole-buffer sites hand-copy the sequencing — **MEDIUM**

The same five-line shape is written out five times, with per-site variation
in how the "original" is retained:

```
let p = project(raw);
let text = match p { Some(ref p) => p.text.clone(), None => <fallback> };
… write text …
if let Some(p) = p { record(…, p.spans, <raw>) }
```

`rpc.rs:8407-8431`, `rpc.rs:8708-8727`, `llm_stream.rs:1078-1108`,
`llm_stream.rs:2214-2249`, `lifecycle.rs:812-838`. Two of them
(`llm_stream`) additionally keep a whole `String` clone alive
(`raw_result = ansi.as_ref().map(|_| content.clone())`) purely to have the
original around for `record`.

The load-bearing rule — *text first, spans second, because `edit_text`
clears spans* — is restated as a comment at each site, which is exactly the
signal that the ordering wants to be enforced by a signature rather than by
five comments.

- Action: fold into one `ansi_ingest` helper that takes a closure writing
  the text and does the `record` itself, e.g.
  `project_and_record(blocks, ctx, block_id, raw, |text| …write…)`. The
  fallback differs per site (`result.text_out()` vs the original `String`),
  so the closure gets `&str` and the helper owns the `Option` matching.
- Size **M**. Risk: the two `llm_stream` sites also feed the projected text
  into the *conversation message*, not just the block — the helper must
  return the text, not swallow it.

### 1.3 Two ranged-style shaping currencies in the surface — **MEDIUM**

After `ab185532` the surface shapes ranged styles two ways:

- `VelloFont::layout_spanned` + `collect_msdf_glyphs_styled_deferred`
  (`layout_bridge.rs:100`), brush = `peniko::Brush`
- `VelloFont::layout_styled` + `collect_msdf_glyphs_ansi_deferred`
  (`layout_bridge.rs:116`), brush = `text::ansi::StyledBrush`

`shape_cache.rs:672-700` calls both, branching on whether the block has ANSI
spans. `StyledBrush` is a strict superset of what the `Brush` path carries
(color + style index + MSDF weight), and `content.rs:150-158` says so
outright: *"where they do meet, `shape_chunk` prefers these, because they
are the currency that can express the other's coloring and not the
reverse."*

Which means the `Brush` ranged path is the narrower of two paths that do the
same job, kept because markdown/diff span building predates `StyledBrush`.

- Action: not urgent, but the direction is clear — map `SpanBrush` →
  `StyledBrush` at the boundary and delete `layout_spanned` +
  `collect_msdf_glyphs_styled_deferred` from the *surface* path
  (`diff_view/render.rs:431` is a separate consumer and can keep it, or
  follow later). That also removes the `style_spans` vs `spans` two-list
  split on `FormattedBlock` (`content.rs:146` + `:165`) and the two-part
  `spans_fingerprint` (`content.rs:221-234`).
- Size **L**. Risk: real — this touches markdown coloring on every block.
  Worth doing only when someone is already in `shape_cache`.

### 1.4 Six copies of the float→RGBA8 conversion — **LOW**

`layout_bridge.rs:219-222` says it is *"the one place that decides what
color a brush **is** for this renderer … a second copy of the match would be
a second answer."* True of the `Brush` match; false of the arithmetic. The
identical `(x.clamp(0.0,1.0) * 255.0) as u8` quad appears at
`text/components.rs:46-49` (`color_to_rgba8`), `text/rich.rs:349-352`,
`text/msdf/music_bridge.rs:315-318`, `text/msdf/generator.rs:307-310`,
`text/ansi.rs:116-119` (`palette_rgba8`, added by this work), and
`layout_bridge.rs:228-231`.

All six truncate rather than round, so they agree — which is luck, not
design. A future `+ 0.5` in one of them is a silent one-LSB divergence
between a glyph's color and its underline's `ink`.

- Action: one `pub fn f32x4_to_rgba8([f32;4]) -> [u8;4]` in
  `text::components` next to `color_to_rgba8`; the other five call it.
- Size **S**. Risk: none.

---

## 2. Guards / branches for unreachable states

### 2.1 `kj block reproject` always refuses a capped background block — **MEDIUM**

`kj/block.rs:1089-1099` refuses when `strip(original) != snap.content`. For
a background block that hit `DEFAULT_OUTPUT_CAP`, the contract is
deliberately `content == strip(original) + the cap marker`
(`background_exec.rs:875-884`, `docs/ansi-and-beyond.md:151-158`). So
reproject can *never* succeed on a capped block, and the error it prints —
*"content has diverged from the original (edited since ingest)"* — names the
wrong cause and sends the reader looking for an edit that never happened.

Not a guard for an unreachable state; a guard whose message is wrong for a
state that reliably occurs.

- Action: either tolerate a trailing cap marker in the comparison (strip it
  before comparing and re-append), or detect the suffix and say so:
  *"this block's output was capped; its original covers only the text before
  the cap marker, so reprojection would drop the marker — refusing."*
- Size **S**. Risk: the first option needs the marker string to be a
  constant, which it currently is not (it is an inline literal at
  `background_exec.rs:932-937`).

### 2.2 `--transform` on `kj block reproject` can only ever produce an error — **LOW**

`kj/block.rs:170-172` gives `reproject` a `--transform` flag defaulting to
`ansi-strip`; `kj/block.rs:1043-1050` rejects every other value. The flag is
a knob with one legal position, reflected into published `kj` help.

`kj block original --transform` is different and legitimate — the table is
keyed by transform and `list_block_provenance` can return several.

- Action: either delete the flag on `reproject` (it becomes an argument the
  day a second transform exists — that is one line then), or keep it and say
  so in the `///`: *"Only `ansi-strip` is implemented; other names are
  refused."* Right now the help says "(default: ansi-strip)", which implies
  alternatives.
- Size **S**. Risk: none. This is the "prefer deleting a mechanism to
  generalizing it" call, and CLAUDE.md's stance points at deleting.

### 2.3 `block_original` always runs the second query — **LOW**

`kj/block.rs:948-960` fetches `list_block_provenance` unconditionally, but
uses it only in the `None if !available.is_empty()` arm. Two SQL round trips
on the success path where one would do.

- Action: move the `list` call into the `None` arm.
- Size **S**. Risk: none (the lock is held across both today; splitting them
  is fine — the second read is only for an error message).

### 2.4 `Sink::offsets_exhausted` — **verified NOT debt**, see §6.

---

## 3. Stale names, comments, docs

### 3.1 59 present-tense citations of a renderer deleted in `1fb5ff25` — **HIGH**

Repo-wide grep for the symbols slice 5 deleted
(`build_block_scenes`, `update_block_cell_nodes`, `spawn_block_cells`,
`plan_block_band`, `plan_header_band`, `sync_block_cell_buffers`,
`readback_block_heights`, `reorder_repairs_children_after_order_only_change`,
`ConversationSpacer`, `RoleGroupBorder*`, `FocusedBlockCell`,
`BlockCellContainer`, `view/render.rs`) finds **59 hits across 16 crate
files and 6 docs**, excluding `docs/devlog.md` (which narrates history and
is correct to keep them). Every hit is a comment — verified zero
non-comment references.

Files: `view/surface/{chrome,chunk,content,labels,mod,rich,shape_cache}.rs`,
`view/{components,geometry,render_store}.rs`, `cell/block_border.rs`,
`input/systems.rs`, `text/{msdf/mod.rs,plugin.rs}`,
`ui/{tiling_reconciler.rs,timeline/systems.rs}`, and
`docs/{architecture/app.md,conversation-surface.md,diff.md,issues.md,midi.md}`.

Why it is debt: these comments justify a constant, an inset, a phase order
or a wrap width by *parity with code that no longer exists*. A reader — or a
model — cannot check any of them without git archaeology, and the numbers
they defend are now the only definition of themselves. This is precisely
the "well-meaning preservation of functionality" pattern Amy named: the
functionality was smoothed into the surface, the justification was not.

The three worst, because they cite a **file:line that now points at
something unrelated**:

- `view/surface/shape_cache.rs:599-601` — *"matching `build_block_scenes`'
  plain-text arm (`view/block_render.rs:660-668`)"*. `block_render.rs` is
  948 lines today and 660-668 is `impl Default for
  ExtractedMsdfRenderParams`. Completely unrelated.
- `view/surface/content.rs:194` — *"(same gate as `view/render.rs:152-160`)"*.
  `view/render.rs` does not exist.
- `view/surface/shape_cache.rs:1804` — *"(`view/render.rs:693-745`)"* and
  `:1962` — *"(Verbatim from `render.rs:799-802`.)"*. Same.

Next tier, present tense about a dead thing:

- `view/surface/chrome.rs:119-126` — *"`update_block_cell_nodes` **gives** a
  bordered cell `width: 100%` plus a margin"*.
- `view/surface/labels.rs:60-63` — keeps a named constant *"so the two can be
  compared rather than grepped for"*; there is no second one to compare.
- `view/surface/labels.rs:81-87`, `:228-235` — narrate `build_block_scenes`'
  internal control flow as inspectable.
- `view/surface/chunk.rs:69` — *"rather than a silent zero that would
  disagree with the legacy path"*.
- `view/surface/content.rs:4` — module doc opens by contrasting with
  `sync_block_cell_buffers` + `BlockScene` (`view/render.rs`).
- `text/msdf/surface_renderer.rs:138`, `:733`, `:736`, `:986` — pass order
  and origin justified by *"the order the legacy path composites it in"*.
- `view/surface/mod.rs:220-221` — *"the same phase relationship **Legacy
  has** with its PostUpdate `readback_block_heights`"*. Present tense; the
  architectural point is still true, only the tense and the symbol are wrong.
- `view/surface/shape_cache.rs:1874-1877` and `:3212-3213` — both say
  `spawn_block_cells` *"is flag-gated off here"*, implying a toggle someone
  could flip back. Both the function and `ConversationRenderPath` are
  deleted, not disabled. This is the misleading kind: it describes a
  reversible state that does not exist.
- `view/surface/shape_cache.rs:507`, `:1729` — cite
  `sync_role_group_headers`, deleted with the rest (it lived at
  `block_render.rs:1270` before `1fb5ff25`).
- `view/surface/shape_cache.rs:309`, `:1387-1388` — two more present-tense
  `build_block_scenes` citations without a file:line.

- Action: one mechanical sweep. Rewrite each in past tense and cite the
  commit (`1fb5ff25`) instead of the symbol, or — better for the ones that
  are pure numbers — drop the archaeology and state the rule standalone
  ("a bordered block's content box is inset by `glow_radius * 0.5`"). Delete
  the four dangling `file:line` citations outright; a wrong line number is
  worse than none.
- Size **M** (59 sites, but each is 1-3 lines and mechanical). Risk: none —
  comments only. High value: this is the single largest concentration of
  unverifiable prose in the territory, and it will only get more wrong.

### 3.1a Two comments in the same directory contradict each other about the debounce — **HIGH**

`view/surface/content.rs:10-17`:

> **No streaming debounce here** (slice 3). The legacy path **suppresses**
> re-formatting a large `Running` block that grew by fewer than 200 bytes …
> The legacy debounce **stays where it is, guarding the path that still
> needs it.**

`view/surface/shape_cache.rs:31`, same directory:

> This is what let the 200-char format debounce **die**.

Verified: no 200-byte/char debounce exists anywhere in the tree; the path it
"guards" (`view/render.rs`, cited at `content.rs:4`) does not exist. A
reader chasing the surviving debounce finds nothing.

Note the debounce question is still *live* — `docs/issues.md:7235-7250`
records that a streaming diff re-detects per bump with nothing throttling it
— but the answer "it still exists over there" is false.

- Action: delete `content.rs:14-17`'s last two sentences; say the debounce
  is gone and point at `docs/issues.md` for the open question.
- Size **S**. Risk: none.

### 3.1b `detect_rich_content` is dead, and the module doc names it as the entry point — **MEDIUM**

`text/rich.rs:512` — `pub fn detect_rich_content(text: &str)`,
`#[allow(dead_code)]`, **zero callers** (`grep -rn "detect_rich_content\b"
crates/` returns exactly two hits: its own definition and the doc line
below). Its own body comment admits *"No block status available at this
(unused) call site."*

`text/rich.rs:14` — *"Detection is centralized in `detect_rich_content()`"*.
False: every real caller goes through `detect_rich_content_typed`.

So the module's own front-door documentation points at a dead function.

- Action: delete `detect_rich_content`; fix line 14 to name
  `detect_rich_content_typed`.
- Size **S**. Risk: none (zero callers verified).

### 3.1c `text/rich.rs` describes the sparkline's retired rendering — **MEDIUM**

`text/rich.rs:5-7` and `:70-72` say sparklines are *"plain Bevy UI rectangle
geometry now, not Vello (`text::sparkline::build_sparkline_geometry`)"*.

Two errors: there is no `build_sparkline_geometry` (the function is
`build_sparkline_vertices`, `text/sparkline.rs:171`), and `sparkline.rs`'s
own module doc says the UI-node rendering *"preceded it and is retired"* —
vertices now feed the MSDF geometry lane, which is what
`view/surface/rich.rs:117-136` + `extract.rs`'s `build_geometry_vertices`
actually do. The doc describes the mechanism *before* the one before this one.

- Action: rewrite both to match `sparkline.rs`'s (correct) description.
- Size **S**. Risk: none.

### 3.1d `brush_at_offset`'s comment claims an algorithm the body does not use — **LOW**

`text/rich.rs:165-171` — *"Spans are contiguous and ordered — binary search
on start"*, body is `.iter().find(...)`, a linear scan. One caller
(`layout_bridge.rs:66`), once per shaping run, on short lists, so the
performance claim is harmless — but a reader trusting the comment will
believe an ordering invariant is being exploited when it is not.

- Action: fix the comment (S), or implement the `partition_point` search the
  comment promises (S-M).
- Size **S**.

### 3.1e `readback_block_heights` cited as the live writer of two live fields — **MEDIUM**

`view/components.rs:528`, `:642`, `:846-847` document
`new_blocks_added` / `pending_scroll_anchor` as consumed and set by
`readback_block_heights` (`view/render.rs`, PostUpdate). Both fields are
alive; the writer is now `apply_shaped_measurements`
(`view/surface/shape_cache.rs:1874-1952`, tested at `:3195-3251`).

Worth separating from the bulk sweep in §3.1 because these are the fields
whose orphaned sibling (`FocusTarget.entity`) caused the one real bug slice
5's review caught — a comment naming the wrong writer is how that class of
orphan hides.

- Action: repoint the three citations at `apply_shaped_measurements`.
- Size **S**.

### 3.2 `docs/ansi-and-beyond.md` keeps a design sketch its own reality-check refutes — **HIGH**

`:105-109`: *"**Ingestion atomicity**: clean text, spans, and the provenance
row commit together at the hook site — commit first, publish second."*

`:124-129`, twenty lines later: *"`journal_op` is not a transaction today …
The provenance row is therefore its own statement at the hook site."*

And `d6264828`'s message says it outright: *"Provenance row BEFORE the tag,
**inverting the design doc's sketch**."* The sketch was never deleted, so
the doc now asserts and denies the same thing, and `:105-109` comes first —
which is the half a truncated context keeps.

- Action: delete `:105-109`'s atomicity claim and replace it with the
  ordering rule that actually shipped (row → tag, and why). Keep the
  streaming note.
- Size **S**. Risk: none.

### 3.3 "The CI invariant" does not exist — **HIGH**

Five source comments and two doc lines describe a standing CI check and its
behavior as if it runs:

- `kaijutsu-ansi/src/lib.rs:78-80` — *"The CI invariant … only holds for
  blocks tagged with the current version"*
- `ansi_ingest.rs:103` — *"the CI invariant skips with a warning"*
- `ansi_ingest.rs:114` — *"the CI invariant would report a phantom gap"*
- `ansi_ingest.rs:175` — *"The CI invariant, in miniature"*
- `background_exec.rs:788` — *"the CI invariant `strip(original) ==
  (content, spans)`"*
- `kaijutsu-types/src/block.rs:1458`
- `docs/ansi-and-beyond.md:127`, `:219-221`

There is **no `.github/` directory in the repo at all**, and no script under
`contrib/` that does this. What exists are three in-process unit tests that
assert the property on fixtures: `background_exec.rs:1834`, `:1918-1922`,
`kj/lifecycle.rs:1230-1233`. Those are good tests — but they are not "for
any unedited block with a provenance row, runnable against a real
kernel.db", which is what the doc promises and what `ansi_ingest.rs:103`'s
"skips with a warning" describes the behavior of.

Compounded by B1: as specified, it is not implementable, because "unedited"
is not observable.

- Action: rename every reference to **"the standing invariant"** and point
  at the tests that assert it (`background_exec::ansi_drain_is_split_invariant`,
  `background_ansi_output_is_stripped_with_spans_and_provenance`). Move the
  real-kernel.db sweep to `docs/issues.md` as unbuilt, together with the
  "how do we know a block is unedited" question from B1. Delete the two
  comments that describe the non-existent tool's runtime behavior
  (`ansi_ingest.rs:103`, `:114`).
- Size **S**. Risk: none. High value — these are comments a model reads
  mid-task and will reason from.

### 3.4 `docs/ansi-and-beyond.md:274-277` claims three access paths; two exist — **MEDIUM**

*"**Ad-hoc composition: yes — the transform is also a kaish verb.**
`ansi-strip` usable in a kaish pipeline for experiments (`fastfetch |
ansi-strip --spans`) … One implementation, three access paths (ingest hook,
kaish verb, kj admin verb)."*

Grep of `kaijutsu-kernel/src/runtime/` finds no `ansi-strip` builtin. Two
access paths exist. `:268-272`'s "rc declares which flows get which
transforms, per context type" is likewise unbuilt — the hooks are hardcoded
per site.

- Action: move both to a "not built" subsection of the doc (or to
  `docs/issues.md`) and mark them so. The doc is a living design doc, so
  aspirational content is legitimate — it just has to be *labelled*, because
  right now it reads as description.
- Size **S**. Risk: none.

### 3.5 `docs/issues.md:7270` — "ANSI ingest + styled spans: design settled, **build unstarted**" — **HIGH**

The entry lists three build lanes; all three shipped on 2026-08-19
(`84505811`, `167f41b0`, `9f5b5128`, `a2eb2632`, `ab185532`, `d6264828`).
CLAUDE.md's rule for this file: *"**delete an entry when it ships** (melt the
story into the devlog if it's worth keeping)."*

Left as-is, the next session's first read of the backlog says the feature
does not exist.

- Action: delete. Keep only the genuinely-unbuilt residue as a small new
  entry: the kaish `ansi-strip` verb, rc-declared transform binding (§3.4),
  and the real-kernel.db invariant sweep (§3.3).
- Size **S**. Risk: none.

### 3.6 `docs/issues.md:371` is a heading with no body — **MEDIUM**

Line 371 is `## The conversation surface draws no block border labels or
gutter checkbox (2026-08-18)` and line 372 is the *next* `##` heading. The
body was removed (the gap shipped in `bc02e244`) but the heading was left,
so the file now claims an open issue with no content under it.

- Action: delete line 371.
- Size **S**. Risk: none.

### 3.7 `docs/issues.md:7241` describes the legacy path in the present tense — **LOW**

*"…because `view/surface/content.rs` deliberately dropped the 200-char one
**the legacy path still has** (`view/render.rs:98-108`)"*, and further down
*"the one place the legacy rule was actually earning its keep"*. The legacy
path was deleted the next day.

- Action: rewrite the entry's premise (the debounce question is still live —
  a streaming diff still re-detects per bump — but the "legacy still has it"
  framing and the file:line are dead).
- Size **S**.

### 3.8 A misplaced test steals another test's doc comment — **MEDIUM**

`crates/kaijutsu-kernel/src/kernel_db.rs:11432-11440`: the section banner
*"── 30. insert_document constraint classification, by read-back ──"* and
the doc comment *"/// 1. Re-inserting a `DocumentRow` with an already-used
`document_id` must classify as `DuplicateDocument`…"* are immediately
followed by `fn block_provenance_roundtrip_and_cascade()`. The provenance
test (`167f41b0`) was inserted between a section header and the test that
header describes, so the doc comment now documents the wrong function and
`insert_document_duplicate_id_is_typed_duplicate_document` (`:11488`) has
none.

- Action: move `block_provenance_roundtrip_and_cascade` above the section
  banner (or into its own banner) and reattach the `/// 1.` comment.
- Size **S**. Risk: none.

### 3.9 The output-cap marker leaks a Rust identifier to the model — **MEDIUM**

`background_exec.rs:932-937` appends this text into a block the model reads:

> `[kaijutsu: background output capped at DEFAULT_OUTPUT_CAP bytes — further output is discarded; the process is still running and its exit status will still be recorded]`

Two rules from CLAUDE.md "Writing style" broken at once: *"Text that faces
users, agents, and models must not leak internals"* (`DEFAULT_OUTPUT_CAP` is
unresolvable to its reader) and *"Provide specific values"* (the number is
`256 * 1024` = 262144, `background_exec.rs:132`).

- Action: `format!("… capped at {DEFAULT_OUTPUT_CAP} bytes …")`. One-line.
  While in there, hoist the marker to a `const` — §2.1 wants it.
- Size **S**. Risk: none, unless something greps for the literal (nothing
  does; `:1147` asserts `content.contains("capped")`).

---

## 4. Threaded-but-unused / vestigial fields

### 4.1 `StyleEntry.effect` / `.param` / `._pad` — 12 of 32 bytes reserved and unread — **LOW (judgment call)**

`text/msdf/surface_renderer.rs:186-192` and its WGSL mirror
`assets/shaders/msdf_surface.wgsl:56-62`. Nothing writes a non-zero
`effect`; `build_style_table` (`surface_renderer.rs:205-224`) hardcodes
`effect: 0, param: 0.0` for all 257 entries; the shader never reads either
field (`msdf_surface.wgsl:142-147` reads `flags` and `fg` only).

Documented as reserved for the rainbow/blink/CRT revival
(`docs/ansi-and-beyond.md:236-239`, `:294-311`), and 3 KiB of GPU memory is
nothing. But CLAUDE.md is explicit: *"prefer deleting a mechanism to
generalizing it"*, and this is speculative generality shipped ahead of its
consumer — the exact shape `7b17adfe` already deleted once
(`WindowKey.svg_generation`, *"mechanism without payoff"*).

- Action: **leave, with an expiry.** Add a line to `docs/issues.md`: if the
  rainbow/blink revival has not landed by <date>, delete `effect`/`param`
  and shrink `StyleEntry` to 16 bytes — re-adding them is a two-line change
  to a struct that is already under a mirror contract with a pinning test.
- Size **S** to delete later. Risk: none either way.

### 4.2 `ChromeInstance.anim[1]` (phase) is always `0.0` — **LOW**

`view/surface/chrome.rs:409-413`; both constructors (`:526`, `:571`)
hardcode `0.0`. The shader does read it
(`assets/shaders/surface_chrome.wgsl:147`, `:303-304`), so it is live on the
GPU and dead on the CPU — a per-instance animation-phase offset with no
producer. Same class as 4.1 and honestly documented as *"legacy's behavior
(one global phase)"*.

- Action: same treatment as 4.1 — leave with an expiry note, or drop the
  field and shrink the 72-byte instance.
- Size **S**.

### 4.3 A cluster of `#[allow(dead_code)]` "symmetry" accessors — **LOW**

13 in the surface module. Most are labelled *"Model accessor, exercised by
tests"*, which is house style and fine. Two are not exercised by anything
and say so:

- `view/surface/content.rs:266` — `is_empty()`, *"Symmetry with `len`; **the
  extractor will want it**."* Speculative.
- `view/surface/extract.rs:237` — `capacity()`, *"Buffer-growth accessor for
  tests and diagnostics"* — grep finds no caller, tests included.

- Action: delete both; `is_empty()` is one line to re-add.
- Size **S**. Risk: none.

Verified not debt in this cluster: `chrome.rs:362` `CHROME_KIND_NONE`
(numeric parity with the still-live `block_fx.wgsl` `BK_*` block),
`surface_renderer.rs:280` `snap_baseline_phys` (exists so the shader formula
is testable — the mirror contract at `msdf_surface.wgsl:16-18` depends on
it), `shape_cache.rs:514` `height` (documented as the input the centring
arithmetic was derived from).

### 4.3a `ShapeKey.collapsed` cannot currently differentiate anything — **LOW (leave)**

`view/surface/shape_cache.rs:221-229`. Its own doc says it: the value is
captured at `GeomRow` seed time and never refreshed for a row reconcile
already knows (verified — `view/geometry.rs:320` clones `old_rows[i]`
wholesale), *"so a collapse toggle does not move this field."* The toggle is
caught by `content_version` instead, because collapsing rewrites the
formatted text.

So in every reachable path today the field is a constant per block and never
decides a re-shape. `every_shape_key_field_forces_a_reshape`
(`:2046-2061`) diffs hand-built `ShapeKey`s and cannot detect this.

- Action: **leave.** The doc already names the exact future change (fixing
  the geometry seed) that would make it live, and carries the warning *"Do
  not add a re-shape rule that relies on this field alone."* Deleting it is a
  design conversation, not a cleanup. Recorded here so the next auditor does
  not have to re-derive it.
- Size **S** if ever removed. Risk: removing it silently deletes the
  insurance the warning is about.

### 4.3b `Svg.source` / `Abc.source` — per-block source kept for a diagnostics path that does not exist

`text/rich.rs:85` and `:93`, both `#[allow(dead_code)]`, both documented as
*"kept for error-path diagnostics"* / *"retained for re-parse"*. Verified no
reader: `view/surface/rich.rs`'s `shape()` destructures
`RichContentKind::Abc { tune, .. }` and `Svg { width, height, .. }`,
discarding `source` in both arms.

- Action: `Arc<String>`, so the cost is a pointer clone — but this is a
  planned-feature placeholder with no entry in `docs/issues.md`. Either file
  the diagnostics follow-up or delete the fields.
- Size **S**.

### 4.3c `SvgRasterCache::len()` has zero callers including its own tests

`view/surface/rich.rs:389-392` — `#[allow(dead_code)]` labelled *"Model
accessor for tests and diagnostics"*, but the test module uses only
`is_empty()` (`:464`, `:764`).

- Action: delete.
- Size **S**.

### 4.4 Wire fields — **verified NOT debt**

`kaijutsu.capnp:112-124` (`StyleSpan`), `:218-220` (`BlockSnapshot @42-44`),
`:444-448` (`BlockSpansChange`). Every field is encoded
(`kaijutsu-server/src/rpc.rs:8847-8861`, `context_feed.rs:405-420`) and
decoded (`kaijutsu-client/src/rpc.rs:3593-3616`, `context_feed.rs:348-357`),
with round-trip tests including the missing-field direction. The
`(kind, value)` color encoding is symmetric and both sides warn rather than
fabricate on an unknown kind. No `retiredNN` stubs, no holes in the ordinals.

---

## 5. Test quality

### 5.1 `AnsiDrain::finish`'s span-truncation branch is never exercised — **MEDIUM**

`background_exec.rs:846-861`: on completion the drain truncates `raw` to
`raw_committed` and filters/clamps spans to `committed`. That arithmetic
only runs when a chunk was fed but *not* committed — i.e. when the output
cap tripped or an append failed.

Every existing test commits after every chunk:
`ansi_drain_is_split_invariant` (`:1815-1836`) calls `drain.commit()` in the
loop; the one cap test (`:1128`) runs `yes x | head -c …`, which contains no
escape bytes and therefore takes the `saw_escape == false` early return.

So the branch that guarantees *"for a capped block, `content ==
strip(original) + marker`"* — the load-bearing claim in both
`docs/ansi-and-beyond.md:151-158` and the module doc — has zero coverage.

- Action: a pure unit test on `AnsiDrain` (no child process needed): feed
  two styled chunks, commit only the first, `finish()`, assert
  `strip(original) == (committed_text, spans)` and that no span extends past
  the committed length.
- Size **S**. Risk: none. This is the highest-value missing test in the
  territory — it is exactly the kind of test that *can and will* fail.

### 5.2 `strip_totality` fuzz target asserts a weaker invariant than the crate claims — **MEDIUM**

`crates/kaijutsu-ansi/fuzz/fuzz_targets/strip_totality.rs:21` asserts
`span.start <= span.end`. Everything else in the crate asserts the strict
form: `properties.rs:183` (*"empty or inverted span"*), `properties.rs:286`,
and `Sink::push_char` (`lib.rs:186-219`) can only ever emit `start < end`
because it appends a char first.

An empty span is exactly the bug the fuzzer would be well placed to find,
and it is the one it is told to allow.

Its module doc (`:4-6`) also carries un-edited drafting prose into a
published file: *"disjoint (or at least non-overlapping in a way that
respects `end <= next.start`... actually we require strictly
adjacent-or-later, see below)"*.

- Action: tighten to `span.start < span.end`; rewrite the doc paragraph as
  the three invariants it means.
- Size **S**. Risk: if it starts failing, that is the point.

### 5.3 The corpus-coverage test is duplicated verbatim — **LOW**

`tests/goldens.rs:47-56` (`every_corpus_file_has_a_test`) and
`tests/differential.rs:80-89` (`every_corpus_file_is_covered`) have
identical bodies. `tests/support/mod.rs` already exists and is compiled into
both binaries — this is exactly what it is for.

- Action: move the body to `support::assert_corpus_matches_wired_list()` and
  have both call it.
- Size **S**. Risk: none.

### 5.4 The differential test covers 3 of 5 fixtures, and that is invisible — **LOW**

Measured the corpus directly: `git_log_kaijutsu.raw` (em dash) and
`sgr_torture.raw` (CJK + emoji, 13 KiB — by far the richest fixture) are
non-ASCII and skipped by `skip_reason` (`differential.rs:61-72`). The two
that matter most for SGR breadth are the two that never reach vt100. The
test reports this only via `eprintln!` (`:99`), which `cargo test` swallows
unless it fails.

The scoping is *correct and well-argued* (`differential.rs:6-47`) — the
gap is that nobody reading a green test run learns that the torture corpus
is unverified against a second implementation.

- Action: add an ASCII-only SGR-torture fixture so the widest SGR coverage
  is actually differentially checked, or assert a floor
  (`assert!(in_scope >= 3)`) so a future fixture silently falling out of
  scope is loud.
- Size **S** (the assert) / **M** (a new fixture + snapshot).

### 5.5 `style_table_matches_the_contract` asserts a tautology — **LOW**

`surface_renderer.rs:1307`: `assert!(table.iter().all(|e| e.effect == 0),
"effects are reserved")` cannot fail while `build_style_table` hardcodes
`effect: 0` eight lines away. It is arguably a deliberate tripwire for the
day effects land — but it reads as a property check.

- Action: keep it and say what it is: *"tripwire: when effects land, this
  fails and tells you to extend the contract test."* Or drop the line.
- Size **S**.

### 5.6 `styled_spans_fingerprint` will silently miss a new field — **MEDIUM**

`text/ansi.rs:285-301` hashes `StyledSpan` field by field. Its own doc says
the failure mode: *"An attribute that changed but is not hashed is a
silently stale block."* Nothing makes adding a field to `StyledSpan` (or to
`StyledBrush`) a compile error here.

Same shape at `view/surface/content.rs:221-234` for `SpanBrush`.

- Action: destructure exhaustively at the top of each —
  `let StyledSpan { start, end, brush, bg, ink, underline, strikethrough } =
  span;` — so a new field breaks the build at the one place that must know
  about it. Zero runtime cost.
- Size **S**. Risk: none. This is the cheapest "test that can and will fail"
  in the whole territory.

### 5.1a `shape_visible_blocks` — the surface's central system — has no test — **MEDIUM/HIGH**

`view/surface/shape_cache.rs:1293-1624`, ~330 lines. Every pure helper it
calls is well tested in isolation (`incremental_prefix`, `recolor_block`,
`plan_evictions`, `apply_measurements`, `shape_block`). The system that
wires them together is referenced only by its registration
(`view/surface/mod.rs:260`) and by doc comments — **no test calls it**
(verified by grep).

Untested inside it: the five-outcome dispatch (key match / recolor /
incremental tail / sync reshape / backlog), the conditional
`baked_theme_epoch` assignment at `:1424-1428` (`rich_drawn || ansi_spanned`
— the ANSI half is new in `ab185532`), and the `mutated`/generation-bump
bookkeeping at `:1438-1486`.

That bookkeeping is exactly where two bugs already lived: the comments at
`:1438` and `:1874` both say *"(found by review, 2026-08-18)"* — one was a
cache mutation that never bumped `generation` and so never reached the GPU;
the other was `new_blocks_added` losing its writer with `spawn_block_cells`.
Both were caught by a human reading whole files, because there was no test
that could catch them. Nothing guards against the same class today.

The scaffolding exists: `measure_app` / `install_geometry` (`:3061-3081`)
already build a minimal Bevy `App` and run `apply_shaped_measurements`.

- Action: two tests through that harness would cover the new ANSI-specific
  risk: *"a theme swap re-shapes an ANSI-spanned block but not a plain-text
  one"* (the `baked_theme_epoch` conditional) and *"a recolor bumps
  `ShapedBlockCache::generation`"*.
- Size **M** (needs a real font + stub atlas/`FontDataMap`). Risk: this is
  the highest-complexity least-tested code in the territory.

### 5.1b The end-to-end ANSI wire test never checks the color — **LOW**

`kaijutsu-server/tests/rpc_integration.rs:1526-1600` asserts the span exists
and covers exactly `"OK"`, that the content is escape-free, and that the tag
rode the wire. It never asserts `span.fg == Some(StyleColor::Indexed(2))`,
so a wire encoding that dropped or mangled the color — the one thing the
`(kind, value)` pair exists to carry — passes.

- Action: one added assert.
- Size **S**.

### 5.7 Strong tests worth naming (do not weaken these)

- `properties.rs:71-150` — split at every position, every *pair* of
  positions, byte-at-a-time, and seeded-random. This is the real one.
- `vte_partial_utf8_regression.rs` — pins a *known upstream bug* with an
  `assert_ne!` plus an instruction for what to do when it starts failing.
  Exemplary; matches the "pin a divergence WITH instructions" practice.
- `background_exec.rs:1871-1922` — a real child process emitting 8190 spaces
  so the SGR sequence straddles the 8 KiB `read()` boundary, then asserting
  `strip(original) == (content, spans)` end to end.
- `text/msdf/layout_bridge.rs:486-514` — shapes with the real shipped font
  to prove parley splits runs on `StyledBrush` inequality.

---

## 6. Verified NOT debt

Recorded so the next auditor does not re-litigate these.

- **`Sink::offsets_exhausted`** (`kaijutsu-ansi/src/lib.rs:173-197`). Looks
  like a guard for an impossible state (a 4 GiB block). It is the price of
  the crate's *totality* invariant — `u32::try_from` must not panic, and
  panicking is the one thing `strip` promises never to do
  (`lib.rs:38-42`, `fuzz_targets/strip_totality.rs`). Correct to keep; the
  comment already explains it.

- **`slice_spans` / `slice_styled_spans` duplication**
  (`view/surface/chunk.rs:146-190`). Two five-line loops with the same
  shape. The doc argues the case explicitly: *"A separate function rather
  than a generic one because the two span types share no trait and inventing
  one to hold two five-line loops together would be more machinery than the
  duplication costs."* Agreed — and §1.3 would delete one of them anyway.

- **`CHROME_KIND_NONE`** (`view/surface/chrome.rs:358-364`). Dead constant,
  kept for numeric parity with `block_fx.wgsl`'s `BK_*` block so the two
  shaders read side by side. `block_fx.wgsl` is still live (compose overlay,
  vi editor, diff viewer, dock, role divider all render through
  `BlockFxMaterial`). Deliberate and labelled.

- **`snap_baseline_phys`** (`text/msdf/surface_renderer.rs:280-284`). No
  Rust caller by design — it exists so the shader's baseline-snap formula
  (`msdf_surface.wgsl:119-124`) is testable. Named in the file's mirror
  contract.

- **`SurfaceUniforms.time`** (`surface_renderer.rs:127-129`). Unread by
  `msdf_surface.wgsl`, which makes it look dead — but the uniform block is
  *shared* with `surface_chrome.wgsl`, which does read it (breathe/pulse/
  chase). The Rust doc says so; the WGSL mirror does not, which is the only
  improvement available (one comment line).

- **`view/block_render.rs`**. Survived slice 5 and is not legacy residue —
  the surface imports from it directly (`surface/extract.rs`:
  `ExtractedMsdfRenderParams`; `surface/target.rs`: `GpuTextureLimits`,
  `round_to_physical_px`, `MsdfSurface`; `surface/rich.rs`:
  `create_svg_raster_image`, `IMAGE_PLACEHOLDER_COLOR`). Shared MSDF/GPU
  infrastructure.

- **`labels::{top,bottom}_label_placement` called from two files**. Looks
  like duplicated placement math; verified it is not — `chrome.rs:474-492`
  and `window.rs:200-231` both call the `labels` functions, and no second
  copy of the inset/gap arithmetic exists. The "one arithmetic, two
  consumers" claim in `labels.rs:19-24` is accurate.

- **Spans excluded from `BlockSnapshot::content_eq`**
  (`kaijutsu-types/src/block.rs:2419-2424`). Deliberate: a spans-only change
  is signalled by `BlockFlow::SpansChanged`, not by content identity.
  Correct — and the reason `SpansChanged` exists as its own flow.

- **Hydration blindness.** Spans never reach model text
  (`llm/hydrate.rs`, pinned by test per `84505811`). Verified the tag and
  spans are snapshot-only and never enter the message stream.

- **The `block_provenance` PRIMARY KEY** (`kernel_db.rs:631-643`). Keyed on
  the three `BlockId` components + transform, explicitly *not* on oplog seq,
  with the reason in the schema comment (compaction truncates the oplog and
  would orphan seq-keyed rows). Cascades with the document. Correct.

- **`insert_block_provenance`'s `OR REPLACE`** (`kernel_db.rs:2397-2422`).
  Justified: re-running a hook on the same block must not fail, and both
  writes carry the same bytes by construction.

- **`ConversationSurfaceTarget` and its "invisible to the per-block MSDF
  pass" guard** (`view/surface/target.rs:72-84`, `:210-234`). Reads like
  legacy-compat scaffolding. It is not: `main.rs:234-242` still registers
  `view::block_render::BlockRenderPlugin` for the editor, diff viewer and
  dock chrome, and `MsdfSurface` / `extract_msdf_blocks` /
  `resize_block_textures` are all live. Two MSDF pipelines genuinely coexist
  and the guard prevents a real double-extraction.

- **`is_streaming` threaded through `text/rich.rs`'s detection.** Its doc
  says *"purely diagnostic … changes no control flow"*, which reads
  vestigial. It is load-bearing one level up: `rich_input_fingerprint`
  hashes `block.status`'s discriminant so a Running→Done transition
  invalidates the cache and lets the at-rest SVG parse-failure diagnostic
  fire exactly once (tested, `text/rich.rs:790-801`).

- **The four "near-duplicate" invalidation counters** (`SurfaceMetricsEpoch`,
  `SurfaceThemeEpoch`, `ShapeKey::baked_theme_epoch`,
  `ShapedBlockCache::generation`, mirrored in `WindowKey`,
  `view/surface/mod.rs:93-116`). Each is separately justified in its own doc,
  and two of the justifications name a real missed-repaint bug found without
  it. `WindowKey.svg_generation` was already deleted as a genuine duplicate
  by `7b17adfe`; these survived the same scrutiny.

- **`SurfaceGpuBuffers`'s four parallel maps** (`view/surface/extract.rs:184-334`).
  Look copy-pasted; the upload/grow mechanism is de-duplicated through the
  generic `upload_into<T, K>` (`:120-162`) and only the versioning keys are
  separate, because chrome and glyphs churn on different triggers (`:79-88`).

- **`view/surface/mod.rs`'s own prose.** Unlike the rest of the module, its
  legacy references are correctly past tense (*"was deleted in slice 5"*)
  and its `# Dead code` section (`:57-67`) explains why the blanket
  `#![allow(dead_code)]` was replaced by per-item allows. This is the model
  the §3.1 sweep should follow.

---

## Suggested order

1. **B2** (`background_exec` re-derives the predicate) — S, strictly
   correct, and it is the drift the module doc says cannot happen.
2. **§5.1** (`AnsiDrain::finish` cap-path test) — S, the missing test that
   would catch the next bug in exactly that arithmetic.
3. **§3.3 + §3.2 + §3.5 + §3.6 + §3.1a + §3.1b** — the CI-invariant fiction,
   the refuted atomicity sketch, the "build unstarted" entry for shipped
   work, the empty issue heading, the debounce contradiction, and the dead
   function the module doc calls the entry point. All S, all published text
   a model reads first, and all of them currently *mislead*.
4. **§5.6** (exhaustive destructuring in the two fingerprints) — S, the
   cheapest test-that-can-fail in the territory.
5. **§3.1** (the 59 legacy citations, incl. the four dangling `file:line`) —
   M, mechanical, highest volume. Follow `view/surface/mod.rs`'s tense.
6. **§5.1a** (a test for `shape_visible_blocks`) — M, the biggest coverage
   hole; two ANSI-specific cases would pay for the harness.
7. **B1** (the provenance tag after an edit) — M, needs a ruling from Amy:
   does the tag mean "content is a projection" or "an original exists"?
8. **§1.2** (fold the five whole-buffer hook sites) — M.
9. **§1.3** (one ranged-style shaping currency) — L, only when someone is
   already inside `shape_cache`.
10. Everything else is S and opportunistic.

## Note for the next auditor

Three things in this territory are *deliberate* and keep looking like debt:
the `slice_spans`/`slice_styled_spans` duplication, `ShapeKey.collapsed`,
and the reserved `StyleEntry.effect`/`param` slots. Each carries its own
argument in its own doc comment. §6 has the evidence; do not re-litigate
them without new information.

The single structural observation worth carrying forward: this code's
comments are unusually good at explaining *why*, and that is exactly what
makes §3.1 expensive — 59 careful justifications now point at code that no
longer exists, and the care is what makes them believable.
