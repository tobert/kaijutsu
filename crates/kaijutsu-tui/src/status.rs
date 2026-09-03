//! The status line: screen's window list on the left, kaijutsu's facts on the
//! right.
//!
//! ```text
//! 0 kaijutsu*  1 kaish@  2 lfm2d  3 exo   coder/deepseek-v4  ▮ 42%  ⟳ 91%  ⏱ 4m12s/5m  ● ok
//! ```
//!
//! Left to right: the rank (seat digits, `*` current, `@` activity, `!` an
//! ask waiting in that seat, `!n` the pending count across all contexts);
//! cast and model; context-window occupancy; cache health; connection state.
//! The armed-prefix legend replaces the whole line while `Ctrl+A` is pending.
//! `docs/tui.md`, "Status line" and "Cache health".
//!
//! Pure: every figure is computed from values a caller hands in, including
//! `now`. No clock is read here.

use std::time::Duration;

use kaijutsu_client::{ConnectionStatus, ContextInfo};
use ratatui::text::{Line, Span};

use crate::present::Palette;

/// How much attention a figure is asking for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    Ok,
    Warning,
    Alarm,
}

/// One rendered status-line figure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Figure {
    pub text: String,
    pub severity: Severity,
}

impl Figure {
    fn ok(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            severity: Severity::Ok,
        }
    }

    fn at(severity: Severity, text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            severity,
        }
    }

    fn span(&self, palette: &Palette) -> Span<'static> {
        let style = match self.severity {
            Severity::Ok => palette.status(),
            Severity::Warning => palette.warning(),
            Severity::Alarm => palette.alarm(),
        };
        Span::styled(self.text.clone(), style)
    }
}

/// Cache health for one context, from the last completed LLM call.
///
/// Every field is `None` when the kernel reported `0`, which means unknown —
/// an age is never dressed up as an expiry and a share is never guessed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CacheHealth {
    /// Age of the last completed call.
    pub age: Option<Duration>,
    /// The TTL that call's cache breakpoints chose (300s ephemeral, 3600s
    /// extended). `None` when the provider path emitted no `cache_control`.
    pub ttl: Option<Duration>,
    /// The cached share of the last call's prompt, in whole percent.
    pub cached_share: Option<u32>,
}

/// Read cache health off a `ContextInfo`. `now_millis` is unix milliseconds,
/// the same base `last_call_at` carries.
///
/// The cached share's denominator is `context_used_tokens` — the last call's
/// fill. The wire carries no separate input-token count, so this reads
/// "cached share of the last call" rather than "cached share of its prompt";
/// the two differ by the completion. A future `inputTokens` field on
/// `ContextHandleInfo` is what would make it exact.
pub fn cache_health(info: &ContextInfo, now_millis: u64) -> CacheHealth {
    let age = info
        .last_call_at
        .map(|at| Duration::from_millis(now_millis.saturating_sub(at)));
    let ttl = info.cache_ttl_secs.map(Duration::from_secs);
    let cached_share = match (info.cache_read_tokens, info.context_used_tokens) {
        (Some(read), Some(total)) if total > 0 => Some(((read * 100) / total) as u32),
        _ => None,
    };
    CacheHealth {
        age,
        ttl,
        cached_share,
    }
}

impl CacheHealth {
    /// `⏱ 4m12s`, `⏱ 4m12s/5m`, `⏱ 6m01s ✗5m`, or `⏱ —`.
    ///
    /// The segment warns past 80% of the TTL and alarms past it. With no TTL
    /// known the age stands alone and never warns — there is nothing to be
    /// past.
    pub fn age_figure(&self) -> Figure {
        let Some(age) = self.age else {
            return Figure::ok("⏱ —");
        };
        let age_text = format_age(age);
        match self.ttl {
            None => Figure::ok(format!("⏱ {age_text}")),
            Some(ttl) if age > ttl => Figure::at(
                Severity::Alarm,
                format!("⏱ {age_text} ✗{}", format_ttl(ttl)),
            ),
            Some(ttl) => {
                let warn = age.as_secs() * 100 >= ttl.as_secs() * 80;
                let severity = if warn { Severity::Warning } else { Severity::Ok };
                Figure::at(severity, format!("⏱ {age_text}/{}", format_ttl(ttl)))
            }
        }
    }

    /// `⟳ 91%`, or `⟳ —` when the provider reported no cache accounting.
    pub fn share_figure(&self) -> Figure {
        match self.cached_share {
            Some(pct) => Figure::ok(format!("⟳ {pct}%")),
            None => Figure::ok("⟳ —"),
        }
    }
}

