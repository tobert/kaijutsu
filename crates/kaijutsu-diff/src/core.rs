//! `DiffCore` — a pure, **read-only** vi motion engine over a [`DiffModel`].
//!
//! The sibling of `kaijutsu-editor`'s `EditorCore`, and deliberately not its
//! relative: an editor's intents carry write semantics, a diff viewer's never
//! do. Nothing here can mutate diff content. Keys go in, a cursor, a selection
//! and a short list of [`DiffIntent`]s come out — no Bevy, no kernel, no RPC,
//! no clipboard. That is what makes the whole viewer headless-testable and what
//! would let a future TUI client reuse it unchanged.
//!
//! # The client-neutral seam is `TerminalKey`
//!
//! [`DiffCore::apply_keys`] consumes modalkit [`TerminalKey`]s, exactly like
//! `EditorCore`. modalkit already *is* the shared key vocabulary: a Bevy client
//! converts through `kaijutsu-app`'s `input::vim::keyconv`, and a future
//! `modalkit-ratatui` client gets `TerminalKey` from crossterm natively. There
//! is deliberately no third key-notation crate or data file between them —
//! inventing one would add a translation nobody needs and a version to keep in
//! sync with modalkit's own.
//!
//! [`DiffCore::apply_notation`] parses vim notation (`"Vjy"`, `"]c"`,
//! `"<Esc>"`) into those keys; it exists for tests and for callers that already
//! hold notation.
//!
//! # Read-only by construction
//!
//! The cursor rides a modalkit [`EditBuffer`] holding the model's **canonical
//! unified text** ([`crate::format`]), so motions, counts, `gg`/`G`, visual
//! mode and registers are real vim rather than a hand-rolled subset. Actions
//! that would *change* the buffer are dropped at [`is_read_only`]: only
//! [`EditAction::Motion`] and [`EditAction::Yank`] are ever executed, alongside
//! cursor/selection/mark bookkeeping. A viewer that could edit its buffer would
//! silently diverge from the block it was frozen from.
//!
//! # Rows are the unit
//!
//! [`DiffRow`] maps every line of the canonical text back to what it *is*
//! (file header, hunk header, a body line of a given [`LineKind`]) and where it
//! came from (file/hunk/line indices). Because the text is the canonical text,
//! a yank of rows `a..=b` is *exactly* the canonical unified text of those
//! lines, prefixes included — no re-derivation, no header scraping.
//!
//! # Two yank semantics, chosen by shape
//!
//! A diff viewer is read for two different reasons, and [`YankKind`] is the
//! answer to both without either one lying:
//!
//! - **Whole lines are a patch.** `V`/`yy`/`VGy` yank canonical unified text,
//!   prefixes included, and re-parse as the same model.
//! - **Characters are plain text.** `v` selects characters and its yank drops
//!   every line's prefix column — you are grabbing an identifier, not a
//!   change to one.
//!
//! What stays banned is the middle: an operator-motion yank (`yw`, `y$`,
//! `yiw`) hands back a ragged fragment that still carries a `+` — text that
//! *looks* like a patch and is not one, and that the reader never saw
//! highlighted before it hit the clipboard.
//!
//! # Folds are a projection, never a rewrite
//!
//! [`DiffCore::rows`] always describes **every** line and [`DiffCore::text`] is
//! always the whole canonical patch — folding must never move the ground a
//! yank stands on. What folding produces is a second, derived list:
//! [`DiffCore::visible_rows`], the row indices a renderer should draw, with a
//! folded hunk's body absent and its header kept as the indicator. The cursor
//! is kept out of hidden rows ([`DiffCore::snap_out_of_folds`]) for the same
//! reason column motions were dropped in slice 5: a cursor somewhere the
//! reader cannot see is state that lies.

use editor_types::application::{ApplicationAction, ApplicationInfo};
use editor_types::context::{EditContext, Resolve};
use editor_types::prelude::{CommandType, Count, TargetShape, ViewportContext};
use editor_types::{
    Action, CommandBarAction, EditAction, EditorAction, PromptAction, WindowAction,
};
use modalkit::keybindings::SequenceStatus;

use modalkit::actions::Editable;
use modalkit::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use modalkit::editing::buffer::{CursorGroupId, EditBuffer};
use modalkit::editing::cursor::Cursor;
use modalkit::editing::store::Store;
use modalkit::env::CommonKeyClass;
use modalkit::env::vim::VimMode;
use modalkit::env::vim::keybindings::{InputStep, VimBindings, VimMachine};
use modalkit::key::TerminalKey;
use modalkit::keybindings::{BindingMachine, EdgeEvent, EdgePathPart, EdgeRepeat, InputBindings};
use modalkit::prelude::Register;

use crate::format::{format_file, truncation_marker};
use crate::model::{DiffModel, FoldState, LineKind};

/// What one line of the canonical text *is*.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowKind {
    /// The `#!kaijutsu-diff truncated: …` marker of an incomplete projection.
    Marker,
    /// A file-section header line (`diff --git`, `rename …`, `---`, `+++`).
    FileHeader,
    /// A `@@ … @@` hunk header.
    HunkHeader,
    /// A body line, carrying its own kind.
    Body(LineKind),
    /// A `\ No newline at end of file` marker, belonging to the body line
    /// above it.
    NoNewline,
}

/// One line of the canonical text, with its provenance in the model.
///
/// `start..end` tiles the text exactly as `PreviewLine` does inline: `end` is
/// the next row's `start`, so any byte offset a layout engine hands back — a
/// shaping run's start, a wrapped visual row's start — resolves to exactly one
/// row by containment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DiffRow {
    /// Byte offset of the row's first byte in [`DiffCore::text`].
    pub start: usize,
    /// Byte offset one past the row's newline (= the next row's `start`).
    pub end: usize,
    /// Where the line's own text begins: `start + 1` for a body row (past the
    /// one-ASCII-byte ` `/`+`/`-` prefix), `start` otherwise. A
    /// [`crate::WordSpan`] — which indexes bytes of `DiffLine::text` — maps
    /// into the canonical text by adding this.
    pub text_start: usize,
    /// What the row is.
    pub kind: RowKind,
    /// Index into [`DiffModel::files`], if the row belongs to a file.
    pub file: Option<usize>,
    /// Index into `FileDiff::hunks`, if the row belongs to a hunk.
    pub hunk: Option<usize>,
    /// Index into `Hunk::lines`, if the row belongs to a body line.
    pub line: Option<usize>,
}

/// What a yank *means*, and therefore what its text is.
///
/// **The shape decides the semantics** (`docs/diff.md` slice 6 phase B). There
/// is no third option and no ambiguity: a yank of whole lines is a patch
/// fragment, a yank of characters is plain text with no diff prefixes
/// anywhere in it. The one thing the viewer must never produce is the middle
/// case — a ragged fragment that still carries a `+` and therefore *looks*
/// like a patch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum YankKind {
    /// The exact canonical unified text of whole lines, prefixes included —
    /// a patch fragment, which is what makes `VGy` re-parse as the same
    /// model. Every line-wise yank.
    Patch,
    /// The selected characters with every line's ` `/`+`/`-` prefix column
    /// removed: plain source text, explicitly **not** a patch. Only a
    /// character-wise visual selection produces this — the reader asked for
    /// characters, in a mode that draws exactly what they will get.
    PlainText,
}

/// A character-wise range over rows, in the same coordinates as the row index
/// it names.
///
/// `end_col` is **inclusive** — vim's `v` selects the character under the
/// cursor, and a renderer drawing an exclusive end would leave the last
/// selected glyph unhighlighted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CharSpan {
    /// First row of the selection.
    pub start_row: usize,
    /// First selected column on `start_row`, counted in **chars**.
    pub start_col: usize,
    /// Last row of the selection.
    pub end_row: usize,
    /// Last selected column on `end_row`, inclusive, counted in **chars**.
    pub end_col: usize,
}

/// Something the viewer must act on that [`DiffCore`] cannot do itself.
///
/// Deliberately **not** shared with `EditorCore`'s intents: those carry write
/// semantics (checkpoint, discard, substitute) and a common trait would couple
/// two surfaces whose whole point is that one of them cannot write.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiffIntent {
    /// `y` — copy `text` to the system clipboard. Never emitted empty.
    Yank {
        /// The text to copy. What it *is* depends on `kind`: canonical
        /// unified text (newline-terminated) for [`YankKind::Patch`], prefix
        /// -stripped plain text for [`YankKind::PlainText`].
        text: String,
        /// Which of the viewer's two yank semantics produced `text`.
        kind: YankKind,
    },
    /// `q` / `ZQ` / `ZZ` / `:q` — leave the viewer.
    Close,
    /// `R` / `:e` — the reader asked to rebind to the block's current content.
    ///
    /// Freeze-on-open means the viewer never rebinds on its own; this is the
    /// explicit half of that contract, the action a stale banner offers.
    Refresh,
}

/// The viewer verbs vim has no binding for, routed through modalkit's
/// application-action channel so they inherit counts and mode awareness
/// instead of being sniffed off the key stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ViewerAction {
    /// `]c` — next hunk.
    NextHunk,
    /// `[c` — previous hunk.
    PrevHunk,
    /// `zo` — unfold the hunk under the cursor.
    FoldOpen,
    /// `zc` — fold it.
    FoldClose,
    /// `za` — toggle it.
    FoldToggle,
    /// `q` — close the viewer (vim's macro-record binding is not useful here).
    Close,
    /// `R` — re-read the block (see [`DiffIntent::Refresh`]).
    Refresh,
    /// `<C-v>`/`<C-q>` — deliberately nothing. Visual **block**'s yank is a
    /// *rectangle*: it cuts through the prefix column, so the text is neither
    /// a patch nor a coherent run of source. Character-wise `v` has a
    /// semantics ([`YankKind::PlainText`]); a rectangle of a diff does not,
    /// and it stays out until something asks for it with one attached
    /// (`docs/diff.md` slice 6 phase B).
    Nop,
}

impl ApplicationAction for ViewerAction {
    fn is_edit_sequence(&self, _: &EditContext) -> SequenceStatus {
        SequenceStatus::Break
    }

    fn is_last_action(&self, _: &EditContext) -> SequenceStatus {
        SequenceStatus::Atom
    }

    fn is_last_selection(&self, _: &EditContext) -> SequenceStatus {
        SequenceStatus::Ignore
    }

    fn is_switchable(&self, _: &EditContext) -> bool {
        false
    }
}

/// modalkit's application hook for the viewer. Uninhabited: it carries types,
/// never values.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DiffInfo {}

impl ApplicationInfo for DiffInfo {
    type Error = String;
    type Action = ViewerAction;
    type Store = ();
    type WindowId = String;
    type ContentId = String;

    fn content_of_command(cmdtype: CommandType) -> String {
        match cmdtype {
            CommandType::Search => "*search*".into(),
            CommandType::Command => "*command*".into(),
        }
    }
}

/// The viewer's own key mappings, layered over `VimBindings`.
struct ViewerBindings;

