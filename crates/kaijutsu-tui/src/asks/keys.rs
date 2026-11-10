//! Key routing: which key goes to the card, the ledger view or the draft, and
//! what each key does to the app.

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use kaijutsu_types::{AskVerdict, Remember, RememberScope};

use crate::app::App;
use crate::keys::{Intent, Keys};

use super::fold::{request_ledger, short_ask};
use super::state::{AskWork, LedgerViewState, Offer, filtered_rows};

/// Where a key goes while an ask card is up.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CardRoute {
    /// One of the card's own keys: `a`/`A`/`d`/`v` decide, `Esc` sets aside.
    Card(AskCardKey),
    /// A chord or control key the card does not own — a `Ctrl+A` chord,
    /// `Ctrl+C`, `Ctrl+Z` — acted on as if no card were up.
    Chord(Intent),
    /// Compose text. The card holds it: the draft never changes under a
    /// card, so a decision key and a typed letter are never confused.
    Held,
}

/// Route one key while an ask card is up. The card's own keys win unless
/// the `Ctrl+A` prefix is armed, in which case the whole chord belongs to
/// the prefix table — `Ctrl+A d` is the chord `d`, never a deny.
pub fn route_under_card(key: KeyEvent, keys: &mut Keys) -> CardRoute {
    if !keys.armed()
        && let Some(card_key) = ask_card_key(key)
    {
        return CardRoute::Card(card_key);
    }
    match keys.interpret(key) {
        Intent::InputKey(_) | Intent::Tab => CardRoute::Held,
        intent => CardRoute::Chord(intent),
    }
}

/// What a decided ask's own key answers, on the ask card and on a selected
/// ledger row alike.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AskDecision {
    AllowOnce,
    /// `A`: allow and remember the exact text for anyone, in any context.
    AllowAlways,
    /// `c`: allow and remember the exact text for the performing character.
    AllowForCharacter,
    /// `C`: allow and remember the command family for the performing
    /// character.
    AllowForCharacterFamily,
    Deny,
    ViewLedger,
    /// `e`: hand the ask to the next reviewer above.
    Escalate,
    /// `t`: take the ask for yourself.
    TakeOver,
}

/// What one key does on a live ask card: decide the ask, or put the card
/// aside with the ask still pending in the ledger.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AskCardKey {
    Decide(AskDecision),
    /// `Esc`: the card comes down, the ask stays pending — the seat keeps
    /// its `!` and `Ctrl+A l` reaches it.
    Aside,
}

/// `a`/`A`/`d`/`v`/`Esc` on a live ask card (`docs/tui.md`'s Asks figure).
/// `None` for anything else.
pub fn ask_card_key(key: KeyEvent) -> Option<AskCardKey> {
    if let Some(decision) = ask_key_to_decision(key) {
        return Some(AskCardKey::Decide(decision));
    }
    if key.kind == KeyEventKind::Release {
        return None;
    }
    (key.code == KeyCode::Esc && key.modifiers.is_empty()).then_some(AskCardKey::Aside)
}

/// `a`/`A`/`c`/`C`/`d`/`e`/`t`/`v` on a live ask card (`docs/tui.md`'s Asks figure). `None`
/// for anything else, including a key-release event — a card key is never
/// swallowed by acting on both edges of one press.
pub fn ask_key_to_decision(key: KeyEvent) -> Option<AskDecision> {
    if key.kind == KeyEventKind::Release {
        return None;
    }
    if key.modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) {
        return None;
    }
    match key.code {
        KeyCode::Char('a') => Some(AskDecision::AllowOnce),
        KeyCode::Char('A') => Some(AskDecision::AllowAlways),
        KeyCode::Char('c') => Some(AskDecision::AllowForCharacter),
        KeyCode::Char('C') => Some(AskDecision::AllowForCharacterFamily),
        KeyCode::Char('d') => Some(AskDecision::Deny),
        KeyCode::Char('e') => Some(AskDecision::Escalate),
        KeyCode::Char('t') => Some(AskDecision::TakeOver),
        KeyCode::Char('v') => Some(AskDecision::ViewLedger),
        _ => None,
    }
}

