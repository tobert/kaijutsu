# FORK 1 — Track identity through materialization, jointly with order/tick decoupling

**FINAL DESIGN (synthesized 2026-06-11).** Winning shape: **Option B — track as a first-class, write-once data field**, per the 2-1 judge verdict (wire-evolution and chameleon-fit lenses for B; the restart lens preferred A but every one of its restart-correctness demands is grafted here). All file:line references re-verified against main on 2026-06-11; the workspace compiles clean (`cargo check -p kaijutsu-kernel -p kaijutsu-crdt -p kaijutsu-hyoushigi` exits 0 — both advocates' "step zero: kaish_backend.rs doesn't compile" claim is **stale**; `resident_bytes: None` is present at runtime/kaish_backend.rs:737. Do not schedule fix work for it).

---

## 0. Locked invariants (restated, not relitigated)

- **Track** = stable lane identity (DAW sense); the track persists while players come and go; **the scheduling principal separately records who played** (docs/chameleon.md:33-39). "Voice" reserved for ABC `V:`; "lane" reserved for in-track automation.
- `order_key` stays the CRDT sibling-ordering index; `Tick` stays the kernel's semantic coordinate; materialization emits order_keys that **append** so CRDT order matches Tick order. NO per-track write barriers; the kernel is the sole timeline sequencer. The 4-char agent suffix (crdt block_store.rs:272-281) remains the multi-writer tiebreak — it is **never** repurposed as track identity.
- Kernel meter is `beats_per_phrase`; bars live in ABC/human edges only (not touched by this fork).
- `UseLastGood` on an empty track → Skip (silence). Crash over corruption; no silent fallbacks. TDD: every change below has a named failing-first test (§8).

## 1. Identity model (the fork decision)

### 1.1 `TrackId` — new file `crates/kaijutsu-types/src/track.rs`, re-exported from lib.rs

```rust
/// Stable lane identity on a timeline (DAW sense). The track persists while
/// players come and go; BlockId.principal_id separately records who played.
/// Lane identity ONLY — never an ordering, barrier, or authorship concept.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct TrackId(String);

impl TrackId {
    /// Strict: 1..=64 chars of [a-z0-9_-]. Lowercase-only kills case-aliasing;
    /// charset matches chart/kj-arg ergonomics ("bass", "drums", "keys").
    /// Err on violation — never silent normalization.
    pub fn new(s: impl Into<String>) -> Result<Self, TrackIdError>;
    /// Total, documented, deterministic mapping from a human label:
    /// lowercase, map invalid chars to '-', collapse runs, trim edge '-',
    /// truncate to 64. Returns None when the result is empty.
    /// Emits tracing::info when the slug differs from the input.
    pub fn slugify(label: &str) -> Option<Self>;
    /// The single-lane musician's default chair until band config exists.
    pub fn solo() -> Self;  // "solo"
    pub fn as_str(&self) -> &str;
}
```

String, not UUID: tracks are human/chart-addressable (`$HEARD` keys, `V:`→track maps, kj args); the value is **self-describing on every block** — no registry, no second source of truth, no one-way mint. Rename = a new track (forward-only, like everything else; `UseLastGood` restarts from silence on the new name — documented, no identity-severance machinery needed because there was no derived identity). Both validations the judges asked for are present: empty/whitespace rejection (via the charset rule) and the lowercase charset.

### 1.2 What lands on a materialized block

- **`BlockSnapshot.track: Option<TrackId>`** (data) — `Some` only on blocks materialized from a committed timeline cell, making `track.is_some()` the clean "this block came off the timeline" discriminator. `#[serde(default)]`, write-once at creation (like `tool_name`), never LWW-merged, travels only in `new_blocks` snapshots — `BlockHeader` is untouched, no per-field Lamport clock.
- **`BlockId.principal_id` = who played** (structure, unchanged in meaning): resolver-derived cells are minted under the principal whose turn produced the source ABC; engine-fallback repeats and `Literal` fills are minted under `PrincipalId::beat()` — the transport played them; attributing a vamp-insurance repeat to the player would be false provenance. This keeps `BlockSnapshot::author()`, the capnp "author derived from id.principalId" convention (kaijutsu.capnp:139), the recorded actor-is-Principal invariant, and the locked chameleon wording all intact with **zero** new author plumbing.
- `PrincipalId::beat()` (ids.rs:199-214) survives as the transport's own author lane; its doc comment is rewritten from "author of hyoushigi-materialized cells" to "author of transport-played cells (fallback repeats, literals); player-derived cells are authored by the player." Legacy persisted beat() blocks keep their ids and have `track = None`.
- **Invariant, documented in types, engine, and kernel hyoushigi:** one track's blocks span multiple principals (player + beat). **Principal is never a lane key; `track` is the only lane identity. `track == None` matches no track** — a legacy/untracked block can never satisfy another track's `UseLastGood` or future `$HEARD` query.

### 1.3 Why B over A (synthesizer's note)

The restart-lens judge preferred A, but every restart-correctness item it credited A for (fork-variant seeding, virgin-only Err-ing playhead seed, all-principal seq lanes) is option-agnostic infrastructure and ships here (§3, §4). A's residual advantages do not survive its costs: (a) A inverts `BlockId.principal_id` from "who acted" to "which lane" — against the literal locked wording and the architecture_agent_emerges_not_noun invariant, permanently and invisibly to every future consumer; (b) UUIDv5 is one-way, so A needs a `context_tracks` registry — a second source of truth that does not travel with blocks across fork-to-another-kernel, app replicas, or cold forensics, the exact skew argument A itself used against counter tables; (c) the wire-lens judge found A's `played_by` field was dead-on-arrival as specified (never threaded through `BlockContent`'s copy lists) — fixable, but it demonstrates the field-threading trap §5.3 now guards structurally; (d) rename severs lane history under a name-derived mint; (e) a musician reads a DAW track as where a clip lives, not who recorded it. A's "free per-track order-key suffix" is not needed: ordering never depends on the suffix once append keys are successor-derived (§2.1).

## 2. Order/tick decoupling (common base, fully specified)

### 2.1 Key-successor primitive — `crates/kaijutsu-crdt/src/content.rs`, beside `order_midpoint` (:51)

```rust
/// Decode a fixed-width base62 string to i64. None if any char is not
/// base62 or the value overflows.
pub(crate) fn base62_decode(s: &str) -> Option<i64>;

/// The canonical append key strictly after `pred`, carrying `suffix`
/// (the inserting agent's 4-char lane).
///
/// Canonical fast path: `pred` starts with 'V' + 11 base62 chars (the
/// order_key_for_tick shape, block_store.rs:289-295; trailing suffix chars
/// ignored) → decode the 11-char number n, checked n+1, re-encode width 11
/// → "V{n+1}{suffix}". O(1), shape-identical to every existing canonical key.
///
/// General fallback (legacy/variable-length pred: order_midpoint results,
/// "{after}V" appends, "{:020}" decimal restore keys — which sort below 'V'
/// since '0' < 'V' — or n+1 overflow at i64::MAX): "{pred}V{suffix}" —
/// strictly greater than pred by prefix extension. tracing::warn on this
/// path (it grows keys and should be rare); never wraps.
pub(crate) fn order_key_successor(pred: &str, suffix: &str) -> String;
```

Postcondition at every call site: `debug_assert!(new_key > pred)`; in release, if the comparison would fail (cannot by construction — belt and braces) `tracing::error!` + general fallback. No silent path.

### 2.2 Wiring into `calc_order_key` — crdt block_store.rs:316-377 (three arms)

1. **`after == None`, `tick.is_some()`** (:327-328): if the store is empty, keep `order_key_for_tick(t)` (the only case where the tick legitimately seeds the keyspace); otherwise return `order_key_successor(last_key, &suffix)` where `last_key` is the current tail's key.
2. **Append-after-last** (:348-349): return `order_key_successor(&after_key, &suffix)` — the documented bug arm (it currently reads `next_tick` while already holding `after_key`).
3. **after-not-found fallback** (:362-363): same substitution against `ordered.last()`.

`next_position` (:299-304) still stamps the block's **tick** from `next_tick` — the tick remains the semantic coordinate on every block; what changes is that the **order key** never derives from any counter on appends, so a stale counter structurally cannot mis-sort. `order_key_for_tick` survives for: empty-store first insert and `normalize_timeline` re-keying (:379-414). `insert_from_snapshot` (:797-825) needs no code change — it delegates to `calc_order_key(after, snapshot.tick)` at :820 — but its :811-819 comment is rewritten (tick = pure coordinate; append key = successor of predecessor).

**Consequence (documented in both doc comments):** same-tick blocks previously shared a key with BlockId tiebreak (:816-818); successor keys are strictly increasing, so within-beat order = insertion order at the sole sequencer. Ties at the tick coordinate remain *allowed* (shared-coordinate doctrine, types/block.rs tick docs) — grep confirms no code requires key-equality-at-a-tick.

**Observability graft:** in `insert_from_snapshot`, when appending a snapshot whose `tick` is **less** than the predecessor block's tick, emit `tracing::warn!` — ordering is safe by construction now, but a regressing tick indicates an upstream seeding bug and must be heard.

### 2.3 `merge_ops` restores the tick high-water — crdt block_store.rs:1109-1180

In the `new_blocks` loop (:1121-1138), track `max_tick = max(snap.tick)`; after the loop: `if let Some(t) = max_tick { self.next_tick = self.next_tick.max(t.get() + 1); }` — mirroring the `next_seq` pattern at :1134-1136. This keeps freshly-stamped ticks monotone after the real restore path (kernel block_store.rs load_from_db :2013 → `from_snapshot` → `merge_ops` per oplog row). With §2.2 this is **semantic** correctness (tick values), pinned separately from ordering correctness (test T3 vs T2, per the restart judge's graft).

**Resolved (advocate B's open question 7):** the key-less `new_blocks` fallback at :1123 (`format!("{:020}", self.blocks.len())`) mints decimal keys that sort below every `V` key — a latent PREPEND. No live path sends key-less new blocks today; since this function is already being edited, change the fallback to `order_key_successor` of the current tail (or `order_key_for_tick(next_tick)` when empty), with test T5. Two lines; removes the hazard class this slice exists to kill.

### 2.4 Fork variants seed their counters (judge-mandated, ships in this slice)

`fork` / `fork_at_version` / `fork_filtered` (crdt block_store.rs:1202-1236, :1241-1283, :1290-1349) build via `Self::new` (`next_tick = 0`) with own-principal-only seq guards (:1220-1222, :1267-1269, :1333-1335) and never normalize. Chameleon **rotation is shallow fork** (docs/chameleon.md:179-187) with a hard tick-continuity invariant (:195-198) — so as written, every rotation would re-create the DuplicateBlock hazard and mint duplicate low ticks durably. Fix in all three loops: `forked.next_tick = forked.next_tick.max(snap.tick + 1)` and per-principal lane seeding for **every** copied block's principal (§3). Test T4 is parameterized over all three variants; descoping any variant is forbidden silently — it would go to docs/issues.md loudly, but the design ships all three.

## 3. Seq lanes: the DuplicateBlock fix (structural, not patched)

**Domain:** BlockId uniqueness is per **(context, principal)** (types/block.rs BlockId invariant), not per track — two tracks may share a scheduling principal (one model on two chairs; beat() covering fallbacks on every track). So the counter is keyed by principal; tracks sharing a principal share a lane safely (seq is a row id; the musical coordinate is `tick`).

**Home: the CRDT store, seeded from the loaded block log.** Replace `next_seq: u64` (crdt block_store.rs:80) with:

```rust
/// Per-principal next-seq lanes. next seq for P = max seq of ANY block in
/// this doc minted under P (tombstones included) + 1. Maintained on every
/// insert/merge/restore/fork path; the block log is the single source of
/// truth and these lanes are derived from it.
seq_lanes: HashMap<PrincipalId, u64>,
```

- `new_block_id` (:256-260): read/bump the current `self.principal_id`'s lane — own-lane behavior identical.
- **Delete the `== self.principal_id` guard** (the bug class — it is exactly why beat()'s lane was invisible to restore) at: `insert_from_snapshot` :803-805, `merge_ops` :1134-1136, `from_snapshot` :1403-1405, and all three fork loops; update the lane for every observed principal. `from_snapshot` tombstone restore (:1422-1429) also seeds lanes so a deleted block's seq is never re-minted.
- New crdt API: `pub fn reserve_block_id(&mut self, principal: PrincipalId) -> BlockId` — bumps the lane and returns the id without inserting; plus `pub fn next_seq_for(&self, principal: PrincipalId) -> u64` (test accessor). Kernel passthrough `BlockStore::reserve_block_id(&self, context_id, principal) -> BlockStoreResult<BlockId>` under the document entry lock (the pattern of `insert_from_snapshot_as`, kernel block_store.rs:1171-1184). Reserve-then-insert leaves a seq gap if the insert fails — benign (BlockId requires monotonic-unique, not dense); document so nobody "fixes" it. `CrdtError::DuplicateBlock` (:807-809) remains the loud backstop. Reserve/insert as two lock acquisitions is safe: reserve atomically claims its seq under the lock; the single-kernel sole-sequencer invariant covers the rest.

**The hazard, fixed:** today restart → persisted beat() blocks seq 0..n load, but next_seq recovery skips foreign principals; re-arm resets `BeatState.high_water = 0` (beat.rs:119); the first materialization mints `BlockId::new(ctx, beat(), 0)` (kernel hyoushigi/mod.rs:288) → DuplicateBlock → swallowed as `log::warn` (beat.rs:234) with high_water un-advanced → **silent infinite retry every beat**. After this design, seq never derives from `high_water`; ids come from `reserve_block_id(ctx, cell.played_by)` whose lanes are seeded from the loaded log — first post-restart materialization mints max+1 in the correct principal's lane. The fix covers beat(), players, system(), drift authors — the legacy lane is repaired retroactively. Pinned by T9.

**Rejected:** a KernelDb counter table — wrong key (per-track), dual source of truth vs the oplog, transactional coupling with `journal_op` (kernel block_store.rs:706) whose failure mode is this same bug returning, and per-cell write amplification at tempo. Log-seeding costs one O(blocks) pass `from_snapshot` already makes for lamport seeding (:1432-1439).

## 4. Playhead seeding on re-arm (closes the hyoushigi.md open item)

- `Timeline::seed_playhead(&mut self, at: Tick) -> Result<(), SeedError>` (engine.rs): **Err unless the timeline is virgin** (`playhead == Tick::ZERO && future.is_empty() && committed.is_empty()`) — crash over corruption; a seed attempt on a live timeline is always a caller bug and must be loud in release (judge-mandated over B's original silent forward-only return). Fires no lifecycle actions.
- `Kernel::arm_timeline(context_id, clock, seed: Tick)` (kernel.rs:627-640): seed applied **inside `or_insert_with`** on the freshly-constructed timeline (`.expect()` there — virgin by construction), so idempotent re-arm of a live timeline never re-seeds (preserves the "never clobber an open future" contract :620-622 and arm_is_idempotent, kernel hyoushigi/mod.rs tests).
- New kernel-store helper `BlockStore::max_tick(&self, context_id) -> BlockStoreResult<Option<Tick>>` (beside `last_block_id`, kernel block_store.rs:693) — max over live blocks' ticks; arm is rare, a scan is fine.
- `BeatScheduler::arm` (beat.rs:110-122) computes `documents.max_tick(ctx)` and passes it to `arm_timeline`. Correct under the shared-coordinate doctrine even when conversation inserts advanced the coordinate past the last beat: musical time is global and monotone per context (chameleon.md:195-198).
- Interaction with `insert_from_snapshot`'s tick-derived placement (:820): post-§2.2 an unseeded playhead can no longer mis-SORT; seeding keeps the **semantic** tick honest (windowed reads, `$HEARD`, rotation). Both land together. Note: nothing re-arms musicians after restart today (the only Arm sender is createContext, server rpc.rs:2648) — this design makes re-arm *safe whenever wired*; the re-arm sweep stays on the not-yet list (issues.md).

## 5. Track threading

### 5.1 `Cell` — kaijutsu-hyoushigi/src/cell.rs:113-137

```rust
pub struct Cell {
    pub span: Span,
    pub body: Body,
    pub state: CellState,
    /// The lane this cell belongs to. Required — an untracked cell is
    /// meaningless once tracks are first-class.
    pub track: TrackId,
    /// Who played: the principal whose turn produced this content, or
    /// PrincipalId::beat() when the transport itself did (fallback repeats,
    /// literals). Becomes BlockId.principal_id at materialization.
    pub played_by: PrincipalId,
}
```

Constructors become `Cell::concrete(span, content, track, played_by)` / `Cell::deferred(span, recipe, track, played_by)` — compile-fail drives every call site. **No serde defaults**: Cell is not persisted or wire-carried anywhere today (engine committed log is in-RAM, engine.rs:126-130; grep-verified no kernel encode), so requiring both fields is free now and fail-loud when chameleon's cell persistence lands — a stored track-less cell is corruption and the codec should say so.

### 5.2 Engine — kaijutsu-hyoushigi/src/engine.rs

- `Timeline::schedule` (:177-213): signature unchanged; the cell carries its track.
- `fire_fallback` (:391-414): `UseLastGood` calls `last_committed_content_in(&cell.track, span.start)` — rename of `last_committed_content` (:437-447) gaining a track filter (`.filter(|c| &c.track == track)`). `None` falls through to the existing swap_remove-without-push shape — **empty track → silence (Skip)**, the locked decision via the code path that already exists. The fallback/Literal concrete cell is minted with the missing cell's `track` and `played_by = PrincipalId::beat()`.
- `commit` (:372-388) + `absorb_emitted` (:418-428): an emission is part of the committing parent's act — `commit()` stamps each emitted cell with the parent's `track` and `played_by` before absorbing. **Loud on mismatch (judge-mandated):** `debug_assert_eq!` in debug; in release, when resolver-set values differ from the parent's, `tracing::error!` (never silent normalization). Rationale for stamp-over-drop: both concrete emitted consumers (fork 2's MIDI sibling; future automation lanes, which live *inside* a track) are same-track by definition, all resolvers are in-kernel, and a hard error would turn a doc-comment misread into a Failed cell mid-performance. The rule is documented on `Resolution::with_emitted` (resolver.rs).
- `ResolverCtx::content_before` (:466-476) stays track-blind **deliberately**: no current resolver reads it (AbcToMidi reads CAS by hash, kernel hyoushigi/mod.rs:206-215), and the first real consumer of track-scoped reads is `$HEARD` — per the two-voices rule, design that API with its consumer. Recorded in issues.md.
- New `seed_playhead` per §4.

### 5.3 Snapshot + BlockContent threading (the trap that killed A's wire story, now guarded)

- `BlockSnapshot.track: Option<TrackId>` `#[serde(default)]` after `tick` (types/block.rs, ~:1306 region) + `BlockSnapshotBuilder::track()` (:2074 region).
- `BlockContent` (crdt content.rs) gains `track: Option<TrackId>` in the write-once snapshot-field block (~:119-138), copied in `from_snapshot` (:226-250), `from_snapshot_for_sync` (:260-295, beside the `tick` copy at :278), and emitted in `snapshot()` (:559-600, beside `tick` at :591). `BlockContent::new` initializes `None`.
- **Process rule (codified by test T15, judge-mandated):** any new `BlockSnapshot` field MUST be threaded through `BlockContent` (`from_snapshot`, `from_snapshot_for_sync`, `snapshot()`) in the same change. T15 round-trips a fully-populated `BlockSnapshot` through `BlockContent` and asserts field-by-field equality — it would have caught A's dead `played_by` and will catch the next field.
- `Timeline::materialize` (materialize.rs:28-55): adds `.track(cell.track.clone())` to the builder. `played_by` is not a snapshot field — it IS the BlockId's principal, passed by the caller.

### 5.4 Kernel/server plumbing

- `schedule_abc_cell` (kernel hyoushigi/mod.rs:123-147): gains `track: TrackId, played_by: PrincipalId` params; builds the Cell with them. TrackId being a validated newtype makes "reject an invalid track" structural — no unvalidated string can reach scheduling.
- `materialize_committed` (kernel hyoushigi/mod.rs:255-304): delete `BlockId::new(ctx, beat(), *high_water as u64)` (:288) → `let block_id = blocks.reserve_block_id(context_id, cell.played_by)?;` and insert via `insert_from_snapshot_as(ctx, snapshot, after.as_ref(), Some(cell.played_by))` (:292-297, replacing `Some(PrincipalId::beat())`). Add `debug_assert_eq!(block_id.principal_id, cell.played_by)`. `high_water` keeps its name and its sole remaining meaning — the bridge watermark (count of committed cells materialized); its doc comment (:246-250) drops the "doubles as the seq" clause. Order-key agent suffixes now vary **per player** (insert_from_snapshot_as sets the doc principal, kernel block_store.rs:1184) — correct: the suffix is the replica tiebreak, not identity, and ordering never depends on it post-§2.2.
- `BeatState` (beat.rs:54-66) gains `track: TrackId`; `BeatCommand::Arm` (kernel hyoushigi/mod.rs ~:74) gains `track: TrackId`. The musician create site (server rpc.rs:2648-2661) derives it: `TrackId::new(label).ok().or_else(|| TrackId::slugify(label))` — slugify is loud (tracing::info when it differs) and **hard-errors context creation when the slug is empty** (crash over a silent shared default). [User-level flag: see open calls, §9.]
- `on_turn_completed` (beat.rs:274-292): passes `st.track` and `played_by = b.id.principal_id` (the ABC block's author — already fetched at :282) into `schedule_abc_cell`. **Feedback-loop guard lands in this slice (judge-mandated one-liner):** after fetching the snapshot, `if b.track.is_some() || b.id.principal_id == PrincipalId::beat() { return; }` — never re-schedule a materialized block (or a legacy beat() block) as if it were a model's ABC. The full fix (carrying the turn's output block id on `TurnFlow::Completed`) is fork 2's call — explicit handoff: fork 2 should adopt `track.is_some()` as its discriminator or replace this guard wholesale, in one place.

## 6. Restart semantics (exact walk)

**Cold start** (kernel block_store.rs load_from_db :2013 → crdt `from_snapshot` :1394 → `merge_ops` :1109 per oplog row):
- `seq_lanes`: seeded in `from_snapshot` for EVERY principal observed — players, system(), beat(), drift authors — tombstones included (:1422-1429); advanced by each `merge_ops` replay (guards deleted). Post-load, `reserve_block_id(ctx, P)` = max persisted seq for P + 1, always.
- `next_tick`: seeded by `normalize_timeline` (:1443) as today, AND advanced by every `merge_ops` replay (§2.3) — oplog rows past the snapshot no longer leave it stale.
- `lamport_clock`: unchanged (:1432-1439).
- Order keys: persisted keys load verbatim; fresh appends derive from the predecessor's key — a fresh append sorts last even if a counter were somehow stale. Ordering no longer trusts any restored counter (the locked regression, pinned by T2/T8).

**Counters that intentionally reset:** the Timeline (committed log, future, RAM CAS, squash ledger) is in-RAM and gone; contexts restart **disarmed and stopped** (the only Arm sender is createContext; the re-arm sweep is future work made safe by this design). `BeatState.high_water = 0` is now consistent by construction — it watermarks the (empty) fresh committed Vec and is no longer an id source. `beat_count = 0`: OODA cadence phase resets (a cadence, not a position — documented).

**On re-arm (manual or future sweep):** `BeatScheduler::arm` computes `max_tick(ctx)` and passes it to `arm_timeline`, which seeds the playhead inside `or_insert_with` only — a live timeline is never re-seeded or rewound; a non-virgin `seed_playhead` call is `Err`. Musical time stays globally monotone per context across restarts and rotations (chameleon.md:195-198). `UseLastGood` after restart: every track's engine history is empty → Skip → **silence until the first good phrase** (locked; deliberately NO last-good rehydration from the block log on arm — recorded as a possible future rc-driven arm option in issues.md, not engine default).

**Rotation (shallow fork):** the forked store carries seeded `next_tick` and all-principal `seq_lanes` (§2.4), so a rotated head appends with continuous ticks and collision-free ids — the rotation invariants hold with zero rotation-specific code.

**First post-restart materialization (the hazard, FIXED):** play → beat advances seeded playhead → on_turn_completed reads the player's ABC (guard skips materialized blocks), schedules a cell with (track, played_by) → cell commits → `materialize_committed` reserves `(ctx, player, max_seq+1)`, inserts with a successor order_key (sorts after every persisted block) and a tick above all persisted ticks → journaled, synced, no DuplicateBlock, no silent retry loop. Pinned by T9.

## 7. Wire changes

**capnp** (kaijutsu.capnp, `BlockSnapshot`; last used field ids `signature @37` / `hasSignature @38` at :214-215):
```
# Hyoushigi track — stable lane identity (DAW sense). Set only on blocks
# materialized from a committed timeline cell. Author (who played) remains
# id.principalId; track is the lane, never the author.
track @39 :Text;
hasTrack @40 :Bool;
```
Comment at :139 updated to state the lane-vs-author contract. Conversion sites: server `set_block_snapshot` (kaijutsu-server/src/rpc.rs:5961, beside the tick write at :6074) and client `parse_block_snapshot` (kaijutsu-client/src/rpc.rs:1910, beside the tick read at :2088) plus the client round-trip test builder (:2613). Old readers ignore the unknown field → None; old writers leave hasTrack=false → None.

**CBOR** (`kaijutsu_types::codec` FORMAT_V1, additive-evolution contract):
- `BlockSnapshot.track: Option<TrackId>` `#[serde(default)]`; `TrackId` is `#[serde(transparent)]` String. Old payloads (oplog rows, doc_snapshots, fork copies) decode with `track = None`. New payloads read by old binaries: ciborium + serde derive ignores unknown map keys (no `deny_unknown_fields` anywhere in kaijutsu-types — verified) — old kernels/apps decode cleanly, dropping the field. The **bidirectional** frozen-fixture test (T16) ships in the same change: old-shape blob → `track == None`, AND new-shape blob decoded under an old-shape mimic struct (unknown-key tolerance) — closing the standing docs/issues.md fixture ask.
- `Cell` gains required (non-default) `track` + `played_by` — safe (not persisted/wire-carried today); deliberately fail-loud for future cell persistence.

**App-side impact: zero required.** The app replica rebuilds from CBOR SyncPayload blobs; the field flows through the shared kaijutsu-crdt `BlockContent` paths. order_key never crosses capnp; successor keys are canonical-shaped 16-char strings and the `{pred}V{suffix}` fallback shape already exists in live documents — mixed-shape sorting is unchanged and pure-lexicographic everywhere. Author chips show the player's principal on played phrases and beat()'s on fallback repeats — truthful, mildly noisy until a track chip + "transport" label land (issues.md follow-up).

**Mixed-version matrix:** new kernel + old app — identical ordering, track invisible. Old kernel + new app — track always None (required legacy behavior anyway). DB moved backward to an old kernel — snapshots decode (unknown key dropped); blocks it mints lack track and use tick-derived append keys, still canonical-shaped; next new-kernel load re-seeds everything. No flag day; the only irreversible bit is that track-bearing blocks keep the field, which is the design.

## 8. Failing-first test plan (ordered; each names why it fails today)

Run `cargo check` first to confirm the build is green (it is — do not carry the stale step-zero).

**Phase 1 — CRDT ordering substrate (kaijutsu-crdt):**
- **T1** `order_key_successor` unit suite (content.rs, beside order_midpoint tests): canonical V+11+suffix → decode/+1/re-encode with own suffix at fixed width; legacy shapes (midpoint result, `{key}V`, `{:020}` decimal) → `{pred}V{suffix}` fallback; property loop `successor(pred) > pred` for every shape; i64::MAX → fallback, no wrap. *Fails by absence — write first.*
- **T2** `appends_after_merge_ops_sort_last` (block_store.rs tests): store A with 10 blocks → snapshot → from_snapshot into B → merge_ops 5 more blocks (ticks 10..14, canonical keys) → fresh `insert_block` → assert LAST in `block_ids_ordered()`. *Fails today: stale next_tick mints a mid-document key (:348-349). The locked regression.*
- **T3** `merge_ops_restores_next_tick_high_water`: after merging new_blocks with max tick N, a fresh insert STAMPS tick N+1. *Fails today (:1109-1180 never touches next_tick). Pins tick semantics separately from T2's key ordering (judge-mandated split).*
- **T4** `fork_seeds_tick_and_lanes` — parameterized over `fork` / `fork_at_version` / `fork_filtered`: fork a store with blocks to tick N under principal P ≠ fork principal; fresh insert stamps tick N+1 and sorts last; `next_seq_for(P)` == max P seq + 1. *Fails today (Self::new + own-principal guards, :1202-1349). The rotation-critical graft.*
- **T5** `keyless_merge_new_blocks_append_not_prepend`: merge_ops a payload whose new_blocks lack canonical keys → they sort after existing blocks, not before. *Fails today (decimal fallback at :1123 sorts below 'V').*
- Implement: §2.1 primitive, §2.2 three arms + tick-regression warn, §2.3, §2.4, the :1123 fallback change, doc-comment rewrites (:811-819, :289-295).

**Phase 2 — seq lanes (kaijutsu-crdt):**
- **T6** `seq_lanes_cover_foreign_principals_after_restore`: `from_snapshot` / `merge_ops` / fork each restore blocks authored by P ≠ store principal (one a tombstone); `next_seq_for(P)` == max+1 for each; a subsequent insert of the reserved id does not DuplicateBlock; own-principal minting via `insert_block` unchanged. *Fails today (API absent; guards at :803-805, :1134-1136, :1403-1405).*
- **T7** `reserve_block_id_claims_and_advances`: reserve → seq claimed; failed-insert gap is tolerated; subsequent reserve mints +1. *Fails by absence.*
- Implement: §3 (`seq_lanes`, `reserve_block_id`, `next_seq_for`, guard deletions, tombstone seeding); kernel passthrough + `max_tick` helper.

**Phase 3 — kernel restore integration (kaijutsu-kernel, fresh_db_store :4132 + drop_and_reload :4158):**
- **T8** `test_reload_then_append_sorts_last` (beside test_block_order_preserved :4567, which never appends after reload — the exact gap): insert, force compaction, insert more (oplog past snapshot), drop_and_reload through the REAL path, append → sorts last AND stamps tick > pre-reload max. *Fails today on both assertions.*
- **T9** `materialize_after_reload_mints_fresh_seq_no_duplicate` (kernel hyoushigi/mod.rs tests, beside bridge_materializes_committed_cell_into_block_and_cas :378): materialize one cell (persists a track block), drop_and_reload, fresh timeline + high_water=0, materialize again → Ok, seq = old max + 1. *Fails today with CrdtError::DuplicateBlock swallowed at beat.rs:234 — THE hazard test; written against phase-2/5 code, proves the fix is structural.*

**Phase 4 — track types + engine (kaijutsu-types, kaijutsu-hyoushigi):**
- **T10** `track_id_validation` (track.rs): accepts `bass`/`solo`/`a-1_x`; rejects empty, whitespace, >64, uppercase, spaces, unicode; `slugify("My Musician") == Some("my-musician")`, `slugify("🎵") == None`; serde-transparent round-trip. *Fails by absence.*
- **T11** `use_last_good_does_not_cross_tracks` (engine.rs, extend `deferred_at` helper :509 with track): track B commits at tick 5; track A schedules UseLastGood at tick 10 with empty A-history and misses (diverged basis, no budget) → NOTHING committed for A; B's content not duplicated under A. *The locked two-track cross-contamination test; fails (compile, then behavior — :437-447 is track-blind).*
- **T12** `use_last_good_on_empty_track_resolves_to_skip`: single track, zero history, UseLastGood miss → committed stays empty, no panic, playhead passes. *Pins the locked empty-track→Skip decision (asserted nowhere today per recon).*
- **T13** `emitted_cells_inherit_parent_track_and_played_by`: resolver emits a concrete sibling → absorbed cell carries parent's track + played_by; a deliberately mismatched emission triggers debug_assert (debug) / tracing::error (release, assert via log capture or a counter). *Compile-fail first.*
- **T14** `seed_playhead_errs_on_non_virgin` (engine.rs): seed on fresh timeline OK; seed after a schedule or commit → Err(SeedError). *Fails by absence.*
- Implement: §1.1, §5.1, §5.2, §4 engine half.

**Phase 5 — materialization + wire (types, crdt content, kernel, server, client, capnp):**
- **T15** `block_snapshot_roundtrips_through_block_content_field_by_field` (crdt): fully-populated BlockSnapshot (every Option set, track included) → BlockContent::from_snapshot → .snapshot() → assert equality per field. *Fails when track lands until content.rs threading is complete — the structural guard against A's field-drop trap, permanent for future fields.*
- **T16** `block_snapshot_cbor_bidirectional_fixtures` (types codec tests + crdt sibling of test_snapshot_cbor_roundtrip :2961): (a) FROZEN pre-track CBOR byte-literal decodes with track=None; (b) new-shape blob decodes under an old-shape mimic (unknown-key tolerance); (c) track-bearing round-trip. *(a)/(b) are the permanent CI net the issues.md ask wants; (c) fails by absence.*
- **T17** `materialized_snapshot_carries_track_and_player` (materialize.rs + extend the kernel bridge test :378-426): snapshot.track == Some(cell.track); inserted id.principal_id == cell.played_by (player case) and == PrincipalId::beat() (fallback case); order_key strictly exceeds the previous tail key. *Replaces the beat()-always assertion (:416); compile-fail first.*
- **T18** `track_capnp_roundtrip` (client rpc.rs :2612 region): Some/None both survive Rust→capnp→Rust; hasTrack=false ↔ None.
- **T19** `on_turn_completed_skips_materialized_blocks` (beat.rs tests): last block has track=Some (or beat() author) → no cell scheduled; last block is player ABC → cell scheduled with that author as played_by and st.track. *Fails today (no guard, no params).*
- Implement: §1.2, §5.3, §5.4, §7; update the two MIDI-shape-baking tests (beat.rs:610-659 region, mod.rs:431-479 region) in the same change.

**Phase 6 — playhead seeding wiring (server):**
- **T20** `arm_seeds_playhead_from_max_committed_tick` (beat.rs tests): pre-insert blocks with ticks to T; arm; playhead == T; first beat advances to T+1; re-arm of the live timeline does NOT re-seed. *Fails today (fresh Timeline starts at 0; beat.rs:110-122 never seeds).*
- Implement: §4 kernel/server half (arm_timeline param, max_tick call, BeatCommand::Arm track + musician-create derivation).

**Phase 7 — budget + docs (procedure, not pass/fail):**
- **T21** `cargo test -p kaijutsu-crdt bench_append_hot_path -- --ignored --nocapture` (block_store.rs:1524-1530) before and after — successor parse/encode + HashMap lane on the append path must hold the budget this bench was written for.
- docs/hyoushigi.md: strike "seed playhead on re-arm" from not-yet; record order/tick decoupling + the lane-vs-author contract + the strictly-increasing-append-keys note. docs/issues.md: delete the shipped ordering-regression entry; ADD: beat.rs:234 swallowed materialize-error poison-cell retry channel (per-cell retry budget or Error-block escalation sketched; worsens with multiple tracks), re-arm-on-restart sweep unwired, app track chip + "transport" label for beat(), `ResolverCtx` track-scoped reads await `$HEARD`, optional rc-driven last-good rehydration on arm, band track↔chair mapping source of truth, blocks_ordered() per-sort allocation churn (kept separate from this slice deliberately), kj track listing surface.

## 9. Touch list (complete)

1. `crates/kaijutsu-types/src/track.rs` (NEW) — TrackId, TrackIdError, slugify, solo(), tests; `lib.rs` re-export.
2. `crates/kaijutsu-types/src/block.rs` — BlockSnapshot.track `#[serde(default)]` (after tick ~:1306); BlockSnapshotBuilder::track() (~:2074); invariant docs (Some ⇒ materialized; principal ≠ lane key).
3. `crates/kaijutsu-types/src/ids.rs` :199-214 — beat() doc rewrite (transport-played cells only).
4. `crates/kaijutsu-types/src/codec.rs` tests — T16 fixtures.
5. `kaijutsu.capnp` — track @39 :Text, hasTrack @40 :Bool on BlockSnapshot (after :215); :139 author-comment update.
6. `crates/kaijutsu-crdt/src/content.rs` — :51 region: base62_decode + order_key_successor + tests; BlockContent.track in the write-once block (~:119-138), from_snapshot (:226-250), from_snapshot_for_sync (:260-295), snapshot() (:559-600).
7. `crates/kaijutsu-crdt/src/block_store.rs` — :80 next_seq → seq_lanes; :256-260 new_block_id; NEW reserve_block_id + next_seq_for; :327-328/:348-349/:362-363 successor wiring; :811-819 comment + tick-regression warn in insert_from_snapshot; :803-805/:1134-1136/:1403-1405 guard deletions (+ tombstones :1422-1429); merge_ops next_tick restore + :1123 fallback change; fork variants :1202-1349 tick + lane seeding; :289-295 order_key_for_tick doc note; test updates (exact-seq assertions churn).
8. `crates/kaijutsu-hyoushigi/src/cell.rs` :113-137 — Cell {track, played_by} + constructors; test fixtures.
9. `crates/kaijutsu-hyoushigi/src/engine.rs` — commit() :372-388 stamp + loud mismatch; fire_fallback :391-414 per-track UseLastGood, fallback cells track + beat(); last_committed_content :437-447 → last_committed_content_in(track, before); seed_playhead + SeedError; content_before :466-476 documented track-blind; tests.
10. `crates/kaijutsu-hyoushigi/src/resolver.rs` — Resolution::with_emitted inheritance contract docs.
11. `crates/kaijutsu-hyoushigi/src/materialize.rs` :28-55 — `.track(cell.track.clone())`; tests T17 (hyoushigi half).
12. `crates/kaijutsu-kernel/src/hyoushigi/mod.rs` — BeatCommand::Arm gains track (~:74); schedule_abc_cell :123-147 gains track + played_by; materialize_committed :255-304 reserve-based ids under cell.played_by, high_water doc narrowed (:246-250); bridge tests.
13. `crates/kaijutsu-kernel/src/block_store.rs` — NEW reserve_block_id(context_id, principal) (entry-lock pattern of :1171-1184); NEW max_tick(context_id) (beside :693); restore tests T8.
14. `crates/kaijutsu-kernel/src/kernel.rs` :627-640 — arm_timeline gains seed: Tick, applied inside or_insert_with.
15. `crates/kaijutsu-server/src/beat.rs` — BeatState.track + high_water comment (:54-66); arm :110-122 max_tick seed + track; on_turn_completed :274-292 guard + track/played_by params; tests T19/T20.
16. `crates/kaijutsu-server/src/rpc.rs` — :2648-2661 musician create: TrackId from label (new/slugify, hard-error on empty slug), pass on Arm; :5961/:6074 capnp track write.
17. `crates/kaijutsu-client/src/rpc.rs` — :1910/:2088 capnp track read; :2613 round-trip builder.
18. `docs/hyoushigi.md`, `docs/issues.md` — per Phase 7.

## 10. Resolved open questions / explicit user-level flags

**Resolved by this synthesis:** counter home (CRDT seq_lanes, not KernelDb); fork-variant seeding in-slice; seed_playhead Err-on-non-virgin inside or_insert_with; emitted-cell policy (stamp + loud); merge_ops key-less fallback fixed in-slice; on_turn_completed guard in-slice via track.is_some() + beat(); ResolverCtx stays track-blind until $HEARD; no last-good rehydration on arm; no registry/display-name table (presentation-only later); played_by-on-turn-blocks moot (author IS the principal); blocks_ordered churn stays separate.

**Flagged for Amy (do not block implementation start — phases 1-4 are unaffected):**
1. **TrackId charset** `[a-z0-9_-]{1,64}` lowercase-only: loosening later is easy, tightening is breaking — confirm before the validator freezes (affects Phase 4).
2. **Musician-create track derivation**: label → TrackId::new, else slugify (loud), else **hard-error createContext**. Alternative on the table: fixed "main" fallback with a warn (softer, but a silent-ish shared lane). Affects only the rpc.rs:2648 site (Phase 6).
3. **Fork 2 handoff**: this slice's on_turn_completed guard is `track.is_some() || author == beat()`; fork 2 chooses to keep that discriminator or carry the output block id on TurnFlow::Completed — one decision, made once, there.
