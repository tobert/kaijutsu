//! The single coalescing beat scheduler — kaijutsu's one active timing thing,
//! and the musician's **transport**.
//!
//! `kaijutsu-hyoushigi` is runtime-agnostic: a `Timeline` advances only when
//! something drives its playhead. This module is that driver on the kernel side
//! — the wall-clock timer the doc says is "the kernel/client integrator's job."
//! It runs **one** task for the whole server (not a timer per context): a
//! min-heap of `(wake_instant, ContextId)` plus a message ingress, driven by a
//! single `select!`. A context holds a heap entry only while its clock is
//! **playing**; a stopped/paused context (and every coder) has none — quiescent,
//! zero cost.
//!
//! **The context tick is event-counted.** While playing, each beat advances the
//! playhead by exactly [`STEP`] via `advance_to(playhead + STEP)` — *not* from
//! wall-clock elapsed. So a pause freezes musical time and a resume picks up at
//! +1, with no catch-up: the kernel can run 24×7 and keep its scheduler alive
//! while a context goes quiescent and later continues "as if nothing happened."
//! There is **no rewind** — the playhead is forward-only (the write barrier);
//! revisiting the past is an *export* of committed content, not a transport seek.
//!
//! Two switches per context: the **clock** (`playing`) and the **OODA-arm**
//! (`ooda_armed`). Every beat the scheduler advances the playhead and bridges any
//! freshly-committed cells into the block log ([`materialize_committed`]); every
//! `ooda_every` beats — and only when `playing && ooda_armed` — it fires the
//! `tick` rc verb (`kj drive`) to request the next OODA turn, spawned
//! fire-and-forget so the single driver never blocks (the model turn runs on the
//! turn-driver thread, never here).

use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::mpsc;
use tokio::time::Instant;

use kaijutsu_crdt::BlockId;
use kaijutsu_kernel::block_store::SharedBlockStore;
use kaijutsu_kernel::flows::{BlockFlow, TurnFlow};
use kaijutsu_kernel::hyoushigi::{
    Attachment, BeatAck, BeatCommand, BeatPolicy, BeatRequest, Body, Cadence, Cell, ClockKind,
    ContentRef, DeriverRegistry, MaterializeCursor, Span, TrackSnapshot, materialize_committed,
    schedule_abc_cell,
};
use kaijutsu_kernel::kernel_db::{ContextRow, PersistedAttachment, PersistedTrack};
use kaijutsu_kernel::{ContentStore, Kernel, KjCaller, KjDispatcher};
use kaijutsu_types::{
    BlockSnapshot, ConsentMode, ContentType, ContextId, ContextState, DocKind, PrincipalId,
    SessionId, Tick, TickDelta, TrackId, now_millis,
};

use crate::clock::{ClockSource, ClockSourceKind};
use crate::rpc::ServerRegistry;

/// The score's source-notation mime — the only content a render target consumes
/// (matches `ABC_MIME` in the kernel bridge). A committed cell of any other mime
/// is not handed to a (MIDI) render target.
const ABC_MIME: &str = "text/vnd.abc";

/// Ticks the playhead advances per beat (PPQ 1: one tick per beat). The tick is
/// event-counted, so this is a pure increment, never scaled by elapsed time.
const STEP: TickDelta = TickDelta::new(1);

/// How often the beat scheduler emits a `BeatSync` reference for a sink's
/// continuous timebase (the metronome phasor): every Nth beat, plus the first.
/// Low by design — the phasor free-runs between references, so a reference every
/// beat would make it chase per-beat scheduler jitter (audible wobble). On one
/// machine drift is ~0, so every 8 beats keeps it locked while staying steady.
const BEAT_SYNC_EVERY: u64 = 8;

/// The local instant at which a committed cell starting at `start` should render,
/// given the beat's jitter-free scheduled fire instant (`base`), the live beat
/// `period`, and the current `playhead` (Stage 3 WI 4). A cell commits AHEAD of
/// the playhead (the speculation lead), so `start ≥ playhead` → the instant is in
/// the near future. A late or out-of-order `start` behind the playhead clamps to
/// `base` (offset 0): a render is NEVER scheduled into the past (the `next_fire`
/// MONOTONIC contract, applied at the render seam).
fn render_instant(base: Instant, period: Duration, start: Tick, playhead: Tick) -> Instant {
    let offset_beats = (start - playhead).get().max(0) as u32;
    base + period * offset_beats
}

/// A track's beat bookkeeping — the **clock domain** (`docs/tracks.md`, Stage 1).
/// The clock + playhead live HERE now, not on any one context's timeline: the
/// track persists, contexts attach to be beaten by it and come and go. Keyed by
/// [`TrackId`] in the scheduler's `tracks` map.
struct TrackState {
    /// The track's clock source — what drives *when* it beats (Stage 3). The
    /// SINGLE source of truth for the live beat period; `set_tempo` mutates it and
    /// every period read goes through `clock.period()`, so there is no second,
    /// stale copy. `SystemClock`-only this stage; `Modeled` (MIDI drift) lands at
    /// M3. See [`crate::clock`].
    clock: ClockSourceKind,
    /// Beats per phrase — the only musical chunking knob above the beat. Lives
    /// here (not on the clock — phrasing is not a clock concern); the live period
    /// comes from `clock`. [`Self::phrasing`] rebuilds a [`BeatPolicy`] from the
    /// two for the phrase-math helpers and `transport_vars`.
    beats_per_phrase: u64,
    /// The clock switch. `false` = stopped/paused: no heap entry (quiescent),
    /// playhead frozen. A track is created stopped (no surprise tokens).
    playing: bool,
    /// Monotonic clock-enlistment generation, bumped by every [`play`](BeatScheduler::play).
    /// Each heap entry carries the generation it was enlisted under; `fire_due` drops a
    /// popped entry whose generation is stale. This is what makes `stop`/`pause` then
    /// `play` *within one period* beat exactly once — the pre-`play` entry (still in the
    /// heap, not yet popped) is invalidated by the bump rather than processed alongside
    /// `play`'s fresh entry (which would double-beat). Normal beats re-push under the
    /// same generation, so they stay valid.
    generation: u64,
    /// Musical time, **owned by the track** (moved off the per-context Timeline).
    /// Event-counted: advances one [`STEP`] per beat while playing; a pause freezes
    /// it and a resume picks up at +1, no wall-clock catch-up. Forward-only. Each
    /// beat the attached contexts' timelines are slewed to this value (Stage-1
    /// bridge — Stage 2 dissolves the per-context timeline into the track's score).
    playhead: Tick,
    /// Beats elapsed *while playing* — drives the wakeup/phrase/rotate cadences.
    /// Frozen across a pause, like the playhead.
    beat_count: u64,
    /// The wall-clock instant of the track's most recent beat, as nanoseconds since
    /// the UNIX epoch — latched ONCE per beat in [`process_track`] and read back by
    /// [`transport_env`] as `KJ_EPOCH_NS`. Latching it on the track (not re-reading
    /// the clock per context) is what makes every context woken on the SAME beat see
    /// the IDENTICAL value — the cross-context join key. `0` before the first beat.
    last_epoch_ns: u64,
    /// The track's score context: a real, app-viewable non-producer context whose
    /// document holds the materialized score. Minted once when the track is created
    /// (or recovered from the persisted `tracks` row) and never changes. The score
    /// outlives any producer, which is the point of moving it off the per-context
    /// timeline. Materialize writes here; `KJ_HEARD` reads it (the band view).
    score_context: ContextId,
    /// The single materialization cursor over the TRACK timeline's committed log:
    /// how far whole committed cells have crossed the write barrier (`high_water`)
    /// and how far the in-progress cell's artifact group got. One per track (the
    /// score is the track's), so N producers' interleaved cells materialize through
    /// it exactly once each — never per-attachment double-emit.
    cursor: MaterializeCursor,
    /// Consecutive materialize failures on the SAME poison cell at the track
    /// cursor's head. Reset to 0 when any cell crosses; at [`MATERIALIZE_RETRY_BUDGET`]
    /// the bridge skips the poison cell (loudly) so the beat never silently retries.
    materialize_failures: u32,
    /// How far the TRACK timeline's failure ledger has been drained into Error
    /// blocks. One cursor over the shared ledger; each new event is routed to the
    /// PRODUCING context (matched by `played_by`) so a producer reads its own
    /// failures, never a sibling's. Monotone — never re-surfaces a drained event.
    failure_water: usize,
    /// The binding set: which contexts are attached and how each rides the beat.
    /// The track holds this passive view; the *context* drives the bind (entity #3)
    /// and a forked child re-binds on the way up.
    attached: HashMap<ContextId, AttachedContext>,
    /// The beat's *scheduled* fire instant (the heap entry's `t`), latched in
    /// [`fire_due`] right after the pop — BEFORE `process_track`/`materialize_track`
    /// run. This is the jitter-free reference a render target schedules off: NOT
    /// the `SystemTime`-derived `last_epoch_ns` (the jittery *actual* wakeup), so
    /// per-beat scheduler jitter never accumulates into the output (Stage 3 review).
    /// Init'd to `now` at create; overwritten on the first beat before any emit.
    last_fire_scheduled: Instant,
}

impl TrackState {
    /// Rebuild the [`BeatPolicy`] view (live period + phrasing) for the phrase-math
    /// helpers and `transport_vars`. The period is pulled LIVE from `clock`, so this
    /// view never goes stale against a `set_tempo` — there is no second period to
    /// drift. Cheap: `BeatPolicy` is `Copy`.
    fn phrasing(&self) -> BeatPolicy {
        BeatPolicy {
            period: self.clock.period(),
            beats_per_phrase: self.beats_per_phrase,
        }
    }
}

/// One context's binding to a track. The [`Attachment`] is the durable/wire
/// binding contract (wakeup/rotate/ooda_armed/pulse, persisted in the
/// `attachments` row). Materialization state is no longer here — the score lives
/// on the track now (one cursor/ledger on [`TrackState`]); the only per-context
/// runtime bit left is the producing principal, used to route the shared failure
/// ledger back to the right producer's conversation.
struct AttachedContext {
    /// The binding the context announced: its wakeup divisor, rotate cadence, OODA
    /// arm, and monotonic pulse counter. Travels with a fork.
    attachment: Attachment,
    /// The principal this context plays under (the `played_by` it stamps on its
    /// cells), recorded when it schedules a phrase. `None` until it has produced.
    /// Used to route the track's shared failure ledger: an event whose `played_by`
    /// matches this is surfaced in THIS context's conversation.
    producer_principal: Option<PrincipalId>,
}

impl AttachedContext {
    /// A freshly-bound context: the announced attachment, no production yet.
    fn new(attachment: Attachment) -> Self {
        Self { attachment, producer_principal: None }
    }
}

/// How many consecutive beats may fail to materialize the SAME poison cell before
/// the bridge gives up on it and skips it (loudly). Crash-over-corruption favors a
/// bounded, observable failure over an unbounded silent retry loop.
const MATERIALIZE_RETRY_BUDGET: u32 = 3;

/// What to do after a `materialize_committed` error, given how many consecutive
/// times the SAME poison cell has now failed. Pure so the budget logic is unit-
/// testable without injecting a real materialize failure.
#[derive(Debug, PartialEq, Eq)]
enum PoisonAction {
    /// Within budget — leave `high_water` un-advanced and retry next beat.
    Retry,
    /// Budget exhausted — advance `high_water` past the poison cell and move on.
    SkipPoison,
}

/// Decide the poison-cell action. `failures` is the count INCLUDING the failure
/// that just happened (≥ 1).
fn poison_action(failures: u32) -> PoisonAction {
    if failures >= MATERIALIZE_RETRY_BUDGET {
        PoisonAction::SkipPoison
    } else {
        PoisonAction::Retry
    }
}

/// The transport heartbeat vars seeded into the `tick` rc lifecycle at each OODA
/// boundary — the present-tense facts a player composes against ("you are at
/// phrase 8, tick 128, 120 BPM"). Returned as `(name, value)` pairs the
/// lifecycle folds into the kaish env (referenced as `$TICK` / `$PHRASE` /
/// `$TEMPO` in `S10-drive.kai`).
///
/// `$HEARD` — the windowed look-back at sibling tracks' recent phrases — is
/// deliberately NOT here. It's a **pull** (a kaish read of the committed past,
/// built when a second player makes it load-bearing), not a var pushed every
/// turn. Slice one is a solo bass: its own hydrated history is its continuity.
/// (Design: `docs/chameleon.md`, transport report.)
///
/// Pure so the formatting is unit-testable without a live scheduler.
fn transport_vars(playhead: Tick, beat_count: u64, policy: &BeatPolicy) -> [(String, String); 3] {
    // Phrases elapsed while playing (0-based). A zero `beats_per_phrase` has no
    // phrasing → phrase 0, never a divide-by-zero (mirrors `is_phrase_boundary`).
    let phrase = if policy.beats_per_phrase > 0 {
        beat_count / policy.beats_per_phrase
    } else {
        0
    };
    // BPM from the beat period. Tempos are whole numbers in practice (the
    // `kj transport tempo` verb takes integer BPM); round to the nearest.
    let secs = policy.period.as_secs_f64();
    let bpm = if secs > 0.0 { (60.0 / secs).round() as u64 } else { 0 };
    [
        ("KJ_TICK".to_string(), playhead.get().to_string()),
        ("KJ_PHRASE".to_string(), phrase.to_string()),
        ("KJ_TEMPO".to_string(), bpm.to_string()),
    ]
}

/// How many phrases of recent committed notation `$HEARD` carries. Covers the
/// previous OODA cycle (default cadence = 8 phrases) so a player sees the
/// section it last produced; older notation falls out of the window.
///
/// TODO(chameleon batch 2): make this tunable per context (rc-declared and/or a
/// `kj transport` knob), alongside the cadence/tempo knobs in `docs/issues.md`.
const HEARD_WINDOW_PHRASES: u64 = 8;

/// One recent committed score phrase, as `$HEARD` exposes it: where it sits on
/// the timeline (`tick`), which lane (`track`), and the notation itself (`abc`).
#[derive(serde::Serialize)]
struct HeardEntry {
    tick: i64,
    track: String,
    abc: String,
}

/// Build the `$HEARD` JSON: committed notation within the last `window` ticks of
/// `now`, oldest→newest, across all tracks (a band view; solo bass just yields
/// its own lane). **Why a var at all, solo:** materialized score blocks are
/// `ephemeral` (hydration-silent), so a player composing its next phrase cannot
/// see its own prior notation from the conversation — `$HEARD` is the only
/// channel that shows what was just played.
///
/// A JSON *string* (not a kaish array) — models read it natively in the prompt,
/// and it needs no arrays/hashes support. The derived MIDI sibling is excluded
/// (`ContentType::Abc` only) so `$HEARD` stays notation-pure, like the
/// `UseLastGood` pool. Pure for unit-testing.
///
/// TODO(chameleon batch 2): this is the pragmatic push stopgap. Two follow-ups
/// when the kaish arrays/hashes plan lands (Chameleon is its first consumer):
///   1. Expose `$HEARD` as a real kaish **array of hashes** (one entry per
///      phrase) instead of a JSON string the script can't index — so a drive
///      script can `for phrase in $HEARD` / read `${phrase.abc}` natively.
///   2. Re-shape push → **pull**: a kaish read of the committed past on demand
///      (`kj`-reachable windowed read) so the script chooses depth/track rather
///      than the kernel injecting a fixed window every turn. Shares this read
///      with the RC hydration-marker archive verb (`docs/chameleon.md`).
fn heard_json(blocks: &[BlockSnapshot], now: Tick, window: TickDelta) -> String {
    let since = now - window;
    let mut entries: Vec<(Tick, HeardEntry)> = blocks
        .iter()
        .filter(|b| b.content_type == ContentType::Abc)
        .filter_map(|b| {
            let tick = b.tick?;
            if tick < since {
                return None;
            }
            let track = b
                .track
                .as_ref()
                .map(|t| t.as_str().to_string())
                .unwrap_or_default();
            Some((tick, HeardEntry { tick: tick.get(), track, abc: b.content.clone() }))
        })
        .collect();
    // Oldest→newest so the player reads its line in playing order. `Tick` is
    // `Ord`; ties (shared-coordinate doctrine) keep block-log order via the
    // stable sort.
    entries.sort_by_key(|(t, _)| *t);
    let list: Vec<HeardEntry> = entries.into_iter().map(|(_, e)| e).collect();
    serde_json::to_string(&list).unwrap_or_else(|_| "[]".to_string())
}

/// What one wake produced: which contexts beat, and which crossed an OODA
/// boundary (so should fire the `tick` verb). Split out so the cadence logic is
/// observable in tests without a live dispatcher.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct BeatOutcome {
    pub fired: Vec<ContextId>,
    pub ooda_due: Vec<ContextId>,
    /// Contexts that crossed a phrase boundary this wake (`beat_count %
    /// beats_per_phrase == 0`). The observable seam for cue traps / quantized
    /// flush / standing per-phrase cells; independent of the OODA arm — a phrase
    /// boundary is a position in musical time, not a turn-firing decision.
    pub phrase_due: Vec<ContextId>,
    /// Contexts that hit a rotate horizon this wake. The scheduler has ALREADY
    /// `stop`ped each one synchronously (so no further tick can fire for it); the
    /// run loop fires the `rotate` rc lifecycle for them. A rotating context
    /// appears here and NOT in `ooda_due`/`phrase_due` (it retired this beat).
    pub rotate_due: Vec<ContextId>,
}

/// The coalescing beat scheduler / transport. Holds the kernel (timelines +
/// CAS), the block store (materialization), and an optional kj dispatcher (to
/// fire the `tick` verb). The dispatcher is optional so the core is unit-testable
/// without a full server.
pub struct BeatScheduler {
    kernel: Arc<Kernel>,
    documents: SharedBlockStore,
    dispatcher: Option<Arc<KjDispatcher>>,
    /// The wakeup heap, keyed by **track** now — one entry per playing track, not
    /// per context. A track beats once per pop and wakes its attached contexts.
    heap: BinaryHeap<Reverse<(Instant, TrackId, u64)>>,
    /// The live clock domains, keyed by [`TrackId`]. Replaces the old per-context
    /// `armed` map: the clock + playhead live on the track now (`docs/tracks.md`).
    tracks: HashMap<TrackId, TrackState>,
    /// Reverse index context→track, maintained on attach/detach, so the
    /// turn-completion handler and the env builder can find a context's track
    /// without scanning every track's `attached` set.
    context_track: HashMap<ContextId, TrackId>,
    /// The write-barrier derivation registry (ABC → MIDI). One per scheduler — the
    /// derivers are stateless and shared across all tracks.
    derivers: DeriverRegistry,
}

impl BeatScheduler {
    pub fn new(kernel: Arc<Kernel>, documents: SharedBlockStore) -> Self {
        Self {
            kernel,
            documents,
            dispatcher: None,
            heap: BinaryHeap::new(),
            tracks: HashMap::new(),
            context_track: HashMap::new(),
            derivers: DeriverRegistry::production(),
        }
    }

    /// Resolve a context's track via the reverse index.
    fn track_of(&self, ctx: ContextId) -> Option<TrackId> {
        self.context_track.get(&ctx).cloned()
    }

    /// Mint or recover the track's score context — a real, first-class context (so
    /// it's browsable/viewable in the app like any other) that holds the track's
    /// materialized score but is a *non-producer*: never armed, driven, or hydrated
    /// to a model. Built the `lost+found` way (a real row + document + drift handle,
    /// no rc lifecycle). `existing` is the id persisted on the `tracks` row: `Some`
    /// on restart recovery (rows/document/handle already persist and are
    /// re-registered at cold start), `None` for a brand-new track. A creation
    /// failure is fatal — a track with nowhere to put its score is corruption, not
    /// a fallback.
    fn ensure_score_context(
        &self,
        track_id: &TrackId,
        existing: Option<ContextId>,
    ) -> Result<ContextId, String> {
        if let Some(ctx) = existing {
            return Ok(ctx);
        }
        let score_ctx = ContextId::new();
        // `-`, not `:` — colons are reserved for tag:prefix label syntax.
        let label = format!("score-{}", track_id.as_str());
        let system = PrincipalId::system();
        if let Some(db) = self.documents.db() {
            let db = db.lock();
            let ws = db.get_or_create_default_workspace(system).map_err(|e| {
                format!("beat: score context workspace for {}: {e}", track_id.as_str())
            })?;
            let row = ContextRow {
                context_id: score_ctx,
                label: Some(label.clone()),
                provider: None,
                model: None,
                system_prompt: None,
                consent_mode: ConsentMode::Collaborative,
                context_state: ContextState::Live,
                context_type: "score".to_string(),
                created_at: now_millis() as i64,
                created_by: system,
                forked_from: None,
                fork_kind: None,
                archived_at: None,
                concluded_at: None,
                last_activity_at: None,
                promoted_at: None,
                demoted_at: None,
                paused_at: None,
                workspace_id: None,
                preset_id: None,
            };
            db.insert_context_with_document(&row, ws).map_err(|e| {
                format!("beat: creating score context for {}: {e}", track_id.as_str())
            })?;
        }
        self.documents
            .create_document(score_ctx, DocKind::Conversation, None)
            .map_err(|e| {
                format!("beat: score context document for {}: {e}", track_id.as_str())
            })?;
        self.kernel
            .drift()
            .write()
            .register(score_ctx, Some(label.as_str()), None, system)
            .map_err(|e| {
                format!("beat: registering score context for {}: {e}", track_id.as_str())
            })?;
        Ok(score_ctx)
    }

    /// Reconstruct a track's committed cell log from its score context's materialized
    /// ABC source blocks (Stage 2 WI 7, approach b) — so a freshly-(re)armed track
    /// timeline can rehydrate its `UseLastGood` pool across a restart without a
    /// separate cell-log table. Only the ABC *source* cells are committed Concrete
    /// cells (the MIDI sibling is derived at the barrier, never committed); the
    /// content-derived `ContentRef` hash matches the bytes already in durable CAS, so
    /// a later fallback resolves them. A fresh track (empty score context) yields an
    /// empty log — a clean no-op.
    fn reconstruct_score_cells(&self, score_ctx: ContextId, track_id: &TrackId) -> Vec<Cell> {
        let Ok(blocks) = self.documents.block_snapshots(score_ctx) else {
            return Vec::new();
        };
        blocks
            .into_iter()
            .filter(|b| b.content_type == ContentType::Abc)
            .filter_map(|b| {
                let tick = b.tick?;
                let track = b.track.clone().unwrap_or_else(|| track_id.clone());
                Some(Cell::concrete_on(
                    Span::instant(tick),
                    ContentRef::of(b.content.as_bytes(), ABC_MIME),
                    track,
                    b.id.principal_id,
                ))
            })
            .collect()
    }

    /// A track's current playhead, if the track has a live clock domain. Used by
    /// tests to observe musical position.
    #[cfg(test)]
    fn track_playhead(&self, track_id: &TrackId) -> Option<Tick> {
        self.tracks.get(track_id).map(|t| t.playhead)
    }

    /// A track's score context (where the materialized score lands). Test-only
    /// observer for the Stage-2 re-point.
    #[cfg(test)]
    fn score_context(&self, track_id: &TrackId) -> ContextId {
        self.tracks.get(track_id).expect("track exists").score_context
    }

    /// Attach the kj dispatcher so OODA boundaries fire the `tick` rc verb.
    pub fn with_dispatcher(mut self, dispatcher: Arc<KjDispatcher>) -> Self {
        self.dispatcher = Some(dispatcher);
        self
    }

    /// Attach a context to a track — the context announces its binding
    /// (`docs/tracks.md`, entity #3: the context drives the bind, the track holds
    /// the passive set). If the track doesn't exist yet it is created **stopped**
    /// (no heap entry until `play`), its clock seeded from `policy` and its playhead
    /// from the track's *durable* history. If the track already exists, `policy` is
    /// ignored (the track owns its own clock) and this just registers/updates the
    /// context's [`AttachedContext`]. Idempotent.
    ///
    /// Two playheads are seeded, and they seed **differently** (the review caught
    /// this — see `docs/tracks.md`):
    ///
    /// - **Track playhead** (only when *creating* the track):
    ///   `max(persisted tracks-row playhead, attaching context's max committed
    ///   tick)`. NOT the context's `max_tick` alone — a rotation child has no
    ///   committed blocks (`max_tick == 0`) and would rewind the whole lane on a
    ///   restart re-create. The durable `tracks` row + the lane's blocks ARE the
    ///   track's memory; the clock lives on the track and never leaves, so there is
    ///   no fork-copied "carried playhead" anymore.
    /// - **Per-context Timeline playhead** (every attach):
    ///   `max(this context's max committed tick, the track's current playhead)`.
    ///   The context's own max stops a context WITH committed history (a cold-restart
    ///   re-attach) from being seeded behind its log → a `DuplicateBlock` cascade;
    ///   the track's playhead places a FRESH child joining a running track at current
    ///   musical time, so its next beat is one `advance_to` step, not a catch-up.
    ///
    /// A failed DB read at either seed is **fatal** (`Err`): a rewound seed
    /// overwrites committed history, so fail loud and let the caller retry (crash
    /// over corruption, per CLAUDE.md).
    ///
    /// CONSTRAINT (design §11.5): we seed the playhead tick but deliberately do NOT
    /// seed the fallback pool from the persisted log — the Timeline's committed Vec
    /// (the `UseLastGood` candidate pool) is born empty so a fresh/re-attached track
    /// resolves `UseLastGood` to Skip until its first good phrase. No silent
    /// warm-restart vamping (crash/silence over corruption).
    pub fn attach(
        &mut self,
        track_id: TrackId,
        context_id: ContextId,
        attachment: Attachment,
        policy: BeatPolicy,
    ) -> Result<(), String> {
        // Enforce one track per context (the `context_track` reverse index is 1:1).
        // If this context is already attached to a DIFFERENT track, MOVE it: detach
        // from the old track (live) and delete that stale persisted attachment row —
        // otherwise the reverse index, the `KJ_*` env injection, and a restart
        // re-attach would all see the context on two lanes (one track's beat firing
        // with another's facts; `beat_attach_payload` erroring on the ambiguous
        // `many` case). Rotation is unaffected: a forked child isn't in the index yet,
        // so this never fires for it.
        if let Some(old) = self.track_of(context_id) {
            if old != track_id {
                self.detach(&old, context_id);
                if let Some(db) = self.documents.db() {
                    let _ = db.lock().delete_attachment(old.as_str(), context_id);
                }
            }
        }
        // Read the attaching context's committed high-water ONCE — both seeds below
        // use it, and reading it before the track-create means a read failure aborts
        // BEFORE we insert a half-created (track-without-attachment) state.
        let ctx_from_log = self
            .documents
            .max_tick(context_id)
            .map_err(|e| format!("beat: attach {context_id}: reading max committed tick: {e}"))?
            .unwrap_or(Tick::ZERO);
        // Create the track's live clock domain on first attach, seeding its playhead
        // from the track's DURABLE history (the persisted `tracks` row ∪ the lane's
        // committed blocks via the attaching context), never the context alone.
        if !self.tracks.contains_key(&track_id) {
            let persisted = match self.documents.db() {
                Some(db) => db.lock().get_track(track_id.as_str()).map_err(|e| {
                    format!(
                        "beat: attach to {}: reading persisted track: {e}",
                        track_id.as_str()
                    )
                })?,
                None => None,
            };
            let persisted_playhead = persisted.as_ref().and_then(|t| t.playhead_tick);
            let persisted_score = persisted.as_ref().and_then(|t| t.score_context_id);
            let seed = ctx_from_log.max(persisted_playhead.map(Tick::new).unwrap_or(Tick::ZERO));
            // The clock lives on the TRACK, so a restart must recover it from the
            // durable `tracks` row — NOT from the attaching context's `policy` DTO
            // (which defaults to 120 BPM). `set_tempo` persists `period_ms` precisely
            // so the saved tempo survives; reverting to the DTO here would silently
            // discard it on the first re-attach (gemini-pro Stage-3 SEV-1). The DTO
            // `policy` is the seed ONLY for a genuinely fresh track (no persisted row).
            // `period_ms`/`beats_per_phrase` are validated non-zero/non-negative on
            // read (`get_track`), so this can't resurrect a corrupt clock.
            let active_policy = match persisted.as_ref() {
                Some(t) => BeatPolicy {
                    period: Duration::from_millis(t.period_ms),
                    beats_per_phrase: t.beats_per_phrase,
                },
                None => policy,
            };
            // Reconstruct the clock SOURCE from the persisted `clock_kind` (Stage 3
            // WI 3) — a track slaved to MIDI must re-arm as that driver, not silently
            // revert to the system clock. M1 only builds "system"; a "modeled" row
            // crashes loud (from_persisted). A fresh track defaults to the system clock.
            // Computed BEFORE any side effect (the score-context mint below) so a
            // corrupt clock_kind fails the attach clean, with no half-created state.
            let active_clock = match persisted.as_ref() {
                Some(t) => ClockSourceKind::from_persisted(&t.clock_kind, active_policy.period)?,
                None => ClockSourceKind::system(active_policy.period),
            };
            let score_context = self.ensure_score_context(&track_id, persisted_score)?;
            // Arm the TRACK's timeline once, at creation — the clock + open future +
            // committed score live here now and never leave when a producer rotates out.
            let tl = self.kernel.arm_track_timeline(track_id.clone(), active_policy.clock(), seed);
            // Rehydrate the committed log from the score's durable blocks (WI 7b) so
            // `UseLastGood` survives a restart. The materialize cursor starts PAST these
            // — they're already materialized — so they're never re-emitted. A fresh
            // track yields an empty log (UseLastGood→Skip until the first good phrase).
            let restored = self.reconstruct_score_cells(score_context, &track_id);
            let restored_n = restored.len();
            tl.lock().rehydrate_committed(restored);
            self.tracks.insert(
                track_id.clone(),
                TrackState {
                    clock: active_clock,
                    beats_per_phrase: active_policy.beats_per_phrase,
                    playing: false,
                    generation: 0,
                    playhead: seed,
                    beat_count: 0,
                    last_epoch_ns: 0,
                    score_context,
                    cursor: MaterializeCursor { high_water: restored_n, ..Default::default() },
                    materialize_failures: 0,
                    failure_water: 0,
                    attached: HashMap::new(),
                    last_fire_scheduled: Instant::now(),
                },
            );
        }
        // A context whose committed history sits AHEAD of the track (a restart where
        // the persisted track playhead lagged the lane's blocks) pulls the track up to
        // that frontier — both the scheduler's `track.playhead` and the track timeline —
        // so a thick joining producer isn't frozen behind a stale clock. `seed` already
        // covered this when the track was first created; this only fires when a LATER
        // joiner exceeds an existing track.
        let track_playhead = self.tracks[&track_id].playhead;
        if ctx_from_log > track_playhead {
            if let Some(t) = self.tracks.get_mut(&track_id) {
                t.playhead = ctx_from_log;
            }
            if let Some(tl) = self.kernel.track_timeline(&track_id) {
                let mut g = tl.lock();
                if ctx_from_log > g.playhead() {
                    g.advance_to(ctx_from_log);
                }
            }
        }
        // Register/update the binding (re-attach refreshes the announced attachment).
        let track = self.tracks.get_mut(&track_id).expect("track exists");
        track
            .attached
            .entry(context_id)
            .and_modify(|ac| ac.attachment = attachment)
            .or_insert_with(|| AttachedContext::new(attachment));
        self.context_track.insert(context_id, track_id.clone());
        // Mirror to the DB so a restart can recover the track + binding. Loud but not
        // fatal: the live state is correct; the cold-start re-arm sweep is deferred.
        let _ = self.persist_track(&track_id);
        let _ = self.persist_attachment(&track_id, context_id);
        Ok(())
    }

