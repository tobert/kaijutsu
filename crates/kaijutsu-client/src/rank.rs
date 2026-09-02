//! The rank: every client's session/window list, computed once.
//!
//! Ring 0 ("active") is hand-curated, append-ordered by `promoted_at`, and
//! kernel-capped at ten seats; ring 1 ("recent") fills automatically by last
//! activity. Both stamps ride the existing wire (`ContextHandleInfo.promotedAt`
//! / `.demotedAt`, decoded into [`ContextInfo`]), so no schema change is
//! needed to serve them here.
//!
//! Seat order is computed by [`assign_ring_seats`] — the *same* pure function
//! the desktop app's time well uses. That is the point: every client lists
//! the identical ten seats in the identical order, so `ctrl-a 2` at the desk,
//! seat 2 in an ACP session picker, and seat 2 in the terminal client all
//! name the same conversation.
//!
//! Ring 1 is appended after ring 0 rather than dropped. Ring 0 is empty on a
//! fresh kernel, and a rank that shows nothing is useless; appending keeps
//! ring 0's seats stable at the front (where the muscle memory lives) while
//! still surfacing what you were just working on.

use kaijutsu_types::ContextId;
use kaijutsu_viz::layout::{Band, ContextLifecycle, assign_ring_seats};

use crate::rpc::ContextInfo;

/// One ranked seat: a context id paired with the ring it landed on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RankedSeat {
    pub context_id: ContextId,
    pub band: Band,
}

/// Every live context assigned a seat, ring 0 first (in seat order) then
/// ring 1 (in recency order) — the full rank, with each seat's band.
///
/// Archived contexts are filtered before seating (the app does the same) —
/// they are not sessions/windows anyone wants offered.
pub fn ranked_seats(contexts: &[ContextInfo]) -> Vec<RankedSeat> {
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
    [Band::Active, Band::Recent]
        .into_iter()
        .flat_map(|band| {
            placement.rings[band.index()]
                .iter()
                .map(move |&context_id| RankedSeat { context_id, band })
        })
        .collect()
}

/// Just the ranked ids, in the same order [`ranked_seats`] returns them —
/// for a caller that doesn't need to distinguish bands.
pub fn ranked_context_ids(contexts: &[ContextInfo]) -> Vec<ContextId> {
    ranked_seats(contexts).into_iter().map(|s| s.context_id).collect()
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
            origin_host: None,
            cwd: None,
        }
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

    /// `ranked_seats` is the richer API `ranked_context_ids` is built on —
    /// new with this move, so it earns its own direct test rather than only
    /// being exercised indirectly through the flat helper.
    #[test]
    fn ranked_seats_tags_each_id_with_its_band() {
        let mut promoted = ctx("promoted");
        promoted.promoted_at = Some(1_000);
        let auto = ctx("auto-pool");

        let seats = ranked_seats(&[promoted.clone(), auto.clone()]);
        assert_eq!(seats.len(), 2);
        assert_eq!(seats[0], RankedSeat { context_id: promoted.id, band: Band::Active });
        assert_eq!(seats[1], RankedSeat { context_id: auto.id, band: Band::Recent });
    }
}
