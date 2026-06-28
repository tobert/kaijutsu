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
    BeatCommand, BeatPolicy, DeriverRegistry, MaterializeCursor, materialize_committed,
    schedule_abc_cell,
};
use kaijutsu_kernel::{Kernel, KjCaller, KjDispatcher};
use kaijutsu_types::{
    BlockSnapshot, ContentType, ContextId, PrincipalId, SessionId, Tick, TickDelta, TrackId,
};

use crate::rpc::ServerRegistry;

/// Ticks the playhead advances per beat (PPQ 1: one tick per beat). The tick is
/// event-counted, so this is a pure increment, never scaled by elapsed time.
const STEP: TickDelta = TickDelta::new(1);

/// Per-armed-context beat bookkeeping. The two transport switches live here.
struct BeatState {
    policy: BeatPolicy,
    /// The clock switch. `false` = stopped/paused: no heap entry (quiescent),
    /// playhead frozen. Default on arm (create stopped — no surprise tokens).
    playing: bool,
    /// The OODA switch. The `tick` verb fires only when `playing && ooda_armed`.
    ooda_armed: bool,
    /// Self-fork rotate cadence, in phrases. `Some(n)` (n>0) = at every phrase
    /// horizon where `phrase % n == 0` the scheduler retires this context
    /// (synchronous `stop`) and fires the `rotate` rc lifecycle — the page-turn
    /// (`docs/chameleon.md`). The horizon decision lives HERE in Rust, not in the
    /// rc, precisely so the parent's disarm is synchronous with the scheduler's
    /// own clock (an rc disarm via `kj transport ooda off` is async through the
    /// ingress → a stray-tick / double-fork race; see `docs/issues.md`). `None` =
    /// no rotation (the default; the rc `fork`/`arm` *action* is still rc).
    rotate_every_phrases: Option<u64>,
    /// The materialization cursor: how far whole committed cells have crossed the
    /// write barrier (`high_water`) AND how far the in-progress cell's artifact
    /// group got (`artifacts_done`), so a beat that fails mid-group resumes at the
    /// failed artifact rather than re-inserting (and colliding on) the ones that
    /// already landed. NOT an id source: materialization reserves ids from the
    /// store's per-principal seq lanes (§3).
    cursor: MaterializeCursor,
    /// Beats elapsed *while playing* — drives the OODA cadence. Frozen across a
    /// pause, like the playhead.
    beat_count: u64,
    /// The musician's lane identity — the track its scheduled cells (and so its
    /// materialized blocks) belong to. Lane identity ONLY, never the author.
    track: TrackId,
    /// Consecutive materialize failures on the SAME poison cell — the cell at the
    /// current `high_water` that has refused to materialize on every retry. Reset
    /// to 0 the moment any cell crosses (progress was made). When it reaches
    /// [`MATERIALIZE_RETRY_BUDGET`] the bridge skips the poison cell with a loud
    /// `error!` so the beat loop never silently retries the same cell forever
    /// (the pre-F1 swallowed-Result poison channel; worsens with multiple tracks).
    materialize_failures: u32,
    /// How far the engine failure ledger (`Timeline::failures()`) has been drained
    /// into visible `BlockKind::Error` blocks (design §6, §8). Monotone: each
    /// `process_one` surfaces every event past this cursor, one Error block apiece,
    /// then advances past them — so a resolve failure is read back by the player
    /// next turn (the producer-loop principle) and never re-surfaced on later beats.
    failure_water: usize,
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
        ("TICK".to_string(), playhead.get().to_string()),
        ("PHRASE".to_string(), phrase.to_string()),
        ("TEMPO".to_string(), bpm.to_string()),
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

/// What one ticked beat crossed: the two cadence questions answered side by side.
/// `process_one` computes both from the post-increment `beat_count` so the phrase
/// boundary lands beside the OODA check at the single sequencer.
#[derive(Debug, Default, Clone, Copy)]
struct BeatTickReport {
    /// Crossed an OODA cadence boundary AND OODA is armed — fire the `tick` verb.
    ooda_due: bool,
    /// Crossed a phrase boundary — report on `BeatOutcome.phrase_due`. NOT gated
    /// on the OODA arm: a phrase boundary is a position in musical time.
    phrase_due: bool,
    /// Hit a rotate horizon (`rotate_every_phrases` set AND this phrase boundary
    /// satisfies `phrase % n == 0`). Drives the synchronous parent-retire +
    /// `rotate` lifecycle in `fire_due`.
    rotate_due: bool,
}

/// The coalescing beat scheduler / transport. Holds the kernel (timelines +
/// CAS), the block store (materialization), and an optional kj dispatcher (to
/// fire the `tick` verb). The dispatcher is optional so the core is unit-testable
/// without a full server.
pub struct BeatScheduler {
    kernel: Arc<Kernel>,
    documents: SharedBlockStore,
    dispatcher: Option<Arc<KjDispatcher>>,
    heap: BinaryHeap<Reverse<(Instant, ContextId)>>,
    armed: HashMap<ContextId, BeatState>,
    /// The write-barrier derivation registry (ABC → MIDI). One per scheduler — the
    /// derivers are stateless and shared across all armed contexts.
    derivers: DeriverRegistry,
}

impl BeatScheduler {
    pub fn new(kernel: Arc<Kernel>, documents: SharedBlockStore) -> Self {
        Self {
            kernel,
            documents,
            dispatcher: None,
            heap: BinaryHeap::new(),
            armed: HashMap::new(),
            derivers: DeriverRegistry::production(),
        }
    }

