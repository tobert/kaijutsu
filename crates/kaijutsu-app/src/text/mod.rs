//! Text rendering module using Vello (vector graphics).
//!
//! Provides GPU-accelerated text rendering via bevy_vello, which uses
//! Parley for text layout and Vello for vector path rendering.

pub mod components;
pub mod markdown;
pub mod rich;
pub mod sparkline;
mod plugin;
mod resources;

pub use components::{KjText, KjTextEffects, bevy_color_to_brush, vello_style};
pub use plugin::KjTextPlugin;
pub use resources::{FontHandles, TextMetrics};
pub use rich::{RichContent, detect_rich_content};