    /// Mirror a track's clock state into the `tracks` row — the durable copy a
    /// restart reads to recover the clock domain. Unlike the old per-context persist,
    /// there is **no playhead-snapshot-before-stop dance**: the playhead lives ON the
    /// track and never leaves, so the row simply mirrors `track.playhead`. A db-less
    /// store is a clean no-op (`Ok`); a write failure is loud-but-tolerable (the live
    /// clock is correct; the cold-start re-arm sweep is deferred anyway).
    fn persist_track(&self, track_id: &TrackId) -> Result<(), String> {
        let Some(track) = self.tracks.get(track_id) else {
            return Ok(()); // unknown track: nothing to mirror
        };
        let Some(db) = self.documents.db() else {
            return Ok(()); // db-less store (embedded/test): nothing to persist to.
        };
        // Playhead is forward-only from `Tick::ZERO`, so the i64 coordinate is
        // non-negative; `Some(..)` once the track exists (it has a real playhead).
        let row = PersistedTrack {
            track_id: track_id.as_str().to_string(),
            period_ms: track.clock.period().as_millis() as u64,
            beats_per_phrase: track.beats_per_phrase,
            playhead_tick: Some(track.playhead.get()),
            playing: track.playing,
            score_context_id: Some(track.score_context),
            // MUTABLE: mirror the live driver's kind so a restart reconstructs the
            // same clock source. M1 only ever writes "system".
            clock_kind: track.clock.kind_str().to_string(),
        };
        db.lock().upsert_track(&row).map_err(|e| {
            let msg = format!("beat: failed to persist track {}: {e}", track_id.as_str());
            log::error!("{msg}");
            msg
        })
    }

    /// Mirror one context's binding into the `attachments` row — the durable copy a
    /// fork inherits (via `insert_forked_context`'s `copy_attachments_for_fork`) and
    /// a restart re-announces. db-less = no-op; write failure loud-but-tolerable.
    fn persist_attachment(&self, track_id: &TrackId, context_id: ContextId) -> Result<(), String> {
        let Some(track) = self.tracks.get(track_id) else {
            return Ok(());
        };
        let Some(ac) = track.attached.get(&context_id) else {
            return Ok(()); // not attached: nothing to mirror
        };
        let Some(db) = self.documents.db() else {
            return Ok(());
        };
        let row = PersistedAttachment {
            track_id: track_id.as_str().to_string(),
            context_id,
            wakeup_every: ac.attachment.wakeup.every,
            rotate_every_phrases: ac.attachment.rotate.map(|c| c.every),
            ooda_armed: ac.attachment.ooda_armed,
        };
        db.lock().upsert_attachment(&row).map_err(|e| {
            let msg = format!(
                "beat: failed to persist attachment {} / {context_id}: {e}",
                track_id.as_str()
            );
            log::error!("{msg}");
            msg
        })
    }

    /// Start (or resume) a **track's** clock. Pushes one heap entry one period out;
    /// the playhead resumes at +1 from where it froze (event-counted — no catch-up).
    ///
    /// Bumping the **generation** invalidates any pre-existing heap entry (e.g. the one
    /// re-pushed by the last beat before a `stop`/`pause`), so a `stop`/`pause` then
    /// `play` *within one period* beats exactly once: the stale entry is dropped on pop
    /// (its generation no longer matches), and only this `play`'s fresh entry processes.
    pub fn play(&mut self, track_id: &TrackId, now: Instant) {
        let Some(track) = self.tracks.get_mut(track_id) else {
            log::warn!("beat: play on unknown track {} ignored", track_id.as_str());
            return;
        };
        if track.playing {
            return; // already running — don't stack heap entries
        }
        track.playing = true;
        track.generation += 1; // invalidate any stale in-flight heap entry
        let generation = track.generation;
        let period = track.clock.period();
        self.heap.push(Reverse((now + period, track_id.clone(), generation)));
        let _ = self.persist_track(track_id);
    }

    /// Hold a track's clock — the playhead freezes. The stale heap entry is dropped
    /// on its next pop. Per-attachment OODA arm + rotate cadence are preserved.
    /// Publish a transport flush directive to every attached sink (docs/midi.md
    /// "Render is a wire cue"): drop the speculation lead's buffered phrase +
    /// silence sounding notes, so a stop/pause doesn't play on. Keyed by the
    /// track's score context.
    ///
    /// This is a **contentless directive cue** by design — the mime IS the
    /// message and the payload is deliberately empty. It rides `RenderCue` (not a
    /// separate `BlockFlow` variant) so it fans out on the exact plumbing the
    /// render cues use and the sink dispatches it by mime like any other. Not
    /// gated on `topic_subscribers` like `publish_render_cues`: it's cheap (no CAS
    /// read) and a late-attaching sink flushing a queue it hasn't filled is a
    /// harmless no-op.
    fn publish_render_flush(&self, context_id: ContextId) {
        let cue = kaijutsu_audio::RenderCue {
            mime: kaijutsu_audio::RENDER_FLUSH_MIME.to_string(),
            payload: kaijutsu_audio::CuePayload::Inline(Vec::new()),
            lead: Duration::ZERO,
        };
        self.kernel
            .block_flows()
            .publish(BlockFlow::RenderCue { context_id, cue });
    }

    pub fn pause(&mut self, track_id: &TrackId) {
        let mut flush_ctx = None;
        if let Some(track) = self.tracks.get_mut(track_id) {
            track.playing = false;
            // The speculation lead means a sink's device queue holds ~a phrase of
            // FUTURE events; the clock stopping won't unschedule them. The flush
            // cue below tells every attached wire sink to truncate everything
            // after `now` + silence sounding notes (Stage 3 review SEV-1), so a
            // pause doesn't blindly play the buffered phrase.
            flush_ctx = Some(track.score_context);
        }
        if let Some(ctx) = flush_ctx {
            self.publish_render_flush(ctx);
        }
        let _ = self.persist_track(track_id);
    }

    /// Stop a track's clock — MIDI idiom: **stop = stop the clock only**. Rotation is
    /// suspended/remembered (the attachments keep their `rotate` cadence) and
    /// per-attachment OODA arm is untouched (re-arm with `SetOoda`). The playhead is
    /// forward-only, so `stop` then `play` resumes where it froze (no rewind), which
    /// makes it behaviourally an alias of `pause` here — both kept for transport
    /// vocabulary.
    pub fn stop(&mut self, track_id: &TrackId) {
        let mut flush_ctx = None;
        if let Some(track) = self.tracks.get_mut(track_id) {
            track.playing = false;
            // Same as `pause`: the flush cue below tells every attached wire sink
            // to truncate the buffered (lead-time) events past `now`, so a stop
            // doesn't play out ~a phrase of queued notes and leave them hanging
            // (Stage 3 review SEV-1).
            flush_ctx = Some(track.score_context);
        }
        if let Some(ctx) = flush_ctx {
            self.publish_render_flush(ctx);
        }
        let _ = self.persist_track(track_id);
    }

    /// Set a track's beat period (tempo). Takes effect on the next beat. Routes to
    /// the clock source (the single source of truth for the period) AND re-slaves
    /// the armed track `Timeline`'s speculation `TickClock` (WI 2) so the lead can't
    /// go stale against the new firing period — a sped-up clock would otherwise
    /// under-lead and wedge the fallback. The new `TickClock` is derived from the
    /// now-updated live period via `phrasing().clock()`.
    pub fn set_tempo(&mut self, track_id: &TrackId, period: Duration) {
        let new_tick_clock = self.tracks.get_mut(track_id).map(|track| {
            track.clock.set_period(period);
            track.phrasing().clock()
        });
        if let Some(tick_clock) = new_tick_clock {
            match self.kernel.track_timeline(track_id) {
                Some(tl) => tl.lock().set_clock(tick_clock),
                // A track in `self.tracks` always has an armed timeline today
                // (attach arms it on create, detach never disarms). If that ever
                // stops holding, the firing period would advance while the
                // speculation TickClock stayed stale — a silent divergence that
                // under-leads and wedges the fallback. Make it observable rather
                // than let it pass quietly.
                None => log::warn!(
                    "beat: set_tempo on {} updated the firing clock but found no armed \
                     timeline to re-slave; speculation lead may be stale",
                    track_id.as_str()
                ),
            }
        }
        // Persist the new tempo so a restart recovers it (not the default).
        let _ = self.persist_track(track_id);
    }

    /// Switch the track's beat driver (`docs/midi.md` M3). The current period
    /// carries over — a track slaved mid-song keeps its tempo until the first
    /// reference arrives, and un-slaving freezes the last modeled tempo into
    /// the system clock. Persisted (`clock_kind` is mutable over a track's
    /// life, Stage 3 lock); a same-kind set is an idempotent no-op.
    pub fn set_clock(&mut self, track_id: &TrackId, kind: ClockKind) {
        if let Some(track) = self.tracks.get_mut(track_id) {
            let period = track.clock.period();
            let already = matches!(
                (&track.clock, kind),
                (ClockSourceKind::System(_), ClockKind::System)
                    | (ClockSourceKind::Modeled(_), ClockKind::Modeled)
            );
            if !already {
                track.clock = match kind {
                    ClockKind::System => ClockSourceKind::system(period),
                    ClockKind::Modeled => ClockSourceKind::modeled(period),
                };
                log::info!(
                    "beat: track {} clock → {}",
                    track_id.as_str(),
                    track.clock.kind_str()
                );
            }
        }
        let _ = self.persist_track(track_id);
    }

    /// Apply one observer clock reference (M3) to the sender's track.
    /// Ambient-lenient where `commit_capture` is strict: references flow
    /// before anyone decides to slave, so an unattached sender or a
    /// system-clock track drops the reference at debug — only a *stale*
    /// stamp is loud (it means the pipe is backed up or a clock is skewed).
    fn apply_clock_estimate(
        &mut self,
        context_id: ContextId,
        beat: f64,
        tempo_bps: f64,
        epoch_ns: u64,
        source: &str,
    ) {
        let Some(track_id) = self
            .tracks
            .iter()
            .find(|(_, t)| t.attached.contains_key(&context_id))
            .map(|(id, _)| id.clone())
        else {
            log::debug!("beat: clock estimate from unattached {context_id} ({source}); dropped");
            return;
        };
        // Re-anchor the observer's wallclock stamp at receipt into the local
        // Instant domain (the RenderCue receipt+lead move, run backwards; a
        // cross-node wallclock offset lands here — the recorded NTP caveat).
        let now_ns = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0);
        let age_ns = now_ns.saturating_sub(epoch_ns);
        if age_ns > 5_000_000_000 {
            log::warn!(
                "beat: clock estimate from {source} is {} ms stale; dropped",
                age_ns / 1_000_000
            );
            return;
        }
        let at = Instant::now() - Duration::from_nanos(age_ns);

