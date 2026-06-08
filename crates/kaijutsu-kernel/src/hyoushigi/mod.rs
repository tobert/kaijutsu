//! Kernel-side hyoushigi integration: the per-context timeline registry and the
//! production resolvers that crystallize content into the block log.
//!
//! `kaijutsu-hyoushigi` is the runtime-agnostic engine (the `Timeline`, the
//! speculate→commit-or-squash→fallback loop, the internal-beat `pump`). This
//! module is where the kernel *owns* those timelines, registers real
//! [`Resolver`](kaijutsu_hyoushigi::Resolver)s onto them, and bridges committed
//! cells into the CRDT block log + CAS. The wall-clock timer that drives the
//! beat lives one layer out, in `kaijutsu-server` (see its `beat` module), so
//! this crate stays free of any interval-timer runtime.

use std::sync::Arc;
use std::time::Duration;

use kaijutsu_cas::{ContentHash, ContentStore, FileStore};
use kaijutsu_hyoushigi::{
    Body, Cell, ContextHash, ContextQuery, Fallback, Recipe, ResolveError, Resolution, Resolver,
    ResolverCtx, ResolverId, Span, TickDelta, Timeline,
};
use kaijutsu_types::{BlockId, ContextId, PrincipalId};
use parking_lot::Mutex;

use crate::block_store::SharedBlockStore;
use crate::kernel::Kernel;

/// How a context's beat runs: the wall-clock period of one beat, and how many
/// beats make one coarse **OODA** cadence (when the `tick` rc verb fires). For a
/// composer these are the two musical knobs (defaulted from tempo + bars).
#[derive(Debug, Clone, Copy)]
pub struct BeatPolicy {
    /// Wall-clock duration of one beat (e.g. a quarter note).
    pub period: Duration,
    /// Beats per OODA cadence — the `tick` verb fires every `ooda_every` beats.
    pub ooda_every: u64,
}

impl BeatPolicy {
    /// The composer default: a quarter note at 120 BPM (500 ms/beat) in 4/4,
    /// OODA every 32 bars (= 128 beats ≈ 64 s). Tunable per context via rc.
    pub fn composer_default() -> Self {
        Self {
            period: Duration::from_millis(500),
            ooda_every: 32 * 4,
        }
    }

    /// The wall-clock binding derived from the beat period: one logical tick per
    /// beat, so a materialized cell's tick coordinate reads as its beat index.
    pub fn clock(&self) -> kaijutsu_hyoushigi::TickClock {
        kaijutsu_hyoushigi::TickClock {
            ticks_per_sec: 1.0 / self.period.as_secs_f64(),
            safety_factor: 1.5,
            commit_margin: kaijutsu_hyoushigi::TickDelta::new(1),
        }
    }
}

/// A command to the beat scheduler — the transport control surface. Defined
/// here (kernel-side) so the rc lifecycle and the `kj transport` verb can name
/// and send it; the scheduler that consumes it lives in `kaijutsu-server`. The
/// kernel holds the ingress `Sender`; the server owns the loop.
///
/// Two switches per context: the **clock** (`Play`/`Pause`) and the **OODA-arm**
/// (`SetOoda`). The `tick` verb fires only when the clock is playing *and* OODA
/// is armed. The context tick is **event-counted**: it advances one step per beat
/// *while playing*, so a pause freezes musical time and a resume picks up at +1 —
/// no wall-clock catch-up. That lets the kernel stay alive 24×7 while a context
/// goes quiescent at zero cost.
#[derive(Debug, Clone)]
pub enum BeatCommand {
    /// Arm a context with a beat policy, **stopped** (no surprise token spend):
    /// the timeline is created and beat state registered, but the clock isn't
    /// running until `Play`. Sent when a composer context is created/forked.
    Arm { context_id: ContextId, policy: BeatPolicy },
    /// Start the clock — the playhead advances one beat per period, and (if OODA
    /// is armed) the `tick` verb fires on the coarse cadence.
    Play(ContextId),
    /// Hold the clock — the playhead freezes; a later `Play` resumes at +1. OODA
    /// arm state is preserved.
    Pause(ContextId),
    /// Full halt — pause the clock *and* disarm OODA. A clean stopped state.
    Stop(ContextId),
    /// Set the beat period (tempo) for a context.
    SetTempo { context_id: ContextId, period: Duration },
    /// Arm or disarm the OODA loop without touching the clock.
    SetOoda { context_id: ContextId, armed: bool },
    /// Disarm a context entirely — drop its timeline and beat state (e.g. on
    /// archive).
    Disarm(ContextId),
}

