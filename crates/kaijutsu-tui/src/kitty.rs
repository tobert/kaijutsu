//! Kitty graphics protocol encoder, Unicode placeholder renderer, and SVG rasterizer.
//!
//! Zero-terminal-query invariant: this module NEVER writes a query escape
//! (`a=q`, `CSI ? u`, DSR) to stdout. Support is decided from environment
//! variables (or explicit CLI flags), and cell pixel dimensions come from
//! the local `TIOCGWINSZ` ioctl (zero bytes on the wire).
//!
//! Images are transmitted once with `a=t,q=2` (`q=2` suppresses all terminal
//! replies so Kitty never injects responses into crossterm's key reader) and
//! placed via Unicode placeholders (`a=p,U=1` + `U+10EEEE` cells). Because
//! placeholders live in ratatui's normal cell buffer, scrolling, viewport
//! clipping, overlays, and full-screen surfaces work without cursor-escape
//! conflicts.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, OnceLock};

use base64::Engine as _;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};

/// Base character for Kitty Unicode placeholders.
pub const PLACEHOLDER_CHAR: char = '\u{10EEEE}';

/// Maximum pixel dimension for any rasterized image (guard against runaway
/// allocations on huge terminals or malformed SVGs).
pub const MAX_RASTER_DIM: u32 = 4096;

/// Default maximum number of cached Kitty images before LRU eviction.
const DEFAULT_MAX_IMAGES: usize = 64;

/// Maximum raw base64 bytes per Kitty graphics chunk (`4096` per spec).
const CHUNK_SIZE: usize = 4096;

/// Official Kitty row/column combining diacritics (`rowcolumn-diacritics.txt`),
/// first 64 entries (all Unicode `Mn`, `ccc=230`, no decomposition).
///
/// Each individual engraved system image is well under 64 rows tall, and
/// columns auto-increment from column 0 (`DIACRITICS[0]`) across consecutive
/// `U+10EEEE` cells on the same row.
pub const DIACRITICS: [char; 64] = [
    '\u{0305}', '\u{030D}', '\u{030E}', '\u{0310}', '\u{0312}', '\u{033D}', '\u{033E}', '\u{033F}',
    '\u{0346}', '\u{034A}', '\u{034B}', '\u{034C}', '\u{0350}', '\u{0351}', '\u{0352}', '\u{0357}',
    '\u{035B}', '\u{0363}', '\u{0364}', '\u{0365}', '\u{0366}', '\u{0367}', '\u{0368}', '\u{0369}',
    '\u{036A}', '\u{036B}', '\u{036C}', '\u{036D}', '\u{036E}', '\u{036F}', '\u{0483}', '\u{0484}',
    '\u{0485}', '\u{0486}', '\u{0487}', '\u{0592}', '\u{0593}', '\u{0594}', '\u{0595}', '\u{0597}',
    '\u{0598}', '\u{0599}', '\u{059C}', '\u{059D}', '\u{059E}', '\u{059F}', '\u{05A0}', '\u{05A1}',
    '\u{05A8}', '\u{05A9}', '\u{05AB}', '\u{05AC}', '\u{05AF}', '\u{05C4}', '\u{0610}', '\u{0611}',
    '\u{0612}', '\u{0613}', '\u{0614}', '\u{0615}', '\u{0616}', '\u{0617}', '\u{0657}', '\u{0658}',
];

/// Pixel dimensions of one terminal character cell.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CellPixels {
    pub width: u16,
    pub height: u16,
}

impl Default for CellPixels {
    fn default() -> Self {
        Self { width: 10, height: 20 }
    }
}

/// Detect Kitty Unicode-placeholder graphics support without querying the terminal.
///
/// Obeys `cli_override` when set; otherwise enables for plain Kitty
/// (`TERM=xterm-kitty` or `KITTY_WINDOW_ID`) and Ghostty (`TERM_PROGRAM=ghostty`)
/// when not nested inside GNU `screen` or `tmux`.
pub fn detect_kitty_graphics(cli_override: Option<bool>) -> bool {
    let term = std::env::var("TERM").ok();
    let term_program = std::env::var("TERM_PROGRAM").ok();
    let kitty_window_id = std::env::var("KITTY_WINDOW_ID").ok();
    let tmux = std::env::var("TMUX").ok();
    let sty = std::env::var("STY").ok();
    detect_kitty_graphics_from_env(
        term.as_deref(),
        term_program.as_deref(),
        kitty_window_id.as_deref(),
        tmux.as_deref(),
        sty.as_deref(),
        cli_override,
    )
}