        let Some(track) = self.tracks.get_mut(&track_id) else {
            return;
        };
        match &mut track.clock {
            ClockSourceKind::Modeled(m) => {
                m.apply_estimate(beat, tempo_bps, at);
                // Re-slave the speculation TickClock to the slewed period
                // (the set_tempo pattern; cheap per ~2 Hz reference).
                let tick_clock = track.phrasing().clock();
                match self.kernel.track_timeline(&track_id) {
                    Some(tl) => tl.lock().set_clock(tick_clock),
                    None => log::warn!(
                        "beat: clock estimate updated track {} but found no armed \
                         timeline to re-slave; speculation lead may be stale",
                        track_id.as_str()
                    ),
                }
            }
            ClockSourceKind::System(_) => {
                log::debug!(
                    "beat: clock estimate from {source} at system-clock track {}; \
                     ignored (not slaved)",
                    track_id.as_str()
                );
            }
        }
    }

    /// Arm or disarm one attached context's OODA loop, without touching the clock.
    pub fn set_ooda(&mut self, track_id: &TrackId, context_id: ContextId, armed: bool) {
        if let Some(track) = self.tracks.get_mut(track_id) {
            if let Some(ac) = track.attached.get_mut(&context_id) {
                ac.attachment.ooda_armed = armed;
            }
        }
        let _ = self.persist_attachment(track_id, context_id);
    }

    /// Set (or clear, with `None`) one attached context's self-fork rotate cadence
    /// (in phrases). A `Some(Cadence { every: 0 })` is treated as `None` — never a
    /// `% 0`.
    pub fn set_rotate(
        &mut self,
        track_id: &TrackId,
        context_id: ContextId,
        every: Option<Cadence>,
    ) {
        if let Some(track) = self.tracks.get_mut(track_id) {
            if let Some(ac) = track.attached.get_mut(&context_id) {
                ac.attachment.rotate = every.filter(|c| c.every > 0);
            }
        }
        // Persist so a restart restores the page-turn cadence.
        let _ = self.persist_attachment(track_id, context_id);
    }

    /// Unbind a context from a track — **LIVE-ONLY**. Removes it from the track's
    /// attached set + the reverse index, but does NOT delete the persisted
    /// `attachments` row: the rotate page-turn relies on the child inheriting the
    /// parent's binding via the fork-copy of that row (automatic in
    /// `insert_forked_context`), so deleting it here would break inheritance.
    /// Persisted-row cleanup belongs to context archival (not wired in Stage 1).
    ///
    /// Stage 2: detach does NOT touch the TRACK timeline — the clock + score live on
    /// the track and outlive any producer (continuity is free; that's the whole
    /// point). The track timeline is dropped only on track teardown (not wired yet).
    pub fn detach(&mut self, track_id: &TrackId, context_id: ContextId) {
        if let Some(track) = self.tracks.get_mut(track_id) {
            track.attached.remove(&context_id);
        }
        // Persist the track's playhead at the moment a context leaves — most
        // importantly the rotate-horizon handoff, which is the durable record a
        // 0-block child re-creating the track inherits across a crash (the beat path
        // deliberately does NOT persist every beat; detach is rare). Without this a
        // crash in the rotation gap would re-seed the track from a stale row and
        // rewind the lane (gemini review 2026-06-29).
        let _ = self.persist_track(track_id);
        // Keep the reverse index honest: if this context still rides another lane,
        // point it there; otherwise drop it. (No per-context timeline to disarm
        // anymore — the score is on the track.)
        let other = self
            .tracks
            .iter()
            .find_map(|(tid, t)| (tid != track_id && t.attached.contains_key(&context_id)).then(|| tid.clone()));
        match other {
            Some(survivor) => {
                self.context_track.insert(context_id, survivor);
            }
            None => {
                self.context_track.remove(&context_id);
            }
        }
    }

    /// The next wake instant, if any context is scheduled.
    pub fn next_wake(&self) -> Option<Instant> {
        self.heap.peek().map(|Reverse((t, _, _))| *t)
    }

    /// Beat every **track** due at or before `now`: advance the track one beat, wake
    /// its attached contexts (materialize + cadence), and re-arm the track's next
    /// beat. A popped entry is dropped (not re-armed) when its track is stopped/removed
    /// OR its generation is stale (a `play` re-enlisted the track after this entry was
    /// pushed — the generation token, so a stop+play within one period beats once).
    pub fn fire_due(&mut self, now: Instant) -> BeatOutcome {
        let mut outcome = BeatOutcome::default();
        // `TrackId` isn't `Copy`, so clone the peeked key (one short String).
        while let Some(Reverse((t, track_id, generation))) = self.heap.peek().cloned() {
            if t > now {
                break;
            }
            self.heap.pop();
            let Some((playing, period, cur_gen)) = self
                .tracks
                .get(&track_id)
                .map(|tr| (tr.playing, tr.clock.period(), tr.generation))
            else {
                continue; // unknown/removed track → drop the stale entry
            };
            if !playing {
                continue; // stopped/paused → drop the stale entry (no re-arm)
            }
            if generation != cur_gen {
                continue; // a later `play` re-enlisted the track → this entry is stale
            }
            // Latch the SCHEDULED fire instant (`t`, the heap entry's deadline) BEFORE
            // processing — this is the jitter-free reference the render seam schedules
            // off, not the late actual wakeup `now` (Stage 3 review, deepseek).
            if let Some(tr) = self.tracks.get_mut(&track_id) {
                tr.last_fire_scheduled = t;
            }
            self.process_track(&track_id, &mut outcome);
            // Re-arm the TRACK's next beat under the SAME generation (a normal beat
            // doesn't bump it). Rotation detaches CONTEXTS inside `process_track`, never
            // the track — the clock keeps running across a page-turn (continuity is
            // free; the production gap during the child's boot is absorbed by the
            // speculation lead downstream, docs/tracks.md).
            self.heap.push(Reverse((now + period, track_id, cur_gen)));
        }
        outcome
    }

    /// Beat one track: advance its playhead once (event-counted), latch the beat's
    /// shared wall-clock epoch, then for EACH attached context slew its Timeline to
    /// the track playhead, materialize whatever committed, and answer the cadence
    /// questions. A context at a rotate horizon is detached synchronously this beat
    /// (the clock keeps running). Called only for a playing track.
    fn process_track(&mut self, track_id: &TrackId, outcome: &mut BeatOutcome) {
        // Latch the beat's wall-clock epoch ONCE so every context woken this beat
        // sees the identical `KJ_EPOCH_NS` (the cross-context join key) — never a
        // per-context `now()` that would differ by microseconds.
        let epoch_ns = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0);
        // Phase 1: advance the track once (the brief &mut tracks borrow).
        let (beat_count, playhead, policy, attached_ids) = {
            let Some(track) = self.tracks.get_mut(track_id) else {
                return;
            };
            track.playhead = track.playhead + STEP;
            track.beat_count += 1;
            track.last_epoch_ns = epoch_ns;
            let ids: Vec<ContextId> = track.attached.keys().cloned().collect();
            (track.beat_count, track.playhead, track.phrasing(), ids)
        };
        // Phase 2: per attached context — materialize, then answer the cadence. No
        // &mut tracks borrow is held across `materialize_one` (it re-borrows briefly).
        let mut to_detach: Vec<ContextId> = Vec::new();
        // Materialize the track's newly-committed score ONCE this beat (gemini: hoist
        // out of the per-context loop, or N attached contexts would re-emit each cell),
        // then drain the track's failure ledger to each producer. The score is the
        // track's; the cadence below is per-context.
        self.materialize_track(track_id, playhead);
        // Emit a beat reference for the app's continuous timebase (the metronome
        // phasor) at a LOW rate while the clock rolls — NOT every beat. The
        // phasor free-runs at the reference tempo between references and only
        // gently slews toward each one; a reference every beat would make it
        // chase the kernel's per-beat scheduler jitter and oscillate (audible
        // wobble). So anchor promptly on the first beat, then correct every
        // `BEAT_SYNC_EVERY` (drift is ~0 on one machine, so this stays locked;
        // it also cuts the wire chatter). The click is the sink's opt-in
        // (docs/midi.md "The relative-lead timebase, analyzed").
        if beat_count == 1 || beat_count % BEAT_SYNC_EVERY == 0 {
            self.publish_beat_sync(track_id, playhead);
        }
        self.drain_track_failures(track_id);
        for ctx in attached_ids {
            let Some(att) = self
                .tracks
                .get(track_id)
                .and_then(|t| t.attached.get(&ctx))
                .map(|ac| ac.attachment)
            else {
                continue;
            };
            outcome.fired.push(ctx);
            // Rotate horizon: a phrase boundary whose phrase index is a multiple of
            // the cadence. NOT gated on `ooda_armed` — a paused-OODA player still owes
            // its page-turn. A rotating context retires this beat (detached below) and
            // does NOT also fire ooda/phrase.
            let phrase_due = policy.is_phrase_boundary(beat_count);
            let phrase = if policy.beats_per_phrase > 0 {
                beat_count / policy.beats_per_phrase
            } else {
                0
            };
            let rotate_due = phrase_due
                && policy.beats_per_phrase > 0
                && att.rotate.is_some_and(|c| c.is_due(phrase));
            if rotate_due {
                to_detach.push(ctx);
                outcome.rotate_due.push(ctx);
                continue;
            }
            if phrase_due {
                outcome.phrase_due.push(ctx);
            }
            // OODA wake: gated on the arm (it drives token spend) and the
            // per-attachment wakeup divisor (subsumes the old `ooda_every`).
            if att.ooda_armed && att.wakeup.is_due(beat_count) {
                // Bump the per-attachment monotonic pulse (`KJ_PULSE`, the reliable
                // ordering key) on the wake — once per THIS attachment's wakeup, so a
                // sibling waking on other beats can't skip its sequence.
                if let Some(ac) = self
                    .tracks
                    .get_mut(track_id)
                    .and_then(|t| t.attached.get_mut(&ctx))
                {
                    ac.attachment.pulse += 1;
                }
                outcome.ooda_due.push(ctx);
            }
        }
        // Synchronous detach of rotating contexts — AFTER the iteration so we never
        // mutate `attached` while iterating it. The clock never pauses across the
        // handoff: this is the GAP, never the OVERLAP (never two producers; a few
        // producerless beats while the child boots, covered downstream by the
        // speculation lead — docs/tracks.md "Rotation handoff").
        for ctx in to_detach {
            self.detach(track_id, ctx);
        }
    }

    /// Resolve which attached context a `played_by` principal belongs to, so a
    /// failure/poison on a cell that principal authored surfaces in ITS conversation.
    /// Matched by the producing principal recorded on the attachment; if none match
    /// but exactly one context is attached (the music / single-producer case), it's
    /// that one. `None` → an orphaned producer (e.g. it rotated out) — the caller
    /// routes to the score context so the failure is never silently dropped.
    fn producer_ctx_for(&self, track_id: &TrackId, played_by: PrincipalId) -> Option<ContextId> {
        let track = self.tracks.get(track_id)?;
        if let Some((ctx, _)) = track
            .attached
            .iter()
            .find(|(_, ac)| ac.producer_principal == Some(played_by))
        {
            return Some(*ctx);
        }
        if track.attached.len() == 1 {
            return track.attached.keys().next().copied();
        }
        None
    }

    /// Land a captured-MIDI batch (`docs/midi.md` M2) as ONE data-only block in
    /// the score context of the track `context_id` is attached to.
    ///
    /// Synchronous compute-and-reply like `apply_command` (no awaits). The
    /// batch is parsed loud (a bad batch is a refusal, never a silent drop),
    /// annotated with the quantization anchor (`ext.quantize`) so every
    /// event's grid residual stays derivable from the record forever, and
    /// inserted ephemeral (a durable score record, not a conversation turn —
    /// same hydration-silent stance as materialized score blocks). Captured
    /// events are *past*-stamped, so they bypass the speculation timeline and
    /// its no-backdating write barrier entirely: this is an observation log
    /// landing beside the score, not a cell crossing it. Invisible by design
    /// to `heard_json` and `reconstruct_score_cells` (both filter
    /// `ContentType::Abc`) — score first, perception later.
    fn commit_capture(
        &mut self,
        context_id: ContextId,
        payload: Vec<u8>,
        played_by: PrincipalId,
    ) -> Result<BlockId, String> {
        let mut batch = kaijutsu_audio::CaptureBatch::parse(&payload)
            .map_err(|e| format!("capture batch rejected: {e}"))?;

        // Exactly one track may claim the capture context. Zero is the
        // not-attached refusal. More than one is structurally impossible —
        // `attach` enforces the 1:1 `context_track` reverse index by MOVING a
        // re-attached context — so the multi-claim branch below is a
        // defensive invariant guard (loud refusal, never a HashMap coin flip
        // over which grid quantizes), same stance as materialize's
        // non-concrete-cell bail.
        let mut claims = self
            .tracks
            .iter()
            .filter(|(_, t)| t.attached.contains_key(&context_id));
        let (track_id, score_ctx, period, anchor_ns, playhead, playing) = claims
            .next()
            .map(|(id, t)| {
                (id.clone(), t.score_context, t.clock.period(), t.last_epoch_ns, t.playhead, t.playing)
            })
            .ok_or_else(|| {
                format!(
                    "capture context {context_id} is not attached to any track — \
                     `kj transport attach` first"
                )
            })?;
        if let Some((other, _)) = claims.next() {
            return Err(format!(
                "capture context {context_id} is attached to multiple tracks \
                 ({}, {}, …) — capture needs exactly one",
                track_id.as_str(),
                other.as_str()
            ));
        }
        drop(claims);

        // Quantize against the track's live anchor: the last beat latched
        // (playhead, last_epoch_ns) as a (tick, wallclock) pair and 1 tick ==
        // 1 beat (STEP). Before the first beat, or while stopped/paused, there
        // is no rolling grid — the whole batch lands at the frozen playhead
        // (ties at a tick are structurally allowed) and the events' raw
        // epochs stay the only time truth. The anchor is recorded on the
        // record itself so consumers (the M3 drift model, a future deriver)
        // can recompute every event's exact grid residual without us storing
        // per-event ticks.
        let period_ns = period.as_nanos() as i64;
        let rolling = playing && anchor_ns > 0 && period_ns > 0;
        let window_tick = if rolling {
            let offset_ns = batch.window_start_ns as i64 - anchor_ns as i64;
            let beats = (offset_ns as f64 / period_ns as f64).round() as i64;
            playhead + TickDelta::new(beats)
        } else {
            playhead
        };
        batch.ext.insert(
            "quantize".into(),
            serde_json::json!({
                "anchor_epoch_ns": anchor_ns,
                "anchor_tick": playhead.get(),
                "period_ns": period_ns,
                "ticks_per_beat": STEP.0,
                "rolling": rolling,
            }),
        );
        let content = String::from_utf8(
            batch
                .to_json_bytes()
                .map_err(|e| format!("capture batch re-serialize: {e}"))?,
        )
        .map_err(|e| format!("capture batch is not UTF-8 JSON: {e}"))?;

        let block_id = self
            .documents
            .reserve_block_id(score_ctx, played_by)
            .map_err(|e| format!("capture commit reserve: {e}"))?;
        let snapshot = kaijutsu_types::BlockSnapshotBuilder::new(block_id, kaijutsu_types::BlockKind::Text)
            .tick(window_tick)
            .role(kaijutsu_types::Role::Asset)
            .content(content)
            .content_type(ContentType::from_mime(kaijutsu_audio::MIDI_CAPTURE_MIME))
            .track(track_id)
            .ephemeral(true)
            .build();
        let after = self.documents.last_block_id(score_ctx);
        self.documents
            .insert_from_snapshot_as(score_ctx, snapshot, after.as_ref(), Some(played_by))
            .map_err(|e| format!("capture commit insert: {e}"))
    }

    /// Materialize the TRACK's newly-committed score this beat: pump the track
    /// timeline to the playhead (firing speculate/commit/squash), then run the
    /// CAS+block bridge ONCE into the track's score context with the track's single
    /// cursor + poison budget. A poison-skip is surfaced as an Error block attributed
    /// to the poisoned cell's producer. (The failure-ledger drain is a separate
    /// per-track pass, `drain_track_failures`.)
    fn materialize_track(&mut self, track_id: &TrackId, playhead: Tick) {
        let Some(timeline) = self.kernel.track_timeline(track_id) else {
            return;
        };
        let Some(score_ctx) = self.tracks.get(track_id).map(|t| t.score_context) else {
            return;
        };
        // Pump the track timeline to the beat's playhead — forward-only (`advance_to`
        // panics on backward time, the no-backdating write barrier). This is the
        // legit clock drive, not the deleted per-context Stage-1 bridge.
        {
            let mut g = timeline.lock();
            if playhead > g.playhead() {
                g.advance_to(playhead);
            }
        }
        let cas = self.kernel.cas().clone();
        let Some(mut cursor) = self.tracks.get(track_id).map(|t| t.cursor) else {
            return;
        };
        let hw_before = cursor.high_water;
        let result = materialize_committed(
            &timeline,
            &cas,
            &self.documents,
            score_ctx,
            &mut cursor,
            &self.derivers,
        );
        // Set when the SAME cell exhausts MATERIALIZE_RETRY_BUDGET and is skipped:
        // (skipped high_water index, last error). Surfaced once after the borrow ends.
        let mut poison_skipped: Option<(usize, String)> = None;
        match result {
            Ok(_) => {
                if let Some(t) = self.tracks.get_mut(track_id) {
                    t.cursor = cursor;
                    t.materialize_failures = 0; // a clean beat clears the poison count
                }
                // Publish a wire RenderCue for every cell that just crossed the
                // write barrier (docs/midi.md "Render is a wire cue"). No-op (and
                // no CAS reads) when nothing crossed this beat.
                self.publish_render_cues(track_id, hw_before, cursor.high_water, playhead);
            }
            Err(e) => {
                if let Some(t) = self.tracks.get_mut(track_id) {
                    if cursor.high_water > hw_before {
                        // Whole-cell progress → the poison is a FRESH cell; persist.
                        t.cursor = cursor;
                        t.materialize_failures = 0;
                        log::error!(
                            "beat: materialize failed for track {} after crossing {} \
                             cell(s) this beat; will retry the next cell: {e}",
                            track_id.as_str(),
                            cursor.high_water - hw_before
                        );
                    } else {
                        // Same cell failed again — escalate; skip once the budget is spent.
                        t.cursor = cursor;
                        t.materialize_failures += 1;
                        let failures = t.materialize_failures;
                        log::error!(
                            "beat: materialize failed for track {} on cell at high_water={} \
                             (consecutive failure #{failures} on the same cell): {e}",
                            track_id.as_str(),
                            cursor.high_water
                        );
                        if poison_action(failures) == PoisonAction::SkipPoison {
                            t.cursor.high_water += 1;
                            t.cursor.artifacts_done = 0;
                            t.cursor.source_block_id = None;
                            t.materialize_failures = 0;
                            log::error!(
                                "beat: skipping poison cell at high_water={} for track {} \
                                 after {MATERIALIZE_RETRY_BUDGET} failed attempts — cell \
                                 will NOT be materialized",
                                cursor.high_water,
                                track_id.as_str()
                            );
                            poison_skipped = Some((cursor.high_water, e.to_string()));
                        }
                    }
                }
            }
        }

        // Surface the poison-skip as an Error block in the producer's conversation
        // (anchored at its document tail). The poisoned cell stays in `committed`
        // (skipping only advances the cursor), so we read its author for attribution.
        if let Some((skipped_hw, err)) = poison_skipped {
            let played_by = {
                let g = timeline.lock();
                g.committed().get(skipped_hw).map(|c| c.played_by)
            };
            let target = played_by
                .and_then(|p| self.producer_ctx_for(track_id, p))
                .unwrap_or(score_ctx);
            let payload = kaijutsu_types::ErrorPayload {
                category: kaijutsu_types::ErrorCategory::Kernel,
                severity: kaijutsu_types::ErrorSeverity::Error,
                code: None,
                detail: Some(format!(
                    "a committed phrase could not be materialized after \
                     {MATERIALIZE_RETRY_BUDGET} attempts and was skipped: {err}"
                )),
                span: None,
                source_kind: None,
            };
            let summary = payload.summary_line();
            match self.documents.last_block_id(target) {
                Some(anchor) => {
                    if let Err(e) = self.documents.insert_error_block_as(
                        target,
                        &anchor,
                        &payload,
                        summary,
                        Some(PrincipalId::beat()),
                    ) {
                        log::error!(
                            "beat: failed to surface poison-skip Error block for {target} \
                             (skipped cell at high_water={skipped_hw}): {e}"
                        );
                    }
                }
                None => log::error!(
                    "beat: poison-skip for {target} at high_water={skipped_hw} found no anchor block"
                ),
            }
        }
    }

    /// Publish a wire `RenderCue` for every cell that crossed the write barrier
    /// this beat (`[hw_before, hw_after)` in the track's committed log) to the
    /// track's score context (docs/midi.md "Render is a wire cue"): any attached
    /// app sink renders it, a track with no sink just makes no sound. Each cue
    /// carries the **pre-resolved** ABC `&str` (so a sink never re-hits CAS) and a
    /// `lead` — the near-future local instant translated to a duration from now.
    /// Cheap exit (no CAS reads) when nothing crossed this beat. The materialize
    /// crossing already stored every crossed cell's source bytes in durable CAS,
    /// so a single `retrieve` per cell here resolves the ABC once.
    fn publish_render_cues(
        &mut self,
        track_id: &TrackId,
        hw_before: usize,
        hw_after: usize,
        playhead: Tick,
    ) {
        if hw_after <= hw_before {
            return; // nothing crossed this beat
        }
        // Sink-dependent by design (docs/midi.md "Render is a wire cue"): with no
        // subscriber on the render-cue topic (a headless kernel, no app/edge sink
        // attached) there is nobody to render, so skip the CAS reads + cue build
        // entirely. The score is still durable — `materialize_committed` wrote the
        // blocks into the score context separately; only the ephemeral render is
        // gated here. (Attach a sink later and replay.)
        if self.kernel.block_flows().topic_subscribers("block.render_cue") == 0 {
            return;
        }
        let Some(timeline) = self.kernel.track_timeline(track_id) else {
            return;
        };
        // Snapshot the newly-crossed cells' content refs + start ticks under the
        // lock; resolve the bytes afterwards (the lock is for the committed slice,
        // not the CAS read).
        let crossed: Vec<(ContentRef, Tick)> = {
            let g = timeline.lock();
            let committed = g.committed();
            (hw_before..hw_after)
                .filter_map(|i| committed.get(i))
                .filter_map(|cell| match &cell.body {
                    Body::Concrete(cref) => Some((cref.clone(), cell.span.start)),
                    // A committed cell is always concrete (timeline invariant); a
                    // non-concrete one would already have failed materialize above.
                    Body::Deferred(_) => None,
                })
                .collect()
        };
        // The jitter-free reference + the live period: `at = scheduled fire instant
        // + (cell.start − playhead) beats × period`. Cells commit AHEAD of the
        // playhead (the speculation lead), so the offset is ≥ 0 → `at` is in the
        // near future; clamp at 0 so a render is NEVER scheduled into the past.
        let Some((period, base, score_ctx)) = self
            .tracks
            .get(track_id)
            .map(|t| (t.clock.period(), t.last_fire_scheduled, t.score_context))
        else {
            return;
        };
        let cas = self.kernel.cas().clone();
        let mut rendered: Vec<(String, Instant)> = Vec::with_capacity(crossed.len());
        for (cref, start) in crossed {
            if cref.mime != ABC_MIME {
                continue; // only ABC is handed to a (MIDI) render target this stage
            }
            let bytes = match cas.retrieve(&cref.hash) {
                Ok(Some(b)) => b,
                Ok(None) => {
                    log::error!(
                        "beat: render of track {} skipped a cell — its source bytes \
                         ({}) are missing from durable CAS",
                        track_id.as_str(),
                        cref.hash.as_str()
                    );
                    continue;
                }
                Err(e) => {
                    log::error!(
                        "beat: render of track {} skipped a cell — durable CAS read \
                         for {} failed: {e}",
                        track_id.as_str(),
                        cref.hash.as_str()
                    );
                    continue;
                }
            };
            let abc = match String::from_utf8(bytes) {
                Ok(s) => s,
                Err(e) => {
                    log::error!(
                        "beat: render of track {} skipped a cell — source is not UTF-8 \
                         ABC: {e}",
                        track_id.as_str()
                    );
                    continue;
                }
            };
            rendered.push((abc, render_instant(base, period, start, playhead)));
        }
        if rendered.is_empty() {
            return;
        }
        // Publish a wire `RenderCue` per crossed ABC cell to every attached sink
        // (the app renders it to MIDI). `lead` is relative — `at` is a near-future
        // local instant on the speculation lead, so `at − now` is the transfer +
        // schedule budget the sink re-anchors at `receipt + lead`.
        let now = Instant::now();
        for (abc, at) in &rendered {
            let lead = at.saturating_duration_since(now);
            let cue = kaijutsu_audio::RenderCue {
                mime: kaijutsu_audio::ABC_MIME.to_string(),
                payload: kaijutsu_audio::CuePayload::Inline(abc.clone().into_bytes()),
                lead,
            };
            self.kernel
                .block_flows()
                .publish(BlockFlow::RenderCue { context_id: score_ctx, cue });
        }
    }

    /// Publish a low-rate `BeatSync` reference for the app's continuous timebase
    /// (docs/midi.md "The relative-lead timebase, analyzed") once per beat while
    /// the track's clock rolls. The sink's phasor slews toward it and clicks the
    /// beats between; the beat coordinate is the playhead (1 tick == 1 beat) and
    /// the tempo is `1 / period`. Sink-dependent + gated like
    /// [`publish_render_cues`](Self::publish_render_cues): with no subscriber on
    /// `block.beat_sync` (a headless kernel, no sink attached) this emits nothing.
    fn publish_beat_sync(&self, track_id: &TrackId, playhead: Tick) {
        if self.kernel.block_flows().topic_subscribers("block.beat_sync") == 0 {
            return;
        }
        let Some((period, score_ctx)) = self
            .tracks
            .get(track_id)
            .map(|t| (t.clock.period(), t.score_context))
        else {
            return;
        };
        let secs = period.as_secs_f64();
        if secs <= 0.0 {
            return; // a degenerate (zero) period has no defined tempo
        }
        let beat_ref = kaijutsu_audio::BeatRef {
            beat: playhead.get() as f64,
            tempo_bps: 1.0 / secs,
        };
        self.kernel
            .block_flows()
            .publish(BlockFlow::BeatSync { context_id: score_ctx, beat_ref });
    }

    /// Drain the engine failure ledger past `failure_water`, surfacing exactly one
    /// `BlockKind::Error` block per new `FailureEvent` (design §6, §8). The ledger
    /// is the data source for the "ABC parse-failure rate" eval ruler AND the
    /// player's own feedback channel: an erring resolve (CAS read miss, validator
    /// reject) drops its cell silently in the engine, so the ONLY way the player
    /// learns of the miss is this surfacing. `failure_water` is monotone, so a
    /// persistent ledger never re-surfaces a drained event on later beats.
    ///
    /// Error blocks anchor at the document tail (the failure carries musical ticks,
    /// not a source block id — the cell that would have been its anchor never
    /// committed). A missing anchor or insert failure is loud, never a swallow.
    fn drain_track_failures(&mut self, track_id: &TrackId) {
        let Some(timeline) = self.kernel.track_timeline(track_id) else {
            return;
        };
        // Snapshot the new events (with their author) under the lock, then release it
        // before touching the block store (no nested lock; no `.await` here).
        let new_events: Vec<(kaijutsu_types::Tick, PrincipalId, String, String)> = {
            let g = timeline.lock();
            let failures = g.failures();
            let Some(water) = self.tracks.get(track_id).map(|t| t.failure_water) else {
                return;
            };
            if failures.len() <= water {
                return; // nothing new since the last drain
            }
            failures[water..]
                .iter()
                .map(|ev| (ev.start, ev.played_by, ev.resolver.as_str().to_string(), ev.error.clone()))
                .collect()
        };

        for (start, played_by, resolver, error) in &new_events {
            // Route to the PRODUCING context (so a player reads its own failures, not
            // a sibling's); an orphan (producer rotated out) goes to the score context
            // rather than being dropped.
            let score_ctx = self.tracks.get(track_id).map(|t| t.score_context);
            let Some(target) = self.producer_ctx_for(track_id, *played_by).or(score_ctx) else {
                continue;
            };
            let payload = kaijutsu_types::ErrorPayload {
                category: kaijutsu_types::ErrorCategory::Parse,
                severity: kaijutsu_types::ErrorSeverity::Error,
                code: None,
                detail: Some(format!(
                    "a scheduled phrase failed to resolve at beat {} (resolver {resolver}): {error}",
                    start.get()
                )),
                span: None,
                source_kind: None,
            };
            let summary = payload.summary_line();
            let Some(anchor) = self.documents.last_block_id(target) else {
                log::error!(
                    "beat: failure ledger drain for {target} found no anchor block for the \
                     resolve failure at beat {} (resolver {resolver}): {error}",
                    start.get()
                );
                continue;
            };
            if let Err(e) = self.documents.insert_error_block_as(
                target,
                &anchor,
                &payload,
                summary,
                Some(PrincipalId::beat()),
            ) {
                log::error!(
                    "beat: failed to surface resolve-failure Error block for {target} \
                     (start {}): {e}",
                    start.get()
                );
            }
        }

        // Advance the single track water past every event observed this beat — even
        // any whose insert errored (already logged). Re-surfacing would spam.
        if let Some(t) = self.tracks.get_mut(track_id) {
            t.failure_water += new_events.len();
        }
    }

    /// Snapshot the transport heartbeat for `ctx` — `KJ_TICK`/`KJ_PHRASE`/`KJ_TEMPO`
    /// plus `KJ_HEARD`, `KJ_PULSE`, `KJ_EPOCH_NS` (and `KJ_ROTATE_EVERY` when it
    /// rotates) — read NOW, on the scheduler thread with the track + attachment in
    /// hand, so a spawned lifecycle sees facts coherent with the beat that triggered
    /// it. The position facts come from the TRACK (it owns the clock now). Empty when
    /// the context isn't attached to a track.
    fn transport_env(&self, ctx: ContextId) -> HashMap<String, String> {
        let Some(track_id) = self.track_of(ctx) else {
            return HashMap::new();
        };
        let Some(track) = self.tracks.get(&track_id) else {
            return HashMap::new();
        };
        let Some(ac) = track.attached.get(&ctx) else {
            return HashMap::new();
        };
        let playhead = track.playhead;
        let mut vars: HashMap<String, String> =
            transport_vars(playhead, track.beat_count, &track.phrasing())
                .into_iter()
                .collect();
        // KJ_PHRASE_BEATS: the length of one phrase window in beats — the amount of
        // music a musician should compose per turn (the schedule lead is one phrase,
        // so a turn's phrase tiles the next window). The tick rc precomputes bars
        // from this (kaish math) and hands the model spelled-out fill targets, so the
        // model never has to do the arithmetic. 0 means "no phrasing" (compose freely).
        vars.insert("KJ_PHRASE_BEATS".to_string(), track.beats_per_phrase.to_string());
        // KJ_HEARD: the recent committed notation the player composes against — read
        // from the TRACK's score context now, so it's the real band view (the whole
        // lane across every producer + rotation), not just this context's own blocks.
        // A read failure degrades to an empty list (compose from the chart + now-facts).
        let window = TickDelta::new((HEARD_WINDOW_PHRASES * track.beats_per_phrase) as i64);
        let heard = self
            .documents
            .block_snapshots(track.score_context)
            .map(|blocks| heard_json(&blocks, playhead, window))
            .unwrap_or_else(|_| "[]".to_string());
        vars.insert("KJ_HEARD".to_string(), heard);
        // KJ_PULSE: this attachment's monotonic wakeup counter — the ordering key
        // *within a run* (`KJ_TICK` can repeat/freeze off-beat). It is NOT persisted,
        // so it resets to 0 on a kernel restart (like an uptime counter); that is
        // consistent with the context's conversation being re-hydrated fresh on
        // restart, so a model never carries a stale pulse across the boundary.
        // Cross-restart durability is deferred with the cold-start re-arm sweep.
        // KJ_EPOCH_NS: the beat's shared wall-clock instant, identical for every
        // context woken on this beat (the human "when" + the cross-context join key).
        vars.insert("KJ_PULSE".to_string(), ac.attachment.pulse.to_string());
        vars.insert("KJ_EPOCH_NS".to_string(), track.last_epoch_ns.to_string());
        // KJ_ROTATE_EVERY: the rotate cadence in phrases, when this context rotates.
        // The cadence now TRAVELS with the fork (the attachment row is fork-copied), so
        // an inheriting child re-binds with the same cadence by construction; this var
        // is informational. Absent when the context doesn't rotate.
        if let Some(c) = ac.attachment.rotate {
            vars.insert("KJ_ROTATE_EVERY".to_string(), c.every.to_string());
        }
        vars
    }

    /// Fire an rc lifecycle `verb` for `ctx`, fire-and-forget on the local set so
    /// the scheduler never blocks. The transport env is snapshotted synchronously
    /// (on this thread) and moved into the spawned task.
    fn fire_lifecycle(&self, ctx: ContextId, verb: &'static str) {
        let Some(dispatcher) = self.dispatcher.clone() else {
            return;
        };
        let vars = self.transport_env(ctx);
        tokio::task::spawn_local(async move {
            let caller = KjCaller {
                principal_id: PrincipalId::system(),
                context_id: Some(ctx),
                session_id: SessionId::new(),
                confirmed: false,
                rc_depth: 0,
                privileged: false,
            };
            if let Err(e) = dispatcher
                .run_rc_lifecycle_with_vars(verb, ctx, None, None, None, &vars, &caller)
                .await
            {
                log::warn!("beat: {verb} verb failed for context {ctx}: {e}");
            }
        });
    }

    /// Fire the `tick` rc verb — the OODA hook (`kj drive`). Its kaish only
    /// *requests* a turn (publishes `TurnFlow::Requested`), returning fast; the
    /// model turn runs on the turn-driver thread.
    fn fire_tick(&self, ctx: ContextId) {
        self.fire_lifecycle(ctx, "tick");
    }

    /// Fire the `rotate` rc verb — the page-turn (`docs/chameleon.md`). Called for a
    /// context the scheduler has ALREADY `detach`ed this beat (the synchronous detach
    /// in `process_track`), so the script's `kj fork --preset spawn` + re-attach child
    /// runs with the parent already off the track. The TRACK's clock keeps running
    /// across the handoff (the gap is covered by the speculation lead). Absent rc
    /// scripts → no-op.
    fn fire_rotate(&self, ctx: ContextId) {
        self.fire_lifecycle(ctx, "rotate");
    }

    /// The OODA **Act** handoff: a musician's turn just completed (it wrote ABC),
    /// so crystallize **that turn's output block** onto the timeline one phrase
    /// ahead. The output block id is carried on `TurnFlow::Completed` (F2 §7) —
    /// the old blind last-block read raced the model (it could read the seed
    /// prompt, published at spawn) and is gone. Only for an armed, OODA-armed
    /// context; a non-musician (un-armed) turn is ignored.
    ///
    /// `output_block_id`:
    ///   - `None` → the turn produced no text; nothing to crystallize.
    ///   - `Some(id)` → fetch exactly that block and run three defense-in-depth
    ///     guards before scheduling, because the loop can die three ways and each
    ///     would be a silent feedback loop (the bridge's own output looping back
    ///     through the Act):
    ///       1. ephemeral or excluded — system-managed / user-curated, never a
    ///          player Act. A materialized score block is ephemeral, so this is
    ///          the structural shield against re-crystallizing our own output.
    ///       2. track-bearing (`track.is_some()`) — came off the timeline.
    ///       3. beat()-authored — a legacy transport block.
    ///     A carried id that trips any guard is a BUG (the publish site should
    ///     only ever carry a real player Model block), so each is a loud
    ///     `log::error!`, refused, never silently skipped.
    fn on_turn_completed(&mut self, ctx: ContextId, output_block_id: Option<BlockId>) {
        // Resolve the context's track + attachment; only an OODA-armed musician we
        // manage gets its turn output crystallized. Schedule one phrase of lead ahead
        // of the playhead (design §10) so the fast write-barrier derive has room —
        // lead tracks the policy's phrase length (replaced the old fixed 4-beat
        // OODA_LEAD). The track id IS the lane the cell belongs to.
        let Some(track_id) = self.track_of(ctx) else {
            return;
        };
        let lead = {
            let Some(track) = self.tracks.get(&track_id) else {
                return;
            };
            let Some(ac) = track.attached.get(&ctx) else {
                return;
            };
            if !ac.attachment.ooda_armed {
                return; // not an OODA-armed musician we manage
            }
            track.phrasing().phrase_delta()
        };
        // No output block → the turn produced no text; nothing to crystallize.
        let Some(block_id) = output_block_id else {
            return;
        };
        let b = match self.documents.get_block_snapshot(ctx, &block_id) {
            Ok(Some(b)) => b,
            // A carried id we can't fetch is anomalous but not corruption (the
            // block may have been evicted); don't crash the driver.
            _ => return,
        };
        // Guard 1: ephemeral/excluded. A materialized score block rides the
        // ephemeral flag, so this refuses the bridge's own output structurally.
        if b.ephemeral || b.excluded {
            log::error!(
                "beat: on_turn_completed for {ctx} carried an ephemeral/excluded block \
                 {block_id} (ephemeral={}, excluded={}) — refusing to re-crystallize \
                 timeline output as a player Act",
                b.ephemeral,
                b.excluded
            );
            return;
        }
        // Guards 2+3: a track-bearing block came off the timeline; a beat()-
        // authored block is a legacy transport row. Neither is a player Act.
        if b.track.is_some() || b.id.principal_id == PrincipalId::beat() {
            log::error!(
                "beat: on_turn_completed for {ctx} carried a non-player block {block_id} \
                 (track={:?}, author={}) — refusing to loop bridge output through the Act",
                b.track,
                b.id.principal_id
            );
            return;
        }
        let abc = b.content;
        if abc.trim().is_empty() {
            return;
        }
        // `played_by` is the principal whose turn produced the ABC — the block's
        // own author (who PLAYED), which becomes the materialized cell's
        // principal. `track_id` is the musician's lane.
        let played_by = b.id.principal_id;
        // Record this context's producing principal so the track's shared failure
        // ledger routes a failure on its cell back to ITS conversation (not a
        // sibling producer's). Set on every Act; stable for a given producer.
        if let Some(ac) = self.tracks.get_mut(&track_id).and_then(|t| t.attached.get_mut(&ctx)) {
            ac.producer_principal = Some(played_by);
        }
        if let Err(e) = schedule_abc_cell(&self.kernel, ctx, &abc, lead, track_id, played_by) {
            // A refused/failed schedule must be visible to the player, not just
            // logged (design §7): surface a BlockKind::Error anchored at the
            // turn's own output block so the player reads its own rejection next
            // turn. The schedule-time ABC validator (§3) is the usual culprit —
            // malformed ABC that slipped past the model.
            log::warn!("beat: failed to schedule abc→midi for context {ctx}: {e}");
            let payload = kaijutsu_types::ErrorPayload {
                category: kaijutsu_types::ErrorCategory::Stream,
                severity: kaijutsu_types::ErrorSeverity::Error,
                code: None,
                detail: Some(format!("could not schedule your phrase onto the score: {e}")),
                span: None,
                source_kind: None,
            };
            let summary = payload.summary_line();
            if let Err(insert_err) = self.documents.insert_error_block_as(
                ctx,
                &block_id,
                &payload,
                summary,
                Some(played_by),
            ) {
                log::warn!("beat: failed to surface schedule-error block for {ctx}: {insert_err}");
            }
            // Quarantine the malformed phrase from future hydration windows so a
            // windowed player can't copy its own bad output next turn. The block
            // stays in the durable log (forward-only record); the anchored Error
            // block above — NOT excluded — still carries the rejection feedback,
            // so the player reads *that* it failed without re-reading *what* it
            // got wrong. Takes effect next turn (rehydrate_windowed re-reads the
            // excluded flag). Best-effort: a failed exclude just means the bad
            // phrase lingers in the window, not corruption.
            if let Err(excl_err) = self.documents.set_excluded(ctx, &block_id, true) {
                log::warn!(
                    "beat: failed to exclude malformed phrase {block_id} for {ctx}: {excl_err}"
                );
            }
        }
    }

    /// Apply one transport command and report the truthful outcome — `Ok(())`
    /// when it landed on an armed context, `Err(reason)` when it was a no-op so
    /// the `kj transport` verb can say so instead of blind-claiming success.
    /// `Arm` creates the entry (always `Ok`); `Disarm` is idempotent (`Ok`); the
    /// rest require the context to be armed.
    ///
    /// MUST STAY FULLY SYNCHRONOUS — no `.await`. This runs inside `run`'s
    /// `select!` ingress arm, and a rc-lifecycle script (`fire_rotate` /
    /// `fire_lifecycle`, both `spawn_local` onto *this same LocalSet*) can issue a
    /// `kj transport` command and then await the `BeatAck` reply. That awaiting rc
    /// task only makes progress when this loop yields back to poll it; if
    /// `apply_command` ever awaits, the rc-fired path can self-deadlock (the
    /// scheduler parked awaiting something the rc task can't deliver because the
    /// rc task is parked awaiting this reply). Keep the work here non-blocking and
    /// reply before yielding.
    fn apply_command(&mut self, command: BeatCommand) -> BeatAck {
        match command {
            BeatCommand::Attach { track, context_id, attachment, policy } => {
                // A DB-read failure in `attach` bubbles to the BeatAck so `kj
                // transport` (and the rc `&&` chain) sees a loud failure, never a
                // silent poisoned (rewound) timeline reported as success. The
                // attachment carries wakeup/rotate/ooda, so there is no separate
                // cadence-restore step (attach persists the binding itself).
                self.attach(track, context_id, attachment, policy)
            }
            BeatCommand::Detach { track, context_id } => {
                self.detach(&track, context_id);
                Ok(())
            }
            BeatCommand::Play(track) => {
                self.play(&track, Instant::now());
                self.track_ack(&track)
            }
            BeatCommand::Pause(track) => {
                self.pause(&track);
                self.track_ack(&track)
            }
            BeatCommand::Stop(track) => {
                self.stop(&track);
                self.track_ack(&track)
            }
            BeatCommand::SetTempo { track, period } => {
                self.set_tempo(&track, period);
                self.track_ack(&track)
            }
            BeatCommand::SetOoda { track, context_id, armed } => {
                self.set_ooda(&track, context_id, armed);
                self.attached_ack(&track, context_id)
            }
            BeatCommand::SetRotate { track, context_id, every } => {
                self.set_rotate(&track, context_id, every);
                self.attached_ack(&track, context_id)
            }
            BeatCommand::SetClock { track, kind } => {
                self.set_clock(&track, kind);
                self.track_ack(&track)
            }
        }
    }

    /// Snapshot every track's live state for the wire (`listTracks`). Reads the
    /// in-memory `TrackState` map — the truth the persisted row lags behind
    /// (playhead is only persisted on transport transitions). Synchronous and
    /// allocation-proportional to track count (~handful), so it is safe inside
    /// the ingress arm's no-await contract.
    fn snapshot_tracks(&self) -> Vec<TrackSnapshot> {
        let mut out: Vec<_> = self
            .tracks
            .iter()
            .map(|(id, t)| TrackSnapshot {
                id: id.clone(),
                score_context: t.score_context,
                playing: t.playing,
                playhead: t.playhead.get(),
                period: t.clock.period(),
                beats_per_phrase: t.beats_per_phrase,
                beat_count: t.beat_count,
                last_epoch_ns: t.last_epoch_ns,
                clock_kind: t.clock.kind_str().to_string(),
                attached: t.attached.keys().copied().collect(),
            })
            .collect();
        // Stable order for the wire (HashMap iteration order is arbitrary).
        out.sort_by(|a, b| a.id.cmp(&b.id));
        out
    }

    /// `Ok` if the track has a live clock domain (so a clock-domain command landed),
    /// else a loud `Err` — none of the apply methods drop a track, so checking after
    /// the call is exact.
    fn track_ack(&self, track_id: &TrackId) -> BeatAck {
        if self.tracks.contains_key(track_id) {
            Ok(())
        } else {
            Err(format!(
                "track '{}' has no clock — `kj transport attach` a context to it first",
                track_id.as_str()
            ))
        }
    }

    /// `Ok` if the context is attached to the track (so a per-attachment command
    /// landed), else a loud `Err`.
    fn attached_ack(&self, track_id: &TrackId, ctx: ContextId) -> BeatAck {
        let attached = self
            .tracks
            .get(track_id)
            .is_some_and(|t| t.attached.contains_key(&ctx));
        if attached {
            Ok(())
        } else {
            Err(format!(
                "context {} is not attached to track '{}' — run `kj transport attach` first",
                ctx.short(),
                track_id.as_str()
            ))
        }
    }

    /// Run the scheduler/transport loop until the ingress sender is dropped. One
    /// `select!` over the heap's nearest deadline, the command ingress, and the
    /// turn-completion bus (the OODA Act handoff).
    pub async fn run(mut self, mut ingress: mpsc::UnboundedReceiver<BeatRequest>) {
        log::info!("Beat scheduler online");
        let mut completed = self.kernel.turn_flows().subscribe("turn.completed");
        let mut turn_bus_open = true;
        loop {
            let next = self.next_wake();
            tokio::select! {
                biased;
                msg = ingress.recv() => match msg {
                    Some(BeatRequest::Command { command, reply }) => {
                        // `apply_command` is synchronous on purpose: a rc-fired
                        // `kj transport` (spawn_local on this LocalSet) awaits the
                        // reply below, so this arm must compute + send without
                        // yielding or it self-deadlocks. See `apply_command` doc.
                        let ack = self.apply_command(command);
                        // Report the real outcome to a caller that wants it (`kj
                        // transport`), so its message can't lie about an un-armed
                        // no-op. A dropped receiver (fire-and-forget) is fine.
                        if let Some(reply) = reply {
                            let _ = reply.send(ack);
                        }
                    }
                    // Read-only track snapshot (the `listTracks` wire surface):
                    // same synchronous compute-and-reply contract as commands.
                    Some(BeatRequest::Snapshot { reply }) => {
                        let _ = reply.send(self.snapshot_tracks());
                    }
                    // Captured-MIDI batch commit (`docs/midi.md` M2): same
                    // synchronous compute-and-reply contract — parse, quantize
                    // against the track's in-memory anchor, insert, no awaits.
                    Some(BeatRequest::CommitCapture { context_id, payload, played_by, reply }) => {
                        let _ = reply.send(self.commit_capture(context_id, payload, played_by));
                    }
                    // One clock reference from an edge observer (M3):
                    // fire-and-forget, applied to the sender's track when its
                    // clock is modeled.
                    Some(BeatRequest::ClockEstimate { context_id, beat, tempo_bps, epoch_ns, source }) => {
                        self.apply_clock_estimate(context_id, beat, tempo_bps, epoch_ns, &source);
                    }
                    None => break, // all senders dropped → shut down
                },
                msg = completed.recv(), if turn_bus_open => match msg {
                    Some(m) => {
                        if let TurnFlow::Completed { context_id, output_block_id, .. } = m.payload {
                            self.on_turn_completed(context_id, output_block_id);
                        }
                    }
                    None => turn_bus_open = false, // bus closed → stop polling this arm
                },
                _ = sleep_until_opt(next) => {
                    let outcome = self.fire_due(Instant::now());
                    for ctx in outcome.ooda_due {
                        self.fire_tick(ctx);
                    }
                    // Rotate-due contexts were already `stop`ped synchronously in
                    // fire_due; fire their page-turn lifecycle (fork + arm child).
                    for ctx in outcome.rotate_due {
                        self.fire_rotate(ctx);
                    }
                }
            }
        }
        log::warn!("Beat scheduler: ingress closed, scheduler exiting");
    }
}

