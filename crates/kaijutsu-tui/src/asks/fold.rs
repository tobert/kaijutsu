//! Folding the ledger into the app and landing the kernel's answers: the open
//! set, the card candidate, the reads that put a card or the ledger view up,
//! and the notices a landed answer posts.

use kaijutsu_client::LedgerState;
use kaijutsu_client::actor::CallError;
use kaijutsu_types::{AskAnswerFailureKind, AskDetail, ContextId, PrincipalId};

use crate::app::App;

use super::state::{AnsweredRow, AskCardState, AskWork, LedgerRow, LedgerViewState, Offer, PendingRow, RedeemedMark};

/// A context's label and performer, defaulting to its short id and
/// [`crate::app::NO_PERFORMER`] when [`crate::app::App`] has no
/// [`kaijutsu_client::ContextInfo`] for it (an ask can outlive the context
/// it named — `docs/gate-resume.md`, "Archived contexts are inert") or the
/// context has no performer.
pub fn context_facts(app: &crate::app::App, ctx: kaijutsu_types::ContextId) -> (String, String) {
    match app.info(ctx) {
        Some(info) => (
            if info.label.is_empty() { ctx.short() } else { info.label.clone() },
            info.performer.clone().unwrap_or_else(|| crate::app::NO_PERFORMER.to_string()),
        ),
        None => (ctx.short(), crate::app::NO_PERFORMER.to_string()),
    }
}

/// What folding one ledger state into the app leaves for [`crate::run`] to
/// do. Each item needs a kernel call, which the loop runs on its own task.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct LedgerFold {
    /// The session's first synced fold ([`super::NotifySeen::baseline`]).
    pub baseline: bool,
    /// The context of an ask this fold found newly open: one raised in the
    /// context on screen when there is one, else the first. `None` when no
    /// ask is new.
    pub raised_in: Option<ContextId>,
    /// The card this fold took down because its ask left the open set:
    /// answered from another surface, cancelled, expired, or abandoned. The
    /// loop reads its record for [`answered_notice`].
    pub answered: Option<String>,
}

/// Fold the actor's ledger state into the app: the open set the seats' `!`
/// and the `!n` read, the card's summary (a reassignment shows while the
/// card is up), and the card itself once its ask leaves the open set.
///
/// `None` when the state is not synced. Before the first listing after a
/// (re)connect lands, `open` may be empty only because nothing was read, so
/// the app keeps what it had.
pub fn fold_ledger(app: &mut App, state: &LedgerState) -> Option<LedgerFold> {
    if !state.synced {
        return None;
    }
    let baseline = !std::mem::replace(&mut app.ledger_folded, true);
    let raised: Vec<ContextId> = state
        .open
        .values()
        .filter(|ask| !app.open_asks.contains_key(&ask.request_id))
        .filter_map(|ask| ask.context_id)
        .collect();
    let raised_in = raised
        .iter()
        .copied()
        .find(|ctx| app.current == Some(*ctx))
        .or_else(|| raised.first().copied());
    let mut answered = None;
    if let Some(card) = app.ask_card.as_mut() {
        match state.open.get(&card.request_id) {
            Some(summary) => card.detail.summary = summary.clone(),
            None => answered = Some(card.request_id.clone()),
        }
    }
    if answered.is_some() {
        app.ask_card = None;
    }
    app.asks_set_aside.retain(|id| state.open.contains_key(id));
    app.card_reads_missed.clear();
    app.open_asks = state.open.clone();
    Some(LedgerFold { baseline, raised_in, answered })
}

/// The ask whose card should come up now: none while a card is up, else the
/// oldest open ask raised in the context on screen that was not set aside
/// and whose last card read did not miss. A card is never raised for a seat
/// you are not looking at.
pub fn card_candidate(app: &App) -> Option<String> {
    if app.ask_card.is_some() || app.ledger_view.is_some() {
        return None;
    }
    let current = app.current?;
    app.open_asks
        .values()
        .filter(|ask| ask.context_id == Some(current))
        .filter(|ask| !app.asks_set_aside.contains(&ask.request_id) && !app.card_reads_missed.contains(&ask.request_id))
        .min_by(|a, b| (a.created_at_ms, &a.request_id).cmp(&(b.created_at_ms, &b.request_id)))
        .map(|ask| ask.request_id.clone())
}

