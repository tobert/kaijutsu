//! In-app editor: the kernel-owned editing surface (the `vi`/`edit` builtin +
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

/// The location an editor binds to: the context + block that own a path's
/// text. Edits go to `block_store.edit_text(context_id, block_id, …)`.
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

use kaijutsu_editor::{CloseRequest, CommandRequest, EditorCore, EditorIo};

/// A one-line census entry for an open session — what `kj editor list` shows.
/// Full ids (never truncated), matching the `.data` convention every other
/// `kj` list surface uses (`kj block list`, `kj doc list`, …) so a driver can
/// round-trip these back into other kj commands without re-resolving.
#[derive(Debug, Clone, serde::Serialize)]
pub struct EditorSessionInfo {
    pub session: u64,
    pub path: String,
    /// Full id string of the target document (`ContextId::to_hex`).
    pub context_id: String,
    /// Full id string of the target block (`BlockId::to_key`).
    pub block_id: String,
    /// Whether the buffer differs from the last open/save checkpoint.
    pub dirty: bool,
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
/// kernel layer must flush to disk first and only then checkpoint, or the
/// buffer would read clean before the bytes land (docs/file-buffers.md).
///
/// `forced` carries `:w!`'s bang through to the kernel layer's W12
/// changed-under-us guard (`FileDocumentCache::flush_one_guarded`,
/// docs/file-buffers.md): a plain `:w` (`forced: false`) refuses when disk
/// moved under the buffer since it was loaded; `:w!` (`forced: true`)
/// overrides that refusal. This pure registry has no file cache to check
/// against, so it only carries the bit — the kernel layer is what acts on it.
///
/// `rolled_back` mirrors [`EditorSessions::quit`]'s return: `true` when a
/// `ZQ`/`:q!`-style discard in this batch actually rewrote the block (the
/// buffer held unsaved changes at the moment of quit), `false` for a `ZZ`
/// close (which checkpoints before quitting, so quit's rewrite is always a
/// no-op) and for every `Updated` outcome (no quit ran). The kernel layer
/// marks a file-backed entry dirty whenever this is `true`, on top of
/// `saved` — a discard's rollback writes the block via a raw `edit_text`
/// call the file cache's own flush pipeline never sees, so the block can end
/// up disagreeing with disk with nothing tracking it (docs/file-buffers.md).
#[derive(Debug)]
pub struct KeysUpdate {
    pub state: EditorState,
    pub saved: bool,
    pub forced: bool,
    pub rolled_back: bool,
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
    /// The raw handle value — the currency the `kj editor` / wire surface uses.
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
}

impl EditorState {
    /// Structured `.data` for one session, stamped with its handle — the single
    /// shape every editor front door emits (`kj editor`, the `vi`/`edit`
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
        })
    }
}

/// Who opened a session, and the shell context they opened from — captured at
/// the front door (`vi`/`edit`, `kj editor`) and recorded on the
/// session. Two consumers:
///
/// - **`fg`** re-foregrounds the caller's most-recent session by `principal`.
/// - **`:r !cmd`** materializes a kaish in `(principal, context_id, session_id)`
///   so the command runs in the *opener's* working context and capability
///   allow-set — not the edited block's context.
///
/// `None` for a headless open (a test driver, the wire `editorOpen` handler):
/// nobody to foreground, and `:r !cmd` then fails loud rather than guessing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EditorOpener {
    pub principal: PrincipalId,
    pub context_id: ContextId,
    pub session_id: SessionId,
}

/// One open editor: a pure [`EditorCore`] bound to the kernel block that owns the
/// text, plus the rollback checkpoint.
struct EditorSession {
    core: EditorCore,
    target: EditorTarget,
    /// The path the editor was opened on (as the caller named it — e.g. a
    /// `coder/*` symlink, before resolution). Used to invalidate the shared
    /// `FileDocumentCache` after a write so a kaish `cat` of this path re-reads
    /// the just-edited block instead of a stale shadow copy.
    path: String,
    /// Normalized (terminator-stripped) content as of the last open/save — the
    /// dirty/`ZQ` checkpoint. Matches `EditorCore`'s normalized view so dirty
    /// compares like-to-like (a newline-terminated block opens clean).
    saved_content: String,
    /// The block's trailing terminator (`"\n"` or `""`) captured at open.
    /// `EditorCore` strips modalkit's line terminator, so the terminator lives
    /// here; edits mirror as diffs (never touching it) and `ZQ` re-applies it.
    terminator: String,
    /// Who opened the session + the context they opened from ([`EditorOpener`]).
    /// `fg` finds the caller's suspended editor by `principal`; `:r !cmd` runs in
    /// the opener's `(principal, context_id, session_id)`. `None` for a headless
    /// open (test / wire `editorOpen` — no caller to capture).
    opener: Option<EditorOpener>,
    /// Whether a **peer's** write merged into this session since the last
    /// open/save checkpoint — set by [`EditorSessions::reconcile_block`] (a
    /// non-self merge), reset when the checkpoint advances ([`EditorSessions::save`]).
    /// A `ZQ` rollback to the checkpoint would revert that peer work along with
    /// ours, so [`EditorSessions::quit`] skips the rollback when this is set:
    /// detach, don't retract (see the entanglement guard in `docs/vi.md`).
    peer_wrote: bool,
}

