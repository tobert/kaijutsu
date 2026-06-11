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

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use kaijutsu_abc::ParseMode;
use kaijutsu_cas::{ContentHash, ContentStore, FileStore};
use kaijutsu_hyoushigi::{
    Body, Cell, ContextHash, ContextQuery, Fallback, Recipe, ResolveError, Resolution, Resolver,
    ResolverCtx, ResolverId, Span, TickDelta, Timeline,
};
use kaijutsu_types::{
    BlockId, BlockKind, BlockSnapshotBuilder, ContentType, ContextId, PrincipalId, Role, TrackId,
};
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
    /// Beats per phrase — the kernel's only musical chunking unit above the beat
    /// (phrases, not bars; bars live in ABC content and human-facing edges,
    /// translated at the edge). Plain `u64` for now; the field shape is documented
    /// open to per-phrase counts (irregular phrases) later — consumers go through
    /// [`phrase_delta`](Self::phrase_delta)/[`is_phrase_boundary`](Self::is_phrase_boundary),
    /// never the raw value.
    pub beats_per_phrase: u64,
    /// Beats per OODA cadence — the `tick` verb fires every `ooda_every` beats.
    /// Kept beat-denominated (the default is merely *expressed* in phrases); a
    /// phrase-typed field is deferred (issues.md).
    pub ooda_every: u64,
}

impl BeatPolicy {
    /// The composer default: a quarter note at 120 BPM (500 ms/beat) in 4/4, a
    /// 16-beat phrase (a 4-bar phrase in 4/4 collapsed at this edge), OODA every 8
    /// phrases (= 128 beats ≈ 64 s; numerically identical to the old 32*4, no
    /// cadence change). Tunable per context via rc.
    pub fn composer_default() -> Self {
        Self {
            period: Duration::from_millis(500),
            beats_per_phrase: 16,
            ooda_every: 8 * 16,
        }
    }

    /// One phrase of scheduling lead, as a tick delta. Returns a [`TickDelta`] and
    /// says so in the name — the OODA Act handoff schedules its ABC cell this far
    /// ahead of the playhead so the fast write-barrier derive has room.
    pub fn phrase_delta(&self) -> TickDelta {
        TickDelta::new(self.beats_per_phrase as i64)
    }

    /// Whether `beat_count` lands on a phrase boundary. A zero `beats_per_phrase`
    /// has no boundaries (defensive: never `% 0`).
    pub fn is_phrase_boundary(&self, beat_count: u64) -> bool {
        self.beats_per_phrase > 0 && beat_count % self.beats_per_phrase == 0
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
    /// `track` is the composer's lane identity (its default chair) — the lane the
    /// scheduled cells (and so the materialized blocks) belong to.
    Arm {
        context_id: ContextId,
        policy: BeatPolicy,
        track: TrackId,
    },
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

/// The constant MIME a notation cell commits under. The cell body *is* the ABC
/// text now (notation-first commits, §2); MIDI is derived from it at the write
/// barrier, never committed.
const ABC_MIME: &str = "text/vnd.abc";

/// Validate ABC bytes for the score: UTF-8, then a **Strict** parse that yields
/// ≥1 tune with no errors. Returns the parsed tunes on success, an error string
/// on any violation.
///
/// LOAD-BEARING: this uses [`ParseMode::Strict`], not the crate-default
/// [`kaijutsu_abc::parse`] (Generous). Generous *never* reports errors and always
/// fabricates a tune from arbitrary bytes — using it here would make every
/// validation gate (schedule, resolve, derive) vacuous, the exact opposite of the
/// design's "three loud gates". A scheduled composer phrase claims to be a
/// complete tune (it carries X:/M:/K: headers), so Strict is the honest validator:
/// it is what catches `malformed_abc_rejected_at_schedule`'s garbage. A vamped
/// phrase must yield playable output (no silent fallback to empty MIDI); the only
/// way to guarantee that downstream is to reject un-parseable ABC up front.
fn validate_abc(bytes: &[u8]) -> Result<Vec<kaijutsu_abc::Tune>, String> {
    let abc = std::str::from_utf8(bytes).map_err(|e| format!("ABC is not UTF-8: {e}"))?;
    let parsed = kaijutsu_abc::parse_with_mode(abc, ParseMode::Strict);
    if parsed.has_errors() {
        let first = parsed
            .errors()
            .next()
            .map(|f| f.message.clone())
            .unwrap_or_else(|| "unknown parse error".into());
        return Err(format!("ABC parse error: {first}"));
    }
    if parsed.value.is_empty() {
        return Err("ABC contained no tune".into());
    }
    Ok(parsed.value)
}

/// Register the production resolvers onto a freshly-created timeline. This is
/// the one seam where the kernel teaches a timeline what content it can
/// crystallize; each resolver is a downstream `impl Resolver` (the substrate
/// stays modality-agnostic). `cas` is handed in because content-producing
/// resolvers fetch their inputs (and the engine's crystallized bytes ultimately
/// land) in the content-addressed store.
///
/// Notation-first (§2): the single production resolver is now [`CasCommitResolver`]
/// — it commits the ABC text itself as the cell body. The old `AbcToMidiResolver`
/// (which committed *MIDI* and so polluted the `UseLastGood` candidate pool) is
/// deleted; its parse-and-render logic moved verbatim into [`AbcToMidiDeriver`],
/// run at the write barrier from the [`DeriverRegistry`].
pub fn register_resolvers(timeline: &mut Timeline, cas: Arc<FileStore>) {
    timeline.register_resolver(Box::new(CasCommitResolver { cas }));
}

/// Absorb a completed OODA turn's ABC decision onto a context's timeline — the
/// **Act** handoff. Stores the ABC text in CAS and schedules a `cas_commit` cell
/// `lead` ticks ahead of the playhead (fallback `UseLastGood`, so a missed resolve
/// repeats the last layer rather than dropping out). The model turn that
/// *produced* the ABC is the side-effecting, token-costing part and already ran
/// on the turn path; only this pure commit lands on the timeline.
///
/// Eager validation (§3): the ABC is parsed **before** anything is stored or
/// scheduled — the first of three loud gates (schedule → resolve → derive).
/// Malformed ABC Errs here, leaving nothing in CAS and nothing in the future (it
/// used to become a silently-Failed zombie cell). This preserves the
/// first-commit-validation invariant: every ABC that enters `committed` parsed at
/// least once, which is what makes a derive-time parse failure an *invariant
/// violation* (§5c) rather than weather.
///
/// `track` is the lane this section belongs to; `played_by` is the principal whose
/// turn produced the ABC (becomes BlockId.principal_id at materialization). A
/// `UseLastGood` fallback repeat under this cell is played by `beat()`, not the
/// player — that re-stamp happens in the engine, keeping vamp-insurance provenance
/// truthful.
///
/// Returns the scheduled cell's start tick. Errors (malformed ABC, no timeline,
/// CAS write, schedule-in-the-past) bubble — crash over silently dropping the
/// section.
pub fn schedule_abc_cell(
    kernel: &Kernel,
    context_id: ContextId,
    abc: &str,
    lead: TickDelta,
    track: TrackId,
    played_by: PrincipalId,
) -> anyhow::Result<kaijutsu_types::Tick> {
    // Gate 1 of 3: eager parse. Reject garbage before it touches CAS or the
    // timeline — a rejected schedule must leave zero residue.
    validate_abc(abc.as_bytes())
        .map_err(|e| anyhow::anyhow!("schedule_abc_cell: malformed ABC: {e}"))?;

    let timeline = kernel
        .timeline(context_id)
        .ok_or_else(|| anyhow::anyhow!("schedule_abc_cell: context {context_id} is not armed"))?;
    let hash = kernel.cas().store(abc.as_bytes(), ABC_MIME)?;

    let mut g = timeline.lock();
    let start = g.playhead() + lead;
    g.schedule(Cell::deferred_on(
        Span::instant(start),
        Recipe {
            resolver: ResolverId::new(CasCommitResolver::ID),
            params: serde_json::json!({ "hash": hash.as_str(), "mime": ABC_MIME }),
            query: ContextQuery::default(),
            fallback: Fallback::UseLastGood,
        },
        track,
        played_by,
    ))
    .map_err(|e| anyhow::anyhow!("schedule_abc_cell: {e}"))?;
    Ok(start)
}

/// The single production resolver (§2): commit CAS-referenced bytes onto the score
/// at a tick, validating by mime. The generic "put this content on the score"
/// resolver — notation today (`text/vnd.abc`), automation cells tomorrow (same
/// recipe shape, different mime).
///
/// This is the composer OODA loop's **Act** step, and it is deliberately a *pure*
/// resolver — idempotent, side-effect-free, safe to speculate and discard. It
/// reads bytes by **hash** from `params` (the `hash` the source content was stored
/// under), not from the committed timeline view, so the block log and the timeline
/// stay decoupled at the resolver boundary. The committed body is the *source*
/// (ABC), never a render — MIDI is derived at the write barrier (§4/§5), so the
/// `UseLastGood` candidate pool stays notation-pure by construction.
struct CasCommitResolver {
    cas: Arc<FileStore>,
}

impl CasCommitResolver {
    /// The single resolver id; recipes name it.
    pub const ID: &'static str = "cas_commit";

    fn param_str<'a>(params: &'a serde_json::Value, key: &str) -> Result<&'a str, ResolveError> {
        params
            .get(key)
            .and_then(|v| v.as_str())
            .ok_or_else(|| ResolveError::Failed(format!("cas_commit: missing `{key}` param")))
    }

