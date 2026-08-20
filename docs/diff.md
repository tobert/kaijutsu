# Diff Blocks & Viewer

Diffs where the work happens — in the conversation. A model emits a diff block
as it edits; Amy reads a small preview inline and expands it into a vi-motion
full view with yank. Not git-dependent: the first-class source is kaijutsu's own
file documents; git arrives later as *another source* feeding the same block
type.

## Status

**Designed 2026-08-01 (research + survey session). Slices 0–4 SHIPPED
2026-08-01** — the `kaijutsu-diff` crate, `ContentType::Diff`, the whole
kernel surface (`kj diff`, `diff_block`, hydration-as-projection, `Done`
validation), and the inline app preview. **Slice 5 SHIPPED 2026-08-02** —
`DiffCore`, `Screen::Diff`, the keyboard grab, and yank→clipboard. **Slice 6
phase A SHIPPED 2026-08-04** — word-level highlight, fold rendering, and the
return of column motions. **Phase B SHIPPED 2026-08-04** — the multi-rect
`BlockFxMaterial` extension, character-wise `v` with a plain-text yank
semantics, the minimap, and word-level background washes. The viewer is
feature-complete against this plan. This doc is the plan of record; the
per-slice "Build notes" sections below correct it where the build disagreed.

## Decisions (Amy, 2026-08-01)

1. **Block shape: `ContentType::Diff` (`text/x-diff`) on a `BlockKind::Text`
   block** — the SVG/ABC rail exactly. `contentType` is a free-form MIME string
   on the wire (`BlockSnapshot.contentType`), so **no `kaijutsu.capnp` change and
   nothing in the block store or client**. Content is standard **unified diff text**:
   models read it natively in hydration, old clients degrade to plain text.
   ⚠ `ContentType`'s discriminant order is an LWW merge rule ("richer types win
   ties", `kaijutsu-types/src/block.rs:301`) — place `Diff` deliberately, don't
   just append.
2. **Engine: `imara-diff` 0.2** (Apache-2.0; the engine inside gitoxide and
   Helix). Histogram algorithm by default, Myers fallback, git's
   indent-heuristic slider correction as postprocessing — all off the shelf. It
   diffs **arbitrary token streams**, so word-level refinement is the same
   engine with a word tokenizer. We write no diff algorithm ourselves.
3. **First source: agent edits, zero git.** Every file an agent touches is
   already a kernel document with a full oplog
   (`FileDocumentCache`, `kaijutsu-kernel/src/file_tools/cache.rs`) — before
   and after are both in hand at `apply_edit_plan`
   (`mcp/servers/file.rs`). **On-demand emission first**: a `diff_block` MCP
   tool and a `kj diff` command. Auto-emit-per-edit and live-updating diffs are
   future work (below). Git-backed sources (worktree vs main — Amy's daily
   itch) come **after kaish-extras git lands**, as new sources for the same
   pipeline.
4. **Full view: `Screen::Diff` + an app-local, read-only modalkit motion
   machine (`DiffCore`).** Not a kernel editor session. `docs/vi.md` Decision 6
   ("mode lives kernel-side, period") protects *edit/mode sync with a kernel
   block*; `DiffCore` has no edit state — it emits motions, folds, and yanks,
   never an `EditOp` — so there is nothing to race or corrupt. Real vi grammar
   (counts, `gg`/`G`, `]c`/`[c` hunk motions, visual-line select) via modalkit,
   and **yank lands in the system clipboard** — which is inherently app-side
   anyway, and the concrete reason Amy wants vi here (grab a hunk to paste into
   feedback). Specialization over code-sharing with `EditorCore`: a little
   duplication is fine (Amy).
5. **Preview in conversation**: a stat header (`3 files, +42 −17`) plus the
   first N lines, collapsed by default; expand under Navigation into
   `Screen::Diff`. Preview rows must respect the conversation's virtualization
   contiguity invariant — expansion is a screen change, never an out-of-band
   conversation row.
6. **Minimap: in scope.** `viz::scales` (invertible — click-to-jump comes free
   from `invert`) + a stack of colored rects keyed by hunk type
   (sparkline-style rect children or `MsdfBlockGeometry` quads). The
   issues.md "minimap / semantic-zoom" fence was written about *conversation*
   scroll; a diff is a bounded document — deliberately decided in-scope, fence
   acknowledged.
7. **Richness rank (2026-08-01): `Plain < Markdown < Svg < Abc < Diff < Image`** —
   sol's placement. A deliberate diff typing is highly structured and beats
   almost any competing claim in a tie; and someday a diff may *contain* image
   diffing, so it ranks just under `Image`. Pinned via the explicit
   `richness()` extraction (slice 2), tested at equal timestamps.
8. **Color is semantic, not baked.** Diff block content carries no escape
   codes; the renderer colors it from the theme via `SpanBrush` spans (the
   mechanism markdown already uses). A `ContentType::Ansi` (SGR → span parser)
   is a worthwhile *adjacent* feature for terminal output generally, but the
   diff path stays semantic: theme-aware, re-diffable at word level, foldable,
   minimap-derivable, and hydration-clean for models.

## Architecture

### `kaijutsu-diff` crate (pure — no Bevy, no kernel, no RPC)

