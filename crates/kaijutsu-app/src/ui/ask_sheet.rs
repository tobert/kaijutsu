//! The ask sheet — the app's answer to an approval ask waiting on you.
//!
//! An ask is a request the kernel recorded and will not act on until a
//! reviewer answers it (`docs/approval-identity.md`). The switchboard lamp
//! and `!n` on the hints line say one is waiting; this is the surface you
//! answer from.
//!
//! ## A popup over the screen, not a widget in it
//!
//! One centered MSDF text panel of [`ui::msdf_panel`](super::msdf_panel)'s
//! kind, `PANEL_WIDTH_FRACTION` of the frame wide, top-anchored under the
//! north dock — anchored rather than vertically centered so the panel grows
//! downward as a plan gets longer instead of sliding under the reader's eyes,
//! and so it sits directly beneath the `!n` marker that announced it.
//!
//! ## It raises itself
//!
//! An open ask in the context on screen raises the sheet on
//! `Screen::Conversation` and `Screen::Room`, and never on `Screen::Editor`
//! or `Screen::Diff` — a vi surface owns the keys wherever it is live
//! (`docs/input.md`, "Escape — two meanings total"). Someone else's ask
//! raises it too, read-only: [`AskSummary::answerable_by`] decides only
//! whether the answer keys are live. Esc puts the ask **aside**: the ask stays open, the `!n` marker stays, and
//! the sheet re-raises only on a context switch back or when a new ask
//! arrives.
//!
//! ## Decisions are ledger rows
//!
//! `a`/`A`/`d` write an [`AskDecisionRequested`] and change nothing here.
//! The mirror sends it as `decide_ask`, which authors no block in any
//! transcript. The ledger push that closes the ask is what takes the sheet
//! down, whichever surface answered — and once the closed ask's record is
//! read, a notice says what became of it. A refusal, a lost race, or an ask
//! that is not yours to answer all produce a notice too: a decision key never
//! does nothing silently.
//!
//! ## Honesty
//!
//! The PLAN block renders exactly the statements the ledger carries. There
//! is no plan tree and no per-statement verdict, because the wire has
//! neither; the block is shaped to take them when it does.

use std::collections::HashSet;
use std::time::Duration;

use bevy::input::keyboard::KeyboardInput;
use bevy::prelude::*;
use kaijutsu_client::AskArming;
use kaijutsu_types::{AskDetail, AskSummary, ContextId, PrincipalId, PrincipalRef};

use crate::connection::ledger::{
    short_request_id, AskDecisionRequested, AskDecisionSettled, AskLeft, Decision,
    DecisionOutcome, LedgerMirror,
};
use crate::input::events::ActionFired;
use crate::input::{Action, InputContext};
use crate::shaders::BlockFxMaterial;
use crate::text::msdf::MsdfBlockGlyphs;
use crate::ui::msdf_panel::{
    self as panel, columns, max_scroll, measure_char_width, rows_for_height, scroll_window,
    truncate, wrap,
};
use crate::ui::quick_context::{format_age, LineTone, PanelLine};
use crate::ui::screen::Screen;
use crate::ui::theme::Theme;
use crate::view::ui_rtt::UiRttTexture;

/// How long a notice stays on the hints line. Long enough to read a decision
/// you just made or a race you just lost, short enough that the line goes
/// back to being the keys.
pub const NOTICE_TTL: f64 = 6.0;

/// Continuation indent for a wrapped statement, in characters — enough that
/// a second row reads as more of the same statement rather than a new one.
const PLAN_INDENT: usize = 5;

// ============================================================================
// STATE
// ============================================================================

/// The one-line status notice both approval surfaces speak through.
///
/// It rides the dock's hints line (`ui::dock::update_hints`) rather than a
/// panel of its own: that is where the TUI puts it, it is visible on every
/// screen including the ones the sheet will not raise on, and it costs no new
/// dock furniture. [`expire_notice`] clears it, which is what makes the hints
/// line rebuild.
#[derive(Resource, Default, Debug)]
pub struct AskNotice {
    text: Option<String>,
    set_at: f64,
}

impl AskNotice {
    /// Say something, replacing whatever was there. The newest fact about an
    /// ask is the one worth reading.
    pub fn say(&mut self, text: impl Into<String>, now: f64) {
        self.text = Some(text.into());
        self.set_at = now;
    }

    /// What to show, or `None` once it has aged out.
    pub fn current(&self, now: f64) -> Option<&str> {
        self.text
            .as_deref()
            .filter(|_| now - self.set_at < NOTICE_TTL)
    }
}

/// Which ask the sheet is showing, what has been put aside, and where the
/// plan is scrolled to.
#[derive(Resource, Default, Debug)]
pub struct AskSheetState {
    /// The request id on screen, or `None` when the sheet is down.
    pub showing: Option<String>,
    /// Ids put aside with Esc. They stay open and keep their `!`; they
    /// simply do not raise the sheet again until the situation changes.
    pub aside: HashSet<String>,
    /// Open ids this state has already seen, so a genuinely NEW ask can be
    /// told from the same set read again — the arrival of one lifts the
    /// aside set.
    known: HashSet<String>,
    /// Asks whose sheet came down because they left the open set, each
    /// until its departure read lands ([`note_ask_departures`]).
    left: HashSet<String>,
    /// First plan row drawn (`j`/`k`).
    pub scroll: usize,
    /// The context the sheet last looked at, to notice a switch.
    context: Option<ContextId>,
    /// When the ask the sheet raised starts owning the keys, on `Time`'s
    /// elapsed clock ([`arm_ask_sheet`]).
    arming: Option<AskArming<Duration>>,
    /// Whether the sheet's keys are live — set on a frame that draws them.
    armed: bool,
}

impl AskSheetState {
    /// Whether the sheet has an ask to show.
    pub fn up(&self) -> bool {
        self.showing.is_some()
    }

    /// Whether the sheet owns the keyboard: an ask is up and its keys are
    /// armed. A disarmed sheet is on screen while keys go to the surface
    /// underneath.
    pub fn keys_armed(&self) -> bool {
        self.showing.is_some() && self.armed
    }

    /// Put `next` on screen as the sheet's own raise, at `now`. A raised
    /// ask starts disarmed. `]`/`[` step with keys that are already armed
    /// and do not come through here.
    pub fn raise(&mut self, next: Option<String>, now: Duration) {
        self.arming = next.as_ref().map(|_| AskArming::shown(now));
        self.armed = false;
        self.showing = next;
        self.scroll = 0;
    }

    /// Open `id` at the player's own request (the ribbon's Enter). The
    /// player chose this ask, so its keys are live at once.
    pub fn open(&mut self, id: String) {
        self.aside.remove(&id);
        self.arming = None;
        self.armed = true;
        self.showing = Some(id);
        self.scroll = 0;
    }
}

/// Marker for the sheet's positioned container.
#[derive(Component, Debug, Reflect)]
#[reflect(Component)]
pub struct AskSheetPanel;

/// Marker for the MSDF text surface inside it.
#[derive(Component, Debug, Reflect)]
#[reflect(Component)]
pub struct AskSheetSurface;

// ============================================================================
// PURE: WHICH ASK, AND WHEN
// ============================================================================

/// Whether a screen lets the sheet raise. A vi surface owns the keys where it
/// is live, so the sheet stays down there and the ask waits — the hints line
/// still carries `!n`.
pub fn screen_allows_sheet(screen: Screen) -> bool {
    matches!(screen, Screen::Conversation | Screen::Room)
}

/// Whether [`refresh_aside`] would change anything — the guard that keeps
/// `sync_ask_sheet` from marking the state changed on an idle frame.
pub fn aside_needs_refresh(
    state: &AskSheetState,
    pending_ids: &[&str],
    context_changed: bool,
) -> bool {
    (context_changed && !state.aside.is_empty())
        || pending_ids.iter().any(|id| !state.known.contains(*id))
        || state.known.len() != pending_ids.len()
        || state.aside.iter().any(|id| !pending_ids.contains(&id.as_str()))
}

/// Fold one refresh of the pending set into the aside bookkeeping.
///
/// The aside set is lifted by either thing that changes the situation: a
/// context switch (you looked away and came back) or a pending id nobody has
/// seen before (a new ask arrived). Ids that are no longer pending are
/// dropped, so `aside` cannot grow without bound.
pub fn refresh_aside(state: &mut AskSheetState, pending_ids: &[&str], context_changed: bool) {
    let arrived = pending_ids.iter().any(|id| !state.known.contains(*id));
    if context_changed || arrived {
        state.aside.clear();
    }
    state.known = pending_ids.iter().map(|id| id.to_string()).collect();
    state.aside.retain(|id| pending_ids.contains(&id.as_str()));
}

