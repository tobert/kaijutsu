//! Roster sources (slice 2): turning live kernel state into
//! [`PresenceSnapshotRow`]s for [`RosterStore::reconcile`], plus the
//! scheduled-periodic refresh loop that drives them.
//!
//! Two producers in v1, matching `roster.rs`'s design table exactly:
//!
//! - [`bound_snapshot`] — every currently-attached [`PeerInfo`]
//!   (`peers.rs`), each mapped to a [`RosterEntity::Principal`]. Existence in
//!   this snapshot IS liveness for `bound`: `RosterStore::reconcile` prunes
//!   any presence row this pass didn't reconfirm, which is exactly what a
//!   detached peer wants (module doc on `roster.rs`: "remove, don't downgrade
//!   to a lingering false").
//! - [`recent_snapshot`] — every non-archived context, mapped to a
//!   [`RosterEntity::Context`]. NOTHING is pruned here for merely going
//!   idle — a context stays in the snapshot as long as it exists at all;
//!   only its `live` flag (a window over `recorded_at`) changes.
//!
//! [`refresh_once`] runs both passes, diffs each source's before/after
//! `source_local_id` set to know "appeared" from "already known" (`reconcile`
//! itself only reports what it pruned), and emits one otel transition per
//! appearance/departure (`kaijutsu_telemetry::record_roster_transition` —
//! module doc on `roster.rs`: history is otel, not a table). It then calls
//! [`RosterStore::mark_refreshed`], which is what satisfies the **boot rule**:
//! a fresh kernel's `RosterStore::refreshed_at()` reads `None` until the
//! first `refresh_once` completes, so a read surface built on slice 4 must
//! check that before trusting `snapshot()`.
//!
//! [`spawn_periodic_refresh`] is the scheduled-periodic (never chase-the-
//! clock, `docs/midi.md` "The one timebase") wrapper around `refresh_once`,
//! at a fixed ~10s cadence with `MissedTickBehavior::Delay` (the same pattern
//! `kaijutsu-server`'s other interval loops use, e.g. `rpc.rs`'s VFS activity
//! subscription). **Not wired into `kaijutsu-server`'s boot sequence by this
//! slice** — this crate cannot start/verify a live kernel (offline
//! build-and-test constraint), so the production `tokio::spawn` call at
//! server startup is left as the next integration step rather than added
//! unverified. Everything up to that one call site is complete and tested
//! here directly against `refresh_once`.
//!
//! Push-based refresh (design record: "push where events already exist:
//! peer attach/detach, status post") is only partly built: a status post is
//! already push-only by construction (`RosterStore::write_status` writes
//! immediately, slice 3). An immediate push on peer attach/detach — rather
//! than picking it up on the next ≤10s pull tick — would need a call from
//! `kaijutsu-server`'s RPC attach/detach handlers into `refresh_once` (or a
//! narrower single-peer reconcile); not built here for the same
//! offline-verification reason as the spawn wiring. The ≤10s pull alone is
//! still correct (`bound` presence is never wrong for longer than one
//! interval), just not the lowest-latency version the design allows for.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use kaijutsu_types::now_millis;
use tokio_util::sync::CancellationToken;

use crate::kernel::Kernel;
use crate::kernel_db::{KernelDb, KernelDbResult};
use crate::peers::{PeerInfo, peer_key};
use crate::roster::{LivenessKind, PresenceSnapshotRow, RosterEntity, RosterStore};

/// `roster_presence.source` for the `PeerRegistry`-backed `bound` kind.
pub const SOURCE_BOUND: &str = "peer_registry";
/// `roster_presence.source` for the context-activity-backed `recent` kind.
pub const SOURCE_RECENT: &str = "context_activity";

/// The default scheduled-periodic refresh cadence (design record: "~10s").
pub const DEFAULT_REFRESH_INTERVAL: Duration = Duration::from_secs(10);

/// How long a context's `recorded_at`-relative last-activity gap may be
/// before the `recent` source stops calling it live. A v1 starting guess
/// ("start with less") — not yet tuned against real usage; a config knob is
/// the natural follow-up if it needs adjusting.
pub const RECENT_LIVE_WINDOW_MS: i64 = 15 * 60 * 1000;

