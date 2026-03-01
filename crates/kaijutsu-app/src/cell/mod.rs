//! Cell module — facade re-exporting from view/.
//!
//! All component types and systems now live in `crate::view`. This module
//! re-exports them so existing `crate::cell::X` imports continue to work.

pub mod block_border;
pub mod plugin;

// Re-export everything from view/ so crate::cell::X still resolves
#[allow(unused_imports)]
pub use crate::view::*;
pub use crate::view::document::CachedDocument;
pub use crate::view::lifecycle::EditorEntities;

pub use plugin::CellPlugin;
