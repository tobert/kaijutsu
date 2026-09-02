//! The picker (`Ctrl+A "`) — the well flattened into ACTIVE / RECENT /
//! TRACKS sections, in a viewport that grows to hold it and shrinks on
//! dismiss. `docs/tui.md`, "The picker (`Ctrl+A \"`)".
//!
//! [`PickerModel`] is pure: [`PickerModel::build`] takes a `list_contexts`
//! answer, a `listTracks` answer, and the live-state maps the caller already
//! tracks (activity, tails), plus the clock as a plain `now_millis` — no RPC,
//! no I/O, no live clock read here. [`PickerModel::handle_key`] is pure too:
//! it returns an [`Outcome`] the caller acts on, never touching the kernel
//! itself. [`render`] is the pure mapper to styled lines, the same three-part
//! split every other surface in this crate follows.
//!
//! Ring placement is [`assign_ring_seats`] — the same pure function
//! `kaijutsu_client::rank` calls, so the picker's ACTIVE/RECENT split and the
//! status line's rank agree by construction. This module calls it directly
//! (rather than going through `kaijutsu_client::ranked_seats`) because it also
//! needs the horizon list `ranked_seats` does not expose.
//!
//! [`PickerTails`] is a picker-local, single-line port of the app's
//! `time_well::live::ContextTails`: it folds the same kernel-wide
//! `ServerEvent` stream, but keeps only the newest line per context (the
//! picker's tail column shows one line, not a scrollback) and does not
//! resolve a streaming placeholder in place — a model reply that streams in
//! via `TextAppended` on a context's own feed (not the kernel-wide stream)
//! updates the picker's tail only when a later kernel-wide event lands for
//! that context. `next_onset`/`rearm` are the beat-timer's pure re-arm
//! arithmetic (`docs/tui.md`, "Timing to music"; `docs/midi.md`, "The one
//! timebase") — the `tokio::select!` arm that calls them lives in `run.rs`.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind};
use kaijutsu_client::{ContextInfo, ServerEvent, TrackInfo};
use kaijutsu_types::{BlockKind, BlockSnapshot, ContextId, Role, Status};
use kaijutsu_viz::layout::{Band, ContextLifecycle, assign_ring_seats};
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthChar;

use crate::present::Palette;

/// Beats per bar, for the `bar.beat` figure and the pulse glyph's dot count.
/// The wire carries no time signature (`TrackInfo` has no such field), so
/// this is a presentation-only assumption — 4/4, the common case — not a
/// value read from the kernel. A track actually running in another meter
/// renders a `bar.beat` that free-runs against the wrong bar length; fixing
/// that needs a wire field, not a client-side guess.
pub const BEATS_PER_BAR: u64 = 4;

/// How long a context's tail must have moved to count as `●` chatter-now,
/// vs. `@` (activity since you last looked, which never expires on its own).
const CHATTER_WINDOW_MS: u64 = 4_000;

/// Longest tail text kept per context (matches the app's `TAIL_LINE_CHARS`
/// intent at a terminal-row scale).
const TAIL_CHARS: usize = 90;

// ============================================================================
// ROWS
// ============================================================================

/// One ACTIVE or RECENT row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    pub context_id: ContextId,
    /// The seat digit — `Some` only in ACTIVE, matching the well and the
    /// status line (`Ctrl+A <digit>` and the picker digit both name this
    /// same seat).
    pub digit: Option<usize>,
    pub label: String,
    pub context_type: String,
    /// `●` — a tail line landed within [`CHATTER_WINDOW_MS`].
    pub chatter: bool,
    /// `@` — activity since this context was last on screen (the same flag
    /// the status line's rank carries).
    pub activity: bool,
    pub running: bool,
    pub errored: bool,
    pub paused: bool,
    /// Age of `last_activity_at` (or `created_at` if never touched), in
    /// milliseconds.
    pub age_ms: u64,
    /// The context's own last line, from [`PickerTails`]. `None` before any
    /// kernel-wide event has landed for it this session.
    pub tail: Option<String>,
}

/// One TRACKS row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrackRow {
    pub id: String,
    pub score_context_id: ContextId,
    pub playing: bool,
    pub bpm: u64,
    /// 1-indexed bar and beat, derived from `playhead_tick` and
    /// [`BEATS_PER_BAR`] — see that constant's doc for the assumption this
    /// rests on.
    pub bar: u64,
    pub beat: u64,
}

impl TrackRow {
    /// Beats per second, for the beat timer's `next_onset`/`rearm` — derived
    /// from `bpm` rather than carrying `period_us` separately, since the two
    /// are the same tempo in different units.
    pub fn tempo_bps(&self) -> f64 {
        self.bpm as f64 / 60.0
    }
}

fn bar_beat(playhead_tick: i64, beats_per_bar: u64) -> (u64, u64) {
    let tick = playhead_tick.max(0) as u64;
    (tick / beats_per_bar + 1, tick % beats_per_bar + 1)
}

