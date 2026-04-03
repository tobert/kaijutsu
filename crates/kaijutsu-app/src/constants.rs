//! Application configuration constants.
//!
//! Centralizes hardcoded values for easier configuration and documentation.

/// Initial window dimensions (safe default before monitor info is available).
/// The `adapt_window_to_monitor` system resizes to fit the actual display.
pub const INITIAL_WINDOW_WIDTH: u32 = 960;
pub const INITIAL_WINDOW_HEIGHT: u32 = 600;

/// Fraction of monitor's logical size for the adapted window.
pub const WINDOW_WIDTH_FRACTION: f32 = 0.75;
pub const WINDOW_HEIGHT_FRACTION: f32 = 0.80;

// ============================================================================
// Z-INDEX LAYERS
// ============================================================================

/// Z-Index layers for UI element stacking.
///
/// The UI is organized into layers from back to front:
/// - **Content** (10): Main content area (dashboard, conversation)
/// - **Cursor** (20): Cursor overlay in focused document
/// - **HUD** (50): Dock containers (North/South) and HUD panels
/// - **Modal** (100): Input layer, dropdowns, command palette
/// - **Dropdown** (200): Dropdown menus above modals
/// - **Toast** (250): Notifications, transient messages
///
/// Use these constants instead of magic numbers for maintainability.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ZLayer;

impl ZLayer {
    /// Main content area (dashboard columns, conversation blocks)
    pub const CONTENT: i32 = 10;
    /// Cursor overlay (reserved, cursor now drawn in Vello scene)
    #[allow(dead_code)]
    pub const CURSOR: i32 = 20;
    /// HUD panels (agent status, keybinds, etc.)
    pub const HUD: i32 = 50;
    /// Modal overlays (input layer, command palette)
    pub const MODAL: i32 = 100;
    /// Dropdown menus (above modals)
    #[allow(dead_code)]
    pub const DROPDOWN: i32 = 200;
    /// Toast notifications
    #[allow(dead_code)]
    pub const TOAST: i32 = 250;
}

// Usage: ZIndex(ZLayer::CONTENT)
