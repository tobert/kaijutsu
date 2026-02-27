//! Text rendering module using Vello (vector graphics).
//!
//! Provides GPU-accelerated text rendering via bevy_vello, which uses
//! Parley for text layout and Vello for vector path rendering.

pub mod components;
#[allow(dead_code)] // Phase 4: vello_label/vello_text used when call sites adopt FontHandles
pub mod helpers;
#[allow(dead_code)] // Phase 4: rich markdown rendering via Parley spans
pub mod markdown;
mod plugin;
mod resources;

pub use components::{KjText, KjTextEffects, KjUiText, bevy_color_to_brush};
pub use plugin::KjTextPlugin;
pub use resources::{FontHandles, TextMetrics};
