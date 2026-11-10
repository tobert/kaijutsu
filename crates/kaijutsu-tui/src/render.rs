//! The renderer, generic over `ratatui::Backend`.
//!
//! The tui owns the screen for the session (`docs/tui.md`, "The owned
//! screen"). One frame is the transcript — a view over the current
//! context's blocks, wrapped at the current width and redrawn every frame —
//! the band under it, and an overlay between the two while one is open.
//!
//! Nothing here reaches the kernel, and the only clock it reads is the one a
//! caller passes in as `now_millis`. That is what lets a `TestBackend` render
//! the same frames a real terminal gets.

use chrono::{Local, TimeZone};
use kaijutsu_types::{BlockSnapshot, Status};
use ratatui::backend::Backend;
use ratatui::layout::Rect;
use ratatui::text::Line;
use ratatui::widgets::Paragraph;
use ratatui::{Terminal};

use crate::app::App;
use crate::status::{legend_line, status_line};

/// Rows the band always takes: the in-flight strip, the blank row over the
/// draft, one draft row and the status line. A wrapped draft takes more,
/// up to [`compose_rows_cap`] — [`band_rows`] is the real height.
pub const BAND_ROWS: u16 = 4;

/// Rows the band takes at `width` right now: the strip, the blank row, the
/// draft's own rows (capped at a third of the screen) and the status line.
pub fn band_rows(app: &App, width: u16) -> u16 {
    let draft = crate::compose::input_lines(app, width, &app.palette)
        .len()
        .clamp(1, compose_rows_cap(app).max(1));
    BAND_ROWS.saturating_sub(1).saturating_add(u16::try_from(draft).unwrap_or(u16::MAX))
}

/// The compose region's ceiling: a third of the screen, which it grows
/// toward a row at a time (`docs/tui.md`, "Compose").
pub fn third_of_screen(app: &App) -> u16 {
    (app.screen_rows / 3).max(1)
}

/// Whether the in-flight strip has a running entry to animate — the event
/// loop redraws on `inflight::PHASE_MILLIS` only while it does.
///
/// Never while the terminal is unfocused: a spinner nobody is looking at is
/// a redraw per phase step over ssh for no one (`docs/tui.md`, "What owning
/// the screen lets us use").
pub fn strip_animating(app: &App) -> bool {
    if !app.focused {
        return false;
    }
    let Some(view) = app.current_view() else {
        return false;
    };
    crate::inflight::animating(&crate::inflight::entries(view.mirror.blocks().iter(), 0, &Default::default()))
}

/// `1756819331000` → `14:02:11`, in the terminal's own timezone.
///
/// The one place the wallclock is read; [`crate::present`] takes the result
/// as a string so it stays pure.
pub fn wallclock(millis: u64) -> String {
    match Local.timestamp_millis_opt(millis as i64) {
        chrono::LocalResult::Single(t) => t.format("%H:%M:%S").to_string(),
        // An unrepresentable stamp is a kernel bug, not something to invent a
        // time for.
        _ => "--:--:--".to_string(),
    }
}

/// Whether a block has finished.
///
/// `Waiting` is deliberately not settled: a gate's ask has stopped the block
/// but an answer still moves it, and the in-flight strip is where a held
/// call belongs until then.
pub fn is_settled(block: &BlockSnapshot) -> bool {
    matches!(block.status, Status::Done | Status::Error)
}

/// Whether a blank row goes above a block's divider: one row of air between
/// speakers, and none above the first speaker of a transcript or a copy
/// buffer, where there is nothing to separate from (`docs/tui.md`,
/// "Conversation").
pub fn speaker_gap(show_divider: bool, last_speaker: Option<&str>) -> bool {
    show_divider && last_speaker.is_some()
}

/// What one transcript block needs to render, resolved before the wrap cache
/// is borrowed mutably.
struct BlockPlan {
    speaker: String,
    stamp: String,
    show_divider: bool,
    gap: bool,
    collapsed: bool,
}

/// Every block of the current context that belongs in the transcript, with
/// its speaker, stamp, divider decision and fold resolved.
///
/// The draft is not one of them — it is the compose line. Neither is a block
/// the in-flight strip has taken (`inflight::takes_from_stream`): a block is
/// on screen once. A hidden block still moves the divider decision along, so
/// a call in the strip and its result still read as one unit when the pair
/// lands.
fn transcript_plan(app: &App) -> Vec<(BlockSnapshot, BlockPlan)> {
    let Some(context_id) = app.current else {
        return Vec::new();
    };
    let Some(view) = app.views.get(&context_id) else {
        return Vec::new();
    };
    let info = app.info(context_id);
    let mut out = Vec::new();
    let mut last_speaker: Option<String> = None;
    let mut last_block = None;
    for block in view.mirror.blocks() {
        if block.status == Status::Draft {
            continue;
        }
        let speaker = app.speaker_for(block, info);
        let pair = crate::present::continues_pair(last_block, block);
        let show_divider = !pair && last_speaker.as_deref() != Some(speaker.as_str());
        let gap = !pair && speaker_gap(show_divider, last_speaker.as_deref());
        last_speaker = Some(speaker.clone());
        last_block = Some((block.id, block.kind));
        if crate::inflight::takes_from_stream(block) {
            continue;
        }
        out.push((
            block.clone(),
            BlockPlan {
                speaker,
                stamp: wallclock(block.created_at),
                show_divider,
                gap,
                collapsed: view.is_collapsed(block),
            },
        ));
    }
    out
}

/// The reader's block: the block holding the line the scrolled view rests
/// on, and `None` on the live tail (`docs/tui.md`, "The reader's block").
/// The view is settled first, so a key that arrived since the last frame
/// has moved the reader already. Every act that needs a block asks
/// [`reader_target`], which asks here.
pub fn reader_block(app: &mut App, width: u16) -> Option<kaijutsu_types::BlockId> {
    app.scrolled()?;
    let index = row_index(app, width);
    let height = transcript_height(app, width);
    let scrolled = app.scrolled_mut()?;
    scrolled.settle(&index, height);
    scrolled.reader_block()
}

/// The block an act applies to: `kaijutsu_present::reader::target` over the
/// blocks the transcript draws, from the reader's block. A call or a held
/// result in the in-flight strip is not on screen, so it is not offered.
pub fn reader_target(
    app: &mut App,
    width: u16,
    fits: impl Fn(&crate::app::ContextView, &BlockSnapshot) -> bool,
) -> Option<kaijutsu_types::BlockId> {
    let reader = reader_block(app, width);
    let view = app.current_view()?;
    let plan = transcript_plan(app);
    kaijutsu_present::reader::target(plan.iter().map(|(b, _)| b), reader, |b| fits(view, b))
}

/// The block `Ctrl+O` acts on: the reader's block, or the nearest block
/// above it with a fold worth reaching for (`docs/tui.md`, "Folds").
pub fn fold_target(app: &mut App, width: u16) -> Option<kaijutsu_types::BlockId> {
    reader_target(app, width, |view, block| {
        kaijutsu_present::fold::has_fold(block, view.is_collapsed(block))
    })
}

/// Open or close the fold [`fold_target`] names, on this viewer only — no
/// RPC. `Err` says why nothing changed, for the status line.
pub fn toggle_fold(app: &mut App, width: u16) -> Result<kaijutsu_types::BlockId, &'static str> {
    let context_id = app.current.ok_or("no context on screen")?;
    let target = fold_target(app, width).ok_or("nothing to fold: no reasoning, error or long result at or above the reader")?;
    let view = app.views.get_mut(&context_id).ok_or("no transcript here; Ctrl+A <digit> reattaches")?;
    let toggled = view.toggle_fold(target);
    assert!(toggled, "the fold target {target:?} came from this context's own mirror");
    Ok(target)
}

/// The performer the current context's blocks render under.
fn performer_of(app: &App) -> String {
    app.current
        .and_then(|id| app.info(id))
        .and_then(|c| c.performer.clone())
        .unwrap_or_else(|| crate::app::NO_PERFORMER.to_string())
}

/// The one row a context with no view draws.
///
/// A context this client is not watching renders nothing at all — no blocks,
/// no stream — and an empty area reads as an empty conversation. It is two
/// states, and the status row says which: a background hydrate is still in
/// flight (`run::hydrating_notice`), or the feed ended and a switch
/// reattaches (`run::apply_feed`). Either way the transcript is waiting.
///
/// `None` for a watched context, whose own blocks are the answer, and for no
/// context at all.
fn waiting_line(app: &App) -> Option<Line<'static>> {
    let id = app.current?;
    if app.views.contains_key(&id) {
        return None;
    }
    Some(Line::styled(
        format!("waiting for {}", app.label_for(id)),
        app.palette.status(),
    ))
}