/// Pure env-based detection for unit testing.
pub fn detect_kitty_graphics_from_env(
    term: Option<&str>,
    term_program: Option<&str>,
    kitty_window_id: Option<&str>,
    tmux: Option<&str>,
    sty: Option<&str>,
    cli_override: Option<bool>,
) -> bool {
    if let Some(explicit) = cli_override {
        return explicit;
    }
    let in_multiplexer = tmux.is_some_and(|v| !v.is_empty())
        || sty.is_some_and(|v| !v.is_empty())
        || term.is_some_and(|t| t.starts_with("screen") || t.starts_with("tmux"));
    if in_multiplexer {
        return false;
    }
    term == Some("xterm-kitty")
        || kitty_window_id.is_some_and(|v| !v.is_empty())
        || term_program == Some("ghostty")
}

/// Read cell pixel dimensions via `TIOCGWINSZ` (local kernel ioctl, zero stdout writes).
/// Falls back to `10x20` when pixel dimensions are unavailable.
pub fn query_cell_pixels() -> CellPixels {
    #[cfg(unix)]
    {
        let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
        let rc = unsafe { libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, &mut ws) };
        if rc == 0 && ws.ws_col > 0 && ws.ws_row > 0 && ws.ws_xpixel > 0 && ws.ws_ypixel > 0 {
            let width = (ws.ws_xpixel / ws.ws_col).clamp(4, 64);
            let height = (ws.ws_ypixel / ws.ws_row).clamp(8, 128);
            return CellPixels { width, height };
        }
    }
    CellPixels::default()
}

/// Encode a straight-RGBA8 image upload (`a=t,f=32,q=2`) split into 4096-byte base64 chunks.
pub fn encode_upload_rgba(id: u32, width: u32, height: u32, rgba: &[u8]) -> String {
    let encoded = base64::engine::general_purpose::STANDARD.encode(rgba);
    let bytes = encoded.as_bytes();
    let mut out = String::with_capacity(encoded.len() + 128);
    if bytes.is_empty() {
        out.push_str(&format!(
            "\x1b_Ga=t,f=32,s={width},v={height},i={id},t=d,q=2,m=0;\x1b\\"
        ));
        return out;
    }
    let chunks: Vec<&[u8]> = bytes.chunks(CHUNK_SIZE).collect();
    let last = chunks.len() - 1;
    for (idx, chunk) in chunks.iter().enumerate() {
        let more = u8::from(idx < last);
        // SAFETY: base64 output is strictly ASCII.
        let payload = std::str::from_utf8(chunk).expect("base64 is valid utf-8");
        if idx == 0 {
            out.push_str(&format!(
                "\x1b_Ga=t,f=32,s={width},v={height},i={id},t=d,q=2,m={more};{payload}\x1b\\"
            ));
        } else {
            out.push_str(&format!("\x1b_Gm={more};{payload}\x1b\\"));
        }
    }
    out
}

/// Encode a virtual placement (`a=p,U=1,q=2`) for Unicode placeholders.
pub fn encode_virtual_placement(id: u32, cols: u16, rows: u16) -> String {
    format!("\x1b_Ga=p,U=1,i={id},c={cols},r={rows},q=2;\x1b\\")
}

/// Encode deletion of one image by ID (`a=d,d=i,q=2`).
pub fn encode_delete_image(id: u32) -> String {
    format!("\x1b_Ga=d,d=i,i={id},q=2;\x1b\\")
}

/// Encode deletion of all Kitty images on the current screen (`a=d,d=A,q=2`).
pub fn encode_delete_all() -> &'static str {
    "\x1b_Ga=d,d=A,q=2;\x1b\\"
}

