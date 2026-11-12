//! In-app editor: the kernel-owned editing surface (the `vi` builtin +
//! `vi` default).
//!
//! Two parts:
//! - [`resolve_editor_target`] maps a VFS path to the `(context, block)`
//!   that *owns* its text, through
//!   [`FileDocumentCache::try_get_or_load`](crate::file_tools::FileDocumentCache),
//!   which mints/loads a working-copy file-doc. Every `/config` tree mounts
//!   as an ordinary host directory (`docs/config-namespace.md`), same as any
//!   other path.
//! - [`EditorSessions`] is the registry of open editors. Each session is a pure
//!   [`EditorCore`](kaijutsu_editor::EditorCore) bound to a target; keystrokes
//!   mirror onto the kernel block, and a checkpoint backs `ZQ` rollback. This is
//!   the tool-shaped surface the app renders, a model plays, and tests drive —
//!   all headless. See `docs/vi.md`.

use kaijutsu_types::{BlockId, ContextId};
use kaijutsu_types::{PrincipalId, SessionId};

use crate::block_store::SharedBlockStore;
use crate::file_tools::FileDocumentCache;

/// The well-known nick the Bevy app registers under (see `peers/mod.rs`). The
/// `open_editor` signal targets it. Pass 1 addresses this single app peer; the
/// submitting-peer addressing refinement (multi-user) is tracked in `docs/vi.md`
/// risk #1.
pub const APP_PEER_NICK: &str = "kaijutsu-app";

/// Status-line message for a write batch (`ZZ`, `:w`, `:wq`, `:x`) refused
/// because the caller's seat lacks `Capability::Editor`. Vim's read-only-
/// buffer shape (E45): the buffer stays open and dirty, nothing is
/// checkpointed or flushed. Used by [`EditorSessions::refuse_write`], the
/// one write surface that can discover a write intent only mid-batch — `kj
/// editor save` (`Kernel::editor_save`) is itself always a write intent, so
/// it denies once at the dispatcher instead of needing this message. See
/// `docs/vi.md`.
pub const WRITE_CAPABILITY_REFUSED: &str =
    "write refused — this seat lacks the 'editor' capability";

/// Status-line message for [`EditorSessions::insert_at_cursor`] (a paste)
/// while the `:` command line is open. The `:`-line's own keystrokes mean
/// something different than inserting into the document, and this pure
/// registry does not guess between them — see `docs/vi.md`.
pub const PASTE_REFUSED_COMMAND_LINE: &str =
    "paste refused — close the ':' command line first";

/// The location an editor binds to: the context + block that own a path's
/// text. Edits go to `block_store.edit_text_as(context_id, block_id, …)` with
/// the player who submitted that input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EditorTarget {
    pub context_id: ContextId,
    pub block_id: BlockId,
}

/// Resolve `path` to the `(context, block)` of the kernel document that owns its
/// text, through the file-doc cache. Fails loud (no silent empty/placeholder)
/// when the path names nothing — an editor must not open on a phantom block.
pub async fn resolve_editor_target(
    path: &str,
    file_cache: &FileDocumentCache,
) -> Result<EditorTarget, String> {
    let (context_id, block_id) = file_cache
        .try_get_or_load(path)
        .await
        .map_err(|e| format!("open editor: cannot open '{path}': {e}"))?;
    Ok(EditorTarget {
        context_id,
        block_id,
    })
}

// ============================================================================
// Editor sessions — the kernel-owned editing surface
// ============================================================================

use std::collections::HashMap;

use kaijutsu_editor::{
    ArgMove, CloseRequest, CommandRequest, EditorCore, EditorIo, PendingKeys, Registers,
};
pub use kaijutsu_types::editor::{EditorSelection, SelectionShape};

use crate::kernel_db::{RecoveryFile, RecoveryId};

/// Status-line message for leaving, or quitting, a buffer that differs from
/// its checkpoint without `!` (vim's E37).
pub const NO_WRITE_SINCE_LAST_CHANGE: &str = "No write since last change (add ! to override)";

/// Status-line message for `:q`, `:wq`, `:x` or `ZZ` while another file of
/// the argument list is dirty (vim's E162), naming the first such file.
fn hidden_buffer_dirty(path: &str) -> String {
    format!("No write since last change for buffer \"{path}\"")
}

/// A one-line census entry for an open session — what `kj vi list` shows.
/// Full ids (never truncated), matching the `.data` convention every other
/// `kj` list surface uses (`kj block list`, `kj doc list`, …) so a driver can
/// round-trip these back into other kj commands without re-resolving.
#[derive(Debug, Clone, serde::Serialize)]
pub struct EditorSessionInfo {
    pub session: u64,
    /// The current file of the session's argument list.
    pub path: String,
    /// Full id string of the current file's document (`ContextId::to_hex`).
    pub context_id: String,
    /// Full id string of the current file's block (`BlockId::to_key`).
    pub block_id: String,
    /// The current file's 0-based position in the argument list.
    pub arg_index: usize,
    /// How many files the argument list holds.
    pub arg_count: usize,
    /// Whether any file of the session differs from its checkpoint.
    pub dirty: bool,
    /// The files that differ from their checkpoints, current or hidden, in
    /// list order.
    pub dirty_files: Vec<String>,
    pub mode: Option<String>,
    /// Full id string of the opener's principal, `None` for a headless open.
    pub opener: Option<String>,
}

/// The result of feeding a key batch to a session via [`EditorSessions::keys`].
/// Each variant carries a [`KeysUpdate`] — the renderer state plus the save
/// signal the **kernel** layer (which holds the file-document cache;
/// `EditorSessions` deliberately does not) needs to decide whether to flush a
/// file-backed session to disk. See `docs/file-buffers.md`.
#[derive(Debug)]
pub enum KeysOutcome {
    /// The buffer updated; the new renderer state to push.
    Updated(KeysUpdate),
    /// A `ZZ`/`ZQ` closed the session (already saved/discarded + dropped). The
    /// state is the last view before close; renderers react to the `Closed` push.
    Closed(KeysUpdate),
}

/// A key batch's resulting state, plus whether this batch **requested** a
/// write (`:w`, `:wq`/`:x`, `ZZ`) rather than merely settling back to a clean
/// buffer on its own (e.g. an undo) — only the former means "flush to disk."
///
/// `saved: true` does NOT always mean the checkpoint has already advanced.
/// For a `Closed` outcome it has (there is no separate flush step to gate
/// on). For an `Updated` outcome — a plain `:w` that leaves the session
/// open — it has **not**: `state.dirty` is still `true`, and the
/// kernel layer must flush `path` to disk first and only then checkpoint
/// buffer `buffer`, or the buffer would read clean before the bytes land
/// (docs/file-buffers.md).
///
/// `forced` carries `:w!`'s bang through to the kernel layer's W12
/// changed-under-us guard (`FileDocumentCache::flush_one_guarded`,
/// docs/file-buffers.md): a plain `:w` (`forced: false`) refuses when disk
/// moved under the buffer since it was loaded; `:w!` (`forced: true`)
/// overrides that refusal. This pure registry has no file cache to check
/// against, so it only carries the bit — the kernel layer is what acts on it.
///
/// `quit` reports what a close did to every loaded file (see
/// [`QuitReport`]); it is empty for every `Updated` outcome.
///
/// `go` is a move through the argument list the kernel layer performs after
/// the batch's write has landed ([`EditorSessions::begin_move`]), and `rest`
/// the keys the batch left unfed after the intent that ended it; the kernel
/// layer feeds them to the current buffer once it has acted.
#[derive(Debug)]
pub struct KeysUpdate {
    pub state: EditorState,
    /// The file the batch ran in, and its index in the argument list.
    pub path: String,
    pub buffer: usize,
    pub saved: bool,
    pub forced: bool,
    pub quit: QuitReport,
    pub go: Option<(ArgMove, bool)>,
    pub rest: Option<PendingKeys>,
}

/// What closing a session did to its loaded files.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct QuitReport {
    /// Every loaded file's path. The kernel layer pinned each at load and
    /// releases the pins on close.
    pub loaded: Vec<String>,
    /// The files whose rollback rewrote their block. The kernel layer marks
    /// each dirty in the file cache: the rollback is a raw block edit the
    /// cache's flush pipeline never sees (docs/file-buffers.md).
    pub rolled_back: Vec<String>,
    /// The session's recovery record, which the kernel layer deletes now that
    /// the session is closed. `None` for a headless session.
    pub recovery: Option<RecoveryId>,
    /// Why the kernel layer could not delete that record, when it could not.
    /// The record then stays listed for `vi -r`.
    pub recovery_error: Option<String>,
}

impl KeysOutcome {
    /// The renderer state this outcome carries — the post-edit view, or the
    /// last view before a `ZZ`/`ZQ` close.
    pub fn state(&self) -> &EditorState {
        match self {
            Self::Updated(u) | Self::Closed(u) => &u.state,
        }
    }

    /// Whether this batch requested a write the kernel layer should flush to
    /// disk for a file-backed session (see [`KeysUpdate`] for whether the
    /// checkpoint has already advanced).
    pub fn saved(&self) -> bool {
        match self {
            Self::Updated(u) | Self::Closed(u) => u.saved,
        }
    }
}

/// Handle to one open editor session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct EditorSessionId(u64);

impl EditorSessionId {
    /// The raw handle value — the currency the `kj vi` / wire surface uses.
    pub fn as_u64(self) -> u64 {
        self.0
    }

    /// Reconstruct a handle from a wire value.
    pub fn from_u64(n: u64) -> Self {
        EditorSessionId(n)
    }
}

/// A renderer-facing snapshot of a session: what to draw, plus dirtiness.
///
/// `Serialize`/`Deserialize` so it can ride the in-process [`EditorFlow`](crate::flows::EditorFlow)
/// bus (the push channel the `subscribe_editor` bridge serializes to the wire).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct EditorState {
    pub text: String,
    pub cursor: usize,
    pub mode: Option<String>,
    /// Whether the buffer differs from the last open/save checkpoint.
    pub dirty: bool,
    /// The `:`-line the renderer should draw while command mode is active —
    /// `Some(":wq")` mid-type, `None` when the bar is unfocused. The kernel owns
    /// the bar (modalkit); the renderer draws this read-only, tracking no mode.
    pub command_line: Option<String>,
    /// A transient status/error line (vim's `E492`-area message), e.g. an unknown
    /// `:command` or a bad `:s` regex. `Some` right after the offending submit,
    /// cleared on the next keystroke batch. The session stays open — a bad
    /// `:`-line reports here instead of erroring the whole `editor_keys` call.
    pub message: Option<String>,
    /// The visual-mode selection as char spans to highlight; `None` outside
    /// visual mode. Rebuilt from `EditorCore` on every state.
    pub selection: Option<EditorSelection>,
    /// The current file's path, as the opener named it.
    pub path: String,
    /// The current file's 0-based position in the session's argument list.
    pub arg_index: usize,
    /// How many files the argument list holds.
    pub arg_count: usize,
}

impl EditorState {
    /// Structured `.data` for one session, stamped with its handle — the single
    /// shape every editor front door emits (`kj vi`, the `vi`
    /// builtin). Object form (inspect-style) so a driver reads one
    /// record. Keeping it here means the shape can't drift between front doors.
    pub fn to_json(&self, session: EditorSessionId) -> serde_json::Value {
        serde_json::json!({
            "session": session.as_u64(),
            "text": self.text,
            "cursor": self.cursor,
            "mode": self.mode,
            "dirty": self.dirty,
            "command_line": self.command_line,
            "message": self.message,
            "selection": self.selection,
            "path": self.path,
            "arg_index": self.arg_index,
            "arg_count": self.arg_count,
        })
    }
}

/// Who opened a session, and the shell context they opened from — captured at
/// the front door (`vi`, `kj vi`) and recorded on the
/// session. Three consumers:
///
/// - **`fg`** re-foregrounds the caller's most-recent session by `principal`.
/// - **`vi`** joins the `principal`'s session that already holds a named file.
/// - **`:r !cmd`** preserves requester, performer, reviewer, session, and the
///   context at open. Later shell navigation does not retarget the read.
///
/// `None` for a headless open (a test driver, the wire `editorOpen` handler):
/// nobody to foreground or join, and `:r !cmd` then fails loud rather than
/// guessing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EditorOpener {
    pub principal: PrincipalId,
    pub performer: PrincipalId,
    pub reviewer: Option<PrincipalId>,
    pub context_id: ContextId,
    pub session_id: SessionId,
}

/// What a session's recovery record holds at one moment
/// ([`EditorSessions::recovery_snapshot`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecoverySnapshot {
    pub recovery_id: RecoveryId,
    pub opener_principal: PrincipalId,
    pub opener_context_id: ContextId,
    pub current_index: usize,
    pub files: Vec<RecoveryFile>,
}

/// The 0-based line and column of char offset `at` in `text`.
fn line_col_of(text: &str, at: usize) -> (usize, usize) {
    let mut line = 0;
    let mut col = 0;
    for ch in text.chars().take(at) {
        if ch == '\n' {
            line += 1;
            col = 0;
        } else {
            col += 1;
        }
    }
    (line, col)
}

