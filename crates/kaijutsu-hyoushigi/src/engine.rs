//! The in-memory speculative engine: a single-context [`Timeline`].
//!
//! This is the first proof of the hard core — a **musician-lite** timeline whose
//! playhead *can't block*. Time is driven by an external beat (here, manual
//! [`Timeline::advance_to`] calls standing in for the internal beat — which also
//! makes the loop deterministic and replayable, exactly what TDD wants). Because
//! the beat won't wait, deferred content must be staged ahead of the playhead:
//! pre-resolved against a predicted context, committed if the prediction held,
//! squashed and recovered if it broke.
//!
//! Persistence is faked in RAM here (the committed log + a CAS map) — real block
//! materialization is a later step. The only genuinely live part is the **open
//! future**: pending/speculated cells ahead of the commit point.

use std::collections::HashMap;
use std::time::Duration;

use kaijutsu_cas::ContentHash;
use kaijutsu_types::{PrincipalId, Tick, TickDelta, TrackId};

use crate::cell::{Body, Cell, CellState, Fallback, Recipe, ResolverId};
use crate::content::{ContentRef, ContextHash};
use crate::resolver::{ResolverCtx, Resolver};

/// The wall-clock binding for this proof, collapsed to a tick rate.
///
/// In the real engine PPQ + tempo + epoch make up the binding; here a single
/// `ticks_per_sec` converts an [`estimate_cost`](Resolver::estimate_cost)
/// `Duration` into a [`TickDelta`] lead time. `safety_factor` widens the lead;
/// `commit_margin` is how far before a cell's `start` the first commit attempt
/// fires.
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
    /// Convert a wall-clock duration into ticks, rounding up (never under-lead).
    fn beats_for(&self, d: Duration) -> TickDelta {
        TickDelta::new((d.as_secs_f64() * self.ticks_per_sec).ceil() as i64)
    }
}

/// How a squashed cell recovered — recorded, never hidden.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Recovery {
    /// Enough time remained (`≥ estimate_cost`) to resolve again against the new
    /// context; a final commit attempt is scheduled at `start`.
    ReSpeculated,
    /// No time to recover — the required fallback fired.
    FellBack,
}

/// The most valuable output the system produces: a misprediction, with both the
/// predicted and the actual context digest, so you can see exactly where the
/// anticipation model was wrong — and what it cost.
#[derive(Debug, Clone)]
pub struct SquashEvent {
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
    cell: Cell,
    start: Tick,
    speculate_at: Tick,
    commit_deadline: Tick,
    /// `estimate_cost` in ticks — the budget threshold for re-speculation.
    est_cost: TickDelta,
    predicted_basis: Option<ContextHash>,
    resolution: Option<crate::resolver::Resolution>,
    /// Set once a squash re-speculated; the next commit check is at `start` and
    /// is the last — diverge there and the fallback fires.
    final_attempt: bool,
}

impl Scheduled {
    /// The tick at which this cell's next lifecycle action is due, if any.
    fn next_at(&self) -> Option<Tick> {
        match self.cell.state {
            CellState::Pending => Some(self.speculate_at),
            CellState::Speculated => Some(if self.final_attempt {
                self.start
            } else {
                self.commit_deadline
            }),
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
}

/// Why a [`Timeline::seed_playhead`] call was rejected. Seeding is a
/// virgin-only operation — a seed attempt on a live timeline is always a caller
/// bug, and crashing over corruption beats silently rewinding or clobbering an
/// open future.
#[derive(Debug, thiserror::Error)]
pub enum SeedError {
    #[error(
        "cannot seed playhead to {at:?}: timeline is not virgin \
         (playhead {playhead:?}, {future} future cell(s), {committed} committed)"
    )]
    NotVirgin {
        at: Tick,
        playhead: Tick,
        future: usize,
        committed: usize,
    },
}

