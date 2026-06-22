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

/// Whether `path` names content the config backends own as a CRDT document
/// (`/etc/rc` scripts or `/etc/config` TOMLs). These bind directly to their
/// owning block rather than through the file-doc cache.
///
/// Prefix-based for pass 1; the sharp edge (this should be the mount table's
/// answer, "what owns this path?") is noted in `docs/vi.md`.
pub fn config_owned(path: &str) -> bool {
    matches!(path, "/etc/rc" | "/etc/config")
        || path.starts_with("/etc/rc/")
        || path.starts_with("/etc/config/")
}

/// Resolve `path` to the `(context, block)` of the CRDT document that owns its
/// text. Fails loud (no silent empty/placeholder) when a config path names a
/// document that does not exist — an editor must not open on a phantom block.
pub async fn resolve_editor_target(
    path: &str,
    blocks: &SharedBlockStore,
    file_cache: &FileDocumentCache,
) -> Result<EditorTarget, String> {
    if config_owned(path) {
        let context_id = config_context_id(path);
        let block_id = first_block_id(blocks, context_id).ok_or_else(|| {
            format!("open editor: config document '{path}' does not exist (nothing to edit)")
        })?;
        Ok(EditorTarget { context_id, block_id })
    } else {
        let (context_id, block_id) = file_cache
            .get_or_load(path)
            .await
            .map_err(|e| format!("open editor: cannot open '{path}': {e}"))?;
        Ok(EditorTarget { context_id, block_id })
    }
}

// ============================================================================
// Editor sessions — the kernel-owned editing surface
// ============================================================================

use std::collections::HashMap;

use kaijutsu_editor::EditorCore;

/// Handle to one open editor session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct EditorSessionId(u64);

/// A renderer-facing snapshot of a session: what to draw, plus dirtiness.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EditorState {
    pub text: String,
    pub cursor: usize,
    pub mode: Option<String>,
    /// Whether the buffer differs from the last open/save checkpoint.
    pub dirty: bool,
}