/// One `listTracks` row as a [`TrackRow`] — the single place `period_us` /
/// `playhead_tick` become `bpm` / `bar.beat`, shared by [`PickerModel::build`]
/// and `run.rs`'s periodic `App::tracks` refresh (the status line's `bar.beat`
/// figure reads the latter) so the two never disagree.
pub fn track_row_from(info: &TrackInfo) -> TrackRow {
    let bpm = 60_000_000u64.checked_div(info.period_us).unwrap_or(0);
    let (bar, beat) = bar_beat(info.playhead_tick, BEATS_PER_BAR);
    TrackRow { id: info.id.clone(), score_context_id: info.score_context_id, playing: info.playing, bpm, bar, beat }
}

fn row_from(info: &ContextInfo, digit: Option<usize>, activity: bool, tails: &PickerTails, now_millis: u64) -> Row {
    Row {
        context_id: info.id,
        digit,
        label: if info.label.is_empty() { info.id.short() } else { info.label.clone() },
        context_type: info.context_type.clone(),
        chatter: tails.chatter(info.id, now_millis, CHATTER_WINDOW_MS),
        activity,
        running: info.live_status == Status::Running,
        errored: info.live_status == Status::Error,
        paused: info.paused_at.is_some(),
        age_ms: now_millis.saturating_sub(info.last_activity_at.unwrap_or(info.created_at)),
        tail: tails.last(info.id).map(str::to_string),
    }
}

// ============================================================================
// TAILS
// ============================================================================

/// The context's own last line, fed from the kernel-wide `ServerEvent`
/// stream — a single-line, single-context-keyed cousin of the app's
/// `ContextTails` (`docs/tui.md`, "The picker").
#[derive(Debug, Default)]
pub struct PickerTails {
    lines: HashMap<ContextId, (String, u64)>,
}

impl PickerTails {
    pub fn new() -> Self {
        Self::default()
    }

    /// Fold one kernel-wide event. Returns whether a tail changed (the
    /// caller's redraw signal) — only `BlockInserted` carries a renderable
    /// line; every other event is a no-op here.
    pub fn observe(&mut self, event: &ServerEvent, now_millis: u64) -> bool {
        let ServerEvent::BlockInserted { context_id, block } = event else {
            return false;
        };
        let Some(text) = tail_text(block) else {
            return false;
        };
        self.lines.insert(*context_id, (text, now_millis));
        true
    }

    pub fn last(&self, context_id: ContextId) -> Option<&str> {
        self.lines.get(&context_id).map(|(text, _)| text.as_str())
    }

    /// Whether `context_id`'s tail moved within `window_ms` of `now_millis`.
    pub fn chatter(&self, context_id: ContextId, now_millis: u64, window_ms: u64) -> bool {
        self.lines
            .get(&context_id)
            .is_some_and(|(_, at)| now_millis.saturating_sub(*at) <= window_ms)
    }
}

/// Head of the first non-empty line, truncated. `None` for nothing visible.
fn head_line(content: &str) -> Option<String> {
    let line = content.lines().find(|l| !l.trim().is_empty())?;
    Some(truncate_chars(line.trim(), TAIL_CHARS))
}

/// Map one inserted block to a tail line, or `None` for a block with no
/// glanceable signal — the same kind split as the app's `live::tail_line`,
/// minus its streaming-placeholder resolve (this buffer keeps one line, not
/// eight, so a "⋯ composing" placeholder would just sit stale).
fn tail_text(block: &BlockSnapshot) -> Option<String> {
    match block.kind {
        BlockKind::Text => {
            let head = head_line(&block.content)?;
            let glyph = if block.role == Role::User { "❯" } else { "✦" };
            Some(format!("{glyph} {head}"))
        }
        BlockKind::ToolCall => {
            let name = block.tool_name.as_deref().unwrap_or("tool");
            Some(format!("▸ {name}"))
        }
        BlockKind::ToolResult => {
            let head = head_line(&block.content)?;
            let glyph = if block.is_error { "✕" } else { "◂" };
            Some(format!("{glyph} {head}"))
        }
        BlockKind::Error => {
            Some(format!("✕ {}", head_line(&block.content).unwrap_or_else(|| "error".to_string())))
        }
        BlockKind::Notification => Some(format!("◆ {}", head_line(&block.content)?)),
        _ => None,
    }
}

// ============================================================================
// BEAT TIMER — pure re-arm arithmetic (`docs/tui.md`, "Timing to music")
// ============================================================================

/// The phasor's predicted next onset, from its current unbounded beat
/// `position` and `tempo_bps` (beats per second) at `now`. Arm a redraw here,
/// never at a fixed tick — `docs/midi.md`, "The one timebase".
pub fn next_onset(position: f64, tempo_bps: f64, now: Instant) -> Instant {
    if tempo_bps <= 0.0 {
        return now;
    }
    let beats_to_next = (position.floor() + 1.0 - position).max(0.0);
    now + Duration::from_secs_f64(beats_to_next / tempo_bps)
}

/// Re-arm rule: the next onset is `scheduled + period`, never
/// `actual_wake + period` — a late wake must not push every following onset
/// later, and a missed onset is missed, never replayed.
pub fn rearm(scheduled: Instant, tempo_bps: f64) -> Instant {
    if tempo_bps <= 0.0 {
        return scheduled;
    }
    scheduled + Duration::from_secs_f64(1.0 / tempo_bps)
}

// ============================================================================
// MODEL
// ============================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Section {
    Active,
    Recent,
    Tracks,
}