/// One loaded file: a pure [`EditorCore`] bound to the kernel block that owns
/// the text, plus the rollback checkpoint.
struct Buffer {
    core: EditorCore,
    target: EditorTarget,
    /// Normalized (terminator-stripped) content as of the last open/save — the
    /// dirty/`ZQ` checkpoint. Matches `EditorCore`'s normalized view so dirty
    /// compares like-to-like (a newline-terminated block opens clean).
    saved_content: String,
    /// The block's trailing terminator (`"\n"` or `""`) captured at load.
    /// `EditorCore` strips modalkit's line terminator, so the terminator lives
    /// here; edits mirror as diffs (never touching it) and `ZQ` re-applies it.
    terminator: String,
    /// Whether a **peer's** write merged into this buffer since the last
    /// load/save checkpoint — set by [`EditorSessions::reconcile_block`] (a
    /// non-self merge), reset when the checkpoint advances ([`EditorSessions::save`]).
    /// A `ZQ` rollback to the checkpoint would revert that peer work along with
    /// ours, so [`EditorSessions::quit`] skips the rollback when this is set:
    /// detach, don't retract (see the entanglement guard in `docs/vi.md`).
    peer_wrote: bool,
}

impl Buffer {
    /// Load the block's current text as a fresh buffer and checkpoint.
    fn load(target: EditorTarget, blocks: &SharedBlockStore) -> Result<Self, String> {
        Self::load_over(target, blocks, None, None)
    }

    /// [`load`](Self::load), for a buffer a recovery reopens. `disk_text`
    /// replaces the checkpoint, so a swap's text loads as unsaved changes
    /// against the file on disk; `cursor` is the recorded line and column.
    fn load_over(
        target: EditorTarget,
        blocks: &SharedBlockStore,
        disk_text: Option<&str>,
        cursor: Option<(usize, usize)>,
    ) -> Result<Self, String> {
        let raw = block_text(blocks, &target)?;
        let mut core = EditorCore::new(&raw);
        let terminator = if raw.ends_with('\n') { "\n" } else { "" }.to_string();
        let saved_content = match disk_text {
            Some(disk) => EditorCore::new(disk).text(),
            None => core.text(),
        };
        if let Some((line, col)) = cursor {
            core.set_cursor(line, col);
        }
        Ok(Buffer { core, target, saved_content, terminator, peer_wrote: false })
    }

    fn dirty(&self) -> bool {
        self.core.text() != self.saved_content
    }
}

/// One file of a session's argument list.
struct ArgFile {
    /// The path as the opener named it (e.g. a `coder/*` symlink, before
    /// resolution). The kernel layer flushes and pins the file cache by it.
    path: String,
    /// `None` until the file is first visited; a visited file keeps its
    /// buffer, so its undo history, marks and cursor are there on return.
    buffer: Option<Buffer>,
    /// A recovered file's recorded cursor (line, column), applied when the
    /// file loads.
    restore_cursor: Option<(usize, usize)>,
    /// A file of a recovered session that has not loaded yet: its load takes
    /// the disk text as checkpoint when the file has a swap, and
    /// acknowledges that swap.
    recovering: bool,
}

impl ArgFile {
    fn unloaded(path: &str) -> Self {
        ArgFile { path: path.to_string(), buffer: None, restore_cursor: None, recovering: false }
    }
}

/// One open editor: an argument list of files and the current one. The
/// current file is always loaded. The id never changes as the current file
/// moves, so a renderer keeps its screen and subscription across `:n`.
struct EditorSession {
    files: Vec<ArgFile>,
    current: usize,
    /// Who opened the session + the context they opened from ([`EditorOpener`]).
    /// `fg` finds the caller's suspended editor by `principal`; `:r !cmd` runs in
    /// the opener's `(principal, context_id, session_id)`. Later edits use the
    /// player who submitted each input, not this opener. `None` for a headless
    /// open (test / wire `editorOpen` — no caller to capture).
    opener: Option<EditorOpener>,
    /// The id of this session's recovery record. `Some` exactly when the
    /// session has an opener: a headless session has nobody to recover it.
    recovery: Option<RecoveryId>,
}

impl EditorSession {
    fn path(&self) -> &str {
        &self.files[self.current].path
    }

    /// The buffer at `index`. Every visited file is loaded; an unloaded one
    /// here is a registry defect, reported rather than guessed past.
    fn buffer_at(&mut self, index: usize) -> Result<&mut Buffer, String> {
        self.files
            .get_mut(index)
            .and_then(|f| f.buffer.as_mut())
            .ok_or_else(|| format!("editor: file {index} of the argument list is not loaded"))
    }

    fn buffer(&mut self) -> Result<&mut Buffer, String> {
        self.buffer_at(self.current)
    }

    fn state(&mut self) -> Result<EditorState, String> {
        let (path, arg_index, arg_count) =
            (self.path().to_string(), self.current, self.files.len());
        let buffer = self.buffer()?;
        let checkpoint = buffer.saved_content.clone();
        Ok(state_of(&mut buffer.core, &checkpoint, path, arg_index, arg_count))
    }

    /// The first file of the list, other than the current one, whose buffer
    /// differs from its checkpoint.
    fn first_other_dirty(&self) -> Option<&str> {
        self.files
            .iter()
            .enumerate()
            .filter(|(i, _)| *i != self.current)
            .find(|(_, f)| f.buffer.as_ref().is_some_and(Buffer::dirty))
            .map(|(_, f)| f.path.as_str())
    }

    fn position(&self, path: &str) -> Option<usize> {
        self.files.iter().position(|f| f.path == path)
    }

    /// `:args` — the list with the current file in brackets, as vim prints it.
    fn args_line(&self) -> String {
        self.files
            .iter()
            .enumerate()
            .map(|(i, f)| {
                if i == self.current { format!("[{}]", f.path) } else { f.path.clone() }
            })
            .collect::<Vec<_>>()
            .join(" ")
    }
}

/// The input of one key batch: typed keys, or the keys a previous batch left
/// unfed.
pub enum BatchInput<'a> {
    Keys(&'a str),
    Pending(PendingKeys),
}

/// The first step of a move through the argument list
/// ([`EditorSessions::begin_move`]).
#[derive(Debug, PartialEq, Eq)]
pub enum MoveStep {
    /// The move did not happen; the message goes on the status line.
    Refused(String),
    /// The move landed on a loaded buffer, or on the current one.
    Moved,
    /// The target file is not loaded yet. The kernel layer resolves `path`
    /// (the async step) and completes the move with
    /// [`EditorSessions::finish_load`].
    Load { path: String },
}

/// How [`EditorSessions::finish_load`] completed a move.
#[derive(Debug, PartialEq, Eq)]
pub enum LoadStep {
    /// The buffer was loaded and is now current. `message` announces other
    /// sessions bound to the same file.
    Loaded { message: Option<String> },
    /// Another batch loaded the file while this one resolved it; the session
    /// moved to that buffer. The caller's pin for this load is surplus.
    AlreadyLoaded,
    /// The current buffer became dirty while the file resolved; nothing
    /// moved and nothing loaded. The caller's pin for this load is surplus.
    Refused(String),
}

/// The kernel's registry of open editor sessions.
///
/// Every operation is **synchronous**: the `EditorCore` (which is `!Send` via
/// modalkit) never crosses an `await`. The only async step — resolving a path —
/// happens *before* [`open`](Self::open) and
/// [`finish_load`](Self::finish_load), not inside them. So once wired into the
/// shared kernel this registry can live behind a plain mutex.
#[derive(Default)]
pub struct EditorSessions {
    next_id: u64,
    sessions: HashMap<EditorSessionId, EditorSession>,
    /// Each player's vi registers, keyed by the principal that sends the
    /// keys. They are lent to a session's core for one key batch and taken
    /// back after it, so every session a player types in shares them, and no
    /// player reads or overwrites another's. Held in memory only: a kernel
    /// restart empties them (`docs/vi.md`, "Registers").
    registers: HashMap<PrincipalId, Registers>,
}

impl EditorSessions {
    pub fn new() -> Self {
        Self::default()
    }

    /// Open an editor over the argument list `paths`, on a *pre-resolved*
    /// target for the first of them (resolve with [`resolve_editor_target`]
    /// first — the only async step). The first file's current text becomes
    /// the initial buffer and its rollback checkpoint; the others load when
    /// first visited. A path named twice is listed once.
    ///
    /// Two players on one block is a supported state (`docs/vi.md`) — this
    /// never refuses the open on that account. It only announces: when a
    /// buffer is already bound to `target`, the returned state's `message`
    /// names how many sessions hold it, the same status-line channel a bad
    /// `:s` regex uses, so the caller can go back to the existing session or
    /// shut it down instead of editing blind to it (Amy, 2026-08-19).
    pub fn open<S: AsRef<str>>(
        &mut self,
        paths: &[S],
        target: EditorTarget,
        blocks: &SharedBlockStore,
        opener: Option<EditorOpener>,
    ) -> Result<(EditorSessionId, EditorState), String> {
        let first = paths
            .first()
            .ok_or_else(|| "open editor: no path given".to_string())?
            .as_ref();
        let buffer = Buffer::load(target, blocks)?;
        let message = self.announce_bound(target);
        let mut files = vec![ArgFile { buffer: Some(buffer), ..ArgFile::unloaded(first) }];
        for path in &paths[1..] {
            let path = path.as_ref();
            if !files.iter().any(|f| f.path == path) {
                files.push(ArgFile::unloaded(path));
            }
        }
        let recovery = opener.map(|_| RecoveryId::new());
        let mut session = EditorSession { files, current: 0, opener, recovery };
        let mut state = session.state()?;
        state.message = message;
        let id = EditorSessionId(self.next_id);
        self.next_id += 1;
        self.sessions.insert(id, session);
        Ok((id, state))
    }

    /// The status-line notice that other sessions already hold `target`, or
    /// `None` when none does. Counts sessions, not buffers.
    fn announce_bound(&self, target: EditorTarget) -> Option<String> {
        let existing = self
            .sessions
            .values()
            .filter(|s| {
                s.files.iter().any(|f| f.buffer.as_ref().is_some_and(|b| b.target == target))
            })
            .count();
        (existing > 0).then(|| {
            format!(
                "already open in {existing} other session{} — edits from every session merge live",
                if existing == 1 { "" } else { "s" }
            )
        })
    }

    /// Open a session over a recovered argument list: `files` in list order,
    /// the file at index `current` resolved to `target` and loaded now, the
    /// others when first visited. The others keep their recorded cursors until
    /// they load, and load as recovering ([`is_recovering`](Self::is_recovering)).
    /// `disk_text` is the current file's checkpoint when the file has a swap:
    /// the buffer holds the swap's text and reads as changed against the disk
    /// text. The session carries `recovery`, the id of the record it reopens,
    /// so its writes update that record; a live session already carrying it
    /// refuses the open.
    pub fn open_recovered(
        &mut self,
        files: &[RecoveryFile],
        current: usize,
        target: EditorTarget,
        disk_text: Option<&str>,
        blocks: &SharedBlockStore,
        opener: EditorOpener,
        recovery: RecoveryId,
    ) -> Result<(EditorSessionId, EditorState), String> {
        if let Some((live, _)) = self.sessions.iter().find(|(_, s)| s.recovery == Some(recovery)) {
            return Err(format!(
                "recovery record {} is already open in session {}",
                recovery.short(),
                live.0
            ));
        }
        let first = files.get(current).ok_or_else(|| {
            format!("open editor: recovered index {current} is outside a list of {}", files.len())
        })?;
        let buffer = Buffer::load_over(
            target,
            blocks,
            disk_text,
            Some((first.cursor_line, first.cursor_col)),
        )?;
        let message = self.announce_bound(target);
        let mut buffer = Some(buffer);
        let files = files
            .iter()
            .enumerate()
            .map(|(i, f)| {
                if i == current {
                    ArgFile { buffer: buffer.take(), ..ArgFile::unloaded(&f.path) }
                } else {
                    ArgFile {
                        restore_cursor: f.visited.then_some((f.cursor_line, f.cursor_col)),
                        recovering: true,
                        ..ArgFile::unloaded(&f.path)
                    }
                }
            })
            .collect();
        let mut session =
            EditorSession { files, current, opener: Some(opener), recovery: Some(recovery) };
        let mut state = session.state()?;
        state.message = message;
        let id = EditorSessionId(self.next_id);
        self.next_id += 1;
        self.sessions.insert(id, session);
        Ok((id, state))
    }

    /// Whether `path` is a file of the session that has not loaded yet and
    /// came from a recovery. Its load should take the disk text as checkpoint
    /// when the file has a swap.
    pub fn is_recovering(&self, id: EditorSessionId, path: &str) -> bool {
        self.sessions.get(&id).is_some_and(|s| {
            s.files.iter().any(|f| f.path == path && f.recovering && f.buffer.is_none())
        })
    }

