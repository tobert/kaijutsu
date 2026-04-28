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

## Interactive affordances on blocks

Once a block reaches `Status::Done` it is static. No buttons, no
clickable links to pre-fill the input doc, no expand/collapse handles
the model can attach. A small block-metadata slot for "interactive
affordances" turns model responses from monologues into interfaces.
Related to the streaming-block-handle work in §9 of the (now-retired)
tool-system redesign doc.

## Tool system follow-ups (post-Phase 5)

The MCP-centric kernel landed in five phases plus hook persistence; the
following are explicit follow-ups that did not ship:

- **Persona ListTools-hook bundles + persistence.** v1 of
  `builtin.personas` ships binding-only archetypes (planner / coder /
  explorer / sound engineer); `apply` swaps the calling context's
  `ContextToolBinding`. Adding `HookPhase::ListTools` hook bundles to
  the persona shape and persisting personas in `KernelDb` (alongside
  bindings, D-54) are the remaining work.
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
- **Kaish-backed hooks.** Fill in `HookBody::Kaish`: serialize
  request/result, run script, parse return.
- **MCP `progress` → `StreamingBlockHandle` bridge.** External streaming
  tools wired the same way virtual tools will be.
- **Per-principal budgets + fair queuing.** Adjacent to the now-shipped
  `builtin.policy` get/set surface; per-principal accounting is the
  next layer up.

## LLM providers

- **Extended-thinking signature plumbing.** `ContentBlock::Reasoning`
  preserves in-call thinking across tool-use iterations (A3, shipped in
  fe6934f) but `signature` is hard-coded to `None` because the rig
  adapter flattens streamed reasoning to delta text and drops the
  provider signature. Safe today — extended thinking isn't enabled on
  any production path. If/when it is, Anthropic will reject any
  Reasoning block that follows a tool_use without a valid signature.
  Wire signatures through `RigStreamAdapter` (capture from
  `StreamedAssistantContent::Reasoning` and surface on
  `StreamEvent::ThinkingEnd`), accumulate alongside `assistant_thinking`
  in `llm_stream.rs`, and pass through `with_reasoning_text_and_tool_uses`.
- **Cross-turn thinking continuity.** `hydrate_from_blocks` skips
  `BlockKind::Thinking` entirely (`llm/mod.rs:843`), so reasoning never
  re-enters the conversation between separate `process_llm_stream`
  calls. Out of scope per the original A3 framing; revisit if extended
  thinking lands and the stale-reasoning cost is worth the token spend.
- **Reconsider `rig` as the provider abstraction.** Extended-thinking
  signature plumbing (above), image fallback semantics, reasoning
  round-trip, and truncation-vs-error handling all push against rig's
  current model. We may be hitting the wall where a thin
  provider-specific layer earns its keep over a generic adapter. Worth
  periodic review.
- **Memoize CAS image base64 in `ConversationCache`.**
  `resolve_image_blocks_from_cas` reads + base64-encodes every image's
  bytes per prompt. The spawn_blocking fix took the runtime stall out;
  next is caching by `(context_id, hash)` so a 20-image conversation
  doesn't re-encode every screenshot every turn.
- **`personas_define` instance-id validation.** Define-time check
  against `broker.instances_snapshot()` so a typo doesn't silently
  apply with no tools (the auto-injection guard catches the worst case
  but a stricter check earlier would be friendlier).
- **`xml_escape` allocates per attribute.**
  `crates/kaijutsu-kernel/src/llm/system_prompt.rs:140` does four
  chained `.replace()` calls per attribute string, allocating a new
  `String` each. Single-pass char loop would halve allocation pressure
  on the per-prompt path. Cleanup, not a hotspot.
- **`McpToolCall.legacyServer @0` capnp cleanup.** Field renamed to
  `legacyServer` and ignored on both sides; the only thing keeping it
  in the schema is capnp's contiguous-ordinal rule. Drop it (and
  renumber `tool`/`arguments`) in a coordinated wire-schema bump,
  bundled with any other deprecated-field removals so we eat the
  break once.

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

## Vim modal editing (compose overlay)

State machine and core operators ship in `crates/kaijutsu-app/src/input/vim/`.
Open work:

- **Multi-line visual selection.** Charwise (`v`) on a single visual line
  ships now (`OverlayCursorGeometry.selection_*`). When anchor and cursor
  straddle lines, the highlight is suppressed and only the cursor
  indicates position. Compute one rect per affected line (or pass an
  array uniform) for proper multi-line `v` and linewise `V` behavior.
- **Linewise visual mode (`V`).** Above selection rect doesn't extend
  full line width — needs explicit linewise rendering.
- **Block visual mode (`Ctrl+v`).** Column-shaped selection rect.
- **Undo/redo (`u`, Ctrl+R).** Local undo stack with checkpoints on
  Normal→Insert→Normal. DTE has causal history but no linear undo.
- **Text objects** (`iw`, `aw`, `i"`, `a(`, …).
- **Search** (`/`, `?`, `n`, `N`) — needs UI command bar.
- **Repeat (`.`)**, **Registers (`"a`)**, **Marks**, **Macros (`q`/`@`)**.
- **Command mode (`:`)** — useful for `:w`, `:q`, `:s`.
- **Block editing** — extend modal editing to `CellEditor` (conversation
  blocks).
- **Custom kaijutsu bindings on top of `VimBindings`** (Tab, q, …).
- **Replace mode (`R`).** Currently routes through `mode_kind` as Beam;
  vim convention is an underline cursor. Add a `CursorKind::Underline`
  variant once Replace mode lands.
- **Linewise delete for `dj`/`dgg`/`dG`.** These currently route through
  the charwise extend path in `resolve_target_range` (preserved as
  inclusive in `motion::is_inclusive` to avoid a regression). Vim treats
  them as linewise — `dj` deletes 2 lines.

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
- **Role group borders** are still Vello — should be shader-drawn like
  block borders.

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
  summary-style control. (`--distill-model` knob ships now per M5-F5;
  custom distillation prompts are the remaining shape.)
- **Workspace auto-mounts at context join.** How workspace paths
  translate to VFS mounts on join.
- **kj CLI binary.** Standalone `kj` for headless scripting; thin
  adapter over `KjDispatcher`.
- **Distill model selection.** `formation_edges`-style `auto_distill`
  defaults to the source context's model (potentially Opus). Add a
  `distill_model` knob so cheap models can do summarization.


