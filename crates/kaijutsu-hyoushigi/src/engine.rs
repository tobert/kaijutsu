//! Speculation and commitment on a [`Timeline`] driven by external ticks.
//!
//! Each attempt owns a future. The timeline polls without waiting and validates
//! ready output before commitment. The kernel materializes accepted cells from
//! the in-memory log and content map into durable CAS and score blocks.

use std::collections::{HashMap, VecDeque};
use std::time::Duration;
use std::sync::Arc;

use kaijutsu_cas::ContentHash;
use kaijutsu_types::{PrincipalId, Tick, TickDelta, TrackId};

use crate::cell::{Body, Cell, CellState, Fallback, Recipe, ResolverId};
use crate::content::{ContentRef, ContextHash};
use crate::resolver::{ResolverCtx, Resolver};
use crate::{CostSample, Disposition, SampleOutcome, FallbackReason, Readiness, ResolveFuture, WorkId, WorkStatus};

/// How many measured attempts each resolver's cost window keeps.
pub const COST_WINDOW: usize = 32;

/// Convert preparation costs into lead times at the external clock's tick rate.
/// `safety_factor` widens the initial lead; `commit_margin` leaves time between
/// the first commitment check and the intended start.
#[derive(Debug, Clone, Copy)]
pub struct TickClock {
    pub ticks_per_sec: f64,
    pub safety_factor: f64,
    pub commit_margin: TickDelta,
}

impl Default for TickClock {
    fn default() -> Self {
        Self {
            ticks_per_sec: 1.0,
            safety_factor: 1.5,
            commit_margin: TickDelta::new(1),
        }
    }
}

impl TickClock {
    fn validate(&self) {
        assert!(self.ticks_per_sec.is_finite() && self.ticks_per_sec > 0.0, "tick rate must be finite and positive");
        assert!(self.safety_factor.is_finite() && self.safety_factor >= 0.0, "lead safety factor must be finite and nonnegative");
        assert!(self.commit_margin.get() >= 0, "commit margin must be nonnegative");
    }
    /// Convert a wall-clock duration into ticks, rounding up (never under-lead).
    fn beats_for(&self, d: Duration) -> TickDelta {
        let ticks = (d.as_secs_f64() * self.ticks_per_sec).ceil();
        assert!(ticks.is_finite() && ticks < i64::MAX as f64, "lead time exceeds the tick range");
        TickDelta::new(ticks as i64)
    }

    fn duration_for(&self, ticks: TickDelta) -> Duration {
        Duration::from_secs_f64(ticks.get().max(0) as f64 / self.ticks_per_sec)
    }
}

/// The recovery action chosen at a squash, not the eventual output disposition.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Recovery {
    /// A retry was started because at least `estimate_cost` remained. Its final
    /// settlement is scheduled at `start`; the retry can still fail or diverge.
    ReSpeculated,
    /// No time to recover — the required fallback fired.
    FellBack,
}

/// The most valuable output the system produces: a misprediction, with both the
/// predicted and the actual context digest, so you can see exactly where the
/// anticipation model was wrong — and what it cost.
#[derive(Debug, Clone)]
pub struct SquashEvent {
    pub work_id: WorkId,
    pub attempt: u32,
    pub at: Tick,
    pub start: Tick,
    pub predicted: ContextHash,
    pub actual: ContextHash,
    pub recovery: Recovery,
}

/// A resolve that errored — recorded, never hidden. Sibling to [`SquashEvent`]:
/// a squash is a *recoverable* misprediction (predicted ≠ actual, but the bytes
/// were produced); a failure is a resolve that produced nothing (CAS read miss,
/// validator reject). The ledger is the data source for the "ABC parse-failure
/// rate" eval ruler and the input to the kernel's per-event Error-block surfacing.
#[derive(Debug, Clone)]
pub struct FailureEvent {
    /// Original input anchor for feedback, independent of current attachments.
    pub source_block: Option<kaijutsu_types::BlockId>,
    /// Absent for an emitted cell that could not be admitted.
    pub work_id: Option<WorkId>,
    pub attempt: u32,
    /// The playhead position when the failure was recorded.
    pub at: Tick,
    /// The failed cell's start tick — its intended musical position.
    pub start: Tick,
    /// Which resolver erred (the recipe's resolver id).
    pub resolver: ResolverId,
    /// The resolver's error string, preserved verbatim for surfacing.
    pub error: String,
    /// The principal whose cell failed — its provenance. With N producers sharing
    /// one track timeline, the kernel filters the shared ledger by this so a
    /// producer's failures surface in *its own* conversation, never a sibling's.
    pub played_by: PrincipalId,
}

/// Engine bookkeeping wrapped around a deferred [`Cell`].
struct Scheduled {
    status: WorkStatus,
    order: u64,
    work: Option<ResolveFuture>,
    resolver: Arc<dyn Resolver>,
    observed_at: Tick,
    cell: Cell,
    start: Tick,
    commit_deadline: Tick,
    resolution: Option<crate::resolver::Resolution>,
    /// Set once a squash re-speculated; the next commit check is at `start` and
    /// is the last — diverge there and the fallback fires.
    final_attempt: bool,
}

impl Scheduled {
    /// The tick at which this cell's next lifecycle action is due, if any.
    fn next_at(&self) -> Option<Tick> {
        match self.cell.state {
            CellState::Pending => Some(self.status.prepare_at),
            CellState::Speculating => Some(self.start),
            CellState::Speculated | CellState::Failed => Some((if self.final_attempt {
                self.start
            } else {
                self.commit_deadline
            }).max(self.observed_at)),
            _ => None,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ScheduleError {
    #[error("cannot schedule a concrete cell as deferred work")]
    NotDeferred,
    #[error("cannot schedule at {start:?}: behind the playhead {now:?} — no room to speculate")]
    InThePast { start: Tick, now: Tick },
    #[error("unknown resolver: {0}")]
    UnknownResolver(String),
    #[error("timeline has reached its capacity of {0} open work items")]
    AtCapacity(usize),
    #[error("work is no longer pending on this timeline")]
    NotPending,
    #[error("the supplied resolver does not match the recipe")]
    ResolverMismatch,
}

/// Why a [`Timeline::seed_playhead`] call was rejected. Seeding is a
/// virgin-only operation — a seed attempt on a live timeline is always a caller
/// bug, and crashing over corruption beats silently rewinding or clobbering an
/// open future.
#[derive(Debug, thiserror::Error)]
pub enum SeedError {
    #[error(
        "cannot seed playhead to {at:?}: timeline is not virgin \
         (playhead {playhead:?}, {future} future cell(s), {committed} committed, {admitted} admitted)"
    )]
    NotVirgin {
        at: Tick,
        playhead: Tick,
        future: usize,
        committed: usize,
        admitted: u64,
    },
}

/// A single context's timeline: committed past + open future, over one store.
pub struct Timeline {
    clock: TickClock,
    playhead: Tick,
    resolvers: HashMap<String, Arc<dyn Resolver>>,
    /// The mutable context resolvers read (a beat counter, environment, …).
    ambient: HashMap<String, Vec<u8>>,
    /// The open future: deferred cells ahead of the commit point.
    future: Vec<Scheduled>,
    capacity: usize,
    next_order: u64,
    history: VecDeque<WorkStatus>,
    /// The durable past (in-RAM stand-in for the block log).
    committed: Vec<Cell>,
    /// The content store (in-RAM stand-in for CAS). Crystallized at commit.
    cas: HashMap<ContentHash, Vec<u8>>,
    /// The squash ledger — the bill, and the anticipation-model feedback.
    squashes: Vec<SquashEvent>,
    /// Resolver errors, drained by the kernel into each producer's conversation.
    /// Failed work keeps its deadline until its declared fallback settles.
    failures: Vec<FailureEvent>,
    /// The newest measured attempts per resolver id, oldest first, kept as raw
    /// samples. Nothing reads them yet; predictions will roll them on the fly.
    costs: HashMap<String, VecDeque<CostSample>>,

}

impl Timeline {
    pub fn new(clock: TickClock) -> Self {
        clock.validate();
        Self {
            clock,
            playhead: Tick::ZERO,
            resolvers: HashMap::new(),
            ambient: HashMap::new(),
            future: Vec::new(),
            capacity: 64,
            next_order: 0,
            history: VecDeque::new(),
            committed: Vec::new(),
            cas: HashMap::new(),
            squashes: Vec::new(),
            failures: Vec::new(),
            costs: HashMap::new(),
        }
    }

    pub fn register_resolver(&mut self, resolver: Box<dyn Resolver>) {
        self.resolvers.insert(resolver.id().0, Arc::from(resolver));
    }

    pub fn with_capacity(clock: TickClock, capacity: std::num::NonZeroUsize) -> Self {
        let mut timeline = Self::new(clock);
        timeline.capacity = capacity.get();
        timeline
    }

    /// Open work and the most recent 256 dispositions, local to this timeline.
    pub fn statuses(&self) -> Vec<WorkStatus> {
        self.history.iter().chain(self.future.iter().map(|s| &s.status)).cloned().collect()
    }

    /// The newest measured attempts of one resolver, oldest first.
    pub fn cost_samples(&self, resolver: &ResolverId) -> impl Iterator<Item = &CostSample> {
        self.costs.get(&resolver.0).into_iter().flatten()
    }

    pub fn status(&self, id: WorkId) -> Option<&WorkStatus> {
        self.future.iter().map(|s| &s.status).chain(self.history.iter()).find(|s| s.id == id)
    }