/// Build the `bound` source's current snapshot from a live peer listing.
///
/// Peers with no stamped principal are skipped outright: a roster entry
/// needs an identity to join on (`roster.rs` module doc, "Joinability"), and
/// `PeerConfig::principal` is only ever `None` for a caller that hasn't
/// authenticated onto a principal yet. `host` is left `None` — `PeerInfo`
/// carries no host field today (nullable column; a future peer-reported host
/// can populate it without a schema change).
pub fn bound_snapshot(peers: &[PeerInfo], recorded_at: i64) -> Vec<PresenceSnapshotRow> {
    let _ = recorded_at; // observed_at below is the peer's own attach stamp, not this call's.
    peers
        .iter()
        .filter_map(|p| {
            let principal = p.principal?;
            Some(PresenceSnapshotRow {
                source_local_id: peer_key(&p.nick, &p.instance),
                entity: RosterEntity::Principal(principal),
                entity_label: Some(p.nick.clone()),
                host: None,
                pid: None,
                proc_start: None,
                observed_at: Some(p.attached_at as i64),
                // Existence in this snapshot IS liveness for `bound` — see
                // module doc.
                live: true,
            })
        })
        .collect()
}

/// Build the `recent` source's current snapshot from every non-archived
/// context. Unlike `bound`, nothing here is pruned for merely going idle —
/// see module doc.
pub fn recent_snapshot(
    db: &KernelDb,
    recorded_at: i64,
) -> KernelDbResult<Vec<PresenceSnapshotRow>> {
    let contexts = db.list_active_contexts()?;
    Ok(contexts
        .into_iter()
        .map(|ctx| {
            // Same floor `list_active_contexts`' own ORDER BY uses: a context
            // with no block activity yet is still "recent" as of its own
            // creation, not eternally unknown.
            let last_activity = ctx.last_activity_at.unwrap_or(ctx.created_at);
            PresenceSnapshotRow {
                source_local_id: ctx.context_id.to_hex(),
                entity: RosterEntity::Context(ctx.context_id),
                entity_label: ctx.label,
                host: ctx.origin_host,
                pid: None,
                proc_start: None,
                observed_at: Some(last_activity),
                live: recorded_at.saturating_sub(last_activity) <= RECENT_LIVE_WINDOW_MS,
            }
        })
        .collect())
}

/// Reconcile one source's snapshot into the store, diffing the before/after
/// `source_local_id` set so "appeared" and "gone" each get exactly one otel
/// transition (module doc: history is otel, not a table).
fn apply_source(
    db: &Arc<parking_lot::Mutex<KernelDb>>,
    roster: &RosterStore,
    source: &str,
    kind: LivenessKind,
    rows: &[PresenceSnapshotRow],
    recorded_at: i64,
) -> KernelDbResult<()> {
    let before: HashSet<String> = db.lock().roster_presence_source_local_ids(source)?;
    let removed = roster.reconcile(source, kind, rows, recorded_at)?;
    for _ in &removed {
        kaijutsu_telemetry::record_roster_transition("gone", kind.as_str());
    }
    for row in rows {
        if !before.contains(&row.source_local_id) {
            kaijutsu_telemetry::record_roster_transition("appeared", kind.as_str());
        }
    }
    Ok(())
}

/// Run one full refresh pass: both sources, then
/// [`RosterStore::mark_refreshed`]. This is what a boot sequence must call
/// once before serving any roster read (the boot rule), and what
/// [`spawn_periodic_refresh`] calls on every tick thereafter.
pub fn refresh_once(
    db: &Arc<parking_lot::Mutex<KernelDb>>,
    roster: &RosterStore,
    peers: &[PeerInfo],
) -> KernelDbResult<()> {
    let recorded_at = now_millis() as i64;

    let bound_rows = bound_snapshot(peers, recorded_at);
    apply_source(db, roster, SOURCE_BOUND, LivenessKind::Bound, &bound_rows, recorded_at)?;

    let recent_rows = recent_snapshot(&db.lock(), recorded_at)?;
    apply_source(db, roster, SOURCE_RECENT, LivenessKind::Recent, &recent_rows, recorded_at)?;

    roster.mark_refreshed(recorded_at);
    Ok(())
}