/// The transcript's visible rows: `height` rows of the current context's
/// blocks, wrapped at `width`, ending at the newest row while the view
/// follows the tail ([`crate::app::ContextView::transcript`]).
///
/// Two passes over the plan. The first counts each block's rows through the
/// wrap cache; the second renders only the blocks the window touches, so a
/// frame costs a screenful of work rather than a context's.
pub fn transcript_window(app: &mut App, width: u16, height: u16) -> Vec<Line<'static>> {
    if height == 0 {
        return Vec::new();
    }
    if let Some(placeholder) = waiting_line(app) {
        return vec![placeholder];
    }
    let Some(frame) = plan_rows(app, width) else {
        return Vec::new();
    };
    let PlannedRows { plan, rows, performer } = frame;
    let height = usize::from(height);
    let palette = app.palette;

    let index = crate::copy::RowIndex::new(rows.clone());
    let total = index.total();
    let last_top = total.saturating_sub(height);
    // Off the tail the top row is the reader's anchor, settled against this
    // frame's counts; on it, the newest rows.
    let start = match app.scrolled_mut() {
        Some(scrolled) => scrolled.settle(&index, height),
        None => last_top,
    };

    let mut lines: Vec<Line<'static>> = Vec::with_capacity(height.min(total));
    let mut row = 0usize;
    let mut edge = None;
    for ((block, item), entry) in plan.iter().zip(&rows) {
        let block_rows = entry.rows;
        if row + block_rows <= start {
            row += block_rows;
            continue;
        }
        let view = block_view(item, &performer, block);
        let lineage = app.lineage_for(block);
        let view = crate::present::BlockView { lineage, ..view };
        let mut block_lines = app.wrap.lines(block, &view, width, &palette).to_vec();
        if item.gap {
            block_lines.insert(0, Line::default());
        }
        let skip = start.saturating_sub(row);
        let mut drawn: Vec<Line<'static>> = block_lines.into_iter().skip(skip).collect();
        let room = height - lines.len();
        // The head may be above the window without hiding anything the
        // player is reading; a tail the window could not fit is a different
        // matter, and so is a fold, whose text was never drawn.
        let cropped = drawn.len() > room;
        drawn.truncate(room);
        if !drawn.is_empty() {
            let whole = !cropped && !item.collapsed;
            edge = Some((block.id, whole.then(|| block.content.chars().count() as u64)));
        }
        lines.extend(drawn);
        row += block_rows;
        if lines.len() >= height {
            break;
        }
    }
    if let Some((block, shown)) = edge {
        record_edge(app, block, shown);
    }
    // The reader's line, the marked range and the search matches are painted
    // onto the rows the window drew: the terminal's own cursor is hidden
    // while the view is off the tail.
    if let Some(scrolled) = app.scrolled() {
        scrolled.decorate(start, &mut lines, &palette);
    }
    lines
}

/// Every planned block of the transcript with the rows it takes at `width`
/// — the first of `transcript_window`'s two passes, shared with the
/// whole-transcript render the scrolled view's search and yank read.
struct PlannedRows {
    plan: Vec<(BlockSnapshot, BlockPlan)>,
    /// Where each block sits: its leading rows (the blank row and the
    /// divider) and its rows in all. The anchor counts from the first
    /// content row, so a divider appearing above a block — which a block
    /// inserted above it, or a tool call settling out of the in-flight
    /// strip, can do — leaves the reader on the same text.
    rows: Vec<crate::copy::RowBlock>,
    performer: String,
}

/// `None` when the current context has nothing to draw.
fn plan_rows(app: &mut App, width: u16) -> Option<PlannedRows> {
    let plan = transcript_plan(app);
    if plan.is_empty() {
        return None;
    }
    let palette = app.palette;
    let performer = performer_of(app);
    let mut rows: Vec<crate::copy::RowBlock> = Vec::with_capacity(plan.len());
    for (block, item) in &plan {
        let view = block_view(item, &performer, block);
        let lineage = app.lineage_for(block);
        let view = crate::present::BlockView { lineage, ..view };
        let counted = app.wrap.lines(block, &view, width, &palette).len();
        // `render_block` puts the divider first when there is one, and the
        // gap row goes above that.
        let leading = usize::from(item.gap) + usize::from(item.show_divider);
        rows.push(crate::copy::RowBlock {
            block: block.id,
            leading: leading.min(usize::from(item.gap) + counted),
            rows: usize::from(item.gap) + counted,
        });
    }
    Some(PlannedRows { plan, rows, performer })
}

/// Where every block of the transcript sits at `width`, so a key path that
/// runs between frames can settle the scrolled view against the same counts
/// the next frame will use. Pass one of [`transcript_window`]'s two passes,
/// which the wrap cache makes cheap; no block is rendered.
pub fn row_index(app: &mut App, width: u16) -> crate::copy::RowIndex {
    match plan_rows(app, width) {
        Some(PlannedRows { rows, .. }) => crate::copy::RowIndex::new(rows),
        None => crate::copy::RowIndex::default(),
    }
}

/// The whole transcript, row for row as [`transcript_window`] draws it —
/// what the scrolled view searches and what a yank copies out. Rendering
/// every block is a keystroke's worth of work, not a frame's, so only `/`,
/// `n`, `N` and `y` ask for it.
pub fn transcript_all_lines(app: &mut App, width: u16) -> Vec<Line<'static>> {
    let Some(PlannedRows { plan, rows, performer }) = plan_rows(app, width) else {
        return Vec::new();
    };
    let palette = app.palette;
    let mut out: Vec<Line<'static>> = Vec::with_capacity(rows.iter().map(|r| r.rows).sum());
    for (block, item) in &plan {
        let view = block_view(item, &performer, block);
        let lineage = app.lineage_for(block);
        let view = crate::present::BlockView { lineage, ..view };
        let mut block_lines = app.wrap.lines(block, &view, width, &palette).to_vec();
        if item.gap {
            block_lines.insert(0, Line::default());
        }
        out.extend(block_lines);
    }
    debug_assert_eq!(
        out.len(),
        rows.iter().map(|r| r.rows).sum::<usize>(),
        "the row index and the rendered rows must agree"
    );
    out
}

/// The plain text of an inclusive range of transcript rows, newline-joined —
/// what `y` puts on the clipboard and in the paste buffer.
pub fn transcript_rows_text(app: &mut App, width: u16, start: usize, end: usize) -> String {
    let lines = transcript_all_lines(app, width);
    if lines.is_empty() {
        return String::new();
    }
    let start = start.min(lines.len() - 1);
    let end = end.min(lines.len() - 1);
    lines[start..=end].iter().map(crate::copy::line_text).collect::<Vec<_>>().join("\n")
}

/// How a screen `height` rows tall is divided: the transcript's rows, the
/// overlay's, and the band's. The band takes what it needs from the bottom,
/// an overlay takes what is left of `overlay` rows, and the transcript has
/// the rest.
///
/// One answer for the frame and for the key path, so a page step and the
/// reader's own offset agree with what was drawn. An overlay opening while
/// the view is scrolled shortens the transcript, and a key measured against
/// the taller screen would overshoot.
fn split_rows(height: u16, band: u16, overlay: usize) -> (u16, u16, u16) {
    let band = band.min(height);
    let overlay = u16::try_from(overlay).unwrap_or(u16::MAX).min(height.saturating_sub(band));
    (height.saturating_sub(band).saturating_sub(overlay), overlay, band)
}

/// The rows the transcript area has at `width` right now — the screen less
/// the band and whatever overlay is open ([`split_rows`]). What the key path
/// sizes a page step and the scrolled view's own settle with.
pub fn transcript_height(app: &mut App, width: u16) -> usize {
    let band = band_rows(app, width);
    let overlay = overlay_lines(app, width).len();
    usize::from(split_rows(app.screen_rows, band, overlay).0).max(1)
}

/// The view a planned block renders through, lineage left empty for the
/// caller to fill — it needs `app` immutably, and the wrap cache is a
/// mutable borrow that cannot overlap it.
fn block_view<'a>(
    item: &'a BlockPlan,
    performer: &'a str,
    block: &'a BlockSnapshot,
) -> crate::present::BlockView<'a> {
    let (tool, arg) = crate::app::tool_header(block);
    crate::present::BlockView {
        speaker: &item.speaker,
        performer,
        stamp: &item.stamp,
        show_divider: item.show_divider,
        tool,
        arg,
        lineage: Vec::new(),
        collapsed: item.collapsed,
        local_ctx: Some(block.id.context_id),
    }
}

/// Record the player's edge of context: the block that ended the window,
/// and how many of its characters were actually drawn — `None` when the
/// window cropped its tail or the block is folded, because the kernel never
/// guesses one (`ContextView::edge`, `docs/prompts.md`, "The submit verb").
///
/// A frame that drew no transcript row records nothing: what the player last
/// saw is still what they last saw.
fn record_edge(app: &mut App, block: kaijutsu_types::BlockId, shown: Option<u64>) {
    let Some(context_id) = app.current else {
        return;
    };
    if let Some(view) = app.views.get_mut(&context_id) {
        view.shown_tail = Some((block, shown));
    }
}

/// One frame of the band: its rows, and where the terminal's cursor sits
/// among them as `(row, col)` indexed into `lines`. `None` while nothing is
/// being typed — the armed legend and every overlay that owns the keyboard
/// have no cursor, and the terminal hides it.
pub struct BandFrame {
    pub lines: Vec<Line<'static>>,
    pub cursor: Option<(u16, u16)>,
}

/// The band's rows alone — [`band_frame`] without the cursor. The frame
/// path wants the cursor with them, so this is the tests' way in.
#[cfg(test)]
fn band_lines(app: &mut App, width: u16, now_millis: u64, armed: bool) -> Vec<Line<'static>> {
    band_frame(app, width, now_millis, armed).lines
}