/// A single context's timeline: committed past + open future, over one store.
pub struct Timeline {
    clock: TickClock,
    playhead: Tick,
    resolvers: HashMap<String, Box<dyn Resolver + Send + Sync>>,
    /// The mutable context resolvers read (a beat counter, environment, …).
    ambient: HashMap<String, Vec<u8>>,
    /// The open future: deferred cells ahead of the commit point.
    future: Vec<Scheduled>,
    /// The durable past (in-RAM stand-in for the block log).
    committed: Vec<Cell>,
    /// The content store (in-RAM stand-in for CAS). Crystallized at commit.
    cas: HashMap<ContentHash, Vec<u8>>,
    /// The squash ledger — the bill, and the anticipation-model feedback.
    squashes: Vec<SquashEvent>,
    /// The failure ledger — sibling to `squashes`. An erring resolve appends here
    /// and the cell is removed from `future` (no zombie); the kernel drains this
    /// past a cursor into one Error block per event so the player reads its own
    /// failure next turn.
    failures: Vec<FailureEvent>,
}

impl Timeline {
    pub fn new(clock: TickClock) -> Self {
        Self {
            clock,
            playhead: Tick::ZERO,
            resolvers: HashMap::new(),
            ambient: HashMap::new(),
            future: Vec::new(),
            committed: Vec::new(),
            cas: HashMap::new(),
            squashes: Vec::new(),
            failures: Vec::new(),
        }
    }

