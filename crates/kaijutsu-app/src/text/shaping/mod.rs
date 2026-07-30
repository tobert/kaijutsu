//! Kaijutsu-owned text shaping.
//!
//! The `VelloFont` asset, its parley `layout()`, the style/axes/alignment
//! types, the `.ttf` loader, and the shared font context. `layout()` is the
//! single shaping source for both text paths (MSDF extraction and
//! scene-rendered rich text), which keeps their metrics identical.
//!
//! Nothing here calls into vello's renderer — `layout()` is pure parley.
//! `VelloTextStyle`/`VelloFont` only *name* `Brush` as a color type, and
//! that type now comes from the `peniko` crate directly (a direct
//! dependency, not `vello::peniko`) — `peniko::Brush` and
//! `vello::peniko::Brush` are the identical type (vello re-exports peniko
//! verbatim), so this was a zero-risk import repoint, not a rename. The
//! `Vello*` names themselves are left alone: renaming them would ripple
//! into every consumer (including `ui/dock.rs`, off-limits mid-edit by
//! another agent at the time of this pass) for a naming nicety, not a
//! behavior change.

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
