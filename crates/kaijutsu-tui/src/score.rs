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

use std::collections::{HashMap, hash_map::DefaultHasher};
use std::hash::{Hash, Hasher};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use kaijutsu_abc::ast::Tune;
use kaijutsu_abc::engrave::{
    EngravingOptions,
    ir::engraving_bounds,
    layout::engrave_systems,
    svg::to_svg,
};
use kaijutsu_types::{BlockId, BlockSnapshot, ContentType, Status};
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};

use crate::kitty::{CellPixels, KittyStore, fit_svg_cells, placeholder_lines};
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
    /// A fenced ```` ```abc ```` code block inside `Plain` or `Markdown`,
    /// preserving any prose before and after the fence.
    Fenced {
        before: String,
        inner: String,
        after: String,
    },
    /// Bare ABC notation (`X:` header + `K:` key) inside `Plain`.
    Sniffed,
}

impl OpenAbc {
    /// The ABC source text to parse.
    pub fn source<'a>(&'a self, block_text: &'a str) -> &'a str {
        match self {
            Self::Declared | Self::Sniffed => block_text,
            Self::Fenced { inner, .. } => inner.as_str(),
        }
    }
}

/// Decide whether a block can open as an ABC score.
///
/// `ContentType::Markdown` only matches explicit ```` ```abc ```` fences so
/// prose explaining ABC syntax (`X:1`, `K:C`) is never hijacked as a score.
pub fn openable_abc(content_type: ContentType, text: &str) -> Option<OpenAbc> {
    match content_type {
        ContentType::Abc => Some(OpenAbc::Declared),
        ContentType::Markdown => {
            let (before, inner, after) = extract_abc_fence(text)?;
            Some(OpenAbc::Fenced { before, inner, after })
        }
        ContentType::Plain => {
            if let Some((before, inner, after)) = extract_abc_fence(text) {
                return Some(OpenAbc::Fenced { before, inner, after });
            }
            if looks_like_bare_abc(text) {
                return Some(OpenAbc::Sniffed);
            }
            None
        }
        _ => None,
    }
}

/// Extract the first ```` ```abc ```` fenced block along with any prose before
/// and after it.
fn extract_abc_fence(text: &str) -> Option<(String, String, String)> {
    let lines: Vec<&str> = text.lines().collect();
    let mut start = None;
    for (idx, line) in lines.iter().enumerate() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("```") {
            let lang = rest.trim().to_ascii_lowercase();
            if lang == "abc" || lang == "abc-notation" {
                start = Some(idx);
                break;
            }
        }
    }
    let start = start?;
    let mut end = None;
    for (idx, line) in lines.iter().enumerate().skip(start + 1) {
        if line.trim().starts_with("```") {
            end = Some(idx);
            break;
        }
    }
    let end = end?;
    let inner = lines[start + 1..end].join("\n");
    if inner.trim().is_empty() {
        return None;
    }
    let before = lines[..start].join("\n").trim().to_string();
    let after = lines[end + 1..].join("\n").trim().to_string();
    Some((before, inner.trim().to_string(), after))
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

/// Parse all tunes from an ABC string.
pub fn parse_all_tunes(raw: &str) -> Result<Vec<Tune>, String> {
    let parsed = kaijutsu_abc::parse(raw);
    if !parsed.value.is_empty() {
        Ok(parsed.value)
    } else if let Some(fb) = parsed.feedback.first() {
        Err(fb.message.clone())
    } else {
        Err("no ABC tune found".to_string())
    }
}

/// Parse the first tune from an ABC string.
pub fn parse_first_tune(raw: &str) -> Result<Tune, String> {
    parse_all_tunes(raw).map(|mut v| v.remove(0))
}

/// One pre-engraved system ready for measurement or lazy Kitty upload.
#[derive(Debug, Clone)]
pub(crate) struct EngravedSystem {
    pub svg: String,
    pub rows: u16,
    pub key: u64,
}

