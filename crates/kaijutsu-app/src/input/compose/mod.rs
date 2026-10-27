//! Compose: the overlay's chat and shell surfaces, each a local
//! [`kaijutsu_editor::EditorCore`] (`docs/vi.md`, `docs/tui.md` "Compose").
//!
//! The chat surface is the context's kernel-owned draft. Every edit the vi
//! engine makes goes to the kernel as one `edit_input`, queued on the
//! connection's draft outbox (`outbox.rs`, `kaijutsu_client::draft_outbox`),
//! and the change feed's copy of the draft is reconciled back through a
//! [`DraftLedger`]. The shell surface is local text until it is submitted.
//!
//! This module is the model: no Bevy systems, no RPC. `systems.rs` drives it.

pub mod outbox;
pub mod systems;

use kaijutsu_client::draft_ledger::DraftLedger;
use kaijutsu_editor::{EditOp, EditorCore};
use kaijutsu_types::{BlockId, BlockSnapshot, ContextId, PrincipalId, Status};
use modalkit::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// What one keystroke asked the rest of the app to do.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ComposeAction {
    /// Edits the vi engine made to the buffer, in order.
    pub ops: Vec<EditOp>,
    /// `Enter` outside the `/`·`?`·`:` bar: submit the surface.
    pub submit: bool,
    /// A `:` line submitted from the bar, prefix included.
    pub command: Option<String>,
    /// Something the vi engine reported, such as `Pattern not found: x`.
    pub notice: Option<String>,
}

/// When `Enter` submits a surface, as the tui's two lines do it
/// (`docs/tui.md`, "Compose").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Submit {
    /// The chat draft: `Enter` submits from normal mode; anywhere else it is
    /// the vi engine's own `Enter`, a newline in insert mode.
    FromNormalMode,
    /// The shell line, as the tui's `:` bar: `Enter` runs it in any mode.
    FromAnyMode,
}

/// One compose surface's text, cursor and mode.
pub struct ComposeBuffer {
    editor: EditorCore,
}

impl Default for ComposeBuffer {
    fn default() -> Self {
        Self::over("")
    }
}

impl ComposeBuffer {
    /// A buffer holding `text`, in normal mode with the cursor on its last
    /// character, so `a` continues where the text left off.
    pub fn over(text: &str) -> Self {
        let mut editor = EditorCore::new(text);
        editor.apply_keys("G$");
        Self { editor }
    }

    pub fn text(&self) -> String {
        self.editor.text()
    }

    /// The cursor's char offset into [`Self::text`].
    pub fn cursor(&mut self) -> usize {
        self.editor.cursor()
    }

    /// The vi mode banner, `None` in normal mode.
    pub fn mode(&self) -> Option<String> {
        self.editor.mode()
    }

    /// The `/`·`?`·`:` bar's text with its prefix, while it is open.
    pub fn command_line(&self) -> Option<String> {
        self.editor.command_line()
    }

    /// The visual-mode selection as char spans, `None` outside visual mode.
    pub fn selection(&mut self) -> Option<kaijutsu_editor::EditorSelection> {
        self.editor.selection()
    }

    /// Whether the buffer is in insert mode, where keys are text.
    pub fn inserting(&self) -> bool {
        self.mode().is_some_and(|mode| mode.contains("INSERT"))
    }

    /// Whether the buffer rests in normal mode with the bar closed.
    pub fn in_normal_mode(&self) -> bool {
        self.mode().is_none() && self.command_line().is_none()
    }

    /// One keystroke. `Enter` submits as `submit` says, except while the
    /// `/`·`?`·`:` bar is open, where it ends the bar.
    pub fn press(&mut self, submit: Submit, key: KeyEvent) -> ComposeAction {
        let mut action = ComposeAction::default();
        if let Some(raw) = self.command_line() {
            action.ops = self.editor.apply_key_event(key);
            let reported = self.editor.take_commands();
            if key.code == KeyCode::Enter && raw.starts_with(':') {
                // The core's `:w`/`:q` dialect answers the editor screen, not
                // this surface; the raw line goes back to the caller.
                action.command = Some(raw);
            } else if let Some(Err(message)) = reported {
                action.notice = Some(message);
            }
            return action;
        }
        let submits = match submit {
            Submit::FromNormalMode => self.mode().is_none(),
            Submit::FromAnyMode => true,
        };
        if key.code == KeyCode::Enter && submits {
            action.submit = true;
            return action;
        }
        action.ops = self.editor.apply_key_event(key);
        if let Some(Err(message)) = self.editor.take_commands() {
            action.notice = Some(message);
        }
        action
    }