/// Put up the card for `detail`, read for [`card_candidate`]'s ask, if that
/// ask is still the candidate when the read lands: a switch, an `Esc`, or
/// the ask closing may have moved on since. The summary comes from the
/// folded open set, which the push keeps current. True when it went up.
pub fn open_card(app: &mut App, mut detail: AskDetail) -> bool {
    let request_id = detail.summary.request_id.clone();
    if card_candidate(app).as_deref() != Some(request_id.as_str()) {
        return false;
    }
    let (Some(summary), Some(context_id)) = (app.open_asks.get(&request_id), app.current) else {
        return false;
    };
    detail.summary = summary.clone();
    app.ask_card = Some(AskCardState { request_id, context_id, detail, reassigning: false });
    true
}

/// Land the card candidate's record: put the card up, or, when the read
/// found nothing or failed, leave that ask out of [`card_candidate`] until
/// the next fold.
pub fn land_card_read(app: &mut App, request_id: &str, read: Result<Option<AskDetail>, CallError>) {
    match read {
        Ok(Some(detail)) => {
            open_card(app, detail);
        }
        // Closed before the read; the next push says so.
        Ok(None) => {
            app.card_reads_missed.insert(request_id.to_string());
        }
        Err(error) => {
            app.card_reads_missed.insert(request_id.to_string());
            app.note(format!("cannot read ask {}: {error}", short_ask(request_id)));
        }
    }
}

/// How many answered asks the ledger view lists, newest first.
pub const LEDGER_HISTORY_LIMIT: u32 = 20;

/// Ask for the ledger view: the open asks, oldest first, then the recent
/// answers.
pub fn request_ledger(app: &mut App) {
    let mut open: Vec<&kaijutsu_types::AskSummary> = app.open_asks.values().collect();
    open.sort_by(|a, b| (a.created_at_ms, &a.request_id).cmp(&(b.created_at_ms, &b.request_id)));
    let open = open.into_iter().map(|ask| ask.request_id.clone()).collect();
    app.ask_work.push(AskWork::OpenLedger { open });
}

/// The notice for an ask its reviewer must answer.
pub(super) fn awaits_reviewer(request_id: &str) -> String {
    format!(
        "ask {} awaits its assigned reviewer; cancel it or ask that reviewer to escalate",
        short_ask(request_id)
    )
}

/// Land one answer: say what the ledger did with it. An answer that did
/// not land, other than a lost race, un-sets its ask so its card can come
/// up again.
pub fn land_decision(app: &mut App, request_id: &str, answer: Result<kaijutsu_client::AskAnswer, CallError>) {
    let lost = match &answer {
        Ok(Ok(_)) => false,
        Ok(Err(failure)) => failure.kind != AskAnswerFailureKind::AlreadyAnswered,
        Err(_) => true,
    };
    if lost {
        app.asks_set_aside.remove(request_id);
    }
    app.note(decision_notice(request_id, answer));
}

/// The notice [`land_decision`] posts: the answer, a lost race (ordinary,
/// not an error), a refusal, or a call failure.
pub fn decision_notice(request_id: &str, answer: Result<kaijutsu_client::AskAnswer, CallError>) -> String {
    let id = short_ask(request_id);
    match answer {
        Ok(Ok(answered)) => {
            let verb = answered.summary.status.as_str();
            // The kernel's note says which: `remembered as a standing …` or
            // `NOT remembered: …`.
            match answered.remembered {
                Some(rule) => format!("ask {id} {verb}; {}", rule.note),
                None => format!("ask {id} {verb}"),
            }
        }
        Ok(Err(failure)) if failure.kind == AskAnswerFailureKind::AlreadyAnswered => {
            format!("ask {id} was already answered")
        }
        Ok(Err(failure)) => format!("ask {id} not answered: {}", failure.message),
        Err(error) => format!("cannot answer ask {id}: {error}"),
    }
}

