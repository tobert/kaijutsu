//! In-app editor: the kernel-owned editing surface (the `vi`/`edit` builtin +
//! `kj rc edit` default).
//!
//! Two parts:
//! - [`resolve_editor_target`] maps a VFS path to the CRDT `(context, block)`
//!   that *owns* its text, so an editor binds to the source of truth — never a
//!   copy (see "Bind to the owner" below).
//! - [`EditorSessions`] is the registry of open editors. Each session is a pure
//!   [`EditorCore`](kaijutsu_editor::EditorCore) bound to a target; keystrokes
//!   mirror onto the CRDT block, and a checkpoint backs `ZQ` rollback. This is
//!   the tool-shaped surface the app renders, a model plays, and tests drive —
//!   all headless. See `docs/vi.md`.
//!
//! ## Bind to the owner, not a copy
//!
//! Resolution is **path-kind aware**, and this is load-bearing, not cosmetic:
//!
//! - **config-owned** paths (`/etc/rc/*`, `/etc/config/*`) are sole-owned
//!   single-block [`DocKind::Config`] documents
//!   ([`ConfigCrdtFs`](crate::runtime::ConfigCrdtFs)). The CRDT *is* the owner —
//!   there is no host file. We resolve straight to that document's block.
//! - **ordinary files** resolve through
//!   [`FileDocumentCache::get_or_load`](crate::file_tools::FileDocumentCache),
//!   which mints/loads a working-copy file-doc.
//!
//! Running a config path through `get_or_load` would create a *second* CRDT doc
//! (a `FileDocumentCache` copy) shadowing the ConfigCrdtFs original —
//! reintroducing the dual-ownership write-through bug class the CRDT-owned-config
//! work (`docs/config-crdt-ownership.md`) deleted by construction. So the branch
//! is the whole point. See `docs/vi.md` ("Path resolution").

use kaijutsu_crdt::{BlockId, ContextId};
use kaijutsu_types::{PrincipalId, SessionId};

use crate::block_store::SharedBlockStore;
use crate::config_doc::{config_context_id, first_block_id};
use crate::file_tools::FileDocumentCache;

/// The well-known nick the Bevy app registers under (see `peers/mod.rs`). The
/// `open_editor` signal targets it. Pass 1 addresses this single app peer; the
/// submitting-peer addressing refinement (multi-user) is tracked in `docs/vi.md`
/// risk #1.
pub const APP_PEER_NICK: &str = "kaijutsu-app";

/// The CRDT location an editor binds to: the context + block that own a path's
/// text. Edits go to `block_store.edit_text(context_id, block_id, …)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EditorTarget {
    pub context_id: ContextId,
    pub block_id: BlockId,
}

/// Cheap prefix test for whether `path` is in the rc/config trees.
///
/// **This is NOT the editor's ownership decision** — that is
/// [`resolve_editor_target`], which asks the mount table ([`MountTable::owner_of`]
/// + [`VfsOps::owns_config_docs`](crate::vfs::VfsOps::owns_config_docs)) so the
/// editor and the VFS can't drift on what owns a path. This prefix check survives
/// only as the **synchronous** guard for `Kernel::invalidate_config_file_cache`,
/// a cache-coherence optimization on the sync editor-quit path where an `async`
/// mount-table query would cascade. Tracked in `docs/issues.md`.
pub fn config_owned(path: &str) -> bool {
    matches!(path, "/etc/rc" | "/etc/config")
        || path.starts_with("/etc/rc/")
        || path.starts_with("/etc/config/")
}

