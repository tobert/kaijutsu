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
- **OODA has never been driven end-to-end (runtime validation gap).** Every
  link is wired and unit-tested — `kj context create --type composer` auto-arms
  (rpc.rs ~2685), the beat scheduler ticks, `composer/tick/S10-drive.kai` runs
  `kj drive`, `spawn_turn_driver` runs the model turn, `on_turn_completed` →
  `schedule_abc_cell` → materialize → `$HEARD` feeds back — but no real composer
  has completed a live OODA cycle. The first live run IS the integration test;
  expect it to surface something the unit tests didn't (mailbox windowing under
  a real turn, drive-seed hydration, ABC validation on live model output,
  cadence feel). To run: `kj context create --type composer --name <lane>
  --model <prov/model>` then `kj transport play`. For a fast demo loop crank
  `kj transport tempo` (the cadence-knob below is the clean fix). With a local
  model up (lemonade) this is the immediate next validation.
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
- **RC-driven hydration marker — SHIPPED first cut (Chameleon batch 2,
  2026-06-11):** the cost guard for the per-phrase report blocks mechanism 4
  writes. A windowed context hydrates only `[0, marker] ∪ last-window` instead
  of its whole history. As built: `select_hydration_window` (`llm/mailbox.rs`,
  fail-safe on stale marker) + a `context_hydration` table
  (`set/get/clear_hydration_policy`) + `ConversationMailbox::rehydrate_windowed`
  (per-turn rebuild, prefix byte-stable) wired into `process_llm_stream` (also
  cold-start, so restart-safe) + `kj context hydrate --window/--mark/--clear`
  (Operator-gated) + the composer `S30-hydrate.kai` create seed (`--window 16`).
  Declarative window (the tail slides in memory; row upserted only at create +
  durable revision, no per-turn hook).

  **Independent review 2026-06-12 (Fable + Gemini 3.1 Pro + DeepSeek V4-Pro,
  unanimous). CRITICAL + HIGH batch FIXED (TDD):**
  - **[FIXED] CRITICAL — windowed→full scramble.** The mailbox persists across
    turns; after a windowed turn `seen` has a hole where the archived middle was,
    so a later un-windowed `catch_up` (via `--clear`, or a fail-safe-to-None on
    DB-read failure / unparseable marker) folded the middle and *appended* it
    after the tail → out-of-order wire `[prefix, tail, …middle]` (silent
    corruption). Fix: `ConversationMailbox.windowed` flag; `catch_up` rebuilds
    from scratch on the windowed→full transition. Test
    `catch_up_after_windowed_rebuilds_full_in_chronological_order`.
  - **[FIXED] HIGH — `--mark` not validated against the context.** A parseable
    but non-existent marker persisted durably, then fail-safe-to-whole-log every
    turn = cost guard silently OFF forever. Fix: `context_hydrate` now verifies
    the block exists in the target ctx (`get_block_snapshot`) before persisting.
  - **[FIXED] HIGH — `--window 0` dropped the current turn.** Prefix-only wire,
    so the just-inserted prompt never reached the model. Fix: verb rejects
    `--window 0`; `get_hydration_policy` reads a 0/negative row as None (corrupt
    → hydrate everything).
  - **[FIXED] HIGH — stale marker was silent.** The doc claimed "the caller logs
    the anomaly"; none did. Fix: `rehydrate_windowed` now `warn!`s (recurring,
    every turn) when the marker doesn't resolve and windowing is bypassed.

  **Player spawn = thin fork + rc-rebuilds (LOCKED 2026-06-12).** Resolves
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
  - **Verify in code first:** the disarm-parent → `fork` → arm-child **ordering
    race** across the beat scheduler's tick. **VERIFIED 2026-06-12 (Opus +
    DeepSeek review) — REAL, and it needs Rust: the rotate *trigger* cannot be
    pure rc.** Root cause: `fire_tick` (`beat.rs:659`) spawns the `tick` rc
    fire-and-forget (`spawn_local`), and `kj transport ooda off` only *enqueues*
    `BeatCommand::SetOoda{P,false}` on the same ingress the single-task scheduler
    reads — so the parent's disarm is **asynchronous relative to the scheduler's
    own clock**. Between firing tick T (which spawns the rotate rc) and that
    `SetOoda` being dequeued, the scheduler keeps P armed and keeps ticking it.
    Failure modes: (1) **stray parent ticks** — extra `tick` rc runs (token spend
    + turn requests) on a context that already forked away, until the disarm
    lands; the `phrase mod N` gate blocks a *second* rotate in the common case,
    so usually ≤1 stray tick. (2) **double-fork** — only pathological (disarm
    delayed ≥ a full rotate period, or `ooda_every ≥ N·beats_per_phrase` so
    consecutive ooda ticks both hit the horizon). `biased` select only *reduces*
    the window (a ready disarm wins over a ready deadline); it is not the root and
    cannot close it. **Two fixes evaluated:** an "atomic Rotate `BeatCommand`"
    does NOT close it (still async via ingress — only fixes the child-armed-while-
    parent-armed half-state, not the stray ticks); a rotate-specific in-flight
    latch works only if the scheduler knows the rotate condition. **Cleanest:
    move the horizon decision into the scheduler** — e.g.
    `BeatCommand::RotateOnPhrase { parent, modulus, child_preset }` stored in
    `BeatState`, checked synchronously in `fire_due` at ooda time, disarming the
    parent + arming the child atomically with zero async gap. So `composer/fork/`
    setup stays rc, but the *detach-at-horizon trigger* is Rust. **IMPLEMENTED
    2026-06-12:** `BeatCommand::SetRotate{ctx, every_phrases}` +
    `BeatState.rotate_every_phrases` + `kj transport rotate --every N | off`; at a
    phrase horizon where `phrase % N == 0`, `fire_due` `stop`s the parent
    synchronously (not re-pushed → no further ticks) and reports `rotate_due`
    (suppressing a coincident `ooda_due`), and the run loop fires the `rotate` rc
    lifecycle fire-and-forget. The rotate ACTION (a `composer/rotate/*.kai` that
    forks `--preset spawn` + arms the child) is still rc and unwritten — when it
    lands it's race-free because the parent is already stopped. Tests: scheduler
    (`rotate_horizon_retires_parent_synchronously`, `rotate_cadence_gates_on_the_
    modulus`, `no_rotate_when_cadence_unset`) + verb (`transport_rotate_*`).
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
  repair, drift-a-summary-back). Thin fork is *reuse/reduce*: save tokens for
  a long-running iterating player (now the `window`/`spawn` factory presets
  per `docs/fork-filters.md`; `--shallow` retiring, `--compact` unchanged).
  Copy cost is a non-issue (storage cheap); the axis is KV-cache strategy.
  Two unbuilt primitives this model implied (both since designed):
  - **[DONE 2026-06-12] `kj fork --exclude <block>…` on FULL fork** (commit
    c51544d). `fork_full` now routes through `fork_document_filtered` with
    `exclude_block_ids` when `--exclude` is given (else the plain unfiltered copy,
    zero overhead). Fail-loud validation (parses + exists in source, like
    `--mark`). The orchestrator-repair case works. Scoped to full fork at the
    time; superseded by the fork-filters algebra (`docs/fork-filters.md`) where
    `--exclude` composes with any base — block-key form stays a dedicated flag,
    ranges arrive with `--include`/`--exclude <range>`.
  - **[RESOLVED 2026-06-12 #2] Empty-selection fork seeds `next_tick = 0` —
    that's correct, not a hazard (Amy's call).** A `spawn` (or `--compact`)
    fork copies ~nothing, so there's nothing to seed from and the child's
    timeline starts at tick 0. A `spawn` is a *birth*, not a rotation: a fresh
    player has no prior segment to be continuous with, so tick 0 is the right
    floor. The tick-continuity invariant (`docs/chameleon.md`) is a property of
    **rotation**, where the fork carries a tail window — that fork copies the
    max-tick block, so `fork_filtered` already seeds `next_tick` past it from
    the *copied* set (`block_store.rs` ~L1511, the batch-1 f1-track fix);
    unchanged and correct. DuplicateBlock can't arise from a low tick anyway —
    block identity is `(context, principal, seq)` and the child holds a fresh
    `context_id`. The one open edge is a *partial* selection that drops the
    max-tick tail while keeping earlier ticked blocks (no composer preset does
    this today); decide rotation-vs-fresh intent if such a preset ever lands.
  - **[FOUNDATION FIXED 2026-06-12 #2, slice 2c — field retirement still
    slice 4] `fork_filtered` truncated in the WRONG order.** `fork_filtered`
    now builds its positional universe in document (`order_key`) order, so the
    `selection` keep-set AND `max_blocks` index the timeline, not BlockId
    order. `max_blocks` is correct in the interim (test:
    `fork_filtered_max_blocks_keeps_most_recent_by_timeline`); the field is
    marked deprecated and is retired in slice 4, spelled `--include end-N:`.
    Original analysis preserved below. Two orders exist:
    `BlockId` Ord is `(context_id, principal_id, seq)`, so `BTreeMap<BlockId>`
    iteration (what `fork_filtered` walks at `block_store.rs:1475`) is
    **principal-major, seq-minor**; document order is `order_key` (derived from
    `tick`), exposed canonically by `block_ids_ordered()` (`:191`). The
    `max_blocks` truncation comment (`:1489`, "keep the most recent (by
    order_key, since BTreeMap is ordered)") asserts a **false equivalence** —
    BTreeMap is ordered by BlockId, not order_key. They coincide only for a
    single principal whose seq tracks tick; they diverge for **any
    multi-principal context** (beat()+player, multiple models, drift author,
    multi-user — the normal case), where `.skip(skip)` keeps one principal's
    tail, not the N most recent by timeline. Live only via `kj fork
    --depth`/`--shallow` (`fork.rs:426`) — the flag already slated for
    retirement. `fork`/`fork_at_version` copy *all* blocks (each keeps its own
    order_key, child re-sorts), so they're unaffected; `fork_filtered` is the
    only position-dependent path. **Fix (chosen): fold `--depth N` into the
    selection engine as `--include end-N:` over the `block_ids_ordered()`
    snapshot (correct by construction), retire the `max_blocks` field. Add a
    test proving position ≠ BlockId order on a multi-principal log so the
    divergence can't silently return.** The comment is not an intentional
    design choice — it contradicts the crate's own documented order model
    (`block_store.rs:79–91`: "ordering is the successor-key axis").
  - **A snapshot/savepoint marker verb (speculative, not-now — direction set
    2026-06-12).** Absorbed by the fork-filters range grammar as a future
    **label endpoint** (`docs/fork-filters.md`): a savepoint is a colon-free
    name on a block, usable as a range endpoint (`kj fork --include 0:bridge`)
    — no new fork machinery, no verb semantics of its own. Still not-now;
    build labels when the orchestrator work or the time-well wants named
    points.
  - **[RESOLVED 2026-06-12] What shape does a thin fork copy?** Answered by
    the fork-filters design (`docs/fork-filters.md`): there is no single
    "thin shape" — the kernel primitive is an **interval selection** over the
    ordered block log (`kept = (base ∩ ∪inc) \ ∪exc`, order-free, resolved at
    fork instant), and the shapes are **factory presets**: `window` = the
    hydration keep-set `[0,P] ∪ tail` (prefix-preserving — the KV-reuse /
    API-chair case; loud error if the parent has no policy row) and `spawn` =
    ~nothing (rc-rebuilds — the player case; fresh bytes are the *feature*,
    that's the horizon-latch edit channel). The rc-rebuilds-vs-prefix tension
    dissolves: different intents, chosen per fork. last-N is retired with
    `--shallow`/`--depth` (spelled `--include end-N:`). Hydration policy row
    travels whenever the marked block survives the selection (mechanical
    `(principal, seq)` remap) — which also resolves "fork drops the policy"
    by construction. Coding plan in `tsugi.md`.
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
  - **[FIXED 2026-06-12] Fail-loud on corrupt/unreadable policy.** The
    DB-read-failure path and corrupt stored policy (unparseable marker / window
    < 1) used to degrade silently to full history. Now `get_hydration_policy`
    returns `Err(Validation)` on corruption and `process_llm_stream` fails the
    turn (publishes `TurnFlow::Failed`) on any policy-read error — a silent
    fallback on a safety mechanism is worse than a loud stop. The **runtime**
    stale-marker case (marker parses + names a real block id, but it's absent
    from the log — e.g. the block was excluded) is *deliberately* kept as
    loud-warn + fail-safe-to-whole-log: that's the one genuine "show more, never
    corrupt" case, and failing it would kill a composer just because a block was
    excluded. (Tests: `hydration_policy_{zero_window,unparseable_marker}_is_loud_error`.)
  - **RAM/disk accumulates — and that's fine (LOCKED 2026-06-12).** Windowing
    bounds *tokens*, not memory; cold start loads the full log to window it.
    Rotation-for-space is **dropped** (storage on btrfs+sqlite is cheap; blocks
    stay real and stored so the app shows the whole performance). The thin fork
    is for lean-spawn/structure, not storage — see the player-spawn block above.
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

- **HIGH — `hook_listener` breaks the single-writer invariant + resync TOCTOU.**
  The bg listener is documented as the sole writer of `synced`, but
  `HookListener` also writes (`doc_mut().insert_*` under `synced.lock()`,
  `crates/kaijutsu-mcp/src/hook_listener.rs`). During `resync_synced`'s
  `get_context_sync().await` (no lock held), a hook can author blocks into the
  old doc, then `apply_sync_state` replaces it wholesale → hook-authored blocks
  silently lost. Fix: either route hook authoring through the bg task (a
  command channel), or have `resync_synced` re-apply locally-authored ops that
  landed in the fetch→apply gap.
- **HIGH — hook push frontier never advances → re-pushes ops every hook event.**
  `push_ops` computes `ops_since(doc.sync().frontier())`; locally-authored
  blocks don't advance the SyncManager (inbound) frontier, so every hook event
  re-sends all prior locally-authored ops (idempotent server-side, but O(N)
  bandwidth). Fix: track a separate `last_pushed` frontier and advance it after
  a successful push.
- **HIGH — bg event-listener panic is silently swallowed.** The `tokio::spawn`
  JoinHandle is kept only for its AbortHandle; nobody observes it. If
  `apply_event` panics, the listener dies, `notify_waiters` never fires again,
  and every shell poll degrades to the 500ms fallback forever (looks like a
  hang). Fix: supervise the task (log+surface on join) or `is_finished()`-check
  it in the poll.
- **MED — `std::sync::Mutex` + poisoning on a single-threaded (LocalSet)
  runtime.** Holding `synced.lock()` blocks the whole thread (not just the
  task), and any panic-under-lock poisons it, cascading every
  `.expect("synced mutex poisoned")`. Switching to `parking_lot::Mutex`
  removes poisoning and the blocking-vs-async hazard in one move.
- **MED — Notify lost-wakeup adds up to 500ms poll latency.** Classic check→
  await TOCTOU on `tokio::sync::Notify`; the 500ms fallback prevents a hang but
  not the latency. Fix: a `tokio::sync::watch` carrying a monotonic generation
  counter (read gen, then `changed()` — no window).
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
- **TEST gaps beyond `tests/e2e_context_shell.rs`:** no coverage for Remote
  input tools, the hook-listener socket path, prompts, resources, or
  reconnect/resync. Add e2e cases (the harness in `e2e_context_shell.rs`
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