impl Section {
    fn next(self) -> Self {
        match self {
            Section::Active => Section::Recent,
            Section::Recent => Section::Tracks,
            Section::Tracks => Section::Active,
        }
    }

    fn prev(self) -> Self {
        match self {
            Section::Active => Section::Tracks,
            Section::Recent => Section::Active,
            Section::Tracks => Section::Recent,
        }
    }
}

/// What a keystroke asks the caller to do. The picker never touches the
/// kernel itself — every effect crosses back out through this enum.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    None,
    /// `Esc` from the top level — dismiss the picker.
    Dismiss,
    /// Attach to this context and dismiss.
    Switch(ContextId),
    /// Run a placement verb: `execute_kj(context_id, argv)` — against the
    /// picker's TARGET context (the row acted on), not the attached
    /// context, because a `kj` run authors a tool-call pair into the
    /// context it runs in and this must land on the row's own transcript,
    /// not bleed into whatever the picker was opened from.
    Placement { context_id: ContextId, argv: Vec<String> },
}

pub struct PickerModel {
    pub active: Vec<Row>,
    pub recent: Vec<Row>,
    /// Full row detail for every horizoned context, built eagerly at
    /// [`Self::build`] time (`h` just toggles whether this renders — there is
    /// no async round trip on dive).
    pub horizon: Vec<Row>,
    pub tracks: Vec<TrackRow>,
    pub filter: String,
    pub section: Section,
    pub index: usize,
    pub horizon_open: bool,
    filter_editing: bool,
    /// A placement verb pressed once on this exact (verb, context) pair,
    /// waiting for a repeat to confirm a latched op (`kj context archive`).
    /// A second press of the SAME verb on the SAME row appends `--confirm`;
    /// any other key clears it — see [`Self::handle_key`].
    pending_confirm: Option<(char, ContextId)>,
}

impl PickerModel {
    /// Build a fresh picker from the state a caller already has: the last
    /// `list_contexts` / `listTracks` answers, which contexts carry the `@`
    /// activity flag (the same set the status line's rank reads), the tail
    /// buffer, and the clock.
    pub fn build(
        contexts: &[ContextInfo],
        tracks: &[TrackInfo],
        activity: &std::collections::HashSet<ContextId>,
        tails: &PickerTails,
        now_millis: u64,
    ) -> Self {
        let live: Vec<&ContextInfo> = contexts.iter().filter(|c| !c.archived).collect();
        let by_id: HashMap<ContextId, &ContextInfo> = live.iter().map(|c| (c.id, *c)).collect();
        let lifecycles: Vec<ContextLifecycle<ContextId>> = live
            .iter()
            .map(|c| ContextLifecycle {
                id: c.id,
                created_at: c.created_at as i64,
                concluded_at: c.concluded_at.map(|t| t as i64),
                last_activity_at: c.last_activity_at.unwrap_or(c.created_at) as i64,
                promoted_at: c.promoted_at.map(|t| t as i64),
                demoted_at: c.demoted_at.map(|t| t as i64),
            })
            .collect();
        let placement = assign_ring_seats(&lifecycles);

        let row_of = |id: &ContextId, digit: Option<usize>| -> Option<Row> {
            let info = *by_id.get(id)?;
            Some(row_from(info, digit, activity.contains(id), tails, now_millis))
        };

        let active: Vec<Row> = placement.rings[Band::Active.index()]
            .iter()
            .enumerate()
            .filter_map(|(i, id)| row_of(id, Some(i)))
            .collect();
        let recent: Vec<Row> = placement.rings[Band::Recent.index()]
            .iter()
            .filter_map(|id| row_of(id, None))
            .collect();
        let horizon: Vec<Row> = placement.horizon.iter().filter_map(|id| row_of(id, None)).collect();

        let tracks: Vec<TrackRow> = tracks.iter().map(track_row_from).collect();

        Self {
            active,
            recent,
            horizon,
            tracks,
            filter: String::new(),
            section: Section::Active,
            index: 0,
            horizon_open: false,
            filter_editing: false,
            pending_confirm: None,
        }
    }

    fn matches(&self, label: &str) -> bool {
        self.filter.is_empty() || label.to_lowercase().contains(&self.filter.to_lowercase())
    }

    pub fn visible_active(&self) -> Vec<&Row> {
        self.active.iter().filter(|r| self.matches(&r.label)).collect()
    }

    pub fn visible_recent(&self) -> Vec<&Row> {
        self.recent.iter().filter(|r| self.matches(&r.label)).collect()
    }

    pub fn visible_horizon(&self) -> Vec<&Row> {
        self.horizon.iter().filter(|r| self.matches(&r.label)).collect()
    }

    fn current_len(&self) -> usize {
        if self.horizon_open {
            return self.visible_horizon().len();
        }
        match self.section {
            Section::Active => self.visible_active().len(),
            Section::Recent => self.visible_recent().len(),
            Section::Tracks => self.tracks.len(),
        }
    }

    fn move_index(&mut self, delta: i32) {
        let len = self.current_len();
        if len == 0 {
            self.index = 0;
            return;
        }
        self.index = (self.index as i32 + delta).rem_euclid(len as i32) as usize;
    }