/// A context's timeline, shared between the beat scheduler (which pumps it) and
/// the turn-completion handler (which schedules cells onto it).
///
/// A sync `parking_lot::Mutex` rather than a tokio mutex: the only holder that
/// matters — the beat scheduler — locks it for the duration of a `pump` and
/// never `.await`s under the lock, so there is nothing for an async mutex to
/// buy. `Arc` because two parties hold it.
pub type SharedTimeline = Arc<Mutex<Timeline>>;

/// Register the production resolvers onto a freshly-created timeline. This is
/// the one seam where the kernel teaches a timeline what content it can
/// crystallize; each resolver is a downstream `impl Resolver` (the substrate
/// stays modality-agnostic). `cas` is handed in because content-producing
/// resolvers fetch their inputs (and the engine's crystallized bytes ultimately
/// land) in the content-addressed store.
///
/// Stage 0: no production resolvers yet — the abc→midi resolver lands in a later
/// stage. The seam exists so `arm_timeline` has a stable call.
pub fn register_resolvers(timeline: &mut Timeline, cas: Arc<FileStore>) {
    timeline.register_resolver(Box::new(AbcToMidiResolver { cas }));
}

/// Absorb a completed OODA turn's ABC decision onto a context's timeline — the
/// **Act** handoff. Stores the ABC text in CAS and schedules an `abc_to_midi`
/// cell `lead` ticks ahead of the playhead (fallback `UseLastGood`, so a missed
/// resolve repeats the last layer rather than dropping out). The model turn that
/// *produced* the ABC is the side-effecting, token-costing part and already ran
/// on the turn path; only this pure crystallization lands on the timeline.
///
/// Returns the scheduled cell's start tick. Errors (no timeline, CAS write,
/// schedule-in-the-past) bubble — crash over silently dropping the section.
pub fn schedule_abc_cell(
    kernel: &Kernel,
    context_id: ContextId,
    abc: &str,
    lead: TickDelta,
) -> anyhow::Result<kaijutsu_types::Tick> {
    let timeline = kernel
        .timeline(context_id)
        .ok_or_else(|| anyhow::anyhow!("schedule_abc_cell: context {context_id} is not armed"))?;
    let hash = kernel.cas().store(abc.as_bytes(), "text/vnd.abc")?;

    let mut g = timeline.lock();
    let start = g.playhead() + lead;
    g.schedule(Cell::deferred(
        Span::instant(start),
        Recipe {
            resolver: ResolverId::new(AbcToMidiResolver::ID),
            params: serde_json::json!({ "abc_hash": hash.as_str() }),
            query: ContextQuery::default(),
            fallback: Fallback::UseLastGood,
        },
    ))
    .map_err(|e| anyhow::anyhow!("schedule_abc_cell: {e}"))?;
    Ok(start)
}

/// The first production resolver: crystallize an ABC-notation decision into MIDI.
///
/// This is the composer OODA loop's **Act** step, and it is deliberately a *pure*
/// resolver — idempotent, side-effect-free, safe to speculate and discard. It
/// reads ABC by **hash** from `params` (the `abc_hash` the model turn's ABC block
/// was stored under), not from the committed timeline view, so the block log and
/// the timeline stay decoupled at the resolver boundary. The model turn that
/// *produces* the ABC is the side-effecting, token-costing part and runs
/// elsewhere (the autonomous turn path); only this transform lands on the
/// timeline.
struct AbcToMidiResolver {
    cas: Arc<FileStore>,
}

impl AbcToMidiResolver {
    /// The single resolver id; recipes name it.
    pub const ID: &'static str = "abc_to_midi";

