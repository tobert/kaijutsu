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
use kaijutsu_types::{BlockKind, BlockSnapshot, Status};
use ratatui::backend::Backend;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
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

/// A grown region's ceiling: a third of the screen. The compose region
/// grows toward it a row at a time; the thinking band takes it whole,
/// once, when a turn's reasoning starts (`docs/tui.md`, "The thinking
/// pane" and "Compose").
pub fn third_of_screen(app: &App) -> u16 {
    (app.screen_rows / 3).max(1)
}

/// Rows the thinking pane takes above the band while it is open.
pub fn thinking_band_lines(app: &App) -> u16 {
    third_of_screen(app)
}

/// Whether the thinking pane is open: the current context's turn is running
/// and has shown a `Thinking` block (`App::thinking_pane_latched`). The
/// turn-liveness half is what keeps a block left `Running` by a lost turn
/// from holding the pane open forever (`App::forget_turn_liveness`).
pub fn thinking_pane_open(app: &App) -> bool {
    app.current.is_some_and(|context_id| app.thinking_pane_latched(context_id))
}

/// Whether the in-flight strip has a running entry to animate — the event
/// loop redraws on `inflight::PHASE_MILLIS` only while it does.
pub fn strip_animating(app: &App) -> bool {
    let Some(view) = app.current_view() else {
        return false;
    };
    crate::inflight::animating(&crate::inflight::entries(view.mirror.blocks().iter(), 0))
}

/// The turn's latest `Thinking` block, in document order, whatever its
/// status — the pane shows the newest reasoning until the turn ends.
fn latest_thinking(app: &App) -> Option<BlockSnapshot> {
    let context_id = app.current?;
    let view = app.views.get(&context_id)?;
    view.mirror
        .blocks()
        .iter()
        .rev()
        .find(|b| b.kind == BlockKind::Thinking)
        .cloned()
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

/// The most body rows a tool result takes in the transcript before it is cut
/// to a head plus a footer: one screenful, the transcript rows a reader can
/// see at once (`app.screen_rows` less the band at `width`, which grows with
/// a wrapped draft).
///
/// A coder turn is mostly tool output, and a `cargo build` or a wide `grep`
/// runs to thousands of lines. An uncut result pushes the turn that produced
/// it off the screen, and the footer names where the rest is read.
///
/// The floor keeps the cut sane on a tiny terminal: below it the footer
/// would cost more than it saves.
const TOOL_RESULT_FLOOR: u16 = 6;

pub fn tool_result_budget(app: &App, width: u16) -> usize {
    usize::from(
        app.screen_rows
            .saturating_sub(band_rows(app, width))
            .max(TOOL_RESULT_FLOOR),
    )
}

/// Cut an over-long tool result to its head and say what was dropped.
///
/// Only `ToolResult`, and only in the transcript: `copy_buffer_lines`
/// renders the same block through `render_block` untouched, which is what
/// makes the footer's promise true. Capping inside `render_block` would cut
/// copy mode too and leave the rest reachable nowhere.
///
/// The divider is kept — it names the caller and the command, which is what
/// makes a cut result identifiable at all — and the footer replaces the last
/// row of the budget, so the whole render is exactly one screenful.
fn cap_tool_result(
    block: &BlockSnapshot,
    lines: Vec<Line<'static>>,
    has_divider: bool,
    budget: usize,
    palette: &crate::present::Palette,
) -> Vec<Line<'static>> {
    if block.kind != BlockKind::ToolResult || lines.len() <= budget {
        return lines;
    }
    let head_rows = usize::from(has_divider);
    // `budget` covers the divider and the footer as well as the body, and
    // `budget >= TOOL_RESULT_FLOOR` keeps this from going negative.
    let keep = budget.saturating_sub(1);
    let dropped = lines.len() - keep;
    let mut out: Vec<Line<'static>> = lines.into_iter().take(keep).collect();
    let plural = if dropped == 1 { "" } else { "s" };
    out.push(Line::from(Span::styled(
        format!("… {dropped} more line{plural} — Ctrl+A [ for copy mode"),
        palette.divider(),
    )));
    debug_assert!(out.len() >= head_rows, "the divider is inside the budget");
    out
}