    fn finish(&mut self, mut status: WorkStatus, disposition: Disposition) {
        status.settled_at = Some(self.playhead);
        status.disposition = Some(disposition);
        if self.history.len() == 256 {
            self.history.pop_front();
        }
        self.history.push_back(status);
    }

    pub fn cancel(&mut self, id: WorkId) -> bool {
        let Some(idx) = self.future.iter().position(|s| s.status.id == id) else { return false };
        let s = self.future.swap_remove(idx);
        self.finish(s.status, Disposition::Cancelled);
        true
    }

    /// Validate and admit the replacement before cancelling its predecessor.
    pub fn supersede(&mut self, id: WorkId, cell: Cell) -> Result<WorkId, ScheduleError> {
        let idx = self.future.iter().position(|s| s.status.id == id).ok_or(ScheduleError::NotPending)?;
        let by = self.schedule_inner(cell, true, None)?;
        let old = self.future.swap_remove(idx);
        self.finish(old.status, Disposition::Superseded { by });
        drop(old.work);
        drop(old.resolver);
        let idx = self.future.iter().position(|s| s.status.id == by).expect("replacement was admitted");
        if self.future[idx].status.prepare_at <= self.playhead {
            self.speculate(idx);
        }
        Ok(by)
    }

    /// Use this rate for future admissions. Already admitted work retains the
    /// deadlines derived from the rate in force when it was scheduled.
    pub fn set_clock(&mut self, clock: TickClock) {
        clock.validate();
        self.clock = clock;
    }

    /// TEST-ONLY: the `prepare_at` tick of the Nth open-future cell, so a test
    /// can assert [`set_clock`](Self::set_clock) re-derived the speculation lead.
    #[cfg(any(test, feature = "test-util"))]
    pub fn scheduled_speculate_at(&self, idx: usize) -> Option<Tick> {
        self.future.get(idx).map(|s| s.status.prepare_at)
    }

    /// TEST-ONLY: the timeline's current speculation [`TickClock`], so a kernel/
    /// scheduler test can assert a tempo change slaved the new clock down here.
    #[cfg(any(test, feature = "test-util"))]
    pub fn clock_for_test(&self) -> TickClock {
        self.clock
    }

    /// Poke the ambient context — what a real beat / environment / sibling event
    /// would mutate between turns. Changing this between a speculate and its
    /// commit is what drives a squash.
    pub fn set_ambient(&mut self, key: impl Into<String>, value: impl Into<Vec<u8>>) {
        self.ambient.insert(key.into(), value.into());
    }

    /// TEST-ONLY: push a pre-built concrete cell straight into the committed log
    /// WITHOUT crystallizing its bytes into this timeline's RAM-CAS. Lets a kernel
    /// test construct a committed cref whose bytes are absent from both RAM-CAS and
    /// durable CAS — the corruption case the materializer must bail on, never
    /// silently skip.
    #[cfg(any(test, feature = "test-util"))]
    pub fn push_committed_for_test(&mut self, cell: Cell) {
        self.committed.push(cell);
    }

    /// Rehydrate a freshly-armed timeline's committed log from durable history (the
    /// materialized score reconstructed by the kernel), so `UseLastGood`
    /// (`last_committed_content_in`) sees prior phrases across a restart. Only valid
    /// on a **virgin** timeline (empty committed); the playhead must already be
    /// seeded at/after these cells' ticks. A non-virgin rehydrate is a kernel bug —
    /// crash over corrupting a live committed log. Empty `cells` is a clean no-op
    /// (a fresh track with no history yet).
    pub fn rehydrate_committed(&mut self, cells: Vec<Cell>) {
        assert!(
            self.committed.is_empty(),
            "rehydrate_committed requires a virgin timeline (committed must be empty)"
        );
        self.committed = cells;
    }

    pub fn playhead(&self) -> Tick {
        self.playhead
    }
    pub fn committed(&self) -> &[Cell] {
        &self.committed
    }
    pub fn squashes(&self) -> &[SquashEvent] {
        &self.squashes
    }
    /// Every resolver error, recorded when observed. The kernel drains these
    /// into Error blocks independently of the later fallback commitment.
    pub fn failures(&self) -> &[FailureEvent] {
        &self.failures
    }
    /// Cells awaiting resolution, commitment, or their declared fallback.
    pub fn future_len(&self) -> usize {
        self.future.len()
    }
    /// Fetch crystallized content bytes by hash (in-RAM CAS).
    pub fn content_bytes(&self, hash: &ContentHash) -> Option<&[u8]> {
        self.cas.get(hash).map(|v| v.as_slice())
    }

    /// Schedule a deferred cell. Derives its lead time from `estimate_cost`:
    /// `prepare_at = start − beats_for(estimate × safety)`,
    /// `commit_deadline = start − commit_margin`.
    pub fn schedule(&mut self, cell: Cell) -> Result<WorkId, ScheduleError> {
        self.schedule_inner(cell, false, None)
    }

    /// Admit a dedicated preparation owner and capture its basis immediately.
    /// The resolver belongs to this work item, not the shared registry. Dropping
    /// or rejecting the admission releases it and its pending output channel.
    pub fn schedule_preparing(&mut self, cell: Cell, resolver: Box<dyn Resolver>) -> Result<WorkId, ScheduleError> {
        self.schedule_inner(cell, false, Some(resolver))
    }

    fn schedule_inner(&mut self, cell: Cell, replacing: bool, owned: Option<Box<dyn Resolver>>) -> Result<WorkId, ScheduleError> {
        if !replacing && self.future.len() >= self.capacity {
            return Err(ScheduleError::AtCapacity(self.capacity));
        }
        let Body::Deferred(recipe) = &cell.body else {
            return Err(ScheduleError::NotDeferred);
        };
        let start = cell.span.start;
        if start <= self.playhead {
            return Err(ScheduleError::InThePast {
                start,
                now: self.playhead,
            });
        }
        let preparing = owned.is_some();
        let resolver: Arc<dyn Resolver> = match owned {
            Some(resolver) => {
                if resolver.id() != recipe.resolver { return Err(ScheduleError::ResolverMismatch); }
                Arc::from(resolver)
            }
            None => self.resolvers.get(&recipe.resolver.0)
                .ok_or_else(|| ScheduleError::UnknownResolver(recipe.resolver.0.clone()))?.clone(),
        };

        let ctx = CommittedCtx {
            now: start,
            ambient: &self.ambient,
            committed: &self.committed,
        };
        let est = resolver.estimate_cost(&recipe.params, &ctx);
        let est_cost = self.clock.beats_for(est);
        let lead = self.clock.beats_for(est.mul_f64(self.clock.safety_factor));

        let id = WorkId(uuid::Uuid::new_v4());
        let order = self.next_order;
        self.next_order = self.next_order.checked_add(1).expect("timeline admission order overflow");
        self.future.push(Scheduled {
            status: WorkStatus {
                id, track: cell.track.clone(), played_by: cell.played_by,
                start, admitted_at: self.playhead,
                estimate: est_cost,
                estimate_wall: est,
                prepare_at: if preparing { self.playhead } else { start - lead },
                attempt: 0,
                started_at: None, ready_at: None, readiness: Readiness::Queued,
                predicted: None, actual: None, valid: None, error: None,
                timing: None, settled_at: None, disposition: None,
            },
            order,
            work: None,
            resolver,
            observed_at: self.playhead,
            start,
            commit_deadline: start - self.clock.commit_margin,
            resolution: None,
            final_attempt: false,
            cell,
        });
        if !replacing {
            let idx = self.future.len() - 1;
            if self.future[idx].status.prepare_at <= self.playhead {
                self.speculate(idx);
            }
        }
        Ok(id)
    }

    /// Seed the playhead to `at` on a **virgin** timeline — the re-arm entry
    /// point that restores musical time after a restart or rotation, before any
    /// cell is scheduled or committed.
    ///
    /// `Err(SeedError::NotVirgin)` unless `playhead == Tick::ZERO`, the future is
    /// empty, and the committed log is empty. A seed attempt on a live timeline
    /// is always a caller bug and must be loud — crash over corruption beats
    /// silently rewinding the playhead or clobbering an open future. Fires no
    /// lifecycle actions: it only positions the playhead so the first beat
    /// advances from real musical time.
    pub fn seed_playhead(&mut self, at: Tick) -> Result<(), SeedError> {
        if self.playhead != Tick::ZERO || !self.future.is_empty() || !self.committed.is_empty() || self.next_order != 0 {
            return Err(SeedError::NotVirgin {
                at,
                playhead: self.playhead,
                future: self.future.len(),
                committed: self.committed.len(),
                admitted: self.next_order,
            });
        }
        self.playhead = at;
        Ok(())
    }

    /// Observe work at `target` and process due actions by intended start, then
    /// admission order. Equal ticks poll without advancing time; earlier ticks
    /// are ignored. Readiness and validation are never backdated to crossed ticks.
    pub fn advance_to(&mut self, target: Tick) {
        if target < self.playhead {
            return;
        }
        self.playhead = target;
        // Completions belong to this observation tick, never a deadline crossed
        // on the way here. A result first observed after start is too late.
        for idx in 0..self.future.len() {
            if self.future[idx].cell.state == CellState::Speculating && target <= self.future[idx].start {
                self.poll_work(idx);
            }
        }
        loop {
            let next = self.future.iter().enumerate()
                .filter(|(_, s)| s.next_at().is_some_and(|at| at <= target))
                .min_by_key(|(_, s)| (s.start, s.order))
                .map(|(idx, _)| idx);
            let Some(idx) = next else { break };
            if target > self.future[idx].start && self.future[idx].cell.state != CellState::Failed {
                self.fire_fallback(idx, FallbackReason::DeadlineMissed);
                continue;
            }
            match self.future[idx].cell.state {
                CellState::Pending => self.speculate(idx),
                CellState::Speculating => self.fire_fallback(idx, FallbackReason::DeadlineMissed),
                CellState::Speculated => self.commit_or_squash(idx),
                CellState::Failed => self.fire_fallback(idx, FallbackReason::ResolveFailed),
                _ => unreachable!("next_at only yields actionable states"),
            }
        }
    }