    fn activate_selection(&self) -> Outcome {
        if self.horizon_open {
            return self
                .visible_horizon()
                .get(self.index)
                .map(|r| Outcome::Switch(r.context_id))
                .unwrap_or(Outcome::None);
        }
        match self.section {
            Section::Active => self.visible_active().get(self.index).map(|r| Outcome::Switch(r.context_id)),
            Section::Recent => self.visible_recent().get(self.index).map(|r| Outcome::Switch(r.context_id)),
            Section::Tracks => self.tracks.get(self.index).map(|t| Outcome::Switch(t.score_context_id)),
        }
        .unwrap_or(Outcome::None)
    }

    /// `p`/`d`/`z`/`a`/`c` on the selected row — `docs/input.md`'s placement
    /// verbs, mapped to `kj context <verb>` (`kj/context.rs`'s
    /// `ContextCommand`): `p` promote, `d` demote, `z` pause/resume
    /// (whichever the row's current `paused` flag calls for), `a` archive
    /// (latched — a repeat confirms), `c` conclude.
    fn placement(&mut self, verb: char) -> Outcome {
        let selected = match self.section {
            Section::Active => self.visible_active().get(self.index).map(|r| (r.context_id, r.paused)),
            Section::Recent => self.visible_recent().get(self.index).map(|r| (r.context_id, r.paused)),
            Section::Tracks => None,
        };
        let Some((target, paused)) = selected else {
            return Outcome::None;
        };
        let confirm = self.pending_confirm == Some((verb, target));
        let sub = match verb {
            'p' => "promote",
            'd' => "demote",
            'c' => "conclude",
            'a' => "archive",
            'z' if paused => "resume",
            'z' => "pause",
            _ => return Outcome::None,
        };
        let mut argv = vec!["context".to_string(), sub.to_string(), target.to_hex()];
        if confirm {
            argv.push("--confirm".to_string());
            self.pending_confirm = None;
        } else {
            self.pending_confirm = Some((verb, target));
        }
        Outcome::Placement { context_id: target, argv }
    }

    /// Interpret one key event. Filter-editing and horizon-diving are
    /// distinct modes layered over the same row/selection state; the top
    /// level below is the ordinary navigation surface the figure shows.
    pub fn handle_key(&mut self, key: KeyEvent) -> Outcome {
        if key.kind == KeyEventKind::Release {
            return Outcome::None;
        }

        if self.filter_editing {
            match key.code {
                KeyCode::Esc => {
                    self.filter.clear();
                    self.filter_editing = false;
                    self.index = 0;
                }
                KeyCode::Enter => self.filter_editing = false,
                KeyCode::Backspace => {
                    self.filter.pop();
                    self.index = 0;
                }
                KeyCode::Char(c) => {
                    self.filter.push(c);
                    self.index = 0;
                }
                _ => {}
            }
            return Outcome::None;
        }

        if self.horizon_open {
            return match key.code {
                KeyCode::Esc => {
                    self.horizon_open = false;
                    self.filter.clear();
                    self.index = 0;
                    Outcome::None
                }
                KeyCode::Char('/') => {
                    self.filter_editing = true;
                    self.filter.clear();
                    Outcome::None
                }
                KeyCode::Char('j') | KeyCode::Down => {
                    self.move_index(1);
                    Outcome::None
                }
                KeyCode::Char('k') | KeyCode::Up => {
                    self.move_index(-1);
                    Outcome::None
                }
                KeyCode::Enter => self.activate_selection(),
                _ => Outcome::None,
            };
        }

        match key.code {
            KeyCode::Esc => Outcome::Dismiss,
            KeyCode::Char('/') => {
                self.filter_editing = true;
                self.filter.clear();
                self.pending_confirm = None;
                Outcome::None
            }
            KeyCode::Char('h') => {
                self.horizon_open = true;
                self.index = 0;
                self.pending_confirm = None;
                Outcome::None
            }
            KeyCode::Tab => {
                self.section = self.section.next();
                self.index = 0;
                self.pending_confirm = None;
                Outcome::None
            }
            KeyCode::BackTab => {
                self.section = self.section.prev();
                self.index = 0;
                self.pending_confirm = None;
                Outcome::None
            }
            KeyCode::Char('j') | KeyCode::Down => {
                self.move_index(1);
                self.pending_confirm = None;
                Outcome::None
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.move_index(-1);
                self.pending_confirm = None;
                Outcome::None
            }
            KeyCode::Char(c) if c.is_ascii_digit() => {
                let n = c as usize - '0' as usize;
                self.active
                    .iter()
                    .find(|r| r.digit == Some(n))
                    .map(|r| Outcome::Switch(r.context_id))
                    .unwrap_or(Outcome::None)
            }
            KeyCode::Enter => self.activate_selection(),
            KeyCode::Char(v @ ('p' | 'd' | 'z' | 'a' | 'c')) => self.placement(v),
            _ => Outcome::None,
        }
    }
}

// ============================================================================
// RENDER — pure mapper, `docs/tui.md` "Nothing you would paste is inside a box"
// ============================================================================

fn display_width(s: &str) -> usize {
    s.chars().map(|c| c.width().unwrap_or(0)).sum()
}

fn truncate_chars(s: &str, width: usize) -> String {
    if display_width(s) <= width {
        return s.to_string();
    }
    if width == 0 {
        return String::new();
    }
    let mut out = String::new();
    let mut w = 0usize;
    for ch in s.chars() {
        let cw = ch.width().unwrap_or(0);
        if w + cw > width.saturating_sub(1) {
            break;
        }
        out.push(ch);
        w += cw;
    }
    out.push('…');
    out
}