/// Build `rows` ratatui lines of Kitty Unicode placeholders (`U+10EEEE`) for `id`.
///
/// The 24-bit image ID is encoded in the foreground RGB color; cell `(r, 0)`
/// carries `(DIACRITICS[r], DIACRITICS[0])` and cells `(r, 1..cols)` carry
/// bare `U+10EEEE` so Kitty auto-increments the column across the row.
pub fn placeholder_lines(id: u32, cols: u16, rows: u16) -> Vec<Line<'static>> {
    if cols == 0 || rows == 0 {
        return Vec::new();
    }
    let fg = Color::Rgb(
        ((id >> 16) & 0xFF) as u8,
        ((id >> 8) & 0xFF) as u8,
        (id & 0xFF) as u8,
    );
    let style = Style::default().fg(fg);
    let capped_rows = rows.min(DIACRITICS.len() as u16);
    let mut lines = Vec::with_capacity(usize::from(capped_rows));
    for r in 0..capped_rows {
        let mut s = String::with_capacity(usize::from(cols) * 4 + 6);
        s.push(PLACEHOLDER_CHAR);
        s.push(DIACRITICS[usize::from(r)]);
        s.push(DIACRITICS[0]);
        for _ in 1..cols {
            s.push(PLACEHOLDER_CHAR);
        }
        lines.push(Line::from(Span::styled(s, style)));
    }
    lines
}

/// Shared system font database for `usvg` `<text>` elements (score titles,
/// tempo markings, chord symbols). Loaded once lazily.
fn svg_fontdb() -> Arc<usvg::fontdb::Database> {
    static DB: OnceLock<Arc<usvg::fontdb::Database>> = OnceLock::new();
    DB.get_or_init(|| {
        let mut db = usvg::fontdb::Database::new();
        db.load_system_fonts();
        Arc::new(db)
    })
    .clone()
}

/// Rasterized SVG result sized to terminal cells.
#[derive(Debug, Clone)]
pub struct RasterizedSvg {
    pub px_w: u32,
    pub px_h: u32,
    pub cols: u16,
    pub rows: u16,
    pub rgba: Vec<u8>,
}

/// Parse and rasterize an SVG string into a straight-alpha RGBA8 buffer fitted
/// within `max_cols × max_rows` cells at `cell_px` resolution.
pub fn rasterize_svg(
    svg: &str,
    max_cols: u16,
    max_rows: u16,
    cell_px: CellPixels,
) -> Option<RasterizedSvg> {
    if max_cols == 0 || max_rows == 0 {
        return None;
    }
    let opts = usvg::Options {
        fontdb: svg_fontdb(),
        ..Default::default()
    };
    let tree = usvg::Tree::from_str(svg, &opts).ok()?;
    let size = tree.size();
    let svg_w = size.width();
    let svg_h = size.height();
    if svg_w <= 0.0 || svg_h <= 0.0 {
        return None;
    }

    let max_px_w = (u32::from(max_cols) * u32::from(cell_px.width)).min(MAX_RASTER_DIM);
    let max_px_h = (u32::from(max_rows) * u32::from(cell_px.height)).min(MAX_RASTER_DIM);
    if max_px_w == 0 || max_px_h == 0 {
        return None;
    }

    // Scale to fill available width (scores have aspect ratio, not fixed CSS px),
    // clamped by max_px_h so tall scores never exceed max_rows.
    let scale_w = max_px_w as f32 / svg_w;
    let scale_h = max_px_h as f32 / svg_h;
    let scale = scale_w.min(scale_h);
    let px_w = ((svg_w * scale).round() as u32).clamp(1, max_px_w);
    let px_h = ((svg_h * scale).round() as u32).clamp(1, max_px_h);

    let cols = (px_w.div_ceil(u32::from(cell_px.width)) as u16).clamp(1, max_cols);
    let rows = (px_h.div_ceil(u32::from(cell_px.height)) as u16).clamp(1, max_rows.min(DIACRITICS.len() as u16));

    // Render into the exact cell-grid pixel box so Kitty's 1:1 placement has no
    // aspect distortion (transparent padding fills the few slack pixels).
    let canvas_w = (u32::from(cols) * u32::from(cell_px.width)).min(MAX_RASTER_DIM);
    let canvas_h = (u32::from(rows) * u32::from(cell_px.height)).min(MAX_RASTER_DIM);
    let mut pixmap = tiny_skia::Pixmap::new(canvas_w, canvas_h)?;
    let transform = tiny_skia::Transform::from_scale(scale, scale);
    resvg::render(&tree, transform, &mut pixmap.as_mut());

    let rgba = unpremultiply_rgba(pixmap.data());
    Some(RasterizedSvg {
        px_w: canvas_w,
        px_h: canvas_h,
        cols,
        rows,
        rgba,
    })
}

