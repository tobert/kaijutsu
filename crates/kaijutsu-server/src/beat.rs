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
use kaijutsu_kernel::flows::TurnFlow;
use kaijutsu_kernel::hyoushigi::{
    Attachment, BeatAck, BeatCommand, BeatPolicy, BeatRequest, Cadence, DeriverRegistry,
    MaterializeCursor, materialize_committed, schedule_abc_cell,
};
use kaijutsu_kernel::kernel_db::{ContextRow, PersistedAttachment, PersistedTrack};
use kaijutsu_kernel::{Kernel, KjCaller, KjDispatcher};
use kaijutsu_types::{
    BlockSnapshot, ConsentMode, ContentType, ContextId, ContextState, DocKind, PrincipalId,
    SessionId, Tick, TickDelta, TrackId, now_millis,
};

use crate::rpc::ServerRegistry;

/// Ticks the playhead advances per beat (PPQ 1: one tick per beat). The tick is
/// event-counted, so this is a pure increment, never scaled by elapsed time.
const STEP: TickDelta = TickDelta::new(1);

/// A track's beat bookkeeping — the **clock domain** (`docs/tracks.md`, Stage 1).
/// The clock + playhead live HERE now, not on any one context's timeline: the
/// track persists, contexts attach to be beaten by it and come and go. Keyed by
/// [`TrackId`] in the scheduler's `tracks` map.
struct TrackState {
    /// The track's musical clock: beat period (tempo) + phrase length. Track-level
    /// — the per-context wakeup cadence rides each [`AttachedContext`].
    policy: BeatPolicy,
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
    /// timeline. (Materialize/HEARD re-point onto it in increment 3.)
    score_context: ContextId,
    /// The binding set: which contexts are attached and how each rides the beat.
    /// The track holds this passive view; the *context* drives the bind (entity #3)
    /// and a forked child re-binds on the way up.
    attached: HashMap<ContextId, AttachedContext>,
}

/// One context's binding to a track plus its **runtime** materialization state.
/// The [`Attachment`] is the durable/wire binding contract (wakeup/rotate/
/// ooda_armed/pulse, persisted in the `attachments` row); the cursor + failure
/// counters below are scheduler-only bookkeeping, per-context because each
/// attached context materializes into its *own* timeline (the Stage-1 bridge).
struct AttachedContext {
    /// The binding the context announced: its wakeup divisor, rotate cadence, OODA
    /// arm, and monotonic pulse counter. Travels with a fork.
    attachment: Attachment,
    /// The materialization cursor for THIS context: how far whole committed cells
    /// have crossed the write barrier (`high_water`) AND how far the in-progress
    /// cell's artifact group got (`artifacts_done`), so a beat that fails mid-group
    /// resumes at the failed artifact rather than re-inserting (and colliding on)
    /// the ones that already landed. NOT an id source: materialization reserves ids
    /// from the store's per-principal seq lanes (§3).
    cursor: MaterializeCursor,
    /// Consecutive materialize failures on the SAME poison cell — the cell at this
    /// context's current `high_water` that has refused to materialize on every
    /// retry. Reset to 0 the moment any cell crosses (progress was made). When it
    /// reaches [`MATERIALIZE_RETRY_BUDGET`] the bridge skips the poison cell with a
    /// loud `error!` so the beat loop never silently retries the same cell forever.
    materialize_failures: u32,
    /// How far this context's engine failure ledger (`Timeline::failures()`) has
    /// been drained into visible `BlockKind::Error` blocks (design §6, §8).
    /// Monotone: each beat surfaces every event past this cursor, one Error block
    /// apiece, so a resolve failure is read back by the player next turn (the
    /// producer-loop principle) and never re-surfaced on later beats.
    failure_water: usize,
}

