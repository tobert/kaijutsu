//! The ledger ribbon (`Ctrl+A l`) — every open ask at once, and the last
//! few decisions.
//!
//! The ask sheet answers the ask in front of you. This is the view you work
//! from between them: one row per open ask across every context, then the
//! decisions the mirror still holds, with the redemption that answers "was
//! this consumed" (`docs/tui.md`, "The ledger"). Same centered MSDF panel as
//! the sheet ([`ui::msdf_panel`](super::msdf_panel)), same single-key
//! answers, and `Enter` hands a row to the sheet for the whole story.
//!
//! **Rows truncate, never wrap.** A ledger row is a row: scanning a column
//! down the panel is the point, and a wrapped row would break the column.
//! The sheet is where the untruncated text lives.
//!
//! **It reads only the mirror.** `connection::ledger::LedgerMirror` is the
//! one place in the app that reads the ledger, so the ribbon, the sheet, the
//! switchboard lamps and the dock's `!n` cannot disagree about what is open.
//! The mirror lists the last [`RECENT_CAP`] answered asks, as the TUI's
//! ledger does, which is why the header says `answered n` and not "answered
//! today": nothing on the wire tells this app how many asks were answered
//! today, and a number that looked like it did would be an invention.
//!
//! **One list, one cursor.** PENDING and ANSWERED rows are one list the
//! cursor walks, narrowed by the `/` filter. The cursor follows its ask by
//! request id when the list shifts under it, so a key never lands on a
//! different ask than the one the player was looking at.

use bevy::input::keyboard::KeyboardInput;
use bevy::prelude::*;
use kaijutsu_types::{AskDetail, ContextId, PrincipalId};

use crate::connection::ledger::{
    short_request_id, AskDecisionRequested, AskReassignRequested, LedgerMirror, RECENT_CAP,
};
use crate::input::events::ActionFired;
use crate::input::{Action, InputContext};
use crate::shaders::BlockFxMaterial;
use crate::text::msdf::MsdfBlockGlyphs;
use crate::ui::ask_sheet::{
    answer_key_labels, is_ask_key, request_for, screen_allows_sheet, who, AskNotice, AskRequest,
    AskSheetState,
};
use crate::ui::msdf_panel::{
    self as panel, columns, measure_char_width, rows_for_height, scroll_window, truncate, wrap,
};
use crate::ui::quick_context::{format_age, LineTone, PanelLine};
use crate::ui::screen::Screen;
use crate::ui::theme::Theme;
use crate::view::ui_rtt::UiRttTexture;

/// Column widths, in characters. Fixed so the columns line up down the panel
/// — the reason the rows truncate rather than wrap.
const W_ID: usize = 8;
const W_AGE: usize = 5;
const W_CONTEXT: usize = 10;
const W_WHO: usize = 8;
const W_TOOL: usize = 12;
const W_OPTION: usize = 12;
const W_REDEEMED: usize = 11;

/// The ribbon's own keys, after the answer keys the selected ask offers.
const RIBBON_MOVE_KEYS: &str = "Enter sheet  / filter  j/k move  Esc back";

/// The key line while a filter is being typed: every printable key edits the
/// filter, so no answer key is offered or live.
pub const RIBBON_FILTER_KEYS: &str = "type to filter  Backspace erase  Enter keep  Esc stop typing";

/// The key line's tail when the selected ask is NOT the viewer's to answer:
/// the answer keys are gone rather than offered and then refused, the same
/// honesty the sheet keeps (`ui::ask_sheet::sheet_keys`).
const RIBBON_NOT_YOURS: &str = "\u{00b7} not yours to answer";

// ============================================================================
// STATE
// ============================================================================

/// Whether the ribbon is open, which row is selected, and the filter.
#[derive(Resource, Default, Debug)]
pub struct LedgerRibbonState {
    /// `Ctrl+A l` latches this; Esc and the chord again clear it.
    pub open: bool,
    /// Index into the visible rows (pending, then answered, narrowed by the
    /// filter). Resolved on every frame by [`resolve_selection`].
    pub selected: usize,
    /// The request id the cursor is on, so it can follow its ask when rows
    /// above it come and go.
    pub anchor: Option<String>,
    /// The `/` filter text. Kept after typing stops, until the ribbon closes.
    pub filter: String,
    /// Whether printable keys edit [`Self::filter`] right now.
    pub filtering: bool,
    /// The ask the cursor was on when it vanished from the ledger, so the
    /// cursor moved to the row now under it. The next answer key is refused
    /// once, naming both, because the player aimed at the old one.
    pub retargeted_from: Option<String>,
}

impl LedgerRibbonState {
    /// Close the ribbon and forget the filter: a reopened ribbon starts from
    /// the whole ledger, as the TUI's does.
    pub fn close(&mut self) {
        self.open = false;
        self.filtering = false;
        if !self.filter.is_empty() {
            self.filter.clear();
        }
        self.anchor = None;
        self.selected = 0;
        self.retargeted_from = None;
    }
}

/// Marker for the ribbon's positioned container.
#[derive(Component, Debug, Reflect)]
#[reflect(Component)]
pub struct LedgerRibbonPanel;

/// Marker for the MSDF text surface inside it.
#[derive(Component, Debug, Reflect)]
#[reflect(Component)]
pub struct LedgerRibbonSurface;

// ============================================================================
// PURE: THE ROWS
// ============================================================================

/// Truncate to `width` and pad out to it, so the next column starts where it
/// started on the row above.
fn cell(text: &str, width: usize) -> String {
    let cut = truncate(text, width);
    let len = cut.chars().count();
    format!("{cut}{}", " ".repeat(width.saturating_sub(len)))
}

/// A right-aligned cell, for ages — `12s` and `4m` read as a column when
/// their units line up.
fn cell_right(text: &str, width: usize) -> String {
    let cut = truncate(text, width);
    let len = cut.chars().count();
    format!("{}{cut}", " ".repeat(width.saturating_sub(len)))
}

/// A context's label if this app knows one, else its short id. A context the
/// drift poll has not seen is still named, just less helpfully.
fn context_cell(context_id: Option<ContextId>, label: impl Fn(ContextId) -> Option<String>) -> String {
    match context_id {
        Some(id) => label(id).filter(|l| !l.is_empty()).unwrap_or_else(|| id.short()),
        None => "\u{2014}".to_string(),
    }
}

/// One open row: the ask marker, its handle, how long it has waited, where
/// it came from, who performed the work, the tool, and the first statement.
/// The tool is `\u{2014}` until the ask's full record is read.
pub fn pending_row(
    ask: &AskDetail,
    now_ms: i64,
    selected: bool,
    cols: usize,
    label: impl Fn(ContextId) -> Option<String>,
) -> PanelLine {
    let summary = &ask.summary;
    let age = format_age(now_ms.saturating_sub(summary.created_at_ms));
    let text = format!(
        "{}! {}  {}  {}  {}  {}  {}",
        if selected { "\u{25b8}" } else { " " },
        cell(short_request_id(&summary.request_id), W_ID),
        cell_right(&age, W_AGE),
        cell(&context_cell(summary.context_id, label), W_CONTEXT),
        cell(&who(&summary.performer), W_WHO),
        cell(ask.tool.as_deref().unwrap_or("\u{2014}"), W_TOOL),
        first_statement(ask),
    );
    PanelLine {
        text: truncate(&text, cols),
        tone: if selected { LineTone::Head } else { LineTone::Row },
    }
}

/// The first statement an ask covers, or its description when it records
/// none.
fn first_statement(ask: &AskDetail) -> &str {
    ask.summary
        .statements
        .first()
        .map(String::as_str)
        .filter(|s| !s.is_empty())
        .unwrap_or(ask.summary.description.as_str())
}

