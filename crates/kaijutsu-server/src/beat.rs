//! The single coalescing beat scheduler — kaijutsu's one active timing thing,
//! and the composer's **transport**.
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

use kaijutsu_kernel::block_store::SharedBlockStore;
use kaijutsu_kernel::flows::TurnFlow;
use kaijutsu_kernel::hyoushigi::{BeatCommand, BeatPolicy, materialize_committed, schedule_abc_cell};
use kaijutsu_kernel::{Kernel, KjCaller, KjDispatcher};
use kaijutsu_types::{ContextId, PrincipalId, SessionId, TickDelta};

use crate::rpc::ServerRegistry;

/// Ticks the playhead advances per beat (PPQ 1: one tick per beat). The tick is
/// event-counted, so this is a pure increment, never scaled by elapsed time.
const STEP: TickDelta = TickDelta::new(1);

/// How far ahead of the playhead an OODA turn's ABC section is scheduled (one bar
/// at 4/4) — room for the fast abc→midi resolve to speculate before its beat.
const OODA_LEAD: TickDelta = TickDelta::new(4);

/// Per-armed-context beat bookkeeping. The two transport switches live here.
struct BeatState {
    policy: BeatPolicy,
    /// The clock switch. `false` = stopped/paused: no heap entry (quiescent),
    /// playhead frozen. Default on arm (create stopped — no surprise tokens).
    playing: bool,
    /// The OODA switch. The `tick` verb fires only when `playing && ooda_armed`.
    ooda_armed: bool,
    /// Count of committed cells already materialized — the bridge high-water mark.
    high_water: usize,
    /// Beats elapsed *while playing* — drives the OODA cadence. Frozen across a
    /// pause, like the playhead.
    beat_count: u64,
}

/// What one wake produced: which contexts beat, and which crossed an OODA
/// boundary (so should fire the `tick` verb). Split out so the cadence logic is
/// observable in tests without a live dispatcher.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct BeatOutcome {
    pub fired: Vec<ContextId>,
    pub ooda_due: Vec<ContextId>,
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
}

impl BeatScheduler {
    pub fn new(kernel: Arc<Kernel>, documents: SharedBlockStore) -> Self {
        Self {
            kernel,
            documents,
            dispatcher: None,
            heap: BinaryHeap::new(),
            armed: HashMap::new(),
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
    pub fn arm(&mut self, context_id: ContextId, policy: BeatPolicy) {
        self.kernel.arm_timeline(context_id, policy.clock());
        self.armed
            .entry(context_id)
            .and_modify(|s| s.policy = policy)
            .or_insert(BeatState {
                policy,
                playing: false,
                ooda_armed: true,
                high_water: 0,
                beat_count: 0,
            });
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
        if let Some(st) = self.armed.get_mut(&context_id) {
            st.policy.period = period;
        }
    }

    /// Arm or disarm the OODA loop without touching the clock.
    pub fn set_ooda(&mut self, context_id: ContextId, armed: bool) {
        if let Some(st) = self.armed.get_mut(&context_id) {
            st.ooda_armed = armed;
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
            let ooda_due = self.process_one(ctx);
            self.heap.push(Reverse((now + period, ctx)));
            outcome.fired.push(ctx);
            if ooda_due {
                outcome.ooda_due.push(ctx);
            }
        }
        outcome
    }

    /// Advance one context by one beat (event-counted), flush whatever committed,
    /// count the beat, and report whether it crossed an OODA cadence boundary.
    /// Called only for a playing context.
    fn process_one(&mut self, ctx: ContextId) -> bool {
        let Some(timeline) = self.kernel.timeline(ctx) else {
            // Armed in the scheduler but no timeline in the kernel — shouldn't
            // happen (arming pairs the two), but never panic the driver.
            return false;
        };
        // The beat tick: advance the playhead by one step. Lock held only for the
        // advance; the bridge re-locks internally. No `.await` under the lock.
        {
            let mut g = timeline.lock();
            let target = g.playhead() + STEP;
            g.advance_to(target);
        }

        let cas = self.kernel.cas().clone();
        let mut high_water = self.armed.get(&ctx).map(|s| s.high_water).unwrap_or(0);
        match materialize_committed(&timeline, &cas, &self.documents, ctx, &mut high_water) {
            Ok(_) => {
                if let Some(s) = self.armed.get_mut(&ctx) {
                    s.high_water = high_water;
                }
            }
            Err(e) => log::warn!("beat: materialize failed for context {ctx}: {e}"),
        }

        let Some(s) = self.armed.get_mut(&ctx) else {
            return false;
        };
        s.beat_count += 1;
        s.ooda_armed && s.policy.ooda_every > 0 && s.beat_count % s.policy.ooda_every == 0
    }

    /// Fire the `tick` rc verb for `ctx` — the OODA hook (`kj drive`). Spawned
    /// fire-and-forget on the local set so the scheduler never blocks; the verb's
    /// kaish only *requests* a turn (publishes `TurnFlow::Requested`), returning
    /// fast — the model turn runs on the turn-driver thread.
    fn fire_tick(&self, ctx: ContextId) {
        let Some(dispatcher) = self.dispatcher.clone() else {
            return;
        };
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
                .run_rc_lifecycle("tick", ctx, None, None, None, &caller)
                .await
            {
                log::warn!("beat: tick verb failed for context {ctx}: {e}");
            }
        });
    }