    /// What the recovery record of session `id` should say now: the list, the
    /// index, and the live cursor of every loaded file (the recorded one for a
    /// file not loaded yet). `None` for a headless session, which is not
    /// recorded.
    pub fn recovery_snapshot(&mut self, id: EditorSessionId) -> Result<Option<RecoverySnapshot>, String> {
        let session = self.sessions.get_mut(&id).ok_or_else(|| no_session(id))?;
        let (Some(recovery_id), Some(opener)) = (session.recovery, session.opener) else {
            return Ok(None);
        };
        let files = session
            .files
            .iter_mut()
            .map(|f| {
                let (cursor_line, cursor_col) = match f.buffer.as_mut() {
                    Some(b) => {
                        let at = b.core.cursor();
                        line_col_of(&b.core.text(), at)
                    }
                    None => f.restore_cursor.unwrap_or((0, 0)),
                };
                RecoveryFile {
                    path: f.path.clone(),
                    visited: f.buffer.is_some() || f.restore_cursor.is_some(),
                    cursor_line,
                    cursor_col,
                }
            })
            .collect();
        Ok(Some(RecoverySnapshot {
            recovery_id,
            opener_principal: opener.principal,
            opener_context_id: opener.context_id,
            current_index: session.current,
            files,
        }))
    }

    /// The recovery ids of the open sessions. A record whose id is here
    /// belongs to a live session, not to one a restart cut off.
    pub fn live_recovery_ids(&self) -> Vec<RecoveryId> {
        self.sessions.values().filter_map(|s| s.recovery).collect()
    }

    /// The most recent session `principal` opened whose argument list holds
    /// one of `paths`, and the first such path in `paths` order. Only that
    /// principal's sessions count: `vi` joins a person's own session and
    /// never another player's (`docs/vi.md`, "The argument list").
    pub fn find_joinable<S: AsRef<str>>(
        &self,
        principal: PrincipalId,
        paths: &[S],
    ) -> Option<(EditorSessionId, String)> {
        paths.iter().map(AsRef::as_ref).find_map(|path| {
            self.sessions
                .iter()
                .filter(|(_, s)| s.opener.map(|o| o.principal) == Some(principal))
                .filter(|(_, s)| s.position(path).is_some())
                .max_by_key(|(id, _)| id.0)
                .map(|(id, _)| (*id, path.to_string()))
        })
    }

    /// Append `paths` to a session's argument list, skipping any it already
    /// holds. The current file does not change.
    pub fn append<S: AsRef<str>>(&mut self, id: EditorSessionId, paths: &[S]) -> Result<(), String> {
        let session = self.sessions.get_mut(&id).ok_or_else(|| no_session(id))?;
        for path in paths.iter().map(AsRef::as_ref) {
            if session.position(path).is_none() {
                session.files.push(ArgFile::unloaded(path));
            }
        }
        Ok(())
    }

    /// The most-recently-opened session owned by `principal` (the highest id,
    /// since ids increment monotonically) and its current path — what `fg`
    /// re-foregrounds. `None` if the principal has no open editor (`fg` then
    /// reports "no editor session"). The job-control "most recent" semantics.
    pub fn latest_session_for(
        &self,
        principal: PrincipalId,
    ) -> Option<(EditorSessionId, String)> {
        self.sessions
            .iter()
            .filter(|(_, s)| s.opener.map(|o| o.principal) == Some(principal))
            .max_by_key(|(id, _)| id.0)
            .map(|(id, s)| (*id, s.path().to_string()))
    }

    /// The opener (`principal` + originating context) recorded for `id`, if any.
    /// `:r !cmd` reads this to materialize a shell in the opener's context; a
    /// `None` (headless open) makes `:r !cmd` fail loud rather than guess.
    pub fn session_opener(&self, id: EditorSessionId) -> Option<EditorOpener> {
        self.sessions.get(&id).and_then(|s| s.opener)
    }

    /// The most-recently-opened session of *any* opener — `fg`'s shared-trust
    /// fallback when the caller's principal owns none. In a single-user
    /// instrument "the editor" is unambiguous; precise per-principal targeting
    /// (and threading the opener through the external-MCP path, which `:r !cmd`
    /// also needs) is a multi-user refinement. `None` if no editor is open.
    pub fn latest_session_any(&self) -> Option<(EditorSessionId, String)> {
        self.sessions
            .iter()
            .max_by_key(|(id, _)| id.0)
            .map(|(id, s)| (*id, s.path().to_string()))
    }

    /// The terminator the current buffer set aside at load: the block's raw
    /// text is the buffer text followed by it.
    #[cfg(test)]
    fn terminator_for_test(&mut self, id: EditorSessionId) -> String {
        self.sessions.get_mut(&id).unwrap().buffer().unwrap().terminator.clone()
    }

    /// Feed keys to a session, mirror the produced edits onto the kernel block,
    /// and report the outcome. Every caller in this crate except
    /// `Kernel::editor_keys_checked` wants the unrestricted case — this is
    /// [`keys_checked`](Self::keys_checked) with `can_write: true`.
    pub fn keys(
        &mut self,
        id: EditorSessionId,
        keys: &str,
        blocks: &SharedBlockStore,
        actor: PrincipalId,
    ) -> Result<KeysOutcome, String> {
        self.keys_checked(id, keys, blocks, actor, true)
    }

    /// [`keys`](Self::keys), gated: a write intent in this batch (`ZZ`, or
    /// `:w`/`:wq`/`:x` in the parsed command line) refuses when `can_write` is
    /// false, vim's read-only-buffer shape (E45) — the buffer stays open and
    /// dirty, nothing is checkpointed or quit, and the status line names the
    /// missing capability. A plain edit or navigation batch is unaffected
    /// either way; only a batch that would actually write is gated, and it is
    /// gated here, at the point this state machine turns a write intent into
    /// effect — not by scanning the raw key text beforehand. See
    /// `docs/vi.md`.
    ///
    /// If the batch contained a `ZZ`/`ZQ` (which modalkit, owning the real mode,
    /// distinguishes from an inserted `ZZ`), the session is saved/discarded and
    /// dropped here and the outcome is [`KeysOutcome::Closed`]; otherwise it is
    /// [`KeysOutcome::Updated`] with the new renderer state.
    pub fn keys_checked(
        &mut self,
        id: EditorSessionId,
        keys: &str,
        blocks: &SharedBlockStore,
        actor: PrincipalId,
        can_write: bool,
    ) -> Result<KeysOutcome, String> {
        self.batch(id, BatchInput::Keys(keys), blocks, actor, can_write)
    }

    /// [`keys_checked`](Self::keys_checked) for either input: typed keys, or
    /// the keys a previous batch left unfed ([`KeysUpdate::rest`]), which run
    /// in the session's current buffer, the one a move landed on.
    pub fn batch(
        &mut self,
        id: EditorSessionId,
        input: BatchInput<'_>,
        blocks: &SharedBlockStore,
        actor: PrincipalId,
        can_write: bool,
    ) -> Result<KeysOutcome, String> {
        let (close, commands, rest) = {
            let session = self.sessions.get_mut(&id).ok_or_else(|| no_session(id))?;
            let buffer = session.buffer()?;
            let len_before = buffer.core.text().chars().count();
            buffer.core.give_registers(self.registers.remove(&actor).unwrap_or_default());
            let ops = match input {
                BatchInput::Keys(keys) => buffer.core.apply_keys(keys),
                BatchInput::Pending(keys) => buffer.core.apply_pending(keys),
            };
            self.registers.insert(actor, buffer.core.take_registers());
            let buffer_after = buffer.core.text();
            mirror_ops(
                &ops,
                &buffer.target,
                &buffer.terminator,
                len_before,
                &buffer_after,
                blocks,
                actor,
                "editor keys",
            )?;
            (buffer.core.take_close(), buffer.core.take_commands(), buffer.core.take_rest())
        };

        // `ZZ`/`ZQ` close the session. The returned state is informational (the
        // last view before close); renderers react to the `Closed` push.
        if let Some(close) = close {
            let (path, index) = self.current_file(id)?;
            match close {
                CloseRequest::Write => {
                    if !can_write {
                        return self.refuse_write(id);
                    }
                    // Another dirty file keeps the session open: save this
                    // one (the kernel layer flushes, then checkpoints) and
                    // name the first dirty file.
                    if let Some(other) = self.first_other_dirty(id)? {
                        let mut state = self.state(id)?;
                        state.message = Some(hidden_buffer_dirty(&other));
                        return Ok(KeysOutcome::Updated(KeysUpdate {
                            state,
                            path,
                            buffer: index,
                            saved: true,
                            forced: false,
                            quit: QuitReport::default(),
                            go: None,
                            rest: None,
                        }));
                    }
                    // ZZ: checkpoint current as saved, then quit — the rollback
                    // to that just-taken checkpoint is a no-op. `saved: true`
                    // tells the kernel layer to flush a file-backed session.
                    let state = self.save(id)?;
                    let quit = self.quit(id, blocks, actor)?;
                    return Ok(KeysOutcome::Closed(KeysUpdate {
                        state,
                        path,
                        buffer: index,
                        saved: true,
                        forced: false,
                        quit,
                        go: None,
                        rest: None,
                    }));
                }
                CloseRequest::Discard => {
                    // ZQ: snapshot the view, then roll every file back to its
                    // last checkpoint. Nothing to flush — the discard never
                    // advanced a checkpoint. `quit.rolled_back` tells the
                    // kernel layer which rollbacks actually rewrote a block.
                    let state = self.state(id)?;
                    let quit = self.quit(id, blocks, actor)?;
                    return Ok(KeysOutcome::Closed(KeysUpdate {
                        state,
                        path,
                        buffer: index,
                        saved: false,
                        forced: false,
                        quit,
                        go: None,
                        rest: None,
                    }));
                }
            }
        }

        // A submitted `:`-line (`:w`/`:wq`/`:q!`/`:n`/…). A parsed batch runs in
        // order; an unknown command or a bad `:s` regex (both arrive as
        // `Some(Err)`) reports vim-style on the status line and keeps the session
        // open — never errors the whole `editor_keys` call out from under the
        // renderer (the front door would otherwise surface it as a hard failure).
        let mut outcome = match commands {
            Some(Ok(cmds)) => self.run_commands(id, cmds, blocks, actor, can_write)?,
            Some(Err(msg)) => {
                let mut update = self.update(id)?;
                update.state.message = Some(msg);
                KeysOutcome::Updated(update)
            }
            None => KeysOutcome::Updated(self.update(id)?),
        };
        if let KeysOutcome::Updated(update) = &mut outcome {
            update.rest = rest;
        }
        Ok(outcome)
    }

    /// A plain `Updated` for the session's current file: nothing to save,
    /// close or move.
    fn update(&mut self, id: EditorSessionId) -> Result<KeysUpdate, String> {
        let (path, buffer) = self.current_file(id)?;
        Ok(KeysUpdate {
            state: self.state(id)?,
            path,
            buffer,
            saved: false,
            forced: false,
            quit: QuitReport::default(),
            go: None,
            rest: None,
        })
    }

    /// A write intent refused for lacking `Capability::Editor` — shared by
    /// [`keys_checked`](Self::keys_checked)'s `ZZ` arm and
    /// [`run_commands`](Self::run_commands)'s `Write`/`Quit` arm, so the
    /// outcome shape and message are identical everywhere a refusal can fire.
    /// Nothing is mutated: no checkpoint, no quit, no flush, no move — the
    /// session is exactly as it was before this batch's write intent.
    fn refuse_write(&mut self, id: EditorSessionId) -> Result<KeysOutcome, String> {
        let mut update = self.update(id)?;
        update.state.message = Some(WRITE_CAPABILITY_REFUSED.to_string());
        Ok(KeysOutcome::Updated(update))
    }

