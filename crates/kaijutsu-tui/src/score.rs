//! ABC sheet music engraving, inline transcript scores, and the full-screen
//! score viewer (`ScreenMode::Score`, `Ctrl+A s`).
//!
//! Mirrors `diff.rs`:
//! - **Openable detection** (`openable_abc`): `ContentType::Abc` is
//!   authoritative; `Plain`/`Markdown` blocks with a ```` ```abc ```` fence or
//!   bare `X:` + `K:` ABC headers are sniffed.
//! - **Freeze on open**: `ScoreScreen` parses once on open and holds the
//!   frozen tune + raw source for its lifetime.
//! - **Graceful fallback**: in plain Kitty (`kitty.enabled`), renders crisp
//!   engraved staves via `kaijutsu-abc` → `resvg` → Kitty `U=1` placeholders.
//!   In `screen`, plain SSH, or `vt100` tests (`!kitty.enabled`), renders
//!   structured metadata + raw ABC text.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use kaijutsu_abc::ast::Tune;
use kaijutsu_abc::engrave::{EngravingOptions, layout::engrave_systems, svg::to_svg};
use kaijutsu_types::ContentType;
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};

use crate::kitty::{KittyStore, placeholder_lines};
use crate::present::Palette;

/// Default body height before the first frame stamps the real one.
const DEFAULT_BODY_LINES: usize = 20;

/// Maximum systems rendered inline in the transcript before truncating with a
/// `Ctrl+A s` hint, so a multi-page score never hijacks the conversation.
pub const INLINE_MAX_SYSTEMS: usize = 3;

/// Maximum rows per engraved system when rendered inline in the transcript.
const INLINE_MAX_ROWS_PER_SYSTEM: u16 = 12;

/// Maximum rows per engraved system in the full-screen score viewer.
const FULLSCREEN_MAX_ROWS_PER_SYSTEM: u16 = 18;

/// How a block qualified as an ABC score.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OpenAbc {
    /// `ContentType::Abc` — authoritative.
    Declared,
    /// A fenced ```` ```abc ```` code block inside `Plain` or `Markdown`.
    Fenced { inner: String },
    /// Bare ABC notation (`X:` header + `K:` key) inside `Plain` or `Markdown`.
    Sniffed,
}

impl OpenAbc {
    /// The ABC source text to parse.
    pub fn source<'a>(&'a self, block_text: &'a str) -> &'a str {
        match self {
            Self::Declared | Self::Sniffed => block_text,
            Self::Fenced { inner } => inner.as_str(),
        }
    }
}

/// Decide whether a block can open as an ABC score.
pub fn openable_abc(content_type: ContentType, text: &str) -> Option<OpenAbc> {
    match content_type {
        ContentType::Abc => Some(OpenAbc::Declared),
        ContentType::Plain | ContentType::Markdown => {
            if let Some(inner) = extract_abc_fence(text) {
                return Some(OpenAbc::Fenced { inner });
            }
            if looks_like_bare_abc(text) {
                return Some(OpenAbc::Sniffed);
            }
            None
        }
        _ => None,
    }
}

/// Extract the first ```` ```abc ```` fenced block from markdown/plain text.
fn extract_abc_fence(text: &str) -> Option<String> {
    let mut in_fence = false;
    let mut buf = String::new();
    for line in text.lines() {
        let trimmed = line.trim();
        if !in_fence {
            if let Some(rest) = trimmed.strip_prefix("```") {
                let lang = rest.trim().to_ascii_lowercase();
                if lang == "abc" || lang == "abc-notation" {
                    in_fence = true;
                }
            }
        } else if trimmed.starts_with("```") {
            let trimmed_buf = buf.trim();
            if !trimmed_buf.is_empty() {
                return Some(trimmed_buf.to_string());
            }
            in_fence = false;
            buf.clear();
        } else {
            buf.push_str(line);
            buf.push('\n');
        }
    }
    None
}

