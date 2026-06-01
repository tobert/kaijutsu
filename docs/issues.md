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

## Context-type tool policy (unified governance)

Goal: one capability policy per context, seeded per `context_type` via
rc, governing **both** tool consumers — the in-kernel LLM loop and an
external MCP agent. They are not two stacks; they are two *drivers* that
converge at the kernel + broker (in-kernel: `broker.call_tool`; external:
`context_shell` → `kj`/kaish). Govern the convergence point, with the
enforcement style each driver needs: **hide** at
`broker.list_visible_tools` for the in-kernel model, **refuse** at the
`kj`/kaish layer for the external agent. No rmcp dynamic `list_tools`
needed; `context_shell` stays always-present.

Shipped (Phase 1): the divergent context-creation path is closed — the
RPC `create_context` handler now runs `run_rc_lifecycle("create", …)`
and honors a wire `contextType`, so app- and MCP-born contexts get rc
like `kj context create` always did. `register_session` defaults new
contexts to the new `mcp` context_type.

Remaining:
- **Capability taxonomy + policy row.** Generalize `ContextToolBinding`
  (today instance-granular only — `mcp/binding.rs`) to a tool-granular
  allow-set spanning broker `instance:tool` ids and `facade:` ids
  (`context_shell`, `shell`, `*_input`). Default permissive.
- **rc-native setter.** Add `kj binding`/`kj policy` to the kj
  dispatcher delegating to the public `broker.bind/unbind/binding`, so
  rc `.kai` scripts can write a context's loadout. (Today bindings are
  reachable only from the `builtin.bindings` MCP tool — backwards vs.
  the kj-is-rich stance.)
- **External enforcement.** `kj`/kaish consults the policy before a
  mutating subcommand and refuses — also the hook a read-only "explorer"
  context_type will hang off.
- **Restricted role bundles.** explorer (read-only), director
  (block/peer tools) — deferred; needs the taxonomy above. Note read vs.
  write tools are *mixed* inside `builtin.file`/`builtin.block`, so
  fine-grained roles need tool-granular policy or instance-splitting.

## LLM providers

- **Move per-model knobs out of the config layer, into the app.** `models.toml`
  is the wrong home for settings that want to be live and model-specific.
  Surfaced while editing `~/.config/kaijutsu/models.toml` (2026-06-01):
  - `max_output_tokens` is a flat per-provider number; it should be tunable
    in-app and keyed to the precise model (different ceilings/reasoning
    budgets per model, cf. the deepseek V4 64K note).
  - `default_model` is likewise a static per-provider field; want in-app
    selection rather than a config edit + kernel restart.
  - Model aliases (`fast`/`smart`/`local`/…) should leave the config layer
    entirely and become an in-app affordance. Overlaps the `kj models`
    alias-resolution item under **kj / control plane** below — pick one home
    for alias truth (registry vs. app) when this lands.

- **Credential file option (alongside `api_key_env`).** Provider config
  reads keys only from an env var (`api_key_env`). Add a file-path option so
  a provider can point at e.g. `~/.deepseek-key` / `~/.anthropic-key.txt`,
  removing the requirement that the kernel process inherit every key in its
  environment. (Active work as of 2026-06-01.)

- **Cross-turn thinking continuity.** `hydrate_from_blocks` skips
  `BlockKind::Thinking` entirely (`llm/mod.rs:1041`), so reasoning
  never re-enters the conversation between separate
  `process_llm_stream` calls. Out of scope per the original A3
  framing; revisit if extended thinking lands and the stale-reasoning
  cost is worth the token spend.

- **Push subscriber for ConversationMailbox.** Slice A wires the
  mailbox in pull mode — `catch_up(&block_snapshots)` reconciles
  against the durable log once per turn. A push subscriber on
  `BlockFlow::Inserted` would let the mailbox stay current between
  turns without re-reading the full block list. Defer until: (a) a
  second async-event source needs the gate (drift, peer tool calls),
  or (b) per-turn `catch_up` shows up as a hot path. See
  `docs/conversation-session.md` and
  `architecture_context_invariants` memory.

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

