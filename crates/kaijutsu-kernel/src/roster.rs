//! The live roster — kaijutsu's answer to "who is around right now,"
//! covering both agents and humans (design record, 2026-08-14, Amy + lead).
//! Her governing rule for the whole lane, verbatim: *"start with less, it's
//! easier to add more than take away."*
//!
//! ## Persist the record, re-derive the verdict
//!
//! The roster is a **materialized view** persisted in `kernel.db`
//! (`kernel_db.rs`, the `roster_entity`/`roster_presence`/`roster_activity`/
//! `roster_status` tables). The kernel reads *one* table at decision time and
//! never fans out to sources.
//!
//! The DB stores who was here, what they said, when, on what host. It does
//! **NOT** store liveness as a trusted fact. Liveness is a *computed column*,
//! entirely overwritten on every refresh ([`RosterStore::reconcile`]) — this
//! is what lets the view be persistent without lying after a restart, the
//! problem the older `midi_presence.rs` store sidestepped by being ephemeral
//! only (it had no way to re-derive). This module borrows its patterns
//! directly: an ordered read model, an `AtomicU64` generation stamp
//! (`Release` on bump / `Acquire` on read, copied from
//! `vfs::backends::share::ShareRegistry::index_generation`), and the same
//! "unknown, never absent" stance on a row nobody has reconfirmed.
//!
//! Three kinds of liveness knowledge — conflating them is the bug this
//! design exists to prevent:
//!
//! | kind | source in v1 | how liveness is known |
//! |---|---|---|
//! | [`LivenessKind::Bound`] | `PeerRegistry` (`peers.rs`) | connection open; death is *observed* on detach |
//! | [`LivenessKind::Recent`] | contexts — last block append | durable data already in the block log; a recency window |
//! | [`LivenessKind::Attested`] | **no producer in v1** — reserved | an external authoritative fact re-verified each refresh |
//!
//! There is deliberately **no TTL** anywhere in this module. A fourth kind
//! (`heard` — push a row, decay it on a timer) has no v1 producer: a client
//! posting status does so over its own bound connection, so it is `bound`.
//! `attested` has no producer because the Claude Code session source lives on
//! the un-merged branch `cc-peer-roster`, not `main` — the source
//! abstraction ([`PresenceSnapshotRow`] / [`RosterStore::reconcile`]) is
//! deliberately source-agnostic so that source drops in later without
//! reshaping anything here.
//!
//! ## Liveness vs availability
//!
//! - **liveness** ([`LivenessKind`], [`RosterRow::live`]) — kernel-derived:
//!   is this thing running/connected?
//! - **availability** ([`Availability`]) — client self-reported: is the
//!   human paying attention? A bound connection says the app is running; it
//!   says nothing about whether anyone is at the keyboard.
//!
//! Availability is **routing data, never authorization** — it may influence
//! *where* a prompt goes, never *who* may approve. Nothing in this module (or
//! anything that reads `roster_status`) may gate an approval on it.
//! `locked`/`screensaver` both map to [`Availability::Away`] *before* they
//! ever reach this module: the client reports the state, never the
//! mechanism, so no platform vocabulary reaches the kernel or the schema.
//!
//! ## Clock skew — multi-machine, one kernel
//!
//! Every churny row carries two timestamps: `observed_at` (the source's own
//! clock — provenance/display only) and `recorded_at` (this kernel's clock
//! at write time). **All recency/staleness evaluation uses `recorded_at`
//! only** — mirrors `docs/midi.md` "The one timebase" (never chase a
//! foreign clock; a sink's emission stamp is provenance, the kernel's own
//! stamp decides freshness).
//!
//! ## Joinability
//!
//! Roster rows join on the same kernel-stamped `principal_id`/`context_id`
//! the approval ledger writes — never on a source-asserted name. A
//! [`RosterEntity`] is only ever constructed from an identity the connection
//! layer already stamped (a `KjCaller`, a `PeerConfig::principal`), never
//! parsed from a client-supplied string. The schema backs the `context` half
//! of this with a trigger (`roster_entity_context_must_exist` in
//! `kernel_db.rs`); there is no `principals` table to check the `principal`
//! half against, so that half is Rust-API discipline only (documented, not
//! schema-enforced).
//!
//! ## History is otel, not a table
//!
//! Amy's ruling: *"current only, we have otel traces & logs for history."*
//! These tables hold only *now* — no history table, no append log, no
//! retention policy. Every presence transition (appeared, gone, status
//! changed, availability changed) is expected to emit an otel event via
//! `kaijutsu-telemetry` at the call site that observes the transition
//! (slice 2 — the source refresh loop — and slice 3 — the status write path
//! — are where those call sites live; this module supplies the storage the
//! event describes).

