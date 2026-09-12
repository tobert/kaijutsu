//! The background refresh: the rank, the pending asks and the tracks.
//!
//! The event loop never awaits the kernel for these. On `REFRESH` it copies
//! what a round needs out of its state into a [`Request`], spawns
//! [`fetch`] as its own task, and goes back to reading keys; the task's
//! [`Refreshed`] comes back through a select arm and [`apply`] folds it
//! into the app without an await. A kernel busy with a coder turn is
//! exactly when a key must not wait on it (`docs/tui.md`, "Keys").
//!
//! Rounds are single-flight: a round still running when the next tick
//! lands is left to finish, and the tick after starts the next one.

use std::collections::HashSet;

use kaijutsu_client::{AskDetail, ContextInfo, PendingAsk, TrackInfo};
use kaijutsu_types::{ContextId, PrincipalId};

use crate::app::App;
use crate::asks::AskCardState;
use crate::bridge::KernelBridge;
use crate::picker;

/// What one round needs from the loop, copied out when the round starts.
#[derive(Debug, Clone, Default)]
pub struct Request {
    pub current: Option<ContextId>,
    /// Poll the ledger this round. The loop turns it on for one round per
    /// ledger generation bump, never on the timer: the poll runs `kj ledger
    /// list` in the context, which authors a ToolCall/ToolResult pair.
    pub poll_asks: bool,
    /// The ids offered so far; pruned and extended by the poll.
    pub seen_asks: HashSet<String>,
    /// The ask card up when the round started, so its decision can be read
    /// for the status-line notice if its ask leaves the pending set.
    pub card: Option<(String, ContextId)>,
}

/// One finished round. A `None` is a call that failed: the app keeps what
/// it had, the way the loop always has.
#[derive(Default)]
pub struct Refreshed {
    pub contexts: Option<Vec<ContextInfo>>,
    pub tracks: Option<Vec<TrackInfo>>,
    pub asks: Option<AskPoll>,
}

/// The ledger half of a round.
#[derive(Default)]
pub struct AskPoll {
    /// `seen_asks` after the poll: only ids already offered as cards. An ask
    /// stays unseen until the TUI can present it.
    pub seen: HashSet<String>,
    pub new_asks: Vec<PendingAsk>,
    /// Every ask still pending, including asks not yet presented as a card.
    pub pending_count: usize,
    /// The card to open: the first new ask raised in the polled context,
    /// read in full. Opened only if no card is up when the round lands.
    pub card: Option<AskCardState>,
    /// The request's card, once its ask left the pending set: its id and
    /// its decision. `None` inside when `kj ledger show` could not be read.
    pub answered: Option<(String, Option<AskDetail>)>,
}

/// Run one round against the kernel. Every await in the refresh lives here.
pub async fn fetch(bridge: KernelBridge, request: Request) -> Refreshed {
    let contexts = bridge.list_contexts().await.ok();
    let asks = match (request.poll_asks, request.current) {
        (true, Some(ctx)) => poll(&bridge, ctx, request.seen_asks, request.card).await,
        _ => None,
    };
    let tracks = bridge.actor().list_tracks().await.ok();
    Refreshed { contexts, tracks, asks }
}

async fn poll(
    bridge: &KernelBridge,
    ctx: ContextId,
    mut seen: HashSet<String>,
    card: Option<(String, ContextId)>,
) -> Option<AskPoll> {
    let actor = bridge.actor();
    let new_asks = kaijutsu_client::poll_new_asks(actor, ctx, &mut seen).await.ok()?;
    let answered = match card {
        Some((id, card_ctx)) if !seen.contains(&id) => {
            let detail = kaijutsu_client::show_ask_detail(actor, card_ctx, &id).await.ok().flatten();
            Some((id, detail))
        }
        _ => None,
    };
    // A card is auto-shown only for the polled context's own ask, never a
    // modal for a seat you are not looking at (`docs/tui.md`, "Asks").
    let mut opened = None;
    for ask in &new_asks {
        if opened.is_none() && ask.info.context_id == ctx {
            if let Ok(Some(detail)) = kaijutsu_client::show_ask_detail(actor, ctx, &ask.request_id).await {
                // An id becomes seen only when it has actually been offered
                // as a card. Marking every listed id here loses later asks.
                seen.insert(ask.request_id.clone());
                opened = Some(AskCardState { request_id: ask.request_id.clone(), context_id: ctx, detail });
            }
        }
    }
    let pending_count = seen.len()
        + new_asks.iter().filter(|ask| !seen.contains(&ask.request_id)).count();
    Some(AskPoll { pending_count, seen, new_asks, card: opened, answered })
}