impl AttachedContext {
    /// A freshly-bound context: the announced attachment + zeroed runtime state.
    fn new(attachment: Attachment) -> Self {
        Self {
            attachment,
            cursor: MaterializeCursor::default(),
            materialize_failures: 0,
            failure_water: 0,
        }
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

    /// A track's current playhead, if the track has a live clock domain. Used by
    /// tests to observe musical position.
    #[cfg(test)]
    fn track_playhead(&self, track_id: &TrackId) -> Option<Tick> {
        self.tracks.get(track_id).map(|t| t.playhead)
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
            let score_context = self.ensure_score_context(&track_id, persisted_score)?;
            self.tracks.insert(
                track_id.clone(),
                TrackState {
                    policy,
                    playing: false,
                    generation: 0,
                    playhead: seed,
                    beat_count: 0,
                    last_epoch_ns: 0,
                    score_context,
                    attached: HashMap::new(),
                },
            );
        }
        // Seed the context's per-context Timeline at `max(its own committed tick, the
        // track's current playhead)` — see the doc above for why both terms.
        let track_playhead = self.tracks[&track_id].playhead;
        let clock = self.tracks[&track_id].policy.clock();
        let timeline_seed = ctx_from_log.max(track_playhead);
        self.kernel.arm_timeline(context_id, clock, timeline_seed);
        // Register/update the binding. A re-attach of a live binding keeps its
        // runtime materialization state (cursor/failures) and refreshes the
        // announced attachment.
        let track = self.tracks.get_mut(&track_id).expect("track exists");
        // A context whose committed history sits AHEAD of the track (a restart where
        // the persisted track playhead lagged the lane's blocks) pulls the track up to
        // that frontier — otherwise the forward-only slew guard in `materialize_one`
        // would freeze this context's Timeline for the gap while its OODA kept firing
        // (gemini review 2026-06-29). `timeline_seed >= track.playhead` already, so
        // this only fires when `ctx_from_log` genuinely exceeds the track.
        if timeline_seed > track.playhead {
            track.playhead = timeline_seed;
        }
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
            period_ms: track.policy.period.as_millis() as u64,
            beats_per_phrase: track.policy.beats_per_phrase,
            playhead_tick: Some(track.playhead.get()),
            playing: track.playing,
            score_context_id: Some(track.score_context),
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
        let period = track.policy.period;
        self.heap.push(Reverse((now + period, track_id.clone(), generation)));
        let _ = self.persist_track(track_id);
    }

