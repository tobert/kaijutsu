# Diff Blocks & Viewer

Diffs where the work happens — in the conversation. A model emits a diff block
as it edits; Amy reads a small preview inline and expands it into a vi-motion
full view with yank. Not git-dependent: the first-class source is kaijutsu's own
CRDT file documents; git arrives later as *another source* feeding the same
block type.

## Status

**Designed 2026-08-01 (research + survey session), not built.** The `text/`
branch gate this design queued behind lifted the same day (msdf-music + de-vello
landed on main), so the slices below are ready to start, in order. This doc is
the plan of record.

## Decisions (Amy, 2026-08-01)

1. **Block shape: `ContentType::Diff` (`text/x-diff`) on a `BlockKind::Text`
   block** — the SVG/ABC rail exactly. `contentType` is a free-form MIME string
   on the wire (`BlockSnapshot.contentType`), so **no `kaijutsu.capnp` change,
   nothing in the CRDT or client**. Content is standard **unified diff text**:
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
   already a CRDT document with a full oplog
   (`FileDocumentCache`, `kaijutsu-kernel/src/file_tools/cache.rs`) — before
   and after are both in hand at `apply_edit_plan`
   (`mcp/servers/file.rs`). **On-demand emission first**: a `diff_block` MCP
   tool and a `kj diff` command. Auto-emit-per-edit and live-updating diffs are
   future work (below). Git-backed sources (worktree vs main — Amy's daily
   itch) come **after kaish-extras git lands**, as new sources for the same
   pipeline.
4. **Full view: `Screen::Diff` + an app-local, read-only modalkit motion
   machine (`DiffCore`).** Not a kernel editor session. `docs/vi.md` Decision 6
   ("mode lives kernel-side, period") protects *edit/mode sync with a CRDT
   block*; `DiffCore` has no edit state — it emits motions, folds, and yanks,
   never an `EditOp` — so there is nothing to race or corrupt. Real vi grammar
   (counts, `gg`/`G`, `]c`/`[c` hunk motions, visual-line select) via modalkit,
   and **yank lands in the system clipboard** — which is inherently app-side
   anyway, and the concrete reason Amy wants vi here (grab a hunk to paste into
   feedback). Specialization over code-sharing with `EditorCore`: a little
   duplication is fine (Amy).
5. **Preview in conversation**: a stat header (`3 files, +42 −17`) plus the
   first N lines, collapsed by default; expand under Navigation into
   `Screen::Diff`. Preview rows must respect the virtualization contiguity
   invariant (`view/render.rs` two-spacer model) — expansion is a screen
   change, never an out-of-band conversation row.
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

- **`kj diff <a> [<b>]`** — sources: two VFS paths; a file's CRDT document vs
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
3. Kernel: `kj diff` + `diff_block`, wire e2e asserting a typed block lands.
   Source resolution is **ownership-aware from day one** — the
   `resolve_editor_target` pattern (config-owned docs answer through the
   mount table; never raw `get_or_load` for a config path, which would mint a
   shadow CRDT doc — the exact bug class `docs/config-crdt-ownership.md`
   killed). Internally a typed source descriptor even while the CLI stays
   simple. **Hydration is a projection, not a passthrough**: the canonical
   block keeps the full diff; the model-facing envelope gets diffstat +
   whole-hunk-bounded content with an explicit complete/truncated marker
   (the existing char-count truncation can cut mid-hunk and leave a
   plausible-looking partial patch). Test hydration end-to-end for **both**
   producers — `kj diff` output and `diff_block` may enter different
   role/kind hydration branches.
4. App preview: rich-content arm + render arm + fence sniff + theme.
5. `Screen::Diff` + `DiffCore` + grab/bindings + yank→clipboard.
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
6. Minimap, folds, word-level highlight polish; multi-rect material slice.

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
  frontier_a, frontier_b)` and recomputes as the underlying CRDT document
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