/// Which ask the sheet should show now.
///
/// An ask already on screen stays on screen while it is open, whatever
/// context it belongs to — `]`/`[` walk the whole open set, so the sheet must
/// not snap back to the current context between keypresses. A context switch
/// is the exception: attention moved, so the choice is made afresh from the
/// new context's own asks, skipping anything put aside. Whether the player
/// may answer does not matter here; it only decides whether the keys are
/// live.
pub fn raise_choice(
    showing: Option<&str>,
    open: &[AskSummary],
    current: Option<ContextId>,
    aside: &HashSet<String>,
    context_changed: bool,
) -> Option<String> {
    if !context_changed
        && let Some(id) = showing
        && open.iter().any(|ask| ask.request_id == id)
    {
        return Some(id.to_string());
    }
    let current = current?;
    open.iter()
        .find(|ask| ask.context_id == Some(current) && !aside.contains(&ask.request_id))
        .map(|ask| ask.request_id.clone())
}

/// Step to the next or previous pending ask, wrapping. `]`/`[` reach every
/// context's asks, which is what makes the sheet a review surface rather than
/// a per-context prompt.
pub fn step_showing(showing: Option<&str>, pending_ids: &[&str], delta: isize) -> Option<String> {
    if pending_ids.is_empty() {
        return None;
    }
    let at = showing
        .and_then(|id| pending_ids.iter().position(|p| *p == id))
        .unwrap_or(0) as isize;
    let len = pending_ids.len() as isize;
    let next = (at + delta).rem_euclid(len) as usize;
    Some(pending_ids[next].to_string())
}

/// The one-line outcome of an ask that left the open set (`docs/tui.md`,
/// "Asks"): `ask 01a04eb6 allow once by you`, `… by banto`, `… expired`.
///
/// The words come from the recorded decision option when there is one and
/// from the status when there is not — an expired or abandoned ask was never
/// decided by anyone, and saying "allow" about it would be a lie.
pub fn departure_notice(ask: &AskDetail, me: Option<PrincipalId>) -> String {
    let id = short_request_id(&ask.summary.request_id);
    let decision = ask.decision.as_ref();
    let Some(option) = decision
        .and_then(|d| d.option.as_deref())
        .filter(|s| !s.is_empty())
    else {
        return format!("ask {id} {}", ask.summary.status);
    };
    let words = option.replace('_', " ");
    // A remembered rule decides without a principal; there is no "by".
    let by = decision.and_then(|d| d.decided_by.as_ref()).map(|who| {
        if me == Some(who.id) {
            "you".to_string()
        } else {
            name_of(who)
        }
    });
    match by {
        Some(by) => format!("ask {id} {words} by {by}"),
        None => format!("ask {id} {words}"),
    }
}

/// The notice for an ask whose sheet came down but whose closed record could
/// not be read: it is closed, and that is all this app knows.
pub fn departure_unread_notice(request_id: &str) -> String {
    format!("ask {} closed", short_request_id(request_id))
}

/// The notice for a finished decision, or `None` when the ask closing says
/// it already.
pub fn settled_notice(request_id: &str, decision: Decision, outcome: &DecisionOutcome) -> Option<String> {
    let id = short_request_id(request_id);
    match outcome {
        // Taken: the ask leaving the open set says so, with the decider's
        // name attached. A second notice here would only race it.
        DecisionOutcome::Accepted { unlearned: None } => None,
        // The answer stands, but the rule the player asked for does not.
        DecisionOutcome::Accepted { unlearned: Some(note) } => {
            Some(format!("ask {id} {}, but no rule was learned: {note}", decision.label()))
        }
        DecisionOutcome::AlreadyDecided => {
            Some(format!("ask {id} was already answered \u{2014} {} lost the race", decision.label()))
        }
        DecisionOutcome::Failed(detail) => {
            Some(format!("ask {id} {} failed: {detail}", decision.label()))
        }
    }
}

/// The notice for a decision key on an ask this player may not answer.
pub fn not_yours_notice(ask: &AskSummary) -> String {
    match &ask.reviewer {
        Some(reviewer) => format!("not yours to answer: reviewer {}", name_of(reviewer)),
        None => "not yours to answer: this ask names no reviewer".to_string(),
    }
}

// ============================================================================
// PURE: THE ROWS
// ============================================================================

/// A principal's name for display, or its short id when the name is empty.
/// Never blank: "nobody is assigned" and "the name did not arrive" are
/// different facts.
pub fn name_of(who: &PrincipalRef) -> String {
    if who.name.is_empty() {
        who.id.short()
    } else {
        who.name.clone()
    }
}

/// A name for a slot an ask may leave empty: [`name_of`], or `\u{2014}` when
/// the ask records no one at all.
pub fn who(slot: &Option<PrincipalRef>) -> String {
    match slot {
        Some(who) => name_of(who),
        None => "\u{2014}".to_string(),
    }
}

/// How many wrapped lines of the escalation "why" the sheet shows before
/// it caps with a "+n more" marker (the full text is in `kj ledger show`).
const DESCRIPTION_MAX_ROWS: usize = 4;

/// The fixed rows above the plan: what the ask is, who it is between, and
/// what it says it will do.
pub fn sheet_head(ask: &AskDetail, me: Option<PrincipalId>, now_ms: i64, cols: usize) -> Vec<PanelLine> {
    let summary = &ask.summary;
    let mut rows = Vec::new();

    let mut header = format!("ask {}", short_request_id(&summary.request_id));
    if let Some(tool) = ask.tool.as_deref().filter(|t| !t.is_empty()) {
        header.push_str(&format!("  {tool}"));
    }
    header.push_str(&format!(
        "  waiting {}",
        format_age(now_ms.saturating_sub(summary.created_at_ms))
    ));
    rows.push(PanelLine {
        text: truncate(&header, cols),
        tone: LineTone::Head,
    });

    // Provenance, only what the ask actually carries.
    let mut provenance = vec![format!("origin {}", summary.origin)];
    for (label, value) in [
        ("instance", &ask.instance),
        ("hook", &ask.hook_id),
        ("label", &ask.label),
    ] {
        if let Some(value) = value.as_deref().filter(|v| !v.is_empty()) {
            provenance.push(format!("{label} {value}"));
        }
    }
    rows.push(PanelLine {
        text: truncate(&provenance.join("  "), cols),
        tone: LineTone::Dim,
    });

    // The heads: who asked, who acted, who may answer.
    let can_review = me.is_some_and(|me| summary.answerable_by(me));
    let mut heads = format!(
        "requester {}  performer {}  reviewer {}",
        who(&summary.requester),
        who(&summary.performer),
        who(&summary.reviewer),
    );
    if can_review {
        heads.push_str(" \u{00b7} you");
    }
    rows.push(PanelLine {
        text: truncate(&heads, cols),
        tone: LineTone::Row,
    });

    // Someone else's ask: say so, and say who would run what.
    if !can_review {
        for text in wrap(
            &format!(
                "not yours to answer \u{2014} the reviewer runs: kj ledger allow {}",
                short_request_id(&summary.request_id)
            ),
            cols,
            2,
        ) {
            rows.push(PanelLine {
                text,
                tone: LineTone::Warn,
            });
        }
    }

    rows
}

