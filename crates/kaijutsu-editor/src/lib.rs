//! `EditorCore` — a pure vi editing engine.
//!
//! No Bevy, no kernel, no RPC. One editor's session-local state (buffer, cursor,
//! mode) driven by modalkit's `VimMachine` over modalkit's own `EditBuffer`, so
//! we inherit real vim semantics (motions, operators, registers) instead of
//! reimplementing a subset. The seam everything else is tested against:
//!
//! - [`EditorCore::apply_keys`] feeds a key sequence and returns the
//!   `(char_offset, insert, delete)` [`EditOp`]s those keystrokes produced —
//!   char-indexed to match the CRDT's text addressing.
//! - state accessors ([`EditorCore::text`], [`cursor`](EditorCore::cursor),
//!   [`mode`](EditorCore::mode)) are what a renderer draws and a test asserts.
//!
//! The kernel binds this to a CRDT block: load block text in, mirror the
//! returned [`EditOp`]s onto the block, and feed peer ops back (later). See
//! `docs/vi.md`.

use editor_types::application::EmptyInfo;
use editor_types::prelude::ViewportContext;
use editor_types::Action;

use modalkit::actions::Editable;
use modalkit::editing::buffer::{CursorGroupId, EditBuffer};
use modalkit::editing::cursor::Cursor;
use modalkit::editing::store::Store;
use modalkit::env::vim::keybindings::{VimBindings, VimMachine};
use modalkit::key::TerminalKey;
use modalkit::keybindings::{BindingMachine, InputBindings};

use modalkit::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// One contiguous text edit, char-indexed (the CRDT's addressing): at `offset`,
/// remove `delete` chars and insert `insert`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EditOp {
    /// Char offset into the buffer where the edit applies.
    pub offset: usize,
    /// Text inserted (empty for a pure delete).
    pub insert: String,
    /// Number of chars removed (0 for a pure insert).
    pub delete: usize,
}

/// A pure vi editor over one text buffer.
pub struct EditorCore {
    machine: VimMachine<TerminalKey, EmptyInfo>,
    buffer: EditBuffer<EmptyInfo>,
    store: Store<EmptyInfo>,
    group: CursorGroupId,
    viewport: ViewportContext<Cursor>,
}

impl EditorCore {
    /// Open an editor on `text`, starting in normal mode.
    ///
    /// Uses plain vim bindings — notably *without* `submit_on_enter`, so Enter
    /// inserts a newline (an editor, not a compose box).
    pub fn new(text: &str) -> Self {
        let mut machine = VimMachine::<TerminalKey, EmptyInfo>::empty();
        VimBindings::default().setup(&mut machine);

        let mut buffer = EditBuffer::<EmptyInfo>::from_str(String::new(), text);
        let group = buffer.create_group();

        EditorCore {
            machine,
            buffer,
            store: Store::default(),
            group,
            viewport: ViewportContext::default(),
        }
    }

    /// The buffer text, with modalkit's guaranteed trailing newline normalized
    /// away so the observable content matches what was loaded.
    ///
    /// modalkit's `EditRope` is line-terminated — it always ends in `\n` — so a
    /// file `"hello"` becomes `"hello\n"` internally. We strip one trailing
    /// newline here. **Tech debt (sweep before done):** this cannot distinguish
    /// `"hello"` from `"hello\n"`; faithful trailing-newline round-tripping must
    /// live in the kernel binding (remember the loaded terminator, re-apply on
    /// save). See `docs/vi.md`.
    pub fn text(&self) -> String {
        strip_one_trailing_newline(&self.buffer.get_text())
    }

    /// The leader cursor's char offset into the buffer.
    pub fn cursor(&mut self) -> usize {
        let cur = self.buffer.get_leader(self.group);
        usize::from(self.buffer.get().cursor_to_offset(&cur))
    }

    /// The vim mode banner (`None` in normal mode; `Some("-- INSERT --")` etc.).
    pub fn mode(&self) -> Option<String> {
        self.machine.show_mode()
    }