    /// Snapshot the basis and start an owned resolution without waiting for it.
    fn speculate(&mut self, idx: usize) {
        let recipe = self.deferred_recipe(idx);
        let resolver = &self.future[idx].resolver;
        let ctx = CommittedCtx {
            now: self.future[idx].start,
            ambient: &self.ambient,
            committed: &self.committed,
        };
        let basis = resolver.compute_basis(&recipe.params, &ctx);
        let work = resolver.resolve(&recipe.params, &ctx);
        let s = &mut self.future[idx];
        debug_assert!(s.cell.state.can_advance_to(CellState::Speculating));
        s.cell.state = CellState::Speculating;
        s.status.attempt += 1;
        s.status.started_at = Some(self.playhead);
        s.status.ready_at = None;
        s.status.readiness = Readiness::Running;
        s.status.predicted = Some(basis.clone());
        s.status.actual = None;
        s.status.valid = None;
        s.status.timing = None;
        s.work = Some(work);
        self.poll_work(idx);
    }

    fn poll_work(&mut self, idx: usize) {
        let s = &mut self.future[idx];
        let mut ctx = std::task::Context::from_waker(std::task::Waker::noop());
        let polled = s.work.as_mut().expect("running work owns its future").as_mut().poll(&mut ctx);
        let std::task::Poll::Ready(result) = polled else { return };
        s.work = None;
        s.observed_at = self.playhead;
        let timing = match &result {
            Ok(res) => res.timing,
            Err(error) => error.timing,
        };
        s.status.timing = timing;
        if let Some(timing) = timing {
            let outcome = if result.is_ok() { SampleOutcome::Ready } else { SampleOutcome::Failed };
            let sample = CostSample {
                at: self.playhead, played_by: s.cell.played_by,
                estimate: s.status.estimate_wall, timing, outcome,
            };
            let resolver = s.resolver.id();
            self.record_cost(resolver, sample);
        }
        let s = &mut self.future[idx];
        match result {
            Ok(res) => {
                s.status.readiness = Readiness::Ready;
                s.status.ready_at = Some(self.playhead);
                s.resolution = Some(res);
                s.cell.state = CellState::Speculated;
            }
            Err(crate::resolver::ResolveError { message: error, .. }) => {
                s.status.readiness = Readiness::Failed;
                s.status.error = Some(error.clone());
                s.cell.state = CellState::Failed;
                let Body::Deferred(recipe) = &s.cell.body else { unreachable!("scheduled work is deferred") };
                self.failures.push(FailureEvent {
                    source_block: s.resolver.source_block(),
                    work_id: Some(s.status.id), attempt: s.status.attempt,
                    at: self.playhead, start: s.start, resolver: recipe.resolver.clone(),
                    error, played_by: s.cell.played_by,
                });
            }
        }
    }

    /// At the commit deadline (or the final attempt at `start`): recompute the
    /// basis against current context. Match → commit + crystallize. Diverge →
    /// squash, then re-speculate if budget remains, else fire the fallback.
    fn commit_or_squash(&mut self, idx: usize) {
        let recipe = self.deferred_recipe(idx);
        // Basis validation sees the same intended start as speculation. The
        // current playhead measures the remaining recovery budget separately.
        let current_tick = self.playhead;
        let start = self.future[idx].start;

        let resolver = &self.future[idx].resolver;
        let ctx = CommittedCtx {
            now: start,
            ambient: &self.ambient,
            committed: &self.committed,
        };
        let actual = resolver.compute_basis(&recipe.params, &ctx);
        let retry_allowed = resolver.can_respeculate();
        let predicted = self.future[idx].status.predicted.clone().expect("speculated");

        self.future[idx].status.actual = Some(actual.clone());
        self.future[idx].status.valid = Some(actual == predicted);
        if actual == predicted {
            self.commit(idx);
            return;
        }

        // --- squash ---------------------------------------------------------
        let est_cost = self.future[idx].status.estimate;
        let budget = start - current_tick; // ticks left until the content is actually needed
        let can_respeculate = retry_allowed && !self.future[idx].final_attempt && budget >= est_cost;

        let recovery = if can_respeculate {
            Recovery::ReSpeculated
        } else {
            Recovery::FellBack
        };
        self.squashes.push(SquashEvent {
            work_id: self.future[idx].status.id,
            attempt: self.future[idx].status.attempt,
            at: current_tick,
            start,
            predicted,
            actual,
            recovery,
        });

        {
            let s = &mut self.future[idx];
            debug_assert!(s.cell.state.can_advance_to(CellState::Squashed));
            s.cell.state = CellState::Squashed;
        }

        if can_respeculate {
            // Re-speculate immediately against the new context; the next commit
            // check is the final one at `start`. The Squashed → Speculating edge
            // is legal; `speculate` re-enters from there.
            {
                let s = &mut self.future[idx];
                s.final_attempt = true;
                s.resolution = None;
            }
            self.speculate(idx);
        } else {
            self.fire_fallback(idx, FallbackReason::InvalidBasis);
        }
    }

    /// Commit: crystallize the speculated bytes to CAS, append the cell to the
    /// durable past, and release any emitted cells into the open future.
    fn commit(&mut self, idx: usize) {
        let mut s = self.future.swap_remove(idx);
        let res = s.resolution.take().expect("speculated has a resolution");
        let cref = res.content_ref();
        self.cas.entry(cref.hash.clone()).or_insert(res.bytes);

        debug_assert!(s.cell.state.can_advance_to(CellState::Committed));
        s.cell.body = Body::Concrete(cref.clone());
        s.cell.state = CellState::Committed;

        // The committing parent's lane + player are the authority for everything
        // it emits — an emission is part of the committing parent's act. Capture
        // them before the cell moves into the committed log.
        let parent_track = s.cell.track.clone();
        let parent_player = s.cell.played_by;
        self.finish(s.status, Disposition::Committed { content: cref.clone() });
        self.committed.push(s.cell);

        // Emitted cells become real only on commit — a squashed resolution's
        // emissions simply vanish. A loop thus unrolls into distinct memories.
        for mut emitted in res.emitted {
            // An emission lives inside the parent's track (the only concrete
            // consumers — MIDI siblings, in-track automation lanes — are
            // same-track by definition). Stamp it with the parent's lane +
            // player; a resolver that set divergent values has misread the
            // contract. Loud on mismatch: hard assert in debug, tracing::error
            // in release — never silent normalization, but never a Failed cell
            // mid-performance either.
            debug_assert_eq!(
                emitted.track, parent_track,
                "emitted cell track must match the committing parent's track"
            );
            debug_assert_eq!(
                emitted.played_by, parent_player,
                "emitted cell played_by must match the committing parent's player"
            );
            if emitted.track != parent_track || emitted.played_by != parent_player {
                // Log BOTH axes (track and player): the guard above checks both, so
                // a player-only mismatch (track equal, played_by divergent) produces
                // this same error — without the player fields it would be
                // indistinguishable from a track mismatch in production logs.
                tracing::error!(
                    parent_track = parent_track.as_str(),
                    emitted_track = emitted.track.as_str(),
                    parent_player = %parent_player,
                    emitted_player = %emitted.played_by,
                    "emitted cell track/player diverged from committing parent; \
                     stamping with parent's values"
                );
            }
            emitted.track = parent_track.clone();
            emitted.played_by = parent_player;
            self.absorb_emitted(emitted);
        }
    }

    /// The required real-time miss handler. Never undefined behavior.
    ///
    /// Fallback repeats and literals are authored by [`PrincipalId::beat()`], not
    /// by the player: the transport played them. Attributing vamp-insurance to
    /// the player would be false provenance. They stay on the missing cell's
    /// `track` — the lane persists even when no player covered this beat.
    fn record_cost(&mut self, resolver: ResolverId, sample: CostSample) {
        let window = self.costs.entry(resolver.0).or_default();
        if window.len() == COST_WINDOW {
            window.pop_front();
        }
        window.push_back(sample);
    }

    fn fire_fallback(&mut self, idx: usize, reason: FallbackReason) {
        let s = self.future.swap_remove(idx);
        if s.cell.state == CellState::Speculating
            && let Some(started) = s.status.started_at
        {
            let ran = self.clock.duration_for(self.playhead - started);
            self.record_cost(s.resolver.id(), CostSample {
                at: self.playhead, played_by: s.cell.played_by, estimate: s.status.estimate_wall,
                timing: crate::Timing { queued: Duration::ZERO, compute: ran },
                outcome: SampleOutcome::Missed,
            });
        }
        let Body::Deferred(recipe) = &s.cell.body else { unreachable!("scheduled cells are deferred") };
        let policy = recipe.fallback.clone();
        let content = match &policy {
            Fallback::Skip => None,
            Fallback::UseLastGood => self.last_committed_content_in(&s.cell.track, s.start),
            Fallback::Literal(content) => Some(content.clone()),
        };
        if let Some(content) = &content {
            self.committed.push(Cell::concrete_on(s.cell.span, content.clone(), s.cell.track, PrincipalId::beat()));
        }
        self.finish(s.status, Disposition::Fallback { reason, policy, content });
    }