/// Heuristic for bare ABC: an `X:` reference line and a `K:` key line.
fn looks_like_bare_abc(text: &str) -> bool {
    let mut has_x = false;
    let mut has_k = false;
    for line in text.lines().take(32) {
        let t = line.trim_start();
        if t.starts_with("X:") && t[2..].trim().chars().next().is_some_and(|c| c.is_ascii_digit()) {
            has_x = true;
        } else if t.starts_with("K:") && !t[2..].trim().is_empty() {
            has_k = true;
        }
        if has_x && has_k {
            return true;
        }
    }
    false
}

/// Summary metadata pulled from a parsed ABC tune for headers and text fallback.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScoreMeta {
    pub title: Option<String>,
    pub key: String,
    pub meter: String,
    pub tempo: Option<String>,
    pub voices: usize,
}

impl ScoreMeta {
    pub fn from_tune(tune: &Tune, raw: &str) -> Self {
        let title = tune
            .header
            .titles
            .first()
            .cloned()
            .filter(|t| !t.trim().is_empty())
            .or_else(|| raw_header_value(raw, "T:"));
        let key = raw_header_value(raw, "K:").unwrap_or_else(|| "C".to_string());
        let meter = raw_header_value(raw, "M:").unwrap_or_else(|| "4/4".to_string());
        let tempo = raw_header_value(raw, "Q:");
        let voices = tune.voices.len().max(1);
        Self {
            title,
            key,
            meter,
            tempo,
            voices,
        }
    }

    pub fn summary_line(&self, fallback_title: &str) -> String {
        let name = self.title.as_deref().unwrap_or(fallback_title);
        let mut out = format!("♪ {name} · K:{} · M:{}", self.key, self.meter);
        if let Some(q) = &self.tempo {
            out.push_str(&format!(" · Q:{q}"));
        }
        if self.voices > 1 {
            out.push_str(&format!(" · {}v", self.voices));
        }
        out
    }
}

fn raw_header_value(raw: &str, prefix: &str) -> Option<String> {
    raw.lines()
        .map(str::trim)
        .find_map(|l| l.strip_prefix(prefix).map(|v| v.trim().to_string()))
        .filter(|v| !v.is_empty())
}

/// Parse the first tune from an ABC string.
pub fn parse_first_tune(raw: &str) -> Result<Tune, String> {
    let parsed = kaijutsu_abc::parse(raw);
    if let Some(tune) = parsed.value.into_iter().next() {
        Ok(tune)
    } else if let Some(fb) = parsed.feedback.first() {
        Err(fb.message.clone())
    } else {
        Err("no ABC tune found".to_string())
    }
}

/// Foreground hex color for engraved notation matching the current ground.
fn notation_color(palette: &Palette) -> &'static str {
    if palette.light_ground {
        "#1f2335"
    } else {
        "#c0caf5"
    }
}

/// Engrave `tune` into per-system SVG strings sized for `cols` terminal columns.
fn engrave_system_svgs(tune: &Tune, cols: u16, cell_px_w: u16, fg_hex: &str) -> Vec<String> {
    // Map terminal pixel width to SVG user units (staff_spacing=10.0 looks crisp
    // at ~1.5-2.0 px per SVG unit on HiDPI/regular Kitty displays).
    let px_w = f64::from(cols.max(20)) * f64::from(cell_px_w.max(6));
    let svg_width = (px_w / 1.6).clamp(320.0, 1400.0);
    let opts = EngravingOptions {
        staff_spacing: 10.0,
        margin: 12.0,
        color: fg_hex.to_string(),
    };
    let systems = engrave_systems(tune, &opts, svg_width);
    systems
        .iter()
        .map(|elems| to_svg(elems, opts.margin, &opts.color))
        .collect()
}

fn cache_key(raw: &str, system_idx: usize, cols: u16, max_rows: u16, fg_hex: &str) -> u64 {
    let mut h = DefaultHasher::new();
    raw.hash(&mut h);
    system_idx.hash(&mut h);
    cols.hash(&mut h);
    max_rows.hash(&mut h);
    fg_hex.hash(&mut h);
    h.finish()
}