/// Sleep until `deadline`, or park forever if nothing is scheduled (so `select!`
/// only resolves on the ingress arm — zero CPU while idle).
async fn sleep_until_opt(deadline: Option<Instant>) {
    match deadline {
        Some(t) => tokio::time::sleep_until(t).await,
        None => std::future::pending::<()>().await,
    }
}

/// Spawn the server-lifetime beat scheduler on its own thread (turn-driver
/// pattern: dedicated current-thread runtime + LocalSet, since firing the `tick`
/// verb uses `spawn_local`). Installs the ingress sender on the kernel so the rc
/// lifecycle and `kj transport` can arm/drive musician contexts.
pub fn spawn_beat_scheduler(registry: Arc<ServerRegistry>) {
    let (tx, rx) = mpsc::unbounded_channel::<BeatRequest>();
    registry.kernel.kernel.set_beat_ingress(tx);

    let kernel = registry.kernel.kernel.clone();
    let documents = registry.kernel.documents.clone();
    let dispatcher = registry.kernel.kj_dispatcher.clone();
    // A `rotate` page-turn runs a deeply self-re-entrant rc chain on THIS
    // thread (fire_rotate → run_rc_lifecycle → `kj fork` → the child's fork +
    // attach rc → `kj transport attach`/`play`, each `kj` re-entering kaish via
    // `.await`, so the whole nest accumulates on one stack). The default 2 MiB
    // thread stack is too small for that depth with kaish's interpreter — it
    // SIGABRTs the scheduler mid-rotate. Reserve a generous stack; it's virtual
    // address space, committed page-by-page only as used. See docs/issues.md.
    let builder = std::thread::Builder::new()
        .name("beat-scheduler".to_string())
        .stack_size(kaijutsu_kernel::KAISH_RC_THREAD_STACK);
    if let Err(e) = builder.spawn(move || {
        let rt = match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(rt) => rt,
            Err(e) => {
                log::error!("beat-scheduler: failed to build runtime: {e}");
                return;
            }
        };
        let local = tokio::task::LocalSet::new();
        local.block_on(&rt, async move {
            BeatScheduler::new(kernel, documents)
                .with_dispatcher(dispatcher)
                .run(rx)
                .await;
        });
    }) {
        log::error!("Failed to spawn beat-scheduler thread: {e}");
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use kaijutsu_hyoushigi::{
        Cell, ContextHash, ContextQuery, Fallback, Recipe, ResolveError, Resolution, Resolver,
        ResolverCtx, ResolverId, Span, Tick, TickClock, TickDelta,
    };
    use kaijutsu_kernel::Kernel;
    use kaijutsu_kernel::block_store::{BlockStore, DocumentKind, SharedBlockStore};
    use kaijutsu_kernel::flows::{BlockFlow, FlowBus, SharedBlockFlowBus};
    use kaijutsu_kernel::kernel_db::{PersistedAttachment, PersistedTrack};
    use kaijutsu_kernel::hyoushigi::{Attachment, BeatPolicy, Cadence, ClockKind};
    use kaijutsu_types::{ContextId, PrincipalId, TrackId};
    use tokio::time::Instant;

    use super::BeatScheduler;
    use super::{
        MATERIALIZE_RETRY_BUDGET, PoisonAction, heard_json, poison_action, render_instant,
        transport_vars,
    };
    use kaijutsu_types::{
        BlockId, BlockKind, BlockSnapshot, BlockSnapshotBuilder, ContentType, Role as BlockRole,
    };

    /// A committed score block shaped as the materializer emits the ABC source:
    /// `Role::Model` + `ContentType::Abc` + a tick + a track.
    fn abc_block(seq: u64, tick: i64, track: &str, abc: &str) -> BlockSnapshot {
        BlockSnapshotBuilder::new(
            BlockId::new(ContextId::new(), PrincipalId::new(), seq),
            BlockKind::Text,
        )
        .role(BlockRole::Model)
        .content(abc)
        .content_type(ContentType::Abc)
        .tick(Tick::new(tick))
        .track(TrackId::new(track).unwrap())
        .build()
    }

    /// `$HEARD` collects committed notation in the window, oldest→newest, with
    /// track labels — the recent line the player composes against.
    #[test]
    fn heard_json_collects_recent_notation_in_order() {
        // Built out of order to prove the sort; window 32 ticks (2 phrases).
        let blocks = vec![
            abc_block(2, 16, "bass", "B2"),
            abc_block(1, 0, "bass", "A2"),
            abc_block(3, 32, "bass", "C2"),
        ];
        let json = heard_json(&blocks, Tick::new(32), TickDelta::new(32));
        let v: serde_json::Value = serde_json::from_str(&json).expect("valid JSON");
        let arr = v.as_array().expect("array");
        assert_eq!(arr.len(), 3, "all three in [0,32] window");
        assert_eq!(arr[0]["tick"], 0, "oldest first");
        assert_eq!(arr[0]["abc"], "A2");
        assert_eq!(arr[0]["track"], "bass");
        assert_eq!(arr[2]["tick"], 32, "newest last");
        assert_eq!(arr[2]["abc"], "C2");
    }

    /// The window drops notation older than `now - window`, and MIDI siblings
    /// (non-Abc) never enter — `$HEARD` is notation-pure.
    #[test]
    fn heard_json_excludes_old_and_non_notation() {
        let midi_sibling = BlockSnapshotBuilder::new(
            BlockId::new(ContextId::new(), PrincipalId::new(), 99),
            BlockKind::Text,
        )
        .role(BlockRole::Asset)
        .content("deadbeefdeadbeefdeadbeefdeadbeef")
        .content_type(ContentType::Plain)
        .tick(Tick::new(48))
        .track(TrackId::new("bass").unwrap())
        .build();
        let blocks = vec![
            abc_block(1, 0, "bass", "OLD"),  // tick 0 < since(32) → dropped
            abc_block(2, 16, "bass", "OLD"), // tick 16 < since(32) → dropped
            abc_block(3, 48, "bass", "KEEP"),
            midi_sibling, // non-Abc → never included
        ];
        let json = heard_json(&blocks, Tick::new(64), TickDelta::new(32));
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        let arr = v.as_array().unwrap();
        assert_eq!(arr.len(), 1, "only the in-window notation block survives");
        assert_eq!(arr[0]["abc"], "KEEP");
    }

    /// No notation yet → an empty JSON array (a fresh player, not an error).
    #[test]
    fn heard_json_empty_is_array() {
        assert_eq!(heard_json(&[], Tick::new(0), TickDelta::new(32)), "[]");
    }

    fn vars_map(pairs: [(String, String); 3]) -> std::collections::HashMap<String, String> {
        pairs.into_iter().collect()
    }

    /// The transport heartbeat reports the present-tense now-facts: playhead
    /// tick, phrases elapsed, and tempo in whole BPM. These are what a player
    /// composes its next phrase against; `$HEARD` is deliberately absent (a pull,
    /// not a pushed var — see `transport_vars` docs). Keys are `KJ_`-prefixed
    /// (the kernel-injection convention from `docs/tracks.md`).
    #[test]
    fn transport_vars_report_now_facts() {
        // musician_default: 500 ms/beat (= 120 BPM), 32 beats/phrase (8 bars).
        let m = vars_map(transport_vars(Tick::new(128), 128, &BeatPolicy::musician_default()));
        assert_eq!(m["KJ_TICK"], "128", "playhead tick verbatim");
        assert_eq!(m["KJ_PHRASE"], "4", "128 beats / 32 per phrase = phrase 4");
        assert_eq!(m["KJ_TEMPO"], "120", "500 ms/beat rounds to 120 BPM");
    }

    /// A faster tempo and a mid-phrase playhead: tick is exact, phrase floors,
    /// BPM rounds. Guards the formatting against off-by-one phrase math.
    #[test]
    fn transport_vars_floor_phrase_and_round_bpm() {
        // `ooda_every` is gone — it moved onto `Attachment::wakeup`. `BeatPolicy`
        // is now purely track-level musical knobs (period + beats_per_phrase).
        let policy = BeatPolicy {
            period: Duration::from_millis(300), // 200 BPM
            beats_per_phrase: 16,
        };
        let m = vars_map(transport_vars(Tick::new(40), 40, &policy));
        assert_eq!(m["KJ_TICK"], "40");
        assert_eq!(m["KJ_PHRASE"], "2", "40 / 16 floors to 2 (mid third phrase)");
        assert_eq!(m["KJ_TEMPO"], "200", "300 ms/beat = 200 BPM");
    }

    /// Defensive: a zero `beats_per_phrase` (no phrasing) reports phrase 0 rather
    /// than dividing by zero — same guard as `is_phrase_boundary`.
    #[test]
    fn transport_vars_zero_phrase_guard() {
        let policy = BeatPolicy {
            period: Duration::from_millis(500),
            beats_per_phrase: 0,
        };
        let m = vars_map(transport_vars(Tick::new(5), 5, &policy));
        assert_eq!(m["KJ_PHRASE"], "0", "no phrasing → phrase 0, never a divide-by-zero");
    }

    /// The poison-cell retry budget: the bridge retries the SAME failing cell up to
    /// the budget, then skips it (loudly) so a swallowed materialize error can never
    /// become a silent infinite retry loop (design §7 / the F1 blocker fix). Pins
    /// the boundary exactly.
    #[test]
    fn poison_action_retries_within_budget_then_skips() {
        // The first `BUDGET - 1` consecutive failures stay in Retry (leave the
        // poison cell in place, try again next beat).
        for failures in 1..MATERIALIZE_RETRY_BUDGET {
            assert_eq!(
                poison_action(failures),
                PoisonAction::Retry,
                "failure #{failures} (< budget) must retry, not skip"
            );
        }
        // At the budget — and beyond — the bridge gives up and skips the poison
        // cell so the loop terminates.
        assert_eq!(
            poison_action(MATERIALIZE_RETRY_BUDGET),
            PoisonAction::SkipPoison,
            "the budget-th consecutive failure must skip the poison cell"
        );
        assert_eq!(
            poison_action(MATERIALIZE_RETRY_BUDGET + 5),
            PoisonAction::SkipPoison,
            "past the budget stays SkipPoison"
        );
    }

    /// A resolver that always errs — drives a cell into the engine failure ledger
    /// (§6) so the scheduler-side drain (§8 / T21) can be exercised. Cost ZERO so
    /// it resolves the instant the playhead reaches the cell's tick (like Marker),
    /// keeping the beat-test window small.
    struct AlwaysFails;
    impl Resolver for AlwaysFails {
        fn id(&self) -> ResolverId {
            ResolverId::new("always_fails")
        }
        fn estimate_cost(&self, _p: &serde_json::Value, _c: &dyn ResolverCtx) -> Duration {
            Duration::ZERO
        }
        fn compute_basis(&self, _p: &serde_json::Value, _c: &dyn ResolverCtx) -> ContextHash {
            ContextHash::of(b"stable")
        }
        fn resolve(
            &self,
            _p: &serde_json::Value,
            _c: &dyn ResolverCtx,
        ) -> Result<Resolution, ResolveError> {
            Err(ResolveError::Failed("CAS read failed: missing entry".to_string()))
        }
    }

    /// A beat marker: at tick T it emits `beat-T`. Distinct content per beat so
    /// ordering is observable. Stable basis → clean commit, no squashes.
    struct Marker;
    impl Resolver for Marker {
        fn id(&self) -> ResolverId {
            ResolverId::new("marker")
        }
        fn estimate_cost(&self, _p: &serde_json::Value, _c: &dyn ResolverCtx) -> Duration {
            Duration::ZERO
        }
        fn compute_basis(&self, _p: &serde_json::Value, _c: &dyn ResolverCtx) -> ContextHash {
            ContextHash::of(b"stable")
        }
        fn resolve(
            &self,
            _p: &serde_json::Value,
            c: &dyn ResolverCtx,
        ) -> Result<Resolution, ResolveError> {
            Ok(Resolution::new(format!("beat-{}", c.now().get()), "text/plain"))
        }
    }

    fn marker_cell(tick: i64) -> Cell {
        Cell::deferred_on(
            Span::instant(Tick::new(tick)),
            Recipe {
                resolver: ResolverId::new("marker"),
                params: serde_json::Value::Null,
                query: ContextQuery::default(),
                fallback: Fallback::Skip,
            },
            TrackId::solo(),
            PrincipalId::beat(),
        )
    }

    /// A resolver that commits a fixed, valid ABC tune (mime `text/vnd.abc`) — the
    /// render-cue tests need a real ABC cell to cross the barrier.
    struct ValidAbc;
    impl Resolver for ValidAbc {
        fn id(&self) -> ResolverId {
            ResolverId::new("valid_abc")
        }
        fn estimate_cost(&self, _p: &serde_json::Value, _c: &dyn ResolverCtx) -> Duration {
            Duration::ZERO
        }
        fn compute_basis(&self, _p: &serde_json::Value, _c: &dyn ResolverCtx) -> ContextHash {
            ContextHash::of(b"stable")
        }
        fn resolve(
            &self,
            _p: &serde_json::Value,
            _c: &dyn ResolverCtx,
        ) -> Result<Resolution, ResolveError> {
            Ok(Resolution::new(b"X:1\nK:C\nCDEF|\n".to_vec(), "text/vnd.abc"))
        }
    }

    async fn fresh_kernel_and_docs() -> (Arc<Kernel>, SharedBlockStore) {
        let kernel = Arc::new(Kernel::new_ephemeral("test").await);
        let bus: SharedBlockFlowBus = Arc::new(FlowBus::new(256));
        let documents: SharedBlockStore =
            Arc::new(BlockStore::with_flows(PrincipalId::new(), bus));
        (kernel, documents)
    }

    /// Pre-arm a context's timeline with marker cells at ticks `1..=count`. Clock
    /// margin 0 so committing a cell coincides with the playhead reaching its
    /// tick. Pre-arming wins (the scheduler's arm() is idempotent).
    fn preseed_markers(kernel: &Kernel, ctx: ContextId, count: i64) {
        let tl = kernel.arm_timeline(
            ctx,
            TickClock { ticks_per_sec: 1.0, safety_factor: 1.0, commit_margin: TickDelta::new(0) },
            Tick::ZERO,
        );
        let mut g = tl.lock();
        g.register_resolver(Box::new(Marker));
        for t in 1..=count {
            g.schedule(marker_cell(t)).unwrap();
        }
    }

    /// Stage-2 marker setup: arm `track`'s timeline with the marker clock
    /// (commit_margin 0 so a cell commits exactly when the playhead reaches its tick)
    /// and seed `count` marker cells. `arm_track_timeline` is idempotent, so a
    /// following `attach` (which would arm with `policy.clock()`, commit_margin 1)
    /// keeps THIS clock. Call after the `track` id is known and BEFORE `attach`. The
    /// score lives on the track now, so materialized markers land in the score context
    /// (`sched.score_context(track)`), not the producing context.
    fn arm_track_with_markers(kernel: &Kernel, track: &TrackId, count: i64) {
        let tl = kernel.arm_track_timeline(
            track.clone(),
            TickClock { ticks_per_sec: 1.0, safety_factor: 1.0, commit_margin: TickDelta::new(0) },
            Tick::ZERO,
        );
        let mut g = tl.lock();
        g.register_resolver(Box::new(Marker));
        for t in 1..=count {
            g.schedule(marker_cell(t)).unwrap();
        }
    }

    /// A 1-second beat policy with a large phrase length (phrase boundaries never
    /// fire in the span of these tests). `ooda_every` is now on the `Attachment`
    /// — use `slow_attachment()` alongside this policy.
    fn slow_policy() -> BeatPolicy {
        BeatPolicy {
            period: Duration::from_secs(1),
            beats_per_phrase: 1_000_000,
        }
    }

    /// A matching attachment for `slow_policy()`: wakeup cadence so large that
    /// OODA never fires in the span of these tests (subsumes the old `ooda_every:
    /// 1_000_000`). OODA is armed so tests that check it still work; a test that
    /// explicitly wants OODA off calls `set_ooda` separately.
    fn slow_attachment() -> Attachment {
        Attachment {
            wakeup: Cadence::new(1_000_000),
            rotate: None,
            ooda_armed: true,
            pulse: 0,
        }
    }

    fn contents(documents: &SharedBlockStore, ctx: ContextId) -> Vec<String> {
        documents
            .block_snapshots(ctx)
            .unwrap()
            .into_iter()
            .map(|b| b.content)
            .collect()
    }

    /// `snapshot_tracks` (the `listTracks` wire answer) reports the scheduler's
    /// in-memory truth: track identity, score context, transport state, live
    /// playhead, tempo, and the attachment roster — sorted by id for a stable
    /// wire order.
    #[tokio::test]
    async fn snapshot_tracks_reports_live_in_memory_state() {
        let (kernel, documents) = fresh_kernel_and_docs().await;
        let ctx = ContextId::new();
        documents.create_document(ctx, DocumentKind::Conversation, None).unwrap();

        let mut sched = BeatScheduler::new(kernel.clone(), documents.clone());
        assert!(sched.snapshot_tracks().is_empty(), "no tracks yet");

        let base = Instant::now();
        // Two tracks, attached out of name order to prove the sort.
        let zither = TrackId::new("zither").unwrap();
        let bass = TrackId::new("bass").unwrap();
        arm_track_with_markers(&kernel, &zither, 3);
        arm_track_with_markers(&kernel, &bass, 3);
        sched.attach(zither.clone(), ctx, slow_attachment(), slow_policy()).unwrap();
        let ctx2 = ContextId::new();
        documents.create_document(ctx2, DocumentKind::Conversation, None).unwrap();
        sched.attach(bass.clone(), ctx2, slow_attachment(), slow_policy()).unwrap();

        let snap = sched.snapshot_tracks();
        assert_eq!(snap.len(), 2);
        assert_eq!(snap[0].id, bass, "sorted by id for a stable wire order");
        assert_eq!(snap[1].id, zither);

        let b = &snap[0];
        assert_eq!(b.score_context, sched.score_context(&bass));
        assert!(!b.playing, "a track is created stopped");
        assert_eq!(b.playhead, 0, "no beats yet");
        assert_eq!(b.period, Duration::from_secs(1), "slow_policy period");
        assert_eq!(b.clock_kind, "system");
        assert_eq!(b.attached, vec![ctx2], "the roster");
        assert_eq!(b.last_epoch_ns, 0, "never fired");

        // Play + beat: the snapshot follows the LIVE playhead (the persisted
        // row would still say 0 here — that lag is why the wire reads memory).
        sched.play(&bass, base);
        sched.fire_due(base + Duration::from_secs(1));
        let snap = sched.snapshot_tracks();
        let b = &snap[0];
        assert!(b.playing);
        assert!(b.playhead > 0, "playhead advanced in memory: {}", b.playhead);
        assert_eq!(b.beat_count, 1);
        assert!(b.last_epoch_ns > 0, "epoch latched on the beat");
    }

    fn capture_batch(window_start_ns: u64, window_end_ns: u64, epochs: &[u64]) -> Vec<u8> {
        let batch = kaijutsu_audio::CaptureBatch {
            v: kaijutsu_audio::MIDI_CAPTURE_VERSION,
            window_start_ns,
            window_end_ns,
            events: epochs
                .iter()
                .map(|&e| kaijutsu_audio::CaptureEvent {
                    epoch_ns: e,
                    source: "24:0".into(),
                    bytes: vec![0x90, 60, 100],
                })
                .collect(),
            lost: 0,
            ext: serde_json::Map::new(),
        };
        batch.to_json_bytes().unwrap()
    }

    /// `commit_capture` (docs/midi.md M2): a batch from an attached capture
    /// context lands ONE data-only block in the track's score context —
    /// frozen-playhead tick while stopped, ephemeral, capture mime, annotated
    /// with the quantization anchor — and stays invisible to the notation
    /// consumers (`heard_json`, `reconstruct_score_cells`) by design.
    #[tokio::test]
    async fn commit_capture_lands_annotated_block_in_score_context() {
        let (kernel, documents) = fresh_kernel_and_docs().await;
        let ctx = ContextId::new();
        documents.create_document(ctx, DocumentKind::Conversation, None).unwrap();
        let mut sched = BeatScheduler::new(kernel.clone(), documents.clone());
        let track_id = TrackId::new("ear").unwrap();
        arm_track_with_markers(&kernel, &track_id, 3);
        sched.attach(track_id.clone(), ctx, slow_attachment(), slow_policy()).unwrap();
        let score = sched.score_context(&track_id);
        let played_by = PrincipalId::new();

        let payload = capture_batch(1_000, 2_000, &[1_100, 1_500]);
        let id = sched.commit_capture(ctx, payload, played_by).unwrap();

        let blocks = documents.block_snapshots(score).unwrap();
        assert_eq!(blocks.len(), 1, "one block per batch");
        let b = &blocks[0];
        assert_eq!(b.id, id, "reply carries the landed block id");
        assert_eq!(b.id.principal_id, played_by, "played_by = the shipping caller");
        assert_eq!(b.tick, Some(Tick::new(0)), "stopped track → frozen playhead");
        assert_eq!(b.track.as_ref(), Some(&track_id));
        assert!(b.ephemeral, "a score record, not a conversation turn");
        assert_ne!(b.content_type, ContentType::Abc, "not notation");

        // The stored record parses back and carries its quantization anchor.
        let back = kaijutsu_audio::CaptureBatch::parse(b.content.as_bytes()).unwrap();
        assert_eq!(back.events.len(), 2, "events survive the annotate round-trip");
        let q = &back.ext["quantize"];
        assert_eq!(q["rolling"], serde_json::json!(false), "no rolling grid yet");
        assert_eq!(q["anchor_tick"], serde_json::json!(0));
        assert_eq!(q["ticks_per_beat"], serde_json::json!(1));

        // Invisible to the notation consumers — score first, perception later.
        let heard = heard_json(&blocks, Tick::new(10), TickDelta::new(100));
        assert_eq!(heard, "[]", "KJ_HEARD stays notation-pure: {heard}");
        assert!(
            sched.reconstruct_score_cells(score, &track_id).is_empty(),
            "timeline reconstruction ignores capture blocks"
        );
    }

    /// While the clock rolls, the batch's window quantizes against the live
    /// anchor: the last beat's (playhead, wallclock) pair + the period.
    #[tokio::test]
    async fn commit_capture_quantizes_against_the_rolling_anchor() {
        let (kernel, documents) = fresh_kernel_and_docs().await;
        let ctx = ContextId::new();
        documents.create_document(ctx, DocumentKind::Conversation, None).unwrap();
        let mut sched = BeatScheduler::new(kernel.clone(), documents.clone());
        let track_id = TrackId::new("ear").unwrap();
        arm_track_with_markers(&kernel, &track_id, 3);
        sched.attach(track_id.clone(), ctx, slow_attachment(), slow_policy()).unwrap();
        let score = sched.score_context(&track_id);

        // One real beat: playhead 0→1, last_epoch_ns latched from wallclock.
        let base = Instant::now();
        sched.play(&track_id, base);
        sched.fire_due(base + Duration::from_secs(1));
        let snap = sched.snapshot_tracks();
        let anchor_ns = snap[0].last_epoch_ns;
        assert!(anchor_ns > 0, "the beat latched an epoch anchor");
        assert_eq!(snap[0].playhead, 1);

        // Window opens 2.4 beats after the anchor (period = 1s): rounds to +2.
        let start = anchor_ns + 2_400_000_000;
        let payload = capture_batch(start, start + 1_000_000_000, &[start + 1_000]);
        let played_by = PrincipalId::new();
        let id = sched.commit_capture(ctx, payload, played_by).unwrap();

        // Find by the returned id — the beat also materialized marker cells
        // into this score context, so content-type sniffing is ambiguous.
        let blocks = documents.block_snapshots(score).unwrap();
        let b = blocks.iter().find(|b| b.id == id).expect("the capture block");
        assert_eq!(b.tick, Some(Tick::new(3)), "playhead 1 + round(2.4) beats");
        let back = kaijutsu_audio::CaptureBatch::parse(b.content.as_bytes()).unwrap();
        let q = &back.ext["quantize"];
        assert_eq!(q["rolling"], serde_json::json!(true));
        assert_eq!(q["anchor_tick"], serde_json::json!(1));
        assert_eq!(q["anchor_epoch_ns"], serde_json::json!(anchor_ns));
    }

    /// Refusals are loud and land nothing: an unattached context, a malformed
    /// batch, and an unknown record version each return `Err` with the reason.
    #[tokio::test]
    async fn commit_capture_refuses_unattached_and_malformed() {
        let (kernel, documents) = fresh_kernel_and_docs().await;
        let ctx = ContextId::new();
        documents.create_document(ctx, DocumentKind::Conversation, None).unwrap();
        let mut sched = BeatScheduler::new(kernel.clone(), documents.clone());
        let played_by = PrincipalId::new();

        // Not attached to any track.
        let err = sched
            .commit_capture(ctx, capture_batch(0, 10, &[5]), played_by)
            .unwrap_err();
        assert!(err.contains("not attached"), "names the refusal: {err}");

        // Attached, but the payload is garbage.
        let track_id = TrackId::new("ear").unwrap();
        arm_track_with_markers(&kernel, &track_id, 3);
        sched.attach(track_id.clone(), ctx, slow_attachment(), slow_policy()).unwrap();
        let err = sched
            .commit_capture(ctx, b"not json".to_vec(), played_by)
            .unwrap_err();
        assert!(err.contains("rejected"), "malformed batch is refused: {err}");

        // Nothing landed in the score context either way.
        let score = sched.score_context(&track_id);
        assert!(documents.block_snapshots(score).unwrap().is_empty());
    }

    /// Multi-track attach is impossible by construction — `attach` enforces
    /// the 1:1 `context_track` reverse index by MOVING a re-attached context
    /// to the new track. Capture follows the move: the batch lands on the
    /// NEW track's score context, and the old track receives nothing. (The
    /// multi-claim refusal inside `commit_capture` stays as a defensive
    /// guard on that invariant; this pins the invariant itself.)
    #[tokio::test]
    async fn commit_capture_follows_a_reattach_move() {
        let (kernel, documents) = fresh_kernel_and_docs().await;
        let ctx = ContextId::new();
        documents.create_document(ctx, DocumentKind::Conversation, None).unwrap();
        let mut sched = BeatScheduler::new(kernel.clone(), documents.clone());
        let a = TrackId::new("ear-a").unwrap();
        let b = TrackId::new("ear-b").unwrap();
        arm_track_with_markers(&kernel, &a, 3);
        arm_track_with_markers(&kernel, &b, 3);
        sched.attach(a.clone(), ctx, slow_attachment(), slow_policy()).unwrap();
        sched.attach(b.clone(), ctx, slow_attachment(), slow_policy()).unwrap();

        let id = sched
            .commit_capture(ctx, capture_batch(0, 10, &[5]), PrincipalId::new())
            .expect("capture commits against the moved-to track");
        let on_b = documents.block_snapshots(sched.score_context(&b)).unwrap();
        assert!(on_b.iter().any(|blk| blk.id == id), "lands on the NEW track's score");
        assert!(
            documents.block_snapshots(sched.score_context(&a)).unwrap().is_empty(),
            "the old track's score receives nothing"
        );
    }

    fn epoch_now_ns() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos() as u64
    }

    /// `kj transport clock`: switching drivers preserves the period, reports
    /// through the snapshot, is idempotent, and estimates only bite when the
    /// track is modeled.
    #[tokio::test]
    async fn set_clock_switches_driver_and_estimates_slave_it() {
        let (kernel, documents) = fresh_kernel_and_docs().await;
        let ctx = ContextId::new();
        documents.create_document(ctx, DocumentKind::Conversation, None).unwrap();
        let mut sched = BeatScheduler::new(kernel.clone(), documents.clone());
        let track_id = TrackId::new("slaved").unwrap();
        arm_track_with_markers(&kernel, &track_id, 3);
        sched.attach(track_id.clone(), ctx, slow_attachment(), slow_policy()).unwrap();

        // System clock: an estimate is ignored (ambient sense, not slaved).
        sched.apply_clock_estimate(ctx, 8.0, 2.4, epoch_now_ns(), "24:0");
        let snap = sched.snapshot_tracks();
        assert_eq!(snap[0].clock_kind, "system");
        assert_eq!(snap[0].period, Duration::from_secs(1), "unchanged");

        // Switch to modeled: period carries over, kind flips + persists.
        sched.set_clock(&track_id, ClockKind::Modeled);
        let snap = sched.snapshot_tracks();
        assert_eq!(snap[0].clock_kind, "modeled");
        assert_eq!(snap[0].period, Duration::from_secs(1), "period carried over");

        // Now an estimate bites — slew-limited (max 5% per reference).
        sched.apply_clock_estimate(ctx, 8.0, 2.4, epoch_now_ns(), "24:0");
        let snap = sched.snapshot_tracks();
        assert_eq!(snap[0].period, Duration::from_millis(950), "1000 ms − 5%");

        // Idempotent same-kind set keeps the slewed state.
        sched.set_clock(&track_id, ClockKind::Modeled);
        assert_eq!(sched.snapshot_tracks()[0].period, Duration::from_millis(950));

        // Back to system: the last modeled tempo freezes in.
        sched.set_clock(&track_id, ClockKind::System);
        let snap = sched.snapshot_tracks();
        assert_eq!(snap[0].clock_kind, "system");
        assert_eq!(snap[0].period, Duration::from_millis(950));
    }

    /// Estimate hygiene: unattached senders and stale stamps are dropped
    /// without panic or effect.
    #[tokio::test]
    async fn clock_estimates_from_strangers_and_the_past_are_dropped() {
        let (kernel, documents) = fresh_kernel_and_docs().await;
        let ctx = ContextId::new();
        documents.create_document(ctx, DocumentKind::Conversation, None).unwrap();
        let mut sched = BeatScheduler::new(kernel.clone(), documents.clone());

        // Unattached sender: nothing to resolve, no panic.
        sched.apply_clock_estimate(ContextId::new(), 1.0, 2.0, epoch_now_ns(), "24:0");

        let track_id = TrackId::new("slaved").unwrap();
        arm_track_with_markers(&kernel, &track_id, 3);
        sched.attach(track_id.clone(), ctx, slow_attachment(), slow_policy()).unwrap();
        sched.set_clock(&track_id, ClockKind::Modeled);

        // A stamp 10 s in the past is stale — dropped, period untouched.
        sched.apply_clock_estimate(ctx, 8.0, 2.4, epoch_now_ns() - 10_000_000_000, "24:0");
        assert_eq!(sched.snapshot_tracks()[0].period, Duration::from_secs(1));
    }

    /// Playing drives one ticked block per beat in tick order; attaching alone
    /// produces nothing (track created stopped); detach halts.
    #[tokio::test]
    async fn play_produces_ticked_blocks_in_order_arm_is_stopped() {
        let (kernel, documents) = fresh_kernel_and_docs().await;
        let ctx = ContextId::new();
        documents.create_document(ctx, DocumentKind::Conversation, None).unwrap();

        let mut sched = BeatScheduler::new(kernel.clone(), documents.clone());
        let base = Instant::now();
        let track_id = TrackId::solo();
        arm_track_with_markers(&kernel, &track_id, 5);

        // Attached but not played: track created stopped → no heap entry, no blocks.
        sched.attach(track_id.clone(), ctx, slow_attachment(), slow_policy()).unwrap();
        let score = sched.score_context(&track_id);
        assert!(sched.next_wake().is_none(), "attach alone schedules nothing (track stopped)");
        let out = sched.fire_due(base + Duration::from_secs(1));
        assert!(out.fired.is_empty());
        assert_eq!(contents(&documents, score).len(), 0);

        // Play: now it beats — the score materializes into the track's score context.
        sched.play(&track_id, base);
        for i in 1..=5 {
            let out = sched.fire_due(base + Duration::from_secs(i));
            assert_eq!(out.fired, vec![ctx], "beat {i}");
            assert_eq!(contents(&documents, score).len(), i as usize);
        }
        assert_eq!(
            contents(&documents, score),
            vec!["beat-1", "beat-2", "beat-3", "beat-4", "beat-5"],
        );

        // Detach: the context is removed; the track clock keeps running but fires
        // no contexts. This is `disarm` in the old API — now explicit with track.
        sched.detach(&track_id, ctx);
        let out = sched.fire_due(base + Duration::from_secs(6));
        assert!(out.fired.is_empty(), "detached → context no longer fires");
    }

    /// 5c-2 core: when a committed ABC cell crosses the write barrier, the
    /// materialize crossing publishes a wire `RenderCue{ text/vnd.abc }` — keyed
    /// by the track's score context, carrying the resolved ABC inline. This is the
    /// path the app MIDI sink consumes; it replaced the deleted in-process
    /// `AlsaMidiOut` emit, so it needs its own regression guard.
    #[tokio::test]
    async fn crossing_an_abc_cell_publishes_a_render_cue() {
        let (kernel, documents) = fresh_kernel_and_docs().await;
        let ctx = ContextId::new();
        documents.create_document(ctx, DocumentKind::Conversation, None).unwrap();

        let track = TrackId::solo();
        // Arm the track with a DEFERRED cell + a resolver that commits valid ABC;
        // resolving commits it Concrete AND stores its bytes in CAS, so the
        // publish path's `retrieve` finds them (commit-margin 0 → the cell commits
        // when the playhead reaches its tick).
        let tl = kernel.arm_track_timeline(
            track.clone(),
            TickClock { ticks_per_sec: 1.0, safety_factor: 1.0, commit_margin: TickDelta::new(0) },
            Tick::ZERO,
        );
        {
            let mut g = tl.lock();
            g.register_resolver(Box::new(ValidAbc));
            g.schedule(Cell::deferred_on(
                Span::instant(Tick::new(1)),
                Recipe {
                    resolver: ResolverId::new("valid_abc"),
                    params: serde_json::Value::Null,
                    query: ContextQuery::default(),
                    fallback: Fallback::Skip,
                },
                track.clone(),
                PrincipalId::beat(),
            ))
            .unwrap();
        }
        let abc = "X:1\nK:C\nCDEF|\n"; // what ValidAbc resolves to

        let mut sched = BeatScheduler::new(kernel.clone(), documents.clone());
        let base = Instant::now();
        sched.attach(track.clone(), ctx, slow_attachment(), slow_policy()).unwrap();
        let score = sched.score_context(&track);

        // Subscribe BEFORE playing: the sink-dependency gate needs an active
        // receiver on the topic, and the FlowBus is a live broadcast.
        let mut sub = kernel.block_flows().subscribe("block.render_cue");

        sched.play(&track, base);
        sched.fire_due(base + Duration::from_secs(1)); // playhead reaches tick 1

        let msg = sub
            .try_recv()
            .expect("a RenderCue must be published for the crossed ABC cell");
        match msg.payload {
            BlockFlow::RenderCue { context_id, cue } => {
                assert_eq!(context_id, score, "cue is keyed by the track's score context");
                assert_eq!(cue.mime, kaijutsu_audio::ABC_MIME, "ABC cue mime");
                match cue.payload {
                    kaijutsu_audio::CuePayload::Inline(bytes) => {
                        assert_eq!(bytes, abc.as_bytes(), "the resolved ABC rides inline");
                    }
                    other => panic!("expected Inline payload, got {other:?}"),
                }
            }
            other => panic!("expected RenderCue, got {other:?}"),
        }
    }

    /// 5c-2 flush: `stop` (and `pause`) publish a contentless `RENDER_FLUSH_MIME`
    /// cue keyed by the track's score context, so every wire sink drops its
    /// buffered phrase + silences — the wire twin of the deleted in-process
    /// `flush_scheduled_after`. Unlike the render cue this fires even for a track
    /// that never played (a bare `attach` then `stop`).
    #[tokio::test]
    async fn stop_publishes_a_render_flush_cue() {
        let (kernel, documents) = fresh_kernel_and_docs().await;
        let ctx = ContextId::new();
        documents.create_document(ctx, DocumentKind::Conversation, None).unwrap();

        let mut sched = BeatScheduler::new(kernel.clone(), documents.clone());
        let track = TrackId::solo();
        arm_track_with_markers(&kernel, &track, 1);
        sched.attach(track.clone(), ctx, slow_attachment(), slow_policy()).unwrap();
        let score = sched.score_context(&track);

        let mut sub = kernel.block_flows().subscribe("block.render_cue");
        sched.stop(&track);

        let msg = sub.try_recv().expect("stop must publish a flush cue");
        match msg.payload {
            BlockFlow::RenderCue { context_id, cue } => {
                assert_eq!(context_id, score, "flush is keyed by the track's score context");
                assert_eq!(cue.mime, kaijutsu_audio::RENDER_FLUSH_MIME, "flush cue mime");
                assert!(
                    matches!(cue.payload, kaijutsu_audio::CuePayload::Inline(ref b) if b.is_empty()),
                    "flush is a contentless directive cue",
                );
            }
            other => panic!("expected a flush RenderCue, got {other:?}"),
        }
    }

    /// The metronome substrate (slice 3): while a track's clock rolls, each beat
    /// auto-emits a `BeatSync` reference keyed by the score context — the beat
    /// coordinate is the playhead (1 tick == 1 beat) and the tempo is `1/period`.
    /// This feeds the app's continuous timebase (the phasor); the audible click is
    /// the sink's opt-in. Emitted independent of whether any cell crossed (a bare
    /// rolling clock still ticks), and gated on a subscriber like the render cue.
    #[tokio::test]
    async fn a_rolling_clock_emits_beat_sync_references() {
        let (kernel, documents) = fresh_kernel_and_docs().await;
        let ctx = ContextId::new();
        documents.create_document(ctx, DocumentKind::Conversation, None).unwrap();

        let mut sched = BeatScheduler::new(kernel.clone(), documents.clone());
        let track = TrackId::solo();
        arm_track_with_markers(&kernel, &track, 4);
        // slow_policy() → 1s period → 1.0 beats/sec (60 BPM).
        sched.attach(track.clone(), ctx, slow_attachment(), slow_policy()).unwrap();
        let score = sched.score_context(&track);

        // Subscribe BEFORE playing — the sink-dependency gate needs a live receiver.
        let mut sub = kernel.block_flows().subscribe("block.beat_sync");

        let base = Instant::now();
        sched.play(&track, base);
        // Fire beats 1..8. The phasor free-runs between references, so the kernel
        // emits at a LOW rate: beat 1 (anchor) + beat 8 (BEAT_SYNC_EVERY), and
        // NOT the beats in between.
        for beat in 1..=8u64 {
            sched.fire_due(base + Duration::from_secs(beat));
        }

        let (ctx1, ref1) = match sub.try_recv().expect("beat 1 anchor reference").payload {
            BlockFlow::BeatSync { context_id, beat_ref } => (context_id, beat_ref),
            other => panic!("expected BeatSync, got {other:?}"),
        };
        assert_eq!(ctx1, score, "keyed by the track's score context");
        assert!((ref1.tempo_bps - 1.0).abs() < 1e-9, "60 BPM = 1 beat/sec, got {}", ref1.tempo_bps);

        // Beats 2..7 emit nothing (low-rate) — the next reference is beat 8.
        let (_ctx2, ref2) = match sub.try_recv().expect("beat 8 reference").payload {
            BlockFlow::BeatSync { context_id, beat_ref } => (context_id, beat_ref),
            other => panic!("expected the beat-8 BeatSync, got {other:?}"),
        };
        assert!((ref2.tempo_bps - 1.0).abs() < 1e-9);
        assert!(
            (ref2.beat - ref1.beat - 7.0).abs() < 1e-9,
            "references are 7 beats apart (beat 1 → beat 8): {} → {}",
            ref1.beat,
            ref2.beat,
        );

        // And nothing else was published in that window (no per-beat chatter).
        assert!(sub.try_recv().is_none(), "only beats 1 and 8 emit, not every beat");
    }

    /// A headless kernel (no sink subscribed on `block.beat_sync`) emits no beat
    /// references — the same sink-dependency gate as the render cue. No subscriber,
    /// no work.
    #[tokio::test]
    async fn no_subscriber_no_beat_sync() {
        let (kernel, documents) = fresh_kernel_and_docs().await;
        let ctx = ContextId::new();
        documents.create_document(ctx, DocumentKind::Conversation, None).unwrap();

        let mut sched = BeatScheduler::new(kernel.clone(), documents.clone());
        let track = TrackId::solo();
        arm_track_with_markers(&kernel, &track, 2);
        sched.attach(track.clone(), ctx, slow_attachment(), slow_policy()).unwrap();

        // Subscribe only AFTER the beat fires — the gate saw zero subscribers, so
        // nothing was published; this late subscriber receives nothing.
        let base = Instant::now();
        sched.play(&track, base);
        sched.fire_due(base + Duration::from_secs(1));

        let mut sub = kernel.block_flows().subscribe("block.beat_sync");
        assert!(
            sub.try_recv().is_none(),
            "no beat references should have been published without a subscriber",
        );
    }

    /// `set_tempo` re-slaves the armed track timeline's speculation `TickClock`
    /// (WI 2), not just the clock source — so a tempo change can't leave the lead
    /// stale and wedge the fallback.
    #[tokio::test]
    async fn set_tempo_reslaves_the_track_timeline_clock() {
        let (kernel, documents) = fresh_kernel_and_docs().await;
        let ctx = ContextId::new();
        documents.create_document(ctx, DocumentKind::Conversation, None).unwrap();

        let mut sched = BeatScheduler::new(kernel.clone(), documents.clone());
        let track_id = TrackId::solo();
        // attach arms the track timeline from slow_policy()'s 1s period → 1 tick/sec.
        sched.attach(track_id.clone(), ctx, slow_attachment(), slow_policy()).unwrap();
        assert_eq!(
            kernel.track_timeline(&track_id).unwrap().lock().clock_for_test().ticks_per_sec,
            1.0,
            "armed at the 1s policy period",
        );

        // Quadruple the tempo (250 ms/beat): both the clock source AND the timeline's
        // speculation clock must move to 4 ticks/sec.
        sched.set_tempo(&track_id, Duration::from_millis(250));
        assert_eq!(
            kernel.track_timeline(&track_id).unwrap().lock().clock_for_test().ticks_per_sec,
            4.0,
            "set_tempo slaved the new TickClock down to the track timeline",
        );
    }

    /// SEV-1 (gemini-pro Stage-3 review): a cold restart must recover the
    /// **persisted** tempo + phrasing, not silently revert to the attaching
    /// context's [`BeatPolicy`] DTO (which defaults to 120 BPM). `set_tempo`
    /// persists the new period *precisely so a restart recovers it* — but `attach`
    /// armed the live clock + speculation timeline from the DTO, throwing the saved
    /// tempo away on the first re-attach. This persists a fast 240-BPM / 4-beat
    /// track, then re-attaches with the SLOW default policy and asserts the live
    /// clock came from the row, not the DTO.
    #[tokio::test]
    async fn attach_recovers_persisted_tempo_not_the_dto_policy() {
        let (kernel, documents, db, ctx) = db_backed_kernel_and_docs().await;
        let track = TrackId::new("bass").unwrap();

        // A prior session saved a fast track: 250 ms/beat (240 BPM), 4-beat phrase.
        db.lock()
            .upsert_track(&PersistedTrack {
                track_id: "bass".to_string(),
                period_ms: 250,
                beats_per_phrase: 4,
                playhead_tick: None,
                playing: false,
                score_context_id: None,
                clock_kind: "system".to_string(),
            })
            .unwrap();

        // Restart: a fresh scheduler re-attaches with the SLOW default policy (1
        // s/beat, 1_000_000-beat phrase) — the DTO the Attach command carries, NOT
        // the saved tempo.
        let mut sched = BeatScheduler::new(kernel.clone(), documents.clone());
        sched.attach(track.clone(), ctx, slow_attachment(), slow_policy()).unwrap();

        // The live clock and the armed speculation timeline both reflect the
        // PERSISTED 250 ms/beat → 4 ticks/sec, not the DTO's 1 s/beat → 1 tick/sec.
        let phrasing = sched.tracks.get(&track).expect("track exists").phrasing();
        assert_eq!(
            phrasing.period,
            Duration::from_millis(250),
            "restart recovers the persisted beat period, not the DTO default",
        );
        assert_eq!(
            phrasing.beats_per_phrase, 4,
            "restart recovers the persisted phrase length, not the DTO default",
        );
        assert_eq!(
            kernel.track_timeline(&track).unwrap().lock().clock_for_test().ticks_per_sec,
            4.0,
            "the speculation timeline is armed from the persisted period, not the DTO",
        );
    }

    /// WI 3 (Stage 3): re-attach reconstructs the clock SOURCE from the persisted
    /// `clock_kind`. A `"system"` row re-arms a system clock (no regression). A
    /// `"modeled"` row names a driver M1 can't construct ([`ModeledClock`] is
    /// uninhabited until M3) — `attach` crashes loud rather than silently
    /// downgrading the clock to the system one (CLAUDE.md: crash over corruption).
    #[tokio::test]
    async fn attach_crashes_loud_on_an_unconstructable_clock_kind() {
        let (kernel, documents, db, ctx) = db_backed_kernel_and_docs().await;

        // A persisted "modeled" row is legal since M3: it reconstructs a
        // free-running modeled clock at the persisted tempo (the anchor is
        // process-local; the observer re-locks it). This was the M1-era
        // crash case — its premise dissolved when ModeledClock got a body.
        db.lock()
            .upsert_track(&PersistedTrack {
                track_id: "bass".to_string(),
                period_ms: 250,
                beats_per_phrase: 4,
                playhead_tick: None,
                playing: false,
                score_context_id: None,
                clock_kind: "modeled".to_string(),
            })
            .unwrap();
        let mut sched = BeatScheduler::new(kernel.clone(), documents.clone());
        let bass = TrackId::new("bass").unwrap();
        sched
            .attach(bass.clone(), ctx, slow_attachment(), slow_policy())
            .expect("a persisted modeled clock reconstructs free-running");
        let snap = sched.snapshot_tracks();
        assert_eq!(snap[0].clock_kind, "modeled");
        assert_eq!(snap[0].period, Duration::from_millis(250), "persisted tempo, not the DTO");

        // An UNKNOWN kind still crashes loud — no silent downgrade.
        db.lock()
            .upsert_track(&PersistedTrack {
                track_id: "keys".to_string(),
                period_ms: 250,
                beats_per_phrase: 4,
                playhead_tick: None,
                playing: false,
                score_context_id: None,
                clock_kind: "quantum-flux".to_string(),
            })
            .unwrap();
        let keys = TrackId::new("keys").unwrap();
        let ctx2 = ContextId::new();
        documents.create_document(ctx2, DocumentKind::Conversation, None).unwrap();
        let err = sched
            .attach(keys.clone(), ctx2, slow_attachment(), slow_policy())
            .expect_err("an unknown clock_kind must fail loud");
        assert!(
            err.contains("clock_kind") && err.contains("quantum-flux"),
            "the error names the unknown clock_kind, got: {err}"
        );
        assert!(
            sched.tracks.get(&keys).is_none(),
            "a failed clock reconstruction leaves no half-armed track"
        );
    }

    /// Pause freezes the track playhead; resume picks up at +1 with no wall-clock
    /// catch-up, even after a long quiescent gap.
    #[tokio::test]
    async fn pause_freezes_and_resume_continues_at_plus_one() {
        let (kernel, documents) = fresh_kernel_and_docs().await;
        let ctx = ContextId::new();
        documents.create_document(ctx, DocumentKind::Conversation, None).unwrap();

        let mut sched = BeatScheduler::new(kernel.clone(), documents.clone());
        let base = Instant::now();
        let track_id = TrackId::solo();
        arm_track_with_markers(&kernel, &track_id, 5);
        sched.attach(track_id.clone(), ctx, slow_attachment(), slow_policy()).unwrap();
        let score = sched.score_context(&track_id);
        sched.play(&track_id, base);

        sched.fire_due(base + Duration::from_secs(1)); // tick 1
        sched.fire_due(base + Duration::from_secs(2)); // tick 2
        assert_eq!(contents(&documents, score), vec!["beat-1", "beat-2"]);

        // Pause; the stale heap entry fires once and is dropped — no advance.
        sched.pause(&track_id);
        sched.fire_due(base + Duration::from_secs(3));
        assert_eq!(contents(&documents, score).len(), 2, "paused → frozen");

        // Resume much later in wall-clock; the tick continues at 3, not jumping.
        sched.play(&track_id, base + Duration::from_secs(3600));
        sched.fire_due(base + Duration::from_secs(3601));
        assert_eq!(
            contents(&documents, score),
            vec!["beat-1", "beat-2", "beat-3"],
            "resume at +1, no catch-up for the hour spent paused"
        );
    }

    /// A second context attached to the same track mid-run also beats when the
    /// track fires. Both contexts share one clock domain (the track), so the
    /// second `play` call is a no-op (the track is already playing).
    #[tokio::test]
    async fn second_context_played_midrun_also_beats() {
        let (kernel, documents) = fresh_kernel_and_docs().await;
        let a = ContextId::new();
        let b = ContextId::new();
        documents.create_document(a, DocumentKind::Conversation, None).unwrap();
        documents.create_document(b, DocumentKind::Conversation, None).unwrap();

        let mut sched = BeatScheduler::new(kernel.clone(), documents.clone());
        let base = Instant::now();
        let track_id = TrackId::solo();
        arm_track_with_markers(&kernel, &track_id, 5);
        sched.attach(track_id.clone(), a, slow_attachment(), slow_policy()).unwrap();
        let score = sched.score_context(&track_id);
        sched.play(&track_id, base);

        let out1 = sched.fire_due(base + Duration::from_secs(1));
        let out2 = sched.fire_due(base + Duration::from_secs(2));
        assert_eq!(out1.fired, vec![a]);
        assert_eq!(out2.fired, vec![a], "only a attached so far");
        // The score is the TRACK's — materialized once per beat regardless of how
        // many contexts are attached (the hoist; no per-producer double-emit).
        assert_eq!(contents(&documents, score).len(), 2);

        // Attach b to the already-playing track (same clock domain). `play` is a
        // no-op — the track is already running.
        sched.attach(track_id.clone(), b, slow_attachment(), slow_policy()).unwrap();
        sched.play(&track_id, base + Duration::from_secs(2));
        // Beat 3: BOTH a and b fire (the beat wakes every attached context); the
        // shared score advances by exactly one, not once per producer.
        let out3 = sched.fire_due(base + Duration::from_secs(3));
        assert!(
            out3.fired.contains(&a) && out3.fired.contains(&b),
            "both contexts fire on beat 3"
        );
        assert_eq!(contents(&documents, score).len(), 3, "shared score advanced once");
    }

    /// The wakeup cadence fires every N beats — but only while OODA is armed.
    /// `set_ooda(false)` suppresses it without touching the clock. The wakeup
    /// cadence moved from `BeatPolicy.ooda_every` onto `Attachment.wakeup`.
    #[tokio::test]
    async fn ooda_cadence_fires_every_n_beats_when_armed() {
        let (kernel, documents) = fresh_kernel_and_docs().await;
        let ctx = ContextId::new();
        documents.create_document(ctx, DocumentKind::Conversation, None).unwrap();
        kernel.arm_timeline(
            ctx,
            TickClock { ticks_per_sec: 1.0, safety_factor: 1.0, commit_margin: TickDelta::new(0) },
            Tick::ZERO,
        );

        let mut sched = BeatScheduler::new(kernel.clone(), documents.clone());
        let base = Instant::now();
        let track_id = TrackId::solo();
        // wakeup every 3 beats (was `ooda_every: 3` on BeatPolicy).
        sched.attach(
            track_id.clone(),
            ctx,
            Attachment { wakeup: Cadence::new(3), rotate: None, ooda_armed: true, pulse: 0 },
            BeatPolicy { period: Duration::from_secs(1), beats_per_phrase: 1_000_000 },
        )
        .unwrap();
        sched.play(&track_id, base);

        let mut ooda_beats = Vec::new();
        for i in 1..=6 {
            let out = sched.fire_due(base + Duration::from_secs(i));
            if !out.ooda_due.is_empty() {
                ooda_beats.push(i);
            }
        }
        assert_eq!(ooda_beats, vec![3, 6], "OODA boundary every 3 beats while armed");

        // Disarm OODA: wakeup cadence boundaries no longer report due.
        sched.set_ooda(&track_id, ctx, false);
        let mut after = Vec::new();
        for i in 7..=12 {
            let out = sched.fire_due(base + Duration::from_secs(i));
            if !out.ooda_due.is_empty() {
                after.push(i);
            }
        }
        assert!(after.is_empty(), "OODA disarmed → no cadence fires (beat 9 would have)");
    }

    /// T3 (design-chameleon-batch1-f2-notation §16) — phrase boundaries are
    /// reported on `BeatOutcome.phrase_due` every `beats_per_phrase` beats. With
    /// `beats_per_phrase = 4`, beats 4, 8, 12 are phrase-due (and 1-3, 5-7, etc.
    /// are not). This is the observable seam for cue traps / quantized flush /
    /// standing per-phrase cells; unlike OODA, it is independent of the OODA arm.
    #[tokio::test]
    async fn phrase_boundaries_reported_every_beats_per_phrase() {
        let (kernel, documents) = fresh_kernel_and_docs().await;
        let ctx = ContextId::new();
        documents.create_document(ctx, DocumentKind::Conversation, None).unwrap();
        kernel.arm_timeline(
            ctx,
            TickClock { ticks_per_sec: 1.0, safety_factor: 1.0, commit_margin: TickDelta::new(0) },
            Tick::ZERO,
        );

        let mut sched = BeatScheduler::new(kernel.clone(), documents.clone());
        let base = Instant::now();
        let track_id = TrackId::solo();
        // beats_per_phrase = 4; large wakeup so the OODA cadence never coincides.
        sched.attach(
            track_id.clone(),
            ctx,
            Attachment { wakeup: Cadence::new(1_000_000), rotate: None, ooda_armed: true, pulse: 0 },
            BeatPolicy { period: Duration::from_secs(1), beats_per_phrase: 4 },
        )
        .unwrap();
        sched.play(&track_id, base);

        let mut phrase_beats = Vec::new();
        for i in 1..=12 {
            let out = sched.fire_due(base + Duration::from_secs(i));
            if !out.phrase_due.is_empty() {
                assert_eq!(out.phrase_due, vec![ctx], "phrase_due names the context");
                phrase_beats.push(i);
            }
        }
        assert_eq!(
            phrase_beats,
            vec![4, 8, 12],
            "phrase boundary every 4 beats"
        );
    }

    /// At a rotate horizon the scheduler DETACHES the parent SYNCHRONOUSLY in
    /// `fire_due` — it reports `rotate_due` (NOT `ooda_due`, even when the OODA
    /// cadence coincides) and the parent does not fire again. This closes the
    /// stray-tick / double-fork race that an async disarm could not. In the new
    /// model, "retirement" is a synchronous detach (not just stop), and the
    /// track's clock KEEPS RUNNING — continuity is free.
    #[tokio::test]
    async fn rotate_horizon_retires_parent_synchronously() {
        let (kernel, documents) = fresh_kernel_and_docs().await;
        let ctx = ContextId::new();
        documents.create_document(ctx, DocumentKind::Conversation, None).unwrap();
        kernel.arm_timeline(
            ctx,
            TickClock { ticks_per_sec: 1.0, safety_factor: 1.0, commit_margin: TickDelta::new(0) },
            Tick::ZERO,
        );

        let mut sched = BeatScheduler::new(kernel.clone(), documents.clone());
        let base = Instant::now();
        let track_id = TrackId::solo();
        // phrase = 2 beats; wakeup also every 2 beats so the rotate beat COINCIDES
        // with an OODA boundary — proving rotate suppresses the stray ooda tick.
        sched.attach(
            track_id.clone(),
            ctx,
            Attachment { wakeup: Cadence::new(2), rotate: None, ooda_armed: true, pulse: 0 },
            BeatPolicy { period: Duration::from_secs(1), beats_per_phrase: 2 },
        )
        .unwrap();
        sched.set_rotate(&track_id, ctx, Some(Cadence::new(1))); // rotate every phrase
        sched.play(&track_id, base);

        // Beat 1: mid-phrase — nothing due.
        let b1 = sched.fire_due(base + Duration::from_secs(1));
        assert_eq!(b1.fired, vec![ctx]);
        assert!(b1.rotate_due.is_empty() && b1.ooda_due.is_empty());

        // Beat 2: phrase 1 horizon → rotate. Reported on rotate_due, and NOT on
        // ooda_due even though beat_count % wakeup_cadence == 0.
        let b2 = sched.fire_due(base + Duration::from_secs(2));
        assert_eq!(b2.rotate_due, vec![ctx], "horizon reports rotate_due");
        assert!(b2.ooda_due.is_empty(), "rotate suppresses the coincident ooda tick");
        // In the new model, rotation DETACHES the context (not just stops it).
        // The context is removed from the track's attached set and reverse index.
        assert!(
            !sched.tracks[&track_id].attached.contains_key(&ctx),
            "parent is detached from the track after a rotate horizon (synchronous retire)"
        );
        assert!(
            !sched.context_track.contains_key(&ctx),
            "parent removed from the context→track reverse index"
        );
        // The track clock KEEPS RUNNING — continuity is free across the page-turn.
        assert!(
            sched.tracks[&track_id].playing,
            "the track clock never pauses across a rotation page-turn"
        );

        // Beat 3+: the parent was detached → it never fires again. No stray
        // ticks could leak between the rotate decision and a (now unnecessary)
        // async disarm, because there is no async disarm.
        let b3 = sched.fire_due(base + Duration::from_secs(3));
        assert!(b3.fired.is_empty(), "retired parent does not tick again");
    }

    /// Rotation only fires at `phrase % every == 0`: with `every = 2` and a
    /// 2-beat phrase, phrase 1 (beat 2) is a phrase boundary but NOT a rotate
    /// horizon — it ticks normally; phrase 2 (beat 4) rotates.
    #[tokio::test]
    async fn rotate_cadence_gates_on_the_modulus() {
        let (kernel, documents) = fresh_kernel_and_docs().await;
        let ctx = ContextId::new();
        documents.create_document(ctx, DocumentKind::Conversation, None).unwrap();
        kernel.arm_timeline(
            ctx,
            TickClock { ticks_per_sec: 1.0, safety_factor: 1.0, commit_margin: TickDelta::new(0) },
            Tick::ZERO,
        );

        let mut sched = BeatScheduler::new(kernel.clone(), documents.clone());
        let base = Instant::now();
        let track_id = TrackId::solo();
        sched.attach(
            track_id.clone(),
            ctx,
            Attachment { wakeup: Cadence::new(1_000_000), rotate: None, ooda_armed: true, pulse: 0 },
            BeatPolicy { period: Duration::from_secs(1), beats_per_phrase: 2 },
        )
        .unwrap();
        sched.set_rotate(&track_id, ctx, Some(Cadence::new(2))); // rotate every 2 phrases
        sched.play(&track_id, base);

        let mut rotate_beats = Vec::new();
        for i in 1..=4 {
            let out = sched.fire_due(base + Duration::from_secs(i));
            if !out.rotate_due.is_empty() {
                rotate_beats.push(i);
                break; // parent retires here; later wakes won't fire it
            }
        }
        assert_eq!(rotate_beats, vec![4], "phrase 2 (beat 4) is the first rotate horizon, not phrase 1");
    }

    /// With no rotate cadence set, a phrase horizon is a normal beat — no
    /// rotate_due, the context keeps ticking (the default, non-rotating player).
    #[tokio::test]
    async fn no_rotate_when_cadence_unset() {
        let (kernel, documents) = fresh_kernel_and_docs().await;
        let ctx = ContextId::new();
        documents.create_document(ctx, DocumentKind::Conversation, None).unwrap();
        kernel.arm_timeline(
            ctx,
            TickClock { ticks_per_sec: 1.0, safety_factor: 1.0, commit_margin: TickDelta::new(0) },
            Tick::ZERO,
        );

        let mut sched = BeatScheduler::new(kernel.clone(), documents.clone());
        let base = Instant::now();
        let track_id = TrackId::solo();
        sched.attach(
            track_id.clone(),
            ctx,
            Attachment { wakeup: Cadence::new(1_000_000), rotate: None, ooda_armed: true, pulse: 0 },
            BeatPolicy { period: Duration::from_secs(1), beats_per_phrase: 2 },
        )
        .unwrap();
        sched.play(&track_id, base);

        for i in 1..=4 {
            let out = sched.fire_due(base + Duration::from_secs(i));
            assert!(out.rotate_due.is_empty(), "no rotate cadence → never rotate (beat {i})");
            assert_eq!(out.fired, vec![ctx], "keeps ticking (beat {i})");
        }
    }

    /// Insert a player-authored Model/Text ABC block and return its id. The
    /// shape the turn stream produces: `Role::Model`, no track, a real author.
    fn insert_player_abc(
        documents: &SharedBlockStore,
        ctx: ContextId,
        player: PrincipalId,
        abc: &str,
    ) -> kaijutsu_crdt::BlockId {
        use kaijutsu_crdt::{BlockKind, ContentType, Role, Status};
        documents
            .insert_block_as(
                ctx,
                None,
                documents.last_block_id(ctx).as_ref(),
                Role::Model,
                BlockKind::Text,
                abc.to_string(),
                Status::Done,
                ContentType::Plain,
                Some(player),
            )
            .unwrap()
    }

    /// T17 (design-chameleon-batch1-f2-notation §16) — a completed turn schedules
    /// its ABC **one phrase ahead** of the playhead, and the ABC+MIDI pair
    /// materializes on the beat. The lead is `phrase_delta()` (16 at the default,
    /// here 4 so the test window can reach it), not the deleted fixed-4 OODA_LEAD.
    /// The output block id is carried explicitly (§7) — no blind last-block read.
    #[tokio::test]
    async fn completed_turn_schedules_one_phrase_ahead() {
        use kaijutsu_crdt::Role;

        let (kernel, documents) = fresh_kernel_and_docs().await;
        let ctx = ContextId::new();
        documents.create_document(ctx, DocumentKind::Conversation, None).unwrap();
        let player = PrincipalId::new();
        let abc_block =
            insert_player_abc(&documents, ctx, player, "X:1\nT:Test\nM:4/4\nL:1/8\nK:C\nCDEFGABc|\n");

        // beats_per_phrase = 4 so the scheduling lead (= phrase_delta() = 4) is
        // reachable in this 8-beat window; large wakeup so the OODA cadence never fires.
        let phrase = BeatPolicy {
            period: Duration::from_secs(1),
            beats_per_phrase: 4,
        };
        let mut sched = BeatScheduler::new(kernel.clone(), documents.clone());
        let base = Instant::now();
        let track_id = TrackId::solo();
        sched.attach(track_id.clone(), ctx, slow_attachment(), phrase).unwrap();
        let score = sched.score_context(&track_id);
        sched.play(&track_id, base);

        sched.fire_due(base + Duration::from_secs(1)); // playhead → 1
        // The cell is scheduled at playhead(1) + phrase_delta()(4) = tick 5. The
        // one-phrase-ahead start is observable on the materialized block's tick
        // (asserted below) — `phrase_delta()` (4 here) not the deleted 4-const.
        sched.on_turn_completed(ctx, Some(abc_block));

        // The score materializes into the TRACK's score context now, not the producer's.
        let asset_before = documents
            .block_snapshots(score)
            .unwrap()
            .iter()
            .filter(|b| b.role == Role::Asset)
            .count();
        assert_eq!(asset_before, 0, "no MIDI block before the scheduled cell commits");

        // Advance past tick 5 so the cell commits and materializes the pair.
        for i in 2..=8 {
            sched.fire_due(base + Duration::from_secs(i));
        }

        let snaps = documents.block_snapshots(score).unwrap();
        // The ABC source materializes as a Model staff (ephemeral) and the MIDI
        // sibling as an Asset hash — the F2 score↔render pair.
        let midi = snaps
            .iter()
            .find(|b| b.role == Role::Asset)
            .expect("the abc→midi MIDI sibling materialized");
        assert_eq!(midi.content.len(), 32, "MIDI block content is the 32-hex CAS hash");
        // The one-phrase-ahead lead is observable here: the cell committed at
        // tick playhead(1) + phrase_delta()(4) = 5, and the materialized pair
        // carries that beat coordinate.
        assert_eq!(
            midi.tick,
            Some(Tick::new(5)),
            "materialized at playhead + phrase_delta() (1 + 4), the one-phrase lead"
        );
        assert_eq!(
            midi.track,
            Some(TrackId::solo()),
            "the materialized block carries the musician's lane"
        );
        assert_eq!(
            midi.id.principal_id, player,
            "the materialized block is authored by the player (played_by), not beat()"
        );
    }

    /// T16 (design §16) — THE silent-feedback-loop pin. `on_turn_completed` must
    /// schedule from the **carried output block id**, with defense-in-depth
    /// guards so the bridge's own output can never loop back into the OODA Act:
    ///   - `Completed { None }` → schedules nothing.
    ///   - a carried id pointing at a materialized (track-bearing) or ephemeral
    ///     or beat()-authored block → refused (loud), schedules nothing.
    ///   - a real player Model block → scheduled.
    #[tokio::test]
    async fn materialized_abc_is_not_rescheduled_on_turn_completed() {
        use kaijutsu_crdt::{BlockKind, ContentType, Role, Status};

        let phrase = BeatPolicy {
            period: Duration::from_secs(1),
            beats_per_phrase: 4,
        };

        // Helper: count scheduled future cells on the track's (shared) timeline.
        fn future_len(kernel: &Kernel, _ctx: ContextId) -> usize {
            kernel.track_timeline(&TrackId::solo()).unwrap().lock().future_len()
        }

        // — Case None: Completed{None} schedules nothing —
        {
            let (kernel, documents) = fresh_kernel_and_docs().await;
            let ctx = ContextId::new();
            documents.create_document(ctx, DocumentKind::Conversation, None).unwrap();
            let mut sched = BeatScheduler::new(kernel.clone(), documents.clone());
            let base = Instant::now();
            sched.attach(TrackId::solo(), ctx, slow_attachment(), phrase).unwrap();
            sched.play(&TrackId::solo(), base);
            sched.fire_due(base + Duration::from_secs(1));
            sched.on_turn_completed(ctx, None);
            assert_eq!(future_len(&kernel, ctx), 0, "None → nothing scheduled");
        }

        // — Case materialized: a track-bearing block (what materialization stamps) is
        //   refused. Materialized blocks live in the score context now, so to exercise
        //   the `b.track.is_some()` guard we hand `on_turn_completed` a track-bearing
        //   block placed in the producer's own doc directly (the case the guard defends).
        {
            use kaijutsu_crdt::BlockSnapshotBuilder;
            let (kernel, documents) = fresh_kernel_and_docs().await;
            let ctx = ContextId::new();
            documents.create_document(ctx, DocumentKind::Conversation, None).unwrap();
            let player = PrincipalId::new();
            let seq = documents.reserve_block_id(ctx, player).unwrap().seq;
            let track_bearing = kaijutsu_crdt::BlockId::new(ctx, player, seq);
            let snap = BlockSnapshotBuilder::new(track_bearing, BlockKind::Text)
                .role(Role::Model)
                .content("X:1\nK:C\nCDEF|\n")
                .content_type(ContentType::Abc)
                .track(TrackId::solo())
                .build();
            documents.insert_from_snapshot_as(ctx, snap, None, Some(player)).unwrap();
            let mut sched = BeatScheduler::new(kernel.clone(), documents.clone());
            let base = Instant::now();
            sched.attach(TrackId::solo(), ctx, slow_attachment(), phrase).unwrap();
            sched.play(&TrackId::solo(), base);
            sched.fire_due(base + Duration::from_secs(1));
            let future_before = future_len(&kernel, ctx);
            sched.on_turn_completed(ctx, Some(track_bearing));
            assert_eq!(
                future_len(&kernel, ctx),
                future_before,
                "a track-bearing block must not be re-scheduled (the feedback loop)"
            );
        }

        // — Case beat()-authored: a legacy transport block is refused —
        {
            let (kernel, documents) = fresh_kernel_and_docs().await;
            let ctx = ContextId::new();
            documents.create_document(ctx, DocumentKind::Conversation, None).unwrap();
            let beat_block = documents
                .insert_block_as(
                    ctx,
                    None,
                    None,
                    Role::Model,
                    BlockKind::Text,
                    "X:1\nK:C\nCDEF|\n".to_string(),
                    Status::Done,
                    ContentType::Plain,
                    Some(PrincipalId::beat()),
                )
                .unwrap();
            let mut sched = BeatScheduler::new(kernel.clone(), documents.clone());
            let base = Instant::now();
            sched.attach(TrackId::solo(), ctx, slow_attachment(), phrase).unwrap();
            sched.play(&TrackId::solo(), base);
            sched.fire_due(base + Duration::from_secs(1));
            let future_before = future_len(&kernel, ctx);
            sched.on_turn_completed(ctx, Some(beat_block));
            assert_eq!(
                future_len(&kernel, ctx),
                future_before,
                "a beat()-authored block must not be re-scheduled"
            );
        }

        // — Case ephemeral: a system-managed ephemeral block is refused —
        {
            let (kernel, documents) = fresh_kernel_and_docs().await;
            let ctx = ContextId::new();
            documents.create_document(ctx, DocumentKind::Conversation, None).unwrap();
            let player = PrincipalId::new();
            let eph = documents
                .insert_block_as(
                    ctx,
                    None,
                    None,
                    Role::Model,
                    BlockKind::Text,
                    "X:1\nK:C\nCDEF|\n".to_string(),
                    Status::Done,
                    ContentType::Plain,
                    Some(player),
                )
                .unwrap();
            documents.set_ephemeral(ctx, &eph, true).unwrap();
            let mut sched = BeatScheduler::new(kernel.clone(), documents.clone());
            let base = Instant::now();
            sched.attach(TrackId::solo(), ctx, slow_attachment(), phrase).unwrap();
            sched.play(&TrackId::solo(), base);
            sched.fire_due(base + Duration::from_secs(1));
            let future_before = future_len(&kernel, ctx);
            sched.on_turn_completed(ctx, Some(eph));
            assert_eq!(
                future_len(&kernel, ctx),
                future_before,
                "an ephemeral block must not be re-scheduled"
            );
        }

        // — Case real player Model block: IS scheduled —
        {
            let (kernel, documents) = fresh_kernel_and_docs().await;
            let ctx = ContextId::new();
            documents.create_document(ctx, DocumentKind::Conversation, None).unwrap();
            let player = PrincipalId::new();
            let abc = insert_player_abc(&documents, ctx, player, "X:1\nK:C\nCDEFGABc|\n");
            let mut sched = BeatScheduler::new(kernel.clone(), documents.clone());
            let base = Instant::now();
            sched.attach(TrackId::solo(), ctx, slow_attachment(), phrase).unwrap();
            sched.play(&TrackId::solo(), base);
            sched.fire_due(base + Duration::from_secs(1));
            let future_before = future_len(&kernel, ctx);
            sched.on_turn_completed(ctx, Some(abc));
            assert_eq!(
                future_len(&kernel, ctx),
                future_before + 1,
                "a genuine player Model block IS scheduled (one new cell)"
            );
        }
    }

    /// Design §7 — a schedule failure surfaces a visible `BlockKind::Error`, not
    /// just a log line. Carry a player block whose ABC is malformed: the §3
    /// schedule-time validator Errs, and `on_turn_completed` anchors an Error
    /// block at the offending output block so the player reads its own rejection.
    #[tokio::test]
    async fn schedule_failure_surfaces_error_block() {
        use kaijutsu_crdt::BlockKind;

        let (kernel, documents) = fresh_kernel_and_docs().await;
        let ctx = ContextId::new();
        documents.create_document(ctx, DocumentKind::Conversation, None).unwrap();
        let player = PrincipalId::new();
        // Not valid ABC — no X:/K: headers, just prose. validate_abc (Strict)
        // rejects it, so schedule_abc_cell returns Err.
        let bad = insert_player_abc(&documents, ctx, player, "this is not music at all");

        let mut sched = BeatScheduler::new(kernel.clone(), documents.clone());
        let base = Instant::now();
        sched.attach(
            TrackId::solo(),
            ctx,
            slow_attachment(),
            BeatPolicy { period: Duration::from_secs(1), beats_per_phrase: 4 },
        )
        .unwrap();
        sched.play(&TrackId::solo(), base);
        sched.fire_due(base + Duration::from_secs(1));

        let errors_before = documents
            .block_snapshots(ctx)
            .unwrap()
            .into_iter()
            .filter(|b| b.kind == BlockKind::Error)
            .count();
        sched.on_turn_completed(ctx, Some(bad));
        let errors_after: Vec<_> = documents
            .block_snapshots(ctx)
            .unwrap()
            .into_iter()
            .filter(|b| b.kind == BlockKind::Error)
            .collect();
        assert_eq!(
            errors_after.len(),
            errors_before + 1,
            "a failed schedule must surface exactly one visible Error block"
        );
        // The Error is anchored at (a child of) the offending output block.
        assert_eq!(
            errors_after.last().unwrap().parent_id,
            Some(bad),
            "the schedule-error block points back at the player's output block"
        );
        // The malformed phrase is quarantined: excluded from future hydration
        // windows so the model can't copy its own bad ABC next turn. The block
        // stays in the durable log; the Error block above carries the feedback.
        let bad_snap = documents
            .get_block_snapshot(ctx, &bad)
            .unwrap()
            .expect("the offending output block still exists in the log");
        assert!(
            bad_snap.excluded,
            "a phrase that fails to schedule must be excluded from future hydration"
        );
    }

    /// T20 (design §8 Phase 6) — arming seeds the playhead from the document's
    /// max committed tick, so musical time stays globally monotone per context
    /// across restarts. Pre-existing blocks carry ticks to T; arming positions
    /// the playhead at T; the first beat advances to T+1. Re-arming a LIVE
    /// timeline must NOT re-seed (seed lands inside `or_insert_with`, virgin-only).
    #[tokio::test]
    async fn arm_seeds_playhead_from_max_committed_tick() {
        use kaijutsu_crdt::{BlockId, BlockKind, BlockSnapshotBuilder};

        let (kernel, documents) = fresh_kernel_and_docs().await;
        let ctx = ContextId::new();
        documents.create_document(ctx, DocumentKind::Conversation, None).unwrap();

        // Pre-insert blocks carrying ticks up to T = 7 (the persisted musical
        // high-water: what a restart would load before re-arm).
        let player = PrincipalId::new();
        let big_t = 7i64;
        for t in [0i64, 3, big_t, 1] {
            let seq = documents.reserve_block_id(ctx, player).unwrap().seq;
            let snap = BlockSnapshotBuilder::new(
                BlockId::new(ctx, player, seq),
                BlockKind::Text,
            )
            .tick(Tick::new(t))
            .order_key(format!("V{:0>11}AAAA", t))
            .content("c")
            .build();
            documents.insert_from_snapshot_as(ctx, snap, None, Some(player)).unwrap();
        }
        assert_eq!(documents.max_tick(ctx).unwrap(), Some(Tick::new(big_t)));

        let mut sched = BeatScheduler::new(kernel.clone(), documents.clone());

        // Arm: the playhead seeds to the max committed tick, not Tick::ZERO.
        sched.attach(TrackId::solo(), ctx, slow_attachment(), slow_policy()).unwrap();
        let playhead_after_arm = sched.track_playhead(&TrackId::solo()).unwrap();
        assert_eq!(
            playhead_after_arm,
            Tick::new(big_t),
            "arm seeds the playhead from max_tick (musical time stays monotone)"
        );

        // First beat advances from the seeded position: T → T+1.
        let base = Instant::now();
        sched.play(&TrackId::solo(), base);
        sched.fire_due(base + Duration::from_secs(1));
        let playhead_after_beat = sched.track_playhead(&TrackId::solo()).unwrap();
        assert_eq!(
            playhead_after_beat,
            Tick::new(big_t + 1),
            "the first beat advances from the seeded playhead, not from zero"
        );

        // Re-arm of the now-LIVE timeline must NOT re-seed or rewind: the
        // playhead stays where the beat left it (seed_playhead is virgin-only,
        // applied inside or_insert_with which never fires on a re-arm).
        sched.attach(TrackId::solo(), ctx, slow_attachment(), slow_policy()).unwrap();
        let playhead_after_rearm = sched.track_playhead(&TrackId::solo()).unwrap();
        assert_eq!(
            playhead_after_rearm,
            Tick::new(big_t + 1),
            "re-arm of a live timeline never re-seeds (or_insert_with is a no-op)"
        );
    }

    /// Build a db-backed block store + the context row its `beat_state` FKs to, so
    /// the scheduler's persistence write-through has somewhere to land. Returns the
    /// shared DB handle (to read `beat_state` back) and the context id.
    async fn db_backed_kernel_and_docs() -> (
        Arc<Kernel>,
        SharedBlockStore,
        kaijutsu_kernel::block_store::DbHandle,
        ContextId,
    ) {
        use kaijutsu_kernel::block_store::shared_block_store_with_db;
        use kaijutsu_kernel::kernel_db::{ContextRow, KernelDb};
        use kaijutsu_types::{ConsentMode, ContextState};

        let kernel = Arc::new(Kernel::new_ephemeral("test").await);
        let db: kaijutsu_kernel::block_store::DbHandle =
            Arc::new(parking_lot::Mutex::new(KernelDb::in_memory().unwrap()));
        let principal = PrincipalId::new();
        let ctx = ContextId::new();
        let ws_id = {
            let g = db.lock();
            let ws = g.get_or_create_default_workspace(principal).unwrap();
            // beat_state FKs to contexts(context_id): the row must exist first.
            g.insert_context_with_document(
                &ContextRow {
                    context_id: ctx,
                    label: Some("bass".to_string()),
                    provider: None,
                    model: None,
                    system_prompt: None,
                    consent_mode: ConsentMode::default(),
                    context_state: ContextState::Live,
                    context_type: "musician".to_string(),
                    created_at: 0,
                    created_by: principal,
                    forked_from: None,
                    fork_kind: None,
                    archived_at: None,
                    workspace_id: None,
                    preset_id: None,
                    concluded_at: None,
                    last_activity_at: None,
                    promoted_at: None,
                    demoted_at: None,
                    paused_at: None,
                },
                ws,
            )
            .unwrap();
            ws
        };
        let documents = shared_block_store_with_db(db.clone(), ws_id, principal);
        documents
            .create_document(ctx, DocumentKind::Conversation, None)
            .unwrap();
        (kernel, documents, db, ctx)
    }

    /// Attach + tempo + rotate write the live clock through to the `tracks` row and
    /// the binding through to the `attachments` row, so a kernel restart can recover
    /// the clock domain + binding with their real values. The durable half of "re-arm
    /// after a cold start"; the read half is `kj transport attach`.
    #[tokio::test]
    async fn attach_and_tempo_persist_track_and_attachment() {
        let (kernel, documents, db, ctx) = db_backed_kernel_and_docs().await;
        let mut sched = BeatScheduler::new(kernel, documents);

        // Attach with a non-default policy + attachment on the `bass` lane.
        let policy = BeatPolicy { period: Duration::from_millis(500), beats_per_phrase: 12 };
        let track = TrackId::new("bass").unwrap();
        let attachment =
            Attachment { wakeup: Cadence::new(96), rotate: None, ooda_armed: true, pulse: 0 };
        sched.attach(track.clone(), ctx, attachment, policy).unwrap();

        // The track row mirrors the clock (tempo + phrase). Fresh ctx → playhead 0.
        // attach also minted a score context; the row carries its id.
        let score_ctx = sched.tracks.get(&track).expect("track exists").score_context;
        assert_eq!(
            db.lock().get_track("bass").unwrap(),
            Some(PersistedTrack {
                track_id: "bass".to_string(),
                period_ms: 500,
                beats_per_phrase: 12,
                playhead_tick: Some(0),
                playing: false,
                clock_kind: "system".to_string(),
                score_context_id: Some(score_ctx),
            }),
            "attach mirrors the clock into the tracks row for restart recovery"
        );
        // The attachment row mirrors the binding.
        assert_eq!(
            db.lock().get_attachment("bass", ctx).unwrap(),
            Some(PersistedAttachment {
                track_id: "bass".to_string(),
                context_id: ctx,
                wakeup_every: 96,
                rotate_every_phrases: None,
                ooda_armed: true,
            }),
            "attach mirrors the binding into the attachments row"
        );

        // A tempo change to 240 BPM (250 ms) updates the persisted period in place.
        sched.set_tempo(&track, Duration::from_millis(250));
        assert_eq!(
            db.lock().get_track("bass").unwrap().unwrap().period_ms,
            250,
            "set_tempo write-through updates the persisted period"
        );

        // Setting a rotate cadence write-throughs to the attachment row.
        sched.set_rotate(&track, ctx, Some(Cadence::new(4)));
        assert_eq!(
            db.lock().get_attachment("bass", ctx).unwrap().unwrap().rotate_every_phrases,
            Some(4),
            "set_rotate persists the cadence for restart recovery"
        );
        // Clearing it persists the cleared (NULL) state.
        sched.set_rotate(&track, ctx, None);
        assert_eq!(
            db.lock().get_attachment("bass", ctx).unwrap().unwrap().rotate_every_phrases,
            None,
            "rotate off persists as NULL"
        );
    }

    /// Stage 2 increment 2: attaching creates the track's SCORE CONTEXT — a real,
    /// app-viewable context (so it has a `contexts` row + a document) that is a
    /// NON-producer (never armed for a turn, not attached, not in the reverse
    /// index). A "restart" (a fresh scheduler over the same DB) reuses the persisted
    /// score context rather than minting a second one.
    #[tokio::test]
    async fn attach_mints_a_viewable_nonproducer_score_context_reused_on_restart() {
        let (kernel, documents, db, ctx) = db_backed_kernel_and_docs().await;
        let track = TrackId::new("bass").unwrap();

        let mut sched = BeatScheduler::new(kernel.clone(), documents.clone());
        sched
            .attach(track.clone(), ctx, Attachment::musician_default(), BeatPolicy::musician_default())
            .unwrap();
        let score = sched.tracks.get(&track).expect("track exists").score_context;

        // A real, viewable context: it has a `contexts` row (so `kj context list` /
        // the app can see it), tagged `context_type = "score"`.
        let row = db.lock().get_context(score).unwrap().expect("score context has a row");
        assert_eq!(row.context_type, "score", "score context is tagged for the app to recognize");
        assert_eq!(
            row.context_state,
            kaijutsu_types::ContextState::Live,
            "score context is live (not archived)"
        );

        // It is NOT a producer: no timeline is armed for it, it isn't bound to a
        // track, and it never appears in the reverse index.
        assert!(kernel.timeline(score).is_none(), "score context never gets a (producer) timeline");
        assert!(sched.track_of(score).is_none(), "score context is not itself attached to a track");
        assert_ne!(score, ctx, "the score context is distinct from the producing context");

        // Restart: a fresh scheduler over the same DB recovers the SAME score context
        // from the persisted `tracks` row — it does not mint a second one.
        let mut sched2 = BeatScheduler::new(kernel.clone(), documents.clone());
        sched2
            .attach(track.clone(), ctx, Attachment::musician_default(), BeatPolicy::musician_default())
            .unwrap();
        assert_eq!(
            sched2.tracks.get(&track).expect("track exists").score_context,
            score,
            "restart reuses the persisted score context, not a fresh mint"
        );
    }

    /// Stage 2 WI 7b: on a (re-)arm the track's committed log is rehydrated from its
    /// score context's materialized ABC blocks, so `UseLastGood` survives a restart —
    /// AND the materialize cursor starts PAST the rehydrated cells so they are never
    /// re-emitted (no duplicate score, no DuplicateBlock).
    #[tokio::test]
    async fn attach_rehydrates_committed_from_persisted_score() {
        use kaijutsu_crdt::{BlockId, BlockKind, BlockSnapshotBuilder, Role};

        let (kernel, documents, db, ctx) = db_backed_kernel_and_docs().await;
        let track = TrackId::new("bass").unwrap();

        // A pre-existing score context carrying one committed ABC phrase at tick 5 (a
        // prior session's materialized score) plus a non-ABC sibling that must be
        // ignored. The persisted `tracks` row points the track at it (restart state).
        let score = ContextId::new();
        documents.create_document(score, DocumentKind::Conversation, None).unwrap();
        let player = PrincipalId::new();
        let seq = documents.reserve_block_id(score, player).unwrap().seq;
        let abc = BlockSnapshotBuilder::new(BlockId::new(score, player, seq), BlockKind::Text)
            .role(Role::Model)
            .content("X:1\nK:C\nCDEF|\n")
            .content_type(ContentType::Abc)
            .tick(Tick::new(5))
            .track(track.clone())
            .build();
        documents.insert_from_snapshot_as(score, abc, None, Some(player)).unwrap();
        let seq2 = documents.reserve_block_id(score, player).unwrap().seq;
        let midi = BlockSnapshotBuilder::new(BlockId::new(score, player, seq2), BlockKind::Text)
            .role(Role::Asset)
            .content("00112233445566778899aabbccddeeff")
            .content_type(ContentType::Plain)
            .tick(Tick::new(5))
            .track(track.clone())
            .build();
        documents.insert_from_snapshot_as(score, midi, None, Some(player)).unwrap();
        db.lock()
            .upsert_track(&PersistedTrack {
                track_id: "bass".to_string(),
                period_ms: 500,
                beats_per_phrase: 16,
                playhead_tick: Some(5),
                playing: false,
                score_context_id: Some(score),
                clock_kind: "system".to_string(),
            })
            .unwrap();

        let mut sched = BeatScheduler::new(kernel.clone(), documents.clone());
        sched.attach(track.clone(), ctx, slow_attachment(), slow_policy()).unwrap();
        assert_eq!(sched.score_context(&track), score, "recovered the persisted score context");

        // The committed log was rehydrated from the ABC source (the MIDI sibling is not
        // a committed cell) — UseLastGood now has a view across the restart.
        let tl = kernel.track_timeline(&track).unwrap();
        assert_eq!(
            tl.lock().committed().len(),
            1,
            "the prior ABC phrase rehydrated into the committed log; the MIDI sibling is excluded"
        );

        // The cursor starts past it: a beat must NOT re-materialize the rehydrated cell
        // (the score context keeps exactly its one ABC block).
        let base = Instant::now();
        sched.play(&track, base);
        sched.fire_due(base + Duration::from_secs(1));
        let abc_blocks = documents
            .block_snapshots(score)
            .unwrap()
            .into_iter()
            .filter(|b| b.content_type == ContentType::Abc)
            .count();
        assert_eq!(abc_blocks, 1, "the rehydrated cell is not re-materialized (cursor past it)");
    }

    /// Continuity across a rotation page-turn WITHOUT a carry: the TRACK owns the
    /// playhead and persists it in the `tracks` row. A child attaching to a track
    /// whose durable row carries playhead 512 — but which has no committed blocks of
    /// its own — seeds the (re-created) track AND its Timeline from the track's
    /// durable playhead, so musical time continues from 512, not 0. This replaces the
    /// old per-context `beat_state.playhead_tick` carry: the clock stayed on the
    /// track, so "copy the number" became "the track never left."
    #[tokio::test]
    async fn attach_continues_from_persisted_track_playhead_no_carry() {
        let (kernel, documents, db, ctx) = db_backed_kernel_and_docs().await;

        // The track's durable row carries playhead 512 (e.g. left by the parent
        // before the page-turn); the attaching child has no committed blocks.
        db.lock()
            .upsert_track(&PersistedTrack {
                track_id: "bass".to_string(),
                period_ms: 500,
                beats_per_phrase: 16,
                playhead_tick: Some(512),
                playing: false,
                score_context_id: None,
                clock_kind: "system".to_string(),
            })
            .unwrap();
        assert_eq!(
            documents.max_tick(ctx).unwrap(),
            None,
            "the thin child has no committed blocks — without the track row, seed = 0"
        );

        let mut sched = BeatScheduler::new(kernel.clone(), documents);
        sched
            .attach(TrackId::new("bass").unwrap(), ctx, slow_attachment(), slow_policy())
            .unwrap();

        assert_eq!(
            sched.track_playhead(&TrackId::new("bass").unwrap()).unwrap(),
            Tick::new(512),
            "attach continues musical time from the track's persisted playhead, not tick 0"
        );
    }

    /// The persisted track playhead can never *hide* committed musical time: the
    /// track seed is `max(max_tick, persisted track playhead)`, so a lane with real
    /// committed history (a cold-restart re-attach) trusts its block log even when a
    /// stale `tracks` row carries a lower playhead. Guards the `max()` direction (no
    /// carry; the clock is on the track now).
    #[tokio::test]
    async fn committed_max_tick_wins_over_stale_persisted_track_playhead() {
        use kaijutsu_crdt::{BlockId, BlockKind, BlockSnapshotBuilder};

        let (kernel, documents, db, ctx) = db_backed_kernel_and_docs().await;

        // Real committed history up to tick 1000 (what a restart loads).
        let player = PrincipalId::new();
        let big_t = 1000i64;
        for t in [0i64, 500, big_t] {
            let seq = documents.reserve_block_id(ctx, player).unwrap().seq;
            let snap =
                BlockSnapshotBuilder::new(BlockId::new(ctx, player, seq), BlockKind::Text)
                    .tick(Tick::new(t))
                    .order_key(format!("V{:0>11}AAAA", t))
                    .content("c")
                    .build();
            documents.insert_from_snapshot_as(ctx, snap, None, Some(player)).unwrap();
        }
        // A stale persisted track playhead BELOW the committed high-water.
        db.lock()
            .upsert_track(&PersistedTrack {
                track_id: "bass".to_string(),
                period_ms: 500,
                beats_per_phrase: 16,
                playhead_tick: Some(42),
                playing: false,
                score_context_id: None,
                clock_kind: "system".to_string(),
            })
            .unwrap();

        let mut sched = BeatScheduler::new(kernel.clone(), documents);
        sched
            .attach(TrackId::new("bass").unwrap(), ctx, slow_attachment(), slow_policy())
            .unwrap();

        assert_eq!(
            sched.track_playhead(&TrackId::new("bass").unwrap()).unwrap(),
            Tick::new(big_t),
            "the committed max tick wins over a stale lower persisted track playhead"
        );
    }

    /// A DB read failure during `attach` is FATAL, not a seed-from-zero fallback:
    /// seeding a rewound playhead onto a virgin timeline would overwrite committed
    /// history (`DuplicateBlock` cascade). A corrupt `tracks` row (here `period_ms =
    /// 0`, which `get_track` rejects) must fail the attach loudly and leave NO
    /// timeline behind — never silently seed from zero (CLAUDE.md
    /// crash-over-corruption).
    #[tokio::test]
    async fn attach_fails_loud_on_corrupt_track_row_never_seeds_zero() {
        let (kernel, documents, db, ctx) = db_backed_kernel_and_docs().await;

        // Poison the track row: a zero period is corrupt (it would spin the beat
        // scheduler), and `get_track` rejects it on read. Stands in for bit-rot /
        // tampering / a non-Rust writer.
        db.lock()
            .upsert_track(&PersistedTrack {
                track_id: "bass".to_string(),
                period_ms: 0,
                beats_per_phrase: 16,
                playhead_tick: Some(0),
                playing: false,
                score_context_id: None,
                clock_kind: "system".to_string(),
            })
            .unwrap();
        assert!(kernel.timeline(ctx).is_none(), "no timeline before attach");

        let mut sched = BeatScheduler::new(kernel.clone(), documents);
        let result =
            sched.attach(TrackId::new("bass").unwrap(), ctx, slow_attachment(), slow_policy());

        assert!(result.is_err(), "attach must bubble the corrupt-row read error, not seed zero");
        assert!(
            kernel.timeline(ctx).is_none(),
            "a failed attach must NOT seed a (rewound) timeline — crash over corruption"
        );
    }

    /// Rotation never pauses the clock and never rewinds: at the rotate horizon the
    /// scheduler detaches the retiring context synchronously, but the TRACK keeps
    /// beating (the playhead advances right through the page-turn). Replaces the old
    /// "persist-before-stop defer" test — there is no persist-before-stop anymore
    /// (the clock lives on the track and never leaves, so there is no horizon race to
    /// defer; `docs/tracks.md` "the seed/carry logic collapses").
    #[tokio::test]
    async fn rotate_horizon_detaches_context_but_track_keeps_beating() {
        let (kernel, documents) = fresh_kernel_and_docs().await;
        let ctx = ContextId::new();
        documents.create_document(ctx, DocumentKind::Conversation, None).unwrap();
        preseed_markers(&kernel, ctx, 6);

        let mut sched = BeatScheduler::new(kernel.clone(), documents.clone());
        let base = Instant::now();
        let track_id = TrackId::solo();
        // 2-beat phrase, rotate every phrase → beat 2 is the rotate horizon.
        let policy = BeatPolicy { period: Duration::from_secs(1), beats_per_phrase: 2 };
        let attachment = Attachment {
            wakeup: Cadence::new(1_000_000),
            rotate: Some(Cadence::new(1)),
            ooda_armed: true,
            pulse: 0,
        };
        sched.attach(track_id.clone(), ctx, attachment, policy).unwrap();
        sched.play(&track_id, base);

        // Beat 1: mid-phrase, no rotate.
        let b1 = sched.fire_due(base + Duration::from_secs(1));
        assert!(b1.rotate_due.is_empty());

        // Beat 2: the horizon — the context rotates (is detached) this beat.
        let b2 = sched.fire_due(base + Duration::from_secs(2));
        assert_eq!(b2.rotate_due, vec![ctx], "the context hits its rotate horizon");
        assert!(
            sched.track_of(ctx).is_none(),
            "the rotating context is detached synchronously (live-only)"
        );

        // Beat 3: the TRACK kept beating across the page-turn — its playhead is at 3,
        // not paused/rewound — even though no context is attached to produce.
        let b3 = sched.fire_due(base + Duration::from_secs(3));
        assert!(
            b3.fired.is_empty(),
            "no producer is attached during the gap (the child re-binds via rc)"
        );
        assert_eq!(
            sched.track_playhead(&track_id),
            Some(Tick::new(3)),
            "the track's clock never paused across the page-turn (continuity is free)"
        );
    }

    /// Pre-insert `count` beat()-authored blocks at seqs `0..count` carrying ticks
    /// `0..count`, as a restart would load before re-arm. Returns the seqs used.
    /// The materialize barrier must mint PAST these on the same beat() lane.
    fn preseed_beat_blocks(
        documents: &SharedBlockStore,
        ctx: ContextId,
        count: u64,
    ) -> Vec<u64> {
        use kaijutsu_crdt::{BlockId, BlockKind, BlockSnapshotBuilder};
        let mut seqs = Vec::new();
        for i in 0..count {
            let seq = documents.reserve_block_id(ctx, PrincipalId::beat()).unwrap().seq;
            let snap = BlockSnapshotBuilder::new(
                BlockId::new(ctx, PrincipalId::beat(), seq),
                BlockKind::Text,
            )
            .tick(Tick::new(i as i64))
            .order_key(format!("V{:0>11}AAAA", i))
            .content(format!("persisted-beat-{i}"))
            .build();
            documents.insert_from_snapshot_as(ctx, snap, None, Some(PrincipalId::beat())).unwrap();
            seqs.push(seq);
        }
        seqs
    }

    /// T18 (design-chameleon-batch1-f2-notation §16, §11.1) — the post-restart
    /// seq-lane regression, end-to-end through the scheduler. A restart loads
    /// persisted beat()-authored blocks at seqs 0..=2 but evaporates the
    /// MaterializeCursor (it counts this-process materialization only; the durable
    /// cursor is the block-log seq lane). A FRESH scheduler then arms, plays, and
    /// commits a beat cell. Materialization must mint the new block PAST the
    /// persisted seqs (≥ 3) off the beat() lane — never re-mint seq 0 →
    /// `DuplicateBlock` → the known silent poison-retry loop. The lane seed is
    /// `observe_seq` on the snapshot inserts (F1's structural fix); this pins it
    /// holds through the real materialize barrier, not just at the store level.
    #[tokio::test]
    async fn track_lane_seq_seeds_from_block_log_no_duplicate_after_rearm() {
        use kaijutsu_crdt::BlockKind;

        let (kernel, documents) = fresh_kernel_and_docs().await;
        let ctx = ContextId::new();
        documents.create_document(ctx, DocumentKind::Conversation, None).unwrap();

        // Arm the track timeline with a marker (commit-margin-0), then attach to learn
        // the SCORE context — the score lives there now, so its beat() seq lane is what
        // materialization mints from.
        let track = TrackId::solo();
        arm_track_with_markers(&kernel, &track, 1);
        let mut sched = BeatScheduler::new(kernel.clone(), documents.clone());
        let base = Instant::now();
        sched.attach(track.clone(), ctx, slow_attachment(), slow_policy()).unwrap();
        let score = sched.score_context(&track);

        // Persisted beat() blocks at seqs 0,1,2 in the SCORE context (a restart's
        // loaded history on the lane materialization will mint onto).
        let seeded = preseed_beat_blocks(&documents, score, 3);
        assert_eq!(seeded, vec![0, 1, 2], "preseeded the score beat() lane 0..=2");

        sched.play(&track, base);
        sched.fire_due(base + Duration::from_secs(1));

        // No Error block: a DuplicateBlock collision would surface (or wedge) here.
        let snaps = documents.block_snapshots(score).unwrap();
        let errors = snaps.iter().filter(|b| b.kind == BlockKind::Error).count();
        assert_eq!(errors, 0, "materialization after re-arm must not error (no DuplicateBlock)");

        // The materialized marker landed on the beat() lane PAST the persisted
        // seqs — never re-minting seq 0/1/2.
        let materialized = snaps
            .iter()
            .find(|b| b.content == "beat-1")
            .expect("the marker cell materialized");
        assert_eq!(
            materialized.id.principal_id,
            PrincipalId::beat(),
            "the transport marker is authored by beat()"
        );
        assert!(
            materialized.id.seq >= 3,
            "the post-restart materialize must mint past the persisted beat() seqs (0..=2), \
             got seq {} — a seq ≤ 2 is the DuplicateBlock collision",
            materialized.id.seq,
        );
    }

    /// Pre-arm a context's timeline with `count` always-failing cells at ticks
    /// `1..=count`. Each will record a `FailureEvent` in the engine ledger when the
    /// playhead reaches its tick — the data source the scheduler drains into Error
    /// blocks (§6, §8).
    fn preseed_failing_cells(kernel: &Kernel, _ctx: ContextId, count: i64) {
        // Schedule onto the TRACK timeline (the score lives there now); a following
        // `attach` keeps this commit-margin-0 clock (arm is idempotent).
        let tl = kernel.arm_track_timeline(
            TrackId::solo(),
            TickClock { ticks_per_sec: 1.0, safety_factor: 1.0, commit_margin: TickDelta::new(0) },
            Tick::ZERO,
        );
        let mut g = tl.lock();
        g.register_resolver(Box::new(AlwaysFails));
        for t in 1..=count {
            g.schedule(Cell::deferred_on(
                Span::instant(Tick::new(t)),
                Recipe {
                    resolver: ResolverId::new("always_fails"),
                    params: serde_json::Value::Null,
                    query: ContextQuery::default(),
                    fallback: Fallback::Skip,
                },
                TrackId::solo(),
                PrincipalId::beat(),
            ))
            .unwrap();
        }
    }

    /// T21 (design-chameleon-batch1-f2-notation §16, §6, §8) — a resolve failure
    /// surfaces a visible `BlockKind::Error` block. The engine records a
    /// `FailureEvent` per erring resolve; the scheduler drains the ledger past a
    /// `failure_water` cursor each `process_one`, inserting exactly ONE Error block
    /// per event — so the player reads its own failure next turn (the producer-loop
    /// principle), and never a duplicate on later beats. Two failing cells →
    /// exactly two Error blocks across the run; advancing further adds none.
    #[tokio::test]
    async fn resolve_failure_surfaces_error_block() {
        use kaijutsu_crdt::BlockKind;

        let (kernel, documents) = fresh_kernel_and_docs().await;
        let ctx = ContextId::new();
        documents.create_document(ctx, DocumentKind::Conversation, None).unwrap();
        // An anchor block so the Error blocks (which parent to the document tail)
        // have something to attach to.
        let player = PrincipalId::new();
        insert_player_abc(&documents, ctx, player, "X:1\nK:C\nCDEF|\n");

        // Two cells that will fail at ticks 1 and 2.
        preseed_failing_cells(&kernel, ctx, 2);

        let mut sched = BeatScheduler::new(kernel.clone(), documents.clone());
        let base = Instant::now();
        sched.attach(TrackId::solo(), ctx, slow_attachment(), slow_policy()).unwrap();
        sched.play(&TrackId::solo(), base);

        let errors = |documents: &SharedBlockStore| -> Vec<String> {
            documents
                .block_snapshots(ctx)
                .unwrap()
                .into_iter()
                .filter(|b| b.kind == BlockKind::Error)
                .map(|b| b.content)
                .collect()
        };

        // Beat 1: the tick-1 cell fails → one Error block drained.
        sched.fire_due(base + Duration::from_secs(1));
        assert_eq!(errors(&documents).len(), 1, "first failure surfaces one Error block");

        // Beat 2: the tick-2 cell fails → a second, distinct Error block.
        sched.fire_due(base + Duration::from_secs(2));
        assert_eq!(errors(&documents).len(), 2, "second failure surfaces a second Error block");

        // Later beats with no new failures must NOT re-drain old events: the
        // failure_water cursor is monotone, so no duplicates.
        for i in 3..=6 {
            sched.fire_due(base + Duration::from_secs(i));
        }
        assert_eq!(
            errors(&documents).len(),
            2,
            "failure_water advances past drained events — no duplicate Error blocks"
        );
    }

    /// Stage 2 concurrent producers: two contexts share ONE track timeline, each
    /// producing under its own principal. When each one's cell fails, the shared
    /// failure ledger routes the Error block to the PRODUCING context's conversation
    /// (matched by `played_by`) — a player reads its OWN failure, never a sibling's.
    /// This is the one real new mechanism behind the "concurrent producers are free"
    /// claim (the rest is the existing invariants).
    #[tokio::test]
    async fn two_producers_failures_route_to_their_own_conversations() {
        use kaijutsu_crdt::BlockKind;

        let (kernel, documents) = fresh_kernel_and_docs().await;
        let a = ContextId::new();
        let b = ContextId::new();
        documents.create_document(a, DocumentKind::Conversation, None).unwrap();
        documents.create_document(b, DocumentKind::Conversation, None).unwrap();
        // Each producer plays under its own principal; anchor blocks give the Error
        // blocks a document tail to attach to.
        let pa = PrincipalId::new();
        let pb = PrincipalId::new();
        insert_player_abc(&documents, a, pa, "X:1\nK:C\nCDEF|\n");
        insert_player_abc(&documents, b, pb, "X:1\nK:C\nCDEF|\n");

        // Two failing cells on the shared TRACK timeline: one authored by pa (tick 1),
        // one by pb (tick 2).
        let track = TrackId::solo();
        let tl = kernel.arm_track_timeline(
            track.clone(),
            TickClock { ticks_per_sec: 1.0, safety_factor: 1.0, commit_margin: TickDelta::new(0) },
            Tick::ZERO,
        );
        {
            let mut g = tl.lock();
            g.register_resolver(Box::new(AlwaysFails));
            for (t, played_by) in [(1i64, pa), (2i64, pb)] {
                g.schedule(Cell::deferred_on(
                    Span::instant(Tick::new(t)),
                    Recipe {
                        resolver: ResolverId::new("always_fails"),
                        params: serde_json::Value::Null,
                        query: ContextQuery::default(),
                        fallback: Fallback::Skip,
                    },
                    track.clone(),
                    played_by,
                ))
                .unwrap();
            }
        }

        let mut sched = BeatScheduler::new(kernel.clone(), documents.clone());
        let base = Instant::now();
        sched.attach(track.clone(), a, slow_attachment(), slow_policy()).unwrap();
        sched.attach(track.clone(), b, slow_attachment(), slow_policy()).unwrap();
        // Record each context's producing principal (normally set on its first Act,
        // in `on_turn_completed`). Same-module access to the private field.
        sched.tracks.get_mut(&track).unwrap().attached.get_mut(&a).unwrap().producer_principal =
            Some(pa);
        sched.tracks.get_mut(&track).unwrap().attached.get_mut(&b).unwrap().producer_principal =
            Some(pb);
        sched.play(&track, base);

        let errors = |ctx: ContextId| -> usize {
            documents
                .block_snapshots(ctx)
                .unwrap()
                .into_iter()
                .filter(|b| b.kind == BlockKind::Error)
                .count()
        };

        sched.fire_due(base + Duration::from_secs(1)); // pa's cell fails
        sched.fire_due(base + Duration::from_secs(2)); // pb's cell fails

        assert_eq!(errors(a), 1, "producer a's failure surfaces in a's conversation");
        assert_eq!(errors(b), 1, "producer b's failure surfaces in b's conversation — not a's");
    }

    /// A resolver that COMMITS garbage bytes under the ABC mime — bypassing the
    /// cas_commit validator — so the cell commits clean but its derivation at the
    /// write barrier fails every beat (unparseable ABC violates the first-commit
    /// invariant, §5c). The persistent materialize wedge the poison budget bounds.
    struct GarbageAbc;
    impl Resolver for GarbageAbc {
        fn id(&self) -> ResolverId {
            ResolverId::new("garbage_abc")
        }
        fn estimate_cost(&self, _p: &serde_json::Value, _c: &dyn ResolverCtx) -> Duration {
            Duration::ZERO
        }
        fn compute_basis(&self, _p: &serde_json::Value, _c: &dyn ResolverCtx) -> ContextHash {
            ContextHash::of(b"stable")
        }
        fn resolve(
            &self,
            _p: &serde_json::Value,
            _c: &dyn ResolverCtx,
        ) -> Result<Resolution, ResolveError> {
            Ok(Resolution::new(b"this is not abc {{{ ][".to_vec(), "text/vnd.abc"))
        }
    }

    /// Design §6 / issues.md poison-cell entry (remaining clause) — when a poison
    /// cell exhausts `MATERIALIZE_RETRY_BUDGET`, the bounded skip must surface a
    /// visible `BlockKind::Error` block, not just drop the cell silently from the
    /// operator's view after the error logs. A committed-but-underivable ABC cell
    /// wedges materialization every beat; once the budget is spent the scheduler
    /// skips it AND emits exactly one Error block (deduped — the skip happens once).
    #[tokio::test]
    async fn poison_cell_skip_surfaces_error_block() {
        use kaijutsu_crdt::BlockKind;

        let (kernel, documents) = fresh_kernel_and_docs().await;
        let ctx = ContextId::new();
        documents.create_document(ctx, DocumentKind::Conversation, None).unwrap();
        // Anchor block for the Error block (parents to the document tail).
        let player = PrincipalId::new();
        insert_player_abc(&documents, ctx, player, "X:1\nK:C\nCDEF|\n");

        // One cell at tick 1 that COMMITS clean but can never DERIVE (garbage ABC).
        // Scheduled onto the TRACK timeline (the score lives there); the later
        // `attach` keeps this commit-margin-0 clock (arm is idempotent).
        let tl = kernel.arm_track_timeline(
            TrackId::solo(),
            TickClock { ticks_per_sec: 1.0, safety_factor: 1.0, commit_margin: TickDelta::new(0) },
            Tick::ZERO,
        );
        {
            let mut g = tl.lock();
            g.register_resolver(Box::new(GarbageAbc));
            g.schedule(Cell::deferred_on(
                Span::instant(Tick::new(1)),
                Recipe {
                    resolver: ResolverId::new("garbage_abc"),
                    params: serde_json::Value::Null,
                    query: ContextQuery::default(),
                    fallback: Fallback::Skip,
                },
                TrackId::solo(),
                PrincipalId::beat(),
            ))
            .unwrap();
        }

        let mut sched = BeatScheduler::new(kernel.clone(), documents.clone());
        let base = Instant::now();
        sched.attach(TrackId::solo(), ctx, slow_attachment(), slow_policy()).unwrap();
        sched.play(&TrackId::solo(), base);

        let errors = |documents: &SharedBlockStore| -> usize {
            documents
                .block_snapshots(ctx)
                .unwrap()
                .into_iter()
                .filter(|b| b.kind == BlockKind::Error)
                .count()
        };

        // Beats 1..MATERIALIZE_RETRY_BUDGET wedge: the cell commits at tick 1 and
        // fails to derive every beat (Retry, no Error block yet).
        for i in 1..MATERIALIZE_RETRY_BUDGET as u64 {
            sched.fire_due(base + Duration::from_secs(i));
            assert_eq!(
                errors(&documents),
                0,
                "no poison Error block while still within the retry budget (beat {i})"
            );
        }

        // The budget-th failure trips SkipPoison → exactly one Error block.
        sched.fire_due(base + Duration::from_secs(MATERIALIZE_RETRY_BUDGET as u64));
        assert_eq!(
            errors(&documents),
            1,
            "budget exhaustion surfaces exactly one poison-skip Error block"
        );

        // The cell was skipped (high_water advanced past it): later beats add no
        // duplicate poison Error block.
        for i in (MATERIALIZE_RETRY_BUDGET as u64 + 1)..(MATERIALIZE_RETRY_BUDGET as u64 + 5) {
            sched.fire_due(base + Duration::from_secs(i));
        }
        assert_eq!(
            errors(&documents),
            1,
            "the skipped poison cell surfaces ONE Error block, not one per later beat"
        );
    }

    /// Stage 3 WI 4: the `at` arithmetic in isolation. A cell starting `n` beats
    /// ahead of the playhead renders `n × period` after the jitter-free base; a
    /// cell at or behind the playhead clamps to the base (never the past).
    #[test]
    fn render_instant_offsets_by_lead_and_clamps_at_zero() {
        let base = Instant::now();
        let period = Duration::from_millis(500);
        // 3 beats ahead → base + 1500 ms.
        assert_eq!(
            render_instant(base, period, Tick::new(8), Tick::new(5)),
            base + Duration::from_millis(1500)
        );
        // Exactly at the playhead → base (offset 0).
        assert_eq!(render_instant(base, period, Tick::new(5), Tick::new(5)), base);
        // Behind the playhead (shouldn't happen for a fresh commit) → clamped to base.
        assert_eq!(render_instant(base, period, Tick::new(3), Tick::new(5)), base);
    }

    /// T19 (design §8 Phase 5) — the feedback-loop guard. `on_turn_completed`
    /// must NOT re-schedule a block that came off the timeline (`track.is_some()`)
    /// or a legacy transport block (author == beat()) — that would loop the
    /// bridge's own output back through the OODA Act. A genuine player-ABC block
    /// (no track, real author) IS scheduled, with that author as `played_by` and
    /// the musician's lane as `track`.
    #[tokio::test]
    async fn on_turn_completed_skips_materialized_blocks() {
        use kaijutsu_crdt::{BlockKind, ContentType, Role, Status};

        // — Case A: a track-bearing block (what materialization stamps) is refused.
        //   Materialized blocks live in the score context now, so we place the guard's
        //   target — a track-bearing block — directly in the producer's own doc.
        use kaijutsu_crdt::BlockSnapshotBuilder;
        let (kernel, documents) = fresh_kernel_and_docs().await;
        let ctx = ContextId::new();
        documents.create_document(ctx, DocumentKind::Conversation, None).unwrap();
        let mut sched = BeatScheduler::new(kernel.clone(), documents.clone());
        let base = Instant::now();
        sched.attach(TrackId::solo(), ctx, slow_attachment(), slow_policy()).unwrap();
        let score = sched.score_context(&TrackId::solo());
        let player_a = PrincipalId::new();
        let seq_a = documents.reserve_block_id(ctx, player_a).unwrap().seq;
        let track_bearing = kaijutsu_crdt::BlockId::new(ctx, player_a, seq_a);
        let snap_a = BlockSnapshotBuilder::new(track_bearing, BlockKind::Text)
            .role(Role::Model)
            .content("X:1\nK:C\nCDEF|\n")
            .content_type(ContentType::Abc)
            .track(TrackId::solo())
            .build();
        documents.insert_from_snapshot_as(ctx, snap_a, None, Some(player_a)).unwrap();
        sched.play(&TrackId::solo(), base);

        // on_turn_completed carrying the track-bearing block schedules NOTHING — no
        // Asset (MIDI) ever appears in the score (the guard refuses it).
        sched.on_turn_completed(ctx, Some(track_bearing));
        for i in 1..=8 {
            sched.fire_due(base + Duration::from_secs(i));
        }
        let assets = documents
            .block_snapshots(score)
            .unwrap()
            .into_iter()
            .filter(|b| b.role == Role::Asset)
            .count();
        assert_eq!(
            assets, 0,
            "a track-bearing block must not be re-scheduled as ABC (no abc→midi asset)"
        );

        // — Case B: last block is a beat()-authored legacy block (no track) —
        let (kernel_b, docs_b) = fresh_kernel_and_docs().await;
        let cb = ContextId::new();
        docs_b.create_document(cb, DocumentKind::Conversation, None).unwrap();
        docs_b
            .insert_block_as(
                cb,
                None,
                None,
                Role::Model,
                BlockKind::Text,
                "X:1\nK:C\nCDEF|\n".to_string(),
                Status::Done,
                ContentType::Plain,
                Some(PrincipalId::beat()), // legacy transport author
            )
            .unwrap();
        let mut sched_b = BeatScheduler::new(kernel_b.clone(), docs_b.clone());
        let base_b = Instant::now();
        sched_b.attach(TrackId::solo(), cb, slow_attachment(), slow_policy()).unwrap();
        let score_b = sched_b.score_context(&TrackId::solo());
        sched_b.play(&TrackId::solo(), base_b);
        sched_b.fire_due(base_b + Duration::from_secs(1));
        sched_b.on_turn_completed(cb, docs_b.last_block_id(cb));
        for i in 2..=8 {
            sched_b.fire_due(base_b + Duration::from_secs(i));
        }
        let assets_b = docs_b
            .block_snapshots(score_b)
            .unwrap()
            .into_iter()
            .filter(|b| b.role == Role::Asset)
            .count();
        assert_eq!(
            assets_b, 0,
            "a beat()-authored legacy block must not be re-scheduled as ABC"
        );

        // — Case C: a genuine player-ABC block (no track, real author) IS
        // scheduled, materializing on the musician's lane under the player —
        let (kernel_c, docs_c) = fresh_kernel_and_docs().await;
        let cc = ContextId::new();
        docs_c.create_document(cc, DocumentKind::Conversation, None).unwrap();
        let player = PrincipalId::new();
        docs_c
            .insert_block_as(
                cc,
                None,
                None,
                Role::Model,
                BlockKind::Text,
                "X:1\nT:Test\nM:4/4\nL:1/8\nK:C\nCDEFGABc|\n".to_string(),
                Status::Done,
                ContentType::Plain,
                Some(player),
            )
            .unwrap();
        let mut sched_c = BeatScheduler::new(kernel_c.clone(), docs_c.clone());
        let base_c = Instant::now();
        // Short 4-beat phrase so the scheduling lead (= phrase_delta()) is small
        // enough to commit within this 8-beat window; large ooda so the cadence
        // never coincides.
        sched_c
            .attach(
                TrackId::solo(),
                cc,
                slow_attachment(),
                BeatPolicy { period: Duration::from_secs(1), beats_per_phrase: 4 },
            )
            .unwrap();
        let score_c = sched_c.score_context(&TrackId::solo());
        sched_c.play(&TrackId::solo(), base_c);
        sched_c.fire_due(base_c + Duration::from_secs(1));
        sched_c.on_turn_completed(cc, docs_c.last_block_id(cc));
        for i in 2..=8 {
            sched_c.fire_due(base_c + Duration::from_secs(i));
        }
        let midi = docs_c
            .block_snapshots(score_c)
            .unwrap()
            .into_iter()
            .find(|b| b.role == Role::Asset);
        let midi = midi.expect("a player-ABC block IS scheduled and materializes a MIDI asset");
        assert_eq!(midi.track, Some(TrackId::solo()), "scheduled on the musician's lane");
        assert_eq!(
            midi.id.principal_id, player,
            "played_by is the ABC block's author (the player), threaded into the cell"
        );
    }

    // ── Stage-1 track-model behaviours (the gemini-review TDD targets) ──────────

    /// One beat that wakes TWO contexts on one track gives them the IDENTICAL
    /// `KJ_EPOCH_NS` (the cross-context join key — latched ONCE per beat, not
    /// re-read per context) and the same `KJ_TICK`. Pins the epoch-isolation finding.
    #[tokio::test]
    async fn one_beat_shares_epoch_and_tick_across_woken_contexts() {
        let (kernel, documents) = fresh_kernel_and_docs().await;
        let a = ContextId::new();
        let b = ContextId::new();
        documents.create_document(a, DocumentKind::Conversation, None).unwrap();
        documents.create_document(b, DocumentKind::Conversation, None).unwrap();

        let mut sched = BeatScheduler::new(kernel.clone(), documents.clone());
        let base = Instant::now();
        let track_id = TrackId::solo();
        // Both wake every beat (wakeup 1), OODA armed.
        let att = Attachment { wakeup: Cadence::new(1), rotate: None, ooda_armed: true, pulse: 0 };
        sched.attach(track_id.clone(), a, att, slow_policy()).unwrap();
        sched.attach(track_id.clone(), b, att, slow_policy()).unwrap();
        sched.play(&track_id, base);

        let out = sched.fire_due(base + Duration::from_secs(1));
        assert!(
            out.ooda_due.contains(&a) && out.ooda_due.contains(&b),
            "both contexts woke on beat 1"
        );
        let env_a = sched.transport_env(a);
        let env_b = sched.transport_env(b);
        assert_eq!(
            env_a.get("KJ_EPOCH_NS"),
            env_b.get("KJ_EPOCH_NS"),
            "both see the beat's single latched wall-clock epoch"
        );
        assert!(
            env_a.get("KJ_EPOCH_NS").is_some_and(|e| e != "0"),
            "epoch is latched to a real instant"
        );
        assert_eq!(env_a.get("KJ_TICK"), env_b.get("KJ_TICK"), "same musical tick");
    }

    /// `KJ_PULSE` counts each attachment's OWN wakeups, not the track's beats: a
    /// probe (wakeup 1) and a musician (wakeup 2) on one track diverge — after 4
    /// beats the probe pulsed 4 times, the musician 2. Pins the pulse-isolation
    /// finding (a sibling waking on other beats can't skip another's sequence).
    #[tokio::test]
    async fn pulse_is_per_attachment_not_per_track_beat() {
        let (kernel, documents) = fresh_kernel_and_docs().await;
        let probe = ContextId::new();
        let musician = ContextId::new();
        documents.create_document(probe, DocumentKind::Conversation, None).unwrap();
        documents.create_document(musician, DocumentKind::Conversation, None).unwrap();

        let mut sched = BeatScheduler::new(kernel.clone(), documents.clone());
        let base = Instant::now();
        let track_id = TrackId::solo();
        let probe_att =
            Attachment { wakeup: Cadence::new(1), rotate: None, ooda_armed: true, pulse: 0 };
        let musician_att =
            Attachment { wakeup: Cadence::new(2), rotate: None, ooda_armed: true, pulse: 0 };
        sched.attach(track_id.clone(), probe, probe_att, slow_policy()).unwrap();
        sched.attach(track_id.clone(), musician, musician_att, slow_policy()).unwrap();
        sched.play(&track_id, base);

        for i in 1..=4 {
            sched.fire_due(base + Duration::from_secs(i));
        }
        // Probe woke on beats 1,2,3,4 → pulse 4. Musician woke on 2,4 → pulse 2.
        assert_eq!(sched.transport_env(probe).get("KJ_PULSE").unwrap(), "4");
        assert_eq!(sched.transport_env(musician).get("KJ_PULSE").unwrap(), "2");
        // On beat 4 BOTH woke → they share that beat's epoch + tick.
        assert_eq!(
            sched.transport_env(probe).get("KJ_EPOCH_NS"),
            sched.transport_env(musician).get("KJ_EPOCH_NS"),
        );
        assert_eq!(sched.transport_env(probe).get("KJ_TICK").unwrap(), "4");
    }

    /// A FRESH context attached to an already-running track is seeded at the track's
    /// CURRENT playhead, so its next beat is exactly ONE step — not a giant catch-up
    /// from zero through a window it was never alive for. Pins the seed-gap finding
    /// (the "rotation swaps the binding with zero discontinuity" target).
    #[tokio::test]
    async fn fresh_context_attaches_at_track_playhead_no_catchup() {
        let (kernel, documents) = fresh_kernel_and_docs().await;
        let a = ContextId::new();
        documents.create_document(a, DocumentKind::Conversation, None).unwrap();

        let mut sched = BeatScheduler::new(kernel.clone(), documents.clone());
        let base = Instant::now();
        let track_id = TrackId::solo();
        sched.attach(track_id.clone(), a, slow_attachment(), slow_policy()).unwrap();
        sched.play(&track_id, base);
        for i in 1..=5 {
            sched.fire_due(base + Duration::from_secs(i));
        }
        assert_eq!(sched.track_playhead(&track_id), Some(Tick::new(5)));

        // A FRESH context b (no pre-existing timeline) attaches to the running track.
        let b = ContextId::new();
        documents.create_document(b, DocumentKind::Conversation, None).unwrap();
        sched.attach(track_id.clone(), b, slow_attachment(), slow_policy()).unwrap();
        assert_eq!(
            sched.track_playhead(&track_id).unwrap(),
            Tick::new(5),
            "fresh context seeds at the track's current playhead, not zero"
        );

        // One more beat: b's Timeline advances exactly one step (5→6), no catch-up.
        sched.fire_due(base + Duration::from_secs(6));
        assert_eq!(
            sched.track_playhead(&track_id).unwrap(),
            Tick::new(6),
            "the first beat after attach is one step, not a catch-up from zero"
        );
    }

    /// `stop` halts ONE track's clock and leaves a sibling track beating — two
    /// independent clock domains (the myaku "metrics keep sampling while music
    /// pauses" property is just two tracks).
    #[tokio::test]
    async fn stop_halts_one_track_leaves_sibling_running() {
        let (kernel, documents) = fresh_kernel_and_docs().await;
        let a = ContextId::new();
        let b = ContextId::new();
        documents.create_document(a, DocumentKind::Conversation, None).unwrap();
        documents.create_document(b, DocumentKind::Conversation, None).unwrap();

        let mut sched = BeatScheduler::new(kernel.clone(), documents.clone());
        let base = Instant::now();
        let solo = TrackId::solo();
        let bass = TrackId::new("bass").unwrap();
        arm_track_with_markers(&kernel, &solo, 5);
        arm_track_with_markers(&kernel, &bass, 5);
        sched.attach(solo.clone(), a, slow_attachment(), slow_policy()).unwrap();
        sched.attach(bass.clone(), b, slow_attachment(), slow_policy()).unwrap();
        let score_a = sched.score_context(&solo);
        let score_b = sched.score_context(&bass);
        sched.play(&solo, base);
        sched.play(&bass, base);

        sched.fire_due(base + Duration::from_secs(1));
        assert_eq!(contents(&documents, score_a).len(), 1);
        assert_eq!(contents(&documents, score_b).len(), 1);

        // Stop only the solo track; the bass track is a separate clock domain.
        sched.stop(&solo);
        let out = sched.fire_due(base + Duration::from_secs(2));
        assert_eq!(out.fired, vec![b], "only the bass track still beats");
        assert_eq!(contents(&documents, score_a).len(), 1, "solo frozen");
        assert_eq!(contents(&documents, score_b).len(), 2, "bass kept beating");
    }

    /// `stop` is clock-only: it does NOT disarm a context's OODA. After stop+play the
    /// context fires its `tick` again with no manual re-arm (MIDI transport idiom;
    /// the OLD per-context `Stop` cleared OODA, the track `Stop` does not).
    #[tokio::test]
    async fn stop_is_clock_only_does_not_disarm_ooda() {
        let (kernel, documents) = fresh_kernel_and_docs().await;
        let ctx = ContextId::new();
        documents.create_document(ctx, DocumentKind::Conversation, None).unwrap();

        let mut sched = BeatScheduler::new(kernel.clone(), documents.clone());
        let base = Instant::now();
        let track_id = TrackId::solo();
        let att = Attachment { wakeup: Cadence::new(1), rotate: None, ooda_armed: true, pulse: 0 };
        sched.attach(track_id.clone(), ctx, att, slow_policy()).unwrap();
        sched.play(&track_id, base);

        let out1 = sched.fire_due(base + Duration::from_secs(1));
        assert_eq!(out1.ooda_due, vec![ctx], "OODA fires while playing");

        // Stop, then play again within one period — OODA must survive (clock-only stop),
        // no manual re-arm, and the generation token keeps it to a single beat.
        sched.stop(&track_id);
        sched.play(&track_id, base + Duration::from_secs(1));
        let out2 = sched.fire_due(base + Duration::from_secs(2));
        assert_eq!(
            out2.ooda_due,
            vec![ctx],
            "stop did not disarm OODA — it fires again after play with no re-arm"
        );
    }

    /// `stop` (or `pause`) then `play` WITHIN one beat period beats exactly ONCE: the
    /// generation token invalidates the pre-stop heap entry (re-pushed by the last
    /// beat) so it doesn't process alongside `play`'s fresh entry — the double-beat the
    /// old lazy-drop allowed. Without the token both entries are due at `+2` and the
    /// track would beat twice.
    #[tokio::test]
    async fn stop_then_play_within_one_period_beats_once() {
        let (kernel, documents) = fresh_kernel_and_docs().await;
        let ctx = ContextId::new();
        documents.create_document(ctx, DocumentKind::Conversation, None).unwrap();

        let mut sched = BeatScheduler::new(kernel.clone(), documents.clone());
        let base = Instant::now();
        let track_id = TrackId::solo();
        arm_track_with_markers(&kernel, &track_id, 10);
        sched.attach(track_id.clone(), ctx, slow_attachment(), slow_policy()).unwrap();
        let score = sched.score_context(&track_id);
        sched.play(&track_id, base);
        sched.fire_due(base + Duration::from_secs(1)); // beat 1 → re-pushes at +2 (gen 1)
        assert_eq!(contents(&documents, score).len(), 1);

        // stop + play WITHIN the same period, before the re-pushed (gen 1) entry pops.
        sched.stop(&track_id);
        sched.play(&track_id, base + Duration::from_secs(1)); // gen 2, pushes at +2

        // At +2 BOTH the stale gen-1 re-push and the fresh gen-2 entry are due; only the
        // current generation processes.
        let out = sched.fire_due(base + Duration::from_secs(2));
        assert_eq!(out.fired, vec![ctx], "exactly one beat, not two (generation token)");
        assert_eq!(contents(&documents, score).len(), 2, "playhead advanced once: +beat-2");
    }

    /// A context whose committed history is AHEAD of an already-running (stale) track
    /// pulls the track's playhead UP to its frontier on attach — so the forward-only
    /// slew guard never freezes a producing context behind a lagging track (the
    /// restart "thin attaches first, thick attaches later" wedge gemini flagged).
    #[tokio::test]
    async fn thick_context_attach_bumps_a_lagging_track_playhead() {
        use kaijutsu_crdt::{BlockId, BlockKind, BlockSnapshotBuilder};
        let (kernel, documents) = fresh_kernel_and_docs().await;
        let thin = ContextId::new();
        documents.create_document(thin, DocumentKind::Conversation, None).unwrap();

        let mut sched = BeatScheduler::new(kernel.clone(), documents.clone());
        let base = Instant::now();
        let track_id = TrackId::solo();
        // A thin context (no committed blocks) creates the track and runs it to tick 5.
        sched.attach(track_id.clone(), thin, slow_attachment(), slow_policy()).unwrap();
        sched.play(&track_id, base);
        for i in 1..=5 {
            sched.fire_due(base + Duration::from_secs(i));
        }
        assert_eq!(sched.track_playhead(&track_id), Some(Tick::new(5)));

        // A thick context with committed history up to tick 20 attaches.
        let thick = ContextId::new();
        documents.create_document(thick, DocumentKind::Conversation, None).unwrap();
        let player = PrincipalId::new();
        let seq = documents.reserve_block_id(thick, player).unwrap().seq;
        let snap = BlockSnapshotBuilder::new(BlockId::new(thick, player, seq), BlockKind::Text)
            .tick(Tick::new(20))
            .order_key(format!("V{:0>11}AAAA", 20))
            .content("c")
            .build();
        documents.insert_from_snapshot_as(thick, snap, None, Some(player)).unwrap();
        assert_eq!(documents.max_tick(thick).unwrap(), Some(Tick::new(20)));

        sched.attach(track_id.clone(), thick, slow_attachment(), slow_policy()).unwrap();
        assert_eq!(
            sched.track_playhead(&track_id),
            Some(Tick::new(20)),
            "the track is pulled up to the joining context's committed frontier"
        );
        assert_eq!(
            kernel.track_timeline(&track_id).unwrap().lock().playhead(),
            Tick::new(20),
            "the thick context's Timeline is at its frontier, not frozen behind the track"
        );
    }

    /// `detach` persists the track's CURRENT playhead — the rotate-horizon handoff is
    /// the durable record a 0-block child inherits across a crash. The beat path does
    /// NOT persist every beat, so without the detach-persist a crash in the rotation
    /// gap would rewind the lane on the child's re-attach (gemini review).
    #[tokio::test]
    async fn detach_persists_the_track_playhead_for_crash_safe_handoff() {
        let (kernel, documents, db, ctx) = db_backed_kernel_and_docs().await;
        let mut sched = BeatScheduler::new(kernel, documents);
        let track = TrackId::new("bass").unwrap();
        let base = Instant::now();
        sched.attach(track.clone(), ctx, slow_attachment(), slow_policy()).unwrap();
        sched.play(&track, base);
        for i in 1..=4 {
            sched.fire_due(base + Duration::from_secs(i));
        }
        // The beat path didn't persist: the row still reads the attach/play playhead.
        assert_eq!(
            db.lock().get_track("bass").unwrap().unwrap().playhead_tick,
            Some(0),
            "the beat path does not persist every beat (by design)"
        );
        // Detach (the rotation handoff) snapshots the live playhead durably.
        sched.detach(&track, ctx);
        assert_eq!(
            db.lock().get_track("bass").unwrap().unwrap().playhead_tick,
            Some(4),
            "detach persists the handoff playhead so a 0-block child inherits it across a crash"
        );
    }
}