    /// Hold a track's clock — the playhead freezes. The stale heap entry is dropped
    /// on its next pop. Per-attachment OODA arm + rotate cadence are preserved.
    pub fn pause(&mut self, track_id: &TrackId) {
        if let Some(track) = self.tracks.get_mut(track_id) {
            track.playing = false;
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
        if let Some(track) = self.tracks.get_mut(track_id) {
            track.playing = false;
        }
        let _ = self.persist_track(track_id);
    }

    /// Set a track's beat period (tempo). Takes effect on the next beat.
    pub fn set_tempo(&mut self, track_id: &TrackId, period: Duration) {
        if let Some(track) = self.tracks.get_mut(track_id) {
            track.policy.period = period;
        }
        // Persist the new tempo so a restart recovers it (not the default).
        let _ = self.persist_track(track_id);
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
    /// attached set + the reverse index and drops its kernel timeline, but does NOT
    /// delete the persisted `attachments` row: the rotate page-turn relies on the
    /// child inheriting the parent's binding via the fork-copy of that row (automatic
    /// in `insert_forked_context`), so deleting it here would break inheritance.
    /// Persisted-row cleanup belongs to context archival (not wired in Stage 1). The
    /// track persists with its remaining attachments (folds the old `Disarm`).
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
        // Drop the kernel timeline + the reverse index ONLY if this context is no
        // longer attached to ANY track. The 1:1 invariant means this is normally the
        // last attachment, but guard defensively: a context that still rides another
        // lane must keep its timeline (else that lane's `materialize_one` would
        // silently no-op) and a reverse-index entry pointing at a surviving track.
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
                self.kernel.disarm_timeline(context_id);
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
                .map(|tr| (tr.playing, tr.policy.period, tr.generation))
            else {
                continue; // unknown/removed track → drop the stale entry
            };
            if !playing {
                continue; // stopped/paused → drop the stale entry (no re-arm)
            }
            if generation != cur_gen {
                continue; // a later `play` re-enlisted the track → this entry is stale
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
            (track.beat_count, track.playhead, track.policy, ids)
        };
        // Phase 2: per attached context — materialize, then answer the cadence. No
        // &mut tracks borrow is held across `materialize_one` (it re-borrows briefly).
        let mut to_detach: Vec<ContextId> = Vec::new();
        for ctx in attached_ids {
            self.materialize_one(track_id, ctx, playhead);
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

    /// Materialize one attached context's newly-committed cells this beat: slew its
    /// Timeline up to the track playhead (forward-only), run the CAS+block bridge with
    /// the context's OWN cursor/poison budget, surface a poison-skip Error block, and
    /// drain the engine failure ledger. All per-context state lives on the
    /// [`AttachedContext`], so one context's poison cell never wedges a sibling.
    fn materialize_one(&mut self, track_id: &TrackId, ctx: ContextId, track_playhead: Tick) {
        let Some(timeline) = self.kernel.timeline(ctx) else {
            // Attached but no timeline — shouldn't happen (attach pairs them), but
            // never panic the driver.
            return;
        };
        // Slew the context's Timeline to the track playhead (the Stage-1 bridge).
        // Guarded forward-only: a context whose own committed log sits AHEAD of the
        // track is left alone rather than advanced backward (`advance_to` panics on
        // backward time — the write-barrier no-backdating stance).
        {
            let mut g = timeline.lock();
            if track_playhead > g.playhead() {
                g.advance_to(track_playhead);
            }
        }
        let cas = self.kernel.cas().clone();
        // Copy the cursor out so the &mut borrow of the attachment doesn't collide
        // with the shared &self.documents/&self.derivers reads in the bridge.
        let Some(mut cursor) = self
            .tracks
            .get(track_id)
            .and_then(|t| t.attached.get(&ctx))
            .map(|ac| ac.cursor)
        else {
            return;
        };
        let hw_before = cursor.high_water;
        let result = materialize_committed(
            &timeline,
            &cas,
            &self.documents,
            ctx,
            &mut cursor,
            &self.derivers,
        );
        // Set when the SAME cell exhausts MATERIALIZE_RETRY_BUDGET and is skipped:
        // (skipped high_water index, last error). Surfaced as ONE Error block after
        // the attachment borrow ends — deduped by construction (skip happens once).
        let mut poison_skipped: Option<(usize, String)> = None;
        match result {
            Ok(_) => {
                if let Some(ac) =
                    self.tracks.get_mut(track_id).and_then(|t| t.attached.get_mut(&ctx))
                {
                    ac.cursor = cursor;
                    ac.materialize_failures = 0; // a clean beat clears the poison count
                }
            }
            Err(e) => {
                if let Some(ac) =
                    self.tracks.get_mut(track_id).and_then(|t| t.attached.get_mut(&ctx))
                {
                    if cursor.high_water > hw_before {
                        // Whole-cell progress → the poison is a FRESH cell; persist
                        // and reset the count.
                        ac.cursor = cursor;
                        ac.materialize_failures = 0;
                        log::error!(
                            "beat: materialize failed for context {ctx} after crossing \
                             {} cell(s) this beat; will retry the next cell: {e}",
                            cursor.high_water - hw_before
                        );
                    } else {
                        // Same cell failed again — escalate; once the budget is spent,
                        // skip the poison cell so the loop never silently retries.
                        ac.cursor = cursor;
                        ac.materialize_failures += 1;
                        let failures = ac.materialize_failures;
                        log::error!(
                            "beat: materialize failed for context {ctx} on cell at \
                             high_water={} (consecutive failure #{failures} on the \
                             same cell): {e}",
                            cursor.high_water
                        );
                        if poison_action(failures) == PoisonAction::SkipPoison {
                            ac.cursor.high_water += 1;
                            ac.cursor.artifacts_done = 0;
                            ac.cursor.source_block_id = None;
                            ac.materialize_failures = 0;
                            log::error!(
                                "beat: skipping poison cell at high_water={} for context \
                                 {ctx} after {MATERIALIZE_RETRY_BUDGET} failed attempts — \
                                 cell will NOT be materialized",
                                cursor.high_water
                            );
                            poison_skipped = Some((cursor.high_water, e.to_string()));
                        }
                    }
                } else {
                    log::error!("beat: materialize failed for un-attached context {ctx}: {e}");
                }
            }
        }

        // Surface the poison-skip as a visible Error block now (the attachment borrow
        // has ended). Anchored at the document tail like the resolve-failure drain.
        if let Some((skipped_hw, err)) = poison_skipped {
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
            match self.documents.last_block_id(ctx) {
                Some(anchor) => {
                    if let Err(e) = self.documents.insert_error_block_as(
                        ctx,
                        &anchor,
                        &payload,
                        summary,
                        Some(PrincipalId::beat()),
                    ) {
                        log::error!(
                            "beat: failed to surface poison-skip Error block for {ctx} \
                             (skipped cell at high_water={skipped_hw}): {e}"
                        );
                    }
                }
                None => log::error!(
                    "beat: poison-skip for {ctx} at high_water={skipped_hw} found no anchor block"
                ),
            }
        }

        // Drain the engine failure ledger into visible Error blocks (§6, §8) so the
        // player reads its own resolve failures next turn (the producer-loop principle).
        self.drain_failures(track_id, ctx, &timeline);
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
    fn drain_failures(
        &mut self,
        track_id: &TrackId,
        ctx: ContextId,
        timeline: &kaijutsu_kernel::hyoushigi::SharedTimeline,
    ) {
        // Snapshot the new events under the lock, then release it before touching
        // the block store (no nested lock; no `.await` was ever here anyway).
        let new_events: Vec<(kaijutsu_types::Tick, kaijutsu_types::Tick, String, String)> = {
            let g = timeline.lock();
            let failures = g.failures();
            let Some(st) = self.tracks.get(track_id).and_then(|t| t.attached.get(&ctx)) else {
                return;
            };
            if failures.len() <= st.failure_water {
                return; // nothing new since the last drain
            }
            failures[st.failure_water..]
                .iter()
                .map(|ev| {
                    (ev.at, ev.start, ev.resolver.as_str().to_string(), ev.error.clone())
                })
                .collect()
        };

        for (at, start, resolver, error) in &new_events {
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
            // Anchor at the current document tail. A musician context always has at
            // least its rc/stance blocks, so a tail is expected; its absence is a
            // structural anomaly we surface loudly rather than silently skip.
            let Some(anchor) = self.documents.last_block_id(ctx) else {
                log::error!(
                    "beat: failure ledger drain for {ctx} found no anchor block for the \
                     resolve failure at beat {} (resolver {resolver}): {error}",
                    start.get()
                );
                continue;
            };
            if let Err(e) = self.documents.insert_error_block_as(
                ctx,
                &anchor,
                &payload,
                summary,
                Some(PrincipalId::beat()),
            ) {
                log::error!(
                    "beat: failed to surface resolve-failure Error block for {ctx} \
                     (failure at playhead {}, start {}): {e}",
                    at.get(),
                    start.get()
                );
            }
        }

        // Advance the water past every event observed this beat — even any whose
        // Error-block insert errored above (already logged loudly). Re-surfacing a
        // drained event next beat would spam; the ledger length is the cursor.
        if let Some(st) = self.tracks.get_mut(track_id).and_then(|t| t.attached.get_mut(&ctx)) {
            st.failure_water += new_events.len();
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
            transport_vars(playhead, track.beat_count, &track.policy)
                .into_iter()
                .collect();
        // KJ_HEARD: the recent committed notation the player composes against. A read
        // failure degrades to an empty list (compose from the chart + now-facts).
        let window = TickDelta::new((HEARD_WINDOW_PHRASES * track.policy.beats_per_phrase) as i64);
        let heard = self
            .documents
            .block_snapshots(ctx)
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
    fn on_turn_completed(&self, ctx: ContextId, output_block_id: Option<BlockId>) {
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
            track.policy.phrase_delta()
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
        }
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
                    Some(BeatRequest { command, reply }) => {
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
    use kaijutsu_kernel::flows::{FlowBus, SharedBlockFlowBus};
    use kaijutsu_kernel::kernel_db::{PersistedAttachment, PersistedTrack};
    use kaijutsu_kernel::hyoushigi::{Attachment, BeatPolicy, Cadence};
    use kaijutsu_types::{ContextId, PrincipalId, TrackId};
    use tokio::time::Instant;

    use super::BeatScheduler;
    use super::{
        MATERIALIZE_RETRY_BUDGET, PoisonAction, heard_json, poison_action, transport_vars,
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
        // musician_default: 500 ms/beat (= 120 BPM), 16 beats/phrase.
        let m = vars_map(transport_vars(Tick::new(128), 128, &BeatPolicy::musician_default()));
        assert_eq!(m["KJ_TICK"], "128", "playhead tick verbatim");
        assert_eq!(m["KJ_PHRASE"], "8", "128 beats / 16 per phrase = phrase 8");
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

    /// Playing drives one ticked block per beat in tick order; attaching alone
    /// produces nothing (track created stopped); detach halts.
    #[tokio::test]
    async fn play_produces_ticked_blocks_in_order_arm_is_stopped() {
        let (kernel, documents) = fresh_kernel_and_docs().await;
        let ctx = ContextId::new();
        documents.create_document(ctx, DocumentKind::Conversation, None).unwrap();
        preseed_markers(&kernel, ctx, 5);

        let mut sched = BeatScheduler::new(kernel.clone(), documents.clone());
        let base = Instant::now();
        let track_id = TrackId::solo();

        // Attached but not played: track created stopped → no heap entry, no blocks.
        sched.attach(track_id.clone(), ctx, slow_attachment(), slow_policy()).unwrap();
        assert!(sched.next_wake().is_none(), "attach alone schedules nothing (track stopped)");
        let out = sched.fire_due(base + Duration::from_secs(1));
        assert!(out.fired.is_empty());
        assert_eq!(contents(&documents, ctx).len(), 0);

        // Play: now it beats.
        sched.play(&track_id, base);
        for i in 1..=5 {
            let out = sched.fire_due(base + Duration::from_secs(i));
            assert_eq!(out.fired, vec![ctx], "beat {i}");
            assert_eq!(contents(&documents, ctx).len(), i as usize);
        }
        assert_eq!(
            contents(&documents, ctx),
            vec!["beat-1", "beat-2", "beat-3", "beat-4", "beat-5"],
        );

        // Detach: the context is removed; the track clock keeps running but fires
        // no contexts. This is `disarm` in the old API — now explicit with track.
        sched.detach(&track_id, ctx);
        let out = sched.fire_due(base + Duration::from_secs(6));
        assert!(out.fired.is_empty(), "detached → context no longer fires");
    }

    /// Pause freezes the track playhead; resume picks up at +1 with no wall-clock
    /// catch-up, even after a long quiescent gap.
    #[tokio::test]
    async fn pause_freezes_and_resume_continues_at_plus_one() {
        let (kernel, documents) = fresh_kernel_and_docs().await;
        let ctx = ContextId::new();
        documents.create_document(ctx, DocumentKind::Conversation, None).unwrap();
        preseed_markers(&kernel, ctx, 5);

        let mut sched = BeatScheduler::new(kernel.clone(), documents.clone());
        let base = Instant::now();
        let track_id = TrackId::solo();
        sched.attach(track_id.clone(), ctx, slow_attachment(), slow_policy()).unwrap();
        sched.play(&track_id, base);

        sched.fire_due(base + Duration::from_secs(1)); // tick 1
        sched.fire_due(base + Duration::from_secs(2)); // tick 2
        assert_eq!(contents(&documents, ctx), vec!["beat-1", "beat-2"]);

        // Pause; the stale heap entry fires once and is dropped — no advance.
        sched.pause(&track_id);
        sched.fire_due(base + Duration::from_secs(3));
        assert_eq!(contents(&documents, ctx).len(), 2, "paused → frozen");

        // Resume much later in wall-clock; the tick continues at 3, not jumping.
        sched.play(&track_id, base + Duration::from_secs(3600));
        sched.fire_due(base + Duration::from_secs(3601));
        assert_eq!(
            contents(&documents, ctx),
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
        preseed_markers(&kernel, a, 5);
        preseed_markers(&kernel, b, 5);

        let mut sched = BeatScheduler::new(kernel.clone(), documents.clone());
        let base = Instant::now();
        let track_id = TrackId::solo();
        sched.attach(track_id.clone(), a, slow_attachment(), slow_policy()).unwrap();
        sched.play(&track_id, base);

        sched.fire_due(base + Duration::from_secs(1));
        sched.fire_due(base + Duration::from_secs(2));
        assert_eq!(contents(&documents, a).len(), 2);
        assert_eq!(contents(&documents, b).len(), 0, "b not attached yet");

        // Attach b to the already-playing track. b's timeline was pre-seeded at
        // Tick::ZERO by preseed_markers (arm_timeline is idempotent), so it stays
        // at 0 and catches up when the track next fires at beat 3.
        sched.attach(track_id.clone(), b, slow_attachment(), slow_policy()).unwrap();
        // `play` is a no-op — the track is already running.
        sched.play(&track_id, base + Duration::from_secs(2));
        // Beat 3: both a and b fire. b's timeline slews from 0→3 (catch-up),
        // materializing beat-1, beat-2, beat-3.
        sched.fire_due(base + Duration::from_secs(3));
        assert_eq!(contents(&documents, a).len(), 3);
        assert!(
            !contents(&documents, b).is_empty(),
            "b fires on its first beat (attached to a running track)"
        );
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
        sched.play(&track_id, base);

        sched.fire_due(base + Duration::from_secs(1)); // playhead → 1
        // The cell is scheduled at playhead(1) + phrase_delta()(4) = tick 5. The
        // one-phrase-ahead start is observable on the materialized block's tick
        // (asserted below) — `phrase_delta()` (4 here) not the deleted 4-const.
        sched.on_turn_completed(ctx, Some(abc_block));

        let asset_before = documents
            .block_snapshots(ctx)
            .unwrap()
            .iter()
            .filter(|b| b.role == Role::Asset)
            .count();
        assert_eq!(asset_before, 0, "no MIDI block before the scheduled cell commits");

        // Advance past tick 5 so the cell commits and materializes the pair.
        for i in 2..=8 {
            sched.fire_due(base + Duration::from_secs(i));
        }

        let snaps = documents.block_snapshots(ctx).unwrap();
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

        // Helper: count scheduled future cells.
        fn future_len(kernel: &Kernel, ctx: ContextId) -> usize {
            kernel.timeline(ctx).unwrap().lock().future_len()
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

        // — Case materialized: a track-bearing block is refused —
        {
            let (kernel, documents) = fresh_kernel_and_docs().await;
            let ctx = ContextId::new();
            documents.create_document(ctx, DocumentKind::Conversation, None).unwrap();
            preseed_markers(&kernel, ctx, 1);
            let mut sched = BeatScheduler::new(kernel.clone(), documents.clone());
            let base = Instant::now();
            sched.attach(TrackId::solo(), ctx, slow_attachment(), phrase).unwrap();
            sched.play(&TrackId::solo(), base);
            sched.fire_due(base + Duration::from_secs(1)); // marker materializes (track=Some)
            let materialized = documents.last_block_id(ctx).unwrap();
            assert!(
                documents.get_block_snapshot(ctx, &materialized).unwrap().unwrap().track.is_some(),
                "the marker carries a track"
            );
            let future_before = future_len(&kernel, ctx);
            sched.on_turn_completed(ctx, Some(materialized));
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
        let playhead_after_arm = kernel.timeline(ctx).unwrap().lock().playhead();
        assert_eq!(
            playhead_after_arm,
            Tick::new(big_t),
            "arm seeds the playhead from max_tick (musical time stays monotone)"
        );

        // First beat advances from the seeded position: T → T+1.
        let base = Instant::now();
        sched.play(&TrackId::solo(), base);
        sched.fire_due(base + Duration::from_secs(1));
        let playhead_after_beat = kernel.timeline(ctx).unwrap().lock().playhead();
        assert_eq!(
            playhead_after_beat,
            Tick::new(big_t + 1),
            "the first beat advances from the seeded playhead, not from zero"
        );

        // Re-arm of the now-LIVE timeline must NOT re-seed or rewind: the
        // playhead stays where the beat left it (seed_playhead is virgin-only,
        // applied inside or_insert_with which never fires on a re-arm).
        sched.attach(TrackId::solo(), ctx, slow_attachment(), slow_policy()).unwrap();
        let playhead_after_rearm = kernel.timeline(ctx).unwrap().lock().playhead();
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
            kernel.timeline(ctx).unwrap().lock().playhead(),
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
            })
            .unwrap();

        let mut sched = BeatScheduler::new(kernel.clone(), documents);
        sched
            .attach(TrackId::new("bass").unwrap(), ctx, slow_attachment(), slow_policy())
            .unwrap();

        assert_eq!(
            kernel.timeline(ctx).unwrap().lock().playhead(),
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

        // Persisted beat() blocks at seqs 0,1,2 (a restart's loaded history).
        let seeded = preseed_beat_blocks(&documents, ctx, 3);
        assert_eq!(seeded, vec![0, 1, 2], "preseeded the beat() lane 0..=2");

        // A FRESH scheduler (cursor born at high_water=0) arms and materializes a
        // marker cell — which plays under beat(), the same lane as the persisted
        // blocks. preseed_markers wins (arm is idempotent) and seeds the timeline.
        preseed_markers(&kernel, ctx, 1);
        let mut sched = BeatScheduler::new(kernel.clone(), documents.clone());
        let base = Instant::now();
        sched.attach(TrackId::solo(), ctx, slow_attachment(), slow_policy()).unwrap();
        sched.play(&TrackId::solo(), base);
        sched.fire_due(base + Duration::from_secs(1));

        // No Error block: a DuplicateBlock collision would surface (or wedge) here.
        let snaps = documents.block_snapshots(ctx).unwrap();
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
    fn preseed_failing_cells(kernel: &Kernel, ctx: ContextId, count: i64) {
        let tl = kernel.arm_timeline(
            ctx,
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
        let tl = kernel.arm_timeline(
            ctx,
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

    /// T19 (design §8 Phase 5) — the feedback-loop guard. `on_turn_completed`
    /// must NOT re-schedule a block that came off the timeline (`track.is_some()`)
    /// or a legacy transport block (author == beat()) — that would loop the
    /// bridge's own output back through the OODA Act. A genuine player-ABC block
    /// (no track, real author) IS scheduled, with that author as `played_by` and
    /// the musician's lane as `track`.
    #[tokio::test]
    async fn on_turn_completed_skips_materialized_blocks() {
        use kaijutsu_crdt::{BlockKind, ContentType, Role, Status};

        // — Case A: last block already carries a track (came off the timeline) —
        let (kernel, documents) = fresh_kernel_and_docs().await;
        let ctx = ContextId::new();
        documents.create_document(ctx, DocumentKind::Conversation, None).unwrap();
        // Materialize one cell so the last block carries track=Some (the shim
        // cell plays under beat() on the solo() lane).
        preseed_markers(&kernel, ctx, 1);
        let mut sched = BeatScheduler::new(kernel.clone(), documents.clone());
        let base = Instant::now();
        sched.attach(TrackId::solo(), ctx, slow_attachment(), slow_policy()).unwrap();
        sched.play(&TrackId::solo(), base);
        sched.fire_due(base + Duration::from_secs(1)); // marker materializes (track=Some)
        let last = documents
            .get_block_snapshot(ctx, &documents.last_block_id(ctx).unwrap())
            .unwrap()
            .unwrap();
        assert!(last.track.is_some(), "the materialized marker carries a track");

        // on_turn_completed carrying the track-bearing block's own id schedules
        // NOTHING — no new Asset (MIDI) block ever appears, even after a lead.
        // (The carried id is the materialized marker; the guard refuses it.)
        sched.on_turn_completed(ctx, documents.last_block_id(ctx));
        for i in 2..=8 {
            sched.fire_due(base + Duration::from_secs(i));
        }
        let assets = documents
            .block_snapshots(ctx)
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
        sched_b.play(&TrackId::solo(), base_b);
        sched_b.fire_due(base_b + Duration::from_secs(1));
        sched_b.on_turn_completed(cb, docs_b.last_block_id(cb));
        for i in 2..=8 {
            sched_b.fire_due(base_b + Duration::from_secs(i));
        }
        let assets_b = docs_b
            .block_snapshots(cb)
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
        sched_c.play(&TrackId::solo(), base_c);
        sched_c.fire_due(base_c + Duration::from_secs(1));
        sched_c.on_turn_completed(cc, docs_c.last_block_id(cc));
        for i in 2..=8 {
            sched_c.fire_due(base_c + Duration::from_secs(i));
        }
        let midi = docs_c
            .block_snapshots(cc)
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
            kernel.timeline(b).unwrap().lock().playhead(),
            Tick::new(5),
            "fresh context seeds at the track's current playhead, not zero"
        );

        // One more beat: b's Timeline advances exactly one step (5→6), no catch-up.
        sched.fire_due(base + Duration::from_secs(6));
        assert_eq!(
            kernel.timeline(b).unwrap().lock().playhead(),
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
        preseed_markers(&kernel, a, 5);
        preseed_markers(&kernel, b, 5);

        let mut sched = BeatScheduler::new(kernel.clone(), documents.clone());
        let base = Instant::now();
        let solo = TrackId::solo();
        let bass = TrackId::new("bass").unwrap();
        sched.attach(solo.clone(), a, slow_attachment(), slow_policy()).unwrap();
        sched.attach(bass.clone(), b, slow_attachment(), slow_policy()).unwrap();
        sched.play(&solo, base);
        sched.play(&bass, base);

        sched.fire_due(base + Duration::from_secs(1));
        assert_eq!(contents(&documents, a).len(), 1);
        assert_eq!(contents(&documents, b).len(), 1);

        // Stop only the solo track; the bass track is a separate clock domain.
        sched.stop(&solo);
        let out = sched.fire_due(base + Duration::from_secs(2));
        assert_eq!(out.fired, vec![b], "only the bass track still beats");
        assert_eq!(contents(&documents, a).len(), 1, "solo frozen");
        assert_eq!(contents(&documents, b).len(), 2, "bass kept beating");
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
        preseed_markers(&kernel, ctx, 10);

        let mut sched = BeatScheduler::new(kernel.clone(), documents.clone());
        let base = Instant::now();
        let track_id = TrackId::solo();
        sched.attach(track_id.clone(), ctx, slow_attachment(), slow_policy()).unwrap();
        sched.play(&track_id, base);
        sched.fire_due(base + Duration::from_secs(1)); // beat 1 → re-pushes at +2 (gen 1)
        assert_eq!(contents(&documents, ctx).len(), 1);

        // stop + play WITHIN the same period, before the re-pushed (gen 1) entry pops.
        sched.stop(&track_id);
        sched.play(&track_id, base + Duration::from_secs(1)); // gen 2, pushes at +2

        // At +2 BOTH the stale gen-1 re-push and the fresh gen-2 entry are due; only the
        // current generation processes.
        let out = sched.fire_due(base + Duration::from_secs(2));
        assert_eq!(out.fired, vec![ctx], "exactly one beat, not two (generation token)");
        assert_eq!(contents(&documents, ctx).len(), 2, "playhead advanced once: +beat-2");
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
            kernel.timeline(thick).unwrap().lock().playhead(),
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