/// Render an ABC block inline for the conversation transcript (Slice 2).
///
/// Returns `None` when Kitty graphics are disabled, when the block is not an
/// ABC score, or when parsing fails — callers fall back to normal text lines.
pub fn inline_abc_lines(
    content_type: ContentType,
    text: &str,
    width: u16,
    palette: &Palette,
    kitty: &mut KittyStore,
) -> Option<Vec<Line<'static>>> {
    if !kitty.enabled || width < 20 {
        return None;
    }
    let how = openable_abc(content_type, text)?;
    let raw = how.source(text);
    let tune = parse_first_tune(raw).ok()?;
    let meta = ScoreMeta::from_tune(&tune, raw);
    let fg = notation_color(palette);
    let svgs = engrave_system_svgs(&tune, width, kitty.cell_px.width, fg);
    if svgs.is_empty() {
        return None;
    }

    let mut out = Vec::new();
    let header = format!("{}  (Ctrl+A s)", meta.summary_line("score"));
    out.push(Line::from(Span::styled(header, palette.score_heading())));

    let shown = svgs.len().min(INLINE_MAX_SYSTEMS);
    for (idx, svg) in svgs.iter().take(shown).enumerate() {
        let key = cache_key(raw, idx, width, INLINE_MAX_ROWS_PER_SYSTEM, fg);
        if let Some(img) = kitty.ensure_svg(key, svg, width, INLINE_MAX_ROWS_PER_SYSTEM) {
            out.extend(placeholder_lines(img.id, img.cols, img.rows));
        }
    }
    if svgs.len() > shown {
        let more = svgs.len() - shown;
        out.push(Line::from(Span::styled(
            format!("… +{more} more {} (Ctrl+A s to open full score)", if more == 1 { "system" } else { "systems" }),
            palette.score_dim(),
        )));
    }
    Some(out)
}

/// Full-screen ABC score viewer (`ScreenMode::Score`, Slice 1).
pub struct ScoreScreen {
    pub title: String,
    pub raw_abc: String,
    parsed: Result<Tune, String>,
    meta: Option<ScoreMeta>,
    pub show_raw: bool,
    top: usize,
    body_h: usize,
}

/// What a key on [`ScoreScreen`] asked for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScoreKey {
    Close,
    Yank(String),
    ToggledRaw,
    Scrolled,
    Ignored,
}

impl ScoreScreen {
    /// Freeze an ABC block into a full-screen score viewer.
    pub fn open(title: impl Into<String>, raw_abc: impl Into<String>) -> Self {
        let title = title.into();
        let raw_abc = raw_abc.into();
        let parsed = parse_first_tune(&raw_abc);
        let meta = parsed.as_ref().ok().map(|t| ScoreMeta::from_tune(t, &raw_abc));
        Self {
            title,
            raw_abc,
            parsed,
            meta,
            show_raw: false,
            top: 0,
            body_h: DEFAULT_BODY_LINES,
        }
    }

    pub fn body_h(&self) -> usize {
        self.body_h
    }

    pub fn scroll(&mut self, delta: isize, total: usize) {
        let max_top = total.saturating_sub(self.body_h);
        if delta < 0 {
            self.top = self.top.saturating_sub(delta.unsigned_abs());
        } else {
            self.top = (self.top + delta as usize).min(max_top);
        }
    }

    /// Build one frame of the full-screen score viewer.
    pub fn frame(
        &mut self,
        width: u16,
        height: u16,
        palette: &Palette,
        kitty: &mut KittyStore,
    ) -> Vec<Line<'static>> {
        let height = usize::from(height.max(3));
        let body_h = height.saturating_sub(2);
        self.body_h = body_h;