/// One plain-character key as an edge path part.
fn key_path(keys: &str) -> Vec<EdgePathPart<TerminalKey, CommonKeyClass>> {
    keys.chars()
        .map(|c| {
            let key = TerminalKey::from(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
            (EdgeRepeat::Once, EdgeEvent::Key(key))
        })
        .collect()
}

/// One `Ctrl`-chorded character as an edge path part.
fn ctrl_path(c: char) -> Vec<EdgePathPart<TerminalKey, CommonKeyClass>> {
    let key = TerminalKey::from(KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL));
    vec![(EdgeRepeat::Once, EdgeEvent::Key(key))]
}

impl InputBindings<TerminalKey, InputStep<DiffInfo>> for ViewerBindings {
    fn setup(&self, machine: &mut VimMachine<TerminalKey, DiffInfo>) {
        let verbs = [
            ("]c", ViewerAction::NextHunk),
            ("[c", ViewerAction::PrevHunk),
            ("zo", ViewerAction::FoldOpen),
            ("zc", ViewerAction::FoldClose),
            ("za", ViewerAction::FoldToggle),
            ("q", ViewerAction::Close),
            ("R", ViewerAction::Refresh),
        ];
        for (keys, verb) in verbs {
            let step = InputStep::<DiffInfo>::new().actions(vec![Action::Application(verb)]);
            let path = key_path(keys);
            for mode in [VimMode::Normal, VimMode::Visual] {
                machine.add_mapping(mode, &path, &step);
            }
        }
        // `<C-v>` (and vim's `<C-q>` alias) enter visual BLOCK, whose yank is
        // a rectangle: it cuts *through* the prefix column, so the result is
        // neither a patch nor a coherent run of source text. Character-wise
        // `v` is allowed because it has a meaning (plain text, prefixes
        // stripped per line); a rectangle has none, so it stays Nop'd.
        let nop = InputStep::<DiffInfo>::new().actions(vec![Action::Application(ViewerAction::Nop)]);
        for c in ['v', 'q'] {
            let path = ctrl_path(c);
            for mode in [VimMode::Normal, VimMode::Visual] {
                machine.add_mapping(mode, &path, &nop);
            }
        }
    }
}

/// What a folded hunk hides, for the indicator its header carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FoldSummary {
    /// How many rows of the canonical text the fold is hiding — body lines
    /// plus any `\ No newline` markers among them. Never zero: an empty hunk
    /// cannot exist in the dialect.
    pub hidden_rows: usize,
}

/// A read-only vi surface over one frozen [`DiffModel`].
pub struct DiffCore {
    /// The frozen model. Only [`FoldState`] — view state — ever changes.
    model: DiffModel,
    /// `format(&model)`, and the buffer's contents.
    text: String,
    /// One entry per line of `text`, contiguous.
    rows: Vec<DiffRow>,
    /// Indices into `rows` a renderer should draw: everything, minus the body
    /// of every folded hunk. Rebuilt whenever a fold verb runs — never on the
    /// render path, which asks the same question every frame.
    visible: Vec<usize>,
    /// Bumped on every fold change. A renderer caching a laid-out window keys
    /// on it: the *content* hash cannot see a fold, and two folds of equal
    /// size between frames would leave the visible row count unchanged while
    /// the visible rows themselves differ.
    fold_seq: u64,
    machine: VimMachine<TerminalKey, DiffInfo>,
    buffer: EditBuffer<DiffInfo>,
    store: Store<DiffInfo>,
    group: CursorGroupId,
    viewport: ViewportContext<Cursor>,
    /// True while modalkit's `:`/`/` bar owns the keystrokes (its own buffer).
    cmdline_active: bool,
    cmdline_prefix: String,
    cmdline: EditBuffer<DiffInfo>,
    cmdline_group: CursorGroupId,
    /// Cursor row and visual-line selection as of the last action.
    ///
    /// Cached because modalkit's cursor accessors want `&mut`, and a
    /// *renderer* asking where the cursor is has no business holding a mutable
    /// borrow of the viewer. Refreshed after every action, so it is never
    /// behind by more than the keystroke currently being processed.
    cursor_row: usize,
    cursor_col: usize,
    selection: Option<(usize, usize)>,
    /// The selection's shape, straight from modalkit's cursor state. It is
    /// what decides which of the two yank semantics runs — see
    /// [`DiffCore::yank_semantics`].
    selection_shape: Option<TargetShape>,
    /// The selection's exact character extent, cached only while it is
    /// character-wise. `None` in normal mode and under `V`.
    char_selection: Option<CharSpan>,
}

impl DiffCore {
    /// Open a viewer on `model`. The model is frozen from here on: callers
    /// that want the block's newer content build a *new* `DiffCore`.
    pub fn new(model: DiffModel) -> Self {
        let (text, rows) = build_rows(&model);

        let mut machine = VimMachine::<TerminalKey, DiffInfo>::empty();
        VimBindings::default().setup(&mut machine);
        ViewerBindings.setup(&mut machine);

        let mut buffer = EditBuffer::<DiffInfo>::from_str(String::from("diff"), &text);
        let group = buffer.create_group();

        let mut cmdline = EditBuffer::<DiffInfo>::from_str(String::from("cmdline"), "");
        let cmdline_group = cmdline.create_group();

        let visible = visible_row_indices(&model, &rows);

        Self {
            model,
            text,
            rows,
            visible,
            fold_seq: 0,
            machine,
            buffer,
            store: Store::default(),
            group,
            viewport: ViewportContext::default(),
            cmdline_active: false,
            cmdline_prefix: String::new(),
            cmdline,
            cmdline_group,
            cursor_row: 0,
            cursor_col: 0,
            selection: None,
            selection_shape: None,
            char_selection: None,
        }
    }

    /// The frozen model. Folds are the only thing that moves.
    pub fn model(&self) -> &DiffModel {
        &self.model
    }

    /// The canonical unified text the cursor rides. Yank slices come from
    /// here, which is what makes them exact.
    pub fn text(&self) -> &str {
        &self.text
    }

    /// Every line of [`text`](Self::text), classified and traced back to the
    /// model. Contiguous and in order.
    pub fn rows(&self) -> &[DiffRow] {
        &self.rows
    }

    /// The rows a renderer should draw, as indices into [`rows`](Self::rows),
    /// ascending.
    ///
    /// Identical to `0..rows().len()` while nothing is folded. A folded hunk
    /// contributes its header row and nothing else — the header *is* the
    /// indicator, carrying [`fold_summary`](Self::fold_summary) — so the
    /// projection never invents a row that has no counterpart in the canonical
    /// text. Everything the viewer addresses by index (cursor, selection,
    /// window) can therefore be mapped in both directions.
    pub fn visible_rows(&self) -> &[usize] {
        &self.visible
    }

    /// What the hunk owning `row` is hiding, or `None` if it is not folded.
    ///
    /// Only ever `Some` for a [`RowKind::HunkHeader`] row, because that is the
    /// only row of a folded hunk that is still drawn.
    pub fn fold_summary(&self, row: usize) -> Option<FoldSummary> {
        let r = self.rows.get(row)?;
        if r.kind != RowKind::HunkHeader {
            return None;
        }
        let hunk = self
            .model
            .files
            .get(r.file?)
            .and_then(|f| f.hunks.get(r.hunk?))?;
        if hunk.fold != FoldState::Folded {
            return None;
        }
        Some(FoldSummary {
            hidden_rows: hunk.lines.len() + hunk.lines.iter().filter(|l| l.no_newline).count(),
        })
    }

    /// The cursor's row index (0-based), clamped into `rows`.
    pub fn cursor_row(&self) -> usize {
        self.cursor_row
    }

    /// The cursor's column on its row, counted in **chars** (modalkit's own
    /// unit — its buffer is a char-indexed rope).
    ///
    /// Real again as of slice 6: word-level rendering is what makes a column
    /// visible, and a cursor drawn where it actually is can be trusted. Slice
    /// 5 pinned it to 0 precisely because it could not be drawn.
    pub fn cursor_col(&self) -> usize {
        self.cursor_col
    }

    /// The cursor's position in [`visible_rows`](Self::visible_rows) — what a
    /// renderer laying out only the visible rows needs.
    ///
    /// The cursor is never on a hidden row (see
    /// [`snap_out_of_folds`](Self::snap_out_of_folds)), so this is an exact
    /// lookup rather than a nearest-match.
    pub fn cursor_visible_row(&self) -> usize {
        self.visible_index_of(self.cursor_row).unwrap_or(0)
    }

    /// The visual-line selection as an inclusive range of
    /// [`visible_rows`](Self::visible_rows) indices.
    ///
    /// A selection *spanning* a folded hunk keeps every hidden row selected —
    /// the yank is unaffected by folding — and draws as the rows that remain,
    /// which is why the ends are clamped inward to visible rows rather than
    /// mapped exactly.
    pub fn selection_visible_rows(&self) -> Option<(usize, usize)> {
        let (a, b) = self.selection?;
        let first = self.visible.partition_point(|&r| r < a);
        let last = self.visible.partition_point(|&r| r <= b);
        if first >= last {
            // Every selected row is hidden inside one fold.
            return None;
        }
        Some((first, last - 1))
    }

    /// How many times a fold has changed. A renderer caching a laid-out
    /// window includes it in the cache key — see the field's own comment.
    pub fn fold_seq(&self) -> u64 {
        self.fold_seq
    }

    /// Where `row` sits in [`visible_rows`](Self::visible_rows), if it is
    /// drawn at all.
    pub fn visible_index_of(&self, row: usize) -> Option<usize> {
        let i = self.visible.partition_point(|&r| r < row);
        (self.visible.get(i) == Some(&row)).then_some(i)
    }

    /// The inclusive **row** range covered by the visual selection, whatever
    /// its shape. A character-wise selection covers whole rows here too — the
    /// rows it touches — which is what a coarse consumer (the window, the
    /// status counter) wants.
    pub fn selection_rows(&self) -> Option<(usize, usize)> {
        self.selection
    }

    /// The selection's exact character extent, in canonical row coordinates —
    /// `None` unless it is character-wise.
    ///
    /// This is the half of a `v` selection a line-band renderer cannot draw,
    /// and the reason character-wise visual waited for the multi-rect
    /// material.
    pub fn char_selection(&self) -> Option<CharSpan> {
        self.char_selection
    }

    /// [`char_selection`](Self::char_selection) in **visible** row
    /// coordinates, clamped to drawn rows the way
    /// [`selection_visible_rows`](Self::selection_visible_rows) is.
    ///
    /// A clamped end loses its column with its row: an end row that is folded
    /// away becomes "to the end of the last drawn row", because the alternative
    /// is drawing a column that belongs to a line nobody can see.
    pub fn char_selection_visible(&self) -> Option<CharSpan> {
        let span = self.char_selection?;
        let (first, last) = self.selection_visible_rows()?;
        let (first_row, last_row) = (self.visible[first], self.visible[last]);
        Some(CharSpan {
            start_row: first,
            start_col: if first_row == span.start_row {
                span.start_col
            } else {
                0
            },
            end_row: last,
            end_col: if last_row == span.end_row {
                span.end_col
            } else {
                self.last_col(last_row)
            },
        })
    }

