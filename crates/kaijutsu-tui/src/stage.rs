//! Full-screen Live Music / Stage surface (`Ctrl+A m`).
//!
//! Layout (top-to-bottom, nothing scrolls the pinned bars):
//! - Row 0: pinned **Transport Bar** (`▶/⏸ <track> · <bpm> BPM · bar.beat · ▪▪●▫ · <clock>`)
//! - Middle: **Track & Chair Roster** (left) + **Piano Roll / Engraved Score** (right)
//! - Bottom row: key hints (or `Ctrl+A` legend while armed)
//!
//! The piano roll uses pure Unicode (`█`/`▬`/`│`/`┆`) so the live playhead
//! sweeps smoothly at 12.5 fps over SSH with zero Kitty image re-uploads and
//! works identically in `screen` and `vt100` tests. Pressing `Tab` switches
//! the main pane to the track's engraved sheet music via our cached Kitty
//! `U=1` pipeline (`score.rs` / `kitty.rs`).

use std::collections::HashMap;
use std::time::Instant;

use crossterm::event::{KeyCode, KeyEvent};
use kaijutsu_abc::MidiParams;
use kaijutsu_abc::ast::Tune;
use kaijutsu_abc::midi::events as midi_events;
use kaijutsu_client::TrackInfo;
use kaijutsu_types::{BlockSnapshot, ContextId};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::app::App;
use crate::kitty::{CellPixels, placeholder_lines};
use crate::present::Palette;
use crate::score::{
    EngravedSystem, engrave_measured_systems, notation_color, openable_abc, parse_first_tune,
};

/// Default beats per bar (matches `picker::BEATS_PER_BAR`).
const BEATS_PER_BAR: u64 = 4;

/// Maximum rows per engraved system in the Stage score pane.
const STAGE_MAX_ROWS_PER_SYSTEM: u16 = 14;

/// Which visualization the main Stage pane shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum StageViewMode {
    #[default]
    PianoRoll,
    Score,
}

/// One timed note extracted from `kaijutsu_abc::midi::events`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct NoteSpan {
    pub start_beat: f64,
    pub end_beat: f64,
    pub pitch: u8,
    pub channel: u8,
}

/// Pure piano-roll model extracted from an ABC [`Tune`].
#[derive(Debug, Clone, PartialEq)]
pub struct PianoRoll {
    pub notes: Vec<NoteSpan>,
    pub min_pitch: u8,
    pub max_pitch: u8,
    pub total_beats: f64,
}

/// Extract timed [`NoteSpan`]s from a parsed ABC tune via `kaijutsu_abc::midi::events`.
pub fn extract_piano_roll(tune: &Tune) -> PianoRoll {
    let params = MidiParams::default();
    let tpb = f64::from(params.ticks_per_beat.max(1));
    let events = midi_events(tune, &params);

    let mut active: HashMap<(u8, u8), f64> = HashMap::new();
    let mut notes = Vec::new();
    let mut max_beat = 0.0_f64;

    for ev in events {
        let beat = f64::from(ev.tick) / tpb;
        if beat > max_beat {
            max_beat = beat;
        }
        if ev.data.len() < 3 {
            continue;
        }
        let status = ev.data[0];
        let kind = status & 0xF0;
        let ch = status & 0x0F;
        let pitch = ev.data[1];
        let vel = ev.data[2];

        if kind == 0x90 && vel > 0 {
            if let Some(prev_start) = active.insert((ch, pitch), beat)
                && beat > prev_start
            {
                notes.push(NoteSpan {
                    start_beat: prev_start,
                    end_beat: beat,
                    pitch,
                    channel: ch,
                });
            }
        } else if (kind == 0x80 || (kind == 0x90 && vel == 0))
            && let Some(start) = active.remove(&(ch, pitch))
        {
            let end = beat.max(start + 0.125);
            notes.push(NoteSpan {
                start_beat: start,
                end_beat: end,
                pitch,
                channel: ch,
            });
        }
    }

    // Close any dangling NoteOn at the end of the tune.
    let end_beat = max_beat.max(4.0);
    for ((ch, pitch), start) in active {
        notes.push(NoteSpan {
            start_beat: start,
            end_beat: end_beat.max(start + 0.25),
            pitch,
            channel: ch,
        });
    }

    let (min_pitch, max_pitch) = if notes.is_empty() {
        (60, 72)
    } else {
        let mut lo = u8::MAX;
        let mut hi = 0u8;
        for n in &notes {
            lo = lo.min(n.pitch);
            hi = hi.max(n.pitch);
        }
        // Pad narrow ranges so single-pitch or drum parts stay legible.
        let lo = lo.saturating_sub(2);
        let hi = hi.saturating_add(2).min(127);
        (lo, hi.max(lo + 4))
    };

    let total_beats = (max_beat.ceil()).max(4.0);
    PianoRoll {
        notes,
        min_pitch,
        max_pitch,
        total_beats,
    }
}