/// Land one reassignment: say who reviews the ask now, or why it did not
/// move. The ask stays open either way; the push carries the new reviewer.
pub fn land_escalation(app: &mut App, request_id: &str, answer: Result<kaijutsu_client::AskAnswer, CallError>, took: bool) {
    if let Some(card) = app.ask_card.as_mut().filter(|card| card.request_id == request_id) {
        card.reassigning = false;
    }
    app.note(escalation_notice(request_id, answer, took));
}

/// The notice [`land_escalation`] posts.
pub fn escalation_notice(request_id: &str, answer: Result<kaijutsu_client::AskAnswer, CallError>, took: bool) -> String {
    let id = short_ask(request_id);
    match answer {
        Ok(Ok(moved)) => {
            let name = moved.summary.reviewer.as_ref().map_or("(unassigned)", |who| who.name.as_str());
            format!("ask {id} now reviewed by {name}")
        }
        Ok(Err(failure)) => {
            format!("ask {id} not {}: {}", if took { "taken over" } else { "escalated" }, failure.message)
        }
        Err(error) => format!("cannot {} ask {id}: {error}", if took { "take over" } else { "escalate" }),
    }
}

/// Land a `Show` read into the open ledger view.
pub fn land_show(app: &mut App, request_id: &str, read: Result<Option<AskDetail>, CallError>) {
    match read {
        Ok(Some(detail)) => {
            if let Some(view) = app.ledger_view.as_mut() {
                view.detail = Some(detail);
            }
        }
        Ok(None) => app.note(format!("ask {} is not in the ledger", short_ask(request_id))),
        Err(error) => app.note(format!("cannot read ask {}: {error}", short_ask(request_id))),
    }
}

/// Every record the ledger view asked for, in the order asked: the open
/// asks, then the recent answers.
pub type LedgerReads = Vec<(String, Result<Option<AskDetail>, CallError>)>;

/// Land the ledger view's reads at `now_ms`. A record read answered is an
/// ANSWERED row wherever it was listed, and an ask in both listings is
/// listed once. An unreadable row is named on the status line.
pub fn land_ledger(app: &mut App, reads: Result<LedgerReads, CallError>, now_ms: u64) {
    let reads = match reads {
        Ok(reads) => reads,
        Err(error) => {
            app.note(format!("cannot read ledger history: {error}"));
            return;
        }
    };
    let mut rows = Vec::with_capacity(reads.len());
    let mut listed = std::collections::HashSet::new();
    for (request_id, read) in reads {
        match read {
            Ok(Some(detail)) if listed.insert(request_id.clone()) => {
                rows.push(if detail.summary.status.is_open() {
                    LedgerRow::Pending(pending_row(app, &detail, now_ms))
                } else {
                    LedgerRow::Answered(answered_row(app, &detail))
                })
            }
            Ok(_) => {}
            Err(error) => app.note(format!("cannot read ask {}: {error}", short_ask(&request_id))),
        }
    }
    rows.sort_by_key(|row| !matches!(row, LedgerRow::Pending(_)));
    // The ledger takes the keys, so it takes the screen: a card up under it
    // would be drawn over rows the keys now answer.
    if let Some(card) = app.ask_card.take() {
        app.asks_set_aside.insert(card.request_id);
    }
    app.ledger_view = Some(LedgerViewState { rows, filter: String::new(), selected: 0, filtering: false, detail: None });
}

/// One ask's record as the ledger view's PENDING row, its age taken at
/// `now_ms`.
fn pending_row(app: &App, detail: &AskDetail, now_ms: u64) -> PendingRow {
    let summary = &detail.summary;
    let (context_label, performer) = summary
        .context_id
        .map(|ctx| context_facts(app, ctx))
        .unwrap_or_else(|| ("(unknown)".to_string(), crate::app::NO_PERFORMER.to_string()));
    PendingRow {
        request_id: summary.request_id.clone(),
        age: Some(crate::status::format_age(std::time::Duration::from_millis(
            now_ms.saturating_sub(summary.created_at_ms.max(0) as u64),
        ))),
        context_label,
        performer,
        hook: detail.tool.clone().unwrap_or_else(|| "-".to_string()),
        asker: summary.performer.as_ref().map(|who| who.name.clone()),
        reviewer: summary.reviewer.as_ref().map(|who| who.name.clone()),
        offer: Offer::of(summary, app.principal),
        statement: summary.statements.first().cloned().unwrap_or_else(|| summary.description.clone()),
    }
}

