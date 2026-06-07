# Open Issues

Live work items distilled from prior design and TODO docs, plus architectural observations from code reviews. Code is truth; this exists to track what's *not* in the code yet.

Organized by area. Keep entries terse — link to file:line when a pointer makes the work concrete. When an item ships, delete the entry.

---

## Architecture & System Design

- **VFS Multiplexing:** `Kernel` implements `VfsOps` directly (`crates/kaijutsu-kernel/src/kernel.rs:870`). As the VFS grows to support multiple mount backends (local, memory, remote), this might become a bottleneck or overly complex. Consider extracting to a dedicated `VfsManager` or `VfsRouter`.
- **Server RPC Modularization:** `crates/kaijutsu-server/src/rpc.rs` is a massive file (~292KB). The monolithic implementation of the Cap'n Proto traits should be split into smaller modules by domain (e.g., `rpc/vfs.rs`, `rpc/llm.rs`, `rpc/mcp.rs`).
- **Cap'n Proto Schema Clarity:** There is slight conceptual overlap between `BlockKind` and `ContentType` in `kaijutsu.capnp`. Consider documenting the strict boundaries (e.g., `BlockKind` is the structural DAG role, `ContentType` is the raw MIME rendering hint).
- **Context-type tool policy (unified governance):** The `kj` surface is now
  capability-gated — escalation-relevant verbs check the caller's loadout via
  `KjDispatcher::require_cap` (five authority caps: `drive`/`fork`/`drift`/
  `transport`/`operator`, plus reuse of `rc-write` and the `builtin.block`/
  `builtin.policy` tool caps). `kj` was previously an ungated hole behind
  `facade:shell`. Remaining:
  - Dynamic / principal-scoped overrides.
  - Self-lockout ergonomics (narrowing binding to exclude `builtin.bindings`).
  - Per-principal budgets + fair queuing.
  - **Unjoined-`attach` authorization:** `kj attach` is gated on `operator`, but
    a caller with no joined context has no loadout to check, so an unjoined
    session is refused. Decide whether attach (switch + run target's rc) should
    be ungated like `switch`, or authorize against the target/session instead.
  - **Live contexts need reseed/restart:** broadened role loadouts only reach
    newly-created contexts; existing ones keep their old (now authority-less)
    binding until `kj rc reseed` + re-create or a kernel restart.
- **LLM providers:**
  - Move per-model knobs out of the config layer (`models.toml`), into the app.
  - Credential file option (alongside `api_key_env`).
  - Cross-turn thinking continuity (`hydrate_from_blocks` skips `Thinking`).
  - Push subscriber for `ConversationMailbox`.

## Persistence & Sync

- **`KernelDb` connection pool:** Currently `Arc<parking_lot::Mutex<KernelDb>>` in `block_store.rs:69`. This bottleneck prevents utilizing SQLite's WAL mode for concurrent readers. Migrate to `r2d2` or `sqlx` to allow non-blocking reads during LLM streams and heavy writes.
- **Config CRDT ops:** Config backend needs DTE integration so changes replicate across peers.
- **CRDT `order_index` BTreeMap:** `blocks_ordered()` is O(N log N). Works correctly but scales poorly; add a secondary sorted index when scale demands.
- **Latch state should persist with the context:** 
  - `set -o latch` mode is per-shell and lost on restart.
  - Latch nonces should eventually live in a SQLite table rather than in-memory.

## User Interface (kaijutsu-app) & UX

- **User presence (novel surface):** The compose input is a shared CRDT document. Surfacing in-flight compose state to an opted-in model would enable mid-sentence collaboration. Gate with explicit user opt-in.
- **Connection Polling Efficiency:** `ActorPlugin` in `crates/kaijutsu-app/src/connection/actor_plugin.rs` polls broadcast channels every frame. While `UpdateMode::reactive` helps, consider event-driven wakeups or bridging async streams directly into Bevy events more efficiently if latency/power becomes an issue.
- **Card-stack view:** Card size tuning, read-only scroll on focused card, dive-in (Enter), mouse click to focus, momentum scrolling, camera parallax, streaming card texture updates, card grouping evolution, ambient environment.
- **Text rendering (MSDF / 次):** TAA temporal super-resolution, glyph spacing per-font tuning, 1-frame blank flash on texture resize, large-context Vello "paint too large" crash.

## Control Plane & Navigation (kj)

- **`kj model` / `kj models` subcommand:** Add discovery for available providers/models and inspect the current context's model from `kj`.
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
- **Read-only explorer kaish:** A variant tool restricted to project exploration.

## Domain-Specific (ABC Parser & Engraving, Index)

- **`hnsw_rs` reverse-edge quirk:** Reverse edges written at neighbour's assigned layer.
- **ABC multi-tune files vs blocks:** Split tunes across sibling blocks or stack inside one block.
- **ABC file-header inheritance:** `M:`/`L:`/`Q:` defaults prevent proper inheritance.
- **ABC features:** `I:linebreak`, `m:` macro expansion, `%%` directives, Unicode escapes/fonts.
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
  exists, but the OODA cadence (`ooda_every`, default 32 bars) is fixed in
  `BeatPolicy::composer_default()`. Make the cadence a settable knob (rc-declared
  and/or a `kj transport` arg), persisted per context. Fine to do later.
- **Transport surface beyond `kj`:** app transport buttons / spacebar + a capnp
  transport surface (today `kj transport play|pause|stop|tempo|ooda` only).
- **Restart re-arm + playhead recovery:** a kernel restart resets composers to
  stopped and does *not* re-arm them; on re-arm, seed the playhead from the max
  committed tick rather than 0. (No archive RPC yet → disarm-on-archive also TODO.)
- **Section-placement policy:** the OODA `abc_to_midi` cell is scheduled a fixed
  bar ahead (`OODA_LEAD`); a real composer wants musical placement (next section
  boundary, loop region) and a richer `compute_basis`.
- **`Midi` render variant + UI timeline:** `audio/midi` projects to `ContentType::Plain`
  today; add a `Midi` variant + renderer, and the scrubbable timeline render.
- **Trace span attribute:** attach `hyoushigi.tick` on the materialize→insert
  spans now that a producer exists.

## Testing & Tooling

- **Live eval fork copy scope:** `kj fork` is a full copy. Decide if fork should be selective by default.
- **russh teardown panic:** `ChannelCloseOnDrop::drop` panics with "there is no reactor running" in tests.