        let mode_tag = if self.show_raw {
            "raw"
        } else if kitty.enabled && self.parsed.is_ok() {
            "kitty"
        } else {
            "text"
        };
        let header_text = match &self.meta {
            Some(m) => format!(" {}  [{mode_tag}]", m.summary_line(&self.title)),
            None => format!(" ♪ {}  [{mode_tag}]", self.title),
        };
        let header = Line::from(Span::styled(
            header_text,
            palette.score_heading().add_modifier(Modifier::BOLD),
        ));

        let body = self.body_lines(width, palette, kitty);
        let max_top = body.len().saturating_sub(body_h);
        if self.top > max_top {
            self.top = max_top;
        }

        let mut out = Vec::with_capacity(height);
        out.push(header);
        for line in body.iter().skip(self.top).take(body_h) {
            out.push(line.clone());
        }
        while out.len() < height - 1 {
            out.push(Line::default());
        }

        let footer_text = format!(
            " j/k scroll · r {} · y yank · q/Esc close  ({}/{})",
            if self.show_raw { "score" } else { "raw" },
            (self.top + 1).min(body.len().max(1)),
            body.len().max(1),
        );
        out.push(Line::from(Span::styled(footer_text, palette.score_dim())));
        out
    }

    fn body_lines(
        &self,
        width: u16,
        palette: &Palette,
        kitty: &mut KittyStore,
    ) -> Vec<Line<'static>> {
        if !self.show_raw
            && kitty.enabled
            && let Ok(tune) = &self.parsed
        {
            let fg = notation_color(palette);
            let svgs = engrave_system_svgs(tune, width, kitty.cell_px.width, fg);
            let mut out = Vec::new();
            for (idx, svg) in svgs.iter().enumerate() {
                if idx > 0 {
                    out.push(Line::default());
                }
                let key = cache_key(&self.raw_abc, idx, width, FULLSCREEN_MAX_ROWS_PER_SYSTEM, fg);
                if let Some(img) = kitty.ensure_svg(key, svg, width, FULLSCREEN_MAX_ROWS_PER_SYSTEM) {
                    out.extend(placeholder_lines(img.id, img.cols, img.rows));
                }
            }
            if !out.is_empty() {
                return out;
            }
        }

        let mut lines = Vec::new();
        if let Err(err) = &self.parsed {
            lines.push(Line::from(Span::styled(
                format!("abc parse error: {err}"),
                palette.alarm(),
            )));
            lines.push(Line::default());
        } else if !self.show_raw && !kitty.enabled {
            lines.push(Line::from(Span::styled(
                "kitty graphics unavailable in this terminal — showing raw ABC (use --kitty-graphics in plain kitty)",
                palette.score_dim(),
            )));
            lines.push(Line::default());
        }
        for raw_line in self.raw_abc.lines() {
            let trimmed = raw_line.trim_start();
            let style = if trimmed.starts_with('%') {
                palette.score_dim()
            } else if trimmed.len() >= 2 && trimmed.as_bytes()[1] == b':' && trimmed.as_bytes()[0].is_ascii_alphabetic() {
                palette.score_heading()
            } else {
                palette.score_code()
            };
            lines.push(Line::from(Span::styled(raw_line.to_string(), style)));
        }
        lines
    }
}