/// Format a MIDI pitch number as a short note name (`C4`, `F#5`, …).
pub fn midi_note_name(pitch: u8) -> String {
    const NAMES: [&str; 12] = [
        "C", "C#", "D", "D#", "E", "F", "F#", "G", "G#", "A", "A#", "B",
    ];
    let octave = (i16::from(pitch) / 12) - 1;
    let name = NAMES[usize::from(pitch % 12)];
    format!("{name}{octave}")
}

/// Render a 4-beat pulse indicator (`▪ ▪ ● ▫`) for the current beat position.
pub fn beat_pulse_strip(beat_pos: Option<f64>, playing: bool) -> String {
    if !playing {
        return "▫ ▫ ▫ ▫".to_string();
    }
    let Some(pos) = beat_pos else {
        return "○ ○ ○ ○".to_string();
    };
    let beat_in_bar = (pos.floor().max(0.0) as u64) % BEATS_PER_BAR;
    let frac = pos - pos.floor();
    let bright = frac < 0.35;
    let mut out = String::with_capacity(12);
    for b in 0..BEATS_PER_BAR {
        if b > 0 {
            out.push(' ');
        }
        if b == beat_in_bar {
            out.push(if bright { '●' } else { '◉' });
        } else if b < beat_in_bar {
            out.push('▪');
        } else {
            out.push('▫');
        }
    }
    out
}

/// Render a [`PianoRoll`] into `height` terminal lines of width `width`, with an
/// optional live playhead swept by `playhead_beat` (wrapped modulo `total_beats`).
pub fn render_piano_roll(
    roll: &PianoRoll,
    playhead_beat: Option<f64>,
    width: u16,
    height: u16,
    palette: &Palette,
) -> Vec<Line<'static>> {
    let width = usize::from(width);
    let height = usize::from(height);
    if width < 8 || height == 0 {
        return Vec::new();
    }
    let gutter_w = 4usize; // e.g. "C4 │"
    let grid_w = width.saturating_sub(gutter_w);
    if grid_w == 0 {
        return Vec::new();
    }

    let play_col = playhead_beat.map(|b| {
        let wrapped = b.rem_euclid(roll.total_beats.max(1.0));
        ((wrapped / roll.total_beats.max(1.0)) * grid_w as f64).floor() as usize
    });

    let pitch_span = usize::from(roll.max_pitch.saturating_sub(roll.min_pitch)).max(1);
    let mut out = Vec::with_capacity(height);

    // Reserve bottom row for bar numbers when height >= 4.
    let roll_rows = if height >= 4 { height - 1 } else { height };
    let beats_per_col = roll.total_beats / grid_w as f64;

    let channel_colors = [
        Color::Cyan,
        Color::LightGreen,
        Color::LightMagenta,
        Color::Yellow,
        Color::LightBlue,
    ];

    for row in 0..roll_rows {
        // Top row = max_pitch, bottom row = min_pitch.
        let pitch = if roll_rows <= 1 {
            roll.max_pitch
        } else {
            let frac = row as f64 / (roll_rows - 1) as f64;
            let p = f64::from(roll.max_pitch) - frac * pitch_span as f64;
            p.round().clamp(0.0, 127.0) as u8
        };
        let is_c = pitch.is_multiple_of(12);
        let show_label = row == 0 || row + 1 == roll_rows || is_c;
        let label = if show_label {
            format!("{:<3}│", midi_note_name(pitch))
        } else {
            "   │".to_string()
        };

        let mut spans = Vec::with_capacity(grid_w + 1);
        spans.push(Span::styled(
            label,
            if is_c { palette.score_heading() } else { palette.score_dim() },
        ));

        let pitch_lo = pitch.saturating_sub(if pitch_span > roll_rows { 1 } else { 0 });
        let pitch_hi = pitch.saturating_add(if pitch_span > roll_rows { 1 } else { 0 });

        for col in 0..grid_w {
            let b0 = col as f64 * beats_per_col;
            let b1 = (col + 1) as f64 * beats_per_col;
            let hit = roll.notes.iter().find(|n| {
                n.pitch >= pitch_lo && n.pitch <= pitch_hi && n.start_beat < b1 && n.end_beat > b0
            });
            let is_play = play_col == Some(col);
            let is_bar = col > 0 && ((b0 / BEATS_PER_BAR as f64).floor() != ((b0 - beats_per_col).max(0.0) / BEATS_PER_BAR as f64).floor());

            if let Some(note) = hit {
                let color = channel_colors[usize::from(note.channel) % channel_colors.len()];
                let style = if is_play {
                    Style::default().fg(Color::White).add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(color)
                };
                spans.push(Span::styled("█", style));
            } else if is_play {
                spans.push(Span::styled("│", Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)));
            } else if is_bar {
                spans.push(Span::styled("┆", palette.score_dim()));
            } else if is_c {
                spans.push(Span::styled("·", palette.score_dim()));
            } else {
                spans.push(Span::raw(" "));
            }
        }
        out.push(Line::from(spans));
    }

    if height >= 4 {
        let mut ruler = vec![' '; grid_w];
        let total_bars = (roll.total_beats / BEATS_PER_BAR as f64).ceil() as usize;
        for bar in 0..total_bars {
            let col = ((bar * BEATS_PER_BAR as usize) as f64 / roll.total_beats * grid_w as f64).round() as usize;
            let tag = format!("{}", bar + 1);
            for (i, ch) in tag.chars().enumerate() {
                if col + i < grid_w {
                    ruler[col + i] = ch;
                }
            }
        }
        let ruler_str: String = ruler.into_iter().collect();
        out.push(Line::from(vec![
            Span::styled("bar ", palette.score_dim()),
            Span::styled(ruler_str, palette.score_dim()),
        ]));
    }
    out
}