fn pad(s: &str, width: usize) -> String {
    let w = display_width(s);
    if w >= width {
        truncate_chars(s, width)
    } else {
        format!("{s}{}", " ".repeat(width - w))
    }
}

/// `4s`, `2m`, `41m`, `3h`, `1d` — the picker's compact idle-age column.
fn format_age(age_ms: u64) -> String {
    let secs = age_ms / 1000;
    let (d, h, m, s) = (secs / 86_400, (secs % 86_400) / 3_600, (secs % 3_600) / 60, secs % 60);
    if d > 0 {
        format!("{d}d")
    } else if h > 0 {
        format!("{h}h")
    } else if m > 0 {
        format!("{m}m")
    } else {
        format!("{s}s")
    }
}

/// A 4-dot pulse glyph — `●○○○` at beat 1, `○●○○` at beat 2 — the beat made
/// visible for one track (`docs/tui.md`, "TRACKS + beat").
fn pulse_glyph(beat: u64, beats_per_bar: u64) -> String {
    let lit = (beat.saturating_sub(1)) % beats_per_bar.max(1);
    (0..beats_per_bar).map(|i| if i == lit { '●' } else { '○' }).collect()
}

const KEY_LINE_FULL: &str = "j/k move  Tab section  Enter switch  0-9 seat  p promote  d demote  z pause  a archive  c conclude  / filter  h horizon  Esc dismiss";
const KEY_LINE_SHORT: &str = "j/k move  Enter switch  / filter  h horizon  Esc dismiss";

fn key_line(width: usize, palette: &Palette) -> Line<'static> {
    let text = if display_width(KEY_LINE_FULL) <= width { KEY_LINE_FULL } else { KEY_LINE_SHORT };
    Line::from(Span::styled(text.to_string(), palette.divider()))
}

fn row_line(row: &Row, selected: bool, label_w: usize, ctype_w: usize, width: usize, palette: &Palette) -> Line<'static> {
    let marker = if selected { "› " } else { "  " };
    let digit = row.digit.map(|d| d.to_string()).unwrap_or_else(|| " ".to_string());
    let flag = if row.chatter { "●" } else if row.activity { "@" } else { " " };
    let state = if row.errored { "error" } else if row.running { "running" } else { "idle" };
    let head = format!(
        "{marker}{digit} {} {flag} {} {:<7} {:>3}  ",
        pad(&row.label, label_w),
        pad(&row.context_type, ctype_w),
        state,
        format_age(row.age_ms),
    );
    let remaining = width.saturating_sub(display_width(&head));
    let tail = truncate_chars(row.tail.as_deref().unwrap_or(""), remaining);
    let style = if row.errored { palette.alarm() } else { palette.status() };
    Line::from(Span::styled(format!("{head}{tail}"), style))
}

fn horizon_row_line(row: &Row, selected: bool, label_w: usize, width: usize, palette: &Palette) -> Line<'static> {
    let marker = if selected { "› " } else { "  " };
    let text = format!("{marker}{} {}  {}", pad(&row.label, label_w), pad(&row.context_type, 10), format_age(row.age_ms));
    Line::from(Span::styled(truncate_chars(&text, width), palette.status()))
}

fn track_row_line(row: &TrackRow, selected: bool, id_w: usize, width: usize, palette: &Palette) -> Line<'static> {
    let marker = if selected { "› " } else { "  " };
    let glyph = if row.playing { "▮" } else { "▯" };
    let text = format!(
        "{marker}{glyph} {} {:>3} bpm  {}.{}  {}",
        pad(&row.id, id_w),
        row.bpm,
        row.bar,
        row.beat,
        pulse_glyph(row.beat, BEATS_PER_BAR),
    );
    Line::from(Span::styled(truncate_chars(&text, width), palette.status()))
}

fn label_width(rows: &[&Row]) -> usize {
    rows.iter().map(|r| display_width(&r.label)).max().unwrap_or(0).clamp(8, 20)
}

fn ctype_width(rows: &[&Row]) -> usize {
    rows.iter().map(|r| display_width(&r.context_type)).max().unwrap_or(0).clamp(6, 10)
}