/// One open editor: a pure [`EditorCore`] bound to the CRDT block that owns the
/// text, plus the rollback checkpoint.
struct EditorSession {
    core: EditorCore,
    target: EditorTarget,
    #[allow(dead_code)] // carried for save-to-disk of file docs (TBD) + diagnostics
    path: String,
    /// Text as of the last open/save — the `ZQ` rollback target.
    saved_text: String,
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
    ) -> Result<(EditorSessionId, EditorState), String> {
        let text = block_text(blocks, &target)?;
        let mut core = EditorCore::new(&text);
        let state = state_of(&mut core, &text);
        let id = EditorSessionId(self.next_id);
        self.next_id += 1;
        self.sessions.insert(
            id,
            EditorSession {
                core,
                target,
                path: path.to_string(),
                saved_text: text,
            },
        );
        Ok((id, state))
    }

    /// Feed keys to a session, mirror the produced edits onto the CRDT block,
    /// and return the new state. Fails loud if a mirror write fails — the buffer
    /// and the block must never silently diverge.
    pub fn keys(
        &mut self,
        id: EditorSessionId,
        keys: &str,
        blocks: &SharedBlockStore,
    ) -> Result<EditorState, String> {
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
        let saved = session.saved_text.clone();
        Ok(state_of(&mut session.core, &saved))
    }

    /// Current state of a session.
    pub fn state(&mut self, id: EditorSessionId) -> Result<EditorState, String> {
        let session = self.sessions.get_mut(&id).ok_or_else(|| no_session(id))?;
        let saved = session.saved_text.clone();
        Ok(state_of(&mut session.core, &saved))
    }

    /// `ZZ` — checkpoint the current buffer as saved, returning the now-clean
    /// state. For config/rc blocks the CRDT is already the persistent owner;
    /// file-doc disk flush is TBD (see `docs/vi.md` tech-debt sweep).
    pub fn save(&mut self, id: EditorSessionId) -> Result<EditorState, String> {
        let session = self.sessions.get_mut(&id).ok_or_else(|| no_session(id))?;
        session.saved_text = session.core.text();
        let saved = session.saved_text.clone();
        Ok(state_of(&mut session.core, &saved))
    }

    /// `ZQ` — discard changes since the last checkpoint by writing the saved
    /// text back onto the block (an inverse forward edit — the CRDT has no
    /// history erasure), then drop the session. Pass 1 restores the whole
    /// checkpoint (last-writer w.r.t. concurrent peer edits — see `docs/vi.md`).
    pub fn quit(&mut self, id: EditorSessionId, blocks: &SharedBlockStore) -> Result<(), String> {
        let session = self.sessions.remove(&id).ok_or_else(|| no_session(id))?;
        let current = block_text(blocks, &session.target)?;
        if current != session.saved_text {
            blocks
                .edit_text(
                    session.target.context_id,
                    &session.target.block_id,
                    0,
                    &session.saved_text,
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
    EditorState { text, cursor, mode, dirty }
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

        // A bare FileDocumentCache is never consulted for a config path; the
        // resolver must answer from the ConfigCrdtFs owner instead.
        let file_cache = FileDocumentCache::new(blocks.clone(), Arc::new(crate::vfs::MountTable::new()));

        let full = "/etc/rc/coder/create/S00-stance.kai";
        let target = resolve_editor_target(full, &blocks, &file_cache)
            .await
            .expect("rc path resolves to its owning block");

        // The target is the ConfigCrdtFs-owned document, NOT a file-doc copy.
        let expected_ctx = config_context_id(full);
        assert_eq!(target.context_id, expected_ctx, "must bind the config owner");
        assert_eq!(
            target.block_id,
            first_block_id(&blocks, expected_ctx).unwrap(),
            "must bind the owning block",
        );
    }

    #[tokio::test]
    async fn missing_config_doc_fails_loud_not_empty() {
        let blocks = blocks_with_db();
        let file_cache = FileDocumentCache::new(blocks.clone(), Arc::new(crate::vfs::MountTable::new()));

        // No document was ever seeded at this path.
        let err = resolve_editor_target("/etc/rc/nope/create/S00.kai", &blocks, &file_cache)
            .await
            .expect_err("a phantom config doc must error, not open an empty editor");
        assert!(err.contains("does not exist"), "fail-loud message, got: {err}");
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
        let fc = FileDocumentCache::new(blocks.clone(), Arc::new(MountTable::new()));
        let target = resolve_editor_target(RC_PATH, &blocks, &fc).await.unwrap();
        (blocks, target)
    }

    #[tokio::test]
    async fn keystrokes_mirror_to_the_owning_block() {
        let (blocks, target) = seeded(b"hello").await;
        let mut sessions = EditorSessions::new();
        let (id, st) = sessions.open(RC_PATH, target, &blocks).unwrap();
        assert_eq!(st.text, "hello");
        assert!(!st.dirty);

        // Insert "X" at the start: i X <Esc>.
        let st = sessions.keys(id, "iX<Esc>", &blocks).unwrap();
        assert_eq!(st.text, "Xhello");
        assert!(st.dirty, "buffer diverged from checkpoint");

        // The invariant that makes this surface trustworthy: the CRDT block now
        // equals the editor buffer (edit mirroring is faithful).
        assert_eq!(block_text(&blocks, &target).unwrap(), "Xhello");
    }

    #[tokio::test]
    async fn save_clears_dirty_and_moves_the_checkpoint() {
        let (blocks, target) = seeded(b"hello").await;
        let mut sessions = EditorSessions::new();
        let (id, _) = sessions.open(RC_PATH, target, &blocks).unwrap();

        sessions.keys(id, "iX<Esc>", &blocks).unwrap();
        let st = sessions.save(id).unwrap();
        assert_eq!(st.text, "Xhello");
        assert!(!st.dirty, "save must clear dirty");
    }

    #[tokio::test]
    async fn quit_rolls_the_block_back_to_the_open_checkpoint() {
        let (blocks, target) = seeded(b"hello").await;
        let mut sessions = EditorSessions::new();
        let (id, _) = sessions.open(RC_PATH, target, &blocks).unwrap();

        // Delete the first char, mirror lands on the block...
        sessions.keys(id, "x", &blocks).unwrap();
        assert_eq!(block_text(&blocks, &target).unwrap(), "ello");

        // ...then ZQ restores the block to what we opened.
        sessions.quit(id, &blocks).unwrap();
        assert_eq!(block_text(&blocks, &target).unwrap(), "hello");
        assert!(!sessions.is_open(id), "quit drops the session");
    }

    #[tokio::test]
    async fn quit_rolls_back_to_last_save_not_to_original() {
        let (blocks, target) = seeded(b"hello").await;
        let mut sessions = EditorSessions::new();
        let (id, _) = sessions.open(RC_PATH, target, &blocks).unwrap();

        sessions.keys(id, "iX<Esc>", &blocks).unwrap(); // -> "Xhello"
        sessions.save(id).unwrap(); // checkpoint = "Xhello"
        sessions.keys(id, "iY<Esc>", &blocks).unwrap(); // -> "YXhello"
        sessions.quit(id, &blocks).unwrap();

        // Rolls back to the *saved* checkpoint, keeping the saved edit.
        assert_eq!(block_text(&blocks, &target).unwrap(), "Xhello");
    }

    #[tokio::test]
    async fn keys_on_a_dropped_session_fails_loud() {
        let (blocks, target) = seeded(b"hello").await;
        let mut sessions = EditorSessions::new();
        let (id, _) = sessions.open(RC_PATH, target, &blocks).unwrap();
        sessions.quit(id, &blocks).unwrap();
        let err = sessions.keys(id, "x", &blocks).unwrap_err();
        assert!(err.contains("no such session"), "got: {err}");
    }
}
