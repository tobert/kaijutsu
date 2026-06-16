# Open Issues

Live work items distilled from prior design and TODO docs, plus architectural observations from code reviews. Code is truth; this exists to track what's *not* in the code yet.

Organized by area. Keep entries terse — link to file:line when a pointer makes the work concrete. When an item ships, delete the entry.

---

## Architecture & System Design

- **VFS facade delegation:** `Kernel` implements `VfsOps` directly (`crates/kaijutsu-kernel/src/kernel.rs:984`) as a facade. Backend multiplexing already exists — `MountTable` impls `VfsOps` over `MemoryBackend`/`LocalBackend` (`crates/kaijutsu-kernel/src/vfs/mount.rs:261`). The open question is whether the `Kernel`-level facade should delegate more to `MountTable` (and what stays on `Kernel`), not whether to build a manager from scratch.
- **Server RPC Modularization:** `crates/kaijutsu-server/src/rpc.rs` is a massive file (~301KB / ~7,000 lines — by far the largest in the server). The monolithic implementation of the Cap'n Proto traits should be split into smaller modules by domain (e.g., `rpc/vfs.rs`, `rpc/llm.rs`, `rpc/mcp.rs`).
- **Cap'n Proto Schema Clarity (doc-only):** The `BlockKind` vs `ContentType` boundary is already settled — `BlockKind` is the structural DAG role, `ContentType` is the raw MIME rendering hint. Remaining work is purely to write that distinction into `kaijutsu.capnp` as schema comments so it stops reading as overlap.
- **Context-type tool policy (unified governance):** The `kj` surface is now
  capability-gated — escalation-relevant verbs check the caller's loadout via
  `KjDispatcher::require_cap` (five authority caps: `drive`/`fork`/`drift`/
  `transport`/`operator`, plus reuse of `rc-write` and the `builtin.block`/
  `builtin.policy` tool caps). `kj` was previously an ungated hole behind
  `facade:shell`. Remaining:
  - Dynamic / principal-scoped overrides.
  - Self-lockout ergonomics (narrowing binding to exclude `builtin.bindings`).
  - Per-principal budgets + fair queuing.
  - **Live contexts need re-create/restart:** broadened role loadouts only reach
    newly-created contexts; existing ones keep their old (now authority-less)
    binding until they're re-created or the kernel restarts. (Editing the seed
    via `kj rc edit` / `kj rc reset` changes what *new* contexts get, not live
    ones — rc fires at lifecycle boundaries, not retroactively.)