    /// Move the cursor off a folded-away row, in the direction it was going.
    ///
    /// Vim's own fold behavior, and for the reason the whole viewer is built
    /// on: the cursor must be where the reader can see it. Coming *down* into
    /// a fold, the cursor lands past it — the fold behaves as one line, so `j`
    /// steps over it. Any other way in (moving up, jumping, or folding the
    /// hunk the cursor is already inside) parks it on the header, which is the
    /// one row of the fold still drawn.
    ///
    /// `previous` is where the cursor was before this action, which is the
    /// only thing that distinguishes "walking down into it" from the rest.
    fn snap_out_of_folds(&mut self, previous: usize) {
        if self.visible.is_empty() {
            return;
        }
        let row = self.buffer.get_leader(self.group).get_y();
        if self.visible_index_of(row).is_some() {
            return;
        }
        let at = self.visible.partition_point(|&r| r < row);
        let target = if row > previous {
            // The next visible row after the fold; the end of the document
            // has none, so fall back to the header behind us.
            self.visible
                .get(at)
                .copied()
                .or_else(|| self.visible.get(at.saturating_sub(1)).copied())
        } else {
            self.visible
                .get(at.saturating_sub(1))
                .copied()
                .or_else(|| self.visible.first().copied())
        };
        if let Some(target) = target {
            let col = self.buffer.get_leader(self.group).get_x();
            self.buffer
                .set_leader(self.group, Cursor::new(target, col));
        }
    }

    /// How many chars a row holds, newline excluded.
    fn row_char_len(&self, row: usize) -> usize {
        self.rows
            .get(row)
            .map(|r| self.text[r.start..r.end].trim_end_matches('\n').chars().count())
            .unwrap_or(0)
    }

    /// The last column a cursor may occupy on `row` — the bound a renderer
    /// can actually draw on.
    ///
    /// **A count is not a column.** `row_char_len` is a count, so the last
    /// valid index is one less: vim's normal mode puts the cursor *on* the
    /// last character, never past it (that is an insert-mode position, and
    /// this surface has no insert mode). Reporting the count let `$` then `j`
    /// onto a shorter line hand the renderer a column whose byte offset is the
    /// line's newline — which Parley resolves onto the following visual row,
    /// drawing the cursor one line low.
    fn last_col(&self, row: usize) -> usize {
        self.row_char_len(row).saturating_sub(1)
    }

    /// Rebuild the visible-row projection after a fold changed.
    fn rebuild_visible(&mut self) {
        self.visible = visible_row_indices(&self.model, &self.rows);
        self.fold_seq = self.fold_seq.wrapping_add(1);
    }

    /// Re-read the cursor and selection out of modalkit into the cache.
    ///
    /// `self.cursor_row` is deliberately read here **before** it is written:
    /// the fold snap needs where the cursor *was* to tell "walking down into a
    /// fold" from every other way in, and this runs once per popped action
    /// inside [`apply_keys`](Self::apply_keys)'s loop, so the stale value is
    /// exactly one action old — the previous position. Reordering the two
    /// statements, or hoisting this out of the pop loop to run once per batch,
    /// silently breaks the direction test.
    fn sync_cursor(&mut self) {
        self.snap_out_of_folds(self.cursor_row);
        let last = self.rows.len().saturating_sub(1);
        let leader = self.buffer.get_leader(self.group);
        self.cursor_row = leader.get_y().min(last);
        // Report a column the row actually has. modalkit keeps a *goal*
        // column (vim's `$`-sticky behavior) that can sit past a short line,
        // and a renderer handed that would draw the cursor off the end of the
        // text. Clamping the reported value leaves the goal column alone.
        self.cursor_col = if leader.get_y() > last {
            0
        } else {
            leader.get_x().min(self.last_col(self.cursor_row))
        };
        let selection = self.buffer.get_leader_selection(self.group);
        self.selection = selection.as_ref().map(|(start, end, _shape)| {
            let (a, b) = (start.get_y().min(last), end.get_y().min(last));
            (a.min(b), a.max(b))
        });
        self.selection_shape = selection.as_ref().map(|(_, _, shape)| *shape);
        // modalkit sorts the pair by (row, column), so the ends are already in
        // reading order. Columns are clamped the same way the cursor's is: a
        // goal column past the end of a short row would draw a selection off
        // the end of the text.
        self.char_selection = selection.and_then(|(start, end, shape)| {
            (shape == TargetShape::CharWise).then(|| {
                let (sr, er) = (start.get_y().min(last), end.get_y().min(last));
                CharSpan {
                    start_row: sr,
                    start_col: start.get_x().min(self.last_col(sr)),
                    end_row: er,
                    end_col: end.get_x().min(self.last_col(er)),
                }
            })
        });
    }

    /// The vim mode banner (`None` in normal mode, `Some("-- VISUAL LINE --")`
    /// in visual-line mode).
    pub fn mode(&self) -> Option<String> {
        self.machine.show_mode()
    }

    /// The `:`-line being typed, with its prefix, or `None` when the bar is
    /// closed. A renderer draws this; nothing else depends on it.
    pub fn command_line(&self) -> Option<String> {
        if self.cmdline_active {
            Some(format!(
                "{}{}",
                self.cmdline_prefix,
                strip_one_trailing_newline(&self.cmdline.get_text())
            ))
        } else {
            None
        }
    }

    /// Feed modalkit keys — **the client-neutral seam** — and drain whatever
    /// intents they produced.
    pub fn apply_keys<I: IntoIterator<Item = TerminalKey>>(&mut self, keys: I) -> Vec<DiffIntent> {
        let mut out = Vec::new();
        for key in keys {
            self.machine.input_key(key);
            while let Some((action, ctx)) = self.machine.pop() {
                self.handle(action, &ctx, &mut out);
                // Keep the cache fresh *within* a batch: a `j]c` must see the
                // cursor the `j` moved, not the one the batch started on.
                self.sync_cursor();
            }
        }
        out
    }

    /// Feed a vim-notation key sequence (`"Vjy"`, `"3]c"`, `"<Esc>"`). A thin
    /// wrapper over [`apply_keys`](Self::apply_keys) for tests and for callers
    /// that hold notation rather than keys.
    pub fn apply_notation(&mut self, keys: &str) -> Vec<DiffIntent> {
        self.apply_keys(parse_keys(keys))
    }

    /// Route one popped modalkit action.
    fn handle(&mut self, action: Action<DiffInfo>, ctx: &EditContext, out: &mut Vec<DiffIntent>) {
        match action {
            // The `:`/`/` bar is a separate buffer; the document must not see
            // its keystrokes (the exact corruption `EditorCore` guards too).
            Action::CommandBar(CommandBarAction::Focus(prefix, _ct, _act)) => {
                self.cmdline_active = true;
                self.cmdline_prefix = prefix;
                self.cmdline = EditBuffer::<DiffInfo>::from_str(String::from("cmdline"), "");
                self.cmdline_group = self.cmdline.create_group();
            }
            Action::CommandBar(CommandBarAction::Unfocus) => {
                self.cmdline_active = false;
            }
            Action::Prompt(PromptAction::Submit) if self.cmdline_active => {
                let body = strip_one_trailing_newline(&self.cmdline.get_text());
                if self.cmdline_prefix.starts_with(':')
                    && let Some(intent) = parse_ex_command(&body)
                {
                    out.push(intent);
                }
                self.cmdline_active = false;
            }
            Action::Prompt(_) if self.cmdline_active => {
                self.cmdline_active = false;
            }
            Action::Editor(ea) if self.cmdline_active => {
                let ictx = (self.cmdline_group, &self.viewport, ctx);
                let _ = self.cmdline.editor_command(&ea, &ictx, &mut self.store);
            }
            Action::Editor(ea) => self.editor_action(ea, ctx, out),
            Action::Application(verb) => self.viewer_action(verb, ctx, out),
            // `ZZ`/`ZQ`. There is nothing to write, so both simply leave —
            // modalkit is the only thing that knows these are a quit and not
            // two literal keystrokes.
            Action::Window(WindowAction::Close(..)) if !self.cmdline_active => {
                out.push(DiffIntent::Close);
            }
            _ => {}
        }
    }

    /// Run an editor action against the document buffer — if it is read-only.
    fn editor_action(&mut self, ea: EditorAction, ctx: &EditContext, out: &mut Vec<DiffIntent>) {
        if !is_read_only(&ea, ctx) {
            return;
        }
        // A yank with no semantics attached does not run at all. See
        // [`DiffCore::yank_semantics`].
        let yank = match is_yank(&ea, ctx) {
            true => match self.yank_semantics(&ea, ctx) {
                Some(kind) => Some(kind),
                None => return,
            },
            false => None,
        };
        // Read the span *before* executing: the yank collapses the selection
        // it was taken from.
        let span = self.char_selection;

        let ictx = (self.group, &self.viewport, ctx);
        let ran = self.buffer.editor_command(&ea, &ictx, &mut self.store);
        if ran.is_err() {
            return;
        }
        let text = match yank {
            // A line-wise yank lands in the unnamed register with modalkit's
            // own semantics — counts, motions and the visual selection all
            // already resolved. Read it back rather than re-deriving the
            // range, which is how the yanked text stays *exactly* the
            // canonical text of those rows.
            Some(YankKind::Patch) => self
                .store
                .registers
                .get(&Register::Unnamed)
                .map(|cell| cell.value.to_string())
                .unwrap_or_default(),
            // The register is no use here: it holds the raw slice of the
            // canonical text, prefixes and all, which is the one thing a
            // character-wise yank must not hand back.
            Some(YankKind::PlainText) => match span {
                Some(span) => self.charwise_plain_text(span),
                None => return,
            },
            None => return,
        };
        if !text.is_empty() {
            out.push(DiffIntent::Yank {
                text,
                kind: yank.expect("a yank with text"),
            });
        }
    }

    /// What this yank *means* — or `None` if it means nothing the viewer is
    /// willing to put on the clipboard.
    ///
    /// **The shape decides** (`docs/diff.md` slice 6 phase B). Three answers:
    ///
    /// - Whole lines → [`YankKind::Patch`]. Canonical unified text, prefixes
    ///   included, so a yanked hunk really is a patch.
    /// - A character-wise **visual selection** → [`YankKind::PlainText`]. The
    ///   reader is looking at exactly the characters they asked for, and what
    ///   they want is the *code* — an identifier, an expression — not a diff
    ///   of it. Stripping the prefix column makes that explicitly-not-a-patch,
    ///   so it can never be mistaken for one.
    /// - Anything else → `None`, unchanged from phase A. `yw`, `y$`, `yiw`,
    ///   `` y`a ``, `y^` and vim's `yvj` shape override all resolve to a
    ///   ragged fragment that the reader never *saw* as a selection. A
    ///   fragment nobody looked at is an accident of a motion; the same
    ///   characters under `v` are a decision. That is the whole difference,
    ///   and it is why enabling `v` does not loosen the operator guard by one
    ///   inch.
    ///
    /// The selection branch reproduces modalkit's own rule for
    /// `EditTarget::Selection` — `ctx.get_target_shape()` if the keybinding
    /// forced one, else the cursor state's shape (`editing/buffer/mod.rs`).
    /// Asking the *context* alone would miss a plain `v` selection, which
    /// carries no forced shape at all.
    fn yank_semantics(&self, ea: &EditorAction, ctx: &EditContext) -> Option<YankKind> {
        use editor_types::prelude::EditTarget;
        if let EditorAction::Edit(_, EditTarget::Selection) = ea {
            return match ctx.get_target_shape().or(self.selection_shape) {
                Some(TargetShape::LineWise) => Some(YankKind::Patch),
                Some(TargetShape::CharWise) => {
                    // No cached span means no selection to take characters
                    // from; refusing beats yanking the register's raw text.
                    self.char_selection.map(|_| YankKind::PlainText)
                }
                // `<C-v>` is Nop'd, so this is unreachable today — and a
                // rectangle is exactly the shape with no semantics attached.
                Some(TargetShape::BlockWise) | None => None,
            };
        }
        is_line_wise_yank(ea, ctx).then_some(YankKind::Patch)
    }