    fn abc_hash_param(params: &serde_json::Value) -> Result<ContentHash, ResolveError> {
        let s = params
            .get("abc_hash")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ResolveError::Failed("abc_to_midi: missing `abc_hash` param".into()))?;
        // Crash on a malformed hash — a bad hash is structural corruption, never
        // a lenient fallback (the doc's asymmetric boundary).
        ContentHash::from_str_checked(s)
            .map_err(|e| ResolveError::Failed(format!("abc_to_midi: malformed abc_hash: {e}")))
    }
}

impl Resolver for AbcToMidiResolver {
    fn id(&self) -> ResolverId {
        ResolverId::new(Self::ID)
    }

    fn estimate_cost(&self, _params: &serde_json::Value, _rctx: &dyn ResolverCtx) -> Duration {
        // Parsing ABC + emitting SMF is sub-millisecond; keep a small floor so
        // lead-time derivation has something to chew on.
        Duration::from_millis(20)
    }

    fn compute_basis(&self, params: &serde_json::Value, _rctx: &dyn ResolverCtx) -> ContextHash {
        // The equivalence class is exactly the input ABC: same hash → same MIDI →
        // commits cleanly. (A richer basis — e.g. tempo/key from ambient — lands
        // when the composer needs it; the doc defers compute_basis tuning.)
        let h = params
            .get("abc_hash")
            .and_then(|v| v.as_str())
            .unwrap_or_default();
        ContextHash::of(h.as_bytes())
    }

    fn resolve(
        &self,
        params: &serde_json::Value,
        _rctx: &dyn ResolverCtx,
    ) -> Result<Resolution, ResolveError> {
        let hash = Self::abc_hash_param(params)?;
        let bytes = self
            .cas
            .retrieve(&hash)
            .map_err(|e| ResolveError::Failed(format!("abc_to_midi: CAS read: {e}")))?
            .ok_or_else(|| {
                ResolveError::Failed(format!("abc_to_midi: abc_hash not in CAS: {}", hash.as_str()))
            })?;
        let abc = String::from_utf8(bytes)
            .map_err(|e| ResolveError::Failed(format!("abc_to_midi: ABC is not UTF-8: {e}")))?;

        let parsed = kaijutsu_abc::parse(&abc);
        if parsed.has_errors() {
            let first = parsed
                .errors()
                .next()
                .map(|f| f.message.clone())
                .unwrap_or_else(|| "unknown parse error".into());
            return Err(ResolveError::Failed(format!("abc_to_midi: ABC parse error: {first}")));
        }
        let tune = parsed
            .value
            .first()
            .ok_or_else(|| ResolveError::Failed("abc_to_midi: ABC contained no tune".into()))?;

        let midi = kaijutsu_abc::to_midi(tune, &kaijutsu_abc::MidiParams::default());
        Ok(Resolution::new(midi, "audio/midi"))
    }
}

