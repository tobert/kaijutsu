//! In-app editor open path (the `vi`/`edit` builtin + `kj rc edit` default).
//!
//! One job today: resolve a VFS path to the CRDT `(context, block)` that *owns*
//! its text, so an editor binds to the source of truth — never a copy. The peer
//! signal that tells the app to open the editor builds on this (added next).
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