/// The scrolling rows: the plan as the gate recorded it, then the
/// environment it would run in.
///
/// The plan is the ask's `statements`, numbered, one per statement. That is
/// all the ledger carries today — no tree, no per-statement verdict — and
/// inventing either would put words in the kernel's mouth.
///
/// `gap` is [`LedgerMirror::record_gap`]: while the full record is unread,
/// the sheet says so where the environment would be, so an empty environment
/// is never mistaken for one the ask recorded.
pub fn sheet_body(ask: &AskDetail, gap: Option<&str>, cols: usize) -> Vec<PanelLine> {
    let summary = &ask.summary;
    let mut rows = vec![PanelLine {
        text: "PLAN".to_string(),
        tone: LineTone::Head,
    }];

    if summary.statements.is_empty() {
        rows.push(PanelLine {
            text: "no statements recorded".to_string(),
            tone: LineTone::Warn,
        });
    }
    for (i, statement) in summary.statements.iter().enumerate() {
        for text in wrap(&format!("{:>2}. {statement}", i + 1), cols, PLAN_INDENT) {
            rows.push(PanelLine {
                text,
                tone: LineTone::Row,
            });
        }
    }

    if let Some(gap) = gap {
        for text in wrap(gap, cols, 2) {
            rows.push(PanelLine {
                text,
                tone: LineTone::Warn,
            });
        }
    }
    if let Some(cwd) = ask.cwd.as_deref().filter(|c| !c.is_empty()) {
        for text in wrap(&format!("cwd {cwd}"), cols, 4) {
            rows.push(PanelLine {
                text,
                tone: LineTone::Dim,
            });
        }
    }
    for var in &ask.env {
        // `None` is "unset at ask time", which is a recorded value, not a
        // missing row (`kaijutsu_types::AskEnv`).
        let text = match &var.value {
            Some(value) => format!("env {}={value}", var.name),
            None => format!("env {} unset", var.name),
        };
        for text in wrap(&text, cols, 4) {
            rows.push(PanelLine {
                text,
                tone: LineTone::Dim,
            });
        }
    }
    if let Some(source) = ask.exec_source.as_deref().filter(|s| !s.is_empty()) {
        rows.push(PanelLine {
            text: truncate(&format!("source {source}"), cols),
            tone: LineTone::Dim,
        });
    }

    // Why the gate escalated, capped: the hook's or gate's note is useful but
    // secondary to the plan, so it sits below it and never crowds the
    // statement off the top. It scrolls with the rest of the body.
    if !summary.description.is_empty() {
        rows.push(PanelLine {
            text: "why".to_string(),
            tone: LineTone::Head,
        });
        let wrapped = wrap(&summary.description, cols, 2);
        let shown = wrapped.len().min(DESCRIPTION_MAX_ROWS);
        for text in wrapped.iter().take(shown) {
            rows.push(PanelLine {
                text: text.clone(),
                tone: LineTone::Dim,
            });
        }
        if wrapped.len() > shown {
            rows.push(PanelLine {
                text: format!("  \u{2026}+{} more", wrapped.len() - shown),
                tone: LineTone::Dim,
            });
        }
    }

    rows
}

/// The key line of a sheet whose keys are not armed yet. It stands in for
/// the keys, so the panel keeps its height when they arm.
pub const SHEET_ARMING_LINE: &str = "arming… keys still go where you were typing";

/// The key line, always the last row in the panel.
///
/// An ask this player may not answer offers no approval keys — the sheet is
/// then a window onto someone else's decision, and a key that would be
/// refused has no business being advertised (`docs/tui.md`, "Asks").
pub fn sheet_keys(can_review: bool, pending_total: usize) -> String {
    let mut keys = String::new();
    if can_review {
        keys.push_str("[a]llow once  [A]llow always  [d]eny  ");
    }
    keys.push_str("[v]iew ledger  Esc aside");
    if pending_total > 1 {
        keys.push_str("  ] next  [ prev");
    }
    keys
}

/// Every row of the sheet, in order, for a panel this many characters wide
/// and this many rows tall. `armed` is [`AskSheetState::keys_armed`]; `gap`
/// is as [`sheet_body`] takes it.
///
/// The key line is laid down last and the plan scrolls above it, so it never
/// leaves the bottom edge however long the plan gets.
pub fn sheet_rows(
    ask: &AskDetail,
    gap: Option<&str>,
    me: Option<PrincipalId>,
    now_ms: i64,
    pending_total: usize,
    cols: usize,
    rows: usize,
    scroll: usize,
    armed: bool,
) -> Vec<PanelLine> {
    let head = sheet_head(ask, me, now_ms, cols);
    let body = sheet_body(ask, gap, cols);
    let keys = if armed {
        let can_review = me.is_some_and(|me| ask.summary.answerable_by(me));
        PanelLine {
            text: truncate(&sheet_keys(can_review, pending_total), cols),
            tone: LineTone::Head,
        }
    } else {
        PanelLine { text: truncate(SHEET_ARMING_LINE, cols), tone: LineTone::Dim }
    };

    // The head and the key line are never scrolled away; whatever is left
    // over is the plan's window, and at minimum it is one row.
    let body_rows = rows.saturating_sub(head.len() + 1).max(1);
    let mut out = head;
    out.extend(scroll_window(&body, scroll, body_rows));
    out.push(keys);
    out
}

/// How far `j` may scroll the plan for a panel of this shape.
pub fn sheet_max_scroll(
    ask: &AskDetail,
    gap: Option<&str>,
    me: Option<PrincipalId>,
    now_ms: i64,
    cols: usize,
    rows: usize,
) -> usize {
    let head_len = sheet_head(ask, me, now_ms, cols).len();
    let body_rows = rows.saturating_sub(head_len + 1).max(1);
    max_scroll(sheet_body(ask, gap, cols).len(), body_rows)
}

// ============================================================================
// SYSTEMS
// ============================================================================

/// Decide what the sheet shows this frame. An ask on screen that left the
/// open set takes the sheet down, and [`note_ask_departures`] says what
/// became of it once its closed record is read.
///
/// Runs before `input::context::sync_input_context` so the contexts derived
/// this frame already know whether the sheet is up.
/// Every write here is guarded: a `ResMut` deref marks the resource changed,
/// and `input::context::sync_input_context` gates on that, so touching this
/// state unconditionally would rederive the whole context set every frame.
pub fn sync_ask_sheet(
    mirror: Res<LedgerMirror>,
    doc_cache: Res<crate::view::document::DocumentCache>,
    screen: Res<State<Screen>>,
    time: Res<Time>,
    mut state: ResMut<AskSheetState>,
) {
    let current = doc_cache.active_id();
    let context_changed = state.context != current;

    let open_ids: Vec<&str> = mirror
        .open()
        .iter()
        .map(|ask| ask.request_id.as_str())
        .collect();

    // An ask that was on screen and is no longer open is waiting for its
    // closing notice.
    if let Some(showing) = state.showing.as_deref()
        && !open_ids.contains(&showing)
        && !state.left.contains(showing)
    {
        let showing = showing.to_string();
        state.left.insert(showing);
    }

    if !screen_allows_sheet(*screen.get()) {
        // A vi surface has the keys. Put the sheet down without putting the
        // ask aside — it is still waiting, and the hints line still says so.
        if state.showing.is_some() {
            state.raise(None, time.elapsed());
        }
        if context_changed {
            state.context = current;
        }
        return;
    }

    if aside_needs_refresh(&state, &open_ids, context_changed) {
        refresh_aside(&mut state, &open_ids, context_changed);
    }
    let next = raise_choice(
        state.showing.as_deref(),
        mirror.open(),
        current,
        &state.aside,
        context_changed,
    );
    if next != state.showing {
        state.raise(next, time.elapsed());
    }
    if context_changed {
        state.context = current;
    }
}

/// Arm the raised ask's keys once its hold has passed
/// ([`kaijutsu_client::ask_arming`]).
///
/// Every key pressed while the sheet is disarmed pushes arming out, and the
/// sheet arms only on a frame with no key in it, so the frame that draws the
/// keys comes before any key that uses them. Runs after [`sync_ask_sheet`]
/// and before `input::context::sync_input_context`, which reads
/// [`AskSheetState::keys_armed`]. It reads raw key presses only to time
/// them; the dispatcher still routes every key.
pub fn arm_ask_sheet(
    mut keyboard: MessageReader<KeyboardInput>,
    time: Res<Time>,
    mut state: ResMut<AskSheetState>,
) {
    let pressed = keyboard.read().filter(|event| event.state.is_pressed()).count();
    if state.showing.is_none() || state.armed {
        return;
    }
    let now = time.elapsed();
    if pressed > 0 {
        // Moving the deadline changes nothing on screen or in the derived
        // contexts, so it does not mark the state changed.
        let state = state.bypass_change_detection();
        state.arming.get_or_insert_with(|| AskArming::shown(now)).keystroke(now);
    } else if state.arming.is_none_or(|arming| arming.armed(now)) {
        state.armed = true;
    }
}