/// Engrave `tune` into per-system SVGs with precomputed terminal row heights.
pub(crate) fn engrave_measured_systems(
    raw: &str,
    tune_idx: usize,
    tune: &Tune,
    cols: u16,
    max_rows_per_system: u16,
    cell_px: CellPixels,
    fg_hex: &str,
) -> Vec<EngravedSystem> {
    if cols == 0 {
        return Vec::new();
    }
    // Map terminal pixel width to SVG user units (staff_spacing=10.0 looks crisp
    // at ~1.6 px per SVG unit on Kitty displays).
    let px_w = f64::from(cols.max(20)) * f64::from(cell_px.width.max(6));
    let svg_width = (px_w / 1.6).clamp(320.0, 1400.0);
    let opts = EngravingOptions {
        staff_spacing: 10.0,
        margin: 12.0,
        color: fg_hex.to_string(),
    };
    let systems = engrave_systems(tune, &opts, svg_width);
    systems
        .iter()
        .enumerate()
        .filter_map(|(idx, elems)| {
            let bounds = engraving_bounds(elems, opts.margin);
            let (_, rows, _) = fit_svg_cells(
                bounds.width.round() as f32,
                bounds.height.round() as f32,
                cols,
                max_rows_per_system,
                cell_px,
            )?;
            let svg = to_svg(elems, opts.margin, &opts.color);
            let key = cache_key(raw, tune_idx, idx, cols, max_rows_per_system, fg_hex);
            Some(EngravedSystem { svg, rows, key })
        })
        .collect()
}

fn cache_key(
    raw: &str,
    tune_idx: usize,
    system_idx: usize,
    cols: u16,
    max_rows: u16,
    fg_hex: &str,
) -> u64 {
    let mut h = DefaultHasher::new();
    raw.hash(&mut h);
    tune_idx.hash(&mut h);
    system_idx.hash(&mut h);
    cols.hash(&mut h);
    max_rows.hash(&mut h);
    fg_hex.hash(&mut h);
    h.finish()
}

/// Foreground hex color for engraved notation matching the current ground.
pub(crate) fn notation_color(palette: &Palette) -> &'static str {
    if palette.light_ground {
        "#1f2335"
    } else {
        "#c0caf5"
    }
}

/// Pre-measured inline ABC layout cached per `(block, version, width, cell_px, fg)`.
///
/// Measurement (`plan_rows` / `row_index`) reads `total_rows` directly without
/// rasterizing or uploading anything to Kitty; drawing (`transcript_window`)
/// uploads only systems intersecting the visible window.
#[derive(Debug, Clone)]
pub struct InlineScoreEntry {
    before_lines: Vec<Line<'static>>,
    header: Line<'static>,
    systems: Vec<EngravedSystem>,
    footer: Option<Line<'static>>,
    after_lines: Vec<Line<'static>>,
    raw_abc: String,
    pub truncated: bool,
    pub total_rows: usize,
}

impl InlineScoreEntry {
    /// Render only the visible slice `[skip .. skip + take)` of this inline score,
    /// lazily uploading only systems that intersect the visible viewport.
    pub fn render_slice(
        &self,
        width: u16,
        skip: usize,
        take: usize,
        kitty: &mut KittyStore,
    ) -> Vec<Line<'static>> {
        if take == 0 {
            return Vec::new();
        }
        let win_start = skip;
        let win_end = skip.saturating_add(take);
        let mut out = Vec::with_capacity(take.min(self.total_rows));
        let mut cur = 0usize;