/// The kernel's registry of open editor sessions.
///
/// Every operation is **synchronous**: the `EditorCore` (which is `!Send` via
/// modalkit) never crosses an `await`. The only async step — resolving a path —
/// happens *before* [`open`](Self::open), not inside it. So once wired into the
/// shared kernel this registry can live behind a plain mutex.
#[derive(Default)]
pub struct EditorSessions {
    next_id: u64,
    sessions: HashMap<EditorSessionId, EditorSession>,
}

impl EditorSessions {
    pub fn new() -> Self {
        Self::default()
    }

    /// Open an editor on a *pre-resolved* target (resolve with
    /// [`resolve_editor_target`] first — the only async step). The block's
    /// current text becomes the initial buffer and the rollback checkpoint.
    pub fn open(
        &mut self,
        path: &str,
        target: EditorTarget,
        blocks: &SharedBlockStore,
        opener: Option<EditorOpener>,
    ) -> Result<(EditorSessionId, EditorState), String> {
        let raw = block_text(blocks, &target)?;
        let mut core = EditorCore::new(&raw);
        // EditorCore strips modalkit's terminator; keep the block's own
        // terminator aside so dirty/rollback compare against the normalized view.
        let terminator = if raw.ends_with('\n') { "\n" } else { "" }.to_string();
        let saved_content = core.text();
        let state = state_of(&mut core, &saved_content);
        let id = EditorSessionId(self.next_id);
        self.next_id += 1;
        self.sessions.insert(
            id,
            EditorSession {
                core,
                target,
                path: path.to_string(),
                saved_content,
                terminator,
                opener,
                peer_wrote: false,
            },
        );
        Ok((id, state))
    }