    /// Place an emitted cell: a deferred emission re-enters scheduling; a concrete
    /// emission appends to the past (a recorded memory). Never rewrites committed.
    fn absorb_emitted(&mut self, cell: Cell) {
        match &cell.body {
            Body::Concrete(_) => self.committed.push(cell),
            Body::Deferred(recipe) => {
                let resolver = recipe.resolver.clone();
                let start = cell.span.start;
                let played_by = cell.played_by;
                if let Err(error) = self.schedule(cell) {
                    self.failures.push(FailureEvent {
                        source_block: None,
                        work_id: None, attempt: 0,
                        at: self.playhead, start, resolver, played_by,
                        error: format!("emitted work was not admitted: {error}"),
                    });
                }
            }
        }
    }

    fn deferred_recipe(&self, idx: usize) -> Recipe {
        match &self.future[idx].cell.body {
            Body::Deferred(r) => r.clone(),
            Body::Concrete(_) => unreachable!("scheduled cells are deferred"),
        }
    }

    /// The most recent committed content **on `track`** at or before `before`.
    ///
    /// Lane-scoped by construction: `track` is the only lane key. A cell on
    /// another lane (or a legacy track-blind cell, were one ever present) can
    /// never satisfy this track's `UseLastGood` — the player principal is never
    /// consulted here, because the principal is never a lane key.
    fn last_committed_content_in(&self, track: &TrackId, before: Tick) -> Option<ContentRef> {
        self.committed
            .iter()
            .filter(|c| c.span.start <= before)
            .filter(|c| &c.track == track)
            .filter_map(|c| match &c.body {
                Body::Concrete(cref) => Some((c.span.start, cref.clone())),
                _ => None,
            })
            .max_by_key(|(t, _)| *t)
            .map(|(_, cref)| cref)
    }
}

/// The read-only committed view handed to a resolver. Holds only the committed
/// past + ambient — an uncommitted cell has no representation here, so a
/// speculation *cannot* read another speculation.
struct CommittedCtx<'a> {
    now: Tick,
    ambient: &'a HashMap<String, Vec<u8>>,
    committed: &'a [Cell],
}