/// One selectable row in the Stage left-hand roster.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RosterRow {
    Track { track_idx: usize },
    Chair { track_idx: usize, context_id: ContextId },
}

/// Action requested by a keystroke in [`StageModel`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StageOutcome {
    None,
    Dismiss,
    TogglePlayPause { track_id: String, playing: bool },
    Stop { track_id: String },
    SetTempo { track_id: String, bpm: u64 },
    SwitchContext(ContextId),
    OpenFullScore { title: String, raw_abc: String },
}

/// Cached engraving for the active track's latest ABC score in Stage score mode.
struct StageScoreCache {
    raw_abc: String,
    width: u16,
    cell_px: CellPixels,
    fg: &'static str,
    systems: Vec<EngravedSystem>,
}

/// Full-screen Live Music / Stage surface state (`Ctrl+A m`).
#[derive(Default)]
pub struct StageModel {
    pub selected: usize,
    pub view_mode: StageViewMode,
    pub score_scroll: usize,
    score_cache: Option<StageScoreCache>,
}

impl StageModel {
    pub fn new() -> Self {
        Self::default()
    }

    /// Build the flat selectable roster of tracks and their attached chairs.
    pub fn roster(&self, tracks: &[TrackInfo]) -> Vec<RosterRow> {
        let mut rows = Vec::new();
        for (idx, t) in tracks.iter().enumerate() {
            rows.push(RosterRow::Track { track_idx: idx });
            for &ctx in &t.attached {
                if ctx != t.score_context_id {
                    rows.push(RosterRow::Chair {
                        track_idx: idx,
                        context_id: ctx,
                    });
                }
            }
        }
        rows
    }

