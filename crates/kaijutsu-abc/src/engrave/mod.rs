//! Music engraving: ABC AST → positioned IR → SVG.
//!
//! The pipeline is:
//! 1. `layout::engrave(tune, options)` → `Vec<EngravingElement>`
//! 2. `svg::to_svg(elements, margin)` → `String`
//!
//! Or use the convenience function `engrave_to_svg` which does both.

pub mod font;
pub mod ir;
pub mod layout;
pub mod svg;

pub use ir::{EngravingElement, EngravingOptions, SourceSpan};

use crate::ast::Tune;

/// Convenience: engrave a tune directly to an SVG string.
pub fn engrave_to_svg(tune: &Tune, options: &EngravingOptions) -> String {
    let elements = layout::engrave(tune, options);
    svg::to_svg(&elements, options.margin, &options.color)
}