    /// Insert `text` at the cursor as one edit, in whatever mode the buffer
    /// is in. Text, not keystrokes: normal mode does not read it as commands.
    pub fn paste(&mut self, text: &str) -> Vec<EditOp> {
        self.editor.insert_at_cursor(text)
    }

    /// The mode a surface opens in: insert when it is empty, so typing starts
    /// at once; normal otherwise, so the text can be read before it is
    /// changed. Returns the edits leaving a mode made (normally none).
    pub fn settle_for_focus(&mut self) -> Vec<EditOp> {
        let mut ops = self.leave_to_normal();
        if self.text().is_empty() {
            ops.extend(self.editor.apply_key_event(key(KeyCode::Char('i'))));
        }
        ops
    }

    /// A buffer holding `text` with the cursor after it, in insert mode:
    /// the prefilled-`kj` prompt (`docs/input.md`).
    pub fn prefilled(text: &str) -> Self {
        let mut buffer = Self::over(text);
        let start = if text.is_empty() { 'i' } else { 'a' };
        buffer.editor.apply_key_event(key(KeyCode::Char(start)));
        buffer
    }

    /// Close the bar and leave any mode for normal mode.
    pub fn leave_to_normal(&mut self) -> Vec<EditOp> {
        let mut ops = Vec::new();
        // Visual, operator-pending, insert and the bar each close with one
        // Esc; the bound keeps a mode that never closes from hanging.
        for _ in 0..3 {
            if self.in_normal_mode() {
                break;
            }
            ops.extend(self.editor.apply_key_event(key(KeyCode::Esc)));
            let _ = self.editor.take_commands();
        }
        ops
    }

    /// Take the kernel's copy of the text, keeping the cursor on the same
    /// text. Returns whether the buffer changed.
    pub fn apply_remote_text(&mut self, text: &str) -> bool {
        self.editor.apply_remote_text(text)
    }
}

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

/// Chat input that arrived while the chat draft was loading, replayed into
/// the chat draft once it has, whatever surface is active by then.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HeldInput {
    Key(KeyEvent),
    Paste(String),
}

/// A draft load just started.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LoadStarted {
    /// The read's token, for [`ComposeState::finish_load`].
    pub read: u64,
    /// Inputs held for an earlier load that never landed, dropped: they
    /// were typed for a draft that is no longer the one loading.
    pub dropped: usize,
}

/// Which kernel draft the chat buffer holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DraftTarget {
    /// No context, or no connection to read one through.
    #[default]
    Detached,
    /// The draft of `context` is being read; input waits for it.
    Loading {
        context: ContextId,
        principal: PrincipalId,
        read: u64,
    },
    /// The buffer is `context`'s draft.
    Ready {
        context: ContextId,
        principal: PrincipalId,
    },
}

impl DraftTarget {
    /// The context and principal this draft belongs to, loading or loaded.
    pub fn owner(&self) -> Option<(ContextId, PrincipalId)> {
        match *self {
            DraftTarget::Detached => None,
            DraftTarget::Loading { context, principal, .. } | DraftTarget::Ready { context, principal } => {
                Some((context, principal))
            }
        }
    }
}

/// Both compose surfaces, and the chat draft's bookkeeping.
///
/// `EditorCore` holds modalkit's machine, which is not `Send` because of a
/// boxed dialog it never builds here; see the `unsafe impl` below.
#[derive(bevy::prelude::Resource)]
pub struct ComposeState {
    pub chat: ComposeBuffer,
    pub shell: ComposeBuffer,
    /// Our edits to the chat draft and their acks.
    ledger: DraftLedger,
    draft: DraftTarget,
    /// Input that arrived while the draft was loading.
    held: Vec<HeldInput>,
    /// Draft reads started, to tell the newest from a stale one.
    reads: u64,
    /// The block the last submit became: a feed that still shows it as the
    /// draft is stale, and it is never read back.
    submitted: Option<BlockId>,
    /// The mirror version last offered to the ledger.
    offered: Option<u64>,
    /// Submits sent and not yet answered. Until one is, the feed may still
    /// show the submitted text as the draft, so no mirror is taken.
    submits: usize,
}