/// One answered row: what was decided, by whom, and whether it was ever
/// redeemed.
///
/// `redeemed \u{2014}` means the decision was never consumed, which is the
/// question the redemption incident taught us to ask (`docs/issues.md`).
pub fn answered_row(
    ask: &AskDetail,
    now_ms: i64,
    selected: bool,
    cols: usize,
    label: impl Fn(ContextId) -> Option<String>,
) -> PanelLine {
    let summary = &ask.summary;
    let age = match summary.decided_at_ms {
        Some(at) => format_age(now_ms.saturating_sub(at)),
        None => "?".to_string(),
    };
    let decision = ask.decision.as_ref();
    let option = decision
        .and_then(|d| d.option.as_deref())
        .filter(|o| !o.is_empty())
        .map(|o| o.replace('_', " "))
        // No recorded option means nobody decided it: expired, abandoned.
        .unwrap_or_else(|| summary.status.to_string());
    let decided_by = decision.and_then(|d| d.decided_by.clone());
    let redeemed = match ask.redeemed_at_ms {
        Some(at) => format!("redeemed {}", format_age(now_ms.saturating_sub(at))),
        None => "\u{2014}".to_string(),
    };
    let text = format!(
        "{} {}  {}  {}  {}  {}  {}  {}",
        if selected { "\u{25b8}" } else { " " },
        cell(short_request_id(&summary.request_id), W_ID),
        cell_right(&age, W_AGE),
        cell(&context_cell(summary.context_id, label), W_CONTEXT),
        cell(&option, W_OPTION),
        cell(&who(&decided_by), W_WHO),
        cell(&redeemed, W_REDEEMED),
        first_statement(ask),
    );
    PanelLine {
        text: truncate(&text, cols),
        tone: if selected { LineTone::Head } else { LineTone::Dim },
    }
}

/// One row of the cursor's list: an ask and which section it belongs to.
#[derive(Debug, Clone, Copy)]
pub struct Entry<'a> {
    pub ask: &'a AskDetail,
    pub pending: bool,
}

/// Whether `filter` admits `ask`: a case-insensitive substring over every
/// column a player might type — the id, the context, the tool, who is
/// involved, the statement, and how it was decided. An empty filter admits
/// everything.
pub fn admits(ask: &AskDetail, filter: &str, label: impl Fn(ContextId) -> Option<String>) -> bool {
    if filter.is_empty() {
        return true;
    }
    let summary = &ask.summary;
    let decision = ask.decision.as_ref();
    let hay = [
        summary.request_id.clone(),
        context_cell(summary.context_id, label),
        ask.tool.clone().unwrap_or_default(),
        who(&summary.requester),
        who(&summary.performer),
        who(&summary.reviewer),
        summary.statements.join(" "),
        summary.description.clone(),
        decision.and_then(|d| d.option.clone()).unwrap_or_default().replace('_', " "),
        who(&decision.and_then(|d| d.decided_by.clone())),
        // What the row itself shows when nobody decided: expired, abandoned.
        summary.status.to_string(),
        summary.origin.to_string(),
        ask.label.clone().unwrap_or_default(),
        ask.cwd.clone().unwrap_or_default(),
    ]
    .join(" ")
    .to_lowercase();
    hay.contains(&filter.to_lowercase())
}

/// The rows the cursor walks: pending first, then answered, each narrowed by
/// the filter, in the order the mirror holds them.
pub fn visible_entries<'a>(
    pending: &'a [AskDetail],
    recent: &'a [AskDetail],
    filter: &str,
    label: impl Fn(ContextId) -> Option<String> + Copy,
) -> Vec<Entry<'a>> {
    let pending = pending.iter().map(|ask| Entry { ask, pending: true });
    let answered = recent.iter().map(|ask| Entry { ask, pending: false });
    pending
        .chain(answered)
        .filter(|entry| admits(entry.ask, filter, label))
        .collect()
}

/// Everything the ribbon draws from, so one call lays out one frame.
pub struct RibbonView<'a> {
    /// The viewing player, whose keys the key line offers.
    pub me: Option<PrincipalId>,
    pub pending: &'a [AskDetail],
    pub recent: &'a [AskDetail],
    pub selected: usize,
    pub filter: &'a str,
    pub filtering: bool,
    /// [`LedgerMirror::synced`].
    pub synced: bool,
    /// [`LedgerMirror::history_gap`].
    pub history_gap: Option<&'a str>,
    pub now_ms: i64,
}

/// The key line for the selected row: the answer keys it offers, then the
/// ribbon's own keys. An answered row, or no row, offers only the ribbon's.
pub fn ribbon_keys(selected: Option<Entry<'_>>, me: Option<PrincipalId>, filtering: bool) -> String {
    if filtering {
        return RIBBON_FILTER_KEYS.to_string();
    }
    let mut parts: Vec<String> = Vec::new();
    let mut not_yours = false;
    if let Some(entry) = selected.filter(|e| e.pending) {
        parts = answer_key_labels(&entry.ask.summary, me, false);
        not_yours = !me.is_some_and(|me| entry.ask.summary.answerable_by(me));
    }
    parts.push(RIBBON_MOVE_KEYS.to_string());
    if not_yours {
        parts.push(RIBBON_NOT_YOURS.to_string());
    }
    parts.join("  ")
}

/// Every row of the ribbon, in order. The key lines are last, as on the
/// sheet.
///
/// A pending section with nothing in it says so: an empty body under a
/// PENDING heading must never be allowed to mean "we did not look". The same
/// goes for a ledger that is not synced and a history that was not read.
pub fn ribbon_rows(
    view: &RibbonView<'_>,
    cols: usize,
    rows: usize,
    label: impl Fn(ContextId) -> Option<String> + Copy,
) -> Vec<PanelLine> {
    let entries = visible_entries(view.pending, view.recent, view.filter, label);
    let selected = view.selected.min(entries.len().saturating_sub(1));

    let mut fixed = vec![PanelLine {
        text: truncate(
            &format!(
                "LEDGER    pending {}   answered {}",
                view.pending.len(),
                view.recent.len()
            ),
            cols,
        ),
        tone: LineTone::Head,
    }];
    if !view.synced {
        fixed.push(PanelLine {
            text: truncate(
                "ledger not synced \u{2014} showing the last asks this app read",
                cols,
            ),
            tone: LineTone::Warn,
        });
    }
    if view.filtering || !view.filter.is_empty() {
        fixed.push(PanelLine {
            text: truncate(
                &format!("/{}{}", view.filter, if view.filtering { "_" } else { "" }),
                cols,
            ),
            tone: LineTone::Head,
        });
    }

    let mut body: Vec<PanelLine> = Vec::new();
    let mut selected_line = 0;
    let mut section: Option<bool> = None;
    for (i, entry) in entries.iter().enumerate() {
        if section != Some(entry.pending) {
            section = Some(entry.pending);
            body.push(PanelLine {
                text: if entry.pending {
                    "PENDING".to_string()
                } else {
                    format!("ANSWERED (last {RECENT_CAP})")
                },
                tone: LineTone::Head,
            });
        }
        if i == selected {
            selected_line = body.len();
        }
        let is_selected = i == selected;
        body.push(if entry.pending {
            pending_row(entry.ask, view.now_ms, is_selected, cols, label)
        } else {
            answered_row(entry.ask, view.now_ms, is_selected, cols, label)
        });
        if !entry.pending
            && let Some(reason) = &entry.ask.publication_abandoned
        {
            for text in wrap(&format!("publication abandoned: {reason}"), cols, 2) {
                body.push(PanelLine { text, tone: LineTone::Warn });
            }
        }
    }
    // An empty section says so, whether the ledger is empty or the filter
    // admits nothing.
    if entries.is_empty() && !view.filter.is_empty() {
        body.push(PanelLine {
            text: truncate(&format!("nothing matches {:?}", view.filter), cols),
            tone: LineTone::Dim,
        });
    } else if view.filter.is_empty() && !entries.iter().any(|e| e.pending) {
        body.insert(
            0,
            PanelLine { text: "PENDING".to_string(), tone: LineTone::Head },
        );
        body.insert(
            1,
            PanelLine { text: "nothing waiting".to_string(), tone: LineTone::Dim },
        );
        selected_line += 2;
    }
    if let Some(gap) = view.history_gap {
        body.push(PanelLine {
            text: truncate(&format!("answered history not read: {gap}"), cols),
            tone: LineTone::Warn,
        });
    }

    let keys: Vec<PanelLine> = wrap(
        &ribbon_keys(entries.get(selected).copied(), view.me, view.filtering),
        cols,
        2,
    )
    .into_iter()
    .map(|text| PanelLine { text, tone: LineTone::Head })
    .collect();

    // The head and the key lines are never scrolled away; the list scrolls
    // between them with the cursor in view, and says what is out of sight.
    let room = rows.saturating_sub(fixed.len() + keys.len()).max(2);
    let shown = room.saturating_sub(1);
    let scroll = (selected_line + 1).saturating_sub(shown);
    let mut out = fixed;
    out.extend(scroll_window(&body, scroll, room));
    out.extend(keys);
    out
}