use std::sync::Arc;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};

use kaijutsu_types::{ContextId, PrincipalId};

use crate::kernel_db::{KernelDb, KernelDbResult};

// ============================================================================
// Types
// ============================================================================

/// The thing a roster row names. Principal for humans, context for agents —
/// never a third kind in v1 (design record).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RosterEntity {
    Principal(PrincipalId),
    Context(ContextId),
}

impl RosterEntity {
    pub fn kind_str(&self) -> &'static str {
        match self {
            RosterEntity::Principal(_) => "principal",
            RosterEntity::Context(_) => "context",
        }
    }

    pub fn id_bytes(&self) -> [u8; 16] {
        match self {
            RosterEntity::Principal(p) => *p.as_bytes(),
            RosterEntity::Context(c) => *c.as_bytes(),
        }
    }

    /// Reconstruct from a stored `(entity_kind, entity_id)` pair. `None` on
    /// an unrecognized kind or malformed id bytes — callers turn this into a
    /// loud typed error rather than silently dropping the row (no-silent-
    /// fallbacks).
    pub fn from_parts(kind: &str, id: &[u8]) -> Option<Self> {
        match kind {
            "principal" => PrincipalId::try_from_slice(id).map(RosterEntity::Principal),
            "context" => ContextId::try_from_slice(id).map(RosterEntity::Context),
            _ => None,
        }
    }
}

/// How liveness is known for one presence row. See the module doc's table.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LivenessKind {
    /// A connection is open (`PeerRegistry`); death is observed on detach.
    Bound,
    /// Durable block-log activity; a recency window over `recorded_at`.
    Recent,
    /// An external authoritative fact re-verified each refresh. Reserved —
    /// no v1 producer.
    Attested,
}

impl LivenessKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            LivenessKind::Bound => "bound",
            LivenessKind::Recent => "recent",
            LivenessKind::Attested => "attested",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "bound" => Some(LivenessKind::Bound),
            "recent" => Some(LivenessKind::Recent),
            "attested" => Some(LivenessKind::Attested),
            _ => None,
        }
    }
}

/// Self-reported attention state. Exactly four values — the client reports
/// the STATE, never the mechanism (design record: no `locked`/`screensaver`
/// ever reaches this type; a client maps those to [`Availability::Away`]
/// before it ever calls in). Routing data, never authorization.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Availability {
    Active,
    Idle,
    Away,
    Dnd,
}

impl Availability {
    pub fn as_str(&self) -> &'static str {
        match self {
            Availability::Active => "active",
            Availability::Idle => "idle",
            Availability::Away => "away",
            Availability::Dnd => "dnd",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "active" => Some(Availability::Active),
            "idle" => Some(Availability::Idle),
            "away" => Some(Availability::Away),
            "dnd" => Some(Availability::Dnd),
            _ => None,
        }
    }
}

