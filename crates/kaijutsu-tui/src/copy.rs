//! The scrolled transcript — scrolling is copy mode (`docs/tui.md`,
//! "Scrolling is copy mode"). Leaving the live tail enters it; reaching the
//! tail again leaves it.
//!
//! There is no frozen snapshot. The transcript view the render pass already
//! produces *is* the buffer: a block still streaming grows under the reader,
//! and the view keeps its place by block and line rather than by row
//! ([`Anchor`]), so growth above or below does not move what is being read.
//!
//! While scrolled the transcript owns copy mode's keys — vi motions, `/` and
//! `?` with `n`/`N`, `v` marks a line range, `y` or `Enter` copies it, `q`
//! and `Esc` return to the tail. **Every other key snaps the view to the
//! tail and is handled as if typed there** ([`Outcome::Snap`]), which is
//! what makes `Space` the snap key and the draft always reachable.
//!
//! Pure: row counts and plain text in, cursor/anchor/search state out. No
//! terminal, no clock, no kernel — the way `picker.rs` and `compose.rs` stay
//! pure. The render pass owns the conversion between an [`Anchor`] and a row
//! ([`RowIndex`]); the caller owns the clipboard write and the full-text
//! search ([`find`]).

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use kaijutsu_types::BlockId;
use ratatui::style::Style;
use ratatui::text::{Line, Span};

use crate::present::Palette;

// ────────────────────────────────────────────────────────────────────────────
// Anchoring
// ────────────────────────────────────────────────────────────────────────────

/// A place in the transcript that survives a re-wrap, a block growing above
/// or below it, and the block itself going away.
///
/// The line is counted from the block's **first content row**, so a divider
/// or a blank row appearing above it — which a block inserted above, or a
/// tool call settling back out of the in-flight strip, can do — leaves the
/// reader on the same text. A negative line is one of those leading rows
/// itself.
///
/// The neighbours are what the anchor falls to when its own block is
/// excluded or edited away: the next block's first content row, or failing
/// that the previous block's last.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Anchor {
    pub block: BlockId,
    pub line: isize,
    prev: Option<BlockId>,
    next: Option<BlockId>,
}

/// One block's place in the transcript: where its rows start, how many of
/// them are the divider and the blank row above it, and how many there are
/// in all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RowBlock {
    pub block: BlockId,
    /// The blank row and the divider row drawn above the block's own text.
    pub leading: usize,
    /// Every row the block takes, the leading ones included.
    pub rows: usize,
}

/// Every transcript block in document order with the rows it occupies — the
/// map between an absolute transcript row and an [`Anchor`].
///
/// Built by the render pass, the only side that knows how a block wraps at
/// the current width (`render::transcript_window`).
#[derive(Debug, Default, Clone)]
pub struct RowIndex {
    blocks: Vec<RowBlock>,
    total: usize,
}

impl RowIndex {
    pub fn new(blocks: Vec<RowBlock>) -> Self {
        let total = blocks.iter().map(|b| b.rows).sum();
        Self { blocks, total }
    }

    /// Rows the whole transcript takes.
    pub fn total(&self) -> usize {
        self.total
    }

    /// Where a block's rows start, and the block itself.
    fn find(&self, block: BlockId) -> Option<(usize, RowBlock)> {
        let mut base = 0usize;
        for entry in &self.blocks {
            if entry.block == block {
                return Some((base, *entry));
            }
            base += entry.rows;
        }
        None
    }

    /// The anchor for an absolute row: the block it falls in, the line
    /// counted from that block's first content row, and the neighbours the
    /// anchor falls to if the block goes away. `None` past the last row.
    pub fn anchor_at(&self, row: usize) -> Option<Anchor> {
        let mut base = 0usize;
        for (i, entry) in self.blocks.iter().enumerate() {
            if row < base + entry.rows {
                return Some(Anchor {
                    block: entry.block,
                    line: row as isize - (base + entry.leading) as isize,
                    prev: i.checked_sub(1).map(|j| self.blocks[j].block),
                    next: self.blocks.get(i + 1).map(|b| b.block),
                });
            }
            base += entry.rows;
        }
        None
    }

    /// The absolute row an anchor names now, its own block only. The line
    /// clamps into the block's rows, which is what a re-wrap at a narrower
    /// width needs; `None` when the block is gone.
    pub fn row_of(&self, anchor: &Anchor) -> Option<usize> {
        let (base, entry) = self.find(anchor.block)?;
        let row = (base + entry.leading) as isize + anchor.line;
        let last = (base + entry.rows.saturating_sub(1)) as isize;
        Some(row.clamp(base as isize, last.max(base as isize)) as usize)
    }

    /// The row an anchor names, falling to its neighbours when its own block
    /// has been excluded or edited away: the next block's first content row,
    /// else the previous block's last row. `None` when none of the three
    /// survives, and the caller falls back to the row it last drew.
    pub fn row_of_or_neighbour(&self, anchor: &Anchor) -> Option<usize> {
        if let Some(row) = self.row_of(anchor) {
            return Some(row);
        }
        if let Some((base, entry)) = anchor.next.and_then(|b| self.find(b)) {
            return Some(base + entry.leading.min(entry.rows.saturating_sub(1)));
        }
        let (base, entry) = anchor.prev.and_then(|b| self.find(b))?;
        Some(base + entry.rows.saturating_sub(1))
    }
}

// ────────────────────────────────────────────────────────────────────────────
// The scrolled view
// ────────────────────────────────────────────────────────────────────────────

/// What the key path asked the next frame to do. The key path has no row
/// counts of its own — those are the render pass's — so a move is recorded
/// here and applied against fresh counts in [`Scrolled::settle`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Pending {
    /// Scroll the view by rows, the reader's line riding at its screen row.
    Rows(isize),
    /// Put the reader on this absolute row, scrolling the least that shows it.
    Row(usize),
}

