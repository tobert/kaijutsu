//! The in-memory speculative engine: a single-context [`Timeline`].
//!
//! This is the first proof of the hard core — a **composer-lite** timeline whose
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
use kaijutsu_types::{Tick, TickDelta};

use crate::cell::{Body, Cell, CellState, Fallback, Recipe};
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

/// A single context's timeline: committed past + open future, over one store.
pub struct Timeline {
    clock: TickClock,
    playhead: Tick,
    resolvers: HashMap<String, Box<dyn Resolver>>,
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
        }
    }

    pub fn register_resolver(&mut self, resolver: Box<dyn Resolver>) {
        self.resolvers.insert(resolver.id().0, resolver);
    }

    /// Poke the ambient context — what a real beat / environment / sibling event
    /// would mutate between turns. Changing this between a speculate and its
    /// commit is what drives a squash.
    pub fn set_ambient(&mut self, key: impl Into<String>, value: impl Into<Vec<u8>>) {
        self.ambient.insert(key.into(), value.into());
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

    /// Advance the playhead to `target`, firing each due lifecycle action in tick
    /// order. The clock can't block: actions happen *because* the playhead
    /// arrives, not because a resolve finished.
    pub fn advance_to(&mut self, target: Tick) {
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
            Err(_) => {
                // A resolve that errors is a Failed cell — crash over corruption,
                // never a silent empty commit.
                s.cell.state = CellState::Failed;
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
        self.committed.push(s.cell);

        // Emitted cells become real only on commit — a squashed resolution's
        // emissions simply vanish. A loop thus unrolls into distinct memories.
        for emitted in res.emitted {
            self.absorb_emitted(emitted);
        }
    }

    /// The required real-time miss handler. Never undefined behavior.
    fn fire_fallback(&mut self, idx: usize) {
        let s = &self.future[idx];
        let Body::Deferred(recipe) = &s.cell.body else {
            unreachable!("scheduled cells are deferred")
        };
        let span = s.cell.span;
        match recipe.fallback.clone() {
            Fallback::Skip => {
                // Emit nothing — the playhead passes a hole.
                self.future.swap_remove(idx);
            }
            Fallback::UseLastGood => {
                let last = self.last_committed_content(span.start);
                self.future.swap_remove(idx);
                if let Some(cref) = last {
                    self.committed.push(Cell::concrete(span, cref));
                }
            }
            Fallback::Literal(cref) => {
                self.future.swap_remove(idx);
                self.committed.push(Cell::concrete(span, cref));
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

    fn last_committed_content(&self, before: Tick) -> Option<ContentRef> {
        self.committed
            .iter()
            .filter(|c| c.span.start <= before)
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
        Cell::deferred(
            Span::instant(Tick::new(start)),
            Recipe {
                resolver: ResolverId::new("echo"),
                params: Value::Null,
                query: ContextQuery::default(),
                fallback,
            },
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
}