/// Fold a finished round into the app. No await: this runs on the loop.
pub fn apply(app: &mut App, refreshed: Refreshed, seen_asks: &mut HashSet<String>) {
    if let Some(contexts) = refreshed.contexts {
        app.set_contexts(contexts);
    }
    if let Some(poll) = refreshed.asks {
        // A card whose ask left the pending set comes down first, so the
        // next ask below gets the card instead of only a `!`.
        if let Some(card) = app.take_answered_card(&poll.seen) {
            let detail = poll
                .answered
                .as_ref()
                .filter(|(id, _)| *id == card.request_id)
                .and_then(|(_, detail)| detail.as_ref());
            app.note(answered_notice(app.principal, &card.request_id, detail));
        }
        for ask in &poll.new_asks {
            app.note_ask(ask.request_id.clone(), ask.info.context_id);
        }
        if app.ask_card.is_none()
            && let Some(card) = poll.card
            && app.current == Some(card.context_id)
        {
            app.ask_card = Some(card);
        }
        app.forget_asks_not_pending(&poll.seen);
        app.pending_asks = poll.pending_count;
        *seen_asks = poll.seen;
    }
    if let Some(tracks) = refreshed.tracks {
        app.tracks = tracks.iter().map(picker::track_row_from).collect();
    }
}

/// The status-line notice for a card whose ask was decided from another
/// surface: `ask <id> allowed by you` / `by <principal>` / `expired`. Falls
/// back to `no longer pending` when the decision could not be read — the
/// card is already down either way.
pub fn answered_notice(me: Option<PrincipalId>, request_id: &str, detail: Option<&AskDetail>) -> String {
    let id = short_ask(request_id);
    let Some(detail) = detail else {
        return format!("ask {id} no longer pending");
    };
    let outcome = decision_words(detail);
    match detail.decided_by {
        Some(by) if me == Some(by) => format!("ask {id} {outcome} by you"),
        Some(by) => format!("ask {id} {outcome} by {}", by.short()),
        None => format!("ask {id} {outcome}"),
    }
}

/// The first segment of a request id — enough to find it in `kj ledger
/// list`, short enough for a status-line notice beside the facts.
pub fn short_ask(request_id: &str) -> &str {
    request_id.split('-').next().unwrap_or(request_id)
}

