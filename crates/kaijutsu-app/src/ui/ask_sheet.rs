//! The ask sheet — the app's answer to an approval ask waiting on you.
//!
//! An ask is a request the kernel recorded and will not act on until a
//! reviewer answers it (`docs/approval-identity.md`). Until now the app could
//! only *show* that one was waiting: a lamp on the switchboard, `!n` on the
//! hints line. This is the surface you answer from.
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
//! A pending ask in the context on screen raises the sheet on
//! `Screen::Conversation` and `Screen::Room`, and never on `Screen::Editor`
//! or `Screen::Diff` — a vi surface owns the keys wherever it is live
//! (`docs/input.md`, "Escape — two meanings total"). Esc puts the ask
//! **aside**: the ask stays pending, the `!n` marker stays, and the sheet
//! re-raises only on a context switch back or when a new ask arrives.
//!
//! ## Decisions are kernel verbs
//!
//! `a`/`A`/`d` write an [`AskDecisionRequested`] and change nothing here.
//! The kernel decides; its ledger-generation bump refreshes
//! [`LedgerMirror`], and the ask leaving the pending set is what takes the
//! sheet down — with a notice saying what became of it. A refusal, a lost
//! race, or an ask that is not yours to answer all produce a notice too:
//! a decision key never does nothing silently.
//!
//! ## Honesty
//!
//! The PLAN block renders exactly the statements `kj ledger show` carries.
//! There is no plan tree and no per-statement verdict, because the wire has
//! neither; the block is shaped to take them when it does.

use std::collections::HashSet;

use bevy::prelude::*;
use kaijutsu_client::AskDetail;
use kaijutsu_types::{ContextId, PrincipalId};

use crate::connection::ledger::{
    short_request_id, AskDecisionRequested, AskDecisionSettled, Decision, DecisionOutcome,
    LedgerMirror,
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
    /// Ids put aside with Esc. They stay pending and keep their `!`; they
    /// simply do not raise the sheet again until the situation changes.
    pub aside: HashSet<String>,
    /// Pending ids this state has already seen, so a genuinely NEW ask can
    /// be told from the same set polled again — the arrival of one lifts the
    /// aside set.
    known: HashSet<String>,
    /// First plan row drawn (`j`/`k`).
    pub scroll: usize,
    /// The context the sheet last looked at, to notice a switch.
    context: Option<ContextId>,
}