        let push_lines = |out: &mut Vec<Line<'static>>, cur: &mut usize, lines: &[Line<'static>]| {
            for line in lines {
                if *cur >= win_start && *cur < win_end {
                    out.push(line.clone());
                }
                *cur += 1;
                if *cur >= win_end {
                    break;
                }
            }
        };

        push_lines(&mut out, &mut cur, &self.before_lines);
        if cur < win_end {
            push_lines(&mut out, &mut cur, std::slice::from_ref(&self.header));
        }
        for sys in &self.systems {
            if cur >= win_end {
                break;
            }
            let sys_rows = usize::from(sys.rows);
            let sys_end = cur + sys_rows;
            if sys_end > win_start && cur < win_end {
                let s_skip = win_start.saturating_sub(cur);
                let s_take = (win_end - cur.max(win_start)).min(sys_rows.saturating_sub(s_skip));
                if let Some(img) = kitty.ensure_svg(sys.key, &sys.svg, width, INLINE_MAX_ROWS_PER_SYSTEM) {
                    let mut pl = placeholder_lines(img.id, img.cols, img.rows);
                    while pl.len() < sys_rows {
                        pl.push(Line::default());
                    }
                    out.extend(pl.into_iter().skip(s_skip).take(s_take));
                } else {
                    for _ in 0..s_take {
                        out.push(Line::default());
                    }
                }
            }
            cur = sys_end;
        }
        if cur < win_end
            && let Some(footer) = &self.footer
        {
            push_lines(&mut out, &mut cur, std::slice::from_ref(footer));
        }
        if cur < win_end {
            push_lines(&mut out, &mut cur, &self.after_lines);
        }
        out
    }

    /// Plain-text projection with the exact same `total_rows` height, used by
    /// copy-mode search and yank so `U+10EEEE` placeholder bytes never leak
    /// into the clipboard or vi registers.
    pub fn render_plain_lines(&self) -> Vec<Line<'static>> {
        let mut out = Vec::with_capacity(self.total_rows);
        out.extend(self.before_lines.iter().cloned());
        out.push(self.header.clone());
        let sys_total: usize = self.systems.iter().map(|s| usize::from(s.rows)).sum();
        let mut raw_iter = self.raw_abc.lines();
        for _ in 0..sys_total {
            let text = raw_iter.next().unwrap_or("");
            out.push(Line::from(text.to_string()));
        }
        if let Some(footer) = &self.footer {
            out.push(footer.clone());
        }
        out.extend(self.after_lines.iter().cloned());
        debug_assert_eq!(out.len(), self.total_rows);
        out
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct InlineCacheKey {
    block_id: BlockId,
    version: u64,
    width: u16,
    cell_px: CellPixels,
    fg: &'static str,
}

/// Per-session cache of measured inline ABC layouts (including negative hits).
#[derive(Default)]
pub struct InlineScoreCache {
    entries: HashMap<InlineCacheKey, Option<InlineScoreEntry>>,
}

impl InlineScoreCache {
    pub fn new() -> Self {
        Self::default()
    }

    /// Measure a settled block for inline ABC rendering without rasterizing or
    /// uploading any Kitty images. Caches negative results so non-ABC blocks
    /// cost one hash lookup on subsequent frames and keystrokes.
    pub fn measure(
        &mut self,
        block: &BlockSnapshot,
        version: u64,
        width: u16,
        cell_px: CellPixels,
        palette: &Palette,
    ) -> Option<&InlineScoreEntry> {
        if width < 20 || !matches!(block.status, Status::Done | Status::Error) {
            return None;
        }
        let fg = notation_color(palette);
        let key = InlineCacheKey {
            block_id: block.id,
            version,
            width,
            cell_px,
            fg,
        };
        if !self.entries.contains_key(&key) {
            // Simple cap to keep memory bounded across long sessions.
            if self.entries.len() >= 1024 {
                self.entries.clear();
            }
            let built = build_inline_entry(block.content_type, &block.content, width, cell_px, palette, fg);
            self.entries.insert(key, built);
        }
        self.entries.get(&key).and_then(|v| v.as_ref())
    }
}

fn wrap_prose_lines(text: &str, width: u16, palette: &Palette) -> Vec<Line<'static>> {
    if text.is_empty() {
        return Vec::new();
    }
    let w = usize::from(width.max(1));
    let mut out = Vec::new();
    for para in text.lines() {
        if para.trim().is_empty() {
            out.push(Line::default());
            continue;
        }
        let mut cur = String::new();
        for word in para.split_whitespace() {
            if !cur.is_empty() && cur.chars().count() + 1 + word.chars().count() > w {
                out.push(Line::from(Span::styled(std::mem::take(&mut cur), palette.compose())));
            }
            if !cur.is_empty() {
                cur.push(' ');
            }
            cur.push_str(word);
        }
        if !cur.is_empty() {
            out.push(Line::from(Span::styled(cur, palette.compose())));
        }
    }
    out
}