/// Spawn the scheduled-periodic refresh loop. Cancels cleanly via `cancel`
/// (mirrors `kaijutsu-server`'s connection-scoped interval loops, e.g.
/// `rpc.rs`'s VFS activity subscription) — a kernel-wide task, not
/// connection-scoped, so its lifetime is the caller's (typically the server
/// process) to own. **Not called from anywhere in production yet** — see
/// module doc.
pub fn spawn_periodic_refresh(
    kernel: Arc<Kernel>,
    db: Arc<parking_lot::Mutex<KernelDb>>,
    roster: Arc<RosterStore>,
    interval: Duration,
    cancel: CancellationToken,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                _ = cancel.cancelled() => {
                    tracing::debug!("roster refresh loop cancelled");
                    break;
                }
                _ = ticker.tick() => {
                    let peers = kernel.list_peers().await;
                    if let Err(e) = refresh_once(&db, &roster, &peers) {
                        tracing::warn!("roster refresh failed: {e}");
                    }
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kernel_db::{ContextRow, KernelDb};
    use crate::peers::PeerConfig;
    use kaijutsu_types::{ConsentMode, ContextId, ContextState, PrincipalId};

    fn peer(nick: &str, instance: &str, principal: Option<PrincipalId>) -> PeerInfo {
        PeerInfo::from_config(PeerConfig {
            nick: nick.to_string(),
            instance: instance.to_string(),
            principal,
        })
    }

    fn db_with_context(label: &str) -> (Arc<parking_lot::Mutex<KernelDb>>, ContextId) {
        let db = KernelDb::in_memory().expect("in-memory KernelDb");
        let ws = db.get_or_create_default_workspace(PrincipalId::system()).unwrap();
        let ctx_id = ContextId::new();
        let row = ContextRow {
            context_id: ctx_id,
            label: Some(label.to_string()),
            provider: None,
            model: None,
            system_prompt: None,
            consent_mode: ConsentMode::default(),
            context_state: ContextState::Live,
            context_type: "default".to_string(),
            created_at: now_millis() as i64,
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

    #[test]
    fn bound_snapshot_skips_peers_with_no_principal() {
        let p1 = peer("app", "win-a", Some(PrincipalId::new()));
        let p2 = peer("mcp", "sess-b", None);
        let rows = bound_snapshot(&[p1, p2], 100);
        assert_eq!(rows.len(), 1, "the principal-less peer must not become a phantom roster entry");
        assert_eq!(rows[0].source_local_id, "win-a");
        assert!(rows[0].live);
    }

    #[test]
    fn recent_snapshot_marks_a_fresh_context_live_and_an_old_one_not() {
        let (db, ctx) = db_with_context("agent-a");
        let recorded_at = now_millis() as i64;
        let rows = recent_snapshot(&db.lock(), recorded_at).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].entity, RosterEntity::Context(ctx));
        assert!(rows[0].live, "a just-created context is within the recency window");

        let far_future = recorded_at + RECENT_LIVE_WINDOW_MS + 1;
        let rows = recent_snapshot(&db.lock(), far_future).unwrap();
        assert!(!rows[0].live, "outside the window, the row survives but is not live");
    }

    /// `refresh_once` end to end: a bound peer and a context both land, the
    /// boot rule's `refreshed_at` stamp gets set, and the store's snapshot
    /// reflects both sources in one read.
    #[test]
    fn refresh_once_reconciles_both_sources_and_marks_refreshed() {
        let (db, ctx) = db_with_context("agent-a");
        let roster = RosterStore::new(db.clone());
        assert!(roster.refreshed_at().is_none());

        let principal = PrincipalId::new();
        let peers = vec![peer("app", "win-a", Some(principal))];
        refresh_once(&db, &roster, &peers).unwrap();

        assert!(roster.refreshed_at().is_some(), "boot rule: refresh_once must stamp refreshed_at");
        let snap = roster.snapshot().unwrap();
        assert_eq!(snap.len(), 2, "one bound peer + one recent context");
        assert!(snap.iter().any(|r| r.entity == RosterEntity::Principal(principal) && r.live));
        assert!(snap.iter().any(|r| r.entity == RosterEntity::Context(ctx) && r.live));
    }

    /// The restart-shaped guarantee, exercised through the real source
    /// function this time (not a hand-built snapshot row): a peer reconciled
    /// in by one `refresh_once` must not survive a second `refresh_once`
    /// called with an empty peer list (what a freshly restarted kernel's
    /// `PeerRegistry` actually reports).
    #[test]
    fn a_second_refresh_with_no_peers_drops_the_stale_bound_peer() {
        let (db, _ctx) = db_with_context("agent-a");
        let roster = RosterStore::new(db.clone());
        let principal = PrincipalId::new();
        refresh_once(&db, &roster, &[peer("app", "win-a", Some(principal))]).unwrap();
        assert!(roster.snapshot().unwrap().iter().any(|r| r.entity == RosterEntity::Principal(principal)));

        refresh_once(&db, &roster, &[]).unwrap();
        assert!(
            !roster.snapshot().unwrap().iter().any(|r| r.entity == RosterEntity::Principal(principal)),
            "a bound peer absent from the refresh's own peer list must not linger"
        );
    }
}