    fn hash_param(params: &serde_json::Value) -> Result<ContentHash, ResolveError> {
        let s = Self::param_str(params, "hash")?;
        // Crash on a malformed hash — a bad hash is structural corruption, never
        // a lenient fallback (the doc's asymmetric boundary).
        ContentHash::from_str_checked(s)
            .map_err(|e| ResolveError::Failed(format!("cas_commit: malformed hash: {e}")))
    }
}

impl Resolver for CasCommitResolver {
    fn id(&self) -> ResolverId {
        ResolverId::new(Self::ID)
    }

    fn estimate_cost(&self, _params: &serde_json::Value, _rctx: &dyn ResolverCtx) -> Duration {
        // Hashing + a validate parse is sub-millisecond; keep a small floor so
        // lead-time derivation has something to chew on.
        Duration::from_millis(20)
    }

    fn compute_basis(&self, params: &serde_json::Value, _rctx: &dyn ResolverCtx) -> ContextHash {
        // The equivalence class is exactly the input hash: same hash → same bytes →
        // commits cleanly. Constant per cell, so this resolver never squashes on
        // its own basis. (ContextQuery growth is future work.)
        let h = params.get("hash").and_then(|v| v.as_str()).unwrap_or_default();
        ContextHash::of(h.as_bytes())
    }

    fn resolve(
        &self,
        params: &serde_json::Value,
        _rctx: &dyn ResolverCtx,
    ) -> Result<Resolution, ResolveError> {
        let hash = Self::hash_param(params)?;
        let mime = Self::param_str(params, "mime")?.to_string();
        let bytes = self
            .cas
            .retrieve(&hash)
            .map_err(|e| ResolveError::Failed(format!("cas_commit: CAS read: {e}")))?
            .ok_or_else(|| {
                ResolveError::Failed(format!("cas_commit: hash not in CAS: {}", hash.as_str()))
            })?;

        // Gate 2 of 3: per-mime validate. `text/vnd.abc` must parse; an unknown
        // mime is a defined pass-through (most content needs no kernel opinion) —
        // a documented default, never a silent fallback.
        if mime.as_str() == ABC_MIME {
            validate_abc(&bytes).map_err(|e| ResolveError::Failed(format!("cas_commit: {e}")))?;
        }

        // The committed body IS the source bytes under its source mime — never a
        // render. (MIDI for ABC is derived at the write barrier.)
        Ok(Resolution::new(bytes, mime))
    }
}

/// A pure, fast (≲1 ms) projection run at the write barrier, on the beat thread,
/// under the timeline lock (§4). Derivers MUST be deterministic, allocation-light,
/// and never block. Anything heavier (midi→pcm synthesis) is NOT a deriver — it
/// stays a timeline resolver with real lead time.
pub trait Deriver: Send + Sync {
    /// Exact-match mime key — the source content type this deriver projects from.
    fn source_mime(&self) -> &'static str;
    /// `(bytes, mime)` pairs for derived sibling blocks. `Err` is an **invariant
    /// violation**: content that passed first-commit validation must derive (§5c).
    fn derive(&self, bytes: &[u8]) -> anyhow::Result<Vec<(Vec<u8>, String)>>;
}

/// Mime-keyed derivation at the write barrier — the single extension point for
/// second consumers (automation cells, future baked renders). The substrate's
/// mime-blindness holds: the open-MIME→closed-render-hint projection still lives
/// in `materialize.rs`; the *registry* switch lives one layer up in the kernel
/// integrator, exactly where the kaijutsu-abc dependency already is. A mime with
/// no deriver derives nothing — the defined default (§9).
pub struct DeriverRegistry {
    by_mime: HashMap<&'static str, Box<dyn Deriver>>,
}

impl DeriverRegistry {
    /// The production registry: ABC → MIDI.
    pub fn production() -> Self {
        let mut by_mime: HashMap<&'static str, Box<dyn Deriver>> = HashMap::new();
        let d = AbcToMidiDeriver;
        by_mime.insert(d.source_mime(), Box::new(d));
        Self { by_mime }
    }

    /// An empty registry — no mime derives anything. Used to pin the pass-through
    /// default (an automation cell materializes without a sibling).
    pub fn empty() -> Self {
        Self {
            by_mime: HashMap::new(),
        }
    }

    pub fn get(&self, mime: &str) -> Option<&dyn Deriver> {
        self.by_mime.get(mime).map(|b| b.as_ref())
    }
}

/// ABC → MIDI: the old `AbcToMidiResolver`'s parse + `to_midi` moved verbatim, now
/// run at the write barrier rather than as a timeline resolver. A parse error here
/// is an invariant violation (the ABC passed schedule-time and resolve-time
/// validation, so it MUST parse again — §5c), surfaced as `Err`, never a silent
/// skip or a garbage block.
struct AbcToMidiDeriver;

impl Deriver for AbcToMidiDeriver {
    fn source_mime(&self) -> &'static str {
        ABC_MIME
    }

    fn derive(&self, bytes: &[u8]) -> anyhow::Result<Vec<(Vec<u8>, String)>> {
        let tunes = validate_abc(bytes).map_err(|e| {
            // Invariant violation: first-commit validation passed, so this must
            // parse. Loud `Err`, never a fabricated empty MIDI.
            anyhow::anyhow!("abc→midi derive: committed ABC failed to re-parse: {e}")
        })?;
        let tune = tunes
            .first()
            .ok_or_else(|| anyhow::anyhow!("abc→midi derive: validated ABC had no tune"))?;
        // §5b LOAD-BEARING: a vamped phrase must always yield playable output. The
        // derive runs for EVERY committed source — resolver commits AND engine
        // fallback copies alike — so a repeat is never a silent, render-less phrase.
        let midi = kaijutsu_abc::to_midi(tune, &kaijutsu_abc::MidiParams::default());
        Ok(vec![(midi, "audio/midi".to_string())])
    }
}

/// Per-context materialization cursor, owned by `BeatState` (§5). Replaces the
/// bare `high_water: usize`: a write-barrier crossing can fail mid-group (one
/// sibling journaled, the next not), so the cursor records both how far whole
/// cells have crossed AND how far the in-progress cell's artifact group got, so a
/// retry resumes at the failed artifact rather than re-inserting (and colliding
/// on) the ones that already landed.
#[derive(Debug, Default, Clone, Copy)]
pub struct MaterializeCursor {
    /// Committed cells fully materialized by THIS process's timeline (the bridge
    /// watermark — monotonic, NOT an id source). Reset to 0 on restart, consistent
    /// with the RAM committed log it indexes also being empty.
    pub high_water: usize,
    /// Artifacts of the in-progress cell already inserted — the resume point for
    /// per-artifact retry. 0 unless the previous attempt failed mid-group.
    pub artifacts_done: usize,
    /// The id of the in-progress cell's already-inserted SOURCE block, carried
    /// across a mid-group fault so a resume parents the derived siblings to the
    /// REAL source — never rediscovered from the log tail, which another writer
    /// (async model output, failure/poison Error blocks) may have advanced past
    /// the source between the faulting beat and the retry. Invariant: `Some` iff
    /// `artifacts_done >= 1`; set when artifact 0 lands, cleared at the cell
    /// boundary.
    pub source_block_id: Option<BlockId>,
}