    /// Attach the kj dispatcher so OODA boundaries fire the `tick` rc verb.
    pub fn with_dispatcher(mut self, dispatcher: Arc<KjDispatcher>) -> Self {
        self.dispatcher = Some(dispatcher);
        self
    }

    /// Arm a context **stopped**: ensure its kernel timeline exists and register
    /// beat state, but don't start the clock (no heap entry until `play`). OODA
    /// starts armed. Idempotent — re-arming updates the policy, preserving the
    /// running state and playhead.
    ///
    /// The playhead is seeded from the document's max committed tick (design §4)
    /// so musical time stays globally monotone per context across restarts even
    /// when conversation inserts advanced the coordinate past the last beat
    /// (chameleon.md:195-198). The seed lands only on a freshly-created timeline
    /// inside `arm_timeline`'s `or_insert_with`, so a re-arm of a live timeline
    /// never re-seeds. A failed `max_tick` scan is loud, not silent: we'd rather
    /// hear it than seed a stale (zero) playhead and mis-place the first beat.
    ///
    /// CONSTRAINT (design §11.5): we seed the playhead tick but **deliberately do
    /// NOT seed the fallback pool** from the persisted block log. The Timeline's
    /// committed Vec — the `UseLastGood` candidate pool — is born empty here, so a
    /// freshly (re)armed track resolves `UseLastGood` to Skip until its first good
    /// phrase. Restart IS an empty track. Seeding the pool from the log would make
    /// the first phrase after a restart a vamped repeat of a phrase the band has
    /// no live memory of playing — a silent stale fallback, the exact failure mode
    /// this project rejects (crash/silence over corruption). Warm-restart vamping,
    /// if ever wanted, is an explicit opt-in feature (issues.md), never the default
    /// that falls out of seeding a pool here.
    pub fn arm(&mut self, context_id: ContextId, policy: BeatPolicy, track: TrackId) {
        let seed = match self.documents.max_tick(context_id) {
            Ok(t) => t.unwrap_or(kaijutsu_types::Tick::ZERO),
            Err(e) => {
                log::error!(
                    "beat: max_tick failed for context {context_id}: {e}; seeding playhead to zero"
                );
                kaijutsu_types::Tick::ZERO
            }
        };
        self.kernel.arm_timeline(context_id, policy.clock(), seed);
        self.armed
            .entry(context_id)
            .and_modify(|s| s.policy = policy)
            .or_insert(BeatState {
                policy,
                playing: false,
                ooda_armed: true,
                rotate_every_phrases: None,
                cursor: MaterializeCursor::default(),
                beat_count: 0,
                track,
                materialize_failures: 0,
                failure_water: 0,
            });
        // Mirror the (possibly new) policy + lane to the DB so a kernel restart can
        // re-arm this musician with its real values via `kj transport arm`.
        self.persist_state(context_id);
    }

    /// Mirror a context's live policy + lane into the `beat_state` table — the
    /// durable copy a `kj transport arm` reads to recover the musician after a
    /// kernel restart (the scheduler's `armed` map starts empty on cold start).
    /// Called on every policy mutation (arm, tempo) so the row never drifts behind
    /// the live `BeatState`.
    ///
    /// A db-less store (embedded / unit test) has nowhere to persist — a clean
    /// no-op. A db-backed store whose write FAILS is loud (`log::error!`), never
    /// silent: the live beat is unaffected, but the restart-recovery copy is now
    /// stale, and a musician silently re-arming to the default tempo after a crash
    /// is exactly the kind of silent fallback this project rejects.
    fn persist_state(&self, context_id: ContextId) {
        let Some(st) = self.armed.get(&context_id) else {
            return;
        };
        let Some(db) = self.documents.db() else {
            return; // db-less store (embedded/test): nothing to persist to.
        };
        let state = kaijutsu_kernel::kernel_db::PersistedBeatState {
            period_ms: st.policy.period.as_millis() as u64,
            beats_per_phrase: st.policy.beats_per_phrase,
            ooda_every: st.policy.ooda_every,
            track: st.track.as_str().to_string(),
        };
        if let Err(e) = db.lock().upsert_beat_state(context_id, &state) {
            log::error!("beat: failed to persist beat state for {context_id}: {e}");
        }
    }