fn build_inline_entry(
    content_type: ContentType,
    text: &str,
    width: u16,
    cell_px: CellPixels,
    palette: &Palette,
    fg: &'static str,
) -> Option<InlineScoreEntry> {
    let how = openable_abc(content_type, text)?;
    let (before_lines, after_lines) = match &how {
        OpenAbc::Fenced { before, after, .. } => (
            wrap_prose_lines(before, width, palette),
            wrap_prose_lines(after, width, palette),
        ),
        _ => (Vec::new(), Vec::new()),
    };
    let raw = how.source(text);
    let tunes = parse_all_tunes(raw).ok()?;
    let tune = tunes.first()?;
    let meta = ScoreMeta::from_tune(tune, raw);
    let mut systems = engrave_measured_systems(
        raw,
        0,
        tune,
        width,
        INLINE_MAX_ROWS_PER_SYSTEM,
        cell_px,
        fg,
    );
    if systems.is_empty() {
        return None;
    }
    let total_systems = systems.len();
    let truncated = total_systems > INLINE_MAX_SYSTEMS || tunes.len() > 1;
    systems.truncate(INLINE_MAX_SYSTEMS);

    let tune_note = if tunes.len() > 1 {
        format!(" · tune 1/{}", tunes.len())
    } else {
        String::new()
    };
    let header = Line::from(Span::styled(
        format!("{}{tune_note}  (Ctrl+A s)", meta.summary_line("score")),
        palette.score_heading(),
    ));
    let footer = if total_systems > INLINE_MAX_SYSTEMS {
        let more = total_systems - INLINE_MAX_SYSTEMS;
        Some(Line::from(Span::styled(
            format!(
                "… +{more} more {} (Ctrl+A s to open full score)",
                if more == 1 { "system" } else { "systems" }
            ),
            palette.score_dim(),
        )))
    } else {
        None
    };

    let sys_rows: usize = systems.iter().map(|s| usize::from(s.rows)).sum();
    let total_rows =
        before_lines.len() + 1 + sys_rows + usize::from(footer.is_some()) + after_lines.len();
    Some(InlineScoreEntry {
        before_lines,
        header,
        systems,
        footer,
        after_lines,
        raw_abc: raw.to_string(),
        truncated,
        total_rows,
    })
}

/// Render an ABC block inline for the conversation transcript (Slice 2).
///
/// Convenience helper for direct unit tests; `render.rs` uses [`InlineScoreCache`]
/// so measurement never triggers rasterization or off-screen uploads.
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
    let fg = notation_color(palette);
    let entry = build_inline_entry(content_type, text, width, kitty.cell_px, palette, fg)?;
    Some(entry.render_slice(width, 0, entry.total_rows, kitty))
}

/// Cached per-system engravings for one tune at a fixed `(width, cell_px, fg)`.
struct ScoreEngraveCache {
    tune_idx: usize,
    width: u16,
    cell_px: CellPixels,
    fg: &'static str,
    systems: Vec<EngravedSystem>,
    /// Total rows across all systems + 1-row gaps between systems.
    total_rows: usize,
}