/// Bridge a timeline's newly-committed cells into the context's CRDT block log.
///
/// This is the write-barrier crossing: a cell that has committed in the open
/// future becomes a durable, synced block. For each committed cell past the
/// `high_water` mark it (1) crystallizes the cell's bytes from the timeline's
/// in-RAM store into the durable CAS under the same content hash, and (2)
/// materializes the cell to a [`BlockSnapshot`](kaijutsu_types::BlockSnapshot)
/// (tick = the cell's beat coordinate) and inserts it at the log tail, which
/// emits `BlockFlow::Inserted` so it syncs and persists like any block.
///
/// `high_water` is the count of committed cells already materialized — monotonic,
/// and it doubles as the per-context materialization `seq` under the
/// [`PrincipalId::beat`] author lane, so a beat block's row id can never collide
/// with a turn block's. Idempotent across calls: re-invoking with the same
/// `high_water` materializes only what is genuinely new.
///
/// Crashes over corrupting: a CAS write or block insert that fails returns `Err`
/// and leaves `high_water` un-advanced for the failed cell, so a retry re-tries
/// it rather than skipping it.
pub fn materialize_committed(
    timeline: &SharedTimeline,
    cas: &FileStore,
    blocks: &SharedBlockStore,
    context_id: ContextId,
    high_water: &mut usize,
) -> anyhow::Result<Vec<BlockId>> {
    let guard = timeline.lock();
    let committed = guard.committed();
    let mut inserted = Vec::new();
    let mut after = blocks.last_block_id(context_id);

    while *high_water < committed.len() {
        let cell = &committed[*high_water];
        // Committed cells are always concrete; the guard is defensive, not a
        // silent skip — a non-concrete committed cell would be a real bug.
        let Body::Concrete(cref) = &cell.body else {
            anyhow::bail!(
                "committed cell at index {} is not concrete — timeline invariant violated",
                *high_water
            );
        };

        // Crystallize the RAM-CAS bytes into the durable store under the *same*
        // hash (ContentHash::from_data both sides). `store` is idempotent.
        if let Some(bytes) = guard.content_bytes(&cref.hash) {
            let stored = cas.store(bytes, &cref.mime)?;
            debug_assert_eq!(
                stored, cref.hash,
                "durable CAS hash must match the cell's ContentRef hash"
            );
        }

        let block_id = BlockId::new(context_id, PrincipalId::beat(), *high_water as u64);
        let snapshot = guard
            .materialize(cell, block_id)
            .expect("a concrete cell always materializes");
        let id = blocks.insert_from_snapshot_as(
            context_id,
            snapshot,
            after.as_ref(),
            Some(PrincipalId::beat()),
        )?;
        after = Some(id);
        inserted.push(id);
        *high_water += 1;
    }

    Ok(inserted)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use kaijutsu_hyoushigi::{
        Cell, ContextHash, ContextQuery, Fallback, Recipe, ResolveError, Resolution, Resolver,
        ResolverCtx, ResolverId, Span, Tick, TickClock, TickDelta, Timeline,
    };
    use kaijutsu_types::ContextId;
    use parking_lot::Mutex;

    use super::{SharedTimeline, materialize_committed};
    use crate::block_store::{BlockStore, DocumentKind};
    use crate::flows::{BlockFlow, FlowBus, SharedBlockFlowBus};
    use crate::kernel::Kernel;

    /// A resolver that emits fixed bytes under a chosen MIME with a stable basis —
    /// drives a committed cell of any content type through the real loop.
    struct Fixed {
        bytes: Vec<u8>,
        mime: String,
    }
    impl Resolver for Fixed {
        fn id(&self) -> ResolverId {
            ResolverId::new("fixed")
        }
        fn estimate_cost(&self, _p: &serde_json::Value, _c: &dyn ResolverCtx) -> std::time::Duration {
            std::time::Duration::from_secs(1)
        }
        fn compute_basis(&self, _p: &serde_json::Value, _c: &dyn ResolverCtx) -> ContextHash {
            ContextHash::of(b"stable")
        }
        fn resolve(
            &self,
            _p: &serde_json::Value,
            _c: &dyn ResolverCtx,
        ) -> Result<Resolution, ResolveError> {
            Ok(Resolution::new(self.bytes.clone(), self.mime.clone()))
        }
    }

    /// Build a timeline that has already committed one cell of the given content.
    fn timeline_with_one_committed(mime: &str, bytes: &[u8]) -> SharedTimeline {
        let mut tl = Timeline::new(TickClock {
            ticks_per_sec: 1.0,
            safety_factor: 2.0,
            commit_margin: TickDelta::new(1),
        });
        tl.register_resolver(Box::new(Fixed {
            bytes: bytes.to_vec(),
            mime: mime.to_string(),
        }));
        tl.schedule(Cell::deferred(
            Span::instant(Tick::new(10)),
            Recipe {
                resolver: ResolverId::new("fixed"),
                params: serde_json::Value::Null,
                query: ContextQuery::default(),
                fallback: Fallback::Skip,
            },
        ))
        .unwrap();
        tl.advance_to(Tick::new(10));
        assert_eq!(tl.committed().len(), 1, "cell should have committed");
        Arc::new(Mutex::new(tl))
    }

    /// The bridge crosses the write barrier: a committed binary (MIDI-shaped) cell
    /// lands in CAS under its hash, materializes into the block log with its beat
    /// tick, and emits `BlockFlow::Inserted` so it syncs. Re-running the bridge is
    /// idempotent.
    #[tokio::test]
    async fn bridge_materializes_committed_cell_into_block_and_cas() {
        let dir = tempfile::tempdir().unwrap();
        let cas = kaijutsu_cas::FileStore::at_path(dir.path());
        let bus: SharedBlockFlowBus = Arc::new(FlowBus::new(64));
        let blocks: crate::block_store::SharedBlockStore =
            Arc::new(BlockStore::with_flows(kaijutsu_types::PrincipalId::new(), bus.clone()));
        let mut sub = bus.subscribe("block.>");

        let ctx = ContextId::new();
        blocks.create_document(ctx, DocumentKind::Conversation, None).unwrap();

        let tl = timeline_with_one_committed("audio/midi", b"MThd-test-bytes");
        let mut high_water = 0usize;

        let inserted = materialize_committed(&tl, &cas, &blocks, ctx, &mut high_water).unwrap();
        assert_eq!(inserted.len(), 1, "one committed cell → one block");
        assert_eq!(high_water, 1, "high-water advances past the materialized cell");

        // (1) bytes are crystallized in the durable CAS under the cell's hash.
        let cref = {
            let g = tl.lock();
            match &g.committed()[0].body {
                kaijutsu_hyoushigi::Body::Concrete(c) => c.clone(),
                _ => panic!("committed cell must be concrete"),
            }
        };
        use kaijutsu_cas::ContentStore;
        assert_eq!(
            cas.retrieve(&cref.hash).unwrap().as_deref(),
            Some(b"MThd-test-bytes".as_slice()),
            "durable CAS holds the cell bytes"
        );

        // (2) the block is in the log, carrying its beat tick, content = CAS hash.
        let snaps = blocks.block_snapshots(ctx).unwrap();
        assert_eq!(snaps.len(), 1);
        assert_eq!(snaps[0].tick, Some(Tick::new(10)), "block carries the beat coordinate");
        assert_eq!(snaps[0].content, cref.hash.as_str(), "binary content points at CAS by hash");
        assert_eq!(snaps[0].id.principal_id, kaijutsu_types::PrincipalId::beat());

        // (3) a BlockFlow::Inserted was emitted (it syncs).
        let msg = sub.try_recv().expect("an Inserted event should have been emitted");
        assert!(matches!(msg.payload, BlockFlow::Inserted { .. }));

        // Idempotent: nothing new on a second pass.
        let again = materialize_committed(&tl, &cas, &blocks, ctx, &mut high_water).unwrap();
        assert!(again.is_empty(), "no new committed cells → no new blocks");
        assert_eq!(blocks.block_snapshots(ctx).unwrap().len(), 1);
    }

    /// The abc→midi resolver crystallizes ABC (read by hash from CAS) into valid
    /// SMF MIDI on the timeline. This is the composer's Act step as a pure cell.
    #[tokio::test]
    async fn abc_to_midi_resolver_crystallizes_midi_from_cas() {
        use kaijutsu_cas::{ContentStore, FileStore};

        let dir = tempfile::tempdir().unwrap();
        let cas = Arc::new(FileStore::at_path(dir.path()));

        // The (fake) OODA turn's decision: ABC stored in CAS, referenced by hash.
        let abc = "X:1\nT:Test\nM:4/4\nL:1/8\nK:C\nCDEFGABc|\n";
        let abc_hash = cas.store(abc.as_bytes(), "text/vnd.abc").unwrap();

        let mut tl = Timeline::new(TickClock {
            ticks_per_sec: 1.0,
            safety_factor: 1.0,
            commit_margin: TickDelta::new(0),
        });
        super::register_resolvers(&mut tl, cas.clone());

        tl.schedule(Cell::deferred(
            Span::instant(Tick::new(1)),
            Recipe {
                resolver: ResolverId::new(super::AbcToMidiResolver::ID),
                params: serde_json::json!({ "abc_hash": abc_hash.as_str() }),
                query: ContextQuery::default(),
                fallback: Fallback::UseLastGood,
            },
        ))
        .unwrap();
        tl.advance_to(Tick::new(1));

        assert_eq!(tl.committed().len(), 1, "the abc→midi cell committed");
        let cref = match &tl.committed()[0].body {
            kaijutsu_hyoushigi::Body::Concrete(c) => c.clone(),
            _ => panic!("committed cell must be concrete"),
        };
        assert_eq!(cref.mime, "audio/midi");
        let midi = tl.content_bytes(&cref.hash).expect("crystallized to RAM CAS");
        assert!(midi.starts_with(b"MThd"), "valid Standard MIDI File header");

        // Materialized: binary byte-home → Asset role, content = 32-hex CAS hash.
        let block = tl
            .materialize(
                &tl.committed()[0],
                kaijutsu_types::BlockId::new(ContextId::new(), kaijutsu_types::PrincipalId::beat(), 0),
            )
            .unwrap();
        assert_eq!(block.role, kaijutsu_types::Role::Asset);
        assert_eq!(block.content, cref.hash.as_str());
        assert_eq!(block.content.len(), 32);
    }

    /// A malformed `abc_hash` param fails the resolve (crash over corruption),
    /// never a silent empty MIDI commit. The cell ends `Failed`, nothing commits.
    #[tokio::test]
    async fn abc_to_midi_rejects_malformed_hash() {
        use kaijutsu_cas::FileStore;
        let dir = tempfile::tempdir().unwrap();
        let cas = Arc::new(FileStore::at_path(dir.path()));

        let mut tl = Timeline::new(TickClock {
            ticks_per_sec: 1.0,
            safety_factor: 1.0,
            commit_margin: TickDelta::new(0),
        });
        super::register_resolvers(&mut tl, cas);

        tl.schedule(Cell::deferred(
            Span::instant(Tick::new(1)),
            Recipe {
                resolver: ResolverId::new(super::AbcToMidiResolver::ID),
                params: serde_json::json!({ "abc_hash": "not-a-hash" }),
                query: ContextQuery::default(),
                fallback: Fallback::Skip,
            },
        ))
        .unwrap();
        tl.advance_to(Tick::new(1));

        // resolve() errored → the cell is Failed and nothing committed.
        assert!(tl.committed().is_empty(), "a bad hash must not commit content");
    }

    /// Arming a context gives it a timeline; an un-armed context has none.
    /// (The registry is the whole "paused is no heap entry" mechanic.)
    #[tokio::test]
    async fn arm_then_lookup_returns_some_unarmed_returns_none() {
        let kernel = Kernel::new_ephemeral("test").await;
        let armed = ContextId::new();
        let bare = ContextId::new();

        assert!(kernel.timeline(armed).is_none(), "not armed yet");
        kernel.arm_timeline(armed, TickClock::default());
        assert!(kernel.timeline(armed).is_some(), "armed → has a timeline");
        assert!(kernel.timeline(bare).is_none(), "a different context stays bare");
    }

    /// Re-arming never clobbers the live timeline — the same handle comes back.
    #[tokio::test]
    async fn arm_is_idempotent() {
        let kernel = Kernel::new_ephemeral("test").await;
        let ctx = ContextId::new();
        let a = kernel.arm_timeline(ctx, TickClock::default());
        let b = kernel.arm_timeline(ctx, TickClock::default());
        assert!(std::sync::Arc::ptr_eq(&a, &b), "re-arm returns the same timeline");
    }

    /// Disarming drops the entry — the scheduler will skip a context with none.
    #[tokio::test]
    async fn disarm_removes_the_timeline() {
        let kernel = Kernel::new_ephemeral("test").await;
        let ctx = ContextId::new();
        kernel.arm_timeline(ctx, TickClock::default());
        kernel.disarm_timeline(ctx);
        assert!(kernel.timeline(ctx).is_none(), "disarmed → no timeline");
    }
}