    /// Start (or resume) the clock. Pushes a heap entry one period out; the
    /// playhead resumes at +1 from where it froze (event-counted — no catch-up).
    pub fn play(&mut self, context_id: ContextId, now: Instant) {
        let Some(st) = self.armed.get_mut(&context_id) else {
            log::warn!("beat: play on un-armed context {context_id} ignored");
            return;
        };
        if st.playing {
            return; // already running — don't stack heap entries
        }
        st.playing = true;
        let period = st.policy.period;
        self.heap.push(Reverse((now + period, context_id)));
    }

    /// Hold the clock — the playhead freezes; OODA arm state is preserved. The
    /// stale heap entry is dropped on its next pop (it won't re-arm while paused).
    pub fn pause(&mut self, context_id: ContextId) {
        if let Some(st) = self.armed.get_mut(&context_id) {
            st.playing = false;
        }
    }

    /// Full halt — pause the clock *and* disarm OODA.
    pub fn stop(&mut self, context_id: ContextId) {
        if let Some(st) = self.armed.get_mut(&context_id) {
            st.playing = false;
            st.ooda_armed = false;
        }
    }

    /// Set the beat period (tempo). Takes effect on the next re-arm.
    pub fn set_tempo(&mut self, context_id: ContextId, period: Duration) {
        let updated = match self.armed.get_mut(&context_id) {
            Some(st) => {
                st.policy.period = period;
                true
            }
            None => false,
        };
        // Persist the new tempo so a restart re-arm carries it (not the default).
        // Outside the `get_mut` borrow so `persist_state(&self)` can re-read.
        if updated {
            self.persist_state(context_id);
        }
    }

    /// Arm or disarm the OODA loop without touching the clock.
    pub fn set_ooda(&mut self, context_id: ContextId, armed: bool) {
        if let Some(st) = self.armed.get_mut(&context_id) {
            st.ooda_armed = armed;
        }
    }

    /// Set (or clear, with `None`) the self-fork rotate cadence in phrases. A
    /// `Some(0)` is treated as `None` (no rotation) — never a `% 0`.
    pub fn set_rotate(&mut self, context_id: ContextId, every_phrases: Option<u64>) {
        if let Some(st) = self.armed.get_mut(&context_id) {
            st.rotate_every_phrases = every_phrases.filter(|n| *n > 0);
        }
    }

    /// Disarm a context entirely: drop beat state and the kernel timeline. The
    /// heap entry is skipped lazily on pop.
    pub fn disarm(&mut self, context_id: ContextId) {
        self.armed.remove(&context_id);
        self.kernel.disarm_timeline(context_id);
    }

    /// The next wake instant, if any context is scheduled.
    pub fn next_wake(&self) -> Option<Instant> {
        self.heap.peek().map(|Reverse((t, _))| *t)
    }

    /// Fire every context due at or before `now`: advance its playhead one beat,
    /// bridge new committed cells, count the beat, and re-arm its next beat.
    /// A popped entry for a paused/disarmed context is dropped (not re-armed).
    pub fn fire_due(&mut self, now: Instant) -> BeatOutcome {
        let mut outcome = BeatOutcome::default();
        while let Some(Reverse((t, ctx))) = self.heap.peek().copied() {
            if t > now {
                break;
            }
            self.heap.pop();
            let Some((playing, period)) =
                self.armed.get(&ctx).map(|s| (s.playing, s.policy.period))
            else {
                continue; // disarmed → drop the stale entry
            };
            if !playing {
                continue; // paused/stopped → drop the stale entry (no re-arm)
            }
            let report = self.process_one(ctx);
            outcome.fired.push(ctx);
            // Rotate horizon: retire the parent SYNCHRONOUSLY, right here, in the
            // same step that decided to rotate — before the loop can `select!`
            // again, so no stray tick can fire for it and no second rotate can
            // race in. `stop` (pause clock + disarm OODA) leaves the entry
            // quiescent (zero cost, blocks/timeline preserved for the app). It is
            // NOT re-pushed onto the heap → the parent's beat ends here. The
            // `rotate` rc (fork + arm child) runs fire-and-forget; that async work
            // is safe now that the parent is stopped.
            if report.rotate_due {
                self.stop(ctx);
                outcome.rotate_due.push(ctx);
                continue;
            }
            self.heap.push(Reverse((now + period, ctx)));
            if report.ooda_due {
                outcome.ooda_due.push(ctx);
            }
            if report.phrase_due {
                outcome.phrase_due.push(ctx);
            }
        }
        outcome
    }