    /// The plain text of a character-wise selection: the selected characters
    /// of each row, **without** the diff prefix column.
    ///
    /// Pure given the frozen text and rows, and the whole of what
    /// [`YankKind::PlainText`] means. Three rules, each with a reason:
    ///
    /// - **A body row's prefix is never included**, even when the selection
    ///   starts at column 0. The prefix is not part of the line's text — it is
    ///   the diff's annotation of it — and letting it through is precisely how
    ///   a fragment starts looking like a patch.
    /// - **A header row contributes its whole text** (clipped to the
    ///   selection). `@@ -1,3 +1,4 @@` has no prefix column to strip, and it
    ///   cannot be confused for a patch on its own.
    /// - **Row structure survives**: one output line per selected row,
    ///   including empty ones. Vim's character-wise register has no trailing
    ///   newline and neither does this.
    fn charwise_plain_text(&self, span: CharSpan) -> String {
        let mut out = String::new();
        for r in span.start_row..=span.end_row.min(self.rows.len().saturating_sub(1)) {
            let Some(row) = self.rows.get(r) else {
                break;
            };
            if r > span.start_row {
                out.push('\n');
            }
            let raw = self.text[row.start..row.end].trim_end_matches('\n');
            // The prefix is one ASCII byte, so byte and char agree on its
            // width — `text_start` is the model's own answer for where the
            // line's text begins.
            let prefix = row.text_start - row.start;
            let len = raw.chars().count();
            let lo = if r == span.start_row {
                span.start_col.max(prefix)
            } else {
                prefix
            };
            let hi = if r == span.end_row {
                (span.end_col + 1).min(len)
            } else {
                len
            };
            if hi <= lo {
                continue;
            }
            let from = char_byte(raw, lo);
            let to = char_byte(raw, hi);
            out.push_str(&raw[from..to]);
        }
        out
    }

    /// Run one viewer verb.
    fn viewer_action(&mut self, verb: ViewerAction, ctx: &EditContext, out: &mut Vec<DiffIntent>) {
        let count: usize = ctx.resolve(&Count::Contextual);
        match verb {
            ViewerAction::NextHunk => self.jump_hunk(count as isize),
            ViewerAction::PrevHunk => self.jump_hunk(-(count as isize)),
            ViewerAction::FoldOpen => self.set_fold(Some(FoldState::Expanded)),
            ViewerAction::FoldClose => self.set_fold(Some(FoldState::Folded)),
            ViewerAction::FoldToggle => self.set_fold(None),
            ViewerAction::Close => out.push(DiffIntent::Close),
            ViewerAction::Refresh => out.push(DiffIntent::Refresh),
            ViewerAction::Nop => {}
        }
    }

    /// Move the cursor `n` hunk headers forward (negative: backward), clamping
    /// at the first and last hunk rather than wrapping — a diff is a bounded
    /// document and wrapping past the end loses the reader's place.
    ///
    /// ⚠ **This walks canonical rows, not [`visible_rows`](Self::visible_rows).**
    /// It is correct only because a folded hunk *keeps its header row* — every
    /// jump target is drawn. If a future fold level ever hides a hunk or file
    /// header (file-level collapse is the obvious one), `]c` would land the
    /// cursor on a row nobody draws: the exact invisible-state failure
    /// `snap_out_of_folds` exists to prevent. Move it to visible space at the
    /// same time, not afterwards.
    fn jump_hunk(&mut self, n: isize) {
        if n == 0 {
            return;
        }
        let headers: Vec<usize> = self
            .rows
            .iter()
            .enumerate()
            .filter(|(_, r)| r.kind == RowKind::HunkHeader)
            .map(|(i, _)| i)
            .collect();
        if headers.is_empty() {
            return;
        }
        let cursor = self.cursor_row();
        let target = if n > 0 {
            let ahead: Vec<usize> = headers.iter().copied().filter(|&i| i > cursor).collect();
            let step = (n as usize).min(ahead.len());
            if step == 0 {
                return;
            }
            ahead[step - 1]
        } else {
            let behind: Vec<usize> = headers.iter().copied().filter(|&i| i < cursor).collect();
            let step = (n.unsigned_abs()).min(behind.len());
            if step == 0 {
                return;
            }
            behind[behind.len() - step]
        };
        self.buffer.set_leader(self.group, Cursor::new(target, 0));
    }

    /// Apply a fold to the hunk under the cursor. `None` toggles.
    fn set_fold(&mut self, state: Option<FoldState>) {
        let row = self.cursor_row();
        let Some(row) = self.rows.get(row) else {
            return;
        };
        let (Some(fi), Some(hi)) = (row.file, row.hunk) else {
            return;
        };
        let Some(hunk) = self
            .model
            .files
            .get_mut(fi)
            .and_then(|f| f.hunks.get_mut(hi))
        else {
            return;
        };
        hunk.fold = state.unwrap_or(match hunk.fold {
            FoldState::Expanded => FoldState::Folded,
            FoldState::Folded => FoldState::Expanded,
        });
        self.rebuild_visible();
    }
}

/// The rows a renderer should draw: everything except the body of a folded
/// hunk.
///
/// Pure, and deliberately so — the projection is the whole of what folding
/// *means* to a viewer, and it is testable without a keystroke. The header row
/// of a folded hunk stays: it is the indicator, and keeping it means the
/// projection is always a subsequence of the real rows, never a rewrite with
/// synthetic entries the cursor could not address.
pub fn visible_row_indices(model: &DiffModel, rows: &[DiffRow]) -> Vec<usize> {
    let folded = |row: &DiffRow| -> bool {
        let (Some(fi), Some(hi)) = (row.file, row.hunk) else {
            return false;
        };
        model
            .files
            .get(fi)
            .and_then(|f| f.hunks.get(hi))
            .is_some_and(|h| h.fold == FoldState::Folded)
    };
    rows.iter()
        .enumerate()
        .filter(|(_, row)| row.kind == RowKind::HunkHeader || !folded(row))
        .map(|(i, _)| i)
        .collect()
}

/// Is this editor action safe on a read-only surface?
///
/// The allow-list is deliberately short and positive: motions and yanks touch
/// no text, cursor/selection/mark actions are bookkeeping. Everything else —
/// inserts, deletes, replaces, joins, indents, case changes, undo/redo,
/// completion — is dropped. A read-only surface that could be edited would
/// silently stop matching the block it was frozen from.
fn is_read_only(ea: &EditorAction, ctx: &EditContext) -> bool {
    match ea {
        EditorAction::Edit(spec, _) => {
            let op: EditAction = ctx.resolve(spec);
            matches!(op, EditAction::Motion | EditAction::Yank)
        }
        EditorAction::Cursor(_) | EditorAction::Selection(_) | EditorAction::Mark(_) => true,
        EditorAction::InsertText(_) | EditorAction::History(_) | EditorAction::Complete(..) => {
            false
        }
        _ => false,
    }
}

/// Is this **yank's** target line-wise — the only granularity the viewer
/// yanks?
///
/// Slice 5 applied this to every editor action, because the cursor was drawn
/// at column 0 and a column motion would have moved modalkit's real column
/// with nothing on screen to show for it. Slice 6 draws the cursor at its real
/// column, so the motions are honest again and the rule narrows to what it was
/// always really about: **what comes out of a yank**.
///
/// Yank stays line-wise (`docs/diff.md` slice 5, unchanged): a yank is "the
/// exact canonical unified text of the selected lines, prefixes included", and
/// that is the property that makes `VGy` re-parse as the same model and makes
/// a hunk pasteable into feedback. A `yw` or `y$` would produce a fragment
/// with a `+` prefix and no line — text that looks like a patch and is not
/// one. Partial-line yank is not blocked by rendering; it is blocked by
/// meaning, so it stays out until something asks for it with a semantics
/// attached.
///
/// The list is **positive**: a `MoveType` we have not classified is rejected,
/// because "yanks something that isn't whole lines" is the failure worth
/// defaulting against.
///
/// Text-object targets are held to the same rule: `yiw` would yank a word out
/// of the middle of a line, quietly breaking the same contract.
fn is_line_wise_yank(ea: &EditorAction, ctx: &EditContext) -> bool {
    use editor_types::prelude::{EditTarget, MoveType, RangeType, SearchType, TargetShape};

    // A *forced* shape settles it before the target is even looked at: `y^`
    // carries an explicit `CharWise` (modalkit's own binding does), and so do
    // vim's shape overrides `yvj` / `y<C-v>j`. Only an explicitly line-wise
    // force, or no force at all, can go on to the target check.
    if !matches!(ctx.get_target_shape(), None | Some(TargetShape::LineWise)) {
        return false;
    }

    let EditorAction::Edit(_, target) = ea else {
        return true; // not a motion/operator at all
    };
    match target {
        // No movement at all. (`EditTarget::Selection` never reaches here —
        // a visual selection is settled by its own shape in
        // [`DiffCore::yank_semantics`], one level up.)
        EditTarget::CurrentPosition => true,
        // `'a` is a LINE address, `` `a `` is a character one — vim yanks
        // them line-wise and character-wise respectively, and a mark is
        // settable here (`ma` is bookkeeping, so it passes `is_read_only`).
        // `` y`a `` from a column really did hand back a fragment with a
        // ragged end.
        EditTarget::LineJump(_) => true,
        EditTarget::CharJump(_) => false,
        // `f`/`t`/`;`/`,` are pure column motions, and a regex search is
        // exclusive/character-wise in vim too. Search is not wired here yet
        // (the `/`-bar submits nothing), so refusing it now costs nothing and
        // means wiring it later cannot silently produce a fragment yank.
        EditTarget::Search(SearchType::Char(_), _, _) => false,
        EditTarget::Search(_, _, _) => false,
        // Text objects: only the whole-line and whole-buffer ones.
        EditTarget::Range(range, _, _) | EditTarget::Boundary(range, _, _, _) => {
            matches!(range, RangeType::Line | RangeType::Buffer)
        }
        EditTarget::Motion(mv, _) => matches!(
            mv,
            // `gg` / `G` / `NG` / `N%`
            MoveType::BufferPos(_)
                | MoveType::BufferLineOffset
                | MoveType::BufferLinePercent
                // `j` / `k` / `gj` / `gk`
                | MoveType::Line(_)
                | MoveType::ScreenLine(_)
                // `H` / `M` / `L`
                | MoveType::ViewportPos(_)
                // `{` / `}` / `[[` / `]]`
                | MoveType::ParagraphBegin(_)
                | MoveType::SectionBegin(_)
                | MoveType::SectionEnd(_)
                // `+` / `-` / `_` — line steps that then seek a column,
                // which a line-wise yank discards. Their same-line cousins
                // `^` and `g^` do NOT belong here: `^` is caught by the
                // forced-`CharWise` shape above, and `g^` (which carries no
                // shape) is why `ScreenFirstWord` is absent from this list —
                // `yg^` would otherwise yank from the cursor back to the
                // first non-blank.
                | MoveType::FirstWord(_)
        ),
        // `EditTarget` is `#[non_exhaustive]`. A target we have never seen is
        // exactly the case this function exists to refuse.
        _ => false,
    }
}