/// One ask's record as the ledger view's ANSWERED row: when it was decided,
/// how (`allow once`/`allow always`/`deny`, or the status for an expired or
/// abandoned ask), and by whom (`you`, the decider's name, or `—` for a
/// rule's decision).
fn answered_row(app: &App, detail: &AskDetail) -> AnsweredRow {
    let summary = &detail.summary;
    let (context_label, _performer) = summary
        .context_id
        .map(|ctx| context_facts(app, ctx))
        .unwrap_or_else(|| ("(unknown)".to_string(), crate::app::NO_PERFORMER.to_string()));
    let redeemed = match detail.redeemed_at_ms {
        Some(at) => RedeemedMark::At(crate::render::wallclock(at as u64)),
        None => RedeemedMark::Never,
    };
    AnsweredRow {
        request_id: summary.request_id.clone(),
        time: summary.decided_at_ms.map(|at| crate::render::wallclock(at as u64)),
        context_label,
        decision: Some(decision_words(detail)),
        principal: detail.decision.as_ref().and_then(|decision| decision.decided_by.as_ref()).map(|by| {
            if app.principal == Some(by.id) { "you".to_string() } else { by.name.clone() }
        }),
        redeemed,
        statement: summary.statements.first().cloned().unwrap_or_else(|| summary.description.clone()),
    }
}

/// The status-line notice for a card whose ask left the open set without
/// this card answering it: `ask <id> allow once by you`, `by <name>`, or
/// `expired`. Falls back to `no longer pending` when the record could not
/// be read; the card is already down either way.
pub fn answered_notice(me: Option<PrincipalId>, request_id: &str, detail: Option<&AskDetail>) -> String {
    let id = short_ask(request_id);
    let Some(detail) = detail else {
        return format!("ask {id} no longer pending");
    };
    let outcome = decision_words(detail);
    match detail.decision.as_ref().and_then(|decision| decision.decided_by.as_ref()) {
        Some(by) if me == Some(by.id) => format!("ask {id} {outcome} by you"),
        Some(by) => format!("ask {id} {outcome} by {}", by.name),
        None => format!("ask {id} {outcome}"),
    }
}

/// The first segment of a request id: enough to find it in `kj ledger
/// list`, short enough for a status-line notice beside the facts.
pub fn short_ask(request_id: &str) -> &str {
    request_id.split('-').next().unwrap_or(request_id)
}