/// The band: the in-flight strip, a blank row, the draft (or the `:` bar, or
/// the armed prefix legend) and the status line.
pub fn band_frame(app: &mut App, width: u16, now_millis: u64, armed: bool) -> BandFrame {
    let palette = app.palette;
    let mut lines = Vec::new();

    // The in-flight strip: one row, always, so the band never resizes for a
    // tool call coming or going — only the row's text changes.
    let strip = match app.current_view() {
        Some(view) => crate::inflight::entries(view.mirror.blocks().iter(), now_millis, &crate::inflight::ask_index(&app.open_asks)),
        None => Vec::new(),
    };
    lines.push(crate::inflight::strip_line(
        &strip,
        width,
        &palette,
        crate::inflight::phase(now_millis),
    ));

    // One blank row separates what is being read from what is being typed
    // (`docs/tui.md`, "Conversation").
    lines.push(Line::default());

    // The draft wraps and takes up to a third of the screen. Past the cap it
    // shows the rows around the cursor, the way vim scrolls a long command
    // line.
    let mut input = crate::compose::input_lines(app, width, &palette);
    let (mut cursor_row, cursor_col) = app.compose.cursor_cell(width);
    let cap = compose_rows_cap(app);
    debug_assert_eq!(
        usize::from(band_rows(app, width)),
        input.len().min(cap.max(1)).max(1) + 3,
        "the band's counted height and its drawn rows must agree"
    );
    if input.len() > cap {
        let start = usize::from(cursor_row).saturating_sub(cap - 1).min(input.len() - cap);
        input = input[start..start + cap].to_vec();
        cursor_row = u16::try_from(usize::from(cursor_row) - start).unwrap_or(0);
    }

    // While `Ctrl+A` is pending the legend takes the compose row, not the
    // status line: the status line's seat digits are what the player is
    // about to press, and covering them was the bug Amy hit.
    let cursor = if armed {
        lines.push(legend_line(width, &palette));
        None
    } else {
        // The terminal's own cursor, on the draft's vi cursor: the row is
        // the compose region's first line plus the draft row it is on. An
        // overlay that owns the keyboard hides it — the keys go there, not
        // to the draft.
        let first = u16::try_from(lines.len()).unwrap_or(u16::MAX);
        lines.extend(input);
        // The draft stays drawn and live while the transcript is scrolled —
        // typing snaps, so it never changes there — but the terminal's
        // cursor is hidden, because the keys are the transcript's.
        (!overlay_holds_keys(app) && app.following())
            .then_some((first.saturating_add(cursor_row), cursor_col))
    };
    // Off the tail the status row is the scrolled view's own: the position,
    // the keys, and the search prompt while one is being typed. One row
    // changes; the rest of the band is what it always was.
    lines.push(match app.scrolled() {
        Some(scrolled) => scrolled.hint_line(width, &palette),
        None => status_line(&app.status_model(now_millis), width, &palette),
    });
    BandFrame { lines, cursor }
}

/// Whether an overlay owns the keyboard. The picker, an armed ask card and
/// the ledger take every key while they are up, so the draft shows no
/// cursor; a disarmed ask card leaves the keys, and the cursor, to the
/// draft.
fn overlay_holds_keys(app: &App) -> bool {
    app.picker.is_some() || app.ask_card_armed() || app.ledger_view.is_some()
}

/// The overlay's rows: the picker, or an ask card or the ledger view,
/// whichever is open, and empty when none is. One overlay at a time, drawn
/// between the transcript and the band (`docs/tui.md`, "The owned screen").
pub fn overlay_lines(app: &mut App, width: u16) -> Vec<Line<'static>> {
    if let Some(picker) = &app.picker {
        return crate::picker::render(picker, width, &app.palette);
    }
    crate::asks::active_view_lines(app, width).unwrap_or_default()
}

/// One frame of the owned screen: the transcript on top, the band at the
/// bottom, and an overlay between them while one is open.
///
/// Every region keeps its tail when the screen is too short for it: the
/// status line is the last row, an overlay's key-hints line is the last row
/// it has, and the transcript shows its newest lines. Showing everything but
/// the way to answer is the bug that cropping from the front closes.
pub fn draw_screen<B: Backend>(
    terminal: &mut Terminal<B>,
    app: &mut App,
    now_millis: u64,
    armed: bool,
) -> Result<(), B::Error> {
    let size = terminal.size()?;
    let (width, height) = (size.width, size.height);
    let overlay = overlay_lines(app, width);
    let (transcript_rows, overlay_rows, _) = split_rows(height, band_rows(app, width), overlay.len());
    // The transcript is rendered first: it is what settles the scrolled
    // view, and the band's status row reads the settled position. Built the
    // other way round, the readout showed the frame before.
    let transcript = transcript_window(app, width, transcript_rows);
    let band = band_frame(app, width, now_millis, armed);
    let band_rows = u16::try_from(band.lines.len()).unwrap_or(u16::MAX).min(height);

    terminal.draw(|frame| {
        let area = frame.area();
        // The transcript is bottom-aligned in its own rows: a conversation
        // shorter than the screen sits on the band the way a shell's output
        // sits on its prompt.
        let drawn = u16::try_from(transcript.len()).unwrap_or(u16::MAX).min(transcript_rows);
        if drawn > 0 {
            let rect = Rect {
                y: area.y + transcript_rows - drawn,
                height: drawn,
                ..area
            };
            frame.render_widget(Paragraph::new(transcript), rect);
        }
        if overlay_rows > 0 {
            let start = overlay.len() - usize::from(overlay_rows);
            let rect = Rect {
                y: area.y + transcript_rows,
                height: overlay_rows,
                ..area
            };
            frame.render_widget(Paragraph::new(overlay[start..].to_vec()), rect);
        }
        let start = band.lines.len() - usize::from(band_rows);
        let rect = Rect {
            y: area.y + height - band_rows,
            height: band_rows,
            ..area
        };
        frame.render_widget(Paragraph::new(band.lines[start..].to_vec()), rect);
        // A cursor on a row the crop dropped stays hidden with the row.
        if let Some((row, col)) = band.cursor
            && let Some(y) = usize::from(row).checked_sub(start)
        {
            let y = u16::try_from(y).unwrap_or(u16::MAX);
            frame.set_cursor_position((rect.x + col, rect.y + y));
        }
    })?;
    Ok(())
}

/// One full-screen surface — the editor, the diff viewer — drawn
/// over the whole owned screen. `cursor` places the terminal's cursor;
/// `None` hides it, which is what a surface with no insertion point wants.
pub fn draw_surface<B: Backend>(
    terminal: &mut Terminal<B>,
    lines: Vec<Line<'static>>,
    cursor: Option<(u16, u16)>,
) -> Result<(), B::Error> {
    terminal.draw(|frame| {
        let area = frame.area();
        frame.render_widget(Paragraph::new(lines), area);
        if let Some(position) = cursor {
            frame.set_cursor_position(position);
        }
    })?;
    Ok(())
}