/// The rows a block takes in the transcript once [`cap_tool_result`] has
/// had its say — counted without rendering, so the window pass can skip
/// blocks it will not draw.
fn capped_rows(block: &BlockSnapshot, rows: usize, budget: usize) -> usize {
    if block.kind == BlockKind::ToolResult && rows > budget {
        budget
    } else {
        rows
    }
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
/// its speaker, stamp, divider decision and collapse resolved.
///
/// The draft is not one of them — it is the compose line. Neither is a block
/// the in-flight strip has taken (`inflight::takes_from_stream`) or the one
/// the thinking pane is holding: a block is on screen once. A hidden block
/// still moves the divider decision along, so a call in the strip and its
/// result still read as one unit when the pair lands.
fn transcript_plan(app: &App) -> Vec<(BlockSnapshot, BlockPlan)> {
    let Some(context_id) = app.current else {
        return Vec::new();
    };
    let Some(view) = app.views.get(&context_id) else {
        return Vec::new();
    };
    let pane_block = thinking_pane_open(app)
        .then(|| latest_thinking(app))
        .flatten()
        .map(|b| b.id);
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
        if crate::inflight::takes_from_stream(block) || Some(block.id) == pane_block {
            continue;
        }
        out.push((
            block.clone(),
            BlockPlan {
                speaker,
                stamp: wallclock(block.created_at),
                show_divider,
                gap,
                // A `Thinking` block is one `▸` stub in the transcript
                // whatever its collapse state: the reasoning was read as it
                // streamed in the pane, and copy mode and `kj block read`
                // keep the whole text (`docs/tui.md`, "The thinking pane").
                collapsed: view.is_collapsed(block) || block.kind == BlockKind::Thinking,
            },
        ));
    }
    out
}

/// The context type the current context's blocks render under.
fn context_type_of(app: &App) -> String {
    app.current
        .and_then(|id| app.info(id))
        .map(|c| c.context_type.clone())
        .unwrap_or_else(|| "default".to_string())
}

/// The transcript's visible rows: `height` rows of the current context's
/// blocks, wrapped at `width`, ending at the newest row while the view
/// follows the tail (`App::transcript`).
///
/// Two passes over the plan. The first counts each block's rows through the
/// wrap cache; the second renders only the blocks the window touches, so a
/// frame costs a screenful of work rather than a context's.
pub fn transcript_window(app: &mut App, width: u16, height: u16) -> Vec<Line<'static>> {
    let plan = transcript_plan(app);
    let height = usize::from(height);
    if height == 0 || plan.is_empty() {
        return Vec::new();
    }
    let budget = tool_result_budget(app, width);
    let palette = app.palette;
    let context_type = context_type_of(app);

    let mut rows: Vec<usize> = Vec::with_capacity(plan.len());
    for (block, item) in &plan {
        let view = block_view(item, &context_type, block);
        let lineage = app.lineage_for(block);
        let view = crate::present::BlockView { lineage, ..view };
        let counted = capped_rows(block, app.wrap.lines(block, &view, width, &palette).len(), budget);
        rows.push(usize::from(item.gap) + counted);
    }
    let total: usize = rows.iter().sum();
    let last_top = total.saturating_sub(height);
    let start = if app.transcript.follow {
        last_top
    } else {
        app.transcript.top.min(last_top)
    };

    let mut lines: Vec<Line<'static>> = Vec::with_capacity(height.min(total));
    let mut row = 0usize;
    let mut edge = None;
    for ((block, item), block_rows) in plan.iter().zip(rows) {
        if row + block_rows <= start {
            row += block_rows;
            continue;
        }
        let view = block_view(item, &context_type, block);
        let lineage = app.lineage_for(block);
        let view = crate::present::BlockView { lineage, ..view };
        let rendered = app.wrap.lines(block, &view, width, &palette).to_vec();
        let cut = capped_rows(block, rendered.len(), budget) != rendered.len();
        let mut block_lines = cap_tool_result(block, rendered, item.show_divider, budget, &palette);
        if item.gap {
            block_lines.insert(0, Line::default());
        }
        let skip = start.saturating_sub(row);
        let mut drawn: Vec<Line<'static>> = block_lines.into_iter().skip(skip).collect();
        let room = height - lines.len();
        // The head may be above the window without hiding anything the
        // player is reading; a tail the window could not fit is a different
        // matter, and so is one the cap took.
        let cropped = drawn.len() > room;
        drawn.truncate(room);
        if !drawn.is_empty() {
            edge = Some((
                block.id,
                (!cut && !cropped).then(|| block.content.chars().count() as u64),
            ));
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
    lines
}

/// The view a planned block renders through, lineage left empty for the
/// caller to fill — it needs `app` immutably, and the wrap cache is a
/// mutable borrow that cannot overlap it.
fn block_view<'a>(
    item: &'a BlockPlan,
    context_type: &'a str,
    block: &'a BlockSnapshot,
) -> crate::present::BlockView<'a> {
    let (tool, arg) = crate::app::tool_header(block);
    crate::present::BlockView {
        speaker: &item.speaker,
        context_type,
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
/// and how many of its characters were actually drawn — `None` when a cap or
/// a crop hid its tail, because the kernel never guesses one
/// (`ContextView::edge`, `docs/prompts.md`, "The submit verb").
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
        Some(view) => crate::inflight::entries(view.mirror.blocks().iter(), now_millis),
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
        (!overlay_holds_keys(app)).then_some((first.saturating_add(cursor_row), cursor_col))
    };
    lines.push(status_line(&app.status_model(now_millis), width, &palette));
    BandFrame { lines, cursor }
}