- **RPC session reaping — mostly closed (2026-06-14).** The original report
  ("warned every 60s for 21+ hours that session `019eb229` is still active —
  no reap path") was two conflated problems, both now addressed:
  - *Reaping* is handled at the transport layer by SSH **keepalive**
    (`ssh.rs` server `Config`, added 2026-05-24: 30s × 3 ≈ 90s dead-peer
    detection — postdates the 21h zombie). A vanished peer's transport now
    EOFs, `rpc_system.await` returns, and `ConnectionState::Drop` removes the
    `session_contexts` entry. Verified on the live server (booted 2026-06-13):
    every session that was ever warned about *ended cleanly*, including ones
    open for hours; no surviving zombie.
  - *The watchdog was a false alarm.* `run_rpc_watchdog` logged `WARN ...
    still active` every 60s for the entire life of **any** session, so a
    healthy hour-long connection emitted ~58 lines — it could not tell a
    long-lived session from a wedge, burying the one signal it existed to
    surface. Now **activity-gated** (`ssh.rs`): an `ActivityStream` wrapper
    stamps a timestamp on every byte read/written, and the watchdog warns only
    when a connection is open but idle past `RPC_IDLE_WARN_THRESHOLD` (120s,
    above the keepalive reap window).
  - **Residual (by design, low):** a *truly* wedged `current_thread` LocalSet
    (a blocking handler) can't be force-killed from outside without thread
    injection, and the in-thread watchdog goes quiet with it — that silence is
    the only remaining signal. Not worth chasing until it actually recurs.
  - Related: auto-memory `tech_debt_peer_reattach_on_reconnect` (app doesn't
    re-attach after kernel restart). Original find: 2026-06-11 journaling
    forensics.
- **LLM providers:**
  - Move per-model knobs out of the config layer (`models.toml`), into the app.
  - Push subscriber for `ConversationMailbox`.
- **Reasoning-continuity cross-provider guard (policy, not Rust):** cross-turn
  thinking now rehydrates — `BlockSnapshot.signature` is an opaque "rehydratable"
  token (real Anthropic/Gemini sig, or a DeepSeek nonce), set on `ThinkingEnd`,
  persisted (CRDT snapshot + Cap'n Proto wire), and `hydrate` re-emits *signed*
  Thinking as `Reasoning` (one block per thinking block). Remaining: block
  `kj context set --model` across provider families when signed Thinking exists
  in history (a DeepSeek nonce fed to Anthropic 400s); allow the transition only
  at `fork`, where an rc script decides to elide thinking or downgrade it to
  plain blocks.
- **Cold start seeds no binding-admin context (want a ROOT director).** The
  bootstrap (`kaijutsu-server/src/rpc.rs:1369`) seeds exactly one **`coder`**
  context (`genesis`) when the kernel comes up with zero contexts — nothing with
  `admin`/`rc-write`. Consequence: any binding-admin op (e.g. repairing a live
  context whose loadout came from a stale seed — see the stale-rc entry under
  Control Plane — or running `kj rc reseed`, which needs `rc-write`) requires
  manually `kj context create <x> --type director` first, since only rc-privileged
  callers or an `admin`-capped context can widen another's loadout, and no
  user-facing shell is rc-privileged. Want: a fresh kernel seeds a **ROOT
  director** (the `director` type already grants `admin`+`rc-write`). Design
  wrinkle: a `director` loadout has **no `drive`/`fork` authority**, so ROOT can't
  itself be the conversational default the app opens into — either seed *both*
  (ROOT director + a usable coder), or have ROOT spawn the coder and let the app
  default to the coder. Confirmed not implemented as of 2026-06-13; genesis was
  repaired by hand this session via a throwaway director.

## Drift — June 2026 audit

- **Extract `ContextRegistry` from `DriftRouter`:** DriftRouter carries ~7
  responsibilities (context registry, per-context LLM config, staging
  queue, dead-letter queue, lost+found lifecycle, context state, trace-ID
  assignment) — `drift.rs:172-563`. Everything that needs "what contexts
  exist" takes a dependency on drift, inverting the hierarchy. Pull
  register/resolve/list/llm-config/trace-id into a `ContextRegistry`;
  drift keeps the queues. Cold-start hydration (`rpc.rs:1150-1183`) moves
  with the registry. (Considered 2026-06-13; deferred — it's a cohesive
  multi-file extraction touching drift.rs + rpc.rs + every "what contexts
  exist" caller, best done when the kernel isn't under concurrent edit.)
- **`drift_flush` is non-atomic over the router lock:** takes the write
  lock four separate times (`kj/drift.rs:422`, `:510`, `:516`, `:521`),
  allowing interleaving with concurrent stage/cancel between windows.
  Document why that's safe or restructure drain→requeue as one critical
  section. (The suspected lock-across-await is NOT real — db lock at
  `:455-471` drops before the `:487` await.)
- **`kj/drift.rs` orchestration bloat:** push/pull/merge/flush each inline
  variations of "insert drift block + record edge + run rc lifecycle".
  Extract the shared operation; the command layer should dispatch, not
  orchestrate.
- **Drift distillation half-integrated:** `build_distillation_prompt`
  machinery sits behind a "drift engines removed" comment + TODO
  (`drift.rs:602-665`). Decide: integrate or delete.

## Turn Loop (kaijutsu-server/src/llm_stream.rs) — June 2026 audit

- **Decompose the agentic loop** (after FlowBus settles; they share event
  paths): mailbox catch-up/snapshot (`:341-391`), cache-breakpoint policy
  via ad-hoc DB reads (`:500-511`), one-shot image resolution that goes
  stale across tool iterations (`:403`), dual-layer timeout semantics
  (`:603-634`) are all inlined in one ~1,235-line file.

## Cleanup — June 2026 audit

- **App-side ABC parse failure renders `Tune::default()` silently**
  (`kaijutsu-app/src/text/rich.rs:413-423`) — render the kernel's
  structured ABC error spans instead. Also: the app re-parses ABC on every
  view; consider a cached AST keyed on block content version.

## Persistence & Sync

- **Graceful-shutdown WAL checkpoint on SIGTERM:** `SharedKernelState::drop`
  checkpoints only on clean exit, but the server `run()` loop never returns and
  dies on SIGKILL/SIGTERM without unwinding, so systemd `stop` skips it.
  Proactive compaction checkpoints cover durability (no data loss); this gap
  only affects bare-file forensics between the last compaction and shutdown.
  Fix: a `tokio::signal` SIGTERM handler that checkpoints before exit (needs the
  run loop to become interruptible). Forensics hygiene: tracing logs UTC,
  systemd speaks local — cite both zones when recording restart times.
- **`KernelDb` connection pool:** Currently `Arc<parking_lot::Mutex<KernelDb>>` (`block_store.rs:74`). This bottleneck prevents utilizing SQLite's WAL mode for concurrent readers. Migrate to `r2d2` or `sqlx` to allow non-blocking reads during LLM streams and heavy writes. Note: SQLite serializes *writes* regardless of pooling, so the win is concurrent reads (and only with WAL enabled) — verify WAL before assuming a pool helps; narrowing lock scope may matter as much.
- **Config CRDT ops:** Config backend needs DTE integration so changes replicate across peers.
- **`blocks_ordered()` allocation churn + sort:** `block_store.rs:185-188` calls `order_key().to_string()` for *every block*, then `sort_by` on the strings — so it's O(N log N) **plus a String allocation per block per call**. It runs on per-frame hot paths (`kaijutsu-app/src/ui/card_stack/sync.rs:48`, `view/components.rs:163`), so the allocation churn is likely the bigger cost than the asymptotics. Fixes: compare `order_key` without stringifying, and/or cache the ordering and invalidate on block change. Add a secondary sorted index when scale demands.
- **Latch state should persist with the context:** 
  - `set -o latch` mode is per-shell and lost on restart.
  - Latch nonces should eventually live in a SQLite table rather than in-memory.

## User Interface (kaijutsu-app) & UX

- **Rename `BlockScene` → `BlockContent`:** the component no longer holds a
  scene (scene + `built_*` live on `VelloUiScene`); it's now pure build-
  bookkeeping (`content_version`/`last_built_version`/`scene_version`/`text`/
  `color`). Name is misleading. Mechanical rename across `block_render.rs`,
  `lifecycle.rs`, `overlay.rs`, `shell_dock.rs`, `render.rs`.
- **Verify two unexercised render surfaces:** (1) a Vello-content *cell*
  (ABC/SVG/sparkline, `has_vello_content == true`) rasterizing via
  `render_vello_scenes` then compositing MSDF labels on top — needs a
  conversation with rich content; (2) the unfocused-pane summary, the one
  surface on Bevy's native `Text` pipeline (`tiling_reconciler`), needs a
  multi-pane layout. All MSDF-only surfaces + docks + role borders verified.
- **User presence (novel surface):** The compose input is a shared CRDT document. Surfacing in-flight compose state to an opted-in model would enable mid-sentence collaboration. Gate with explicit user opt-in.
- **Connection Polling Efficiency:** `ActorPlugin` in `crates/kaijutsu-app/src/connection/mod.rs` polls broadcast channels every frame. While `UpdateMode::reactive` helps, consider event-driven wakeups or bridging async streams directly into Bevy events more efficiently if latency/power becomes an issue.
- **Card-stack view:** Card size tuning, read-only scroll on focused card, dive-in (Enter), mouse click to focus, momentum scrolling, camera parallax, streaming card texture updates, card grouping evolution, ambient environment.
- **Card-stack texture quality (3D direction):** the renderer presents vello/MSDF
  content as textures on cards, so the 3D move brings (a) **mipmaps** on block/card
  textures — cards receding in perspective shimmer without them; (b) **reading-mode
  hi-res re-render** — promoting a card close to the camera re-renders its content at
  higher resolution (discrete, debounced — same machinery as re-render-on-change);
  (c) **MSDF live-quad escape hatch** — MSDF's scale-independence is spent at bake
  time, so if reading-mode text quality disappoints, render MSDF as live quads in the
  3D scene (the atlas + shaping pipeline already support it; a renderer change, not
  architectural). Arbitrary zoom over vector content is explicitly declined.
- **Text rendering (MSDF / 次):** TAA temporal super-resolution, glyph spacing per-font tuning, 1-frame blank flash on texture resize, large-context Vello "paint too large" crash.
- **Auto-follow on local submit:** the conversation only re-engages
  scroll-follow when already at the bottom
  (`view/sync.rs:200-206`); a shell-dock submit is a strong signal of
  intent to watch the result — force `start_following()` on local
  submits (mirror the `InputCleared` handler at `sync.rs:309`). A
  "new content below" affordance would cover non-local appends.
- **Stale GpuImage-preparation comments:** "ImageNode ensures the
  GpuImage is prepared" (`view/lifecycle.rs:258`,
  `view/block_render.rs:877-878`) is not how Bevy 0.18 works — GpuImage
  prep is `AssetEvent`-driven with an inherent one-frame delay (the
  benign single "MSDF render skipped … target_gpu=false" warn per cell).
  Correct the comments so the next renderer investigation doesn't chase
  the wrong layer.

## Control Plane & Navigation (kj)

- **Workspace path mount points:** `kj workspace add --mount <target>` was
  documented + parsed but silently ignored (no backing storage) — removed during
  the clap migration so it now fails loud. To implement: add a `mount` column to
  `WorkspacePathRow` (`kernel_db.rs:168`, SQL migration), thread it through
  `workspace_add` and the context-mounting path, decide mount semantics, then
  re-add the `--mount` flag + help example.
- **Tab completion:** Context labels, preset labels, workspace labels, tag syntax. Integrate with kaish.
- **Cross-kernel drift:** Schema preserves `kernel_id` everywhere; not yet implemented.
- **Compact quality:** Distill model selection, preset-level or context-level summary-style control.
- **POSIX context quartet:** Implement `kj wait` and `kj stop` to complete the fork/drive/wait/merge paradigm.
- **`kj drive` follow-up:** Add verb-level refusal for driving Staging contexts.
- **Autonomous turn runaway guard:** Add a `drive_depth` cap to prevent unbounded fan-out from `--prompt` forks.
- **TurnFlow bus lossy + in-memory:** overflow eviction is now LOUD (`FlowBus::publish` warns when a full channel drops a slow subscriber's oldest event, `flows.rs`); the zero-subscriber case was already surfaced by `kj drive`/`kj fork --prompt`. Durable delivery (persistence) for `turn.*` remains the follow-up.
- **Headless turn cwd is `/`:** Decide whether to thread the context's stored shell cwd into the headless `ExecContext`.
- **`--switch --prompt` double-drives:** Clarify semantics when both human and autonomous turn try to drive a child.

### kj / MCP ergonomics (UX)

- **MIDI / blob readback is a two-step with no single tool.** A derived block
  (e.g. ABC→MIDI sibling) stores a 32-hex CAS hash as its `content`, and the
  block is `ephemeral` (won't hydrate into the conversation). Retrieving the
  bytes is: enumerate blocks (`kj block` / `kaijutsu://docs/{doc}`) → find the
  `audio/midi` sibling → read its hash → `kj cas get <hash> --out <file>`. There
  is no MCP/`kj` "give me the rendered artifact for this turn" affordance. Add a
  `kj block cat`/blob-by-block helper (and/or an MCP resource that resolves a
  block's CAS content) so consumers don't hand-assemble the hash lookup.
- **Stale rc seed → missing authorities (per-file upgrade is now `kj rc reset`).**
  rc scripts live as host files under `~/.config/kaijutsu/rc/<type>/<verb>/`; the
  deployed tree is the live source of truth and boot only bootstraps it when
  fresh (2026-06-13 model change), so a pre-existing seed file never auto-upgrades
  to a newer embedded default. Symptom (2026-06-13): a fresh `mcp` context had the
  old 125-byte binding (`*` + `facade:*` only), missing the
  `drive`/`fork`/`drift`/`transport`/`operator` authorities the current embedded
  binding grants — so it could not run `kj transport` or self-widen (`allow` needs
  a binding-admin/rc context). **Targeted fix: `kj rc reset
  /etc/rc/mcp/create/S10-binding.kai`** (restore that file from its embedded
  seed). Remaining gap: nothing *detects* a live seed has drifted behind the
  embedded default — `reset` is a manual pull, by design (live is truth). A
  staleness indicator (compare live body vs `seed_body()`, e.g. in `kj rc list`)
  would surface "this file is behind its seed" without reintroducing auto-overwrite.
  Recurred for `coder`/`genesis` 2026-06-13 (same 2-line seed). The live-context
  half is worse than the seed half: `reset`/`reseed` only fix *future* contexts —
  a context already created from a stale seed keeps its broken loadout and can only
  be repaired from a binding-admin context, which the cold-start bootstrap doesn't
  provide (see "Cold start seeds no binding-admin context" under Architecture).
- **`local` is a kaish reserved word (like `set`).** `--model local` lexes as
  the `local` builtin keyword → `found ';' expected identifier`. Same class as
  the `set` reserved-word gotcha; quote it (`--model "local"`) or pass the full
  spec. Consider letting reserved words bind as plain args after a flag.
  (kaish-lexer change in `~/src/kaish`, not kaijutsu-side.) NOTE: alias
  *resolution* is now fixed — `kj context create/set --model "local"` expands
  the `models.toml [model_aliases]` entry to its concrete `provider/model`
  before storage (`resolve_context_config`, 2026-06-14), so the quoted form
  works end-to-end; only the bare-`local` lexer footgun remains.
- **Local-model turn HANGS silently when given tools.** A small local model
  (Gemma-4-E4B via lemonade) handed the full tool palette
  (`[providers.lemonade.default_tools] type = "all"`) emits a thinking block and
  then stalls — GPU goes cold, no `Completed`, no error, turn never terminates
  (observed 2026-06-13). Lemonade itself is fine (direct stream completes in
  <1s); the hang is in the new local-model + tool-call turn path. Counter to the
  fail-loud stance — a stuck turn should time out / surface an error. Also: the
  player loadout should not be `all` tools for a small model (the composer rc is
  now tool-free); make per-provider/per-context `default_tools` the norm for
  players, and add a turn-level watchdog so a wedged tool loop fails loudly.
- **P3 — external `mcp__kaijutsu__shell` `data` needs a persisted block field.**
  The *in-kernel* `builtin.shell` now carries kj's `.data` in its `structured`
  envelope (shipped 2026-06-14, `mcp/servers/shell.rs`), and `kj <cmd> --json`
  returns the payload in stdout for any consumer. The remaining gap is the
  *external* `mcp__kaijutsu__shell`, which observes the result via CRDT sync
  (polls a block snapshot, reads `snapshot.output`) rather than a return value.
  Root cause (traced 2026-06-14): kj sets `ExecResult.data` (a kaish `Value`),
  but the server's `shell_execute` only persists `ExecResult.output()`
  (`OutputData`) onto the block (`rpc.rs:6104` → `set_output`), and the block
  carries only `output: OutputData` — which can't faithfully hold arbitrary JSON
  (an inspect object). Faithful fix: a new persisted `data` field on the block,
  mirroring the `.output` vs `.data` split — thread through `kaijutsu-types`
  `BlockSnapshot`, `kaijutsu-crdt` (content/document/block_store), the capnp
  `BlockSnapshot` wire (the real cost — three-binary bounce), then `set_data` in
  `shell_execute` and read it in the MCP `to_json` (`kaijutsu-mcp/src/lib.rs`).
  CBOR oplog evolution is additive (safe); capnp is the work. P3 because the
  `--json` envelope already unblocks consumers today.

- **`StreamingBlockHandle` implementation:** Single-block streaming primitive.
- **LLM streaming rewrite:** Move `process_llm_stream` onto `StreamingBlockHandle`.
- **Block content abstraction:** Blocks as containers for multiple content artifacts.
- **MCP `progress` → `StreamingBlockHandle` bridge.**

## Domain-Specific (ABC Parser & Engraving, Index)

- **`hnsw_rs` reverse-edge quirk:** Reverse edges written at neighbour's assigned layer.
- **ABC multi-tune files vs blocks:** Split tunes across sibling blocks or stack inside one block.
- **ABC file-header inheritance:** `M:`/`L:`/`Q:` defaults prevent proper inheritance.
- **ABC features:** `I:linebreak`, `m:` macro expansion, `%%` directives, Unicode escapes/fonts.

## Viz substrate (kaijutsu-viz) — see docs/viz-substrate.md

- **Time-well step-4 polish (shipped 2026-06-16, `view/time_well/`):**
  - *Fixed-pitch overlap:* band slots use a fixed angular pitch (TAU/24) so
    append stays motion-free; but a band with >24 cards wraps slots onto each
    other (coincident cards → z-fight/draw-swap; `AlphaMode::Mask` mitigates the
    sort but not coincidence). Real fix for very full bands: sub-rings, smaller
    cards, or radius LOD. Band 0 is meant for ~10 so this only bites test data.
  - *Status coverage:* the data tick reflects only already-subscribed contexts;
    a `subscribeBlocksFiltered` over the full visible set so every rim card
    pulses is the follow-up (gap 3).
  - *Readability:* card sizing/camera zoom is functional but text is small at
    the default framing; tune when the active view (step 6) lands.
- **`ScaleLinear`/`ScaleTime` round-trip loses precision under extreme
  domain→range compression** (≳10³–10⁸×): inverting through a tiny range
  amplifies f64 representation error past any sane tolerance. This is an f64
  limitation, not a logic bug — the `invert` algebra is exact. The proptest
  strategy constrains the compression ratio to a realistic band (`rwidth_factor`
  ∈ [0.1, 10]) so the property isn't flaky; the well's actual domains (time, band
  fractions) never approach the pathological ratio. Follow-up if it ever bites: a
  one-line doc note on `ScaleLinear` about the compression boundary (parallel to
  the existing 2³ ms note on `ScaleTime`). Discovered during the scales spike
  (deepseek review N3), 2026-06-15.
- **ABC duration-summing ruler:** kaijutsu-abc has no total-beats-per-voice
  machinery; needed to validate that a committed phrase's ABC sums to
  `beats_per_phrase` (Chameleon eval ruler, new code). The tuplet/broken-rhythm
  handling in `midi.rs:261-274` is the acceptance spec.
- **ABC layout:** Linear duration spacing (needs Gould spacing/justification), system bracket/brace, closed-score layout.

## Hyoushigi / Composer

- **Composer `kj` loadout — tool-free (2026-06-13).** `composer` seeds
  `assets/defaults/rc/composer/create/S10-binding.kai` granting only `drive`:
  no `builtin.*` tool instances, no `facade:shell`/`submit_input`, no
  `fork`/`drift`/`transport`/`operator`. A player is an ABC-only voice — its
  turn text *is* the score (`on_turn_completed` eager-parses it), so it needs no
  tools, and a small local model handed the full palette stalls the turn. The
  generic ABC-output primer rides the system slot (`create/S15-abc-primer.md`);
  the gig (key/tune/register) belongs to the stance + producer chart, NOT the
  base rc — migrate any song-specific primer content to the producer/chart
  layer when it lands ("big models author vocabularies").
- **Decouple the OODA Act from ABC (generalize the loop primitive).** The Act
  path is hardwired to one notation: `on_turn_completed` → `schedule_abc_cell`
  eager-*parses ABC* to validate, and the `DeriverRegistry` derives MIDI from
  it. The loop *shape* — drive → validate turn output → crystallize a cell →
  derive sibling artifacts — is general and would serve other loops: a
  MIDI-native model (emits MIDI directly, no ABC), non-music content, or any
  "model produces structured artifact on a beat" workflow. Generalize to a
  content-type-keyed `schedule_cell(content, content_type)` where validation is
  pluggable (the player's track/role declares its expected content type) and
  derivation stays the already-content-type-keyed `DeriverRegistry`. Then the
  malformed-quarantine (just shipped, beat.rs:850 `set_excluded`) and the
  header-carry follow-up below both become per-content-type validator behavior,
  not ABC special cases. Keep ABC as the first registered validator/deriver.
- **Header-carry for headerless player output (robustness).** A windowed player
  naturally emits a bare continuation body (no `X:`/`K:` header) once it has a
  full tune in its context; the schedule-time validator then rejects it. Today
  we lean on the tick prompt to demand a complete tune every turn — brittle for
  small models. Robust fix: in the score scheduler, if the output is a bare body
  for a track with a last-good tune, prepend that track's last-good header
  before validating/deriving. Pairs with the decouple above (a per-content-type
  "complete the fragment" step).
- **Composers are not re-armed on kernel cold-start (fail-silent).** Auto-arm
  fires only on context *create* (`create_context_inner` / kj `context_create`,
  via `BeatCommand::Arm`); the beat scheduler starts with an empty `armed` map
  on restart and nothing re-arms existing composer contexts from the DB. So a
  kernel restart silently stops every composer's beat until it is re-created —
  and there is no `kj transport arm` verb to recover (only play/pause/stop/
  tempo/ooda/rotate, all no-ops on an un-armed context). Re-arm live composers
  on cold start (scan `context_type = composer`, `Arm` each, seeding the
  playhead from max committed tick as the create path does). Adjacent to
  `tech_debt_peer_reattach_on_reconnect` (restart-recovery gaps).
- **Cadence/tempo should be settable per context:** `kj transport tempo <bpm>`
  exists, but the OODA cadence (`ooda_every`, default 8 phrases = 128 beats) is
  fixed in `BeatPolicy::composer_default()`. Make the cadence a settable knob
  (rc-declared and/or a `kj transport` arg), persisted per context. Fine to do
  later. (Until then, a high BPM via `kj transport tempo` shrinks the wall-clock
  per OODA turn for testing — 128 beats @ 1000 BPM ≈ 7.7 s.)
- **`kj transport meter` inbound verb (Chameleon batch 1, F2):** add
  `kj transport meter <beats_per_phrase>` with a `--bars N --beats-per-bar M`
  convenience that multiplies to beats *at the edge* → new
  `BeatCommand::SetMeter`. Home is `kj/transport.rs`, and it gets the first
  bars→beats translation test (the kernel only ever sees beats; bars live in the
  human-facing arg). Pairs with the cadence-knob item above.
- **`ooda_every` stays beat-denominated (Chameleon batch 1, F2):** the OODA
  cadence field is kept in beats even though its default is *expressed* in
  phrases (`8 * 16`); a phrase-typed `ooda_every` is deliberately deferred —
  revisit once irregular phrases (per-phrase beat counts) make the beat
  denomination awkward.
- **Transport surface beyond `kj`:** app transport buttons / spacebar + a capnp
  transport surface (today `kj transport play|pause|stop|tempo|ooda` only).
- **Re-arm-on-restart sweep unwired:** a kernel restart resets composers to
  stopped and does *not* re-arm them — the only `BeatCommand::Arm` sender is
  `createContext` (`rpc.rs`). The seeding half is **done** (Chameleon batch 1, F1):
  `arm` now reads `max_tick(ctx)` and seeds the playhead inside `arm_timeline`'s
  `or_insert_with`, virgin-only (a non-virgin `seed_playhead` is `Err`), so re-arm
  is safe whenever wired. Remaining: an actual restart sweep that re-arms persisted
  composers. (No archive RPC yet → disarm-on-archive also TODO.) **This is one
  work item with `BeatPolicy` persistence (Chameleon batch 1, F2):** policy
  (`beats_per_phrase`, `ooda_every`, period) and `beat_count` all evaporate on
  restart, but persisting them alone is useless because nothing re-arms contexts
  post-restart at all — the sweep and the persistence land together or not at all.
- **App track chip + "transport" label for beat():** author chips show the
  player's principal on played phrases and `beat()`'s on transport fallback
  repeats — truthful but mildly noisy. Add a track chip (the lane identity) and a
  "transport" label for `beat()`-authored fallback repeats so a vamp insurance
  repeat reads as the transport, not a mystery principal.
- **`$HEARD` shipped as a JSON push; array + pull are follow-ups (Chameleon
  batch 2, 2026-06-11):** `$HEARD` ships as a pragmatic **JSON-string push** —
  `beat.rs::heard_json` reads committed notation in the last
  `HEARD_WINDOW_PHRASES` (block-log tick-window, `ContentType::Abc` only, all
  tracks) and seeds it as a JSON array string. Load-bearing **even solo**: score
  blocks are `ephemeral` (hydration-silent), so this is the only way a player
  sees its own prior phrases. **Two follow-ups (TODOs on the code), when the
  kaish arrays/hashes plan lands:** (1) expose `$HEARD` as a real kaish **array
  of hashes** (indexable, `for phrase in $HEARD`) instead of a JSON string the
  script can't index; (2) re-shape **push → pull** — a `kj`-reachable windowed
  read so the script chooses depth/track rather than a fixed injected window
  (shares the read with the RC hydration-marker archive verb). Also open:
  per-context window tuning (`HEARD_WINDOW_PHRASES` is a const). `content_before`
  in `ResolverCtx` stays deliberately track-blind regardless (no resolver reads
  it; `CasCommitResolver` reads CAS by hash).
- **Player spawn = thin fork + rc-rebuilds (hydration marker SHIPPED + review-
  hardened 2026-06-12; design LOCKED 2026-06-12).** Resolves
  "fork drops the hydration policy" — see the "Players are rc programs" decision
  in `docs/chameleon.md`. A player is spawned by a `spawn`-preset fork
  (`kj fork --preset spawn` per `docs/fork-filters.md`; formerly `--shallow`)
  that keeps ~nothing; the child's `composer/fork/` rc re-establishes setup and
  re-runs `kj context hydrate --window N` (mirror of `create`, marker defaults
  to the child's tail). Because the child is thin, re-anchoring at the tail is
  cheap and correct — which is *why* we dropped the alternative (copy the row /
  preserve `P_parent` via a new `KJ_PARENT_HYDRATION_MARKER` read surface): a
  thin child makes the naive re-anchor right, so the read surface isn't needed
  for fork. (We considered it; the thin fork dissolved the need.) What this
  needs, sequenced:
  - **Lock now (small):** `composer/fork/S30-hydrate.kai` (rebuild + re-mark)
    and confirm a composer fork is thin. `kj transport ooda on|off --context`
    already exists, so transport-follow (arm child / disarm parent) is pure rc.
  - **Rotate action rc (unwritten):** the scheduler-side detach-at-horizon
    trigger is built — `BeatCommand::SetRotate{ctx, every_phrases}` +
    `kj transport rotate --every N | off`; at a phrase horizon (`phrase % N == 0`)
    `fire_due` `stop`s the parent synchronously (no further ticks) and fires the
    `rotate` rc lifecycle. Still unwritten: the rotate ACTION itself, a
    `composer/rotate/*.kai` that forks `--preset spawn` + arms the child. Race-free
    when it lands (the parent is already stopped). (The ordering race that forced
    the trigger into Rust rather than pure rc is closed by this synchronous stop.)
  - **Build when convenient — the windowed-notation pull primitive.** No
    cross-context block-copy verb exists today; a player carrying recent
    notation into its thin-forked child needs one. This is the *same* windowed
    read as `$HEARD`'s push→pull follow-up and the marker-archive read — **one
    read, three consumers** (`$HEARD` indexable array, fork-carry, marker
    archive). Strong signal it's the right primitive; keeps the carry in rc.
  - **Defer — horizon self-fork-rotate (page-turns / song sections).** The
    player self-`kj fork --preset spawn`s on a phrase horizon; fork-lineage becomes
    song form. Two trigger forms: **(a)** a `composer/tick/SXX-rotate.kai` that
    fires every tick (`$PHRASE` is seeded) and acts only at the horizon
    (`phrase mod N == 0`) — **NOT zero new machinery after all** (see the verified
    ordering race above: the rc disarm is async, so pure-rc rotation leaks stray
    parent ticks); the horizon trigger must be scheduler-side Rust
    (`RotateOnPhrase`), with `composer/fork/` doing only the rebuild; **(b) later**
    a declarative "fire script at tick T" timeline scheduler riding
    `schedule_abc_cell`'s rails, worth building once the producer schedules more
    than rotates (section/tempo/dynamics events — clear second consumers). Not
    needed for solo-bass slice 1 (the marker bounds cost; thin-fork-at-spawn gives
    the lean start).
  - **Marker-advance on durable revision** still pairs here: when the producer
    writes revision blocks, re-run `kj context hydrate` to advance P. Pure rc
    once the producer exists.

- **Fork primitives — full/thin mental model (Amy, 2026-06-12).** Full fork
  (regular `kj fork`) is the *powerful* path: take the whole context into a fresh
  lineage = a **new KV cache** (resume-a-session-as-another-model, orchestrator
  repair, drift-a-summary-back). Thin fork is *reuse/reduce*: save tokens for a
  long-running iterating player (the `window`/`spawn` factory presets per
  `docs/fork-filters.md`). Copy cost is a non-issue (storage cheap); the axis is
  KV-cache strategy. Remaining open primitives:
  - **Retire the `max_blocks` fork field (slice 4):** `fork_filtered` now builds
    its positional universe in document (`order_key`) order, so `max_blocks`
    indexes the timeline correctly in the interim (test
    `fork_filtered_max_blocks_keeps_most_recent_by_timeline`), but the field is
    only deprecated, not removed. Fold `--depth N` into the selection engine as
    `--include end-N:` over the `block_ids_ordered()` snapshot and delete the
    field. (BlockId order is `(context, principal, seq)` — principal-major; it
    only coincides with timeline order for a single principal, so a multi-principal
    `max_blocks` over raw BTreeMap iteration was the original bug.)
  - **A snapshot/savepoint marker verb (speculative, not-now — direction set
    2026-06-12).** Absorbed by the fork-filters range grammar as a future
    **label endpoint** (`docs/fork-filters.md`): a savepoint is a colon-free
    name on a block, usable as a range endpoint (`kj fork --include 0:bridge`)
    — no new fork machinery, no verb semantics of its own. Still not-now;
    build labels when the orchestrator work or the time-well wants named
    points.
  - **Presets as a deep kaijutsu concept (design thread, 2026-06-12).**
    Preset = a named **ensemble of argument values**, not a behavior — the
    audio patch-recall model (hit "e-piano", every knob moves, same synth).
    Extends the existing model/prompt preset table (normalized `preset_args`
    child table, verb-scoped from day one) to carry fork filters; a `player`
    patch can move filter + model knobs in one recall. Recall-then-tweak:
    scalars override, filters compose under the include invariant; recall is
    a snapshot (horizon-latched, like rc scripts). Fork is the only wired
    verb for now — generalizing to other verbs (discovery, user banks,
    sharing) deserves its own design session.

  **Remaining follow-ups (deferred — from the same review):**
  - **P1 ×2 — absorbed into the shared SEAM MODULE (re-prioritized
    2026-06-12: FIRST in the fork-filters build order).** The tool-pair /
    turn-boundary tail snap (orphan `tool_result` silently dropped by the
    snapshot repair; a marker on a `tool_call` injects a synthetic
    "interrupted" result every turn forever) and the missing archive seam
    (prefix+tail concatenate with no "[N blocks archived]" signal; cross-gap
    `Model/Text` fragments can merge into false continuity) were "latent
    until composer gets tools" as hydration bugs — but fork-filters' hand-cut
    ranges make both reachable immediately. One first-class module owns every
    keep-set cut edge: turn-boundary snapping (never start an interval on
    `ToolResult`/`Model`-continuation), synthetic user-role seam injection
    (after the prefix, cache-stable), tool-pair integrity. Consumers:
    `rehydrate_windowed`, fork selection, the pull primitive. Contract in
    `docs/fork-filters.md`.
  - **`window` counts RAW blocks, not turns/phrases** (~2-3 blocks per OODA turn,
    and composer score/Trace blocks are hydration-silent so the *visible* tail is
    smaller still) — revisit if a phrase/turn-denominated window reads cleaner.
  - **Cache-breakpoint ↔ window interaction** — the composer's S20 cache
    breakpoints sit at message indices that windowing shifts; harmless for the
    local bass (no prompt cache; composer sets no breakpoints today so the
    byte-stable prefix is inert), reconcile when API-model chairs join.
- **Optional rc-driven last-good rehydration on arm:** after restart every
  track's engine history is empty → `UseLastGood` → Skip → **silence until the
  first good phrase** (locked default). A future rc-driven arm option could
  rehydrate last-good content from the block log on arm — an opt-in, never the
  engine default.
- **Standing per-phrase `UseLastGood` cells (whole-turn-miss hole) (Chameleon
  batch 1, F2):** `UseLastGood` only fires when a cell was *scheduled* and then
  squashed; a turn that produces no cell at all (the model never spoke) leaves no
  cell to fall back on, so the phrase is silent rather than a vamp repeat. The
  natural hook is the new `phrase_due` boundary: stand up a per-phrase
  `UseLastGood` cell at each phrase boundary so an unscheduled phrase still vamps
  the last good one. Out of scope for batch 1; recorded so the hole is known.
- **Deriver-budget enforcement beyond convention (Chameleon batch 1, F2):** the
  `Deriver` contract says ≲1 ms per cell (it runs on the beat thread under the
  timeline lock) but nothing enforces it — today it is a measured convention
  (T22 prints ~300 µs release for the ABC deriver). Add a timed `debug_assert`
  (or a soft warn) around `derive()` so a future heavy deriver trips loudly in
  dev rather than silently stalling the beat under the lock.
- **In-RAM committed `Vec` / RAM-CAS unbounded growth (Chameleon batch 1, F2):**
  the timeline's committed `Vec` and RAM CAS grow without bound for a long-armed
  composer (every phrase appends). **Rotation is the answer** — the chameleon
  rotation tick-continuity invariant retires old committed history into the
  durable block log + CAS and starts a fresh window — but it is not built. Until
  then a marathon set leaks RAM.
- **Band track↔chair mapping source of truth:** composer-create derives a track
  from the context label (`TrackId::new`→`slugify`, hard-error on empty slug).
  Once a band config exists (multiple chairs on one timeline), decide where the
  track↔chair mapping lives — there is no registry today (track is self-describing
  on every block, by design).
- **`played_by` collapses to `system()` — `who-played` provenance is degenerate
  (Chameleon batch 1, F2):** F1 §1.2 records "who played" as `BlockId.principal_id`,
  meant to be the player's principal. But the composer turn's model-text output
  block is inserted under `PrincipalId::system()` (`llm_stream.rs` `StreamEvent::TextStart`,
  the standing model-text convention), and `on_turn_completed` (`beat.rs`) sets
  `played_by = b.id.principal_id` = `system()`. The OODA `tick` verb also fires
  under `system()` (`beat.rs::fire_tick`), so `TurnFlow::Completed.principal_id`
  carries `system()` too — reading it instead of the block author would NOT help.
  So every materialized score block is authored by `system()` (plus `PrincipalId::beat()`
  for fallback repeats). **Harmless today** — one model per composer context, and
  lanes key on `track`, not principal, so no correctness/collision issue (the
  per-principal seq lane just has a single `system()` writer). **Will mis-attribute**
  the moment multiple models share a context or we want to distinguish player from
  transport. Not a one-liner: needs the composer turn to run (and author its
  output) under a distinct per-player principal. Surfaced in the F2 adversarial
  review (deepseek+gemini, 2026-06-11); the two silent-failure bugs from that pass
  (resume parent-id from log tail; hydration-failure publishing no terminal event)
  were fixed in-slice.
- **`kj track` listing surface:** no way to enumerate the tracks present on a
  context's timeline. Add a `kj` listing surface (which tracks exist, which
  principals played each) once tracks are user-visible.
- **Section-placement policy:** the OODA notation cell is scheduled a fixed
  **one phrase** ahead (`phrase_delta()`; `OODA_LEAD` is gone, Chameleon batch 1,
  F2); a real composer wants musical placement (next section boundary, loop
  region) and a richer `compute_basis`.
- **`Midi` render variant + UI timeline:** `audio/midi` projects to `ContentType::Plain`
  today; add a `Midi` variant + renderer, and the scrubbable timeline render.
  **Deliberately deferred to its first consumer (playback slice 2), not added in
  Chameleon batch 1, F2:** `ContentType` is a closed enum that rides
  `BlockHeader` inside `SyncPayload` ops, and the CBOR codec is fail-loud by
  design — a new variant breaks old decoders. Per the project rule a variant
  lands with its renderer, never speculatively. Interim sink key:
  `Role::Asset && parent_id → ABC source` (one hop); the authoritative mime is in
  the CAS sidecar.
- **midi→pcm re-anchor (playback slice 3) (Chameleon batch 1, F2):** the
  `abc_to_midi` *resolver* is gone — ABC→MIDI is now a barrier-side `Deriver`,
  not a timeline resolver, so the midi→pcm chain for dumb (PCM-only) sinks has no
  resolver shape to copy. Two candidate re-anchor shapes to pick between when
  playback slice 3 lands: (a) a deferred PCM **cell keyed on the derived MIDI
  hash** (real lead time, scheduled like any resolver), or (b) a measured
  **budget-excepted deriver** (only if midi→pcm proves fast enough to run at the
  barrier — almost certainly not, soundfont synthesis is heavy). See
  `docs/playback.md` item 8.
- **Trace span attribute:** attach `hyoushigi.tick` on the materialize→insert
  spans now that a producer exists.
- **Playback via peer sinks — design settled, see `docs/playback.md`:**
  peers advertise sound output at attach; kernel schedules objects via
  hyoushigi (materialized beat blocks = the scheduling unit); sinks pull
  from CAS and fire on a locally-held clock. Decisions 2026-06-10:
  pull-from-CAS (out-of-band capable later), transport state on FlowBus
  (new `TransportFlow`), and a **pause/stop verb remap** — pause = mute
  (clock keeps running, presentation-only, no BeatCommand), stop = clock
  freeze + OODA disarm (today's `BeatCommand::Pause`/`Stop`,
  `kj/transport.rs:43-54`). Prep checklist + slices in the doc; slice one
  is sink advertisement + clock distribution + a local 拍子木 metronome
  click. Scheduled after the registry extraction + FlowBus cleanups.
  Longer-term design conversation, not a task yet: unify hyoushigi
  beat-time and conversation wall-time ("the conversation has a tempo")
  so the timeline is the kernel's one clock rather than a music sidecar.

## kaijutsu-mcp — June 2026 SyncedDocument migration review

Surfaced by a DeepSeek (concurrency) + Gemini (architecture) review of commit
`ac5f518` (Remote backend cut over to `kaijutsu_client::SyncedDocument`). The
dropped-stdout bug and the content/exit_code completion race are fixed (poll now
does an authoritative `get_context_sync` read after terminal status); these are
the *remaining* findings, triaged.

- **HIGH (PARTIAL) — hook authoring vs resync: sole-writer + pushed-frontier.**
  `HookListener` writes blocks directly (`doc_mut().insert_*` under
  `synced.lock()`), so the bg listener is NOT the sole writer. `apply_sync_state`
  replaces the doc wholesale, so un-pushed hook blocks could be wiped on resync;
  and `push_ops` bases `ops_since` on the inbound SyncManager frontier, which
  local authoring never advances → every hook event re-pushes all prior local
  ops (idempotent but O(N)). MITIGATED 2026-06-13: `resync_synced` now FLUSHES
  local ops (`flush_local_ops`) before fetching the snapshot, so hook blocks
  round-trip through the server and survive the common case. REMAINING (cohesive
  follow-up, needs design + CRDT-frontier testing): (a) a dedicated "pushed"
  frontier so flush stops re-sending; (b) close the residual flush→apply window
  where a block authored mid-resync is still lost — cleanest via a command
  channel that makes the bg task the true sole writer (authoring + push + resync
  all serialized in one task).
- **MED — multi-context operations silently collapse to one in Remote.**
  `search_context`, `list_resources`, the `kaijutsu://docs` reader, and
  completions call `context_ids()`, which in Remote returns only the single
  joined context (`crates/kaijutsu-mcp/src/lib.rs`). A global search now silently
  skips every other context on the server. Fix: add an async
  `actor.list_contexts()`-backed lister for Remote multi-context surfaces.
- **MED — resource/prompt handlers hardcode `kind = "Conversation"` for Remote**
  (`analyze_document`, doc-tree, `read_resource`). Loses the real context type.
  Fix: carry the kind through the sync state or a metadata RPC.
- **MED — Remote input tools vs Local divergence:** Local `read/write/edit_input`
  swallow `create_input_doc` errors via `let _ =`; `submit_input` is
  unimplemented in Local mode. Either implement Local submit or document the gap.
- **LOW — hook insert/push failures only `warn!` then return `allow`.** The
  agent proceeds while its action's CRDT blocks are silently dropped — counter
  to the crash-loud stance. Consider returning `deny` (or a visible error) on
  push/insert failure.
- **LOW — `SyncedDocument::pending_events` not drained on `apply_sync_state`**
  (`crates/kaijutsu-client/src/synced_document.rs`): events buffered before a
  resync are never replayed against the new doc. Harmless if the server snapshot
  is always ahead of the buffered events; otherwise a silent loss.
- **LOW — dead `push_to_server` on `KaijutsuMcp`** (lib.rs): nothing calls it
  (the hook listener has its own `push_ops`); carries the same stale-frontier
  bug. Delete or consolidate.
- **PERF follow-up — the shell poll's authoritative read pulls the full context
  snapshot per command** (`execute_and_poll_shell`, Phase 2). Fine for short MCP
  contexts; a per-block read RPC (`actor.get_block(ctx, id)`) would avoid the
  O(blocks) transfer for large conversations.
- **TEST gaps beyond `tests/e2e_shell.rs`:** no coverage for Remote
  input tools, the hook-listener socket path, prompts, resources, or
  reconnect/resync. Add e2e cases (the harness in `e2e_shell.rs`
  generalizes).

## Testing & Tooling

- **russh teardown panic:** `ChannelCloseOnDrop::drop` panics with "there is no reactor running" in tests.
- **Capnp schema change ⇒ three binaries to bounce:** the dev runner
  only rebuilds/restarts `kaijutsu-app`; `kaijutsu-server.service`
  (systemd user unit) and `~/bin/kaijutsu-mcp` (running MCP processes
  hold the old binary; `cp --remove-destination` to replace, then
  reconnect MCP) keep stale codegen and fail handshakes with
  `Message contains non-list pointer where data was expected` (worse
  now that Kernel interface ordinals renumber on method deletion,
  e4c8417). Teach `contrib/kaijutsu-runner.sh`/`kj rebuild` to rebuild +
  restart all three, or at least print a loud reminder when
  `kaijutsu.capnp` changed.

---

## Architecture mapping pass — 2026-06-16

New observations from the crate-by-crate architecture sweep (see
`docs/architecture/`). Not fixed; recorded for later. Items that confirm an
existing entry are marked *(confirms above)*.

**Silent fallbacks (violate the "crash over confuse" stance):**
- `Kernel::list_tool_defs_via_broker` returns `Vec::new()` on *any* broker error
  (`kaijutsu-kernel/src/kernel.rs:467`) — a broken binding silently presents the
  LLM a tool-less context, no log/trace.
- `dispatch_tool_via_broker` does `broker.binding(...).unwrap_or_default()`
  (`kernel.rs:346`) — binding-fetch failure silently becomes deny-all; surfaces
  later as a confusing `ToolNotFound`.
- `MountBackend::read` falls through to raw on-disk content on *any*
  `FileDocumentCache::read_content` error, not just "missing/binary"
  (`kaijutsu-kernel/src/runtime/mount_backend.rs:267`) — a CRDT error could serve
  stale bytes.
- Additive `ALTER TABLE` migrations swallow SQL errors with `let _ =`
  (`kaijutsu-kernel/src/kernel_db.rs:873`) — a real failure surfaces as a
  confusing read-time error later.

**CRDT data model:**
- **Dual storage impls.** `BlockStore` (target) and `BlockDocument` (legacy) are
  both `pub` and in use; the legacy path returns newer fields
  (`ephemeral`/`stderr`/`signature`/`track`) as hardcoded `None`/`false`
  (`kaijutsu-crdt/src/document.rs:482`) and retains the duplicate-block seq bug
  fixed in `BlockStore` (`document.rs:892` vs `block_store.rs:320`). Pick a
  migration deadline.
- `calc_order_key` calls `block_ids_ordered()` (O(N) sort) on **every** insert
  (`kaijutsu-crdt/src/block_store.rs:390`); the bench exposing it is `#[ignore]`d.
- Tombstones aren't a first-class `BlockSnapshot` field — they ride a side
  `deleted_blocks` list re-applied by hand (`block_store.rs:1637`).
- `StoreSnapshot` has a breaking-format note with no migration path ("delete
  existing databases when upgrading", `block_store.rs:1680`).

**UTF-8 offset hazard:**
- `EditEngine` passes **byte** offsets/lengths to `block_store.edit_text`
  (`kaijutsu-kernel/src/file_tools/edit.rs:132`) while `FileDocumentCache` is
  careful to use **char** counts (`cache.rs:276`). Multi-byte content can corrupt
  the CRDT splice. Audit `edit_text`'s parameter semantics and unify.

**`LocalBackend::setattr` mtime is a no-op** (`kaijutsu-kernel/src/vfs/backends/
local.rs:354`) — it opens the file but doesn't set the timestamp, yet mtime is
load-bearing for `FileDocumentCache` staleness detection.

**LLM providers:**
- **Gemini is a dead stub** — `prompt`/`stream` return `Unavailable`, yet
  `available_models()` advertises three uncallable models
  (`kaijutsu-kernel/src/llm/gemini/mod.rs:64`). A config pointing at Gemini fails
  only at runtime. The `unrig` effort intended a real Gemini provider; finishing
  it (or removing the variant + its advertised models) is the open work.
- Stale doc: `Provider::prompt_with_system` comment still says "Phase 1:
  real-provider variants return Unavailable" (`llm/mod.rs:484`) — false for Claude
  and OpenAI now.

**`kj` single-source guarantee is manual** — `dispatch()` routing and
`kj_command()` schema tree must be hand-kept in sync; a subcommand added to one
but not the other is unreflectable (`kaijutsu-kernel/src/kj/mod.rs:589`).

**Types-crate layering** — `ThemeData` (~60 visual fields + `include_str!` of
`assets/defaults/theme.toml`) lives in the foundational `kaijutsu-types`
(`theme.rs:59`). Belongs in a UI/config crate.

**`kaijutsu-index`:**
- `rebuild()` is a TODO stub (`lib.rs:214`) — evicted HNSW slots accumulate
  forever.
- Metadata lock held across ONNX `embed()` (`lib.rs:160`) serializes all
  `index_context` calls.
- `ort` uses `download-binaries` — fetches ONNX Runtime at build time, breaks
  air-gapped builds.

**`kaijutsu-cas`** — no refcounting/GC (`remove` is unconditional,
`store.rs:330`); object+metadata write isn't atomic (crash between leaves a
metadataless blob, `store.rs:254`).

**`kaijutsu-telemetry`** — the Bevy path leaks a `tokio::runtime::Runtime` and
upcasts its `EnterGuard` to `'static` (`otel.rs:28`); soundness rests on the
leaked runtime outliving the guard.

**`kaijutsu-client`:**
- Backoff reset bug — `finish_closing` reads `self.state` *after* `mem::replace`
  moved it to `Idle` (`actor.rs:1451`), so the attempt counter isn't preserved
  through `Closing → Cooldown`; backoff always resets to 1 s after a post-connect
  failure.
- `is_disconnect_error` matches on the capnp error `Display` text
  (`actor.rs:1214`) — fragile; a capnp formatting change would stop triggering
  reconnect. Prefer a typed `ErrorKind::Disconnected` match.
- Peer-reattach residual: initial `attach_peer` isn't remembered until the first
  *successful* user call, so a kernel restart before that leaves the peer
  un-reattached (`actor.rs:1933`). *(extends `tech_debt_peer_reattach_on_reconnect`)*

**App (`kaijutsu-app`):**
- Triple Chat/Shell discriminator — `FocusArea` + `ActiveSurface` +
  `InputOverlay.mode` (the last unread by submit); collapse to
  `FocusArea::Compose(ActiveSurface)` (`input/focus.rs:71`,`:116`,
  `view/components.rs:285`).
- 77 `#[allow(dead_code)]` suppressors for future-phase API — prefer
  `#[cfg(feature)]` so dead-code discovery still works.

**`kaijutsu-abc`** — `to_abc()` round-trip silently drops
`InlineField`/`Decoration`/`VoiceSwitch` (`lib.rs:406`); tuplet writer omits the
optional `:r` count (`lib.rs:366`).

**Server `unwrap()`** — `create_shared_kernel` panics on workspace-insert failure
(`rpc.rs:1092`) instead of `?`-propagating like its neighbors.

**Cap'n Proto evolution is comment-only** — no `@version`; removed-method ordinals
are renumbered/reused with a "safe because all clients updated" comment
(`kaijutsu.capnp:921`,`:933`,`:1169`). *(confirms above — fragile with 7+ dependent
crates)*