/// The sheet's keys: three decisions, the ledger, two steps, a scroll, and
/// Esc.
#[allow(clippy::too_many_arguments)]
pub fn handle_ask_sheet_actions(
    mut actions: MessageReader<ActionFired>,
    mirror: Res<LedgerMirror>,
    session: Res<crate::cell::SessionPrincipal>,
    time: Res<Time>,
    mut state: ResMut<AskSheetState>,
    mut notice: ResMut<AskNotice>,
    mut decisions: MessageWriter<AskDecisionRequested>,
    mut ribbon: ResMut<super::ledger_ribbon::LedgerRibbonState>,
) {
    for ActionFired { action, context } in actions.read() {
        if *context != InputContext::AskSheet {
            continue;
        }
        let Some(showing) = state.showing.clone() else {
            continue;
        };

        match action {
            Action::AskAllowOnce | Action::AskAllowAlways | Action::AskDeny => {
                let decision = match action {
                    Action::AskAllowOnce => Decision::AllowOnce,
                    Action::AskAllowAlways => Decision::AllowAlways,
                    _ => Decision::Deny,
                };
                // The ask may have left the open set between the frame
                // that drew the key line and this one.
                let Some(ask) = mirror.summary(&showing) else {
                    notice.say(
                        format!("ask {} is no longer open", short_request_id(&showing)),
                        time.elapsed_secs_f64(),
                    );
                    continue;
                };
                if !session.0.is_some_and(|me| ask.answerable_by(me)) {
                    notice.say(not_yours_notice(ask), time.elapsed_secs_f64());
                    continue;
                }
                decisions.write(AskDecisionRequested {
                    request_id: showing.clone(),
                    decision,
                });
            }

            // `v` — the ledger over the sheet. The ask stays open and the
            // sheet stays raised behind it.
            Action::OpenLedger => ribbon.open = true,

            Action::AskNext | Action::AskPrev => {
                let ids: Vec<&str> = mirror
                    .open()
                    .iter()
                    .map(|ask| ask.request_id.as_str())
                    .collect();
                let delta = if matches!(action, Action::AskNext) { 1 } else { -1 };
                let next = step_showing(Some(showing.as_str()), &ids, delta);
                if next != state.showing {
                    state.showing = next;
                    state.scroll = 0;
                }
            }

            Action::StepNext => state.scroll = state.scroll.saturating_add(1),
            Action::StepPrev => state.scroll = state.scroll.saturating_sub(1),

            // Esc — aside, with the ask still open.
            Action::AskAside => {
                state.aside.insert(showing);
                state.showing = None;
                state.scroll = 0;
            }

            _ => {}
        }
    }
}

/// Turn a refused or raced decision into a notice. An accepted one needs
/// none: the ask leaving the open set says it, with the decider attached.
pub fn note_ask_decisions(
    mut settled: MessageReader<AskDecisionSettled>,
    time: Res<Time>,
    mut notice: ResMut<AskNotice>,
) {
    for AskDecisionSettled {
        request_id,
        decision,
        outcome,
    } in settled.read()
    {
        if let Some(text) = settled_notice(request_id, *decision, outcome) {
            notice.say(text, time.elapsed_secs_f64());
        }
    }
}

/// Say what became of the ask whose sheet came down, once its closed record
/// is read. Asks that closed while off screen get no notice; the ribbon's
/// ANSWERED rows carry them.
pub fn note_ask_departures(
    mut left: MessageReader<AskLeft>,
    session: Res<crate::cell::SessionPrincipal>,
    time: Res<Time>,
    mut state: ResMut<AskSheetState>,
    mut notice: ResMut<AskNotice>,
) {
    for AskLeft { request_id, record } in left.read() {
        // Clearing the wait changes nothing the context derivation reads.
        if !state.bypass_change_detection().left.remove(request_id) {
            continue;
        }
        let text = match record {
            Some(record) => departure_notice(record, session.0),
            None => departure_unread_notice(request_id),
        };
        notice.say(text, time.elapsed_secs_f64());
    }
}

/// Age the notice out. A write here is what makes the hints line rebuild.
pub fn expire_notice(time: Res<Time>, mut notice: ResMut<AskNotice>) {
    if notice.text.is_some() && notice.current(time.elapsed_secs_f64()).is_none() {
        notice.text = None;
    }
}

