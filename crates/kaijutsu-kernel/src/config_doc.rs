//! Shared CRDT config-document model.
//!
//! One backend, [`ConfigCrdtFs`], owns all config-class content as CRDT
//! documents — `/etc/rc`, `/etc/config`, `/etc/client`, and `/etc/midi` are
//! each one mounted instance of it. (An earlier design split rc and config
//! across two backends, `ConfigCrdtBackend` and `ConfigCrdtFs`; the former was
//! deleted when the config TOMLs moved onto this same CRDT-native model — see
//! `config_seed.rs` and `config_crdt_fs.rs` module docs for that history.)
//! Every mount shares one doc model — a single-block [`DocKind::Config`] (or
//! [`DocKind::Symlink`]) document keyed by a deterministic UUIDv5 of its path
//! — so the four trees can never drift into near-identical-but-subtly-different
//! stores. These free functions ARE that shared model; `ConfigCrdtFs` calls
//! them rather than re-deriving the id or re-implementing the read.
//!
//! [`ConfigCrdtFs`]: crate::runtime::config_crdt_fs::ConfigCrdtFs

use kaijutsu_types::{BlockId, ContextId};
use kaijutsu_types::DocKind;

use crate::block_store::SharedBlockStore;

/// Deterministic `ContextId` for a config/rc path.
///
/// UUIDv5 over the URL namespace, so the same path always maps to the same
/// document id — kernel-wide and stable across restarts (the persisted
/// `documents` row keys on it). Config documents aren't real contexts, but the
/// `BlockStore` is keyed by `ContextId`, so they borrow the same address space.
pub fn config_context_id(path: &str) -> ContextId {
    let uuid = uuid::Uuid::new_v5(
        &uuid::Uuid::NAMESPACE_URL,
        format!("kaijutsu:config:{}", path).as_bytes(),
    );
    ContextId::from_bytes(*uuid.as_bytes())
}

/// `DocKind` every config/rc document carries. Centralized so a doc seeded by
/// one backend is recognized (and enumerated) by the other.
pub const CONFIG_DOC_KIND: DocKind = DocKind::Config;

/// The id of a config doc's first (and only) block, or `None` when the
/// document is absent or — the halted-replay case — registered but blockless.
pub fn first_block_id(blocks: &SharedBlockStore, ctx: ContextId) -> Option<BlockId> {
    blocks.get(ctx)?.doc.blocks_ordered().first().map(|b| b.id)
}

/// Read the content of a config doc's single block. `None` when the document is
/// absent or blockless (caller decides whether that is "not found" or a seed
/// opportunity — this never invents empty content).
pub fn read_content(blocks: &SharedBlockStore, ctx: ContextId) -> Option<String> {
    blocks
        .get(ctx)?
        .doc
        .blocks_ordered()
        .first()
        .map(|b| b.content.clone())
}

/// Char length of a config doc's single block (CRDT text edits are char-, not
/// byte-, indexed, so this is the correct end offset for a full replace).
/// `0` when the document is absent or blockless.
pub fn content_char_len(blocks: &SharedBlockStore, ctx: ContextId) -> usize {
    read_content(blocks, ctx)
        .map(|c| c.chars().count())
        .unwrap_or(0)
}