    /// Act on a parsed `:`-command batch (`docs/vi.md` → *Command mode*). `Quit`
    /// closes — refusing a dirty buffer without `!` (vim's E37 "No write since
    /// last change", reported on the status line like every dialect-level
    /// failure), and refusing while another file of the argument list is
    /// dirty (vim's E162). Returns [`KeysOutcome::Closed`] if a `Quit` ran,
    /// else [`KeysOutcome::Updated`]. The returned `saved` flag (true whenever
    /// this batch ran a `Write`, including inside `:wq`/`:x`) is the kernel
    /// layer's cue to flush a file-backed session.
    ///
    /// **`Write` does not checkpoint by itself.** The checkpoint must not
    /// advance until the write actually lands, and this pure registry has no
    /// file cache to flush through — that lives in the kernel layer
    /// (`Kernel::editor_keys`/`editor_save`), which flushes first and
    /// checkpoints only on success (docs/file-buffers.md). A `Quit` in the
    /// same batch (`:wq`/`:x`) is the one case that checkpoints
    /// unconditionally: the session is about to be dropped, `quit`'s
    /// rollback reads the checkpoint to know what to restore, and there is
    /// no later kernel-layer moment left to defer to — deferring here would
    /// let quit roll back the very edit `:wq` promised to keep. The disk
    /// flush that follows in the kernel layer can still fail; that surfaces
    /// as the existing session-lost hard error on `KeysOutcome::Closed`,
    /// never a reverted edit.
    ///
    /// **`Go` does not move here either.** It rides out on
    /// [`KeysUpdate::go`] so the kernel layer moves only after the batch's
    /// write landed (`:wn`), and can resolve a file not yet loaded.
    ///
    /// `can_write: false` refuses the `Write` command outright — same
    /// message and outcome shape as [`refuse_write`](Self::refuse_write) —
    /// before it can set `write_requested` or advance anything, so a `:wq`
    /// with no write capability never reaches `Quit` either.
    fn run_commands(
        &mut self,
        id: EditorSessionId,
        commands: Vec<CommandRequest>,
        blocks: &SharedBlockStore,
        actor: PrincipalId,
        can_write: bool,
    ) -> Result<KeysOutcome, String> {
        let (path, index) = self.current_file(id)?;
        let mut write_requested = false;
        let mut forced = false;
        let mut message = None;
        for cmd in commands {
            match cmd {
                // `force` rides through to the returned `KeysUpdate` for the
                // kernel layer's W12 changed-under-us guard
                // (`FileDocumentCache::flush_one_guarded`,
                // docs/file-buffers.md): a plain `:w` refuses when disk moved
                // under the buffer since it was loaded, and `:w!` overrides
                // that refusal. This pure registry has no file cache to check
                // against — it only carries the bit.
                CommandRequest::Write { force } => {
                    if !can_write {
                        return self.refuse_write(id);
                    }
                    write_requested = true;
                    forced = force;
                }
                CommandRequest::Args => {
                    let session = self.sessions.get(&id).ok_or_else(|| no_session(id))?;
                    message = Some(session.args_line());
                }
                CommandRequest::Go { to, force } => {
                    let mut update = self.update(id)?;
                    update.saved = write_requested;
                    update.forced = forced;
                    update.go = Some((to, force));
                    return Ok(KeysOutcome::Updated(update));
                }
                CommandRequest::Quit { force } => {
                    // A dialect-level refusal, not an RPC failure: ride the
                    // status line (like E492) and keep the session open. A
                    // hard error here never reaches the GUI and leaves the
                    // renderer's `:`-strip showing the stale submitted line.
                    let refusal = if force {
                        None
                    } else if !write_requested && self.state(id)?.dirty {
                        Some(NO_WRITE_SINCE_LAST_CHANGE.to_string())
                    } else {
                        self.first_other_dirty(id)?.map(|other| hidden_buffer_dirty(&other))
                    };
                    if let Some(refusal) = refusal {
                        // `:wq` still writes this file; the kernel layer
                        // flushes, then checkpoints it.
                        let mut update = self.update(id)?;
                        update.state.message = Some(refusal);
                        update.saved = write_requested;
                        update.forced = forced;
                        return Ok(KeysOutcome::Updated(update));
                    }
                    let state = if write_requested { self.save(id)? } else { self.state(id)? };
                    let quit = self.quit(id, blocks, actor)?;
                    // `write_requested` covers `:wq`/`:x` (Write ran earlier
                    // in this same batch); a bare `:q`/`:q!` never set it, so
                    // nothing to flush. `quit.rolled_back` is independent of
                    // that — a `:q!` discard can still have rewritten blocks.
                    return Ok(KeysOutcome::Closed(KeysUpdate {
                        state,
                        path,
                        buffer: index,
                        saved: write_requested,
                        forced,
                        quit,
                        go: None,
                        rest: None,
                    }));
                }
            }
        }
        // No `Quit` or `Go` ran — a bare `:w`/`:w!`, `:args`, or an empty `:`
        // line. The checkpoint defers to the kernel layer's
        // flush-then-checkpoint, so `state` here still reads dirty.
        let mut update = self.update(id)?;
        update.state.message = message;
        update.saved = write_requested;
        update.forced = forced;
        Ok(KeysOutcome::Updated(update))
    }

    /// The current file of a session: its path and its index in the list.
    pub fn current_file(&self, id: EditorSessionId) -> Result<(String, usize), String> {
        let session = self.sessions.get(&id).ok_or_else(|| no_session(id))?;
        Ok((session.path().to_string(), session.current))
    }

    /// The first file of a session's list, other than the current one, that
    /// differs from its checkpoint.
    fn first_other_dirty(&self, id: EditorSessionId) -> Result<Option<String>, String> {
        let session = self.sessions.get(&id).ok_or_else(|| no_session(id))?;
        Ok(session.first_other_dirty().map(str::to_string))
    }

    /// Start a move through a session's argument list. A move to a loaded
    /// buffer, or to the current file, completes here. Without `force`, it
    /// refuses to leave a buffer that differs from its checkpoint (vim's
    /// E37); with it, that buffer stays dirty and hidden — nothing is
    /// discarded. A move past either end refuses with vim's message. A move
    /// to a file not loaded yet returns [`MoveStep::Load`]; `:e` of a path
    /// not in the list appends it only once it loads.
    pub fn begin_move(
        &mut self,
        id: EditorSessionId,
        to: &ArgMove,
        force: bool,
    ) -> Result<MoveStep, String> {
        let session = self.sessions.get_mut(&id).ok_or_else(|| no_session(id))?;
        let last = session.files.len() - 1;
        let index = match to {
            ArgMove::Next(n) => match session.current.checked_add(*n).filter(|i| *i <= last) {
                Some(i) => Some(i),
                None => return Ok(MoveStep::Refused("Cannot go beyond last file".to_string())),
            },
            ArgMove::Prev(n) => match session.current.checked_sub(*n) {
                Some(i) => Some(i),
                None => return Ok(MoveStep::Refused("Cannot go before first file".to_string())),
            },
            ArgMove::First => Some(0),
            ArgMove::Last => Some(last),
            ArgMove::Edit(path) => session.position(path),
        };
        if index == Some(session.current) {
            return Ok(MoveStep::Moved);
        }
        if !force && session.buffer()?.dirty() {
            return Ok(MoveStep::Refused(NO_WRITE_SINCE_LAST_CHANGE.to_string()));
        }
        match index {
            Some(i) if session.files[i].buffer.is_some() => {
                session.current = i;
                Ok(MoveStep::Moved)
            }
            Some(i) => Ok(MoveStep::Load { path: session.files[i].path.clone() }),
            None => match to {
                ArgMove::Edit(path) => Ok(MoveStep::Load { path: path.clone() }),
                _ => Err("editor: argument list move found no file".to_string()),
            },
        }
    }

    /// Complete a [`MoveStep::Load`] on a *pre-resolved* target: load the
    /// file's buffer (appending the path to the list when it is not in it)
    /// and make it current. Re-checks the current buffer's dirtiness, which
    /// another batch may have changed while the path resolved.
    pub fn finish_load(
        &mut self,
        id: EditorSessionId,
        path: &str,
        target: EditorTarget,
        force: bool,
        blocks: &SharedBlockStore,
    ) -> Result<LoadStep, String> {
        self.finish_load_over(id, path, target, force, blocks, None)
    }

    /// [`finish_load`](Self::finish_load), with `disk_text` as the loaded
    /// buffer's checkpoint when the file is one a recovery brought back
    /// ([`is_recovering`](Self::is_recovering)). The file's recorded cursor
    /// is restored on any load.
    pub fn finish_load_over(
        &mut self,
        id: EditorSessionId,
        path: &str,
        target: EditorTarget,
        force: bool,
        blocks: &SharedBlockStore,
        disk_text: Option<&str>,
    ) -> Result<LoadStep, String> {
        let message = self.announce_bound(target);
        let session = self.sessions.get_mut(&id).ok_or_else(|| no_session(id))?;
        if session.position(path) == Some(session.current) {
            return Ok(LoadStep::AlreadyLoaded);
        }
        if !force && session.buffer()?.dirty() {
            return Ok(LoadStep::Refused(NO_WRITE_SINCE_LAST_CHANGE.to_string()));
        }
        let index = match session.position(path) {
            Some(i) => i,
            None => {
                session.files.push(ArgFile::unloaded(path));
                session.files.len() - 1
            }
        };
        if session.files[index].buffer.is_some() {
            session.current = index;
            return Ok(LoadStep::AlreadyLoaded);
        }
        let cursor = session.files[index].restore_cursor;
        session.files[index].buffer = Some(Buffer::load_over(target, blocks, disk_text, cursor)?);
        session.files[index].restore_cursor = None;
        session.files[index].recovering = false;
        session.current = index;
        Ok(LoadStep::Loaded { message })
    }

    /// Reconcile every loaded buffer bound to `(context_id, block_id)`, in
    /// every session, against the block's *current* text, after some
    /// **other** writer (a sibling editor session, an MCP file edit, a
    /// streaming turn) mutated it. Returns the `(id, new state)` of every
    /// session whose current buffer actually changed — the caller publishes
    /// those on the editor push channel. A hidden buffer merges too, so it
    /// is current when its file is visited again.
    ///
    /// A buffer that already matches the block is skipped: that is the
    /// session's *own* mirror write echoing back through the block flow (the
    /// mirror is faithful, so its buffer equals the block), and reconciling it
    /// would jolt the cursor on every keystroke. Reads the block at most once,
    /// and only when a buffer is actually bound here, so the hot path (no
    /// editor open, or an unrelated block) costs just the match scan.
    pub fn reconcile_block(
        &mut self,
        context_id: ContextId,
        block_id: BlockId,
        blocks: &SharedBlockStore,
    ) -> Vec<(EditorSessionId, EditorState)> {
        let bound = |b: &Buffer| b.target.context_id == context_id && b.target.block_id == block_id;
        let Some(target) = self
            .sessions
            .values()
            .flat_map(|s| s.files.iter().filter_map(|f| f.buffer.as_ref()))
            .find(|b| bound(b))
            .map(|b| b.target)
        else {
            return Vec::new();
        };
        // The block's text is the merged truth; reconcile against its normalized
        // (terminator-stripped) view, matching EditorCore's normalized buffer.
        let raw = match block_text(blocks, &target) {
            Ok(t) => t,
            Err(_) => return Vec::new(), // block gone (deleted) — nothing to do
        };
        let merged = raw.strip_suffix('\n').unwrap_or(&raw);

        let mut changed = Vec::new();
        for (id, session) in self.sessions.iter_mut() {
            let mut current_moved = false;
            for (index, file) in session.files.iter_mut().enumerate() {
                let Some(buffer) = file.buffer.as_mut().filter(|b| bound(b)) else {
                    continue;
                };
                if buffer.core.apply_remote_text(merged) {
                    // A non-self write merged in: this buffer's checkpoint no
                    // longer owns the block's delta, so a ZQ rollback would
                    // clobber peer work. Sticky until the checkpoint advances.
                    buffer.peer_wrote = true;
                    current_moved |= index == session.current;
                }
            }
            if current_moved && let Ok(state) = session.state() {
                changed.push((*id, state));
            }
        }
        changed
    }

    /// Take any `:r` read intent the last [`keys`](Self::keys) batch surfaced on
    /// a session's current buffer (the kernel fulfills it asynchronously —
    /// fetch, then [`insert_text`](Self::insert_text)). `None` if no such
    /// session or no intent.
    pub fn take_io(&mut self, id: EditorSessionId) -> Option<EditorIo> {
        self.sessions.get_mut(&id)?.buffer().ok()?.core.take_io()
    }

    /// The session's current buffer index and its leader-cursor char offset,
    /// or `None` if no such session. The kernel captures this at `:r`-submit
    /// time so the async insert lands in that buffer, where the command was
    /// issued, not wherever a concurrent batch moved the session or cursor
    /// during the fetch.
    pub fn session_cursor(&mut self, id: EditorSessionId) -> Option<(usize, usize)> {
        let session = self.sessions.get_mut(&id)?;
        let index = session.current;
        Some((index, session.buffer().ok()?.core.cursor()))
    }

    /// Insert kernel-fetched `text` at `offset` in buffer `index` (the buffer
    /// and cursor captured when the `:r` was submitted — see
    /// [`session_cursor`](Self::session_cursor)), mirror the produced ops onto
    /// the owning kernel block, and return the session's new state. Fails loud
    /// if the mirror write fails.
    pub fn insert_text(
        &mut self,
        id: EditorSessionId,
        index: usize,
        text: &str,
        offset: usize,
        blocks: &SharedBlockStore,
        actor: PrincipalId,
    ) -> Result<EditorState, String> {
        let session = self.sessions.get_mut(&id).ok_or_else(|| no_session(id))?;
        let buffer = session.buffer_at(index)?;
        let len_before = buffer.core.text().chars().count();
        let ops = buffer.core.insert_at(text, offset);
        let buffer_after = buffer.core.text();
        mirror_ops(
            &ops,
            &buffer.target,
            &buffer.terminator,
            len_before,
            &buffer_after,
            blocks,
            actor,
            "editor :r",
        )?;
        session.state()
    }

    /// Whether buffer `index` of a session differs from its checkpoint.
    pub fn buffer_dirty(&mut self, id: EditorSessionId, index: usize) -> Result<bool, String> {
        let session = self.sessions.get_mut(&id).ok_or_else(|| no_session(id))?;
        Ok(session.buffer_at(index)?.dirty())
    }