/// Bridge a timeline's newly-committed cells into the context's CRDT block log —
/// the write-barrier crossing (§5).
///
/// Notation-first: the committed body is the *source* (ABC text). For each
/// committed cell past `cursor.high_water`, in strict order:
///
/// 1. **Bytes.** RAM-CAS first, durable CAS second (covers a `Fallback::Literal`
///    pre-stored durably), else **bail loudly** — missing bytes for a committed
///    cref is corruption; crash over it (replaces the pre-F2 silent `if let Some`
///    skip that left a dangling-hash block).
/// 2. **Derive BEFORE insert.** The [`DeriverRegistry`] projects sibling artifacts
///    from the source bytes; a derive failure bails before *any* insert, so a
///    half-cell never lands (§5c). Derive-before-insert is the resumability spine:
///    derivation is deterministic, so a retry regenerates the identical artifact
///    list and resumes at `cursor.artifacts_done`.
/// 3. **Crystallize.** `cas.store` for source and every derived artifact
///    (idempotent; the ABC re-store is a same-hash no-op).
/// 4. **Insert per-artifact, resumably.** The artifact list is `[source,
///    derived…]`. Skip the first `cursor.artifacts_done` (already landed on a
///    prior attempt). For each remaining, reserve an id in `cell.played_by`'s lane
///    (seq seeded from the loaded log across restart — the structural
///    DuplicateBlock fix, §3), insert, then bump `artifacts_done` immediately. On
///    insert/journal error: return `Err` with `high_water` un-advanced — the next
///    beat retries the SAME cell and resumes at the failed artifact (no
///    DuplicateBlock wedge, no silent repair). After the full group lands:
///    `artifacts_done = 0; high_water += 1`.
///
/// **Linkage.** The MIDI sibling carries `parent_id` = the ABC source block,
/// `tick` = the cell's tick (same beat coordinate), and the cell's `track` —
/// one-hop score↔render provenance a sink resolves today, before F1's track wire
/// fields are universal. Both blocks share the tick, so the `(order_key, BlockId)`
/// tie breaks by seq → ABC deterministically before its MIDI.
///
/// **Ephemeral.** Both artifacts are stamped `ephemeral = true` (§8): a
/// materialized phrase would otherwise hydrate as assistant text every phrase
/// (including fallback duplicates) — the flood. Score blocks are a durable
/// record of the shared score, not conversation turns, so the existing honest
/// hydration skip is the right bit to ride today; batch 2's hydration-marker
/// machinery will express "born past the marker" semantically and score blocks
/// migrate to it then. The blessing of `ephemeral` for machine-record (score)
/// use is documented at the type (`kaijutsu_types::block`), and the silence is
/// pinned by `materialized_score_blocks_are_hydration_silent` (mailbox tests)
/// across both hydration entry points.
///
/// `played_by` is who played — `PrincipalId::beat()` for transport-authored
/// fallbacks/literals, the player otherwise; minting ids under it keeps
/// `BlockId.principal_id` truthful (track is the lane, not the author).
///
/// Crashes over corrupting: a derive/CAS/insert/journal failure returns `Err` and
/// leaves `high_water` un-advanced for the failed cell, so a retry re-tries it.
pub fn materialize_committed(
    timeline: &SharedTimeline,
    cas: &FileStore,
    blocks: &SharedBlockStore,
    context_id: ContextId,
    cursor: &mut MaterializeCursor,
    derivers: &DeriverRegistry,
) -> anyhow::Result<Vec<BlockId>> {
    let guard = timeline.lock();
    let committed = guard.committed();
    let mut inserted = Vec::new();
    let mut after = blocks.last_block_id(context_id);

    while cursor.high_water < committed.len() {
        let cell = &committed[cursor.high_water];
        // Committed cells are always concrete; the guard is defensive, not a
        // silent skip — a non-concrete committed cell would be a real bug.
        let Body::Concrete(cref) = &cell.body else {
            anyhow::bail!(
                "committed cell at index {} is not concrete — timeline invariant violated",
                cursor.high_water
            );
        };

        // 1. Bytes. RAM-CAS first; durable CAS second (Fallback::Literal pre-stored
        //    durably); else BAIL. A committed cref with no bytes anywhere is
        //    corruption — crash over it, never a dangling-hash block. (This replaces
        //    the pre-F2 silent crystallize-skip.)
        let src_bytes = match guard.content_bytes(&cref.hash) {
            Some(b) => b.to_vec(),
            None => cas
                .retrieve(&cref.hash)
                .map_err(|e| anyhow::anyhow!("materialize: durable CAS read: {e}"))?
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "materialize: committed cref {} has no bytes in RAM-CAS or durable CAS — \
                         corruption, refusing to materialize a dangling-hash block",
                        cref.hash.as_str()
                    )
                })?,
        };

        // 2. Derive BEFORE insert. A derive failure (invariant violation, §5c) bails
        //    before any insert so no half-cell lands; the cell retries (and re-fails)
        //    every beat — a loud head-of-line wedge, never a silent skip.
        let derived = match derivers.get(&cref.mime) {
            Some(d) => d.derive(&src_bytes).map_err(|e| {
                anyhow::anyhow!(
                    "materialize: deriver for mime {} failed (invariant violation — \
                     committed content must derive): {e}",
                    cref.mime
                )
            })?,
            None => Vec::new(), // no deriver registered → no siblings (the §9 default)
        };

        // 3. Crystallize source + every derived artifact into durable CAS
        //    (idempotent; the source re-store is a same-hash no-op).
        let stored = cas.store(&src_bytes, &cref.mime)?;
        debug_assert_eq!(
            stored, cref.hash,
            "durable CAS hash must match the cell's ContentRef hash"
        );
        let mut artifacts: Vec<(String, ContentHash)> = Vec::with_capacity(1 + derived.len());
        artifacts.push((cref.mime.clone(), cref.hash.clone()));
        for (bytes, dmime) in derived {
            let dhash = cas.store(&bytes, &dmime)?;
            artifacts.push((dmime, dhash));
        }

        // 4. Insert per-artifact, resumably. artifact[0] is the source; the rest are
        //    derived siblings whose parent_id is the source block.
        //
        // §5c escape hatch (judge-3 graft, DOCUMENTED NOT BUILT): a resumed cell
        // re-derives the identical artifact list (derivation is deterministic), so
        // `cursor.artifacts_done` skips the artifacts that already landed. If a wedge
        // ever proves operationally noisy from *transient-but-recurring* contention
        // (journal lock, NOT data corruption), the approved remedy is an idempotent
        // resume that VERIFIES the existing block matches before continuing — written
        // down here so it never gets hot-patched into silent repair under gig
        // pressure. Trigger: a wedge whose cause is confirmed transient-recurring.
        let mut src_id: Option<BlockId> = None;
        for (i, (amime, ahash)) in artifacts.iter().enumerate() {
            if i < cursor.artifacts_done {
                // Already landed on a prior attempt. Recover the source id from the
                // cursor (carried across the fault), NOT from `after`/the log tail —
                // a concurrent writer may have appended past the source between the
                // faulting beat and this retry, and parenting a sibling to that
                // foreign block would silently corrupt the score↔render provenance.
                if i == 0 {
                    src_id = cursor.source_block_id;
                    debug_assert!(
                        src_id.is_some(),
                        "resume with artifacts_done>0 must carry the source block id"
                    );
                }
                continue;
            }

            let block_id = blocks.reserve_block_id(context_id, cell.played_by)?;
            debug_assert_eq!(block_id.principal_id, cell.played_by);

            let snapshot = if i == 0 {
                // Source block: substrate-materialized (text → Role::Model + inline +
                // ContentType::Abc + tick + track), then stamped ephemeral kernel-side.
                // The stamp rides the existing honest hydration skip: a score block is
                // a durable record of the shared score, not a conversation turn, so it
                // must not flood the LLM as assistant text once per materialized phrase
                // (see the type-level blessing and the §8 doc above). Pinned by
                // materialized_score_blocks_are_hydration_silent.
                let mut snap = guard
                    .materialize(cell, block_id)
                    .expect("a concrete cell always materializes");
                snap.ephemeral = true;
                snap
            } else {
                // Derived sibling: built kernel-side. Same tick (same beat
                // coordinate), Role::Asset, content = the 32-hex CAS hash (img_block
                // convention), parent_id = the source block (one-hop provenance), the
                // cell's track, ephemeral.
                let parent = src_id.expect("source artifact precedes its siblings");
                BlockSnapshotBuilder::new(block_id, BlockKind::Text)
                    .tick(cell.span.start)
                    .role(Role::Asset)
                    .content(ahash.as_str())
                    .content_type(ContentType::from_mime(amime))
                    .parent_id(parent)
                    .track(cell.track.clone())
                    .ephemeral(true)
                    .build()
            };

            let id = blocks.insert_from_snapshot_as(
                context_id,
                snapshot,
                after.as_ref(),
                Some(cell.played_by),
            )?;
            if i == 0 {
                src_id = Some(id);
                // Persist the source id BEFORE advancing artifacts_done — a fault on
                // the next artifact must resume with this exact id, not the log tail.
                cursor.source_block_id = Some(id);
            }
            after = Some(id);
            inserted.push(id);
            // Advance the resume point immediately — a failure on the NEXT artifact
            // must resume here, not re-insert this one.
            cursor.artifacts_done = i + 1;
        }

        // The full group landed — advance past the cell and clear the resume point
        // (artifacts_done back to 0, source id dropped: the invariant is `Some` iff
        // a partial group is outstanding).
        cursor.artifacts_done = 0;
        cursor.source_block_id = None;
        cursor.high_water += 1;
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

    use super::{DeriverRegistry, MaterializeCursor, SharedTimeline, materialize_committed};
    use crate::block_store::{BlockStore, DocumentKind};
    use crate::flows::{BlockFlow, FlowBus, SharedBlockFlowBus};
    use crate::kernel::Kernel;

    /// The canonical test phrase — a B♭ Dorian two-chord vamp, M:4/4, exactly one
    /// 16-beat phrase. Shared with the kaijutsu-abc fixture suite (T1). Used wherever
    /// a test needs real, parse-clean ABC that derives to playable MIDI.
    const VAMP_ABC: &str = include_str!("../../../kaijutsu-abc/tests/fixtures/chameleon_vamp.abc");

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

    /// Build a timeline that has already committed one cell of the given content via
    /// the `Fixed` resolver (used by the F1-landed bridge/restart pins, which test
    /// the generic single-block bridge independent of ABC→MIDI derivation).
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
        tl.schedule(Cell::deferred_on(
            Span::instant(Tick::new(10)),
            Recipe {
                resolver: ResolverId::new("fixed"),
                params: serde_json::Value::Null,
                query: ContextQuery::default(),
                fallback: Fallback::Skip,
            },
            kaijutsu_types::TrackId::solo(),
            kaijutsu_types::PrincipalId::beat(),
        ))
        .unwrap();
        tl.advance_to(Tick::new(10));
        assert_eq!(tl.committed().len(), 1, "cell should have committed");
        Arc::new(Mutex::new(tl))
    }

    /// Build a timeline that has already committed one **notation** cell: schedule
    /// ABC through the real `cas_commit` resolver (the production path) so the
    /// committed body is `text/vnd.abc` and the write barrier derives its MIDI
    /// sibling. `cas` is the durable store the resolver reads the ABC from.
    fn timeline_with_committed_abc(cas: &Arc<kaijutsu_cas::FileStore>, abc: &str) -> SharedTimeline {
        use kaijutsu_cas::ContentStore;
        let hash = cas.store(abc.as_bytes(), super::ABC_MIME).unwrap();
        let mut tl = Timeline::new(TickClock {
            ticks_per_sec: 1.0,
            safety_factor: 1.0,
            commit_margin: TickDelta::new(0),
        });
        super::register_resolvers(&mut tl, cas.clone());
        tl.schedule(Cell::deferred_on(
            Span::instant(Tick::new(10)),
            Recipe {
                resolver: ResolverId::new(super::CasCommitResolver::ID),
                params: serde_json::json!({ "hash": hash.as_str(), "mime": super::ABC_MIME }),
                query: ContextQuery::default(),
                fallback: Fallback::UseLastGood,
            },
            kaijutsu_types::TrackId::solo(),
            kaijutsu_types::PrincipalId::beat(),
        ))
        .unwrap();
        tl.advance_to(Tick::new(10));
        assert_eq!(tl.committed().len(), 1, "the notation cell committed");
        Arc::new(Mutex::new(tl))
    }

    /// Common DB-less block store + a fresh durable CAS for the materializer tests.
    fn store_and_cas() -> (
        crate::block_store::SharedBlockStore,
        Arc<kaijutsu_cas::FileStore>,
        ContextId,
        tempfile::TempDir,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let cas = Arc::new(kaijutsu_cas::FileStore::at_path(dir.path()));
        let blocks: crate::block_store::SharedBlockStore =
            Arc::new(BlockStore::new(kaijutsu_types::PrincipalId::new()));
        let ctx = ContextId::new();
        blocks
            .create_document(ctx, DocumentKind::Conversation, None)
            .unwrap();
        (blocks, cas, ctx, dir)
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
        let mut cursor = MaterializeCursor::default();
        // An empty registry: this F1 pin exercises the generic single-block bridge
        // (binary cell → Asset block), independent of ABC→MIDI derivation.
        let derivers = DeriverRegistry::empty();

        let inserted =
            materialize_committed(&tl, &cas, &blocks, ctx, &mut cursor, &derivers).unwrap();
        assert_eq!(inserted.len(), 1, "one committed cell → one block");
        assert_eq!(cursor.high_water, 1, "high-water advances past the materialized cell");

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

        // (2) the block is in the log, carrying its beat tick, content = CAS hash,
        // and its LANE (track). The shim cell plays under beat() on the solo() lane,
        // so track=Some(solo()) and the author is beat() — the lane is NOT the author.
        let snaps = blocks.block_snapshots(ctx).unwrap();
        assert_eq!(snaps.len(), 1);
        assert_eq!(snaps[0].tick, Some(Tick::new(10)), "block carries the beat coordinate");
        assert_eq!(snaps[0].content, cref.hash.as_str(), "binary content points at CAS by hash");
        assert_eq!(
            snaps[0].track,
            Some(kaijutsu_types::TrackId::solo()),
            "the materialized block carries its lane"
        );
        assert_eq!(snaps[0].id.principal_id, kaijutsu_types::PrincipalId::beat());

        // (3) a BlockFlow::Inserted was emitted (it syncs).
        let msg = sub.try_recv().expect("an Inserted event should have been emitted");
        assert!(matches!(msg.payload, BlockFlow::Inserted { .. }));

        // Idempotent: nothing new on a second pass.
        let again =
            materialize_committed(&tl, &cas, &blocks, ctx, &mut cursor, &derivers).unwrap();
        assert!(again.is_empty(), "no new committed cells → no new blocks");
        assert_eq!(blocks.block_snapshots(ctx).unwrap().len(), 1);
    }

    /// T9 (design-chameleon-batch1-f1-track §8 Phase 3) — THE DuplicateBlock hazard
    /// pin. After F1, materialize_committed reserves its id from the store's
    /// per-principal seq lane (seeded from the loaded log across restart), so the
    /// first post-restart materialization mints max+1 in cell.played_by's lane — no
    /// collision, no silent retry. Walks the REAL restore path.
    #[tokio::test]
    async fn materialize_after_reload_mints_fresh_seq_no_duplicate() {
        use kaijutsu_types::{PrincipalId, now_millis};

        use crate::block_store::{DbHandle, SharedBlockStore};
        use crate::kernel_db::{DocumentRow, KernelDb};

        let dir = tempfile::tempdir().unwrap();
        let cas = kaijutsu_cas::FileStore::at_path(dir.path());

        // A DB-backed store so the materialized block survives drop+reload.
        let db_path = dir.path().join("t9.db");
        let db: DbHandle = Arc::new(Mutex::new(KernelDb::open(&db_path).expect("open DB")));
        let creator = PrincipalId::system();
        let ws_id = {
            let g = db.lock();
            g.get_or_create_default_workspace(creator).expect("workspace")
        };

        let ctx = ContextId::new();
        {
            let g = db.lock();
            g.insert_document(&DocumentRow {
                document_id: ctx,
                workspace_id: ws_id,
                doc_kind: DocumentKind::Conversation,
                language: None,
                path: None,
                created_at: now_millis() as i64,
                created_by: creator,
            })
            .unwrap();
        }

        let blocks: SharedBlockStore = Arc::new(BlockStore::with_db(db.clone(), ws_id, creator));
        blocks
            .create_document(ctx, DocumentKind::Conversation, None)
            .unwrap();

        // First materialization (pre-restart). The shim Cell plays under beat().
        let tl1 = timeline_with_one_committed("audio/midi", b"MThd-pre-restart");
        let mut cursor = MaterializeCursor::default();
        let derivers = DeriverRegistry::empty();
        let inserted =
            materialize_committed(&tl1, &cas, &blocks, ctx, &mut cursor, &derivers).unwrap();
        assert_eq!(inserted.len(), 1);
        let pre_seq = inserted[0].seq;
        assert_eq!(inserted[0].principal_id, PrincipalId::beat(), "shim cell plays under beat()");

        // Drop the store; reload through the REAL restore path. The fresh store's
        // seq_lanes must be seeded from the loaded log so beat()'s lane = pre+1.
        drop(blocks);
        let blocks2: SharedBlockStore = Arc::new(BlockStore::with_db(db.clone(), ws_id, creator));
        blocks2.load_from_db().expect("load_from_db");
        assert_eq!(
            blocks2.block_snapshots(ctx).unwrap().len(),
            1,
            "the pre-restart block survives reload"
        );

        // The re-arm shape: a brand-new committed timeline, cursor back to 0.
        let tl2 = timeline_with_one_committed("audio/midi", b"MThd-post-restart");
        let mut cursor2 = MaterializeCursor::default();

        // THE hazard pin: must be Ok (NOT DuplicateBlock swallowed at beat.rs:234).
        let inserted2 = materialize_committed(&tl2, &cas, &blocks2, ctx, &mut cursor2, &derivers)
            .expect("post-restart materialization must not DuplicateBlock");
        assert_eq!(inserted2.len(), 1, "post-restart cell → one fresh block");
        assert_eq!(
            inserted2[0].seq,
            pre_seq + 1,
            "fresh seq must be old max + 1 (seeded from the loaded log), not a re-mint of seq {pre_seq}",
        );
        assert_eq!(
            inserted2[0].principal_id,
            PrincipalId::beat(),
            "fresh block stays in the player's (beat()) lane"
        );

        // Both blocks coexist — no collision, no overwrite.
        assert_eq!(
            blocks2.block_snapshots(ctx).unwrap().len(),
            2,
            "the reloaded block and the fresh one coexist"
        );
    }

    /// T7 (design §3, §16) — malformed ABC is rejected at SCHEDULE time, eagerly,
    /// before anything is stored or scheduled. The first of three loud validation
    /// points (schedule → resolve → derive). Garbage must never enter the score.
    #[tokio::test]
    async fn malformed_abc_rejected_at_schedule() {
        let kernel = Kernel::new_ephemeral("test").await;
        let ctx = ContextId::new();
        kernel.arm_timeline(ctx, TickClock::default(), kaijutsu_types::Tick::ZERO);

        let lead = TickDelta::new(16);
        let track = kaijutsu_types::TrackId::solo();
        let player = kaijutsu_types::PrincipalId::beat();

        let err = super::schedule_abc_cell(
            &kernel,
            ctx,
            "this is not abc {{{ ][",
            lead,
            track.clone(),
            player,
        );
        assert!(err.is_err(), "malformed ABC must be rejected loudly at schedule time");

        let tl = kernel.timeline(ctx).expect("armed");
        let g = tl.lock();
        assert_eq!(g.future_len(), 0, "a rejected schedule leaves the future empty");
        assert_eq!(g.committed().len(), 0, "nothing committed either");
    }

    /// A malformed `hash` param fails the `cas_commit` resolve (crash over
    /// corruption), never a silent commit. The cell ends `Failed`, nothing commits.
    /// (Renamed from `abc_to_midi_rejects_malformed_hash`; same asymmetric-boundary
    /// behavior, new resolver.)
    #[tokio::test]
    async fn cas_commit_rejects_malformed_hash() {
        use kaijutsu_cas::FileStore;
        let dir = tempfile::tempdir().unwrap();
        let cas = Arc::new(FileStore::at_path(dir.path()));

        let mut tl = Timeline::new(TickClock {
            ticks_per_sec: 1.0,
            safety_factor: 1.0,
            commit_margin: TickDelta::new(0),
        });
        super::register_resolvers(&mut tl, cas);

        tl.schedule(Cell::deferred_on(
            Span::instant(Tick::new(1)),
            Recipe {
                resolver: ResolverId::new(super::CasCommitResolver::ID),
                params: serde_json::json!({ "hash": "not-a-hash", "mime": super::ABC_MIME }),
                query: ContextQuery::default(),
                fallback: Fallback::Skip,
            },
            kaijutsu_types::TrackId::solo(),
            kaijutsu_types::PrincipalId::beat(),
        ))
        .unwrap();
        tl.advance_to(Tick::new(1));

        assert!(tl.committed().is_empty(), "a bad hash must not commit content");
    }

    /// T8 (design §16) — a score cell commits its ABC and the write barrier derives
    /// a MIDI sibling. block[0] is the ABC source (Role::Model, inline ABC,
    /// ContentType::Abc, ephemeral, tick == cell tick); block[1] is the MIDI sibling
    /// (Role::Asset, 32-hex hash, parent_id == block[0].id, same tick, ephemeral,
    /// CAS bytes start MThd). A second materialize pass is idempotent.
    #[tokio::test]
    async fn score_cell_commits_abc_and_derives_midi_sibling() {
        use kaijutsu_cas::ContentStore;
        let (blocks, cas, ctx, _dir) = store_and_cas();

        let tl = timeline_with_committed_abc(&cas, VAMP_ABC);
        let mut cursor = MaterializeCursor::default();
        let derivers = DeriverRegistry::production();

        // T22 (design-chameleon-batch1-f2-notation §16, §14.22): the deriver-budget
        // convention (≲1 ms per cell, §4) gets a measured number. This times the
        // WHOLE write-barrier crossing for one cell — bytes fetch + ABC parse +
        // to_midi derive + CAS store + both block inserts — not just the derive. Run
        // with `-- --nocapture` to read it; it is a print, never an assert, because a
        // CI-machine threshold would be flaky, but a vamp phrase that blew the budget
        // here would still be a loud signal at the bench.
        let t22 = std::time::Instant::now();
        let inserted =
            materialize_committed(&tl, &cas, &blocks, ctx, &mut cursor, &derivers).unwrap();
        let t22_elapsed = t22.elapsed();
        println!(
            "T22 derive+insert per cell (ABC parse + to_midi + CAS + 2 inserts): {:?} \
             (budget ≲1 ms)",
            t22_elapsed
        );
        assert_eq!(inserted.len(), 2, "ABC source + one derived MIDI sibling");
        assert_eq!(cursor.high_water, 1, "one committed cell crossed");

        let snaps = blocks.block_snapshots(ctx).unwrap();
        assert_eq!(snaps.len(), 2);

        let src = &snaps[0];
        assert_eq!(src.role, kaijutsu_types::Role::Model, "ABC inlines as a model staff");
        assert_eq!(src.content, VAMP_ABC, "the committed body IS the ABC text");
        assert_eq!(src.content_type, kaijutsu_types::ContentType::Abc);
        assert!(src.ephemeral, "the score block is ephemeral (hydration-silent)");
        assert_eq!(src.tick, Some(Tick::new(10)), "ABC carries the beat coordinate");

        let sib = &snaps[1];
        assert_eq!(sib.role, kaijutsu_types::Role::Asset, "MIDI is an asset");
        assert_eq!(sib.content.len(), 32, "content is the 32-hex CAS hash");
        assert_eq!(
            sib.parent_id,
            Some(src.id),
            "the MIDI sibling parents to its ABC source (one-hop provenance)"
        );
        assert_eq!(sib.tick, Some(Tick::new(10)), "same beat coordinate as the ABC");
        assert!(sib.ephemeral, "the MIDI sibling is ephemeral too");
        assert_eq!(sib.track, src.track, "same lane");

        let hash = kaijutsu_cas::ContentHash::from_str_checked(&sib.content).unwrap();
        let midi = cas.retrieve(&hash).unwrap().expect("MIDI bytes in CAS");
        assert!(midi.starts_with(b"MThd"), "derived MIDI is a valid SMF");

        // Idempotent: a second pass inserts nothing (siblings included).
        let again =
            materialize_committed(&tl, &cas, &blocks, ctx, &mut cursor, &derivers).unwrap();
        assert!(again.is_empty(), "second pass is a no-op");
        assert_eq!(blocks.block_snapshots(ctx).unwrap().len(), 2);
    }

    /// The per-artifact resume must parent the derived sibling to the REAL source
    /// block of the in-progress cell, NOT whatever happens to be the log tail at
    /// retry time. The existing T13 pins resume mechanics but never appends between
    /// the fault and the retry, so its source stays the tail and a tail-recovery
    /// bug passes silently. Here a foreign writer (async model output, a
    /// failure/poison Error block) lands between the beats; if the resume rebuilt
    /// the source id from `last_block_id`, the MIDI sibling would parent that
    /// foreign block and corrupt the one-hop score↔render provenance playback needs.
    #[tokio::test]
    async fn resume_parents_sibling_to_real_source_not_log_tail() {
        use kaijutsu_types::{BlockKind, ContentType, PrincipalId, Role, Status};

        let (blocks, cas, ctx, _dir) = store_and_cas();
        let tl = timeline_with_committed_abc(&cas, VAMP_ABC);
        let mut cursor = MaterializeCursor::default();
        let derivers = DeriverRegistry::production();

        // Fault the sibling insert (artifact list is [source, MIDI]; the MIDI is
        // insert #2) so the source lands and the cell faults mid-group — the REAL
        // boundary, same seam as T13.
        blocks.arm_insert_fault(2);
        let r1 = materialize_committed(&tl, &cas, &blocks, ctx, &mut cursor, &derivers);
        assert!(r1.is_err(), "the injected sibling fault surfaces as Err");
        assert_eq!(cursor.artifacts_done, 1, "the source landed; resume at the sibling");
        let source_id = blocks.block_snapshots(ctx).unwrap()[0].id;
        assert_eq!(
            cursor.source_block_id,
            Some(source_id),
            "the faulting beat carried the source id forward on the cursor"
        );

        // A foreign writer (async model output, a failure/poison Error block, …)
        // appends between beats — the source is NO LONGER the log tail.
        let foreign = blocks
            .insert_block_as(
                ctx,
                None,
                Some(&source_id),
                Role::Model,
                BlockKind::Text,
                "an interleaved chat/tool/error block",
                Status::Done,
                ContentType::Plain,
                Some(PrincipalId::system()),
            )
            .unwrap();
        assert_ne!(foreign, source_id);
        assert_eq!(
            blocks.last_block_id(ctx),
            Some(foreign),
            "the log tail is now the foreign block, not the source"
        );

        // The retry beat resumes the SAME cell at the MIDI sibling (fault spent).
        let inserted =
            materialize_committed(&tl, &cas, &blocks, ctx, &mut cursor, &derivers).unwrap();
        assert_eq!(inserted.len(), 1, "resume inserts exactly the missing MIDI sibling");

        let sib = blocks.get_block_snapshot(ctx, &inserted[0]).unwrap().unwrap();
        assert_eq!(
            sib.parent_id,
            Some(source_id),
            "the resumed MIDI sibling parents its REAL ABC source, not the log tail"
        );
        assert_ne!(
            sib.parent_id,
            Some(foreign),
            "and emphatically NOT the foreign block that became the tail"
        );
    }

    /// Commit the VAMP ABC at a chosen tick via the real `cas_commit` resolver,
    /// returning a timeline with exactly that one notation cell committed. The
    /// tick parameter lets a second (post-restart) phrase land after the first.
    fn timeline_with_committed_abc_at(
        cas: &Arc<kaijutsu_cas::FileStore>,
        abc: &str,
        tick: i64,
    ) -> SharedTimeline {
        use kaijutsu_cas::ContentStore;
        let hash = cas.store(abc.as_bytes(), super::ABC_MIME).unwrap();
        let mut tl = Timeline::new(TickClock {
            ticks_per_sec: 1.0,
            safety_factor: 1.0,
            commit_margin: TickDelta::new(0),
        });
        super::register_resolvers(&mut tl, cas.clone());
        tl.schedule(Cell::deferred_on(
            Span::instant(Tick::new(tick)),
            Recipe {
                resolver: ResolverId::new(super::CasCommitResolver::ID),
                params: serde_json::json!({ "hash": hash.as_str(), "mime": super::ABC_MIME }),
                query: ContextQuery::default(),
                fallback: Fallback::UseLastGood,
            },
            kaijutsu_types::TrackId::solo(),
            kaijutsu_types::PrincipalId::beat(),
        ))
        .unwrap();
        tl.advance_to(Tick::new(tick));
        assert_eq!(tl.committed().len(), 1, "the notation cell committed");
        Arc::new(Mutex::new(tl))
    }

    /// T20 (design-chameleon-batch1-f2-notation §16, §11.1) — the restart pin over
    /// the NEW two-block (ABC source + MIDI sibling) shape, end-to-end. The F1
    /// single-block pin (`materialize_after_reload_mints_fresh_seq_no_duplicate`)
    /// uses `DeriverRegistry::empty()` and one `audio/midi` block; this exercises
    /// the production deriver, so each phrase materializes a PAIR. After a real
    /// drop+reload (the only Timeline-evaporating boundary), a second phrase must:
    ///   - mint BOTH new blocks past every persisted seq on the beat() lane (no
    ///     `DuplicateBlock` — the silent poison loop), and
    ///   - sort AFTER the reloaded pair (the post-restart phrase is later musically,
    ///     so its ticks/order keys append, never interleave mid-document).
    #[tokio::test]
    async fn materialize_after_restart_does_not_collide() {
        use kaijutsu_types::{PrincipalId, now_millis};

        use crate::block_store::{DbHandle, SharedBlockStore};
        use crate::kernel_db::{DocumentRow, KernelDb};

        let dir = tempfile::tempdir().unwrap();
        let cas = Arc::new(kaijutsu_cas::FileStore::at_path(dir.path()));

        // A DB-backed store so the materialized pair survives drop+reload.
        let db_path = dir.path().join("t20.db");
        let db: DbHandle = Arc::new(Mutex::new(KernelDb::open(&db_path).expect("open DB")));
        let creator = PrincipalId::system();
        let ws_id = {
            let g = db.lock();
            g.get_or_create_default_workspace(creator).expect("workspace")
        };
        let ctx = ContextId::new();
        {
            let g = db.lock();
            g.insert_document(&DocumentRow {
                document_id: ctx,
                workspace_id: ws_id,
                doc_kind: DocumentKind::Conversation,
                language: None,
                path: None,
                created_at: now_millis() as i64,
                created_by: creator,
            })
            .unwrap();
        }

        let blocks: SharedBlockStore = Arc::new(BlockStore::with_db(db.clone(), ws_id, creator));
        blocks.create_document(ctx, DocumentKind::Conversation, None).unwrap();

        let derivers = DeriverRegistry::production();

        // Pre-restart: a phrase at tick 10 materializes the ABC+MIDI pair.
        let tl1 = timeline_with_committed_abc_at(&cas, VAMP_ABC, 10);
        let mut cursor1 = MaterializeCursor::default();
        let pre = materialize_committed(&tl1, &cas, &blocks, ctx, &mut cursor1, &derivers).unwrap();
        assert_eq!(pre.len(), 2, "pre-restart phrase → ABC source + MIDI sibling");
        let pre_max_seq = pre.iter().map(|id| id.seq).max().unwrap();

        // Drop the store; reload through the REAL restore path. The Timeline and
        // cursor evaporate; only the block log + CAS persist. seq_lanes must be
        // re-seeded from the loaded log so the beat() lane resumes past pre_max_seq.
        drop(blocks);
        let blocks2: SharedBlockStore = Arc::new(BlockStore::with_db(db.clone(), ws_id, creator));
        blocks2.load_from_db().expect("load_from_db");
        let reloaded = blocks2.block_snapshots(ctx).unwrap();
        assert_eq!(reloaded.len(), 2, "the pre-restart pair survives reload");

        // The re-arm shape: a brand-new committed timeline at a LATER tick, cursor
        // back to 0 (it counts this-process materialization only).
        let tl2 = timeline_with_committed_abc_at(&cas, VAMP_ABC, 26);
        let mut cursor2 = MaterializeCursor::default();
        let post = materialize_committed(&tl2, &cas, &blocks2, ctx, &mut cursor2, &derivers)
            .expect("post-restart materialization must not DuplicateBlock");
        assert_eq!(post.len(), 2, "post-restart phrase → its own ABC+MIDI pair");

        // Both new blocks mint PAST every persisted seq — no re-mint, no collision.
        for id in &post {
            assert!(
                id.seq > pre_max_seq,
                "post-restart block seq {} must exceed the persisted max seq {} \
                 (re-minting is the DuplicateBlock collision)",
                id.seq,
                pre_max_seq,
            );
        }

        // Four blocks coexist and the post-restart pair sorts AFTER the reloaded
        // pair: the later phrase appends, never interleaves mid-document.
        let final_snaps = blocks2.block_snapshots(ctx).unwrap();
        assert_eq!(final_snaps.len(), 4, "two pairs coexist after restart");
        let post_set: std::collections::HashSet<_> = post.iter().copied().collect();
        let tail_two: Vec<_> = final_snaps.iter().rev().take(2).map(|s| s.id).collect();
        for id in &tail_two {
            assert!(
                post_set.contains(id),
                "the two trailing blocks must be the post-restart pair (later phrase sorts last)"
            );
        }
    }

    /// T9 (design §16) — THE mandated fallback verification. A `UseLastGood` repeat
    /// commits a *concrete copy* of the last good ABC at a new tick; the materializer
    /// cannot tell it from a resolver commit, so it derives MIDI for it too. Same ABC
    /// → same MIDI bytes → same CAS hash, but a NEW block pair at the new tick.
    ///
    /// §5b LOAD-BEARING: the fallback path derives a MIDI sibling because a vamped
    /// phrase MUST yield playable output — there is no second repeat mechanism, no
    /// sink-side fallback hook, no silent "fallback emits nothing". A pull-from-CAS
    /// sink fires the repeat from the fresh block pair exactly as it fires the
    /// original. (The hash identity is the structural proof it is the SAME phrase.)
    #[tokio::test]
    async fn fallback_committed_abc_also_derives_midi() {
        let (blocks, cas, ctx, _dir) = store_and_cas();

        // A resolver that commits the VAMP ABC but whose BASIS reads ambient "beat" —
        // a change to ambient between speculate and commit drives a squash. With no
        // respeculation budget the squash fires the `UseLastGood` fallback: a concrete
        // copy of the last good ABC on this lane, at the new tick. This is the
        // production miss path (a missed/diverged phrase), not a resolve error (which
        // §6 routes to the failure ledger, not fallback).
        struct AbcBeat;
        impl Resolver for AbcBeat {
            fn id(&self) -> ResolverId {
                ResolverId::new("abc_beat")
            }
            fn estimate_cost(&self, _p: &serde_json::Value, _c: &dyn ResolverCtx) -> std::time::Duration {
                std::time::Duration::from_secs(3) // est 3 ticks > budget → no re-spec
            }
            fn compute_basis(&self, _p: &serde_json::Value, c: &dyn ResolverCtx) -> ContextHash {
                ContextHash::of(&c.ambient("beat").unwrap_or_default())
            }
            fn resolve(
                &self,
                _p: &serde_json::Value,
                _c: &dyn ResolverCtx,
            ) -> Result<Resolution, ResolveError> {
                Ok(Resolution::new(VAMP_ABC.as_bytes().to_vec(), super::ABC_MIME.to_string()))
            }
        }

        let mut tl = Timeline::new(TickClock {
            ticks_per_sec: 1.0,
            safety_factor: 2.0,
            commit_margin: TickDelta::new(1),
        });
        tl.register_resolver(Box::new(AbcBeat));
        tl.set_ambient("beat", *b"A");

        // Good phrase at tick 10 (commits the VAMP ABC under the solo lane).
        tl.schedule(Cell::deferred_on(
            Span::instant(Tick::new(10)),
            Recipe {
                resolver: ResolverId::new("abc_beat"),
                params: serde_json::Value::Null,
                query: ContextQuery::default(),
                fallback: Fallback::UseLastGood,
            },
            kaijutsu_types::TrackId::solo(),
            kaijutsu_types::PrincipalId::beat(),
        ))
        .unwrap();
        tl.advance_to(Tick::new(10));
        assert_eq!(tl.committed().len(), 1, "good phrase committed");

        // Next phrase at tick 20: speculate against "A", diverge ambient → squash with
        // no budget → UseLastGood fires a concrete copy of the good ABC at tick 20.
        tl.schedule(Cell::deferred_on(
            Span::instant(Tick::new(20)),
            Recipe {
                resolver: ResolverId::new("abc_beat"),
                params: serde_json::Value::Null,
                query: ContextQuery::default(),
                fallback: Fallback::UseLastGood,
            },
            kaijutsu_types::TrackId::solo(),
            kaijutsu_types::PrincipalId::beat(),
        ))
        .unwrap();
        tl.advance_to(Tick::new(14)); // speculate against "A"
        tl.set_ambient("beat", *b"B"); // diverge before commit
        tl.advance_to(Tick::new(20)); // squash → UseLastGood fallback
        assert_eq!(tl.squashes().len(), 1, "the missed phrase squashed");
        assert_eq!(tl.committed().len(), 2, "the fallback pushed a concrete repeat of the good ABC");

        let tl = Arc::new(Mutex::new(tl));
        let mut cursor = MaterializeCursor::default();
        let derivers = DeriverRegistry::production();
        let inserted =
            materialize_committed(&tl, &cas, &blocks, ctx, &mut cursor, &derivers).unwrap();
        // Two cells × (ABC + MIDI) = 4 blocks.
        assert_eq!(inserted.len(), 4, "both phrases materialize ABC + MIDI");

        let snaps = blocks.block_snapshots(ctx).unwrap();
        assert_eq!(snaps.len(), 4);
        let midis: Vec<_> = snaps
            .iter()
            .filter(|s| s.role == kaijutsu_types::Role::Asset)
            .collect();
        assert_eq!(midis.len(), 2, "the original and the fallback each got a MIDI sibling");
        assert_eq!(
            midis[0].content, midis[1].content,
            "same ABC → same MIDI bytes → same CAS hash (the fallback is the SAME phrase)"
        );
        assert_ne!(
            midis[0].id, midis[1].id,
            "but distinct BlockIds at distinct ticks — a fresh repeat the sink can fire"
        );
        let ticks: Vec<_> = midis.iter().map(|s| s.tick).collect();
        assert!(
            ticks.contains(&Some(Tick::new(10))) && ticks.contains(&Some(Tick::new(20))),
            "the fallback MIDI lands at the new tick (20), not just the original (10)"
        );
    }

    /// T10 (design §16) — derivation failure materializes NOTHING and bails. A
    /// committed `text/vnd.abc` cell carrying garbage bytes (a Fixed resolver that
    /// bypasses the validator) violates the first-commit invariant at the derive
    /// step: Err, zero blocks, high_water unchanged; a retry errs again.
    #[tokio::test]
    async fn derivation_failure_materializes_nothing_and_bails() {
        let (blocks, cas, ctx, _dir) = store_and_cas();

        // A Fixed resolver commits raw garbage UNDER the ABC mime, bypassing the
        // cas_commit validator — the only way to reach a committed-but-unparseable
        // ABC cell (which §5c calls corruption, not weather).
        let mut tl = Timeline::new(TickClock {
            ticks_per_sec: 1.0,
            safety_factor: 2.0,
            commit_margin: TickDelta::new(1),
        });
        tl.register_resolver(Box::new(Fixed {
            bytes: b"this is not abc {{{ ][".to_vec(),
            mime: super::ABC_MIME.to_string(),
        }));
        tl.schedule(Cell::deferred_on(
            Span::instant(Tick::new(10)),
            Recipe {
                resolver: ResolverId::new("fixed"),
                params: serde_json::Value::Null,
                query: ContextQuery::default(),
                fallback: Fallback::Skip,
            },
            kaijutsu_types::TrackId::solo(),
            kaijutsu_types::PrincipalId::beat(),
        ))
        .unwrap();
        tl.advance_to(Tick::new(10));
        let tl = Arc::new(Mutex::new(tl));

        let mut cursor = MaterializeCursor::default();
        let derivers = DeriverRegistry::production();
        let r1 = materialize_committed(&tl, &cas, &blocks, ctx, &mut cursor, &derivers);
        assert!(r1.is_err(), "unparseable committed ABC must bail at derive");
        assert_eq!(cursor.high_water, 0, "high_water unchanged — no half-cell");
        assert_eq!(
            blocks.block_snapshots(ctx).unwrap().len(),
            0,
            "zero blocks: a derive failure inserts nothing"
        );

        // Retry errs again (a loud head-of-line wedge, never a silent skip).
        let r2 = materialize_committed(&tl, &cas, &blocks, ctx, &mut cursor, &derivers);
        assert!(r2.is_err(), "the wedge persists across beats");
        assert_eq!(blocks.block_snapshots(ctx).unwrap().len(), 0);
    }

    /// T10 companion (design §16) — `materialize_bails_on_missing_bytes`. A committed
    /// cref whose bytes are in NEITHER the RAM-CAS nor the durable CAS → Err, never a
    /// dangling-hash block. Pins the fix for the pre-F2 silent crystallize-skip. Uses
    /// an UNREGISTERED mime so the bail must come from the bytes-missing branch, not
    /// the deriver (which would also fail on empty bytes).
    #[tokio::test]
    async fn materialize_bails_on_missing_bytes() {
        let (blocks, cas, ctx, _dir) = store_and_cas();

        // Commit a cell via Fixed (bytes land in the producer's RAM-CAS), then clone
        // the concrete cell into a SECOND timeline with an empty RAM-CAS and never
        // store it durably — bytes are absent from both, so materialize must BAIL.
        // The mime is unregistered so the bail can ONLY come from the missing-bytes
        // branch (no deriver to mask it).
        let mut producer = Timeline::new(TickClock {
            ticks_per_sec: 1.0,
            safety_factor: 2.0,
            commit_margin: TickDelta::new(1),
        });
        producer.register_resolver(Box::new(Fixed {
            bytes: b"some-automation-bytes".to_vec(),
            mime: "application/x-test-absent".to_string(),
        }));
        producer
            .schedule(Cell::deferred_on(
                Span::instant(Tick::new(10)),
                Recipe {
                    resolver: ResolverId::new("fixed"),
                    params: serde_json::Value::Null,
                    query: ContextQuery::default(),
                    fallback: Fallback::Skip,
                },
                kaijutsu_types::TrackId::solo(),
                kaijutsu_types::PrincipalId::beat(),
            ))
            .unwrap();
        producer.advance_to(Tick::new(10));
        let committed_cell = producer.committed()[0].clone();

        let mut empty = Timeline::new(TickClock {
            ticks_per_sec: 1.0,
            safety_factor: 2.0,
            commit_margin: TickDelta::new(1),
        });
        empty.push_committed_for_test(committed_cell);
        let tl = Arc::new(Mutex::new(empty));

        let mut cursor = MaterializeCursor::default();
        let derivers = DeriverRegistry::production();
        let r = materialize_committed(&tl, &cas, &blocks, ctx, &mut cursor, &derivers);
        assert!(r.is_err(), "missing bytes must bail loudly, never a dangling-hash block");
        assert_eq!(blocks.block_snapshots(ctx).unwrap().len(), 0, "no block inserted");
        assert_eq!(cursor.high_water, 0);
    }

    /// T11 (design §16) — an automation mime materializes WITHOUT a sibling. A
    /// `cas_commit` cell on an unregistered mime → exactly one block, Ok. (No deriver
    /// → no siblings: the §9 default. The same UseLastGood/materialize machinery, zero
    /// new code for the second consumer.)
    #[tokio::test]
    async fn automation_mime_materializes_without_sibling() {
        let (blocks, cas, ctx, _dir) = store_and_cas();

        use kaijutsu_cas::ContentStore;
        let knob = br#"{"knob":"cutoff","value":0.5}"#;
        let hash = cas.store(knob, "application/vnd.kaijutsu.knob+json").unwrap();

        let mut tl = Timeline::new(TickClock {
            ticks_per_sec: 1.0,
            safety_factor: 1.0,
            commit_margin: TickDelta::new(0),
        });
        super::register_resolvers(&mut tl, cas.clone());
        tl.schedule(Cell::deferred_on(
            Span::instant(Tick::new(10)),
            Recipe {
                resolver: ResolverId::new(super::CasCommitResolver::ID),
                params: serde_json::json!({
                    "hash": hash.as_str(),
                    "mime": "application/vnd.kaijutsu.knob+json",
                }),
                query: ContextQuery::default(),
                fallback: Fallback::Skip,
            },
            kaijutsu_types::TrackId::solo(),
            kaijutsu_types::PrincipalId::beat(),
        ))
        .unwrap();
        tl.advance_to(Tick::new(10));
        let tl = Arc::new(Mutex::new(tl));

        let mut cursor = MaterializeCursor::default();
        let derivers = DeriverRegistry::production();
        let inserted =
            materialize_committed(&tl, &cas, &blocks, ctx, &mut cursor, &derivers).unwrap();
        assert_eq!(inserted.len(), 1, "unregistered mime → exactly one block, no sibling");
        assert_eq!(blocks.block_snapshots(ctx).unwrap().len(), 1);
    }

    /// T12 (design §16) — hygiene pin: the committed log NEVER contains MIDI. A
    /// materialize cycle leaves the timeline's `committed()` holding only
    /// `text/vnd.abc` crefs — `audio/midi` exists ONLY as derived sibling blocks,
    /// never on the timeline (so the UseLastGood pool can't pick up a render).
    #[tokio::test]
    async fn committed_log_never_contains_midi() {
        let (blocks, cas, ctx, _dir) = store_and_cas();
        let tl = timeline_with_committed_abc(&cas, VAMP_ABC);
        let mut cursor = MaterializeCursor::default();
        let derivers = DeriverRegistry::production();
        materialize_committed(&tl, &cas, &blocks, ctx, &mut cursor, &derivers).unwrap();

        let g = tl.lock();
        for cell in g.committed() {
            if let kaijutsu_hyoushigi::Body::Concrete(c) = &cell.body {
                assert_ne!(
                    c.mime, "audio/midi",
                    "no MIDI cref may ever enter the committed log (pool purity)"
                );
                assert_eq!(c.mime, super::ABC_MIME, "committed bodies are notation only");
            }
        }
    }

    /// T13 (design §16) — `partial_insert_resumes_per_artifact`. A fault-injecting
    /// block store fails the sibling insert once: the first call Errs with the source
    /// inserted + artifacts_done == 1 + high_water unchanged; the second call inserts
    /// ONLY the sibling, then advances — no DuplicateBlock, no doubled source.
    #[tokio::test]
    async fn partial_insert_resumes_per_artifact() {
        let dir = tempfile::tempdir().unwrap();
        let cas = Arc::new(kaijutsu_cas::FileStore::at_path(dir.path()));

        let blocks: crate::block_store::SharedBlockStore =
            Arc::new(BlockStore::new(kaijutsu_types::PrincipalId::new()));
        let ctx = ContextId::new();
        blocks
            .create_document(ctx, DocumentKind::Conversation, None)
            .unwrap();

        let tl = timeline_with_committed_abc(&cas, VAMP_ABC);
        let mut cursor = MaterializeCursor::default();
        let derivers = DeriverRegistry::production();

        // Fault the SECOND insert (artifact list is [source, sibling]; the sibling is
        // insert #2). The source lands, the sibling faults → Err.
        blocks.arm_insert_fault(2);
        let r1 = materialize_committed(&tl, &cas, &blocks, ctx, &mut cursor, &derivers);
        assert!(r1.is_err(), "the injected sibling fault surfaces as Err");
        assert_eq!(cursor.artifacts_done, 1, "the source landed; resume at the sibling");
        assert_eq!(cursor.high_water, 0, "the cell has NOT fully crossed");
        assert_eq!(
            blocks.block_snapshots(ctx).unwrap().len(),
            1,
            "only the source is in the log"
        );
        let src_id = blocks.block_snapshots(ctx).unwrap()[0].id;

        // Second call: the fault is spent; only the sibling inserts (the source is NOT
        // re-inserted — its id is unchanged). No DuplicateBlock, no doubled source.
        let r2 = materialize_committed(&tl, &cas, &blocks, ctx, &mut cursor, &derivers);
        assert!(r2.is_ok(), "the resume succeeds once the fault clears");
        let snaps = blocks.block_snapshots(ctx).unwrap();
        assert_eq!(snaps.len(), 2, "source + sibling, no doubled source");
        assert_eq!(snaps[0].id, src_id, "the source kept its id across the resume");
        assert_eq!(cursor.high_water, 1, "the cell fully crossed");
        assert_eq!(cursor.artifacts_done, 0, "resume point reset after the group landed");
        let sib = &snaps[1];
        assert_eq!(sib.role, kaijutsu_types::Role::Asset);
        assert_eq!(sib.parent_id, Some(src_id), "sibling still parents the original source");
    }

    /// Arming a context gives it a timeline; an un-armed context has none.
    #[tokio::test]
    async fn arm_then_lookup_returns_some_unarmed_returns_none() {
        let kernel = Kernel::new_ephemeral("test").await;
        let armed = ContextId::new();
        let bare = ContextId::new();

        assert!(kernel.timeline(armed).is_none(), "not armed yet");
        kernel.arm_timeline(armed, TickClock::default(), kaijutsu_types::Tick::ZERO);
        assert!(kernel.timeline(armed).is_some(), "armed → has a timeline");
        assert!(kernel.timeline(bare).is_none(), "a different context stays bare");
    }

    /// Re-arming never clobbers the live timeline — the same handle comes back.
    #[tokio::test]
    async fn arm_is_idempotent() {
        let kernel = Kernel::new_ephemeral("test").await;
        let ctx = ContextId::new();
        let a = kernel.arm_timeline(ctx, TickClock::default(), kaijutsu_types::Tick::ZERO);
        let b = kernel.arm_timeline(ctx, TickClock::default(), kaijutsu_types::Tick::ZERO);
        assert!(std::sync::Arc::ptr_eq(&a, &b), "re-arm returns the same timeline");
    }

    /// T2 (design-chameleon-batch1-f2-notation §16) — the composer default speaks
    /// in phrases. `beats_per_phrase` is the kernel's only musical chunking unit
    /// above the beat (16 = a 4-bar phrase in 4/4); the OODA cadence stays
    /// beat-denominated at 128 (= the old 32*4, no change). Consumers go through
    /// `phrase_delta()`/`is_phrase_boundary()`, never the raw field.
    #[test]
    fn composer_default_speaks_phrases() {
        let p = super::BeatPolicy::composer_default();
        assert_eq!(p.beats_per_phrase, 16, "a 4-bar phrase in 4/4 is 16 beats");
        assert_eq!(p.ooda_every, 128, "OODA cadence stays beat-denominated (8 phrases = 128 beats)");
        assert_eq!(p.phrase_delta(), TickDelta::new(16), "one phrase of lead is 16 beat-ticks");
        assert!(p.is_phrase_boundary(16), "beat 16 is a phrase boundary");
        assert!(p.is_phrase_boundary(32), "beat 32 is a phrase boundary");
        assert!(!p.is_phrase_boundary(17), "beat 17 is mid-phrase, not a boundary");
    }

    /// Disarming drops the entry — the scheduler will skip a context with none.
    #[tokio::test]
    async fn disarm_removes_the_timeline() {
        let kernel = Kernel::new_ephemeral("test").await;
        let ctx = ContextId::new();
        kernel.arm_timeline(ctx, TickClock::default(), kaijutsu_types::Tick::ZERO);
        kernel.disarm_timeline(ctx);
        assert!(kernel.timeline(ctx).is_none(), "disarmed → no timeline");
    }
}