/// Whether an overlay owns the keyboard. The picker, an ask card and the
/// ledger take every key while they are up, so the draft shows no cursor.
fn overlay_holds_keys(app: &App) -> bool {
    app.picker.is_some() || app.ask_card.is_some() || app.ledger_view.is_some()
}

/// The overlay's rows: the picker, an ask card, the ledger view or the
/// thinking pane, whichever is open, and empty when none is. One overlay at
/// a time, drawn between the transcript and the band (`docs/tui.md`, "The
/// owned screen").
pub fn overlay_lines(app: &mut App, width: u16) -> Vec<Line<'static>> {
    if let Some(picker) = &app.picker {
        return crate::picker::render(picker, width, &app.palette);
    }
    if let Some(lines) = crate::asks::active_view_lines(app, width) {
        return lines;
    }
    thinking_pane(app, width)
}

/// The thinking pane: the tail of the turn's latest reasoning, dim and
/// italic, a third of the screen at most, so the answer streaming in never
/// scrolls the reasoning out of it (`docs/tui.md`, "The thinking pane").
fn thinking_pane(app: &mut App, width: u16) -> Vec<Line<'static>> {
    if !thinking_pane_open(app) {
        return Vec::new();
    }
    let Some(block) = latest_thinking(app) else {
        return Vec::new();
    };
    let palette = app.palette;
    let context_type = context_type_of(app);
    let stamp = wallclock(block.created_at);
    let view = crate::present::BlockView {
        speaker: "",
        context_type: &context_type,
        stamp: &stamp,
        show_divider: false,
        tool: None,
        arg: None,
        lineage: Vec::new(),
        collapsed: false,
        local_ctx: Some(block.id.context_id),
    };
    let rows = usize::from(thinking_band_lines(app));
    let rendered = app.wrap.lines(&block, &view, width, &palette);
    let keep = rows.min(rendered.len());
    rendered[rendered.len() - keep..].to_vec()
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
    let band = band_frame(app, width, now_millis, armed);
    let band_rows = u16::try_from(band.lines.len()).unwrap_or(u16::MAX).min(height);
    let overlay = overlay_lines(app, width);
    let overlay_rows = u16::try_from(overlay.len())
        .unwrap_or(u16::MAX)
        .min(height.saturating_sub(band_rows));
    let transcript_rows = height.saturating_sub(band_rows).saturating_sub(overlay_rows);
    let transcript = transcript_window(app, width, transcript_rows);

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

/// One full-screen surface — the editor, the diff viewer, copy mode — drawn
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