    /// Advance one context by one beat (event-counted), flush whatever committed,
    /// count the beat, and report whether it crossed an OODA cadence and/or a
    /// phrase boundary. Called only for a playing context.
    fn process_one(&mut self, ctx: ContextId) -> BeatTickReport {
        let Some(timeline) = self.kernel.timeline(ctx) else {
            // Armed in the scheduler but no timeline in the kernel — shouldn't
            // happen (arming pairs the two), but never panic the driver.
            return BeatTickReport::default();
        };
        // The beat tick: advance the playhead by one step. Lock held only for the
        // advance; the bridge re-locks internally. No `.await` under the lock.
        {
            let mut g = timeline.lock();
            let target = g.playhead() + STEP;
            g.advance_to(target);
        }

        let cas = self.kernel.cas().clone();
        // Take the cursor out so the &mut borrow of `s.cursor` doesn't collide with
        // the shared &self.derivers read. Whole-cell progress = `high_water`
        // advanced; the per-artifact `artifacts_done` makes a mid-group retry
        // resumable but is NOT counted as poison-clearing progress on its own (a
        // cell that always fails on the SAME sibling must still trip the budget).
        let Some(mut cursor) = self.armed.get(&ctx).map(|s| s.cursor) else {
            return BeatTickReport::default();
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
        // (skipped high_water index, last error string). Surfaced as ONE Error
        // block after the borrow of `s` ends — deduped by construction (the skip
        // happens exactly once per poison cell, when the budget trips).
        let mut poison_skipped: Option<(usize, String)> = None;
        match result {
            Ok(_) => {
                if let Some(s) = self.armed.get_mut(&ctx) {
                    s.cursor = cursor;
                    s.materialize_failures = 0; // a clean beat clears the poison count
                }
            }
            Err(e) => {
                // The materialize bridge errored. `materialize_committed` advances
                // `high_water` per crossed cell, so any whole-cell progress means the
                // poison is a *fresh* cell at the new high_water — persist the
                // progress and reset the failure count. Zero whole-cell progress means
                // the SAME cell failed again (possibly after partial-artifact progress
                // captured in `artifacts_done`): escalate loudly and, once the retry
                // budget is spent, skip the poison cell so the beat loop never silently
                // retries forever. (Pre-F1 this was a swallowed `log::warn` with no
                // loop bound — the §7 poison-cell debt; worse with multiple tracks.)
                if let Some(s) = self.armed.get_mut(&ctx) {
                    if cursor.high_water > hw_before {
                        s.cursor = cursor;
                        s.materialize_failures = 0;
                        log::error!(
                            "beat: materialize failed for context {ctx} after crossing \
                             {} cell(s) this beat; will retry the next cell: {e}",
                            cursor.high_water - hw_before
                        );
                    } else {
                        // Persist the partial-artifact resume point even on failure so
                        // a transient fault clears with a resume, not a re-insert.
                        s.cursor = cursor;
                        s.materialize_failures += 1;
                        let failures = s.materialize_failures;
                        log::error!(
                            "beat: materialize failed for context {ctx} on cell at \
                             high_water={} (consecutive failure #{failures} \
                             on the same cell): {e}",
                            cursor.high_water
                        );
                        if poison_action(failures) == PoisonAction::SkipPoison {
                            // Bounded give-up: advance past the poison cell so the
                            // loop terminates. Reset the per-artifact resume point —
                            // we are abandoning this cell, not resuming it. The
                            // operator has already seen MATERIALIZE_RETRY_BUDGET loud
                            // errors for it.
                            s.cursor.high_water += 1;
                            s.cursor.artifacts_done = 0;
                            s.cursor.source_block_id = None;
                            s.materialize_failures = 0;
                            log::error!(
                                "beat: skipping poison cell at high_water={} \
                                 for context {ctx} after {MATERIALIZE_RETRY_BUDGET} \
                                 failed attempts — cell will NOT be materialized",
                                cursor.high_water
                            );
                            // Surface the skip as a visible Error block (closes the
                            // issues.md poison-cell "remaining" clause): a silently
                            // dropped phrase is invisible to the player after the
                            // logs scroll. The skip happens once, so this dedupes
                            // naturally — no per-beat spam.
                            poison_skipped = Some((cursor.high_water, e.to_string()));
                        }
                    }
                } else {
                    log::error!("beat: materialize failed for un-armed context {ctx}: {e}");
                }
            }
        }

        // The borrow of `s` has ended — surface the poison-skip as a visible Error
        // block now (the document insert does not touch `self.armed`). Anchored at
        // the document tail like the resolve-failure drain (the poison cell never
        // committed a block to parent to). Insert failure is loud, never swallowed.
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

        // Drain the engine failure ledger into visible Error blocks (§6, §8): a
        // resolve that erred (CAS read miss, validator reject) recorded a
        // FailureEvent and dropped its cell (a hole, never a fake). Surface each so
        // the player reads its own failure next turn — the producer-loop principle.
        self.drain_failures(ctx, &timeline);

        let Some(s) = self.armed.get_mut(&ctx) else {
            return BeatTickReport::default();
        };
        s.beat_count += 1;
        // The cadence questions answered side by side from the post-increment
        // beat_count. OODA is gated on the arm (it drives token spend); the phrase
        // boundary is not (it is a position in musical time, observed regardless).
        let phrase_due = s.policy.is_phrase_boundary(s.beat_count);
        // Rotate horizon: a phrase boundary whose phrase index is a multiple of
        // the configured cadence. `beat_count` is post-increment, so the first
        // boundary is phrase 1 (beat_count == beats_per_phrase) — a birth never
        // self-rotates. Gated on `playing` (a stopped context can't rotate) but
        // NOT on `ooda_armed`: a player paused mid-OODA still owes its page-turn.
        let rotate_due = phrase_due
            && s.policy.beats_per_phrase > 0
            && s.rotate_every_phrases.is_some_and(|n| {
                let phrase = s.beat_count / s.policy.beats_per_phrase;
                phrase % n == 0
            });
        BeatTickReport {
            ooda_due: s.ooda_armed
                && s.policy.ooda_every > 0
                && s.beat_count % s.policy.ooda_every == 0,
            phrase_due,
            rotate_due,
        }
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
    fn drain_failures(&mut self, ctx: ContextId, timeline: &kaijutsu_kernel::hyoushigi::SharedTimeline) {
        // Snapshot the new events under the lock, then release it before touching
        // the block store (no nested lock; no `.await` was ever here anyway).
        let new_events: Vec<(kaijutsu_types::Tick, kaijutsu_types::Tick, String, String)> = {
            let g = timeline.lock();
            let failures = g.failures();
            let Some(st) = self.armed.get(&ctx) else {
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
        if let Some(st) = self.armed.get_mut(&ctx) {
            st.failure_water += new_events.len();
        }
    }

    /// Snapshot the transport heartbeat for `ctx` — `$TICK`/`$PHRASE`/`$TEMPO`
    /// plus the `$HEARD` notation window — read NOW, on the scheduler thread with
    /// the beat state in hand, so a spawned lifecycle sees facts coherent with the
    /// beat that triggered it. Empty when the context isn't armed.
    fn transport_env(&self, ctx: ContextId) -> HashMap<String, String> {
        let Some(st) = self.armed.get(&ctx) else {
            return HashMap::new();
        };
        let playhead = self
            .kernel
            .timeline(ctx)
            .map(|t| t.lock().playhead())
            .unwrap_or(Tick::ZERO);
        let mut vars: HashMap<String, String> =
            transport_vars(playhead, st.beat_count, &st.policy)
                .into_iter()
                .collect();
        // $HEARD: the recent committed notation the player composes against. A
        // read failure degrades to an empty list (the player composes from the
        // chart + now-facts), never a dropped beat.
        let window = TickDelta::new((HEARD_WINDOW_PHRASES * st.policy.beats_per_phrase) as i64);
        let heard = self
            .documents
            .block_snapshots(ctx)
            .map(|blocks| heard_json(&blocks, playhead, window))
            .unwrap_or_else(|_| "[]".to_string());
        vars.insert("HEARD".to_string(), heard);
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

    /// Fire the `rotate` rc verb — the page-turn (`docs/chameleon.md`). Called for
    /// a context the scheduler has ALREADY `stop`ped this beat (the synchronous
    /// detach in `fire_due`), so the script's `kj fork --preset spawn` + arm-child
    /// runs with no parent still on the clock to race. Absent rc scripts → no-op.
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
        let (track, lead) = match self.armed.get(&ctx) {
            // Schedule one phrase of lead ahead of the playhead (design §10): the
            // fast write-barrier derive has ample room. Replaces the old fixed
            // 4-beat OODA_LEAD const, which assumed bars; lead now tracks the
            // policy's phrase length.
            Some(st) if st.ooda_armed => (st.track.clone(), st.policy.phrase_delta()),
            _ => return, // not an OODA-armed musician we manage
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
        // principal. `track` is the musician's lane.
        let played_by = b.id.principal_id;
        if let Err(e) = schedule_abc_cell(&self.kernel, ctx, &abc, lead, track, played_by) {
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

    /// Run the scheduler/transport loop until the ingress sender is dropped. One
    /// `select!` over the heap's nearest deadline, the command ingress, and the
    /// turn-completion bus (the OODA Act handoff).
    pub async fn run(mut self, mut ingress: mpsc::UnboundedReceiver<BeatCommand>) {
        log::info!("Beat scheduler online");
        let mut completed = self.kernel.turn_flows().subscribe("turn.completed");
        let mut turn_bus_open = true;
        loop {
            let next = self.next_wake();
            tokio::select! {
                biased;
                msg = ingress.recv() => match msg {
                    Some(BeatCommand::Arm { context_id, policy, track }) => {
                        self.arm(context_id, policy, track)
                    }
                    Some(BeatCommand::Play(ctx)) => self.play(ctx, Instant::now()),
                    Some(BeatCommand::Pause(ctx)) => self.pause(ctx),
                    Some(BeatCommand::Stop(ctx)) => self.stop(ctx),
                    Some(BeatCommand::SetTempo { context_id, period }) => {
                        self.set_tempo(context_id, period)
                    }
                    Some(BeatCommand::SetOoda { context_id, armed }) => {
                        self.set_ooda(context_id, armed)
                    }
                    Some(BeatCommand::SetRotate { context_id, every_phrases }) => {
                        self.set_rotate(context_id, every_phrases)
                    }
                    Some(BeatCommand::Disarm(ctx)) => self.disarm(ctx),
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
    let (tx, rx) = mpsc::unbounded_channel::<BeatCommand>();
    registry.kernel.kernel.set_beat_ingress(tx);

    let kernel = registry.kernel.kernel.clone();
    let documents = registry.kernel.documents.clone();
    let dispatcher = registry.kernel.kj_dispatcher.clone();
    let builder = std::thread::Builder::new().name("beat-scheduler".to_string());
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
    use kaijutsu_kernel::hyoushigi::BeatPolicy;
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
    /// not a pushed var — see `transport_vars` docs).
    #[test]
    fn transport_vars_report_now_facts() {
        // musician_default: 500 ms/beat (= 120 BPM), 16 beats/phrase.
        let m = vars_map(transport_vars(Tick::new(128), 128, &BeatPolicy::musician_default()));
        assert_eq!(m["TICK"], "128", "playhead tick verbatim");
        assert_eq!(m["PHRASE"], "8", "128 beats / 16 per phrase = phrase 8");
        assert_eq!(m["TEMPO"], "120", "500 ms/beat rounds to 120 BPM");
    }

    /// A faster tempo and a mid-phrase playhead: tick is exact, phrase floors,
    /// BPM rounds. Guards the formatting against off-by-one phrase math.
    #[test]
    fn transport_vars_floor_phrase_and_round_bpm() {
        let policy = BeatPolicy {
            period: Duration::from_millis(300), // 200 BPM
            beats_per_phrase: 16,
            ooda_every: 16,
        };
        let m = vars_map(transport_vars(Tick::new(40), 40, &policy));
        assert_eq!(m["TICK"], "40");
        assert_eq!(m["PHRASE"], "2", "40 / 16 floors to 2 (mid third phrase)");
        assert_eq!(m["TEMPO"], "200", "300 ms/beat = 200 BPM");
    }

    /// Defensive: a zero `beats_per_phrase` (no phrasing) reports phrase 0 rather
    /// than dividing by zero — same guard as `is_phrase_boundary`.
    #[test]
    fn transport_vars_zero_phrase_guard() {
        let policy = BeatPolicy {
            period: Duration::from_millis(500),
            beats_per_phrase: 0,
            ooda_every: 1,
        };
        let m = vars_map(transport_vars(Tick::new(5), 5, &policy));
        assert_eq!(m["PHRASE"], "0", "no phrasing → phrase 0, never a divide-by-zero");
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

    /// A 1-second beat policy with a large OODA cadence and phrase length (neither
    /// cadence nor phrase boundaries fire in the span of these tests).
    fn slow_policy() -> BeatPolicy {
        BeatPolicy {
            period: Duration::from_secs(1),
            beats_per_phrase: 1_000_000,
            ooda_every: 1_000_000,
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

    /// Playing drives one ticked block per beat in tick order; arming alone
    /// produces nothing (create stopped); disarm halts.
    #[tokio::test]
    async fn play_produces_ticked_blocks_in_order_arm_is_stopped() {
        let (kernel, documents) = fresh_kernel_and_docs().await;
        let ctx = ContextId::new();
        documents.create_document(ctx, DocumentKind::Conversation, None).unwrap();
        preseed_markers(&kernel, ctx, 5);

        let mut sched = BeatScheduler::new(kernel.clone(), documents.clone());
        let base = Instant::now();

        // Armed but not played: stopped → no heap entry, no blocks.
        sched.arm(ctx, slow_policy(), TrackId::solo());
        assert!(sched.next_wake().is_none(), "arm alone schedules nothing (stopped)");
        let out = sched.fire_due(base + Duration::from_secs(1));
        assert!(out.fired.is_empty());
        assert_eq!(contents(&documents, ctx).len(), 0);

        // Play: now it beats.
        sched.play(ctx, base);
        for i in 1..=5 {
            let out = sched.fire_due(base + Duration::from_secs(i));
            assert_eq!(out.fired, vec![ctx], "beat {i}");
            assert_eq!(contents(&documents, ctx).len(), i as usize);
        }
        assert_eq!(
            contents(&documents, ctx),
            vec!["beat-1", "beat-2", "beat-3", "beat-4", "beat-5"],
        );

        sched.disarm(ctx);
        let out = sched.fire_due(base + Duration::from_secs(6));
        assert!(out.fired.is_empty(), "disarmed → no beats");
    }

    /// Pause freezes the playhead; resume picks up at +1 with no wall-clock
    /// catch-up, even after a long quiescent gap.
    #[tokio::test]
    async fn pause_freezes_and_resume_continues_at_plus_one() {
        let (kernel, documents) = fresh_kernel_and_docs().await;
        let ctx = ContextId::new();
        documents.create_document(ctx, DocumentKind::Conversation, None).unwrap();
        preseed_markers(&kernel, ctx, 5);

        let mut sched = BeatScheduler::new(kernel.clone(), documents.clone());
        let base = Instant::now();
        sched.arm(ctx, slow_policy(), TrackId::solo());
        sched.play(ctx, base);

        sched.fire_due(base + Duration::from_secs(1)); // tick 1
        sched.fire_due(base + Duration::from_secs(2)); // tick 2
        assert_eq!(contents(&documents, ctx), vec!["beat-1", "beat-2"]);

        // Pause; the stale heap entry fires once and is dropped — no advance.
        sched.pause(ctx);
        sched.fire_due(base + Duration::from_secs(3));
        assert_eq!(contents(&documents, ctx).len(), 2, "paused → frozen");

        // Resume much later in wall-clock; the tick continues at 3, not jumping.
        sched.play(ctx, base + Duration::from_secs(3600));
        sched.fire_due(base + Duration::from_secs(3601));
        assert_eq!(
            contents(&documents, ctx),
            vec!["beat-1", "beat-2", "beat-3"],
            "resume at +1, no catch-up for the hour spent paused"
        );
    }

    /// A context played mid-run gets its own beats — the heap coalesces contexts.
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
        sched.arm(a, slow_policy(), TrackId::solo());
        sched.play(a, base);

        sched.fire_due(base + Duration::from_secs(1));
        sched.fire_due(base + Duration::from_secs(2));
        assert_eq!(contents(&documents, a).len(), 2);
        assert_eq!(contents(&documents, b).len(), 0, "b not playing yet");

        sched.arm(b, slow_policy(), TrackId::solo());
        sched.play(b, base + Duration::from_secs(2));
        sched.fire_due(base + Duration::from_secs(3)); // both due
        assert_eq!(contents(&documents, a).len(), 3);
        assert_eq!(contents(&documents, b).len(), 1, "b got its first beat");
    }

    /// The OODA cadence fires every `ooda_every` beats — but only while OODA is
    /// armed. `stop` (OODA off) and `set_ooda(false)` suppress it.
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
        sched.arm(
            ctx,
            BeatPolicy {
                period: Duration::from_secs(1),
                beats_per_phrase: 1_000_000,
                ooda_every: 3,
            },
            TrackId::solo(),
        );
        sched.play(ctx, base);

        let mut ooda_beats = Vec::new();
        for i in 1..=6 {
            let out = sched.fire_due(base + Duration::from_secs(i));
            if !out.ooda_due.is_empty() {
                ooda_beats.push(i);
            }
        }
        assert_eq!(ooda_beats, vec![3, 6], "OODA boundary every 3 beats while armed");

        // Disarm OODA: cadence boundaries no longer report due.
        sched.set_ooda(ctx, false);
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
        // beats_per_phrase = 4; large ooda so the OODA cadence never coincides.
        sched.arm(
            ctx,
            BeatPolicy {
                period: Duration::from_secs(1),
                beats_per_phrase: 4,
                ooda_every: 1_000_000,
            },
            TrackId::solo(),
        );
        sched.play(ctx, base);

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

    /// The rotate-race fix (docs/issues.md): at a rotate horizon the scheduler
    /// retires the parent SYNCHRONOUSLY in `fire_due` — it reports `rotate_due`
    /// (NOT `ooda_due`, even when the OODA cadence coincides) and the parent does
    /// not fire again. This is what closes the stray-tick / double-fork race that
    /// a pure-rc `kj transport ooda off` (async via the ingress) could not.
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
        // phrase = 2 beats; OODA also every 2 beats so the rotate beat COINCIDES
        // with an OODA boundary — proving rotate suppresses the stray ooda tick.
        sched.arm(
            ctx,
            BeatPolicy { period: Duration::from_secs(1), beats_per_phrase: 2, ooda_every: 2 },
            TrackId::solo(),
        );
        sched.set_rotate(ctx, Some(1)); // rotate every phrase
        sched.play(ctx, base);

        // Beat 1: mid-phrase — nothing due.
        let b1 = sched.fire_due(base + Duration::from_secs(1));
        assert_eq!(b1.fired, vec![ctx]);
        assert!(b1.rotate_due.is_empty() && b1.ooda_due.is_empty());

        // Beat 2: phrase 1 horizon → rotate. Reported on rotate_due, and NOT on
        // ooda_due even though 2 % ooda_every == 0 (the rotate suppresses it).
        let b2 = sched.fire_due(base + Duration::from_secs(2));
        assert_eq!(b2.rotate_due, vec![ctx], "horizon reports rotate_due");
        assert!(b2.ooda_due.is_empty(), "rotate suppresses the coincident ooda tick");
        assert!(!sched.armed.get(&ctx).unwrap().playing, "parent stopped synchronously");
        assert!(!sched.armed.get(&ctx).unwrap().ooda_armed, "parent OODA disarmed");

        // Beat 3+: the parent was not re-armed → it never fires again. No stray
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
        sched.arm(
            ctx,
            BeatPolicy { period: Duration::from_secs(1), beats_per_phrase: 2, ooda_every: 1_000_000 },
            TrackId::solo(),
        );
        sched.set_rotate(ctx, Some(2)); // rotate every 2 phrases
        sched.play(ctx, base);

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
        sched.arm(
            ctx,
            BeatPolicy { period: Duration::from_secs(1), beats_per_phrase: 2, ooda_every: 1_000_000 },
            TrackId::solo(),
        );
        sched.play(ctx, base);

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
        // reachable in this 8-beat window; large ooda so the cadence never fires.
        let phrase = BeatPolicy {
            period: Duration::from_secs(1),
            beats_per_phrase: 4,
            ooda_every: 1_000_000,
        };
        let mut sched = BeatScheduler::new(kernel.clone(), documents.clone());
        let base = Instant::now();
        sched.arm(ctx, phrase, TrackId::solo());
        sched.play(ctx, base);

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
            ooda_every: 1_000_000,
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
            sched.arm(ctx, phrase, TrackId::solo());
            sched.play(ctx, base);
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
            sched.arm(ctx, phrase, TrackId::solo());
            sched.play(ctx, base);
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
            sched.arm(ctx, phrase, TrackId::solo());
            sched.play(ctx, base);
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
            sched.arm(ctx, phrase, TrackId::solo());
            sched.play(ctx, base);
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
            sched.arm(ctx, phrase, TrackId::solo());
            sched.play(ctx, base);
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
        sched.arm(
            ctx,
            BeatPolicy { period: Duration::from_secs(1), beats_per_phrase: 4, ooda_every: 1_000_000 },
            TrackId::solo(),
        );
        sched.play(ctx, base);
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
        sched.arm(ctx, slow_policy(), TrackId::solo());
        let playhead_after_arm = kernel.timeline(ctx).unwrap().lock().playhead();
        assert_eq!(
            playhead_after_arm,
            Tick::new(big_t),
            "arm seeds the playhead from max_tick (musical time stays monotone)"
        );

        // First beat advances from the seeded position: T → T+1.
        let base = Instant::now();
        sched.play(ctx, base);
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
        sched.arm(ctx, slow_policy(), TrackId::solo());
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

    /// Arm and tempo-change write the live policy + lane through to `beat_state`,
    /// so a kernel restart can re-arm the musician with its real values instead of
    /// `BeatPolicy::musician_default()`. This is the durable half of "re-arm is
    /// possible after a cold start" — the read half is `kj transport arm`.
    #[tokio::test]
    async fn arm_and_tempo_persist_beat_state() {
        use kaijutsu_kernel::kernel_db::PersistedBeatState;

        let (kernel, documents, db, ctx) = db_backed_kernel_and_docs().await;
        let mut sched = BeatScheduler::new(kernel, documents);

        // Arm with a non-default policy on the `bass` lane.
        let policy = BeatPolicy { period: Duration::from_millis(500), beats_per_phrase: 12, ooda_every: 96 };
        let track = TrackId::new("bass").unwrap();
        sched.arm(ctx, policy, track.clone());

        assert_eq!(
            db.lock().get_beat_state(ctx).unwrap(),
            Some(PersistedBeatState {
                period_ms: 500,
                beats_per_phrase: 12,
                ooda_every: 96,
                track: "bass".to_string(),
            }),
            "arm mirrors the policy + lane into beat_state for restart re-arm"
        );

        // A tempo change to 240 BPM (250 ms) updates the persisted period in place,
        // leaving the rest of the policy intact.
        sched.set_tempo(ctx, Duration::from_millis(250));
        assert_eq!(
            db.lock().get_beat_state(ctx).unwrap(),
            Some(PersistedBeatState {
                period_ms: 250,
                beats_per_phrase: 12,
                ooda_every: 96,
                track: "bass".to_string(),
            }),
            "set_tempo write-through updates the persisted period, not the whole row"
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
        sched.arm(ctx, slow_policy(), TrackId::solo());
        sched.play(ctx, base);
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
        sched.arm(ctx, slow_policy(), TrackId::solo());
        sched.play(ctx, base);

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
        sched.arm(ctx, slow_policy(), TrackId::solo());
        sched.play(ctx, base);

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
        sched.arm(ctx, slow_policy(), TrackId::solo());
        sched.play(ctx, base);
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
        sched_b.arm(cb, slow_policy(), TrackId::solo());
        sched_b.play(cb, base_b);
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
        sched_c.arm(
            cc,
            BeatPolicy { period: Duration::from_secs(1), beats_per_phrase: 4, ooda_every: 1_000_000 },
            TrackId::solo(),
        );
        sched_c.play(cc, base_c);
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
}