- **Engine wrapper** over imara-diff: line tokenizer + word tokenizer,
  **recursive refinement** (line diff, then re-diff each changed line-pair
  region at word granularity — jj's `color-words` design) with dmp-style
  semantic cleanup so intra-line highlights stay word-shaped.
- **Tokenizers are per-content-type** (imara-diff diffs any token stream, so
  this is configuration, not new algorithm): code/plain → line + word
  refinement; **markdown/prose → paragraph-aware with word refinement** (prose
  rewraps; line identity is weak); **ABC → bar-aligned** (`|` is a natural
  token boundary, so hunks land on measures, not arbitrary text lines).
  **v1 ships line+word only** — paragraph and ABC bar-aligned are deferred
  behind the same profile seam (batch-review consensus: ABC edge cases —
  repeats, overlays, multi-voice — deserve a corpus, not a guess). The crate
  stays decoupled from `kaijutsu-types`: it exposes its own `DiffProfile`
  enum; kernel/app map `ContentType` → profile at the edge, so MIME growth
  never touches the algorithm crate.
- **`DiffModel`**: files → hunks → lines, with intra-line word spans and fold
  state. One model, multiple projections (unified now; side-by-side /
  interleaved later — the research consensus is that these are switchable views
  of one hunk model).
- **Unified-diff format + parse**: the block content is unified text; the app
  parses it back to a `DiffModel` and recomputes word spans locally. Property
  test: format∘parse roundtrips.
- **`DiffCore`**: modalkit `VimMachine` driving a cursor over a `DiffModel` —
  motions, counts, `]c`/`[c`, `zo`/`zc`/`za` folds, visual-line, `y` (drains as
  a yank intent carrying the selected text), `q`/`ZQ`/`:q` (drains as a close
  intent). Headless-testable exactly like `EditorCore`: `apply_keys` in, cursor
  + intents out.

### Kernel

- **`kj diff <a> [<b>]`** — sources: two VFS paths; a file's kernel document vs
  its on-disk state; two frontier versions of one document (the oplog makes any
  historical pair derivable). Output is a typed block via
  `KjResult::ok_typed(text, ContentType::Diff)`. Subsumes the
  `kj block diff --original` recovery wish (issues.md, `builtin.file`
  hardening).
- **`diff_block` MCP tool** on `builtin.block` (~15 lines beside
  `svg_block`/`abc_block`): content + optional pre-validation (parse the
  unified text strictly; `ExecResult::failure` on garbage, the ABC pattern).
- **Hydration**: add the `Diff` arm in `llm/hydrate.rs` so models read diffs
  back in later turns.

### App

- `RichContentKind::Diff` + a `ContentType::Diff` arm in
  `detect_rich_content_typed`, plus a ` ```diff ` fence sniff in the `Plain`
  fall-through (both mechanisms exist; SVG has both).
- **Preview renderer** in `block_render.rs`, modeled on the Markdown arm:
  Parley layout + `SpanBrush` coloring for `+`/`−`/`@@`/headers;
  `MsdfBlockGeometry` flat quads (pure math, currently ABC-only by convention,
  not by structure) for per-line background bands. Theme colors in
  `ui/theme.rs`.
- **`Screen::Diff`** full surface (the `Screen::Editor` pattern: chrome-hide,
  focus park, own MSDF surface). Input: a new `KeyboardGrab` variant feeding
  `DiffCore` (the `ComposeVim` precedent — an app-local modalkit machine behind
  the grab), so Global bindings and the Ctrl+A prefix still win. Esc follows
  doctrine: the vi surface owns it (visual → normal); leaving the screen is
  `q`/`ZQ` → close intent → pop to Conversation.
- **Selection/hunk highlight rendering**: line bands ride geometry quads
  (available now). Visual-mode selection highlighting wants the **multi-rect
  `BlockFxMaterial` extension** — the same prerequisite `docs/vi.md` lists for
  editor selections. Build it as its own slice; both surfaces benefit.

## Slices (TDD, in order)

0. **Contracts + golden fixtures** (batch-review amendment): pin the accepted
   unified-diff dialect (multi-file, add/delete + `/dev/null`, no-newline
   markers, quoted paths; binary and unknown extensions rejected *loudly*);
   three roundtrip properties (`parse∘format == id` on models,
   `format∘parse == id` on canonical text, external text canonicalizes);
   CRLF normalized to `\n` at emission; size ceilings for
   generation/render/hydration; the freeze-on-open viewer snapshot contract;
   the LWW richness rank for `Diff` (decided — Decision 7). Golden fixtures shared by
   kernel and app tests so the two sides cannot invent divergent dialects.
1. `kaijutsu-diff`: engine wrapper + `DiffModel` + unified format/parse +
   refinement + `DiffProfile` + diffstat + **hunk-aware projection/truncation
   helpers** (a truncated diff must never look like a complete patch).
   Malformed input is a typed error, never a silent empty. Pure
   unit/property/fixture tests.
2. Types: `ContentType::Diff`, mime mapping, hydrate arm — and **first**
   extract an explicit `ContentType::richness()` for the LWW tiebreak
   (deepseek seam review 2026-08-01: discriminant-order-as-merge-rule is
   fragile; ~20 lines, decouples merge semantics from declaration order
   before the new variant lands).
3. **SHIPPED.** Kernel: `kj diff` + `diff_block`, wire e2e asserting a typed block lands.
   Source resolution is **ownership-aware from day one** — the
   `resolve_editor_target` pattern (config-owned docs answer through the
   mount table; never raw `get_or_load` for a config path, which would mint a
   shadow document — the exact bug class `docs/config-ownership.md` killed). Internally a typed source descriptor even while the CLI stays
   simple. **Hydration is a projection, not a passthrough**: the canonical
   block keeps the full diff; the model-facing envelope gets diffstat +
   whole-hunk-bounded content with an explicit complete/truncated marker
   (the existing char-count truncation can cut mid-hunk and leave a
   plausible-looking partial patch). Test hydration end-to-end for **both**
   producers — `kj diff` output and `diff_block` may enter different
   role/kind hydration branches.
4. **SHIPPED.** App preview: rich-content arm + render arm + fence sniff + theme.
5. **SHIPPED.** `Screen::Diff` + `DiffCore` + grab/bindings + yank→clipboard.
   **Freeze-on-open**: the viewer binds to `(block_id, content
   version/hash)`; if the block changes underneath, show a stale banner with
   explicit refresh — never silently rebind, and yank always reads the frozen
   model, not current block content. **Visual mode is line-wise (`V`) only
   until the multi-rect material lands** — line-band quads render it today;
   character-wise `v` without rect support would be an invisible selection.
   Yank semantics: exact canonical unified text of the selected lines
   (prefixes included); a "yank as patch" variant must include file+hunk
   headers or not exist. Clipboard access must not block the Bevy main
   thread. The one
   honest share with `EditorCore`/compose: extract the Bevy→modalkit
   key-notation mapping (pure data, no modalkit-version coupling) — and home
   it where a non-Bevy client could reach it (NOT buried in `kaijutsu-app`;
   a future TUI client would pair `modalkit-ratatui`'s crossterm mapping with
   the same notation data — decide the exact crate when this slice lands);
   do NOT share the intent-drain types — Editor intents carry write semantics,
   Diff intents don't, and a shared trait would couple their evolution.
6. **Phase A SHIPPED.** Word-level highlight, fold rendering, column motions.
   **Phase B SHIPPED.** Multi-rect material, character-wise visual mode,
   minimap, word-level background washes.

## Build notes — slices 0+1 SHIPPED 2026-08-01 (corrections to the plan above)

Slices 0+1 are built (opus agent, worktree; slice 2 landed on main the same
day: `richness()` extraction + `ContentType::Diff` + hydrate arm). What the
build taught us, for the slices still to come:

- **imara-diff 0.2 (crates.io) has no word tokenizer** — the survey's
  `sources::words` lives in gitoxide's fork (`gix-imara-diff`), not upstream.
  Ours is ~30 lines in `tokenize.rs`; the profile seam holds as designed.
  The Myers fallback is *internal* to imara-diff's histogram (pathological
  inputs drop to Myers automatically); the indent heuristic is exposed and on
  by default.
- **Dialect decisions pinned in the crate** (fixtures are the authority):
  renames *represented, not detected* (detection waits for gix-diff); copies
  rejected as `UnsupportedExtension`; `---`/`+++` authoritative over the
  advisory `diff --git` line; `index`/mode/similarity headers accepted then
  dropped (lossy canonicalization, documented); CRLF normalized on ingest —
  a terminator-only change is `DiffError::LineEndingsOnly`, never an empty
  diff. The truncation marker `#!kaijutsu-diff truncated: …` LEADS the output
  and parses back into `DiffModel::truncated`.
- **Slice 3**: `FileChange` is required input, not inferred —
  `apply_edit_plan` must say added/deleted/modified/renamed via the `FileSpec`
  constructors. Hydration projection = `truncate_to_bytes(&model,
  limits::MAX_HYDRATION_BYTES)` (32 KiB) + `DiffStat::to_string()`; never the
  char-count path.
- **Slices 4/5**: `limits::MAX_RENDER_BYTES` 1 MiB, `DEFAULT_PREVIEW_LINES`
  20. Freeze-on-open and declared-Diff-that-doesn't-parse contracts are in
  the crate rustdoc — a parse failure is a visible error state, never an
  empty viewer. `FoldState` on `DiffModel` is view state formatting ignores:
  keep folds out of any equality/roundtrip check.
- **Review warnings for slices 4/5** (gemini-pro deliberate, 2026-08-01,
  pre-merge review — approved): `WordSpan` indexes **bytes** of
  `DiffLine::text` — verify Parley span styling consumes byte ranges (it
  should; if it wants char/grapheme indices, translate at the edge or
  multibyte text misaligns). Apply `truncate_to_bytes(MAX_RENDER_BYTES)`
  **before** any Parley layout — parse ceilings are 16 MiB, far past what
  the main thread survives. Map `ContentType` → `DiffProfile` through an
  exhaustive `match` at the app edge so a future ContentType variant is a
  compile error, not a silent fallback profile.

## Build notes — slice 3 SHIPPED 2026-08-01 (kernel)

`kj diff`, `diff_block`, and the hydration projection are built. What the
build settled, for slices 4/5:

- **Source descriptor**: `kj/diff.rs`'s `DiffSource` is `Disk(path)` /
  `Document(path)` / `DocumentAt { path, seq }`. The CLI stayed two
  positionals: one path = disk vs document (the recovery view), two paths =
  document vs document, `--from/--to` = history. Git lands as a variant.
- **Ownership**: both document sources call
  `editor_target()`, a one-line delegate to `resolve_editor_target` — the
  editor and `kj diff` cannot drift on what owns a path. Pinned by a test that
  asserts the *absence* of a `file_context_id()` document after diffing an rc
  script (`file_context_id` is now `pub(crate)` for exactly that).
- **History is real, and bounded**: `BlockStore::block_content_at_seq` replays
  the latest compaction snapshot + oplog up to a seq into a throwaway store;
  `oplog_seq_range` reports what is addressable and rides in every `kj diff`
  `.data` payload (there is no `kj doc history` surface, so this is how a
  caller discovers a seq). It refuses loudly on: no DB, a seq past head, a seq
  older than the newest snapshot, a block that didn't exist yet.
- **Provenance can't ride in the diff text** — block content must be canonical
  unified text — so `disk:/p` / `doc:/p` / `doc:/p@N` travel in `.data`
  alongside the diffstat. **Two different paths format as a rename**: a
  `Modified` section with differing paths re-parses as `Renamed`, so that is
  the only shape that survives its own roundtrip.
- **An empty diff is plain text, not a Diff block.** A bare file header means
  "renamed, contents identical" in this dialect; `kj diff` says "no
  differences" instead.
- **Two hydration branches, one projection.** `diff_block` writes
  `(Tool, Text)`; `kj diff` output is a **ToolResult** block whose
  `content_type` the RPC shell path stamps from `ExecResult.content_type` —
  a different arm of `translate_block` entirely, and the user-shell
  (`[User ran …]`) and agent-tool-result sub-branches inside it are two more.
  All of them go through `hydrate::project_diff_for_hydration`. The
  `<tool_output>` envelope is now reusable via
  `kaijutsu_types::format_tool_content_envelope(block, body)`.
- **The stat describes the whole diff, the body is what fits.** Projection =
  `DiffStat` of the complete model + `truncate_to_bytes(MAX_HYDRATION_BYTES)`,
  whose marker leads the body and re-parses. A declared-Diff block that does
  not parse hydrates as plain text with a `[declared as a diff but does not
  parse …]` note — never dropped, never fatal.
- **Not validated on `Done`**: `validate_content_and_attach_errors` still
  matches only Abc/Svg. Tracked in issues.md; the fix wants slice 4's error
  state to render into.

## Build notes — slice 4 SHIPPED 2026-08-01 (app preview)

The inline preview is built: `RichContentKind::Diff`, the render arm, the
fence sniff, theme colors, and the kernel's `Done` validation. Corrections and
discoveries only:

- **Parley span styling is BYTE-indexed** — `RangedBuilder::push(property,
  range)` takes byte ranges into the text, and so does the app's own
  `SpanBrush`. `WordSpan` needs no translation. **But** the app does not use
  parley ranged styles at all: `collect_msdf_glyphs` colors *per shaping run*,
  looking the brush up from `run.text_range().start`. Runs never span a line
  break, so line-level coloring is exact — and **slice 6's word-level
  highlight cannot ride this path as-is**: a span starting mid-run is ignored.
  Slice 6 must either push real parley `StyleProperty::Brush` ranges at build
  time (parley then splits runs) or track per-cluster offsets in the bridge.
  Decide there; nothing in slice 4 blocks either.
- **`PreviewLine::text_start`** is the translation slice 6 wants: a body line's
  own text starts one ASCII byte past the line start (the `+`/`-`/space
  prefix), so a `WordSpan` maps by adding it. Pinned by a multibyte test.
- **Preview line ranges tile the text** (contiguous, no gaps). Both lookups
  are by byte offset — the brush from a run's start, the background band from
  a *wrapped visual row's* start (`Line::text_range().start`, with
  `min_coord`/`max_coord` for the row box). A gap would fall through to the
  fallback brush mid-line; index-based row lookup would tint only the first
  row of a wrapped `+` line.