/// Build copy mode's buffer for the context on screen: every block in
/// document order, rendered exactly as the transcript would
/// ([`crate::present::render_block`]) but whole — no cut tool result, no
/// thinking stub (`docs/tui.md`, "Copy mode"). The draft is excluded — it is
/// the compose line, not the transcript.
///
/// Frozen at the moment `Ctrl+A [` is pressed, the same "freeze on open"
/// contract the diff screen keeps (`diff.rs`): a still-streaming block's
/// later growth does not move the reader's place inside a buffer already
/// open. `None` with no context on screen.
pub fn copy_buffer_lines(app: &App, width: u16) -> Option<(String, Vec<Line<'static>>)> {
    let context_id = app.current?;
    let view = app.views.get(&context_id)?;
    let info = app.info(context_id);
    let context_type = info
        .map(|c| c.context_type.clone())
        .unwrap_or_else(|| "default".to_string());
    let label = info
        .map(|c| c.label.clone())
        .filter(|l| !l.is_empty())
        .unwrap_or_else(|| context_id.short());

    let mut lines = Vec::new();
    let mut last_speaker: Option<String> = None;
    for block in view.mirror.blocks() {
        if block.status == Status::Draft {
            continue;
        }
        let speaker = app.speaker_for(block, info);
        let show_divider = last_speaker.as_deref() != Some(speaker.as_str());
        let stamp = wallclock(block.created_at);
        let (tool, arg) = crate::app::tool_header(block);
        let block_view = crate::present::BlockView {
            speaker: &speaker,
            context_type: &context_type,
            stamp: &stamp,
            show_divider,
            tool,
            arg,
            lineage: app.lineage_for(block),
            // Copy mode is where reasoning stays findable, so a `Thinking`
            // block renders whole here even when a sibling collapsed it
            // (`docs/tui.md`, "The thinking pane").
            collapsed: view.is_collapsed(block) && block.kind != BlockKind::Thinking,
            local_ctx: Some(context_id),
        };
        if speaker_gap(show_divider, last_speaker.as_deref()) {
            lines.push(Line::default());
        }
        lines.extend(crate::present::render_block(block, &block_view, width, &app.palette));
        last_speaker = Some(speaker);
    }
    Some((label, lines))
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
            context_type: "coder".to_string(),
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
            cast_label: None,
            origin_host: None,
            cwd: None,
            last_call_at: Some(0),
            cache_read_tokens: Some(910),
            cache_write_tokens: None,
            cache_ttl_secs: Some(300),
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

    /// Replace the current context's blocks.
    fn snapshot(app: &mut App, id: ContextId, blocks: Vec<BlockSnapshot>) {
        let mut mirror = ContextMirror::new(id);
        mirror.apply_snapshot(blocks, 1).expect("snapshot applies");
        app.views.insert(id, ContextView::new(mirror));
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

    /// A context this client is not watching renders nothing at all — no
    /// transcript, no stream. It is the state a switch that skipped
    /// `watch_context` reached, and the state a feed that ended leaves
    /// behind (`run.rs`'s `apply_feed`, `FeedEvent::Terminated`).
    #[test]
    fn an_unwatched_context_renders_no_transcript() {
        let (mut app, _) = fixture();
        let unwatched = ContextId::new();
        app.switch_to(unwatched);
        assert!(transcript_text(&mut app, 80).is_empty());
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

    /// Following shows the newest rows; a view that is not following shows
    /// the rows from where it stopped (`docs/tui.md`, "Scrolling is copy
    /// mode"; the keys are the next slice).
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

        app.transcript.follow = false;
        app.transcript.top = 1;
        let scrolled = text_of(&transcript_window(&mut app, 80, 5));
        assert_eq!(scrolled.first().map(String::as_str), Some("line 0"), "got {scrolled:?}");
        assert_eq!(scrolled.len(), 5);
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

    /// A cut tail is a count the client cannot make: the cap hid the end of
    /// the block, so the edge names the block and no count rather than
    /// claiming the player saw it whole.
    #[test]
    fn a_capped_result_at_the_tail_sends_no_count() {
        let (mut app, id) = fixture();
        app.screen_rows = 24;
        snapshot(&mut app, id, long_result(id, 300));
        let _ = transcript_window(&mut app, 80, u16::MAX);
        let edge = app.views[&id].edge().expect("the result was drawn");
        let result = app.views[&id].mirror.blocks().last().expect("the result").clone();
        assert_eq!(edge.block, result.id);
        assert_eq!(edge.shown, None, "the cap hid the tail");
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
        app.transcript.follow = false;
        app.transcript.top = 0;
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

    /// Build a settled tool call and its result, whose body is `rows` lines.
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

    /// A `cargo build`'s worth of output is cut to one screenful with a
    /// footer that counts what was dropped, so the turn that produced it is
    /// still on the screen with it.
    #[test]
    fn a_long_tool_result_is_cut_to_a_head_and_a_footer() {
        let (mut app, id) = fixture();
        app.screen_rows = 30;
        snapshot(&mut app, id, long_result(id, 500));

        let rows = transcript_text(&mut app, 80);
        let budget = tool_result_budget(&app, 80);
        assert_eq!(budget, 26, "30 rows less the band");

        let footer = rows.last().expect("a footer");
        assert!(
            footer.starts_with("… ") && footer.contains("more lines"),
            "the footer counts what was dropped: {footer:?}"
        );
        assert!(
            footer.contains("Ctrl+A ["),
            "the footer names where the rest is: {footer:?}"
        );
        // The head is real output, and the divider naming the command
        // survives the cut — a result you cannot attribute is worse than a
        // long one.
        assert!(
            rows.iter().any(|r| r.contains("cargo build")),
            "the call's divider survives: {rows:?}"
        );
        assert!(rows.iter().any(|r| r.contains("line 0")), "{rows:?}");
        assert!(
            !rows.iter().any(|r| r.contains("line 499")),
            "the tail is cut: {rows:?}"
        );
    }

    /// The whole render is one screenful — the point of the cut. A render
    /// that merely got shorter would still push the turn off the screen.
    #[test]
    fn a_cut_result_takes_exactly_one_screenful() {
        for screen_rows in [24u16, 30, 50, 120] {
            let (mut app, id) = fixture();
            app.screen_rows = screen_rows;
            snapshot(&mut app, id, long_result(id, 4_000));

            let rows = transcript_text(&mut app, 80);
            let budget = tool_result_budget(&app, 80);
            // The call's own header rides above the result's budget.
            assert!(
                rows.len() <= budget + 4,
                "screen_rows={screen_rows}: {} rows for a budget of {budget}",
                rows.len()
            );
        }
    }

    /// A short result is untouched — the cut must not tax ordinary output.
    #[test]
    fn a_short_tool_result_keeps_every_line_and_gains_no_footer() {
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
        assert!(
            !rows.iter().any(|r| r.contains("more lines")),
            "no footer on a short result: {rows:?}"
        );
    }

    /// The footer's promise has to be true: copy mode renders the same
    /// block through `render_block` with no cut, so the dropped tail is
    /// reachable exactly where the footer says it is.
    #[test]
    fn copy_mode_still_holds_the_lines_the_footer_promised() {
        let (mut app, id) = fixture();
        app.screen_rows = 24;
        snapshot(&mut app, id, long_result(id, 300));

        let shown = transcript_text(&mut app, 80);
        assert!(
            !shown.iter().any(|r| r.contains("line 299")),
            "the transcript is cut: {shown:?}"
        );

        let (_, copy) = copy_buffer_lines(&app, 80).expect("copy mode builds");
        let copy_rows = text_of(&copy);
        assert!(
            copy_rows.iter().any(|r| r.contains("line 299")),
            "copy mode keeps the tail the footer pointed at"
        );
        assert!(
            !copy_rows.iter().any(|r| r.contains("more lines")),
            "copy mode carries no footer: it was never cut"
        );
    }

    /// The budget is one screenful less the band as it stands: a draft that
    /// wrapped to a second row takes that row from the screenful too.
    #[test]
    fn the_budget_follows_the_bands_real_height() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let (mut app, _) = fixture();
        app.screen_rows = 30;
        let one_row = tool_result_budget(&app, 80);
        assert_eq!(one_row, 26, "30 rows less a four-row band");
        for code in [
            KeyCode::Char('i'),
            KeyCode::Char('a'),
            KeyCode::Enter,
            KeyCode::Char('b'),
        ] {
            app.compose.press(KeyEvent::new(code, KeyModifiers::NONE));
        }
        assert_eq!(
            tool_result_budget(&app, 80),
            one_row - 1,
            "a second draft row takes a row from the screenful"
        );
    }

    /// A tiny terminal still gets a usable head rather than a footer alone.
    #[test]
    fn the_budget_never_falls_below_the_floor() {
        let (mut app, _) = fixture();
        for screen_rows in [0u16, 1, 8, 9, 12] {
            app.screen_rows = screen_rows;
            assert!(
                tool_result_budget(&app, 80) >= usize::from(TOOL_RESULT_FLOOR),
                "screen_rows={screen_rows} fell below the floor"
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
        crate::asks::AskCardState {
            request_id: "01a04eb6".to_string(),
            context_id: id,
            detail: kaijutsu_client::AskDetail {
                request_id: "01a04eb6".to_string(),
                context_id: Some(id),
                principal_id: None,
                principal_name: None,
                actor_id: None,
                actor_name: None,
                reviewer_id: None,
                reviewer_name: None,
                status: "pending".to_string(),
                origin: "shell_gate".to_string(),
                tool: Some("shell_write".to_string()),
                hook_id: None,
                instance: None,
                description: statement.to_string(),
                authorized_label: None,
                statements: vec![statement.to_string()],
                exec_source: None,
                cwd: None,
                env: Vec::new(),
                created_at: None,
                decided_at: None,
                decided_by: None,
                decided_by_name: None,
                decided_option: None,
                remember_scope: None,
                redeemed_at: None,
            },
        }
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
                        context_type: "coder".to_string(),
                        hook: "shell_write".to_string(),
                        asker: None,
                        reviewer: None,
                        reviewable: true,
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

    /// Copy mode's buffer holds the whole context and excludes the draft,
    /// which is compose's line, not the transcript.
    #[test]
    fn copy_buffer_lines_covers_the_whole_context_but_not_the_draft() {
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

        let (label, lines) = copy_buffer_lines(&app, 80).expect("a context is on screen");
        assert_eq!(label, "kaijutsu");
        let text = text_of(&lines);
        assert!(text.iter().any(|l| l.contains("and getattr?")), "got {text:?}");
        assert!(text.iter().any(|l| l.contains("rename and getattr")), "got {text:?}");
        assert!(!text.iter().any(|l| l.contains("still typing")), "the draft is not the transcript: {text:?}");
    }

    #[test]
    fn copy_buffer_lines_is_none_with_no_context_on_screen() {
        let app = App::new("amy");
        assert!(copy_buffer_lines(&app, 80).is_none());
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

    // ────────────────────────────────────────────────────────────────────
    // The thinking pane (docs/tui.md, "The thinking pane")
    // ────────────────────────────────────────────────────────────────────

    /// A completed `Thinking` block is one `▸` stub in the transcript,
    /// naming its size and first line, whatever its collapse state.
    #[test]
    fn a_completed_thinking_block_renders_as_a_stub() {
        let (mut app, id) = fixture();
        snapshot(
            &mut app,
            id,
            vec![block(
                id,
                9,
                BlockKind::Thinking,
                Role::Model,
                Status::Done,
                "first the unlink path\nthen rename\nthen getattr",
            )],
        );
        let rows = transcript_text(&mut app, 80);
        let stub = rows.last().expect("a stub row");
        assert!(stub.starts_with("▸ thinking · 3 lines · first the unlink path"), "got {stub:?}");
        assert!(!rows.iter().any(|r| r.contains("then rename")), "the body stays out: {rows:?}");
    }

    /// Copy mode is where the whole reasoning stays findable: the copy
    /// buffer renders the same block expanded.
    #[test]
    fn the_copy_buffer_keeps_thinking_whole() {
        let (mut app, id) = fixture();
        let mut mirror = ContextMirror::new(id);
        mirror
            .apply_snapshot(
                vec![block(
                    id,
                    9,
                    BlockKind::Thinking,
                    Role::Model,
                    Status::Done,
                    "first the unlink path\nthen rename",
                )],
                1,
            )
            .expect("snapshot applies");
        let thinking_id = mirror.blocks()[0].id;
        let mut view = ContextView::new(mirror);
        // Even a collapse a sibling sent for it does not hide it here.
        view.collapsed.insert(thinking_id, true);
        app.views.insert(id, view);
        let (_, lines) = copy_buffer_lines(&app, 80).expect("a buffer");
        let rows = text_of(&lines);
        assert!(rows.iter().any(|r| r == "then rename"), "got {rows:?}");
        assert!(!rows.iter().any(|r| r.starts_with("▸ thinking")), "no stub here: {rows:?}");
    }

    /// A turn whose reasoning is still streaming. Returns the thinking
    /// block's id — what a delivery names as touched.
    fn streaming_thinking(app: &mut App, id: ContextId, lines: usize) -> BlockId {
        let body = (0..lines).map(|n| format!("thought {n}")).collect::<Vec<_>>().join("\n");
        let thinking = block(id, 9, BlockKind::Thinking, Role::Model, Status::Running, &body);
        let block_id = thinking.id;
        snapshot(app, id, vec![thinking]);
        block_id
    }

    /// A turn whose reasoning already completed and whose answer is now
    /// streaming: a `Done` Thinking block of `lines` rows, then a `Running`
    /// Text block.
    fn thinking_then_answer(app: &mut App, id: ContextId, lines: usize) -> BlockId {
        let body = (0..lines).map(|n| format!("thought {n}")).collect::<Vec<_>>().join("\n");
        let thinking = block(id, 9, BlockKind::Thinking, Role::Model, Status::Done, &body);
        let block_id = thinking.id;
        snapshot(
            app,
            id,
            vec![
                thinking,
                block(id, 10, BlockKind::Text, Role::Model, Status::Running, "The unlink bug"),
            ],
        );
        block_id
    }

    /// The pane opens at a running turn's first `Thinking` block and holds
    /// until the turn ends — not until the block does, which on a fast
    /// model is 400 ms later. A turn this client cannot see running never
    /// opens it.
    #[test]
    fn the_pane_latches_at_the_turns_first_thinking_and_holds_to_its_end() {
        let (mut app, id) = fixture();
        let thinking = streaming_thinking(&mut app, id, 3);
        assert!(!app.observe_thinking(id, &[thinking]), "no known turn: nothing to latch");
        assert!(overlay_lines(&mut app, 80).is_empty(), "no known turn: no pane");
        app.mark_turn_running(id);
        assert!(app.observe_thinking(id, &[thinking]));
        assert!(!overlay_lines(&mut app, 80).is_empty());

        let done = thinking_then_answer(&mut app, id, 3);
        assert!(!overlay_lines(&mut app, 80).is_empty(), "the block completing is not the dismiss");
        assert_eq!(done.seq, thinking.seq, "the same block, now settled");
        app.mark_turn_ended(id);
        assert!(overlay_lines(&mut app, 80).is_empty(), "the turn's end closes it");
    }

    /// Reasoning an earlier turn left in the mirror does not open the pane:
    /// the latch is on a block this delivery touched.
    #[test]
    fn stale_reasoning_does_not_latch_a_new_turn() {
        let (mut app, id) = fixture();
        let _stale = streaming_thinking(&mut app, id, 3);
        app.mark_turn_running(id);
        assert!(!app.observe_thinking(id, &[]), "a delivery that touched no thinking block");
        assert!(overlay_lines(&mut app, 80).is_empty(), "no pane for reasoning nobody sent");
    }

    /// A completed thinking block seen only after it completed — one
    /// delivery carried the whole thing — still opens the pane.
    #[test]
    fn a_thinking_block_that_completed_inside_one_delivery_still_latches() {
        let (mut app, id) = fixture();
        app.mark_turn_running(id);
        let thinking = thinking_then_answer(&mut app, id, 3);
        assert!(app.observe_thinking(id, &[thinking]));
    }

    /// The pane holds the reasoning's tail, a third of the screen, and the
    /// block it is showing is not repeated in the transcript below it.
    #[test]
    fn the_pane_holds_the_latest_reasoning_and_the_transcript_drops_it() {
        let (mut app, id) = fixture();
        app.mark_turn_running(id);
        let thinking = thinking_then_answer(&mut app, id, 30);
        assert!(app.observe_thinking(id, &[thinking]));

        // A 24-row screen gives the pane a third: eight rows of tail.
        assert_eq!(thinking_band_lines(&app), 8);
        let pane = text_of(&overlay_lines(&mut app, 80));
        assert_eq!(pane.len(), 8, "got {pane:?}");
        assert_eq!(pane.first().map(String::as_str), Some("thought 22"), "got {pane:?}");
        assert_eq!(pane.last().map(String::as_str), Some("thought 29"), "got {pane:?}");

        let transcript = transcript_text(&mut app, 80);
        assert!(transcript.iter().any(|r| r.contains("The unlink bug")), "the answer streams: {transcript:?}");
        assert!(
            !transcript.iter().any(|r| r.starts_with("▸ thinking")),
            "the pane's block is not repeated as a stub: {transcript:?}"
        );
    }
}