/// What one key does inside the ledger view (`Ctrl+A l`). Mode-dependent:
/// while `filtering`, every printable key edits the filter text instead of
/// answering a row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LedgerAction {
    AllowOnce,
    AllowAlways,
    AllowForCharacter,
    AllowForCharacterFamily,
    Deny,
    Escalate,
    TakeOver,
    Show,
    Up,
    Down,
    StartFilter,
    FilterInsert(char),
    FilterBackspace,
    CommitFilter,
    CancelFilter,
    Back,
    Ignored,
}

/// Interpret one key inside the ledger view. `docs/tui.md`'s ledger key
/// line, plus `/` to enter filter-typing mode and `Esc`/`Enter` to leave it.
pub fn ledger_key_to_action(key: KeyEvent, filtering: bool) -> LedgerAction {
    if key.kind == KeyEventKind::Release {
        return LedgerAction::Ignored;
    }
    // Every ledger key is bare; a Ctrl or Alt chord is never a decision.
    if key.modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) {
        return LedgerAction::Ignored;
    }
    if filtering {
        return match key.code {
            KeyCode::Esc => LedgerAction::CancelFilter,
            KeyCode::Enter => LedgerAction::CommitFilter,
            KeyCode::Backspace => LedgerAction::FilterBackspace,
            KeyCode::Char(c) => LedgerAction::FilterInsert(c),
            _ => LedgerAction::Ignored,
        };
    }
    match key.code {
        KeyCode::Char('a') => LedgerAction::AllowOnce,
        KeyCode::Char('A') => LedgerAction::AllowAlways,
        KeyCode::Char('c') => LedgerAction::AllowForCharacter,
        KeyCode::Char('C') => LedgerAction::AllowForCharacterFamily,
        KeyCode::Char('d') => LedgerAction::Deny,
        KeyCode::Char('e') => LedgerAction::Escalate,
        KeyCode::Char('t') => LedgerAction::TakeOver,
        KeyCode::Enter => LedgerAction::Show,
        KeyCode::Char('j') | KeyCode::Down => LedgerAction::Down,
        KeyCode::Char('k') | KeyCode::Up => LedgerAction::Up,
        KeyCode::Char('/') => LedgerAction::StartFilter,
        KeyCode::Esc => LedgerAction::Back,
        _ => LedgerAction::Ignored,
    }
}

/// The verdict and standing rule a decision key asks for. `None` for the
/// keys that answer nothing: `v`, `e` and `t`.
pub fn verdict_for(decision: AskDecision) -> Option<(AskVerdict, Option<Remember>)> {
    match decision {
        AskDecision::AllowOnce => Some((AskVerdict::Allow, None)),
        AskDecision::AllowAlways => {
            Some((AskVerdict::Allow, Some(Remember { scope: RememberScope::Always, family: false })))
        }
        AskDecision::AllowForCharacter => {
            Some((AskVerdict::Allow, Some(Remember { scope: RememberScope::Character, family: false })))
        }
        AskDecision::AllowForCharacterFamily => {
            Some((AskVerdict::Allow, Some(Remember { scope: RememberScope::Character, family: true })))
        }
        AskDecision::Deny => Some((AskVerdict::Deny, None)),
        AskDecision::ViewLedger | AskDecision::Escalate | AskDecision::TakeOver => None,
    }
}

/// One of the armed card's own keys. Every key that takes the card down
/// sets its ask aside, so [`super::card_candidate`] does not raise it again while
/// it stays open; a failed answer clears that ([`super::land_decision`]).
pub fn card_key(app: &mut App, key: AskCardKey) {
    let decision = match key {
        AskCardKey::Aside => {
            let Some(card) = app.ask_card.take() else { return };
            app.note(format!("ask {} set aside, still pending (Ctrl+A l)", short_ask(&card.request_id)));
            app.asks_set_aside.insert(card.request_id);
            return;
        }
        AskCardKey::Decide(decision) => decision,
    };
    let Some(card) = app.ask_card.as_ref() else { return };
    let request_id = card.request_id.clone();
    let offer = Offer::of(&card.detail.summary, app.principal);
    if decision == AskDecision::ViewLedger {
        app.ask_card = None;
        app.asks_set_aside.insert(request_id);
        request_ledger(app);
        return;
    }
    if let Some(notice) = offer.refusal(decision, &request_id) {
        app.note(notice);
        return;
    }
    // A reassignment leaves the card up: the push shows the new reviewer on
    // it. One is in flight at a time, so a double tap cannot append a second
    // `escalated` event.
    if matches!(decision, AskDecision::Escalate | AskDecision::TakeOver) {
        if card.reassigning {
            app.note(format!("ask {} is already being reassigned", short_ask(&request_id)));
            return;
        }
        if let Some(card) = app.ask_card.as_mut() {
            card.reassigning = true;
        }
    }
    if dispatch(app, decision, request_id.clone()) {
        return;
    }
    app.ask_card = None;
    app.asks_set_aside.insert(request_id);
}