/// Full-screen ABC score viewer (`ScreenMode::Score`, Slice 1).
pub struct ScoreScreen {
    pub title: String,
    pub raw_abc: String,
    parsed: Result<Vec<Tune>, String>,
    tune_idx: usize,
    engrave_cache: Option<ScoreEngraveCache>,
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
        let parsed = parse_all_tunes(&raw_abc);
        Self {
            title,
            raw_abc,
            parsed,
            tune_idx: 0,
            engrave_cache: None,
            show_raw: false,
            top: 0,
            body_h: DEFAULT_BODY_LINES,
        }
    }

    pub fn body_h(&self) -> usize {
        self.body_h
    }

    pub fn tune_count(&self) -> usize {
        self.parsed.as_ref().map_or(0, Vec::len)
    }

    pub fn step_tune(&mut self, delta: isize) -> bool {
        let count = self.tune_count();
        if count <= 1 {
            return false;
        }
        let next = (self.tune_idx as isize + delta).rem_euclid(count as isize) as usize;
        if next != self.tune_idx {
            self.tune_idx = next;
            self.top = 0;
            self.engrave_cache = None;
            true
        } else {
            false
        }
    }

    pub fn scroll(&mut self, delta: isize) {
        if delta < 0 {
            self.top = self.top.saturating_sub(delta.unsigned_abs());
        } else {
            self.top = self.top.saturating_add(delta as usize);
        }
    }

    fn active_meta(&self) -> Option<ScoreMeta> {
        let tunes = self.parsed.as_ref().ok()?;
        let tune = tunes.get(self.tune_idx)?;
        Some(ScoreMeta::from_tune(tune, &self.raw_abc))
    }

    /// Ensure `engrave_cache` matches `(tune_idx, width, cell_px, fg)` — no
    /// work is done when scrolling at a constant terminal size.
    fn ensure_engraved(&mut self, width: u16, cell_px: CellPixels, fg: &'static str) -> bool {
        let hit = self.engrave_cache.as_ref().is_some_and(|c| {
            c.tune_idx == self.tune_idx
                && c.width == width
                && c.cell_px == cell_px
                && c.fg == fg
        });
        if hit {
            return true;
        }
        let Ok(tunes) = &self.parsed else {
            return false;
        };
        let Some(tune) = tunes.get(self.tune_idx) else {
            return false;
        };
        let systems = engrave_measured_systems(
            &self.raw_abc,
            self.tune_idx,
            tune,
            width,
            FULLSCREEN_MAX_ROWS_PER_SYSTEM,
            cell_px,
            fg,
        );
        if systems.is_empty() {
            self.engrave_cache = None;
            return false;
        }
        let total_rows: usize = systems.iter().map(|s| usize::from(s.rows)).sum::<usize>()
            + systems.len().saturating_sub(1);
        self.engrave_cache = Some(ScoreEngraveCache {
            tune_idx: self.tune_idx,
            width,
            cell_px,
            fg,
            systems,
            total_rows,
        });
        true
    }

    /// Build one frame of the full-screen score viewer.
    ///
    /// Only systems intersecting `self.top..self.top + body_h` are uploaded to
    /// Kitty, so multi-page scores never churn or evict visible images.
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

        let fg = notation_color(palette);
        let use_kitty = !self.show_raw
            && kitty.enabled
            && width >= 20
            && self.ensure_engraved(width, kitty.cell_px, fg);

        let (visible_body, total_rows, rendered_kitty) = if use_kitty {
            let cache = self.engrave_cache.as_ref().expect("just populated");
            let total = cache.total_rows;
            let max_top = total.saturating_sub(body_h);
            if self.top > max_top {
                self.top = max_top;
            }
            let win_start = self.top;
            let win_end = win_start.saturating_add(body_h);
            let mut rows_out = Vec::with_capacity(body_h);
            let mut cur_row = 0usize;
            let mut any_uploaded = false;
            for (idx, sys) in cache.systems.iter().enumerate() {
                if idx > 0 {
                    if cur_row >= win_start && cur_row < win_end {
                        rows_out.push(Line::default());
                    }
                    cur_row += 1;
                }
                let sys_rows = usize::from(sys.rows);
                let sys_end = cur_row + sys_rows;
                if sys_end > win_start && cur_row < win_end {
                    let skip = win_start.saturating_sub(cur_row);
                    let take = (win_end - cur_row.max(win_start)).min(sys_rows.saturating_sub(skip));
                    if let Some(img) = kitty.ensure_svg(
                        sys.key,
                        &sys.svg,
                        width,
                        FULLSCREEN_MAX_ROWS_PER_SYSTEM,
                    ) {
                        any_uploaded = true;
                        let mut pl = placeholder_lines(img.id, img.cols, img.rows);
                        // Guard in case rasterized rows differed by 1.
                        while pl.len() < sys_rows {
                            pl.push(Line::default());
                        }
                        rows_out.extend(pl.into_iter().skip(skip).take(take));
                    } else {
                        for _ in 0..take {
                            rows_out.push(Line::default());
                        }
                    }
                }
                cur_row = sys_end;
                if cur_row >= win_end {
                    break;
                }
            }
            if any_uploaded {
                (rows_out, total, true)
            } else {
                let raw = self.raw_body_lines(palette, kitty.enabled);
                let max_top = raw.len().saturating_sub(body_h);
                if self.top > max_top {
                    self.top = max_top;
                }
                let total = raw.len();
                let slice: Vec<Line<'static>> = raw.into_iter().skip(self.top).take(body_h).collect();
                (slice, total, false)
            }
        } else {
            let raw = self.raw_body_lines(palette, kitty.enabled);
            let max_top = raw.len().saturating_sub(body_h);
            if self.top > max_top {
                self.top = max_top;
            }
            let total = raw.len();
            let slice: Vec<Line<'static>> = raw.into_iter().skip(self.top).take(body_h).collect();
            (slice, total, false)
        };

        let mode_tag = if self.show_raw {
            "raw"
        } else if rendered_kitty {
            "kitty"
        } else {
            "text"
        };
        let tune_suffix = if self.tune_count() > 1 {
            format!(" · tune {}/{}", self.tune_idx + 1, self.tune_count())
        } else {
            String::new()
        };
        let header_text = match self.active_meta() {
            Some(m) => format!(" {}{tune_suffix}  [{mode_tag}]", m.summary_line(&self.title)),
            None => format!(" ♪ {}{tune_suffix}  [{mode_tag}]", self.title),
        };
        let header = Line::from(Span::styled(
            header_text,
            palette.score_heading().add_modifier(Modifier::BOLD),
        ));

        let mut out = Vec::with_capacity(height);
        out.push(header);
        out.extend(visible_body);
        while out.len() < height - 1 {
            out.push(Line::default());
        }

        let tune_hint = if self.tune_count() > 1 {
            " · [/] tune"
        } else {
            ""
        };
        let footer_text = format!(
            " j/k scroll{tune_hint} · r {} · y yank · q/Esc close  ({}/{})",
            if self.show_raw { "score" } else { "raw" },
            (self.top + 1).min(total_rows.max(1)),
            total_rows.max(1),
        );
        out.push(Line::from(Span::styled(footer_text, palette.score_dim())));
        out
    }

    fn raw_body_lines(&self, palette: &Palette, kitty_enabled: bool) -> Vec<Line<'static>> {
        let mut lines = Vec::new();
        if let Err(err) = &self.parsed {
            lines.push(Line::from(Span::styled(
                format!("abc parse error: {err}"),
                palette.alarm(),
            )));
            lines.push(Line::default());
        } else if !self.show_raw && !kitty_enabled {
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
            } else if trimmed.len() >= 2
                && trimmed.as_bytes()[1] == b':'
                && trimmed.as_bytes()[0].is_ascii_alphabetic()
            {
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
        KeyCode::Char('[') => {
            if screen.step_tune(-1) {
                ScoreKey::Scrolled
            } else {
                ScoreKey::Ignored
            }
        }
        KeyCode::Char(']') => {
            if screen.step_tune(1) {
                ScoreKey::Scrolled
            } else {
                ScoreKey::Ignored
            }
        }
        KeyCode::Char('j') | KeyCode::Down => {
            screen.scroll(1);
            ScoreKey::Scrolled
        }
        KeyCode::Char('k') | KeyCode::Up => {
            screen.scroll(-1);
            ScoreKey::Scrolled
        }
        KeyCode::Char('d') if ctrl => {
            screen.scroll(half);
            ScoreKey::Scrolled
        }
        KeyCode::Char('u') if ctrl => {
            screen.scroll(-half);
            ScoreKey::Scrolled
        }
        KeyCode::PageDown | KeyCode::Char(' ') => {
            screen.scroll(page);
            ScoreKey::Scrolled
        }
        KeyCode::PageUp => {
            screen.scroll(-page);
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

        let fenced = format!("Here is a tune:\n```abc\n{SAMPLE_ABC}```\nI raised the tempo.\n");
        let Some(OpenAbc::Fenced { before, inner, after }) = openable_abc(ContentType::Markdown, &fenced) else {
            panic!("expected fenced abc");
        };
        assert_eq!(before, "Here is a tune:");
        assert!(inner.starts_with("X:1"));
        assert_eq!(after, "I raised the tempo.");

        // Bare ABC in Markdown prose (e.g. explaining ABC syntax) is NOT hijacked.
        assert_eq!(openable_abc(ContentType::Markdown, SAMPLE_ABC), None);
    }

    #[test]
    fn mixed_markdown_preserves_prose_and_plain_lines_omit_placeholders() {
        let palette = Palette::tokyo_midnight();
        let mut kitty = KittyStore::enabled(CellPixels { width: 10, height: 20 });
        let md = format!("Intro prose.\n```abc\n{SAMPLE_ABC}```\nOutro prose.\n");
        let entry = build_inline_entry(
            ContentType::Markdown,
            &md,
            80,
            kitty.cell_px,
            &palette,
            notation_color(&palette),
        )
        .expect("builds inline entry");

        // Measurement queued zero Kitty uploads.
        assert!(kitty.take_pending_wire().is_empty());

        let drawn = entry.render_slice(80, 0, entry.total_rows, &mut kitty);
        assert_eq!(drawn.len(), entry.total_rows);
        let first: String = drawn[0].spans.iter().map(|s| s.content.as_ref()).collect();
        let last: String = drawn.last().unwrap().spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(first.contains("Intro prose."), "got {first:?}");
        assert!(last.contains("Outro prose."), "got {last:?}");

        // Plain lines for copy-mode search/yank match total_rows and never leak U+10EEEE.
        let plain = entry.render_plain_lines();
        assert_eq!(plain.len(), entry.total_rows);
        assert!(plain.iter().all(|l| l.spans.iter().all(|s| !s.content.contains(PLACEHOLDER_CHAR))));
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

    #[test]
    fn score_screen_g_then_j_does_not_overflow_and_uploads_lazily() {
        let long_abc = format!(
            "X:1\nT:Long\nM:4/4\nL:1/8\nK:C\n{}\n",
            "CDEF GABc|cBAG FEDC|".repeat(24)
        );
        let palette = Palette::tokyo_midnight();
        let mut kitty = KittyStore::enabled(CellPixels { width: 10, height: 20 });
        let mut screen = ScoreScreen::open("block #2", long_abc);

        // First frame at a tiny height only uploads the top visible system(s).
        let _ = screen.frame(60, 8, &palette, &mut kitty);
        let sys_count = screen.engrave_cache.as_ref().map_or(0, |c| c.systems.len());
        assert!(sys_count >= 3, "expected multiple systems, got {sys_count}");
        let initial_uploads = kitty.take_pending_wire().len() / 2;
        assert!(
            initial_uploads < sys_count,
            "lazy upload should not upload all {sys_count} systems for an 8-row viewport (uploaded {initial_uploads})"
        );

        // `G` followed immediately by `j` before the next frame must not overflow.
        handle_key(&mut screen, &KeyEvent::new(KeyCode::Char('G'), KeyModifiers::NONE), 6);
        handle_key(&mut screen, &KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE), 6);
        let bottom = screen.frame(60, 8, &palette, &mut kitty);
        assert_eq!(bottom.len(), 8);
    }

    #[test]
    fn score_screen_steps_between_multiple_tunes() {
        let two_tunes = "X:1\nT:First\nK:C\nCDEF|\n\nX:2\nT:Second\nK:G\nGABc|\n";
        let palette = Palette::tokyo_midnight();
        let mut kitty = KittyStore::disabled();
        let mut screen = ScoreScreen::open("block #3", two_tunes);
        assert_eq!(screen.tune_count(), 2);

        let f1 = screen.frame(80, 12, &palette, &mut kitty);
        let h1: String = f1[0].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(h1.contains("tune 1/2"), "got {h1:?}");

        assert_eq!(
            handle_key(&mut screen, &KeyEvent::new(KeyCode::Char(']'), KeyModifiers::NONE), 10),
            ScoreKey::Scrolled
        );
        let f2 = screen.frame(80, 12, &palette, &mut kitty);
        let h2: String = f2[0].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(h2.contains("tune 2/2"), "got {h2:?}");
    }
}