impl AskSheetState {
    /// Whether the sheet has an ask to show.
    pub fn up(&self) -> bool {
        self.showing.is_some()
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
/// An ask already on screen stays on screen while it is pending, whatever
/// context it belongs to — `]`/`[` walk the whole pending set, so the sheet
/// must not snap back to the current context between keypresses. A context
/// switch is the exception: attention moved, so the choice is made afresh
/// from the new context's own asks, skipping anything put aside.
pub fn raise_choice(
    showing: Option<&str>,
    pending: &[AskDetail],
    current: Option<ContextId>,
    aside: &HashSet<String>,
    context_changed: bool,
) -> Option<String> {
    if !context_changed
        && let Some(id) = showing
        && pending.iter().any(|ask| ask.request_id == id)
    {
        return Some(id.to_string());
    }
    let current = current?;
    pending
        .iter()
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

/// The one-line outcome of an ask that left the pending set (`docs/tui.md`,
/// "Asks"): `ask 01a04eb6 allow once by you`, `… by 2b1ffa32`, `… expired`.
///
/// The words come from `decided_option` when the kernel recorded one and from
/// `status` when it did not — an expired or abandoned ask was never decided
/// by anyone, and saying "allow" about it would be a lie.
pub fn departure_notice(ask: &AskDetail, me: Option<PrincipalId>) -> String {
    let id = short_request_id(&ask.request_id);
    let Some(option) = ask.decided_option.as_deref().filter(|s| !s.is_empty()) else {
        let status = if ask.status.is_empty() {
            "gone"
        } else {
            ask.status.as_str()
        };
        return format!("ask {id} {status}");
    };
    let words = option.replace('_', " ");
    let by = match (ask.decided_by, me) {
        (Some(who), Some(me)) if who == me => Some("you".to_string()),
        (Some(who), _) => Some(
            ask.decided_by_name
                .clone()
                .filter(|n| !n.is_empty())
                .unwrap_or_else(|| who.short()),
        ),
        // A remembered rule decides without a principal; there is no "by".
        (None, _) => None,
    };
    match by {
        Some(by) => format!("ask {id} {words} by {by}"),
        None => format!("ask {id} {words}"),
    }
}

/// The notice for a decision the kernel refused.
pub fn settled_notice(request_id: &str, decision: Decision, outcome: &DecisionOutcome) -> Option<String> {
    let id = short_request_id(request_id);
    match outcome {
        // Taken: the ask leaving the pending set says so, with the decider's
        // name attached. A second notice here would only race it.
        DecisionOutcome::Accepted => None,
        DecisionOutcome::AlreadyDecided => {
            Some(format!("ask {id} was already answered \u{2014} {} lost the race", decision.label()))
        }
        DecisionOutcome::Failed(detail) => {
            Some(format!("ask {id} {} failed: {detail}", decision.label()))
        }
    }
}

/// The notice for a decision key on an ask this player may not answer.
pub fn not_yours_notice(ask: &AskDetail) -> String {
    match ask.reviewer_name.as_deref().filter(|n| !n.is_empty()) {
        Some(name) => format!("not yours to answer: reviewer {name}"),
        None => match ask.reviewer_id {
            Some(id) => format!("not yours to answer: reviewer {}", id.short()),
            None => "not yours to answer: this ask names no reviewer".to_string(),
        },
    }
}

// ============================================================================
// PURE: THE ROWS
// ============================================================================

/// A name for display: the character's name when the sheet has one, its short
/// principal id when it does not, and `\u{2014}` when the ask records no one
/// at all. An empty slot never renders as blank — "nobody is assigned" and
/// "the name did not arrive" are different facts.
fn who(name: &Option<String>, id: &Option<PrincipalId>) -> String {
    if let Some(name) = name.as_deref().filter(|n| !n.is_empty()) {
        return name.to_string();
    }
    match id {
        Some(id) => id.short(),
        None => "\u{2014}".to_string(),
    }
}

/// The fixed rows above the plan: what the ask is, who it is between, and
/// what it says it will do.
/// How many wrapped lines of the escalation "why" the sheet shows before
/// it caps with a "+n more" marker (the full text is in `kj ledger show`).
const DESCRIPTION_MAX_ROWS: usize = 4;

pub fn sheet_head(ask: &AskDetail, me: Option<PrincipalId>, now_ms: i64, cols: usize) -> Vec<PanelLine> {
    let mut rows = Vec::new();

    let mut header = format!("ask {}", short_request_id(&ask.request_id));
    if let Some(tool) = ask.tool.as_deref().filter(|t| !t.is_empty()) {
        header.push_str(&format!("  {tool}"));
    }
    let waiting = match ask.created_at {
        Some(at) => format_age(now_ms.saturating_sub(at)),
        // The kernel stamps `created_at`; no stamp is a fact, not a zero.
        None => "?".to_string(),
    };
    header.push_str(&format!("  waiting {waiting}"));
    rows.push(PanelLine {
        text: truncate(&header, cols),
        tone: LineTone::Head,
    });

    // Provenance, only what the ask actually carries.
    let mut provenance: Vec<String> = Vec::new();
    if !ask.origin.is_empty() {
        provenance.push(format!("origin {}", ask.origin));
    }
    for (label, value) in [
        ("instance", &ask.instance),
        ("hook", &ask.hook_id),
        ("label", &ask.authorized_label),
    ] {
        if let Some(value) = value.as_deref().filter(|v| !v.is_empty()) {
            provenance.push(format!("{label} {value}"));
        }
    }
    if !provenance.is_empty() {
        rows.push(PanelLine {
            text: truncate(&provenance.join("  "), cols),
            tone: LineTone::Dim,
        });
    }

    // The heads: who asked, who acted, who may answer.
    let can_review = me.is_some_and(|me| ask.can_review(me));
    let mut heads = format!(
        "requester {}  performer {}  reviewer {}",
        who(&ask.principal_name, &ask.principal_id),
        who(&ask.actor_name, &ask.actor_id),
        who(&ask.reviewer_name, &ask.reviewer_id),
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
                short_request_id(&ask.request_id)
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
/// all `kj ledger show` carries today — no tree, no per-statement verdict —
/// and inventing either would put words in the kernel's mouth.
pub fn sheet_body(ask: &AskDetail, cols: usize) -> Vec<PanelLine> {
    let mut rows = vec![PanelLine {
        text: "PLAN".to_string(),
        tone: LineTone::Head,
    }];

    if ask.statements.is_empty() {
        rows.push(PanelLine {
            text: "no statements recorded".to_string(),
            tone: LineTone::Warn,
        });
    }
    for (i, statement) in ask.statements.iter().enumerate() {
        for text in wrap(&format!("{:>2}. {statement}", i + 1), cols, PLAN_INDENT) {
            rows.push(PanelLine {
                text,
                tone: LineTone::Row,
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
        // missing row (`kaijutsu_client::EnvVar`).
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

    // Why the gate escalated, capped: the classifier's note is useful but
    // secondary to the plan, so it sits below it and never crowds the
    // statement off the top. It scrolls with the rest of the body.
    if !ask.description.is_empty() {
        rows.push(PanelLine {
            text: "why".to_string(),
            tone: LineTone::Head,
        });
        let wrapped = wrap(&ask.description, cols, 2);
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
/// and this many rows tall.
///
/// The key line is laid down last and the plan scrolls above it, so it never
/// leaves the bottom edge however long the plan gets.
pub fn sheet_rows(
    ask: &AskDetail,
    me: Option<PrincipalId>,
    now_ms: i64,
    pending_total: usize,
    cols: usize,
    rows: usize,
    scroll: usize,
) -> Vec<PanelLine> {
    let head = sheet_head(ask, me, now_ms, cols);
    let body = sheet_body(ask, cols);
    let keys = PanelLine {
        text: truncate(&sheet_keys(me.is_some_and(|me| ask.can_review(me)), pending_total), cols),
        tone: LineTone::Head,
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
pub fn sheet_max_scroll(ask: &AskDetail, me: Option<PrincipalId>, now_ms: i64, cols: usize, rows: usize) -> usize {
    let head_len = sheet_head(ask, me, now_ms, cols).len();
    let body_rows = rows.saturating_sub(head_len + 1).max(1);
    max_scroll(sheet_body(ask, cols).len(), body_rows)
}

// ============================================================================
// SYSTEMS
// ============================================================================

/// Decide what the sheet shows this frame, and say what became of an ask that
/// left.
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
    session: Res<crate::cell::SessionPrincipal>,
    time: Res<Time>,
    mut state: ResMut<AskSheetState>,
    mut notice: ResMut<AskNotice>,
) {
    let current = doc_cache.active_id();
    let context_changed = state.context != current;

    if !screen_allows_sheet(*screen.get()) {
        // A vi surface has the keys. Put the sheet down without putting the
        // ask aside — it is still waiting, and the hints line still says so.
        if state.showing.is_some() {
            state.showing = None;
            state.scroll = 0;
        }
        if context_changed {
            state.context = current;
        }
        return;
    }

    let pending_ids: Vec<&str> = mirror
        .pending
        .iter()
        .map(|ask| ask.request_id.as_str())
        .collect();

    // An ask that was on screen and is no longer pending gets its one line.
    if let Some(showing) = state.showing.as_deref()
        && !pending_ids.contains(&showing)
        && let Some(ask) = mirror.get(showing)
    {
        notice.say(
            departure_notice(ask, session.0),
            time.elapsed_secs_f64(),
        );
    }

    if aside_needs_refresh(&state, &pending_ids, context_changed) {
        refresh_aside(&mut state, &pending_ids, context_changed);
    }
    let next = raise_choice(
        state.showing.as_deref(),
        &mirror.pending,
        current,
        &state.aside,
        context_changed,
    );
    if next != state.showing {
        state.showing = next;
        state.scroll = 0;
    }
    if context_changed {
        state.context = current;
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
                // The ask may have left the pending set between the frame
                // that drew the key line and this one.
                let Some(ask) = mirror.get(&showing) else {
                    notice.say(
                        format!("ask {} is no longer readable", short_request_id(&showing)),
                        time.elapsed_secs_f64(),
                    );
                    continue;
                };
                if !session.0.is_some_and(|me| ask.can_review(me)) {
                    notice.say(not_yours_notice(ask), time.elapsed_secs_f64());
                    continue;
                }
                decisions.write(AskDecisionRequested {
                    request_id: showing.clone(),
                    decision,
                });
            }

            // `v` — the ledger over the sheet. The ask stays pending and the
            // sheet stays raised behind it.
            Action::OpenLedger => ribbon.open = true,

            Action::AskNext | Action::AskPrev => {
                let ids: Vec<&str> = mirror
                    .pending
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

            // Esc — aside, with the ask still pending.
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
/// none: the ask leaving the pending set says it, with the decider attached.
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
    let Some(ask) = mirror.get(&showing) else {
        return;
    };
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
        ask,
        session.0,
        kaijutsu_types::now_millis() as i64,
        mirror.pending_count(),
        cols,
        rows,
        state.scroll,
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
        mirror.get(&showing),
        windows.single(),
        fonts.get(&handles.mono),
    ) else {
        return;
    };
    let width = panel::panel_width(window.width()) as f64;
    let cols = columns(width, measure_char_width(font));
    let rows = rows_for_height((window.height() * panel::PANEL_MAX_HEIGHT_FRACTION) as f64);
    let limit = sheet_max_scroll(
        ask,
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
                sync_ask_sheet
                    .in_set(crate::input::InputPhase::SyncContext)
                    .before(crate::input::context::sync_input_context),
            )
            .add_systems(
                Update,
                (
                    handle_ask_sheet_actions,
                    note_ask_decisions,
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
    use kaijutsu_client::EnvVar;

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

    /// A pending ask the given principal may review.
    fn ask(id: &str, context: Option<ContextId>, reviewer: PrincipalId) -> AskDetail {
        AskDetail {
            request_id: format!("{id}-aaaa-bbbb-cccc-000000000001"),
            context_id: context,
            principal_id: Some(principal(9)),
            principal_name: Some("amy".into()),
            actor_id: Some(principal(4)),
            actor_name: Some("coder".into()),
            reviewer_id: Some(reviewer),
            reviewer_name: Some("amy".into()),
            status: "pending".into(),
            origin: "shell_gate".into(),
            tool: Some("shell_write".into()),
            hook_id: None,
            instance: Some("kaish-1".into()),
            description: "rm -rf ~/src/wt/kaish-arith".into(),
            authorized_label: None,
            statements: vec!["rm -rf ~/src/wt/kaish-arith".into()],
            exec_source: Some("kaish".into()),
            cwd: Some("/home/amy/src/wt/kaish-arith".into()),
            env: Vec::new(),
            created_at: Some(0),
            decided_at: None,
            decided_by: None,
            decided_by_name: None,
            decided_option: None,
            remember_scope: None,
            redeemed_at: None, publication_abandoned: None,
        }
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
            raise_choice(None, &pending, Some(ctx(1)), &HashSet::new(), false),
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
            raise_choice(None, &pending, Some(ctx(1)), &HashSet::new(), false),
            None
        );
    }

    /// Esc puts the ask aside with the ask still pending. The sheet must not
    /// raise it again on the next refresh — that is the whole point of aside.
    #[test]
    fn an_ask_put_aside_stays_down_while_nothing_changes() {
        let pending = vec![ask("aaaa", Some(ctx(1)), principal(1))];
        assert_eq!(
            raise_choice(None, &pending, Some(ctx(1)), &aside(&[&full_id("aaaa")]), false),
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
                &pending,
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
                &pending,
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
        let mut decided = ask("01a04eb6", Some(ctx(1)), me);
        decided.decided_option = Some("allow_once".into());
        decided.decided_by = Some(me);
        decided.decided_by_name = Some("amy".into());
        assert_eq!(
            departure_notice(&decided, Some(me)),
            "ask 01a04eb6 allow once by you"
        );
    }

    #[test]
    fn someone_elses_decision_names_them() {
        let mut decided = ask("01a04eb6", Some(ctx(1)), principal(1));
        decided.decided_option = Some("allow_always".into());
        decided.decided_by = Some(principal(7));
        decided.decided_by_name = Some("banto".into());
        assert_eq!(
            departure_notice(&decided, Some(principal(1))),
            "ask 01a04eb6 allow always by banto"
        );

        // No name on the sheet: the short principal id, never a blank.
        decided.decided_by_name = None;
        assert_eq!(
            departure_notice(&decided, Some(principal(1))),
            format!("ask 01a04eb6 allow always by {}", principal(7).short())
        );
    }

    /// An expired ask was never decided by anyone. Saying "allow" about it
    /// would be a lie, so the status is what gets said.
    #[test]
    fn an_ask_that_expired_says_expired_and_names_nobody() {
        let mut gone = ask("01a04eb6", Some(ctx(1)), principal(1));
        gone.status = "expired".into();
        assert_eq!(departure_notice(&gone, Some(principal(1))), "ask 01a04eb6 expired");
    }

    /// A remembered rule decides with no principal behind it.
    #[test]
    fn an_auto_decision_names_no_decider() {
        let mut auto = ask("01a04eb6", Some(ctx(1)), principal(1));
        auto.decided_option = Some("auto_allow".into());
        auto.status = "allowed".into();
        assert_eq!(departure_notice(&auto, Some(principal(1))), "ask 01a04eb6 auto allow");
    }

    /// A key on an already-answered ask reports the lost race. Silence would
    /// look like a key that does not work.
    #[test]
    fn a_lost_race_is_reported_and_an_accepted_decision_is_not() {
        assert_eq!(
            settled_notice(&full_id("01a04eb6"), Decision::AllowOnce, &DecisionOutcome::Accepted),
            None,
            "the ask leaving the pending set already says this"
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

    #[test]
    fn an_ask_that_is_not_yours_names_its_reviewer() {
        let other = ask("01a04eb6", Some(ctx(1)), principal(3));
        assert_eq!(not_yours_notice(&other), "not yours to answer: reviewer amy");

        let mut nameless = other.clone();
        nameless.reviewer_name = None;
        assert_eq!(
            not_yours_notice(&nameless),
            format!("not yours to answer: reviewer {}", principal(3).short())
        );

        let mut unassigned = other;
        unassigned.reviewer_name = None;
        unassigned.reviewer_id = None;
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

    /// A missing `created_at` is a fact, not a zero — "waiting now" would be
    /// a claim the kernel never made.
    #[test]
    fn an_ask_with_no_creation_stamp_says_so() {
        let me = principal(1);
        let mut a = ask("01a04eb6", Some(ctx(1)), me);
        a.created_at = None;
        let rows = sheet_head(&a, Some(me), 12_000, 120);
        assert!(rows[0].text.ends_with("waiting ?"), "{:?}", rows[0].text);
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
        a.statements = vec!["git worktree remove".into(), "rm -rf /tmp/x".into()];
        let rows = sheet_body(&a, 120);
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
        a.statements.clear();
        let rows = sheet_body(&a, 120);
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
            EnvVar { name: "TARGET".into(), value: Some("kaish-arith".into()) },
            EnvVar { name: "FORCE".into(), value: None },
        ];
        let all = text(&sheet_body(&a, 120));
        assert!(all.contains("env TARGET=kaish-arith"), "{all}");
        assert!(all.contains("env FORCE unset"), "{all}");
    }

    #[test]
    fn the_environment_follows_the_plan() {
        let me = principal(1);
        let all = text(&sheet_body(&ask("01a04eb6", Some(ctx(1)), me), 120));
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
        a.statements = (0..60).map(|i| format!("statement number {i}")).collect();
        for rows in [8usize, 14, 40] {
            let out = sheet_rows(&a, Some(me), 0, 1, 100, rows, 0);
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
        a.statements = (0..60).map(|i| format!("statement number {i}")).collect();
        let top = sheet_rows(&a, Some(me), 0, 1, 100, 14, 0);
        let joined = text(&top);
        assert!(joined.contains("\u{2191}0 \u{2193}"), "{joined}");

        let scrolled = sheet_rows(&a, Some(me), 0, 1, 100, 14, 5);
        assert!(text(&scrolled).contains("\u{2191}5 \u{2193}"), "{}", text(&scrolled));
        assert_ne!(text(&top), text(&scrolled), "j moved the plan");

        // And `j` stops: the limit is what the panel can actually show.
        let limit = sheet_max_scroll(&a, Some(me), 0, 100, 14);
        assert!(limit > 0);
        assert_eq!(
            text(&sheet_rows(&a, Some(me), 0, 1, 100, 14, limit)),
            text(&sheet_rows(&a, Some(me), 0, 1, 100, 14, limit + 50)),
            "scrolling past the end shows the end"
        );
    }

    /// Every row fits the panel — a wide statement wraps and a wide header
    /// truncates, and neither overflows.
    #[test]
    fn no_row_is_wider_than_the_panel() {
        let me = principal(1);
        let mut a = ask("01a04eb6", Some(ctx(1)), me);
        a.description = "x".repeat(300);
        a.statements = vec!["/very/long/path/".repeat(40)];
        a.cwd = Some("/another/very/long/path/".repeat(20));
        for cols in [24usize, 40, 80] {
            for row in sheet_rows(&a, Some(me), 0, 4, cols, 30, 0) {
                assert!(
                    row.text.chars().count() <= cols,
                    "cols={cols} overflowed with {:?}",
                    row.text
                );
            }
        }
    }
}