/// The notice for a key refused because the cursor's ask left under it.
pub fn retarget_notice(old: &str, new: &str) -> String {
    format!(
        "ask {} left; {} is selected \u{2014} press again",
        short_request_id(old),
        short_request_id(new)
    )
}

/// Resolve the cursor against the rows that exist now. The anchor wins: the
/// cursor follows its ask by request id wherever the list has moved it. An
/// anchor that left falls back to the old index, clamped, and the row there
/// becomes the anchor. Returns the index and the anchor to hold.
pub fn resolve_selection(
    ids: &[&str],
    selected: usize,
    anchor: Option<&str>,
) -> (usize, Option<String>) {
    if ids.is_empty() {
        return (0, None);
    }
    if let Some(at) = anchor.and_then(|a| ids.iter().position(|id| *id == a)) {
        return (at, Some(ids[at].to_string()));
    }
    let at = selected.min(ids.len() - 1);
    (at, Some(ids[at].to_string()))
}

/// The anchor's id when it has left the ledger altogether — pending and
/// answered both, whatever the filter hides — and a different row now
/// stands under the cursor. A filter that merely hides the anchor is the
/// player's own doing and is not this.
pub fn departed_anchor(all_ids: &[&str], visible_len: usize, anchor: Option<&str>) -> Option<String> {
    let anchor = anchor?;
    (visible_len > 0 && !all_ids.contains(&anchor)).then(|| anchor.to_string())
}

/// Move the selection, stopping at the ends. `j` past the bottom stays at
/// the bottom: a wrapping selection in a list you are deciding from is a way
/// to answer the wrong ask.
pub fn step_selection(selected: usize, len: usize, delta: isize) -> usize {
    if len == 0 {
        return 0;
    }
    let next = (selected as isize + delta).clamp(0, len as isize - 1);
    next as usize
}

// ============================================================================
// SYSTEMS
// ============================================================================

/// The label closure every ribbon system shares: a context's name from the
/// drift poll.
fn context_label(drift: &crate::connection::drift::DriftState) -> impl Fn(ContextId) -> Option<String> + Copy + '_ {
    move |id: ContextId| {
        drift
            .contexts
            .iter()
            .find(|c| c.id == id)
            .map(|c| c.label.clone())
    }
}

/// `Ctrl+A l` from anywhere: toggle the ribbon where it can open, and say
/// why when it cannot.
pub fn handle_ledger_chord(
    mut actions: MessageReader<ActionFired>,
    screen: Res<State<Screen>>,
    time: Res<Time>,
    mut ribbon: ResMut<LedgerRibbonState>,
    mut notice: ResMut<AskNotice>,
) {
    for ActionFired { action, context } in actions.read() {
        // The chord fires as a Global action (`input::prefix`); the sheet's
        // own `v` and the ribbon's own toggle carry their contexts instead.
        if *context != InputContext::Global || *action != Action::OpenLedger {
            continue;
        }
        if !screen_allows_sheet(*screen.get()) {
            notice.say(
                "the ledger needs the conversation or the room \u{2014} Ctrl+A d first",
                time.elapsed_secs_f64(),
            );
            continue;
        }
        if ribbon.open {
            ribbon.close();
        } else {
            ribbon.open = true;
        }
    }
}

/// Type into the filter while it is being typed. Reads the text of each key
/// press directly, because the ribbon's letters are all bound to answers and
/// the handler ignores them for as long as `filtering` is set. Runs before
/// [`handle_ledger_ribbon_actions`], so the `/` that starts the filter is not
/// its first character.
pub fn type_ledger_filter(
    mut keyboard: MessageReader<KeyboardInput>,
    keys: Res<ButtonInput<KeyCode>>,
    prefix: Res<crate::input::prefix::PrefixState>,
    mut ribbon: ResMut<LedgerRibbonState>,
) {
    // Nothing to type into: drop the frame's keys unread.
    if !ribbon.open || !ribbon.filtering {
        keyboard.clear();
        return;
    }
    let held_modifier = keys.any_pressed([
        KeyCode::ControlLeft,
        KeyCode::ControlRight,
        KeyCode::AltLeft,
        KeyCode::AltRight,
        KeyCode::SuperLeft,
        KeyCode::SuperRight,
    ]);
    let mut text = String::new();
    let mut erase = 0;
    apply_filter_keys(
        keyboard.read().filter(|event| event.state.is_pressed()),
        prefix.armed(),
        held_modifier,
        &mut text,
        &mut erase,
    );
    if text.is_empty() && erase == 0 {
        return;
    }
    for _ in 0..erase {
        ribbon.filter.pop();
    }
    ribbon.filter.push_str(&text);
}

/// Fold one frame's key presses into filter edits: the text to append and
/// how many characters to erase first. Keys that belong to the `Ctrl+A`
/// chord are not text: the prefix key itself, and the one key after it
/// (`prefix_armed` says a chord was already pending when the frame began;
/// dispatch resolves it, so this reader must not also type it).
pub fn apply_filter_keys<'a>(
    events: impl Iterator<Item = &'a KeyboardInput>,
    prefix_armed: bool,
    held_modifier: bool,
    text: &mut String,
    erase: &mut usize,
) {
    let mut chord_pending = prefix_armed;
    for event in events {
        if crate::input::prefix::is_bare_modifier(event.key_code) {
            continue;
        }
        if chord_pending {
            chord_pending = false;
            continue;
        }
        if held_modifier && event.key_code == KeyCode::KeyA {
            // Ctrl+A arms the prefix; the key after it is the chord's.
            chord_pending = true;
            continue;
        }
        if event.logical_key == bevy::input::keyboard::Key::Backspace {
            if text.pop().is_none() {
                *erase += 1;
            }
            continue;
        }
        if held_modifier {
            continue;
        }
        if let Some(typed) = &event.text {
            text.extend(typed.chars().filter(|c| !c.is_control()));
        }
    }
}