- **`kj model` / `kj models` subcommand.** No way to discover available
  providers/models or inspect the current context's model from `kj`
  today — callers have to know the exact `provider/model` spec (e.g.
  `deepseek/deepseek-v4-pro`) for `kj fork --model`, and `--model` does
  *not* resolve `models.toml` aliases (bare names resolve provider to the
  default, not via alias). Want `kj models` to list registered providers
  + their models + aliases (from the `LlmRegistry`), and `kj model [ctx]`
  to show/peek the context's resolved provider+model. Reuse
  `LlmRegistry::{list, models_for_provider, resolve_alias}` and the
  DriftRouter handle's `provider`/`model`. Consider letting
  `--model <alias>` resolve through `resolve_alias` so fork/context-set
  accept the friendly names too.
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
- **POSIX context quartet.** The frame for autonomy is four verbs
  mirroring POSIX process control: `fork` (snapshot), `drive` (clock one
  turn / exec), `wait` (join), `drift merge` (return). Shipped: `fork`,
  `drift merge` (defaults to the `forked_from` parent), and `kj drive
  [<ctx>] [--prompt]`, which clocks one autonomous turn via the shared
  `publish_turn_request` helper (`kj/fork.rs`); `kj fork --prompt` is now
  sugar over the same path. The drift-then-drive idiom (`kj drift <c>
  "tools down"; kj drive <c>`) is a graceful steer-don't-kill shutdown —
  the drift lands in the log and the driven turn's mailbox `catch_up`
  folds it in. Remaining: `kj wait` and `kj stop` (below).
- **`kj drive` follow-up.** No staging guard at the verb: driving a
  `Staging` context isn't refused up front — it degrades to a visible
  Error block because `spawn_llm_for_prompt` rejects staging server-side.
  A verb-level refusal (mirroring `request_child_turn`'s staging skip) is
  cleaner. Shares the lossy/in-memory-bus exposure below.
- **`kj stop`.** The interrupt verb — the firm rungs above drift-and-
  drive's gentle steer. `kj stop <ctx>` = soft (finish current tool, no
  next LLM call); `kj stop <ctx> --now` = hard (abort stream + kill kaish
  jobs). The engine exists (`ContextInterruptState::soft()/hard()` in
  `server/src/interrupt.rs`, reachable today only via the
  `interruptContext @75` RPC). Needs the same kernel→server bridge as
  drive: a `TurnFlow::Interrupt { context_id, immediate }` variant + a
  `turn.interrupt` subscriber arm in `spawn_turn_driver` that calls
  `soft()/hard()` on `context_interrupts`. A missed *stop* is worse than
  a missed *drive*, so `kj stop` must report its receiver count and `Err`
  on zero subscribers (like `kj drive`).
- **`kj wait`.** POSIX `wait()` for forks: block the caller until a
  child context drifts back (or exits). `kj fork --prompt` / `kj drive`
  run the child autonomously and the parent keeps going (`turn.requested`
  bus → server turn driver); `kj wait <child>` is the join side — let a
  parent gather results when it explicitly wants them, instead of only
  via async drift-on-next-turn. Same shape as Claude Code subagents.
  Open UX (from design): `--timeout <dur>` bounds the wait; `--poll`
  makes timeout a non-error (exit 0, status "running") so it's loop-
  friendly and idempotent. Default trigger should be the child's
  *drift-return* (semantic "done"), not a single `turn.completed` — this
  also serves a never-exiting persistent explorer that drifts back on
  each drive; `--turn` opts into the low-level one-turn signal. The
  completion substrate now exists: the turn driver publishes
  `TurnFlow::Completed { context_id, principal_id }` on success and
  `TurnFlow::Failed { context_id, principal_id, error }` on failure
  (topics `turn.completed` / `turn.failed`), so the join side finally has
  something to block on. Remaining work: the `kj wait` command itself +
  its blocking semantics + the cross-fork "what does turn-complete
  actually mean" question (one turn vs. a quiescent child that may itself
  have fanned out more `--prompt` children).
- **Autonomous turn runaway guard.** A `--prompt` fork drives one turn;
  if that turn forks-and-seeds again, drives fan out unbounded. No cap
  today (deliberate — wait for a real problem before paying for it).
  Still needs `drive_depth` plumbing: add a `drive_depth` field to
  `TurnFlow::Requested`, thread the incoming depth through the turn
  driver (`rpc.rs` `spawn_turn_driver`) → `ExecContext`/`KjCaller` →
  `request_child_turn` (publishing `depth + 1`), and refuse to drive past
  a cap, mirroring `MAX_RC_DEPTH` in `kj/lifecycle.rs`.