    /// The most-recently-opened session owned by `principal` (the highest id,
    /// since ids increment monotonically) and the path it edits — what `fg`
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
            .map(|(id, s)| (*id, s.path.clone()))
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
            .map(|(id, s)| (*id, s.path.clone()))
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
    ) -> Result<KeysOutcome, String> {
        self.keys_checked(id, keys, blocks, true)
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
        can_write: bool,
    ) -> Result<KeysOutcome, String> {
        let (close, commands) = {
            let session = self.sessions.get_mut(&id).ok_or_else(|| no_session(id))?;
            let ops = session.core.apply_keys(keys);
            for op in &ops {
                blocks
                    .edit_text(
                        session.target.context_id,
                        &session.target.block_id,
                        op.offset,
                        &op.insert,
                        op.delete,
                    )
                    .map_err(|e| format!("editor keys: block mirror failed: {e}"))?;
            }
            (session.core.take_close(), session.core.take_commands())
        };

        // `ZZ`/`ZQ` close the session. The returned state is informational (the
        // last view before close); renderers react to the `Closed` push.
        if let Some(close) = close {
            let (final_state, saved, rolled_back) = match close {
                CloseRequest::Write => {
                    if !can_write {
                        return self.refuse_write(id);
                    }
                    // ZZ: checkpoint current as saved, then quit — the rollback
                    // to that just-taken checkpoint is a no-op. `saved: true`
                    // tells the kernel layer to flush a file-backed session.
                    let state = self.save(id)?;
                    let rolled_back = self.quit(id, blocks)?;
                    (state, true, rolled_back)
                }
                CloseRequest::Discard => {
                    // ZQ: snapshot the view, then roll back to the last
                    // checkpoint. Nothing to flush — the discard never advanced
                    // the checkpoint. `rolled_back` tells the kernel layer
                    // whether that rollback actually rewrote the block.
                    let state = self.state(id)?;
                    let rolled_back = self.quit(id, blocks)?;
                    (state, false, rolled_back)
                }
            };
            return Ok(KeysOutcome::Closed(KeysUpdate {
                state: final_state,
                saved,
                forced: false,
                rolled_back,
            }));
        }

        // A submitted `:`-line (`:w`/`:wq`/`:q!`/…). A parsed batch runs in
        // order; an unknown command or a bad `:s` regex (both arrive as
        // `Some(Err)`) reports vim-style on the status line and keeps the session
        // open — never errors the whole `editor_keys` call out from under the
        // renderer (the front door would otherwise surface it as a hard failure).
        if let Some(parsed) = commands {
            match parsed {
                Ok(cmds) => return self.run_commands(id, cmds, blocks, can_write),
                Err(msg) => {
                    let session = self.sessions.get_mut(&id).ok_or_else(|| no_session(id))?;
                    let checkpoint = session.saved_content.clone();
                    let mut state = state_of(&mut session.core, &checkpoint);
                    state.message = Some(msg);
                    return Ok(KeysOutcome::Updated(KeysUpdate {
                        state,
                        saved: false,
                        forced: false,
                        rolled_back: false,
                    }));
                }
            }
        }

        let session = self.sessions.get_mut(&id).ok_or_else(|| no_session(id))?;
        let checkpoint = session.saved_content.clone();
        Ok(KeysOutcome::Updated(KeysUpdate {
            state: state_of(&mut session.core, &checkpoint),
            saved: false,
            forced: false,
            rolled_back: false,
        }))
    }

    /// A write intent refused for lacking `Capability::Editor` — shared by
    /// [`keys_checked`](Self::keys_checked)'s `ZZ` arm and
    /// [`run_commands`](Self::run_commands)'s `Write`/`Quit` arm, so the
    /// outcome shape and message are identical everywhere a refusal can fire.
    /// Nothing is mutated: no checkpoint, no quit, no flush — the session is
    /// exactly as it was before this batch's write intent.
    fn refuse_write(&mut self, id: EditorSessionId) -> Result<KeysOutcome, String> {
        let mut state = self.state(id)?;
        state.message = Some(WRITE_CAPABILITY_REFUSED.to_string());
        Ok(KeysOutcome::Updated(KeysUpdate {
            state,
            saved: false,
            forced: false,
            rolled_back: false,
        }))
    }

    /// Act on a parsed `:`-command batch (`docs/vi.md` → *Command mode*). `Quit`
    /// closes — refusing a dirty buffer without `!` (vim's E37 "No write since
    /// last change", reported on the status line like every dialect-level
    /// failure). Returns [`KeysOutcome::Closed`] if a `Quit` ran, else
    /// [`KeysOutcome::Updated`]. The returned `saved` flag (true whenever this
    /// batch ran a `Write`, including inside `:wq`/`:x`) is the kernel layer's
    /// cue to flush a file-backed session.
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
    /// `can_write: false` refuses the `Write` command outright — same
    /// message and outcome shape as [`refuse_write`](Self::refuse_write) —
    /// before it can set `write_requested` or advance anything, so a `:wq`
    /// with no write capability never reaches `Quit` either.
    fn run_commands(
        &mut self,
        id: EditorSessionId,
        commands: Vec<CommandRequest>,
        blocks: &SharedBlockStore,
        can_write: bool,
    ) -> Result<KeysOutcome, String> {
        let mut write_requested = false;
        let mut forced = false;
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
                CommandRequest::Quit { force } => {
                    let state = if write_requested {
                        self.save(id)?
                    } else {
                        self.state(id)?
                    };
                    if !force && state.dirty {
                        // A dialect-level refusal, not an RPC failure: ride the
                        // status line (like E492) and keep the session open. A
                        // hard error here never reaches the GUI and leaves the
                        // renderer's `:`-strip showing the stale submitted line.
                        let mut state = state;
                        state.message =
                            Some("No write since last change (add ! to override)".to_string());
                        return Ok(KeysOutcome::Updated(KeysUpdate {
                            state,
                            saved: false,
                            forced: false,
                            rolled_back: false,
                        }));
                    }
                    let rolled_back = self.quit(id, blocks)?;
                    // `write_requested` covers `:wq`/`:x` (Write ran earlier
                    // in this same batch); a bare `:q`/`:q!` never set it, so
                    // nothing to flush. `rolled_back` is independent of that —
                    // a `:q!` discard can still have rewritten the block.
                    return Ok(KeysOutcome::Closed(KeysUpdate {
                        state,
                        saved: write_requested,
                        forced,
                        rolled_back,
                    }));
                }
            }
        }
        // No `Quit` ran — a bare `:w`/`:w!`, or an empty `:` line. The
        // checkpoint defers to the kernel layer's flush-then-checkpoint, so
        // `state` here still reads dirty.
        let state = self.state(id)?;
        Ok(KeysOutcome::Updated(KeysUpdate {
            state,
            saved: write_requested,
            forced,
            rolled_back: false,
        }))
    }

    /// Reconcile every open session bound to `(context_id, block_id)` against
    /// the block's *current* text, after some **other** writer (a sibling editor
    /// session, an MCP file edit, a streaming turn) mutated it. Returns the
    /// `(id, new state)` of every session whose buffer actually changed — the
    /// caller publishes those on the editor push channel.
    ///
    /// A session whose buffer already matches the block is skipped: that is the
    /// session's *own* mirror write echoing back through the block flow (the
    /// mirror is faithful, so its buffer equals the block), and reconciling it
    /// would jolt the cursor on every keystroke. Reads the block at most once,
    /// and only when a session is actually bound here, so the hot path (no
    /// editor open, or an unrelated block) costs just the match scan.
    pub fn reconcile_block(
        &mut self,
        context_id: ContextId,
        block_id: BlockId,
        blocks: &SharedBlockStore,
    ) -> Vec<(EditorSessionId, EditorState)> {
        let bound: Vec<EditorSessionId> = self
            .sessions
            .iter()
            .filter(|(_, s)| s.target.context_id == context_id && s.target.block_id == block_id)
            .map(|(id, _)| *id)
            .collect();
        if bound.is_empty() {
            return Vec::new();
        }
        // The block's text is the merged truth; reconcile against its normalized
        // (terminator-stripped) view, matching EditorCore's normalized buffer.
        // Reuse a bound session's own target (every bound session shares this
        // (context_id, block_id)) rather than constructing a fresh
        // `EditorTarget`.
        let target = self.sessions[&bound[0]].target;
        let raw = match block_text(blocks, &target) {
            Ok(t) => t,
            Err(_) => return Vec::new(), // block gone (deleted) — nothing to do
        };
        let merged = raw.strip_suffix('\n').unwrap_or(&raw);

        let mut changed = Vec::new();
        for id in bound {
            let session = self.sessions.get_mut(&id).expect("just collected");
            if session.core.apply_remote_text(merged) {
                // A non-self write merged in: this session's checkpoint no longer
                // owns the block's delta, so a ZQ rollback would clobber peer
                // work. Sticky until the checkpoint advances (save).
                session.peer_wrote = true;
                let saved = session.saved_content.clone();
                changed.push((id, state_of(&mut session.core, &saved)));
            }
        }
        changed
    }

    /// Take any `:r` read intent the last [`keys`](Self::keys) batch surfaced on
    /// a session (the kernel fulfills it asynchronously — fetch, then
    /// [`insert_text`](Self::insert_text)). `None` if no such session or no
    /// intent.
    pub fn take_io(&mut self, id: EditorSessionId) -> Option<EditorIo> {
        self.sessions.get_mut(&id)?.core.take_io()
    }

    /// The session's current leader-cursor char offset, or `None` if no such
    /// session. The kernel captures this at `:r`-submit time so the async insert
    /// lands where the command was issued, not wherever a concurrent keystroke
    /// moved the cursor during the fetch.
    pub fn session_cursor(&mut self, id: EditorSessionId) -> Option<usize> {
        self.sessions.get_mut(&id).map(|s| s.core.cursor())
    }

    /// Insert kernel-fetched `text` at `offset` (the cursor captured when the
    /// `:r` was submitted — see [`session_cursor`](Self::session_cursor)), mirror
    /// the produced ops onto the owning kernel block, and return the new state.
    /// Fails loud if the mirror write fails.
    pub fn insert_text(
        &mut self,
        id: EditorSessionId,
        text: &str,
        offset: usize,
        blocks: &SharedBlockStore,
    ) -> Result<EditorState, String> {
        let session = self.sessions.get_mut(&id).ok_or_else(|| no_session(id))?;
        let ops = session.core.insert_at(text, offset);
        for op in &ops {
            blocks
                .edit_text(
                    session.target.context_id,
                    &session.target.block_id,
                    op.offset,
                    &op.insert,
                    op.delete,
                )
                .map_err(|e| format!("editor :r: block mirror failed: {e}"))?;
        }
        let saved = session.saved_content.clone();
        Ok(state_of(&mut session.core, &saved))
    }

    /// Current state of a session.
    pub fn state(&mut self, id: EditorSessionId) -> Result<EditorState, String> {
        let session = self.sessions.get_mut(&id).ok_or_else(|| no_session(id))?;
        let saved = session.saved_content.clone();
        Ok(state_of(&mut session.core, &saved))
    }

    /// The path a session was opened on, or `None` if no such session. Captured
    /// before a `ZZ`/`ZQ` (which drops the session) so the caller can invalidate
    /// the file cache for it afterward.
    pub fn session_path(&self, id: EditorSessionId) -> Option<String> {
        self.sessions.get(&id).map(|s| s.path.clone())
    }

    /// `ZZ` — checkpoint the current buffer as saved, returning the now-clean
    /// state. The kernel layer (which holds the file-document cache this pure
    /// session registry does not) flushes it to disk after this call — see
    /// `Kernel::editor_save`/`Kernel::editor_keys`.
    pub fn save(&mut self, id: EditorSessionId) -> Result<EditorState, String> {
        let session = self.sessions.get_mut(&id).ok_or_else(|| no_session(id))?;
        session.saved_content = session.core.text();
        // The checkpoint now contains every merged peer edit, so a rollback to
        // it can no longer clobber them — the entanglement resets.
        session.peer_wrote = false;
        let saved = session.saved_content.clone();
        Ok(state_of(&mut session.core, &saved))
    }

    /// `ZQ` — discard changes since the last checkpoint by writing the saved
    /// text back onto the block (an inverse forward edit — the block log has no
    /// history erasure), then drop the session.
    ///
    /// **Entanglement guard: detach, don't retract.** The rollback runs only
    /// when this session was provably alone with the block since its checkpoint.
    /// If a sibling session is still bound to it, or a peer's write merged in
    /// since the checkpoint ([`EditorSession::peer_wrote`]), the merged text is
    /// shared work — rolling it back would clobber them. A shared-session `ZQ`
    /// therefore just quits *this* player out of the doc and leaves the block's
    /// merged truth for the others (Amy, 2026-07-07; `docs/vi.md` → Rollback).
    ///
    /// **Removes the session only once the rollback has actually landed** (P1,
    /// kaibo review of `d45e0484`/`4369bd77`/`f02f3688`, "BUG 1"): this used to
    /// `remove` up front and roll back after, so a rollback failure
    /// (`block_text`/`edit_text` erroring — a block deleted out from under the
    /// session, a store error) returned `Err` with the session already gone.
    /// Both `Kernel` callers (`editor_keys_checked`, `editor_quit`) propagate
    /// that `Err` with `?` before ever reaching their `unpin`, so the
    /// file-cache pin taken at open (`docs/file-buffers.md`) leaked for the
    /// kernel's lifetime — nothing left to call `unpin`, and no session left
    /// to retry. One invariant here (never remove before the rollback
    /// succeeds) closes it at both call sites at once, instead of teaching
    /// each caller to unpin on its own error path.
    ///
    /// Returns whether the rollback actually rewrote the block (`false` for
    /// the sibling/peer-wrote early return, or when the block already held
    /// the checkpoint). This pure registry has no file cache to inform, so
    /// it can't mark the entry dirty itself — `Kernel::editor_quit` reads
    /// this to do that (kaibo review of `d45e0484`/`4369bd77`/`f02f3688`,
    /// the `ack`-then-`ZQ` divergence: a content-changing rollback mutates
    /// the block via a raw `edit_text` call the file cache's own
    /// dirty/flush bookkeeping never sees).
    pub fn quit(&mut self, id: EditorSessionId, blocks: &SharedBlockStore) -> Result<bool, String> {
        let session = self.sessions.get(&id).ok_or_else(|| no_session(id))?;
        let sibling_bound = self
            .sessions
            .iter()
            .any(|(sid, s)| *sid != id && s.target == session.target);
        if sibling_bound || session.peer_wrote {
            self.sessions.remove(&id);
            return Ok(false);
        }
        // Restore the normalized checkpoint *plus* the block's terminator, so a
        // rollback never strips a trailing newline the block opened with.
        let restore = format!("{}{}", session.saved_content, session.terminator);
        let current = block_text(blocks, &session.target)?;
        let rolled_back = current != restore;
        if rolled_back {
            blocks
                .edit_text(
                    session.target.context_id,
                    &session.target.block_id,
                    0,
                    &restore,
                    current.chars().count(),
                )
                .map_err(|e| format!("editor quit: rollback failed: {e}"))?;
        }
        self.sessions.remove(&id);
        Ok(rolled_back)
    }

    /// Whether a session is still open.
    pub fn is_open(&self, id: EditorSessionId) -> bool {
        self.sessions.contains_key(&id)
    }

    /// A census of every open session — `kj editor list`'s data. A session
    /// whose opener walked away (a model that opened and never quit, a dead
    /// headless driver) is otherwise invisible and immortal; this makes it
    /// visible. No GC/eviction here — observability first, judgment later.
    /// Sorted by session id ascending (open order).
    pub fn list(&self) -> Vec<EditorSessionInfo> {
        let mut out: Vec<EditorSessionInfo> = self
            .sessions
            .iter()
            .map(|(id, s)| EditorSessionInfo {
                session: id.as_u64(),
                path: s.path.clone(),
                context_id: s.target.context_id.to_hex(),
                block_id: s.target.block_id.to_key(),
                dirty: s.core.text() != s.saved_content,
                mode: s.core.mode(),
                opener: s.opener.map(|o| o.principal.to_hex()),
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
fn state_of(core: &mut EditorCore, checkpoint: &str) -> EditorState {
    let text = core.text();
    let dirty = text != checkpoint;
    let cursor = core.cursor();
    let mode = core.mode();
    let command_line = core.command_line();
    EditorState {
        text,
        cursor,
        mode,
        dirty,
        command_line,
        // A fresh state carries no status message; the command path sets one only
        // when a `:`-line errored, and it clears on the next keystroke batch.
        message: None,
    }
}

fn no_session(id: EditorSessionId) -> String {
    format!("editor: no such session {}", id.0)
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
        let (id, st) = sessions.open(RC_PATH, target, &blocks, None).unwrap();
        assert_eq!(st.text, "hello");
        assert!(!st.dirty);

        // Insert "X" at the start: i X <Esc>.
        let outcome = sessions.keys(id, "iX<Esc>", &blocks).unwrap();
        assert_eq!(outcome.state().text, "Xhello");
        assert!(outcome.state().dirty, "buffer diverged from checkpoint");

        // The invariant that makes this surface trustworthy: the kernel block now
        // equals the editor buffer (edit mirroring is faithful).
        assert_eq!(block_text(&blocks, &target).unwrap(), "Xhello");
    }

    #[tokio::test]
    async fn save_clears_dirty_and_moves_the_checkpoint() {
        let (blocks, target) = seeded(b"hello").await;
        let mut sessions = EditorSessions::new();
        let (id, _) = sessions.open(RC_PATH, target, &blocks, None).unwrap();

        sessions.keys(id, "iX<Esc>", &blocks).unwrap();
        let st = sessions.save(id).unwrap();
        assert_eq!(st.text, "Xhello");
        assert!(!st.dirty, "save must clear dirty");
    }

    #[tokio::test]
    async fn quit_rolls_the_block_back_to_the_open_checkpoint() {
        let (blocks, target) = seeded(b"hello").await;
        let mut sessions = EditorSessions::new();
        let (id, _) = sessions.open(RC_PATH, target, &blocks, None).unwrap();

        // Delete the first char, mirror lands on the block...
        sessions.keys(id, "x", &blocks).unwrap();
        assert_eq!(block_text(&blocks, &target).unwrap(), "ello");

        // ...then ZQ restores the block to what we opened.
        sessions.quit(id, &blocks).unwrap();
        assert_eq!(block_text(&blocks, &target).unwrap(), "hello");
        assert!(!sessions.is_open(id), "quit drops the session");
    }

    #[tokio::test]
    async fn zz_through_keys_saves_and_closes() {
        // The race-free path: a `ZZ` in the key stream (not a separate RPC)
        // checkpoints the edit and drops the session in one shot.
        let (blocks, target) = seeded(b"hello").await;
        let mut sessions = EditorSessions::new();
        let (id, _) = sessions.open(RC_PATH, target, &blocks, None).unwrap();

        sessions.keys(id, "iX<Esc>", &blocks).unwrap(); // -> "Xhello"
        let outcome = sessions.keys(id, "ZZ", &blocks).unwrap();
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
        let (id, _) = sessions.open(RC_PATH, target, &blocks, None).unwrap();

        sessions.keys(id, "iX<Esc>", &blocks).unwrap(); // -> "Xhello"
        let outcome = sessions.keys(id, "ZQ", &blocks).unwrap();
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
        let (id, _) = sessions.open(RC_PATH, target, &blocks, None).unwrap();

        let outcome = sessions.keys(id, "iZZ", &blocks).unwrap();
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
        let (id, _) = sessions.open(RC_PATH, target, &blocks, None).unwrap();

        // Dirty the buffer so the rollback isn't a no-op (current != restore).
        sessions.keys(id, "x", &blocks).unwrap();

        // The document the session is bound to is gone — `block_text` (the
        // first step of `quit`'s rollback) must now fail.
        blocks.delete_document(target.context_id).unwrap();

        let err = sessions
            .quit(id, &blocks)
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
        // still see (`kj editor list`), still holds its rightful pin, and
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
        let (id, _) = sessions.open(RC_PATH, target, &blocks, None).unwrap();

        sessions.keys(id, "iX<Esc>", &blocks).unwrap(); // -> "Xhello"
        sessions.save(id).unwrap(); // checkpoint = "Xhello"
        sessions.keys(id, "iY<Esc>", &blocks).unwrap(); // -> "YXhello"
        sessions.quit(id, &blocks).unwrap();

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
        let (id, st) = sessions.open(RC_PATH, target, &blocks, None).unwrap();
        assert_eq!(st.text, "hello");
        assert!(!st.dirty, "a newline-terminated block must open clean");

        sessions.keys(id, "iX<Esc>", &blocks).unwrap();
        assert_eq!(block_text(&blocks, &target).unwrap(), "Xhello\n");

        sessions.quit(id, &blocks).unwrap();
        assert_eq!(
            block_text(&blocks, &target).unwrap(),
            "hello\n",
            "quit must restore content AND the trailing newline"
        );
    }

    #[tokio::test]
    async fn reconcile_skips_self_write_and_merges_a_sibling() {
        // Two sessions on one block: when A writes (mirroring onto the block),
        // reconcile_block must SKIP A (its buffer already matches — the mirror
        // is faithful) and MERGE the stale sibling B, reporting only B.
        let (blocks, target) = seeded(b"hello").await;
        let mut sessions = EditorSessions::new();
        let (a, _) = sessions.open(RC_PATH, target, &blocks, None).unwrap();
        let (b, _) = sessions.open(RC_PATH, target, &blocks, None).unwrap();

        sessions.keys(a, "iX<Esc>", &blocks).unwrap();
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
        let (a, _) = sessions.open(RC_PATH, target, &blocks, None).unwrap();
        let (b, _) = sessions.open(RC_PATH, target, &blocks, None).unwrap();

        sessions.keys(a, "iX<Esc>", &blocks).unwrap(); // -> "Xhello"
        let outcome = sessions.keys(a, ":q!<CR>", &blocks).unwrap();
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
        let (a, _) = sessions.open(RC_PATH, target, &blocks, None).unwrap();
        let (b, _) = sessions.open(RC_PATH, target, &blocks, None).unwrap();

        sessions.keys(b, "iB<Esc>", &blocks).unwrap(); // -> "Bhello"
        // The server's reconciler drives this off the block flow; simulate it.
        sessions.reconcile_block(target.context_id, target.block_id, &blocks);
        let outcome = sessions.keys(b, "ZZ", &blocks).unwrap(); // B saves + leaves
        assert!(matches!(outcome, KeysOutcome::Closed(_)));

        sessions.quit(a, &blocks).unwrap(); // A aborts
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
        let (a, _) = sessions.open(RC_PATH, target, &blocks, None).unwrap();
        let (b, _) = sessions.open(RC_PATH, target, &blocks, None).unwrap();

        sessions.keys(b, "iB<Esc>", &blocks).unwrap(); // -> "Bhello"
        sessions.reconcile_block(target.context_id, target.block_id, &blocks);
        sessions.save(a).unwrap(); // A's checkpoint = "Bhello"; entanglement resets
        sessions.keys(b, "ZZ", &blocks).unwrap(); // B leaves

        sessions.keys(a, "iA<Esc>", &blocks).unwrap(); // -> "ABhello"
        sessions.quit(a, &blocks).unwrap(); // alone + untangled → real rollback
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
        let (id, _) = sessions.open(RC_PATH, target, &blocks, None).unwrap();
        sessions.quit(id, &blocks).unwrap();
        let err = sessions.keys(id, "x", &blocks).unwrap_err();
        assert!(err.contains("no such session"), "got: {err}");
    }

    // ── `:` command mode (Slice 3) ───────────────────────────────────────────

    #[tokio::test]
    async fn colon_wq_saves_and_closes() {
        // `:wq` is the muscle-memory twin of `ZZ` — save the edit, drop the
        // session, keep the change on the block.
        let (blocks, target) = seeded(b"hello").await;
        let mut sessions = EditorSessions::new();
        let (id, _) = sessions.open(RC_PATH, target, &blocks, None).unwrap();

        sessions.keys(id, "iX<Esc>", &blocks).unwrap(); // -> "Xhello"
        let outcome = sessions.keys(id, ":wq<CR>", &blocks).unwrap();
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
        let (id, _) = sessions.open(RC_PATH, target, &blocks, None).unwrap();

        sessions.keys(id, "iX<Esc>", &blocks).unwrap(); // dirty
        let outcome = sessions.keys(id, ":q<CR>", &blocks).unwrap();
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
        let outcome = sessions.keys(id, "l", &blocks).unwrap();
        assert!(outcome.state().message.is_none(), "message clears on the next batch");
        let outcome = sessions.keys(id, ":q!<CR>", &blocks).unwrap();
        assert!(matches!(outcome, KeysOutcome::Closed(_)));
    }

    #[tokio::test]
    async fn colon_q_bang_discards_and_closes() {
        let (blocks, target) = seeded(b"hello").await;
        let mut sessions = EditorSessions::new();
        let (id, _) = sessions.open(RC_PATH, target, &blocks, None).unwrap();

        sessions.keys(id, "iX<Esc>", &blocks).unwrap(); // -> "Xhello"
        let outcome = sessions.keys(id, ":q!<CR>", &blocks).unwrap();
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
        let (id, _) = sessions.open(RC_PATH, target, &blocks, None).unwrap();

        sessions.keys(id, "iX<Esc>", &blocks).unwrap();
        let outcome = sessions.keys(id, ":w<CR>", &blocks).unwrap();
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
        let outcome = sessions.keys(id, ":q<CR>", &blocks).unwrap();
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
        let (id, _) = sessions.open(RC_PATH, target, &blocks, None).unwrap();

        let outcome = sessions.keys(id, ":frobnicate<CR>", &blocks).unwrap();
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
        let outcome = sessions.keys(id, "l", &blocks).unwrap();
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
        let (id, _) = sessions.open(RC_PATH, target, &blocks, None).unwrap();

        let outcome = sessions.keys(id, ":s/alpha/ALPHA/g<CR>", &blocks).unwrap();
        assert_eq!(outcome.state().text, "ALPHA beta ALPHA");
        assert!(outcome.state().dirty, "a substitution dirties the buffer");
        // The invariant: the block equals the edited buffer.
        assert_eq!(block_text(&blocks, &target).unwrap(), "ALPHA beta ALPHA");
    }

    #[tokio::test]
    async fn colon_percent_s_then_wq_persists() {
        let (blocks, target) = seeded(b"x y\nx y").await;
        let mut sessions = EditorSessions::new();
        let (id, _) = sessions.open(RC_PATH, target, &blocks, None).unwrap();

        sessions.keys(id, ":%s/x/Z/g<CR>", &blocks).unwrap();
        let outcome = sessions.keys(id, ":wq<CR>", &blocks).unwrap();
        assert!(matches!(outcome, KeysOutcome::Closed(_)), ":wq closes");
        // The substitution survived the save+close.
        assert_eq!(block_text(&blocks, &target).unwrap(), "Z y\nZ y");
    }

    #[tokio::test]
    async fn colon_s_then_q_bang_rolls_back() {
        // A substitution followed by `:q!` discards it (rollback to checkpoint).
        let (blocks, target) = seeded(b"keep me").await;
        let mut sessions = EditorSessions::new();
        let (id, _) = sessions.open(RC_PATH, target, &blocks, None).unwrap();

        sessions.keys(id, ":s/keep/DROP/<CR>", &blocks).unwrap();
        assert_eq!(block_text(&blocks, &target).unwrap(), "DROP me");
        sessions.keys(id, ":q!<CR>", &blocks).unwrap();
        assert_eq!(block_text(&blocks, &target).unwrap(), "keep me", ":q! discards :s");
    }

    #[tokio::test]
    async fn bad_substitute_pattern_reports_on_the_status_line_and_leaves_block_clean() {
        // A bad `:s` regex arrives on the same `Some(Err)` channel as an unknown
        // command, so it too reports on the status line and keeps the session
        // open — the block is left untouched, no silent edit.
        let (blocks, target) = seeded(b"hello").await;
        let mut sessions = EditorSessions::new();
        let (id, _) = sessions.open(RC_PATH, target, &blocks, None).unwrap();

        let outcome = sessions.keys(id, ":s/[/x/<CR>", &blocks).unwrap();
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
        let (id, _) = sessions.open(RC_PATH, target, &blocks, None).unwrap();

        let outcome = sessions.keys(id, ":w", &blocks).unwrap();
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
                principal: p,
                context_id: ContextId::new(),
                session_id: SessionId::new(),
            })
        };

        let (_a, _) = sessions.open(RC_PATH, target, &blocks, as_opener(me)).unwrap();
        let (b, _) = sessions.open(RC_PATH, target, &blocks, as_opener(me)).unwrap();

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
        let (_c, _) = sessions.open(RC_PATH, target, &blocks, None).unwrap();
        assert_eq!(
            sessions.latest_session_for(me).map(|(id, _)| id),
            Some(b),
            "a None-opener session doesn't become anyone's fg target"
        );
    }

    #[tokio::test]
    async fn list_censuses_every_open_session_by_id_dirty_and_opener() {
        // `kj editor list`'s data source: a leaked session (an opener who
        // walked away) must show up here, since there is no other way to see
        // it. Two sessions on the same block, only one dirtied — and without
        // a reconcile the sibling's own buffer stays untouched (that's the
        // reconcile-is-a-separate-step invariant `reconcile_skips_self_write_
        // and_merges_a_sibling` covers; here we're only checking `list()`).
        let (blocks, target) = seeded(b"hello").await;
        let mut sessions = EditorSessions::new();
        let (a, _) = sessions.open(RC_PATH, target, &blocks, None).unwrap();
        let (b, _) = sessions.open(RC_PATH, target, &blocks, None).unwrap();

        sessions.keys(a, "iX<Esc>", &blocks).unwrap(); // -> "Xhello", dirties A only

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

        sessions.quit(a, &blocks).unwrap();
        let list = sessions.list();
        assert_eq!(list.len(), 1, "quitting A leaves only B");
        assert_eq!(list[0].session, b.as_u64());

        // Opener round-trips into the record; a headless (None) session
        // reports no opener.
        assert!(list[0].opener.is_none(), "B was opened headless");
        let opener = Some(EditorOpener {
            principal: PrincipalId::system(),
            context_id: ContextId::new(),
            session_id: SessionId::new(),
        });
        let (c, _) = sessions.open(RC_PATH, target, &blocks, opener).unwrap();
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
}