/// The ribbon's keys: move, answer, filter, open the sheet, close.
#[allow(clippy::too_many_arguments)]
pub fn handle_ledger_ribbon_actions(
    mut actions: MessageReader<ActionFired>,
    mirror: Res<LedgerMirror>,
    drift: Res<crate::connection::drift::DriftState>,
    session: Res<crate::cell::SessionPrincipal>,
    time: Res<Time>,
    mut ribbon: ResMut<LedgerRibbonState>,
    mut sheet: ResMut<AskSheetState>,
    mut notice: ResMut<AskNotice>,
    mut decisions: MessageWriter<AskDecisionRequested>,
    mut reassignments: MessageWriter<AskReassignRequested>,
) {
    let label = context_label(&drift);
    for ActionFired { action, context } in actions.read() {
        if *context != InputContext::LedgerRibbon {
            continue;
        }

        // While the filter is being typed, the letters are text: only the
        // two keys that end typing act.
        if ribbon.filtering {
            if matches!(action, Action::Activate | Action::CloseLedger | Action::OpenLedger) {
                ribbon.filtering = false;
            }
            continue;
        }

        let pending = mirror.open_records();
        let entries = visible_entries(&pending, mirror.recent(), &ribbon.filter, label);
        let selected = ribbon.selected.min(entries.len().saturating_sub(1));
        let row = entries.get(selected).copied();

        match action {
            Action::StepNext | Action::StepPrev => {
                let delta = if matches!(action, Action::StepNext) { 1 } else { -1 };
                let next = step_selection(selected, entries.len(), delta);
                ribbon.retargeted_from = None;
                ribbon.selected = next;
                ribbon.anchor = entries.get(next).map(|e| e.ask.summary.request_id.clone());
            }

            Action::LedgerFilter => ribbon.filtering = true,

            action if is_ask_key(action) => {
                let Some(entry) = row.filter(|e| e.pending) else {
                    notice.say("nothing waiting to answer", time.elapsed_secs_f64());
                    continue;
                };
                let ask = &entry.ask.summary;
                // The ask the player aimed at left and another stands under
                // the cursor: say so once, and send nothing.
                if let Some(old) = ribbon.retargeted_from.take() {
                    notice.say(retarget_notice(&old, &ask.request_id), time.elapsed_secs_f64());
                    continue;
                }
                match request_for(action, ask, session.0) {
                    Some(Ok(AskRequest::Decide(decision))) => {
                        decisions.write(AskDecisionRequested {
                            request_id: ask.request_id.clone(),
                            decision,
                        });
                    }
                    Some(Ok(AskRequest::Reassign(reassign))) => {
                        reassignments.write(AskReassignRequested {
                            request_id: ask.request_id.clone(),
                            reassign,
                        });
                    }
                    Some(Err(why)) => notice.say(why, time.elapsed_secs_f64()),
                    None => {}
                }
            }

            // Enter hands the row to the sheet, which is the untruncated
            // view — so the ribbon closes rather than sitting in front of
            // it. An answered row opens as a closed ask.
            Action::Activate => {
                let Some(entry) = row else {
                    continue;
                };
                let id = entry.ask.summary.request_id.clone();
                if entry.pending {
                    sheet.open(id);
                } else {
                    sheet.open_closed(id);
                }
                ribbon.close();
            }

            // Esc, and the chord again.
            Action::CloseLedger | Action::OpenLedger => ribbon.close(),

            _ => {}
        }
    }
}

/// Keep the cursor on its ask, inside the visible rows, and close the ribbon
/// when a vi screen takes the keyboard.
pub fn sync_ledger_ribbon(
    mirror: Res<LedgerMirror>,
    drift: Res<crate::connection::drift::DriftState>,
    screen: Res<State<Screen>>,
    mut ribbon: ResMut<LedgerRibbonState>,
) {
    if ribbon.open && !screen_allows_sheet(*screen.get()) {
        ribbon.close();
        return;
    }
    if !ribbon.open {
        return;
    }
    let pending = mirror.open_records();
    let entries = visible_entries(&pending, mirror.recent(), &ribbon.filter, context_label(&drift));
    let ids: Vec<&str> = entries.iter().map(|e| e.ask.summary.request_id.as_str()).collect();
    let all = visible_entries(&pending, mirror.recent(), "", context_label(&drift));
    let all_ids: Vec<&str> = all.iter().map(|e| e.ask.summary.request_id.as_str()).collect();
    let gone = departed_anchor(&all_ids, ids.len(), ribbon.anchor.as_deref());
    let (selected, anchor) = resolve_selection(&ids, ribbon.selected, ribbon.anchor.as_deref());
    if gone.is_some() {
        ribbon.retargeted_from = gone;
    }
    if selected != ribbon.selected {
        ribbon.selected = selected;
    }
    if anchor != ribbon.anchor {
        ribbon.anchor = anchor;
    }
}

/// Show or hide the ribbon.
pub fn sync_ledger_ribbon_visibility(
    ribbon: Res<LedgerRibbonState>,
    mut panels: Query<&mut Visibility, With<LedgerRibbonPanel>>,
) {
    if !ribbon.is_changed() {
        return;
    }
    let want = if ribbon.open {
        Visibility::Inherited
    } else {
        Visibility::Hidden
    };
    for mut vis in panels.iter_mut() {
        if *vis != want {
            *vis = want;
        }
    }
}

/// Spawn the ribbon, hidden, as a child of the tiling root.
pub fn spawn_ledger_ribbon(
    mut commands: Commands,
    theme: Res<Theme>,
    mut fx_materials: ResMut<Assets<BlockFxMaterial>>,
    tiling_root: Query<Entity, With<super::tiling_reconciler::TilingRoot>>,
    existing: Query<Entity, With<LedgerRibbonPanel>>,
) {
    if !existing.is_empty() {
        return;
    }
    let Ok(root) = tiling_root.single() else {
        return;
    };
    panel::spawn_centered_panel(
        &mut commands,
        root,
        &theme,
        fx_materials.add(BlockFxMaterial::default()),
        crate::constants::ZLayer::LEDGER_RIBBON,
        LedgerRibbonPanel,
        LedgerRibbonSurface,
    );
}

/// Rebuild the ribbon's glyphs. Same `PostUpdate` contract as the sheet.
#[allow(clippy::too_many_arguments)]
pub fn render_ledger_ribbon(
    ribbon: Res<LedgerRibbonState>,
    mirror: Res<LedgerMirror>,
    session: Res<crate::cell::SessionPrincipal>,
    drift: Res<crate::connection::drift::DriftState>,
    theme: Res<Theme>,
    time: Res<Time>,
    viewport: Res<panel::LayoutViewport>,
    mut fonts: crate::ui::msdf_panel::PanelFonts,
    mut surface: Query<
        (&mut MsdfBlockGlyphs, &mut UiRttTexture, &mut Node),
        With<LedgerRibbonSurface>,
    >,
    mut container: Query<&mut Node, (With<LedgerRibbonPanel>, Without<LedgerRibbonSurface>)>,
    mut last_build: Local<f64>,
) {
    if !ribbon.open {
        return;
    }
    let now_secs = time.elapsed_secs_f64();
    let stale = now_secs - *last_build >= 1.0;
    if !ribbon.is_changed()
        && !mirror.is_changed()
        && !theme.is_changed()
        && !viewport.is_changed()
        && !stale
    {
        return;
    }
    let Ok((mut glyphs_out, mut rtt, mut node)) = surface.single_mut() else {
        return;
    };
    let Some(font) = fonts.fonts.get(&fonts.handles.mono) else {
        return;
    };
    let Some(atlas) = fonts.atlas.as_mut() else {
        return;
    };
    *last_build = now_secs;

    let width = panel::panel_width(viewport.size.x) as f64;
    let cols = columns(width, measure_char_width(font));
    let rows = rows_for_height((viewport.size.y * panel::PANEL_MAX_HEIGHT_FRACTION) as f64);
    let open = mirror.open_records();
    let lines = ribbon_rows(
        &RibbonView {
            me: session.0,
            pending: &open,
            recent: mirror.recent(),
            selected: ribbon.selected,
            filter: &ribbon.filter,
            filtering: ribbon.filtering,
            synced: mirror.synced,
            history_gap: mirror.history_gap(),
            now_ms: kaijutsu_types::now_millis() as i64,
        },
        cols,
        rows,
        context_label(&drift),
    );
    let (glyphs, height) =
        panel::collect_panel_glyphs(&lines, &theme, font, atlas, &mut fonts.font_data_map);

    glyphs_out.glyphs = glyphs;
    glyphs_out.version = glyphs_out.version.wrapping_add(1).max(1);
    rtt.built_width = width as f32;
    rtt.built_height = height as f32;
    node.width = Val::Px(width as f32);
    node.height = Val::Px(height as f32);
    if let Ok(mut outer) = container.single_mut() {
        outer.left = Val::Px(panel::panel_left(viewport.size.x));
        outer.width = Val::Px(width as f32);
    }
}

// ============================================================================
// PLUGIN
// ============================================================================

pub struct LedgerRibbonPlugin;

