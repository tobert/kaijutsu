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
use editor_types::prelude::{CloseFlags, ViewportContext};
use editor_types::{Action, CommandBarAction, PromptAction, WindowAction};

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

/// A `ZZ`/`ZQ` close request modalkit surfaced during [`EditorCore::apply_keys`].
///
/// modalkit's vim bindings map `ZZ` → `WindowAction::Close(_, CloseFlags::WQ)`
/// (write + quit) and `ZQ` → `CloseFlags::FQ` (force quit). We have no window
/// system, so rather than execute the close we record the *intent* and let the
/// kernel act on it (checkpoint+drop vs discard+drop) — modalkit, which owns the
/// real mode state, is the only place that can tell `ZZ` from an inserted `ZZ`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CloseRequest {
    /// `ZZ` — checkpoint (flush) then quit.
    Write,
    /// `ZQ` — discard changes since the last checkpoint, then quit.
    Discard,
}

/// An ex-command (`:`-line) intent the editor surfaced — the command-mode
/// sibling of [`CloseRequest`].
///
/// `kaijutsu-editor` is pure (no kernel, no RPC), so it parses the typed `:`
/// command into *intents* and lets the kernel act on them (`Write` → checkpoint,
/// `Quit` → drop/rollback) — exactly the [`take_close`](EditorCore::take_close)
/// pattern, now for the command line. The kernel consumes a batch via
/// [`take_commands`](EditorCore::take_commands).
///
/// Pass-1 dialect (`docs/vi.md` → *Command mode*): `:w :q :wq :q! :x :w!`.
/// Substitute / shell / read / edit are later passes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CommandRequest {
    /// `:w` / `:w!` — checkpoint (save) the buffer; **stay open**. `force` (`!`)
    /// is recorded for a future read-only / permission gate (rc/config has none
    /// today), so it is presently a no-op distinction.
    Write { force: bool },
    /// `:q` / `:q!` — quit. `force` (`!`) discards changes since the last
    /// checkpoint; without it a *dirty* buffer must refuse (vim's "No write
    /// since last change"), which is the kernel's call to make.
    Quit { force: bool },
}

/// A pure vi editor over one text buffer.
pub struct EditorCore {
    machine: VimMachine<TerminalKey, EmptyInfo>,
    buffer: EditBuffer<EmptyInfo>,
    store: Store<EmptyInfo>,
    group: CursorGroupId,
    viewport: ViewportContext<Cursor>,
    /// True while modalkit's command-line / search bar (`:`, `/`, `?`) is
    /// focused. That bar is a *separate* buffer (`cmdline`); the document must
    /// not see its keystrokes. Set on `CommandBar(Focus)`, cleared when the
    /// prompt submits/aborts.
    cmdline_active: bool,
    /// The bar's prefix (`:` for ex-commands, `/`·`?` for search) — kept so the
    /// renderer can draw the full line (`:wq`) and so submit knows which dialect
    /// to parse.
    cmdline_prefix: String,
    /// The command-line text buffer modalkit's edit actions flow into while the
    /// bar is focused — a real `EditBuffer`, so backspace / cursor motion work
    /// like vim's `:`-line. Reset on each focus.
    cmdline: EditBuffer<EmptyInfo>,
    cmdline_group: CursorGroupId,
    /// A `ZZ`/`ZQ` close intent produced by the last `apply_keys`, awaiting the
    /// kernel to consume it via [`take_close`](EditorCore::take_close).
    pending_close: Option<CloseRequest>,
    /// The outcome of a `:`-line submitted during the last `apply_keys`, awaiting
    /// the kernel via [`take_commands`](EditorCore::take_commands). `None` if no
    /// command was submitted; `Some(Err)` for an unknown command (fail loud).
    pending_commands: Option<Result<Vec<CommandRequest>, String>>,
    /// A `:s` substitution submitted during the current `apply_keys`, applied to
    /// the document buffer right after the keystroke that submitted it — so the
    /// per-keystroke diff turns it into [`EditOp`]s the kernel mirrors (unlike
    /// `:w`/`:q`, substitute is an *edit*, not a session-lifecycle command).
    pending_substitution: Option<Substitution>,
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

        let mut cmdline = EditBuffer::<EmptyInfo>::from_str(String::new(), "");
        let cmdline_group = cmdline.create_group();