/// `4m12s`, `17s`, `1h04m`.
fn format_age(age: Duration) -> String {
    let secs = age.as_secs();
    let (h, m, s) = (secs / 3600, (secs % 3600) / 60, secs % 60);
    if h > 0 {
        format!("{h}h{m:02}m")
    } else if m > 0 {
        format!("{m}m{s:02}s")
    } else {
        format!("{s}s")
    }
}

/// `5m`, `1h`, `90s` — the TTL as the shortest exact unit.
fn format_ttl(ttl: Duration) -> String {
    let secs = ttl.as_secs();
    if secs > 0 && secs.is_multiple_of(3600) {
        format!("{}h", secs / 3600)
    } else if secs > 0 && secs.is_multiple_of(60) {
        format!("{}m", secs / 60)
    } else {
        format!("{secs}s")
    }
}

/// One seat in the rank: `2 lfm2d@`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SeatCell {
    pub digit: usize,
    pub label: String,
    /// `*` — the context on screen.
    pub current: bool,
    /// `@` — activity since you last looked (screen's monitor flag).
    pub activity: bool,
    /// `!` — an ask waiting in that seat.
    pub ask: bool,
}

impl SeatCell {
    fn text(&self) -> String {
        let mut flags = String::new();
        if self.current {
            flags.push('*');
        }
        if self.activity {
            flags.push('@');
        }
        if self.ask {
            flags.push('!');
        }
        format!("{} {}{}", self.digit, self.label, flags)
    }
}

/// Everything the status line draws.
#[derive(Debug, Clone, Default)]
pub struct StatusModel {
    pub seats: Vec<SeatCell>,
    /// `coder/deepseek-v4` — the cast and the model.
    pub cast_model: Option<String>,
    /// Context-window occupancy in whole percent.
    pub occupancy: Option<u32>,
    pub cache: CacheHealth,
    /// Pending asks across every context. Rendered `!n`.
    pub pending_asks: usize,
    pub connection: Option<ConnectionStatus>,
    /// A notice replaces the seat list until the next keystroke — the
    /// scrollback-staleness channel (`docs/tui.md`, "Conversation").
    pub notice: Option<String>,
    /// `bar.beat` + pulse for the playing track (`docs/tui.md`, "TRACKS +
    /// beat" / "Status line"). `None` when nothing is playing.
    pub track: Option<TrackFigure>,
}

/// The status line's `17.3 ●` figure: bar.beat from the last `listTracks`
/// poll, pulse from the phasor's live envelope at redraw time — the same
/// `bar`/`beat` [`crate::picker::bar_beat`] derives, so the picker's TRACKS
/// row and this figure never disagree about which beat a track is on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TrackFigure {
    pub bar: u64,
    pub beat: u64,
    /// Whether the phasor's envelope is over the beat threshold right now —
    /// `●` on the beat, `○` between beats.
    pub pulse: bool,
}

impl TrackFigure {
    fn figure(&self) -> Figure {
        let glyph = if self.pulse { "●" } else { "○" };
        Figure::ok(format!("{}.{} {glyph}", self.bar, self.beat))
    }
}

impl StatusModel {
    /// The left half: the rank, or a notice when one is set.
    pub fn left_figures(&self) -> Vec<Figure> {
        if let Some(notice) = &self.notice {
            return vec![Figure::at(Severity::Warning, notice.clone())];
        }
        let mut figures: Vec<Figure> = self
            .seats
            .iter()
            .map(|seat| {
                let severity = if seat.ask {
                    Severity::Warning
                } else {
                    Severity::Ok
                };
                Figure::at(severity, seat.text())
            })
            .collect();
        if self.pending_asks > 0 {
            figures.push(Figure::at(
                Severity::Warning,
                format!("!{}", self.pending_asks),
            ));
        }
        figures
    }

    /// The right half: cast/model, occupancy, cache health, connection.
    pub fn right_figures(&self) -> Vec<Figure> {
        let mut figures = Vec::new();
        if let Some(cast_model) = &self.cast_model {
            figures.push(Figure::ok(cast_model.clone()));
        }
        figures.push(match self.occupancy {
            Some(pct) if pct >= 90 => Figure::at(Severity::Alarm, format!("▮ {pct}%")),
            Some(pct) if pct >= 75 => Figure::at(Severity::Warning, format!("▮ {pct}%")),
            Some(pct) => Figure::ok(format!("▮ {pct}%")),
            None => Figure::ok("▮ —"),
        });
        figures.push(self.cache.share_figure());
        figures.push(self.cache.age_figure());
        if let Some(track) = &self.track {
            figures.push(track.figure());
        }
        figures.push(connection_figure(self.connection.as_ref()));
        figures
    }
}

