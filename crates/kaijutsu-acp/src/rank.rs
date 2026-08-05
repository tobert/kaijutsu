//! The session picker is **the rank**.
//!
//! ACP's `session/list` is a session picker; kaijutsu already has one — the
//! time well's rings. Ring 0 ("active") is hand-curated, append-ordered by
//! `promoted_at`, and kernel-capped at ten seats; ring 1 ("recent") fills
//! automatically by last activity. Both stamps ride the existing wire
//! (`ContextHandleInfo.promotedAt` / `.demotedAt`, decoded into
//! `ContextInfo`), so no schema change was needed to serve them here.
//!
//! Seat order is computed by `kaijutsu_viz::layout::assign_ring_seats` — the
//! *same* pure function the desktop app's time well uses. That is the point:
//! a phone lists the identical ten seats in the identical order, so `ctrl-a 2`
//! at the desk and seat 2 on the phone are the same conversation.
//!
//! Ring 1 is appended after ring 0 rather than dropped. Ring 0 is empty on a
//! fresh kernel, and a session picker that shows nothing is useless; appending
//! keeps ring 0's seats stable at the front (where the muscle memory lives)
//! while still surfacing what you were just working on.

use std::path::{Path, PathBuf};

use agent_client_protocol::schema::v1::{SessionId, SessionInfo};
use kaijutsu_client::ContextInfo;
use kaijutsu_crdt::ContextId;
use kaijutsu_viz::layout::{Band, ContextLifecycle, assign_ring_seats};

/// The ACP session id for a context: its hex form. Round-trips through
/// [`context_id_of`], so `session/load` can name any context the list showed.
pub fn session_id_of(id: ContextId) -> SessionId {
    SessionId::new(id.to_hex())
}

/// Parse an ACP session id back into a context id.
pub fn context_id_of(session: &SessionId) -> Option<ContextId> {
    ContextId::parse(session.0.as_ref()).ok()
}

/// Ring 0 in seat order, then ring 1 in recency order.
///
/// Archived contexts are filtered before seating (the app does the same) —
/// they are not sessions anyone wants offered.
pub fn ranked_context_ids(contexts: &[ContextInfo]) -> Vec<ContextId> {
    let live: Vec<&ContextInfo> = contexts.iter().filter(|c| !c.archived).collect();
    let lifecycles: Vec<ContextLifecycle<ContextId>> = live
        .iter()
        .map(|c| ContextLifecycle {
            id: c.id,
            created_at: c.created_at as i64,
            concluded_at: c.concluded_at.map(|t| t as i64),
            // A never-touched context coalesces to its creation time, matching
            // the app's `effective_activity`.
            last_activity_at: c.last_activity_at.unwrap_or(c.created_at) as i64,
            promoted_at: c.promoted_at.map(|t| t as i64),
            demoted_at: c.demoted_at.map(|t| t as i64),
        })
        .collect();

    let placement = assign_ring_seats(&lifecycles);
    let mut out = placement.rings[Band::Active.index()].clone();
    out.extend(placement.rings[Band::Recent.index()].iter().copied());
    out
}

/// The ACP session list: the rank, rendered as `SessionInfo`.
///
/// `cwd` is the bridge's working directory — ACP requires one per session and
/// a kj context does not carry one, so every listed session reports the
/// bridge's. Noted in docs/acp.md as a known impedance mismatch.
pub fn ranked_sessions(contexts: &[ContextInfo], cwd: &Path) -> Vec<SessionInfo> {
    let by_id: std::collections::HashMap<ContextId, &ContextInfo> =
        contexts.iter().map(|c| (c.id, c)).collect();
    ranked_context_ids(contexts)
        .into_iter()
        .filter_map(|id| by_id.get(&id).copied())
        .map(|c| session_info(c, cwd))
        .collect()
}

fn session_info(c: &ContextInfo, cwd: &Path) -> SessionInfo {
    let title = if c.label.is_empty() {
        c.id.short()
    } else {
        c.label.clone()
    };
    let mut info = SessionInfo::new(session_id_of(c.id), PathBuf::from(cwd)).title(title);
    if let Some(ts) = c.last_activity_at.or(Some(c.created_at)) {
        if let Some(stamp) = rfc3339_millis(ts) {
            info = info.updated_at(stamp);
        }
    }
    info
}