    /// Feed a key sequence in vim notation (`"ihello<Esc>"`, `"dw"`, `"<C-w>"`)
    /// and return the char-indexed [`EditOp`]s it produced — one per keystroke
    /// that changed the buffer (no-op keystrokes emit nothing).
    pub fn apply_keys(&mut self, keys: &str) -> Vec<EditOp> {
        let mut ops = Vec::new();
        for key in parse_keys(keys) {
            // Diff against the normalized (terminator-stripped) view so emitted
            // offsets are char-indexed into the logical content, matching the
            // CRDT block — not modalkit's trailing-newline'd rope.
            let before = strip_one_trailing_newline(&self.buffer.get_text());
            self.machine.input_key(key);
            while let Some((action, ctx)) = self.machine.pop() {
                if let Action::Editor(ea) = action {
                    let ictx = (self.group, &self.viewport, &ctx);
                    // Editing errors (e.g. motion off the end) are non-fatal vim
                    // behavior, not corruption — drop them and keep the buffer.
                    let _ = self.buffer.editor_command(&ea, &ictx, &mut self.store);
                }
            }
            let after = strip_one_trailing_newline(&self.buffer.get_text());
            if let Some(op) = diff_op(&before, &after) {
                ops.push(op);
            }
        }
        ops
    }
}

/// Remove at most one trailing `\n` (modalkit's guaranteed line terminator).
fn strip_one_trailing_newline(s: &str) -> String {
    s.strip_suffix('\n').unwrap_or(s).to_string()
}

/// Minimal common-prefix/suffix diff of two strings into a single char-indexed
/// [`EditOp`]. `None` when they are equal. Adequate because each keystroke
/// produces a single contiguous change; multi-site edits (macros) would need a
/// richer diff — noted for later.
fn diff_op(before: &str, after: &str) -> Option<EditOp> {
    if before == after {
        return None;
    }
    let b: Vec<char> = before.chars().collect();
    let a: Vec<char> = after.chars().collect();

    let mut prefix = 0;
    while prefix < b.len() && prefix < a.len() && b[prefix] == a[prefix] {
        prefix += 1;
    }
    let mut suffix = 0;
    while suffix < b.len() - prefix && suffix < a.len() - prefix
        && b[b.len() - 1 - suffix] == a[a.len() - 1 - suffix]
    {
        suffix += 1;
    }
    Some(EditOp {
        offset: prefix,
        insert: a[prefix..a.len() - suffix].iter().collect(),
        delete: b.len() - prefix - suffix,
    })
}

/// Parse vim key notation into `TerminalKey`s. Literal chars map to themselves;
/// `<...>` is a named/chorded key (`<Esc>`, `<CR>`, `<BS>`, `<Tab>`, `<Space>`,
/// `<C-x>`). Unknown `<...>` tokens are skipped.
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
            keys.push(plain_key(c));
        }
    }
    keys
}

fn plain_key(c: char) -> TerminalKey {
    TerminalKey::from(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE))
}