    /// Insert `text` at the session's *live* cursor — a paste, not
    /// keystrokes (`docs/vi.md`: `editor_keys` reads `<` as the start of a
    /// key token). Unlike [`insert_text`](Self::insert_text) (which the
    /// `:r` async fetch path uses with an offset captured at submit time,
    /// because the cursor can move during the await), a paste is
    /// synchronous — nothing runs between the caller reading the cursor and
    /// this call — so [`EditorCore::insert_at_cursor`] reads it itself.
    ///
    /// `EditorCore::insert_at_cursor` splices the buffer directly; it never
    /// touches the vim state machine, so the session's mode is exactly what
    /// it was before the paste — insert stays insert, normal stays normal.
    ///
    /// Refuses while the `:` command line is open ([`PASTE_REFUSED_COMMAND_LINE`]):
    /// the buffer is left unchanged and the message rides the returned
    /// state, the same dialect-level channel a bad `:s` regex uses — never
    /// an RPC error the renderer can't display. Both renderers draw an open
    /// `:` line in preference to the message, so a client that knows the
    /// line is open refuses the paste itself with its own notice (the tui's
    /// `paste_target`); this refusal is the backstop for one that does not.
    pub fn insert_at_cursor(
        &mut self,
        id: EditorSessionId,
        text: &str,
        blocks: &SharedBlockStore,
        actor: PrincipalId,
    ) -> Result<EditorState, String> {
        let current = self.state(id)?;
        if current.command_line.is_some() {
            let mut state = current;
            state.message = Some(PASTE_REFUSED_COMMAND_LINE.to_string());
            return Ok(state);
        }
        let session = self.sessions.get_mut(&id).ok_or_else(|| no_session(id))?;
        let buffer = session.buffer()?;
        let len_before = buffer.core.text().chars().count();
        let ops = buffer.core.insert_at_cursor(text);
        let buffer_after = buffer.core.text();
        mirror_ops(
            &ops,
            &buffer.target,
            &buffer.terminator,
            len_before,
            &buffer_after,
            blocks,
            actor,
            "editor insert",
        )?;
        session.state()
    }

    /// Current state of a session.
    pub fn state(&mut self, id: EditorSessionId) -> Result<EditorState, String> {
        self.sessions.get_mut(&id).ok_or_else(|| no_session(id))?.state()
    }

    /// The current path of a session, or `None` if no such session. Captured
    /// before a `ZZ`/`ZQ` (which drops the session) so the caller can
    /// invalidate the file cache for it afterward.
    pub fn session_path(&self, id: EditorSessionId) -> Option<String> {
        self.sessions.get(&id).map(|s| s.path().to_string())
    }

    /// `ZZ` — checkpoint the current buffer as saved, returning the now-clean
    /// state. The kernel layer (which holds the file-document cache this pure
    /// session registry does not) flushes it to disk after this call — see
    /// `Kernel::editor_save`/`Kernel::editor_keys`.
    pub fn save(&mut self, id: EditorSessionId) -> Result<EditorState, String> {
        let (_, index) = self.current_file(id)?;
        self.save_at(id, index)
    }

    /// Checkpoint buffer `index` of a session — the buffer a flush landed
    /// for, which may no longer be current — and return the session's state.
    pub fn save_at(&mut self, id: EditorSessionId, index: usize) -> Result<EditorState, String> {
        let session = self.sessions.get_mut(&id).ok_or_else(|| no_session(id))?;
        let buffer = session.buffer_at(index)?;
        buffer.saved_content = buffer.core.text();
        // The checkpoint now contains every merged peer edit, so a rollback to
        // it can no longer clobber them — the entanglement resets.
        buffer.peer_wrote = false;
        session.state()
    }

    /// `ZQ` — discard changes since the last checkpoint by writing each loaded
    /// file's saved text back onto its block (an inverse forward edit — the
    /// block log has no history erasure), then drop the session.
    ///
    /// **Entanglement guard: detach, don't retract.** A file's rollback runs
    /// only when its buffer was provably alone with the block since its
    /// checkpoint. If another buffer (in any session) is still bound to it,
    /// or a peer's write merged in since the checkpoint ([`Buffer::peer_wrote`]),
    /// the merged text is shared work — rolling it back would clobber them. A
    /// shared-session `ZQ` therefore just quits *this* player out of the doc
    /// and leaves the block's merged truth for the others (Amy, 2026-07-07;
    /// `docs/vi.md` → Rollback).
    ///
    /// Remove the session only after every rollback succeeds. On failure,
    /// callers retain both the session and its file-cache pins so the player
    /// can retry.
    ///
    /// The report names every loaded file and the ones whose rollback
    /// rewrote the block. The kernel uses it to release pins and mark the
    /// file cache dirty; this registry has no file cache of its own.
    /// Detaching after a peer write or restoring an unchanged checkpoint does
    /// not mutate the block.
    pub fn quit(
        &mut self,
        id: EditorSessionId,
        blocks: &SharedBlockStore,
        actor: PrincipalId,
    ) -> Result<QuitReport, String> {
        let session = self.sessions.get(&id).ok_or_else(|| no_session(id))?;
        let mut report = QuitReport { recovery: session.recovery, ..QuitReport::default() };
        for (index, file) in session.files.iter().enumerate() {
            let Some(buffer) = &file.buffer else { continue };
            report.loaded.push(file.path.clone());
            let sibling_bound = self.sessions.iter().any(|(sid, s)| {
                s.files.iter().enumerate().any(|(i, f)| {
                    (*sid != id || i != index)
                        && f.buffer.as_ref().is_some_and(|b| b.target == buffer.target)
                })
            });
            if sibling_bound || buffer.peer_wrote {
                continue;
            }
            // Restore the normalized checkpoint *plus* the block's terminator,
            // so a rollback never strips a trailing newline the block opened with.
            let restore = format!("{}{}", buffer.saved_content, buffer.terminator);
            let current = block_text(blocks, &buffer.target)?;
            if current != restore {
                blocks
                    .edit_text_as(
                        buffer.target.context_id,
                        &buffer.target.block_id,
                        0,
                        &restore,
                        current.chars().count(),
                        Some(actor),
                    )
                    .map_err(|e| format!("editor quit: rollback of {} failed: {e}", file.path))?;
                report.rolled_back.push(file.path.clone());
            }
        }
        self.sessions.remove(&id);
        Ok(report)
    }

    /// Drop a session without touching any block: for a session that has
    /// just opened and taken no input, when the kernel layer cannot pin it.
    /// Returns the session's recovery record id, for the kernel layer to
    /// delete.
    pub fn forget(&mut self, id: EditorSessionId) -> Option<RecoveryId> {
        self.sessions.remove(&id).and_then(|s| s.recovery)
    }

    /// Whether a session is still open.
    pub fn is_open(&self, id: EditorSessionId) -> bool {
        self.sessions.contains_key(&id)
    }

    /// A census of every open session — `kj vi list`'s data. A session
    /// whose opener walked away (a model that opened and never quit, a dead
    /// headless driver) is otherwise invisible and immortal; this makes it
    /// visible. No GC/eviction here — observability first, judgment later.
    /// Sorted by session id ascending (open order).
    pub fn list(&self) -> Vec<EditorSessionInfo> {
        let mut out: Vec<EditorSessionInfo> = self
            .sessions
            .iter()
            .filter_map(|(id, s)| {
                let buffer = s.files[s.current].buffer.as_ref()?;
                let dirty_files: Vec<String> = s
                    .files
                    .iter()
                    .filter(|f| f.buffer.as_ref().is_some_and(Buffer::dirty))
                    .map(|f| f.path.clone())
                    .collect();
                Some(EditorSessionInfo {
                    session: id.as_u64(),
                    path: s.path().to_string(),
                    context_id: buffer.target.context_id.to_hex(),
                    block_id: buffer.target.block_id.to_key(),
                    arg_index: s.current,
                    arg_count: s.files.len(),
                    dirty: !dirty_files.is_empty(),
                    dirty_files,
                    mode: buffer.core.mode(),
                    opener: s.opener.map(|o| o.principal.to_hex()),
                })
            })
            .collect();
        out.sort_by_key(|info| info.session);
        out
    }
}

/// [`EditorSessions`] wrapped to assert `Send`, so the registry can be a field
/// of the shared (`Send + Sync`) kernel behind a sync mutex.
///
/// SAFETY: `EditorCore` is `!Send` only *structurally* — modalkit's `VimMachine`
/// holds a `Box<dyn Dialog>` with no `Send` bound. We never install a dialog
/// (there is no command-bar dialog UI in the kernel), so it carries no
/// thread-affine state. Every access is serialized through the kernel's mutex
/// (one thread touches a session at a time); the only thread crossing is the
/// lock handoff, which moves plain data. This mirrors the app's documented
/// `unsafe impl Send for VimMachineResource`.
pub struct SendSessions(pub EditorSessions);

// SAFETY: see the type doc above — no thread-affine state; access is serialized.
unsafe impl Send for SendSessions {}

/// Build a renderer-facing state, marking dirty against `checkpoint`.
fn state_of(
    core: &mut EditorCore,
    checkpoint: &str,
    path: String,
    arg_index: usize,
    arg_count: usize,
) -> EditorState {
    let text = core.text();
    let dirty = text != checkpoint;
    let cursor = core.cursor();
    let mode = core.mode();
    let command_line = core.command_line();
    let selection = core.selection();
    EditorState {
        text,
        cursor,
        mode,
        dirty,
        command_line,
        selection,
        // A fresh state carries no status message; the command path sets one only
        // when a `:`-line errored, and it clears on the next keystroke batch.
        message: None,
        path,
        arg_index,
        arg_count,
    }
}

/// Mirror a batch of buffer edits onto the kernel block, then reconcile the
/// terminator boundary.
///
/// The block's raw text is the buffer's text followed by the `terminator`
/// (`"\n"` or `""`) the session set aside at open. `EditOp` offsets count the
/// buffer, which does not include that character, so an edit reaching the end
/// of the buffer cannot say whether it means to land before or after it. The
/// case that matters: inserting text that ends in the terminator at the very
/// end splices in front of the raw one and stacks a second (`"end\n"` plus
/// `"X\n"` giving `"endX\n\n"`).
///
/// So the ops go on verbatim and the boundary is settled once afterwards, and
/// only for an edit that could have doubled it: one that reached the end of
/// the buffer and supplied a terminator of its own. Such a batch loses the
/// extra terminator if the block now carries two. Every other disagreement is
/// left alone. That matters twice over. Two players on one block is a
/// supported state (`docs/vi.md`), so rewriting the block to match this buffer
/// would discard the other player's work. And `buffer_after` is not always the
/// block's content minus the terminator: `EditorCore::insert_at` rebuilds the
/// buffer from its own normalized text, which drops one trailing newline, so a
/// block ending in a blank line comes back one newline short. Reconciling on
/// that alone would delete a newline the edit never touched.
///
/// One rule, one place: `keys_checked` (typed keys, including Enter),
/// `insert_at_cursor` (paste), and `insert_text` (`:r`'s fetch-then-insert)
/// all call this instead of splicing ops onto the raw block themselves.
fn mirror_ops(
    ops: &[kaijutsu_editor::EditOp],
    target: &EditorTarget,
    terminator: &str,
    len_before: usize,
    buffer_after: &str,
    blocks: &SharedBlockStore,
    actor: PrincipalId,
    caller: &str,
) -> Result<(), String> {
    let mut len = len_before;
    let mut supplied_terminator_at_tail = false;
    for op in ops {
        if !terminator.is_empty()
            && op.offset + op.delete == len
            && op.insert.ends_with(terminator)
        {
            supplied_terminator_at_tail = true;
        }
        blocks
            .edit_text_as(
                target.context_id,
                &target.block_id,
                op.offset,
                &op.insert,
                op.delete,
                Some(actor),
            )
            .map_err(|e| format!("{caller}: block mirror failed: {e}"))?;
        len = len
            .checked_sub(op.delete)
            .ok_or_else(|| {
                format!("{caller}: edit op deletes {} of {len} characters", op.delete)
            })?
            + op.insert.chars().count();
    }
    if !supplied_terminator_at_tail {
        return Ok(());
    }
    let expected = format!("{buffer_after}{terminator}");
    let actual = block_text(blocks, target)?;
    if actual == format!("{expected}{terminator}") {
        blocks
            .edit_text_as(
                target.context_id,
                &target.block_id,
                expected.chars().count(),
                "",
                terminator.chars().count(),
                Some(actor),
            )
            .map_err(|e| format!("{caller}: terminator reconcile failed: {e}"))?;
    }
    Ok(())
}

/// The text every "unknown editor session" error starts with. A client
/// matches it to tell a kernel that lost the session (a restart) from a
/// transient failure.
pub const NO_SUCH_SESSION: &str = "editor: no such session";

fn no_session(id: EditorSessionId) -> String {
    format!("{NO_SUCH_SESSION} {}", id.0)
}

/// Read the current text of a specific `(context, block)`.
fn block_text(blocks: &SharedBlockStore, target: &EditorTarget) -> Result<String, String> {
    let entry = blocks
        .get(target.context_id)
        .ok_or_else(|| format!("editor: document {} not found", target.context_id))?;
    entry
        .doc
        .blocks_ordered()
        .iter()
        .find(|b| b.id == target.block_id)
        .map(|b| b.content.clone())
        .ok_or_else(|| format!("editor: block not found in {}", target.context_id))
}

#[cfg(test)]
mod session_tests {
    //! e2e editor-session lifecycle against a live block store. No GUI: drives
    //! the same surface the app/model/test all share (vi.md test layer 2).
    use super::*;
    use crate::block_store::shared_block_store_with_db;
    use crate::kernel_db::KernelDb;
    use crate::vfs::{LocalBackend, MountTable};
    use kaijutsu_types::PrincipalId;
    use kaijutsu_types::paths::RC_ROOT;
    use std::sync::Arc;

    const RC_PATH: &str = "/config/rc/coder/create/S00.kai";