// SAFETY: `EditorCore` holds a modalkit `ModalMachine`, whose only
// non-Send/Sync member is an internal `Box<dyn Dialog>` for terminal
// command-line dialogs. Compose never builds one, and no accessor on
// `EditorCore` exposes it, so nothing here can hand it to a second thread.
// Bevy's resource borrow rules give exclusive access for every mutation.
// `ActiveDiffView` relies on the same argument.
unsafe impl Send for ComposeState {}
unsafe impl Sync for ComposeState {}

impl Default for ComposeState {
    fn default() -> Self {
        Self {
            chat: ComposeBuffer::default(),
            shell: ComposeBuffer::default(),
            ledger: DraftLedger::default(),
            draft: DraftTarget::Detached,
            held: Vec::new(),
            reads: 0,
            submitted: None,
            offered: None,
            submits: 0,
        }
    }
}

impl ComposeState {
    pub fn draft(&self) -> DraftTarget {
        self.draft
    }

    /// The context the chat buffer is the loaded draft of.
    pub fn ready_context(&self) -> Option<ContextId> {
        match self.draft {
            DraftTarget::Ready { context, .. } => Some(context),
            _ => None,
        }
    }

    /// Edits sent for the chat draft and not yet landed.
    #[cfg(test)]
    pub fn edits_in_flight(&self) -> usize {
        self.ledger.in_flight()
    }

    /// One keystroke on the chat surface. `None` when the draft is loading:
    /// the key is held and replayed by [`Self::finish_load`].
    pub fn press_chat(&mut self, key: KeyEvent) -> Option<ComposeAction> {
        if matches!(self.draft, DraftTarget::Loading { .. }) {
            self.held.push(HeldInput::Key(key));
            return None;
        }
        Some(self.chat.press(Submit::FromNormalMode, key))
    }

    /// One keystroke on the shell surface.
    pub fn press_shell(&mut self, key: KeyEvent) -> ComposeAction {
        self.shell.press(Submit::FromAnyMode, key)
    }

    /// A paste into the chat draft. `None` while the draft is loading: the
    /// paste is held and replayed with the keys.
    pub fn paste_chat(&mut self, text: &str) -> Option<Vec<EditOp>> {
        if matches!(self.draft, DraftTarget::Loading { .. }) {
            self.held.push(HeldInput::Paste(text.to_string()));
            return None;
        }
        Some(self.chat.paste(text))
    }

    /// Count one chat edit as sent. Returns the generation to send with it.
    pub fn sent(&mut self) -> u64 {
        self.ledger.sent()
    }

    /// One of our chat edits came back: `Some(version)` when acknowledged,
    /// `None` when it failed. Returns whether the buffer changed.
    pub fn landed(&mut self, generation: u64, version: Option<u64>) -> bool {
        match self.ledger.landed(generation, version) {
            Some(text) => self.chat.apply_remote_text(&text),
            None => false,
        }
    }

    /// Start loading `context`'s draft for `principal`: the buffer empties,
    /// earlier acks stop counting, and input waits for
    /// [`Self::finish_load`]. Input still held for an earlier load is
    /// dropped and counted, never applied to this draft.
    pub fn begin_load(&mut self, context: ContextId, principal: PrincipalId) -> LoadStarted {
        let dropped = std::mem::take(&mut self.held).len();
        self.reads += 1;
        self.draft = DraftTarget::Loading { context, principal, read: self.reads };
        // The empty box shown while loading keeps the player's mode: keys
        // typed into an insert-mode box are text.
        self.chat = if self.chat.inserting() { ComposeBuffer::prefilled("") } else { ComposeBuffer::default() };
        self.ledger.restart();
        self.offered = None;
        LoadStarted { read: self.reads, dropped }
    }

    /// The chat buffer has no draft to follow (no context, or no
    /// connection). Held input is dropped with the draft it was for, and
    /// returned so the caller can say so.
    pub fn detach(&mut self) -> Vec<HeldInput> {
        self.draft = DraftTarget::Detached;
        self.chat = ComposeBuffer::default();
        self.ledger.restart();
        self.offered = None;
        std::mem::take(&mut self.held)
    }