/// Resolve `path` to the `(context, block)` of the CRDT document that owns its
/// text. The mount table answers "what owns this path?": a backend that
/// [`owns_config_docs`](crate::vfs::VfsOps::owns_config_docs) (the rc/config
/// `ConfigCrdtFs`) binds straight to its block; anything else goes through the
/// file-doc cache. Fails loud (no silent empty/placeholder) when a config path
/// names a document that does not exist — an editor must not open on a phantom
/// block.
pub async fn resolve_editor_target(
    path: &str,
    blocks: &SharedBlockStore,
    file_cache: &FileDocumentCache,
    mounts: &crate::vfs::MountTable,
) -> Result<EditorTarget, String> {
    // Ask the VFS which backend owns this path. The config-doc backends answer
    // for themselves — no hardcoded `/etc/rc` prefix to drift from the mounts.
    if let Some((mount_root, fs)) = mounts.owner_of(std::path::Path::new(path)).await {
        if fs.owns_config_docs() {
            // Follow any rc/config symlink to its terminal document FIRST, exactly
            // as the read/exec path (`ConfigCrdtFs`) does. Without this the editor
            // binds the *symlink's own* block (e.g. `coder/*` → `lib/*`, the init.d
            // composition) while reads resolve to the target — so saved edits land
            // on a block nothing else reads (docs/issues.md). Resolving here makes
            // the editor and the executor agree on one block. A fresh
            // `ConfigCrdtFs` at the mount root does the lexical walk (it is
            // stateless — blocks + root); `resolve_canonical` is not on `VfsOps`.
            let root = mount_root.to_string_lossy().into_owned();
            let config_fs =
                crate::runtime::config_crdt_fs::ConfigCrdtFs::new(blocks.clone(), root);
            let resolved = config_fs
                .resolve_canonical(path)
                .map_err(|e| format!("open editor: resolve '{path}': {e}"))?;
            let context_id = config_context_id(&resolved);
            let block_id = first_block_id(blocks, context_id).ok_or_else(|| {
                format!("open editor: config document '{path}' does not exist (nothing to edit)")
            })?;
            return Ok(EditorTarget {
                context_id,
                block_id,
            });
        }
    }
    let (context_id, block_id) = file_cache
        .get_or_load(path)
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

/// The result of feeding a key batch to a session via [`EditorSessions::keys`].
#[derive(Debug)]
pub enum KeysOutcome {
    /// The buffer updated; the new renderer state to push.
    Updated(EditorState),
    /// A `ZZ`/`ZQ` closed the session (already saved/discarded + dropped). The
    /// state is the last view before close; renderers react to the `Closed` push.
    Closed(EditorState),
}

impl KeysOutcome {
    /// The renderer state this outcome carries — the post-edit view, or the
    /// last view before a `ZZ`/`ZQ` close.
    pub fn state(&self) -> &EditorState {
        match self {
            Self::Updated(s) | Self::Closed(s) => s,
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
    /// builtin, `kj rc edit`). Object form (inspect-style) so a driver reads one
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
/// the front door (`vi`/`edit`, `kj editor`, `kj rc edit`) and recorded on the
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

/// One open editor: a pure [`EditorCore`] bound to the CRDT block that owns the
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

    /// Feed keys to a session, mirror the produced edits onto the CRDT block,
    /// and report the outcome. Fails loud if a mirror write fails — the buffer
    /// and the block must never silently diverge.
    ///
    /// If the batch contained a `ZZ`/`ZQ` (which modalkit, owning the real mode,
    /// distinguishes from an inserted `ZZ`), the session is saved/discarded and
    /// dropped here and the outcome is [`KeysOutcome::Closed`]; otherwise it is
    /// [`KeysOutcome::Updated`] with the new renderer state.
    pub fn keys(
        &mut self,
        id: EditorSessionId,
        keys: &str,
        blocks: &SharedBlockStore,
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
                    .map_err(|e| format!("editor keys: CRDT mirror failed: {e}"))?;
            }
            (session.core.take_close(), session.core.take_commands())
        };

        // `ZZ`/`ZQ` close the session. The returned state is informational (the
        // last view before close); renderers react to the `Closed` push.
        if let Some(close) = close {
            let final_state = match close {
                CloseRequest::Write => {
                    // ZZ: checkpoint current as saved (flush to owner), then quit
                    // — the rollback to that just-taken checkpoint is a no-op.
                    let state = self.save(id)?;
                    self.quit(id, blocks)?;
                    state
                }
                CloseRequest::Discard => {
                    // ZQ: snapshot the view, then roll back to the last checkpoint.
                    let state = self.state(id)?;
                    self.quit(id, blocks)?;
                    state
                }
            };
            return Ok(KeysOutcome::Closed(final_state));
        }

        // A submitted `:`-line (`:w`/`:wq`/`:q!`/…). A parsed batch runs in
        // order; an unknown command or a bad `:s` regex (both arrive as
        // `Some(Err)`) reports vim-style on the status line and keeps the session
        // open — never errors the whole `editor_keys` call out from under the
        // renderer (the front door would otherwise surface it as a hard failure).
        if let Some(parsed) = commands {
            match parsed {
                Ok(cmds) => return self.run_commands(id, cmds, blocks),
                Err(msg) => {
                    let session = self.sessions.get_mut(&id).ok_or_else(|| no_session(id))?;
                    let saved = session.saved_content.clone();
                    let mut state = state_of(&mut session.core, &saved);
                    state.message = Some(msg);
                    return Ok(KeysOutcome::Updated(state));
                }
            }
        }

        let session = self.sessions.get_mut(&id).ok_or_else(|| no_session(id))?;
        let saved = session.saved_content.clone();
        Ok(KeysOutcome::Updated(state_of(&mut session.core, &saved)))
    }

    /// Act on a parsed `:`-command batch (`docs/vi.md` → *Command mode*). `Write`
    /// checkpoints and stays open; `Quit` closes — refusing a dirty buffer
    /// without `!` (vim's "No write since last change"). `[Write, Quit]` (`:wq`)
    /// saves-clean then closes. Returns [`KeysOutcome::Closed`] if a `Quit` ran,
    /// else [`KeysOutcome::Updated`] with the post-save state.
    fn run_commands(
        &mut self,
        id: EditorSessionId,
        commands: Vec<CommandRequest>,
        blocks: &SharedBlockStore,
    ) -> Result<KeysOutcome, String> {
        let mut saved_state = None;
        for cmd in commands {
            match cmd {
                // `force` (`:w!`) is reserved for a future read-only/permission
                // gate; rc/config has none today, so a forced write == a write.
                CommandRequest::Write { force: _ } => {
                    saved_state = Some(self.save(id)?);
                }
                CommandRequest::Quit { force } => {
                    let state = self.state(id)?;
                    if !force && state.dirty {
                        return Err(
                            "No write since last change (add ! to override)".to_string()
                        );
                    }
                    self.quit(id, blocks)?;
                    return Ok(KeysOutcome::Closed(state));
                }
            }
        }
        // No `Quit` ran (a bare `:w`, or an empty `:` line). Report the post-save
        // state, or — for a no-op line — the current view.
        let state = match saved_state {
            Some(state) => state,
            None => self.state(id)?,
        };
        Ok(KeysOutcome::Updated(state))
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
        let raw = match block_text(blocks, &EditorTarget { context_id, block_id }) {
            Ok(t) => t,
            Err(_) => return Vec::new(), // block gone (deleted) — nothing to do
        };
        let merged = raw.strip_suffix('\n').unwrap_or(&raw);

        let mut changed = Vec::new();
        for id in bound {
            let session = self.sessions.get_mut(&id).expect("just collected");
            if session.core.apply_remote_text(merged) {
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
    /// the produced ops onto the owning CRDT block, and return the new state.
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
                .map_err(|e| format!("editor :r: CRDT mirror failed: {e}"))?;
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
    /// state. For config/rc blocks the CRDT is already the persistent owner;
    /// file-doc disk flush is TBD (see `docs/vi.md` tech-debt sweep).
    pub fn save(&mut self, id: EditorSessionId) -> Result<EditorState, String> {
        let session = self.sessions.get_mut(&id).ok_or_else(|| no_session(id))?;
        session.saved_content = session.core.text();
        let saved = session.saved_content.clone();
        Ok(state_of(&mut session.core, &saved))
    }

    /// `ZQ` — discard changes since the last checkpoint by writing the saved
    /// text back onto the block (an inverse forward edit — the CRDT has no
    /// history erasure), then drop the session. Pass 1 restores the whole
    /// checkpoint (last-writer w.r.t. concurrent peer edits — see `docs/vi.md`).
    pub fn quit(&mut self, id: EditorSessionId, blocks: &SharedBlockStore) -> Result<(), String> {
        let session = self.sessions.remove(&id).ok_or_else(|| no_session(id))?;
        // Restore the normalized checkpoint *plus* the block's terminator, so a
        // rollback never strips a trailing newline the block opened with.
        let restore = format!("{}{}", session.saved_content, session.terminator);
        let current = block_text(blocks, &session.target)?;
        if current != restore {
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
        Ok(())
    }

    /// Whether a session is still open.
    pub fn is_open(&self, id: EditorSessionId) -> bool {
        self.sessions.contains_key(&id)
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
mod tests {
    use super::*;
    use crate::block_store::shared_block_store_with_db;
    use crate::kernel_db::KernelDb;
    use crate::runtime::config_crdt_fs::ConfigCrdtFs;
    use crate::vfs::VfsOps as _;
    use kaijutsu_crdt::PrincipalId;
    use std::path::Path;
    use std::sync::Arc;

    /// A block store backed by an in-memory KernelDb, so config docs created via
    /// `create_document_with_path` land in the `documents` manifest (mirrors the
    /// ConfigCrdtFs test fixture).
    fn blocks_with_db() -> SharedBlockStore {
        let creator = PrincipalId::system();
        let db = Arc::new(parking_lot::Mutex::new(KernelDb::in_memory().unwrap()));
        let ws_id = db.lock().get_or_create_default_workspace(creator).unwrap();
        shared_block_store_with_db(db, ws_id, creator)
    }

    /// A mount table with the rc `ConfigCrdtFs` mounted at `/etc/rc` — the
    /// production shape the resolver queries to decide config-ownership.
    async fn mounts_with_rc(blocks: &SharedBlockStore) -> Arc<crate::vfs::MountTable> {
        let mt = crate::vfs::MountTable::new();
        mt.mount("/etc/rc", ConfigCrdtFs::new(blocks.clone(), "/etc/rc"))
            .await;
        Arc::new(mt)
    }

    #[test]
    fn config_owned_covers_rc_and_config_trees_only() {
        assert!(config_owned("/etc/rc"));
        assert!(config_owned("/etc/rc/coder/create/S00-stance.kai"));
        assert!(config_owned("/etc/config"));
        assert!(config_owned("/etc/config/model.toml"));
        // Not the config trees:
        assert!(!config_owned("/etc"));
        assert!(!config_owned("/etc/passwd"));
        assert!(!config_owned("/etc/rcfoo")); // prefix, not a child
        assert!(!config_owned("/home/atobey/src/kaijutsu/notes.md"));
    }

    #[tokio::test]
    async fn resolves_rc_path_to_its_configcrdtfs_owner_block() {
        let blocks = blocks_with_db();
        // Seed an rc script through the owning backend, exactly as `kj rc` does.
        let rc = ConfigCrdtFs::new(blocks.clone(), "/etc/rc");
        rc.write_all(Path::new("coder/create/S00-stance.kai"), b"be kind")
            .await
            .unwrap();

        // The mount table owns the answer: it routes the path to the rc
        // ConfigCrdtFs (config-owned), so the file cache is never consulted.
        let mounts = mounts_with_rc(&blocks).await;
        let file_cache = FileDocumentCache::new(blocks.clone(), mounts.clone());

        let full = "/etc/rc/coder/create/S00-stance.kai";
        let target = resolve_editor_target(full, &blocks, &file_cache, &mounts)
            .await
            .expect("rc path resolves to its owning block");

        // The target is the ConfigCrdtFs-owned document, NOT a file-doc copy.
        let expected_ctx = config_context_id(full);
        assert_eq!(
            target.context_id, expected_ctx,
            "must bind the config owner"
        );
        assert_eq!(
            target.block_id,
            first_block_id(&blocks, expected_ctx).unwrap(),
            "must bind the owning block",
        );
    }

    #[tokio::test]
    async fn resolves_symlinked_rc_path_to_its_target_block() {
        // The init.d composition: `coder/*` rc scripts are symlinks to the
        // shared `lib/*` source. The editor must bind the TARGET's block — the
        // one the executor reads — not the symlink's own block, or saved edits
        // land on a doc nothing else reads (docs/issues.md, fixed here).
        let blocks = blocks_with_db();
        let rc = ConfigCrdtFs::new(blocks.clone(), "/etc/rc");
        // The real source lives under lib/.
        rc.write_all(Path::new("lib/create/S10-binding.kai"), b"kj binding allow \"*\"")
            .await
            .unwrap();
        // coder/ composes it in via a symlink (absolute target, like the seed).
        rc.symlink(
            Path::new("coder/create/S10-binding.kai"),
            Path::new("/etc/rc/lib/create/S10-binding.kai"),
        )
        .await
        .unwrap();

        let mounts = mounts_with_rc(&blocks).await;
        let file_cache = FileDocumentCache::new(blocks.clone(), mounts.clone());

        let link_path = "/etc/rc/coder/create/S10-binding.kai";
        let target = resolve_editor_target(link_path, &blocks, &file_cache, &mounts)
            .await
            .expect("symlinked rc path resolves to its target block");

        // Binds the TARGET (lib) document — what the executor reads…
        let target_ctx = config_context_id("/etc/rc/lib/create/S10-binding.kai");
        assert_eq!(
            target.context_id, target_ctx,
            "must bind the symlink target's owner"
        );
        assert_eq!(
            target.block_id,
            first_block_id(&blocks, target_ctx).unwrap(),
            "must bind the target block",
        );
        // …and NOT the symlink doc's own (coder-path) context.
        assert_ne!(
            target.context_id,
            config_context_id(link_path),
            "must not bind the symlink doc itself"
        );
    }

    #[tokio::test]
    async fn missing_config_doc_fails_loud_not_empty() {
        let blocks = blocks_with_db();
        let mounts = mounts_with_rc(&blocks).await;
        let file_cache = FileDocumentCache::new(blocks.clone(), mounts.clone());

        // No document was ever seeded at this path, but the mount table still
        // routes it to the config backend → fail loud (not a file-cache miss).
        let err =
            resolve_editor_target("/etc/rc/nope/create/S00.kai", &blocks, &file_cache, &mounts)
                .await
                .expect_err("a phantom config doc must error, not open an empty editor");
        assert!(
            err.contains("does not exist"),
            "fail-loud message, got: {err}"
        );
    }
}

#[cfg(test)]
mod session_tests {
    //! e2e editor-session lifecycle against a live block store. No GUI: drives
    //! the same surface the app/model/test all share (vi.md test layer 2).
    use super::*;
    use crate::block_store::shared_block_store_with_db;
    use crate::kernel_db::KernelDb;
    use crate::runtime::config_crdt_fs::ConfigCrdtFs;
    use crate::vfs::{MountTable, VfsOps as _};
    use kaijutsu_crdt::PrincipalId;
    use std::path::Path;
    use std::sync::Arc;

    const RC_PATH: &str = "/etc/rc/coder/create/S00.kai";

    /// A block store seeded with one rc script (`"hello"`) through its owning
    /// ConfigCrdtFs backend, plus the resolved editor target for it.
    async fn seeded(initial: &[u8]) -> (SharedBlockStore, EditorTarget) {
        let creator = PrincipalId::system();
        let db = Arc::new(parking_lot::Mutex::new(KernelDb::in_memory().unwrap()));
        let ws = db.lock().get_or_create_default_workspace(creator).unwrap();
        let blocks = shared_block_store_with_db(db, ws, creator);
        let rc = ConfigCrdtFs::new(blocks.clone(), "/etc/rc");
        rc.write_all(Path::new("coder/create/S00.kai"), initial)
            .await
            .unwrap();
        let mounts = Arc::new({
            let mt = MountTable::new();
            mt.mount("/etc/rc", ConfigCrdtFs::new(blocks.clone(), "/etc/rc"))
                .await;
            mt
        });
        let fc = FileDocumentCache::new(blocks.clone(), mounts.clone());
        let target = resolve_editor_target(RC_PATH, &blocks, &fc, &mounts)
            .await
            .unwrap();
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

        // The invariant that makes this surface trustworthy: the CRDT block now
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
    async fn colon_q_refuses_a_dirty_buffer() {
        // Plain `:q` on unsaved changes must fail loud (vim's "No write since
        // last change"), not silently lose the edit.
        let (blocks, target) = seeded(b"hello").await;
        let mut sessions = EditorSessions::new();
        let (id, _) = sessions.open(RC_PATH, target, &blocks, None).unwrap();

        sessions.keys(id, "iX<Esc>", &blocks).unwrap(); // dirty
        let err = sessions.keys(id, ":q<CR>", &blocks).unwrap_err();
        assert!(err.contains("No write since last change"), "got: {err}");
        assert!(sessions.is_open(id), ":q must not drop a dirty session");
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
    async fn colon_w_saves_and_stays_open() {
        let (blocks, target) = seeded(b"hello").await;
        let mut sessions = EditorSessions::new();
        let (id, _) = sessions.open(RC_PATH, target, &blocks, None).unwrap();

        sessions.keys(id, "iX<Esc>", &blocks).unwrap();
        let outcome = sessions.keys(id, ":w<CR>", &blocks).unwrap();
        assert!(
            matches!(outcome, KeysOutcome::Updated(_)),
            ":w keeps the session open"
        );
        assert!(!outcome.state().dirty, ":w clears dirty");
        assert!(sessions.is_open(id));
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
        // `:s` is an edit — it must mirror onto the owning CRDT block like any
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
}