/// Show or hide the sheet. The ribbon, when open, is in front of it.
pub fn sync_ask_sheet_visibility(
    state: Res<AskSheetState>,
    ribbon: Res<super::ledger_ribbon::LedgerRibbonState>,
    mut panels: Query<&mut Visibility, With<AskSheetPanel>>,
) {
    if !state.is_changed() && !ribbon.is_changed() {
        return;
    }
    let want = if state.up() && !ribbon.open {
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

/// Spawn the sheet, hidden, as a child of the tiling root.
pub fn spawn_ask_sheet(
    mut commands: Commands,
    theme: Res<Theme>,
    mut fx_materials: ResMut<Assets<BlockFxMaterial>>,
    tiling_root: Query<Entity, With<super::tiling_reconciler::TilingRoot>>,
    existing: Query<Entity, With<AskSheetPanel>>,
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
        crate::constants::ZLayer::ASK_SHEET,
        AskSheetPanel,
        AskSheetSurface,
    );
}

/// Rebuild the sheet's glyphs.
///
/// `PostUpdate`, after layout and before `resize_block_textures` — the
/// contract every hand-rolled MSDF surface in the app honors. Skips entirely
/// while the sheet is down.
#[allow(clippy::too_many_arguments)]
pub fn render_ask_sheet(
    state: Res<AskSheetState>,
    mirror: Res<LedgerMirror>,
    session: Res<crate::cell::SessionPrincipal>,
    theme: Res<Theme>,
    time: Res<Time>,
    windows: Query<&Window, With<bevy::window::PrimaryWindow>>,
    mut fonts: crate::ui::msdf_panel::PanelFonts,
    mut surface: Query<(&mut MsdfBlockGlyphs, &mut UiRttTexture, &mut Node), With<AskSheetSurface>>,
    mut container: Query<&mut Node, (With<AskSheetPanel>, Without<AskSheetSurface>)>,
    mut last_build: Local<f64>,
) {
    let Some(showing) = state.showing.clone() else {
        return;
    };
    let now_secs = time.elapsed_secs_f64();
    // The waiting age is the only thing that moves on its own, and it moves
    // at second resolution at best.
    let stale = now_secs - *last_build >= 1.0;
    if !state.is_changed() && !mirror.is_changed() && !theme.is_changed() && !stale {
        return;
    }
    let Some(ask) = mirror.record(&showing) else {
        return;
    };
    let gap = mirror.record_gap(&showing);
    let Ok(window) = windows.single() else {
        return;
    };
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

    let width = panel::panel_width(window.width()) as f64;
    let max_height = (window.height() * panel::PANEL_MAX_HEIGHT_FRACTION) as f64;
    let cols = columns(width, measure_char_width(font));
    let rows = rows_for_height(max_height);

    let lines = sheet_rows(
        &ask,
        gap.as_deref(),
        session.0,
        kaijutsu_types::now_millis() as i64,
        mirror.pending_count(),
        cols,
        rows,
        state.scroll,
        state.keys_armed(),
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
        outer.left = Val::Px(panel::panel_left(window.width()));
        outer.width = Val::Px(width as f32);
    }
}

/// Clamp the plan scroll to what the panel can actually show, so `j` at the
/// bottom stops instead of paging into nothing.
pub fn clamp_ask_sheet_scroll(
    mirror: Res<LedgerMirror>,
    session: Res<crate::cell::SessionPrincipal>,
    windows: Query<&Window, With<bevy::window::PrimaryWindow>>,
    fonts: Res<Assets<crate::text::shaping::VelloFont>>,
    handles: Res<crate::text::ShapingFonts>,
    mut state: ResMut<AskSheetState>,
) {
    if !state.is_changed() || state.scroll == 0 {
        return;
    }
    let Some(showing) = state.showing.clone() else {
        return;
    };
    let (Some(ask), Ok(window), Some(font)) = (
        mirror.record(&showing),
        windows.single(),
        fonts.get(&handles.mono),
    ) else {
        return;
    };
    let width = panel::panel_width(window.width()) as f64;
    let cols = columns(width, measure_char_width(font));
    let rows = rows_for_height((window.height() * panel::PANEL_MAX_HEIGHT_FRACTION) as f64);
    let limit = sheet_max_scroll(
        &ask,
        mirror.record_gap(&showing).as_deref(),
        session.0,
        kaijutsu_types::now_millis() as i64,
        cols,
        rows,
    );
    if state.scroll > limit {
        state.scroll = limit;
    }
}

// ============================================================================
// PLUGIN
// ============================================================================

pub struct AskSheetPlugin;

impl Plugin for AskSheetPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<AskSheetState>()
            .init_resource::<AskNotice>()
            .register_type::<AskSheetPanel>()
            .register_type::<AskSheetSurface>()
            // PostStartup, like the docks: TilingRoot is spawned in Startup.
            .add_systems(PostStartup, spawn_ask_sheet)
            .add_systems(
                Update,
                // Before the context derivation, so the keys this frame
                // already belong to the sheet when it raises.
                (sync_ask_sheet, arm_ask_sheet)
                    .chain()
                    .in_set(crate::input::InputPhase::SyncContext)
                    .before(crate::input::context::sync_input_context),
            )
            .add_systems(
                Update,
                (
                    handle_ask_sheet_actions,
                    note_ask_decisions,
                    note_ask_departures,
                    expire_notice,
                    clamp_ask_sheet_scroll,
                    sync_ask_sheet_visibility,
                )
                    .chain()
                    .in_set(crate::input::InputPhase::Handle),
            )
            .add_systems(
                PostUpdate,
                render_ask_sheet
                    .after(bevy::ui::UiSystems::Layout)
                    .before(crate::view::block_render::resize_block_textures),
            );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kaijutsu_types::{AskDecision, AskEnv, AskOrigin, AskStatus};

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

    fn named(id: PrincipalId, name: &str) -> PrincipalRef {
        PrincipalRef { id, name: name.into() }
    }

    /// An open ask the given principal may review: performed by `coder`,
    /// reviewed by `reviewer`.
    fn ask(id: &str, context: Option<ContextId>, reviewer: PrincipalId) -> AskDetail {
        AskDetail {
            summary: AskSummary {
                request_id: format!("{id}-aaaa-bbbb-cccc-000000000001"),
                status: AskStatus::Pending,
                origin: AskOrigin::ShellGate,
                context_id: context,
                description: "rm -rf ~/src/wt/kaish-arith".into(),
                statements: vec!["rm -rf ~/src/wt/kaish-arith".into()],
                requester: Some(named(principal(9), "amy")),
                performer: Some(named(principal(4), "coder")),
                reviewer: Some(named(reviewer, "amy")),
                created_at_ms: 0,
                decided_at_ms: None,
            },
            instance: Some("kaish-1".into()),
            tool: Some("shell_write".into()),
            hook_id: None,
            label: None,
            tool_call_block_id: None,
            exec_source: Some("kaish".into()),
            cwd: Some("/home/amy/src/wt/kaish-arith".into()),
            env: Vec::new(),
            decision: None,
            redeemed_at_ms: None,
            publication_abandoned: None,
            reassignments: Vec::new(),
        }
    }

    /// The open set a list of asks makes, as the mirror holds it.
    fn open(asks: &[AskDetail]) -> Vec<AskSummary> {
        asks.iter().map(|ask| ask.summary.clone()).collect()
    }

    /// `ask` closed by `by` with `option`.
    fn decided(mut ask: AskDetail, option: &str, by: Option<PrincipalRef>) -> AskDetail {
        ask.summary.status = AskStatus::Allowed;
        ask.decision = Some(AskDecision {
            decided_by: by,
            option: Some(option.into()),
            remember_scope: None,
            auto_reason: None,
        });
        ask
    }

    fn aside(ids: &[&str]) -> HashSet<String> {
        ids.iter().map(|id| id.to_string()).collect()
    }

    fn full_id(id: &str) -> String {
        format!("{id}-aaaa-bbbb-cccc-000000000001")
    }

    fn text(rows: &[PanelLine]) -> String {
        rows.iter().map(|r| r.text.clone()).collect::<Vec<_>>().join(" | ")
    }

    // ── arming ────────────────────────────────────────────────────────────

    fn ms(n: u64) -> std::time::Duration {
        std::time::Duration::from_millis(n)
    }

    /// The arming system alone, over a clock the test sets.
    fn arming_app() -> App {
        let mut app = App::new();
        app.init_resource::<Time>()
            .init_resource::<AskSheetState>()
            .add_message::<bevy::input::keyboard::KeyboardInput>()
            .add_systems(Update, arm_ask_sheet);
        app
    }

    /// The sheet raises its own ask at `at`.
    fn raise_at(app: &mut App, at: u64) {
        app.world_mut().resource_mut::<Time>().advance_to(ms(at));
        app.world_mut()
            .resource_mut::<AskSheetState>()
            .raise(Some(full_id("aaaa")), ms(at));
    }

    /// One frame at `at`, with a key pressed in it or not. Returns whether
    /// the sheet owns the keys after the frame, which is what the next
    /// frame's context derivation reads.
    fn frame_at(app: &mut App, at: u64, key: bool) -> bool {
        app.world_mut().resource_mut::<Time>().advance_to(ms(at));
        if key {
            app.world_mut()
                .resource_mut::<Messages<bevy::input::keyboard::KeyboardInput>>()
                .write(bevy::input::keyboard::KeyboardInput {
                    key_code: KeyCode::KeyA,
                    logical_key: bevy::input::keyboard::Key::Character("a".into()),
                    state: bevy::input::ButtonState::Pressed,
                    text: Some("a".into()),
                    repeat: false,
                    window: Entity::PLACEHOLDER,
                });
        }
        app.update();
        app.world().resource::<AskSheetState>().keys_armed()
    }

    /// A freshly raised ask does not own the keys; its keys arm one delay
    /// after it appears (`docs/input.md`, "The approval surfaces").
    #[test]
    fn a_raised_ask_arms_one_hundred_ms_after_it_appears() {
        let mut app = arming_app();
        raise_at(&mut app, 1_000);
        assert!(!frame_at(&mut app, 1_000, false), "a new ask is disarmed");
        assert!(!frame_at(&mut app, 1_099, false));
        assert!(frame_at(&mut app, 1_100, false), "armed at 100 ms");
    }

    /// A key inside the first 100 ms is not the sheet's: the frame it lands
    /// in derives contexts with the sheet disarmed, so it goes to the draft.
    #[test]
    fn a_key_inside_the_first_hundred_ms_is_not_the_sheets() {
        let mut app = arming_app();
        raise_at(&mut app, 1_000);
        assert!(!frame_at(&mut app, 1_050, true));
        assert_eq!(
            app.world().resource::<AskSheetState>().showing,
            Some(full_id("aaaa")),
            "the ask stays on screen, pending"
        );
    }

    /// Typing at 80 ms moves arming to 280 ms: a key at 250 ms is still not
    /// the sheet's.
    #[test]
    fn typing_holds_the_sheet_disarmed_and_a_key_inside_the_hold_passes() {
        let mut app = arming_app();
        raise_at(&mut app, 1_000);
        assert!(!frame_at(&mut app, 1_080, true));
        assert!(!frame_at(&mut app, 1_200, false), "100 ms has passed, but typing held it");
        assert!(!frame_at(&mut app, 1_250, true));
    }

    /// Typing at 80 ms moves arming to 280 ms: at 290 ms the sheet owns the
    /// keys, so a key there answers.
    #[test]
    fn typing_holds_the_sheet_disarmed_and_a_key_after_the_hold_answers() {
        let mut app = arming_app();
        raise_at(&mut app, 1_000);
        assert!(!frame_at(&mut app, 1_080, true));
        assert!(!frame_at(&mut app, 1_279, false));
        assert!(frame_at(&mut app, 1_285, false));
        assert!(frame_at(&mut app, 1_290, true), "an armed sheet stays armed through a key");
    }

    /// The sheet arms on a frame with no key in it, so the frame that
    /// draws its keys comes before any key that uses them.
    #[test]
    fn the_sheet_arms_only_on_a_frame_without_a_key() {
        let mut app = arming_app();
        raise_at(&mut app, 1_000);
        assert!(!frame_at(&mut app, 1_150, true), "past the delay, but a key landed");
        assert!(frame_at(&mut app, 1_166, false));
    }

    /// An ask the player opens from the ribbon is theirs to answer at once.
    #[test]
    fn an_ask_opened_from_the_ribbon_is_armed_at_once() {
        let mut app = arming_app();
        raise_at(&mut app, 1_000);
        app.world_mut().resource_mut::<AskSheetState>().open(full_id("bbbb"));
        assert!(frame_at(&mut app, 1_010, true));
    }

    /// A different ask raised in its place starts disarmed again.
    #[test]
    fn each_raised_ask_starts_disarmed() {
        let mut app = arming_app();
        raise_at(&mut app, 1_000);
        assert!(frame_at(&mut app, 1_100, false));
        app.world_mut()
            .resource_mut::<AskSheetState>()
            .raise(Some(full_id("bbbb")), ms(1_200));
        assert!(!frame_at(&mut app, 1_200, false));
        assert!(frame_at(&mut app, 1_300, false));
    }

    /// Arming is presentation: the key line says the keys are not live
    /// yet, in place of the keys, so the panel does not change height.
    #[test]
    fn a_disarmed_sheet_says_so_instead_of_offering_keys() {
        let me = principal(1);
        let a = ask("01a04eb6", Some(ctx(1)), me);
        let disarmed = sheet_rows(&a, None, Some(me), 0, 1, 100, 14, 0, false);
        let armed = sheet_rows(&a, None, Some(me), 0, 1, 100, 14, 0, true);
        let last = disarmed.last().expect("rows");
        assert_eq!(last.text, SHEET_ARMING_LINE);
        assert_eq!(last.tone, LineTone::Dim);
        assert!(!text(&disarmed).contains("llow"), "{}", text(&disarmed));
        assert_eq!(disarmed.len(), armed.len());
    }

    // ── when the sheet raises ─────────────────────────────────────────────

    /// A vi surface owns the keys where it is live, so the sheet never
    /// raises over the editor or the diff viewer.
    #[test]
    fn only_the_conversation_and_the_room_raise_the_sheet() {
        assert!(screen_allows_sheet(Screen::Conversation));
        assert!(screen_allows_sheet(Screen::Room));
        assert!(!screen_allows_sheet(Screen::Editor));
        assert!(!screen_allows_sheet(Screen::Diff));
    }

    #[test]
    fn a_pending_ask_in_the_current_context_raises_the_sheet() {
        let me = principal(1);
        let pending = vec![ask("aaaa", Some(ctx(2)), me), ask("bbbb", Some(ctx(1)), me)];
        assert_eq!(
            raise_choice(None, &open(&pending), Some(ctx(1)), &HashSet::new(), false),
            Some(full_id("bbbb")),
            "the current context's ask, not merely the first pending one"
        );
    }

    /// An ask in another context does not raise the sheet by itself — the
    /// hints line carries it, and `Ctrl+A l` reaches it.
    #[test]
    fn an_ask_in_another_context_does_not_raise_the_sheet() {
        let pending = vec![ask("aaaa", Some(ctx(2)), principal(1))];
        assert_eq!(
            raise_choice(None, &open(&pending), Some(ctx(1)), &HashSet::new(), false),
            None
        );
    }

    /// Esc puts the ask aside with the ask still pending. The sheet must not
    /// raise it again on the next refresh — that is the whole point of aside.
    #[test]
    fn an_ask_put_aside_stays_down_while_nothing_changes() {
        let pending = vec![ask("aaaa", Some(ctx(1)), principal(1))];
        assert_eq!(
            raise_choice(None, &open(&pending), Some(ctx(1)), &aside(&[&full_id("aaaa")]), false),
            None
        );
    }

    /// Stepping with `]` walks off the current context, and the sheet keeps
    /// showing where you walked to.
    #[test]
    fn an_ask_on_screen_stays_on_screen_while_it_is_pending() {
        let me = principal(1);
        let pending = vec![ask("aaaa", Some(ctx(9)), me), ask("bbbb", Some(ctx(1)), me)];
        assert_eq!(
            raise_choice(
                Some(&full_id("aaaa")),
                &open(&pending),
                Some(ctx(1)),
                &HashSet::new(),
                false
            ),
            Some(full_id("aaaa")),
            "]/[ reach every context; the choice must not snap back"
        );
    }

    /// A context switch is a change of attention: the choice is made afresh
    /// from the new context's asks.
    #[test]
    fn a_context_switch_rechooses_from_the_new_context() {
        let me = principal(1);
        let pending = vec![ask("aaaa", Some(ctx(9)), me), ask("bbbb", Some(ctx(1)), me)];
        assert_eq!(
            raise_choice(
                Some(&full_id("aaaa")),
                &open(&pending),
                Some(ctx(1)),
                &HashSet::new(),
                true
            ),
            Some(full_id("bbbb"))
        );
    }

    #[test]
    fn a_decided_ask_leaves_the_sheet_with_nothing_to_show() {
        assert_eq!(
            raise_choice(Some(&full_id("aaaa")), &[], Some(ctx(1)), &HashSet::new(), false),
            None
        );
    }

    // ── the aside set ─────────────────────────────────────────────────────

    #[test]
    fn a_context_switch_lifts_the_aside_set() {
        let mut state = AskSheetState::default();
        state.aside = aside(&["a"]);
        state.known = aside(&["a"]);
        refresh_aside(&mut state, &["a"], true);
        assert!(state.aside.is_empty(), "coming back is a fresh look");
    }

    /// A new ask arriving changes the situation, so what was put aside is
    /// offered again.
    #[test]
    fn a_new_pending_ask_lifts_the_aside_set() {
        let mut state = AskSheetState::default();
        state.aside = aside(&["a"]);
        state.known = aside(&["a"]);
        refresh_aside(&mut state, &["a", "b"], false);
        assert!(state.aside.is_empty());
    }

    /// The same set polled again is not news, and must not undo an Esc.
    #[test]
    fn re_reading_the_same_pending_set_keeps_the_aside_set() {
        let mut state = AskSheetState::default();
        state.aside = aside(&["a"]);
        state.known = aside(&["a", "b"]);
        refresh_aside(&mut state, &["a", "b"], false);
        assert_eq!(state.aside, aside(&["a"]));
    }

    /// An answered ask's id must not sit in `aside` forever.
    #[test]
    fn the_aside_set_drops_ids_that_are_no_longer_pending() {
        let mut state = AskSheetState::default();
        state.aside = aside(&["a", "b"]);
        state.known = aside(&["a", "b"]);
        refresh_aside(&mut state, &["a"], false);
        assert_eq!(state.aside, aside(&["a"]));
    }

    /// An idle frame must not touch the state: `sync_input_context` gates on
    /// its change tick, so a `ResMut` deref every frame would rederive the
    /// whole context set every frame.
    #[test]
    fn an_idle_frame_needs_no_aside_refresh() {
        let mut state = AskSheetState::default();
        state.aside = aside(&["a"]);
        state.known = aside(&["a", "b"]);
        assert!(!aside_needs_refresh(&state, &["a", "b"], false));

        // ...and every case that DOES change something asks for the write.
        assert!(aside_needs_refresh(&state, &["a", "b"], true), "a context switch");
        assert!(aside_needs_refresh(&state, &["a", "b", "c"], false), "a new ask");
        assert!(aside_needs_refresh(&state, &["b"], false), "an ask answered");
        assert!(
            !aside_needs_refresh(&AskSheetState::default(), &[], true),
            "nothing aside, nothing to lift"
        );
    }

    // ── stepping ──────────────────────────────────────────────────────────

    #[test]
    fn stepping_wraps_around_the_whole_pending_set() {
        let ids = ["a", "b", "c"];
        assert_eq!(step_showing(Some("a"), &ids, 1).as_deref(), Some("b"));
        assert_eq!(step_showing(Some("c"), &ids, 1).as_deref(), Some("a"));
        assert_eq!(step_showing(Some("a"), &ids, -1).as_deref(), Some("c"));
    }

    #[test]
    fn stepping_with_nothing_pending_shows_nothing() {
        assert_eq!(step_showing(Some("a"), &[], 1), None);
    }

    /// An id that is no longer pending steps from the top rather than
    /// nowhere.
    #[test]
    fn stepping_from_an_unknown_ask_lands_on_the_first() {
        assert_eq!(step_showing(Some("gone"), &["a", "b"], 1).as_deref(), Some("b"));
    }

    // ── notices ───────────────────────────────────────────────────────────

    #[test]
    fn a_decision_of_yours_reads_as_by_you() {
        let me = principal(1);
        let closed = decided(ask("01a04eb6", Some(ctx(1)), me), "allow_once", Some(named(me, "amy")));
        assert_eq!(
            departure_notice(&closed, Some(me)),
            "ask 01a04eb6 allow once by you"
        );
    }

    #[test]
    fn someone_elses_decision_names_them() {
        let closed = decided(
            ask("01a04eb6", Some(ctx(1)), principal(1)),
            "allow_always",
            Some(named(principal(7), "banto")),
        );
        assert_eq!(
            departure_notice(&closed, Some(principal(1))),
            "ask 01a04eb6 allow always by banto"
        );

        // No name on the sheet: the short principal id, never a blank.
        let nameless = decided(
            ask("01a04eb6", Some(ctx(1)), principal(1)),
            "allow_always",
            Some(named(principal(7), "")),
        );
        assert_eq!(
            departure_notice(&nameless, Some(principal(1))),
            format!("ask 01a04eb6 allow always by {}", principal(7).short())
        );
    }

    /// An expired ask was never decided by anyone. Saying "allow" about it
    /// would be a lie, so the status is what gets said.
    #[test]
    fn an_ask_that_expired_says_expired_and_names_nobody() {
        let mut gone = ask("01a04eb6", Some(ctx(1)), principal(1));
        gone.summary.status = AskStatus::Expired;
        assert_eq!(departure_notice(&gone, Some(principal(1))), "ask 01a04eb6 expired");
    }

    /// A remembered rule decides with no principal behind it.
    #[test]
    fn an_auto_decision_names_no_decider() {
        let auto = decided(ask("01a04eb6", Some(ctx(1)), principal(1)), "auto_allow", None);
        assert_eq!(departure_notice(&auto, Some(principal(1))), "ask 01a04eb6 auto allow");
    }

    #[test]
    fn a_closed_ask_whose_record_was_not_read_still_gets_a_line() {
        assert_eq!(departure_unread_notice(&full_id("01a04eb6")), "ask 01a04eb6 closed");
    }

    /// A key on an already-answered ask reports the lost race. Silence would
    /// look like a key that does not work.
    #[test]
    fn a_lost_race_is_reported_and_an_accepted_decision_is_not() {
        assert_eq!(
            settled_notice(
                &full_id("01a04eb6"),
                Decision::AllowOnce,
                &DecisionOutcome::Accepted { unlearned: None }
            ),
            None,
            "the ask leaving the open set already says this"
        );
        let raced = settled_notice(
            &full_id("01a04eb6"),
            Decision::Deny,
            &DecisionOutcome::AlreadyDecided,
        )
        .expect("a race is reported");
        assert!(raced.contains("01a04eb6"), "{raced}");
        assert!(raced.contains("already answered"), "{raced}");

        let failed = settled_notice(
            &full_id("01a04eb6"),
            Decision::AllowAlways,
            &DecisionOutcome::Failed("no live connection".into()),
        )
        .expect("a failure is reported");
        assert!(failed.contains("no live connection"), "{failed}");
    }

    /// `A` asked for a standing rule. When none was learned the answer
    /// stands, but the player must not believe the rule exists.
    #[test]
    fn an_allow_always_that_learned_nothing_says_so() {
        let text = settled_notice(
            &full_id("01a04eb6"),
            Decision::AllowAlways,
            &DecisionOutcome::Accepted { unlearned: Some("compound statement".into()) },
        )
        .expect("a missing rule is reported");
        assert_eq!(
            text,
            "ask 01a04eb6 allow always, but no rule was learned: compound statement"
        );
    }

    #[test]
    fn an_ask_that_is_not_yours_names_its_reviewer() {
        let other = ask("01a04eb6", Some(ctx(1)), principal(3)).summary;
        assert_eq!(not_yours_notice(&other), "not yours to answer: reviewer amy");

        let mut nameless = other.clone();
        nameless.reviewer = Some(named(principal(3), ""));
        assert_eq!(
            not_yours_notice(&nameless),
            format!("not yours to answer: reviewer {}", principal(3).short())
        );

        let mut unassigned = other;
        unassigned.reviewer = None;
        assert!(not_yours_notice(&unassigned).contains("names no reviewer"));
    }

    /// A notice reads for a while and then gets out of the way.
    #[test]
    fn a_notice_ages_out() {
        let mut notice = AskNotice::default();
        assert_eq!(notice.current(0.0), None);
        notice.say("ask 01a04eb6 expired", 10.0);
        assert_eq!(notice.current(10.0), Some("ask 01a04eb6 expired"));
        assert_eq!(notice.current(10.0 + NOTICE_TTL - 0.1), Some("ask 01a04eb6 expired"));
        assert_eq!(notice.current(10.0 + NOTICE_TTL), None);
    }

    // ── the rows ──────────────────────────────────────────────────────────

    #[test]
    fn the_header_names_the_ask_its_tool_and_how_long_it_has_waited() {
        let me = principal(1);
        let rows = sheet_head(&ask("01a04eb6", Some(ctx(1)), me), Some(me), 12_000, 120);
        assert_eq!(rows[0].text, "ask 01a04eb6  shell_write  waiting 12s");
        assert_eq!(rows[0].tone, LineTone::Head);
    }

    #[test]
    fn provenance_shows_only_what_the_ask_carries() {
        let me = principal(1);
        let rows = sheet_head(&ask("01a04eb6", Some(ctx(1)), me), Some(me), 0, 120);
        let all = text(&rows);
        assert!(all.contains("origin shell_gate"), "{all}");
        assert!(all.contains("instance kaish-1"), "{all}");
        assert!(!all.contains("hook"), "an absent hook is not rendered: {all}");
    }

    /// The reviewer is marked when it is you — that mark is what says the
    /// decision keys will work.
    #[test]
    fn the_heads_line_marks_you_as_the_reviewer() {
        let me = principal(1);
        let rows = sheet_head(&ask("01a04eb6", Some(ctx(1)), me), Some(me), 0, 120);
        let heads = rows.iter().find(|r| r.text.starts_with("requester")).expect("heads line");
        assert_eq!(
            heads.text,
            "requester amy  performer coder  reviewer amy \u{00b7} you"
        );
    }

    /// Someone else's ask says whose it is and what they would run, and its
    /// key line offers no approval keys at all.
    #[test]
    fn someone_elses_ask_offers_guidance_instead_of_approval_keys() {
        let me = principal(1);
        let other = ask("01a04eb6", Some(ctx(1)), principal(3));
        let rows = sheet_head(&other, Some(me), 0, 120);
        let all = text(&rows);
        assert!(all.contains("not yours to answer"), "{all}");
        assert!(all.contains("kj ledger allow 01a04eb6"), "{all}");
        assert!(rows.iter().any(|r| r.tone == LineTone::Warn));

        let keys = sheet_keys(false, 1);
        assert!(!keys.contains("[a]llow"), "{keys}");
        assert!(!keys.contains("[d]eny"), "{keys}");
        assert!(keys.contains("Esc aside"), "{keys}");
    }

    #[test]
    fn the_key_line_offers_stepping_only_when_there_is_somewhere_to_step() {
        assert!(!sheet_keys(true, 1).contains("] next"));
        let many = sheet_keys(true, 3);
        assert!(many.contains("] next"), "{many}");
        assert!(many.contains("[ prev"), "{many}");
        assert!(many.starts_with("[a]llow once  [A]llow always  [d]eny"), "{many}");
    }

    #[test]
    fn the_plan_numbers_the_statements_the_gate_recorded() {
        let me = principal(1);
        let mut a = ask("01a04eb6", Some(ctx(1)), me);
        a.summary.statements = vec!["git worktree remove".into(), "rm -rf /tmp/x".into()];
        let rows = sheet_body(&a, None, 120);
        assert_eq!(rows[0].text, "PLAN");
        assert_eq!(rows[1].text, " 1. git worktree remove");
        assert_eq!(rows[2].text, " 2. rm -rf /tmp/x");
    }

    /// An ask with no statements says so. An empty PLAN block that looked
    /// like a short plan would be the worst of both.
    #[test]
    fn a_plan_with_no_statements_says_so() {
        let me = principal(1);
        let mut a = ask("01a04eb6", Some(ctx(1)), me);
        a.summary.statements.clear();
        let rows = sheet_body(&a, None, 120);
        assert_eq!(rows[1].text, "no statements recorded");
        assert_eq!(rows[1].tone, LineTone::Warn);
    }

    /// `value: None` is "unset at ask time", a recorded fact the panel must
    /// not render as an empty string.
    #[test]
    fn an_unset_free_variable_renders_as_unset() {
        let me = principal(1);
        let mut a = ask("01a04eb6", Some(ctx(1)), me);
        a.env = vec![
            AskEnv { name: "TARGET".into(), value: Some("kaish-arith".into()) },
            AskEnv { name: "FORCE".into(), value: None },
        ];
        let all = text(&sheet_body(&a, None, 120));
        assert!(all.contains("env TARGET=kaish-arith"), "{all}");
        assert!(all.contains("env FORCE unset"), "{all}");
    }

    /// While the full record is unread, the sheet says so where the
    /// environment would be: an empty environment must not read as one the
    /// ask recorded.
    #[test]
    fn an_unread_record_says_so_where_the_environment_would_be() {
        let me = principal(1);
        let a = crate::connection::ledger::summary_record(&ask("01a04eb6", Some(ctx(1)), me).summary);
        let rows = sheet_body(&a, Some("reading the full record"), 120);
        let gap = rows
            .iter()
            .find(|r| r.text == "reading the full record")
            .expect("the gap is a row");
        assert_eq!(gap.tone, LineTone::Warn);
        assert!(!text(&sheet_body(&a, None, 120)).contains("reading"));
    }

    #[test]
    fn the_environment_follows_the_plan() {
        let me = principal(1);
        let all = text(&sheet_body(&ask("01a04eb6", Some(ctx(1)), me), None, 120));
        assert!(all.contains("cwd /home/amy/src/wt/kaish-arith"), "{all}");
        assert!(all.contains("source kaish"), "{all}");
    }

    // ── the whole sheet ───────────────────────────────────────────────────

    /// The key line is the last row whatever the plan does — that is what
    /// "the key line never leaves the bottom edge" means.
    #[test]
    fn the_key_line_is_always_the_last_row() {
        let me = principal(1);
        let mut a = ask("01a04eb6", Some(ctx(1)), me);
        a.summary.statements = (0..60).map(|i| format!("statement number {i}")).collect();
        for rows in [8usize, 14, 40] {
            let out = sheet_rows(&a, None, Some(me), 0, 1, 100, rows, 0, true);
            assert!(
                out.last().expect("rows").text.starts_with("[a]llow once"),
                "rows={rows} got {:?}",
                out.last()
            );
        }
    }

    /// A plan taller than the panel scrolls, and says how much is out of
    /// sight rather than dropping it.
    #[test]
    fn a_long_plan_scrolls_and_counts_what_it_hides() {
        let me = principal(1);
        let mut a = ask("01a04eb6", Some(ctx(1)), me);
        a.summary.statements = (0..60).map(|i| format!("statement number {i}")).collect();
        let top = sheet_rows(&a, None, Some(me), 0, 1, 100, 14, 0, true);
        let joined = text(&top);
        assert!(joined.contains("\u{2191}0 \u{2193}"), "{joined}");

        let scrolled = sheet_rows(&a, None, Some(me), 0, 1, 100, 14, 5, true);
        assert!(text(&scrolled).contains("\u{2191}5 \u{2193}"), "{}", text(&scrolled));
        assert_ne!(text(&top), text(&scrolled), "j moved the plan");

        // And `j` stops: the limit is what the panel can actually show.
        let limit = sheet_max_scroll(&a, None, Some(me), 0, 100, 14);
        assert!(limit > 0);
        assert_eq!(
            text(&sheet_rows(&a, None, Some(me), 0, 1, 100, 14, limit, true)),
            text(&sheet_rows(&a, None, Some(me), 0, 1, 100, 14, limit + 50, true)),
            "scrolling past the end shows the end"
        );
    }

    /// Every row fits the panel — a wide statement wraps and a wide header
    /// truncates, and neither overflows.
    #[test]
    fn no_row_is_wider_than_the_panel() {
        let me = principal(1);
        let mut a = ask("01a04eb6", Some(ctx(1)), me);
        a.summary.description = "x".repeat(300);
        a.summary.statements = vec!["/very/long/path/".repeat(40)];
        a.cwd = Some("/another/very/long/path/".repeat(20));
        for cols in [24usize, 40, 80] {
            for row in sheet_rows(&a, None, Some(me), 0, 4, cols, 30, 0, true) {
                assert!(
                    row.text.chars().count() <= cols,
                    "cols={cols} overflowed with {:?}",
                    row.text
                );
            }
        }
    }

    // ── the sheet follows the open set ───────────────────────────────────

    /// `sync_ask_sheet` and `note_ask_departures` over a mirror the test
    /// feeds, as the mirror plugin would.
    fn sheet_app(me: PrincipalId) -> App {
        let mut app = App::new();
        let mut docs = crate::view::document::DocumentCache::default();
        docs.set_active(ctx(1));
        app.init_resource::<Time>()
            .init_resource::<LedgerMirror>()
            .init_resource::<AskSheetState>()
            .init_resource::<AskNotice>()
            .insert_resource(docs)
            .insert_resource(State::new(Screen::Conversation))
            .insert_resource(crate::cell::SessionPrincipal(Some(me)))
            .add_message::<AskLeft>()
            .add_systems(Update, (sync_ask_sheet, note_ask_departures).chain());
        app
    }

    fn set_open(app: &mut App, asks: &[AskDetail], generation: i64) {
        let state = kaijutsu_client::LedgerState::from_listing(generation, open(asks));
        app.world_mut().resource_mut::<LedgerMirror>().apply_state(&state);
    }

    fn showing(app: &App) -> Option<String> {
        app.world().resource::<AskSheetState>().showing.clone()
    }

    fn notice(app: &App) -> Option<String> {
        let now = app.world().resource::<Time>().elapsed_secs_f64();
        app.world().resource::<AskNotice>().current(now).map(str::to_string)
    }

    /// An ask that arrives in the open set, in the context on screen, for
    /// this player, raises the sheet; the same ask leaving the open set —
    /// answered here or anywhere — takes it down, and its closed record
    /// says what became of it.
    #[test]
    fn the_sheet_rises_with_an_open_ask_and_falls_when_it_closes() {
        let me = principal(1);
        let mut app = sheet_app(me);
        let a = ask("01a04eb6", Some(ctx(1)), me);

        set_open(&mut app, &[a.clone()], 1);
        app.update();
        assert_eq!(showing(&app), Some(full_id("01a04eb6")));

        set_open(&mut app, &[], 2);
        app.update();
        assert_eq!(showing(&app), None, "a closed ask takes its sheet down");
        assert_eq!(notice(&app), None, "nothing is said before the record is read");

        app.world_mut().write_message(AskLeft {
            request_id: full_id("01a04eb6"),
            record: Some(decided(a, "deny", Some(named(principal(7), "banto")))),
        });
        app.update();
        assert_eq!(notice(&app).as_deref(), Some("ask 01a04eb6 deny by banto"));
    }

    /// Someone else's ask in the context on screen raises the sheet
    /// read-only: it is shown, and its keys offer no answer.
    #[test]
    fn someone_elses_ask_raises_a_read_only_sheet() {
        let me = principal(1);
        let mut app = sheet_app(me);
        let theirs = ask("01a04eb6", Some(ctx(1)), principal(3));
        set_open(&mut app, &[theirs.clone()], 1);
        app.update();
        assert_eq!(showing(&app), Some(full_id("01a04eb6")));
        assert!(!sheet_keys(theirs.summary.answerable_by(me), 1).contains("[a]llow"));
    }

    /// An ask that closed while it was not on screen is not announced; the
    /// ribbon's ANSWERED rows carry it.
    #[test]
    fn an_ask_that_closed_off_screen_is_not_announced() {
        let me = principal(1);
        let mut app = sheet_app(me);
        let theirs = ask("01a04eb6", Some(ctx(2)), principal(3));

        set_open(&mut app, &[theirs.clone()], 1);
        app.update();
        assert_eq!(showing(&app), None, "an ask in another context does not raise");

        set_open(&mut app, &[], 2);
        app.update();
        app.world_mut().write_message(AskLeft {
            request_id: full_id("01a04eb6"),
            record: Some(decided(theirs, "allow_once", Some(named(principal(3), "amy")))),
        });
        app.update();
        assert_eq!(notice(&app), None);
    }

    /// Two sheets that come down before either departure read lands each
    /// get their line: the second must not overwrite the first's wait.
    #[test]
    fn two_departures_in_a_row_each_get_their_notice() {
        let me = principal(1);
        let mut app = sheet_app(me);
        let a = ask("aaaaaaaa", Some(ctx(1)), me);
        let b = ask("bbbbbbbb", Some(ctx(1)), me);

        set_open(&mut app, &[a.clone(), b.clone()], 1);
        app.update();
        assert_eq!(showing(&app), Some(full_id("aaaaaaaa")));
        set_open(&mut app, &[b.clone()], 2);
        app.update();
        assert_eq!(showing(&app), Some(full_id("bbbbbbbb")));
        set_open(&mut app, &[], 3);
        app.update();
        assert_eq!(showing(&app), None);

        app.world_mut().write_message(AskLeft {
            request_id: full_id("aaaaaaaa"),
            record: Some(decided(a, "deny", Some(named(me, "amy")))),
        });
        app.update();
        assert_eq!(notice(&app).as_deref(), Some("ask aaaaaaaa deny by you"));

        app.world_mut().write_message(AskLeft { request_id: full_id("bbbbbbbb"), record: None });
        app.update();
        assert_eq!(notice(&app).as_deref(), Some("ask bbbbbbbb closed"));
        assert!(app.world().resource::<AskSheetState>().left.is_empty(), "nothing waits forever");
    }
}