/// The picker's lines: the header, ACTIVE/RECENT/TRACKS or the horizon list,
/// and the key line every grown view ends with.
pub fn render(model: &PickerModel, width: u16, palette: &Palette) -> Vec<Line<'static>> {
    let width = usize::from(width.max(1));
    let mut lines = Vec::new();

    if model.filter_editing {
        lines.push(Line::from(Span::styled(format!("/ {}_", model.filter), palette.compose())));
    }

    if model.horizon_open {
        let rows = model.visible_horizon();
        lines.push(Line::from(Span::styled(
            format!("HORIZON  {} of {}", rows.len(), model.horizon.len()),
            palette.divider(),
        )));
        let label_w = label_width(&rows);
        for (i, row) in rows.iter().enumerate() {
            lines.push(horizon_row_line(row, i == model.index, label_w, width, palette));
        }
        lines.push(key_line(width, palette));
        return lines;
    }

    let active = model.visible_active();
    lines.push(Line::from(Span::styled("ACTIVE", palette.divider())));
    let label_w = label_width(&active.iter().chain(model.visible_recent().iter()).copied().collect::<Vec<_>>());
    let ctype_w = ctype_width(&active.iter().chain(model.visible_recent().iter()).copied().collect::<Vec<_>>());
    for (i, row) in active.iter().enumerate() {
        let selected = model.section == Section::Active && i == model.index;
        lines.push(row_line(row, selected, label_w, ctype_w, width, palette));
    }

    let recent = model.visible_recent();
    lines.push(Line::from(Span::styled("RECENT", palette.divider())));
    for (i, row) in recent.iter().enumerate() {
        let selected = model.section == Section::Recent && i == model.index;
        lines.push(row_line(row, selected, label_w, ctype_w, width, palette));
    }

    let horizon_count = model.horizon.len();
    if horizon_count > 0 {
        lines.push(Line::from(Span::styled(
            format!("+{horizon_count} beyond the horizon"),
            palette.divider(),
        )));
    }

    lines.push(Line::from(Span::styled("TRACKS", palette.divider())));
    let id_w = model.tracks.iter().map(|t| display_width(&t.id)).max().unwrap_or(0).clamp(6, 16);
    for (i, track) in model.tracks.iter().enumerate() {
        let selected = model.section == Section::Tracks && i == model.index;
        lines.push(track_row_line(track, selected, id_w, width, palette));
    }

    lines.push(key_line(width, palette));
    lines
}

