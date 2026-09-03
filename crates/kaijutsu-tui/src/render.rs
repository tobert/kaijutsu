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
use kaijutsu_types::{BlockSnapshot, ContextId, Status};
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
/// and copy reach it. A grown view (picker, ledger) is a later lane and is
/// what changes this.
pub const VIEWPORT_LINES: u16 = 6;

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
    for block in pending {
        let speaker = app.speaker_for(&block, info.as_ref());
        let show_divider = last_speaker.as_deref() != Some(speaker.as_str());
        let stamp = wallclock(block.created_at);
        let lines = {
            let view = app.block_view(&block, &speaker, &stamp, show_divider);
            crate::present::render_block(&block, &view, width, &app.palette)
        };
        app.mark_printed(context_id, block.id, &speaker);
        last_speaker = Some(speaker);
        prints.push(Print {
            context_id,
            block_id: block.id,
            lines,
        });
    }
    prints
}

/// What one live block needs to render, resolved before the wrap cache is
/// borrowed mutably.
struct BlockPlan {
    speaker: String,
    stamp: String,
    show_divider: bool,
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
    for block in view.mirror.blocks() {
        if view.printed.contains(&block.id) || block.status == Status::Draft {
            continue;
        }
        let speaker = app.speaker_for(block, info);
        let show_divider = last_speaker.as_deref() != Some(speaker.as_str());
        last_speaker = Some(speaker.clone());
        out.push((
            block.clone(),
            BlockPlan {
                speaker,
                stamp: wallclock(block.created_at),
                show_divider,
                collapsed: view.is_collapsed(block),
            },
        ));
    }
    out
}