    pub fn register_resolver(&mut self, resolver: Box<dyn Resolver + Send + Sync>) {
        self.resolvers.insert(resolver.id().0, resolver);
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

    pub fn playhead(&self) -> Tick {
        self.playhead
    }
    pub fn committed(&self) -> &[Cell] {
        &self.committed
    }
    pub fn squashes(&self) -> &[SquashEvent] {
        &self.squashes
    }
    /// The failure ledger: every resolve that errored, in record order. The
    /// kernel drains this past a cursor into Error blocks; tests assert the
    /// preserved error string and the no-zombie invariant.
    pub fn failures(&self) -> &[FailureEvent] {
        &self.failures
    }
    /// Count of cells still in the open future — used to pin the no-zombie
    /// invariant (a failed cell must not linger here).
    pub fn future_len(&self) -> usize {
        self.future.len()
    }
    /// Fetch crystallized content bytes by hash (in-RAM CAS).
    pub fn content_bytes(&self, hash: &ContentHash) -> Option<&[u8]> {
        self.cas.get(hash).map(|v| v.as_slice())
    }

    /// Schedule a deferred cell. Derives its lead time from `estimate_cost`:
    /// `speculate_at = start − beats_for(estimate × safety)`,
    /// `commit_deadline = start − commit_margin`.
    pub fn schedule(&mut self, cell: Cell) -> Result<(), ScheduleError> {
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
        let resolver = self
            .resolvers
            .get(&recipe.resolver.0)
            .ok_or_else(|| ScheduleError::UnknownResolver(recipe.resolver.0.clone()))?;

        let ctx = CommittedCtx {
            now: start,
            ambient: &self.ambient,
            committed: &self.committed,
        };
        let est = resolver.estimate_cost(&recipe.params, &ctx);
        let est_cost = self.clock.beats_for(est);
        let lead = self.clock.beats_for(est.mul_f64(self.clock.safety_factor));

        self.future.push(Scheduled {
            start,
            speculate_at: start - lead,
            commit_deadline: start - self.clock.commit_margin,
            est_cost,
            predicted_basis: None,
            resolution: None,
            final_attempt: false,
            cell,
        });
        Ok(())
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
        if self.playhead != Tick::ZERO || !self.future.is_empty() || !self.committed.is_empty() {
            return Err(SeedError::NotVirgin {
                at,
                playhead: self.playhead,
                future: self.future.len(),
                committed: self.committed.len(),
            });
        }
        self.playhead = at;
        Ok(())
    }

    /// Map wall-clock elapsed-since-epoch to a [`Tick`] via the clock's rate.
    ///
    /// This is the (stub) wall-clock binding — PPQ + tempo collapse to
    /// `ticks_per_sec` here; the real binding carries epoch + tempo + PPQ. It is
    /// the boundary the doc calls "a logical integer coordinate with a pluggable
    /// wall-clock binding": ticks are pure integers, wall-clock lives only here.
    pub fn tick_at(&self, since_epoch: Duration) -> Tick {
        Tick::new((since_epoch.as_secs_f64() * self.clock.ticks_per_sec).round() as i64)
    }

    /// Drive the playhead from a wall-clock reading — the **internal beat**.
    ///
    /// The beat advances on its own schedule; it does *not* wait for resolves.
    /// Every speculation, commit, squash, and fallback the playhead crosses fires
    /// as it passes. This is the can't-block discipline: an integrator (kernel or
    /// client timer) calls `pump` on each beat tick, and the playhead is wherever
    /// wall-clock says — content is either staged ahead in time or the fallback
    /// fired. Monotonic: a reading behind the playhead is ignored.
    pub fn pump(&mut self, since_epoch: Duration) {
        let target = self.tick_at(since_epoch);
        if target > self.playhead {
            self.advance_to(target);
        }
    }

    /// Advance the playhead to `target`, firing each due lifecycle action in tick
    /// order. The clock can't block: actions happen *because* the playhead
    /// arrives, not because a resolve finished. Time only moves forward — a
    /// non-advancing target is a no-op (the write barrier never walks backward).
    pub fn advance_to(&mut self, target: Tick) {
        if target <= self.playhead {
            return;
        }
        loop {
            // Find the earliest pending action at or before `target`.
            let mut next: Option<(usize, Tick)> = None;
            for (i, s) in self.future.iter().enumerate() {
                if let Some(at) = s.next_at() {
                    if at <= target && next.is_none_or(|(_, best)| at < best) {
                        next = Some((i, at));
                    }
                }
            }
            let Some((idx, at)) = next else { break };
            self.playhead = at;
            match self.future[idx].cell.state {
                CellState::Pending => self.speculate(idx),
                CellState::Speculated => self.commit_or_squash(idx),
                _ => unreachable!("next_at only yields actionable states"),
            }
        }
        self.playhead = target;
    }

    /// Run the resolver against the current committed view, snapshotting the basis.
    /// `Pending → Speculating → Speculated`.
    fn speculate(&mut self, idx: usize) {
        let recipe = self.deferred_recipe(idx);
        let now = self.future[idx].start;

        // Disjoint immutable borrows (resolvers / ambient / committed); the owned
        // results end the borrows before we mutate `future`.
        let resolver = self
            .resolvers
            .get(&recipe.resolver.0)
            .expect("resolver presence checked at schedule time");
        let ctx = CommittedCtx {
            now,
            ambient: &self.ambient,
            committed: &self.committed,
        };
        let basis = resolver.compute_basis(&recipe.params, &ctx);
        let resolution = resolver.resolve(&recipe.params, &ctx);

        let s = &mut self.future[idx];
        debug_assert!(s.cell.state.can_advance_to(CellState::Speculating));
        match resolution {
            Ok(res) => {
                s.predicted_basis = Some(basis);
                s.resolution = Some(res);
                s.cell.state = CellState::Speculated;
            }
            Err(err) => {
                // A resolve that errors records a FailureEvent and the cell is
                // removed from the open future — a hole, never a silent empty
                // commit and never a zombie that re-fires every beat. We pass the
                // cell through `Failed` first so the state-machine asserts stay
                // honest (Pending/Squashed → Failed is a legal edge), then drop it.
                debug_assert!(s.cell.state.can_advance_to(CellState::Failed));
                s.cell.state = CellState::Failed;
                let start = s.start;
                let played_by = s.cell.played_by;
                // Single-variant enum today; an irrefutable `let` keeps the error
                // string verbatim (a future variant would force this back to a
                // match — fine, the ledger only needs a String).
                let crate::resolver::ResolveError::Failed(error) = err;
                // `s`'s borrow ends here; the self-field writes below are disjoint.
                self.failures.push(FailureEvent {
                    at: self.playhead,
                    start,
                    resolver: recipe.resolver.clone(),
                    error,
                    played_by,
                });
                // DEFERRED (flagged for Amy, issues.md): routing a Failed cell to
                // `fire_fallback` instead of dropping it. That would amend the
                // "crash over corruption, never a silent empty commit" stance —
                // an approved-later drop-in. The ledger/removal machinery above is
                // identical whether or not the routing lands; only the next line
                // changes (`self.fire_fallback(idx)` instead of `swap_remove`).
                self.future.swap_remove(idx);
            }
        }
    }

    /// At the commit deadline (or the final attempt at `start`): recompute the
    /// basis against current context. Match → commit + crystallize. Diverge →
    /// squash, then re-speculate if budget remains, else fire the fallback.
    fn commit_or_squash(&mut self, idx: usize) {
        let recipe = self.deferred_recipe(idx);
        let now = self.playhead;

        let resolver = self
            .resolvers
            .get(&recipe.resolver.0)
            .expect("resolver presence checked at schedule time");
        let ctx = CommittedCtx {
            now,
            ambient: &self.ambient,
            committed: &self.committed,
        };
        let actual = resolver.compute_basis(&recipe.params, &ctx);
        let predicted = self.future[idx].predicted_basis.clone().expect("speculated");

        if actual == predicted {
            self.commit(idx);
            return;
        }

        // --- squash ---------------------------------------------------------
        let start = self.future[idx].start;
        let est_cost = self.future[idx].est_cost;
        let budget = start - now; // ticks left until the content is actually needed
        let can_respeculate = !self.future[idx].final_attempt && budget >= est_cost;

        let recovery = if can_respeculate {
            Recovery::ReSpeculated
        } else {
            Recovery::FellBack
        };
        self.squashes.push(SquashEvent {
            at: now,
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
                s.predicted_basis = None;
                s.resolution = None;
            }
            self.speculate(idx);
        } else {
            self.fire_fallback(idx);
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
        s.cell.body = Body::Concrete(cref);
        s.cell.state = CellState::Committed;

        // The committing parent's lane + player are the authority for everything
        // it emits — an emission is part of the committing parent's act. Capture
        // them before the cell moves into the committed log.
        let parent_track = s.cell.track.clone();
        let parent_player = s.cell.played_by;
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
    fn fire_fallback(&mut self, idx: usize) {
        let s = &self.future[idx];
        let Body::Deferred(recipe) = &s.cell.body else {
            unreachable!("scheduled cells are deferred")
        };
        let span = s.cell.span;
        let track = s.cell.track.clone();
        match recipe.fallback.clone() {
            Fallback::Skip => {
                // Emit nothing — the playhead passes a hole.
                self.future.swap_remove(idx);
            }
            Fallback::UseLastGood => {
                // Per-track: a repeat may only reuse THIS lane's last good
                // content. `track == None`/another-track's content can never
                // satisfy this track's UseLastGood — `None` (empty lane history)
                // falls through to silence (Skip), the locked decision.
                let last = self.last_committed_content_in(&track, span.start);
                self.future.swap_remove(idx);
                if let Some(cref) = last {
                    self.committed
                        .push(Cell::concrete_on(span, cref, track, PrincipalId::beat()));
                }
            }
            Fallback::Literal(cref) => {
                self.future.swap_remove(idx);
                self.committed
                    .push(Cell::concrete_on(span, cref, track, PrincipalId::beat()));
            }
        }
    }

    /// Place an emitted cell: a deferred emission re-enters scheduling; a concrete
    /// emission appends to the past (a recorded memory). Never rewrites committed.
    fn absorb_emitted(&mut self, cell: Cell) {
        match &cell.body {
            Body::Concrete(_) => self.committed.push(cell),
            Body::Deferred(_) => {
                // Best-effort: a future emission only schedules if it's genuinely
                // ahead of the playhead. A backdated emission is dropped rather
                // than allowed to rewrite the past.
                let _ = self.schedule(cell);
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
    /// Deliberately **track-blind**: returns the most recent committed content
    /// across *all* lanes at or before `tick`. No current resolver reads this
    /// (AbcToMidi reads CAS by hash), and the first real consumer of a
    /// track-scoped read is `$HEARD` — that API is designed with its consumer
    /// (two-voices rule), not speculatively widened here. Contrast
    /// [`Timeline::last_committed_content_in`], which is lane-scoped for the
    /// `UseLastGood` fallback.
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
        fn resolve(&self, _p: &Value, ctx: &dyn ResolverCtx) -> Result<Resolution, ResolveError> {
            let beat = ctx.ambient("beat").unwrap_or_default();
            Ok(Resolution::new(beat, "text/plain"))
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
        tl.advance_to(Tick::new(100)); // deadline=97 → squash+re-spec against "B"; final commit at 100

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

    /// The internal beat: wall-clock pumps alone drive the playhead through the
    /// lifecycle — no manual `advance_to`. The beat doesn't wait for the resolve.
    #[test]
    fn beat_drives_commit_from_wallclock() {
        let clock = TickClock {
            ticks_per_sec: 10.0, // 10 ticks/sec → tick 10 == 1.0s
            safety_factor: 2.0,
            commit_margin: TickDelta::new(2),
        };
        let mut tl = Timeline::new(clock);
        tl.register_resolver(Box::new(EchoBeat {
            cost: Duration::from_millis(300), // est 3 ticks, lead 6 → speculate_at=4
        }));
        tl.set_ambient("beat", *b"A");
        tl.schedule(deferred_at(10, Fallback::Skip)).unwrap(); // start at 1.0s

        // Beat ticks arrive on the wall clock; nobody calls advance_to.
        tl.pump(Duration::from_millis(500)); // → tick 5: crosses speculate_at(4)
        assert!(tl.committed().is_empty(), "not committed before its deadline");
        tl.pump(Duration::from_millis(1000)); // → tick 10: crosses commit_deadline(8)

        assert_eq!(tl.committed().len(), 1);
        assert!(tl.squashes().is_empty());
        match &tl.committed()[0].body {
            Body::Concrete(cref) => assert_eq!(cref.hash, concrete_hash(b"A")),
            _ => panic!("beat should have committed concrete content"),
        }
    }

    /// A wall-clock reading behind the playhead is ignored — the beat never walks
    /// time backward.
    #[test]
    fn pump_is_monotonic() {
        let mut tl = Timeline::new(TickClock::default());
        tl.pump(Duration::from_secs(5));
        let p = tl.playhead();
        tl.pump(Duration::from_secs(2)); // stale reading
        assert_eq!(tl.playhead(), p, "stale beat reading must not rewind the playhead");
    }

    /// Scheduling behind the playhead is rejected — crash over corruption, never a
    /// silently-backdated cell.
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
        fn resolve(&self, _p: &Value, _c: &dyn ResolverCtx) -> Result<Resolution, ResolveError> {
            let sibling = Cell::concrete_on(
                Span::instant(Tick::new(self.sibling_tick)),
                ContentRef::of(b"sibling", "text/plain"),
                self.sibling_track.clone(),
                self.sibling_player,
            );
            Ok(Resolution::new(b"parent".to_vec(), "text/plain").with_emitted(vec![sibling]))
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
        fn estimate_cost(&self, _p: &Value, _c: &dyn ResolverCtx) -> Duration {
            Duration::from_secs(1)
        }
        fn compute_basis(&self, _p: &Value, _c: &dyn ResolverCtx) -> ContextHash {
            ContextHash::of(b"stable")
        }
        fn resolve(&self, _p: &Value, _c: &dyn ResolverCtx) -> Result<Resolution, ResolveError> {
            Err(ResolveError::Failed(self.message.to_string()))
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

        tl.advance_to(Tick::new(24)); // B speculates@14 + commits@19 (beat=A); A speculates@24 (beat=A)
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
}
