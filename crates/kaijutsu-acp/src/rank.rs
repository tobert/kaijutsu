//! The session picker is **the rank**.
//!
//! ACP's `session/list` is a session picker; kaijutsu already has one — the
//! time well's rings. Seat order itself is `kaijutsu_client::rank` (moved
//! there so the app, ACP and the terminal client all seat the same ten ids
//! in the same order); this module is the ACP-specific half: mapping a
//! [`ContextId`] to and from an ACP [`SessionId`], and rendering the ranked
//! contexts as [`SessionInfo`] with a cwd every entry can show.

use std::path::{Path, PathBuf};

use agent_client_protocol::schema::v1::{SessionId, SessionInfo};
use kaijutsu_client::ContextInfo;
use kaijutsu_types::ContextId;

/// The ACP session id for a context: its hex form. Round-trips through
/// [`context_id_of`], so `session/load` can name any context the list showed.
pub fn session_id_of(id: ContextId) -> SessionId {
    SessionId::new(id.to_hex())
}

/// Parse an ACP session id back into a context id.
pub fn context_id_of(session: &SessionId) -> Option<ContextId> {
    ContextId::parse(session.0.as_ref()).ok()
}

/// The ACP session list: the rank, rendered as `SessionInfo`.
///
/// ACP requires a cwd, and every ranked context gets an entry — a context
/// with no durable cwd (one created by a hook, the app, or `kj context
/// create` rather than an ACP session) is defaulted to `fallback_cwd`
/// (the directory the ACP client itself connected from), never dropped.
///
/// Amy's ruling (2026-08-18): omitting a cwd-less context used to hide it
/// from the picker entirely — on a live 67-context kernel, only the
/// ACP-created ones (a third of the total) ever showed up. Resuming or
/// loading a session sets its real cwd anyway (`start_session` calls
/// `set_context_cwd`), so the fallback is a default a session outgrows on
/// first use, not a fabrication left standing — and a defaulted directory
/// is a smaller lie than an invisible session.
pub fn ranked_sessions(contexts: &[ContextInfo], fallback_cwd: &Path) -> Vec<SessionInfo> {
    let by_id: std::collections::HashMap<ContextId, &ContextInfo> =
        contexts.iter().map(|c| (c.id, c)).collect();
    kaijutsu_client::ranked_context_ids(contexts)
        .into_iter()
        .filter_map(|id| by_id.get(&id).copied())
        .map(|c| session_info(c, &effective_cwd(c, fallback_cwd)))
        .collect()
}

/// A context's display cwd: its own durable value if it has one, else the
/// fallback. Named (not an inline `unwrap_or`) so a reader of
/// `ranked_sessions` can see exactly which entries are defaulted.
fn effective_cwd(context: &ContextInfo, fallback: &Path) -> PathBuf {
    context
        .cwd
        .clone()
        .unwrap_or_else(|| fallback.to_path_buf())
}

fn session_info(c: &ContextInfo, cwd: &Path) -> SessionInfo {
    let title = if c.label.is_empty() {
        c.id.short()
    } else {
        c.label.clone()
    };
    let mut info = SessionInfo::new(session_id_of(c.id), PathBuf::from(cwd)).title(title);
    if let Some(ts) = c.last_activity_at.or(Some(c.created_at))
        && let Some(stamp) = rfc3339_millis(ts)
    {
        info = info.updated_at(stamp);
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
            last_call_at: None,
            cache_read_tokens: None,
            cache_write_tokens: None,
            cache_ttl_secs: None,
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
    fn session_ids_round_trip_through_context_ids() {
        let id = ContextId::new();
        assert_eq!(context_id_of(&session_id_of(id)), Some(id));
    }

    #[test]
    fn a_garbage_session_id_does_not_resolve() {
        assert!(context_id_of(&SessionId::new("not-a-uuid")).is_none());
    }

    #[test]
    fn session_info_carries_the_label_as_title() {
        let mut c = ctx("household-agent");
        c.last_activity_at = Some(1_700_000_000_000);
        c.cwd = Some(PathBuf::from("/tmp/work"));
        let infos = ranked_sessions(&[c.clone()], Path::new("/fallback"));
        assert_eq!(infos.len(), 1);
        assert_eq!(infos[0].session_id, session_id_of(c.id));
        assert_eq!(infos[0].title.as_deref(), Some("household-agent"));
        assert_eq!(infos[0].cwd, PathBuf::from("/tmp/work"));
        assert!(infos[0].updated_at.is_some());
    }

    #[test]
    fn an_unlabelled_context_falls_back_to_its_short_id() {
        let mut c = ctx("");
        c.cwd = Some(PathBuf::from("/tmp"));
        let infos = ranked_sessions(std::slice::from_ref(&c), Path::new("/fallback"));
        assert_eq!(infos[0].title.as_deref(), Some(c.id.short().as_str()));
    }

    /// The defect-2 regression guard: a context with no durable cwd used to
    /// be silently dropped from the picker (`filter_map` over a lookup
    /// miss). It must now appear, carrying the fallback cwd instead of the
    /// context's own (absent) one.
    #[test]
    fn context_without_a_durable_cwd_gets_the_client_fallback() {
        let c = ctx("legacy");
        assert!(c.cwd.is_none(), "fixture must start with no durable cwd");
        let fallback = Path::new("/home/amy/src/kaijutsu");
        let infos = ranked_sessions(&[c.clone()], fallback);
        assert_eq!(infos.len(), 1, "a cwd-less context must still be listed");
        assert_eq!(infos[0].cwd, fallback.to_path_buf());
    }

    /// The general form of the same guard: however many contexts the rank
    /// produces, `ranked_sessions` must return exactly that many entries —
    /// a `filter_map`/lookup-miss silently dropping one is exactly the bug
    /// that made two thirds of a live kernel's contexts unbrowsable.
    #[test]
    fn returned_count_always_equals_the_ranked_context_count() {
        let with_cwd = {
            let mut c = ctx("has-cwd");
            c.cwd = Some(PathBuf::from("/work/has-cwd"));
            c
        };
        let without_cwd = ctx("no-cwd");
        let contexts = [with_cwd, without_cwd];
        let ranked_count = kaijutsu_client::ranked_context_ids(&contexts).len();
        let infos = ranked_sessions(&contexts, Path::new("/fallback"));
        assert_eq!(infos.len(), ranked_count);
        assert_eq!(infos.len(), 2);
    }

    #[test]
    fn each_session_reports_its_own_context_cwd_or_the_fallback() {
        let mut a = ctx("alpha");
        a.last_activity_at = Some(2_000);
        a.cwd = Some(PathBuf::from("/work/alpha"));
        let mut b = ctx("beta");
        b.last_activity_at = Some(1_000);
        // beta has no durable cwd — it must get the fallback, not vanish.
        let infos = ranked_sessions(&[a.clone(), b.clone()], Path::new("/fallback"));
        let by_id = infos
            .into_iter()
            .map(|info| (info.session_id, info.cwd))
            .collect::<std::collections::HashMap<_, _>>();
        assert_eq!(by_id[&session_id_of(a.id)], PathBuf::from("/work/alpha"));
        assert_eq!(by_id[&session_id_of(b.id)], PathBuf::from("/fallback"));
    }

    #[test]
    fn timestamps_render_as_rfc3339() {
        let s = rfc3339_millis(1_700_000_000_000).expect("in range");
        assert!(s.starts_with("2023-11-14T"), "unexpected stamp: {s}");
    }
}
