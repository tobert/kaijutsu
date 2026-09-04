//! The renderer, generic over `ratatui::Backend`.
//!
//! The transcript is `insert_before` output: a block that completes is
//! printed into the terminal's own scrollback as styled lines and never
//! touched again. The viewport holds what is live — streaming blocks, the
//! compose line, the status line. Scrolling, search and copy are the
//! terminal's (`docs/tui.md`, "Conversation").
//!
//! Nothing here reaches the kernel, and the only clock it reads is the one a
//! caller passes in as `now_millis`. That is what lets a `TestBackend` render
//! the same frames a real terminal gets.

use chrono::{Local, TimeZone};
use kaijutsu_types::{BlockKind, BlockSnapshot, ContextId, Status};
use ratatui::backend::Backend;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::text::Line;
use ratatui::widgets::{Paragraph, Widget};
use ratatui::{Frame, Terminal};

use crate::app::App;
use crate::status::{legend_line, status_line};

/// Lines the live region always holds: the compose line and the status line.
pub const LIVE_CHROME_LINES: u16 = 2;

/// Rows the inline viewport occupies.
///
/// Small on purpose: the viewport is what the terminal gives up, and the
/// transcript is in scrollback where the terminal's own scrolling, search
/// and copy reach it. A grown view (the picker, an ask card, the ledger)
/// changes this — [`viewport_lines`] is where.
pub const VIEWPORT_LINES: u16 = 8;

/// Rows of the thinking band: the tail of the turn's latest reasoning,
/// drawn above the stream while the thinking pane is open (`docs/tui.md`,
/// "The thinking pane").
/// A grown region's ceiling: a third of the screen. The compose region
/// grows toward it a row at a time; the thinking band takes it whole,
/// once, when a turn's reasoning starts (`docs/tui.md`, "The thinking
/// pane" and "Compose").
pub fn third_of_screen(app: &App) -> u16 {
    (app.screen_rows / 3).max(1)
}

/// Rows the thinking band takes above the stream while the pane is open.
pub fn thinking_band_lines(app: &App) -> u16 {
    third_of_screen(app)
}

/// The band's height with the thinking pane open: the ordinary rows, the
/// thinking band, and one blank row between them.
pub fn thinking_pane_lines(app: &App) -> u16 {
    VIEWPORT_LINES + thinking_band_lines(app) + 1
}

/// Whether the thinking pane is open: the current context's turn is running
/// and has shown a `Thinking` block (`App::thinking_pane_latched`). The
/// turn-liveness half is what keeps a block left `Running` by a lost turn
/// from holding the band open forever (`App::forget_turn_liveness`).
pub fn thinking_pane_open(app: &App) -> bool {
    app.current.is_some_and(|context_id| app.thinking_pane_latched(context_id))
}

/// Whether the in-flight strip has a running entry to animate — the event
/// loop redraws on `inflight::PHASE_MILLIS` only while it does.
pub fn strip_animating(app: &App) -> bool {
    let plan = live_plan(app);
    crate::inflight::animating(&crate::inflight::entries(plan.iter().map(|(b, _)| b), 0))
}

/// The turn's latest `Thinking` block, in document order, whatever its
/// status — the band shows the newest reasoning until the turn ends.
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

/// Whether a block has finished and can leave the viewport for scrollback.
///
/// `Waiting` is deliberately not settled: a gate's ask has stopped the block
/// but an answer still moves it, and a block printed to scrollback can never
/// be redrawn.
pub fn is_settled(block: &BlockSnapshot) -> bool {
    matches!(block.status, Status::Done | Status::Error)
}

/// One block's lines, ready for `insert_before`.
pub struct Print {
    pub context_id: ContextId,
    pub block_id: kaijutsu_types::BlockId,
    pub lines: Vec<Line<'static>>,
}

/// Every block of the current context that has completed since the last
/// frame, rendered once and marked printed.
///
/// The mark is what makes this idempotent: a block is printed exactly once,
/// and a later change to it is the status line's business, not a redraw
/// (`App::observe_change`).
pub fn take_settled_prints(app: &mut App, width: u16) -> Vec<Print> {
    let Some(context_id) = app.current else {
        return Vec::new();
    };
    let Some(view) = app.views.get(&context_id) else {
        return Vec::new();
    };
    let info = app.info(context_id).cloned();
    let pending: Vec<BlockSnapshot> = view
        .mirror
        .blocks()
        .iter()
        .filter(|b| is_settled(b) && !view.printed.contains(&b.id) && b.status != Status::Draft)
        .cloned()
        .collect();

    let mut prints = Vec::new();
    let mut last_speaker = view.last_printed_speaker.clone();
    let mut last_printed = view.last_printed;
    for block in pending {
        let speaker = app.speaker_for(&block, info.as_ref());
        // A result directly after its call shares the call's header: no
        // divider, no gap (`docs/tui.md`, "Conversation").
        let pair = crate::present::continues_pair(last_printed, &block);
        let show_divider = !pair && last_speaker.as_deref() != Some(speaker.as_str());
        let gap = !pair && speaker_gap(show_divider, last_speaker.as_deref());
        let stamp = wallclock(block.created_at);
        let mut lines = Vec::new();
        if gap {
            lines.push(Line::default());
        }
        {
            let mut view = app.block_view(&block, &speaker, &stamp, show_divider);
            // A completed `Thinking` block leaves one `▸` stub in scrollback,
            // whatever its collapse state: the reasoning was read as it
            // streamed in the thinking pane, and the whole text stays in
            // copy mode and `kj block read` (`docs/tui.md`, "The thinking
            // pane").
            if block.kind == BlockKind::Thinking {
                view.collapsed = true;
            }
            lines.extend(crate::present::render_block(&block, &view, width, &app.palette));
        }
        app.mark_printed(context_id, block.id, block.kind, &speaker);
        last_printed = Some((block.id, block.kind));
        last_speaker = Some(speaker);
        prints.push(Print {
            context_id,
            block_id: block.id,
            lines,
        });
    }
    prints
}