/// The most rows the compose region may take: [`third_of_screen`].
pub fn compose_rows_cap(app: &App) -> usize {
    usize::from(third_of_screen(app))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::ContextView;
    use kaijutsu_client::{ContextInfo, ContextMirror};
    use kaijutsu_types::{
        BlockId, BlockKind, BlockSnapshotBuilder, ContextId, PrincipalId, Role,
    };
    use ratatui::backend::TestBackend;

    fn ctx(id: ContextId, label: &str) -> ContextInfo {
        ContextInfo {
            id,
            label: label.to_string(),
            forked_from: None,
            provider: String::new(),
            model: "deepseek/deepseek-v4".to_string(),
            created_at: 1_000,
            trace_id: [0u8; 16],
            fork_kind: None,
            archived: false,
            concluded_at: None,
            keywords: Vec::new(),
            top_block_preview: None,
            live_status: Status::Pending,
            last_activity_at: None,
            track_id: None,
            promoted_at: Some(1_000),
            demoted_at: None,
            paused_at: None,
            context_window: Some(1_000),
            context_used_tokens: Some(1_000),
            context_used_pct: Some(42.0),
            background_running_count: 0,
            background_oldest_running_started_at: None,
            background_last_finished_at: None,
            background_last_finished_status: None,
            background_last_exit_code: None,
            origin_host: None,
            cwd: None,
            last_call_at: Some(0),
            cache_read_tokens: Some(910),
            cache_write_tokens: None,
            cache_ttl_secs: Some(300),
            input_tokens: Some(1_000),
            cost: None,
            performer: Some("coder".to_string()),
            performed_by_person: false,
            is_root_console: false,
            is_console: false,
            is_mcp_session: false,
        }
    }

    fn block(
        context: ContextId,
        seq: u64,
        kind: BlockKind,
        role: Role,
        status: Status,
        content: &str,
    ) -> BlockSnapshot {
        BlockSnapshotBuilder::new(BlockId::new(context, PrincipalId::new(), seq), kind)
            .role(role)
            .status(status)
            .content(content)
            .build()
    }

    /// Two settled blocks, a status line and a 24-row screen.
    fn fixture() -> (App, ContextId) {
        let id = ContextId::new();
        let mut app = App::new("amy");
        app.set_contexts(vec![ctx(id, "kaijutsu")]);
        let mut mirror = ContextMirror::new(id);
        mirror
            .apply_snapshot(
                vec![
                    block(id, 1, BlockKind::Text, Role::User, Status::Done, "and getattr?"),
                    block(
                        id,
                        2,
                        BlockKind::Text,
                        Role::Model,
                        Status::Done,
                        "rename and getattr share the cause.",
                    ),
                ],
                1,
            )
            .expect("snapshot applies");
        app.views.insert(id, ContextView::new(mirror));
        app.switch_to(id);
        (app, id)
    }

    /// Replace the current context's blocks, the way a rehydrate does:
    /// the mirror is new, the view — and with it the reader's place in the
    /// transcript — is the one the context already had.
    fn snapshot(app: &mut App, id: ContextId, blocks: Vec<BlockSnapshot>) {
        let mut mirror = ContextMirror::new(id);
        mirror.apply_snapshot(blocks, 1).expect("snapshot applies");
        match app.views.get_mut(&id) {
            Some(view) => {
                view.mirror = mirror;
                view.auto_fold();
            }
            None => {
                app.views.insert(id, ContextView::new(mirror));
            }
        }
        app.switch_to(id);
    }

    fn rows(terminal: &Terminal<TestBackend>) -> Vec<String> {
        let buf = terminal.backend().buffer();
        (0..buf.area.height)
            .map(|y| {
                (0..buf.area.width)
                    .map(|x| buf[(x, y)].symbol())
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect()
    }

    fn text_of(lines: &[Line<'static>]) -> Vec<String> {
        lines
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect()
    }

    /// The whole transcript at `width`, as plain text: a window taller than
    /// any fixture crops nothing.
    fn transcript_text(app: &mut App, width: u16) -> Vec<String> {
        text_of(&transcript_window(app, width, u16::MAX))
    }

    fn band_text(app: &mut App, width: u16) -> Vec<String> {
        text_of(&band_lines(app, width, 0, false))
    }

    /// What a frame over a long context costs — the measurement behind the
    /// decision not to cap rendered lines (`docs/tui.md`, "The buffer":
    /// *"a cap on rendered lines per transcript is the first thing to add if
    /// memory says so"*).
    ///
    /// A frame is two passes: [`plan_rows`] touches every block, and the
    /// window renders only the rows it draws. The plan pass is O(blocks) —
    /// [`transcript_plan`] clones each block and names its speaker — but the
    /// wrap cache makes its measurement a render key and a hash lookup, not
    /// a wrap. Over 5,000 blocks a warm frame is 2.4 ms in release and
    /// 12.9 ms in debug, against an 80 ms redraw tick that only fires when
    /// something changed. No cap: it would bound the render pass, which is
    /// already a screenful, and leave the plan pass — where the cost is —
    /// exactly as it is.
    ///
    /// The bound below is a regression tripwire for a frame that started
    /// wrapping the whole context again, not the measurement itself.
    #[test]
    fn a_warm_frame_over_five_thousand_blocks_costs_a_screenful_not_a_context() {
        let (mut app, id) = fixture();
        let blocks: Vec<BlockSnapshot> = (0..5_000)
            .map(|n| {
                block(
                    id,
                    n + 1,
                    BlockKind::Text,
                    if n % 2 == 0 { Role::User } else { Role::Model },
                    Status::Done,
                    &format!("block {n} body text that wraps at eighty columns once or twice over"),
                )
            })
            .collect();
        snapshot(&mut app, id, blocks);
        app.screen_rows = 40;

        // The first frame wraps every block; the warm frames that follow are
        // what a stream of appends actually costs.
        let _ = transcript_window(&mut app, 80, 40);
        let start = std::time::Instant::now();
        for _ in 0..10 {
            let _ = transcript_window(&mut app, 80, 40);
        }
        let per_frame = start.elapsed() / 10;
        assert!(
            per_frame < std::time::Duration::from_millis(50),
            "a warm 5,000-block frame took {per_frame:?}; the plan pass is wrapping again"
        );
        eprintln!("warm frame over 5,000 blocks: {per_frame:?}");
    }

    /// A context this client is not watching has no blocks to draw, so it
    /// draws one line saying so rather than an empty area that reads as an
    /// empty conversation. It is the state a switch to a context still being
    /// hydrated reaches (`run::switch_seat`), and the state a feed that
    /// ended leaves behind (`run::apply_feed`, `FeedEvent::Terminated`).
    #[test]
    fn a_context_with_no_view_draws_one_waiting_row() {
        let (mut app, watched) = fixture();
        let mut info = app.contexts.first().cloned().unwrap_or_else(|| {
            panic!("the fixture seats its context");
        });
        info.id = ContextId::new();
        info.label = "hotseat".to_string();
        let unwatched = info.id;
        app.set_contexts(vec![info]);
        app.switch_to(unwatched);

        assert_eq!(
            transcript_text(&mut app, 80),
            vec!["waiting for hotseat".to_string()],
            "the area says what it is waiting for"
        );

        // A watched context answers with its own blocks, never this row.
        app.switch_to(watched);
        let rows = transcript_text(&mut app, 80);
        assert!(!rows.is_empty(), "the fixture has blocks");
        assert!(
            !rows.iter().any(|r| r.contains("waiting for")),
            "a watched context never draws the waiting row: {rows:?}"
        );
    }

    /// The transcript is the whole context, redrawn every frame: a block
    /// does not leave it, and a second frame renders the same rows.
    #[test]
    fn the_transcript_holds_every_block_on_every_frame() {
        let (mut app, _) = fixture();
        let first = transcript_text(&mut app, 80);
        assert_eq!(first.len(), 5, "divider, text, blank, divider, text: {first:?}");
        assert_eq!(transcript_text(&mut app, 80), first, "the same frame twice");
    }

    /// A run of blocks by one speaker carries one divider.
    #[test]
    fn a_run_by_one_speaker_repeats_no_divider() {
        let id = ContextId::new();
        let mut app = App::new("amy");
        app.set_contexts(vec![ctx(id, "kaijutsu")]);
        snapshot(
            &mut app,
            id,
            vec![
                block(id, 1, BlockKind::Thinking, Role::Model, Status::Done, "hmm"),
                block(id, 2, BlockKind::Text, Role::Model, Status::Done, "ok"),
            ],
        );
        let rows = transcript_text(&mut app, 80);
        assert_eq!(
            rows.iter().filter(|r| r.starts_with('─')).count(),
            1,
            "one divider for one speaker: {rows:?}"
        );
    }

    /// A streaming block is in the transcript like any other, and its newest
    /// text is what a following view shows.
    #[test]
    fn a_streaming_block_is_in_the_transcript() {
        let (mut app, id) = fixture();
        snapshot(
            &mut app,
            id,
            vec![block(id, 3, BlockKind::Text, Role::Model, Status::Running, "still going")],
        );
        let text = transcript_text(&mut app, 80);
        assert!(text.iter().any(|l| l.contains("still going")), "got {text:?}");
    }

    /// The wrap follows the width: the same block takes more rows at half
    /// the columns, with no cached line surviving the change
    /// (`docs/tui.md`, "The buffer").
    #[test]
    fn a_narrower_width_rewraps_the_transcript() {
        let (mut app, id) = fixture();
        let long = "wrapme ".repeat(20);
        snapshot(
            &mut app,
            id,
            vec![block(id, 3, BlockKind::Text, Role::Model, Status::Done, &long)],
        );
        let wide = transcript_text(&mut app, 80).len();
        let narrow = transcript_text(&mut app, 40).len();
        assert!(narrow > wide, "40 columns takes more rows than 80: {narrow} vs {wide}");
        assert_eq!(transcript_text(&mut app, 80).len(), wide, "and back again");
    }

    /// Following shows the newest rows; leaving the tail holds the view
    /// where the reader put it while the block keeps streaming
    /// (`docs/tui.md`, "Scrolling is copy mode").
    #[test]
    fn the_view_follows_the_tail_until_it_is_scrolled() {
        let (mut app, id) = fixture();
        let body = (0..60).map(|n| format!("line {n}")).collect::<Vec<_>>().join("\n");
        snapshot(
            &mut app,
            id,
            vec![block(id, 9, BlockKind::Text, Role::Model, Status::Running, &body)],
        );
        let tail = text_of(&transcript_window(&mut app, 80, 5));
        assert_eq!(tail.len(), 5);
        assert_eq!(tail.last().map(String::as_str), Some("line 59"), "the tail: {tail:?}");

        let mut scrolled = crate::copy::Scrolled::entering(5);
        scrolled.scroll(-4);
        assert!(app.set_scrolled(scrolled), "the fixture context is watched");
        let window = text_of(&transcript_window(&mut app, 80, 5));
        assert_eq!(window.last().map(String::as_str), Some("line 55"), "got {window:?}");

        // The block streams on; the anchored view does not move.
        let body = (0..70).map(|n| format!("line {n}")).collect::<Vec<_>>().join("\n");
        snapshot(
            &mut app,
            id,
            vec![block(id, 9, BlockKind::Text, Role::Model, Status::Running, &body)],
        );
        let held = text_of(&transcript_window(&mut app, 80, 5));
        assert_eq!(held, window, "the anchor held the view still: {held:?}");
    }

    /// The text on the reader's own line: the one row `decorate` painted in
    /// reverse, since the terminal's cursor is hidden while scrolled.
    fn reader_line(window: &[Line<'static>]) -> String {
        let painted: Vec<&Line<'static>> = window
            .iter()
            .filter(|l| {
                l.spans
                    .iter()
                    .any(|s| s.style.add_modifier.contains(ratatui::style::Modifier::REVERSED))
            })
            .collect();
        assert_eq!(painted.len(), 1, "exactly one row is the reader's: {:?}", text_of(window));
        crate::copy::line_text(painted[0])
    }

    /// A block arriving above the anchored one by a different speaker gives
    /// that block a divider and a blank row, moving every one of its rows
    /// down two. The anchor counts from the block's first *content* row, so
    /// the reader keeps the same text — a row index would have slid.
    #[test]
    fn a_divider_arriving_above_the_anchor_leaves_the_reader_on_the_same_text() {
        let (mut app, id) = fixture();
        let body = |tag: &str| (0..30).map(|n| format!("{tag}-{n}")).collect::<Vec<_>>().join("\n");
        let m0 = block(id, 1, BlockKind::Text, Role::Model, Status::Done, &body("m0"));
        let m1 = block(id, 2, BlockKind::Text, Role::Model, Status::Done, &body("m1"));
        let interjection = block(id, 3, BlockKind::Text, Role::User, Status::Done, "wait");

        // Two model blocks in a row: the second carries no divider.
        snapshot(&mut app, id, vec![m0.clone(), m1.clone()]);
        let mut scrolled = crate::copy::Scrolled::entering(10);
        scrolled.scroll(-20);
        assert!(app.set_scrolled(scrolled), "the fixture context is watched");
        let before = reader_line(&transcript_window(&mut app, 80, 10));
        assert!(before.starts_with("m1-"), "the reader is inside the second block: {before:?}");

        // A different speaker lands between them: the second block gains a
        // blank row and a divider.
        snapshot(&mut app, id, vec![m0, interjection, m1]);
        let after = reader_line(&transcript_window(&mut app, 80, 10));
        assert_eq!(after, before, "the reader's own text moved");
    }

    /// The very first frame off the tail already says where the reader is.
    /// The band's status row carries the readout and the transcript is what
    /// settles the view, so the transcript is rendered first; built the
    /// other way round the first frame read `line 0/0`.
    #[test]
    fn the_first_frame_off_the_tail_reads_the_position() {
        let (mut app, id) = fixture();
        app.screen_rows = 12;
        let body = (0..60).map(|n| format!("line {n}")).collect::<Vec<_>>().join("\n");
        snapshot(
            &mut app,
            id,
            vec![block(id, 9, BlockKind::Text, Role::Model, Status::Done, &body)],
        );
        // Entered but never settled — what `Ctrl+A [` leaves for the frame.
        assert!(app.set_scrolled(crate::copy::Scrolled::entering(8)), "the fixture context is watched");

        let mut terminal = Terminal::new(TestBackend::new(80, 12)).expect("test terminal");
        draw_screen(&mut terminal, &mut app, 0, false).expect("draw");
        let status = rows(&terminal).last().expect("a status row").clone();
        let at = status.find("line ").unwrap_or_else(|| panic!("no readout: {status:?}")) + 5;
        let figure = status[at..].split_whitespace().next().expect("a figure");
        let (n, m) = figure.split_once('/').unwrap_or_else(|| panic!("not N/M: {figure:?}"));
        let (n, m): (usize, usize) = (n.parse().expect("N"), m.parse().expect("M"));
        assert!(n >= 1 && m >= 1, "the first frame read {n}/{m}");
        assert_eq!(m, 61, "sixty body rows under the block's divider");
        assert_eq!(n, 61, "entered on the newest row");
    }

    /// The reader's line is painted, since the terminal's own cursor is
    /// hidden while the view is off the tail.
    #[test]
    fn the_scrolled_view_paints_the_readers_line() {
        let (mut app, id) = fixture();
        let body = (0..60).map(|n| format!("line {n}")).collect::<Vec<_>>().join("\n");
        snapshot(
            &mut app,
            id,
            vec![block(id, 9, BlockKind::Text, Role::Model, Status::Running, &body)],
        );
        assert!(app.set_scrolled(crate::copy::Scrolled::entering(5)), "the fixture context is watched");
        let window = transcript_window(&mut app, 80, 5);
        let reversed: Vec<usize> = window
            .iter()
            .enumerate()
            .filter(|(_, l)| {
                l.spans
                    .iter()
                    .any(|s| s.style.add_modifier.contains(ratatui::style::Modifier::REVERSED))
            })
            .map(|(i, _)| i)
            .collect();
        assert_eq!(reversed, vec![4], "the reader entered on the last row: {reversed:?}");
    }

    // ────────────────────────────────────────────────────────────────────
    // The player's edge (docs/prompts.md, "The submit verb")
    // ────────────────────────────────────────────────────────────────────

    /// The edge is the block that ended the window, with the count of
    /// characters actually rendered for it.
    #[test]
    fn the_edge_is_the_last_block_the_window_drew() {
        let (mut app, id) = fixture();
        let _ = transcript_window(&mut app, 80, u16::MAX);
        let edge = app.views[&id].edge().expect("two settled blocks were drawn");
        let last = app.views[&id].mirror.blocks().last().expect("a last block").clone();
        assert_eq!(edge.block, last.id);
        assert_eq!(edge.shown, Some(last.content.chars().count() as u64));
    }

    /// A scrolled view crops the last block's tail the same way.
    #[test]
    fn a_cropped_tail_sends_no_count() {
        let (mut app, id) = fixture();
        let body = (0..60).map(|n| format!("line {n}")).collect::<Vec<_>>().join("\n");
        snapshot(
            &mut app,
            id,
            vec![block(id, 9, BlockKind::Text, Role::Model, Status::Running, &body)],
        );
        let mut scrolled = crate::copy::Scrolled::entering(5);
        scrolled.scroll(-60);
        assert!(app.set_scrolled(scrolled), "the fixture context is watched");
        let _ = transcript_window(&mut app, 80, 5);
        let edge = app.views[&id].edge().expect("the block was drawn");
        assert_eq!(edge.shown, None, "the window cropped the tail");
    }

    /// A frame with no transcript rows records no edge: a screen full of
    /// overlay leaves the last one the player actually saw standing.
    #[test]
    fn a_window_with_no_rows_records_nothing() {
        let (mut app, id) = fixture();
        let _ = transcript_window(&mut app, 80, u16::MAX);
        let before = app.views[&id].edge();
        assert!(before.is_some());
        let _ = transcript_window(&mut app, 80, 0);
        assert_eq!(app.views[&id].edge(), before, "an empty window leaves the edge alone");
    }

    /// The terminal's own cursor rests on the compose row, past the prompt
    /// and the draft — no painted cell stands in for it — and the armed
    /// legend, which takes that row, leaves no cursor at all.
    #[test]
    fn the_cursor_sits_on_the_compose_row_after_the_draft() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let (mut app, _id) = fixture();
        for c in ['i', 'h', 'i'] {
            app.compose.press(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
        }
        let mut terminal = Terminal::new(TestBackend::new(80, 12)).expect("test terminal");
        draw_screen(&mut terminal, &mut app, 0, false).expect("draw");
        let compose_row = rows(&terminal)
            .iter()
            .position(|r| r.starts_with("❯ hi"))
            .expect("the compose row is drawn");
        let at = terminal.get_cursor_position().expect("cursor position");
        assert_eq!((at.x, at.y), (4, compose_row as u16), "past `❯ hi`");

        let armed = band_frame(&mut app, 80, 0, true);
        assert_eq!(armed.cursor, None, "the legend row has no cursor");
    }

    /// A settled tool call and its result, whose body is `rows` lines.
    fn long_result(id: ContextId, rows: usize) -> Vec<BlockSnapshot> {
        let call = BlockSnapshotBuilder::new(
            BlockId::new(id, PrincipalId::new(), 9),
            BlockKind::ToolCall,
        )
        .role(Role::Model)
        .status(Status::Done)
        .tool_name("builtin.shell.shell")
        .tool_input(r#"{"command":"cargo build"}"#)
        .content(r#"{"command":"cargo build"}"#)
        .created_at(1_000)
        .build();
        let body: String = (0..rows)
            .map(|n| format!("line {n}\n"))
            .collect::<String>();
        let result = BlockSnapshotBuilder::new(
            BlockId::new(id, PrincipalId::new(), 10),
            BlockKind::ToolResult,
        )
        .role(Role::Tool)
        .status(Status::Done)
        .tool_call_id(call.id)
        .content(body)
        .created_at(2_000)
        .build();
        vec![call, result]
    }

    /// A long result that nothing follows yet renders whole: it folds only
    /// once the next block arrives (`docs/tui.md`, "Folds"). There is no
    /// cap and no footer — the transcript scrolls, so a `cargo build`'s
    /// worth of output renders every line.
    #[test]
    fn a_long_tool_result_renders_whole_until_something_follows_it() {
        let (mut app, id) = fixture();
        app.screen_rows = 30;
        snapshot(&mut app, id, long_result(id, 500));

        // Off the tail, at the top: the first line is on screen.
        let mut scrolled = crate::copy::Scrolled::entering(26);
        scrolled.jump_to(0);
        assert!(app.set_scrolled(scrolled), "the fixture context is watched");
        let head = text_of(&transcript_window(&mut app, 80, 26));
        assert!(head.iter().any(|r| r.contains("cargo build")), "the call's divider: {head:?}");
        assert!(head.iter().any(|r| r.contains("line 0")), "got {head:?}");

        // Every one of the five hundred lines is in the transcript, and no
        // footer stands in for any of them.
        let whole = text_of(&transcript_all_lines(&mut app, 80));
        for n in [0usize, 1, 250, 498, 499] {
            assert!(whole.iter().any(|r| r == &format!("line {n}")), "line {n} is missing");
        }
        assert!(!whole.iter().any(|r| r.contains("more lines")), "no footer: {whole:?}");
    }

    /// A short result is untouched, as it always was.
    #[test]
    fn a_short_tool_result_keeps_every_line() {
        let (mut app, id) = fixture();
        app.screen_rows = 40;
        snapshot(&mut app, id, long_result(id, 3));

        let rows = transcript_text(&mut app, 80);
        for n in 0..3 {
            assert!(
                rows.iter().any(|r| r.contains(&format!("line {n}"))),
                "line {n} must survive: {rows:?}"
            );
        }
    }


    /// One frame: the transcript on top, the band under it, the status line
    /// last.
    #[test]
    fn the_frame_carries_the_transcript_and_the_status_line() {
        let (mut app, _) = fixture();
        // Five transcript rows, then the band's four: the strip, the blank
        // row, the prompt, the status line.
        let mut terminal =
            Terminal::new(TestBackend::new(96, 9)).expect("test backend builds");
        app.screen_rows = 9;
        draw_screen(&mut terminal, &mut app, 60_000, false).expect("draw");

        let rows = rows(&terminal);
        assert!(
            rows[0].starts_with("─ amy · coder ─"),
            "the user's divider leads: {rows:?}"
        );
        assert_eq!(rows[1], "and getattr?");
        assert_eq!(rows[2], "", "one blank row of air above the next speaker");
        assert!(
            rows[3].starts_with("─ deepseek-v4 · coder ─"),
            "the model's divider follows: {rows:?}"
        );
        assert_eq!(rows[4], "rename and getattr share the cause.");
        assert!(rows[7].starts_with('❯'), "the draft is the band's third row: {rows:?}");

        let status = rows.last().expect("a status line");
        assert!(status.starts_with("0 kaijutsu*"), "got {status:?}");
        assert!(status.contains("-- NORMAL --"), "the vi mode is the status line's figure: {status:?}");
        // Tokens over window, the cache share, the age in whole minutes —
        // no icons, the separator left of the mode (`docs/tui.md`, "Status
        // line").
        assert!(status.contains("│  -- NORMAL --  1.0/1.0k  91%  1m"), "got {status:?}");
        assert!(!status.contains('▮') && !status.contains('⟳') && !status.contains('⏱'), "got {status:?}");
        assert!(status.ends_with("○ offline"), "got {status:?}");
    }

    #[test]
    /// The legend takes the compose row while `Ctrl+A` is pending; the
    /// status line stays, because its seat digits are what the player is
    /// about to press.
    fn the_armed_prefix_legend_takes_the_compose_row_and_the_status_line_stays() {
        let (mut app, _) = fixture();
        let idle = band_text(&mut app, 96);
        let status = idle.last().expect("a status line").clone();
        assert!(idle.iter().any(|l| l.starts_with(crate::compose::PROMPT)), "idle draws the compose row");

        let armed = text_of(&band_lines(&mut app, 96, 0, true));
        assert_eq!(armed.last(), Some(&status), "the status line is untouched");
        assert!(armed.iter().any(|l| l.starts_with("Ctrl+A:")), "the legend is drawn: {armed:?}");
        assert!(!armed.iter().any(|l| l.starts_with(crate::compose::PROMPT)), "the compose row is hidden: {armed:?}");
    }

    /// Nothing you would paste is inside a box: no border glyph opens any
    /// transcript row, and the only ruled lines are role dividers.
    #[test]
    fn the_transcript_has_no_border_glyphs() {
        let (mut app, _) = fixture();
        for text in transcript_text(&mut app, 96) {
            for glyph in ['│', '╭', '╮', '╰', '╯', '┌', '└'] {
                assert!(!text.contains(glyph), "{text:?} carries {glyph}");
            }
        }
    }

    /// The strip takes a tool call's body out of the transcript and names it
    /// on one row instead; a running result's output still streams in the
    /// transcript; and the band is the same height with or without either
    /// (`docs/tui.md`, "The in-flight strip").
    #[test]
    fn an_unsettled_tool_call_is_a_strip_entry_and_the_band_does_not_grow() {
        let (mut app, id) = fixture();
        assert_eq!(band_lines(&mut app, 80, 0, false).len(), usize::from(BAND_ROWS));

        let call = BlockSnapshotBuilder::new(BlockId::new(id, PrincipalId::new(), 9), BlockKind::ToolCall)
            .role(Role::Model)
            .status(Status::Running)
            .tool_name("builtin.shell.shell")
            .tool_input(r#"{"command":"cargo test -p kaijutsu-kernel"}"#)
            .content(r#"{"command":"cargo test -p kaijutsu-kernel"}"#)
            .created_at(1_000)
            .build();
        let result = BlockSnapshotBuilder::new(BlockId::new(id, PrincipalId::new(), 10), BlockKind::ToolResult)
            .role(Role::Tool)
            .status(Status::Running)
            .tool_call_id(call.id)
            .content("test vfs::unlink_symlink ... ok")
            .created_at(2_000)
            .build();
        snapshot(&mut app, id, vec![call, result]);

        let band = text_of(&band_lines(&mut app, 80, 5_000, false));
        assert_eq!(band.len(), usize::from(BAND_ROWS), "a tool call never grows the band");
        assert!(
            band.iter().any(|l| l.contains("◐ shell cargo test -p kaijutsu-kernel · 4s")),
            "the strip names the running call: {band:?}"
        );
        let transcript = transcript_text(&mut app, 80);
        assert!(
            !transcript.iter().any(|l| l.contains(r#"{"command""#)),
            "the call's body left the transcript: {transcript:?}"
        );
        assert!(
            transcript.iter().any(|l| l.contains("unlink_symlink ... ok")),
            "the running result's output still streams: {transcript:?}"
        );
    }

    /// With nothing in flight the strip is still there — an empty row of
    /// its own ground directly above the blank row over `❯` — so the band's
    /// row count is the same in both states.
    #[test]
    fn the_strip_row_is_present_when_empty() {
        let (mut app, _) = fixture();
        let band = band_lines(&mut app, 40, 0, false);
        let prompt = band.iter().position(|l| l.spans.iter().any(|s| s.content.contains('❯'))).expect("prompt row");
        let blank: String = band[prompt - 1].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(blank.trim().is_empty());
        let strip = &band[prompt - 2];
        let text: String = strip.spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(text, " ".repeat(40), "an empty strip is a full-width row of ground");
        assert!(strip.spans.iter().all(|s| s.style.bg.is_some()), "the strip's ground is painted");
    }

    /// A tool call and its result render as one unit: the pair header
    /// (`─ caller · tool ─ arg ─… stamp`), then the result's body with no
    /// second divider and no gap; the model's next words get their own
    /// divider again (`docs/tui.md`, "Conversation").
    #[test]
    fn a_tool_pair_renders_under_one_header() {
        let (mut app, id) = fixture();
        let principal = PrincipalId::new();
        let call = BlockSnapshotBuilder::new(BlockId::new(id, principal, 9), BlockKind::ToolCall)
            .role(Role::Model)
            .status(Status::Done)
            .tool_name("shell")
            .tool_input(r#"{"command":"kj ledger list"}"#)
            .content(r#"{"command":"kj ledger list"}"#)
            .created_at(1_000)
            .build();
        let result = BlockSnapshotBuilder::new(BlockId::new(id, principal, 10), BlockKind::ToolResult)
            .role(Role::Tool)
            .status(Status::Done)
            .tool_call_id(call.id)
            .content("(no pending approvals)")
            .created_at(2_000)
            .build();
        let after = block(id, 11, BlockKind::Text, Role::Model, Status::Done, "Nothing pending.");
        snapshot(&mut app, id, vec![call, result, after]);

        let rows = transcript_text(&mut app, 96);
        let header = rows.iter().position(|r| r.starts_with("─ deepseek-v4 · shell ─ kj ledger list ─")).expect("the pair header");
        assert_eq!(rows[header + 1], "(no pending approvals)", "the result follows the header directly: {rows:?}");
        assert_eq!(rows.iter().filter(|r| r.contains("· shell")).count(), 1, "one header for the pair: {rows:?}");
        assert!(!rows.iter().any(|r| r.contains(r#"{"command""#)), "the call body folded into the header: {rows:?}");
        let next = rows.iter().position(|r| r == "Nothing pending.").expect("the model's next words");
        assert!(rows[next - 1].starts_with("─ deepseek-v4 · coder ─"), "the model's words get their divider back: {rows:?}");
    }

    /// A draft past one row grows the band, and the transcript gives up the
    /// rows — one screen, shared.
    #[test]
    fn a_multi_line_draft_takes_rows_from_the_transcript() {
        let (mut app, id) = fixture();
        let body = (0..20).map(|n| format!("line {n}")).collect::<Vec<_>>().join("\n");
        snapshot(
            &mut app,
            id,
            vec![block(id, 9, BlockKind::Text, Role::Model, Status::Running, &body)],
        );
        let one_line = band_lines(&mut app, 80, 0, false).len();
        assert_eq!(one_line, usize::from(BAND_ROWS));
        let mut terminal = Terminal::new(TestBackend::new(80, 12)).expect("test backend builds");
        app.screen_rows = 12;
        draw_screen(&mut terminal, &mut app, 0, false).expect("draw");
        let before = rows(&terminal);
        let newest = before
            .iter()
            .position(|r| r == "line 19")
            .unwrap_or_else(|| panic!("the newest line: {before:?}"));

        // `i` first: a fresh draft rests in normal mode.
        for code in [
            ratatui::crossterm::event::KeyCode::Char('i'),
            ratatui::crossterm::event::KeyCode::Char('a'),
            ratatui::crossterm::event::KeyCode::Enter,
            ratatui::crossterm::event::KeyCode::Char('b'),
        ] {
            app.compose.press(
                ratatui::crossterm::event::KeyEvent::new(
                    code,
                    ratatui::crossterm::event::KeyModifiers::NONE,
                ),
            );
        }
        assert_eq!(
            band_lines(&mut app, 80, 0, false).len(),
            one_line + 1,
            "the band grew one row for the draft"
        );
        draw_screen(&mut terminal, &mut app, 0, false).expect("draw");
        let after = rows(&terminal);
        assert!(after.iter().any(|l| l.starts_with("❯ a")), "got {after:?}");
        assert!(after.iter().any(|l| l.trim() == "b"), "the second draft row: {after:?}");
        assert_eq!(
            after.iter().position(|r| r == "line 19"),
            Some(newest - 1),
            "the transcript gave up a row: {after:?}"
        );

        // Past the cap the band stops growing and the draft scrolls instead.
        app.screen_rows = 6; // cap = 2 rows
        for _ in 0..3 {
            app.compose.press(ratatui::crossterm::event::KeyEvent::new(
                ratatui::crossterm::event::KeyCode::Enter,
                ratatui::crossterm::event::KeyModifiers::NONE,
            ));
        }
        assert_eq!(
            band_lines(&mut app, 80, 0, false).len(),
            one_line + 1,
            "capped at a third of the screen"
        );
    }

    /// `:` swaps the compose row for the `:` bar, and the prompt glyph
    /// swaps with it (`docs/tui.md`, "The `:` line").
    #[test]
    fn the_colon_bar_replaces_the_compose_line() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let (mut app, _) = fixture();
        let press = |app: &mut App, code: KeyCode| {
            app.compose.press(KeyEvent::new(code, KeyModifiers::NONE));
        };
        press(&mut app, KeyCode::Esc);
        press(&mut app, KeyCode::Char(':'));
        for c in "kj con".chars() {
            press(&mut app, KeyCode::Char(c));
        }

        let text = band_text(&mut app, 96);
        assert!(text.iter().any(|l| l.starts_with(":kj con")), "got {text:?}");
        assert!(!text.iter().any(|l| l.contains("❯")), "the draft prompt left with the draft: {text:?}");
    }

    #[test]
    fn a_waiting_block_is_not_settled() {
        let id = ContextId::new();
        let waiting = block(id, 1, BlockKind::ToolCall, Role::Model, Status::Waiting, "danger");
        assert!(!is_settled(&waiting), "an unanswered ask can still move");
        let done = block(id, 2, BlockKind::ToolCall, Role::Model, Status::Done, "ls");
        assert!(is_settled(&done));
    }

    /// An ask card, ready to put on `App::ask_card`.
    fn ask_card(id: ContextId, statement: &str) -> crate::asks::AskCardState {
        crate::asks::fixtures::card("01a04eb6", id, statement)
    }

    /// A ledger view of `n` pending rows.
    fn ledger_view(n: usize) -> crate::asks::LedgerViewState {
        crate::asks::LedgerViewState {
            rows: (0..n)
                .map(|i| {
                    crate::asks::LedgerRow::Pending(crate::asks::PendingRow {
                        request_id: format!("p{i}"),
                        age: Some("1s".to_string()),
                        context_label: "kaijutsu".to_string(),
                        performer: "coder".to_string(),
                        hook: "shell_write".to_string(),
                        asker: None,
                        reviewer: None,
                        offer: crate::asks::Offer { review: true, ..Default::default() },
                        statement: "git worktree remove --force ~/src/wt/kaish-arith".to_string(),
                    })
                })
                .collect(),
            filter: String::new(),
            selected: 0,
            filtering: false,
            detail: None,
        }
    }

    /// An open ask card is an overlay between the transcript and the band:
    /// the band's own rows — the draft and the status line — stay
    /// (`docs/tui.md`, "Asks").
    #[test]
    fn an_ask_card_is_an_overlay_over_the_transcript() {
        let (mut app, id) = fixture();
        app.ask_card = Some(ask_card(id, "git worktree remove --force"));
        let mut terminal = Terminal::new(TestBackend::new(96, 12)).expect("test backend builds");
        app.screen_rows = 12;
        draw_screen(&mut terminal, &mut app, 0, false).expect("draw");
        let text = rows(&terminal);
        let card = text
            .iter()
            .position(|l| l.contains("⚠ ask 01a04eb6"))
            .unwrap_or_else(|| panic!("the card is drawn: {text:?}"));
        assert!(text[card].contains("shell_write"), "got {text:?}");
        assert!(text.iter().any(|l| l.contains("git worktree remove --force")), "got {text:?}");
        let compose = text
            .iter()
            .position(|l| l.starts_with('❯'))
            .unwrap_or_else(|| panic!("the draft stays: {text:?}"));
        assert!(card < compose, "the card is above the band: {text:?}");
        assert!(text.last().unwrap().starts_with("0 kaijutsu*"), "status line still renders: {text:?}");
    }

    /// The overlay keeps its tail when the screen cannot hold it: the
    /// key-hints line — the only way to answer a pending ask — is the last
    /// row it has, never the one that drops (kaibo review, 2026-09-03).
    #[test]
    fn a_long_ask_statement_keeps_the_key_hints_line_last() {
        let (mut app, id) = fixture();
        app.ask_card = Some(ask_card(
            id,
            "one two three four five six seven eight nine ten eleven twelve thirteen",
        ));
        let t0 = std::time::Instant::now();
        app.tick_ask_arming(t0);
        app.tick_ask_arming(t0 + kaijutsu_client::ASK_ARM_DELAY);
        let overlay = text_of(&overlay_lines(&mut app, 16));
        assert!(
            overlay.last().is_some_and(|l| l.contains("Esc aside")),
            "key hints must be the overlay's last row, got {overlay:?}"
        );
    }

    /// A ledger with more rows than the screen loses the same key-hints
    /// line the same way — `a allow once ... Esc back` must stay visible.
    #[test]
    fn a_long_ledger_keeps_the_key_hints_line_last() {
        let (mut app, _id) = fixture();
        app.ledger_view = Some(ledger_view(10));
        let overlay = text_of(&overlay_lines(&mut app, 96));
        assert!(
            overlay.last().is_some_and(|l| l.starts_with("a allow once")),
            "key hints must be the overlay's last row, got {overlay:?}"
        );
    }

    /// A screen too short for the overlay crops it from the FRONT, so the
    /// key-hints line is what survives, directly above the band.
    #[test]
    fn a_screen_shorter_than_the_overlay_keeps_its_tail() {
        let (mut app, _id) = fixture();
        app.ledger_view = Some(ledger_view(10));
        let mut terminal = Terminal::new(TestBackend::new(96, 6)).expect("test backend builds");
        app.screen_rows = 6;
        draw_screen(&mut terminal, &mut app, 0, false).expect("draw");
        let text = rows(&terminal);
        assert!(
            text.last().unwrap().starts_with("0 kaijutsu*"),
            "status line still last: {text:?}"
        );
        assert!(
            text.iter().any(|l| l.starts_with("a allow once")),
            "the key hints survive the crop: {text:?}"
        );
    }

    /// The whole-transcript render the scrolled view searches and yanks
    /// holds every block and excludes the draft, which is compose's line,
    /// not the transcript.
    #[test]
    fn transcript_all_lines_covers_the_whole_context_but_not_the_draft() {
        let (mut app, id) = fixture();
        snapshot(
            &mut app,
            id,
            vec![
                block(id, 1, BlockKind::Text, Role::User, Status::Done, "and getattr?"),
                block(
                    id,
                    2,
                    BlockKind::Text,
                    Role::Model,
                    Status::Done,
                    "rename and getattr share the cause.",
                ),
                BlockSnapshotBuilder::new(BlockId::new(id, PrincipalId::new(), 3), BlockKind::Text)
                    .role(Role::User)
                    .status(Status::Draft)
                    .content("still typing")
                    .build(),
            ],
        );

        let text = text_of(&transcript_all_lines(&mut app, 80));
        assert!(text.iter().any(|l| l.contains("and getattr?")), "got {text:?}");
        assert!(text.iter().any(|l| l.contains("rename and getattr")), "got {text:?}");
        assert!(!text.iter().any(|l| l.contains("still typing")), "the draft is not the transcript: {text:?}");

        // Row for row, it is what a full-height window draws — the yank and
        // the search address the same rows the reader is looking at.
        assert_eq!(text, text_of(&transcript_window(&mut app, 80, u16::MAX)));
    }

    #[test]
    fn transcript_all_lines_is_empty_with_no_context_on_screen() {
        let mut app = App::new("amy");
        assert!(transcript_all_lines(&mut app, 80).is_empty());
    }

    /// The ledger view is an overlay too, both sections visible
    /// (`docs/tui.md`, "The ledger").
    #[test]
    fn the_ledger_view_draws_over_the_transcript() {
        let (mut app, _id) = fixture();
        app.ledger_view = Some(ledger_view(1));
        let overlay = text_of(&overlay_lines(&mut app, 96));
        assert!(overlay[0].starts_with("LEDGER"), "got {overlay:?}");
        assert!(overlay.iter().any(|l| l.contains("p0") && l.contains("shell_write")), "got {overlay:?}");
    }

    /// The key path and the frame divide the screen with one function, so an
    /// overlay's rows come out of the transcript's for both. Measured
    /// against the taller screen a page step overshoots, and the reader's
    /// offset slides the moment an overlay opens.
    #[test]
    fn an_open_overlay_shortens_the_transcript_for_the_key_path_too() {
        let (mut app, _id) = fixture();
        app.screen_rows = 24;
        let bare = transcript_height(&mut app, 80);
        assert_eq!(bare, 24 - usize::from(band_rows(&app, 80)));

        app.ledger_view = Some(ledger_view(2));
        let overlay = overlay_lines(&mut app, 80).len();
        assert!(overlay > 0, "the ledger is open");
        assert_eq!(
            transcript_height(&mut app, 80),
            bare - overlay,
            "the overlay's rows come out of the transcript's"
        );
    }

    // ────────────────────────────────────────────────────────────────────
    // Folds (docs/tui.md, "Folds")
    // ────────────────────────────────────────────────────────────────────

    fn reasoning(id: ContextId, seq: u64, status: Status, lines: usize) -> BlockSnapshot {
        let body = (0..lines).map(|n| format!("thought {n}")).collect::<Vec<_>>().join("\n");
        block(id, seq, BlockKind::Thinking, Role::Model, status, &body)
    }

    /// Streaming reasoning is an ordinary block at the tail: it renders
    /// whole, in document order under the model's divider, once — no pane
    /// holds a second copy, and no overlay takes rows for it.
    #[test]
    fn streaming_reasoning_renders_inline_once() {
        let (mut app, id) = fixture();
        app.mark_turn_running(id);
        let question = block(id, 1, BlockKind::Text, Role::User, Status::Done, "and getattr?");
        snapshot(&mut app, id, vec![question, reasoning(id, 2, Status::Running, 3)]);

        let rows = transcript_text(&mut app, 80);
        let first = rows.iter().position(|r| r == "thought 0").unwrap_or_else(|| panic!("inline: {rows:?}"));
        assert_eq!(rows[first + 1], "thought 1", "{rows:?}");
        assert_eq!(rows[first + 2], "thought 2", "{rows:?}");
        assert!(rows[first - 1].starts_with("─ deepseek-v4 · coder ─"), "under the model's divider: {rows:?}");
        assert_eq!(rows.iter().filter(|r| r.starts_with("thought")).count(), 3, "drawn once: {rows:?}");
        assert!(overlay_lines(&mut app, 80).is_empty(), "no overlay takes rows for it");
    }

    /// Settled reasoning that is the newest block stays open; the moment the
    /// answer follows it, it is one stub with the answer's first line
    /// directly under it.
    #[test]
    fn the_answer_lands_directly_under_folded_reasoning() {
        let (mut app, id) = fixture();
        let thinking = reasoning(id, 9, Status::Done, 3);
        snapshot(&mut app, id, vec![thinking.clone()]);
        let open = transcript_text(&mut app, 80);
        assert!(open.iter().any(|r| r == "thought 2"), "settled and last: still open: {open:?}");

        let answer = block(id, 10, BlockKind::Text, Role::Model, Status::Running, "The unlink bug");
        snapshot(&mut app, id, vec![thinking, answer]);
        let rows = transcript_text(&mut app, 80);
        let stub = rows
            .iter()
            .position(|r| r.starts_with("▸ thinking · 3 lines · thought 0"))
            .unwrap_or_else(|| panic!("a stub: {rows:?}"));
        assert_eq!(rows[stub + 1], "The unlink bug", "the answer is directly under the stub: {rows:?}");
        assert!(!rows.iter().any(|r| r == "thought 2"), "the body folded away: {rows:?}");
    }

    /// A long settled result folds once something follows it; its call's
    /// header stays and the stub is its one body line. A short result and
    /// a running one render whole.
    #[test]
    fn only_a_long_settled_result_folds_and_its_header_stays() {
        let (mut app, id) = fixture();
        let after = block(id, 20, BlockKind::Text, Role::Model, Status::Done, "Built.");
        let mut blocks = long_result(id, 40);
        blocks.push(after.clone());
        snapshot(&mut app, id, blocks);
        let rows = transcript_text(&mut app, 80);
        let header = rows
            .iter()
            .position(|r| r.starts_with("─ deepseek-v4 · builtin.shell.shell ─ cargo build"))
            .unwrap_or_else(|| panic!("the call's header: {rows:?}"));
        assert_eq!(rows[header + 1], "▸ 40 lines · line 39", "{rows:?}");
        assert!(!rows.iter().any(|r| r == "line 20"), "{rows:?}");

        let mut blocks = long_result(id, 3);
        blocks.push(after.clone());
        snapshot(&mut app, id, blocks);
        let rows = transcript_text(&mut app, 80);
        assert!(rows.iter().any(|r| r == "line 2"), "a short result renders whole: {rows:?}");

        let mut blocks = long_result(id, 40);
        blocks[1].status = Status::Running;
        blocks.push(after);
        snapshot(&mut app, id, blocks);
        let rows = transcript_text(&mut app, 80);
        assert!(rows.iter().any(|r| r == "line 20"), "a running result streams whole: {rows:?}");
    }

    /// `Ctrl+O` on the live tail opens the newest fold, and the delivery
    /// after it does not fold it again: the fold is decided once.
    #[test]
    fn the_toggle_on_the_tail_opens_the_newest_fold_for_good() {
        let (mut app, id) = fixture();
        let thinking = reasoning(id, 9, Status::Done, 3);
        let answer = block(id, 10, BlockKind::Text, Role::Model, Status::Done, "The unlink bug");
        snapshot(&mut app, id, vec![thinking.clone(), answer.clone()]);
        assert_eq!(fold_target(&mut app, 80), Some(thinking.id), "the newest foldable block");

        toggle_fold(&mut app, 80).expect("a fold to toggle");
        let rows = transcript_text(&mut app, 80);
        assert!(rows.iter().any(|r| r == "thought 2"), "opened: {rows:?}");

        let next = block(id, 11, BlockKind::Text, Role::User, Status::Done, "and then?");
        snapshot(&mut app, id, vec![thinking, answer, next]);
        let rows = transcript_text(&mut app, 80);
        assert!(rows.iter().any(|r| r == "thought 2"), "the next delivery left it open: {rows:?}");
    }

    /// On the tail the toggle passes over a short open result — nothing to
    /// open there — and reaches the reasoning stub above it.
    #[test]
    fn the_toggle_on_the_tail_passes_over_a_short_open_result() {
        let (mut app, id) = fixture();
        let thinking = reasoning(id, 8, Status::Done, 3);
        let mut blocks = vec![thinking.clone()];
        blocks.extend(long_result(id, 3));
        let result = blocks.last().expect("the result").clone();
        snapshot(&mut app, id, blocks);
        assert_eq!(fold_target(&mut app, 80), Some(thinking.id), "the stub, not the short result");

        toggle_fold(&mut app, 80).expect("a fold to toggle");
        let view = app.current_view().expect("view");
        assert!(!view.is_collapsed(&thinking), "the stub opened");
        assert!(!view.is_collapsed(&result), "the short result was left alone");
    }

    /// The reader's block is `None` on the tail and the block holding the
    /// reader's line while scrolled.
    #[test]
    fn the_readers_block_is_none_on_the_tail_and_the_line_holder_when_scrolled() {
        let (mut app, id) = fixture();
        let thinking = reasoning(id, 9, Status::Done, 30);
        let answer = block(id, 10, BlockKind::Text, Role::Model, Status::Done, &(0..30).map(|n| format!("answer {n}")).collect::<Vec<_>>().join("\n"));
        snapshot(&mut app, id, vec![thinking.clone(), answer.clone()]);
        assert_eq!(reader_block(&mut app, 80), None, "the tail has no reader's block");

        let rows = text_of(&transcript_all_lines(&mut app, 80));
        let at = rows.iter().position(|r| r == "answer 3").expect("answer row");
        let mut scrolled = crate::copy::Scrolled::entering(10);
        scrolled.jump_to(at);
        assert!(app.set_scrolled(scrolled));
        assert_eq!(reader_block(&mut app, 80), Some(answer.id), "no frame drew first; the call settles");

        let stub = rows.iter().position(|r| r.starts_with("▸ thinking")).expect("stub row");
        app.scrolled_mut().expect("scrolled").jump_to(stub);
        assert_eq!(reader_block(&mut app, 80), Some(thinking.id));
    }

    /// With nothing to fold, the toggle says so rather than doing nothing
    /// silently.
    #[test]
    fn the_toggle_with_nothing_foldable_says_so() {
        let (mut app, _id) = fixture();
        assert_eq!(fold_target(&mut app, 80), None);
        assert!(toggle_fold(&mut app, 80).is_err());
    }

    /// Scrolled, the toggle acts on the block under the reader, the view
    /// stays scrolled, and the reader is still on that block — on its stub
    /// once it folds, on its first line once it opens.
    #[test]
    fn the_toggle_while_scrolled_acts_under_the_reader_and_keeps_the_place() {
        let (mut app, id) = fixture();
        let thinking = reasoning(id, 9, Status::Done, 30);
        let answer = block(id, 10, BlockKind::Text, Role::Model, Status::Done, &(0..30).map(|n| format!("answer {n}")).collect::<Vec<_>>().join("\n"));
        snapshot(&mut app, id, vec![thinking.clone(), answer]);
        let stub_row = text_of(&transcript_all_lines(&mut app, 80))
            .iter()
            .position(|r| r.starts_with("▸ thinking"))
            .expect("folded");
        let mut scrolled = crate::copy::Scrolled::entering(10);
        scrolled.jump_to(stub_row);
        assert!(app.set_scrolled(scrolled));
        let _ = transcript_window(&mut app, 80, 10);
        assert_eq!(app.scrolled().and_then(|s| s.reader_block()), Some(thinking.id));

        toggle_fold(&mut app, 80).expect("a fold to toggle");
        let window = transcript_window(&mut app, 80, 10);
        assert!(app.scrolled().is_some(), "the toggle did not snap to the tail");
        assert_eq!(reader_line(&window), "thought 0", "the reader is on the block's first line");
        assert_eq!(app.scrolled().and_then(|s| s.reader_block()), Some(thinking.id));

        toggle_fold(&mut app, 80).expect("a fold to toggle");
        let window = transcript_window(&mut app, 80, 10);
        assert!(reader_line(&window).starts_with("▸ thinking"), "back on the stub");
    }

    /// The row index the key path settles against agrees with the rows a
    /// frame draws once a block folds, so search and yank address what is
    /// on screen.
    #[test]
    fn the_row_index_agrees_with_the_drawn_rows_after_a_fold() {
        let (mut app, id) = fixture();
        let mut blocks = vec![reasoning(id, 8, Status::Done, 12)];
        blocks.extend(long_result(id, 40));
        blocks.push(block(id, 20, BlockKind::Text, Role::Model, Status::Done, "Built."));
        snapshot(&mut app, id, blocks);
        let drawn = transcript_all_lines(&mut app, 80);
        assert!(text_of(&drawn).iter().any(|r| r.starts_with("▸ 40 lines")), "folded: {:?}", text_of(&drawn));
        assert_eq!(row_index(&mut app, 80).total(), drawn.len());
        assert_eq!(text_of(&drawn), text_of(&transcript_window(&mut app, 80, u16::MAX)));
    }

    /// A folded block's text was not shown, so an edge that ends on one
    /// carries the block and no count (`docs/prompts.md`, "The submit
    /// verb").
    #[test]
    fn a_folded_tail_block_sends_no_count() {
        let (mut app, id) = fixture();
        let error = block(id, 9, BlockKind::Error, Role::System, Status::Error, "the kernel restarted");
        snapshot(&mut app, id, vec![error.clone()]);
        let _ = transcript_window(&mut app, 80, u16::MAX);
        let edge = app.views[&id].edge().expect("the error was drawn");
        assert_eq!(edge.block, error.id);
        assert_eq!(edge.shown, None, "a stub is not the text");
    }

    /// An unfocused terminal shows nobody the spinner, so the loop stops
    /// redrawing for it (`docs/tui.md`, "What owning the screen lets us
    /// use": focus reporting pauses the pulse and the spinner).
    #[test]
    fn the_strip_stops_animating_while_the_terminal_is_unfocused() {
        let (mut app, id) = fixture();
        snapshot(
            &mut app,
            id,
            vec![block(id, 5, BlockKind::ToolCall, Role::Model, Status::Running, "kj ledger list")],
        );
        assert!(strip_animating(&app), "a running tool call animates while focused");
        app.focused = false;
        assert!(!strip_animating(&app), "nothing animates for a terminal nobody is looking at");
    }
}