    /// A draft read came back. A read that is not the newest is ignored
    /// (`None`). Otherwise the buffer takes the text (an unread draft leaves
    /// it empty) and the held input comes back to be replayed in order.
    pub fn finish_load(&mut self, context: ContextId, read: u64, text: Option<&str>) -> Option<Vec<HeldInput>> {
        let DraftTarget::Loading { context: loading, principal, read: newest } = self.draft else {
            return None;
        };
        if loading != context || read != newest {
            return None;
        }
        // The loaded draft opens in the mode the waiting box showed: insert
        // at its end when the player was typing, so held keys land as text
        // and never run as normal-mode commands against the loaded text.
        let text = text.unwrap_or("");
        self.chat = if self.chat.inserting() { ComposeBuffer::prefilled(text) } else { ComposeBuffer::over(text) };
        self.draft = DraftTarget::Ready { context, principal };
        Some(std::mem::take(&mut self.held))
    }

    /// The chat draft was sent as a submit: the buffer starts over on an
    /// empty draft, as the kernel's will, and no mirror is taken until the
    /// submit is answered ([`Self::finish_submit`]).
    pub fn begin_submit(&mut self) {
        self.chat = ComposeBuffer::default();
        self.ledger.restart();
        self.offered = None;
        self.submits += 1;
    }

    /// A submit was answered: `Some(block)` is the block the draft became,
    /// never read back as the draft; `None` is a failure, after which the
    /// caller reloads the kernel's draft.
    pub fn finish_submit(&mut self, block: Option<BlockId>) {
        self.submits = self.submits.checked_sub(1).expect("a submit was answered that was never sent");
        if block.is_some() {
            self.submitted = block;
        }
        self.offered = None;
    }

    /// Empty the chat buffer, keeping its draft: returns the one edit that
    /// deletes the kernel's copy, for the caller to send.
    pub fn clear_chat(&mut self) -> Option<EditOp> {
        let len = self.chat.text().chars().count();
        self.chat = ComposeBuffer::default();
        (len > 0).then(|| EditOp { offset: 0, insert: String::new(), delete: len })
    }

    /// Offer the change feed's copy of the draft at mirror `version`.
    /// Returns whether the buffer changed. A version already offered is
    /// skipped, so this can run every frame.
    pub fn offer_mirror(&mut self, text: &str, version: u64) -> bool {
        if self.submits > 0 || self.offered == Some(version) {
            return false;
        }
        self.offered = Some(version);
        self.ledger.offer(text, version) && self.chat.apply_remote_text(text)
    }

    /// The draft block in `blocks` that belongs to `principal`, skipping the
    /// block the last submit became.
    pub fn draft_in<'a>(&self, blocks: &'a [BlockSnapshot], principal: PrincipalId) -> Option<&'a str> {
        blocks
            .iter()
            .find(|b| b.status == Status::Draft && b.id.principal_id == principal && Some(b.id) != self.submitted)
            .map(|b| b.content.as_str())
    }
}

/// A shell line whose first word is `kj`, as its argv after `kj`, split on
/// whitespace with no shell quoting: the tui's `:kj` rule
/// (`kaijutsu-tui`'s `cmdline::parse`). Such a line runs through
/// `execute_kj`, whose result data names where to go next; any other line
/// runs in the shell.
pub fn shell_kj_argv(line: &str) -> Option<Vec<String>> {
    let mut words = line.split_whitespace();
    (words.next()? == "kj").then(|| words.map(str::to_string).collect())
}

/// The shell statement a `:vi <args>` line on the `:` bar runs: `vi <args>`,
/// trimmed, the same as `:!vi <args>` in the tui (`kaijutsu-tui`'s
/// `cmdline::parse`). `None` for any other line, `:vim` included.
pub fn colon_vi_statement(line: &str) -> Option<String> {
    let body = line.strip_prefix(':')?;
    if body == "vi" {
        return Some("vi".to_string());
    }
    body.strip_prefix("vi ").map(|args| format!("vi {}", args.trim()))
}

/// A char offset into `text` as a byte offset, for the parley layout.
pub fn byte_offset(text: &str, chars: usize) -> usize {
    text.char_indices().nth(chars).map_or(text.len(), |(byte, _)| byte)
}

#[cfg(test)]
mod tests;