- **Two mechanisms, two failure policies.** Declared `Diff` always renders as
  a diff, error preview included. A sniffed ` ```diff ` fence that does not
  parse falls through untouched — sniffing may enrich, never accuse. (SVG's
  fall-through is right for SVG and wrong here: for a diff, plain text is
  indistinguishable from "we chose not to color it".)
- **A per-line elision was needed on top of the two budgets.**
  `truncate_to_bytes(MAX_RENDER_BYTES)` + `truncate_to_lines(20)` bound the
  total and the count but not the *shape*: twenty lines of a minified bundle
  fits both and is a megabyte of wrapped text. `MAX_PREVIEW_LINE_CHARS` = 500.
- **`MsdfBlockGeometry` had a latent staleness bug**, not just an outdated
  comment: nothing ever cleared it, so a block that stopped being ABC (now:
  ABC *or* Diff) kept compositing old shapes behind new content. It is now
  cleared at the top of every rebuild; the producers refill it in the same
  pass under the shared `MsdfBlockGlyphs.version` gate. Bands reuse
  `stroke_line_quad` — a band *is* a butt-capped horizontal stroke.
- **Theme colors are compiled-in**, like the markdown and sparkline colors
  beside them — `ThemeData`/`theme.toml` exposes neither. Wiring rich-content
  colors through the kernel-owned theme is its own (unscheduled) piece of work.
- **The kernel arm landed with this slice**, not after it: `validate_diff`
  in `validate_content_and_attach_errors` + the `Diff` gate in `set_status`,
  with `DiffError::line()` added so the `ErrorPayload` gets a real span.

## Post-ship review — slice 4 (deepseek + gemini-pro deliberate, 2026-08-02)

Holistic reads of the shipped files (no diff), independent casts, convergent
verdicts — slice 4 is clean, no defects found in the slice itself. Both
pointed questions resolved in favor of what shipped: the unconditional `msdf_geometry.vertices.clear()`
is *correct and necessary* (clearing an empty Vec is one branch + a len store;
geometry and glyphs rebuild atomically in one system pass, so no flicker; the
narrower per-arm fix is the error-prone shape), and the declared-vs-sniffed
asymmetry is *coherent policy* (its table of per-type fallback contracts is
now in `detect_rich_content_typed`'s doc comment). Two small fixes applied
same-day: that doc comment, and a comment on `validate_diff` pinning why
default parse options are equivalent to the app's explicit profile (options
steer word-span refinement, never validity).

Findings deferred to their slices:

- **Slice 5 must decide: re-parse-on-open vs retained model.** `DiffPreview`
  deliberately drops the parsed `DiffModel` (and `PreviewLine` carries no
  hunk/file indices), so the viewer cannot be driven from the preview.
  Freeze-on-open already wants its own snapshot bound to `(block_id, content
  hash)`, so parse-at-open is the natural shape — but it re-pays an O(n) parse
  (16 MiB ceiling) possibly on the main thread; a resident model per diff
  block in the conversation is the memory-side trade. Yank must re-`format()`
  from the frozen model either way, never scrape `file_header`/`hunk_header`
  strings.
- **Slice 6: preview-line truncation cuts word spans.** `error_preview` and
  the normal path truncate lines at `MAX_PREVIEW_LINE_CHARS` (500) *after*
  `text_start` is fixed; a `WordSpan` past the cut would index garbage. Filter
  spans against the cut point when word coloring lands. (The band geometry
  path is unaffected — bands stay per-line under word coloring.)
- **Follow-up (gemini, low, in issues.md): line-anchored parse errors render
  as a generic banner.** The kernel attaches `DiffError::line()` spans to the
  ErrorPayload, but the preview's error state doesn't point at the offending
  line. Inline line annotation is high-value polish once slice 5's error
  surface exists.

## Build notes — slice 5 SHIPPED 2026-08-02 (`Screen::Diff` + `DiffCore`)

The viewer is built: `DiffCore` in `kaijutsu-diff` behind a `viewer` feature,
`Screen::Diff` + `KeyboardGrab::DiffView` in the app, `v` on a focused diff
block to open, yank to the system clipboard. Corrections and discoveries only:

- **The key-notation question is answered by deleting it.** `TerminalKey` IS
  the client-neutral seam: `DiffCore::apply_keys` takes modalkit keys exactly
  like `EditorCore`, the Bevy side already converts through
  `input/vim/keyconv.rs`, and a `modalkit-ratatui` client gets them from
  crossterm natively. A third notation crate would be a translation nobody
  needs plus a version to keep in sync with modalkit's. `apply_notation`
  parses `"Vjy"`-style strings for tests. **Slice 5's plan item is resolved,
  not deferred.**
- **The feature gate protects the *dependency edge*, not the kernel binary.**
  `cargo tree -p kaijutsu-kernel -e features -i kaijutsu-diff` shows `default`
  alone — but modalkit still reaches the kernel through `kaijutsu-editor`,
  which owns the vi *editor* session. That predates this work. Also note cargo
  unifies features across a workspace build, so a build that includes the app
  compiles `kaijutsu-diff/viewer` for everyone; `cargo build -p
  kaijutsu-kernel` alone is the meaningful check.
- **Viewer verbs ride modalkit's application-action channel**
  (`Action::Application`), added with `machine.add_mapping` + hand-built
  `EdgePath`s — modalkit's own key-string `parse` is private, but
  `EdgeEvent`/`EdgeRepeat`/`CommonKeyClass` are public, so a plain-char path
  is four lines. This is what makes `]c` take counts and keeps a `q` typed
  into the `:`-line from closing the viewer. `q` overrides vim's
  macro-record binding; `v` is mapped to an explicit no-op (character-wise
  visual stays absent, as decided).
- **Yank reads modalkit's unnamed register** after letting the buffer run the
  `Yank`, instead of re-deriving the range. Counts, motions and the visual
  selection are then vim's own semantics, and the text is *literally* a slice
  of `format(&model)` — `yanking_the_whole_diff_round_trips_through_parse`
  pins it. Read-only is an allow-list on `EditAction` (`Motion`/`Yank` only),
  asserted by a 28-key editing battery.
- **Rows are built from `format_file`'s own output**, assigned by structure
  (how many lines each hunk contributes), so a future canonical header line
  is absorbed instead of shifting every classification after it.
- **`DiffIntent::Refresh` was added to the plan's intent list.** Freeze-on-open
  promises "a stale banner with explicit refresh", and the grab means no app
  binding can reach the screen — the refresh has to be a viewer verb (`R`,
  `:e`).
- **Parse-at-open is synchronous** (deferral recorded, as instructed). It is
  bounded: `truncate_to_bytes(MAX_RENDER_BYTES)` runs before the core exists,
  so the canonical text — and every yank — is the 1 MiB projection with its
  `#!kaijutsu-diff truncated:` marker. Move it to `AsyncComputeTaskPool` if a
  megabyte-scale open ever shows up in a frame graph.
