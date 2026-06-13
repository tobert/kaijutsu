//! Kaijutsu-owned text shaping.
//!
//! The `VelloFont` asset, its parley `layout()`, the style/axes/alignment
//! types, the `.ttf` loader, and the shared font context. `layout()` is the
//! single shaping source for both text paths (MSDF extraction and
//! scene-rendered rich text), which keeps their metrics identical.

mod context;
mod font;
mod loader;
mod types;

pub use font::VelloFont;
pub use loader::VelloFontLoader;
pub use types::{VelloFontAxes, VelloTextAlign, VelloTextStyle};

use bevy::prelude::*;

/// Registers the `VelloFont` asset and its `.ttf` loader.
///
/// Bevy disambiguates asset loaders by the requested asset type.
pub struct ShapingPlugin;

impl Plugin for ShapingPlugin {
    fn build(&self, app: &mut App) {
        app.init_asset::<VelloFont>()
            .init_asset_loader::<VelloFontLoader>();
    }
}