/// Record the kernel work a permitted key asks for. True when it was a
/// reassignment, whose ask is not set aside.
fn dispatch(app: &mut App, decision: AskDecision, request_id: String) -> bool {
    match decision {
        AskDecision::Escalate => {
            app.ask_work.push(AskWork::Escalate { request_id, to: None });
            true
        }
        AskDecision::TakeOver => {
            let me = app.principal.expect("a permitted take over has a principal");
            app.ask_work.push(AskWork::Escalate { request_id, to: Some(me) });
            true
        }
        _ => {
            let (verdict, remember) = verdict_for(decision).expect("an answer key carries a verdict");
            app.ask_work.push(AskWork::Decide { request_id, verdict, remember });
            false
        }
    }
}

/// Clamp the ledger view's cursor into the rows its filter admits.
fn clamp_selected(view: &mut LedgerViewState) {
    view.selected = view.selected.min(view.visible_len().saturating_sub(1));
}

/// One key inside the ledger view: navigate, filter, show, answer or
/// reassign the selected row. An answer or reassignment closes the view; the ledger push updates the
/// seat flags and the pending count.
pub fn ledger_key(app: &mut App, key: KeyEvent) {
    if app.ledger_view.as_ref().is_some_and(|view| view.detail.is_some()) {
        if key.code == KeyCode::Esc && key.modifiers.is_empty()
            && let Some(view) = app.ledger_view.as_mut()
        {
            view.detail = None;
        }
        return;
    }
    let Some(view) = app.ledger_view.as_mut() else { return };
    let filtering = view.filtering;
    let action = ledger_key_to_action(key, filtering);
    let decision = match action {
        LedgerAction::Back => {
            app.ledger_view = None;
            return;
        }
        LedgerAction::Up => {
            view.move_up();
            return;
        }
        LedgerAction::Down => {
            view.move_down();
            return;
        }
        LedgerAction::StartFilter => {
            view.filtering = true;
            return;
        }
        LedgerAction::FilterInsert(c) => {
            view.filter.push(c);
            clamp_selected(view);
            return;
        }
        LedgerAction::FilterBackspace => {
            view.filter.pop();
            clamp_selected(view);
            return;
        }
        LedgerAction::CommitFilter | LedgerAction::CancelFilter => {
            view.filtering = false;
            return;
        }
        LedgerAction::Show => {
            if let Some(request_id) = view.selected_request_id() {
                app.ask_work.push(AskWork::Show(request_id));
            }
            return;
        }
        LedgerAction::AllowOnce => AskDecision::AllowOnce,
        LedgerAction::AllowAlways => AskDecision::AllowAlways,
        LedgerAction::AllowForCharacter => AskDecision::AllowForCharacter,
        LedgerAction::AllowForCharacterFamily => AskDecision::AllowForCharacterFamily,
        LedgerAction::Deny => AskDecision::Deny,
        LedgerAction::Escalate => AskDecision::Escalate,
        LedgerAction::TakeOver => AskDecision::TakeOver,
        LedgerAction::Ignored => return,
    };
    let Some(request_id) = view.selected_request_id() else { return };
    let offer = filtered_rows(&view.rows, &view.filter).get(view.selected).map(|row| row.offer()).unwrap_or_default();
    if let Some(notice) = offer.refusal(decision, &request_id) {
        app.note(notice);
        return;
    }
    app.ledger_view = None;
    if !dispatch(app, decision, request_id.clone()) {
        app.asks_set_aside.insert(request_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::asks::fixtures::{app_with_card, app_with_program_card, reviewer, row_with};
    use crate::asks::{AskWork, LedgerRow, LedgerViewState, PendingRow, card_candidate, land_decision, land_escalation};
    use kaijutsu_types::PrincipalId;
    use kaijutsu_client::actor::CallError;

    fn press(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn ask_keys_map_to_decisions() {
        assert_eq!(ask_key_to_decision(press(KeyCode::Char('a'))), Some(AskDecision::AllowOnce));
        assert_eq!(ask_key_to_decision(press(KeyCode::Char('A'))), Some(AskDecision::AllowAlways));
        assert_eq!(ask_key_to_decision(press(KeyCode::Char('d'))), Some(AskDecision::Deny));
        assert_eq!(ask_key_to_decision(press(KeyCode::Char('v'))), Some(AskDecision::ViewLedger));
        assert_eq!(ask_key_to_decision(press(KeyCode::Char('c'))), Some(AskDecision::AllowForCharacter));
        assert_eq!(ask_key_to_decision(press(KeyCode::Char('C'))), Some(AskDecision::AllowForCharacterFamily));
        assert_eq!(ask_key_to_decision(press(KeyCode::Char('e'))), Some(AskDecision::Escalate));
        assert_eq!(ask_key_to_decision(press(KeyCode::Char('t'))), Some(AskDecision::TakeOver));
        assert_eq!(ask_key_to_decision(press(KeyCode::Char('x'))), None);
        for c in ['c', 'C', 'e', 't'] {
            let chord = KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL);
            assert_eq!(ask_key_to_decision(chord), None, "Ctrl+{c} is never a card key");
        }
    }

    /// `Ctrl+A` is the prefix and `Ctrl+D` is a vi scroll; neither answers
    /// the selected ask.
    #[test]
    fn a_ctrl_chord_is_never_a_ledger_decision() {
        for c in ['a', 'd', 'A', 'c', 'C', 'e', 't'] {
            let chord = KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL);
            assert_eq!(ledger_key_to_action(chord, false), LedgerAction::Ignored);
            let alt = KeyEvent::new(KeyCode::Char(c), KeyModifiers::ALT);
            assert_eq!(ledger_key_to_action(alt, false), LedgerAction::Ignored);
        }
    }

    #[test]
    fn a_ctrl_chord_is_never_an_ask_decision() {
        let ctrl_a = KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL);
        assert_eq!(ask_key_to_decision(ctrl_a), None);
    }

    /// `Esc` puts the card aside; a decision key still decides; any other
    /// key is still swallowed by the card rather than reaching compose.
    #[test]
    fn esc_puts_the_ask_card_aside() {
        assert_eq!(ask_card_key(press(KeyCode::Esc)), Some(AskCardKey::Aside));
        assert_eq!(ask_card_key(press(KeyCode::Char('d'))), Some(AskCardKey::Decide(AskDecision::Deny)));
        assert_eq!(ask_card_key(press(KeyCode::Char('x'))), None);
        let released = KeyEvent { kind: KeyEventKind::Release, ..press(KeyCode::Esc) };
        assert_eq!(ask_card_key(released), None);
    }

    #[test]
    fn ledger_keys_answer_a_selected_row() {
        assert_eq!(ledger_key_to_action(press(KeyCode::Char('a')), false), LedgerAction::AllowOnce);
        assert_eq!(ledger_key_to_action(press(KeyCode::Char('A')), false), LedgerAction::AllowAlways);
        assert_eq!(ledger_key_to_action(press(KeyCode::Char('d')), false), LedgerAction::Deny);
        assert_eq!(ledger_key_to_action(press(KeyCode::Enter), false), LedgerAction::Show);
        assert_eq!(ledger_key_to_action(press(KeyCode::Char('j')), false), LedgerAction::Down);
        assert_eq!(ledger_key_to_action(press(KeyCode::Char('k')), false), LedgerAction::Up);
        assert_eq!(ledger_key_to_action(press(KeyCode::Esc), false), LedgerAction::Back);
    }

    #[test]
    fn slash_enters_filter_mode_and_typed_chars_edit_it_not_answer_a_row() {
        assert_eq!(ledger_key_to_action(press(KeyCode::Char('/')), false), LedgerAction::StartFilter);
        assert_eq!(
            ledger_key_to_action(press(KeyCode::Char('a')), true),
            LedgerAction::FilterInsert('a'),
            "while filtering, 'a' types into the filter, it does not allow a row"
        );
        assert_eq!(ledger_key_to_action(press(KeyCode::Backspace), true), LedgerAction::FilterBackspace);
        assert_eq!(ledger_key_to_action(press(KeyCode::Enter), true), LedgerAction::CommitFilter);
        assert_eq!(ledger_key_to_action(press(KeyCode::Esc), true), LedgerAction::CancelFilter);
    }

    #[test]
    fn allow_always_asks_for_a_standing_rule_and_view_answers_nothing() {
        use kaijutsu_types::{AskVerdict, Remember, RememberScope};
        assert_eq!(verdict_for(AskDecision::AllowOnce), Some((AskVerdict::Allow, None)));
        assert_eq!(
            verdict_for(AskDecision::AllowAlways),
            Some((AskVerdict::Allow, Some(Remember { scope: RememberScope::Always, family: false })))
        );
        assert_eq!(verdict_for(AskDecision::Deny), Some((AskVerdict::Deny, None)));
        assert_eq!(verdict_for(AskDecision::ViewLedger), None);
    }

    #[test]
    fn esc_on_the_card_sets_its_ask_aside_and_lets_the_next_one_up() {
        let (mut app, _ctx) = app_with_card(&["a-1", "b-2"]);
        card_key(&mut app, AskCardKey::Aside);
        assert!(app.ask_card.is_none());
        assert!(app.asks_set_aside.contains("a-1"));
        assert_eq!(card_candidate(&app).as_deref(), Some("b-2"), "the set-aside card does not come straight back");
        assert!(app.ask_work.is_empty(), "Esc asks the kernel for nothing");
    }

    #[test]
    fn a_decision_key_records_the_answer_and_v_records_the_ledger() {
        let (mut app, _ctx) = app_with_card(&["a-1"]);
        card_key(&mut app, AskCardKey::Decide(AskDecision::AllowAlways));
        assert_eq!(card_candidate(&app), None, "an answered card waits for the push to close it");
        let (verdict, remember) = verdict_for(AskDecision::AllowAlways).unwrap();
        assert_eq!(app.ask_work, vec![AskWork::Decide { request_id: "a-1".into(), verdict, remember }]);

        let (mut app, _ctx) = app_with_card(&["a-1"]);
        card_key(&mut app, AskCardKey::Decide(AskDecision::ViewLedger));
        assert_eq!(card_candidate(&app), None);
        assert_eq!(app.ask_work, vec![AskWork::OpenLedger { open: vec!["a-1".into()] }]);
    }

    #[test]
    fn a_card_the_viewer_cannot_answer_stays_up_and_records_nothing() {
        let (mut app, _ctx) = app_with_card(&["a-1"]);
        app.principal = Some(PrincipalId::new());
        card_key(&mut app, AskCardKey::Decide(AskDecision::AllowOnce));
        assert_eq!(app.ask_card.as_ref().map(|c| c.request_id.as_str()), Some("a-1"));
        assert!(app.ask_work.is_empty());
        assert!(!app.asks_set_aside.contains("a-1"));
    }

    #[test]
    fn an_answer_that_did_not_land_puts_its_card_back_and_a_lost_race_does_not() {
        use kaijutsu_types::{AskAnswerFailure, AskAnswerFailureKind};
        let (mut app, _ctx) = app_with_card(&["a-1"]);
        card_key(&mut app, AskCardKey::Decide(AskDecision::Deny));
        land_decision(&mut app, "a-1", Err(CallError::PermanentlyFailed("pipe closed".into())));
        assert_eq!(app.notice(), Some("cannot answer ask a: permanently failed: pipe closed"));
        assert_eq!(card_candidate(&app).as_deref(), Some("a-1"), "the ask is still open and answerable");

        card_key(&mut app, AskCardKey::Aside);
        app.asks_set_aside.insert("a-1".into());
        let refused = AskAnswerFailure { kind: AskAnswerFailureKind::Archived, message: "archived".into() };
        land_decision(&mut app, "a-1", Ok(Err(refused)));
        assert_eq!(card_candidate(&app).as_deref(), Some("a-1"));

        app.asks_set_aside.insert("a-1".into());
        let raced = AskAnswerFailure { kind: AskAnswerFailureKind::AlreadyAnswered, message: "claimed".into() };
        land_decision(&mut app, "a-1", Ok(Err(raced)));
        assert_eq!(card_candidate(&app), None, "the push is about to close it");
    }

    fn ledger_with(ids: &[&str]) -> App {
        let mut app = App::new("amy");
        app.ledger_view = Some(LedgerViewState {
            rows: ids
                .iter()
                .map(|id| {
                    LedgerRow::Pending(PendingRow {
                        request_id: id.to_string(),
                        age: None,
                        context_label: "kaijutsu".into(),
                        performer: "coder".into(),
                        hook: "shell_write".into(),
                        asker: None,
                        reviewer: None,
                        offer: Offer { review: true, ..Default::default() },
                        statement: format!("echo {id}"),
                    })
                })
                .collect(),
            ..Default::default()
        });
        app
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn typing_a_filter_keeps_the_cursor_on_a_row_it_admits() {
        let mut app = ledger_with(&["alpha", "beta", "gamma"]);
        ledger_key(&mut app, key(KeyCode::Char('j')));
        ledger_key(&mut app, key(KeyCode::Char('j')));
        ledger_key(&mut app, key(KeyCode::Char('/')));
        for c in "alpha".chars() {
            ledger_key(&mut app, key(KeyCode::Char(c)));
        }
        let view = app.ledger_view.as_ref().unwrap();
        assert_eq!(view.selected_request_id().as_deref(), Some("alpha"), "the cursor follows the filter");
        ledger_key(&mut app, key(KeyCode::Enter));
        ledger_key(&mut app, key(KeyCode::Enter));
        assert_eq!(app.ask_work, vec![AskWork::Show("alpha".into())]);
        ledger_key(&mut app, key(KeyCode::Char('d')));
        assert!(app.ledger_view.is_none(), "an answer closes the view");
        assert!(matches!(app.ask_work.last(), Some(AskWork::Decide { request_id, .. }) if request_id == "alpha"));
    }

    #[test]
    fn the_character_keys_ask_for_a_rule_for_the_performer() {
        use kaijutsu_types::{Remember, RememberScope};
        assert_eq!(
            verdict_for(AskDecision::AllowForCharacter),
            Some((AskVerdict::Allow, Some(Remember { scope: RememberScope::Character, family: false })))
        );
        assert_eq!(
            verdict_for(AskDecision::AllowForCharacterFamily),
            Some((AskVerdict::Allow, Some(Remember { scope: RememberScope::Character, family: true })))
        );
        assert_eq!(verdict_for(AskDecision::Escalate), None);
        assert_eq!(verdict_for(AskDecision::TakeOver), None);
    }

    #[test]
    fn character_keys_work_only_when_the_ask_carries_its_program() {
        let (mut app, _) = app_with_program_card(|_, _| {});
        card_key(&mut app, AskCardKey::Decide(AskDecision::AllowForCharacterFamily));
        let (verdict, remember) = verdict_for(AskDecision::AllowForCharacterFamily).unwrap();
        assert_eq!(app.ask_work, vec![AskWork::Decide { request_id: "a-1".into(), verdict, remember }]);

        let (mut app, _) = app_with_program_card(|ask, _| ask.has_exec_source = false);
        card_key(&mut app, AskCardKey::Decide(AskDecision::AllowForCharacter));
        assert!(app.ask_work.is_empty(), "no program, no rule: {:?}", app.ask_work);
        assert!(app.ask_card.is_some(), "the card stays up");
        assert_eq!(app.notice(), Some("ask a carries no shell program to remember for its performer"));
        card_key(&mut app, AskCardKey::Decide(AskDecision::AllowOnce));
        assert_eq!(app.ask_work.len(), 1, "the plain answer still works");
    }

    #[test]
    fn escalate_goes_up_and_leaves_the_card_to_follow_the_push() {
        let (mut app, _) = app_with_program_card(|_, _| {});
        card_key(&mut app, AskCardKey::Decide(AskDecision::Escalate));
        assert_eq!(app.ask_work, vec![AskWork::Escalate { request_id: "a-1".into(), to: None }]);
        assert!(app.ask_card.is_some());
        assert!(!app.asks_set_aside.contains("a-1"));

        let (mut app, _) = app_with_program_card(|ask, _| ask.status = kaijutsu_types::AskStatus::Claimed);
        card_key(&mut app, AskCardKey::Decide(AskDecision::Escalate));
        assert!(app.ask_work.is_empty(), "the kernel refuses a claimed ask");
        assert_eq!(app.notice(), Some("ask a cannot be escalated by you; only its pending reviewer may"));
    }

    #[test]
    fn take_over_is_the_lineage_roots_and_names_itself() {
        let (mut app, me) = app_with_program_card(|ask, me| {
            ask.reviewer = reviewer(PrincipalId::new(), "lead");
            ask.lineage_root = reviewer(me, "amy");
        });
        card_key(&mut app, AskCardKey::Decide(AskDecision::TakeOver));
        assert_eq!(app.ask_work, vec![AskWork::Escalate { request_id: "a-1".into(), to: Some(me) }]);
        card_key(&mut app, AskCardKey::Decide(AskDecision::AllowOnce));
        assert_eq!(app.ask_work.len(), 1, "a root that is not the reviewer cannot answer");
        assert!(app.notice().is_some_and(|n| n.contains("awaits its assigned reviewer")));

        // The reviewer who is not the root cannot take; nor can a stranger.
        let (mut app, _) = app_with_program_card(|_, _| {});
        card_key(&mut app, AskCardKey::Decide(AskDecision::TakeOver));
        assert!(app.ask_work.is_empty());
        let (mut app, _) = app_with_program_card(|ask, _| ask.lineage_root = reviewer(PrincipalId::new(), "root"));
        card_key(&mut app, AskCardKey::Decide(AskDecision::TakeOver));
        assert!(app.ask_work.is_empty());
    }

    fn ledger_of(offer: Offer) -> App {
        let mut app = App::new("amy");
        app.ledger_view = Some(LedgerViewState { rows: vec![row_with("p-1", offer)], ..Default::default() });
        app
    }

    #[test]
    fn ledger_keys_follow_the_selected_rows_offer() {
        use kaijutsu_types::{Remember, RememberScope};
        let full = Offer { review: true, always: true, remember: true, escalate: true, take_over: false };
        let mut app = ledger_of(full);
        ledger_key(&mut app, key(KeyCode::Char('C')));
        assert!(app.ledger_view.is_none());
        assert_eq!(
            app.ask_work,
            vec![AskWork::Decide {
                request_id: "p-1".into(),
                verdict: AskVerdict::Allow,
                remember: Some(Remember { scope: RememberScope::Character, family: true }),
            }]
        );

        let mut app = ledger_of(full);
        ledger_key(&mut app, key(KeyCode::Char('e')));
        assert_eq!(app.ask_work, vec![AskWork::Escalate { request_id: "p-1".into(), to: None }]);

        let root = Offer { take_over: true, ..Offer::default() };
        let mut app = ledger_of(root);
        ledger_key(&mut app, key(KeyCode::Char('c')));
        ledger_key(&mut app, key(KeyCode::Char('e')));
        ledger_key(&mut app, key(KeyCode::Char('a')));
        assert!(app.ask_work.is_empty(), "a root that is not the reviewer holds only t: {:?}", app.ask_work);
        assert!(app.ledger_view.is_some(), "a refused key leaves the view up");
        // `t` names the viewer, who the ledger view does not know by itself.
        app.principal = Some(PrincipalId::new());
        let me = app.principal;
        ledger_key(&mut app, key(KeyCode::Char('t')));
        assert_eq!(app.ask_work, vec![AskWork::Escalate { request_id: "p-1".into(), to: me }]);

        let program_less = Offer { remember: false, ..full };
        let mut app = ledger_of(program_less);
        ledger_key(&mut app, key(KeyCode::Char('c')));
        assert!(app.ask_work.is_empty());
    }

    #[test]
    fn a_reassignment_in_flight_blocks_a_second_one_until_it_lands() {
        let (mut app, _) = app_with_program_card(|_, _| {});
        card_key(&mut app, AskCardKey::Decide(AskDecision::Escalate));
        card_key(&mut app, AskCardKey::Decide(AskDecision::Escalate));
        assert_eq!(app.ask_work.len(), 1, "a double tap sends one: {:?}", app.ask_work);
        assert_eq!(app.notice(), Some("ask a is already being reassigned"));
        land_escalation(&mut app, "a-1", Err(CallError::PermanentlyFailed("x".into())), false);
        card_key(&mut app, AskCardKey::Decide(AskDecision::Escalate));
        assert_eq!(app.ask_work.len(), 2, "after it lands the key works again");
    }
}

#[cfg(test)]
mod card_route_tests {
    use super::*;

    fn press(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn ctrl(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
    }

    /// The card's own keys decide or set aside; a typed letter is held.
    #[test]
    fn the_card_owns_its_keys_and_holds_text() {
        let mut keys = Keys::new();
        assert_eq!(
            route_under_card(press(KeyCode::Char('a')), &mut keys),
            CardRoute::Card(AskCardKey::Decide(AskDecision::AllowOnce))
        );
        assert_eq!(route_under_card(press(KeyCode::Esc), &mut keys), CardRoute::Card(AskCardKey::Aside));
        for (c, decision) in [
            ('c', AskDecision::AllowForCharacter),
            ('C', AskDecision::AllowForCharacterFamily),
            ('e', AskDecision::Escalate),
            ('t', AskDecision::TakeOver),
        ] {
            assert_eq!(
                route_under_card(press(KeyCode::Char(c)), &mut keys),
                CardRoute::Card(AskCardKey::Decide(decision)),
                "{c}"
            );
        }
        assert_eq!(route_under_card(press(KeyCode::Char('x')), &mut keys), CardRoute::Held);
        assert_eq!(route_under_card(press(KeyCode::Tab), &mut keys), CardRoute::Held);
    }

    /// `Ctrl+A 4` switches seats with a card up, exactly as it does without
    /// one — the prefix is the one key that works everywhere.
    #[test]
    fn a_seat_chord_acts_under_a_card() {
        let mut keys = Keys::new();
        assert_eq!(route_under_card(ctrl('a'), &mut keys), CardRoute::Chord(Intent::LegendChanged));
        assert_eq!(route_under_card(press(KeyCode::Char('4')), &mut keys), CardRoute::Chord(Intent::SwitchSeat(4)));
        assert_eq!(route_under_card(ctrl('a'), &mut keys), CardRoute::Chord(Intent::LegendChanged));
        assert_eq!(route_under_card(press(KeyCode::Char('"')), &mut keys), CardRoute::Chord(Intent::TogglePicker));
    }

    /// With the prefix armed, `d` is the chord `d`, not a deny.
    #[test]
    fn an_armed_prefix_takes_a_card_letter_as_its_chord() {
        let mut keys = Keys::new();
        route_under_card(ctrl('a'), &mut keys);
        assert!(matches!(
            route_under_card(press(KeyCode::Char('d')), &mut keys),
            CardRoute::Chord(Intent::NotYet(_))
        ));
        assert_eq!(
            route_under_card(press(KeyCode::Char('d')), &mut keys),
            CardRoute::Card(AskCardKey::Decide(AskDecision::Deny)),
            "the prefix is spent; the next d is the card's"
        );
    }

    /// `Ctrl+C` and `Ctrl+Z` are never held by a card.
    #[test]
    fn control_keys_act_under_a_card() {
        let mut keys = Keys::new();
        assert_eq!(route_under_card(ctrl('c'), &mut keys), CardRoute::Chord(Intent::Interrupt));
        assert_eq!(route_under_card(ctrl('z'), &mut keys), CardRoute::Chord(Intent::CtrlZ));
    }
}