impl Default for Pending {
    fn default() -> Self {
        Pending::Rows(0)
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
enum SearchDirection {
    #[default]
    Forward,
    Backward,
}

impl SearchDirection {
    fn glyph(self) -> char {
        match self {
            SearchDirection::Forward => '/',
            SearchDirection::Backward => '?',
        }
    }
}

/// The `/` or `?` bar, live while it is being typed.
#[derive(Debug, Clone)]
struct SearchPrompt {
    direction: SearchDirection,
    text: String,
}

#[derive(Debug, Default, Clone)]
struct SearchState {
    prompt: Option<SearchPrompt>,
    /// The committed pattern, case-insensitive substring — what the visible
    /// rows are highlighted against and what `n`/`N` step through.
    needle: Option<String>,
    direction: SearchDirection,
}

/// Where the reader is while the transcript is off the live tail.
#[derive(Debug, Clone)]
pub struct Scrolled {
    /// The top row, anchored by block and line. `None` until the first frame
    /// resolves one.
    top: Option<Anchor>,
    /// The top row of the last frame — the fallback when the anchored block
    /// is gone. `usize::MAX` means the tail, which the first frame clamps.
    top_row: usize,
    pending: Pending,
    /// The reader's line, counted from the top row. `usize::MAX` means the
    /// bottom row, which the first frame clamps.
    reader_offset: usize,
    /// `v`'s mark — the other end of the line range, the reader's line being
    /// the one end — anchored the same way the top is.
    mark: Option<Anchor>,
    mark_row: Option<usize>,
    /// Rows the transcript had and rows the view drew, last frame.
    total_rows: usize,
    body_h: usize,
    /// `g` pressed once, waiting for a second `g` (vi's `gg`).
    pending_g: bool,
    search: SearchState,
}

impl Scrolled {
    /// Leave the tail without moving: the view sits exactly where the
    /// following view left it, and the reader is on its last row — the way
    /// tmux enters copy mode at the current screen.
    ///
    /// `body_h` is the transcript's current height, so a page step taken
    /// before the first scrolled frame is the right size.
    pub fn entering(body_h: usize) -> Self {
        Self {
            top: None,
            top_row: usize::MAX,
            pending: Pending::default(),
            reader_offset: usize::MAX,
            mark: None,
            mark_row: None,
            total_rows: 0,
            body_h: body_h.max(1),
            pending_g: false,
            search: SearchState::default(),
        }
    }

    /// Ask the next frame to scroll by `rows` — how `Up` and `PageUp` leave
    /// the tail, one line and one screen.
    pub fn scroll(&mut self, rows: isize) {
        self.pending = match self.pending {
            Pending::Rows(d) => Pending::Rows(d.saturating_add(rows)),
            Pending::Row(_) => Pending::Rows(rows),
        };
    }

    /// A screen's worth of rows, as the last frame drew it.
    pub fn page(&self) -> usize {
        self.body_h
    }

    /// Put the reader on an absolute row — what a committed search or a
    /// stepped match asks for once the caller has found it ([`find`]).
    pub fn jump_to(&mut self, row: usize) {
        self.pending = Pending::Row(row);
    }

    /// The reader's absolute row, as the last frame settled it.
    pub fn reader_row(&self) -> usize {
        self.top_row.saturating_add(self.reader_offset)
    }

    /// The inclusive row range `y` copies: the marked range, or the reader's
    /// own line when nothing is marked.
    pub fn range(&self) -> (usize, usize) {
        let cursor = self.reader_row();
        match self.mark_row {
            Some(mark) => (mark.min(cursor), mark.max(cursor)),
            None => (cursor, cursor),
        }
    }

    /// Settle the view against this frame's row counts and return the
    /// absolute row it starts at.
    ///
    /// The anchor is resolved first, so a block that grew above the reader
    /// moves the row number without moving the screen; the key path's
    /// pending move is applied to that fresh position, and the anchor and
    /// the mark are re-taken from where the view landed.
    pub fn settle(&mut self, index: &RowIndex, body_h: usize) -> usize {
        let total = index.total();
        let body_h = body_h.max(1);
        let last_top = total.saturating_sub(body_h);
        let base = self
            .top
            .as_ref()
            .and_then(|anchor| index.row_of_or_neighbour(anchor))
            .unwrap_or(self.top_row)
            .min(last_top);

        let (start, reader_offset) = match std::mem::take(&mut self.pending) {
            Pending::Rows(delta) => (base.saturating_add_signed(delta).min(last_top), self.reader_offset),
            Pending::Row(target) => {
                let target = target.min(total.saturating_sub(1));
                let start = if target < base {
                    target
                } else if target >= base + body_h {
                    target + 1 - body_h
                } else {
                    base
                }
                .min(last_top);
                (start, target - start)
            }
        };

        self.top_row = start;
        self.total_rows = total;
        self.body_h = body_h;
        self.reader_offset = reader_offset
            .min(body_h - 1)
            .min(total.saturating_sub(start).saturating_sub(1));
        self.top = index.anchor_at(start);
        // A mark whose block is gone is gone with it: there is no line left
        // to copy, and a range that silently slid onto a neighbour would
        // yank text nobody marked.
        match (self.mark, self.mark_row) {
            (Some(anchor), _) => {
                self.mark_row = index.row_of(&anchor);
                if self.mark_row.is_none() {
                    self.mark = None;
                }
            }
            (None, Some(row)) => self.mark = index.anchor_at(row),
            (None, None) => {}
        }
        start
    }

    /// Layer the reader's line, the marked range and the search matches over
    /// a window of transcript rows beginning at absolute row `start`.
    ///
    /// The terminal's cursor is hidden while scrolled, so the reader's line
    /// is painted rather than pointed at.
    pub fn decorate(&self, start: usize, lines: &mut [Line<'static>], palette: &Palette) {
        let reader = self.reader_row();
        let (mark_start, mark_end) = self.range();
        let marked = self.mark_row.is_some();
        let needle = self.search.needle.as_deref().map(str::to_lowercase);
        for (offset, line) in lines.iter_mut().enumerate() {
            let row = start + offset;
            let style = if row == reader {
                palette.copy_cursor()
            } else if marked && (mark_start..=mark_end).contains(&row) {
                palette.copy_selection()
            } else if needle
                .as_deref()
                .is_some_and(|n| line_text(line).to_lowercase().contains(n))
            {
                palette.copy_search()
            } else {
                continue;
            };
            *line = overlay(line, style);
        }
    }

    /// The band's bottom row while scrolled: the search prompt while one is
    /// being typed, the position and the keys otherwise — the same "every
    /// grown view renders its own keys" rule the picker and the ledger
    /// follow (`docs/tui.md`, "Keys").
    ///
    /// The key list shortens to fit `width` so `q leave` is never the part
    /// that falls off the right edge, the way `status::legend_line` shortens.
    pub fn hint_line(&self, width: u16, palette: &Palette) -> Line<'static> {
        if let Some(prompt) = &self.search.prompt {
            return Line::from(Span::styled(
                format!("{}{}", prompt.direction.glyph(), prompt.text),
                palette.status(),
            ));
        }
        let position = if self.total_rows == 0 {
            "line 0/0".to_string()
        } else {
            format!("line {}/{}", self.reader_row() + 1, self.total_rows)
        };
        let lead = format!("{position}   ");
        let full = "j/k  ^D/^U  gg/G  / search  v mark  y copy  q leave";
        let short = "v mark  y copy  q leave";
        let room = usize::from(width).saturating_sub(lead.chars().count());
        let keys = if full.chars().count() <= room { full } else { short };
        Line::from(Span::styled(format!("{lead}{keys}"), palette.status()))
    }

    // ── the keys' own edits ─────────────────────────────────────────────

    fn move_by(&mut self, delta: isize) {
        let room = self.body_h.saturating_sub(1);
        let want = (self.reader_offset as isize) + delta;
        if want < 0 {
            self.reader_offset = 0;
            self.scroll(want);
        } else if want as usize > room {
            let over = want as usize - room;
            self.reader_offset = room;
            self.scroll(over as isize);
        } else {
            self.reader_offset = want as usize;
        }
    }

    /// `Up` and `Down`, and the scroll that leaves the tail: move the view a
    /// line with the reader's line pinned to its screen row.
    ///
    /// That is what the wheel means — the terminal sends a tick as three
    /// `Up` presses, and a tick moves the screen three lines, the way tmux
    /// copy mode scrolls. `j` and `k` are vim's cursor motions instead, and
    /// scroll only at the edge.
    ///
    /// At an edge the view cannot move, so the reader's line does: the first
    /// and the last row stay reachable with the arrows alone.
    fn scroll_view(&mut self, delta: isize) {
        let at_top = self.top_row == 0;
        let at_end = self.top_row >= self.total_rows.saturating_sub(self.body_h);
        if (delta < 0 && at_top) || (delta > 0 && at_end) {
            self.move_by(delta);
        } else {
            self.scroll(delta);
        }
    }

    /// Whether the reader is already on the transcript's last row, which is
    /// where a downward move returns to the live tail.
    fn at_bottom(&self) -> bool {
        self.total_rows > 0 && self.reader_row() + 1 >= self.total_rows
    }

    fn toggle_mark(&mut self) {
        if self.mark_row.is_some() {
            self.mark = None;
            self.mark_row = None;
        } else {
            self.mark = None;
            self.mark_row = Some(self.reader_row());
        }
    }

    fn open_prompt(&mut self, direction: SearchDirection) {
        self.search.prompt = Some(SearchPrompt { direction, text: String::new() });
    }

    /// Commit the typed prompt and say what to look for. An empty pattern
    /// closes the prompt and searches for nothing.
    fn commit_search(&mut self) -> Outcome {
        let Some(prompt) = self.search.prompt.take() else {
            return Outcome::Moved;
        };
        if prompt.text.is_empty() {
            return Outcome::Moved;
        }
        self.search.direction = prompt.direction;
        self.search.needle = Some(prompt.text.clone());
        Outcome::Find {
            needle: prompt.text,
            from: self.reader_row(),
            forward: self.search.direction == SearchDirection::Forward,
            skip_current: false,
        }
    }

    /// `n` (`same_direction`) or `N` — step to the next match, wrapping
    /// around the whole transcript once it runs out.
    fn step_match(&self, same_direction: bool) -> Outcome {
        let Some(needle) = self.search.needle.clone() else {
            return Outcome::Moved;
        };
        let forward = (self.search.direction == SearchDirection::Forward) == same_direction;
        Outcome::Find { needle, from: self.reader_row(), forward, skip_current: true }
    }
}

/// The plain text of one rendered line — what the search and the yank read.
pub fn line_text(line: &Line<'_>) -> String {
    line.spans.iter().map(|s| s.content.as_ref()).collect()
}

/// Layer `style` over every span in `line`, keeping the span's own style
/// underneath (`Style::patch`) — the same way `render_block` layers a
/// markdown tone over a block's own.
fn overlay(line: &Line<'static>, style: Style) -> Line<'static> {
    Line::from(
        line.spans
            .iter()
            .map(|s| Span::styled(s.content.clone(), s.style.patch(style)))
            .collect::<Vec<_>>(),
    )
}

/// The row a search lands on: case-insensitive substring, wrapping around
/// the whole transcript once. `skip_current` is what separates `n` from the
/// `Enter` that committed the pattern — one steps off the reader's line, the
/// other accepts it.
pub fn find(
    rows: &[String],
    needle: &str,
    from: usize,
    forward: bool,
    skip_current: bool,
) -> Option<usize> {
    if rows.is_empty() || needle.is_empty() {
        return None;
    }
    let needle = needle.to_lowercase();
    let hit = |row: usize| rows[row].to_lowercase().contains(&needle);
    let len = rows.len();
    let from = from.min(len - 1);
    let first = usize::from(skip_current);
    (first..=len)
        .map(|step| {
            if forward {
                (from + step) % len
            } else {
                (from + len - (step % len)) % len
            }
        })
        .find(|&row| hit(row))
}

// ────────────────────────────────────────────────────────────────────────────
// Keys
// ────────────────────────────────────────────────────────────────────────────

/// What one key did to the scrolled view.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// The scrolled view took it; redraw.
    Moved,
    /// `q`, `Esc`, `G`, or a move past the bottom — back to the live tail.
    Leave,
    /// A search wants a row: the caller renders the whole transcript, calls
    /// [`find`], and hands the answer to [`Scrolled::jump_to`].
    Find {
        needle: String,
        from: usize,
        forward: bool,
        skip_current: bool,
    },
    /// `y` or `Enter` — the caller assembles this inclusive row range's
    /// text, writes it over OSC 52, keeps it in the paste buffer, and
    /// returns to the tail.
    Yank(usize, usize),
    /// Every other key: snap the view to the tail and handle the key there
    /// (`docs/tui.md`, "Scrolling is copy mode").
    Snap,
}

/// Interpret one key against the scrolled view.
pub fn handle_key(view: &mut Scrolled, key: &KeyEvent) -> Outcome {
    // The search prompt takes every key while it is open — the same "one
    // surface at a time holds the keyboard" rule the rest of the client
    // follows. Nothing snaps out from under a half-typed pattern.
    if view.search.prompt.is_some() {
        return match key.code {
            KeyCode::Esc => {
                view.search.prompt = None;
                Outcome::Moved
            }
            KeyCode::Enter => view.commit_search(),
            KeyCode::Backspace => {
                view.search.prompt.as_mut().expect("checked Some above").text.pop();
                Outcome::Moved
            }
            KeyCode::Char(c) => {
                view.search.prompt.as_mut().expect("checked Some above").text.push(c);
                Outcome::Moved
            }
            _ => Outcome::Moved,
        };
    }

    if !matches!(key.code, KeyCode::Char('g')) {
        view.pending_g = false;
    }

    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let page = view.body_h.max(1) as isize;
    let half_page = (view.body_h / 2).max(1) as isize;

    // A downward move from the last row is what tmux's "scroll past the
    // bottom" is here: the live tail, and the transcript follows again.
    let down = |view: &mut Scrolled, delta: isize| {
        if view.at_bottom() {
            Outcome::Leave
        } else {
            view.move_by(delta);
            Outcome::Moved
        }
    };

    match key.code {
        KeyCode::Char('q') | KeyCode::Esc => Outcome::Leave,
        KeyCode::Char('j') => down(view, 1),
        KeyCode::Char('k') => {
            view.move_by(-1);
            Outcome::Moved
        }
        KeyCode::Down => {
            if view.at_bottom() {
                Outcome::Leave
            } else {
                view.scroll_view(1);
                Outcome::Moved
            }
        }
        KeyCode::Up => {
            view.scroll_view(-1);
            Outcome::Moved
        }
        KeyCode::Char('d') if ctrl => down(view, half_page),
        KeyCode::Char('u') if ctrl => {
            view.move_by(-half_page);
            Outcome::Moved
        }
        KeyCode::Char('f') if ctrl => down(view, page),
        KeyCode::Char('b') if ctrl => {
            view.move_by(-page);
            Outcome::Moved
        }
        KeyCode::PageDown => down(view, page),
        KeyCode::PageUp => {
            view.move_by(-page);
            Outcome::Moved
        }
        KeyCode::Char('g') => {
            if view.pending_g {
                view.pending_g = false;
                view.jump_to(0);
                Outcome::Moved
            } else {
                view.pending_g = true;
                Outcome::Moved
            }
        }
        KeyCode::Home => {
            view.jump_to(0);
            Outcome::Moved
        }
        // `G` is the bottom, and the bottom is the live tail.
        KeyCode::Char('G') | KeyCode::End => Outcome::Leave,
        KeyCode::Char('/') => {
            view.open_prompt(SearchDirection::Forward);
            Outcome::Moved
        }
        KeyCode::Char('?') => {
            view.open_prompt(SearchDirection::Backward);
            Outcome::Moved
        }
        KeyCode::Char('n') => view.step_match(true),
        KeyCode::Char('N') => view.step_match(false),
        // `v` marks, the vim spelling. `Space` no longer marks: it snaps,
        // the habit wezterm calls `scroll_to_bottom_on_input` (Amy: *"live
        // typing should snap back to the tail, I often hit space just to do
        // that"*).
        KeyCode::Char('v') => {
            view.toggle_mark();
            Outcome::Moved
        }
        KeyCode::Char('y') | KeyCode::Enter => {
            let (start, end) = view.range();
            Outcome::Yank(start, end)
        }
        _ => Outcome::Snap,
    }
}

/// The OSC 52 clipboard sequence for `text` — `ESC ] 52 ; c ; <base64> BEL`,
/// the only clipboard path over ssh; the targets are iTerm2 and the Linux
/// terminals (`docs/tui.md`, guidance 7). Pure: the caller writes the bytes
/// to the terminal, under the same lock every other write takes
/// (`run.rs`'s `term_lock`); this only encodes them.
pub fn osc52_sequence(text: &str) -> String {
    format!("\x1b]52;c;{}\x07", base64_encode(text.as_bytes()))
}

/// Standard base64, padded — hand-rolled rather than a new dependency: the
/// alphabet is fixed and OSC 52 is the one caller.
fn base64_encode(data: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b0 = chunk[0];
        let b1 = chunk.get(1).copied().unwrap_or(0);
        let b2 = chunk.get(2).copied().unwrap_or(0);
        let n = (u32::from(b0) << 16) | (u32::from(b1) << 8) | u32::from(b2);
        out.push(ALPHABET[((n >> 18) & 0x3F) as usize] as char);
        out.push(ALPHABET[((n >> 12) & 0x3F) as usize] as char);
        out.push(if chunk.len() > 1 { ALPHABET[((n >> 6) & 0x3F) as usize] as char } else { '=' });
        out.push(if chunk.len() > 2 { ALPHABET[(n & 0x3F) as usize] as char } else { '=' });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use kaijutsu_types::{ContextId, PrincipalId};

    fn block(seq: u64) -> BlockId {
        // One context and one principal: the seq is what tells the fixture's
        // blocks apart.
        static IDS: std::sync::OnceLock<(ContextId, PrincipalId)> = std::sync::OnceLock::new();
        let (ctx, principal) = *IDS.get_or_init(|| (ContextId::new(), PrincipalId::new()));
        BlockId::new(ctx, principal, seq)
    }

    /// Blocks of `rows` rows each with no divider and no gap — the shape
    /// the motion tests start from, where a row is a row.
    fn rows_index(rows: &[usize]) -> RowIndex {
        RowIndex::new(
            rows.iter()
                .enumerate()
                .map(|(i, r)| RowBlock { block: block(i as u64), leading: 0, rows: *r })
                .collect(),
        )
    }

    /// Blocks of `(leading, rows)` — a divider or a blank row above the
    /// block's own text, which is what the anchor counts from.
    fn led_index(blocks: &[(usize, usize)]) -> RowIndex {
        RowIndex::new(
            blocks
                .iter()
                .enumerate()
                .map(|(i, &(leading, rows))| RowBlock { block: block(i as u64), leading, rows })
                .collect(),
        )
    }

    /// The anchor a settled view holds, for a test that names the block and
    /// the line and does not care which neighbours came with it.
    fn placed(view: &Scrolled) -> (BlockId, isize) {
        let anchor = view.top.expect("the frame took an anchor");
        (anchor.block, anchor.line)
    }

    fn press(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn ctrl(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
    }

    /// A view settled once over `rows`-shaped blocks with a ten-row screen.
    fn settled(rows: &[usize]) -> (Scrolled, RowIndex) {
        let index = rows_index(rows);
        let mut view = Scrolled::entering(10);
        view.settle(&index, 10);
        (view, index)
    }

    // ── entering ────────────────────────────────────────────────────────

    #[test]
    fn entering_lands_on_the_tail_with_the_reader_on_the_last_row() {
        let (view, _) = settled(&[10, 10, 10]);
        assert_eq!(view.top_row, 20, "the last screenful of thirty rows");
        assert_eq!(view.reader_row(), 29, "the newest row, the way tmux enters at the current screen");
    }

    #[test]
    fn entering_a_transcript_shorter_than_the_screen_starts_at_the_top() {
        let (view, _) = settled(&[4]);
        assert_eq!(view.top_row, 0);
        assert_eq!(view.reader_row(), 3, "the last row there is");
    }

    /// `Up` at the draft's first line leaves the tail and scrolls one line;
    /// the reader rides at the same screen row.
    #[test]
    fn the_entry_scroll_moves_the_view_by_one_line() {
        let index = rows_index(&[10, 10, 10]);
        let mut view = Scrolled::entering(10);
        view.scroll(-1);
        let start = view.settle(&index, 10);
        assert_eq!(start, 19);
        assert_eq!(view.reader_row(), 28);
    }

    // ── anchoring ───────────────────────────────────────────────────────

    /// Rows appended *below* the reader do not move what they are looking
    /// at: the anchored block and line are the same, and so is the top row.
    #[test]
    fn the_anchor_keeps_its_place_when_rows_are_appended_below() {
        let index = rows_index(&[10, 10, 10]);
        let mut view = Scrolled::entering(10);
        view.scroll(-12);
        let start = view.settle(&index, 10);
        assert_eq!(start, 8);
        let anchor = view.top.expect("the frame took an anchor");
        assert_eq!(placed(&view), (block(0), 8));

        // The last block streams in twenty more rows.
        let grown = rows_index(&[10, 10, 30]);
        assert_eq!(view.settle(&grown, 10), 8, "the top row is unchanged");
        assert_eq!(view.top, Some(anchor), "and so is the anchor");
    }

    /// A block growing *above* the reader moves the row number and not the
    /// screen — the anchor is the whole point.
    #[test]
    fn the_anchor_keeps_its_block_and_line_when_rows_are_inserted_above() {
        let index = rows_index(&[10, 10, 10]);
        let mut view = Scrolled::entering(10);
        view.scroll(-5);
        assert_eq!(view.settle(&index, 10), 15);
        let anchor = view.top.expect("anchored");
        assert_eq!(placed(&view), (block(1), 5));

        // The first block grows by seven rows.
        let grown = rows_index(&[17, 10, 10]);
        assert_eq!(view.settle(&grown, 10), 22, "the same block and line, seven rows further down");
        assert_eq!(view.top, Some(anchor), "the anchor did not move");
    }

    /// A narrower width wraps a block into fewer rows than the anchor names;
    /// the line clamps into the block rather than sliding into the next one.
    #[test]
    fn a_rewrap_clamps_the_anchor_line_into_its_own_block() {
        let index = rows_index(&[10, 10, 10]);
        let mut view = Scrolled::entering(10);
        view.scroll(-3);
        assert_eq!(view.settle(&index, 10), 17);
        assert_eq!(placed(&view), (block(1), 7));

        let narrow = rows_index(&[10, 4, 10]);
        assert_eq!(view.settle(&narrow, 10), 13, "the last row of the shrunken block");
        assert_eq!(placed(&view), (block(1), 3));
    }

    /// An excluded or edited-away block leaves no anchor to resolve, so the
    /// view falls to a *neighbour* — the next block's first content row —
    /// rather than to an absolute row, which a block inserted above in the
    /// same settle would have moved out from under it.
    #[test]
    fn a_deleted_anchor_block_falls_to_its_neighbour() {
        let index = led_index(&[(0, 10), (2, 10), (2, 10)]);
        let mut view = Scrolled::entering(10);
        view.scroll(-8);
        assert_eq!(view.settle(&index, 10), 12);
        assert_eq!(view.top.map(|a| a.block), Some(block(1)));

        // Block 1 is gone and a block of six rows arrives above everything
        // in the same settle: the old row 12 is now somewhere in block 0.
        let without = RowIndex::new(vec![
            RowBlock { block: block(9), leading: 0, rows: 6 },
            RowBlock { block: block(0), leading: 0, rows: 10 },
            RowBlock { block: block(2), leading: 2, rows: 10 },
        ]);
        assert_eq!(view.settle(&without, 10), 16, "block 2's first content row, not row 12");
        assert_eq!(view.top.map(|a| a.block), Some(block(2)));
    }

    /// With no next block the anchor falls back to the previous block's
    /// last row.
    #[test]
    fn a_deleted_last_block_falls_to_the_one_before_it() {
        let index = rows_index(&[10, 10, 10]);
        let mut view = Scrolled::entering(10);
        view.settle(&index, 10);
        assert_eq!(view.top.map(|a| a.block), Some(block(2)));

        let without = rows_index(&[10, 10]);
        view.settle(&without, 10);
        assert_eq!(view.top.map(|a| a.block), Some(block(1)), "the block before it");
    }

    /// A divider appearing above the anchored block — a block inserted above
    /// it by a different speaker, or a tool call settling back out of the
    /// in-flight strip — must not shift the text on the reader's row. The
    /// anchor counts from the block's first *content* row, so it does not.
    #[test]
    fn a_divider_appearing_above_the_anchor_leaves_the_text_put() {
        // Block 1 has no divider yet: it continues its predecessor's pair.
        let before = led_index(&[(0, 10), (0, 10), (0, 10)]);
        let mut view = Scrolled::entering(10);
        view.scroll(-8);
        assert_eq!(view.settle(&before, 10), 12);
        assert_eq!(placed(&view), (block(1), 2), "the third content row of block 1");

        // A different speaker lands above it: block 1 gains a blank row and
        // a divider, so every one of its rows moves down two.
        let after = led_index(&[(0, 10), (2, 12), (0, 10)]);
        assert_eq!(view.settle(&after, 10), 14, "the same content row, two rows further down");
        assert_eq!(placed(&view), (block(1), 2));
    }

    /// A mark whose block is excluded is gone with it: nothing slides onto
    /// a neighbour and gets yanked in its place.
    #[test]
    fn a_deleted_marked_block_clears_the_mark() {
        let index = rows_index(&[10, 10, 10]);
        let mut view = Scrolled::entering(10);
        view.scroll(-15);
        view.settle(&index, 10);
        handle_key(&mut view, &press(KeyCode::Char('v')));
        view.settle(&index, 10);
        assert!(view.mark_row.is_some() && view.mark.is_some());

        let without = RowIndex::new(vec![
            RowBlock { block: block(0), leading: 0, rows: 10 },
            RowBlock { block: block(2), leading: 0, rows: 10 },
        ]);
        view.settle(&without, 10);
        assert_eq!(view.mark_row, None, "the marked block is gone");
        assert_eq!(view.mark, None, "and so is the mark");
        // `y` now copies the reader's own line, not a stale range.
        let reader = view.reader_row();
        assert_eq!(view.range(), (reader, reader));
    }

    // ── motions ─────────────────────────────────────────────────────────

    /// `k` is vim's cursor motion: it walks the reader up the screen and
    /// scrolls only once it is on the top row.
    #[test]
    fn k_walks_the_reader_up_the_screen_before_it_scrolls() {
        let index = rows_index(&[10, 10, 10]);
        let mut view = Scrolled::entering(10);
        view.settle(&index, 10);
        for expected in [28, 27, 26] {
            assert_eq!(handle_key(&mut view, &press(KeyCode::Char('k'))), Outcome::Moved);
            view.settle(&index, 10);
            assert_eq!(view.reader_row(), expected);
            assert_eq!(view.top_row, 20, "the view has not scrolled yet");
        }
        for _ in 0..6 {
            handle_key(&mut view, &press(KeyCode::Char('k')));
            view.settle(&index, 10);
        }
        assert_eq!(view.reader_row(), 20, "the reader is on the top row now");
        handle_key(&mut view, &press(KeyCode::Char('k')));
        view.settle(&index, 10);
        assert_eq!(view.top_row, 19, "past the top row the view scrolls");
        assert_eq!(view.reader_row(), 19);
    }

    /// `Up` scrolls the view a line with the reader pinned to its screen
    /// row: one wheel tick is three of them and moves the screen three
    /// lines, never a cursor two rows and the screen one.
    #[test]
    fn up_scrolls_the_view_with_the_reader_pinned_to_its_screen_row() {
        let index = rows_index(&[10, 10, 10]);
        let mut view = Scrolled::entering(10);
        view.scroll(-1);
        view.settle(&index, 10);
        assert_eq!((view.top_row, view.reader_row()), (19, 28), "the entry scroll is a view scroll");
        for expected in [18, 17, 16] {
            assert_eq!(handle_key(&mut view, &press(KeyCode::Up)), Outcome::Moved);
            view.settle(&index, 10);
            assert_eq!(view.top_row, expected);
            assert_eq!(view.reader_row(), expected + 9, "pinned to the last screen row");
        }
        assert_eq!(view.top_row, 16, "three presses moved the screen three lines");
    }

    /// At the top the view cannot scroll, so the arrow moves the reader
    /// instead and line 0 stays reachable without `gg`.
    #[test]
    fn up_at_the_top_moves_the_reader_rather_than_stalling() {
        let index = rows_index(&[10, 10, 10]);
        let mut view = Scrolled::entering(10);
        view.jump_to(0);
        view.settle(&index, 10);
        assert_eq!((view.top_row, view.reader_row()), (0, 0));
        // The reader at the bottom of a view already at the top.
        for _ in 0..9 {
            handle_key(&mut view, &press(KeyCode::Char('j')));
            view.settle(&index, 10);
        }
        assert_eq!(view.reader_row(), 9);
        handle_key(&mut view, &press(KeyCode::Up));
        view.settle(&index, 10);
        assert_eq!((view.top_row, view.reader_row()), (0, 8), "the reader moved, the view could not");
    }

    #[test]
    fn a_downward_move_from_the_last_row_returns_to_the_tail() {
        let index = rows_index(&[10, 10, 10]);
        let mut view = Scrolled::entering(10);
        view.settle(&index, 10);
        assert_eq!(handle_key(&mut view, &press(KeyCode::Char('j'))), Outcome::Leave);
        assert_eq!(handle_key(&mut view, &press(KeyCode::Down)), Outcome::Leave);
        assert_eq!(handle_key(&mut view, &press(KeyCode::PageDown)), Outcome::Leave);
    }

    #[test]
    fn capital_g_returns_to_the_tail_and_gg_goes_to_the_top() {
        let index = rows_index(&[10, 10, 10]);
        let mut view = Scrolled::entering(10);
        view.settle(&index, 10);
        assert_eq!(handle_key(&mut view, &press(KeyCode::Char('G'))), Outcome::Leave);

        handle_key(&mut view, &press(KeyCode::Char('g')));
        assert_eq!(handle_key(&mut view, &press(KeyCode::Char('g'))), Outcome::Moved);
        view.settle(&index, 10);
        assert_eq!(view.top_row, 0);
        assert_eq!(view.reader_row(), 0);
    }

    #[test]
    fn a_single_g_is_cancelled_by_any_other_key() {
        let index = rows_index(&[10, 10, 10]);
        let mut view = Scrolled::entering(10);
        view.settle(&index, 10);
        handle_key(&mut view, &press(KeyCode::Char('g')));
        handle_key(&mut view, &press(KeyCode::Char('k')));
        handle_key(&mut view, &press(KeyCode::Char('g')));
        view.settle(&index, 10);
        assert_ne!(view.top_row, 0, "the cancelled g must not fire on a later lone g");
    }

    #[test]
    fn ctrl_u_and_page_up_step_by_half_a_screen_and_a_screen() {
        let index = rows_index(&[10, 10, 10]);
        let mut view = Scrolled::entering(10);
        view.settle(&index, 10);
        handle_key(&mut view, &ctrl('u'));
        view.settle(&index, 10);
        assert_eq!(view.reader_row(), 24, "half of a ten-row screen");
        handle_key(&mut view, &press(KeyCode::PageUp));
        view.settle(&index, 10);
        assert_eq!(view.reader_row(), 14);
    }

    // ── the snap rule ───────────────────────────────────────────────────

    /// Every key the scrolled view does not claim snaps to the tail and is
    /// handled there — `Space` most of all (Amy: *"I often hit space just to
    /// do that"*).
    #[test]
    fn an_unclaimed_key_snaps_to_the_tail() {
        let index = rows_index(&[10, 10, 10]);
        let mut view = Scrolled::entering(10);
        view.settle(&index, 10);
        for code in [KeyCode::Char(' '), KeyCode::Char('i'), KeyCode::Char(':'), KeyCode::Tab] {
            assert_eq!(handle_key(&mut view, &press(code)), Outcome::Snap, "{code:?}");
        }
        assert_eq!(handle_key(&mut view, &ctrl('a')), Outcome::Snap, "a Ctrl+A chord snaps and arms");
    }

    /// `Space` snaps, and `v` is the one key that marks.
    #[test]
    fn space_no_longer_marks() {
        let index = rows_index(&[10, 10, 10]);
        let mut view = Scrolled::entering(10);
        view.settle(&index, 10);
        handle_key(&mut view, &press(KeyCode::Char(' ')));
        assert_eq!(view.mark_row, None, "Space left no mark behind");
        handle_key(&mut view, &press(KeyCode::Char('v')));
        assert_eq!(view.mark_row, Some(29), "v marks the reader's line");
    }

    // ── mark and yank ───────────────────────────────────────────────────

    #[test]
    fn v_then_k_then_y_yanks_two_lines() {
        let index = rows_index(&[10, 10, 10]);
        let mut view = Scrolled::entering(10);
        view.settle(&index, 10);
        handle_key(&mut view, &press(KeyCode::Char('v')));
        view.settle(&index, 10);
        handle_key(&mut view, &press(KeyCode::Char('k')));
        view.settle(&index, 10);
        assert_eq!(handle_key(&mut view, &press(KeyCode::Char('y'))), Outcome::Yank(28, 29));
    }

    /// `y` or `Enter` with nothing marked copies the reader's own line.
    #[test]
    fn y_with_no_mark_copies_the_readers_line() {
        let index = rows_index(&[10, 10, 10]);
        let mut view = Scrolled::entering(10);
        view.settle(&index, 10);
        assert_eq!(handle_key(&mut view, &press(KeyCode::Char('y'))), Outcome::Yank(29, 29));
        assert_eq!(handle_key(&mut view, &press(KeyCode::Enter)), Outcome::Yank(29, 29));
    }

    /// The mark is anchored, so a block growing above it keeps the range on
    /// the same two lines.
    #[test]
    fn the_mark_rides_its_block_when_rows_are_inserted_above() {
        let index = rows_index(&[10, 10, 10]);
        let mut view = Scrolled::entering(10);
        view.settle(&index, 10);
        handle_key(&mut view, &press(KeyCode::Char('v')));
        view.settle(&index, 10);
        assert_eq!(view.mark_row, Some(29));

        let grown = rows_index(&[14, 10, 10]);
        view.settle(&grown, 10);
        assert_eq!(view.mark_row, Some(33), "the same line of the same block");
    }

    // ── search ──────────────────────────────────────────────────────────

    #[test]
    fn a_committed_search_asks_the_caller_for_a_row() {
        let index = rows_index(&[10, 10, 10]);
        let mut view = Scrolled::entering(10);
        view.settle(&index, 10);
        assert_eq!(handle_key(&mut view, &press(KeyCode::Char('/'))), Outcome::Moved);
        for c in "alpha".chars() {
            handle_key(&mut view, &press(KeyCode::Char(c)));
        }
        assert_eq!(
            handle_key(&mut view, &press(KeyCode::Enter)),
            Outcome::Find { needle: "alpha".to_string(), from: 29, forward: true, skip_current: false }
        );
        assert_eq!(
            handle_key(&mut view, &press(KeyCode::Char('n'))),
            Outcome::Find { needle: "alpha".to_string(), from: 29, forward: true, skip_current: true }
        );
        assert_eq!(
            handle_key(&mut view, &press(KeyCode::Char('N'))),
            Outcome::Find { needle: "alpha".to_string(), from: 29, forward: false, skip_current: true }
        );
    }

    /// While the prompt is open every key belongs to it: `q` types a literal
    /// `q` into the pattern rather than leaving, and `Space` does not snap.
    #[test]
    fn the_open_prompt_takes_every_key() {
        let index = rows_index(&[10, 10, 10]);
        let mut view = Scrolled::entering(10);
        view.settle(&index, 10);
        handle_key(&mut view, &press(KeyCode::Char('/')));
        assert_eq!(handle_key(&mut view, &press(KeyCode::Char('q'))), Outcome::Moved);
        assert_eq!(handle_key(&mut view, &press(KeyCode::Char(' '))), Outcome::Moved);
        assert_eq!(view.search.prompt.as_ref().expect("open").text, "q ");
        assert_eq!(handle_key(&mut view, &press(KeyCode::Esc)), Outcome::Moved);
        assert!(view.search.prompt.is_none(), "Esc closed the prompt without leaving");
    }

    #[test]
    fn find_is_a_case_insensitive_substring_that_wraps_once() {
        let rows: Vec<String> = ["one", "Contains ALPHA here", "three", "alpha again"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(find(&rows, "alpha", 0, true, false), Some(1));
        assert_eq!(find(&rows, "alpha", 1, true, true), Some(3), "n steps off the current line");
        assert_eq!(find(&rows, "alpha", 3, true, true), Some(1), "and wraps");
        assert_eq!(find(&rows, "alpha", 2, false, true), Some(1));
        assert_eq!(find(&rows, "alpha", 1, false, true), Some(3), "N wraps backward");
        assert_eq!(find(&rows, "nowhere", 0, true, false), None);
    }

    // ── the hint line ───────────────────────────────────────────────────

    #[test]
    fn the_hint_line_carries_the_position_and_keeps_q_leave_on_screen() {
        let index = rows_index(&[10, 10, 10]);
        let mut view = Scrolled::entering(10);
        view.settle(&index, 10);
        let palette = Palette::builtin();
        for width in [40u16, 60, 80, 120] {
            let hint = line_text(&view.hint_line(width, &palette));
            assert!(hint.contains("line 30/30"), "width {width}: {hint:?}");
            assert!(hint.ends_with("q leave"), "width {width}: {hint:?}");
            assert!(
                hint.chars().count() <= usize::from(width) || width < 40,
                "width {width}: the hint is {} wide: {hint:?}",
                hint.chars().count()
            );
        }
    }

    #[test]
    fn the_search_prompt_replaces_the_hint_while_it_is_typed() {
        let index = rows_index(&[10]);
        let mut view = Scrolled::entering(10);
        view.settle(&index, 10);
        handle_key(&mut view, &press(KeyCode::Char('?')));
        handle_key(&mut view, &press(KeyCode::Char('a')));
        assert_eq!(line_text(&view.hint_line(80, &Palette::builtin())), "?a");
    }

    // ── osc 52 ──────────────────────────────────────────────────────────

    /// One byte short of a group, two short, and an exact group: the
    /// padding branches and the one sextet that needs none.
    #[test]
    fn osc52_encodes_the_known_base64_of_short_strings() {
        assert_eq!(osc52_sequence("hi"), "\x1b]52;c;aGk=\x07", "one pad byte");
        assert_eq!(osc52_sequence("abc"), "\x1b]52;c;YWJj\x07", "an exact group, no padding");
        assert_eq!(osc52_sequence("abcd"), "\x1b]52;c;YWJjZA==\x07", "two pad bytes");
        assert_eq!(osc52_sequence("a"), "\x1b]52;c;YQ==\x07");
    }
}