        EditorCore {
            machine,
            buffer,
            store: Store::default(),
            group,
            viewport: ViewportContext::default(),
            cmdline_active: false,
            cmdline_prefix: String::new(),
            cmdline,
            cmdline_group,
            pending_close: None,
            pending_commands: None,
            pending_substitution: None,
        }
    }

    /// Take any `ZZ`/`ZQ` close intent the most recent `apply_keys` produced.
    /// The kernel calls this after every key batch to learn whether the session
    /// should be saved/discarded and dropped.
    pub fn take_close(&mut self) -> Option<CloseRequest> {
        self.pending_close.take()
    }

    /// Take any `:`-line command batch the most recent `apply_keys` submitted.
    /// `None` if no command was entered; `Some(Ok(cmds))` for a parsed dialect
    /// (possibly empty — a bare `:` or an unwired `/` search); `Some(Err(msg))`
    /// for an unknown command, which the kernel surfaces (fail loud, vim's
    /// "Not an editor command"). Consumed like [`take_close`](Self::take_close).
    pub fn take_commands(&mut self) -> Option<Result<Vec<CommandRequest>, String>> {
        self.pending_commands.take()
    }

    /// The command line the renderer should draw, with its prefix — e.g.
    /// `Some(":wq")` while the user is typing an ex-command, `None` when the bar
    /// is unfocused. The kernel surfaces this on `EditorState` so a renderer can
    /// show the `:`-strip without tracking mode itself.
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

    /// Reconcile the buffer to `new_text` — the authoritative, already-merged
    /// CRDT block content after a **peer's** edit landed on the bound block —
    /// and transform the leader cursor so it tracks the change. Returns whether
    /// the buffer actually changed; `false` when `new_text` already equals the
    /// buffer (e.g. this session's own mirrored write echoing back through the
    /// block flow — the caller relies on this to skip self-writes).
    ///
    /// Pass 1 reconciles at the **text level**: the block is the merged truth,
    /// so a re-read is canonical, and the single-region [`diff_op`] drives a
    /// one-region cursor transform (an edit fully before the cursor shifts it by
    /// the net length change; an edit at/after it leaves it put; a straddling
    /// edit lands it at the end of the replacement). Richer multi-site op
    /// transforms are future work (docs/vi.md risk #2). `set_text` resets undo
    /// history — acceptable: a remote merge is already a disruptive event.
    pub fn apply_remote_text(&mut self, new_text: &str) -> bool {
        let old = self.text();
        let Some(op) = diff_op(&old, new_text) else {
            return false; // identical — nothing to merge (self-write echo)
        };
        let new_cursor = transform_cursor(self.cursor(), &op).min(new_text.chars().count());
        self.buffer.set_text(new_text);
        let (line, col) = line_col(new_text, new_cursor);
        self.buffer.set_leader(self.group, Cursor::new(line, col));
        true
    }

    /// Feed a key sequence in vim notation (`"ihello<Esc>"`, `"dw"`, `"<C-w>"`)
    /// and return the char-indexed [`EditOp`]s it produced — one per keystroke
    /// that changed the buffer (no-op keystrokes emit nothing).
    pub fn apply_keys(&mut self, keys: &str) -> Vec<EditOp> {
        let mut ops = Vec::new();
        // A fresh batch; any close/command intent from a prior batch was already
        // consumed by the kernel (or is irrelevant — the session is still open).
        self.pending_close = None;
        self.pending_commands = None;
        self.pending_substitution = None;
        for key in parse_keys(keys) {
            // Diff against the normalized (terminator-stripped) view so emitted
            // offsets are char-indexed into the logical content, matching the
            // CRDT block — not modalkit's trailing-newline'd rope.
            let before = strip_one_trailing_newline(&self.buffer.get_text());
            self.machine.input_key(key);
            while let Some((action, ctx)) = self.machine.pop() {
                match action {
                    // `:`/`/`/`?` focus the command-line/search bar — a separate
                    // `EditBuffer` (`cmdline`). Reset it and remember the prefix
                    // so subsequent keystrokes type into it, not the document.
                    Action::CommandBar(CommandBarAction::Focus(prefix, _ct, _act)) => {
                        self.cmdline_active = true;
                        self.cmdline_prefix = prefix;
                        self.cmdline = EditBuffer::<EmptyInfo>::from_str(String::new(), "");
                        self.cmdline_group = self.cmdline.create_group();
                    }
                    Action::CommandBar(CommandBarAction::Unfocus) => {
                        self.cmdline_active = false;
                    }
                    // `<CR>` in the bar submits: parse the typed line for its
                    // dialect. Lifecycle verbs (`:w`/`:q`/…) queue intents the
                    // kernel acts on; a `:s` queues a buffer edit applied below.
                    Action::Prompt(PromptAction::Submit) if self.cmdline_active => {
                        let body = strip_one_trailing_newline(&self.cmdline.get_text());
                        match parse_command_line(&self.cmdline_prefix, &body) {
                            Ok(ParsedLine::Commands(cmds)) => {
                                self.pending_commands = Some(Ok(cmds));
                            }
                            Ok(ParsedLine::Substitute(sub)) => {
                                self.pending_substitution = Some(sub);
                            }
                            Ok(ParsedLine::Noop) => {}
                            Err(e) => self.pending_commands = Some(Err(e)),
                        }
                        self.cmdline_active = false;
                    }
                    // `<Esc>` / `<C-C>` (Abort) and any other prompt action close
                    // the bar without running anything.
                    Action::Prompt(_) if self.cmdline_active => {
                        self.cmdline_active = false;
                    }
                    // While the bar is focused, edits go to `cmdline` — real
                    // command-line editing (insert, backspace, motion).
                    Action::Editor(ea) if self.cmdline_active => {
                        let ictx = (self.cmdline_group, &self.viewport, &ctx);
                        let _ = self.cmdline.editor_command(&ea, &ictx, &mut self.store);
                    }
                    Action::Editor(ea) => {
                        let ictx = (self.group, &self.viewport, &ctx);
                        // Editing errors (e.g. motion off the end) are non-fatal
                        // vim behavior, not corruption — drop them, keep the buffer.
                        let _ = self.buffer.editor_command(&ea, &ictx, &mut self.store);
                    }
                    // `ZZ`/`ZQ`: modalkit knows the real mode, so it only emits a
                    // window-close here when the keys truly mean quit (an inserted
                    // `ZZ` produces InsertText, not this). We have no windows —
                    // record the intent for the kernel. `WQ` = write+quit (`ZZ`);
                    // anything else here is force-quit (`ZQ` = `FQ`).
                    Action::Window(WindowAction::Close(_, flags)) if !self.cmdline_active => {
                        self.pending_close = Some(if flags.contains(CloseFlags::WRITE) {
                            CloseRequest::Write
                        } else {
                            CloseRequest::Discard
                        });
                    }
                    _ => {}
                }
            }
            // A submitted `:s` edits the document here, so the diff below turns
            // it into the EditOp(s) the kernel mirrors onto the CRDT block.
            if let Some(sub) = self.pending_substitution.take() {
                self.apply_substitution(&sub);
            }
            let after = strip_one_trailing_newline(&self.buffer.get_text());
            if let Some(op) = diff_op(&before, &after) {
                ops.push(op);
            }
        }
        ops
    }

    /// Run a parsed `:s` substitution against the document buffer and move the
    /// leader cursor onto the result (clamped). A no-match substitution leaves
    /// the buffer untouched (no EditOp). `set_text` resets undo — acceptable for
    /// pass 1 (a `:s` is a deliberate bulk edit); finer-grained undo is later
    /// work, tracked in `docs/vi.md`.
    fn apply_substitution(&mut self, sub: &Substitution) {
        let text = self.text();
        let cursor = self.cursor();
        let cursor_line = line_col(&text, cursor).0;
        let new = sub.apply(&text, cursor_line);
        if new == text {
            return; // no match — nothing changed
        }
        let new_cursor = cursor.min(new.chars().count());
        self.buffer.set_text(new.as_str());
        let (line, col) = line_col(&new, new_cursor);
        self.buffer.set_leader(self.group, Cursor::new(line, col));
    }
}