/// Convert tiny-skia premultiplied RGBA8 into straight-alpha RGBA8 for Kitty `f=32`.
fn unpremultiply_rgba(premul: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(premul.len());
    for &[r, g, b, a] in premul.as_chunks::<4>().0 {
        match a {
            0 => out.extend_from_slice(&[0, 0, 0, 0]),
            255 => out.extend_from_slice(&[r, g, b, 255]),
            _ => {
                let af = f32::from(a) / 255.0;
                out.push((f32::from(r) / af).min(255.0).round() as u8);
                out.push((f32::from(g) / af).min(255.0).round() as u8);
                out.push((f32::from(b) / af).min(255.0).round() as u8);
                out.push(a);
            }
        }
    }
    out
}

/// Metadata for one uploaded Kitty image.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CachedImage {
    pub id: u32,
    pub cols: u16,
    pub rows: u16,
}

/// Per-session Kitty image cache and pending wire-command queue.
pub struct KittyStore {
    pub enabled: bool,
    pub cell_px: CellPixels,
    next_id: u32,
    cache: HashMap<u64, CachedImage>,
    lru: VecDeque<u64>,
    pending_wire: Vec<String>,
    max_images: usize,
}

impl Default for KittyStore {
    fn default() -> Self {
        Self::disabled()
    }
}

impl KittyStore {
    /// Disabled store — used by `TestBackend`, `vt100` tests, and non-Kitty terminals.
    pub fn disabled() -> Self {
        Self {
            enabled: false,
            cell_px: CellPixels::default(),
            next_id: 1,
            cache: HashMap::new(),
            lru: VecDeque::new(),
            pending_wire: Vec::new(),
            max_images: DEFAULT_MAX_IMAGES,
        }
    }

    /// Enabled store with the given cell pixel dimensions.
    pub fn enabled(cell_px: CellPixels) -> Self {
        Self {
            enabled: true,
            cell_px,
            ..Self::disabled()
        }
    }

    /// Ensure an SVG is rasterized and uploaded under `key`, returning its
    /// cached placement metadata (or `None` when disabled or rasterization fails).
    pub fn ensure_svg(
        &mut self,
        key: u64,
        svg: &str,
        max_cols: u16,
        max_rows: u16,
    ) -> Option<CachedImage> {
        if !self.enabled {
            return None;
        }
        if let Some(&hit) = self.cache.get(&key) {
            return Some(hit);
        }
        let raster = rasterize_svg(svg, max_cols, max_rows, self.cell_px)?;
        let id = self.alloc_id();
        self.pending_wire
            .push(encode_upload_rgba(id, raster.px_w, raster.px_h, &raster.rgba));
        self.pending_wire
            .push(encode_virtual_placement(id, raster.cols, raster.rows));
        let entry = CachedImage {
            id,
            cols: raster.cols,
            rows: raster.rows,
        };
        self.cache.insert(key, entry);
        self.lru.push_back(key);
        while self.lru.len() > self.max_images {
            if let Some(old_key) = self.lru.pop_front()
                && let Some(old) = self.cache.remove(&old_key)
            {
                self.pending_wire.push(encode_delete_image(old.id));
            }
        }
        Some(entry)
    }

    /// Drain queued Kitty wire commands to flush before `terminal.draw(...)`.
    pub fn take_pending_wire(&mut self) -> Vec<String> {
        std::mem::take(&mut self.pending_wire)
    }

    /// Drop all cached images (e.g. on terminal resize when cell pixels change)
    /// and queue a delete-all command.
    pub fn clear(&mut self) {
        if !self.cache.is_empty() {
            self.cache.clear();
            self.lru.clear();
            if self.enabled {
                self.pending_wire.push(encode_delete_all().to_string());
            }
        }
    }