    /// The OODA **Act** handoff: a composer's turn just completed (it wrote ABC),
    /// so crystallize that ABC onto the timeline. Reads the model's last block as
    /// the ABC and schedules an `abc_to_midi` cell a bar ahead. Only for an armed,
    /// OODA-armed context; a non-composer (un-armed) turn is ignored.
    fn on_turn_completed(&self, ctx: ContextId) {
        match self.armed.get(&ctx) {
            Some(st) if st.ooda_armed => {}
            _ => return, // not an OODA-armed composer we manage
        }
        let Some(block_id) = self.documents.last_block_id(ctx) else {
            return;
        };
        let abc = match self.documents.get_block_snapshot(ctx, &block_id) {
            Ok(Some(b)) => b.content,
            _ => return,
        };
        if abc.trim().is_empty() {
            return;
        }
        if let Err(e) = schedule_abc_cell(&self.kernel, ctx, &abc, OODA_LEAD) {
            log::warn!("beat: failed to schedule abc→midi for context {ctx}: {e}");
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
                    Some(BeatCommand::Arm { context_id, policy }) => self.arm(context_id, policy),
                    Some(BeatCommand::Play(ctx)) => self.play(ctx, Instant::now()),
                    Some(BeatCommand::Pause(ctx)) => self.pause(ctx),
                    Some(BeatCommand::Stop(ctx)) => self.stop(ctx),
                    Some(BeatCommand::SetTempo { context_id, period }) => {
                        self.set_tempo(context_id, period)
                    }
                    Some(BeatCommand::SetOoda { context_id, armed }) => {
                        self.set_ooda(context_id, armed)
                    }
                    Some(BeatCommand::Disarm(ctx)) => self.disarm(ctx),
                    None => break, // all senders dropped → shut down
                },
                msg = completed.recv(), if turn_bus_open => match msg {
                    Some(m) => {
                        if let TurnFlow::Completed { context_id, .. } = m.payload {
                            self.on_turn_completed(context_id);
                        }
                    }
                    None => turn_bus_open = false, // bus closed → stop polling this arm
                },
                _ = sleep_until_opt(next) => {
                    let outcome = self.fire_due(Instant::now());
                    for ctx in outcome.ooda_due {
                        self.fire_tick(ctx);
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
/// lifecycle and `kj transport` can arm/drive composer contexts.
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
    use kaijutsu_types::{ContextId, PrincipalId};
    use tokio::time::Instant;

    use super::BeatScheduler;

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
        Cell::deferred(
            Span::instant(Tick::new(tick)),
            Recipe {
                resolver: ResolverId::new("marker"),
                params: serde_json::Value::Null,
                query: ContextQuery::default(),
                fallback: Fallback::Skip,
            },
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
        );
        let mut g = tl.lock();
        g.register_resolver(Box::new(Marker));
        for t in 1..=count {
            g.schedule(marker_cell(t)).unwrap();
        }
    }

    /// A 1-second beat policy with a large OODA cadence (cadence won't fire).
    fn slow_policy() -> BeatPolicy {
        BeatPolicy { period: Duration::from_secs(1), ooda_every: 1_000_000 }
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
        sched.arm(ctx, slow_policy());
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
        sched.arm(ctx, slow_policy());
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
        sched.arm(a, slow_policy());
        sched.play(a, base);

        sched.fire_due(base + Duration::from_secs(1));
        sched.fire_due(base + Duration::from_secs(2));
        assert_eq!(contents(&documents, a).len(), 2);
        assert_eq!(contents(&documents, b).len(), 0, "b not playing yet");

        sched.arm(b, slow_policy());
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
        );

        let mut sched = BeatScheduler::new(kernel.clone(), documents.clone());
        let base = Instant::now();
        sched.arm(ctx, BeatPolicy { period: Duration::from_secs(1), ooda_every: 3 });
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

    /// The OODA Act handoff: a completed turn's ABC (the last block) is scheduled
    /// as an abc→midi cell and materializes into a MIDI block on the beat.
    #[tokio::test]
    async fn turn_completion_schedules_abc_and_midi_materializes() {
        use kaijutsu_crdt::{BlockKind, ContentType, Role, Status};

        let (kernel, documents) = fresh_kernel_and_docs().await;
        let ctx = ContextId::new();
        documents.create_document(ctx, DocumentKind::Conversation, None).unwrap();
        // Simulate the model turn's ABC output as the last block.
        documents
            .insert_block_as(
                ctx,
                None,
                None,
                Role::Model,
                BlockKind::Text,
                "X:1\nT:Test\nM:4/4\nL:1/8\nK:C\nCDEFGABc|\n".to_string(),
                Status::Done,
                ContentType::Plain,
                Some(PrincipalId::new()),
            )
            .unwrap();

        // arm_timeline registers the real abc_to_midi resolver onto the timeline.
        let mut sched = BeatScheduler::new(kernel.clone(), documents.clone());
        let base = Instant::now();
        sched.arm(ctx, slow_policy());
        sched.play(ctx, base);

        sched.fire_due(base + Duration::from_secs(1)); // playhead → 1
        sched.on_turn_completed(ctx); // schedules abc→midi at playhead + OODA_LEAD

        let asset_before = documents
            .block_snapshots(ctx)
            .unwrap()
            .iter()
            .filter(|b| b.role == Role::Asset)
            .count();
        assert_eq!(asset_before, 0, "no MIDI block before the scheduled cell commits");

        // Advance past the scheduled tick so the cell commits and materializes.
        for i in 2..=8 {
            sched.fire_due(base + Duration::from_secs(i));
        }

        let snaps = documents.block_snapshots(ctx).unwrap();
        let midi = snaps.iter().find(|b| b.role == Role::Asset);
        assert!(midi.is_some(), "an abc→midi MIDI block should have materialized");
        let midi = midi.unwrap();
        assert_eq!(midi.content.len(), 32, "binary block content is the 32-hex CAS hash");
        assert!(midi.tick.is_some(), "the MIDI block carries a beat tick");
    }
}
