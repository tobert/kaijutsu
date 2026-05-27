# Open Issues

Live work items distilled from prior design and TODO docs. Code is truth;
this exists to track what's *not* in the code yet.

Organized by area. Keep entries terse — link to file:line when a pointer
makes the work concrete. When an item ships, delete the entry.

---

## User presence (novel surface)

The compose input is a shared CRDT document (`editInput @75`,
`getInputState @76`, `pushInputOps @77`). Today the model only sees the
atomic `submitInput @78`. Surfacing in-flight compose state to an
opted-in model would enable mid-sentence collaboration that no other
agent framework can do — kaijutsu has the substrate; nothing currently
uses it. Gate with explicit user opt-in to avoid creepy default behavior.

## Tool system follow-ups (post-Phase 5)

The MCP-centric kernel landed in five phases plus hook persistence; the
following are explicit follow-ups that did not ship:

- **rc system follow-ups.** `context_type` is now defined as "the
  bucket of rc scripts that gives a context its mode" (a SysV-flavored
  init bundle); capnp wire surface can land using that framing.
  CRDT-VFS bridge for collaborative script editing (speculative).
- **`StreamingBlockHandle` implementation.** Single-block streaming
  primitive. Build when the first caller arrives (likely the LLM
  streaming rewrite). Resolve async-drop strategy and append granularity
  at implementation time.
- **LLM streaming rewrite.** Move
  `kaijutsu-server/src/llm_stream.rs::process_llm_stream` onto
  `StreamingBlockHandle` plus an outer orchestrator that mints sibling
  blocks for thinking/text/tool_call boundaries.
- **Block content abstraction.** Blocks as containers for multiple
  content artifacts; prerequisite for richer resource-subscription
  rendering.
- **MCP `progress` → `StreamingBlockHandle` bridge.** External streaming
  tools wired the same way virtual tools will be.
- **Per-principal budgets + fair queuing.** Adjacent to the now-shipped
  `builtin.policy` get/set surface; per-principal accounting is the
  next layer up.

## LLM providers

- **Cross-turn thinking continuity.** `hydrate_from_blocks` skips
  `BlockKind::Thinking` entirely (`llm/mod.rs:1041`), so reasoning
  never re-enters the conversation between separate
  `process_llm_stream` calls. Out of scope per the original A3
  framing; revisit if extended thinking lands and the stale-reasoning
  cost is worth the token spend.

## Persistence & sync

- **`KernelDb` connection pool.** Currently `Arc<parking_lot::Mutex<KernelDb>>`
  in `block_store.rs:73`. Every RPC reads/writes through one lock. Migrate
  to `r2d2` / `sqlx` for concurrent reads.
- **Config CRDT ops.** Config backend needs DTE integration so changes
  replicate across peers.
- **CRDT `order_index` BTreeMap.** `blocks_ordered()` is O(N log N).
  Works correctly but scales poorly; add a secondary sorted index when
  scale demands.

## Index

- **`hnsw_rs` reverse-edge quirk.** `reverse_update_neighborhood_simple`
  writes reverse edges at the neighbour's own assigned layer rather than
  the current search layer. Points landing at level > 0 may not appear
  in later-inserted points' layer-0 neighbour list, silently degrading
  ANN recall. Tests work around it; production code should switch
  libraries, patch upstream, or accept reduced recall.

## Card-stack view

Holographic shader trio + entry animation shipped. Open:

- **Card size tuning.** Focused card should fill ~70% of viewport width.
  Camera at z=180, `card_width=200`. BRP-tweak `CardStackLayout` to find
  good values.
- **Read-only scroll** on focused card when content exceeds card height.
- **Dive-in (Enter)** to switch to Conversation view scrolled to that
  card's blocks.
- **Mouse click** to focus a card.
- **Momentum scrolling** with velocity decay.
- **Camera parallax** tracking focused card position.
- **Streaming card texture updates.** Cards are spawned once on
  `OnEnter`; streaming blocks during model response should refresh the
  card's child quads.
- **Card grouping evolution.** Currently role-run; consider user-turn +
  model-response as one "exchange" card or collapsible tool-call groups.
- **Ambient environment.** Particle field / star-field; post-process
  bloom for edge glow.

## Text rendering (MSDF / 次)

- **TAA temporal super-resolution (deferred).** Per-block history
  textures + Halton jitter + YCoCg variance clipping. Significant
  complexity for the per-block architecture; revisit when subpixel
  shimmer at 12–14 px sizes becomes a real problem or when 3D text
  compositing is on the table. Source material exists in the pre-vello
  worktree.
- **Glyph spacing slightly tight** — anchor math may need per-font
  tuning.
- **1-frame blank flash on texture resize** (GpuImage upload latency).
  Self-heals; could be hidden with a "pending texture" guard.
- **Large-context Vello "paint too large" crash** (16384 px tall
  textures, ~173 blocks). Not MSDF-related but worth investigating
  separately.

## kj / control plane

- **Tab completion.** Phase 6 of the kj rollout — context labels (with
  prefix resolution), preset labels, workspace labels, tag syntax
  (`opusplan:` then hex prefix suggestions). Integrate with kaish's
  completion system.
- **Cross-kernel drift.** Schema preserves `kernel_id` everywhere; not
  yet implemented.