    /// The currently selected [`TrackInfo`] (whether a track row or one of its
    /// attached chairs is highlighted).
    pub fn selected_track<'a>(&self, tracks: &'a [TrackInfo]) -> Option<&'a TrackInfo> {
        let rows = self.roster(tracks);
        if rows.is_empty() {
            return None;
        }
        let idx = self.selected.min(rows.len() - 1);
        let track_idx = match rows[idx] {
            RosterRow::Track { track_idx } | RosterRow::Chair { track_idx, .. } => track_idx,
        };
        tracks.get(track_idx)
    }

    /// Context ID that `Enter` should jump to.
    pub fn selected_context(&self, tracks: &[TrackInfo]) -> Option<ContextId> {
        let rows = self.roster(tracks);
        if rows.is_empty() {
            return None;
        }
        let idx = self.selected.min(rows.len() - 1);
        match rows[idx] {
            RosterRow::Track { track_idx } => tracks.get(track_idx).map(|t| t.score_context_id),
            RosterRow::Chair { context_id, .. } => Some(context_id),
        }
    }

    /// Handle a keystroke on the Stage screen.
    pub fn handle_key(
        &mut self,
        key: &KeyEvent,
        tracks: &[TrackInfo],
        latest_abc: Option<(String, String)>,
    ) -> StageOutcome {
        let rows = self.roster(tracks);
        let count = rows.len();
        if count > 0 && self.selected >= count {
            self.selected = count - 1;
        }

        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => StageOutcome::Dismiss,
            KeyCode::Char('j') | KeyCode::Down => {
                if count > 0 {
                    self.selected = (self.selected + 1).min(count - 1);
                    self.score_scroll = 0;
                }
                StageOutcome::None
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.selected = self.selected.saturating_sub(1);
                self.score_scroll = 0;
                StageOutcome::None
            }
            KeyCode::Tab => {
                self.view_mode = match self.view_mode {
                    StageViewMode::PianoRoll => StageViewMode::Score,
                    StageViewMode::Score => StageViewMode::PianoRoll,
                };
                self.score_scroll = 0;
                StageOutcome::None
            }
            KeyCode::Char(' ') => {
                if let Some(track) = self.selected_track(tracks) {
                    StageOutcome::TogglePlayPause {
                        track_id: track.id.clone(),
                        playing: track.playing,
                    }
                } else {
                    StageOutcome::None
                }
            }
            KeyCode::Char('s') => {
                if let Some(track) = self.selected_track(tracks) {
                    StageOutcome::Stop {
                        track_id: track.id.clone(),
                    }
                } else {
                    StageOutcome::None
                }
            }
            KeyCode::Char('+') | KeyCode::Char('=') => {
                if let Some(track) = self.selected_track(tracks) {
                    let bpm = bpm_from_period(track.period_us).saturating_add(5).min(300);
                    StageOutcome::SetTempo {
                        track_id: track.id.clone(),
                        bpm,
                    }
                } else {
                    StageOutcome::None
                }
            }
            KeyCode::Char('-') => {
                if let Some(track) = self.selected_track(tracks) {
                    let bpm = bpm_from_period(track.period_us).saturating_sub(5).max(20);
                    StageOutcome::SetTempo {
                        track_id: track.id.clone(),
                        bpm,
                    }
                } else {
                    StageOutcome::None
                }
            }
            KeyCode::Char('o') => {
                if let Some((title, raw_abc)) = latest_abc {
                    StageOutcome::OpenFullScore { title, raw_abc }
                } else {
                    StageOutcome::None
                }
            }
            KeyCode::Enter => match self.selected_context(tracks) {
                Some(ctx) => StageOutcome::SwitchContext(ctx),
                None => StageOutcome::None,
            },
            _ => StageOutcome::None,
        }
    }

    fn ensure_engraved(
        &mut self,
        raw_abc: &str,
        tune: &Tune,
        width: u16,
        cell_px: CellPixels,
        fg: &'static str,
    ) -> &[EngravedSystem] {
        let hit = self.score_cache.as_ref().is_some_and(|c| {
            c.raw_abc == raw_abc && c.width == width && c.cell_px == cell_px && c.fg == fg
        });
        if !hit {
            let systems = engrave_measured_systems(
                raw_abc,
                0,
                tune,
                width,
                STAGE_MAX_ROWS_PER_SYSTEM,
                cell_px,
                fg,
            );
            self.score_cache = Some(StageScoreCache {
                raw_abc: raw_abc.to_string(),
                width,
                cell_px,
                fg,
                systems,
            });
        }
        self.score_cache
            .as_ref()
            .map_or(&[], |c| c.systems.as_slice())
    }
}

/// Find the newest openable ABC block in a context's mirror blocks.
pub fn latest_abc_in_blocks(blocks: &[BlockSnapshot]) -> Option<(String, String)> {
    for block in blocks.iter().rev() {
        if let Some(how) = openable_abc(block.content_type, &block.content) {
            let title = format!("block #{}", block.id.seq);
            let raw = how.source(&block.content).to_string();
            return Some((title, raw));
        }
    }
    None
}

fn bpm_from_period(period_us: u64) -> u64 {
    60_000_000u64.checked_div(period_us).map_or(120, |bpm| bpm.max(1))
}