/// Whether a blank row goes above a block's divider: one row of air between
/// speakers, and none above the first speaker of a transcript or a copy
/// buffer, where there is nothing to separate from (`docs/tui.md`,
/// "Conversation").
pub fn speaker_gap(show_divider: bool, last_speaker: Option<&str>) -> bool {
    show_divider && last_speaker.is_some()
}

/// What one live block needs to render, resolved before the wrap cache is
/// borrowed mutably.
struct BlockPlan {
    speaker: String,
    stamp: String,
    show_divider: bool,
    gap: bool,
    collapsed: bool,
}

/// Every block of the current context still in the viewport, with its
/// speaker, stamp, divider decision and collapse resolved.
fn live_plan(app: &App) -> Vec<(BlockSnapshot, BlockPlan)> {
    let Some(context_id) = app.current else {
        return Vec::new();
    };
    let Some(view) = app.views.get(&context_id) else {
        return Vec::new();
    };
    let info = app.info(context_id);
    let mut out = Vec::new();
    let mut last_speaker = view.last_printed_speaker.clone();
    let mut last_block = view.last_printed;
    for block in view.mirror.blocks() {
        if view.printed.contains(&block.id) || block.status == Status::Draft {
            continue;
        }
        let speaker = app.speaker_for(block, info);
        let pair = crate::present::continues_pair(last_block, block);
        let show_divider = !pair && last_speaker.as_deref() != Some(speaker.as_str());
        let gap = !pair && speaker_gap(show_divider, last_speaker.as_deref());
        last_speaker = Some(speaker.clone());
        last_block = Some((block.id, block.kind));
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

/// One frame of the live region: its rows, and where the terminal's cursor
/// sits among them as `(row, col)` indexed into `lines`. `None` while
/// nothing is being typed — the picker, an ask card, the ledger and the
/// armed legend have no cursor, and the terminal hides it.
pub struct LiveFrame {
    pub lines: Vec<Line<'static>>,
    pub cursor: Option<(u16, u16)>,
}

/// The live region's rows alone — [`live_frame`] without the cursor.
pub fn live_lines(app: &mut App, width: u16, now_millis: u64, armed: bool) -> Vec<Line<'static>> {
    live_frame(app, width, now_millis, armed).lines
}

/// The live region: still-streaming blocks, the compose line, the status
/// line (or the armed-prefix legend in its place).
pub fn live_frame(app: &mut App, width: u16, now_millis: u64, armed: bool) -> LiveFrame {
    // The picker grows the viewport and replaces the live region entirely
    // while open — its own key line is the view's key line, the same
    // contract every grown view (`docs/tui.md`, "The picker") follows.
    if let Some(picker) = &app.picker {
        return LiveFrame { lines: crate::picker::render(picker, width, &app.palette), cursor: None };
    }

    let palette = app.palette;

    // The ask card and the ledger view replace the block stream and compose
    // line entirely while open — `docs/tui.md`'s "grows the viewport"
    // treatment. The full render is returned untruncated: `viewport_lines`
    // sizes the real viewport to hold it before this ever draws, so there is
    // nothing to budget here in the common case. A terminal too short to
    // hold what it asked for is `draw_live`'s problem, not this fn's — it
    // is the one place that actually knows the terminal's real height.
    if let Some(mut lines) = crate::asks::active_view_lines(app, width) {
        lines.push(if armed {
            legend_line(width, &palette)
        } else {
            status_line(&app.status_model(now_millis), width, &palette)
        });
        return LiveFrame { lines, cursor: None };
    }

    let mut lines = Vec::new();
    // The band this frame is drawn into — [`VIEWPORT_LINES`], or the
    // thinking pane's height while one is open.
    let band = viewport_lines(app, width);
    let pane_open = thinking_pane_open(app);

    // Resolve everything the wrap needs while `app` is only borrowed
    // immutably; the cache itself is a mutable borrow and cannot overlap.
    let plan: Vec<(BlockSnapshot, BlockPlan)> = live_plan(app);
    let context_type = app
        .current
        .and_then(|id| app.info(id))
        .map(|c| c.context_type.clone())
        .unwrap_or_else(|| "default".to_string());

    // The thinking band: the tail of the turn's latest reasoning, its own
    // rows above the stream so the answer streaming in never scrolls it
    // away, then one blank row. Thinking blocks leave the stream while the
    // pane holds them.
    let mut thinking = Vec::new();
    if pane_open && let Some(block) = latest_thinking(app) {
        let stamp = wallclock(block.created_at);
        let block_view = crate::present::BlockView {
            speaker: "",
            context_type: &context_type,
            stamp: &stamp,
            show_divider: false,
            tool: None,
            arg: None,
            collapsed: false,
            local_ctx: Some(block.id.context_id),
        };
        let band_rows = usize::from(thinking_band_lines(app));
        let rendered = app.wrap.lines(&block, &block_view, width, &palette);
        let keep = band_rows.min(rendered.len());
        thinking.extend(rendered[rendered.len() - keep..].iter().cloned());
        thinking.push(Line::default());
    }

    for (block, item) in &plan {
        if pane_open && block.kind == BlockKind::Thinking {
            continue;
        }
        // Unsettled calls and gate-held results are the strip's, not the
        // stream's: their bodies would otherwise resize the stream every
        // time one arrived or wrapped (`docs/tui.md`, "The in-flight strip").
        if crate::inflight::takes_from_stream(block) {
            continue;
        }
        let (tool, arg) = crate::app::tool_header(block);
        let block_view = crate::present::BlockView {
            speaker: &item.speaker,
            context_type: &context_type,
            stamp: &item.stamp,
            show_divider: item.show_divider,
            tool,
            arg,
            collapsed: item.collapsed,
            local_ctx: Some(block.id.context_id),
        };
        if item.gap {
            lines.push(Line::default());
        }
        lines.extend(
            app.wrap
                .lines(block, &block_view, width, &palette)
                .iter()
                .cloned(),
        );
    }

    // The input region is drawn first because it sizes the transcript. A
    // draft longer than the cap (a third of the screen) shows the rows
    // around the cursor, the way vim scrolls a long command line.
    let mut input = crate::compose::input_lines(app, width, &palette);
    let (mut cursor_row, cursor_col) = app.compose.cursor_cell(width);
    let cap = compose_rows_cap(app);
    if input.len() > cap {
        let start = usize::from(cursor_row).saturating_sub(cap - 1).min(input.len() - cap);
        input = input[start..start + cap].to_vec();
        cursor_row = u16::try_from(usize::from(cursor_row) - start).unwrap_or(0);
    }

    // Keep the tail: a long stream shows its newest lines, not its oldest.
    // The budget is the viewport's own height, so a line this function emits
    // is a line the terminal actually shows.
    // One status line, one blank row above the input region, the in-flight
    // strip's row, plus however many rows the input region takes.
    let chrome = 3 + u16::try_from(input.len()).unwrap_or(u16::MAX);
    let budget = usize::from(band.saturating_sub(chrome)).saturating_sub(thinking.len());
    if lines.len() > budget {
        lines.drain(..lines.len() - budget);
    }
    if !thinking.is_empty() {
        thinking.append(&mut lines);
        lines = thinking;
    }

    // The `:kj ` completion popup rides above the compose line, only while
    // the bar is actually mid-`:kj ` — a stale popup left over from an
    // earlier `Tab` press does not reappear once the bar has moved past it
    // (`completion.rs`).
    if let Some(completion) = &app.completion
        && app.compose.kj_typed().is_some()
    {
        lines.extend(crate::completion::render_popup(completion, width, &palette));
    }

    // The in-flight strip: one row, always, so the band never resizes for
    // a tool call coming or going — only the row's text changes.
    let strip = crate::inflight::entries(plan.iter().map(|(b, _)| b), now_millis);
    lines.push(crate::inflight::strip_line(&strip, width, &palette, crate::inflight::phase(now_millis)));

    // One blank row separates what is being read from what is being typed
    // (`docs/tui.md`, "Conversation").
    lines.push(Line::default());

    // While `Ctrl+A` is pending the legend takes the compose row, not the
    // status line: the status line's seat digits are what the player is
    // about to press, and covering them was the bug Amy hit.
    let cursor = if armed {
        lines.push(legend_line(width, &palette));
        None
    } else {
        // The terminal's own cursor, on the draft's vi cursor: the row is
        // the compose region's first line plus the draft row it is on.
        let first = u16::try_from(lines.len()).unwrap_or(u16::MAX);
        lines.extend(input);
        Some((first.saturating_add(cursor_row), cursor_col))
    };
    lines.push(status_line(&app.status_model(now_millis), width, &palette));
    LiveFrame { lines, cursor }
}

/// Print completed blocks into the terminal's scrollback, above the inline
/// viewport.
pub fn print_scrollback<B: Backend>(
    terminal: &mut Terminal<B>,
    prints: &[Print],
) -> Result<(), B::Error> {
    for print in prints {
        let height = u16::try_from(print.lines.len()).unwrap_or(u16::MAX);
        if height == 0 {
            continue;
        }
        let lines = print.lines.clone();
        terminal.insert_before(height, move |buf| {
            Paragraph::new(lines).render(buf.area, buf);
        })?;
    }
    Ok(())
}

/// Redraw the live region, bottom-aligned in the viewport: the status line
/// is the viewport's last row and the band's unused rows are the gap above
/// compose, never a gap under the status line (`docs/tui.md`, ruling 1: "a
/// viewport at the bottom"). A grown view that fills its band is unmoved.
pub fn draw_live<B: Backend>(
    terminal: &mut Terminal<B>,
    app: &mut App,
    now_millis: u64,
    armed: bool,
) -> Result<(), B::Error> {
    let width = terminal.size()?.width;
    let LiveFrame { lines, cursor } = live_frame(app, width, now_millis, armed);
    terminal.draw(|frame| {
        let area = frame.area();
        let height = u16::try_from(lines.len()).unwrap_or(u16::MAX).min(area.height);
        let bottom = Rect {
            y: area.y + area.height - height,
            height,
            ..area
        };
        // The common case never reaches the crop below: `viewport_lines`
        // already grew the real viewport to hold every line, so `height`
        // equals `lines.len()`. When the terminal itself is shorter than
        // that (too small a window, or a resize still catching up), crop
        // from the FRONT and keep the tail — the tail is where a grown
        // view's key-hints line lives (`asks.rs`'s ask card and ledger),
        // and showing everything but the way to answer is the bug this
        // closes.
        let start = lines.len().saturating_sub(usize::from(height));
        frame.render_widget(Paragraph::new(lines[start..].to_vec()), bottom);
        // A cursor on a row the crop dropped stays hidden with the row.
        if let Some((row, col)) = cursor
            && let Some(y) = usize::from(row).checked_sub(start)
        {
            let y = u16::try_from(y).unwrap_or(u16::MAX);
            frame.set_cursor_position((bottom.x + col, bottom.y + y));
        }
    })?;
    Ok(())
}

/// Draw the transcript and the live region into one frame.
///
/// A real terminal never does this — the transcript lives in scrollback and
/// only the live region is drawn — but it is the same content in one buffer,
/// which is what a `TestBackend` can observe.
pub fn draw_full<B: Backend>(
    terminal: &mut Terminal<B>,
    transcript: &[Line<'static>],
    live: &[Line<'static>],
) -> Result<(), B::Error> {
    terminal.draw(|frame: &mut Frame| {
        let live_height = u16::try_from(live.len()).unwrap_or(u16::MAX);
        let [top, bottom] = Layout::vertical([Constraint::Min(0), Constraint::Length(live_height)])
            .areas::<2>(frame.area());
        frame.render_widget(Paragraph::new(transcript.to_vec()), top);
        frame.render_widget(Paragraph::new(live.to_vec()), bottom);
    })?;
    Ok(())
}

/// Build copy mode's buffer for the context on screen: every block in
/// document order, rendered exactly as the transcript printer would
/// ([`crate::present::render_block`]) — the whole context, not only what
/// scrollback has already shown (`docs/tui.md`, "Copy mode"). The draft is
/// excluded — it is the compose line, not the transcript.
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

/// Rows the inline viewport should occupy right now — [`VIEWPORT_LINES`]
/// ordinarily, or a grown view's own height while one is open: the picker's
/// (width-independent), the ask card's (wraps by width, so counted at
/// `width` — the same width `draw_live` will render at), or the ledger's
/// (width-independent, same as the picker). The one place viewport growth
/// lands (`docs/tui.md`, "grows the viewport and shrinks on dismiss"): a
/// future grown view adds its own arm here rather than each surface picking
/// its own resize path.
pub fn viewport_lines(app: &App, width: u16) -> u16 {
    if let Some(picker) = &app.picker {
        return VIEWPORT_LINES.max(crate::picker::viewport_lines(picker));
    }
    if let Some(active) = crate::asks::active_view_viewport_lines(app, width) {
        return VIEWPORT_LINES.max(active);
    }
    let mut want = VIEWPORT_LINES;
    if thinking_pane_open(app) {
        want = want.max(thinking_pane_lines(app));
    }
    // A draft past one row grows the band a row per wrapped row, up to a
    // third of the screen, so the stream keeps its rows while a long
    // prompt is typed; the band shrinks once, when the draft is submitted
    // (`docs/tui.md`, "Compose").
    let rows = crate::compose::input_lines(app, width, &app.palette).len();
    let extra = rows.min(compose_rows_cap(app)).saturating_sub(1);
    want.saturating_add(u16::try_from(extra).unwrap_or(u16::MAX))
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

    /// Two settled blocks and a status line, rendered into a `TestBackend`.
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

    #[test]
    fn a_settled_block_is_printed_once_and_never_again() {
        let (mut app, _) = fixture();
        let first = take_settled_prints(&mut app, 80);
        assert_eq!(first.len(), 2);
        let second = take_settled_prints(&mut app, 80);
        assert!(second.is_empty(), "a printed block is never re-offered");
    }

    /// A run of blocks by one speaker carries one divider, even when they
    /// print on separate frames — the streamed reply that follows a
    /// `Thinking` block is the case that showed this.
    #[test]
    fn a_second_frame_by_the_same_speaker_repeats_no_divider() {
        let id = ContextId::new();
        let mut app = App::new("amy");
        app.set_contexts(vec![ctx(id, "kaijutsu")]);
        let mut mirror = ContextMirror::new(id);
        mirror
            .apply_snapshot(
                vec![block(id, 1, BlockKind::Thinking, Role::Model, Status::Done, "hmm")],
                1,
            )
            .expect("snapshot applies");
        app.views.insert(id, ContextView::new(mirror));
        app.switch_to(id);
        let first = take_settled_prints(&mut app, 80);
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].lines.len(), 2, "divider plus the thinking stub");

        let mut mirror = ContextMirror::new(id);
        mirror
            .apply_snapshot(
                vec![block(id, 2, BlockKind::Text, Role::Model, Status::Done, "ok")],
                1,
            )
            .expect("snapshot applies");
        let view = app.views.get_mut(&id).expect("a view");
        view.mirror = mirror;
        let second = take_settled_prints(&mut app, 80);
        assert_eq!(second.len(), 1);
        assert_eq!(second[0].lines.len(), 1, "no second divider for one speaker");
    }

    #[test]
    fn a_streaming_block_stays_in_the_live_region() {
        let (mut app, id) = fixture();
        let mut mirror = ContextMirror::new(id);
        mirror
            .apply_snapshot(
                vec![block(
                    id,
                    3,
                    BlockKind::Text,
                    Role::Model,
                    Status::Running,
                    "still going",
                )],
                1,
            )
            .expect("snapshot applies");
        app.views.insert(id, ContextView::new(mirror));

        assert!(take_settled_prints(&mut app, 80).is_empty());
        let live = live_lines(&mut app, 80, 0, false);
        let text: Vec<String> = live
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect();
        assert!(text.iter().any(|l| l.contains("still going")), "got {text:?}");
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
        let mut terminal = Terminal::new(TestBackend::new(80, 7)).expect("test terminal");
        draw_live(&mut terminal, &mut app, 0, false).expect("draw");
        let compose_row = rows(&terminal)
            .iter()
            .position(|r| r.starts_with("❯ hi"))
            .expect("the compose row is drawn");
        let at = terminal.get_cursor_position().expect("cursor position");
        assert_eq!((at.x, at.y), (4, compose_row as u16), "past `❯ hi`");

        let armed = live_frame(&mut app, 80, 0, true);
        assert_eq!(armed.cursor, None, "the legend row has no cursor");
    }

    #[test]
    fn the_frame_carries_the_transcript_and_the_status_line() {
        let (mut app, _) = fixture();
        // Five transcript rows, then the live region's four: the strip, the
        // blank row, the prompt, the status line.
        let mut terminal =
            Terminal::new(TestBackend::new(96, 9)).expect("test backend builds");

        let prints = take_settled_prints(&mut app, 96);
        let transcript: Vec<Line<'static>> =
            prints.into_iter().flat_map(|p| p.lines).collect();
        let live = live_lines(&mut app, 96, 60_000, false);
        draw_full(&mut terminal, &transcript, &live).expect("draw");

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
        let idle: Vec<String> = live_lines(&mut app, 96, 0, false)
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect();
        let status = idle.last().expect("a status line").clone();
        assert!(idle.iter().any(|l| l.starts_with(crate::compose::PROMPT)), "idle draws the compose row");

        let armed: Vec<String> = live_lines(&mut app, 96, 0, true)
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect();
        assert_eq!(armed.last(), Some(&status), "the status line is untouched");
        assert!(armed.iter().any(|l| l.starts_with("Ctrl+A:")), "the legend is drawn: {armed:?}");
        assert!(!armed.iter().any(|l| l.starts_with(crate::compose::PROMPT)), "the compose row is hidden: {armed:?}");
    }

    /// Nothing you would paste is inside a box: no border glyph opens any
    /// transcript row, and the only ruled lines are role dividers.
    #[test]
    fn the_transcript_has_no_border_glyphs() {
        let (mut app, _) = fixture();
        let prints = take_settled_prints(&mut app, 96);
        for print in prints {
            for line in print.lines {
                let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
                for glyph in ['│', '╭', '╮', '╰', '╯', '┌', '└'] {
                    assert!(!text.contains(glyph), "{text:?} carries {glyph}");
                }
            }
        }
    }

    /// The strip takes a tool call's body out of the band and names it on
    /// one row instead; a running result's output still streams in the
    /// stream; and the band is the same height with or without either
    /// (`docs/tui.md`, "The in-flight strip").
    #[test]
    fn an_unsettled_tool_call_is_a_strip_entry_and_the_band_does_not_grow() {
        let (mut app, id) = fixture();
        assert_eq!(viewport_lines(&app, 80), VIEWPORT_LINES);

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
        let mut mirror = ContextMirror::new(id);
        mirror.apply_snapshot(vec![call, result], 1).expect("snapshot applies");
        app.views.insert(id, ContextView::new(mirror));

        let live = live_lines(&mut app, 80, 5_000, false);
        assert_eq!(viewport_lines(&app, 80), VIEWPORT_LINES, "a tool call never grows the band");
        assert!(live.len() <= usize::from(VIEWPORT_LINES), "{}", live.len());
        let text: Vec<String> = live
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect();
        assert!(
            text.iter().any(|l| l.contains("◐ shell cargo test -p kaijutsu-kernel · 4s")),
            "the strip names the running call: {text:?}"
        );
        assert!(
            !text.iter().any(|l| l.contains(r#"{"command""#)),
            "the call's body left the band: {text:?}"
        );
        assert!(
            text.iter().any(|l| l.contains("unlink_symlink ... ok")),
            "the running result's output still streams: {text:?}"
        );
    }

    /// With nothing in flight the strip is still there — an empty row of
    /// its own ground directly above the blank row over `❯` — so the band's
    /// row count is the same in both states.
    #[test]
    fn the_strip_row_is_present_when_empty() {
        let (mut app, _) = fixture();
        let live = live_lines(&mut app, 40, 0, false);
        let prompt = live.iter().position(|l| l.spans.iter().any(|s| s.content.contains('❯'))).expect("prompt row");
        let blank: String = live[prompt - 1].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(blank.trim().is_empty());
        let strip = &live[prompt - 2];
        let text: String = strip.spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(text, " ".repeat(40), "an empty strip is a full-width row of ground");
        assert!(strip.spans.iter().all(|s| s.style.bg.is_some()), "the strip's ground is painted");
    }

    /// A tool call and its result print as one unit: the pair header
    /// (`─ caller · tool ─ arg ─… stamp`), then the result's body with no
    /// second divider and no gap; the model's next words get their own
    /// divider again (`docs/tui.md`, "Conversation").
    #[test]
    fn a_tool_pair_prints_under_one_header() {
        let (mut app, id) = fixture();
        let _ = take_settled_prints(&mut app, 96);
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
        let mut mirror = ContextMirror::new(id);
        mirror.apply_snapshot(vec![call, result, after], 1).expect("snapshot applies");
        app.views.insert(id, ContextView::new(mirror));

        let prints = take_settled_prints(&mut app, 96);
        let rows: Vec<String> = prints
            .iter()
            .flat_map(|p| p.lines.iter())
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect::<String>())
            .collect();
        let header = rows.iter().position(|r| r.starts_with("─ deepseek-v4 · shell ─ kj ledger list ─")).expect("the pair header");
        assert_eq!(rows[header + 1], "(no pending approvals)", "the result follows the header directly: {rows:?}");
        assert_eq!(rows.iter().filter(|r| r.contains("· shell")).count(), 1, "one header for the pair: {rows:?}");
        assert!(!rows.iter().any(|r| r.contains(r#"{"command""#)), "the call body folded into the header: {rows:?}");
        let next = rows.iter().position(|r| r == "Nothing pending.").expect("the model's next words");
        assert!(rows[next - 1].starts_with("─ deepseek-v4 · coder ─"), "the model's words get their divider back: {rows:?}");
    }

    #[test]
    fn a_long_stream_keeps_its_newest_lines() {
        let (mut app, id) = fixture();
        let body = (0..60).map(|n| format!("line {n}")).collect::<Vec<_>>().join("\n");
        let mut mirror = ContextMirror::new(id);
        mirror
            .apply_snapshot(
                vec![block(id, 9, BlockKind::Text, Role::Model, Status::Running, &body)],
                1,
            )
            .expect("snapshot applies");
        app.views.insert(id, ContextView::new(mirror));

        let live = live_lines(&mut app, 80, 0, false);
        assert!(live.len() <= usize::from(VIEWPORT_LINES));
        let text: Vec<String> = live
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect();
        assert!(text.iter().any(|l| l == "line 59"), "got {text:?}");
        assert!(!text.iter().any(|l| l == "line 0"), "got {text:?}");
    }

    /// A multi-line draft grows the compose region inside the viewport, and
    /// the transcript gives up the rows.
    #[test]
    fn a_multi_line_draft_takes_rows_from_the_transcript() {
        let (mut app, id) = fixture();
        let body = (0..20).map(|n| format!("line {n}")).collect::<Vec<_>>().join("\n");
        let mut mirror = ContextMirror::new(id);
        mirror
            .apply_snapshot(
                vec![block(id, 9, BlockKind::Text, Role::Model, Status::Running, &body)],
                1,
            )
            .expect("snapshot applies");
        app.views.insert(id, ContextView::new(mirror));

        let one_line = live_lines(&mut app, 80, 0, false).len();
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
        let grown = live_lines(&mut app, 80, 0, false);
        // The band grows one row for the second draft row (a third of a
        // 24-row screen is the cap, far above two), so the stream keeps its
        // rows and the frame is one line taller.
        assert_eq!(viewport_lines(&app, 80), VIEWPORT_LINES + 1, "the band grew one row for the draft");
        assert_eq!(grown.len(), one_line + 1, "the frame is one row taller");
        // Past the cap the band stops growing and the draft scrolls instead.
        app.screen_rows = 6; // cap = 2 rows
        for _ in 0..3 {
            app.compose.press(ratatui::crossterm::event::KeyEvent::new(
                ratatui::crossterm::event::KeyCode::Enter,
                ratatui::crossterm::event::KeyModifiers::NONE,
            ));
        }
        assert_eq!(viewport_lines(&app, 80), VIEWPORT_LINES + 1, "capped at a third of the screen");
        app.screen_rows = 24;
        let text: Vec<String> = grown
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect();
        assert!(text.iter().any(|l| l.starts_with("❯ a")), "got {text:?}");
        assert!(text.iter().any(|l| l.trim() == "b"), "the second draft row: {text:?}");
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

        let live = live_lines(&mut app, 96, 0, false);
        let text: Vec<String> = live
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect();
        assert!(text.iter().any(|l| l.starts_with(":kj con")), "got {text:?}");
        assert!(!text.iter().any(|l| l.contains("❯")), "the draft prompt left with the draft: {text:?}");
    }

    #[test]
    fn a_waiting_block_is_not_settled() {
        let id = ContextId::new();
        let waiting = block(id, 1, BlockKind::ToolCall, Role::Model, Status::Waiting, "rm -rf");
        assert!(!is_settled(&waiting), "an unanswered ask can still move");
        let done = block(id, 2, BlockKind::ToolCall, Role::Model, Status::Done, "ls");
        assert!(is_settled(&done));
    }

    /// An open ask card replaces the block stream and compose line entirely
    /// — the status line still renders beneath it (`docs/tui.md`, "Asks").
    #[test]
    fn an_ask_card_replaces_the_live_region_and_keeps_the_status_line() {
        let (mut app, id) = fixture();
        app.ask_card = Some(crate::asks::AskCardState {
            request_id: "01a04eb6".to_string(),
            context_id: id,
            detail: kaijutsu_client::AskDetail {
                request_id: "01a04eb6".to_string(),
                context_id: Some(id),
                status: "pending".to_string(),
                origin: "shell_gate".to_string(),
                tool: Some("shell_write".to_string()),
                hook_id: None,
                instance: None,
                description: "rm -rf ~/src/wt/kaish-arith".to_string(),
                authorized_label: None,
                statements: vec!["rm -rf ~/src/wt/kaish-arith".to_string()],
                exec_source: None,
                cwd: None,
                env: Vec::new(),
                created_at: None,
                decided_at: None,
                decided_by: None,
                decided_option: None,
                remember_scope: None,
                redeemed_at: None,
            },
        });
        let mut terminal = Terminal::new(TestBackend::new(96, 4)).expect("test backend builds");
        draw_live(&mut terminal, &mut app, 0, false).expect("draw");
        let text = rows(&terminal);
        assert!(text[0].contains("⚠ ask 01a04eb6"), "got {text:?}");
        assert!(text[0].contains("shell_write"), "got {text:?}");
        assert!(text.iter().any(|l| l.contains("rm -rf ~/src/wt/kaish-arith")), "got {text:?}");
        assert!(text.last().unwrap().starts_with("0 kaijutsu*"), "status line still renders: {text:?}");
    }

    /// A long ask statement at a narrow width wraps past the old fixed
    /// `VIEWPORT_LINES - 1` budget; the key-hints line — the only way to
    /// answer the ask — must be the last content line, not the header or a
    /// middle row of the wrapped statement (kaibo review, 2026-09-03).
    #[test]
    fn a_long_ask_statement_keeps_the_key_hints_line_last() {
        let (mut app, id) = fixture();
        let statement =
            "one two three four five six seven eight nine ten eleven twelve thirteen".to_string();
        app.ask_card = Some(crate::asks::AskCardState {
            request_id: "01a04eb6".to_string(),
            context_id: id,
            detail: kaijutsu_client::AskDetail {
                request_id: "01a04eb6".to_string(),
                context_id: Some(id),
                status: "pending".to_string(),
                origin: "shell_gate".to_string(),
                tool: Some("shell_write".to_string()),
                hook_id: None,
                instance: None,
                description: statement.clone(),
                authorized_label: None,
                statements: vec![statement],
                exec_source: None,
                cwd: None,
                env: Vec::new(),
                created_at: None,
                decided_at: None,
                decided_by: None,
                decided_option: None,
                remember_scope: None,
                redeemed_at: None,
            },
        });
        let live = live_lines(&mut app, 16, 0, false);
        let last_content: String = live[live.len() - 2].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(
            last_content.contains("[a]llow"),
            "key hints must be the last content line before the status line, got {last_content:?} in {live:?}"
        );
    }

    /// A ledger with more rows than the old fixed budget loses the same
    /// key-hints line the same way — `a allow once ... Esc back` must stay
    /// visible.
    #[test]
    fn a_long_ledger_keeps_the_key_hints_line_last() {
        let (mut app, _id) = fixture();
        let ledger_rows = (0..10)
            .map(|i| {
                crate::asks::LedgerRow::Pending(crate::asks::PendingRow {
                    request_id: format!("p{i}"),
                    age: Some("1s".to_string()),
                    context_label: "kaijutsu".to_string(),
                    context_type: "coder".to_string(),
                    hook: "shell_write".to_string(),
                    statement: "git worktree remove --force ~/src/wt/kaish-arith".to_string(),
                })
            })
            .collect();
        app.ledger_view = Some(crate::asks::LedgerViewState {
            rows: ledger_rows,
            filter: String::new(),
            selected: 0,
            filtering: false,
        });
        let live = live_lines(&mut app, 96, 0, false);
        let last_content: String = live[live.len() - 2].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(
            last_content.starts_with("a allow once"),
            "key hints must be the last content line before the status line, got {last_content:?}"
        );
    }

    /// [`viewport_lines`] must grow to fit an open ask card's full render,
    /// the same treatment the picker already gets — a long statement at a
    /// narrow width needs more than [`VIEWPORT_LINES`].
    #[test]
    fn viewport_lines_grows_for_an_open_ask_card() {
        let (mut app, id) = fixture();
        let statement =
            "one two three four five six seven eight nine ten eleven twelve thirteen".to_string();
        app.ask_card = Some(crate::asks::AskCardState {
            request_id: "01a04eb6".to_string(),
            context_id: id,
            detail: kaijutsu_client::AskDetail {
                request_id: "01a04eb6".to_string(),
                context_id: Some(id),
                status: "pending".to_string(),
                origin: "shell_gate".to_string(),
                tool: Some("shell_write".to_string()),
                hook_id: None,
                instance: None,
                description: statement.clone(),
                authorized_label: None,
                statements: vec![statement],
                exec_source: None,
                cwd: None,
                env: Vec::new(),
                created_at: None,
                decided_at: None,
                decided_by: None,
                decided_option: None,
                remember_scope: None,
                redeemed_at: None,
            },
        });
        assert!(viewport_lines(&app, 12) > VIEWPORT_LINES);
    }

    /// Same for the ledger, and its growth must not depend on width — its
    /// rows never wrap, they truncate (`ledger_viewport_lines`).
    #[test]
    fn viewport_lines_grows_for_an_open_ledger() {
        let (mut app, _id) = fixture();
        let ledger_rows = (0..10)
            .map(|i| {
                crate::asks::LedgerRow::Pending(crate::asks::PendingRow {
                    request_id: format!("p{i}"),
                    age: Some("1s".to_string()),
                    context_label: "kaijutsu".to_string(),
                    context_type: "coder".to_string(),
                    hook: "shell_write".to_string(),
                    statement: "git worktree remove --force ~/src/wt/kaish-arith".to_string(),
                })
            })
            .collect();
        app.ledger_view = Some(crate::asks::LedgerViewState {
            rows: ledger_rows,
            filter: String::new(),
            selected: 0,
            filtering: false,
        });
        assert!(viewport_lines(&app, 96) > VIEWPORT_LINES);
    }

    /// A terminal shorter than the grown view crops from the FRONT, keeping
    /// the tail — the key-hints line is the last thing an ask card or the
    /// ledger renders, so it is the last thing that should disappear, not
    /// the first (`docs/tui.md`, "The picker").
    #[test]
    fn a_terminal_shorter_than_the_grown_view_keeps_the_tail() {
        let (mut app, _id) = fixture();
        let ledger_rows = (0..10)
            .map(|i| {
                crate::asks::LedgerRow::Pending(crate::asks::PendingRow {
                    request_id: format!("p{i}"),
                    age: Some("1s".to_string()),
                    context_label: "kaijutsu".to_string(),
                    context_type: "coder".to_string(),
                    hook: "shell_write".to_string(),
                    statement: "git worktree remove --force ~/src/wt/kaish-arith".to_string(),
                })
            })
            .collect();
        app.ledger_view = Some(crate::asks::LedgerViewState {
            rows: ledger_rows,
            filter: String::new(),
            selected: 0,
            filtering: false,
        });
        // The full view needs more than 4 rows; hand draw_live a terminal
        // that only has 4.
        let mut terminal = Terminal::new(TestBackend::new(96, 4)).expect("test backend builds");
        draw_live(&mut terminal, &mut app, 0, false).expect("draw");
        let text = rows(&terminal);
        assert!(
            text.last().unwrap().starts_with("0 kaijutsu*"),
            "status line still last: {text:?}"
        );
        assert!(
            text[text.len() - 2].starts_with("a allow once"),
            "key hints must be the row above status when the terminal is short: {text:?}"
        );
    }

    /// Copy mode's buffer holds the whole context — including a block
    /// already printed to scrollback, which `live_plan`/`take_settled_prints`
    /// deliberately exclude — and excludes the draft, which is compose's
    /// line, not the transcript.
    #[test]
    fn copy_buffer_lines_covers_the_whole_context_but_not_the_draft() {
        let (mut app, id) = fixture();
        // `fixture` already settled two blocks; print them so they would be
        // invisible to `live_plan`.
        let _ = take_settled_prints(&mut app, 80);
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
                    BlockSnapshotBuilder::new(BlockId::new(id, PrincipalId::new(), 3), BlockKind::Text)
                        .role(Role::User)
                        .status(Status::Draft)
                        .content("still typing")
                        .build(),
                ],
                1,
            )
            .expect("snapshot applies");
        app.views.get_mut(&id).expect("a view").mirror = mirror;

        let (label, lines) = copy_buffer_lines(&app, 80).expect("a context is on screen");
        assert_eq!(label, "kaijutsu");
        let text: Vec<String> = lines.iter().map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect()).collect();
        assert!(text.iter().any(|l| l.contains("and getattr?")), "got {text:?}");
        assert!(text.iter().any(|l| l.contains("rename and getattr")), "got {text:?}");
        assert!(!text.iter().any(|l| l.contains("still typing")), "the draft is not the transcript: {text:?}");
    }

    #[test]
    fn copy_buffer_lines_is_none_with_no_context_on_screen() {
        let app = App::new("amy");
        assert!(copy_buffer_lines(&app, 80).is_none());
    }

    /// The ledger view replaces the live region the same way, both sections
    /// visible (`docs/tui.md`, "The ledger").
    #[test]
    fn the_ledger_view_replaces_the_live_region() {
        let (mut app, _id) = fixture();
        app.ledger_view = Some(crate::asks::LedgerViewState {
            rows: vec![crate::asks::LedgerRow::Pending(crate::asks::PendingRow {
                request_id: "p1".to_string(),
                age: Some("12s".to_string()),
                context_label: "kaijutsu".to_string(),
                context_type: "coder".to_string(),
                hook: "shell_write".to_string(),
                statement: "git worktree remove --force".to_string(),
            })],
            filter: String::new(),
            selected: 0,
            filtering: false,
        });
        // LEDGER header + PENDING + one row + key line + status: 5 lines,
        // the full render `viewport_lines` would grow the real viewport to
        // hold (`a_terminal_shorter_than_the_grown_view_keeps_the_tail`
        // covers the undersized case on purpose; this test wants the
        // ordinary one).
        let mut terminal = Terminal::new(TestBackend::new(96, 5)).expect("test backend builds");
        draw_live(&mut terminal, &mut app, 0, false).expect("draw");
        let text = rows(&terminal);
        assert!(text[0].starts_with("LEDGER"), "got {text:?}");
        assert!(text.iter().any(|l| l.contains("p1") && l.contains("shell_write")), "got {text:?}");
    }

    // ────────────────────────────────────────────────────────────────────
    // The thinking pane (docs/tui.md, "The thinking pane")
    // ────────────────────────────────────────────────────────────────────

    fn text_of(lines: &[Line<'static>]) -> Vec<String> {
        lines
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect()
    }

    /// A completed `Thinking` block prints as one `▸` stub naming its size
    /// and first line, whatever its collapse state.
    #[test]
    fn a_completed_thinking_block_prints_as_a_stub() {
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
                    "first the unlink path\nthen rename\nthen getattr",
                )],
                1,
            )
            .expect("snapshot applies");
        app.views.insert(id, ContextView::new(mirror));
        let prints = take_settled_prints(&mut app, 80);
        let rows = text_of(&prints.last().expect("a print").lines);
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

    fn streaming_thinking(app: &mut App, id: ContextId, lines: usize) {
        let body = (0..lines).map(|n| format!("thought {n}")).collect::<Vec<_>>().join("\n");
        let mut mirror = ContextMirror::new(id);
        mirror
            .apply_snapshot(
                vec![block(id, 9, BlockKind::Thinking, Role::Model, Status::Running, &body)],
                1,
            )
            .expect("snapshot applies");
        app.views.insert(id, ContextView::new(mirror));
    }

    /// A turn whose reasoning already completed and whose answer is now
    /// streaming: a `Done` Thinking block of `lines` rows, then a `Running`
    /// Text block.
    fn thinking_then_answer(app: &mut App, id: ContextId, lines: usize) {
        let body = (0..lines).map(|n| format!("thought {n}")).collect::<Vec<_>>().join("\n");
        let mut mirror = ContextMirror::new(id);
        mirror
            .apply_snapshot(
                vec![
                    block(id, 9, BlockKind::Thinking, Role::Model, Status::Done, &body),
                    block(id, 10, BlockKind::Text, Role::Model, Status::Running, "The unlink bug"),
                ],
                1,
            )
            .expect("snapshot applies");
        app.views.insert(id, ContextView::new(mirror));
    }

    /// The pane opens at a running turn's first `Thinking` block and holds
    /// until the turn ends — not until the block does, which on a fast
    /// model is 400 ms later. A turn this client cannot see running never
    /// opens it.
    #[test]
    fn the_pane_latches_at_the_turns_first_thinking_and_holds_to_its_end() {
        let (mut app, id) = fixture();
        streaming_thinking(&mut app, id, 3);
        assert!(!app.observe_thinking(id), "no known turn: nothing to latch");
        assert_eq!(viewport_lines(&app, 80), VIEWPORT_LINES, "no known turn: no pane");
        app.mark_turn_running(id);
        assert!(app.observe_thinking(id));
        assert_eq!(viewport_lines(&app, 80), thinking_pane_lines(&app));

        thinking_then_answer(&mut app, id, 3);
        assert_eq!(viewport_lines(&app, 80), thinking_pane_lines(&app), "the block completing is not the dismiss");
        app.mark_turn_ended(id);
        assert_eq!(viewport_lines(&app, 80), VIEWPORT_LINES, "the turn's end closes it");
    }

    /// A completed thinking block seen only after it completed — one
    /// delivery carried the whole thing — still opens the pane, as long as
    /// its stub has not printed yet.
    #[test]
    fn a_thinking_block_that_completed_inside_one_delivery_still_latches() {
        let (mut app, id) = fixture();
        app.mark_turn_running(id);
        thinking_then_answer(&mut app, id, 3);
        assert!(app.observe_thinking(id));
    }

    /// The band above the stream keeps the reasoning's tail while the
    /// answer streams below it, so the answer never scrolls the reasoning
    /// out of the pane.
    #[test]
    fn the_band_holds_the_latest_reasoning_above_the_streaming_answer() {
        let (mut app, id) = fixture();
        app.mark_turn_running(id);
        thinking_then_answer(&mut app, id, 30);
        assert!(app.observe_thinking(id));
        let live = live_lines(&mut app, 80, 0, false);
        // A short answer leaves the frame under the viewport's height;
        // `draw_live` bottom-aligns it. What must hold is the band's shape.
        assert!(live.len() <= usize::from(thinking_pane_lines(&app)), "{}", live.len());
        let rows = text_of(&live);
        let newest = rows.iter().position(|r| r == "thought 29").expect("the newest line");
        // A 24-row screen gives the band a third: eight rows of tail.
        assert_eq!(thinking_band_lines(&app), 8);
        let oldest = rows.iter().position(|r| r == "thought 22").expect("eight rows of tail");
        assert!(!rows.iter().any(|r| r == "thought 21"), "the band is a third of the screen: {rows:?}");
        let answer = rows.iter().position(|r| r.contains("The unlink bug")).expect("the answer streams");
        assert!(oldest < newest && newest < answer, "band above the stream: {rows:?}");
        assert_eq!(rows[newest + 1], "", "one blank row between them: {rows:?}");
    }
}