/// The live region: still-streaming blocks, the compose line, the status
/// line (or the armed-prefix legend in its place).
pub fn live_lines(app: &mut App, width: u16, now_millis: u64, armed: bool) -> Vec<Line<'static>> {
    // The picker grows the viewport and replaces the live region entirely
    // while open — its own key line is the view's key line, the same
    // contract every grown view (`docs/tui.md`, "The picker") follows.
    if let Some(picker) = &app.picker {
        return crate::picker::render(picker, width, &app.palette);
    }

    let palette = app.palette;

    // The ask card and the ledger view replace the block stream and compose
    // line entirely while open — `docs/tui.md`'s "grows the viewport"
    // treatment (`asks.rs`'s doc names why this stays budget-truncated
    // rather than a real terminal resize for now).
    if let Some(mut lines) = crate::asks::active_view_lines(app, width) {
        let budget = usize::from(VIEWPORT_LINES.saturating_sub(1));
        if lines.len() > budget {
            lines.truncate(budget);
        }
        lines.push(if armed {
            legend_line(width, &palette)
        } else {
            status_line(&app.status_model(now_millis), width, &palette)
        });
        return lines;
    }

    let mut lines = Vec::new();

    // Resolve everything the wrap needs while `app` is only borrowed
    // immutably; the cache itself is a mutable borrow and cannot overlap.
    let plan: Vec<(BlockSnapshot, BlockPlan)> = live_plan(app);
    let context_type = app
        .current
        .and_then(|id| app.info(id))
        .map(|c| c.context_type.clone())
        .unwrap_or_else(|| "default".to_string());
    for (block, item) in &plan {
        let block_view = crate::present::BlockView {
            speaker: &item.speaker,
            context_type: &context_type,
            stamp: &item.stamp,
            show_divider: item.show_divider,
            collapsed: item.collapsed,
            local_ctx: Some(block.id.context_id),
        };
        lines.extend(
            app.wrap
                .lines(block, &block_view, width, &palette)
                .iter()
                .cloned(),
        );
    }

    // The input region is drawn first because it sizes the transcript: a
    // multi-line draft grows the compose region inside the viewport and the
    // transcript gives up the rows (`docs/tui.md`, "Compose").
    let input = crate::compose::input_lines(app, width, &palette);

    // Keep the tail: a long stream shows its newest lines, not its oldest.
    // The budget is the viewport's own height, so a line this function emits
    // is a line the terminal actually shows.
    // One status line, plus however many rows the input region takes.
    let chrome = 1 + u16::try_from(input.len()).unwrap_or(u16::MAX);
    let budget = usize::from(VIEWPORT_LINES.saturating_sub(chrome));
    if lines.len() > budget {
        lines.drain(..lines.len() - budget);
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

    lines.extend(input);
    lines.push(if armed {
        legend_line(width, &palette)
    } else {
        status_line(&app.status_model(now_millis), width, &palette)
    });
    lines
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
    let lines = live_lines(app, width, now_millis, armed);
    terminal.draw(|frame| {
        let area = frame.area();
        let height = u16::try_from(lines.len()).unwrap_or(u16::MAX).min(area.height);
        let bottom = Rect {
            y: area.y + area.height - height,
            height,
            ..area
        };
        frame.render_widget(Paragraph::new(lines), bottom);
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
}/// Rows the inline viewport should occupy right now — [`VIEWPORT_LINES`]
/// ordinarily, or a grown view's own height while one is open. The one place
/// viewport growth lands (`docs/tui.md`, "grows the viewport and shrinks on
/// dismiss"): a future grown view (ledger, asks) adds its own arm here rather
/// than each surface picking its own resize path.
pub fn viewport_lines(app: &App) -> u16 {
    match &app.picker {
        Some(picker) => VIEWPORT_LINES.max(crate::picker::viewport_lines(picker)),
        None => VIEWPORT_LINES,
    }
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
        assert_eq!(first[0].lines.len(), 3, "divider plus the two thinking lines");

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

    #[test]
    fn the_frame_carries_the_transcript_and_the_status_line() {
        let (mut app, _) = fixture();
        let mut terminal =
            Terminal::new(TestBackend::new(96, 8)).expect("test backend builds");

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
        assert!(
            rows[2].starts_with("─ deepseek-v4 · coder ─"),
            "the model's divider follows: {rows:?}"
        );
        assert_eq!(rows[3], "rename and getattr share the cause.");

        let status = rows.last().expect("a status line");
        assert!(status.starts_with("0 kaijutsu*"), "got {status:?}");
        assert!(status.contains("coder/deepseek-v4"), "got {status:?}");
        assert!(status.contains("▮ 42%"), "got {status:?}");
        assert!(status.contains("⟳ 91%"), "got {status:?}");
        assert!(status.contains("⏱ 1m00s/5m"), "got {status:?}");
        assert!(status.ends_with("○ offline"), "got {status:?}");
    }

    #[test]
    fn the_armed_prefix_legend_replaces_the_status_line() {
        let (mut app, _) = fixture();
        let live = live_lines(&mut app, 96, 0, true);
        let last: String = live
            .last()
            .expect("a last line")
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect();
        assert!(last.starts_with("Ctrl+A:"), "got {last:?}");
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
        let now = std::time::Instant::now();
        for code in [
            ratatui::crossterm::event::KeyCode::Char('a'),
            ratatui::crossterm::event::KeyCode::Enter,
            ratatui::crossterm::event::KeyCode::Char('b'),
        ] {
            app.compose.press(
                ratatui::crossterm::event::KeyEvent::new(
                    code,
                    ratatui::crossterm::event::KeyModifiers::NONE,
                ),
                now,
            );
        }
        let grown = live_lines(&mut app, 80, 0, false);
        assert_eq!(grown.len(), one_line, "the viewport height did not change");
        let text: Vec<String> = grown
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect();
        assert!(text.iter().any(|l| l.starts_with("❯ a")), "got {text:?}");
        assert!(text.iter().any(|l| l.trim() == "b"), "the second draft row: {text:?}");
    }

    /// `:` swaps the compose row for the `:` bar, and the bar rides behind
    /// the same `❯` prompt (`docs/tui.md`, "The `:` line").
    #[test]
    fn the_colon_bar_replaces_the_compose_line() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let (mut app, _) = fixture();
        let now = std::time::Instant::now();
        let press = |app: &mut App, code: KeyCode| {
            app.compose.press(KeyEvent::new(code, KeyModifiers::NONE), now);
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
        assert!(text.iter().any(|l| l.starts_with("❯ :kj con")), "got {text:?}");
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
        let mut terminal = Terminal::new(TestBackend::new(96, 4)).expect("test backend builds");
        draw_live(&mut terminal, &mut app, 0, false).expect("draw");
        let text = rows(&terminal);
        assert!(text[0].starts_with("LEDGER"), "got {text:?}");
        assert!(text.iter().any(|l| l.contains("p1") && l.contains("shell_write")), "got {text:?}");
    }
}