/// Remove at most one trailing `\n` (modalkit's guaranteed line terminator).
fn strip_one_trailing_newline(s: &str) -> String {
    s.strip_suffix('\n').unwrap_or(s).to_string()
}

/// What a submitted `:`-line parsed into. Lifecycle verbs ([`CommandRequest`])
/// are handed to the kernel; a [`Substitution`] is applied to the buffer here;
/// [`Noop`](ParsedLine::Noop) is a bare `:` or an unwired `/` search.
enum ParsedLine {
    Commands(Vec<CommandRequest>),
    Substitute(Substitution),
    Noop,
}

/// Parse a submitted command line. `prefix` selects the dialect: `:` is the
/// ex-command line; `/`·`?` are search, **not wired yet** — a safe no-op so an
/// accidental search never edits the document. `body` is the text after the
/// prefix.
fn parse_command_line(prefix: &str, body: &str) -> Result<ParsedLine, String> {
    if prefix.starts_with(':') {
        parse_ex_command(body)
    } else {
        // `/`·`?` search — deferred (docs/vi.md). Submitting one does nothing.
        Ok(ParsedLine::Noop)
    }
}

/// Parse the ex-command dialect. Substitute (`[range]s/pat/rep/flags`) is
/// recognized first (it isn't a bare verb); then the lifecycle verbs `w q wq x
/// exit` with an optional trailing `!` (force). A bare line is a no-op; an
/// unknown verb is a fail-loud error the kernel surfaces.
fn parse_ex_command(body: &str) -> Result<ParsedLine, String> {
    use CommandRequest::{Quit, Write};
    let body = body.trim();

    if let Some(sub) = parse_substitute(body)? {
        return Ok(ParsedLine::Substitute(sub));
    }

    // A trailing `!` forces; strip it before matching the verb.
    let (verb, force) = match body.strip_suffix('!') {
        Some(v) => (v.trim_end(), true),
        None => (body, false),
    };
    let cmds = match verb {
        "" => Vec::new(),
        "w" | "write" => vec![Write { force }],
        "q" | "quit" => vec![Quit { force }],
        "wq" => vec![Write { force }, Quit { force }],
        "x" | "xit" | "exit" => vec![Write { force }, Quit { force }],
        other => return Err(format!("Not an editor command: :{other}")),
    };
    Ok(ParsedLine::Commands(cmds))
}