    fn alloc_id(&mut self) -> u32 {
        let id = self.next_id;
        // Stay within 24-bit RGB range (1..=0x00FF_FFFF) so foreground color
        // carries the whole image ID with no 3rd diacritic needed.
        self.next_id = if self.next_id >= 0x00FF_FFFF {
            1
        } else {
            self.next_id + 1
        };
        id
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_detection_enables_plain_kitty_and_ghostty_only() {
        assert!(detect_kitty_graphics_from_env(Some("xterm-kitty"), None, None, None, None, None));
        assert!(detect_kitty_graphics_from_env(Some("xterm-256color"), None, Some("1"), None, None, None));
        assert!(detect_kitty_graphics_from_env(Some("xterm-ghostty"), Some("ghostty"), None, None, None, None));
        // Multiplexers suppress auto-detection even when KITTY_WINDOW_ID is inherited.
        assert!(!detect_kitty_graphics_from_env(Some("screen.xterm-kitty"), None, Some("1"), None, Some("1234.pts"), None));
        assert!(!detect_kitty_graphics_from_env(Some("tmux-256color"), None, Some("1"), Some("/tmp/tmux"), None, None));
        // Plain xterm / vt100 off by default.
        assert!(!detect_kitty_graphics_from_env(Some("xterm-256color"), None, None, None, None, None));
        // Explicit CLI override wins either way.
        assert!(detect_kitty_graphics_from_env(Some("screen"), None, None, None, Some("1234"), Some(true)));
        assert!(!detect_kitty_graphics_from_env(Some("xterm-kitty"), None, Some("1"), None, None, Some(false)));
    }

    #[test]
    fn upload_chunks_at_4096_and_suppresses_responses() {
        let rgba = vec![0xAAu8; 4096]; // > 4096 base64 chars -> 2 chunks
        let wire = encode_upload_rgba(7, 32, 32, &rgba);
        assert!(wire.starts_with("\x1b_Ga=t,f=32,s=32,v=32,i=7,t=d,q=2,m=1;"));
        assert!(wire.contains("\x1b\\\x1b_Gm=0;"));
        assert!(wire.ends_with("\x1b\\"));
    }

    #[test]
    fn placeholder_lines_encode_rgb_id_and_diacritics() {
        let lines = placeholder_lines(0x010203, 4, 2);
        assert_eq!(lines.len(), 2);
        let span0 = &lines[0].spans[0];
        assert_eq!(span0.style.fg, Some(Color::Rgb(1, 2, 3)));
        let chars0: Vec<char> = span0.content.chars().collect();
        assert_eq!(chars0[0], PLACEHOLDER_CHAR);
        assert_eq!(chars0[1], DIACRITICS[0]);
        assert_eq!(chars0[2], DIACRITICS[0]);
        assert_eq!(chars0[3..], [PLACEHOLDER_CHAR; 3]);

        let chars1: Vec<char> = lines[1].spans[0].content.chars().collect();
        assert_eq!(chars1[1], DIACRITICS[1]);
        assert_eq!(chars1[2], DIACRITICS[0]);
    }

    #[test]
    fn rasterize_svg_and_store_caches_and_evicts() {
        let svg = r##"<svg xmlns="http://www.w3.org/2000/svg" width="200" height="80"><line x1="0" y1="40" x2="200" y2="40" stroke="#ffffff" stroke-width="2"/></svg>"##;
        let mut store = KittyStore::enabled(CellPixels { width: 10, height: 20 });
        store.max_images = 2;

        let img1 = store.ensure_svg(1, svg, 20, 10).expect("rasterizes");
        assert_eq!(img1.id, 1);
        assert_eq!(img1.cols, 20);
        assert_eq!(img1.rows, 4);
        assert_eq!(store.take_pending_wire().len(), 2, "upload + virtual placement");

        // Cache hit emits no new wire commands.
        let hit = store.ensure_svg(1, svg, 20, 10).expect("cached");
        assert_eq!(hit, img1);
        assert!(store.take_pending_wire().is_empty());

        // Inserting two more evicts key 1.
        let _ = store.ensure_svg(2, svg, 20, 10).unwrap();
        let _ = store.ensure_svg(3, svg, 20, 10).unwrap();
        let wire = store.take_pending_wire();
        assert!(wire.iter().any(|w| w == &encode_delete_image(1)));
    }
}
