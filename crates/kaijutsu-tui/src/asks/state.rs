//! The asks' own state: the card, its arming, the ledger view's rows and
//! cursor, and the kernel work a key asks for.

use kaijutsu_types::{AskDetail, AskVerdict, ContextId, PrincipalId, Remember};

use super::fold::{awaits_reviewer, short_ask};
use super::keys::AskDecision;

/// Which keys a viewer may use on one ask, each from the shared
/// [`kaijutsu_types::AskSummary`] predicate of the same name; the client
/// derives no authorization of its own. The kernel stays authoritative if
/// the ask changes while a control is on screen.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Offer {
    /// `a` `d`: the viewer is the assigned reviewer.
    pub review: bool,
    /// `A`: a standing rule can be learned. A drift ask has none.
    pub always: bool,
    /// `c` `C`: a standing rule for the performer can be learned.
    pub remember: bool,
    /// `e`: the viewer may hand the ask on.
    pub escalate: bool,
    /// `t`: the viewer may take the ask.
    pub take_over: bool,
}

impl Offer {
    /// The keys `me` has on `summary`; none when the viewer is unknown.
    pub fn of(summary: &kaijutsu_types::AskSummary, me: Option<PrincipalId>) -> Self {
        let Some(me) = me else { return Self::default() };
        Self {
            review: summary.answerable_by(me),
            always: summary.can_remember(me),
            remember: summary.can_remember_for_character(me),
            escalate: summary.can_escalate(me),
            take_over: summary.can_take_over(me),
        }
    }

    /// The status-line notice for a key the viewer does not have on this
    /// ask, or `None` when `decision` is theirs to use.
    pub fn refusal(&self, decision: AskDecision, request_id: &str) -> Option<String> {
        let id = short_ask(request_id);
        match decision {
            AskDecision::ViewLedger => None,
            AskDecision::AllowOnce | AskDecision::AllowAlways | AskDecision::Deny if !self.review => {
                Some(awaits_reviewer(request_id))
            }
            AskDecision::AllowForCharacter | AskDecision::AllowForCharacterFamily if !self.review => {
                Some(awaits_reviewer(request_id))
            }
            AskDecision::AllowAlways if !self.always => {
                Some(format!("ask {id} is answered once; it cannot become a standing rule"))
            }
            AskDecision::AllowForCharacter | AskDecision::AllowForCharacterFamily if !self.remember => {
                Some(format!("ask {id} carries no shell program to remember for its performer"))
            }
            AskDecision::Escalate if !self.escalate => {
                Some(format!("ask {id} cannot be escalated by you; only its pending reviewer may"))
            }
            AskDecision::TakeOver if !self.take_over => {
                Some(format!("ask {id} cannot be taken over by you; only the lineage root may take a pending ask"))
            }
            _ => None,
        }
    }
}

/// One row of the ledger view's PENDING section.
pub struct PendingRow {
    pub request_id: String,
    /// Formatted age (`format_age` in `status.rs`'s convention) from
    /// `created_at`, or `None` when the ask has no stamp.
    pub age: Option<String>,
    pub context_label: String,
    pub performer: String,
    pub hook: String,
    pub asker: Option<String>,
    pub reviewer: Option<String>,
    /// The keys the viewer has on this ask.
    pub offer: Offer,
    pub statement: String,
}

/// One row of the ledger view's ANSWERED section.
pub struct AnsweredRow {
    pub request_id: String,
    /// Formatted decision time (`decided_at`), or `None` when the ask has
    /// no stamp.
    pub time: Option<String>,
    pub context_label: String,
    /// `"allow once"`, `"allow always"`, `"deny"` — or `None` when the wire
    /// carried only `status` (`allowed`/`denied`) and not `decided_option`.
    pub decision: Option<String>,
    /// Who decided it — `you`, a principal's short id, or `None` for a
    /// rule's auto-decision.
    pub principal: Option<String>,
    /// `redeemed <time>` / `redeemed ×N` / `—`, pre-formatted by the caller
    /// because a rule's redeem count is a different query than the ask's own
    /// `redeemed_at` and this module has no opinion on which a caller used.
    pub redeemed: RedeemedMark,
    pub statement: String,
}

/// How an answered row's redemption renders — `docs/tui.md`'s "was this
/// consumed" column.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RedeemedMark {
    /// This exact ask's decision was never redeemed.
    Never,
    /// This exact ask's decision was redeemed once, at this wallclock.
    At(String),
}

impl RedeemedMark {
    pub(super) fn text(&self) -> String {
        match self {
            Self::Never => "—".to_string(),
            Self::At(when) => format!("redeemed {when}"),
        }
    }
}

/// A pending or answered row, addressed by the ledger view's cursor
/// (`j`/`k`) — the request id is enough to route `Enter`/`a`/`A`/`d` back to
/// the right ask.
pub enum LedgerRow {
    Pending(PendingRow),
    Answered(AnsweredRow),
}

impl LedgerRow {
    pub fn request_id(&self) -> &str {
        match self {
            Self::Pending(r) => &r.request_id,
            Self::Answered(r) => &r.request_id,
        }
    }

    /// The keys the viewer has on this row; none on an answered row.
    pub fn offer(&self) -> Offer {
        match self {
            Self::Pending(row) => row.offer,
            Self::Answered(_) => Offer::default(),
        }
    }

    /// Substring match across every column a filter might target — the id,
    /// context, hook (pending only) and statement.
    fn matches(&self, needle: &str) -> bool {
        if needle.is_empty() {
            return true;
        }
        let needle = needle.to_ascii_lowercase();
        let hay = match self {
            Self::Pending(r) => format!(
                "{} {} {} {} {} {}",
                r.request_id,
                r.context_label,
                r.hook,
                r.asker.as_deref().unwrap_or(""),
                r.reviewer.as_deref().unwrap_or(""),
                r.statement
            ),
            Self::Answered(r) => format!(
                "{} {} {}",
                r.request_id, r.context_label, r.statement
            ),
        };
        hay.to_ascii_lowercase().contains(&needle)
    }
}