- **TurnFlow bus is lossy + in-memory.** The FlowBus drops the oldest
  events on overflow and holds nothing across a server restart. A dropped
  `turn.requested` = a child that was seeded but never acts. The
  zero-subscriber case now at least writes a visible Error block
  (`request_child_turn`), but an overflow-drop with a live subscriber is
  silent. Revisit with persistence / reconciliation (e.g. a "pending
  turns" table the driver drains on startup) when it bites.
- **Headless turn cwd is `/`.** The turn driver synthesizes its
  `ExecContext` with cwd `/` (`rpc.rs` `spawn_turn_driver`), so a
  `--prompt` child running shell tools won't be in the project dir — even
  though fork copies the parent's shell config (`fork_context_config`).
  Decide whether to thread the context's stored shell cwd
  (`get_context_shell`) into the headless `ExecContext`.
- **`--switch --prompt` double-drives.** With both flags the caller is
  moved into the child (Switch) *and* an autonomous turn fires there
  (`request_child_turn`). The human and the autonomous turn then race in
  the same child. Decide whether `--switch` should suppress the
  autonomous turn (let the human drive) or keep both.

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

- **`I:linebreak` `<none>` / `!` mode selection.** `I:linebreak $`
  now recognizes `$` as a line-break marker (§6.1); the dialect modes
  `<none>` (suppress all auto breaks) and `!` (decoration-collision
  marker) are still no-ops, and `I:linebreak $` does not yet suppress
  source-newline breaks per the spec.

- **`m:` macro expansion.** `m:` macro definitions are captured as
  `InfoField` (header `other_fields`, body `Element::InlineField`) but
  the body parser doesn't expand `~G2` etc. into their definition.
  Spec §9 covers both static macros and the transposing form
  `m:~n2 = ...`.

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

## ABC engraving / kaijutsu-abc layout

Surfaced while debugging 4-part hymn rendering (2026-05-29). The parser
fix for body `V:` attributes shipped (`fix(abc): parse voice attributes
on body V: switches`) and the in-app fit-height cap was raised from 400
to the texture ceiling (`fix(app): raise ABC/SVG fit-height cap …`), so
multi-staff scores no longer get squeezed. The following engraving-quality
items remain. Open score (one staff per voice) is the supported path and
is fine for testing; these are polish + the conventional hymn layout.

- **Linear duration spacing — no measure justification.**
  `duration_to_width` (`crates/kaijutsu-abc/src/engrave/layout.rs:1585`)
  is purely proportional with a 0.25-unit floor: a half note is exactly
  2× a quarter, a whole note 4×. Real engraving compresses this
  (roughly logarithmic / Gould spacing, ~1.3–1.5× per doubling) and
  justifies each measure to a target width. The current output looks
  airy/loose for long-note material and ragged within a bar. Fix is a
  spacing model in the layout pass; the duration→width call is the seam.

- **No system bracket/brace joining staves (open score).** Each voice's
  staff is laid out independently (`layout.rs` `engrave()` loop, ~line
  331) with no left-edge bracket and no barlines drawn through the
  inter-staff gap, so a multi-voice score reads as N detached staves
  rather than one system. Cosmetic; ~half day (bracket glyph at left,
  span barlines vertically across the group).

- **Closed-score (grand-staff) layout + voice-on-staff grouping.**
  Hymns conventionally print S+A on one treble staff (S stems up, A
  down) and T+B on one bass staff — two staves, not four. Deferred;
  not a current use case (hymns are test material, open score is fine).
  Difficulty breakdown if revisited:
  1. Parse `%%score (S A) (T B)` into a staff-grouping structure
     (prereq; see the `%%` stylesheet-directives item above — `%%score`
     is currently skipped). ~2h.
  2. Render N voices onto one staff sharing lines/cursor/barlines —
     the duration-proportional x-grid already aligns beats across
     voices. ~half day.
  3. Per-voice forced stem direction (voice 0 up, voice 1 down). ~2h.
  4. **Collision handling** (notehead/accidental/rest displacement and
     merging when two voices coincide). This is the hard, open-ended
     part — 80/20 version (offset unisons/seconds, stack accidentals)
     ~1 day; "always correct" Gould-quality is a long tail.
  5. Brace + spanning barlines (overlaps the open-score bracket item). ~3h.
  Pieces 1–3 + 5 (~1.5 days) give structurally-correct closed score that
  reads fine for non-colliding voices; piece 4 is the unbounded cost.