/// `allow once` / `allow always` / `deny` from `decided_option`, else the
/// coarser `status` (`allowed`, `denied`, `expired`, `abandoned`).
pub fn decision_words(detail: &AskDetail) -> String {
    match detail.decided_option.as_deref() {
        Some(option) => option.replace('_', " "),
        None => detail.status.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kaijutsu_client::AskInfo;

    fn detail(request_id: &str, ctx: ContextId) -> AskDetail {
        AskDetail {
            request_id: request_id.to_string(),
            context_id: Some(ctx),
            principal_id: None,
            principal_name: None,
            actor_id: None,
            actor_name: None,
            reviewer_id: None,
            reviewer_name: None,
            status: "pending".to_string(),
            origin: "shell".to_string(),
            tool: Some("shell_write".to_string()),
            hook_id: None,
            instance: None,
            description: "rm -rf ~/src/wt/x".to_string(),
            authorized_label: None,
            statements: vec![],
            exec_source: None,
            cwd: None,
            env: vec![],
            created_at: None,
            decided_at: None,
            decided_by: None,
            decided_by_name: None,
            decided_option: None,
            remember_scope: None,
            redeemed_at: None,
        }
    }

    fn pending(request_id: &str, ctx: ContextId) -> PendingAsk {
        PendingAsk {
            request_id: request_id.to_string(),
            info: AskInfo { context_id: ctx, description: "rm".to_string() },
        }
    }

    fn poll_with(seen: &[&str], new_asks: Vec<PendingAsk>, card: Option<AskCardState>) -> AskPoll {
        AskPoll {
            seen: seen.iter().map(|s| s.to_string()).collect(),
            new_asks,
            pending_count: seen.len(),
            card,
            answered: None,
        }
    }

    #[test]
    fn a_new_ask_in_the_current_context_opens_the_card_and_counts() {
        let ctx = ContextId::new();
        let mut app = App::new("amy");
        app.current = Some(ctx);
        let mut seen = HashSet::new();
        let card = AskCardState { request_id: "a-1".into(), context_id: ctx, detail: detail("a-1", ctx) };
        let refreshed = Refreshed {
            asks: Some(poll_with(&["a-1"], vec![pending("a-1", ctx)], Some(card))),
            ..Default::default()
        };
        apply(&mut app, refreshed, &mut seen);
        assert_eq!(app.ask_card.as_ref().map(|c| c.request_id.as_str()), Some("a-1"));
        assert!(app.has_pending_ask(ctx));
        assert_eq!(app.pending_asks, 1);
        assert!(seen.contains("a-1"), "the loop's seen set follows the poll's");
    }

    #[test]
    fn a_card_never_opens_over_one_already_up() {
        let ctx = ContextId::new();
        let mut app = App::new("amy");
        app.current = Some(ctx);
        app.ask_card = Some(AskCardState { request_id: "a-1".into(), context_id: ctx, detail: detail("a-1", ctx) });
        let mut seen: HashSet<String> = ["a-1".to_string()].into_iter().collect();
        let newer = AskCardState { request_id: "b-2".into(), context_id: ctx, detail: detail("b-2", ctx) };
        let refreshed = Refreshed {
            asks: Some(poll_with(&["a-1", "b-2"], vec![pending("b-2", ctx)], Some(newer))),
            ..Default::default()
        };
        apply(&mut app, refreshed, &mut seen);
        assert_eq!(app.ask_card.as_ref().map(|c| c.request_id.as_str()), Some("a-1"));
        assert_eq!(app.pending_asks, 2);
    }

    #[test]
    fn a_card_whose_ask_left_pending_comes_down_with_who_decided_it() {
        let ctx = ContextId::new();
        let me = PrincipalId::new();
        let mut app = App::new("amy");
        app.current = Some(ctx);
        app.principal = Some(me);
        app.ask_card = Some(AskCardState { request_id: "a-1".into(), context_id: ctx, detail: detail("a-1", ctx) });
        app.note_ask("a-1".into(), ctx);
        let mut seen: HashSet<String> = ["a-1".to_string()].into_iter().collect();
        let mut decided = detail("a-1", ctx);
        decided.status = "allowed".into();
        decided.decided_by = Some(me);
        decided.decided_option = Some("allow_once".into());
        let mut poll = poll_with(&[], vec![], None);
        poll.answered = Some(("a-1".into(), Some(decided)));
        let refreshed = Refreshed { asks: Some(poll), ..Default::default() };
        apply(&mut app, refreshed, &mut seen);
        assert!(app.ask_card.is_none());
        assert_eq!(app.notice(), Some("ask a allow once by you"));
        assert!(!app.has_pending_ask(ctx), "the seat's `!` clears with the ask");
        assert_eq!(app.pending_asks, 0);
        assert!(seen.is_empty());
    }

    #[test]
    fn a_failed_call_keeps_what_the_app_had() {
        let ctx = ContextId::new();
        let mut app = App::new("amy");
        app.note_ask("a-1".into(), ctx);
        app.pending_asks = 1;
        let mut seen: HashSet<String> = ["a-1".to_string()].into_iter().collect();
        apply(&mut app, Refreshed::default(), &mut seen);
        assert!(app.has_pending_ask(ctx));
        assert_eq!(app.pending_asks, 1);
        assert_eq!(seen.len(), 1);
    }

    #[test]
    fn notice_wording_names_the_other_principal_or_the_outcome_alone() {
        let ctx = ContextId::new();
        let other = PrincipalId::new();
        let mut decided = detail("01a04eb6-77", ctx);
        decided.status = "denied".into();
        decided.decided_by = Some(other);
        decided.decided_option = Some("deny".into());
        assert_eq!(
            answered_notice(None, "01a04eb6-77", Some(&decided)),
            format!("ask 01a04eb6 deny by {}", other.short())
        );
        let mut expired = detail("01a04eb6-77", ctx);
        expired.status = "expired".into();
        assert_eq!(answered_notice(None, "01a04eb6-77", Some(&expired)), "ask 01a04eb6 expired");
        assert_eq!(answered_notice(None, "01a04eb6-77", None), "ask 01a04eb6 no longer pending");
    }
}