/// Which lines a `:s` touches.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SubRange {
    /// No range prefix — the line the cursor is on.
    CurrentLine,
    /// `%` — the whole buffer.
    Whole,
    /// `N` or `N,M` — 1-indexed, inclusive (clamped to the buffer).
    Lines(usize, usize),
}

/// A parsed `:s` substitution: a compiled regex applied over a line range,
/// first-match-per-line unless `global` (`g`). The dialect is **Rust regex**
/// (and Rust replacement syntax — `$1` capture refs), a deliberate choice over
/// chasing vim's BRE flavor (docs/vi.md): the `:` line is its own dialect.
#[derive(Debug)]
struct Substitution {
    regex: regex::Regex,
    replacement: String,
    range: SubRange,
    global: bool,
}

impl Substitution {
    /// Apply to `text`, where `cursor_line` (0-indexed) is the current line for
    /// a rangeless `:s`. Returns the new text (equal to `text` on no match).
    fn apply(&self, text: &str, cursor_line: usize) -> String {
        let mut lines: Vec<String> = text.split('\n').map(str::to_string).collect();
        let last = lines.len().saturating_sub(1);
        let (start, end) = match self.range {
            SubRange::CurrentLine => (cursor_line.min(last), cursor_line.min(last)),
            SubRange::Whole => (0, last),
            // 1-indexed → 0-indexed, clamped; an inverted range touches nothing.
            SubRange::Lines(a, b) => (a.saturating_sub(1).min(last), b.saturating_sub(1).min(last)),
        };
        for line in lines.iter_mut().take(end + 1).skip(start) {
            *line = if self.global {
                self.regex.replace_all(line, self.replacement.as_str()).into_owned()
            } else {
                self.regex.replace(line, self.replacement.as_str()).into_owned()
            };
        }
        lines.join("\n")
    }
}

/// Recognize and parse a substitute line (`[range]s<delim>pat<delim>rep<delim>flags`).
/// Returns `Ok(None)` when `body` isn't a substitute (so the caller falls
/// through to the lifecycle verbs), `Ok(Some)` when it parses, and `Err` for a
/// malformed substitute (bad regex, unknown flag, empty pattern) — fail loud.
fn parse_substitute(body: &str) -> Result<Option<Substitution>, String> {
    let (range, rest) = split_range(body);
    let Some(after_s) = rest.strip_prefix('s') else {
        return Ok(None); // not `s...`
    };
    // `s` must be followed by a delimiter (a non-alphanumeric, non-space char);
    // otherwise this is `set`/`s`/etc., not our substitute.
    let delim = match after_s.chars().next() {
        Some(d) if !d.is_alphanumeric() && !d.is_whitespace() => d,
        _ => return Ok(None),
    };

    let (pat, rep, flags) = split_substitute(after_s, delim);
    if pat.is_empty() {
        return Err("empty :s pattern is not supported".to_string());
    }
    let mut global = false;
    let mut ignore_case = false;
    for f in flags.chars() {
        match f {
            'g' => global = true,
            'i' => ignore_case = true,
            other => return Err(format!("unsupported :s flag '{other}'")),
        }
    }
    let regex = regex::RegexBuilder::new(&pat)
        .case_insensitive(ignore_case)
        .build()
        .map_err(|e| format!("invalid :s pattern: {e}"))?;
    Ok(Some(Substitution {
        regex,
        replacement: rep,
        range,
        global,
    }))
}

/// Split a leading line-range off a command body, returning the range and the
/// remainder. Supports `%` (whole), `N` / `N,M` (1-indexed); anything else is
/// [`SubRange::CurrentLine`] with the body untouched.
fn split_range(body: &str) -> (SubRange, &str) {
    if let Some(rest) = body.strip_prefix('%') {
        return (SubRange::Whole, rest);
    }
    let d1: String = body.chars().take_while(|c| c.is_ascii_digit()).collect();
    if d1.is_empty() {
        return (SubRange::CurrentLine, body);
    }
    let after1 = &body[d1.len()..];
    if let Some(rest2) = after1.strip_prefix(',') {
        let d2: String = rest2.chars().take_while(|c| c.is_ascii_digit()).collect();
        if !d2.is_empty() {
            let n = d1.parse().unwrap_or(1);
            let m = d2.parse().unwrap_or(n);
            return (SubRange::Lines(n, m), &rest2[d2.len()..]);
        }
    }
    let n = d1.parse().unwrap_or(1);
    (SubRange::Lines(n, n), after1)
}