impl Plugin for LedgerRibbonPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<LedgerRibbonState>()
            .register_type::<LedgerRibbonPanel>()
            .register_type::<LedgerRibbonSurface>()
            .add_systems(PostStartup, spawn_ledger_ribbon)
            .add_systems(
                Update,
                sync_ledger_ribbon
                    .in_set(crate::input::InputPhase::SyncContext)
                    .before(crate::input::context::sync_input_context),
            )
            // Before dispatch resolves this frame's keys, so a pending
            // `Ctrl+A` chord is still pending when this reads them.
            .add_systems(
                Update,
                type_ledger_filter.in_set(crate::input::InputPhase::SyncContext),
            )
            .add_systems(
                Update,
                (
                    handle_ledger_chord,
                    handle_ledger_ribbon_actions,
                    sync_ledger_ribbon_visibility,
                )
                    .chain()
                    .in_set(crate::input::InputPhase::Handle),
            )
            .add_systems(
                PostUpdate,
                render_ledger_ribbon
                    .after(bevy::ui::UiSystems::Layout)
                    .before(crate::view::block_render::resize_block_textures),
            );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kaijutsu_types::{AskDecision, AskOrigin, AskStatus, AskSummary, PrincipalRef};

    fn ctx(n: u8) -> ContextId {
        let mut bytes = [0u8; 16];
        bytes[0] = n;
        ContextId::from_bytes(bytes)
    }

    fn principal(n: u8) -> PrincipalId {
        let mut bytes = [0u8; 16];
        bytes[15] = n;
        PrincipalId::from_bytes(bytes)
    }

    fn named(n: u8, name: &str) -> PrincipalRef {
        PrincipalRef { id: principal(n), name: name.into() }
    }

    fn ask(id: &str) -> AskDetail {
        AskDetail {
            summary: AskSummary {
                request_id: format!("{id}-aaaa-bbbb-cccc-000000000001"),
                status: AskStatus::Pending,
                origin: AskOrigin::ShellGate,
                context_id: Some(ctx(1)),
                description: "a description".into(),
                statements: vec!["git worktree remove --force ~/src/wt/kaish-arith".into()],
                requester: Some(named(9, "amy")),
                performer: Some(named(4, "coder")),
                reviewer: Some(named(1, "amy")),
                created_at_ms: 0,
                decided_at_ms: None,
                tool_call_block_id: None,
                lineage_root: None,
                has_exec_source: false,
            },
            instance: None,
            tool: Some("shell_write".into()),
            hook_id: None,
            label: None,
            exec_source: Some("kaish".into()),
            cwd: None,
            env: Vec::new(),
            decision: None,
            redeemed_at_ms: None,
            publication_abandoned: None,
            reassignments: Vec::new(),
        }
    }

    /// `ask` closed with `status`, decided as `option` by `by`.
    fn closed(id: &str, status: AskStatus, option: &str, by: Option<PrincipalRef>) -> AskDetail {
        let mut ask = ask(id);
        ask.summary.status = status;
        ask.summary.decided_at_ms = Some(0);
        ask.decision = Some(AskDecision {
            decided_by: by,
            option: Some(option.into()),
            remember_scope: None,
            auto_reason: None,
        });
        ask
    }

    fn no_labels(_: ContextId) -> Option<String> {
        None
    }

    fn text(rows: &[PanelLine]) -> String {
        rows.iter().map(|r| r.text.clone()).collect::<Vec<_>>().join("\n")
    }

    /// A view of `pending` and `recent` for the viewer `principal(1)` (amy,
    /// the reviewer of every `ask`), synced, nothing filtered.
    fn view<'a>(pending: &'a [AskDetail], recent: &'a [AskDetail]) -> RibbonView<'a> {
        RibbonView {
            me: Some(principal(1)),
            pending,
            recent,
            selected: 0,
            filter: "",
            filtering: false,
            synced: true,
            history_gap: None,
            now_ms: 0,
        }
    }

    fn render(view: &RibbonView<'_>, cols: usize, rows: usize) -> Vec<PanelLine> {
        ribbon_rows(view, cols, rows, no_labels)
    }

    // ── rows ──────────────────────────────────────────────────────────────

    #[test]
    fn a_pending_row_carries_the_marker_the_handle_and_the_wait() {
        let row = pending_row(&ask("01a04eb6"), 12_000, false, 200, no_labels);
        assert!(row.text.starts_with(" ! 01a04eb6"), "{:?}", row.text);
        assert!(row.text.contains("12s"), "{:?}", row.text);
        assert!(row.text.contains("coder"), "{:?}", row.text);
        assert!(row.text.contains("shell_write"), "{:?}", row.text);
        assert!(row.text.contains("git worktree remove"), "{:?}", row.text);
    }

    /// The selection has to be visible in the text itself: one row is one
    /// brush, so a color alone could not carry it.
    #[test]
    fn the_selected_row_is_marked_in_its_own_text() {
        let row = pending_row(&ask("01a04eb6"), 0, true, 200, no_labels);
        assert!(row.text.starts_with("\u{25b8}!"), "{:?}", row.text);
        assert_eq!(row.tone, LineTone::Head);
        let answered = answered_row(&closed("01a04eaa", AskStatus::Allowed, "allow_once", None), 0, true, 200, no_labels);
        assert!(answered.text.starts_with("\u{25b8} 01a04eaa"), "{:?}", answered.text);
        assert_eq!(answered.tone, LineTone::Head);
    }

    /// A context this app has a label for reads by name; one it does not
    /// still reads, by short id.
    #[test]
    fn a_row_names_its_context_however_much_it_knows() {
        let named = pending_row(&ask("01a04eb6"), 0, false, 200, |_| {
            Some("kaijutsu".to_string())
        });
        assert!(named.text.contains("kaijutsu"), "{:?}", named.text);

        let bare = pending_row(&ask("01a04eb6"), 0, false, 200, no_labels);
        assert!(bare.text.contains(&ctx(1).short()), "{:?}", bare.text);
    }

    /// Until an ask's full record is read the tool is unknown, and the
    /// column says so rather than staying blank.
    #[test]
    fn a_row_read_only_from_its_summary_has_no_tool() {
        let unread = crate::connection::ledger::summary_record(&ask("01a04eb6").summary);
        let row = pending_row(&unread, 0, false, 200, no_labels);
        assert!(!row.text.contains("shell_write"), "{:?}", row.text);
        assert!(row.text.contains('\u{2014}'), "{:?}", row.text);
    }

    #[test]
    fn publication_abandonment_is_visible_with_the_original_decision() {
        for (status, option) in [(AskStatus::Allowed, "allow_once"), (AskStatus::Denied, "deny")] {
            let mut retired = closed("01a04eaa", status, option, Some(named(1, "amy")));
            retired.redeemed_at_ms = Some(0);
            retired.publication_abandoned = Some("Caller stopped. Source did not run.".into());
            let recent = [retired];
            let mut v = view(&[], &recent);
            v.now_ms = 5000;
            let rows = render(&v, 100, 40);
            let text = rows.iter().map(|line| line.text.as_str()).collect::<Vec<_>>().join(" ");
            assert!(text.contains(&option.replace('_', " ")), "{text}");
            assert!(text.contains("publication abandoned"), "{text}");
            assert!(text.contains("Source did not run."), "{text}");
            assert!(rows.last().unwrap().text.contains("Esc back"));
        }
    }

    #[test]
    fn an_answered_row_says_whether_it_was_redeemed() {
        let mut decided = closed("01a04eaa", AskStatus::Allowed, "allow_once", Some(named(1, "amy")));
        decided.redeemed_at_ms = Some(0);
        let row = answered_row(&decided, 5_000, false, 200, no_labels);
        assert!(row.text.contains("allow once"), "{:?}", row.text);
        assert!(row.text.contains("amy"), "{:?}", row.text);
        assert!(row.text.contains("redeemed 5s"), "{:?}", row.text);
        assert_eq!(row.tone, LineTone::Dim);

        decided.redeemed_at_ms = None;
        let never = answered_row(&decided, 5_000, false, 200, no_labels);
        assert!(never.text.contains('\u{2014}'), "{:?}", never.text);
        assert!(!never.text.contains("redeemed"), "{:?}", never.text);
    }

    /// An expired ask was decided by nobody. The option column must say that
    /// rather than borrow an allow.
    #[test]
    fn an_answered_row_with_no_decision_shows_its_status() {
        let mut gone = ask("01a04eaa");
        gone.summary.status = AskStatus::Expired;
        gone.summary.decided_at_ms = Some(0);
        let row = answered_row(&gone, 0, false, 200, no_labels);
        assert!(row.text.contains("expired"), "{:?}", row.text);
        assert!(!row.text.contains("allow"), "{:?}", row.text);
    }

    // ── the whole ribbon ──────────────────────────────────────────────────

    #[test]
    fn the_header_counts_what_the_mirror_holds() {
        let pending = vec![ask("aaaaaaaa"), ask("bbbbbbbb")];
        let recent = vec![closed("cccccccc", AskStatus::Denied, "deny", None)];
        let rows = render(&view(&pending, &recent), 200, 40);
        assert_eq!(rows[0].text, "LEDGER    pending 2   answered 1");
    }

    /// An empty PENDING section must never be allowed to mean "we did not
    /// look".
    #[test]
    fn an_empty_ledger_says_nothing_is_waiting() {
        let all = text(&render(&view(&[], &[]), 200, 40));
        assert!(all.contains("PENDING"), "{all}");
        assert!(all.contains("nothing waiting"), "{all}");
        assert!(!all.contains("ANSWERED"), "no decisions, no heading: {all}");
    }

    /// Answered asks beyond the third are the point of the history read.
    #[test]
    fn the_answered_section_lists_every_row_the_mirror_holds() {
        let recent: Vec<AskDetail> = (0..20)
            .map(|i| closed(&format!("id{i:06}"), AskStatus::Allowed, "allow_once", None))
            .collect();
        let all = text(&render(&view(&[], &recent), 200, 60));
        assert!(all.contains("ANSWERED (last 20)"), "{all}");
        assert!(all.contains("id000019"), "the 20th answered ask is listed: {all}");
        assert!(all.contains("answered 20"), "{all}");
    }

    #[test]
    fn the_key_lines_are_always_the_last_rows() {
        let pending: Vec<AskDetail> = (0..40).map(|i| ask(&format!("{i:08}"))).collect();
        for rows in [6usize, 12, 60] {
            let out = render(&view(&pending, &[]), 200, rows);
            assert!(out.len() <= rows, "rows={rows} drew {}", out.len());
            assert!(out.last().expect("rows").text.contains("Esc back"), "rows={rows}");
        }
    }

    /// A ledger longer than the panel scrolls with the cursor and counts
    /// what is out of sight — never silently.
    #[test]
    fn a_ledger_taller_than_the_panel_scrolls_with_the_cursor() {
        let pending: Vec<AskDetail> = (0..40).map(|i| ask(&format!("id{i:06}"))).collect();
        let mut v = view(&pending, &[]);
        let top = render(&v, 200, 10);
        assert!(top.len() <= 10);
        assert!(text(&top).contains('\u{2193}'), "{}", text(&top));
        assert!(text(&top).contains("id000000"));
        v.selected = 30;
        let down = render(&v, 200, 10);
        let all = text(&down);
        assert!(all.contains("\u{25b8}! id000030"), "the cursor stays in view: {all}");
        assert!(!all.contains("id000000"), "{all}");
    }

    /// Rows truncate; scanning a column down the panel is the point.
    #[test]
    fn no_row_wraps_or_overflows() {
        let mut wide = ask("01a04eb6");
        wide.summary.statements = vec!["x".repeat(400)];
        let pending = vec![wide];
        let mut decided = ask("01a04eaa");
        decided.summary.decided_at_ms = Some(0);
        decided.summary.statements = vec!["y".repeat(400)];
        let recent = vec![decided];
        for cols in [30usize, 60, 100] {
            let out = render(&view(&pending, &recent), cols, 40);
            for row in &out {
                assert!(
                    row.text.chars().count() <= cols,
                    "cols={cols} overflowed with {:?}",
                    row.text
                );
            }
            let asks = out
                .iter()
                .filter(|r| r.text.starts_with("\u{25b8}!") || r.text.starts_with(" !") || r.text.starts_with("  01a04eaa"))
                .count();
            assert_eq!(asks, 2, "cols={cols}: one row per ask, never wrapped");
        }
    }

    #[test]
    fn an_unsynced_ledger_says_so_and_a_missing_history_says_so() {
        let mut v = view(&[], &[]);
        v.synced = false;
        v.history_gap = Some("timed out");
        let all = text(&render(&v, 200, 40));
        assert!(all.contains("ledger not synced"), "{all}");
        assert!(all.contains("answered history not read: timed out"), "{all}");
        let synced = text(&render(&view(&[], &[]), 200, 40));
        assert!(!synced.contains("not synced"), "{synced}");
    }

    // ── the filter ────────────────────────────────────────────────────────

    fn mixed() -> (Vec<AskDetail>, Vec<AskDetail>) {
        let mut push = ask("aaaaaaaa");
        push.summary.statements = vec!["git push origin main".into()];
        let mut cargo = ask("bbbbbbbb");
        cargo.summary.statements = vec!["cargo test".into()];
        let mut done = closed("cccccccc", AskStatus::Allowed, "allow_once", Some(named(1, "amy")));
        done.summary.statements = vec!["git push --force".into()];
        let mut old = closed("dddddddd", AskStatus::Denied, "deny", Some(named(1, "amy")));
        old.summary.statements = vec!["rm -rf target".into()];
        (vec![push, cargo], vec![done, old])
    }

    #[test]
    fn a_filter_narrows_both_sections() {
        let (pending, recent) = mixed();
        let rows = visible_entries(&pending, &recent, "git", no_labels);
        let ids: Vec<&str> = rows.iter().map(|e| short_request_id(&e.ask.summary.request_id)).collect();
        assert_eq!(ids, vec!["aaaaaaaa", "cccccccc"]);
        assert!(rows[0].pending && !rows[1].pending);
    }

    #[test]
    fn the_filter_ignores_case_and_reads_every_column() {
        let (pending, recent) = mixed();
        let by = |needle: &str| visible_entries(&pending, &recent, needle, no_labels).len();
        assert_eq!(by("GIT PUSH"), 2, "case-insensitive");
        assert_eq!(by("cccccccc"), 1, "request id");
        assert_eq!(by("shell_write"), 4, "tool");
        assert_eq!(by("deny"), 1, "decision option");
        assert_eq!(by("coder"), 4, "performer");
        assert_eq!(by(""), 4, "an empty filter admits everything");
        let labeled = visible_entries(&pending, &recent, "kaijutsu", |_| Some("kaijutsu".into()));
        assert_eq!(labeled.len(), 4, "context label");
    }

    #[test]
    fn a_filter_that_admits_nothing_says_so() {
        let (pending, recent) = mixed();
        let mut v = view(&pending, &recent);
        v.filter = "zzz";
        let all = text(&render(&v, 200, 40));
        assert!(all.contains("nothing matches \"zzz\""), "{all}");
        assert!(!all.contains("nothing waiting"), "{all}");
    }

    #[test]
    fn the_filter_line_shows_the_text_and_a_cursor_while_typing() {
        let (pending, recent) = mixed();
        let mut v = view(&pending, &recent);
        v.filter = "git";
        v.filtering = true;
        let rows = render(&v, 200, 40);
        assert!(rows.iter().any(|r| r.text == "/git_"), "{}", text(&rows));
        assert_eq!(rows.last().unwrap().text, RIBBON_FILTER_KEYS);
        v.filtering = false;
        assert!(render(&v, 200, 40).iter().any(|r| r.text == "/git"));
    }

    // ── the selection ─────────────────────────────────────────────────────

    #[test]
    fn stepping_the_selection_stops_at_the_ends() {
        assert_eq!(step_selection(0, 3, -1), 0);
        assert_eq!(step_selection(2, 3, 1), 2);
        assert_eq!(step_selection(1, 3, 1), 2);
        assert_eq!(step_selection(0, 0, 1), 0);
    }

    /// The cursor follows its ask when rows above it come and go; a key must
    /// not land on a different ask than the one on screen.
    #[test]
    fn the_cursor_follows_its_ask_by_id() {
        assert_eq!(resolve_selection(&["a", "b", "c"], 1, Some("b")), (1, Some("b".into())));
        // "a" departed above the cursor: index 1 is now "c", but the anchor is "b".
        assert_eq!(resolve_selection(&["b", "c"], 1, Some("b")), (0, Some("b".into())));
        // The anchored ask itself left: fall back to the clamped index.
        assert_eq!(resolve_selection(&["a", "c"], 5, Some("b")), (1, Some("c".into())));
        assert_eq!(resolve_selection(&[], 3, Some("b")), (0, None));
        // No anchor yet: the index stands.
        assert_eq!(resolve_selection(&["a", "b"], 1, None), (1, Some("b".into())));
    }

    // ── the keys ──────────────────────────────────────────────────────────

    fn pending_entry(ask: &AskDetail) -> Entry<'_> {
        Entry { ask, pending: true }
    }

    #[test]
    fn the_key_line_drops_the_decision_keys_when_the_row_is_not_yours() {
        let me = Some(principal(1));
        let mine = ask("a");
        let keys = ribbon_keys(Some(pending_entry(&mine)), me, false);
        assert!(keys.starts_with("a allow once  A allow always (global)  d deny"), "{keys}");
        assert!(keys.contains("Enter sheet") && keys.contains("/ filter"), "{keys}");
        assert!(!keys.contains("not yours"), "{keys}");

        let mut theirs = ask("b");
        theirs.summary.reviewer = Some(named(3, "banto"));
        let keys = ribbon_keys(Some(pending_entry(&theirs)), me, false);
        assert!(!keys.contains("allow once"), "{keys}");
        assert!(keys.contains("not yours to answer"), "{keys}");
        assert!(keys.contains("Enter sheet"), "{keys}");
    }

    /// An answered row has nothing to answer, and an empty ledger has no row
    /// to gate on.
    #[test]
    fn an_answered_row_or_no_row_offers_only_the_ribbons_own_keys() {
        let me = Some(principal(1));
        let done = closed("a", AskStatus::Allowed, "allow_once", None);
        let answered = ribbon_keys(Some(Entry { ask: &done, pending: false }), me, false);
        assert_eq!(answered, RIBBON_MOVE_KEYS);
        assert_eq!(ribbon_keys(None, me, false), RIBBON_MOVE_KEYS);
    }

    #[test]
    fn the_character_escalate_and_takeover_keys_follow_their_predicates() {
        let me = Some(principal(1));
        let mut a = ask("a");
        a.summary.has_exec_source = true;
        let keys = ribbon_keys(Some(pending_entry(&a)), me, false);
        assert!(keys.contains("[c] always for coder"), "{keys}");
        assert!(keys.contains("[C] coder, command family"), "{keys}");
        assert!(keys.contains("[e]scalate"), "{keys}");
        assert!(!keys.contains("[t]ake over"), "{keys}");

        // Someone else's ask whose lineage root is me: only take over.
        let mut theirs = ask("b");
        theirs.summary.reviewer = Some(named(3, "banto"));
        theirs.summary.lineage_root = Some(named(1, "amy"));
        let keys = ribbon_keys(Some(pending_entry(&theirs)), me, false);
        assert!(keys.contains("[t]ake over"), "{keys}");
        assert!(!keys.contains("[e]scalate") && !keys.contains("[c]"), "{keys}");
    }

    #[test]
    fn closing_the_ribbon_forgets_the_filter() {
        let mut state = LedgerRibbonState {
            open: true,
            selected: 3,
            anchor: Some("x".into()),
            filter: "git".into(),
            filtering: true,
            retargeted_from: Some("y".into()),
        };
        state.close();
        assert!(!state.open && !state.filtering);
        assert!(state.filter.is_empty());
        assert_eq!((state.selected, state.anchor), (0, None));
        assert_eq!(state.retargeted_from, None);
    }

    // ── the systems ───────────────────────────────────────────────────────

    fn key_press(c: &str) -> KeyboardInput {
        KeyboardInput {
            key_code: KeyCode::KeyA,
            logical_key: bevy::input::keyboard::Key::Character(c.into()),
            state: bevy::input::ButtonState::Pressed,
            text: Some(c.into()),
            repeat: false,
            window: Entity::PLACEHOLDER,
        }
    }

    fn fire(app: &mut App, action: Action) {
        app.world_mut().write_message(ActionFired { action, context: InputContext::LedgerRibbon });
    }

    /// The whole path a player takes: `/`, type an id, Enter to keep it,
    /// Enter again on the one answered row it admits. The letters typed are
    /// answer keys and must not answer anything; the sheet opens on the
    /// closed ask, read-only.
    #[test]
    fn filtering_then_enter_on_an_answered_row_opens_the_closed_ask() {
        let me = principal(1);
        let mut app = App::new();
        app.init_resource::<Time>()
            .init_resource::<ButtonInput<KeyCode>>()
            .init_resource::<crate::input::prefix::PrefixState>()
            .init_resource::<LedgerMirror>()
            .init_resource::<crate::connection::drift::DriftState>()
            .init_resource::<LedgerRibbonState>()
            .init_resource::<AskSheetState>()
            .init_resource::<AskNotice>()
            .insert_resource(State::new(Screen::Conversation))
            .insert_resource(crate::cell::SessionPrincipal(Some(me)))
            .add_message::<ActionFired>()
            .add_message::<KeyboardInput>()
            .add_message::<AskDecisionRequested>()
            .add_message::<AskReassignRequested>()
            .add_systems(
                Update,
                (type_ledger_filter, sync_ledger_ribbon, handle_ledger_ribbon_actions).chain(),
            );
        let pending = ask("aaaaaaaa");
        let done = closed("cccccccc", AskStatus::Allowed, "allow_once", Some(named(1, "amy")));
        {
            let mut mirror = app.world_mut().resource_mut::<LedgerMirror>();
            mirror.apply_state(&kaijutsu_client::LedgerState::from_listing(
                1,
                vec![pending.summary.clone()],
            ));
            mirror.on_departed(done.clone());
        }
        app.world_mut().resource_mut::<LedgerRibbonState>().open = true;
        app.update();

        fire(&mut app, Action::LedgerFilter);
        app.update();
        assert!(app.world().resource::<LedgerRibbonState>().filtering);
        // Not `/` itself, and `a` types rather than allowing.
        for c in "cccccccc".chars() {
            app.world_mut().write_message(key_press(&c.to_string()));
        }
        fire(&mut app, Action::AskAllowOnce);
        app.update();
        let state = app.world().resource::<LedgerRibbonState>();
        assert_eq!(state.filter, "cccccccc");
        assert!(
            app.world().resource::<Messages<AskDecisionRequested>>().is_empty(),
            "a letter typed into the filter must not answer an ask"
        );

        fire(&mut app, Action::Activate);
        app.update();
        let state = app.world().resource::<LedgerRibbonState>();
        assert!(!state.filtering && state.open, "Enter keeps the filter and stays");
        assert_eq!(state.anchor.as_deref(), Some(done.summary.request_id.as_str()));

        fire(&mut app, Action::Activate);
        app.update();
        let sheet = app.world().resource::<AskSheetState>();
        assert_eq!(sheet.showing.as_deref(), Some(done.summary.request_id.as_str()));
        assert!(sheet.viewing_closed() && sheet.keys_armed());
        let ribbon = app.world().resource::<LedgerRibbonState>();
        assert!(!ribbon.open && ribbon.filter.is_empty(), "the ribbon closes and forgets the filter");
    }

    // ── chords, departures, status ───────────────────────────────────────

    fn typed(text: &str, code: KeyCode) -> KeyboardInput {
        KeyboardInput {
            key_code: code,
            logical_key: bevy::input::keyboard::Key::Character(text.into()),
            state: bevy::input::ButtonState::Pressed,
            text: Some(text.into()),
            repeat: false,
            window: Entity::PLACEHOLDER,
        }
    }

    fn fold(events: &[KeyboardInput], armed: bool, modifier: bool) -> (String, usize) {
        let mut text = String::new();
        let mut erase = 0;
        apply_filter_keys(events.iter(), armed, modifier, &mut text, &mut erase);
        (text, erase)
    }

    /// `Ctrl+A d` while filtering is a chord, not the letters `a` and `d`.
    #[test]
    fn a_ctrl_a_chord_is_not_typed_into_the_filter() {
        // The prefix armed on an earlier frame; this frame carries the chord key.
        assert_eq!(fold(&[typed("d", KeyCode::KeyD)], true, false), (String::new(), 0));
        // Both keys in one frame: Ctrl+A, then d.
        assert_eq!(
            fold(&[typed("a", KeyCode::KeyA), typed("d", KeyCode::KeyD)], false, true),
            (String::new(), 0)
        );
        // The key after the chord is text again.
        assert_eq!(
            fold(&[typed("d", KeyCode::KeyD), typed("x", KeyCode::KeyX)], true, false),
            ("x".to_string(), 0)
        );
        // Plain typing, including an `a` with no Ctrl held.
        assert_eq!(
            fold(&[typed("a", KeyCode::KeyA), typed("b", KeyCode::KeyB)], false, false),
            ("ab".to_string(), 0)
        );
    }

    #[test]
    fn backspace_erases_typed_text_first_then_the_filter() {
        let back = KeyboardInput {
            logical_key: bevy::input::keyboard::Key::Backspace,
            text: None,
            ..typed("", KeyCode::Backspace)
        };
        let (text, erase) = fold(&[typed("a", KeyCode::KeyA), back.clone(), back.clone()], false, false);
        assert_eq!((text.as_str(), erase), ("", 1));
    }

    /// System level: the armed prefix keeps the chord's letter out of the
    /// filter, and a closed ribbon drains keys without touching state.
    #[test]
    fn the_filter_system_skips_a_pending_chord() {
        let mut app = App::new();
        app.init_resource::<ButtonInput<KeyCode>>()
            .init_resource::<crate::input::prefix::PrefixState>()
            .init_resource::<LedgerRibbonState>()
            .add_message::<KeyboardInput>()
            .add_systems(Update, type_ledger_filter);
        {
            let mut r = app.world_mut().resource_mut::<LedgerRibbonState>();
            r.open = true;
            r.filtering = true;
            r.filter = "g".into();
        }
        app.world_mut().resource_mut::<crate::input::prefix::PrefixState>().arm();
        app.world_mut().write_message(typed("d", KeyCode::KeyD));
        app.update();
        assert_eq!(app.world().resource::<LedgerRibbonState>().filter, "g");
        app.world_mut().resource_mut::<crate::input::prefix::PrefixState>().disarm();
        app.world_mut().write_message(typed("i", KeyCode::KeyI));
        app.update();
        assert_eq!(app.world().resource::<LedgerRibbonState>().filter, "gi");
        // Closed: keys are read and dropped, the filter is untouched.
        app.world_mut().resource_mut::<LedgerRibbonState>().open = false;
        app.world_mut().write_message(typed("z", KeyCode::KeyZ));
        app.update();
        assert_eq!(app.world().resource::<LedgerRibbonState>().filter, "gi");
    }

    #[test]
    fn the_filter_reads_the_status_a_row_shows() {
        let mut gone = ask("01a04eaa");
        gone.summary.status = AskStatus::Expired;
        gone.summary.decided_at_ms = Some(0);
        assert!(admits(&gone, "expired", no_labels));
        assert!(!admits(&gone, "denied", no_labels));
    }

    fn ribbon_system_app(me: PrincipalId) -> App {
        let mut app = App::new();
        app.init_resource::<Time>()
            .init_resource::<ButtonInput<KeyCode>>()
            .init_resource::<crate::input::prefix::PrefixState>()
            .init_resource::<LedgerMirror>()
            .init_resource::<crate::connection::drift::DriftState>()
            .init_resource::<LedgerRibbonState>()
            .init_resource::<AskSheetState>()
            .init_resource::<AskNotice>()
            .insert_resource(State::new(Screen::Conversation))
            .insert_resource(crate::cell::SessionPrincipal(Some(me)))
            .add_message::<ActionFired>()
            .add_message::<KeyboardInput>()
            .add_message::<AskDecisionRequested>()
            .add_message::<AskReassignRequested>()
            .add_systems(
                Update,
                (type_ledger_filter, sync_ledger_ribbon, handle_ledger_ribbon_actions).chain(),
            );
        app
    }

    /// The ask under the cursor vanishes; the cursor lands on its neighbor.
    /// The next answer key must not answer the neighbor unseen.
    #[test]
    fn a_key_after_the_cursors_ask_left_is_refused_once() {
        let me = principal(1);
        let mut app = ribbon_system_app(me);
        let (a, b) = (ask("aaaaaaaa"), ask("bbbbbbbb"));
        let set = |app: &mut App, asks: &[&AskDetail], generation| {
            let list = asks.iter().map(|a| a.summary.clone()).collect();
            app.world_mut()
                .resource_mut::<LedgerMirror>()
                .apply_state(&kaijutsu_client::LedgerState::from_listing(generation, list));
        };
        set(&mut app, &[&a, &b], 1);
        app.world_mut().resource_mut::<LedgerRibbonState>().open = true;
        app.update();
        assert_eq!(app.world().resource::<LedgerRibbonState>().anchor.as_deref(), Some(a.summary.request_id.as_str()));

        // `a` leaves and its departure record has not landed: gone from the ledger.
        set(&mut app, &[&b], 2);
        app.update();
        let state = app.world().resource::<LedgerRibbonState>();
        assert_eq!(state.anchor.as_deref(), Some(b.summary.request_id.as_str()));
        assert_eq!(state.retargeted_from.as_deref(), Some(a.summary.request_id.as_str()));

        fire(&mut app, Action::AskAllowOnce);
        app.update();
        assert!(
            app.world().resource::<Messages<AskDecisionRequested>>().is_empty(),
            "the first key after a re-target answers nothing"
        );
        let said = app.world().resource::<AskNotice>().current(0.0).map(str::to_string);
        assert_eq!(said.as_deref(), Some("ask aaaaaaaa left; bbbbbbbb is selected \u{2014} press again"));

        // Pressing again is the player's own choice of the new row.
        fire(&mut app, Action::AskAllowOnce);
        app.update();
        assert_eq!(app.world().resource::<Messages<AskDecisionRequested>>().len(), 1);
    }

    /// A filter that hides the cursor's ask is the player's own act, not a
    /// departure.
    #[test]
    fn a_filter_hiding_the_cursor_is_not_a_departure() {
        let me = principal(1);
        let mut app = ribbon_system_app(me);
        let mut a = ask("aaaaaaaa");
        a.summary.statements = vec!["alpha".into()];
        let mut b = ask("bbbbbbbb");
        b.summary.statements = vec!["beta".into()];
        let list = vec![a.summary.clone(), b.summary.clone()];
        app.world_mut()
            .resource_mut::<LedgerMirror>()
            .apply_state(&kaijutsu_client::LedgerState::from_listing(1, list));
        app.world_mut().resource_mut::<LedgerRibbonState>().open = true;
        app.update();
        app.world_mut().resource_mut::<LedgerRibbonState>().filter = "beta".into();
        app.update();
        let state = app.world().resource::<LedgerRibbonState>();
        assert_eq!(state.anchor.as_deref(), Some(b.summary.request_id.as_str()));
        assert_eq!(state.retargeted_from, None);
    }

    #[test]
    fn departed_anchor_needs_the_ask_gone_from_the_whole_ledger() {
        assert_eq!(departed_anchor(&["b"], 1, Some("a")), Some("a".to_string()));
        assert_eq!(departed_anchor(&["a", "b"], 1, Some("a")), None, "hidden by a filter");
        assert_eq!(departed_anchor(&[], 0, Some("a")), None, "nothing to land on");
        assert_eq!(departed_anchor(&["a"], 1, None), None);
    }
}