/// `● ok`, `◐ connecting`, `○ offline`, `✗ terminal`.
pub fn connection_figure(status: Option<&ConnectionStatus>) -> Figure {
    match status {
        Some(ConnectionStatus::Connected { .. }) => Figure::ok("● ok"),
        Some(ConnectionStatus::Connecting { .. }) => Figure::at(Severity::Warning, "◐ connecting"),
        Some(ConnectionStatus::Cooldown { .. }) => Figure::at(Severity::Warning, "◐ retrying"),
        Some(ConnectionStatus::Closing { .. }) => Figure::at(Severity::Warning, "◌ closing"),
        Some(ConnectionStatus::Terminal { .. }) => Figure::at(Severity::Alarm, "✗ terminal"),
        Some(ConnectionStatus::Idle) | None => Figure::at(Severity::Warning, "○ offline"),
    }
}

/// The status line, left figures pushed against the right ones.
///
/// The rank is the elastic half: seats are dropped from the end until the
/// line fits, so the low digits and the fixed-width facts both survive a
/// narrow terminal. A rank of long labels is the ordinary case — the facts
/// would otherwise never be visible at all. The facts go only when the
/// terminal is too narrow to hold them alone.
pub fn status_line(model: &StatusModel, width: u16, palette: &Palette) -> Line<'static> {
    let width = usize::from(width.max(1));
    let mut left = model.left_figures();
    let right = model.right_figures();
    let right_w = join_width(&right);

    if right_w + 2 > width {
        // No room for the facts; the rank alone gets the line.
        let mut spans: Vec<Span<'static>> = Vec::new();
        push_joined(&mut spans, &left, palette);
        return Line::from(spans);
    }
    while !left.is_empty() && join_width(&left) + right_w + 2 > width {
        left.pop();
    }

    let left_w = join_width(&left);
    let mut spans: Vec<Span<'static>> = Vec::new();
    push_joined(&mut spans, &left, palette);
    spans.push(Span::styled(
        " ".repeat(width - left_w - right_w),
        palette.status(),
    ));
    push_joined(&mut spans, &right, palette);
    Line::from(spans)
}

/// The armed-prefix legend, which replaces the status line while `Ctrl+A` is
/// pending. `docs/input.md`, "The prefix table" — the chords this client
/// answers today, in that table's order.
pub fn legend_line(width: u16, palette: &Palette) -> Line<'static> {
    let full = "Ctrl+A: 0-9 seat · Ctrl+A last · \" picker · l ledger · [ copy · Esc cancel";
    let short = "Ctrl+A: 0-9 seat · Ctrl+A last · \" picker";
    let width = usize::from(width.max(1));
    let text = if full.chars().count() <= width { full } else { short };
    Line::from(Span::styled(text.to_string(), palette.warning()))
}

fn join_width(figures: &[Figure]) -> usize {
    if figures.is_empty() {
        return 0;
    }
    figures
        .iter()
        .map(|f| f.text.chars().count())
        .sum::<usize>()
        + 2 * (figures.len() - 1)
}

