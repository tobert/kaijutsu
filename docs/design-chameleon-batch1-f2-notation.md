# Fork 2 — Notation-first commits: ABC is the score, MIDI is derived at the write barrier

**Verdict: Option B, unanimous (3-0), with judge grafts folded in.** All file:line references below verified against main on 2026-06-11 by direct read (not just recon trust).

## 0. Verified ground truth (deltas from the advocate proposals)

- **The "step zero" compile break is already fixed.** `kaish_backend.rs:737` has `resident_bytes: None`; `cargo check -p kaijutsu-kernel` is green on main. Drop step zero from both plans.
- **F1 (Track on Cell, `PrincipalId::track`, per-track UseLastGood) has NOT landed** — no `Track` type in kaijutsu-hyoushigi, no track mint in `kaijutsu-types/src/ids.rs`. Per mandate this design *assumes* F1; §12 gives the landing-order contingency.
- **`kj wait` does not exist** (grep-verified across `kaijutsu-kernel/src/kj/`). The `TurnFlow::Completed` timing change has exactly one non-test subscriber: `beat.rs:299-326` — the code being fixed.
- All of Option B's load-bearing claims verified: spawn-time Completed publish (`rpc.rs:390-395` publishes immediately after `spawn_llm_for_prompt`, which only `spawn_local`s at `llm_stream.rs:305-324`); ephemeral hydration skip (`hydrate.rs:79-81`, ahead of excluded at :82-84); the Model/Text flood path (`hydrate.rs:145-157`); Role::Tool+Abc hydrating as a *user envelope* (`hydrate.rs:198-214` — would flood worse, correctly rejected); free fallback derivation (`engine.rs:402-407` pushes a concrete copy into `committed`); the Failed-cell zombie (`engine.rs:297-302`); the silent crystallize-skip (`kernel hyoushigi/mod.rs:280`); `insert_from_snapshot` never bumping `next_tick` (`crdt block_store.rs:797-825`); the DuplicateBlock hazard (`mod.rs:288` mints from in-RAM `high_water`, reset at `beat.rs:119`, error swallowed at `beat.rs:234`); `absorb_emitted`'s dangling-bytes hazard for concrete emissions (`engine.rs:418-420`) — the latent bug that disqualified A's "existing seam" pitch.
- **chameleon.md:57-58 and :119 say "derived MIDI is an emitted sibling _cell_."** This design amends that wording (same slice, §14) — the decision's substance (notation is the score, MIDI a render) is preserved; the mechanism moves to the write barrier.
- `journal_op` silently no-ops when the store has no db handle (`kernel block_store.rs:711-713`) — beat blocks ride exactly this path; recorded in issues.md (§13), not fixed here.

## 1. The shape in one paragraph

The musician's scheduled cell becomes a **notation cell**: a trivial validating `cas_commit` resolver commits the ABC itself (`text/vnd.abc`) as the cell body. `materialize_committed` — the single write-barrier crossing every committed cell passes, **including fallback copies** — consults a mime-keyed `DeriverRegistry` (kernel integrator layer, where the kaijutsu-abc dep already lives) and inserts the MIDI as a **derived sibling block**: same tick, same track principal, `Role::Asset`, content = 32-hex CAS hash, `parent_id` = the ABC source block. The ABC block materializes as `Role::Model` + inline content + `ContentType::Abc` (materialize.rs:42-47 unchanged) → renders as a staff with zero app change. The MIDI never enters the timeline's committed log, so the `UseLastGood` candidate pool is notation-pure by construction. The speculative engine is untouched except two tiny additive items: a failure ledger (§6) and `seed_playhead` (§10).

## 2. The resolver: `CasCommitResolver` (replaces `AbcToMidiResolver`, kernel hyoushigi/mod.rs:159-234)

```rust
/// Commits CAS-referenced bytes at a tick, validating by mime. The generic
/// "put this content on the score" resolver — notation today, automation
/// cells tomorrow (same recipe shape, different mime).
struct CasCommitResolver { cas: Arc<FileStore> }
impl CasCommitResolver { pub const ID: &'static str = "cas_commit"; }
```