/// Rows the picker's grown viewport needs to hold the whole current view —
/// what `render::viewport_lines` asks for when this picker is open.
pub fn viewport_lines(model: &PickerModel) -> u16 {
    let body = render(model, u16::MAX, &Palette::builtin()).len();
    u16::try_from(body).unwrap_or(u16::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyModifiers;
    use kaijutsu_types::{BlockId, BlockSnapshotBuilder, PrincipalId};

    fn press(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn ctx(label: &str) -> ContextInfo {
        ContextInfo {
            id: ContextId::new(),
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
            last_activity_at: Some(1_000),
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

    fn empty_tails() -> PickerTails {
        PickerTails::new()
    }

    fn no_activity() -> std::collections::HashSet<ContextId> {
        std::collections::HashSet::new()
    }

    #[test]
    fn active_and_recent_split_matches_promotion_and_horizon_counts_the_rest() {
        let mut promoted = ctx("kaijutsu");
        promoted.promoted_at = Some(1_000);
        let auto = ctx("kaish");
        let mut overflow: Vec<ContextInfo> = (0..11)
            .map(|i| {
                let mut c = ctx(&format!("auto-{i}"));
                c.last_activity_at = Some(2_000 + i as u64);
                c
            })
            .collect();
        let contexts: Vec<ContextInfo> =
            std::iter::once(promoted.clone()).chain(std::iter::once(auto.clone())).chain(overflow.drain(..)).collect();

        let model = PickerModel::build(&contexts, &[], &no_activity(), &empty_tails(), 5_000);
        assert_eq!(model.active.len(), 1);
        assert_eq!(model.active[0].context_id, promoted.id);
        assert_eq!(model.active[0].digit, Some(0));
        assert_eq!(model.recent.len(), 10, "RECENT caps at 10 seats");
        assert_eq!(model.horizon.len(), 2, "auto (11+1) minus RECENT's 10 seats spills 2 to the horizon");
    }

    #[test]
    fn a_never_promoted_row_carries_no_digit() {
        let a = ctx("kaish");
        let model = PickerModel::build(std::slice::from_ref(&a), &[], &no_activity(), &empty_tails(), 0);
        assert_eq!(model.recent[0].digit, None);
    }

    #[test]
    fn the_filter_narrows_visible_rows_without_touching_digit_addressing() {
        let mut a = ctx("kaijutsu");
        a.promoted_at = Some(1_000);
        let mut b = ctx("kaish");
        b.promoted_at = Some(2_000);
        let contexts = vec![a.clone(), b.clone()];
        let mut model = PickerModel::build(&contexts, &[], &no_activity(), &empty_tails(), 0);

        model.filter = "kai".to_string();
        assert_eq!(model.visible_active().len(), 2, "both labels contain 'kai'");
        model.filter = "kaish".to_string();
        let visible = model.visible_active();
        assert_eq!(visible.len(), 1);
        assert_eq!(visible[0].context_id, b.id);

        // A digit still resolves the original seat even though the filter
        // would hide it — digits address seats, not the filtered view.
        assert_eq!(model.handle_key(press(KeyCode::Char('0'))), Outcome::Switch(a.id));
    }

    #[test]
    fn tab_hops_sections_and_resets_the_index() {
        let mut model = PickerModel::build(&[], &[], &no_activity(), &empty_tails(), 0);
        model.index = 3;
        assert_eq!(model.section, Section::Active);
        model.handle_key(press(KeyCode::Tab));
        assert_eq!(model.section, Section::Recent);
        assert_eq!(model.index, 0);
        model.handle_key(press(KeyCode::Tab));
        assert_eq!(model.section, Section::Tracks);
        model.handle_key(press(KeyCode::Tab));
        assert_eq!(model.section, Section::Active);
    }

    #[test]
    fn j_and_k_wrap_within_the_current_section() {
        let mut a = ctx("a");
        a.promoted_at = Some(1_000);
        let mut b = ctx("b");
        b.promoted_at = Some(2_000);
        let mut model = PickerModel::build(&[a, b], &[], &no_activity(), &empty_tails(), 0);
        assert_eq!(model.index, 0);
        model.handle_key(press(KeyCode::Char('k')));
        assert_eq!(model.index, 1, "k from the top wraps to the bottom");
        model.handle_key(press(KeyCode::Char('j')));
        assert_eq!(model.index, 0);
    }

    #[test]
    fn esc_dismisses_at_the_top_level() {
        let mut model = PickerModel::build(&[], &[], &no_activity(), &empty_tails(), 0);
        assert_eq!(model.handle_key(press(KeyCode::Esc)), Outcome::Dismiss);
    }

    #[test]
    fn slash_enters_filter_editing_and_esc_there_only_clears_the_filter() {
        let mut model = PickerModel::build(&[], &[], &no_activity(), &empty_tails(), 0);
        model.handle_key(press(KeyCode::Char('/')));
        model.handle_key(press(KeyCode::Char('x')));
        assert_eq!(model.filter, "x");
        assert_eq!(model.handle_key(press(KeyCode::Esc)), Outcome::None);
        assert_eq!(model.filter, "", "filter-editing Esc clears rather than dismissing");
    }

    #[test]
    fn h_opens_the_horizon_as_a_filtered_list() {
        let mut c = ctx("gone");
        c.demoted_at = Some(9_000);
        let mut model = PickerModel::build(&[c.clone()], &[], &no_activity(), &empty_tails(), 0);
        assert_eq!(model.horizon.len(), 1);
        model.handle_key(press(KeyCode::Char('h')));
        assert!(model.horizon_open);
        assert_eq!(model.handle_key(press(KeyCode::Enter)), Outcome::Switch(c.id));
    }

    #[test]
    fn promote_targets_the_row_not_the_attached_context() {
        let mut a = ctx("kaijutsu");
        a.promoted_at = None;
        let mut model = PickerModel::build(std::slice::from_ref(&a), &[], &no_activity(), &empty_tails(), 0);
        // A never-promoted context lives in RECENT, not ACTIVE — hop there
        // before pressing the verb.
        model.handle_key(press(KeyCode::Tab));
        let outcome = model.handle_key(press(KeyCode::Char('p')));
        assert_eq!(
            outcome,
            Outcome::Placement { context_id: a.id, argv: vec!["context".into(), "promote".into(), a.id.to_hex()] }
        );
    }

    #[test]
    fn a_second_press_of_the_same_verb_on_the_same_row_confirms() {
        let mut a = ctx("kaijutsu");
        a.promoted_at = Some(1_000);
        let mut model = PickerModel::build(std::slice::from_ref(&a), &[], &no_activity(), &empty_tails(), 0);
        let first = model.handle_key(press(KeyCode::Char('a')));
        assert_eq!(first, Outcome::Placement { context_id: a.id, argv: vec!["context".into(), "archive".into(), a.id.to_hex()] });
        let second = model.handle_key(press(KeyCode::Char('a')));
        assert_eq!(
            second,
            Outcome::Placement {
                context_id: a.id,
                argv: vec!["context".into(), "archive".into(), a.id.to_hex(), "--confirm".into()]
            }
        );
    }

    #[test]
    fn moving_the_selection_clears_a_pending_confirm() {
        let mut a = ctx("kaijutsu");
        a.promoted_at = Some(1_000);
        let mut b = ctx("kaish");
        b.promoted_at = Some(2_000);
        let mut model = PickerModel::build(&[a.clone(), b.clone()], &[], &no_activity(), &empty_tails(), 0);
        model.handle_key(press(KeyCode::Char('d'))); // pending confirm on a (index 0)
        model.handle_key(press(KeyCode::Char('j'))); // moves to b (index 1), clears it
        let outcome = model.handle_key(press(KeyCode::Char('d')));
        assert_eq!(
            outcome,
            Outcome::Placement { context_id: b.id, argv: vec!["context".into(), "demote".into(), b.id.to_hex()] },
            "a fresh first press on the NEW selection never carries --confirm"
        );
    }

    #[test]
    fn z_toggles_pause_by_the_rows_own_flag() {
        let mut a = ctx("kaijutsu");
        a.promoted_at = Some(1_000);
        a.paused_at = Some(1_000);
        let mut model = PickerModel::build(std::slice::from_ref(&a), &[], &no_activity(), &empty_tails(), 0);
        let outcome = model.handle_key(press(KeyCode::Char('z')));
        assert_eq!(
            outcome,
            Outcome::Placement { context_id: a.id, argv: vec!["context".into(), "resume".into(), a.id.to_hex()] },
            "an already-paused row resumes"
        );
    }

    fn block(kind: BlockKind, role: Role, content: &str) -> BlockSnapshot {
        let id = BlockId::new(ContextId::new(), PrincipalId::new(), 1);
        BlockSnapshotBuilder::new(id, kind).role(role).content(content).build()
    }

    #[test]
    fn the_tail_buffer_keeps_only_the_newest_line() {
        let mut tails = PickerTails::new();
        let ctx_id = ContextId::new();
        let b1 = block(BlockKind::Text, Role::User, "first");
        let b2 = block(BlockKind::ToolCall, Role::Model, "second");
        tails.observe(&ServerEvent::BlockInserted { context_id: ctx_id, block: Box::new(b1) }, 0);
        assert_eq!(tails.last(ctx_id), Some("❯ first"));
        tails.observe(&ServerEvent::BlockInserted { context_id: ctx_id, block: Box::new(b2) }, 100);
        assert!(tails.last(ctx_id).unwrap().starts_with('▸'), "got {:?}", tails.last(ctx_id));
    }

    #[test]
    fn chatter_expires_after_the_window_but_the_line_survives() {
        let mut tails = PickerTails::new();
        let ctx_id = ContextId::new();
        tails.observe(&ServerEvent::BlockInserted { context_id: ctx_id, block: Box::new(block(BlockKind::Text, Role::User, "hi")) }, 1_000);
        assert!(tails.chatter(ctx_id, 1_000, CHATTER_WINDOW_MS));
        assert!(tails.chatter(ctx_id, 1_000 + CHATTER_WINDOW_MS, CHATTER_WINDOW_MS));
        assert!(!tails.chatter(ctx_id, 1_000 + CHATTER_WINDOW_MS + 1, CHATTER_WINDOW_MS));
        assert_eq!(tails.last(ctx_id), Some("❯ hi"), "the line itself never expires");
    }

    #[test]
    fn bar_beat_derives_from_the_tick_and_beats_per_bar() {
        assert_eq!(bar_beat(0, 4), (1, 1));
        assert_eq!(bar_beat(66, 4), (17, 3));
        assert_eq!(bar_beat(-5, 4), (1, 1), "a negative tick clamps rather than panicking");
    }

    #[test]
    fn the_pulse_glyph_lights_the_current_beats_dot() {
        assert_eq!(pulse_glyph(1, 4), "●○○○");
        assert_eq!(pulse_glyph(3, 4), "○○●○");
    }

    #[test]
    fn next_onset_is_the_fractional_beat_away_scaled_by_tempo() {
        let t0 = Instant::now();
        // 2 beats/sec (500ms/beat), halfway through the current beat: the
        // next onset is 0.5 beats away = 250ms.
        let onset = next_onset(0.5, 2.0, t0);
        assert_eq!(onset.duration_since(t0), Duration::from_millis(250));
    }

    #[test]
    fn next_onset_from_exactly_on_the_beat_is_a_full_period_away() {
        let t0 = Instant::now();
        let onset = next_onset(4.0, 2.0, t0);
        assert_eq!(onset.duration_since(t0), Duration::from_millis(500));
    }

    #[test]
    fn rearm_advances_from_the_scheduled_instant_never_the_wake_instant() {
        let t0 = Instant::now();
        let scheduled = t0 + Duration::from_millis(250);
        // The caller "wakes late" at scheduled + 80ms of jitter; rearm must
        // still land at scheduled + period, not late_wake + period.
        let late_wake = scheduled + Duration::from_millis(80);
        let next = rearm(scheduled, 2.0);
        assert_eq!(next, scheduled + Duration::from_millis(500));
        assert!(next < late_wake + Duration::from_millis(500), "sanity: rearm did not anchor on the late wake");
    }

    #[test]
    fn viewport_lines_grows_with_the_row_count() {
        let empty = PickerModel::build(&[], &[], &no_activity(), &empty_tails(), 0);
        let mut a = ctx("kaijutsu");
        a.promoted_at = Some(1_000);
        let one = PickerModel::build(&[a], &[], &no_activity(), &empty_tails(), 0);
        assert!(viewport_lines(&one) > viewport_lines(&empty));
    }

    #[test]
    fn render_has_no_border_glyphs_and_ends_with_the_key_line() {
        let mut a = ctx("kaijutsu");
        a.promoted_at = Some(1_000);
        let model = PickerModel::build(&[a], &[], &no_activity(), &empty_tails(), 0);
        let lines = render(&model, 96, &Palette::builtin());
        for line in &lines {
            let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
            for glyph in ['│', '╭', '╮', '╰', '╯', '┌', '└'] {
                assert!(!text.contains(glyph), "{text:?} carries {glyph}");
            }
        }
        let last: String = lines.last().unwrap().spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(last.starts_with("j/k move"), "got {last:?}");
    }

    #[test]
    fn a_test_backend_render_shows_flush_left_active_rows() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        use ratatui::widgets::Paragraph;

        let mut a = ctx("kaijutsu");
        a.promoted_at = Some(1_000);
        a.live_status = Status::Running;
        let model = PickerModel::build(&[a.clone()], &[], &no_activity(), &empty_tails(), 1_004_000);
        let lines = render(&model, 60, &Palette::builtin());
        let mut terminal = Terminal::new(TestBackend::new(60, u16::try_from(lines.len()).unwrap())).unwrap();
        terminal.draw(|f| f.render_widget(Paragraph::new(lines), f.area())).unwrap();
        let buf = terminal.backend().buffer();
        let row1: String = (0..buf.area.width).map(|x| buf[(x, 1)].symbol()).collect();
        assert!(row1.starts_with("› 0 kaijutsu"), "got {row1:?}");
        assert!(row1.trim_end().contains("running"), "got {row1:?}");
    }
}