- **Compact quality.** `kj fork --compact` uses a generic
  `DISTILLATION_SYSTEM_PROMPT`. Consider preset-level or context-level
  summary-style control. The `--distill-model` knob already lets
  callers pick a cheap summarizer; custom distillation *prompts* are
  the remaining shape.
- **Workspace auto-mounts at context join.** How workspace paths
  translate to VFS mounts on join.
- **kj CLI binary.** Standalone `kj` for headless scripting; thin
  adapter over `KjDispatcher`.
- **Distill model selection.** `formation_edges`-style `auto_distill`
  defaults to the source context's model (potentially Opus). Add a
  `distill_model` knob so cheap models can do summarization.

## Live eval

Surfaced by `crates/kaijutsu-server/tests/live_eval.rs` (slice 1, commit
d43df35). See the `project-live-eval` memory for scope.

- **Fork copy scope.** `kj fork` is a full copy of the source: notification
  blocks from MCP tool registration, in-flight `model/tool_call` +
  `tool/tool_result` pairs, and the conversation all transfer to the child.
  Implementer block lists are much noisier than the conversation length
  suggests. Design question: should fork be selective by default with rc
  scripts opting blocks in/out, or full-copy with rc-on-fork pruning?
  Related to cache-breakpoint policy.
- **russh teardown panic.** After the live-eval test function succeeds, a
  trailing `russh::channels::io::ChannelCloseOnDrop::drop` panics with
  "there is no reactor running" — russh tries to `tokio::spawn` from a drop
  path after the LocalSet runtime has already begun shutting down. Pollutes
  the test output and confuses casual readers. Either close the SSH client
  explicitly before exiting `run_local`, or upstream a fix to russh so drop
  is a no-op when no reactor is available.

## ABC parser / kaijutsu-abc

- **Multi-tune `.abc` files vs. kaijutsu blocks.** `kaijutsu_abc::parse`
  now returns `Vec<Tune>` (`crates/kaijutsu-abc/src/lib.rs:59`) since a
  `.abc` source can hold several tunes delimited by `X:N` per spec §2.2.
  The rendering path in `kaijutsu-app` currently takes the first tune
  and TODO's the rest (`crates/kaijutsu-app/src/text/rich.rs:397`).
  Open design question: when an `abc_block` carries a multi-tune
  library, should the kernel/drift layer split the tunes across
  sibling blocks (so each Tune is its own renderable artifact, with
  CRDT identity), or should the renderer stack them inside one block?
  Affects abc_block storage shape and drift semantics. Most relevant
  to §13-style sample libraries; not blocking single-tune authoring.

- **File-header inheritance for M:/L:/Q:.** Spec §2.2 inheritance is
  implemented for `T:`, `C:`, `R:`, `S:`, `N:`, and the `other_fields`
  bag, but `M:`/`L:`/`Q:` don't inherit yet because `parse_header`
  fills defaults eagerly — once the tune has `Some(default 4/4)` the
  inheritance pass can't tell it apart from an explicit `M:4/4`. Fix:
  track an explicit-vs-default flag, or move default-filling into a
  separate post-parse pass that consults the file header first.

- **Inline `%` comments inside field values.** Lines like
  `X:1                   % tune no 1` or `M:3/4                 % meter`
  feed the trailing `% comment` text into the field-value parser,
  producing "Invalid X: value" / "Invalid meter" warnings. Spec §3.1
  treats `%` as an end-of-line comment marker even inside field
  values. Strip `% ...` from value strings before parsing.

- **`$` line-break + `I:linebreak` directive (§6.1).** `I:linebreak $`
  enables `$` as an explicit score line-break marker in the body. Not
  implemented — `$` hits the unknown-character fallback. Also affects
  `I:linebreak <none>` / `<EOL>` mode selection.

- **`U:` user-defined symbol expansion.** `U:T = !trill!` is captured
  in `Header::other_fields` but the body parser doesn't apply the
  substitution, so `T` mid-music takes its default decoration meaning
  (or hits the fallback). Apply U: mappings at body parse time.

- **`m:` macro expansion.** `m:` macro definitions are captured as
  `InfoField` (header `other_fields`, body `Element::InlineField`) but
  the body parser doesn't expand `~G2` etc. into their definition.
  Spec §9 covers both static macros and the transposing form
  `m:~n2 = ...`.

- **Slur grouping in AST.** `Element::Slur(SlurBoundary::Start/End)`
  markers are emitted but slurred notes aren't grouped into a
  `Slur { notes: Vec<_> }` structure, so renderers / MIDI handlers
  can't tell which notes are inside a slur without tracking depth.

- **`%%` stylesheet directives.** Currently treated as comments and
  silently skipped (only `%%MIDI program` is parsed, in the header
  path). A general directive AST node would let downstream consumers
  see `%%score`, `%%pageheight`, `%%setbarnb`, etc.

- **Unicode escapes and font directives in text strings (§8.2).**
  Mnemonics (`\'e` → é), named entities (`&eacute;`), fixed-width
  escapes (`é`), and the `$1`-`$4` font directives are captured
  verbatim in field values rather than decoded. Decoding belongs at
  the rendering boundary, not the parser, but it has to happen
  somewhere — track it here so it doesn't get lost.

- **`P:` (parts) jump-to-part semantics.** `P:A`, `P:AABB`, etc. are
  captured as info fields but the playback path doesn't reorder bars
  according to a parts string. Per spec §3.2.