    /// A block store seeded with one file loaded through an ordinary
    /// `LocalBackend` mount over a tempdir — the production shape for
    /// `/config/rc` (`docs/config-namespace.md`) — plus the resolved editor
    /// target for it. The tempdir is dropped before this returns: resolution
    /// loads the content into the block store once, and every session
    /// operation after that reads/writes the block directly, never the file.
    async fn seeded(initial: &[u8]) -> (SharedBlockStore, EditorTarget) {
        let creator = PrincipalId::system();
        let db = Arc::new(parking_lot::Mutex::new(KernelDb::temporary().unwrap()));
        let ws = db.lock().get_or_create_default_workspace(creator).unwrap();
        let blocks = shared_block_store_with_db(db.clone(), ws, creator);
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("coder/create/S00.kai");
        std::fs::create_dir_all(script.parent().unwrap()).unwrap();
        std::fs::write(&script, initial).unwrap();
        let mounts = Arc::new({
            let mt = MountTable::new();
            mt.mount(RC_ROOT, LocalBackend::new(dir.path())).await;
            mt
        });
        let fc = FileDocumentCache::new(blocks.clone(), mounts.clone(), db);
        let target = resolve_editor_target(RC_PATH, &fc).await.unwrap();
        (blocks, target)
    }

    #[tokio::test]
    async fn keystrokes_mirror_to_the_owning_block() {
        let (blocks, target) = seeded(b"hello").await;
        let mut sessions = EditorSessions::new();
        let (id, st) = sessions.open(&[RC_PATH], target, &blocks, None).unwrap();
        assert_eq!(st.text, "hello");
        assert!(!st.dirty);

        // Insert "X" at the start: i X <Esc>.
        let outcome = sessions.keys(id, "iX<Esc>", &blocks, PrincipalId::system()).unwrap();
        assert_eq!(outcome.state().text, "Xhello");
        assert!(outcome.state().dirty, "buffer diverged from checkpoint");

        // The invariant that makes this surface trustworthy: the kernel block now
        // equals the editor buffer (edit mirroring is faithful).
        assert_eq!(block_text(&blocks, &target).unwrap(), "Xhello");
    }

    /// The visual selection rides the state and its JSON shape; leaving
    /// visual mode clears both.
    #[tokio::test]
    #[allow(clippy::single_range_in_vec_init)] // a span list holding one `Range` is the intended value
    async fn visual_selection_rides_the_state_and_its_json() {
        let (blocks, target) = seeded(b"hello world").await;
        let mut sessions = EditorSessions::new();
        let (id, st) = sessions.open(&[RC_PATH], target, &blocks, None).unwrap();
        assert_eq!(st.selection, None);

        let outcome = sessions.keys(id, "wve", &blocks, PrincipalId::system()).unwrap();
        let expected = EditorSelection { shape: SelectionShape::Charwise, spans: vec![6..11] };
        assert_eq!(outcome.state().selection, Some(expected));
        assert_eq!(
            outcome.state().to_json(id)["selection"],
            serde_json::json!({"shape": "charwise", "spans": [{"start": 6, "end": 11}]})
        );

        let outcome = sessions.keys(id, "<Esc>", &blocks, PrincipalId::system()).unwrap();
        assert_eq!(outcome.state().selection, None);
        assert!(outcome.state().to_json(id)["selection"].is_null());
    }

    #[tokio::test]
    async fn save_clears_dirty_and_moves_the_checkpoint() {
        let (blocks, target) = seeded(b"hello").await;
        let mut sessions = EditorSessions::new();
        let (id, _) = sessions.open(&[RC_PATH], target, &blocks, None).unwrap();

        sessions.keys(id, "iX<Esc>", &blocks, PrincipalId::system()).unwrap();
        let st = sessions.save(id).unwrap();
        assert_eq!(st.text, "Xhello");
        assert!(!st.dirty, "save must clear dirty");
    }

    #[tokio::test]
    async fn quit_rolls_the_block_back_to_the_open_checkpoint() {
        let (blocks, target) = seeded(b"hello").await;
        let mut sessions = EditorSessions::new();
        let (id, _) = sessions.open(&[RC_PATH], target, &blocks, None).unwrap();

        // Delete the first char, mirror lands on the block...
        sessions.keys(id, "x", &blocks, PrincipalId::system()).unwrap();
        assert_eq!(block_text(&blocks, &target).unwrap(), "ello");

        // ...then ZQ restores the block to what we opened.
        sessions.quit(id, &blocks, PrincipalId::system()).unwrap();
        assert_eq!(block_text(&blocks, &target).unwrap(), "hello");
        assert!(!sessions.is_open(id), "quit drops the session");
    }

    #[tokio::test]
    async fn zz_through_keys_saves_and_closes() {
        // The race-free path: a `ZZ` in the key stream (not a separate RPC)
        // checkpoints the edit and drops the session in one shot.
        let (blocks, target) = seeded(b"hello").await;
        let mut sessions = EditorSessions::new();
        let (id, _) = sessions.open(&[RC_PATH], target, &blocks, None).unwrap();

        sessions.keys(id, "iX<Esc>", &blocks, PrincipalId::system()).unwrap(); // -> "Xhello"
        let outcome = sessions.keys(id, "ZZ", &blocks, PrincipalId::system()).unwrap();
        assert!(
            matches!(outcome, KeysOutcome::Closed(_)),
            "ZZ closes the session"
        );
        assert!(!sessions.is_open(id), "ZZ drops the session");
        // ZZ keeps the edit (write+quit): the block holds the inserted text.
        assert_eq!(block_text(&blocks, &target).unwrap(), "Xhello");
    }

    #[tokio::test]
    async fn zq_through_keys_discards_and_closes() {
        let (blocks, target) = seeded(b"hello").await;
        let mut sessions = EditorSessions::new();
        let (id, _) = sessions.open(&[RC_PATH], target, &blocks, None).unwrap();

        sessions.keys(id, "iX<Esc>", &blocks, PrincipalId::system()).unwrap(); // -> "Xhello"
        let outcome = sessions.keys(id, "ZQ", &blocks, PrincipalId::system()).unwrap();
        assert!(
            matches!(outcome, KeysOutcome::Closed(_)),
            "ZQ closes the session"
        );
        assert!(!sessions.is_open(id), "ZQ drops the session");
        // ZQ discards the unsaved edit: the block is back to what we opened.
        assert_eq!(block_text(&blocks, &target).unwrap(), "hello");
    }

    #[tokio::test]
    async fn inserted_zz_is_text_not_close() {
        // An inserted `ZZ` must stay literal — the kernel relies on modalkit's
        // mode state, so this never trips the close path.
        let (blocks, target) = seeded(b"").await;
        let mut sessions = EditorSessions::new();
        let (id, _) = sessions.open(&[RC_PATH], target, &blocks, None).unwrap();

        let outcome = sessions.keys(id, "iZZ", &blocks, PrincipalId::system()).unwrap();
        assert!(matches!(outcome, KeysOutcome::Updated(_)));
        assert!(sessions.is_open(id), "inserted ZZ leaves the session open");
        assert_eq!(block_text(&blocks, &target).unwrap(), "ZZ");
    }

    /// BUG 1 regression (kaibo review of `d45e0484`/`4369bd77`/`f02f3688`):
    /// `quit` used to remove the session from the registry *before*
    /// attempting the rollback, so a rollback failure (`block_text`/
    /// `edit_text` erroring) returned `Err` with the session already gone.
    /// Both `Kernel` callers (`editor_keys_checked`, `editor_quit`)
    /// propagate that `Err` with `?` before ever reaching their `unpin` —
    /// the file-cache pin taken at open leaked for the kernel's lifetime,
    /// with no session left to release it and no way to retry.
    ///
    /// Forces the rollback to fail by deleting the target's document out
    /// from under the session (`block_text`'s `blocks.get(target.context_id)`
    /// then fails) — a `mocked store error` would work the same way, this is
    /// just the simplest failure this pure registry can construct without a
    /// mock. Falsify by reverting to `self.sessions.remove(&id)` at the top
    /// of `quit`, before the rollback.
    #[tokio::test]
    async fn quit_keeps_the_session_when_rollback_fails_instead_of_leaking_it() {
        let (blocks, target) = seeded(b"hello").await;
        let mut sessions = EditorSessions::new();
        let (id, _) = sessions.open(&[RC_PATH], target, &blocks, None).unwrap();

        // Dirty the buffer so the rollback isn't a no-op (current != restore).
        sessions.keys(id, "x", &blocks, PrincipalId::system()).unwrap();

        // The document the session is bound to is gone — `block_text` (the
        // first step of `quit`'s rollback) must now fail.
        blocks.delete_document(target.context_id).unwrap();

        let err = sessions
            .quit(id, &blocks, PrincipalId::system())
            .expect_err("quit must fail when the rollback can't read the block");
        assert!(
            err.contains("not found"),
            "must be the block_text failure, got: {err}"
        );

        // The session must still be open — the P1 fix. The old code removed
        // it unconditionally before attempting the rollback, so a caller
        // holding a pin on this session's target (`Kernel::editor_open_as`)
        // would leak it forever: the session was gone (nothing left to call
        // `unpin`), and `Kernel::editor_quit`/`editor_keys_checked` both
        // propagate this `Err` via `?` before reaching their own `unpin`
        // call. A session that's still open here is a session a caller can
        // still see (`kj vi list`), still holds its rightful pin, and
        // can still be retried or otherwise resolved — not an orphan.
        assert!(
            sessions.is_open(id),
            "a failed rollback must not drop the session out from under an \
             open pin — that is exactly how the pin leaked"
        );
    }

    #[tokio::test]
    async fn quit_rolls_back_to_last_save_not_to_original() {
        let (blocks, target) = seeded(b"hello").await;
        let mut sessions = EditorSessions::new();
        let (id, _) = sessions.open(&[RC_PATH], target, &blocks, None).unwrap();

        sessions.keys(id, "iX<Esc>", &blocks, PrincipalId::system()).unwrap(); // -> "Xhello"
        sessions.save(id).unwrap(); // checkpoint = "Xhello"
        sessions.keys(id, "iY<Esc>", &blocks, PrincipalId::system()).unwrap(); // -> "YXhello"
        sessions.quit(id, &blocks, PrincipalId::system()).unwrap();

        // Rolls back to the *saved* checkpoint, keeping the saved edit.
        assert_eq!(block_text(&blocks, &target).unwrap(), "Xhello");
    }

    #[tokio::test]
    async fn newline_terminated_block_opens_clean_and_quit_preserves_terminator() {
        // modalkit's rope is line-terminated and EditorCore normalizes it away;
        // the session must compare/roll back against the *normalized* view so a
        // newline-terminated block opens clean (not spuriously dirty) and keeps
        // its terminator through a quit-rollback.
        let (blocks, target) = seeded(b"hello\n").await;
        let mut sessions = EditorSessions::new();
        let (id, st) = sessions.open(&[RC_PATH], target, &blocks, None).unwrap();
        assert_eq!(st.text, "hello");
        assert!(!st.dirty, "a newline-terminated block must open clean");

        sessions.keys(id, "iX<Esc>", &blocks, PrincipalId::system()).unwrap();
        assert_eq!(block_text(&blocks, &target).unwrap(), "Xhello\n");

        sessions.quit(id, &blocks, PrincipalId::system()).unwrap();
        assert_eq!(
            block_text(&blocks, &target).unwrap(),
            "hello\n",
            "quit must restore content AND the trailing newline"
        );
    }

    #[tokio::test]
    async fn paste_ending_in_newline_at_the_tail_does_not_double_the_terminator() {
        // A paste whose own text ends in `\n`, landing exactly at the end of a
        // block that already ends in `\n`, must not stack a second terminator
        // (docs/issues.md, "A paste ending in a newline at a newline-terminated
        // block's end doubles the terminator").
        let (blocks, target) = seeded(b"end\n").await;
        let mut sessions = EditorSessions::new();
        let (id, st) = sessions.open(&[RC_PATH], target, &blocks, None).unwrap();
        assert_eq!(st.text, "end");

        sessions.keys(id, "A", &blocks, PrincipalId::system()).unwrap(); // cursor at true end
        let st = sessions
            .insert_at_cursor(id, "X\n", &blocks, PrincipalId::system())
            .unwrap();
        assert_eq!(st.text, "endX");
        assert_eq!(
            block_text(&blocks, &target).unwrap(),
            "endX\n",
            "the paste's own trailing newline becomes the terminator, not a second one"
        );
    }

    #[tokio::test]
    async fn the_block_always_equals_the_buffer_plus_its_terminator() {
        // `mirror_ops` splices normalized offsets onto raw text, so the one
        // invariant that keeps the two surfaces from drifting is
        // `raw == core.text() + terminator`. Check it across the three tail
        // cases: a paste that supplies its own terminator, a paste that does
        // not, and a typed `Enter`.
        for keys in ["X\n", "X"] {
            let (blocks, target) = seeded(b"end\n").await;
            let mut sessions = EditorSessions::new();
            let (id, _) = sessions.open(&[RC_PATH], target, &blocks, None).unwrap();
            sessions.keys(id, "A", &blocks, PrincipalId::system()).unwrap();
            let st = sessions
                .insert_at_cursor(id, keys, &blocks, PrincipalId::system())
                .unwrap();
            let terminator = sessions.terminator_for_test(id);
            assert_eq!(
                block_text(&blocks, &target).unwrap(),
                format!("{}{terminator}", st.text),
                "paste {keys:?} left the block and the buffer disagreeing"
            );
        }

        let (blocks, target) = seeded(b"end\n").await;
        let mut sessions = EditorSessions::new();
        let (id, _) = sessions.open(&[RC_PATH], target, &blocks, None).unwrap();
        let outcome = sessions
            .keys(id, "A<CR><Esc>", &blocks, PrincipalId::system())
            .unwrap();
        let text = outcome.state().text.clone();
        let terminator = sessions.terminator_for_test(id);
        assert_eq!(
            block_text(&blocks, &target).unwrap(),
            format!("{text}{terminator}"),
            "a typed Enter left the block and the buffer disagreeing"
        );
    }