/// Pre-built Stage frame ready to paint inside `?2026`.
pub struct PreparedStage {
    transport: Line<'static>,
    roster: Vec<Line<'static>>,
    main_pane: Vec<Line<'static>>,
    footer: Line<'static>,
}

/// Build the full-screen Stage frame (including any Kitty score rasterization)
/// before taking `TermLock` or opening `?2026`.
pub fn prepare_stage(app: &mut App, width: u16, height: u16, armed: bool) -> PreparedStage {
    let palette = app.palette;
    let now = Instant::now();
    let tracks = app.track_infos.clone();
    let wide = width >= 64;
    let roster_w = if wide { 28u16 } else { width };
    let main_w = if wide { width.saturating_sub(roster_w + 1) } else { width };
    let body_h = height.saturating_sub(2);
    let main_h = if wide { body_h } else { body_h.saturating_sub(6).max(4) };

    let selected_idx = app.stage.as_ref().map_or(0, |s| s.selected);
    let view_mode = app.stage.as_ref().map_or(StageViewMode::PianoRoll, |s| s.view_mode);
    let selected_track = app.stage.as_ref().and_then(|s| s.selected_track(&tracks)).cloned();

    // 1. Pinned Top Transport Bar
    let transport = match &selected_track {
        Some(t) => {
            let play_glyph = if t.playing { "▶" } else { "⏸" };
            let bpm = bpm_from_period(t.period_us);
            let live_pos = app.beats.beat_position(&t.score_context_id, now);
            let tick = live_pos.map_or(t.playhead_tick.max(0) as u64, |p| p.floor().max(0.0) as u64);
            let bar = tick / BEATS_PER_BAR + 1;
            let beat = tick % BEATS_PER_BAR + 1;
            let pulse = beat_pulse_strip(live_pos, t.playing);
            let mode_str = match view_mode {
                StageViewMode::PianoRoll => "roll",
                StageViewMode::Score => if app.kitty.enabled { "score·kitty" } else { "score·text" },
            };
            Line::from(vec![
                Span::styled(
                    format!(" {play_glyph} {} ", t.id),
                    if t.playing {
                        Style::default().fg(Color::Green).add_modifier(Modifier::BOLD)
                    } else {
                        palette.score_heading().add_modifier(Modifier::BOLD)
                    },
                ),
                Span::styled(format!("· {bpm} BPM · {bar}.{beat}  "), palette.compose()),
                Span::styled(pulse, Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
                Span::styled(
                    format!("  · {} · {}b phrase  [{mode_str}]", t.clock_kind, t.beats_per_phrase),
                    palette.score_dim(),
                ),
            ])
        }
        None => Line::from(Span::styled(
            " ♪ Stage — no active tracks (attach a context with :kj transport attach)",
            palette.score_dim(),
        )),
    };

    // 2. Left Roster (Tracks + attached musician chairs)
    let mut roster = Vec::new();
    let flat_rows = app.stage.as_ref().map_or_else(Vec::new, |s| s.roster(&tracks));
    let clamped_sel = if flat_rows.is_empty() { 0 } else { selected_idx.min(flat_rows.len() - 1) };
    if flat_rows.is_empty() {
        roster.push(Line::from(Span::styled(" (no tracks)", palette.score_dim())));
    } else {
        for (row_idx, row) in flat_rows.iter().enumerate() {
            let is_sel = row_idx == clamped_sel;
            let prefix = if is_sel { "▸ " } else { "  " };
            let line = match row {
                RosterRow::Track { track_idx } => {
                    let t = &tracks[*track_idx];
                    let play = if t.playing { "▶" } else { "⏸" };
                    let bpm = bpm_from_period(t.period_us);
                    let pulse = if t.playing && app.beats.envelope(&t.score_context_id, now) > 0.5 {
                        "●"
                    } else {
                        "○"
                    };
                    let style = if is_sel {
                        palette.score_heading().add_modifier(Modifier::BOLD)
                    } else {
                        palette.compose()
                    };
                    Line::from(Span::styled(
                        format!("{prefix}{play} {} {bpm}bpm {pulse}", t.id),
                        style,
                    ))
                }
                RosterRow::Chair { context_id, .. } => {
                    let label = app.label_for(*context_id);
                    let vibe = app
                        .info(*context_id)
                        .and_then(|c| c.hud.vibe.as_deref())
                        .unwrap_or("");
                    let suffix = if vibe.is_empty() {
                        String::new()
                    } else {
                        format!(" {vibe}")
                    };
                    let style = if is_sel {
                        palette.score_heading().add_modifier(Modifier::BOLD)
                    } else {
                        palette.score_dim()
                    };
                    Line::from(Span::styled(format!("{prefix}  └ {label}{suffix}"), style))
                }
            };
            roster.push(line);
        }
    }

    // 3. Main Stage Pane (Piano Roll or Engraved Score)
    let latest_abc = selected_track.as_ref().and_then(|t| {
        let view = app.views.get(&t.score_context_id)?;
        latest_abc_in_blocks(view.mirror.blocks())
    });
    let playhead_beat = selected_track
        .as_ref()
        .filter(|t| t.playing)
        .and_then(|t| app.beats.beat_position(&t.score_context_id, now));

    let main_pane = match latest_abc {
        Some((title, raw_abc)) => match parse_first_tune(&raw_abc) {
            Ok(tune) => match view_mode {
                StageViewMode::PianoRoll => {
                    let roll = extract_piano_roll(&tune);
                    let mut lines = Vec::with_capacity(usize::from(main_h));
                    lines.push(Line::from(Span::styled(
                        format!(" ♪ {title} — {} notes · {:.0} beats", roll.notes.len(), roll.total_beats),
                        palette.score_heading(),
                    )));
                    lines.extend(render_piano_roll(
                        &roll,
                        playhead_beat,
                        main_w,
                        main_h.saturating_sub(1),
                        &palette,
                    ));
                    lines
                }
                StageViewMode::Score => {
                    let fg = notation_color(&palette);
                    let mut lines = Vec::new();
                    lines.push(Line::from(Span::styled(
                        format!(" ♪ {title}  (Tab piano roll · o full score)"),
                        palette.score_heading(),
                    )));
                    if app.kitty.enabled && main_w >= 20 {
                        let cell_px = app.kitty.cell_px;
                        let systems: Vec<EngravedSystem> = app
                            .stage
                            .as_mut()
                            .map(|s| s.ensure_engraved(&raw_abc, &tune, main_w, cell_px, fg).to_vec())
                            .unwrap_or_default();
                        let avail = usize::from(main_h.saturating_sub(1));
                        let mut used = 0usize;
                        for (idx, sys) in systems.iter().enumerate() {
                            if idx > 0 && used < avail {
                                lines.push(Line::default());
                                used += 1;
                            }
                            if used >= avail {
                                break;
                            }
                            if let Some(img) = app.kitty.ensure_svg(
                                sys.key,
                                &sys.svg,
                                main_w,
                                STAGE_MAX_ROWS_PER_SYSTEM,
                            ) {
                                let take = avail.saturating_sub(used);
                                let pl = placeholder_lines(img.id, img.cols, img.rows);
                                let added = pl.len().min(take);
                                lines.extend(pl.into_iter().take(take));
                                used += added;
                            }
                        }
                    } else {
                        for raw_line in raw_abc.lines().take(usize::from(main_h.saturating_sub(1))) {
                            lines.push(Line::from(Span::styled(raw_line.to_string(), palette.score_code())));
                        }
                    }
                    lines
                }
            },
            Err(err) => vec![Line::from(Span::styled(
                format!(" ABC parse error in {title}: {err}"),
                palette.alarm(),
            ))],
        },
        None => {
            let msg = if let Some(t) = &selected_track {
                if app.views.contains_key(&t.score_context_id) {
                    format!(" no ABC score blocks in track '{}' yet", t.id)
                } else {
                    format!(" hydrating score context for '{}'…", t.id)
                }
            } else {
                " select a track to view its live piano roll or score".to_string()
            };
            vec![Line::from(Span::styled(msg, palette.score_dim()))]
        }
    };

    // 4. Bottom Key Hints (or Ctrl+A legend while armed)
    let footer = if armed {
        crate::status::legend_line(width, &palette)
    } else {
        Line::from(Span::styled(
            " j/k select · Space play/pause · s stop · +/- bpm · Tab roll/score · o full score · Enter open · q/Esc close",
            palette.score_dim(),
        ))
    };

    PreparedStage {
        transport,
        roster,
        main_pane,
        footer,
    }
}

/// Paint a [`PreparedStage`] onto a ratatui [`Frame`].
pub fn render_stage(frame: &mut Frame, prepared: PreparedStage) {
    let area = frame.area();
    let [top_bar, middle, bottom_bar] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(0),
        Constraint::Length(1),
    ])
    .areas(area);

    frame.render_widget(Paragraph::new(prepared.transport), top_bar);
    frame.render_widget(Paragraph::new(prepared.footer), bottom_bar);

    if middle.width >= 64 {
        let [left, _sep, right] = Layout::horizontal([
            Constraint::Length(28),
            Constraint::Length(1),
            Constraint::Min(0),
        ])
        .areas(middle);
        frame.render_widget(Paragraph::new(prepared.roster), left);
        frame.render_widget(Paragraph::new(prepared.main_pane), right);
    } else {
        let roster_h = (prepared.roster.len() as u16).clamp(2, 6).min(middle.height / 2);
        let [top_roster, bottom_main] = Layout::vertical([
            Constraint::Length(roster_h),
            Constraint::Min(0),
        ])
        .areas(middle);
        frame.render_widget(Paragraph::new(prepared.roster), top_roster);
        frame.render_widget(Paragraph::new(prepared.main_pane), bottom_main);
    }
}

