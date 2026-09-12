//! The ledger ribbon (`Ctrl+A l`) — every pending ask at once, and the last
//! few decisions.
//!
//! The ask sheet answers the ask in front of you. This is the view you work
//! from between them: one row per pending ask across every context, then the
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
//! one place in the app that talks to `kj ledger`, so the ribbon, the sheet,
//! the switchboard lamps and the dock's `!n` cannot disagree about what is
//! pending. The mirror keeps the last [`RECENT_CAP`] decisions, which is why
//! the header says `recent n` and not "answered today": nothing on the wire
//! tells this app how many asks were answered today, and a number that
//! looked like it did would be an invention.

use bevy::prelude::*;
use kaijutsu_client::AskDetail;
use kaijutsu_types::{ContextId, PrincipalId};

use crate::connection::ledger::{
    short_request_id, AskDecisionRequested, Decision, LedgerMirror, RECENT_CAP,
};
use crate::input::events::ActionFired;
use crate::input::{Action, InputContext};
use crate::shaders::BlockFxMaterial;
use crate::text::msdf::MsdfBlockGlyphs;
use crate::ui::ask_sheet::{
    not_yours_notice, screen_allows_sheet, AskNotice, AskSheetState,
};
use crate::ui::msdf_panel::{
    self as panel, columns, measure_char_width, rows_for_height, truncate,
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

/// The ribbon's key line.
pub const RIBBON_KEYS: &str =
    "a allow once  A allow always  d deny  Enter sheet  j/k move  Esc back";

// ============================================================================
// STATE
// ============================================================================

/// Whether the ribbon is open, and which pending row is selected.
#[derive(Resource, Default, Debug)]
pub struct LedgerRibbonState {
    /// `Ctrl+A l` latches this; Esc and the chord again clear it.
    pub open: bool,
    /// Index into the mirror's pending list. Clamped on every rebuild, so an
    /// answered ask cannot leave the selection pointing past the end.
    pub selected: usize,
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

/// A name for a column: the character's name, else its short principal id,
/// else an em dash. Never blank — an unassigned slot and a name that did not
/// arrive are different facts.
fn who(name: &Option<String>, id: &Option<PrincipalId>) -> String {
    if let Some(name) = name.as_deref().filter(|n| !n.is_empty()) {
        return name.to_string();
    }
    match id {
        Some(id) => id.short(),
        None => "\u{2014}".to_string(),
    }
}

/// A context's label if this app knows one, else its short id. A context the
/// drift poll has not seen is still named, just less helpfully.
fn context_cell(context_id: Option<ContextId>, label: impl Fn(ContextId) -> Option<String>) -> String {
    match context_id {
        Some(id) => label(id).filter(|l| !l.is_empty()).unwrap_or_else(|| id.short()),
        None => "\u{2014}".to_string(),
    }
}

/// One pending row: the ask marker, its handle, how long it has waited,
/// where it came from, who raised it, and the first statement.
pub fn pending_row(
    ask: &AskDetail,
    now_ms: i64,
    selected: bool,
    cols: usize,
    label: impl Fn(ContextId) -> Option<String>,
) -> PanelLine {
    let age = match ask.created_at {
        Some(at) => format_age(now_ms.saturating_sub(at)),
        None => "?".to_string(),
    };
    let statement = ask
        .statements
        .first()
        .map(String::as_str)
        .filter(|s| !s.is_empty())
        .unwrap_or(ask.description.as_str());
    let text = format!(
        "{}! {}  {}  {}  {}  {}  {}",
        if selected { "\u{25b8}" } else { " " },
        cell(short_request_id(&ask.request_id), W_ID),
        cell_right(&age, W_AGE),
        cell(&context_cell(ask.context_id, label), W_CONTEXT),
        cell(&who(&ask.actor_name, &ask.actor_id), W_WHO),
        cell(ask.tool.as_deref().unwrap_or("\u{2014}"), W_TOOL),
        statement,
    );
    PanelLine {
        text: truncate(&text, cols),
        tone: if selected { LineTone::Head } else { LineTone::Row },
    }
}

/// One answered row: what was decided, by whom, and whether it was ever
/// redeemed.
///
/// `redeemed \u{2014}` means the decision was never consumed, which is the
/// question the redemption incident taught us to ask (`docs/issues.md`).
pub fn answered_row(
    ask: &AskDetail,
    now_ms: i64,
    cols: usize,
    label: impl Fn(ContextId) -> Option<String>,
) -> PanelLine {
    let age = match ask.decided_at {
        Some(at) => format_age(now_ms.saturating_sub(at)),
        None => "?".to_string(),
    };
    let option = ask
        .decided_option
        .as_deref()
        .filter(|o| !o.is_empty())
        .map(|o| o.replace('_', " "))
        // No recorded option means nobody decided it: expired, abandoned.
        .unwrap_or_else(|| {
            if ask.status.is_empty() {
                "\u{2014}".to_string()
            } else {
                ask.status.clone()
            }
        });
    let redeemed = match ask.redeemed_at {
        Some(at) => format!("redeemed {}", format_age(now_ms.saturating_sub(at))),
        None => "\u{2014}".to_string(),
    };
    let statement = ask
        .statements
        .first()
        .map(String::as_str)
        .filter(|s| !s.is_empty())
        .unwrap_or(ask.description.as_str());
    let text = format!(
        "  {}  {}  {}  {}  {}  {}  {}",
        cell(short_request_id(&ask.request_id), W_ID),
        cell_right(&age, W_AGE),
        cell(&context_cell(ask.context_id, label), W_CONTEXT),
        cell(&option, W_OPTION),
        cell(&who(&ask.decided_by_name, &ask.decided_by), W_WHO),
        cell(&redeemed, W_REDEEMED),
        statement,
    );
    PanelLine {
        text: truncate(&text, cols),
        tone: LineTone::Dim,
    }
}

/// Every row of the ribbon, in order. The key line is last, as on the sheet.
///
/// A pending section with nothing in it says so: an empty body under a
/// PENDING heading must never be allowed to mean "we did not look".
pub fn ribbon_rows(
    pending: &[AskDetail],
    recent: &[AskDetail],
    selected: usize,
    now_ms: i64,
    cols: usize,
    rows: usize,
    label: impl Fn(ContextId) -> Option<String> + Copy,
) -> Vec<PanelLine> {
    let mut out = vec![PanelLine {
        text: truncate(
            &format!(
                "LEDGER    pending {}   recent {}",
                pending.len(),
                recent.len()
            ),
            cols,
        ),
        tone: LineTone::Head,
    }];

    out.push(PanelLine {
        text: "PENDING".to_string(),
        tone: LineTone::Head,
    });
    if pending.is_empty() {
        out.push(PanelLine {
            text: "nothing waiting".to_string(),
            tone: LineTone::Dim,
        });
    }
    for (i, ask) in pending.iter().enumerate() {
        out.push(pending_row(ask, now_ms, i == selected, cols, label));
    }

    if !recent.is_empty() {
        out.push(PanelLine {
            text: format!("ANSWERED (last {RECENT_CAP})"),
            tone: LineTone::Head,
        });
        for ask in recent.iter().take(RECENT_CAP) {
            out.push(answered_row(ask, now_ms, cols, label));
        }
    }

    // A ledger longer than the panel drops its tail rather than its keys —
    // the key line is the last thing this view renders, so it is the last
    // thing to go (`docs/tui.md`, "Asks": it crops from the top of the
    // overflow, never over the keys).
    let keys = PanelLine {
        text: truncate(RIBBON_KEYS, cols),
        tone: LineTone::Head,
    };
    let room = rows.saturating_sub(1).max(1);
    if out.len() > room {
        let hidden = out.len() - (room - 1);
        out.truncate(room - 1);
        out.push(PanelLine {
            text: truncate(&format!("+{hidden} more rows"), cols),
            tone: LineTone::Warn,
        });
    }
    out.push(keys);
    out
}

/// Clamp a selection to the pending rows that exist. An answered ask must not
/// leave the selection pointing past the end.
pub fn clamp_selection(selected: usize, pending_len: usize) -> usize {
    if pending_len == 0 {
        return 0;
    }
    selected.min(pending_len - 1)
}

/// Move the selection, stopping at the ends. `j` past the bottom stays at
/// the bottom: a wrapping selection in a list you are deciding from is a way
/// to answer the wrong ask.
pub fn step_selection(selected: usize, pending_len: usize, delta: isize) -> usize {
    if pending_len == 0 {
        return 0;
    }
    let next = (selected as isize + delta).clamp(0, pending_len as isize - 1);
    next as usize
}

// ============================================================================
// SYSTEMS
// ============================================================================

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
        ribbon.open = !ribbon.open;
    }
}