/// Does this action yank (so the unnamed register is worth reading back)?
fn is_yank(ea: &EditorAction, ctx: &EditContext) -> bool {
    match ea {
        EditorAction::Edit(spec, _) => {
            let op: EditAction = ctx.resolve(spec);
            op == EditAction::Yank
        }
        _ => false,
    }
}

/// The `:`-line dialect the viewer understands. A read-only surface has almost
/// nothing to say: quit, and re-read. Anything else is ignored rather than
/// errored — there is no message strip to report it in yet, and a viewer that
/// refuses to ignore `:w` would be more annoying than helpful.
fn parse_ex_command(body: &str) -> Option<DiffIntent> {
    let body = body.trim().trim_end_matches('!').trim();
    match body {
        "q" | "quit" | "qa" | "qall" | "x" | "wq" => Some(DiffIntent::Close),
        "e" | "edit" | "refresh" => Some(DiffIntent::Refresh),
        _ => None,
    }
}

/// Byte offset of char index `n` in `s`, clamped to `s.len()`.
///
/// modalkit counts columns in chars (its buffer is a char-indexed rope) and
/// every slice below is bytes; this is the one conversion between them.
fn char_byte(s: &str, n: usize) -> usize {
    s.char_indices().nth(n).map(|(i, _)| i).unwrap_or(s.len())
}

/// Remove at most one trailing `\n` (modalkit's guaranteed line terminator).
fn strip_one_trailing_newline(s: &str) -> String {
    s.strip_suffix('\n').unwrap_or(s).to_string()
}

/// Build the canonical text and its row table together.
///
/// The text comes from [`crate::format`]'s own writers, never from a
/// re-implementation of them — `format_file` produces each file section and the
/// rows are assigned by *structure* (how many lines each hunk contributes),
/// so a future header line in the canonical form is absorbed automatically
/// instead of shifting every classification after it. `rows_tile_the_canonical
/// _text` pins the equality.
fn build_rows(model: &DiffModel) -> (String, Vec<DiffRow>) {
    let mut text = String::new();
    let mut rows: Vec<DiffRow> = Vec::new();

    let push = |text: &mut String, rows: &mut Vec<DiffRow>, line: &str, row: RowKindAt| {
        let start = text.len();
        text.push_str(line);
        text.push('\n');
        rows.push(DiffRow {
            start,
            end: text.len(),
            text_start: start + usize::from(matches!(row.kind, RowKind::Body(_))),
            kind: row.kind,
            file: row.file,
            hunk: row.hunk,
            line: row.line,
        });
    };

    if let Some(t) = &model.truncated {
        push(
            &mut text,
            &mut rows,
            &truncation_marker(t),
            RowKindAt::new(RowKind::Marker),
        );
    }

    for (fi, file) in model.files.iter().enumerate() {
        let mut section = String::new();
        format_file(&mut section, file);
        let lines: Vec<&str> = section.lines().collect();

        // How many lines the hunks contribute; whatever is left at the front
        // is the file header, however many lines that turns out to be.
        let hunk_lines: usize = file
            .hunks
            .iter()
            .map(|h| 1 + h.lines.len() + h.lines.iter().filter(|l| l.no_newline).count())
            .sum();
        let header_lines = lines.len().saturating_sub(hunk_lines);

        for line in &lines[..header_lines] {
            push(
                &mut text,
                &mut rows,
                line,
                RowKindAt::new(RowKind::FileHeader).file(fi),
            );
        }

        let mut idx = header_lines;
        for (hi, hunk) in file.hunks.iter().enumerate() {
            push(
                &mut text,
                &mut rows,
                lines[idx],
                RowKindAt::new(RowKind::HunkHeader).file(fi).hunk(hi),
            );
            idx += 1;
            for (li, line) in hunk.lines.iter().enumerate() {
                push(
                    &mut text,
                    &mut rows,
                    lines[idx],
                    RowKindAt::new(RowKind::Body(line.kind))
                        .file(fi)
                        .hunk(hi)
                        .line(li),
                );
                idx += 1;
                if line.no_newline {
                    push(
                        &mut text,
                        &mut rows,
                        lines[idx],
                        RowKindAt::new(RowKind::NoNewline)
                            .file(fi)
                            .hunk(hi)
                            .line(li),
                    );
                    idx += 1;
                }
            }
        }
        debug_assert_eq!(
            idx,
            lines.len(),
            "format_file emitted lines the row builder did not account for"
        );
    }

    (text, rows)
}

/// Builder for the provenance half of a [`DiffRow`] — keeps [`build_rows`]
/// readable without four positional `Option`s at every call site.
#[derive(Clone, Copy)]
struct RowKindAt {
    kind: RowKind,
    file: Option<usize>,
    hunk: Option<usize>,
    line: Option<usize>,
}

impl RowKindAt {
    fn new(kind: RowKind) -> Self {
        Self {
            kind,
            file: None,
            hunk: None,
            line: None,
        }
    }

    fn file(mut self, i: usize) -> Self {
        self.file = Some(i);
        self
    }

    fn hunk(mut self, i: usize) -> Self {
        self.hunk = Some(i);
        self
    }

    fn line(mut self, i: usize) -> Self {
        self.line = Some(i);
        self
    }
}

/// Parse vim key notation into `TerminalKey`s: literal chars map to
/// themselves, `<…>` is a named or chorded key. Unknown `<…>` tokens are
/// skipped. (A near-twin of `EditorCore`'s; the two crates share no code by
/// design, and this one is a dozen lines.)
fn parse_keys(s: &str) -> Vec<TerminalKey> {
    let mut keys = Vec::new();
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '<' {
            let mut token = String::new();
            for tc in chars.by_ref() {
                if tc == '>' {
                    break;
                }
                token.push(tc);
            }
            if let Some(k) = named_key(&token) {
                keys.push(k);
            }
        } else {
            keys.push(TerminalKey::from(KeyEvent::new(
                KeyCode::Char(c),
                KeyModifiers::NONE,
            )));
        }
    }
    keys
}