/// Draw a [`PreparedStage`] onto `terminal`.
pub fn draw_prepared_stage<B: ratatui::backend::Backend>(
    terminal: &mut ratatui::Terminal<B>,
    prepared: PreparedStage,
) -> Result<(), B::Error> {
    terminal.draw(|frame| render_stage(frame, prepared))?;
    Ok(())
}

/// Helper to keep unused import clean when `Rect` is referenced in tests.
#[allow(dead_code)]
fn _rect_check(_r: Rect) {}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyModifiers;

    const TUNE: &str = "X:1\nT:Scale\nM:4/4\nL:1/4\nK:C\nC D E F | G A B c |\n";

    #[test]
    fn extract_piano_roll_finds_all_notes_and_bounds() {
        let tune = parse_first_tune(TUNE).expect("parses");
        let roll = extract_piano_roll(&tune);
        assert_eq!(roll.notes.len(), 8);
        assert!(roll.min_pitch <= 60);
        assert!(roll.max_pitch >= 72);
        assert!(roll.total_beats >= 8.0);
    }

    #[test]
    fn render_piano_roll_draws_notes_and_live_playhead() {
        let tune = parse_first_tune(TUNE).expect("parses");
        let roll = extract_piano_roll(&tune);
        let lines = render_piano_roll(&roll, Some(2.5), 40, 10, &Palette::tokyo_midnight());
        assert_eq!(lines.len(), 10);
        let joined: String = lines
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref()))
            .collect();
        assert!(joined.contains('█'), "notes rendered");
        assert!(joined.contains("bar "), "bar ruler rendered");
    }

    #[test]
    fn beat_pulse_strip_tracks_beat_in_bar() {
        assert_eq!(beat_pulse_strip(Some(0.1), true), "● ▫ ▫ ▫");
        assert_eq!(beat_pulse_strip(Some(2.1), true), "▪ ▪ ● ▫");
        assert_eq!(beat_pulse_strip(Some(2.1), false), "▫ ▫ ▫ ▫");
    }

    #[test]
    fn stage_keys_dispatch_transport_and_toggle_mode() {
        let ctx = ContextId::new();
        let tracks = vec![TrackInfo {
            id: "jam".to_string(),
            score_context_id: ctx,
            playing: false,
            playhead_tick: 0,
            period_us: 500_000, // 120 BPM
            beats_per_phrase: 16,
            beat_count: 0,
            last_epoch_ns: 0,
            clock_kind: "system".to_string(),
            attached: vec![ctx],
        }];
        let mut stage = StageModel::new();
        assert_eq!(
            stage.handle_key(&KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE), &tracks, None),
            StageOutcome::TogglePlayPause { track_id: "jam".to_string(), playing: false }
        );
        assert_eq!(
            stage.handle_key(&KeyEvent::new(KeyCode::Char('+'), KeyModifiers::NONE), &tracks, None),
            StageOutcome::SetTempo { track_id: "jam".to_string(), bpm: 125 }
        );
        assert_eq!(
            stage.handle_key(&KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE), &tracks, None),
            StageOutcome::None
        );
        assert_eq!(stage.view_mode, StageViewMode::Score);
        assert_eq!(
            stage.handle_key(&KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &tracks, None),
            StageOutcome::SwitchContext(ctx)
        );
    }
}