/// Split `<delim>pat<delim>rep<delim>flags` (the text from the first delimiter
/// onward) into `(pattern, replacement, flags)`. A `\` escapes the delimiter
/// (kept literal in the pattern); other `\x` escapes pass through to the regex.
/// A trailing delimiter / omitted sections are allowed (`s/a/b`, `s/a/b/`).
fn split_substitute(after_s: &str, delim: char) -> (String, String, String) {
    let mut parts: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut escaped = false;
    // Skip the opening delimiter.
    for c in after_s.chars().skip(1) {
        if escaped {
            if c == delim {
                cur.push(delim); // `\<delim>` → a literal delimiter in the field
            } else {
                cur.push('\\');
                cur.push(c); // keep `\x` for the regex engine (e.g. `\d`)
            }
            escaped = false;
        } else if c == '\\' {
            escaped = true;
        } else if c == delim {
            parts.push(std::mem::take(&mut cur));
        } else {
            cur.push(c);
        }
    }
    if escaped {
        cur.push('\\'); // a dangling trailing backslash
    }
    parts.push(cur);
    let pat = parts.first().cloned().unwrap_or_default();
    let rep = parts.get(1).cloned().unwrap_or_default();
    let flags = parts.get(2).cloned().unwrap_or_default();
    (pat, rep, flags)
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

/// Transform a char-offset cursor against a single [`EditOp`] applied to the
/// same buffer (used when a peer's edit merges in). The three cases mirror
/// standard operational-transform cursor adjustment:
/// - edit entirely **before** the cursor → shift by the net length change;
/// - edit at or **after** the cursor → cursor unaffected;
/// - edit **straddling** the cursor → land at the end of the inserted region.
fn transform_cursor(cursor: usize, op: &EditOp) -> usize {
    let insert_len = op.insert.chars().count();
    if op.offset + op.delete <= cursor {
        // `op.delete <= cursor` here, so the subtraction can't underflow.
        cursor - op.delete + insert_len
    } else if op.offset >= cursor {
        cursor
    } else {
        op.offset + insert_len
    }
}

/// Convert a char offset into a `(line, column)` pair over `text` — the form
/// modalkit's [`Cursor::new`] wants. Clamps implicitly by stopping at the end.
fn line_col(text: &str, char_off: usize) -> (usize, usize) {
    let mut line = 0;
    let mut col = 0;
    for ch in text.chars().take(char_off) {
        if ch == '\n' {
            line += 1;
            col = 0;
        } else {
            col += 1;
        }
    }
    (line, col)
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
    fn zz_records_write_close() {
        let mut ed = EditorCore::new("hello");
        assert_eq!(ed.take_close(), None, "no close before ZZ");
        ed.apply_keys("ZZ");
        assert_eq!(
            ed.take_close(),
            Some(CloseRequest::Write),
            "ZZ is write+quit"
        );
        // take_close consumes it.
        assert_eq!(ed.take_close(), None);
    }

    #[test]
    fn zq_records_discard_close() {
        let mut ed = EditorCore::new("hello");
        ed.apply_keys("ZQ");
        assert_eq!(
            ed.take_close(),
            Some(CloseRequest::Discard),
            "ZQ is force-quit (discard)"
        );
    }

    #[test]
    fn inserted_zz_is_text_not_close() {
        // A `ZZ` typed in insert mode must NOT be read as a quit — modalkit's
        // mode awareness is exactly why this lives in the core, not the app.
        let mut ed = EditorCore::new("");
        ed.apply_keys("iZZ");
        assert_eq!(ed.text(), "ZZ");
        assert_eq!(ed.take_close(), None, "insert-mode ZZ is literal text");
    }

    // ── Command mode (`:` ex-line) ───────────────────────────────────────────
    //
    // The dialect is parsed in `kaijutsu-editor` (pure); the kernel acts on the
    // emitted intents. These prove the parse + the cmdline-buffer capture without
    // a kernel or GPU — the headless gate for Slice 3.

    /// Drain a batch's command intents, asserting one was submitted and parsed.
    fn commands(ed: &mut EditorCore) -> Vec<CommandRequest> {
        ed.take_commands()
            .expect("a command was submitted")
            .expect("the command parsed")
    }

    #[test]
    fn wq_writes_then_quits() {
        let mut ed = EditorCore::new("hello");
        ed.apply_keys(":wq<CR>");
        assert_eq!(
            commands(&mut ed),
            vec![
                CommandRequest::Write { force: false },
                CommandRequest::Quit { force: false }
            ],
        );
        // The buffer is untouched — `:wq` is intent, the kernel does the I/O.
        assert_eq!(ed.text(), "hello");
    }

    #[test]
    fn x_is_write_then_quit() {
        let mut ed = EditorCore::new("hi");
        ed.apply_keys(":x<CR>");
        assert_eq!(
            commands(&mut ed),
            vec![
                CommandRequest::Write { force: false },
                CommandRequest::Quit { force: false }
            ],
        );
    }

    #[test]
    fn bare_write_stays_open() {
        let mut ed = EditorCore::new("hi");
        ed.apply_keys(":w<CR>");
        assert_eq!(commands(&mut ed), vec![CommandRequest::Write { force: false }]);
    }

    #[test]
    fn bang_forces_quit() {
        let mut ed = EditorCore::new("hi");
        ed.apply_keys(":q!<CR>");
        assert_eq!(commands(&mut ed), vec![CommandRequest::Quit { force: true }]);
    }

    #[test]
    fn force_write_quit() {
        let mut ed = EditorCore::new("hi");
        ed.apply_keys(":wq!<CR>");
        assert_eq!(
            commands(&mut ed),
            vec![
                CommandRequest::Write { force: true },
                CommandRequest::Quit { force: true }
            ],
        );
    }

    #[test]
    fn unknown_command_is_a_fail_loud_error() {
        let mut ed = EditorCore::new("hi");
        ed.apply_keys(":frobnicate<CR>");
        let outcome = ed.take_commands().expect("a command was submitted");
        assert!(outcome.is_err(), "an unknown verb must surface an error");
        // ...and it didn't touch the document.
        assert_eq!(ed.text(), "hi");
    }

    #[test]
    fn aborting_the_command_line_runs_nothing() {
        let mut ed = EditorCore::new("hello");
        ed.apply_keys(":wq<Esc>");
        assert_eq!(ed.take_commands(), None, "Esc aborts — no command submitted");
        assert_eq!(ed.text(), "hello");
        // Editing resumes after the bar closes.
        ed.apply_keys("x");
        assert_eq!(ed.text(), "ello");
    }

    #[test]
    fn command_line_text_is_visible_while_typing() {
        // The renderer draws `command_line()`; it tracks the typed line.
        let mut ed = EditorCore::new("hello");
        assert_eq!(ed.command_line(), None, "no bar in normal mode");
        ed.apply_keys(":w");
        assert_eq!(ed.command_line().as_deref(), Some(":w"));
        ed.apply_keys("q");
        assert_eq!(ed.command_line().as_deref(), Some(":wq"));
        // Submitting closes the bar.
        ed.apply_keys("<CR>");
        assert_eq!(ed.command_line(), None, "bar closes on submit");
    }

    #[test]
    fn backspace_edits_the_command_line() {
        // The cmdline is a real EditBuffer, so a typo is fixable.
        let mut ed = EditorCore::new("hello");
        ed.apply_keys(":wx<BS>q<CR>");
        assert_eq!(commands(&mut ed), vec![
            CommandRequest::Write { force: false },
            CommandRequest::Quit { force: false },
        ]);
    }

    #[test]
    fn typed_colon_in_insert_mode_is_text_not_command() {
        // A `:` in insert mode is literal — mode awareness lives in modalkit.
        let mut ed = EditorCore::new("");
        ed.apply_keys("ihttp://x<Esc>");
        assert_eq!(ed.text(), "http://x");
        assert_eq!(ed.take_commands(), None, "no command from an inserted colon");
    }

    // ── Substitute (`:s`) ────────────────────────────────────────────────────
    //
    // `:s` is an *edit*: it mutates the buffer and flows through the normal
    // diff→EditOp path. The dialect is Rust regex + Rust replacement syntax.
    // Extensive on purpose — this is the surface most likely to surprise.

    mod substitute {
        use super::*;

        /// `(initial, keys, expected_text)` — run a `:s` and assert the buffer.
        fn sub(initial: &str, keys: &str, expected: &str) {
            let mut ed = EditorCore::new(initial);
            ed.apply_keys(keys);
            assert_eq!(ed.text(), expected, "keys {keys:?} on {initial:?}");
        }

        #[test]
        fn current_line_first_match() {
            sub("foo foo", ":s/foo/bar/<CR>", "bar foo");
        }

        #[test]
        fn current_line_global() {
            sub("foo foo foo", ":s/foo/bar/g<CR>", "bar bar bar");
        }

        #[test]
        fn whole_buffer_global() {
            sub("a a\nb a", ":%s/a/X/g<CR>", "X X\nb X");
        }

        #[test]
        fn whole_buffer_first_per_line() {
            // No `g`: first match on EACH line in range (vim's per-line model).
            sub("a a\na a", ":%s/a/X/<CR>", "X a\nX a");
        }

        #[test]
        fn rangeless_touches_only_the_cursor_line() {
            let mut ed = EditorCore::new("x x\nx x\nx x");
            ed.apply_keys("j"); // cursor → line 1
            ed.apply_keys(":s/x/Y/g<CR>");
            assert_eq!(ed.text(), "x x\nY Y\nx x", "only the cursor's line changes");
        }

        #[test]
        fn numeric_line_range() {
            sub("x\nx\nx\nx", ":2,3s/x/Y/<CR>", "x\nY\nY\nx");
        }

        #[test]
        fn single_numbered_line() {
            sub("x\nx\nx", ":2s/x/Y/<CR>", "x\nY\nx");
        }

        #[test]
        fn regex_metacharacters() {
            sub("a1b22c", ":s/[0-9]+/#/g<CR>", "a#b#c");
        }

        #[test]
        fn capture_group_replacement() {
            // Rust replacement syntax: `$1`. `${1}` also works; bare `$1` here.
            sub("key=val", ":s/(\\w+)=(\\w+)/$2=$1/<CR>", "val=key");
        }

        #[test]
        fn case_insensitive_flag() {
            sub("HELLO hello", ":s/hello/hi/gi<CR>", "hi hi");
        }

        #[test]
        fn alternate_delimiter() {
            // vim allows any delimiter; useful when the pattern contains `/`.
            sub("a/b/c", ":s#/#_#g<CR>", "a_b_c");
        }

        #[test]
        fn escaped_delimiter_is_literal() {
            sub("a/b end", ":s/a\\/b/X/<CR>", "X end");
        }

        #[test]
        fn no_match_is_a_noop_and_emits_no_op() {
            let mut ed = EditorCore::new("hello");
            let ops = ed.apply_keys(":s/zzz/QQQ/<CR>");
            assert_eq!(ed.text(), "hello", "no match → buffer unchanged");
            assert!(ops.is_empty(), "no match → no EditOp to mirror");
        }

        #[test]
        fn a_real_substitution_emits_an_editop() {
            // The kernel mirrors these ops onto the CRDT block — they must exist.
            let mut ed = EditorCore::new("foo");
            let ops = ed.apply_keys(":s/foo/barbaz/<CR>");
            assert_eq!(ed.text(), "barbaz");
            assert!(!ops.is_empty(), "a substitution must emit at least one EditOp");
        }

        #[test]
        fn invalid_regex_is_a_fail_loud_error() {
            let mut ed = EditorCore::new("hello");
            ed.apply_keys(":s/[/x/<CR>");
            let outcome = ed.take_commands().expect("submit happened");
            assert!(outcome.is_err(), "an unclosed class must error, not edit");
            assert_eq!(ed.text(), "hello", "the buffer is untouched on a bad pattern");
        }

        #[test]
        fn unsupported_flag_is_an_error() {
            let mut ed = EditorCore::new("hello");
            ed.apply_keys(":s/h/H/z<CR>");
            assert!(ed.take_commands().expect("submit").is_err());
            assert_eq!(ed.text(), "hello");
        }

        #[test]
        fn empty_pattern_is_an_error() {
            let mut ed = EditorCore::new("hello");
            ed.apply_keys(":s///<CR>");
            assert!(ed.take_commands().expect("submit").is_err());
        }

        #[test]
        fn bare_s_is_not_a_substitute() {
            // No delimiter → not a substitute; falls through to the verb match,
            // which doesn't know `s` → fail loud (repeat-last-:s is deferred).
            let mut ed = EditorCore::new("hello");
            ed.apply_keys(":s<CR>");
            let outcome = ed.take_commands().expect("submit");
            assert!(outcome.is_err(), "bare :s is an unknown command for now");
            assert_eq!(ed.text(), "hello");
        }

        #[test]
        fn set_is_not_mistaken_for_substitute() {
            // `:set` starts with `s` but `e` is not a delimiter.
            assert!(parse_substitute("set").unwrap().is_none());
        }

        #[test]
        fn cursor_clamps_after_a_shortening_substitution() {
            let mut ed = EditorCore::new("hello world");
            ed.apply_keys("$"); // cursor at end (offset 10)
            ed.apply_keys(":s/world/!/<CR>"); // "hello !" — much shorter
            assert_eq!(ed.text(), "hello !");
            assert!(ed.cursor() <= ed.text().chars().count(), "cursor stays in bounds");
        }

        // ── direct unit tests of the parse helpers ──

        #[test]
        fn split_range_variants() {
            assert_eq!(split_range("s/a/b/"), (SubRange::CurrentLine, "s/a/b/"));
            assert_eq!(split_range("%s/a/b/"), (SubRange::Whole, "s/a/b/"));
            assert_eq!(split_range("3s/a/b/"), (SubRange::Lines(3, 3), "s/a/b/"));
            assert_eq!(split_range("2,5s/a/b/"), (SubRange::Lines(2, 5), "s/a/b/"));
        }

        #[test]
        fn split_substitute_fields() {
            assert_eq!(
                split_substitute("/a/b/g", '/'),
                ("a".into(), "b".into(), "g".into())
            );
            // Omitted trailing sections.
            assert_eq!(
                split_substitute("/a/b", '/'),
                ("a".into(), "b".into(), String::new())
            );
            // Escaped delimiter folds into the pattern; `\w` passes through.
            assert_eq!(
                split_substitute("/a\\/\\w/b/", '/'),
                ("a/\\w".into(), "b".into(), String::new())
            );
        }
    }

    #[test]
    fn close_intent_resets_each_batch() {
        // A normal-mode batch with no quit clears a stale intent.
        let mut ed = EditorCore::new("hello");
        ed.apply_keys("ZQ");
        // Re-open conceptually: a fresh batch of ordinary keys clears the intent.
        ed.apply_keys("x");
        assert_eq!(ed.take_close(), None, "non-quit batch leaves no close intent");
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

    /// Command-line (`:`) and search (`/`·`?`) route through modalkit's prompt
    /// infrastructure we don't wire yet. They must be a safe no-op on the
    /// document, not leak their query text into it. (Fixed: `EditorCore`
    /// suppresses document edits while the command bar is focused.)
    #[test]
    fn command_line_keys_must_not_corrupt_the_buffer() {
        let mut ed = EditorCore::new("hello");
        ed.apply_keys(":d<CR>");
        assert_eq!(ed.text(), "hello", "an unhandled ex-command must not edit the buffer");

        let mut ed = EditorCore::new("hello world");
        ed.apply_keys("/wor<CR>");
        assert_eq!(ed.text(), "hello world", "an unhandled search must not edit the buffer");

        // ...and normal editing must resume once the prompt closes (flag cleared).
        ed.apply_keys("x");
        assert_eq!(ed.text(), "ello world", "editing resumes after the command-line closes");
    }

    #[test]
    fn apply_remote_text_identical_is_a_noop() {
        // The self-write echo case: a session's own mirrored edit comes back
        // through the block flow as the same text. apply_remote_text must report
        // "nothing changed" so the caller skips it (no spurious push, no cursor
        // jump on every keystroke).
        let mut ed = EditorCore::new("hello");
        assert!(!ed.apply_remote_text("hello"), "identical text is a no-op");
        assert_eq!(ed.text(), "hello");
    }

    #[test]
    fn apply_remote_text_insert_before_cursor_shifts_it() {
        // Cursor sits on 'l' (offset 3) of "hello"; a peer inserts "AB" at the
        // start. The merged text is "ABhello" and the cursor tracks its char,
        // landing at offset 5.
        let mut ed = EditorCore::new("hello");
        ed.apply_keys("lll"); // move to offset 3
        assert_eq!(ed.cursor(), 3);
        assert!(ed.apply_remote_text("ABhello"));
        assert_eq!(ed.text(), "ABhello");
        assert_eq!(ed.cursor(), 5, "cursor shifted by the inserted length");
    }

    #[test]
    fn apply_remote_text_insert_after_cursor_leaves_it() {
        // Cursor at offset 1; a peer appends at the end. The cursor is unmoved.
        let mut ed = EditorCore::new("hello");
        ed.apply_keys("l"); // offset 1
        assert_eq!(ed.cursor(), 1);
        assert!(ed.apply_remote_text("hello world"));
        assert_eq!(ed.text(), "hello world");
        assert_eq!(ed.cursor(), 1, "an edit after the cursor doesn't move it");
    }

    #[test]
    fn apply_remote_text_merges_multiline_content() {
        // A peer's edit can introduce newlines; the cursor maps to the right
        // (line, col). Insert "x\n" before the cursor at offset 0.
        let mut ed = EditorCore::new("a");
        assert!(ed.apply_remote_text("x\na"));
        assert_eq!(ed.text(), "x\na");
    }

    #[test]
    fn diff_op_handles_utf8_chars() {
        // café → cafés: insert one char after the multibyte é. Char-indexed
        // offset must be 4, not a byte offset.
        let op = diff_op("café", "cafés").unwrap();
        assert_eq!(op, EditOp { offset: 4, insert: "s".into(), delete: 0 });
    }
}