/// One producer's claim about one live presence, as [`RosterStore::reconcile`]
/// wants it: the full picture a source currently stands behind. A row absent
/// from the next call's slice for the same `source` is pruned — same
/// "remove, don't downgrade to a lingering false" stance as
/// `midi_presence.rs::reap_connection`.
#[derive(Debug, Clone)]
pub struct PresenceSnapshotRow {
    /// The producer's own natural key for this presence (a peer registry
    /// key, a context id's hex form). Unique within `source`.
    pub source_local_id: String,
    pub entity: RosterEntity,
    /// Display label for the entity (a peer's nick, a context's label).
    /// `None` leaves a prior label untouched rather than blanking it.
    pub entity_label: Option<String>,
    pub host: Option<String>,
    pub pid: Option<u32>,
    pub proc_start: Option<i64>,
    /// The source's own clock for this observation — provenance/display
    /// only (module doc: clock skew).
    pub observed_at: Option<i64>,
    /// The liveness verdict the SOURCE computed for this row right now
    /// (e.g. `recent`'s caller applies its own recency window against
    /// `recorded_at` before calling in) — the store does not second-guess
    /// it, only stores it.
    pub live: bool,
}

/// One roster entry as read back — the join `RosterStore::snapshot` renders,
/// and what `kj roster list` / `/run/roster` (later slices) present from.
#[derive(Debug, Clone)]
pub struct RosterRow {
    pub entity: RosterEntity,
    pub label: Option<String>,
    pub source: String,
    pub source_local_id: String,
    pub liveness_kind: LivenessKind,
    pub host: Option<String>,
    pub pid: Option<u32>,
    pub proc_start: Option<i64>,
    pub first_seen_at: i64,
    pub live: bool,
    pub observed_at: Option<i64>,
    pub recorded_at: i64,
    pub status_text: Option<String>,
    pub availability: Option<Availability>,
    pub status_observed_at: Option<i64>,
    pub status_recorded_at: Option<i64>,
}

// ============================================================================
// RosterStore
// ============================================================================

/// The kernel-wide roster store: `kernel.db`-backed persistence plus the
/// in-memory generation/boot-freshness stamps a caching reader (the VFS view,
/// later slices) needs. One per kernel; held behind an `Arc` (mirrors
/// `MidiPresenceStore`'s placement, `midi_presence.rs`).
pub struct RosterStore {
    db: Arc<parking_lot::Mutex<KernelDb>>,
    /// Monotonic coherence stamp, bumped on every accepted mutation —
    /// `Release` on bump / `Acquire` on read, copied verbatim from
    /// `ShareRegistry::index_generation`.
    generation: AtomicU64,
    /// Unix-millis of the last completed refresh, or `0` = never refreshed
    /// since this process started. **Boot rule**: a reader must treat `0` as
    /// "the view is not yet trustworthy" — a row persisted before THIS
    /// process's first refresh must never render as current. Deliberately
    /// in-memory only: the question this answers ("has this process refreshed
    /// yet") is about the current process's own knowledge, not something a
    /// prior process could have answered for it.
    refreshed_at: AtomicI64,
}