    #[tokio::test]
    async fn paste_with_no_trailing_newline_at_the_tail_keeps_the_terminator() {
        // The other side of the same fix: an insert that does not supply its
        // own terminator must not eat the block's existing one.
        let (blocks, target) = seeded(b"end\n").await;
        let mut sessions = EditorSessions::new();
        let (id, _) = sessions.open(&[RC_PATH], target, &blocks, None).unwrap();

        sessions.keys(id, "A", &blocks, PrincipalId::system()).unwrap();
        sessions
            .insert_at_cursor(id, "X", &blocks, PrincipalId::system())
            .unwrap();
        assert_eq!(block_text(&blocks, &target).unwrap(), "endX\n");
    }

    #[tokio::test]
    async fn a_paste_away_from_the_tail_keeps_a_trailing_blank_line() {
        // The terminator reconcile must only fire for an edit that reached the
        // end of the buffer and supplied a terminator of its own. A block
        // already ending in a blank line keeps it when the paste lands
        // elsewhere, even though `insert_at` round-trips through the buffer's
        // lossy normalized text and drops one newline from its own view.
        let (blocks, target) = seeded(b"hello\n\n").await;
        let mut sessions = EditorSessions::new();
        let (id, _) = sessions.open(&[RC_PATH], target, &blocks, None).unwrap();
        sessions
            .insert_at_cursor(id, "X", &blocks, PrincipalId::system())
            .unwrap();
        assert_eq!(
            block_text(&blocks, &target).unwrap(),
            "Xhello\n\n",
            "a paste at the head must not cost the block a trailing newline"
        );
    }

    #[tokio::test]
    async fn typed_enter_at_the_tail_adds_a_line() {
        // A paste whose text ends in the terminator supplies the block's
        // existing one; a typed Enter asks for a line break, so it grows the
        // block. The two are different intents and the mirror must not
        // conflate them.
        let (blocks, target) = seeded(b"end\n").await;
        let mut sessions = EditorSessions::new();
        let (id, _) = sessions.open(&[RC_PATH], target, &blocks, None).unwrap();
        sessions
            .keys(id, "AX<CR><Esc>", &blocks, PrincipalId::system())
            .unwrap();
        assert_eq!(
            block_text(&blocks, &target).unwrap(),
            "endX\n\n",
            "a typed Enter at the tail breaks the line rather than reusing the terminator"
        );
    }

    #[tokio::test]
    async fn reconcile_skips_self_write_and_merges_a_sibling() {
        // Two sessions on one block: when A writes (mirroring onto the block),
        // reconcile_block must SKIP A (its buffer already matches — the mirror
        // is faithful) and MERGE the stale sibling B, reporting only B.
        let (blocks, target) = seeded(b"hello").await;
        let mut sessions = EditorSessions::new();
        let (a, _) = sessions.open(&[RC_PATH], target, &blocks, None).unwrap();
        let (b, _) = sessions.open(&[RC_PATH], target, &blocks, None).unwrap();

        sessions.keys(a, "iX<Esc>", &blocks, PrincipalId::system()).unwrap();
        assert_eq!(block_text(&blocks, &target).unwrap(), "Xhello");

        let changed = sessions.reconcile_block(target.context_id, target.block_id, &blocks);
        assert_eq!(changed.len(), 1, "only the stale sibling reconciles");
        assert_eq!(changed[0].0, b, "it is session B that moved");
        assert_eq!(changed[0].1.text, "Xhello", "B merged A's edit");
        assert!(changed[0].1.dirty, "B now differs from its open checkpoint");

        // Idempotent: a second reconcile against the unchanged block is a no-op
        // for everyone (both buffers now match the block).
        let again = sessions.reconcile_block(target.context_id, target.block_id, &blocks);
        assert!(again.is_empty(), "nothing stale → no reconcile");
    }

    // ── Entanglement guard: detach, don't retract (shared-session ZQ) ────────

    #[tokio::test]
    async fn zq_with_a_live_sibling_detaches_without_rollback() {
        // Amy's shared-session semantics (2026-07-07): ZQ quits THIS player out
        // of the doc and leaves the others going. A's own unsaved edit stays in
        // the merged truth — no rollback yanks the block out from under B.
        let (blocks, target) = seeded(b"hello").await;
        let mut sessions = EditorSessions::new();
        let (a, _) = sessions.open(&[RC_PATH], target, &blocks, None).unwrap();
        let (b, _) = sessions.open(&[RC_PATH], target, &blocks, None).unwrap();

        sessions.keys(a, "iX<Esc>", &blocks, PrincipalId::system()).unwrap(); // -> "Xhello"
        let outcome = sessions.keys(a, ":q!<CR>", &blocks, PrincipalId::system()).unwrap();
        assert!(matches!(outcome, KeysOutcome::Closed(_)), ":q! closes A");
        assert!(!sessions.is_open(a), "A is out of the doc");
        assert!(sessions.is_open(b), "B keeps playing");
        assert_eq!(
            block_text(&blocks, &target).unwrap(),
            "Xhello",
            "a shared-session quit must NOT roll the block back"
        );
    }

    #[tokio::test]
    async fn zq_after_a_departed_peers_write_keeps_their_work() {
        // The clobber scenario: B edits, saves (ZZ), and LEAVES; A then ZQs.
        // No sibling is present, but B's write merged into A (peer_wrote), so
        // the rollback is skipped — B's saved work survives A's abort.
        let (blocks, target) = seeded(b"hello").await;
        let mut sessions = EditorSessions::new();
        let (a, _) = sessions.open(&[RC_PATH], target, &blocks, None).unwrap();
        let (b, _) = sessions.open(&[RC_PATH], target, &blocks, None).unwrap();

        sessions.keys(b, "iB<Esc>", &blocks, PrincipalId::system()).unwrap(); // -> "Bhello"
        // The server's reconciler drives this off the block flow; simulate it.
        sessions.reconcile_block(target.context_id, target.block_id, &blocks);
        let outcome = sessions.keys(b, "ZZ", &blocks, PrincipalId::system()).unwrap(); // B saves + leaves
        assert!(matches!(outcome, KeysOutcome::Closed(_)));

        sessions.quit(a, &blocks, PrincipalId::system()).unwrap(); // A aborts
        assert_eq!(
            block_text(&blocks, &target).unwrap(),
            "Bhello",
            "A's ZQ must not erase the departed peer's saved work"
        );
    }

    #[tokio::test]
    async fn save_resets_entanglement_so_a_later_solo_zq_rolls_back_to_it() {
        // A's save folds B's merged work into A's checkpoint; once B is gone
        // and A is alone again, ZQ reverts only A's post-save edits — landing
        // on the checkpoint that already contains B's work.
        let (blocks, target) = seeded(b"hello").await;
        let mut sessions = EditorSessions::new();
        let (a, _) = sessions.open(&[RC_PATH], target, &blocks, None).unwrap();
        let (b, _) = sessions.open(&[RC_PATH], target, &blocks, None).unwrap();

        sessions.keys(b, "iB<Esc>", &blocks, PrincipalId::system()).unwrap(); // -> "Bhello"
        sessions.reconcile_block(target.context_id, target.block_id, &blocks);
        sessions.save(a).unwrap(); // A's checkpoint = "Bhello"; entanglement resets
        sessions.keys(b, "ZZ", &blocks, PrincipalId::system()).unwrap(); // B leaves

        sessions.keys(a, "iA<Esc>", &blocks, PrincipalId::system()).unwrap(); // -> "ABhello"
        sessions.quit(a, &blocks, PrincipalId::system()).unwrap(); // alone + untangled → real rollback
        assert_eq!(
            block_text(&blocks, &target).unwrap(),
            "Bhello",
            "solo ZQ reverts to A's checkpoint, which keeps B's merged work"
        );
    }

    #[tokio::test]
    async fn reconcile_with_no_bound_session_is_a_noop() {
        let (blocks, target) = seeded(b"hello").await;
        let mut sessions = EditorSessions::new();
        // No editor open on this block — the hot path must do nothing.
        let changed = sessions.reconcile_block(target.context_id, target.block_id, &blocks);
        assert!(changed.is_empty());
    }

    #[tokio::test]
    async fn keys_on_a_dropped_session_fails_loud() {
        let (blocks, target) = seeded(b"hello").await;
        let mut sessions = EditorSessions::new();
        let (id, _) = sessions.open(&[RC_PATH], target, &blocks, None).unwrap();
        sessions.quit(id, &blocks, PrincipalId::system()).unwrap();
        let err = sessions.keys(id, "x", &blocks, PrincipalId::system()).unwrap_err();
        assert!(err.contains("no such session"), "got: {err}");
    }

    // ── `:` command mode (Slice 3) ───────────────────────────────────────────

    #[tokio::test]
    async fn colon_wq_saves_and_closes() {
        // `:wq` is the muscle-memory twin of `ZZ` — save the edit, drop the
        // session, keep the change on the block.
        let (blocks, target) = seeded(b"hello").await;
        let mut sessions = EditorSessions::new();
        let (id, _) = sessions.open(&[RC_PATH], target, &blocks, None).unwrap();

        sessions.keys(id, "iX<Esc>", &blocks, PrincipalId::system()).unwrap(); // -> "Xhello"
        let outcome = sessions.keys(id, ":wq<CR>", &blocks, PrincipalId::system()).unwrap();
        assert!(
            matches!(outcome, KeysOutcome::Closed(_)),
            ":wq closes the session"
        );
        assert!(!sessions.is_open(id), ":wq drops the session");
        assert_eq!(block_text(&blocks, &target).unwrap(), "Xhello");
    }

    #[tokio::test]
    async fn colon_q_refuses_a_dirty_buffer_on_the_status_line() {
        // Plain `:q` on unsaved changes refuses (vim's E37 "No write since last
        // change") — reported on the STATUS LINE like E492, not as an RPC error:
        // a hard error never reaches the GUI, and with no state push the
        // renderer's `:`-strip kept showing the stale submitted `:q`.
        let (blocks, target) = seeded(b"hello").await;
        let mut sessions = EditorSessions::new();
        let (id, _) = sessions.open(&[RC_PATH], target, &blocks, None).unwrap();

        sessions.keys(id, "iX<Esc>", &blocks, PrincipalId::system()).unwrap(); // dirty
        let outcome = sessions.keys(id, ":q<CR>", &blocks, PrincipalId::system()).unwrap();
        assert!(
            matches!(outcome, KeysOutcome::Updated(_)),
            "the refusal keeps the session open, not an error"
        );
        let msg = outcome
            .state()
            .message
            .as_deref()
            .expect("the refusal reports on the status line");
        assert!(msg.contains("No write since last change"), "got: {msg}");
        assert!(sessions.is_open(id), ":q must not drop a dirty session");
        assert!(
            outcome.state().command_line.is_none(),
            "the submitted :q line is gone — the strip shows the message instead"
        );

        // The message is transient and `:q!` still gets you out.
        let outcome = sessions.keys(id, "l", &blocks, PrincipalId::system()).unwrap();
        assert!(outcome.state().message.is_none(), "message clears on the next batch");
        let outcome = sessions.keys(id, ":q!<CR>", &blocks, PrincipalId::system()).unwrap();
        assert!(matches!(outcome, KeysOutcome::Closed(_)));
    }

    #[tokio::test]
    async fn colon_q_bang_discards_and_closes() {
        let (blocks, target) = seeded(b"hello").await;
        let mut sessions = EditorSessions::new();
        let (id, _) = sessions.open(&[RC_PATH], target, &blocks, None).unwrap();

        sessions.keys(id, "iX<Esc>", &blocks, PrincipalId::system()).unwrap(); // -> "Xhello"
        let outcome = sessions.keys(id, ":q!<CR>", &blocks, PrincipalId::system()).unwrap();
        assert!(matches!(outcome, KeysOutcome::Closed(_)));
        assert!(!sessions.is_open(id));
        // Forced quit rolls back to the open checkpoint.
        assert_eq!(block_text(&blocks, &target).unwrap(), "hello");
    }