- **params**: `{ "hash": <32-hex>, "mime": "text/vnd.abc" }`.
- **estimate_cost**: keep the 20 ms floor (mod.rs:184-188).
- **compute_basis**: `ContextHash::of(hash-string)` — unchanged semantics; constant per cell, so this resolver never squashes on its own basis. (ContextQuery growth is future work.)
- **resolve**: retrieve bytes by hash — malformed hash or missing CAS entry is a loud `ResolveError::Failed` (keep today's asymmetric boundary, mod.rs:167-176, and keep the `abc_to_midi_rejects_malformed_hash` test renamed). Then run the per-mime validator: `text/vnd.abc` → UTF-8 + `kaijutsu_abc::parse` must yield ≥1 tune with no errors (loud Failed otherwise); **unknown mime → pass-through** (defined default, not a silent fallback — most content needs no kernel opinion). Return `Resolution::new(bytes, mime)`.
- `abc_to_midi` is **deleted**, not kept alongside: one producer, one mechanism. `register_resolvers` (mod.rs:110-112) registers `CasCommitResolver`.

This preserves the **first-commit-validation invariant**: every ABC cref that enters `committed` parsed successfully at least once — which is what makes derivation failure at the barrier an invariant violation (§5c) rather than weather.

## 3. `schedule_abc_cell` (mod.rs:123-147) — eager validation, phrase lead

Signature unchanged except the caller now passes `st.policy.phrase_delta()` as lead. Internals:

1. **Pre-validate**: `kaijutsu_abc::parse(abc)` — malformed ABC returns `Err` *before* anything is stored or scheduled (today it becomes a silently-Failed zombie cell). The error surfaces per §7's Error-block path.
2. Store to durable CAS (unchanged, :132).
3. Schedule `Recipe { resolver: cas_commit, params: {hash, mime: "text/vnd.abc"}, query: default, fallback: UseLastGood }` at `playhead + lead`. With F1, the cell carries its `Track`.

Defense in depth: validated at schedule, again at resolve, invariant-checked at derive. Three loud gates.

## 4. `DeriverRegistry` — the emission mechanism (kernel hyoushigi/mod.rs, new)

```rust
/// A pure, fast (≲1 ms) projection run at the write barrier, on the beat
/// thread, under the timeline lock. Derivers MUST be deterministic,
/// allocation-light, and never block. Anything heavier (midi→pcm synthesis)
/// is NOT a deriver — it stays a timeline resolver with real lead time.
pub trait Deriver: Send + Sync {
    fn source_mime(&self) -> &'static str;              // exact-match key
    /// (bytes, mime) pairs for derived sibling blocks. Err = invariant
    /// violation (content that passed first-commit validation must derive).
    fn derive(&self, bytes: &[u8]) -> anyhow::Result<Vec<(Vec<u8>, String)>>;
}

pub struct DeriverRegistry { by_mime: HashMap<&'static str, Box<dyn Deriver>> }
impl DeriverRegistry {
    pub fn production() -> Self { /* registers AbcToMidiDeriver */ }
    pub fn get(&self, mime: &str) -> Option<&dyn Deriver> { … }
}

struct AbcToMidiDeriver;  // parse + to_midi moved verbatim from the old resolver;
                          // parse errors → Err (invariant violation, §5c)
```

A mime with no deriver derives nothing — the defined default. The registry is the single extension point for second consumers (§9). The substrate's mime-blindness holds: `materialize.rs:33-36` is still the only place the open MIME string meets the closed render-hint enum; the *registry* switch lives one layer up in the kernel integrator, exactly where the kaijutsu-abc dependency already is.

## 5. `materialize_committed` rewrite (mod.rs:255-304) — the write-barrier crossing

New signature and cursor (BeatState's bare `high_water: usize` is replaced):

```rust
/// Per-context materialization cursor, owned by BeatState.
pub struct MaterializeCursor {
    /// Committed cells fully materialized by THIS process's timeline.
    pub high_water: usize,
    /// Artifacts of the in-progress cell already inserted (resume point for
    /// per-artifact retry; 0 unless the previous attempt failed mid-group).
    pub artifacts_done: usize,
    /// Per-track-principal next BlockId seq, lazily seeded from the block log.
    pub lanes: HashMap<PrincipalId, u64>,
}

pub fn materialize_committed(
    timeline: &SharedTimeline,
    cas: &FileStore,
    blocks: &SharedBlockStore,
    context_id: ContextId,
    cursor: &mut MaterializeCursor,
    derivers: &DeriverRegistry,
) -> anyhow::Result<Vec<BlockId>>
```

Per committed cell past `high_water`, in strict order:

1. **Bytes.** `guard.content_bytes(&cref.hash)`, else `cas.retrieve(&cref.hash)` (covers `Fallback::Literal` pre-stored durably), else **`bail!`** — this **replaces the silent `if let Some` skip at :280**. Missing bytes for a committed cref is corruption; crash over it. (Judge-mandated regardless of option.)
2. **Derive.** `derivers.get(&cref.mime)` → `derive(bytes)?`. Failure bails **before any insert** — `high_water` un-advanced, no half-cell (§5c). Derive-before-insert is the resumability spine: derivation is deterministic, so a retry regenerates the identical artifact list.
3. **Crystallize.** `cas.store(...)` for source and every derived artifact (idempotent; the ABC re-store is a same-hash no-op — keep the `debug_assert_eq!`).
4. **Lane seed (lazy, first touch).** `cursor.lanes.entry(principal)` seeds from `blocks.max_seq_for_principal(context_id, principal).map_or(0, |s| s + 1)` (new kernel helper, §10) — mirroring the own-principal `next_seq` restore at crdt block_store.rs:803-805, which never covers beat()/track principals. **The seed scans seqs actually present in the log per author principal — never a known-tracks list** — so derived lanes and legacy `beat()` rows are both covered.
5. **Insert artifacts per-artifact, resumably** (judge-1 graft replacing B's group atomicity). The artifact list is `[source, derived...]`. Skip the first `cursor.artifacts_done` artifacts (already landed on a previous attempt). For each remaining artifact: mint `BlockId::new(ctx, principal, lane_seq)`, insert, then advance the lane counter **and** `artifacts_done` immediately. On insert/journal error: return `Err` with `high_water` un-advanced — the next beat retries the *same cell* and resumes at the failed artifact with the same id. No DuplicateBlock wedge, no silent repair: progress is loud, deterministic, and resumes the moment the fault clears. After the full group lands: `artifacts_done = 0; high_water += 1`.
   - **Source block**: `guard.materialize(cell, src_id)` (substrate untouched: text → Role::Model + inline + ContentType::Abc + tick; materialize.rs:28-55 as-is), then stamp `snapshot.ephemeral = true` kernel-side, then `insert_from_snapshot_as(ctx, snapshot, after, Some(principal))`.
   - **Derived sibling(s)**: built kernel-side —
     ```rust
     BlockSnapshotBuilder::new(sib_id, BlockKind::Text)
         .tick(cell.span.start)                       // same beat coordinate
         .role(Role::Asset)
         .content(hash.as_str())                      // 32-hex CAS pointer (img_block convention)
         .content_type(ContentType::from_mime(&dmime)) // Plain for audio/midi today
         .parent_id(src_id)                           // provenance: one-hop score↔render pairing
         .ephemeral(true)
         .build()
     ```
     (`parent_id` @ block.rs:2126, `ephemeral` @ :2241, `tick` @ :2246 — all existing builder methods, all existing wire fields.)
6. `after = last inserted id` threads through.

**Linkage decision — `parent_id`, not metadata, not track-name convention.** Existing capnp field (`parentId @1/@2`), already synced/persisted; "attached child" pattern established by Error blocks. A sink holding the MIDI resolves its score in one hop *today*, before F1's track wire fields exist. (Judge-2 graft, binding.)

**Ordering invariants preserved.** Both blocks share `tick = span.start` → same `order_key_for_tick`; the `(order_key, BlockId)` sort breaks the tie by seq → ABC deterministically before its MIDI. Monotone beat ticks → order keys append → CRDT order matches Tick order (docs/hyoushigi.md hard constraint). No per-track write barriers anywhere; the kernel beat thread remains the sole sequencer; tracks are id lanes and key suffixes only.

### 5b. FALLBACK STORY (mandatory)

- **UseLastGood fires** → `fire_fallback` (engine.rs:391-414) pushes a concrete copy of the last good ABC cref into `committed` (:402-407); its bytes are in RAM-CAS (crystallized at the original commit, engine.rs:376). The materializer cannot tell a fallback copy from a resolver commit — **it derives MIDI for both**. Same ABC → same MIDI bytes → same CAS hash (idempotent store) but a **new block pair at the new tick** — exactly what a pull-from-CAS sink (playback.md:28-32) needs to fire the repeat. No sink-side repeat mechanism, no fallback hook, no second code path: the recon's "fallback emits no sibling" risk is dissolved structurally. This satisfies chameleon.md's "do not build a second repeat mechanism" decision *better than Option A's fallback_links did*.
- **UseLastGood on an empty track** → per-track pool (F1) returns `None` → nothing pushed → Skip: silence until first good phrase (locked). Pinned by test T4 (currently unasserted).
- **Whole-turn-missed hole** (no cell scheduled at all → fallback never consulted): out of scope, recorded — the natural hook is the new `phrase_due` boundary (standing per-phrase UseLastGood cells, issues.md §13.4).

### 5c. When derivation itself fails

Committed `text/vnd.abc` that fails parse at the barrier violates the first-commit-validation invariant — corruption, not weather. `materialize_committed` bails before any insert for that cell; `high_water` stays; the cell retries (and re-fails) every beat — **loud head-of-line wedge, never a silent skip or a garbage block**. Surfacing: beat.rs's swallowed `log::warn` at :234 is upgraded to warn **plus one deduped `BlockKind::Error` block** via `insert_error_block_as` (kernel block_store.rs:2781), deduped by tracking the last-surfaced failing `high_water` index in BeatState so a persistent wedge produces one block, not one per beat. No timeout-then-skip — skipping would be a silent fallback. **Pre-agreed escape hatch (judge-3 graft, documented not built):** if a wedge proves operationally noisy, the remedy is an idempotent resume that *verifies the existing block matches* before continuing — written down here so it never gets hot-patched into silent repair under gig pressure. Trigger condition: a wedge whose cause is confirmed transient-but-recurring (journal contention), not data corruption.

## 6. Engine failure ledger + Failed-zombie removal (the only engine behavior change)

Today a resolve `Err` sets `CellState::Failed` and the cell **zombies in `future` forever** (engine.rs:297-302) — invisible, unreported. New behavior (minimal variant; judge 3's graft):

- New ledger sibling to `squashes`: `failures: Vec<FailureEvent>` with `FailureEvent { at: Tick, start: Tick, resolver: ResolverId, error: String }` (+ `track` when F1 lands) and a `failures()` accessor.
- On resolve `Err`: record the event, set state Failed (state-machine asserts intact), then **remove the cell from the open future** — the playhead passes a hole. No zombie.
- **`Failed` → `fire_fallback` routing is explicitly DEFERRED and flagged for Amy** (it amends the documented "crash over corruption, never a silent empty commit" stance at engine.rs:298-299). The minimal variant ships either way; the ledger/Error machinery is identical if the routing is later approved. Recorded in issues.md.
- `BeatScheduler` drains the ledger past a new `BeatState.failure_water: usize` cursor each `process_one`, inserting one `BlockKind::Error` block per event via `insert_error_block_as`. Error blocks hydrate (hydrate.rs:88-96) — **the player reads its own failure next turn, in the receiver's vocabulary** (chameleon producer-loop principle), and the ledger is the data source for the "ABC parse-failure rate" eval ruler.

Schedule-time pre-validation (§3) makes resolve-time ABC failures near-unreachable; the remaining class is CAS read failures.

## 7. on_turn_completed feedback-loop fix (same slice, mandatory)

**Verified forensics:** `TurnFlow::Completed` fires at stream *spawn* (rpc.rs:390-395; `spawn_llm_for_prompt` returns right after `spawn_local`, llm_stream.rs:305-324) — today's last-block read at beat.rs:274-292 races the model and plausibly reads the seed prompt. A filter-only fix inherits the race. **Pick: carry the output block id AND move the publish to actual stream completion, gated.**

- `TurnFlow::Completed` gains `output_block_id: Option<BlockId>` (flows.rs:982-987; `#[serde(default)]`; TurnFlow rides the in-process FlowBus only — never capnp, never journaled — grep-verified).
- `spawn_llm_for_prompt` gains `announce_completion: bool` (judge-2/3 graft — **mandatory**): `true` only at the turn-driver call site (rpc.rs:380); the interactive prompt paths (rpc.rs:2384, rpc.rs:4168) pass `false` — **zero behavior change for human-prompted turns**, so an armed musician context never schedules human-prompted output. Without this gate, B's publish-from-stream would have silently extended Completed to interactive paths.
- `process_llm_stream` publishes, for announced turns only: `Completed { context_id, principal_id, output_block_id }` at the end of its agentic loop on success — `output_block_id` = the last `Role::Model`/`BlockKind::Text` block this stream inserted (`None` if the turn produced no text) — and `Failed` on **every** terminal error path including interrupt/cancel (a wedged announced turn must not leave the scheduler waiting; the OODA cadence bounds residual damage to one lost Act handoff regardless). The premature publish at rpc.rs:391 is deleted; the Failed-on-spawn-error publish in rpc.rs stays for pre-spawn failures.
- `on_turn_completed(ctx, output_block_id)` (beat.rs:274-292): the last-block read is **deleted entirely**. `None` → no-op. `Some(id)` → fetch exactly that block, then defense-in-depth guards (judge graft — the loop dies three ways: explicit id, hydration-shield check, author check): refuse if `block.ephemeral || block.excluded`, refuse if `block.id.principal_id == PrincipalId::beat()` (or any track principal once F1 mints them) — each violation is a loud `log::error!` (a carried id pointing at a beat block is a bug, surfaced, not skipped silently). Empty content → no-op. Then `schedule_abc_cell(..., st.policy.phrase_delta())`; on `Err`, replace the swallowed warn at :289-291 with `insert_error_block_as` so the player reads its own rejection.
- `run()` (beat.rs:319-325) destructures `output_block_id` and passes it through.

## 8. Hydration — batch-1 mitigation: `ephemeral = true`

Once beat blocks are Role::Model + Text + Abc they hydrate as assistant text **every materialized phrase** including fallback duplicates (hydrate.rs:145-157) — the flood. Batch-1 fix (judges 2+3 overruled A's `excluded`): the kernel integrator stamps **both** the ABC source and the MIDI sibling `ephemeral = true` at insert (§5.5).

- `hydrate.rs:79-81` skips ephemeral → zero flood; **both** conversation paths route through `translate_block` (bootstrap hydration and the live `ConversationMailbox` feed/catch_up), pinned by T14.
- The app renders ephemeral blocks normally (kj-help precedent; no ephemeral gate in render.rs) → **the staff renders with zero app change**, and the block carries no user-staging checkbox semantics. This is the decisive edge over `excluded`: ephemeral is system-managed (block.rs:143-145 explicitly contrasts it with the user-toggled `excluded`), it lives outside the user-curation wire surface (`setBlockExcluded`, gutter checkbox), so no bulk re-include UX sweep can ever re-open the flood.
- Rejected alternatives, verified: `BlockKind::Trace` misses the staff render gate; `excluded` is user-curation surface; `Role::Tool` + Abc hydrates as a user envelope (hydrate.rs:198-214) — floods worse.
- **One-sentence doc addition** on `ephemeral` (block.rs:143) blessing machine-record (score) use — the semantic stretch is documented, not implicit. Flagged to Amy per feedback_show_memory_drafts (load-bearing direction), but shipped: both non-restart judges independently endorsed it.
- **Forward story (judge graft):** this is chameleon.md's hydration-marker special case — "new blocks are born excluded past the marker" — with the marker hardcoded to "all timeline-materialized blocks" and `ephemeral` as the bit. Batch 2's marker machinery (pinned prefix + sliding tail, windowed `$HEARD`, transport report) subsumes it; score blocks then migrate to the marker mechanism and `ephemeral` can revert to its narrow meaning. **Deferred to batch 2, explicitly:** all of that machinery. Accepted batch-1 consequence (status quo, strictly honest): the player observes only its own conversation history — committed timeline blocks are invisible to it until the transport report lands. Today's Asset-MIDI was equally invisible (hydrate.rs:196).

## 9. Second consumer: automation cells

An automation cell is a `cas_commit` cell with an automation mime (e.g. `application/vnd.kaijutsu.knob+json`) on the **same timeline** (knobs are cells; no second grid):

- resolve: pass-through (no validator registered; adding one is a kernel-side match arm).
- fallback: per-track `UseLastGood` works unchanged; because derived artifacts never enter `committed`, an automation track's pool can never pick up a MIDI hash — the mandated hygiene falls out of the same construction.
- materialize: no deriver registered → exactly one block (non-`text/` mime → Role::Asset + hash, materialize.rs:48-52; a type wanting inline JSON picks a `text/` mime). A future automation type needing a baked render registers a `Deriver` — identical mechanism, and the fallback path derives for its repeats identically. Whether siblings exist is decided by the registry, never by sink-side content-type filters (playback.md item 7 filters remain selection, not existence).

Pinned by T11.

## 10. `beats_per_phrase` (E)

```rust
pub struct BeatPolicy {
    pub period: Duration,
    /// Beats per phrase — the kernel's only musical chunking unit above the
    /// beat (phrases, not bars; bars live in ABC content and human-facing
    /// edges, translated at the edge). Plain u64 for now; the field shape is
    /// documented open to per-phrase counts (irregular phrases) later —
    /// consumers go through phrase_delta()/is_phrase_boundary(), never raw.
    pub beats_per_phrase: u64,
    /// Beats per OODA cadence (kept beat-denominated; the default is
    /// *expressed* in phrases). A phrase-typed field is deferred.
    pub ooda_every: u64,
}

pub fn musician_default() -> Self {
    Self { period: Duration::from_millis(500),
           beats_per_phrase: 16,      // "a 4-bar phrase in 4/4" collapses to 16 at this edge
           ooda_every: 8 * 16 }       // 8 phrases = 128 beats — numerically identical to the old 32*4; no cadence change
}

pub fn phrase_delta(&self) -> TickDelta { TickDelta::new(self.beats_per_phrase as i64) }
pub fn is_phrase_boundary(&self, beat_count: u64) -> bool {
    self.beats_per_phrase > 0 && beat_count % self.beats_per_phrase == 0
}
```

(Name: `phrase_delta()` — A's name over B's `phrase()`; it returns a TickDelta and says so.)

- `OODA_LEAD` (beat.rs:51) **deleted**; `on_turn_completed` schedules at `playhead + st.policy.phrase_delta()` — one phrase of lead (16 beats = 8 s at default; ample for a 20 ms resolve).
- Phrase-boundary computation lands beside the OODA check at beat.rs:241: `process_one` computes `is_phrase_boundary(beat_count)` and surfaces it as `BeatOutcome.phrase_due: Vec<ContextId>` (B's observable hook) — testable today, the seam for cue traps / quantized flush / standing cells tomorrow. With defaults every OODA boundary is also a phrase boundary (128 % 16 == 0).
- **Decide-later (acceptable per mandate, recorded in issues.md):** (1) BeatPolicy persistence across restart — note persistence alone is useless because *nothing re-arms contexts post-restart at all* (only Arm sender is createContext); the cold-start re-arm sweep and policy persistence are one work item. (2) `kj transport meter` inbound verb: `kj transport meter <beats_per_phrase>` with `--bars N --beats-per-bar M` convenience multiplying to beats at the edge → new `BeatCommand::SetMeter`; kj/transport.rs is the home and gets the first bars→beats translation test.

## 11. Restart semantics (fixed in-slice, per judge 1's binding grafts)

**What persists:** block log (CRDT oplog/snapshots in kernel.db), durable CAS, ContextRow. **What evaporates:** the Timeline (RAM committed Vec, RAM CAS, squash + failure ledgers, playhead), all BeatState (policy incl. beats_per_phrase, switches, beat_count, MaterializeCursor, failure_water), the armed registry. `Cell` is never persisted — zero migration cost for this fork's shapes.

1. **DuplicateBlock hazard — CLOSED in this slice** (not left as an F1 interlock). The lazy lane seed (§5.4) reads `max_seq_for_principal` from the block log at first touch, so any arm — including a future re-arm sweep — mints past persisted history. New kernel helpers: `max_seq_for_principal(context_id, principal)` and `max_tick(context_id)` (thin snapshot scans on the kernel BlockStore). Pinned by T17 (unit) **and** T20 (A's drop_and_reload end-to-end shape — judge graft).
2. **`high_water`/`artifacts_done` reset to 0** — consistent: the RAM committed log they index is also empty. They count this-process materialization only; the block-log seq lane is the durable cursor.
3. **Playhead seeding — CLOSED in this slice.** New additive engine method `Timeline::seed_playhead(&mut self, tick: Tick)` — sets the playhead on a *fresh* timeline only (crash if `committed` or `future` non-empty: seeding is initialization, never time travel). `BeatScheduler::arm` seeds from `blocks.max_tick(ctx)` when the timeline is fresh. Without this, post-restart beat blocks mint low ticks and `insert_from_snapshot`'s tick-derived order key (crdt block_store.rs:820) sorts the now-*visible* staves mid-document. Verified bonus: this hazard exists **without restart** — rc-created blocks consume CRDT ticks, so a fresh musician's playhead-0 cells can already sort below its stance blocks; arm-time seeding closes both. This is also chameleon's rotation tick-continuity invariant (chameleon.md:195-198) landing early.
4. **`next_tick` bump — CLOSED in this slice.** `insert_from_snapshot` (crdt block_store.rs:797-825) gains: if the snapshot carries a tick, `next_tick = max(next_tick, tick + 1)` — mirroring the `next_seq` pattern at :803-805 — so ordinary conversation appends after a beat block sort after it. **Coordinate with the F0 ordering fork** (same counter family as its calc_order_key/merge_ops work); pinned by T19.
5. **Fallback pool after restart:** RAM committed log empty → `UseLastGood` resolves to Skip. Restart IS an empty track: silence until the first good phrase — the locked decision applying uniformly. **Resolved (adopting B's reasoning over A's open question): we deliberately do NOT seed the pool from the block log** — a warm-restart vamp would be a silent stale repeat, the exact failure mode this project rejects; if the band wants warm-restart vamping it is an explicit future feature (issues.md).
6. **`beat_count` → 0:** OODA/phrase cadence phase restarts; acceptable until BeatPolicy persistence lands (recorded together).
7. **Re-arm sweep:** still missing (only Arm sender is createContext, rpc.rs:2648-2660); a restarted musician is silent until re-armed. Batch 1 accepts this; recorded as one issues.md item with policy persistence. **The difference from the advocates' versions: when the sweep lands, arming is already safe** — seeds 1 and 3 are in this slice.
8. **SIGKILL mid-group residue:** source ABC journaled, sibling not, timeline gone → no retry; the log holds an ABC block with no MIDI sibling. One un-renderable phrase; sinks must tolerate source-without-sibling (skip + note in playback.md). Loud at crash time, never silently doubled. Within a process the same window resumes per-artifact (§5.5).

## 12. F1 interlock and landing-order contingency

This design assumes F1 (Track on Cell; `PrincipalId::track(name)` UUIDv5 mint; per-track `UseLastGood`; per-track BlockId lanes; track wire fields @39+). Interlocks restated: (a) the lane seed scans **seqs present in the log per author principal**, never a known-tracks list; (b) `schedule_abc_cell` passes the cell's track; (c) on_turn_completed's author guard covers track principals.

**If F2 lands first** (resolved from B's open question): materialize everything under `PrincipalId::beat()` as the single implicit track — `cursor.lanes` keys on PrincipalId so it is already shaped for many lanes; F1 then only swaps the mint and re-keys nothing structural. The lazy log seed makes the beat() lane restart-safe immediately, fixing the pre-existing DuplicateBlock bug even in the single-track interim.

## 13. Wire changes

**capnp: ZERO new fields.** Everything rides existing surface: `parentId @1/@2` (sibling provenance), `tick @35/@36`, `ephemeral` (existing field + Lamport ts), `contentType @24` as open MIME text — `text/vnd.abc` is just a string; ContentType::Abc is a kernel/app-side projection. F1's track fields are that fork's wire change. Comment-only sweep: the stale tick @35 comment.

**CBOR: ZERO BlockSnapshot changes.** ephemeral/parent_id/tick all exist. **`ContentType::Midi` is deliberately NOT added** — a new closed-enum variant breaks old decoders (ContentType rides BlockHeader inside SyncPayload ops; the codec is fail-loud by design); per the project rule a variant lands with its first consumer — playback slice 2. Rationale recorded verbatim in issues.md (judge-2 graft). Interim sink key: `Role::Asset && parent_id → block with ContentType::Abc` (one hop), authoritative mime in the CAS sidecar.

**FlowBus (in-process only, never capnp/journaled):** `TurnFlow::Completed.output_block_id: Option<BlockId>` with `#[serde(default)]` — additive, wire-inert; sole non-test subscriber is beat.rs.

**Mixed-version:** an older app sees only fields it knows — `text/vnd.abc` projects to its existing Abc variant (staves render); `audio/midi` projects to Plain exactly as today; parentId/tick/ephemeral are long-shipped. Order keys still append-at-tick → no ordering skew for older replicas. A frozen old-shape CBOR fixture test lands in kaijutsu-types codec tests as the general regression net the recon flags as missing (T21).

**App impact: zero code change.** ABC block → existing staff path (Role::Model + Text + ContentType::Abc, render.rs:134-152); MIDI sibling → existing ASSET hash row; no ephemeral gate in the render path.

## 14. Touch list

1. `crates/kaijutsu-kernel/src/hyoushigi/mod.rs:29-56` — BeatPolicy gains `beats_per_phrase` + `phrase_delta()` + `is_phrase_boundary()`; musician_default `{500ms, 16, 8*16}`; doc comments drop bar-speak.
2. `crates/kaijutsu-kernel/src/hyoushigi/mod.rs:110-112` — register `CasCommitResolver`; `abc_to_midi` registration removed.
3. `crates/kaijutsu-kernel/src/hyoushigi/mod.rs:123-147` — `schedule_abc_cell`: eager parse (loud Err), recipe → cas_commit `{hash, mime}`; caller passes `phrase_delta()` lead; track param with F1.
4. `crates/kaijutsu-kernel/src/hyoushigi/mod.rs:159-234` — `AbcToMidiResolver` → `CasCommitResolver` (validate-by-mime; parse logic becomes the abc validator); new `Deriver` trait + `DeriverRegistry` + `AbcToMidiDeriver` (parse + to_midi moved verbatim).
5. `crates/kaijutsu-kernel/src/hyoushigi/mod.rs:255-304` — `materialize_committed` rewrite per §5: MaterializeCursor + DeriverRegistry params; bytes-missing bails (kills the :280 silent skip); derive-before-insert; lazy lane seed; per-artifact resumable insert; sibling build (parent_id, same tick, Role::Asset, ephemeral); ephemeral stamp on the source.
6. `crates/kaijutsu-kernel/src/hyoushigi/mod.rs` tests (~:378-490) — rewrite `abc_to_midi_resolver_crystallizes_midi_from_cas` + the bridge test to the new shape; rename `abc_to_midi_rejects_malformed_hash` → cas_commit equivalent (behavior kept); new tests per §16; vamp fixture via `include_str!`.
7. `crates/kaijutsu-hyoushigi/src/engine.rs` — additive only: `failures: Vec<FailureEvent>` ledger + accessor; resolve-Err path records + removes the cell from `future` (no zombie; hole); `seed_playhead(tick)` with freshness guard. **No changes to commit/squash/fallback/emitted.**
8. `crates/kaijutsu-server/src/beat.rs:47-66` — delete `OODA_LEAD` (:51); BeatState: `high_water: usize` → `MaterializeCursor`, add `failure_water: usize` + last-surfaced-wedge index (dedupe).
9. `crates/kaijutsu-server/src/beat.rs:110-122` — `arm()`: seed playhead from `blocks.max_tick(ctx)` when the timeline is fresh.
10. `crates/kaijutsu-server/src/beat.rs:212-242` — `process_one`: pass cursor + registry; upgrade the :234 swallowed warn (warn + deduped Error block); drain `failures()` past `failure_water` → Error blocks; compute `is_phrase_boundary` beside :241; `BeatOutcome.phrase_due`.
11. `crates/kaijutsu-server/src/beat.rs:274-292` — `on_turn_completed(ctx, output_block_id)` per §7: exact-block fetch, guards, last-block scan deleted, schedule errors → Error block.
12. `crates/kaijutsu-server/src/beat.rs:297-336` — `run()`: destructure `output_block_id`.
13. `crates/kaijutsu-server/src/beat.rs` tests (~:383-660) — rewrite `turn_completion_schedules_abc_and_midi_materializes` (:608-659); new tests per §16; vamp fixture replaces the inline C-major scale at :624.
14. `crates/kaijutsu-kernel/src/flows.rs:982-987` — `Completed.output_block_id: Option<BlockId>` `#[serde(default)]`; doc the publish-at-actual-completion + announce-gated semantics.
15. `crates/kaijutsu-server/src/llm_stream.rs` — `spawn_llm_for_prompt` gains `announce_completion: bool` (false at rpc.rs:2384/4168, true at rpc.rs:380); `process_llm_stream` publishes Completed-with-id on success / Failed on every terminal error incl. interrupt, announced turns only.
16. `crates/kaijutsu-server/src/rpc.rs:390-395` — delete the premature Completed publish (Failed-on-spawn-error stays); thread `announce_completion`.
17. `crates/kaijutsu-kernel/src/block_store.rs` — new helpers `max_seq_for_principal(ctx, principal)`, `max_tick(ctx)`.
18. `crates/kaijutsu-crdt/src/block_store.rs:797-825` — `insert_from_snapshot` bumps `next_tick` past a carried tick (coordinate with F0).
19. `crates/kaijutsu-abc/tests/fixtures/chameleon_vamp.abc` — NEW: B♭ Dorian two-chord vamp. Constraints: `K:Bb dorian`, `M:4/4`, `L:1/8`, `Q:1/4=120` (matches musician_default; the ~94 BPM Head Hunters tempo is a fidelity nicety tests don't depend on — resolved, cosmetic), B♭m7–E♭7 alternation, exactly 4 bars = one 16-beat phrase, parses with zero errors, `to_midi` emits MThd. Modeled on `simple_melody.abc`.
20. `crates/kaijutsu-types/src/block.rs:143` — one-sentence doc addition blessing machine-record (score) use of `ephemeral`.
21. `crates/kaijutsu-types` codec tests — frozen pre-F2 CBOR fixture decode test (general additive-evolution net).
22. `assets/defaults/rc/musician/create/S00-stance.md` — :5 "every N bars" → "every N phrases"; :14 "The system crystallizes your ABC into MIDI on the beat" → "The system commits your ABC to the shared score on the beat; MIDI is derived from the score for playback". **Strict honesty:** no claim that the player can observe committed layers until the transport report exists.
23. `assets/defaults/rc/musician/tick/S10-drive.kai` — :4 "(every N bars; default 32 bars @ 120 BPM)" → "(default: every 8 phrases of 16 beats @ 120 BPM)"; :6 abc_to_midi wording → derivation-at-materialization wording.
24. `docs/chameleon.md` — amend :57-58 and :119: "derived MIDI is an emitted sibling cell" → "derived MIDI is a derived sibling block, paired at the write barrier (parent_id provenance)"; mark foundational change 2 landed; record phrase defaults.
25. `docs/hyoushigi.md` — status sweep: notation-first landed; bar-speak → phrases; "seed playhead on re-arm" open item → landed.
26. `docs/playback.md` — item 8 note: abc→midi is now a Deriver, not a timeline resolver — slice 3 redesigns the pcm chain (deferred pcm cell keyed on the derived MIDI hash, or a measured budget-excepted deriver); item 7 note: interim sink key = Asset + parent→Abc; sinks must tolerate source-without-sibling (crash residue, §11.8).
27. `docs/issues.md` — record: (1) BeatPolicy persistence + cold-start re-arm sweep (one item); (2) `kj transport meter` (+ `--bars` edge translation, `BeatCommand::SetMeter`); (3) Failed→fallback engine routing (deferred, needs Amy; minimal variant shipped); (4) standing per-phrase UseLastGood cells (whole-turn-miss hole; hook = phrase_due); (5) midi→pcm re-anchor (playback slice 3, two candidate shapes); (6) ContentType::Midi deferral rationale (CBOR closed-enum skew); (7) journal_op/compact_document silent no-op without db handle (kernel block_store.rs:711-713) — beat blocks ride this path; (8) deriver-budget enforcement beyond convention (timed debug_assert); (9) duration-summing beats-per-phrase ruler in kaijutsu-abc (tuplet spec = midi.rs:261-274); (10) fallback-pool warm-restart seeding decided NO (stale silent repeat) — explicit future feature if wanted; (11) in-RAM committed Vec/CAS growth (rotation is the answer); (12) ooda_every stays beat-denominated — phrase-typed field revisit.

## 15. User-level calls explicitly flagged for Amy

1. **Failed→fallback engine routing** (§6) — deferred with the minimal variant shipped; approving it later is a drop-in.
2. **`ephemeral` semantic blessing** (§8) — shipped per two judges, but it is load-bearing direction (feedback_show_memory_drafts): confirm before merge.
3. **The §5c wedge escape-hatch trigger** — pre-agreed remedy text; confirm the trigger condition wording.

Everything else the advocates left open is resolved above: interactive turns do not announce (§7); pool not seeded on restart (§11.5); fixture tempo 120 (§14.19); F2-first contingency (§12); ooda_every stays beat-denominated (§10); phrase_delta naming (§10); rc prose strictly honest (§14.22).

## 16. Ordered failing-first test plan

Run `cargo check -p kaijutsu-kernel` first to confirm main is still green (the old compile break is fixed; this is a check, not a work item). Order = dependency order; each test is written red before its implementation step.

- **T1** `kaijutsu-abc` tests: `chameleon_vamp_fixture_parses_and_renders_midi` — fixture parses (mode Dorian, no errors), `to_midi` starts with MThd. *Red: fixture absent.* Lands the shared phrase for everything after.
- **T2** kernel mod.rs: `musician_default_speaks_phrases` — beats_per_phrase==16, ooda_every==128, phrase_delta()==TickDelta(16), is_phrase_boundary(16|32) true / (17) false. *Red: compile.*
- **T3** beat.rs: `phrase_boundaries_reported_every_beats_per_phrase` — beats_per_phrase=4 → phrase_due at beats 4,8,12 only. *Red: no phrase_due.*
- **T4** engine.rs: `use_last_good_with_empty_history_resolves_to_skip` — squash-no-budget + empty committed → nothing commits, no panic. *Pin of the locked decision; may pass immediately (currently unasserted) — write it first anyway so F1 inherits a green pin.*
- **T5** engine.rs: `resolve_failure_records_event_and_removes_cell` — erring resolver → `failures()` has one event with the error string, cell gone from `future` (no zombie), committed empty (hole). *Red: cell zombies, no ledger.*
- **T6** engine.rs: `seed_playhead_only_on_fresh_timeline` — seeds on fresh; panics/errors after a commit. *Red: method absent.*
- **T7** kernel mod.rs: `malformed_abc_rejected_at_schedule` — `schedule_abc_cell("not abc")` Errs; future stays empty. *Red: no schedule-time validation.*
- **T8** kernel mod.rs: `score_cell_commits_abc_and_derives_midi_sibling` (rewrites the :431-479 test + the bridge test) — schedule vamp via cas_commit, advance, materialize: block[0] Role::Model, content==ABC text, ContentType::Abc, ephemeral, tick==cell tick; block[1] Role::Asset, 32-hex hash, parent_id==block[0].id, same tick, ephemeral, CAS bytes start MThd; second materialize pass inserts nothing (idempotency incl. siblings). *Red: committed cell is MIDI; one block; no parent link; no ephemeral.*
- **T9** kernel mod.rs: `fallback_committed_abc_also_derives_midi` — commit one good phrase; force UseLastGood on the next (squashes_and_falls_back pattern); materialize: the fallback copy gets its own MIDI sibling at the fallback tick, hash-identical to the original, distinct BlockIds. **THE mandated fallback verification.** *Red: fallback phrases produce no MIDI.*
- **T10** kernel mod.rs: `derivation_failure_materializes_nothing_and_bails` — committed text/vnd.abc cell with garbage bytes (Fixed test resolver bypassing validation): Err, zero blocks, high_water unchanged; second call errs again. *Red: garbage inlines happily.* Companion: `materialize_bails_on_missing_bytes` — committed cref with bytes in neither RAM-CAS nor durable CAS → Err, never a dangling-hash block (pins the :280 fix). *Red: silent skip.*
- **T11** kernel mod.rs: `automation_mime_materializes_without_sibling` — unregistered mime → exactly one block, Ok. *Red: signature lacks registry.*
- **T12** engine/kernel: `committed_log_never_contains_midi` — full schedule/commit/fallback/materialize cycle; assert no `audio/midi` cref ever appears in `committed()` (hygiene pin so a future emitted-cell experiment can't silently regress the pool purity). *Trivially green by construction once built — pinned deliberately.*
- **T13** kernel mod.rs: `partial_insert_resumes_per_artifact` — fault-injecting block store fails the sibling insert once: first call Errs with source inserted + artifacts_done==1 + high_water unchanged; second call inserts only the sibling (same id), then advances; no DuplicateBlock, no doubled source. *Red: cursor shape absent.*
- **T14** kernel hydrate tests: `materialized_score_blocks_are_hydration_silent` — materialize-shaped ABC+MIDI snapshots through HydrationState **and** a ConversationMailbox catch_up → zero messages; a non-ephemeral Model/Text control block produces one (proves the test can fail). *Red against the new shape without the stamp: ABC floods as assistant text.*
- **T15** server (mock-provider harness, e2e_kj_workflow.rs style): `turn_completed_publishes_at_stream_end_with_output_block_id` — subscribe turn.completed, drive an autonomous turn: event arrives only after the model block exists and carries its id; drive an interactive prompt: nothing published. *Red: published at spawn, no field.*
- **T16** beat.rs: `materialized_abc_is_not_rescheduled_on_turn_completed` — Completed{None} → nothing scheduled; an id pointing at an ephemeral/beat-principal block → refused with loud log, nothing scheduled; a real model block id → scheduled. *Red: blind last-block read schedules the beat block — the silent feedback loop.*
- **T17** beat.rs (rewrites :608-659): `completed_turn_schedules_one_phrase_ahead` — cell start == playhead + phrase_delta() (16, not 4); after advancing, the ABC(ephemeral Model staff)+MIDI(Asset hash) pair materialized. *Red: signature, lead=4, single MIDI block.*
- **T18** beat.rs: `track_lane_seq_seeds_from_block_log_no_duplicate_after_rearm` — pre-insert beat-principal blocks seq 0..2; fresh scheduler, arm, play, commit: materialization succeeds at seq 3+, no DuplicateBlock. *Red: lane starts 0, collides (the known latent poison loop).*
- **T19a** beat.rs: `playhead_seeds_from_max_block_tick_on_arm` — doc with blocks to tick 40; arm fresh; first materialized block has tick > 40 and sorts last. *Red: playhead 0, mid-document sort.* **T19b** crdt block_store.rs: `insert_from_snapshot_bumps_next_tick` — insert snapshot tick 50, then an ordinary append: tick > 50, sorts last. *Red: next_tick never advanced by snapshot inserts. Coordinate with F0.*
- **T20** kernel (drop_and_reload harness, block_store.rs:4158 pattern): `materialize_after_restart_does_not_collide` — persist beat-materialized blocks, reload through the real restore path, re-arm, play one beat: no DuplicateBlock, new blocks sort after old (end-to-end pin over T18/T19). *Red until both seeds land.*
- **T21** beat.rs: `resolve_failure_surfaces_error_block` — erring resolver on the timeline; process_one drains the ledger → exactly one BlockKind::Error block; failure_water advances; no duplicates on later beats. *Red: ledger drain absent.*
- **T22** (run, not write): timing print in T8 confirming derive+insert per cell ≪ 1 ms — the deriver-budget convention gets a measured number; plus kaijutsu-types frozen-CBOR fixture decode test (§14.21).
- **T23** keep-green sweep: existing 17 hyoushigi engine tests, the CRDT ordering/tick suite, transport.rs tests, flows subject/topic tests — none change behavior except where rewritten above. rc prose verified by grep for `bar`/`crystalliz` under `assets/defaults/rc/musician/`.

## 17. Implementation sequencing

1. **Fixture + policy** (T1, T2, T3): vamp fixture; BeatPolicy fields/helpers; `OODA_LEAD` deleted; `phrase_due` on BeatOutcome. Small, compile-driven, unblocks all later tests.
2. **Engine additive items** (T4, T5, T6): empty-pool pin; failure ledger + zombie removal; `seed_playhead`. Contained to kaijutsu-hyoushigi (1.9 s test loop).
3. **Resolver swap + scheduler-side validation** (T7): CasCommitResolver in, abc_to_midi out, schedule-time parse.
4. **The materializer** (T8-T13): DeriverRegistry, MaterializeCursor, the §5 rewrite — the heart of the fork; fallback derivation (T9) falls out and is verified here.
5. **Hydration stamp** (T14): ephemeral on both artifacts; doc-comment blessing.
6. **Turn-completion plumbing** (T15-T17): flows field, announce_completion threading, publish move, on_turn_completed rewrite with guards, Error-block surfacing for schedule failures.
7. **Restart seeds** (T18-T20): kernel store helpers, lazy lane seed wiring, arm-time playhead seed, crdt next_tick bump (sync with F0 before merging this step), drop_and_reload e2e.
8. **Failure surfacing in beat.rs** (T21): ledger drain + deduped materialize-wedge Error block.
9. **Sweeps** (T22-T23): timing number, frozen CBOR fixture, rc prose, chameleon.md/hyoushigi.md/playback.md/issues.md updates — same commit as the behavior change so the kernel never describes behavior it doesn't have.

Steps 1-3 and 6 are independent of 4-5 and can land in either order; 7 depends on 4; 8 depends on 2 and 4. F1 coordination: if F1 lands mid-stream, only step 4's principal mint and `schedule_abc_cell`'s track param change (§12).
