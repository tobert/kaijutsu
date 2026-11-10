//! Asks and the ledger view (`Ctrl+A l`) — an ask rendered as an overlay
//! and answered through `ActorHandle::decide_ask`.
//!
//! Pure: every render fn takes rows and a width and returns `Line`s, every
//! key fn takes a `KeyEvent` and returns an action, and [`fold_ledger`]
//! takes a [`kaijutsu_client::LedgerState`] — no RPC, no I/O, no clock. The open asks come
//! from the actor's ledger watch ([`kaijutsu_client::ledger`]); the calls
//! that read and answer an ask, and wiring them to this module, are
//! [`crate::run`]'s.
//! Spec: `docs/tui.md`, "Asks" and "The ledger (`Ctrl+A l`, proposed chord)".

mod fold;
mod keys;
mod notify;
mod render;
mod state;

#[cfg(test)]
pub(crate) mod fixtures;

pub use fold::{
    LedgerFold, LedgerReads, LEDGER_HISTORY_LIMIT, answered_notice, card_candidate, context_facts, decision_notice,
    decision_words, escalation_notice, fold_ledger, land_card_read, land_decision, land_escalation, land_ledger,
    land_show, open_card, request_ledger, short_ask,
};
pub use keys::{
    AskCardKey, AskDecision, CardRoute, LedgerAction, ask_card_key, ask_key_to_decision, card_key, ledger_key,
    ledger_key_to_action, route_under_card, verdict_for,
};
pub use notify::{NotifySeen, ask_notification, notify_target};
pub use render::{
    ARMING_KEY_LINE, AskCard, AskDetailView, active_view_lines, render_ask_card, render_ask_detail, render_ledger,
};
pub use state::{
    AnsweredRow, AskCardState, AskWork, CardArming, LedgerRow, LedgerViewState, Offer, PendingRow, RedeemedMark,
    filtered_rows,
};