/// Handle one key event on the full-screen score viewer.
pub fn handle_key(screen: &mut ScoreScreen, key: &KeyEvent, body_h: usize) -> ScoreKey {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let page = body_h.max(1) as isize;
    let half = (page / 2).max(1);
    match key.code {
        KeyCode::Esc | KeyCode::Char('q') => ScoreKey::Close,
        KeyCode::Char('r') => {
            screen.show_raw = !screen.show_raw;
            screen.top = 0;
            ScoreKey::ToggledRaw
        }
        KeyCode::Char('y') => ScoreKey::Yank(screen.raw_abc.clone()),
        KeyCode::Char('j') | KeyCode::Down => {
            screen.scroll(1, usize::MAX);
            ScoreKey::Scrolled
        }
        KeyCode::Char('k') | KeyCode::Up => {
            screen.scroll(-1, usize::MAX);
            ScoreKey::Scrolled
        }
        KeyCode::Char('d') if ctrl => {
            screen.scroll(half, usize::MAX);
            ScoreKey::Scrolled
        }
        KeyCode::Char('u') if ctrl => {
            screen.scroll(-half, usize::MAX);
            ScoreKey::Scrolled
        }
        KeyCode::PageDown | KeyCode::Char(' ') => {
            screen.scroll(page, usize::MAX);
            ScoreKey::Scrolled
        }
        KeyCode::PageUp => {
            screen.scroll(-page, usize::MAX);
            ScoreKey::Scrolled
        }
        KeyCode::Char('g') | KeyCode::Home => {
            screen.top = 0;
            ScoreKey::Scrolled
        }
        KeyCode::Char('G') | KeyCode::End => {
            screen.top = usize::MAX;
            ScoreKey::Scrolled
        }
        _ => ScoreKey::Ignored,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kitty::{CellPixels, PLACEHOLDER_CHAR};

    const SAMPLE_ABC: &str = "X:1\nT:Cooley's\nM:4/4\nL:1/8\nQ:1/4=120\nK:Edor\n|:D2|EBBA B2EB|B2AB dBAG|FDAD BDAD|FDAD dAFD|\n";

    #[test]
    fn openable_abc_detects_declared_fenced_and_bare() {
        assert_eq!(openable_abc(ContentType::Abc, "anything"), Some(OpenAbc::Declared));
        assert_eq!(openable_abc(ContentType::Plain, "hello world"), None);
        assert_eq!(openable_abc(ContentType::Plain, SAMPLE_ABC), Some(OpenAbc::Sniffed));

        let fenced = format!("Here is a tune:\n```abc\n{SAMPLE_ABC}```\n");
        let Some(OpenAbc::Fenced { inner }) = openable_abc(ContentType::Markdown, &fenced) else {
            panic!("expected fenced abc");
        };
        assert!(inner.starts_with("X:1"));
    }

    #[test]
    fn inline_abc_renders_kitty_placeholders_when_enabled() {
        let palette = Palette::tokyo_midnight();
        let mut disabled = KittyStore::disabled();
        assert!(inline_abc_lines(ContentType::Abc, SAMPLE_ABC, 80, &palette, &mut disabled).is_none());

        let mut enabled = KittyStore::enabled(CellPixels { width: 10, height: 20 });
        let lines = inline_abc_lines(ContentType::Abc, SAMPLE_ABC, 80, &palette, &mut enabled)
            .expect("inline score renders");
        assert!(lines.len() >= 3);
        let header: String = lines[0].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(header.contains("Cooley's"));
        assert!(header.contains("K:Edor"));
        assert!(lines[1..].iter().any(|l| l.spans.iter().any(|s| s.content.contains(PLACEHOLDER_CHAR))));
        assert!(!enabled.take_pending_wire().is_empty(), "queued kitty upload + virtual placement");
    }

    #[test]
    fn score_screen_toggles_raw_and_handles_keys() {
        let palette = Palette::tokyo_midnight();
        let mut kitty = KittyStore::enabled(CellPixels { width: 10, height: 20 });
        let mut screen = ScoreScreen::open("block #1", SAMPLE_ABC);

        let frame = screen.frame(80, 24, &palette, &mut kitty);
        assert_eq!(frame.len(), 24);
        assert!(frame.iter().any(|l| l.spans.iter().any(|s| s.content.contains(PLACEHOLDER_CHAR))));

        assert_eq!(handle_key(&mut screen, &KeyEvent::new(KeyCode::Char('r'), KeyModifiers::NONE), 22), ScoreKey::ToggledRaw);
        assert!(screen.show_raw);
        let raw_frame = screen.frame(80, 24, &palette, &mut kitty);
        let joined: String = raw_frame.iter().flat_map(|l| l.spans.iter().map(|s| s.content.as_ref())).collect();
        assert!(joined.contains("EBBA"));

        assert_eq!(handle_key(&mut screen, &KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE), 22), ScoreKey::Close);
    }
}