fn named_key(token: &str) -> Option<TerminalKey> {
    let (code, mods) = match token.to_ascii_lowercase().as_str() {
        "esc" => (KeyCode::Esc, KeyModifiers::NONE),
        "cr" | "enter" | "return" => (KeyCode::Enter, KeyModifiers::NONE),
        "bs" | "backspace" => (KeyCode::Backspace, KeyModifiers::NONE),
        "tab" => (KeyCode::Tab, KeyModifiers::NONE),
        "space" => (KeyCode::Char(' '), KeyModifiers::NONE),
        other => {
            if let Some(rest) = other.strip_prefix("c-") {
                let ch = rest.chars().next()?;
                (KeyCode::Char(ch), KeyModifiers::CONTROL)
            } else {
                return None;
            }
        }
    };
    Some(TerminalKey::from(KeyEvent::new(code, mods)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::{DiffOptions, FileSpec, diff_file};
    use crate::format::format;
    use crate::parse::parse;

    /// Two files, three hunks, so hunk motions and multi-file yanks have
    /// something real to walk.
    fn sample() -> DiffModel {
        let before: String = (1..=40).map(|i| format!("line {i}\n")).collect();
        let after = before
            .replace("line 3\n", "THREE\n")
            .replace("line 20\n", "TWENTY\n");
        let a = diff_file(
            &FileSpec::modified("a.txt", &before, &after),
            &DiffOptions::default(),
        )
        .unwrap();
        let b = diff_file(
            &FileSpec::modified("b.txt", "x\n", "y\n"),
            &DiffOptions::default(),
        )
        .unwrap();
        DiffModel::new(vec![a, b])
    }

    fn core() -> DiffCore {
        DiffCore::new(sample())
    }

    fn row_text(core: &DiffCore, i: usize) -> &str {
        let r = core.rows()[i];
        core.text()[r.start..r.end].trim_end_matches('\n')
    }

    // ── rows ────────────────────────────────────────────────────────────────

    /// The invariant everything else rests on: the rows tile the canonical
    /// text with no gap and no overlap, and that text IS `format(&model)`.
    /// Yank correctness is a corollary — a row range is a slice of the
    /// canonical patch, not a re-derivation of it.
    #[test]
    fn rows_tile_the_canonical_text() {
        let c = core();
        assert_eq!(c.text(), format(c.model()), "text must be canonical");

        let mut expected = 0usize;
        for row in c.rows() {
            assert_eq!(row.start, expected, "gap or overlap in row ranges");
            assert!(row.end > row.start);
            assert!(row.text_start >= row.start && row.text_start <= row.end);
            expected = row.end;
        }
        assert_eq!(expected, c.text().len(), "rows must cover the whole text");
    }

    #[test]
    fn rows_carry_their_provenance() {
        let c = core();
        let headers: Vec<usize> = c
            .rows()
            .iter()
            .enumerate()
            .filter(|(_, r)| r.kind == RowKind::HunkHeader)
            .map(|(i, _)| i)
            .collect();
        assert_eq!(headers.len(), 3, "two hunks in a.txt, one in b.txt");
        assert!(row_text(&c, headers[0]).starts_with("@@"));

        let inserts: Vec<&DiffRow> = c
            .rows()
            .iter()
            .filter(|r| r.kind == RowKind::Body(LineKind::Insert))
            .collect();
        assert!(!inserts.is_empty());
        for row in inserts {
            assert!(row.file.is_some() && row.hunk.is_some() && row.line.is_some());
            assert_eq!(
                row.text_start,
                row.start + 1,
                "body rows skip one prefix byte"
            );
        }

        // Every file-header row belongs to a file and to no hunk.
        for row in c.rows().iter().filter(|r| r.kind == RowKind::FileHeader) {
            assert!(row.file.is_some());
            assert!(row.hunk.is_none());
        }
    }

    #[test]
    fn a_truncated_model_leads_with_a_marker_row() {
        let bounded = crate::truncate_to_bytes(&sample(), 200);
        assert!(bounded.truncated.is_some(), "test premise: it truncated");
        let c = DiffCore::new(bounded);
        assert_eq!(c.rows()[0].kind, RowKind::Marker);
        assert!(row_text(&c, 0).starts_with(crate::TRUNCATION_MARKER_PREFIX));
        assert_eq!(c.text(), format(c.model()));
    }

    #[test]
    fn a_no_newline_marker_is_its_own_row_bound_to_its_line() {
        let file = diff_file(
            &FileSpec::modified("a", "one\n", "one"),
            &DiffOptions::default(),
        )
        .unwrap();
        let c = DiffCore::new(DiffModel::new(vec![file]));
        let marker = c
            .rows()
            .iter()
            .position(|r| r.kind == RowKind::NoNewline)
            .expect("a no-newline row");
        assert_eq!(row_text(&c, marker), "\\ No newline at end of file");
        assert!(c.rows()[marker].line.is_some());
        assert_eq!(c.text(), format(c.model()));
    }

    #[test]
    fn an_empty_model_is_an_empty_but_usable_viewer() {
        let mut c = DiffCore::new(DiffModel::default());
        assert!(c.rows().is_empty());
        assert_eq!(c.text(), "");
        // Motions and verbs on nothing must not panic.
        assert!(c.apply_notation("jjGgg]c[czoVy").iter().all(|i| {
            matches!(
                i,
                DiffIntent::Yank { .. } | DiffIntent::Close | DiffIntent::Refresh
            )
        }));
        assert_eq!(c.cursor_row(), 0);
    }

    // ── motions ─────────────────────────────────────────────────────────────

    #[test]
    fn basic_motions_move_the_cursor() {
        let mut c = core();
        assert_eq!(c.cursor_row(), 0);
        c.apply_notation("j");
        assert_eq!(c.cursor_row(), 1);
        c.apply_notation("G");
        assert_eq!(
            c.cursor_row(),
            c.rows().len() - 1,
            "G lands on the last row"
        );
        c.apply_notation("gg");
        assert_eq!(c.cursor_row(), 0);
    }

    #[test]
    fn motions_take_counts() {
        let mut c = core();
        c.apply_notation("5j");
        assert_eq!(c.cursor_row(), 5);
        c.apply_notation("2k");
        assert_eq!(c.cursor_row(), 3);
    }

    #[test]
    fn hunk_motions_walk_the_hunk_headers_across_files() {
        let mut c = core();
        let headers: Vec<usize> = c
            .rows()
            .iter()
            .enumerate()
            .filter(|(_, r)| r.kind == RowKind::HunkHeader)
            .map(|(i, _)| i)
            .collect();

        c.apply_notation("]c");
        assert_eq!(c.cursor_row(), headers[0]);
        c.apply_notation("]c");
        assert_eq!(c.cursor_row(), headers[1]);
        c.apply_notation("]c");
        assert_eq!(c.cursor_row(), headers[2], "]c crosses into the next file");
        c.apply_notation("]c");
        assert_eq!(c.cursor_row(), headers[2], "clamps at the last hunk");
        c.apply_notation("[c");
        assert_eq!(c.cursor_row(), headers[1]);
        c.apply_notation("[c[c");
        assert_eq!(c.cursor_row(), headers[0], "clamps at the first hunk");
    }

    #[test]
    fn hunk_motions_take_counts() {
        let mut c = core();
        let headers: Vec<usize> = c
            .rows()
            .iter()
            .enumerate()
            .filter(|(_, r)| r.kind == RowKind::HunkHeader)
            .map(|(i, _)| i)
            .collect();
        c.apply_notation("3]c");
        assert_eq!(c.cursor_row(), headers[2]);
        c.apply_notation("2[c");
        assert_eq!(c.cursor_row(), headers[0]);
    }

    /// Column motions are real again (slice 6): word-level rendering draws the
    /// cursor at its actual column, so a motion that moves it is visible.
    #[test]
    fn column_motions_move_the_cursor_along_its_row() {
        let wide = {
            let c = core();
            c.rows()
                .iter()
                .position(|r| matches!(r.kind, RowKind::HunkHeader))
                .expect("a hunk header — the longest line in the fixture")
        };
        let at = |keys: &str| -> usize {
            let mut c = core();
            c.apply_notation(&format!("{}G", wide + 1));
            assert_eq!(c.cursor_col(), 0, "G lands at column 0");
            c.apply_notation(keys);
            assert_eq!(c.cursor_row(), wide, "keys {keys:?} left the row");
            c.cursor_col()
        };

        assert!(at("l") > 0, "l must move a column");
        assert!(at("5l") > at("l"));
        assert!(at("w") > 0, "w must move a column");
        assert!(at("3w") > at("w"));
        assert!(at("$") > at("w"), "$ goes to the end of the line");
        assert_eq!(at("$0"), 0, "0 comes back");
        assert_eq!(at("$b0"), 0);
    }

    /// `f`/`t` are the column motions with the most exact contract, so they
    /// get a line whose characters are unambiguous: a context row reads
    /// `" line 4"`, where `n` is at column 3.
    #[test]
    fn find_and_till_land_where_vim_puts_them() {
        let mut c = core();
        let row = c
            .rows()
            .iter()
            .position(|r| r.kind == RowKind::Body(LineKind::Context))
            .expect("a context row");
        c.apply_notation(&format!("{}G", row + 1));
        let text: String = c.text()[c.rows()[row].start..c.rows()[row].end]
            .trim_end_matches('\n')
            .to_string();
        assert_eq!(&text[..6], " line ", "fixture text changed: {text:?}");

        c.apply_notation("fn");
        assert_eq!(c.cursor_col(), 3, "f lands ON the match");
        c.apply_notation("0tn");
        assert_eq!(c.cursor_col(), 2, "t lands one short of it");
        assert_eq!(c.cursor_row(), row, "neither leaves the row");
    }

    /// A row change resets nothing the reader can see: the column travels, as
    /// it does in vim, and stays inside the row it lands on.
    #[test]
    fn the_column_is_clamped_to_the_row_the_cursor_lands_on() {
        let mut c = core();
        let wide = c
            .rows()
            .iter()
            .position(|r| matches!(r.kind, RowKind::HunkHeader))
            .expect("a hunk header");
        c.apply_notation(&format!("{}G$", wide + 1));
        assert!(c.cursor_col() > 0, "test premise: the column is off zero");

        // Every row the cursor visits must report a column inside its text —
        // including after a fold snap, which carries the column with it.
        c.apply_notation("]czc");
        for _ in 0..c.rows().len() {
            c.apply_notation("j");
            let row = c.rows()[c.cursor_row()];
            let text = c.text()[row.start..row.end].trim_end_matches('\n');
            assert!(
                c.cursor_col() <= text.chars().count(),
                "column {} past the end of row {:?}",
                c.cursor_col(),
                text,
            );
        }
    }

    /// **A count is not a column.** `$` on a long line then `j` onto a short
    /// one must report a column that row actually *has a character at* —
    /// reporting the char count instead pointed the renderer at the line's
    /// newline byte, which Parley resolves onto the next visual row: the
    /// cursor drew a line low. (deepseek post-ship review, phase A.)
    #[test]
    fn a_goal_column_never_reports_past_the_last_character() {
        let mut c = core();
        // Walk the whole document with a sticky `$` goal column and check
        // every row, long or short.
        c.apply_notation("gg$");
        for _ in 0..c.rows().len() {
            let row = c.cursor_row();
            let len = c.rows()[row];
            let text = c.text()[len.start..len.end].trim_end_matches('\n');
            let chars = text.chars().count();
            assert!(
                c.cursor_col() < chars.max(1),
                "column {} is not a character of row {row} ({chars} chars): {text:?}",
                c.cursor_col(),
            );
            c.apply_notation("j");
        }
    }

    /// The line motions the viewer is built on still land at column 0.
    #[test]
    fn line_motions_still_work() {
        let mut c = core();
        c.apply_notation("3j");
        assert_eq!(c.cursor_row(), 3);
        c.apply_notation("G");
        assert_eq!(c.cursor_row(), c.rows().len() - 1);
        c.apply_notation("gg");
        assert_eq!(c.cursor_row(), 0);
        c.apply_notation("]c");
        assert!(c.rows()[c.cursor_row()].kind == RowKind::HunkHeader);

        // `+`/`-` are line steps that then seek a column the snap discards.
        c.apply_notation("gg+");
        assert_eq!(c.cursor_row(), 1);
        c.apply_notation("-");
        assert_eq!(c.cursor_row(), 0);
    }

    /// A partial-line yank would produce a fragment carrying a `+` prefix and
    /// no line — text that looks like a patch and is not one. The cursor may
    /// sit at a column; the yank still takes whole lines.
    #[test]
    fn no_yank_can_take_less_than_a_whole_line() {
        let mut c = core();
        c.apply_notation("5G");
        for keys in [
            "yiw", "yaw", "yw", "y$", "ye", "yb", "y0", "yf@",
            // Same-line column seekers, and vim's explicit shape overrides.
            // These reach the guard by a *different* road than the target
            // list — a forced `CharWise`/`BlockWise` shape — and `y^` at a
            // non-zero column is the one that actually got through.
            "y^", "yg^", "yvj", "y<C-v>j",
            // A character mark address. `` y`a `` really did hand back
            // `"line 1\n line 2\n-line 3\n+THRE"` — ragged at both ends.
            "may`a", "maj$y`a",
        ] {
            let mut c = core();
            c.apply_notation("5G$");
            assert!(c.cursor_col() > 0, "test premise: the cursor is mid-line");
            assert!(
                c.apply_notation(keys).is_empty(),
                "keys {keys:?} yanked a fragment",
            );
        }

        // ...while the whole-line motions still yank, mid-line cursor or not.
        for keys in ["yy", "yG", "y_", "y+", "yVj", "3yy", "majy'a"] {
            let mut c = core();
            c.apply_notation("5G$");
            let yanked = c.apply_notation(keys);
            let Some(DiffIntent::Yank { text, .. }) = yanked.first() else {
                panic!("keys {keys:?} yanked nothing: {yanked:?}");
            };
            assert!(
                text.ends_with('\n') && text.starts_with([' ', '+', '-', '@', 'd', '\\']),
                "keys {keys:?} yanked something that is not whole lines: {text:?}",
            );
        }
    }

    /// **Yank stays line-wise even though the cursor no longer is.** `$` then
    /// `yy` is the whole line, prefix included — the contract that makes a
    /// yanked hunk a real patch.
    #[test]
    fn a_yank_after_a_column_motion_is_still_the_whole_line() {
        let mut c = core();
        c.apply_notation("5G");
        let plain = c.apply_notation("yy");
        let mut c2 = core();
        c2.apply_notation("5G$");
        assert!(c2.cursor_col() > 0, "test premise: the column moved");
        let after_column_motion = c2.apply_notation("yy");
        assert_eq!(plain, after_column_motion);
        assert!(matches!(plain[0], DiffIntent::Yank { .. }));
    }

    // ── visual mode ─────────────────────────────────────────────────────────

    #[test]
    fn capital_v_selects_line_wise() {
        let mut c = core();
        c.apply_notation("V");
        assert_eq!(c.mode().as_deref(), Some("-- VISUAL LINE --"));
        assert_eq!(c.selection_rows(), Some((0, 0)));
        c.apply_notation("jj");
        assert_eq!(c.selection_rows(), Some((0, 2)));
        c.apply_notation("<Esc>");
        assert_eq!(c.mode(), None, "Esc returns to normal");
        assert_eq!(c.selection_rows(), None);
    }

    /// Character-wise `v` is real as of slice 6 phase B: the multi-rect
    /// material can draw the selection, and its yank has a semantics of its
    /// own ([`YankKind::PlainText`]).
    #[test]
    fn lowercase_v_selects_character_wise() {
        let mut c = core();
        c.apply_notation("5G0v");
        assert_eq!(c.mode().as_deref(), Some("-- VISUAL --"));
        let span = c.char_selection().expect("a character-wise selection");
        assert_eq!(
            span,
            CharSpan {
                start_row: 4,
                start_col: 0,
                end_row: 4,
                end_col: 0,
            },
            "a fresh `v` selects the character under the cursor",
        );

        c.apply_notation("ll");
        assert_eq!(c.char_selection().map(|s| s.end_col), Some(2));
        c.apply_notation("j");
        let span = c.char_selection().expect("a selection");
        assert_eq!((span.start_row, span.end_row), (4, 5));

        c.apply_notation("<Esc>");
        assert_eq!(c.mode(), None, "Esc returns to normal");
        assert_eq!(c.char_selection(), None);
    }

    /// `V` is line-wise and stays that way: no character span, so a renderer
    /// cannot accidentally draw a ragged band for it.
    #[test]
    fn a_line_wise_selection_has_no_character_span() {
        let mut c = core();
        c.apply_notation("5G$Vj");
        assert_eq!(c.selection_rows(), Some((4, 5)));
        assert_eq!(c.char_selection(), None);
    }

    /// Visual BLOCK is the shape with no semantics: its yank is a
    /// *rectangle*, cutting through the prefix column — `<C-v>jly` used to
    /// hand back `"di\n--"`. It must not be reachable.
    #[test]
    fn block_visual_does_nothing() {
        for keys in ["<C-v>", "<C-q>"] {
            let mut c = core();
            assert!(c.apply_notation(keys).is_empty(), "keys {keys:?}");
            assert_eq!(c.mode(), None, "{keys:?} must not enter visual mode");
            assert_eq!(c.selection_rows(), None);

            let yanked = c.apply_notation("jly");
            assert!(
                yanked.is_empty(),
                "{keys:?} left a yankable selection: {yanked:?}",
            );
        }
    }

    // ── yank ────────────────────────────────────────────────────────────────

    /// The core yank contract: the exact canonical unified text of the
    /// selected rows, prefixes included.
    #[test]
    fn visual_line_yank_is_the_exact_canonical_text_of_those_rows() {
        let mut c = core();
        // Park on the first hunk header, then select it and the two rows below.
        c.apply_notation("]c");
        let first = c.cursor_row();
        let intents = c.apply_notation("Vjjy");
        let expected: String = (first..=first + 2)
            .map(|i| format!("{}\n", row_text(&c, i)))
            .collect();
        assert_eq!(
            intents,
            vec![DiffIntent::Yank {
                text: expected,
                kind: YankKind::Patch,
            }],
        );
        assert_eq!(c.mode(), None, "yank leaves visual mode");
    }

    #[test]
    fn yy_yanks_the_cursor_line_with_its_prefix() {
        let mut c = core();
        let insert = c
            .rows()
            .iter()
            .position(|r| r.kind == RowKind::Body(LineKind::Insert))
            .expect("an insertion row");
        c.apply_notation(&format!("{}G", insert + 1));
        assert_eq!(c.cursor_row(), insert);
        let intents = c.apply_notation("yy");
        let text = format!("{}\n", row_text(&c, insert));
        assert!(text.starts_with('+'), "prefix must survive: {text:?}");
        assert_eq!(
            intents,
            vec![DiffIntent::Yank {
                text,
                kind: YankKind::Patch,
            }],
        );
    }

    #[test]
    fn a_counted_yank_takes_that_many_rows() {
        let mut c = core();
        let intents = c.apply_notation("3yy");
        let expected: String = (0..3).map(|i| format!("{}\n", row_text(&c, i))).collect();
        assert_eq!(
            intents,
            vec![DiffIntent::Yank {
                text: expected,
                kind: YankKind::Patch,
            }],
        );
    }

    /// A whole-document yank must re-parse as the same model — proof that the
    /// yanked text is a real patch in the dialect, not a rendering of one.
    #[test]
    fn yanking_the_whole_diff_round_trips_through_parse() {
        let mut c = core();
        let intents = c.apply_notation("VGy");
        let DiffIntent::Yank { text, .. } = &intents[0] else {
            panic!("expected a yank, got {intents:?}");
        };
        assert_eq!(text, c.text());
        assert_eq!(&parse(text).expect("the yank must parse"), c.model());
    }

    // ── character-wise yank: plain text, never a patch ──────────────────────

    /// The row index of the first insertion row in the fixture, and its text.
    fn insert_row(c: &DiffCore) -> usize {
        c.rows()
            .iter()
            .position(|r| r.kind == RowKind::Body(LineKind::Insert))
            .expect("an insertion row")
    }

    /// **The headline contract of phase B**: a character-wise yank hands back
    /// the *code*, never the diff's annotation of it. `v$y` on `+THREE` is
    /// `THREE`, with no `+` anywhere.
    #[test]
    fn a_character_wise_yank_drops_the_diff_prefix() {
        let mut c = core();
        let row = insert_row(&c);
        c.apply_notation(&format!("{}G0", row + 1));
        let line = row_text(&c, row).to_string();
        assert!(line.starts_with('+'), "fixture changed: {line:?}");

        let yanked = c.apply_notation("v$y");
        assert_eq!(
            yanked,
            vec![DiffIntent::Yank {
                text: line[1..].to_string(),
                kind: YankKind::PlainText,
            }],
            "the prefix column is not part of the line's text",
        );
        assert_eq!(c.mode(), None, "the yank leaves visual mode");
    }

    /// Starting the selection *past* the prefix is the common case — grabbing
    /// an identifier out of the middle of a line.
    #[test]
    fn a_character_wise_yank_takes_exactly_the_selected_characters() {
        let mut c = core();
        let row = insert_row(&c);
        let line = row_text(&c, row).to_string();
        // `+THREE` → skip the prefix and the `T`, then select three chars.
        let yanked = c.apply_notation(&format!("{}G0llvlly", row + 1));
        let chars: Vec<char> = line.chars().collect();
        let expected: String = chars[2..5].iter().collect();
        assert_eq!(
            yanked,
            vec![DiffIntent::Yank {
                text: expected,
                kind: YankKind::PlainText,
            }],
        );
    }

    /// Every row of a multi-row character selection loses its prefix, so the
    /// result is plain source: no line of it can start a patch.
    #[test]
    fn a_multi_row_character_yank_is_plain_text_on_every_line() {
        let mut c = core();
        let row = insert_row(&c);
        c.apply_notation(&format!("{}G0", row + 1));
        let yanked = c.apply_notation("v3j$y");
        let DiffIntent::Yank { text, kind } = &yanked[0] else {
            panic!("expected a yank, got {yanked:?}");
        };
        assert_eq!(*kind, YankKind::PlainText);
        assert_eq!(text.lines().count(), 4, "one line per selected row");
        for line in text.lines() {
            assert!(
                !line.starts_with(['+', '-']),
                "a prefix survived into plain text: {text:?}",
            );
        }
        // And the rows it came from really did carry prefixes.
        assert!(
            (row..row + 4).any(|r| row_text(&c, r).starts_with(['+', '-'])),
            "premise: the selection covered prefixed rows",
        );
    }

    /// A character-wise yank is vim's: no trailing newline, and the rows
    /// between the ends are taken whole.
    #[test]
    fn a_character_wise_yank_has_no_trailing_newline() {
        let mut c = core();
        let row = insert_row(&c);
        c.apply_notation(&format!("{}G0", row + 1));
        let yanked = c.apply_notation("vj$y");
        let DiffIntent::Yank { text, .. } = &yanked[0] else {
            panic!("expected a yank");
        };
        assert!(!text.ends_with('\n'), "char-wise never terminates: {text:?}");
    }

    /// A header row has no prefix column to strip — the `@@` line comes
    /// through whole, and still cannot be mistaken for a patch on its own.
    #[test]
    fn a_header_row_keeps_its_whole_text() {
        let mut c = core();
        let header = c
            .rows()
            .iter()
            .position(|r| r.kind == RowKind::HunkHeader)
            .expect("a hunk header");
        let line = row_text(&c, header).to_string();
        c.apply_notation(&format!("{}G0", header + 1));
        let yanked = c.apply_notation("v$y");
        assert_eq!(
            yanked,
            vec![DiffIntent::Yank {
                text: line,
                kind: YankKind::PlainText,
            }],
        );
    }

    /// Selecting only the prefix column yanks nothing at all — an empty
    /// clipboard write is not an intent the viewer emits.
    #[test]
    fn selecting_only_the_prefix_yanks_nothing() {
        let mut c = core();
        let row = insert_row(&c);
        c.apply_notation(&format!("{}G0", row + 1));
        assert!(
            c.apply_notation("vy").is_empty(),
            "the prefix alone is not text",
        );
    }

    /// **Enabling `v` must not loosen the operator guard by one inch.** The
    /// phase-A battery of fragment yanks is still refused: a fragment nobody
    /// saw highlighted is an accident of a motion, not a decision.
    #[test]
    fn enabling_v_did_not_reopen_the_operator_fragment_yanks() {
        for keys in [
            "yiw", "yaw", "yw", "y$", "ye", "yb", "y0", "yf@", "y^", "yg^", "yvj", "y<C-v>j",
            "may`a",
        ] {
            let mut c = core();
            c.apply_notation("5G$");
            assert!(
                c.apply_notation(keys).is_empty(),
                "keys {keys:?} yanked a fragment",
            );
        }
    }

    /// The two semantics never blur: whatever the cursor column, `V` yanks a
    /// patch and `v` yanks plain text.
    #[test]
    fn the_shape_decides_the_semantics() {
        let row = insert_row(&core());
        let kind = |keys: &str| {
            let mut c = core();
            c.apply_notation(&format!("{}G0", row + 1));
            match c.apply_notation(keys).first() {
                Some(DiffIntent::Yank { kind, .. }) => Some(*kind),
                other => panic!("keys {keys:?} yanked nothing: {other:?}"),
            }
        };
        assert_eq!(kind("Vy"), Some(YankKind::Patch));
        assert_eq!(kind("Vjy"), Some(YankKind::Patch));
        assert_eq!(kind("yy"), Some(YankKind::Patch));
        assert_eq!(kind("v$y"), Some(YankKind::PlainText));
        assert_eq!(kind("vj$y"), Some(YankKind::PlainText));
    }

    /// A character selection clamped by a fold reports visible coordinates,
    /// with a clamped end losing its column along with its row.
    #[test]
    fn the_drawn_character_selection_is_in_visible_coordinates() {
        let mut c = core();
        assert_eq!(c.char_selection_visible(), None);
        c.apply_notation("]czc");
        let header = c.cursor_row();
        c.apply_notation("vj");
        let span = c.char_selection_visible().expect("a selection");
        assert_eq!(span.start_row, c.visible_index_of(header).unwrap());
        assert_eq!(span.end_row, span.start_row + 1, "the fold is not drawn");
    }

    // ── read-only ───────────────────────────────────────────────────────────

    /// The surface is read-only *by construction*, not by convention: every
    /// editing key vim offers must leave the canonical text byte-identical.
    #[test]
    fn no_key_sequence_can_change_the_text() {
        let original = format(&sample());
        for keys in [
            "x",
            "3x",
            "dd",
            "5dd",
            "dw",
            "D",
            "C",
            "cwzzz<Esc>",
            "ihello<Esc>",
            "Ahello<Esc>",
            "onew<Esc>",
            "Onew<Esc>",
            "rZ",
            "J",
            ">>",
            "<<",
            "p",
            "P",
            "yyp",
            "u",
            "<C-r>",
            "Vd",
            "Vc",
            "S",
            "s",
            "~",
            "guu",
            "gUU",
        ] {
            let mut c = core();
            c.apply_notation(keys);
            assert_eq!(c.text(), original, "keys {keys:?} changed the buffer");
            assert_eq!(c.text(), format(c.model()));
        }
    }

    // ── folds ───────────────────────────────────────────────────────────────

    #[test]
    fn fold_verbs_drive_the_hunk_under_the_cursor() {
        let mut c = core();
        c.apply_notation("]c");
        let cursor = c.cursor_row();
        let row = c.rows()[cursor];
        let (fi, hi) = (row.file.unwrap(), row.hunk.unwrap());
        let fold = |c: &DiffCore| c.model().files[fi].hunks[hi].fold;

        assert_eq!(fold(&c), FoldState::Expanded);
        c.apply_notation("zc");
        assert_eq!(fold(&c), FoldState::Folded);
        c.apply_notation("zo");
        assert_eq!(fold(&c), FoldState::Expanded);
        c.apply_notation("za");
        assert_eq!(fold(&c), FoldState::Folded);
        c.apply_notation("za");
        assert_eq!(fold(&c), FoldState::Expanded);
    }

    /// Folds are view state the formatter ignores, so folding must never move
    /// the rows out from under the cursor.
    #[test]
    fn folding_does_not_change_the_canonical_text_or_the_rows() {
        let mut c = core();
        let before_text = c.text().to_string();
        let before_rows = c.rows().to_vec();
        c.apply_notation("]czc");
        assert_eq!(c.text(), before_text);
        assert_eq!(c.rows(), before_rows.as_slice());
    }

    /// Folding is a *projection*: the canonical rows stay, and the visible
    /// list loses exactly the folded hunk's body — header included, because
    /// the header is the indicator.
    #[test]
    fn a_folded_hunk_hides_its_body_and_keeps_its_header() {
        let mut c = core();
        assert_eq!(
            c.visible_rows().len(),
            c.rows().len(),
            "nothing folded: every row is visible",
        );

        c.apply_notation("]c");
        let header = c.cursor_row();
        let (fi, hi) = (c.rows()[header].file.unwrap(), c.rows()[header].hunk.unwrap());
        let body = c.model().files[fi].hunks[hi].lines.len();
        c.apply_notation("zc");

        assert_eq!(c.visible_rows().len(), c.rows().len() - body);
        assert!(
            c.visible_rows().contains(&header),
            "the header must survive as the indicator",
        );
        assert_eq!(
            c.fold_summary(header),
            Some(FoldSummary { hidden_rows: body }),
        );
        // ...and every hidden row belongs to that hunk.
        for (i, row) in c.rows().iter().enumerate() {
            if c.visible_index_of(i).is_none() {
                assert_eq!((row.file, row.hunk), (Some(fi), Some(hi)));
                assert_ne!(row.kind, RowKind::HunkHeader);
            }
        }

        c.apply_notation("zo");
        assert_eq!(c.visible_rows().len(), c.rows().len(), "unfold restores");
        assert_eq!(c.fold_summary(header), None);
    }

    /// The visible list is a subsequence of the real rows — ascending, in
    /// order, no invented entries. Everything the viewer addresses by index
    /// depends on that.
    #[test]
    fn the_visible_projection_is_an_ascending_subsequence() {
        let mut c = core();
        c.apply_notation("]czc]czc");
        let v = c.visible_rows();
        assert!(v.windows(2).all(|w| w[0] < w[1]), "not ascending: {v:?}");
        assert!(v.iter().all(|&i| i < c.rows().len()));
        for (i, &row) in v.iter().enumerate() {
            assert_eq!(c.visible_index_of(row), Some(i));
        }
    }

    /// `j` steps *over* a closed fold: the fold behaves as one line, so
    /// walking down lands past it, never inside it.
    #[test]
    fn j_steps_over_a_closed_fold() {
        let mut c = core();
        c.apply_notation("]czc");
        let header = c.cursor_row();
        let after = c
            .visible_rows()
            .iter()
            .copied()
            .find(|&r| r > header)
            .expect("a row after the fold");

        c.apply_notation("j");
        assert_eq!(c.cursor_row(), after, "j must clear the whole fold");
        c.apply_notation("k");
        assert_eq!(c.cursor_row(), header, "k comes back to the header");
    }

    /// Folding the hunk the cursor is standing *inside* parks it on the
    /// header — the alternative is a cursor on a row that is no longer drawn.
    #[test]
    fn folding_from_inside_parks_the_cursor_on_the_header() {
        let mut c = core();
        c.apply_notation("]cjj");
        let inside = c.cursor_row();
        let header = c.rows()[..inside]
            .iter()
            .rposition(|r| r.kind == RowKind::HunkHeader)
            .expect("a header above");
        assert_ne!(inside, header, "test premise: the cursor is in the body");

        c.apply_notation("zc");
        assert_eq!(c.cursor_row(), header);
        assert!(c.visible_index_of(c.cursor_row()).is_some());
    }

    /// The cursor is never on a hidden row, whatever the motion — the
    /// invariant the whole projection rests on.
    #[test]
    fn no_motion_leaves_the_cursor_inside_a_fold() {
        let mut c = core();
        // Fold every hunk, then walk the document with everything we have.
        c.apply_notation("]czc]czc]czc");
        for keys in ["j", "k", "G", "gg", "5j", "3k", "}", "{", "]c", "[c", "L", "H"] {
            c.apply_notation(keys);
            assert!(
                c.visible_index_of(c.cursor_row()).is_some(),
                "keys {keys:?} left the cursor on hidden row {}",
                c.cursor_row(),
            );
        }
    }

    /// A selection across a folded hunk still yanks every line of it — the
    /// canonical text is what a yank slices, and folding never touches it.
    /// The *drawn* range is the visible rows it covers.
    #[test]
    fn a_selection_over_a_fold_yanks_the_hidden_lines_too() {
        let mut c = core();
        c.apply_notation("]czc");
        let header = c.cursor_row();
        let intents = c.apply_notation("Vjy");
        let DiffIntent::Yank { text, .. } = &intents[0] else {
            panic!("expected a yank, got {intents:?}");
        };
        // `j` cleared the fold, so the selection spans the whole hunk.
        let end = c
            .visible_rows()
            .iter()
            .copied()
            .find(|&r| r > header)
            .unwrap();
        let expected: String = (header..=end)
            .map(|i| format!("{}\n", row_text(&c, i)))
            .collect();
        assert_eq!(text, &expected);
        assert!(
            text.lines().count() > 2,
            "a folded hunk's hidden lines must still be yanked: {text:?}",
        );
    }

    /// The drawn selection is in *visible* coordinates, clamped inward — a
    /// renderer that laid it out in canonical row indices would band the
    /// wrong lines the moment anything above it folded.
    #[test]
    fn the_drawn_selection_is_in_visible_coordinates() {
        let mut c = core();
        assert_eq!(c.selection_visible_rows(), None, "no selection, no range");
        c.apply_notation("]czc");
        let header = c.cursor_row();
        c.apply_notation("Vj");
        let (a, b) = c.selection_visible_rows().expect("a selection");
        assert_eq!(a, c.visible_index_of(header).unwrap());
        assert_eq!(b, a + 1, "the fold's body is not drawn between them");
    }

    #[test]
    fn a_fold_verb_on_a_file_header_row_is_a_no_op() {
        let mut c = core();
        assert_eq!(c.rows()[0].kind, RowKind::FileHeader);
        c.apply_notation("zc");
        assert!(
            c.model()
                .files
                .iter()
                .flat_map(|f| f.hunks.iter())
                .all(|h| h.fold == FoldState::Expanded),
            "a fold on a header row must not fold some other hunk"
        );
    }

    // ── close / refresh ─────────────────────────────────────────────────────

    #[test]
    fn q_zq_zz_and_colon_q_all_close() {
        for keys in ["q", "ZQ", "ZZ", ":q<CR>", ":q!<CR>", ":quit<CR>"] {
            let mut c = core();
            assert_eq!(
                c.apply_notation(keys),
                vec![DiffIntent::Close],
                "keys {keys:?}"
            );
        }
    }

    #[test]
    fn r_and_colon_e_ask_for_a_refresh() {
        for keys in ["R", ":e<CR>", ":refresh<CR>"] {
            let mut c = core();
            assert_eq!(
                c.apply_notation(keys),
                vec![DiffIntent::Refresh],
                "keys {keys:?}"
            );
        }
    }

    /// Esc belongs to the vi surface: it leaves visual mode and never closes
    /// the viewer (`docs/input.md` Esc doctrine, `docs/diff.md` slice 5).
    #[test]
    fn escape_never_closes() {
        let mut c = core();
        assert!(c.apply_notation("<Esc>").is_empty());
        assert!(c.apply_notation("V<Esc>").is_empty());
        assert!(c.apply_notation("<Esc><Esc>").is_empty());
    }

    #[test]
    fn an_aborted_command_line_runs_nothing() {
        let mut c = core();
        assert!(c.apply_notation(":q<Esc>").is_empty(), "Esc aborts the bar");
        assert_eq!(c.command_line(), None);
    }

    #[test]
    fn the_command_line_is_visible_while_typing() {
        let mut c = core();
        assert_eq!(c.command_line(), None);
        c.apply_notation(":q");
        assert_eq!(c.command_line().as_deref(), Some(":q"));
        c.apply_notation("<CR>");
        assert_eq!(c.command_line(), None);
    }

    #[test]
    fn an_unknown_ex_command_is_ignored_not_a_close() {
        let mut c = core();
        assert!(c.apply_notation(":frobnicate<CR>").is_empty());
        assert!(c.apply_notation(":w<CR>").is_empty());
    }

    /// A `q` typed into the `:`-line is a character, not a close — modalkit
    /// owns the mode, which is exactly why the viewer core does.
    #[test]
    fn a_q_inside_the_command_line_is_not_a_close() {
        let mut c = core();
        assert!(c.apply_notation(":q").is_empty(), "no close until submit");
        assert_eq!(c.command_line().as_deref(), Some(":q"));
    }
}
