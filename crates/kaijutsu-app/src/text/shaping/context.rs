//! The parley shaping context.
//!
//! One global `FontContext` holds the shared font collection; each thread gets
//! a cheap clone plus its own `LayoutContext`. This is the single shaping
//! source feeding both text paths (MSDF extraction and scene-rendered rich
//! text), which is what keeps their metrics identical. Ported verbatim from
//! the fork's `integrations::text::context` — `system_fonts: false` because we
//! register our own bundled fonts via the asset loader.

use std::cell::RefCell;

use parley::{
    FontContext, LayoutContext,
    fontique::{Collection, CollectionOptions, SourceCache},
};
use peniko::Brush;

static GLOBAL_FONT_CONTEXT: std::sync::OnceLock<FontContext> = std::sync::OnceLock::new();

pub(crate) fn get_global_font_context() -> &'static FontContext {
    GLOBAL_FONT_CONTEXT.get_or_init(|| FontContext {
        collection: Collection::new(CollectionOptions {
            shared: true,
            system_fonts: false,
        }),
        source_cache: SourceCache::new_shared(),
    })
}

thread_local! {
    pub(crate) static LOCAL_FONT_CONTEXT: RefCell<Option<FontContext>> = const { RefCell::new(None) };
    pub(crate) static LOCAL_LAYOUT_CONTEXT: RefCell<LayoutContext<Brush>> =
        RefCell::new(LayoutContext::new());
    /// The conversation surface's ANSI currency (`text::ansi::StyledBrush`).
    ///
    /// A second context rather than a second *font* context: `LayoutContext`
    /// is brush-typed, so a layout in one currency cannot be built from the
    /// other's cache. The shared `FontContext` above — which holds the actual
    /// font collection — is still the one source both shape against, so
    /// metrics stay identical across the two.
    pub(crate) static LOCAL_STYLED_LAYOUT_CONTEXT:
        RefCell<LayoutContext<crate::text::ansi::StyledBrush>> =
        RefCell::new(LayoutContext::new());
}
