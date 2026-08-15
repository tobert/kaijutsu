//! Cell module — facade re-exporting from view/.
//!
//! All component types and systems now live in `crate::view`. This module
//! re-exports them so existing `crate::cell::X` imports continue to work.

pub mod block_border;
pub mod plugin;

// Re-export everything from view/ so crate::cell::X still resolves
//
// CachedDocument (= kaijutsu_client::DocumentEntry) has no `crate::cell::`
// importer left as of the change-feed migration (docs/change-feed.md) — the
// last one, view/sync.rs, now reads `doc_cache.get()`'s return type
// structurally rather than naming it. Kept as a facade export for whatever
// reaches for it next, same as the wildcard re-export just below.
#[allow(unused_imports)]
pub use crate::view::document::CachedDocument;
pub use crate::view::lifecycle::EditorEntities;
#[allow(unused_imports)]
pub use crate::view::*;

pub use plugin::CellPlugin;
