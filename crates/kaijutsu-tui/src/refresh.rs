//! The background refresh: the rank and the tracks.
//!
//! The event loop never awaits the kernel for these. On `REFRESH` it spawns
//! [`fetch`] as its own task and goes back to reading keys; the task's
//! [`Refreshed`] comes back through a select arm and [`apply`] folds it
//! into the app without an await. A kernel busy with a coder turn is
//! exactly when a key must not wait on it (`docs/tui.md`, "Keys"). The open
//! asks are not polled here: the actor's ledger watch pushes them
//! (`crate::asks::fold_ledger`).
//!
//! Rounds are single-flight: a round still running when the next tick
//! lands is left to finish, and the tick after starts the next one.

use kaijutsu_client::{ContextInfo, TrackInfo};

use crate::app::App;
use crate::bridge::KernelBridge;
use crate::picker;

/// One finished round. A `None` is a call that failed: the app keeps what
/// it had, the way the loop always has.
#[derive(Default)]
pub struct Refreshed {
    pub contexts: Option<Vec<ContextInfo>>,
    pub tracks: Option<Vec<TrackInfo>>,
    /// A failed `list_contexts` round. The rank and the hot set it drives
    /// keep whatever they had — `contexts` is `None` alongside this, so
    /// [`apply`] never overwrites a good listing with nothing — but the
    /// player needs telling why the picker stopped moving.
    pub context_error: Option<String>,
}

/// Run one round against the kernel. Every await in the refresh lives here.
pub async fn fetch(bridge: KernelBridge) -> Refreshed {
    let (contexts, context_error) = match bridge.list_contexts().await {
        Ok(contexts) => (Some(contexts), None),
        Err(error) => (None, Some(format!("cannot refresh the context rank: {error}"))),
    };
    let tracks = bridge.actor().list_tracks().await.ok();
    Refreshed { contexts, tracks, context_error }
}

/// Fold a finished round into the app. No await: this runs on the loop.
pub fn apply(app: &mut App, refreshed: Refreshed) {
    if let Some(contexts) = refreshed.contexts {
        app.set_contexts(contexts);
    }
    if let Some(error) = refreshed.context_error {
        app.note(error);
    }
    if let Some(tracks) = refreshed.tracks {
        app.tracks = tracks.iter().map(picker::track_row_from).collect();
    }
    app.refresh_picker(kaijutsu_types::now_millis());
}

#[cfg(test)]
mod tests {
    use super::*;
    use kaijutsu_types::ContextId;

    /// A minimal `ContextInfo` row, named so a test can tell which one
    /// survived a fold.
    fn context_row(label: &str) -> ContextInfo {
        ContextInfo {
            id: ContextId::new(),
            label: label.to_string(),
            forked_from: None,
            provider: String::new(),
            model: "deepseek/deepseek-v4".to_string(),
            created_at: 1_000,
            trace_id: [0u8; 16],
            fork_kind: None,
            context_type: "coder".to_string(),
            archived: false,
            concluded_at: None,
            keywords: Vec::new(),
            top_block_preview: None,
            live_status: kaijutsu_types::Status::Pending,
            last_activity_at: Some(1_000),
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
            last_call_at: None,
            cache_read_tokens: None,
            cache_write_tokens: None,
            cache_ttl_secs: None,
        }
    }

    /// A failed `list_contexts` round is a `None` alongside a
    /// `context_error`, never an empty listing: the rank and the hot set it
    /// drives keep the last good row, and the player is told why the picker
    /// stopped moving.
    #[test]
    fn a_failed_context_round_keeps_the_old_rank_and_says_why() {
        let mut app = App::new("amy");
        let kept = context_row("kept");
        app.set_contexts(vec![kept.clone()]);
        apply(
            &mut app,
            Refreshed {
                context_error: Some("cannot refresh the context rank: kernel unreachable".into()),
                ..Default::default()
            },
        );
        assert_eq!(app.contexts, vec![kept], "a failed round never blanks the rank");
        assert_eq!(app.notice(), Some("cannot refresh the context rank: kernel unreachable"));
    }
}