/// `allow once` / `allow always` / `deny` from the decision's option, else
/// the coarser status (`allowed`, `denied`, `expired`, `abandoned`).
pub fn decision_words(detail: &AskDetail) -> String {
    match detail.decision.as_ref().and_then(|decision| decision.option.as_deref()) {
        Some(option) => option.replace('_', " "),
        None => detail.summary.status.as_str().to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::asks::fixtures::{app_with_program_card, reviewer, state};
    use crate::asks::fixtures::{card, detail, summary};
    use crate::asks::{card_candidate, open_card};
    use kaijutsu_types::AskDecision as Decision;

    #[test]
    fn an_unsynced_state_changes_nothing() {
        let ctx = ContextId::new();
        let mut app = App::new("amy");
        app.current = Some(ctx);
        app.open_asks.insert("a-1".into(), summary("a-1", ctx, "rm"));
        app.ask_card = Some(card("a-1", ctx, "rm"));
        let unsynced = LedgerState::default();
        assert_eq!(fold_ledger(&mut app, &unsynced), None);
        assert!(app.has_pending_ask(ctx), "an empty listing that was never read is not an empty ledger");
        assert!(app.ask_card.is_some());
        assert_eq!(app.status_model(0).pending_asks, 1);
    }

    #[test]
    fn the_first_fold_is_the_baseline_and_later_ones_report_new_asks() {
        let ctx = ContextId::new();
        let mut app = App::new("amy");
        app.current = Some(ctx);
        let first = fold_ledger(&mut app, &state(&[summary("a-1", ctx, "rm")])).unwrap();
        assert!(first.baseline);
        assert_eq!(first.raised_in, Some(ctx));
        let again = fold_ledger(&mut app, &state(&[summary("a-1", ctx, "rm")])).unwrap();
        assert!(!again.baseline);
        assert_eq!(again.raised_in, None, "an ask already folded in is not news");
        let other = ContextId::new();
        let both = state(&[summary("a-1", ctx, "rm"), summary("b-2", other, "ls"), summary("c-3", ctx, "cp")]);
        assert_eq!(fold_ledger(&mut app, &both).unwrap().raised_in, Some(ctx), "the seat on screen wins");
    }

    #[test]
    fn an_open_ask_in_the_current_context_is_the_card_candidate_and_counts() {
        let ctx = ContextId::new();
        let mut app = App::new("amy");
        app.current = Some(ctx);
        fold_ledger(&mut app, &state(&[summary("a-1", ctx, "rm")]));
        assert_eq!(card_candidate(&app).as_deref(), Some("a-1"));
        assert!(open_card(&mut app, detail("a-1", ctx, "rm")));
        assert_eq!(app.ask_card.as_ref().map(|c| c.request_id.as_str()), Some("a-1"));
        assert!(app.has_pending_ask(ctx));
        assert_eq!(app.status_model(0).pending_asks, 1);
    }

    #[test]
    fn a_card_never_opens_over_one_already_up() {
        let ctx = ContextId::new();
        let mut app = App::new("amy");
        app.current = Some(ctx);
        fold_ledger(&mut app, &state(&[summary("a-1", ctx, "rm")]));
        assert!(open_card(&mut app, detail("a-1", ctx, "rm")));
        fold_ledger(&mut app, &state(&[summary("a-1", ctx, "rm"), summary("b-2", ctx, "ls")]));
        assert_eq!(card_candidate(&app), None);
        assert!(!open_card(&mut app, detail("b-2", ctx, "ls")));
        assert_eq!(app.ask_card.as_ref().map(|c| c.request_id.as_str()), Some("a-1"));
        assert_eq!(app.status_model(0).pending_asks, 2);
    }

    #[test]
    fn the_oldest_open_ask_comes_up_first_and_the_next_follows_it() {
        let ctx = ContextId::new();
        let mut app = App::new("amy");
        app.current = Some(ctx);
        let mut older = summary("z-older", ctx, "rm");
        older.created_at_ms = 10;
        let mut newer = summary("a-newer", ctx, "ls");
        newer.created_at_ms = 20;
        fold_ledger(&mut app, &state(&[older.clone(), newer.clone()]));
        assert_eq!(card_candidate(&app).as_deref(), Some("z-older"));
        assert!(open_card(&mut app, detail("z-older", ctx, "rm")));

        // Answered elsewhere: the card comes down and the next ask is the
        // candidate, so the next card goes up rather than only a `!`.
        let fold = fold_ledger(&mut app, &state(&[newer])).unwrap();
        assert_eq!(fold.answered.as_deref(), Some("z-older"));
        assert!(app.ask_card.is_none());
        assert_eq!(card_candidate(&app).as_deref(), Some("a-newer"));
    }

    #[test]
    fn a_set_aside_ask_waits_for_a_switch_back_and_stays_aside() {
        let ctx = ContextId::new();
        let other = ContextId::new();
        let mut app = App::new("amy");
        app.current = Some(ctx);
        fold_ledger(&mut app, &state(&[summary("a-1", ctx, "rm"), summary("b-2", ctx, "ls")]));
        app.asks_set_aside.insert("a-1".into());
        assert_eq!(card_candidate(&app).as_deref(), Some("b-2"), "Esc on one card lets the next one up");
        assert!(open_card(&mut app, detail("b-2", ctx, "ls")));

        app.switch_to(other);
        assert!(app.ask_card.is_none(), "a switch takes the card down");
        assert_eq!(card_candidate(&app), None, "no card for a seat you are not looking at");
        app.switch_to(ctx);
        assert_eq!(card_candidate(&app).as_deref(), Some("b-2"), "the card comes back on return");
    }

    #[test]
    fn a_card_read_that_lands_after_a_switch_does_not_go_up() {
        let ctx = ContextId::new();
        let mut app = App::new("amy");
        app.current = Some(ctx);
        fold_ledger(&mut app, &state(&[summary("a-1", ctx, "rm")]));
        app.switch_to(ContextId::new());
        assert!(!open_card(&mut app, detail("a-1", ctx, "rm")));
        assert!(app.ask_card.is_none());
    }

    #[test]
    fn an_other_context_open_ask_keeps_its_seat_indicator_without_a_card() {
        let current = ContextId::new();
        let other = ContextId::new();
        let mut app = App::new("amy");
        app.current = Some(current);
        fold_ledger(&mut app, &state(&[summary("o-1", other, "rm")]));
        assert!(app.has_pending_ask(other));
        assert!(!app.has_pending_ask(current));
        assert_eq!(card_candidate(&app), None);
    }

    #[test]
    fn an_open_card_follows_its_reassignment() {
        let ctx = ContextId::new();
        let me = PrincipalId::new();
        let lead = PrincipalId::new();
        let mut app = App::new("amy");
        app.current = Some(ctx);
        app.principal = Some(me);
        let mut mine = summary("a-1", ctx, "rm");
        mine.performer = reviewer(PrincipalId::new(), "coder");
        mine.reviewer = reviewer(me, "amy");
        fold_ledger(&mut app, &state(&[mine.clone()]));
        assert!(open_card(&mut app, detail("a-1", ctx, "rm")));
        assert!(app.ask_card.as_ref().unwrap().detail.summary.answerable_by(me));

        let mut moved = mine;
        moved.reviewer = reviewer(lead, "lead");
        fold_ledger(&mut app, &state(&[moved]));
        let shown = &app.ask_card.as_ref().unwrap().detail.summary;
        assert_eq!(shown.reviewer.as_ref().map(|r| r.id), Some(lead));
        assert!(!shown.answerable_by(me), "the card stops offering approval keys");
    }

    #[test]
    fn a_card_whose_ask_left_the_open_set_comes_down_and_clears_the_seat() {
        let ctx = ContextId::new();
        let mut app = App::new("amy");
        app.current = Some(ctx);
        fold_ledger(&mut app, &state(&[summary("a-1", ctx, "rm")]));
        assert!(open_card(&mut app, detail("a-1", ctx, "rm")));
        app.asks_set_aside.insert("gone".into());
        let fold = fold_ledger(&mut app, &state(&[])).unwrap();
        assert_eq!(fold.answered.as_deref(), Some("a-1"));
        assert!(app.ask_card.is_none());
        assert!(!app.has_pending_ask(ctx), "the seat's `!` clears with the ask");
        assert_eq!(app.status_model(0).pending_asks, 0);
        assert!(app.asks_set_aside.is_empty(), "set-aside ids are pruned to the open set");
    }

    #[test]
    fn a_card_answered_here_raises_no_notice_when_its_ask_closes() {
        let ctx = ContextId::new();
        let mut app = App::new("amy");
        app.current = Some(ctx);
        fold_ledger(&mut app, &state(&[summary("a-1", ctx, "rm")]));
        assert!(open_card(&mut app, detail("a-1", ctx, "rm")));
        // The card's own key takes it down and sets the ask aside, so it
        // does not come back before the push closes it.
        app.ask_card = None;
        app.asks_set_aside.insert("a-1".into());
        assert_eq!(card_candidate(&app), None);
        assert_eq!(fold_ledger(&mut app, &state(&[])).unwrap().answered, None);
    }

    /// An answer goes through the ledger and nowhere else, and the notice
    /// says what the ledger did with it: a lost race is ordinary, not an
    /// error (`docs/tui.md`, "Asks").
    #[test]
    fn a_decision_notice_reports_the_answer_the_race_and_the_refusal() {
        use kaijutsu_types::{AskAnswerFailure, AskAnswerFailureKind, AskAnswered, AskStatus, RememberResult};
        let ctx = ContextId::new();
        let mut allowed = summary("01a04eb6-77", ctx, "rm");
        allowed.status = AskStatus::Allowed;
        let answered = AskAnswered { summary: allowed.clone(), remembered: None };
        assert_eq!(decision_notice("01a04eb6-77", Ok(Ok(answered))), "ask 01a04eb6 allowed");
        let learned = AskAnswered {
            summary: allowed,
            remembered: Some(RememberResult {
                learned: false,
                note: "NOT remembered: no family for this command — the allow on THIS ask still stands".into(),
            }),
        };
        assert_eq!(
            decision_notice("01a04eb6-77", Ok(Ok(learned))),
            "ask 01a04eb6 allowed; NOT remembered: no family for this command — the allow on THIS ask still stands"
        );
        let raced = AskAnswerFailure { kind: AskAnswerFailureKind::AlreadyAnswered, message: "claimed".into() };
        assert_eq!(decision_notice("01a04eb6-77", Ok(Err(raced))), "ask 01a04eb6 was already answered");
        let refused = AskAnswerFailure { kind: AskAnswerFailureKind::NotReviewer, message: "not the reviewer".into() };
        assert_eq!(decision_notice("01a04eb6-77", Ok(Err(refused))), "ask 01a04eb6 not answered: not the reviewer");
    }

    #[test]
    fn a_card_read_that_finds_nothing_is_not_read_again_until_the_next_fold() {
        let ctx = ContextId::new();
        let mut app = App::new("amy");
        app.current = Some(ctx);
        fold_ledger(&mut app, &state(&[summary("a-1", ctx, "rm")]));
        land_card_read(&mut app, "a-1", Ok(None));
        assert_eq!(card_candidate(&app), None, "a stale open set does not spin the read");
        land_card_read(&mut app, "a-1", Err(CallError::PermanentlyFailed("gone".into())));
        assert_eq!(card_candidate(&app), None);
        fold_ledger(&mut app, &state(&[summary("a-1", ctx, "rm")]));
        assert_eq!(card_candidate(&app).as_deref(), Some("a-1"));
        land_card_read(&mut app, "a-1", Ok(Some(detail("a-1", ctx, "rm"))));
        assert!(app.ask_card.is_some());
    }

    #[test]
    fn the_ledger_lists_an_ask_answered_mid_read_once_as_answered() {
        let ctx = ContextId::new();
        let mut app = App::new("amy");
        let open = detail("o-1", ctx, "ls");
        let mut done = detail("a-1", ctx, "rm");
        done.summary.status = kaijutsu_types::AskStatus::Allowed;
        let reads = vec![
            ("a-1".to_string(), Ok(Some(done.clone()))),
            ("o-1".to_string(), Ok(Some(open))),
            ("a-1".to_string(), Ok(Some(done))),
        ];
        land_ledger(&mut app, Ok(reads), 0);
        let rows = &app.ledger_view.as_ref().unwrap().rows;
        let shape: Vec<(bool, &str)> =
            rows.iter().map(|row| (matches!(row, LedgerRow::Pending(_)), row.request_id())).collect();
        assert_eq!(shape, vec![(true, "o-1"), (false, "a-1")]);
    }

    #[test]
    fn notice_wording_names_the_decider_or_the_outcome_alone() {
        let ctx = ContextId::new();
        let me = PrincipalId::new();
        let other = PrincipalId::new();
        let mut decided = detail("01a04eb6-77", ctx, "rm");
        decided.summary.status = kaijutsu_types::AskStatus::Denied;
        decided.decision = Some(Decision {
            decided_by: reviewer(other, "lead"),
            option: Some("deny".into()),
            remember_scope: None,
            auto_reason: None,
        });
        assert_eq!(answered_notice(Some(me), "01a04eb6-77", Some(&decided)), "ask 01a04eb6 deny by lead");
        let mut mine = decided.clone();
        mine.decision = Some(Decision { decided_by: reviewer(me, "amy"), option: Some("allow_once".into()),
            remember_scope: None, auto_reason: None });
        assert_eq!(answered_notice(Some(me), "01a04eb6-77", Some(&mine)), "ask 01a04eb6 allow once by you");
        let mut expired = detail("01a04eb6-77", ctx, "rm");
        expired.summary.status = kaijutsu_types::AskStatus::Expired;
        assert_eq!(answered_notice(None, "01a04eb6-77", Some(&expired)), "ask 01a04eb6 expired");
        assert_eq!(answered_notice(None, "01a04eb6-77", None), "ask 01a04eb6 no longer pending");
    }

    #[test]
    fn a_remembered_rule_and_a_refused_rule_are_told_apart() {
        use kaijutsu_types::{AskAnswered, AskStatus, RememberResult};
        let ctx = ContextId::new();
        let mut allowed = summary("01a04eb6-77", ctx, "rm");
        allowed.status = AskStatus::Allowed;
        let said = |learned: bool, note: &str| {
            let answered = AskAnswered {
                summary: allowed.clone(),
                remembered: Some(RememberResult { learned, note: note.into() }),
            };
            decision_notice("01a04eb6-77", Ok(Ok(answered)))
        };
        // The kernel's own note shapes (`kj/ledger.rs`, `learn_for_answer`).
        let learned = "remembered as a standing character rule (1 statement)";
        assert_eq!(said(true, learned), format!("ask 01a04eb6 allowed; {learned}"));
        let refused = "NOT remembered: no family — the allow on THIS ask still stands";
        assert_eq!(said(false, refused), format!("ask 01a04eb6 allowed; {refused}"));
    }

    #[test]
    fn escalation_notices_name_the_new_reviewer_or_the_kernels_refusal() {
        use kaijutsu_types::{AskAnswered, AskAnswerFailure, AskAnswerFailureKind};
        let ctx = ContextId::new();
        let mut moved = summary("01a04eb6-77", ctx, "rm");
        moved.reviewer = reviewer(PrincipalId::new(), "lead");
        let answered = AskAnswered { summary: moved, remembered: None };
        assert_eq!(escalation_notice("01a04eb6-77", Ok(Ok(answered)), false), "ask 01a04eb6 now reviewed by lead");
        let refused = AskAnswerFailure { kind: AskAnswerFailureKind::Refused, message: "ask is claimed".into() };
        assert_eq!(escalation_notice("01a04eb6-77", Ok(Err(refused.clone())), false), "ask 01a04eb6 not escalated: ask is claimed");
        assert_eq!(escalation_notice("01a04eb6-77", Ok(Err(refused)), true), "ask 01a04eb6 not taken over: ask is claimed");
        assert_eq!(
            escalation_notice("01a04eb6-77", Err(CallError::PermanentlyFailed("pipe closed".into())), false),
            "cannot escalate ask 01a04eb6: permanently failed: pipe closed"
        );
    }

    /// Visible equals keyed: opening the ledger takes the card down and
    /// sets its ask aside, and no card comes up under an open ledger.
    #[test]
    fn opening_the_ledger_takes_the_card_down() {
        let (mut app, _) = app_with_program_card(|_, _| {});
        let reads = vec![("a-1".to_string(), Ok(Some(detail("a-1", ContextId::new(), "rm"))))];
        land_ledger(&mut app, Ok(reads), 0);
        assert!(app.ledger_view.is_some());
        assert!(app.ask_card.is_none(), "the card is not drawn under the ledger");
        assert!(app.asks_set_aside.contains("a-1"));
        app.asks_set_aside.clear();
        assert_eq!(card_candidate(&app), None, "no card rises under an open ledger");
    }
}
