//! Application configuration constants.
//!
//! Centralizes hardcoded values for easier configuration and documentation.


/// Default kernel ID to attach to after connecting.
pub const DEFAULT_KERNEL_ID: &str = "lobby";

/// Default window dimensions.
pub const DEFAULT_WINDOW_WIDTH: u32 = 1280;
pub const DEFAULT_WINDOW_HEIGHT: u32 = 800;

// ============================================================================
// Z-INDEX LAYERS
// ============================================================================

/// Z-Index layers for UI element stacking.
///
/// The UI is organized into layers from back to front:
/// - **Chrome** (0): Header, status bar - always visible base layer
/// - **Content** (10): Main content area (dashboard, conversation)
/// - **Constellation** (15): Context node graph overlay (Map/Orbital modes)
/// - **HUD** (50): Heads-up display panels
/// - **Modal** (100): Input layer, dropdowns, command palette
/// - **Toast** (150): Notifications, transient messages
///
/// Use these constants instead of magic numbers for maintainability.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ZLayer;

impl ZLayer {
    /// Base chrome layer (header, status bar)
    pub const CHROME: i32 = 0;
    /// Main content area (dashboard columns, conversation blocks)
    pub const CONTENT: i32 = 10;
    /// Constellation overlay (context nodes, connections)
    pub const CONSTELLATION: i32 = 15;
    /// Cursor overlay (above constellation, in focused document)
    pub const CURSOR: i32 = 20;
    /// HUD panels (agent status, keybinds, etc.)
    pub const HUD: i32 = 50;
    /// Modal overlays (input layer, command palette)
    pub const MODAL: i32 = 100;
    /// Dropdown menus (above modals)
    pub const DROPDOWN: i32 = 200;
    /// Toast notifications
    pub const TOAST: i32 = 250;
}

// Usage: ZIndex(ZLayer::CONTENT)