/// The ribbon's keys: move, answer, open the sheet, close.
#[allow(clippy::too_many_arguments)]
pub fn handle_ledger_ribbon_actions(
    mut actions: MessageReader<ActionFired>,
    mirror: Res<LedgerMirror>,
    session: Res<crate::cell::SessionPrincipal>,
    time: Res<Time>,
    mut ribbon: ResMut<LedgerRibbonState>,
    mut sheet: ResMut<AskSheetState>,
    mut notice: ResMut<AskNotice>,
    mut decisions: MessageWriter<AskDecisionRequested>,
) {
    for ActionFired { action, context } in actions.read() {
        if *context != InputContext::LedgerRibbon {
            continue;
        }
        let selected = clamp_selection(ribbon.selected, mirror.pending.len());
        let row = mirror.pending.get(selected);

        match action {
            Action::StepNext => {
                ribbon.selected = step_selection(selected, mirror.pending.len(), 1);
            }
            Action::StepPrev => {
                ribbon.selected = step_selection(selected, mirror.pending.len(), -1);
            }

            Action::AskAllowOnce | Action::AskAllowAlways | Action::AskDeny => {
                let Some(ask) = row else {
                    notice.say("nothing waiting to answer", time.elapsed_secs_f64());
                    continue;
                };
                if !session.0.is_some_and(|me| ask.can_review(me)) {
                    notice.say(not_yours_notice(ask), time.elapsed_secs_f64());
                    continue;
                }
                decisions.write(AskDecisionRequested {
                    request_id: ask.request_id.clone(),
                    decision: match action {
                        Action::AskAllowOnce => Decision::AllowOnce,
                        Action::AskAllowAlways => Decision::AllowAlways,
                        _ => Decision::Deny,
                    },
                });
            }

            // Enter hands the row to the sheet, which is the untruncated
            // view — so the ribbon closes rather than sitting in front of it.
            Action::Activate => {
                let Some(ask) = row else {
                    continue;
                };
                sheet.aside.remove(&ask.request_id);
                sheet.showing = Some(ask.request_id.clone());
                sheet.scroll = 0;
                ribbon.open = false;
            }

            // Esc, and the chord again.
            Action::CloseLedger | Action::OpenLedger => ribbon.open = false,

            _ => {}
        }
    }
}