    #[tokio::test]
    async fn colon_w_defers_the_checkpoint_to_the_kernel_layer() {
        // At this pure `EditorSessions` level, `:w` mirrors the edit and
        // reports `saved: true` (the kernel layer's cue to flush), but does
        // NOT itself advance the checkpoint — that happens only once the
        // kernel layer's flush to disk (`Kernel::editor_keys`) actually
        // lands (docs/file-buffers.md). `save` is what a successful flush
        // calls to clear dirty.
        let (blocks, target) = seeded(b"hello").await;
        let mut sessions = EditorSessions::new();
        let (id, _) = sessions.open(&[RC_PATH], target, &blocks, None).unwrap();

        sessions.keys(id, "iX<Esc>", &blocks, PrincipalId::system()).unwrap();
        let outcome = sessions.keys(id, ":w<CR>", &blocks, PrincipalId::system()).unwrap();
        assert!(
            matches!(outcome, KeysOutcome::Updated(_)),
            ":w keeps the session open"
        );
        assert!(outcome.saved(), ":w reports a write intent");
        assert!(
            outcome.state().dirty,
            "the checkpoint has not advanced yet — that's the kernel layer's job"
        );
        assert!(sessions.is_open(id));

        let st = sessions.save(id).unwrap();
        assert!(!st.dirty, "save (the kernel layer's post-flush call) clears dirty");
        // A clean `:q` now succeeds.
        let outcome = sessions.keys(id, ":q<CR>", &blocks, PrincipalId::system()).unwrap();
        assert!(matches!(outcome, KeysOutcome::Closed(_)));
        assert_eq!(block_text(&blocks, &target).unwrap(), "Xhello");
    }

    #[tokio::test]
    async fn unknown_colon_command_reports_on_the_status_line() {
        // vim's E492: an unknown `:command` shows on the status line and the
        // session stays put — it does NOT error `editor_keys` (which the front
        // door would surface as a hard failure, popping the editor).
        let (blocks, target) = seeded(b"hello").await;
        let mut sessions = EditorSessions::new();
        let (id, _) = sessions.open(&[RC_PATH], target, &blocks, None).unwrap();

        let outcome = sessions.keys(id, ":frobnicate<CR>", &blocks, PrincipalId::system()).unwrap();
        assert!(
            matches!(outcome, KeysOutcome::Updated(_)),
            "a bad command keeps the session open"
        );
        let msg = outcome
            .state()
            .message
            .as_deref()
            .expect("a status message is set");
        assert!(msg.contains("Not an editor command"), "got: {msg}");
        assert!(sessions.is_open(id), "a bad command leaves the session open");
        // The buffer is untouched (the bad command edited nothing).
        assert_eq!(block_text(&blocks, &target).unwrap(), "hello");

        // The message clears on the next keystroke batch (vim-ish transience).
        let outcome = sessions.keys(id, "l", &blocks, PrincipalId::system()).unwrap();
        assert!(
            outcome.state().message.is_none(),
            "the status message clears on the next keystroke"
        );
    }

    #[tokio::test]
    async fn colon_s_substitutes_onto_the_block() {
        // `:s` is an edit — it must mirror onto the owning kernel block like any
        // keystroke, so a `cat`/exec of the path sees the substituted text.
        let (blocks, target) = seeded(b"alpha beta alpha").await;
        let mut sessions = EditorSessions::new();
        let (id, _) = sessions.open(&[RC_PATH], target, &blocks, None).unwrap();

        let outcome = sessions.keys(id, ":s/alpha/ALPHA/g<CR>", &blocks, PrincipalId::system()).unwrap();
        assert_eq!(outcome.state().text, "ALPHA beta ALPHA");
        assert!(outcome.state().dirty, "a substitution dirties the buffer");
        // The invariant: the block equals the edited buffer.
        assert_eq!(block_text(&blocks, &target).unwrap(), "ALPHA beta ALPHA");
    }

    #[tokio::test]
    async fn colon_percent_s_then_wq_persists() {
        let (blocks, target) = seeded(b"x y\nx y").await;
        let mut sessions = EditorSessions::new();
        let (id, _) = sessions.open(&[RC_PATH], target, &blocks, None).unwrap();

        sessions.keys(id, ":%s/x/Z/g<CR>", &blocks, PrincipalId::system()).unwrap();
        let outcome = sessions.keys(id, ":wq<CR>", &blocks, PrincipalId::system()).unwrap();
        assert!(matches!(outcome, KeysOutcome::Closed(_)), ":wq closes");
        // The substitution survived the save+close.
        assert_eq!(block_text(&blocks, &target).unwrap(), "Z y\nZ y");
    }

    #[tokio::test]
    async fn colon_s_then_q_bang_rolls_back() {
        // A substitution followed by `:q!` discards it (rollback to checkpoint).
        let (blocks, target) = seeded(b"keep me").await;
        let mut sessions = EditorSessions::new();
        let (id, _) = sessions.open(&[RC_PATH], target, &blocks, None).unwrap();

        sessions.keys(id, ":s/keep/DROP/<CR>", &blocks, PrincipalId::system()).unwrap();
        assert_eq!(block_text(&blocks, &target).unwrap(), "DROP me");
        sessions.keys(id, ":q!<CR>", &blocks, PrincipalId::system()).unwrap();
        assert_eq!(block_text(&blocks, &target).unwrap(), "keep me", ":q! discards :s");
    }

    #[tokio::test]
    async fn bad_substitute_pattern_reports_on_the_status_line_and_leaves_block_clean() {
        // A bad `:s` regex arrives on the same `Some(Err)` channel as an unknown
        // command, so it too reports on the status line and keeps the session
        // open — the block is left untouched, no silent edit.
        let (blocks, target) = seeded(b"hello").await;
        let mut sessions = EditorSessions::new();
        let (id, _) = sessions.open(&[RC_PATH], target, &blocks, None).unwrap();

        let outcome = sessions.keys(id, ":s/[/x/<CR>", &blocks, PrincipalId::system()).unwrap();
        let msg = outcome
            .state()
            .message
            .as_deref()
            .expect("a status message is set");
        assert!(msg.contains("invalid :s pattern"), "got: {msg}");
        assert_eq!(block_text(&blocks, &target).unwrap(), "hello", "no edit on a bad pattern");
        assert!(sessions.is_open(id), "a bad :s leaves the session open");
    }

    #[tokio::test]
    async fn command_line_text_rides_the_state() {
        // While typing the `:`-line, the pushed state carries it so a renderer
        // can draw the strip without tracking mode.
        let (blocks, target) = seeded(b"hello").await;
        let mut sessions = EditorSessions::new();
        let (id, _) = sessions.open(&[RC_PATH], target, &blocks, None).unwrap();

        let outcome = sessions.keys(id, ":w", &blocks, PrincipalId::system()).unwrap();
        assert_eq!(
            outcome.state().command_line.as_deref(),
            Some(":w"),
            "the in-progress command line surfaces on the state"
        );
    }

    #[tokio::test]
    async fn latest_session_for_finds_the_most_recent_per_principal() {
        // `fg` resumes the caller's most-recently-opened session (job-control
        // "most recent"); a principal with no editor gets None.
        let (blocks, target) = seeded(b"hello").await;
        let mut sessions = EditorSessions::new();
        let me = PrincipalId::system();
        let other = PrincipalId::beat();
        // `fg` keys on the opener's principal; context/session are irrelevant here.
        let as_opener = |p: PrincipalId| {
            Some(EditorOpener {
                principal: p, performer: p, reviewer: None,
                context_id: ContextId::new(),
                session_id: SessionId::new(),
            })
        };

        let (_a, _) = sessions.open(&[RC_PATH], target, &blocks, as_opener(me)).unwrap();
        let (b, _) = sessions.open(&[RC_PATH], target, &blocks, as_opener(me)).unwrap();

        assert_eq!(
            sessions.latest_session_for(me).map(|(id, _)| id),
            Some(b),
            "the highest (most recent) session id for the principal"
        );
        assert_eq!(
            sessions.latest_session_for(other),
            None,
            "a principal with no editor has nothing to foreground"
        );
        // An opener-less (headless) session is owned by no principal.
        let (_c, _) = sessions.open(&[RC_PATH], target, &blocks, None).unwrap();
        assert_eq!(
            sessions.latest_session_for(me).map(|(id, _)| id),
            Some(b),
            "a None-opener session doesn't become anyone's fg target"
        );
    }

    #[tokio::test]
    async fn list_censuses_every_open_session_by_id_dirty_and_opener() {
        // `kj vi list`'s data source: a leaked session (an opener who
        // walked away) must show up here, since there is no other way to see
        // it. Two sessions on the same block, only one dirtied — and without
        // a reconcile the sibling's own buffer stays untouched (that's the
        // reconcile-is-a-separate-step invariant `reconcile_skips_self_write_
        // and_merges_a_sibling` covers; here we're only checking `list()`).
        let (blocks, target) = seeded(b"hello").await;
        let mut sessions = EditorSessions::new();
        let (a, _) = sessions.open(&[RC_PATH], target, &blocks, None).unwrap();
        let (b, _) = sessions.open(&[RC_PATH], target, &blocks, None).unwrap();

        sessions.keys(a, "iX<Esc>", &blocks, PrincipalId::system()).unwrap(); // -> "Xhello", dirties A only

        let list = sessions.list();
        assert_eq!(list.len(), 2, "both sessions are open");
        assert_eq!(list[0].session, a.as_u64(), "sorted by session id ascending");
        assert_eq!(list[1].session, b.as_u64());
        assert_eq!(list[0].path, RC_PATH);
        assert_eq!(list[1].path, RC_PATH);
        assert!(list[0].dirty, "A diverged from its checkpoint");
        assert!(
            !list[1].dirty,
            "B's stale buffer hasn't been reconciled, but it hasn't been \
             touched either — dirty compares B's own buffer to B's own \
             checkpoint, both still \"hello\""
        );

        sessions.quit(a, &blocks, PrincipalId::system()).unwrap();
        let list = sessions.list();
        assert_eq!(list.len(), 1, "quitting A leaves only B");
        assert_eq!(list[0].session, b.as_u64());

        // Opener round-trips into the record; a headless (None) session
        // reports no opener.
        assert!(list[0].opener.is_none(), "B was opened headless");
        let opener = Some(EditorOpener {
            principal: PrincipalId::system(), performer: PrincipalId::system(), reviewer: None,
            context_id: ContextId::new(),
            session_id: SessionId::new(),
        });
        let (c, _) = sessions.open(&[RC_PATH], target, &blocks, opener).unwrap();
        let c_info = sessions
            .list()
            .into_iter()
            .find(|s| s.session == c.as_u64())
            .expect("the just-opened session is in the census");
        assert!(
            c_info.opener.is_some(),
            "an opened-with-opener session reports one"
        );
    }

    #[tokio::test]
    async fn open_on_a_path_with_an_existing_session_announces_it() {
        // Amy, 2026-08-19: "like vim it should detect that and tell me, so I
        // can go back to the other one or shut it down." The second open still
        // succeeds — two players on one block is a supported state
        // (docs/vi.md) — but it must tell the caller another session already
        // holds the target.
        let (blocks, target) = seeded(b"hello").await;
        let mut sessions = EditorSessions::new();
        let (_first, st1) = sessions.open(&[RC_PATH], target, &blocks, None).unwrap();
        assert!(st1.message.is_none(), "the first open has nothing to announce");

        let (_second, st2) = sessions.open(&[RC_PATH], target, &blocks, None).unwrap();
        assert!(
            st2.message.is_some(),
            "opening a path that already has a session must announce it"
        );
    }

    /// A player's registers outlive one session: `dd` in one live session
    /// and `p` in another put the line, both ways, while both stay open.
    #[tokio::test]
    async fn a_players_sessions_share_registers_while_both_are_open() {
        let (first_blocks, first_target) = seeded(b"one\ntwo").await;
        let (second_blocks, second_target) = seeded(b"alpha").await;
        let amy = PrincipalId::new();
        let mut sessions = EditorSessions::new();
        let (first, _) = sessions.open(&[RC_PATH], first_target, &first_blocks, None).unwrap();
        let (second, _) = sessions.open(&[RC_PATH], second_target, &second_blocks, None).unwrap();

        sessions.keys(first, "dd", &first_blocks, amy).unwrap();
        let outcome = sessions.keys(second, "p", &second_blocks, amy).unwrap();
        assert_eq!(outcome.state().text, "alpha\none", "the second session puts the first's delete");

        sessions.keys(second, "ggyy", &second_blocks, amy).unwrap();
        let outcome = sessions.keys(first, "p", &first_blocks, amy).unwrap();
        assert_eq!(outcome.state().text, "two\nalpha", "the first session puts the second's yank");
    }

    /// Registers are keyed by the player who typed: another player's
    /// session neither reads nor overwrites them.
    #[tokio::test]
    async fn another_players_session_never_touches_the_registers() {
        let (first_blocks, first_target) = seeded(b"one\ntwo").await;
        let (second_blocks, second_target) = seeded(b"alpha").await;
        let (amy, model) = (PrincipalId::new(), PrincipalId::new());
        let mut sessions = EditorSessions::new();
        let (first, _) = sessions.open(&[RC_PATH], first_target, &first_blocks, None).unwrap();
        let (second, _) = sessions.open(&[RC_PATH], second_target, &second_blocks, None).unwrap();

        sessions.keys(first, "dd", &first_blocks, amy).unwrap();
        let outcome = sessions.keys(second, "p", &second_blocks, model).unwrap();
        assert_eq!(outcome.state().text, "alpha", "the model has nothing to put");

        sessions.keys(second, "yy", &second_blocks, model).unwrap();
        let outcome = sessions.keys(first, "p", &first_blocks, amy).unwrap();
        assert_eq!(outcome.state().text, "two\none", "the model's yank left Amy's register alone");
    }
}