fn push_joined(spans: &mut Vec<Span<'static>>, figures: &[Figure], palette: &Palette) {
    for (i, figure) in figures.iter().enumerate() {
        if i > 0 {
            spans.push(Span::styled("  ", palette.status()));
        }
        spans.push(figure.span(palette));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kaijutsu_types::ContextId;

    fn info() -> ContextInfo {
        ContextInfo {
            id: ContextId::new(),
            label: "kaijutsu".to_string(),
            forked_from: None,
            provider: String::new(),
            model: "deepseek-v4".to_string(),
            created_at: 1_000,
            trace_id: [0u8; 16],
            fork_kind: None,
            context_type: "coder".to_string(),
            archived: false,
            concluded_at: None,
            keywords: Vec::new(),
            top_block_preview: None,
            live_status: kaijutsu_types::Status::Pending,
            last_activity_at: None,
            track_id: None,
            promoted_at: None,
            demoted_at: None,
            paused_at: None,
            context_window: None,
            context_used_tokens: None,
            context_used_pct: None,
            background_running_count: 0,
            background_oldest_running_started_at: None,
            background_last_finished_at: None,
            background_last_finished_status: None,
            background_last_exit_code: None,
            cast_label: None,
            origin_host: None,
            cwd: None,
            last_call_at: None,
            cache_read_tokens: None,
            cache_write_tokens: None,
            cache_ttl_secs: None,
        }
    }

    fn text(line: &Line<'static>) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    #[test]
    fn a_context_that_never_called_a_model_shows_both_dashes() {
        let health = cache_health(&info(), 10_000);
        assert_eq!(health.age_figure().text, "⏱ —");
        assert_eq!(health.share_figure().text, "⟳ —");
    }

    #[test]
    fn last_call_at_is_unix_milliseconds() {
        let mut c = info();
        c.last_call_at = Some(1_000_000);
        // 252 seconds later, in milliseconds.
        let health = cache_health(&c, 1_252_000);
        assert_eq!(health.age, Some(Duration::from_secs(252)));
        assert_eq!(health.age_figure().text, "⏱ 4m12s");
    }

    #[test]
    fn an_age_with_no_ttl_never_warns() {
        let mut c = info();
        c.last_call_at = Some(0);
        let health = cache_health(&c, 3_600_000);
        assert_eq!(health.age_figure().severity, Severity::Ok);
        assert_eq!(health.age_figure().text, "⏱ 1h00m");
    }

    #[test]
    fn a_known_ttl_follows_the_age_as_a_suffix() {
        let mut c = info();
        c.last_call_at = Some(0);
        c.cache_ttl_secs = Some(300);
        let health = cache_health(&c, 60_000);
        assert_eq!(health.age_figure().text, "⏱ 1m00s/5m");
        assert_eq!(health.age_figure().severity, Severity::Ok);
    }

    #[test]
    fn the_ttl_segment_warns_past_eighty_percent() {
        let mut c = info();
        c.last_call_at = Some(0);
        c.cache_ttl_secs = Some(300);
        let health = cache_health(&c, 240_000);
        assert_eq!(health.age_figure().severity, Severity::Warning);
        assert_eq!(health.age_figure().text, "⏱ 4m00s/5m");
    }

    #[test]
    fn past_the_ttl_the_suffix_becomes_a_cross() {
        let mut c = info();
        c.last_call_at = Some(0);
        c.cache_ttl_secs = Some(300);
        let health = cache_health(&c, 361_000);
        assert_eq!(health.age_figure().text, "⏱ 6m01s ✗5m");
        assert_eq!(health.age_figure().severity, Severity::Alarm);
    }

    #[test]
    fn an_hour_ttl_renders_as_one_h() {
        let mut c = info();
        c.last_call_at = Some(0);
        c.cache_ttl_secs = Some(3600);
        let health = cache_health(&c, 60_000);
        assert_eq!(health.age_figure().text, "⏱ 1m00s/1h");
    }

    #[test]
    fn the_cached_share_is_read_over_the_last_calls_fill() {
        let mut c = info();
        c.cache_read_tokens = Some(910);
        c.context_used_tokens = Some(1000);
        assert_eq!(cache_health(&c, 0).share_figure().text, "⟳ 91%");
    }

    #[test]
    fn a_zero_denominator_reads_as_unknown_not_as_a_division() {
        let mut c = info();
        c.cache_read_tokens = Some(910);
        c.context_used_tokens = Some(0);
        assert_eq!(cache_health(&c, 0).share_figure().text, "⟳ —");
    }

    #[test]
    fn seat_flags_render_in_order() {
        let seat = SeatCell {
            digit: 1,
            label: "kaish".to_string(),
            current: true,
            activity: true,
            ask: true,
        };
        assert_eq!(seat.text(), "1 kaish*@!");
    }

    #[test]
    fn the_status_line_pushes_the_facts_to_the_right_edge() {
        let palette = Palette::builtin();
        let model = StatusModel {
            seats: vec![
                SeatCell {
                    digit: 0,
                    label: "kaijutsu".to_string(),
                    current: true,
                    activity: false,
                    ask: false,
                },
                SeatCell {
                    digit: 1,
                    label: "kaish".to_string(),
                    current: false,
                    activity: true,
                    ask: false,
                },
            ],
            cast_model: Some("coder/deepseek-v4".to_string()),
            occupancy: Some(42),
            cache: CacheHealth {
                age: Some(Duration::from_secs(252)),
                ttl: Some(Duration::from_secs(300)),
                cached_share: Some(91),
            },
            pending_asks: 0,
            connection: None,
            notice: None,
            track: None,
        };
        let line = status_line(&model, 100, &palette);
        let rendered = text(&line);
        assert_eq!(rendered.chars().count(), 100);
        assert!(rendered.starts_with("0 kaijutsu*  1 kaish@"), "got {rendered:?}");
        assert!(
            rendered.ends_with("coder/deepseek-v4  ▮ 42%  ⟳ 91%  ⏱ 4m12s/5m  ○ offline"),
            "got {rendered:?}"
        );
    }

    #[test]
    fn the_pending_ask_count_rides_next_to_the_rank() {
        let model = StatusModel {
            pending_asks: 2,
            ..Default::default()
        };
        let figures = model.left_figures();
        assert_eq!(figures.last().unwrap().text, "!2");
        assert_eq!(figures.last().unwrap().severity, Severity::Warning);
    }

    #[test]
    fn a_notice_replaces_the_rank() {
        let model = StatusModel {
            seats: vec![SeatCell {
                digit: 0,
                label: "kaijutsu".to_string(),
                current: true,
                activity: false,
                ask: false,
            }],
            notice: Some("block #12 changed after print".to_string()),
            ..Default::default()
        };
        let figures = model.left_figures();
        assert_eq!(figures.len(), 1);
        assert_eq!(figures[0].text, "block #12 changed after print");
    }

    /// A rank of long labels is the ordinary case, so seats give way to the
    /// facts rather than the other way around — the low digits are the ones
    /// a hand reaches for, and they survive.
    #[test]
    fn a_crowded_rank_drops_its_last_seats_before_the_facts() {
        let palette = Palette::builtin();
        let seat = |digit: usize, label: &str| SeatCell {
            digit,
            label: label.to_string(),
            current: digit == 0,
            activity: false,
            ask: false,
        };
        let model = StatusModel {
            seats: vec![
                seat(0, "kaijutsu"),
                seat(1, "cc-exomemory-0816-1059"),
                seat(2, "cc-kaijutsu-0902-1816"),
                seat(3, "score-cap2-musician-e1"),
            ],
            cast_model: Some("coder/deepseek-v4".to_string()),
            occupancy: Some(42),
            ..Default::default()
        };
        let rendered = text(&status_line(&model, 80, &palette));
        assert_eq!(rendered.chars().count(), 80);
        assert!(rendered.starts_with("0 kaijutsu*"), "got {rendered:?}");
        assert!(!rendered.contains("score-cap2"), "got {rendered:?}");
        assert!(rendered.contains("coder/deepseek-v4"), "got {rendered:?}");
        assert!(rendered.ends_with("○ offline"), "got {rendered:?}");
    }

    #[test]
    fn a_terminal_too_narrow_for_the_facts_keeps_the_rank() {
        let palette = Palette::builtin();
        let model = StatusModel {
            seats: vec![SeatCell {
                digit: 0,
                label: "kaijutsu".to_string(),
                current: true,
                activity: false,
                ask: false,
            }],
            cast_model: Some("coder/deepseek-v4".to_string()),
            ..Default::default()
        };
        let rendered = text(&status_line(&model, 20, &palette));
        assert_eq!(rendered, "0 kaijutsu*");
    }

    #[test]
    fn the_track_figure_renders_bar_dot_beat_and_the_pulse_glyph() {
        let palette = Palette::builtin();
        let model = StatusModel {
            track: Some(TrackFigure { bar: 17, beat: 3, pulse: true }),
            ..Default::default()
        };
        let rendered = text(&status_line(&model, 40, &palette));
        assert!(rendered.contains("17.3 ●"), "got {rendered:?}");
    }

    #[test]
    fn no_playing_track_carries_no_figure() {
        let model = StatusModel { track: None, ..Default::default() };
        let figures = model.right_figures();
        assert!(figures.iter().all(|f| !f.text.contains('.')), "got {figures:?}");
    }

    #[test]
    fn the_legend_shortens_before_it_overflows() {
        let palette = Palette::builtin();
        let wide = text(&legend_line(100, &palette));
        let narrow = text(&legend_line(50, &palette));
        assert!(wide.contains("ledger"));
        assert!(wide.contains("[ copy"), "got {wide:?}");
        assert!(!narrow.contains("ledger"));
        assert!(narrow.chars().count() <= 50);
    }
}