/// Keep the selection inside the pending rows, and close the ribbon when a
/// vi screen takes the keyboard.
pub fn sync_ledger_ribbon(
    mirror: Res<LedgerMirror>,
    screen: Res<State<Screen>>,
    mut ribbon: ResMut<LedgerRibbonState>,
) {
    if ribbon.open && !screen_allows_sheet(*screen.get()) {
        ribbon.open = false;
        return;
    }
    let clamped = clamp_selection(ribbon.selected, mirror.pending.len());
    if clamped != ribbon.selected {
        ribbon.selected = clamped;
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
    drift: Res<crate::connection::drift::DriftState>,
    theme: Res<Theme>,
    time: Res<Time>,
    windows: Query<&Window, With<bevy::window::PrimaryWindow>>,
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
    if !ribbon.is_changed() && !mirror.is_changed() && !theme.is_changed() && !stale {
        return;
    }
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
    let cols = columns(width, measure_char_width(font));
    let rows = rows_for_height((window.height() * panel::PANEL_MAX_HEIGHT_FRACTION) as f64);
    let label = |id: ContextId| {
        drift
            .contexts
            .iter()
            .find(|c| c.id == id)
            .map(|c| c.label.clone())
    };

    let lines = ribbon_rows(
        &mirror.pending,
        &mirror.recent,
        clamp_selection(ribbon.selected, mirror.pending.len()),
        kaijutsu_types::now_millis() as i64,
        cols,
        rows,
        label,
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

    fn ask(id: &str) -> AskDetail {
        AskDetail {
            request_id: format!("{id}-aaaa-bbbb-cccc-000000000001"),
            context_id: Some(ctx(1)),
            principal_id: Some(principal(9)),
            principal_name: Some("amy".into()),
            actor_id: Some(principal(4)),
            actor_name: Some("coder".into()),
            reviewer_id: Some(principal(1)),
            reviewer_name: Some("amy".into()),
            status: "pending".into(),
            origin: "shell_gate".into(),
            tool: Some("shell_write".into()),
            hook_id: None,
            instance: None,
            description: "a description".into(),
            authorized_label: None,
            statements: vec!["git worktree remove --force ~/src/wt/kaish-arith".into()],
            exec_source: Some("kaish".into()),
            cwd: None,
            env: Vec::new(),
            created_at: Some(0),
            decided_at: None,
            decided_by: None,
            decided_by_name: None,
            decided_option: None,
            remember_scope: None,
            redeemed_at: None,
        }
    }

    fn no_labels(_: ContextId) -> Option<String> {
        None
    }

    fn text(rows: &[PanelLine]) -> String {
        rows.iter().map(|r| r.text.clone()).collect::<Vec<_>>().join("\n")
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

    /// "Was this consumed" is the question the redemption incident taught us
    /// to ask, so an unredeemed decision says so rather than showing nothing.
    #[test]
    fn an_answered_row_says_whether_it_was_redeemed() {
        let mut decided = ask("01a04eaa");
        decided.status = "allowed".into();
        decided.decided_option = Some("allow_once".into());
        decided.decided_by = Some(principal(1));
        decided.decided_by_name = Some("amy".into());
        decided.decided_at = Some(0);
        decided.redeemed_at = Some(0);
        let row = answered_row(&decided, 5_000, 200, no_labels);
        assert!(row.text.contains("allow once"), "{:?}", row.text);
        assert!(row.text.contains("amy"), "{:?}", row.text);
        assert!(row.text.contains("redeemed 5s"), "{:?}", row.text);
        assert_eq!(row.tone, LineTone::Dim);

        decided.redeemed_at = None;
        let never = answered_row(&decided, 5_000, 200, no_labels);
        assert!(never.text.contains('\u{2014}'), "{:?}", never.text);
        assert!(!never.text.contains("redeemed"), "{:?}", never.text);
    }

    /// An expired ask was decided by nobody. The option column must say that
    /// rather than borrow an allow.
    #[test]
    fn an_answered_row_with_no_decision_shows_its_status() {
        let mut gone = ask("01a04eaa");
        gone.status = "expired".into();
        gone.decided_at = Some(0);
        let row = answered_row(&gone, 0, 200, no_labels);
        assert!(row.text.contains("expired"), "{:?}", row.text);
        assert!(!row.text.contains("allow"), "{:?}", row.text);
    }

    // ── the whole ribbon ──────────────────────────────────────────────────

    #[test]
    fn the_header_counts_what_the_mirror_holds() {
        let pending = vec![ask("aaaaaaaa"), ask("bbbbbbbb")];
        let rows = ribbon_rows(&pending, &[], 0, 0, 200, 40, no_labels);
        assert_eq!(rows[0].text, "LEDGER    pending 2   recent 0");
    }

    /// An empty PENDING section must never be allowed to mean "we did not
    /// look".
    #[test]
    fn an_empty_ledger_says_nothing_is_waiting() {
        let rows = ribbon_rows(&[], &[], 0, 0, 200, 40, no_labels);
        let all = text(&rows);
        assert!(all.contains("PENDING"), "{all}");
        assert!(all.contains("nothing waiting"), "{all}");
        assert!(!all.contains("ANSWERED"), "no decisions, no heading: {all}");
    }

    #[test]
    fn the_key_line_is_always_the_last_row() {
        let pending: Vec<AskDetail> = (0..40).map(|i| ask(&format!("{i:08}"))).collect();
        for rows in [6usize, 12, 60] {
            let out = ribbon_rows(&pending, &[], 0, 0, 200, rows, no_labels);
            assert_eq!(out.last().expect("rows").text, RIBBON_KEYS, "rows={rows}");
        }
    }

    /// A ledger longer than the panel drops its tail and counts it — never
    /// its keys, and never silently.
    #[test]
    fn a_ledger_taller_than_the_panel_counts_what_it_dropped() {
        let pending: Vec<AskDetail> = (0..40).map(|i| ask(&format!("{i:08}"))).collect();
        let out = ribbon_rows(&pending, &[], 0, 0, 200, 10, no_labels);
        assert_eq!(out.len(), 10);
        assert!(out[8].text.contains("more rows"), "{:?}", out[8].text);
        assert_eq!(out[8].tone, LineTone::Warn);
        assert_eq!(out[9].text, RIBBON_KEYS);
    }

    /// Rows truncate; scanning a column down the panel is the point.
    #[test]
    fn no_row_wraps_or_overflows() {
        let mut wide = ask("01a04eb6");
        wide.statements = vec!["x".repeat(400)];
        let pending = vec![wide];
        let mut decided = ask("01a04eaa");
        decided.decided_at = Some(0);
        decided.statements = vec!["y".repeat(400)];
        for cols in [30usize, 60, 100] {
            let out = ribbon_rows(&pending, &[decided.clone()], 0, 0, cols, 40, no_labels);
            assert_eq!(out.len(), 6, "cols={cols}: rows never wrap into more rows");
            for row in &out {
                assert!(
                    row.text.chars().count() <= cols,
                    "cols={cols} overflowed with {:?}",
                    row.text
                );
            }
        }
    }

    // ── the selection ─────────────────────────────────────────────────────

    /// An answered ask shortens the list. The selection must not point past
    /// the end and answer nothing, or worse, the wrong row.
    #[test]
    fn the_selection_clamps_to_the_rows_that_exist() {
        assert_eq!(clamp_selection(5, 3), 2);
        assert_eq!(clamp_selection(5, 0), 0);
        assert_eq!(clamp_selection(1, 3), 1);
    }

    /// `j` at the bottom stays at the bottom: a wrapping selection in a list
    /// you are deciding from is a way to answer the wrong ask.
    #[test]
    fn stepping_the_selection_stops_at_the_ends() {
        assert_eq!(step_selection(0, 3, -1), 0);
        assert_eq!(step_selection(2, 3, 1), 2);
        assert_eq!(step_selection(1, 3, 1), 2);
        assert_eq!(step_selection(0, 0, 1), 0);
    }
}