/// Every row a filter admits, PENDING first then ANSWERED, in the order the
/// caller supplied within each section (oldest pending first, newest
/// answered first), not re-sorted here.
pub fn filtered_rows<'a>(rows: &'a [LedgerRow], filter: &str) -> Vec<&'a LedgerRow> {
    rows.iter().filter(|r| r.matches(filter)).collect()
}

/// The ask card's live state: which ask is showing, and its full record
/// (`get_ask`). [`crate::run`] reads the record for [`super::card_candidate`] and
/// opens the card with [`super::open_card`]; [`super::fold_ledger`] keeps its summary
/// current and takes it down once the ask leaves the open set.
pub struct AskCardState {
    pub request_id: String,
    pub context_id: ContextId,
    pub detail: AskDetail,
    /// A reassignment (`e` or `t`) is in flight and has not landed.
    pub reassigning: bool,
}

/// The ask card's arming, kept beside the card for the ask it was started
/// for. `armed` is what the last frame drew, so a key acts on the card
/// only after the player has seen its keys (`docs/tui.md`, "Asks").
pub struct CardArming {
    pub request_id: String,
    clock: kaijutsu_client::AskArming<std::time::Instant>,
    armed: bool,
}

impl CardArming {
    /// A card for `request_id` first drawn at `now`, disarmed.
    pub fn shown(request_id: String, now: std::time::Instant) -> Self {
        Self { request_id, clock: kaijutsu_client::AskArming::shown(now), armed: false }
    }

    /// Whether the last frame drew the card armed.
    pub fn armed(&self) -> bool {
        self.armed
    }

    /// Arm if the hold has passed at `now`. True when that changed.
    pub fn tick(&mut self, now: std::time::Instant) -> bool {
        if !self.armed && self.clock.armed(now) {
            self.armed = true;
            return true;
        }
        false
    }

    /// A key that went past the disarmed card at `now`.
    pub fn keystroke(&mut self, now: std::time::Instant) {
        self.clock.keystroke(now);
    }
}

/// The ledger view's live state (`Ctrl+A l`): every row, the cursor, and
/// whether `/` has put it into filter-typing mode.
#[derive(Default)]
pub struct LedgerViewState {
    pub rows: Vec<LedgerRow>,
    pub filter: String,
    pub selected: usize,
    pub filtering: bool,
    /// The selected ask after `Enter`; `Esc` returns to its row list.
    pub detail: Option<AskDetail>,
}

impl LedgerViewState {
    /// How many rows the current filter admits — [`Self::selected`]'s valid
    /// range.
    pub fn visible_len(&self) -> usize {
        filtered_rows(&self.rows, &self.filter).len()
    }

    /// The request id `j`/`k`'s cursor is on, or `None` when the filter
    /// admits nothing.
    pub fn selected_request_id(&self) -> Option<String> {
        filtered_rows(&self.rows, &self.filter)
            .get(self.selected)
            .map(|r| r.request_id().to_string())
    }

    pub fn move_down(&mut self) {
        let len = self.visible_len();
        if len > 0 {
            self.selected = (self.selected + 1).min(len - 1);
        }
    }

    pub fn move_up(&mut self) {
        self.selected = self.selected.saturating_sub(1);
    }
}

/// Kernel work about asks that a key asked for ([`crate::app::App::ask_work`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AskWork {
    /// Read the ledger view's rows: the open asks named here, then the
    /// recent answers.
    OpenLedger { open: Vec<String> },
    /// Read one ask in full for the ledger view (`Enter`).
    Show(String),
    /// Answer one ask.
    Decide { request_id: String, verdict: AskVerdict, remember: Option<Remember> },
    /// Reassign one ask: `to: None` goes to the next reviewer above, and
    /// naming yourself takes it over.
    Escalate { request_id: String, to: Option<PrincipalId> },
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::asks::fixtures::{self, pending};

    /// Each viewer's card shows exactly the keys they have, in the words
    /// the design gives them.
    #[test]
    fn a_drift_ask_offers_no_standing_rule() {
        let me = PrincipalId::new();
        let mut summary = fixtures::summary("drift-1", ContextId::new(), "push drift from lane");
        summary.origin = kaijutsu_types::AskOrigin::Drift;
        summary.statements.clear();
        summary.reviewer = Some(kaijutsu_types::PrincipalRef { id: me, name: "amy".into() });
        summary.performer = Some(kaijutsu_types::PrincipalRef { id: PrincipalId::new(), name: "coder".into() });
        let offer = Offer::of(&summary, Some(me));
        assert!(offer.review && !offer.always && !offer.remember, "{offer:?}");
        assert!(offer.refusal(AskDecision::AllowAlways, "drift-1").is_some());
        assert!(offer.refusal(AskDecision::AllowOnce, "drift-1").is_none());
        assert!(offer.refusal(AskDecision::Deny, "drift-1").is_none());
    }

    #[test]
    fn a_filter_narrows_both_sections() {
        let rows = vec![pending("p1", "kaijutsu"), pending("p2", "scorer")];
        let visible = filtered_rows(&rows, "scorer");
        assert_eq!(visible.len(), 1);
        assert_eq!(visible[0].request_id(), "p2");
    }

    #[test]
    fn an_empty_filter_admits_everything() {
        let rows = vec![pending("p1", "kaijutsu"), pending("p2", "scorer")];
        assert_eq!(filtered_rows(&rows, "").len(), 2);
    }
}