impl RosterStore {
    pub fn new(db: Arc<parking_lot::Mutex<KernelDb>>) -> Self {
        Self { db, generation: AtomicU64::new(1), refreshed_at: AtomicI64::new(0) }
    }

    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::Acquire)
    }

    fn bump_generation(&self) {
        self.generation.fetch_add(1, Ordering::Release);
    }

    /// Unix-millis of the last completed refresh, or `None` if this process
    /// has never refreshed the view. See the boot rule on
    /// [`Self::refreshed_at`] (the field) — callers that serve a read surface
    /// (later slices) must check this before trusting [`Self::snapshot`].
    pub fn refreshed_at(&self) -> Option<i64> {
        match self.refreshed_at.load(Ordering::Acquire) {
            0 => None,
            ts => Some(ts),
        }
    }

    /// Record that a refresh pass completed at `ts` and bump the generation
    /// — a refresh is a change to the picture even when it confirms nothing
    /// moved, because it is what makes the view trustworthy at all (the boot
    /// rule). Sources (slice 2) call this after their reconciliation pass.
    pub fn mark_refreshed(&self, ts: i64) {
        self.refreshed_at.store(ts, Ordering::Release);
        self.bump_generation();
    }

    /// Reconcile one source's full current picture: upsert every row given,
    /// and prune every existing `roster_presence` row under `source` that
    /// this call did NOT reconfirm. This is the one storage primitive both a
    /// `bound` source (a snapshot of currently-attached peers — anything
    /// absent has truly disconnected) and a `recent` source (a snapshot of
    /// every non-archived context — nothing is pruned just for going idle,
    /// only for the entity itself being gone) share; the distinction between
    /// "existence is liveness" and "liveness is a window" lives entirely in
    /// what the caller chooses to include and what `live` it computes per
    /// row, never in this function.
    ///
    /// Returns the `source_local_id`s pruned, for the caller's otel emission
    /// (module doc: history is otel, not a table — this function never
    /// records the removal itself, only performs it).
    pub fn reconcile(
        &self,
        source: &str,
        liveness_kind: LivenessKind,
        rows: &[PresenceSnapshotRow],
        recorded_at: i64,
    ) -> KernelDbResult<Vec<String>> {
        let removed = self.db.lock().roster_reconcile_presence(source, liveness_kind, rows, recorded_at)?;
        self.bump_generation();
        Ok(removed)
    }

    /// Write (or overwrite) one entity's self-reported status. Identity is
    /// the caller's problem to stamp correctly (module doc: joinability) —
    /// this function trusts the `RosterEntity` it is given completely, so
    /// every call site MUST construct it from a connection-stamped identity,
    /// never from client-supplied text.
    pub fn write_status(
        &self,
        entity: RosterEntity,
        entity_label: Option<&str>,
        status_text: Option<&str>,
        availability: Availability,
        observed_at: Option<i64>,
        recorded_at: i64,
    ) -> KernelDbResult<()> {
        self.db.lock().roster_write_status(
            entity,
            entity_label,
            status_text,
            availability,
            observed_at,
            recorded_at,
        )?;
        self.bump_generation();
        Ok(())
    }

    /// The full current view, ordered by `(source, source_local_id)` for
    /// stable iteration. Reads exactly the four roster tables — no fan-out
    /// to `PeerRegistry` or anywhere else (the whole point of a materialized
    /// view). Does NOT itself enforce the boot rule — callers building a
    /// read surface (later slices) check [`Self::refreshed_at`] first.
    pub fn snapshot(&self) -> KernelDbResult<Vec<RosterRow>> {
        self.db.lock().roster_snapshot()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kernel_db::{ContextRow, KernelDb, KernelDbError};
    use kaijutsu_types::{ConsentMode, ContextState};

    fn db_with_context() -> (Arc<parking_lot::Mutex<KernelDb>>, ContextId) {
        let db = KernelDb::in_memory().expect("in-memory KernelDb");
        let ws = db.get_or_create_default_workspace(PrincipalId::system()).unwrap();
        let ctx_id = ContextId::new();
        let row = ContextRow {
            context_id: ctx_id,
            label: Some("test-ctx".to_string()),
            provider: None,
            model: None,
            system_prompt: None,
            consent_mode: ConsentMode::default(),
            context_state: ContextState::Live,
            context_type: "default".to_string(),
            created_at: kaijutsu_types::now_millis() as i64,
            created_by: PrincipalId::system(),
            forked_from: None,
            fork_kind: None,
            archived_at: None,
            concluded_at: None,
            workspace_id: Some(ws),
            preset_id: None,
            last_activity_at: None,
            promoted_at: None,
            demoted_at: None,
            paused_at: None,
            cast_id: None,
            origin_host: None,
        };
        db.insert_context_with_document(&row, ws).expect("create context");
        (Arc::new(parking_lot::Mutex::new(db)), ctx_id)
    }

    fn bound_row(local_id: &str, entity: RosterEntity, recorded_at: i64) -> PresenceSnapshotRow {
        PresenceSnapshotRow {
            source_local_id: local_id.to_string(),
            entity,
            entity_label: Some("moltar-app".to_string()),
            host: Some("moltar".to_string()),
            pid: None,
            proc_start: None,
            observed_at: Some(recorded_at),
            live: true,
        }
    }

    #[test]
    fn a_reconciled_row_reads_back_live() {
        let (db, ctx) = db_with_context();
        let store = RosterStore::new(db);
        let entity = RosterEntity::Context(ctx);
        store
            .reconcile("peer_registry", LivenessKind::Bound, &[bound_row("win-a", entity, 100)], 100)
            .unwrap();

        let snap = store.snapshot().unwrap();
        assert_eq!(snap.len(), 1);
        assert_eq!(snap[0].entity, entity);
        assert!(snap[0].live);
        assert_eq!(snap[0].liveness_kind, LivenessKind::Bound);
        assert_eq!(snap[0].host.as_deref(), Some("moltar"));
    }

    /// The single most important test in the lane: a `bound` row persisted
    /// by a prior process must NOT come back live when the next process's
    /// refresh sees an empty source snapshot (the PeerRegistry a fresh
    /// kernel starts with). This is the restart-shaped case the whole design
    /// turns on.
    #[test]
    fn a_restart_shaped_refresh_from_empty_sources_drops_stale_bound_rows() {
        let (db, ctx) = db_with_context();
        let store = RosterStore::new(db.clone());
        let entity = RosterEntity::Context(ctx);

        // "Before restart": a peer was attached and reconciled in.
        store
            .reconcile("peer_registry", LivenessKind::Bound, &[bound_row("win-a", entity, 100)], 100)
            .unwrap();
        assert_eq!(store.snapshot().unwrap().len(), 1, "sanity: the row landed");

        // "Restart": a fresh RosterStore over the SAME kernel.db (the
        // persisted rows survive; a real process restart would also mint a
        // fresh, empty PeerRegistry — modeled here directly as an empty
        // snapshot, which is exactly what the fresh registry will report).
        let store_after_restart = RosterStore::new(db);
        assert!(
            store_after_restart.refreshed_at().is_none(),
            "a freshly constructed store must not claim to be refreshed yet"
        );

        // The boot refresh runs against the real (post-restart) source
        // picture: no peers attached yet.
        let removed = store_after_restart
            .reconcile("peer_registry", LivenessKind::Bound, &[], 200)
            .unwrap();
        assert_eq!(removed, vec!["win-a".to_string()]);

        let snap = store_after_restart.snapshot().unwrap();
        assert!(snap.is_empty(), "a stale bound row must not survive a refresh from an empty source");
    }

    /// `recent` presence must NOT be pruned merely for falling stale — only
    /// a `bound` source's absence means "truly disconnected". A `recent`
    /// row's continued presence (with `live=false`) is itself the honest
    /// "last seen" answer.
    #[test]
    fn a_recent_row_survives_going_non_live_across_a_refresh() {
        let (db, ctx) = db_with_context();
        let store = RosterStore::new(db);
        let entity = RosterEntity::Context(ctx);
        let mut row = bound_row("ctx-activity", entity, 100);
        row.live = true;
        store.reconcile("context_activity", LivenessKind::Recent, &[row.clone()], 100).unwrap();

        // Next refresh: same entity still included (contexts aren't pruned
        // just for going idle), but now outside the caller's recency window.
        let mut stale = row;
        stale.live = false;
        let removed = store
            .reconcile("context_activity", LivenessKind::Recent, &[stale], 5_000)
            .unwrap();
        assert!(removed.is_empty(), "an idle-but-still-existing context must not be pruned");

        let snap = store.snapshot().unwrap();
        assert_eq!(snap.len(), 1);
        assert!(!snap[0].live, "liveness must reflect the latest refresh, not the first one");
    }

    /// `recorded_at` — never a foreign clock — governs what the view shows
    /// as current. A wildly skewed `observed_at` (far future or far past)
    /// must not perturb the stored `recorded_at`/ordering.
    #[test]
    fn observed_at_skew_never_leaks_into_recorded_at() {
        let (db, ctx) = db_with_context();
        let store = RosterStore::new(db);
        let entity = RosterEntity::Context(ctx);
        let mut row = bound_row("win-a", entity, 100);
        // The reporting source's clock is absurdly far in the future.
        row.observed_at = Some(9_999_999_999_999);
        store.reconcile("peer_registry", LivenessKind::Bound, &[row], 500).unwrap();

        let snap = store.snapshot().unwrap();
        assert_eq!(snap[0].recorded_at, 500, "recorded_at must be the kernel's own clock");
        assert_eq!(snap[0].observed_at, Some(9_999_999_999_999), "observed_at is carried, never trusted for ordering");
    }

    #[test]
    fn generation_bumps_on_reconcile_and_on_status_write() {
        let (db, ctx) = db_with_context();
        let store = RosterStore::new(db);
        let entity = RosterEntity::Context(ctx);
        let g0 = store.generation();

        store.reconcile("peer_registry", LivenessKind::Bound, &[bound_row("win-a", entity, 1)], 1).unwrap();
        let g1 = store.generation();
        assert!(g1 > g0, "reconcile must bump the generation");

        store
            .write_status(entity, None, Some("heads down"), Availability::Dnd, None, 2)
            .unwrap();
        let g2 = store.generation();
        assert!(g2 > g1, "a status write must bump the generation");
    }

    #[test]
    fn mark_refreshed_records_the_boot_stamp_and_bumps_generation() {
        let (db, _ctx) = db_with_context();
        let store = RosterStore::new(db);
        assert!(store.refreshed_at().is_none());
        let g0 = store.generation();
        store.mark_refreshed(1_000);
        assert_eq!(store.refreshed_at(), Some(1_000));
        assert!(store.generation() > g0);
    }

    /// Availability round-trips all four states through the Rust API and
    /// rejects anything else at parse time (the schema's own CHECK is
    /// exercised separately in `kernel_db.rs`'s tests).
    #[test]
    fn availability_round_trips_all_four_states() {
        for a in [Availability::Active, Availability::Idle, Availability::Away, Availability::Dnd] {
            assert_eq!(Availability::parse(a.as_str()), Some(a));
        }
        assert_eq!(Availability::parse("sideways"), None);
        assert_eq!(Availability::parse("locked"), None, "platform vocabulary must never parse");
    }

    #[test]
    fn liveness_kind_round_trips_and_rejects_unknown() {
        for k in [LivenessKind::Bound, LivenessKind::Recent, LivenessKind::Attested] {
            assert_eq!(LivenessKind::parse(k.as_str()), Some(k));
        }
        assert_eq!(LivenessKind::parse("heard"), None, "the reserved fourth kind has no v1 producer and must not parse");
    }

    #[test]
    fn status_write_reads_back_and_updates_in_place() {
        let (db, ctx) = db_with_context();
        let store = RosterStore::new(db);
        let entity = RosterEntity::Context(ctx);
        store
            .write_status(entity, Some("agent"), Some("building slice 3"), Availability::Active, Some(10), 10)
            .unwrap();
        store
            .reconcile("peer_registry", LivenessKind::Bound, &[bound_row("win-a", entity, 10)], 10)
            .unwrap();

        let snap = store.snapshot().unwrap();
        assert_eq!(snap[0].status_text.as_deref(), Some("building slice 3"));
        assert_eq!(snap[0].availability, Some(Availability::Active));

        store
            .write_status(entity, None, Some("done"), Availability::Idle, Some(20), 20)
            .unwrap();
        let snap = store.snapshot().unwrap();
        assert_eq!(snap[0].status_text.as_deref(), Some("done"), "status is one row per entity, updated in place");
        assert_eq!(snap[0].availability, Some(Availability::Idle));
    }

    #[test]
    fn a_source_asserted_context_that_the_kernel_never_minted_is_refused() {
        let (db, _ctx) = db_with_context();
        let store = RosterStore::new(db);
        let phantom = RosterEntity::Context(ContextId::new());
        let err = store
            .reconcile("peer_registry", LivenessKind::Bound, &[bound_row("win-a", phantom, 1)], 1)
            .unwrap_err();
        assert!(
            matches!(err, KernelDbError::Db(_)),
            "expected the schema trigger to fire as a raw SQLite error, got {err:?}"
        );
    }
}