- **The full view needed a row *window*, not just a byte budget.** A megabyte
  of text shaped to draw forty lines stalls the frame regardless. The surface
  lays out a band around the cursor (`window_for`, pure + tested) and slides
  it only when the cursor leaves. `text::diff::view_rows` projects a row range
  into the same `DiffPreview` shape the inline arm produces, so
  `build_diff_span_brushes` / `build_diff_band_geometry` are reused verbatim;
  `preview.lines[i] == core.rows()[first + i]`, and that index — never a byte
  offset — is what the cursor and selection use.
- **`MAX_VIEW_LINE_CHARS` = 2000** (vs the preview's 500): the full screen is
  where you go to read a line, but one minified bundle line still must not
  reach Parley whole. Elision is display-only; yank never sees it.
- **Clipboard writes needed their own thread.** A *read* returns; a *write*
  means becoming the selection owner and serving requests for as long as you
  hold it, so the `arboard::Clipboard` has to outlive the call — `input::
  ClipboardWriter` owns one for the process lifetime behind a channel. The app
  had no clipboard-write path at all before this (docs/input.md's
  "selection auto-copies to PRIMARY" describes intent, not code).
- **`ActiveDiffView` needs `unsafe impl Send/Sync`**, mirroring
  `input::vim::VimMachineResource`: modalkit's `ModalMachine` holds a
  `Box<dyn Dialog>` for TUI dialogs that nothing here can reach.
- **This repo is not rustfmt-clean.** `cargo fmt --all` rewrites hundreds of
  unrelated files; format only what you touched.
- **Folds are tracked, not projected.** `zo`/`zc`/`za` drive `FoldState` and
  `format` ignores it, so rows always describe every line. Hiding folded lines
  is slice 6's rendering call, and slice 6 also owns: word-level coloring
  (with the run-boundary problem from slice 4's notes), the minimap, and the
  multi-rect material that unlocks character-wise `v`.
- **Not yet verified in the running app** (headless tests only): glyph/band
  compositing on the diff surface, the cursor's position on screen, the status
  strip's placement against the dock, and the stale banner firing on a live
  block edit.

### Pre-merge review — slice 5 (gemini deliberate + deepseek, 2026-08-02)

Gemini approved outright. Deepseek found four real issues (no crash, no data
loss); all four fixed before merge. What they changed:

- **The viewer is now line-wise all the way down.** Column motions (`w` `b`
  `e` `0` `$` `|` `f`/`t`) moved modalkit's real column while the drawn cursor
  stayed at line start — invisible state a later yank would read from. Two
  mechanisms now, deliberately belt-and-braces: `is_line_wise` drops
  column-only `EditTarget`s (a **positive** list, so an unclassified
  `MoveType` is rejected — the conservative direction), and
  `snap_to_line_start` forces column 0 after every action so the invariant
  holds by construction for whatever the list misses or a modalkit bump adds.
  The snap alone was tried first and rejected: it makes `b` wrap to the
  previous *line* (a real move) while `w` usually doesn't — coherent-looking
  code, incoherent behavior. Text objects are held to the same rule, so `yiw`
  cannot yank a fragment of a line. **Column motions come back with slice 6's
  word-level rendering**, which is what would make them visible.
- **The row window was doing nothing for the cursor.** One `BuildKey`
  including `cursor_row` meant every `j` re-ran the whole Parley + glyph +
  band pipeline — the window slid correctly and bought nothing. Split into
  `LayoutKey` (content, band, viewport → gates shaping) and `CursorKey`
  (cursor, selection, status, cursor shape → gates a cheap pass over the
  cached layout). Body glyphs and `+`/`-` bands are now the *prefix* of each
  buffer and the cheap pass truncates-and-appends, so no reallocation either.
  The cursor is placed from the cached visual rows rather than from Parley —
  in a line-wise viewer it is a row box, nothing more. Honest limitation
  documented on `WINDOW_SLACK_ROWS`: walking off the *bottom edge* still
  re-shapes one band per row, because the surface draws from the top with no
  scroll offset. Fixing that means caching the parley `Layout` and moving the
  glyph offset; not worth it until a profile says so.
- **The cursor shape was `Beam`** — the insert-mode shape, on a surface that
  is never in insert mode. Now `mode_kind(core.mode())`: block in normal,
  hidden in visual (the selection bands are the visible thing there), hidden
  in the error state, which has no cursor at all.
- **An empty diff said `1/1`.** The counter is omitted when there are no rows.

Verified fine, no action: the truncate-vs-parse ordering (we parse *then*
truncate the model, so the ceiling applies to a real model), the intent drain
order (yank before close, refresh deferred to the end of the batch), and
clipboard ownership ending at process exit (platform reality, tracked with the
PRIMARY entry in `docs/issues.md`).

## Build notes — slice 6 phase A SHIPPED 2026-08-04 (words, folds, columns)

The three items that did not need the multi-rect material are built. What
the build settled:

- **Run splitting: parley's ranged brushes won, not per-cluster tracking in
  the bridge.** `VelloFont::layout_spanned` pushes the span list as real
  `StyleProperty::Brush` ranges; parley then subdivides its glyph runs by
  style index and `GlyphRun::style().brush` hands the color back per run.
  `collect_msdf_glyphs_styled` reads it. The bridge got *simpler*, not
  smarter — the alternative would have re-implemented in our code a split
  parley already performs. Pinned by real shaping in a headless test (the
  loader's registration half is `#[cfg(test)] pub(crate)`, so a test loads a
  shipped `.ttf` into the same font collection the app uses): a mid-run span
  colors exactly its glyphs, the old run-start lookup demonstrably does not,
  and — the one that would have bitten silently — **brush is not a shaping
  property**: spanned and plain layouts position every glyph identically.
- **`build_diff_span_brushes` emits DISJOINT spans**, cutting each line
  around its words rather than stacking word spans over line spans. Parley
  resolves overlap by push order, `brush_at_offset` resolves it by first
  match; disjoint spans mean the two mechanisms cannot disagree.
- **`DiffPreview::words` is a flat list, not a field on `PreviewLine`** —
  the line stays `Copy` (the class and band lookups index it per visual row,
  per frame). Truncation filtering landed as `elision_cut`: the byte offset
  where display elision cut a line, with spans dropped past it and clipped
  across it (never drawn over the `…`). Both budgets go through it.
- **The full view reaches word spans through row provenance**, since spans
  are derived and never serialized. A `\ No newline` row shares its body
  line's `file/hunk/line` and none of its text, so it is excluded *by kind* —
  the one place that indirection can bite.
- **Folds project in `DiffCore`, not in the app.** `visible_rows()` is a
  second derived list — row indices to draw — while `rows()`/`text()` still
  describe the whole patch, so yank and the canonical model never see a fold.
  Projecting in `view_rows` alone would have left modalkit's cursor free to
  sit on a row nobody draws, the same invisible-state failure that cost
  slice 5 its column motions. A folded hunk **keeps its header row** (the app
  appends `⋯ N lines folded`); no synthetic row, so the projection stays a
  subsequence and every drawn line maps back to a canonical one.
- **The cursor is snapped out of folds, direction-aware** (vim's behavior):
  walking down lands past the fold, every other way in parks on the header.
  Cursor and selection publish visible coordinates (`cursor_visible_row`,
  `selection_visible_rows`) so window, bands, status counter and projection
  index one space. A selection spanning a fold still yanks the hidden lines.
- **`fold_seq` had to join the layout cache key.** Folding changes the
  projection without changing a byte of content, and two folds of equal size
  between frames leave even the visible row count unchanged.
- **Column motions returned by narrowing the rule, not deleting it.**
  `snap_to_line_start` is gone and `is_line_wise` became `is_line_wise_yank`,
  applied only when the resolved op is a `Yank`. **Yank stays line-wise** —
  `yw`/`y$`/`yiw` would produce a fragment carrying a `+` and no line, text
  that looks like a patch and is not one. Partial-line yank is blocked by
  *meaning*, not by rendering; it stays out until something asks for it with
  a semantics attached.
- **`cursor_col()` is clamped for reporting only.** modalkit keeps a goal
  column (vim's `$`-sticky) that can sit past a short line; clamping the
  reported value keeps the renderer honest without breaking the goal column.
- **The surface now caches the parley `Layout`** and places the cursor with
  `Cursor::from_byte_index` — the editor's own path. A row box cannot answer
  where a column is, and on a *wrapped* line a late column belongs to a later
  visual row, which cell-width arithmetic would put off the right edge. The
  query searches already-shaped lines, so the cheap pass stays cheap.
  `text::diff::cursor_byte` is the single char-column → byte-offset step.
- **Not verified in the running app** (headless only): word emphasis colors
  against the theme on screen, the fold indicator's readability, the cursor's
  drawn position at a column (especially on a wrapped line), and that folding
  visibly re-lays-out. The `docs/issues.md` diffstat-footer overlap entry is
  still unretested and is the natural thing to check in the same sitting.

### Post-ship review — slice 6 phase A (deepseek + gpt-5.6-sol, 2026-08-04)

Two independent casts, holistic reads (no diff). Both cleared the word-span
translation, the elision clipping, the canonical-vs-visible coordinate
discipline, and `snap_out_of_folds`. Both found the **same** defect, and the
audit it triggered found three more of the same shape:

- **Un-pinning the column re-opened every yank whose justification was "the
  snap discards it."** The positive list was written when the cursor could
  not leave column 0; four entries were only safe *because* of that.
  `y^`/`yg^` (`FirstWord`/`ScreenFirstWord` at a non-zero column),
  `` y`a `` (`CharJump` — a *character* mark address, and `ma` is settable
  because it is bookkeeping), `yvj`/`y<C-v>j` (vim's explicit shape
  overrides), and `<C-v>` block visual (pre-existing since slice 5: `v` was
  Nop'd and `<C-v>` was not) each handed back a fragment carrying a `+` and
  no line. Fixes: a **forced-shape guard** ahead of the target list (only
  `None`/`LineWise` may proceed — this is what catches the overrides, which
  no target-type list could), `CharJump` split from `LineJump`, regex search
  flipped to refused before it is ever wired, and `<C-v>`/`<C-q>` Nop'd.
  **The lesson worth keeping: when an invariant stops being enforced by
  construction, re-derive every rule that leaned on it — a positive list is
  only as honest as the reason each entry is on it.**
- **The layout cache could not see a theme change.** Colors are baked into
  the cached parley layout (as ranged brushes) and into the cached band
  geometry, and the kernel replaces the whole `Theme` resource over RPC. Now
  `theme.is_changed()` joins the relayout condition — change detection, not
  a hash. Font size joined `LayoutKey` (fixed today, a trap for whoever
  makes it adjustable), and sizes ride as f32 bits instead of truncated
  integers: the surface is laid out in *logical* px, which a fractional
  HiDPI scale makes fractional, so a resize inside one integer could still
  move a wrap point.

## Build notes — slice 6 phase B SHIPPED 2026-08-04 (rects, `v`, minimap)

The three things that needed the multi-rect material, plus the word-level
background the material's rect math made cheap. What the build settled:

- **Character-wise `v` is IN, and yank grew a second semantics.** Slice 5 said
  `v` was absent because the selection could not be *drawn*; phase A then said
  yank stays line-wise because a fragment carrying a `+` and no line is
  "blocked by meaning, not rendering". With the drawing problem solved, the
  two had to be reconciled, and the answer is that **the shape decides the
  semantics**: whole lines yank canonical unified text (a patch fragment,
  prefixes included, `VGy` still re-parses); characters under `v` yank the
  selected text with **every line's prefix column removed** — plain source,
  explicitly not a patch, and unable to masquerade as one because no `+`
  survives. That is what `v` is *for*: grabbing an identifier or an expression
  out of a diff you are reading. `DiffIntent::Yank` carries a `YankKind` so the
  distinction is in the type, not in a comment.
  **Enabling `v` did not loosen phase A's operator guard by one inch.** `yw`,
  `y$`, `yiw`, `` y`a ``, `y^`, `yvj` all still yank nothing. The line: a
  visual selection is a decision the reader *saw highlighted* before acting on
  it; an operator-motion fragment is an accident of a motion. `<C-v>` block
  visual stays Nop'd — a rectangle cuts *through* the prefix column, so it is
  neither a patch nor a coherent run of source. It is the shape with no
  semantics attached, and it stays out until something asks for it with one.
- **The selection branch had to read modalkit's own rule, not the context.**
  `EditTarget::Selection` resolves its shape as
  `ctx.get_target_shape().unwrap_or(cursor_state.shape())`. Asking the context
  alone — the obvious thing — would have missed a plain `v`, which forces no
  shape, and silently routed it to the patch path with prefixes intact.
- **The material takes many rects, and interior rows draw full width.** A
  fixed uniform array of 16 (portable everywhere wgpu runs; the shader loops to
  `count`, never to the capacity). `coalesce_selection_rects` replaces the
  interior rows of a multi-row selection with **one** full-width band: it is
  what every editor draws (the band to the right edge is how you see the
  newline is included) *and* it bounds a contiguous selection at three rects,
  so the fixed capacity can never truncate one. The capacity is headroom for
  shapes that do not coalesce (bidi rows, future search matches); overflow
  warns rather than silently dropping what the reader can see.
- **Two compositors, one producer.** The rects come from parley's own
  `Selection::geometry` (the off-the-shelf answer `docs/vi.md` names). The
  editor panel will push them through the *material*, which composites over
  the text; the diff viewer draws them as `MsdfBlockGeometry` quads, because a
  diff selection has to sit on top of the `+`/`-` bands and under the glyphs.
  Neither surface could use the other's compositor, and both use the same
  rects. `shaders::selection` is the shared middle.
- **A count is not a column** (deepseek post-ship review, phase A — a real
  bug found and fixed here). `cursor_col` clamped to `row_char_len`, which is
  one *past* the last character; `cursor_byte` then returned the line's
  newline byte, and parley's `Cursor::from_byte_index(.., Downstream)`
  resolves that onto the **next** visual row. `$` then `j` onto a shorter line
  drew the cursor a line low. Fixed in both halves — `DiffCore::last_col`
  clamps the reported column, and `cursor_byte` clamps to a real character —
  which split the old function in two: `column_byte` stops at the end of the
  text (the exclusive end a *selection* wants), `cursor_byte` stops *on* the
  last character (where a cursor is *drawn*). Using either for the other's job
  is a visible bug: a selection missing its last glyph, or a cursor a line low.
- **The minimap keys on its own tier, coarser than the cursor's.** Buckets are
  O(drawn rows) to build and O(buckets) to draw, so they are computed against
  `(content, fold_seq, bucket count, strip box)` and *memcpy'd* into the
  geometry buffer on every cheap pass; only the viewport band and the cursor
  line are rebuilt per keystroke. It never joins `LayoutKey`, so a minimap
  redraw never re-shapes a glyph.
- **A bucket keeps counts, not a dominant class.** One deleted line inside a
  screenful of context is what a reader scans a minimap *for*; a slot that
  reported only its majority would erase it. Insertions and deletions draw as
  a diverging bar from the strip's centre, each with a two-pixel floor, so
  presence survives compression even where proportion cannot.
- **The minimap is built from `visible_rows`, not from the model**, which is
  the one place this deviates from the seam guidance's "extents live on
  `DiffModel`". A minimap must agree with what is on screen, and that is the
  fold projection — a viewer concept. Folds are reflected for free as a
  result. The row↔bucket mapping is invertible and tested, so click-to-jump
  (Decision 6's reason for wanting `viz::scales`) is a wiring job, not a
  design one.
- **Word-level background washes came free** with the rect math: the same
  `Selection::geometry` query over a `WordHighlight`'s byte range, no cluster
  walk in our code at all. They ride the *layout* tier beside the `+`/`-`
  bands (word spans change only when the content does) and draw over them,
  under the glyphs.
- **Not verified in the running app** (headless only): the WGSL compiles only
  on a real device, so the multi-rect shader loop is unproven until the
  runner draws a selection; the character selection's rects against a wrapped
  line; the minimap's readability and its width against the status strip; and
  the word washes stacked on the line bands.

### Post-ship review — slice 6 phase B (deepseek, 2026-08-04)

Holistic read of the shipped files, no diff. **No correctness defects.** It
verified the three things most likely to be quietly wrong and found them
right: the encase↔WGSL layout of the new uniform matches byte for byte
(`[Vec4; 16]` at offset 0, `count` at 256, struct padded to 272); a dynamic
loop over a uniform array bounded by `min(count, MAX)` is valid on every wgpu
backend; and no consumer still assumes the single-rect uniform. It also traced
that no key sequence can get a `+`/`-` prefix into a `PlainText` yank, and
that the `CharSpan` is always captured *before* the buffer action collapses
the selection it came from. Two robustness notes applied: `coalesce_selection
_rects` now filters both paths through `is_visible` (the multi-row path was
correct only because coalescing happened to give a zero-width rect width), and
the render-side local span no longer carries an `end_col` measured on a row
outside the laid-out window — dead today, a magnet for a future refactor.

## Seam guidance (deepseek review, 2026-08-01)

Consulted deepseek specifically on how the seams should evolve so this work
leaves the codebase easier to maintain. Verdicts (details in the session; the
actionable ones are folded into the slices above):

- **Rich-content dispatch: CONFORM.** Add the 7th `RichContentKind` arm; no
  trait/registry yet — the match arms are a visible checklist of subtle
  per-arm invariants (version bump, `content_height`, glyph clear policy) and
  a registry would scatter them. Revisit at ~10 kinds or when the minimap
  needs cross-cutting "which kinds produce geometry" knowledge. The Diff arm
  follows the **bump-from-own-version** pattern (ABC's); Sparkline/Svg
  currently deviate — tracked in issues.md, don't copy them.
- **ContentType: REFACTOR** — explicit `richness()` (slice 2).
- **DiffCore: CONFORM** to the duplication call; share only key-notation
  translation (slice 5). Three modalkit machines (compose/editor/diff) is
  the "each genuinely different" zone; a fourth vi surface triggers a remap.
- **MsdfBlockGeometry: EXTEND by comment only** — extraction is already
  producer-agnostic; geometry and glyphs rebuild together for Diff exactly as
  for ABC, so the shared version gate holds. Update the "ABC-only" comment.
  Minimap rects ride the same component; extents live on `DiffModel`.
- **Emission: EXTEND** — per-type `diff_block` tool (self-documenting for
  models, the ABC validate-first pattern). Separately (own small PR, not
  bundled): give `block_create` an optional `content_type` param so shell/
  programmatic clients can emit typed blocks at all.
- **Input: CONFORM** — `Screen::Diff` + `KeyboardGrab::DiffView` + a
  `derive_contexts` arm + test; the explicit enum stays auditable to ~8
  screens, a registration system would lose the one-function audit property.

## Batch review verdicts (gpt-5.6-sol + gemini-pro, 2026-08-01)

Both conditionally approved the architecture (rail, unified text, app-local
DiffCore all endorsed); their amendments are folded into the slices above.
Convergent top risks: **hydration token economics** (canonical block vs
model-facing projection — the #1 product risk), **LWW rank must be pinned,
not appended**, and the **slice 5/6 selection-rendering inversion** (fixed:
line-wise `V` only until multi-rect). Still open:

- **`Diff`'s richness rank — DECIDED 2026-08-01 (Decision 7): sol's placement,
  above `Abc`, below `Image`.** The reviewers split: gemini says
  between `Markdown` and `Svg` (a diff is still text); sol says between
  `Image` and `Abc`. With `richness()` explicit (slice 2) this is just
  picking a number, but pick it deliberately and test equal-timestamp ties.
- **Mixed-version policy** (sol): an old client reads unknown MIME as
  `Plain` and may *rewrite* it, erasing the typing; also two versions can
  rank the same tie differently. Early-development answer is likely
  "coordinated upgrade — all clients ship together", but say so explicitly
  and add the old-writer round-trip test when it matters.
- **Live diffs need a different primitive** (both): a durable text block
  repeatedly rewritten is the wrong shape for the future live-diff idea —
  that wants a derived/transient view keyed by explicit source versions
  (frontiers), with debounce, persisting immutable checkpoints if at all.
  Recorded here so v1's text rail isn't bent toward it later.
- **Auto-emit defaults non-hydratable** (both): auto-generated diffs flooding
  hydration is the context-poisoning path; opt-in visibility only.
- Sol's dossier couldn't verify the wire claim; our own survey did:
  `BlockSnapshot.contentType @8` / `BlockMetadata.contentType @3` are
  free-form Text (MIME) in `kaijutsu.capnp` — no schema change needed. Sol
  also flags auditing whether content and content_type update atomically
  under concurrency (they are separate LWW registers) — the renderer must
  tolerate declared-Diff-that-doesn't-parse (slice 1's loud malformed path).

## Future / adjacent (recorded, not scheduled)

- **Live diff blocks** — a diff block that references `(context, block,
  frontier_a, frontier_b)` and recomputes as the underlying kernel document
  changes; "watch the change grow" while a model works. Rad if performance
  holds (Amy); strictly after on-demand ships.
- **Auto-emit policy** — emitting a diff block per file edit (hook point:
  `apply_edit_plan`) needs a noise/token-spend answer first: coalesce per file
  per turn? opt-in? ephemeral/excluded-by-default blocks?
- **Git sources** — after kaish-extras git: worktree-vs-main file lists (gix),
  feeding the same `DiffModel`; `gix-diff`'s rename/rewrite tracking when we
  want file-level moves.
- **`ContentType::Ansi`** — SGR → `SpanBrush` parser as a general
  terminal-output block type (kaish output, external tools). Small, separate.
- **ABC engraved diff** (Amy 2026-08-01) — v1 diffs ABC as text with the
  bar-aligned tokenizer (measure-level hunks). The real prize is *notation*
  diffing: engrave both versions with `kaijutsu-abc` and highlight
  changed/added/removed bars **on the staff** — the music geometry pipeline
  (`MsdfBlockGeometry` + music glyphs) already draws everything needed; a
  changed-bar wash is a background quad per bar extent. Bar-aligned text hunks
  map 1:1 onto engraved measures, so the text diff *is* the index into the
  visual one.
- **Syntax highlighting under diff colors** — the biggest visual lever per the
  research; kaijutsu has span infra but no grammars (kaish tokenizer stubs are
  marked "Phase 4: syntax highlighting via Parley spans"). Design the diff
  span pipeline so a highlighter layers *under* diff coloring later. Two
  candidate sources, not mutually exclusive: **tree-sitter grammars**
  (app-or-kernel, offline, fast, the difftastic/helix path) vs **LSP semantic
  tokens** (Amy 2026-08-01) — a kernel-side LSP broker (the MCP-broker shape
  pointed at language servers) would serve highlighting to *every* client and
  later feeds hover/defs/rename and the IDE-peer entry. Likely answer:
  tree-sitter for fast local coloring, LSP for semantics when a server is
  attached.
- **Structural diffing** — tree-sitter based (difftastic is a binary, not a
  library; vendor-or-reimplement per its manual; HyperAST/mergiraf are Rust
  reference code). Far future.
- **Move detection** — display-time à la `git --color-moved`; BDiff
  (arXiv 2510.21094) for a principled block-assignment version.
- **btrfs snapshots** as a workflow/versioning source to diff against
  (Amy 2026-08-01) — experiment someday.
- **Hunk staging / partial apply** — scm-record (the widget inside `jj split`)
  is the reference UX; pairs with the IDE-peer entry in issues.md.

## Research anchors (2026-08 survey)

imara-diff 0.2 (histogram + indent heuristic, arbitrary tokens; in gitoxide as
`gix_diff::blob`) · jj-lib `lib/src/diff.rs` (recursive refinement, pluggable
tokenizers/equivalence, N-way) · git `--color-moved` (display-time move
detection) · VS Code diff rewrite (word-level sync heuristics, moved-block
arrows) · difftastic manual (tree-diff as Dijkstra over position pairs) ·
BDiff arXiv 2510.21094 (block move/copy edit actions) · scm-record (embeddable
staging TUI).
