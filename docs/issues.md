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
  - **Live contexts need reseed/restart:** broadened role loadouts only reach
    newly-created contexts; existing ones keep their old (now authority-less)
    binding until `kj rc reseed` + re-create or a kernel restart.
- **Zombie RPC session / no session reaping:** the server has warned every
  60s for 21+ hours that session `019eb229` (an app session predating a
  Jun 10 GUI restart) is "still active" — there is no reap path for dead
  sessions. Related: auto-memory `tech_debt_peer_reattach_on_reconnect`
  (app doesn't re-attach after kernel restart). Found during the
  2026-06-11 journaling forensics.
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

## Event Plumbing (FlowBus) — June 2026 audit

- **`InputDocFlow` wiring is optional by construction:**
  `block_store.rs:204` holds `Option<SharedInputDocFlowBus>`; forget
  `set_input_flows()` and input events silently vanish. Consider
  constructor injection.
- **`SyncReset` never emitted (intentional, note only):** per-block DTE
  stores skip compaction so `sync_generation` stays 0 (`rpc.rs:3988`);
  client receive paths exist but are untested live machinery. Revisit when
  compaction returns.

## Drift — June 2026 audit

- **Extract `ContextRegistry` from `DriftRouter`:** DriftRouter carries ~7
  responsibilities (context registry, per-context LLM config, staging
  queue, dead-letter queue, lost+found lifecycle, context state, trace-ID
  assignment) — `drift.rs:172-563`. Everything that needs "what contexts
  exist" takes a dependency on drift, inverting the hierarchy. Pull
  register/resolve/list/llm-config/trace-id into a `ContextRegistry`;
  drift keeps the queues. Cold-start hydration (`rpc.rs:1150-1183`) moves
  with the registry.
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

- **Post-restart oplog journaling gap — RESOLVED as observation artifact,
  hardening remains (forensics 2026-06-11): no data loss.** Two confirmed
  mechanisms produced the misread: (1) timezone double-count — the single
  Jun 10 restart at 11:30:03 EDT logs itself as `15:30:03Z` and was counted
  as two restarts ("11:30 and 15:30"); the "afternoon" block activity was
  14:42–15:08Z = 10:42–11:08 EDT, exactly where the oplog "ends". (2) WAL
  invisibility — `kernel.db`'s main file was last checkpointed 11:08 while
  ~4.1 MB of newer ops sat only in `kernel.db-wal`; the kernel never
  checkpoints proactively (`kernel_db.rs:797-799`), so any bare-file read
  shows history frozen at the last checkpoint. Post-restart journaling
  verified working: seq numbering continued at 189, compaction snapshot at
  11:48:39, and the 11:48 smoke-test block was decoded out of oplog seq
  197. Hardening (Chameleon batch 1 — the block log becomes the
  stamp-turn WAL):
  - DONE (2026-06-11) — fail-loud guard. The five journaling writes
    (`journal_op`, `compact_document`, `write_initial_snapshot`,
    `journal_and_maybe_compact_input`, `compact_input_doc`) routed through
    `BlockStore::journaling_db()`: a store that declares persistence
    (`with_db*`, `persistent = true`) with no db handle now returns
    `NoDatabaseConfigured` instead of silently dropping the op; replica
    stores (`new`/`with_flows`) keep their legitimate no-op. Guard test
    `persistent_store_journaling_without_db_fails_loud` (red-first).
  - DONE (2026-06-11) — `PRAGMA wal_checkpoint(TRUNCATE)` after compaction
    via `KernelDb::checkpoint()`, called from `compact_document` /
    `compact_input_doc`, so the main file stops lagging. Tests:
    `kernel_db::checkpoint_truncates_wal`, `block_store::test_durability_across_kill`
    (insert via real paths, leak/forget the connection to simulate SIGKILL,
    reopen a fresh connection, `load_from_db`, assert everything present).
  - REMAINING — graceful-shutdown WAL checkpoint. `SharedKernelState::drop`
    does a best-effort checkpoint, but it only fires on a clean process exit;
    the server's `run()` loop never returns and the process dies on
    SIGKILL/SIGTERM without unwinding, so systemd `stop` skips it. The
    proactive compaction checkpoint covers durability (no loss either way);
    this gap only affects bare-file forensics between the last compaction and
    shutdown. Fix: a `tokio::signal` SIGTERM handler that checkpoints before
    exit (needs the run loop to become interruptible — bigger than the rest).
  - Forensics hygiene: tracing logs UTC, systemd speaks local — cite both
    zones when recording restarts in issue notes.
- **`KernelDb` connection pool:** Currently `Arc<parking_lot::Mutex<KernelDb>>` (`block_store.rs:74`). This bottleneck prevents utilizing SQLite's WAL mode for concurrent readers. Migrate to `r2d2` or `sqlx` to allow non-blocking reads during LLM streams and heavy writes. Note: SQLite serializes *writes* regardless of pooling, so the win is concurrent reads (and only with WAL enabled) — verify WAL before assuming a pool helps; narrowing lock scope may matter as much.
- **Config CRDT ops:** Config backend needs DTE integration so changes replicate across peers.
- **`blocks_ordered()` allocation churn + sort:** `block_store.rs:185-188` calls `order_key().to_string()` for *every block*, then `sort_by` on the strings — so it's O(N log N) **plus a String allocation per block per call**. It runs on per-frame hot paths (`kaijutsu-app/src/ui/card_stack/sync.rs:48`, `view/components.rs:163`), so the allocation churn is likely the bigger cost than the asymptotics. Fixes: compare `order_key` without stringifying, and/or cache the ordering and invalidate on block change. Add a secondary sorted index when scale demands.
- **Latch state should persist with the context:** 
  - `set -o latch` mode is per-shell and lost on restart.
  - Latch nonces should eventually live in a SQLite table rather than in-memory.

## User Interface (kaijutsu-app) & UX

- **User presence (novel surface):** The compose input is a shared CRDT document. Surfacing in-flight compose state to an opted-in model would enable mid-sentence collaboration. Gate with explicit user opt-in.
- **Connection Polling Efficiency:** `ActorPlugin` in `crates/kaijutsu-app/src/connection/mod.rs` polls broadcast channels every frame. While `UpdateMode::reactive` helps, consider event-driven wakeups or bridging async streams directly into Bevy events more efficiently if latency/power becomes an issue.
- **Card-stack view:** Card size tuning, read-only scroll on focused card, dive-in (Enter), mouse click to focus, momentum scrolling, camera parallax, streaming card texture updates, card grouping evolution, ambient environment.
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
- **TurnFlow bus lossy + in-memory:** Dropped `turn.requested` events are silent. Revisit with persistence.
- **Headless turn cwd is `/`:** Decide whether to thread the context's stored shell cwd into the headless `ExecContext`.
- **`--switch --prompt` double-drives:** Clarify semantics when both human and autonomous turn try to drive a child.

## Tool System Follow-ups (post-Phase 5)

- **`StreamingBlockHandle` implementation:** Single-block streaming primitive.
- **LLM streaming rewrite:** Move `process_llm_stream` onto `StreamingBlockHandle`.
- **Block content abstraction:** Blocks as containers for multiple content artifacts.
- **MCP `progress` → `StreamingBlockHandle` bridge.**

## Domain-Specific (ABC Parser & Engraving, Index)

- **`hnsw_rs` reverse-edge quirk:** Reverse edges written at neighbour's assigned layer.
- **ABC multi-tune files vs blocks:** Split tunes across sibling blocks or stack inside one block.
- **ABC file-header inheritance:** `M:`/`L:`/`Q:` defaults prevent proper inheritance.
- **ABC features:** `I:linebreak`, `m:` macro expansion, `%%` directives, Unicode escapes/fonts.
- **ABC duration-summing ruler:** kaijutsu-abc has no total-beats-per-voice
  machinery; needed to validate that a committed phrase's ABC sums to
  `beats_per_phrase` (Chameleon eval ruler, new code). The tuplet/broken-rhythm
  handling in `midi.rs:261-274` is the acceptance spec.
- **ABC layout:** Linear duration spacing (needs Gould spacing/justification), system bracket/brace, closed-score layout.

## Hyoushigi / Composer

- **Composer `kj` loadout — narrowed (kj capability gates).** `composer` now
  seeds its own `assets/defaults/rc/composer/create/S10-binding.kai`: `drive` +
  the block/read tooling + facades, *not* `fork`/`drift`/`transport`/`operator`.
  The tick (`kj drive`) runs under this loadout, so narrowing the binding now
  actually gates self-driving (it didn't before `kj` grew capability gates).
  Follow-up: revisit whether the composing turn also needs `submit_input` vs.
  relying on the turn driver, and trim further if the tick proves it can.
- **Cadence/tempo should be settable per context:** `kj transport tempo <bpm>`
  exists, but the OODA cadence (`ooda_every`, default 8 phrases = 128 beats) is
  fixed in `BeatPolicy::composer_default()`. Make the cadence a settable knob
  (rc-declared and/or a `kj transport` arg), persisted per context. Fine to do
  later.
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
- **`$HEARD` re-shaped to a pull (Chameleon batch 2, 2026-06-11):** `$HEARD` is
  **no longer a pushed var.** Rather than the kernel computing a window and
  injecting it every turn, the drive script will *read the committed past on
  demand* — a kaish read of recent score blocks across tracks (a block-log
  windowed read, since committed cells leave the in-RAM `Timeline.committed` at
  the barrier but the durable blocks carry `tick`+`track` and sync). Built when
  a **second player** makes it load-bearing (slice one is solo bass; its own
  hydrated history is its continuity), on that windowed-read primitive — which
  the RC hydration-marker archive verb also wants. `content_before` in
  `ResolverCtx` stays deliberately track-blind regardless (no resolver reads it;
  `CasCommitResolver` reads CAS by hash). Design the read API with its consumer
  (two-voices rule), not speculatively here.
- **Transport-report cost guard — the RC-driven hydration marker (Chameleon
  batch 2, 2026-06-11):** mechanism 4's transport report now ships as a durable
  block — `kj drive --prompt "<report>"` writes a real User block each phrase
  (`kj/drive.rs`; previously the prompt was silently dropped). That's correct
  but **appends one block per phrase forever**. The guard is the **RC-driven
  hydration marker** (design in `docs/chameleon.md`): a minimal Rust hydration
  policy on `ConversationMailbox` (keep `[0, marker] ∪ [now − window, now]`,
  default marker = ∞) + a `kj` verb that advances the marker / archives the
  middle + an **on-turn rc hook** that calls it in composer mode (policy lives
  in rc, not Rust). Reuses the existing exclude/edit-at-boundary rule. **Build
  before slice one runs sustained at tempo** — fine while hand-testing below
  tempo. Open: hook placement (reuse `tick` vs. a post-turn verb so the marker
  advances *after* the model reads), and the windowed archive surface (shared
  with `$HEARD`'s pull).
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

## Testing & Tooling

- **Live eval fork copy scope:** `kj fork` is a full copy. Decide if fork should be selective by default.
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