/// Unix millis → RFC3339, which is what ACP's `updatedAt` wants.
fn rfc3339_millis(millis: u64) -> Option<String> {
    let secs = (millis / 1000) as i64;
    let nanos = ((millis % 1000) * 1_000_000) as u32;
    chrono::DateTime::from_timestamp(secs, nanos).map(|dt| dt.to_rfc3339())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx(label: &str) -> ContextInfo {
        ContextInfo {
            id: ContextId::new(),
            label: label.to_string(),
            forked_from: None,
            provider: String::new(),
            model: String::new(),
            created_at: 1_000,
            trace_id: [0u8; 16],
            fork_kind: None,
            context_type: "coder".to_string(),
            archived: false,
            concluded_at: None,
            keywords: Vec::new(),
            top_block_preview: None,
            live_status: kaijutsu_types::Status::Pending,
            last_activity_at: None,
            track_id: None,
            promoted_at: None,
            demoted_at: None,
            paused_at: None,
            context_window: None,
            context_used_tokens: None,
            context_used_pct: None,
            background_running_count: 0,
            background_oldest_running_started_at: None,
            background_last_finished_at: None,
            background_last_finished_status: None,
            background_last_exit_code: None,
            cast_label: None,
        }
    }

    #[test]
    fn session_ids_round_trip_through_context_ids() {
        let id = ContextId::new();
        assert_eq!(context_id_of(&session_id_of(id)), Some(id));
    }

    #[test]
    fn a_garbage_session_id_does_not_resolve() {
        assert!(context_id_of(&SessionId::new("not-a-uuid")).is_none());
    }

    #[test]
    fn ring0_comes_first_in_promote_order() {
        let mut a = ctx("second-promoted");
        a.promoted_at = Some(2_000);
        let mut b = ctx("first-promoted");
        b.promoted_at = Some(1_000);
        let c = ctx("never-promoted");

        let ranked = ranked_context_ids(&[a.clone(), b.clone(), c.clone()]);
        assert_eq!(ranked[0], b.id, "earlier promote takes the earlier seat");
        assert_eq!(ranked[1], a.id);
        assert_eq!(ranked[2], c.id, "the auto pool follows ring 0");
    }

    #[test]
    fn ring1_falls_back_to_recency() {
        let mut older = ctx("older");
        older.last_activity_at = Some(5_000);
        let mut newer = ctx("newer");
        newer.last_activity_at = Some(9_000);
        let ranked = ranked_context_ids(&[older.clone(), newer.clone()]);
        assert_eq!(ranked, vec![newer.id, older.id]);
    }

    #[test]
    fn demoted_contexts_leave_the_picker() {
        let mut gone = ctx("demoted");
        gone.demoted_at = Some(7_000);
        let kept = ctx("kept");
        let ranked = ranked_context_ids(&[gone.clone(), kept.clone()]);
        assert_eq!(ranked, vec![kept.id]);
    }

    #[test]
    fn archived_contexts_are_not_offered_as_sessions() {
        let mut arch = ctx("archived");
        arch.archived = true;
        arch.promoted_at = Some(1); // even a promoted one
        let live = ctx("live");
        let ranked = ranked_context_ids(&[arch, live.clone()]);
        assert_eq!(ranked, vec![live.id]);
    }

    #[test]
    fn concluded_contexts_drop_past_the_horizon() {
        let mut done = ctx("concluded");
        done.concluded_at = Some(8_000);
        let open = ctx("open");
        let ranked = ranked_context_ids(&[done, open.clone()]);
        assert_eq!(ranked, vec![open.id]);
    }

    #[test]
    fn session_info_carries_the_label_as_title() {
        let mut c = ctx("household-agent");
        c.last_activity_at = Some(1_700_000_000_000);
        let infos = ranked_sessions(&[c.clone()], Path::new("/tmp/work"));
        assert_eq!(infos.len(), 1);
        assert_eq!(infos[0].session_id, session_id_of(c.id));
        assert_eq!(infos[0].title.as_deref(), Some("household-agent"));
        assert_eq!(infos[0].cwd, PathBuf::from("/tmp/work"));
        assert!(infos[0].updated_at.is_some());
    }

    #[test]
    fn an_unlabelled_context_falls_back_to_its_short_id() {
        let c = ctx("");
        let infos = ranked_sessions(&[c.clone()], Path::new("/tmp"));
        assert_eq!(infos[0].title.as_deref(), Some(c.id.short().as_str()));
    }

    #[test]
    fn timestamps_render_as_rfc3339() {
        let s = rfc3339_millis(1_700_000_000_000).expect("in range");
        assert!(s.starts_with("2023-11-14T"), "unexpected stamp: {s}");
    }
}