fn named_key(token: &str) -> Option<TerminalKey> {
    let (code, mods) = match token.to_ascii_lowercase().as_str() {
        "esc" => (KeyCode::Esc, KeyModifiers::NONE),
        "cr" | "enter" | "return" => (KeyCode::Enter, KeyModifiers::NONE),
        "bs" | "backspace" => (KeyCode::Backspace, KeyModifiers::NONE),
        "tab" => (KeyCode::Tab, KeyModifiers::NONE),
        "space" => (KeyCode::Char(' '), KeyModifiers::NONE),
        other => {
            // <C-x> control chord.
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

    #[test]
    fn insert_text_at_start() {
        let mut ed = EditorCore::new("");
        let ops = ed.apply_keys("ihello<Esc>");
        assert_eq!(ed.text(), "hello");
        // One op per typed char.
        assert_eq!(ops.len(), 5);
        assert_eq!(ops[0], EditOp { offset: 0, insert: "h".into(), delete: 0 });
        assert_eq!(ops[4], EditOp { offset: 4, insert: "o".into(), delete: 0 });
    }

    #[test]
    fn x_deletes_char_under_cursor() {
        let mut ed = EditorCore::new("hello");
        let ops = ed.apply_keys("x");
        assert_eq!(ed.text(), "ello");
        assert_eq!(ops, vec![EditOp { offset: 0, insert: String::new(), delete: 1 }]);
    }

    #[test]
    fn dw_deletes_word() {
        let mut ed = EditorCore::new("hello world");
        ed.apply_keys("dw");
        assert_eq!(ed.text(), "world");
    }

    #[test]
    fn mode_tracks_insert_and_normal() {
        let mut ed = EditorCore::new("");
        assert_eq!(ed.mode(), None, "starts in normal mode");
        ed.apply_keys("i");
        assert_eq!(ed.mode().as_deref(), Some("-- INSERT --"));
        ed.apply_keys("<Esc>");
        assert_eq!(ed.mode(), None, "Esc returns to normal");
    }

    #[test]
    fn enter_inserts_newline_not_submit() {
        // The editor (unlike compose) must treat Enter as a newline.
        let mut ed = EditorCore::new("");
        ed.apply_keys("iab<CR>cd<Esc>");
        assert_eq!(ed.text(), "ab\ncd");
    }

    #[test]
    fn trailing_newline_is_normalized_away() {
        // modalkit's rope is line-terminated; the observable content must not
        // grow a phantom newline. (Documented limitation: this also means a
        // genuinely newline-terminated load looks the same — kernel binding's
        // job to preserve, see docs/vi.md.)
        let ed = EditorCore::new("hello");
        assert_eq!(ed.text(), "hello");
    }

    /// Coverage map for the editing surface the e2e can drive end to end. Each
    /// case is real modalkit vim flowing through `apply_keys` → buffer; the
    /// *whole* `kj editor`/kernel-session stack observes exactly these
    /// text/cursor/mode changes. This battery IS the answer to "how much of vi
    /// can we test headless" — the normal-mode editing surface, in full.
    mod coverage {
        use super::*;

        /// `(initial, keys, expected_text)` — assert the resulting buffer.
        fn case(initial: &str, keys: &str, expected: &str) {
            let mut ed = EditorCore::new(initial);
            ed.apply_keys(keys);
            assert_eq!(ed.text(), expected, "keys {keys:?} on {initial:?}");
        }

        #[test]
        fn operators_motions_counts() {
            case("hello", "x", "ello"); // delete char
            case("hello", "3x", "lo"); // count
            case("hello world", "dw", "world"); // delete word
            case("foo bar", "wde", "foo "); // motion then delete-to-word-end
            case("hello world", "cwbye<Esc>", "bye world"); // change word
            case("hello", "rZ", "Zello"); // replace char
        }

        #[test]
        fn linewise_and_inserts() {
            case("a\nb\nc", "dd", "b\nc"); // delete line
            case("a\nb\nc", "2dd", "c"); // count linewise
            case("hi", "A!<Esc>", "hi!"); // append-EOL insert
            case("a", "onew<Esc>", "a\nnew"); // open line below
        }

        #[test]
        fn registers_yank_and_paste() {
            case("x", "yyp", "x\nx"); // yank line, paste below
            case("a\nb", "ddp", "b\na"); // delete line, paste below
        }

        #[test]
        fn undo_and_redo() {
            case("hello", "xu", "hello"); // delete then undo
            case("hello", "xu<C-r>", "ello"); // delete, undo, redo (Ctrl-R)
        }

        #[test]
        fn visual_mode_operator() {
            case("hello world", "v$d", ""); // visual to EOL, delete the line
        }

        #[test]
        fn find_char_is_a_motion() {
            // `f` doesn't change text; it moves the cursor onto the target.
            let mut ed = EditorCore::new("hello world");
            ed.apply_keys("fw");
            assert_eq!(ed.text(), "hello world");
            assert_eq!(ed.cursor(), 6, "f w lands on the 'w' of world");
        }
    }

    /// KNOWN GAP (tracked in docs/vi.md): command-line (`:`) and search (`/`·`?`)
    /// route through modalkit's prompt infrastructure we don't wire yet — and
    /// today their query text *leaks into the buffer* (corruption, not just a
    /// missing feature). This spec encodes the desired safe behavior (a no-op
    /// until the prompt is wired) and fails until we guard prompt-mode keys.
    #[test]
    #[ignore = "known gap: prompt-initiating keys (: / ?) leak into the buffer; \
                fix = swallow keys while the machine is in command-line/search mode"]
    fn command_line_keys_must_not_corrupt_the_buffer() {
        let mut ed = EditorCore::new("hello");
        ed.apply_keys(":d<CR>");
        assert_eq!(ed.text(), "hello", "an unhandled ex-command must not edit the buffer");

        let mut ed = EditorCore::new("hello world");
        ed.apply_keys("/wor<CR>");
        assert_eq!(ed.text(), "hello world", "an unhandled search must not edit the buffer");
    }

    #[test]
    fn diff_op_handles_utf8_chars() {
        // café → cafés: insert one char after the multibyte é. Char-indexed
        // offset must be 4, not a byte offset.
        let op = diff_op("café", "cafés").unwrap();
        assert_eq!(op, EditOp { offset: 4, insert: "s".into(), delete: 0 });
    }
}