impl ResolverCtx for CommittedCtx<'_> {
    fn now(&self) -> Tick {
        self.now
    }
    fn ambient(&self, key: &str) -> Option<Vec<u8>> {
        self.ambient.get(key).cloned()
    }
    /// Latest committed content across this timeline's lanes at or before the
    /// target tick. Model score admission uses this as part of its input basis.
    /// `last_committed_content_in` narrows the fallback read to one lane.
    fn content_before(&self, tick: Tick) -> Option<ContentRef> {
        self.committed
            .iter()
            .filter(|c| c.span.start <= tick)
            .filter_map(|c| match &c.body {
                Body::Concrete(cref) => Some((c.span.start, cref.clone())),
                _ => None,
            })
            .max_by_key(|(t, _)| *t)
            .map(|(_, cref)| cref)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cell::{ContextQuery, ResolverId};
    use crate::resolver::{ResolveError, Resolution};
    use crate::{SampleOutcome, Timing};
    use kaijutsu_types::Span;
    use serde_json::Value;

    /// A resolver whose content *and* basis are the ambient `beat` bytes — so
    /// changing `beat` between speculate and commit forces a basis divergence.
    struct EchoBeat {
        cost: Duration,
    }

    impl Resolver for EchoBeat {
        fn id(&self) -> ResolverId {
            ResolverId::new("echo")
        }
        fn estimate_cost(&self, _p: &Value, _ctx: &dyn ResolverCtx) -> Duration {
            self.cost
        }
        fn compute_basis(&self, _p: &Value, ctx: &dyn ResolverCtx) -> ContextHash {
            ContextHash::of(&ctx.ambient("beat").unwrap_or_default())
        }
        fn resolve(&self, p: &Value, ctx: &dyn ResolverCtx) -> crate::ResolveFuture {
            Box::pin(std::future::ready((|| {
                let beat = ctx.ambient("beat").unwrap_or_default();
                if p.get("fail_on").and_then(Value::as_str).map(str::as_bytes) == Some(beat.as_slice()) {
                    return Err(ResolveError::failed("changed input cannot resolve"));
                }
                Ok(Resolution::new(beat, "text/plain"))
            })()))
        }
    }

    /// A resolver whose basis is the validation tick `ctx.now()` — so it commits
    /// only if `speculate` and `commit_or_squash` agree on what `now` is. Pins
    /// SEV-2: both phases must validate at the cell's musical `start`. Registered
    /// under "echo" so it slots into `deferred_at`'s recipe.
    struct NowBasis;

    impl Resolver for NowBasis {
        fn id(&self) -> ResolverId {
            ResolverId::new("echo")
        }
        fn estimate_cost(&self, _p: &Value, _ctx: &dyn ResolverCtx) -> Duration {
            Duration::from_secs(3)
        }
        fn compute_basis(&self, _p: &Value, ctx: &dyn ResolverCtx) -> ContextHash {
            ContextHash::of(&ctx.now().get().to_le_bytes())
        }
        fn resolve(&self, _p: &Value, _ctx: &dyn ResolverCtx) -> crate::ResolveFuture {
            Box::pin(std::future::ready((|| {
                Ok(Resolution::new(b"X".to_vec(), "text/plain"))
            })()))
        }
    }

    fn deferred_at(start: i64, fallback: Fallback) -> Cell {
        deferred_at_track(start, fallback, TrackId::solo())
    }

    /// Like `deferred_at`, but on an explicit lane — drives the cross-track
    /// `UseLastGood` tests. `played_by` is irrelevant to these tests, so it
    /// defaults to `PrincipalId::beat()` (the author axis, independent of the
    /// `track` lane — the two are distinct coordinates).
    fn deferred_at_track(start: i64, fallback: Fallback, track: TrackId) -> Cell {
        Cell::deferred_on(
            Span::instant(Tick::new(start)),
            Recipe {
                resolver: ResolverId::new("echo"),
                params: Value::Null,
                query: ContextQuery::default(),
                fallback,
            },
            track,
            PrincipalId::beat(),
        )
    }

    fn concrete_hash(bytes: &[u8]) -> ContentHash {
        ContentRef::of(bytes, "text/plain").hash
    }

    /// Clean commit: the predicted context still holds at the deadline.
    #[test]
    fn commits_when_context_holds() {
        let clock = TickClock {
            ticks_per_sec: 1.0,
            safety_factor: 2.0,
            commit_margin: TickDelta::new(2),
        };
        let mut tl = Timeline::new(clock);
        tl.register_resolver(Box::new(EchoBeat {
            cost: Duration::from_secs(3),
        }));
        tl.set_ambient("beat", *b"A");

        // start=100 → lead=beats_for(6s)=6 → speculate_at=94; commit_deadline=98.
        tl.schedule(deferred_at(100, Fallback::Skip)).unwrap();

        tl.advance_to(Tick::new(94)); // speculate against beat="A"
        tl.advance_to(Tick::new(100)); // commit_deadline at 98 — basis holds

        assert!(tl.squashes().is_empty(), "no misprediction expected");
        assert_eq!(tl.committed().len(), 1);
        let cell = &tl.committed()[0];
        assert_eq!(cell.state, CellState::Committed);
        match &cell.body {
            Body::Concrete(cref) => {
                assert_eq!(cref.hash, concrete_hash(b"A"));
                // crystallized to CAS at commit
                assert_eq!(tl.content_bytes(&cref.hash), Some(b"A".as_slice()));
            }
            _ => panic!("committed cell must be concrete"),
        }
    }

    /// Work status puts the estimate beside what happened: the estimated cost
    /// and the tick preparation was planned for, then the ticks it started and
    /// was ready at. A player compares them to see whether the estimate held.
    #[test]
    fn status_reports_the_estimate_beside_the_observed_readiness() {
        let clock = TickClock {
            ticks_per_sec: 1.0,
            safety_factor: 2.0,
            commit_margin: TickDelta::new(2),
        };
        let mut tl = Timeline::new(clock);
        tl.register_resolver(Box::new(EchoBeat {
            cost: Duration::from_secs(3),
        }));
        tl.set_ambient("beat", *b"A");

        // start=100, cost 3 s at 1 tick/s, lead 6 → preparation planned at 94.
        let id = tl.schedule(deferred_at(100, Fallback::Skip)).unwrap();
        let queued = tl.status(id).unwrap().clone();
        assert_eq!(queued.estimate, TickDelta::new(3));
        assert_eq!(queued.prepare_at, Tick::new(94));
        assert_eq!(queued.started_at, None);

        tl.advance_to(Tick::new(94));
        tl.advance_to(Tick::new(100));

        let settled = tl.status(id).unwrap();
        assert_eq!(settled.estimate, TickDelta::new(3));
        assert_eq!(settled.prepare_at, Tick::new(94));
        assert_eq!(settled.started_at, Some(Tick::new(94)));
        assert_eq!(settled.ready_at, Some(Tick::new(94)));
        assert!(matches!(settled.disposition, Some(Disposition::Committed { .. })));
    }

    /// SEV-2 (gemini-pro Stage-3 review): `commit_or_squash` must validate the
    /// basis at the cell's musical `start` — the SAME tick `speculate` used — not at
    /// the earlier commit-deadline playhead. A resolver whose basis reads `ctx.now()`
    /// would otherwise always see predicted(start) ≠ actual(deadline) and squash
    /// forever, even in a context that never changed. In a stable context this cell
    /// must COMMIT with no squash.
    #[test]
    fn commit_validates_at_start_not_the_commit_deadline() {
        let clock = TickClock {
            ticks_per_sec: 1.0,
            safety_factor: 2.0,
            commit_margin: TickDelta::new(2), // deadline = start − 2, strictly before start
        };
        let mut tl = Timeline::new(clock);
        tl.register_resolver(Box::new(NowBasis));
        tl.schedule(deferred_at(100, Fallback::Skip)).unwrap();

        tl.advance_to(Tick::new(94)); // speculate at start=100
        tl.advance_to(Tick::new(100)); // crosses commit_deadline=98; must re-validate at start=100

        assert!(
            tl.squashes().is_empty(),
            "a now()-based basis in a stable context must not squash (validate at start, not the deadline)",
        );
        assert_eq!(tl.committed().len(), 1, "the cell commits cleanly");
    }

    /// `set_clock` re-derives the speculation lead for cells scheduled after it
    /// (WI 2): a faster clock leads further ahead in ticks for the same wall-clock
    /// estimate, so the same `start` speculates earlier. Cells already in the open
    /// future keep their original lead.
    #[test]
    fn set_clock_reslaves_the_speculation_lead() {
        // Clock A: 1 tick/sec, safety 2.0 → est 3s ⇒ lead beats_for(6s)=6 ⇒
        // a cell at start=100 speculates at 94.
        let mut tl = Timeline::new(TickClock {
            ticks_per_sec: 1.0,
            safety_factor: 2.0,
            commit_margin: TickDelta::new(2),
        });
        tl.register_resolver(Box::new(EchoBeat {
            cost: Duration::from_secs(3),
        }));
        tl.schedule(deferred_at(100, Fallback::Skip)).unwrap();
        assert_eq!(
            tl.scheduled_speculate_at(0),
            Some(Tick::new(94)),
            "clock A: speculate_at = start(100) − lead(6)"
        );

        // Faster clock B: 2 ticks/sec → est 3s ⇒ lead beats_for(6s)=12 ⇒ a cell at
        // start=101 speculates at 89. Proves the new schedule used B, not stale A.
        tl.set_clock(TickClock {
            ticks_per_sec: 2.0,
            safety_factor: 2.0,
            commit_margin: TickDelta::new(2),
        });
        tl.schedule(deferred_at(101, Fallback::Skip)).unwrap();
        assert_eq!(
            tl.scheduled_speculate_at(1),
            Some(Tick::new(89)),
            "clock B re-slaved: speculate_at = start(101) − lead(12)"
        );
        // The pre-existing cell kept clock A's lead — not re-priced mid-flight.
        assert_eq!(
            tl.scheduled_speculate_at(0),
            Some(Tick::new(94)),
            "already-scheduled cell keeps its original lead"
        );
    }

    /// Squash with no recovery budget → the required fallback fires. The miss is
    /// recorded with predicted ≠ actual.
    #[test]
    fn squashes_and_falls_back_when_no_budget() {
        let clock = TickClock {
            ticks_per_sec: 1.0,
            safety_factor: 2.0,
            commit_margin: TickDelta::new(1), // deadline=99, budget at squash = 1
        };
        let mut tl = Timeline::new(clock);
        tl.register_resolver(Box::new(EchoBeat {
            cost: Duration::from_secs(3), // est_cost=3 ticks > budget 1 → no re-spec
        }));
        tl.set_ambient("beat", *b"A");

        let silence = ContentRef::of(b"SILENCE", "text/plain");
        tl.schedule(deferred_at(100, Fallback::Literal(silence.clone())))
            .unwrap();

        tl.advance_to(Tick::new(94)); // speculate against "A"
        tl.set_ambient("beat", *b"B"); // context diverges before commit
        tl.advance_to(Tick::new(100)); // commit_deadline=99 → squash → fallback

        assert_eq!(tl.squashes().len(), 1);
        let sq = &tl.squashes()[0];
        assert_eq!(sq.recovery, Recovery::FellBack);
        assert_eq!(sq.predicted, ContextHash::of(b"A"));
        assert_eq!(sq.actual, ContextHash::of(b"B"));
        assert_ne!(sq.predicted, sq.actual);

        // The fallback literal landed — never undefined behavior.
        assert_eq!(tl.committed().len(), 1);
        match &tl.committed()[0].body {
            Body::Concrete(cref) => assert_eq!(cref.hash, silence.hash),
            _ => panic!("fallback must commit concrete content"),
        }
    }

    /// Squash with budget remaining → re-speculate against the new context, then
    /// commit the corrected content at `start`.
    #[test]
    fn squashes_then_respeculates_and_commits() {
        let clock = TickClock {
            ticks_per_sec: 1.0,
            safety_factor: 2.0,
            commit_margin: TickDelta::new(3), // deadline=97, budget at squash = 3
        };
        let mut tl = Timeline::new(clock);
        tl.register_resolver(Box::new(EchoBeat {
            cost: Duration::from_secs(2), // est_cost=2 ≤ budget 3 → re-speculate
        }));
        tl.set_ambient("beat", *b"A");

        tl.schedule(deferred_at(100, Fallback::Skip)).unwrap();

        tl.advance_to(Tick::new(96)); // lead=beats_for(4s)=4 → speculate_at=96, against "A"
        tl.set_ambient("beat", *b"B"); // diverges
        tl.advance_to(Tick::new(97)); // retry while budget remains
        tl.advance_to(Tick::new(100)); // final validation

        assert_eq!(tl.squashes().len(), 1);
        assert_eq!(tl.squashes()[0].recovery, Recovery::ReSpeculated);

        assert_eq!(tl.committed().len(), 1);
        match &tl.committed()[0].body {
            Body::Concrete(cref) => {
                assert_eq!(cref.hash, concrete_hash(b"B"), "corrected content commits");
                assert_eq!(tl.content_bytes(&cref.hash), Some(b"B".as_slice()));
            }
            _ => panic!("re-speculated content must commit concrete"),
        }
    }

    #[test]
    fn rejects_scheduling_in_the_past() {
        let mut tl = Timeline::new(TickClock::default());
        tl.register_resolver(Box::new(EchoBeat {
            cost: Duration::from_secs(1),
        }));
        tl.advance_to(Tick::new(50));
        let err = tl.schedule(deferred_at(10, Fallback::Skip)).unwrap_err();
        assert!(matches!(err, ScheduleError::InThePast { .. }));
    }

    /// A resolver that emits one concrete sibling cell on commit. The sibling's
    /// (track, played_by) are supplied by the test so it can exercise both the
    /// matching-inheritance and the deliberate-mismatch paths.
    struct EmitSibling {
        sibling_track: TrackId,
        sibling_player: PrincipalId,
        sibling_tick: i64,
    }

    impl Resolver for EmitSibling {
        fn id(&self) -> ResolverId {
            ResolverId::new("emit")
        }
        fn estimate_cost(&self, _p: &Value, _c: &dyn ResolverCtx) -> Duration {
            Duration::from_secs(1)
        }
        fn compute_basis(&self, _p: &Value, _c: &dyn ResolverCtx) -> ContextHash {
            ContextHash::of(b"stable")
        }
        fn resolve(&self, _p: &Value, _c: &dyn ResolverCtx) -> crate::ResolveFuture {
            Box::pin(std::future::ready((|| {
                let sibling = Cell::concrete_on(
                    Span::instant(Tick::new(self.sibling_tick)),
                    ContentRef::of(b"sibling", "text/plain"),
                    self.sibling_track.clone(),
                    self.sibling_player,
                );
                Ok(Resolution::new(b"parent".to_vec(), "text/plain").with_emitted(vec![sibling]))
            })()))
        }
    }

    fn emit_recipe() -> Recipe {
        Recipe {
            resolver: ResolverId::new("emit"),
            params: Value::Null,
            query: ContextQuery::default(),
            fallback: Fallback::Skip,
        }
    }

    /// A resolver whose `resolve` always errors with a fixed message — drives the
    /// failure-ledger path (T5). `estimate_cost`/`compute_basis` are trivially
    /// stable so the cell reaches `speculate` cleanly before the resolve fails.
    struct AlwaysFails {
        message: &'static str,
    }

    impl Resolver for AlwaysFails {
        fn id(&self) -> ResolverId {
            ResolverId::new("always_fails")
        }
        fn estimate_cost(&self, p: &Value, _c: &dyn ResolverCtx) -> Duration {
            Duration::from_secs(p.get("cost").and_then(Value::as_u64).unwrap_or(1))
        }
        fn compute_basis(&self, _p: &Value, _c: &dyn ResolverCtx) -> ContextHash {
            ContextHash::of(b"stable")
        }
        fn resolve(&self, _p: &Value, _c: &dyn ResolverCtx) -> crate::ResolveFuture {
            Box::pin(std::future::ready((|| {
                Err(ResolveError::failed(self.message))
            })()))
        }
    }

    /// T5 — a resolve error records a [`FailureEvent`] (carrying the error string)
    /// retrievable via `failures()`, removes the cell from the open future (no
    /// zombie), and commits nothing (a hole, never a fake). The state-machine
    /// asserts stay intact: the cell passes through `Failed` before removal.
    #[test]
    fn resolve_failure_records_event_and_removes_cell() {
        let clock = TickClock {
            ticks_per_sec: 1.0,
            safety_factor: 2.0, // cost 1s → lead 2 → speculate_at = start - 2
            commit_margin: TickDelta::new(1),
        };
        let mut tl = Timeline::new(clock);
        tl.register_resolver(Box::new(AlwaysFails {
            message: "CAS read failed: missing entry",
        }));
        let track = TrackId::new("solo-lane").unwrap();
        let cell = Cell::deferred_on(
            Span::instant(Tick::new(20)),
            Recipe {
                resolver: ResolverId::new("always_fails"),
                params: Value::Null,
                query: ContextQuery::default(),
                fallback: Fallback::Skip,
            },
            track,
            PrincipalId::beat(),
        );
        tl.schedule(cell).unwrap();

        // Drive past speculate_at (18): resolve errors → ledger + removal.
        tl.advance_to(Tick::new(20));

        // The ledger holds exactly one event, carrying the resolver's error text.
        let fails = tl.failures();
        assert_eq!(fails.len(), 1, "the erring resolve records exactly one event");
        let ev = &fails[0];
        assert_eq!(ev.start, Tick::new(20), "event carries the cell's start tick");
        assert_eq!(ev.resolver, ResolverId::new("always_fails"));
        assert!(
            ev.error.contains("CAS read failed"),
            "the resolver's error string is preserved: {:?}",
            ev.error
        );

        // No zombie: the cell is gone from the open future, so the playhead can
        // never re-trip it.
        assert_eq!(
            tl.future_len(),
            0,
            "the failed cell is removed from the open future — no zombie"
        );

        // A hole, never a fake: nothing committed.
        assert!(
            tl.committed().is_empty(),
            "a failed resolve leaves a hole, never a phantom commit"
        );

        // Advancing again must not re-record: the cell is truly gone.
        tl.advance_to(Tick::new(40));
        assert_eq!(tl.failures().len(), 1, "no zombie re-firing on later beats");
    }

    #[test]
    fn failed_producer_uses_declared_fallback_at_commitment() {
        let literal = ContentRef::of(b"declared silence", "text/plain");
        for fallback in [Fallback::Skip, Fallback::UseLastGood, Fallback::Literal(literal.clone())] {
            let mut tl = Timeline::new(TickClock {
                ticks_per_sec: 1.0,
                safety_factor: 2.0,
                commit_margin: TickDelta::new(1),
            });
            tl.register_resolver(Box::new(AlwaysFails { message: "producer failed" }));
            tl.register_resolver(Box::new(EchoBeat { cost: Duration::from_secs(1) }));
            let track = TrackId::solo();
            let producer = PrincipalId::new();
            tl.schedule(Cell::deferred_on(
                Span::instant(Tick::new(12)),
                Recipe {
                    resolver: ResolverId::new("always_fails"),
                    params: serde_json::json!({"cost": 5}),
                    query: ContextQuery::default(),
                    fallback: fallback.clone(),
                },
                track.clone(),
                producer,
            )).unwrap();

            // The failed producer reports now, but another producer can still
            // supply the lane's last good phrase before this commitment.
            tl.advance_to(Tick::new(2));
            assert_eq!(tl.failures().len(), 1);
            assert_eq!(tl.failures()[0].at, Tick::new(2));
            assert_eq!(tl.failures()[0].played_by, producer);
            assert_eq!(tl.future_len(), 1, "the declared fallback still owns its deadline");
            assert!(tl.committed().is_empty());

            tl.set_ambient("beat", b"accepted phrase".to_vec());
            tl.schedule(deferred_at_track(6, Fallback::Skip, track.clone())).unwrap();
            tl.advance_to(Tick::new(6));
            let last_good = match &tl.committed()[0].body {
                Body::Concrete(content) => content.clone(),
                _ => panic!("accepted phrase must be concrete"),
            };
            tl.set_ambient("beat", b"another lane".to_vec());
            tl.schedule(deferred_at_track(9, Fallback::Skip, TrackId::new("other").unwrap())).unwrap();
            tl.advance_to(Tick::new(9));
            tl.advance_to(Tick::new(10));
            assert_eq!(tl.committed().len(), 2, "no fallback before its deadline");

            tl.advance_to(Tick::new(11));
            let expected = match fallback {
                Fallback::Skip => None,
                Fallback::UseLastGood => Some(last_good),
                Fallback::Literal(content) => Some(content),
            };
            if let Some(content) = expected {
                assert_eq!(tl.committed().len(), 3);
                let accepted = &tl.committed()[2];
                assert_eq!(accepted.span.start, Tick::new(12));
                assert_eq!(accepted.body, Body::Concrete(content));
                assert_eq!(accepted.track, track);
                assert_eq!(accepted.played_by, PrincipalId::beat());
            } else {
                assert_eq!(tl.committed().len(), 2);
            }
            assert_eq!(tl.future_len(), 0);
            let committed = tl.committed().to_vec();
            tl.advance_to(Tick::new(30));
            assert_eq!(tl.committed(), committed);
            assert_eq!(tl.failures().len(), 1, "failure and fallback both settle once");
        }
    }

    #[test]
    fn late_admission_never_rewinds_failure_or_commitment() {
        let mut tl = Timeline::new(TickClock {
            ticks_per_sec: 1.0,
            safety_factor: 2.0,
            commit_margin: TickDelta::new(4),
        });
        tl.register_resolver(Box::new(AlwaysFails { message: "late failure" }));
        tl.advance_to(Tick::new(10));
        tl.schedule(Cell::deferred_on(
            Span::instant(Tick::new(11)),
            Recipe {
                resolver: ResolverId::new("always_fails"),
                params: Value::Null,
                query: ContextQuery::default(),
                fallback: Fallback::Skip,
            },
            TrackId::solo(),
            PrincipalId::beat(),
        )).unwrap();
        tl.advance_to(Tick::new(11));
        assert_eq!(tl.failures()[0].at, Tick::new(10), "overdue preparation starts when admitted");
        assert_eq!(tl.playhead(), Tick::new(11));
        assert_eq!(tl.future_len(), 0);
    }

    #[test]
    fn failed_respeculation_keeps_final_deadline_and_discards_old_output() {
        let mut tl = Timeline::new(TickClock {
            ticks_per_sec: 1.0,
            safety_factor: 3.0,
            commit_margin: TickDelta::new(2),
        });
        tl.register_resolver(Box::new(EchoBeat { cost: Duration::from_secs(1) }));
        let literal = ContentRef::of(b"declared fallback", "text/plain");
        tl.schedule(Cell::deferred_on(
            Span::instant(Tick::new(100)),
            Recipe {
                resolver: ResolverId::new("echo"),
                params: serde_json::json!({"fail_on": "B"}),
                query: ContextQuery::default(),
                fallback: Fallback::Literal(literal.clone()),
            },
            TrackId::solo(),
            PrincipalId::new(),
        )).unwrap();
        tl.set_ambient("beat", b"A".to_vec());
        tl.advance_to(Tick::new(97));
        tl.set_ambient("beat", b"B".to_vec());
        tl.advance_to(Tick::new(98));
        assert_eq!(tl.squashes()[0].recovery, Recovery::ReSpeculated);
        assert_eq!(tl.failures()[0].at, Tick::new(98));
        assert_eq!(tl.future_len(), 1);
        tl.advance_to(Tick::new(99));
        assert!(tl.committed().is_empty(), "the final attempt owns the start deadline");
        tl.advance_to(Tick::new(100));
        assert_eq!(tl.committed().len(), 1);
        assert_eq!(tl.committed()[0].body, Body::Concrete(literal));
        assert_eq!(tl.committed()[0].span.start, Tick::new(100));
        assert_eq!(tl.committed()[0].played_by, PrincipalId::beat());
        assert!(tl.content_bytes(&ContentRef::of(b"A", "text/plain").hash).is_none());
        tl.advance_to(Tick::new(101));
        assert_eq!(tl.failures().len(), 1);
        assert_eq!(tl.squashes().len(), 1);
        assert_eq!(tl.committed().len(), 1);
        assert_eq!(tl.future_len(), 0);
    }

    #[test]
    fn final_attempt_divergence_falls_back_without_a_third_attempt() {
        let mut tl = Timeline::new(TickClock {
            ticks_per_sec: 1.0,
            safety_factor: 3.0,
            commit_margin: TickDelta::new(2),
        });
        tl.register_resolver(Box::new(EchoBeat { cost: Duration::from_secs(1) }));
        let literal = ContentRef::of(b"fallback", "text/plain");
        tl.schedule(deferred_at(100, Fallback::Literal(literal.clone()))).unwrap();
        tl.set_ambient("beat", b"A".to_vec());
        tl.advance_to(Tick::new(97));
        tl.set_ambient("beat", b"B".to_vec());
        tl.advance_to(Tick::new(98));
        tl.set_ambient("beat", b"C".to_vec());
        tl.advance_to(Tick::new(100));
        assert_eq!(tl.squashes().len(), 2, "each failed basis check records its own decision");
        assert_eq!(tl.squashes()[0].recovery, Recovery::ReSpeculated);
        assert_eq!(tl.squashes()[1].recovery, Recovery::FellBack);
        assert_eq!(tl.committed().len(), 1);
        assert_eq!(tl.committed()[0].body, Body::Concrete(literal));
        assert_eq!(tl.committed()[0].span.start, Tick::new(100));
        assert!(tl.failures().is_empty());
        assert_eq!(tl.future_len(), 0);
        tl.advance_to(Tick::new(110));
        assert_eq!(tl.committed().len(), 1);
        assert_eq!(tl.squashes().len(), 2);
    }

    #[test]
    fn equal_deadlines_keep_admission_order_after_an_unrelated_cancel() {
        let mut tl = Timeline::new(TickClock::default());
        tl.register_resolver(Box::new(AlwaysFails { message: "controlled" }));
        let make = |at, fallback| Cell::deferred_on(Span::instant(Tick::new(at)), Recipe {
            resolver: ResolverId::new("always_fails"),
            params: serde_json::json!({"cost": 10}),
            query: ContextQuery::default(), fallback,
        }, TrackId::solo(), PrincipalId::beat());
        let unrelated = tl.schedule(make(100, Fallback::Skip)).unwrap();
        tl.schedule(make(10, Fallback::Literal(ContentRef::of(b"first", "text/plain")))).unwrap();
        let latest = ContentRef::of(b"second", "text/plain");
        tl.schedule(make(10, Fallback::Literal(latest.clone()))).unwrap();
        tl.schedule(make(10, Fallback::UseLastGood)).unwrap();
        tl.cancel(unrelated);
        tl.advance_to(Tick::new(9));
        assert_eq!(tl.committed().len(), 3);
        assert_eq!(tl.committed()[2].body, Body::Concrete(latest));
    }

    /// T11 — the locked two-track cross-contamination test. Track B commits good
    /// content; track A's UseLastGood misses with an empty A-history and must NOT
    /// pick up B's content. Nothing is committed for A.
    #[test]
    fn use_last_good_does_not_cross_tracks() {
        let clock = TickClock {
            ticks_per_sec: 1.0,
            safety_factor: 2.0, // cost 3s → lead 6
            commit_margin: TickDelta::new(1),
        };
        let mut tl = Timeline::new(clock);
        tl.register_resolver(Box::new(EchoBeat {
            cost: Duration::from_secs(3), // est_cost 3 ticks
        }));
        let track_a = TrackId::new("a").unwrap();
        let track_b = TrackId::new("b").unwrap();

        tl.set_ambient("beat", *b"A");
        // B at 20: speculate_at 14, deadline 19. A at 30: speculate_at 24, deadline 29.
        tl.schedule(deferred_at_track(20, Fallback::Skip, track_b.clone()))
            .unwrap();
        tl.schedule(deferred_at_track(30, Fallback::UseLastGood, track_a.clone()))
            .unwrap();

        tl.advance_to(Tick::new(19)); // B is ready and commits while useful
        tl.advance_to(Tick::new(24)); // A starts against beat=A
        assert_eq!(tl.committed().len(), 1, "only B has committed so far");
        assert_eq!(tl.committed()[0].track, track_b);

        tl.set_ambient("beat", *b"Z"); // diverge before A's deadline
        tl.advance_to(Tick::new(30)); // A deadline@29: budget 1 < est 3 → squash → UseLastGood

        // A's lane has no history → Skip (silence). B's content is NOT duplicated
        // under A: principal is never a lane key, track is the only lane identity.
        assert_eq!(tl.squashes().len(), 1);
        assert_eq!(tl.committed().len(), 1, "A committed nothing — empty lane → Skip");
        assert_eq!(tl.committed()[0].track, track_b);
        assert!(
            tl.committed().iter().all(|c| c.track != track_a),
            "no cell on track A; B's last-good must not cross lanes"
        );
    }

    /// T12 — the locked empty-track → Skip pin. A single track with zero history
    /// firing UseLastGood resolves to silence: committed stays empty, no panic,
    /// the playhead passes the hole.
    #[test]
    fn use_last_good_on_empty_track_resolves_to_skip() {
        let clock = TickClock {
            ticks_per_sec: 1.0,
            safety_factor: 2.0,
            commit_margin: TickDelta::new(1),
        };
        let mut tl = Timeline::new(clock);
        tl.register_resolver(Box::new(EchoBeat {
            cost: Duration::from_secs(3),
        }));
        let track = TrackId::new("solo-lane").unwrap();
        tl.set_ambient("beat", *b"A");
        tl.schedule(deferred_at_track(20, Fallback::UseLastGood, track.clone()))
            .unwrap();

        tl.advance_to(Tick::new(14)); // speculate@14 against beat=A
        tl.set_ambient("beat", *b"Z"); // diverge
        tl.advance_to(Tick::new(20)); // deadline@19: budget 1 < est 3 → squash → UseLastGood → Skip

        assert_eq!(tl.squashes().len(), 1);
        assert!(
            tl.committed().is_empty(),
            "empty-track UseLastGood resolves to Skip (silence), not a panic or a phantom commit"
        );
        assert_eq!(tl.playhead(), Tick::new(20), "playhead passes the hole");
    }

    /// T13 — emitted cells inherit the committing parent's track + played_by.
    #[test]
    fn emitted_cells_inherit_parent_track_and_played_by() {
        let clock = TickClock {
            ticks_per_sec: 1.0,
            safety_factor: 2.0,
            commit_margin: TickDelta::new(1),
        };
        let parent_track = TrackId::new("keys").unwrap();
        let parent_player = PrincipalId::system();

        let mut tl = Timeline::new(clock);
        // The sibling is constructed already on the parent's lane + player (the
        // contract-respecting case).
        tl.register_resolver(Box::new(EmitSibling {
            sibling_track: parent_track.clone(),
            sibling_player: parent_player,
            sibling_tick: 25, // a recorded memory beside the parent
        }));

        let parent = Cell::deferred_on(
            Span::instant(Tick::new(20)),
            emit_recipe(),
            parent_track.clone(),
            parent_player,
        );
        tl.schedule(parent).unwrap();
        tl.advance_to(Tick::new(20)); // parent commits; sibling absorbed

        // Parent + sibling both committed, both on the parent's lane + player.
        assert_eq!(tl.committed().len(), 2);
        for c in tl.committed() {
            assert_eq!(c.track, parent_track, "every committed cell on the parent lane");
            assert_eq!(c.played_by, parent_player, "every committed cell carries the parent player");
        }
    }

    /// T13 (loud half) — an emission whose track diverges from the committing
    /// parent trips the debug_assert. In release this is `tracing::error` + a
    /// stamp-with-parent (never silent normalization). Debug test builds panic.
    #[test]
    #[should_panic(expected = "emitted cell track must match")]
    fn emitted_cell_track_mismatch_is_loud() {
        let parent_track = TrackId::new("keys").unwrap();
        let mut tl = Timeline::new(TickClock {
            ticks_per_sec: 1.0,
            safety_factor: 2.0,
            commit_margin: TickDelta::new(1),
        });
        tl.register_resolver(Box::new(EmitSibling {
            sibling_track: TrackId::new("wrong-lane").unwrap(), // deliberate mismatch
            sibling_player: PrincipalId::beat(),
            sibling_tick: 25,
        }));
        let parent = Cell::deferred_on(
            Span::instant(Tick::new(20)),
            emit_recipe(),
            parent_track,
            PrincipalId::beat(),
        );
        tl.schedule(parent).unwrap();
        tl.advance_to(Tick::new(20)); // commit → emitted-stamp path trips the assert
    }

    /// T13 (player-mismatch half) — an emission whose track matches the parent but
    /// whose `played_by` diverges is equally loud: the player-axis debug_assert
    /// trips in debug, and in release the error log carries both player fields so
    /// the operator can tell a player-only mismatch from a track mismatch.
    #[test]
    #[should_panic(expected = "emitted cell played_by must match")]
    fn emitted_cell_player_mismatch_is_loud() {
        let parent_track = TrackId::new("keys").unwrap();
        let parent_player = PrincipalId::system();
        let mut tl = Timeline::new(TickClock {
            ticks_per_sec: 1.0,
            safety_factor: 2.0,
            commit_margin: TickDelta::new(1),
        });
        tl.register_resolver(Box::new(EmitSibling {
            sibling_track: parent_track.clone(), // track MATCHES the parent
            sibling_player: PrincipalId::beat(),  // player DIVERGES from the parent
            sibling_tick: 25,
        }));
        let parent = Cell::deferred_on(
            Span::instant(Tick::new(20)),
            emit_recipe(),
            parent_track,
            parent_player,
        );
        tl.schedule(parent).unwrap();
        tl.advance_to(Tick::new(20)); // commit → emitted-stamp path trips the player assert
    }

    /// T14 — seed_playhead is virgin-only. A fresh timeline seeds OK; a seed after
    /// any schedule or commit is Err(SeedError::NotVirgin) — crash over corruption.
    #[test]
    fn seed_playhead_errs_on_non_virgin() {
        // Virgin: seeding succeeds and positions the playhead.
        let mut tl = Timeline::new(TickClock::default());
        assert!(tl.seed_playhead(Tick::new(42)).is_ok());
        assert_eq!(tl.playhead(), Tick::new(42));

        // After a schedule, the timeline is no longer virgin (open future).
        let mut tl2 = Timeline::new(TickClock::default());
        tl2.register_resolver(Box::new(EchoBeat {
            cost: Duration::from_secs(1),
        }));
        tl2.schedule(deferred_at(10, Fallback::Skip)).unwrap();
        assert!(matches!(
            tl2.seed_playhead(Tick::new(5)),
            Err(SeedError::NotVirgin { .. })
        ));

        // After a commit, likewise non-virgin (committed log non-empty).
        let mut tl3 = Timeline::new(TickClock {
            ticks_per_sec: 1.0,
            safety_factor: 2.0,
            commit_margin: TickDelta::new(1),
        });
        tl3.register_resolver(Box::new(EchoBeat {
            cost: Duration::from_secs(1),
        }));
        tl3.set_ambient("beat", *b"A");
        tl3.schedule(deferred_at(10, Fallback::Skip)).unwrap();
        tl3.advance_to(Tick::new(10));
        assert_eq!(tl3.committed().len(), 1);
        assert!(matches!(
            tl3.seed_playhead(Tick::new(3)),
            Err(SeedError::NotVirgin { .. })
        ));
    }

    /// A producer that reports the wall-clock time its params declare (or the
    /// ambient `compute_ms`, when set), fails when `fail` is set, never
    /// finishes when `hang` is set, and takes its basis from ambient `beat`.
    struct Timed;

    impl Resolver for Timed {
        fn id(&self) -> ResolverId { ResolverId::new("timed") }
        fn estimate_cost(&self, p: &Value, _c: &dyn ResolverCtx) -> Duration {
            Duration::from_millis(p["cost_ms"].as_u64().unwrap_or(1000))
        }
        fn compute_basis(&self, _p: &Value, c: &dyn ResolverCtx) -> ContextHash {
            ContextHash::of(&c.ambient("beat").unwrap_or_default())
        }
        fn resolve(&self, p: &Value, c: &dyn ResolverCtx) -> crate::ResolveFuture {
            if p["hang"].as_bool().unwrap_or(false) {
                return Box::pin(std::future::pending());
            }
            let ambient = c.ambient("compute_ms")
                .map(|b| String::from_utf8(b).unwrap().parse::<u64>().unwrap());
            let timing = Timing {
                queued: Duration::from_millis(p["queued_ms"].as_u64().unwrap_or(0)),
                compute: Duration::from_millis(ambient.or(p["compute_ms"].as_u64()).unwrap_or(0)),
            };
            let result = if p["fail"].as_bool().unwrap_or(false) {
                Err(ResolveError::failed("timed failure").with_timing(timing))
            } else {
                Ok(Resolution::new(b"T".to_vec(), "text/plain").with_timing(timing))
            };
            Box::pin(std::future::ready(result))
        }
    }

    fn timed_at(start: i64, params: Value) -> Cell {
        Cell::deferred_on(
            Span::instant(Tick::new(start)),
            Recipe {
                resolver: ResolverId::new("timed"),
                params,
                query: ContextQuery::default(),
                fallback: Fallback::Skip,
            },
            TrackId::solo(),
            PrincipalId::beat(),
        )
    }

    fn timed_timeline() -> Timeline {
        let mut tl = Timeline::new(TickClock {
            ticks_per_sec: 1.0,
            safety_factor: 1.0,
            commit_margin: TickDelta::new(0),
        });
        tl.register_resolver(Box::new(Timed));
        tl
    }

    /// The producer's measured time reaches the work status beside the
    /// estimate in the same unit, for failures as well as results, and each
    /// attempt adds one sample to its resolver's window.
    #[test]
    fn producer_timing_reaches_status_and_the_cost_window() {
        let mut tl = timed_timeline();
        let ok = tl.schedule(timed_at(10, serde_json::json!({
            "cost_ms": 2000, "queued_ms": 5, "compute_ms": 1500,
        }))).unwrap();
        let failed = tl.schedule(timed_at(12, serde_json::json!({
            "cost_ms": 1000, "compute_ms": 3000, "fail": true,
        }))).unwrap();
        for tick in 1..=12 {
            tl.advance_to(Tick::new(tick));
        }
        assert!(matches!(tl.status(ok).unwrap().disposition, Some(Disposition::Committed { .. })));

        let ok = tl.status(ok).unwrap();
        assert_eq!(ok.estimate_wall, Duration::from_millis(2000));
        assert_eq!(ok.timing, Some(Timing { queued: Duration::from_millis(5), compute: Duration::from_millis(1500) }));
        let json = serde_json::to_value(ok).unwrap();
        assert_eq!(json["estimate_ms"], 2000.0);
        assert_eq!(json["timing"]["queued_ms"], 5.0);
        assert_eq!(json["timing"]["compute_ms"], 1500.0);
        let back: WorkStatus = serde_json::from_value(json).unwrap();
        assert_eq!(back.timing, ok.timing);
        assert_eq!(back.estimate_wall, ok.estimate_wall);

        let failed = tl.status(failed).unwrap();
        assert_eq!(failed.timing.unwrap().compute, Duration::from_millis(3000));
        assert_eq!(failed.error.as_deref(), Some("timed failure"));

        let samples: Vec<_> = tl.cost_samples(&ResolverId::new("timed")).collect();
        assert_eq!(samples.len(), 2);
        assert_eq!(samples[0].outcome, SampleOutcome::Ready);
        assert_eq!(samples[0].timing.compute, Duration::from_millis(1500));
        assert_eq!(samples[0].estimate, Duration::from_millis(2000));
        assert_eq!(samples[1].outcome, SampleOutcome::Failed);
        assert_eq!(samples[1].timing.compute, Duration::from_millis(3000));
        assert_eq!(samples[1].played_by, PrincipalId::beat());
        assert_eq!(tl.cost_samples(&ResolverId::new("echo")).count(), 0);
    }

    /// Work that reports no timing leaves the status empty and adds no sample:
    /// an unmeasured attempt is not a zero-cost one.
    #[test]
    fn unmeasured_work_adds_no_sample() {
        let mut tl = timed_timeline();
        tl.register_resolver(Box::new(EchoBeat { cost: Duration::from_secs(1) }));
        let id = tl.schedule(deferred_at(5, Fallback::Skip)).unwrap();
        tl.advance_to(Tick::new(5));
        assert_eq!(tl.status(id).unwrap().timing, None);
        assert_eq!(tl.cost_samples(&ResolverId::new("echo")).count(), 0);
    }

    /// A retry after a squash reports its own timing, and both attempts are
    /// samples: the squashed attempt still cost what it cost.
    #[test]
    fn a_respeculated_attempt_reports_its_own_timing() {
        let mut tl = Timeline::new(TickClock {
            ticks_per_sec: 1.0,
            safety_factor: 2.0,
            commit_margin: TickDelta::new(3),
        });
        tl.register_resolver(Box::new(Timed));
        tl.set_ambient("beat", *b"A");
        tl.set_ambient("compute_ms", *b"100");
        let id = tl.schedule(timed_at(100, serde_json::json!({ "cost_ms": 2000 }))).unwrap();
        tl.advance_to(Tick::new(96));
        tl.set_ambient("beat", *b"B");
        tl.set_ambient("compute_ms", *b"250");
        tl.advance_to(Tick::new(97));
        tl.advance_to(Tick::new(100));

        let status = tl.status(id).unwrap();
        assert_eq!(status.attempt, 2);
        assert!(matches!(status.disposition, Some(Disposition::Committed { .. })));
        assert_eq!(status.timing.unwrap().compute, Duration::from_millis(250));
        let computes: Vec<_> = tl.cost_samples(&ResolverId::new("timed")).map(|s| (s.outcome, s.timing.compute)).collect();
        assert_eq!(computes, [
            (SampleOutcome::Ready, Duration::from_millis(100)),
            (SampleOutcome::Ready, Duration::from_millis(250)),
        ]);
    }

    /// Work still running at its deadline is the overrun the window exists to
    /// show. Its sample is a lower bound: the ticks it ran, at the clock's rate.
    #[test]
    fn a_missed_deadline_adds_a_lower_bound_sample() {
        let mut tl = timed_timeline();
        let id = tl.schedule(timed_at(10, serde_json::json!({ "cost_ms": 3000, "hang": true }))).unwrap();
        let never_started = tl.schedule(timed_at(20, serde_json::json!({ "cost_ms": 3000 }))).unwrap();
        for tick in 1..=10 {
            tl.advance_to(Tick::new(tick));
        }
        let status = tl.status(id).unwrap();
        assert!(matches!(status.disposition, Some(Disposition::Fallback { reason: FallbackReason::DeadlineMissed, .. })));
        assert_eq!(status.timing, None, "the producer reported nothing");
        let samples: Vec<_> = tl.cost_samples(&ResolverId::new("timed")).collect();
        assert_eq!(samples.len(), 1);
        assert_eq!(samples[0].outcome, SampleOutcome::Missed);
        assert_eq!(samples[0].timing, Timing { queued: Duration::ZERO, compute: Duration::from_secs(3) });
        assert_eq!(samples[0].estimate, Duration::from_millis(3000));

        // Work that misses before it starts never ran, so it adds nothing.
        tl.advance_to(Tick::new(25));
        assert!(matches!(tl.status(never_started).unwrap().disposition, Some(Disposition::Fallback { reason: FallbackReason::DeadlineMissed, .. })));
        assert_eq!(tl.cost_samples(&ResolverId::new("timed")).count(), 1);
    }

    /// The window keeps the newest samples, oldest first.
    #[test]
    fn the_cost_window_keeps_the_newest_samples() {
        let mut tl = timed_timeline();
        let extra = 3;
        for n in 0..(COST_WINDOW + extra) as i64 {
            tl.schedule(timed_at(10 + n, serde_json::json!({ "compute_ms": n }))).unwrap();
            tl.advance_to(Tick::new(10 + n));
        }
        let samples: Vec<_> = tl.cost_samples(&ResolverId::new("timed")).collect();
        assert_eq!(samples.len(), COST_WINDOW);
        assert_eq!(samples[0].timing.compute, Duration::from_millis(extra as u64));
        assert_eq!(samples.last().unwrap().timing.compute, Duration::from_millis((COST_WINDOW + extra - 1) as u64));
    }
}
