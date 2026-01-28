//! Theme system for Kaijutsu
//!
//! Provides a scriptable theming system via Rhai, inspired by the
//! Tokyo-midnight palette from nvim/wezterm configurations.

use bevy::math::Vec4;
use bevy::prelude::*;

/// ANSI 16-color palette for terminal/syntax rendering.
///
/// Standard ANSI colors (0-7 normal, 8-15 bright):
/// - 0/8: Black/Bright Black (gray)
/// - 1/9: Red/Bright Red
/// - 2/10: Green/Bright Green
/// - 3/11: Yellow/Bright Yellow
/// - 4/12: Blue/Bright Blue
/// - 5/13: Magenta/Bright Magenta
/// - 6/14: Cyan/Bright Cyan
/// - 7/15: White/Bright White
#[derive(Clone, Debug)]
pub struct AnsiColors {
    pub black: Color,
    pub red: Color,
    pub green: Color,
    pub yellow: Color,
    pub blue: Color,
    pub magenta: Color,
    pub cyan: Color,
    pub white: Color,
    // Bright variants (8-15)
    pub bright_black: Color,
    pub bright_red: Color,
    pub bright_green: Color,
    pub bright_yellow: Color,
    pub bright_blue: Color,
    pub bright_magenta: Color,
    pub bright_cyan: Color,
    pub bright_white: Color,
}

impl Default for AnsiColors {
    fn default() -> Self {
        // Tokyo Night inspired ANSI palette
        Self {
            black: Color::srgb(0.10, 0.11, 0.15),         // #1a1b26
            red: Color::srgb(0.97, 0.38, 0.45),           // #f7616a
            green: Color::srgb(0.62, 0.81, 0.42),         // #9ece6a
            yellow: Color::srgb(0.89, 0.79, 0.49),        // #e0c97d
            blue: Color::srgb(0.48, 0.64, 0.97),          // #7aa2f7
            magenta: Color::srgb(0.73, 0.47, 0.91),       // #bb79e8
            cyan: Color::srgb(0.49, 0.85, 0.82),          // #7dd9d1
            white: Color::srgb(0.78, 0.80, 0.85),         // #c8ccd9
            // Bright variants
            bright_black: Color::srgb(0.27, 0.29, 0.35),  // #444b59
            bright_red: Color::srgb(1.00, 0.53, 0.58),    // #ff8894
            bright_green: Color::srgb(0.72, 0.91, 0.52),  // #b8e885
            bright_yellow: Color::srgb(1.00, 0.89, 0.59), // #ffe397
            bright_blue: Color::srgb(0.58, 0.74, 1.00),   // #94bdff
            bright_magenta: Color::srgb(0.83, 0.57, 1.00),// #d491ff
            bright_cyan: Color::srgb(0.59, 0.95, 0.92),   // #96f2eb
            bright_white: Color::srgb(0.90, 0.90, 0.90),  // #e5e5e5
        }
    }
}

/// Application theme resource.
///
/// Contains all colors used throughout the application, from base UI
/// to vim-style mode colors and cursor colors.
#[derive(Resource, Clone)]
pub struct Theme {
    // ═══════════════════════════════════════════════════════════════════════
    // Base UI colors
    // ═══════════════════════════════════════════════════════════════════════
    pub bg: Color,
    pub panel_bg: Color,
    pub fg: Color,
    pub fg_dim: Color,
    pub accent: Color,
    pub accent2: Color,
    pub border: Color,
    pub selection_bg: Color,

    // Row type colors (left border accents)
    pub row_tool: Color,
    pub row_result: Color,

    // ═══════════════════════════════════════════════════════════════════════
    // Block text colors (per-block-type for semantic distinction)
    // ═══════════════════════════════════════════════════════════════════════
    /// User message text color (soft white)
    pub block_user: Color,
    /// Assistant message text color (light blue)
    pub block_assistant: Color,
    /// Thinking block text color (dim gray for de-emphasis)
    pub block_thinking: Color,
    /// Tool call block text color (amber)
    pub block_tool_call: Color,
    /// Tool result block text color (green for success)
    pub block_tool_result: Color,
    /// Tool error block text color (red)
    pub block_tool_error: Color,
    /// Shell command block text color (cyan)
    pub block_shell_cmd: Color,
    /// Shell output block text color (light gray)
    pub block_shell_output: Color,
    /// Focused block highlight color (border/background accent)
    #[allow(dead_code)]
    pub block_focus: Color,

    // ═══════════════════════════════════════════════════════════════════════
    // Semantic colors
    // ═══════════════════════════════════════════════════════════════════════
    pub error: Color,
    pub warning: Color,
    pub success: Color,

    // ═══════════════════════════════════════════════════════════════════════
    // Mode colors (vim-style, for mode indicator)
    // ═══════════════════════════════════════════════════════════════════════
    pub mode_normal: Color,
    pub mode_chat: Color,
    pub mode_shell: Color,
    pub mode_visual: Color,

    // ═══════════════════════════════════════════════════════════════════════
    // Cursor colors (shader Vec4: [r, g, b, a])
    // ═══════════════════════════════════════════════════════════════════════
    pub cursor_normal: Vec4,
    pub cursor_insert: Vec4,
    pub cursor_visual: Vec4,

    // ═══════════════════════════════════════════════════════════════════════
    // ANSI palette (for future terminal/syntax use)
    // ═══════════════════════════════════════════════════════════════════════
    pub ansi: AnsiColors,

    // ═══════════════════════════════════════════════════════════════════════
    // Frame Configuration (9-slice system)
    // ═══════════════════════════════════════════════════════════════════════

    // Frame structure
    pub frame_corner_size: f32,
    pub frame_edge_thickness: f32,
    pub frame_content_padding: f32,

    // Frame colors (per-state)
    pub frame_base: Color,      // Default frame color
    pub frame_focused: Color,   // When focused in normal mode
    pub frame_insert: Color,    // Input modes (Chat/Shell)
    pub frame_visual: Color,    // Visual mode
    pub frame_unfocused: Color, // Lost focus
    pub frame_edge: Color,      // Edge color (usually dimmer)

    // Frame shader params [glow_radius, intensity, pulse_speed, bracket_length]
    pub frame_params_base: Vec4,
    pub frame_params_focused: Vec4,
    pub frame_params_unfocused: Vec4,

    // Edge dimming multipliers (applied to edge colors for visual hierarchy)
    pub frame_edge_dim_unfocused: Vec4, // Color multiplier when unfocused
    pub frame_edge_dim_focused: Vec4,   // Color multiplier when focused

    // ═══════════════════════════════════════════════════════════════════════
    // Shader Effect Parameters (GPU-reactive via ShaderEffectContext)
    // ═══════════════════════════════════════════════════════════════════════
    pub effect_glow_radius: f32,
    pub effect_glow_intensity: f32,
    pub effect_glow_falloff: f32,
    pub effect_sheen_speed: f32,
    pub effect_sheen_sparkle_threshold: f32,
    pub effect_breathe_speed: f32,
    pub effect_breathe_amplitude: f32,

    // Chasing border effect parameters
    pub effect_chase_speed: f32,
    pub effect_chase_width: f32,
    pub effect_chase_glow_radius: f32,
    pub effect_chase_glow_intensity: f32,
    pub effect_chase_color_cycle: f32, // 0 = static color, >0 = rainbow cycle speed

    // ═══════════════════════════════════════════════════════════════════════
    // Input Area Configuration
    // ═══════════════════════════════════════════════════════════════════════

    /// Height of the minimized chasing line (default: 6px)
    pub input_minimized_height: f32,
    /// Default height when docked (default: 80px)
    pub input_docked_height: f32,
    /// Overlay width as percentage of window width (default: 0.6 = 60%)
    pub input_overlay_width_pct: f32,
    /// Backdrop color when in overlay mode
    pub input_backdrop_color: Color,
}

impl Default for Theme {
    fn default() -> Self {
        Self {
            // Base UI - Tokyo Night inspired
            bg: Color::srgb(0.05, 0.07, 0.09),
            panel_bg: Color::srgba(0.05, 0.07, 0.09, 0.9),
            fg: Color::srgb(0.9, 0.9, 0.9),
            fg_dim: Color::srgb(0.5, 0.5, 0.5),
            accent: Color::srgb(0.34, 0.65, 1.0),
            accent2: Color::srgb(0.97, 0.47, 0.73),
            border: Color::srgb(0.19, 0.21, 0.24),
            selection_bg: Color::srgba(0.34, 0.65, 1.0, 0.2),

            // Row type colors
            row_tool: Color::srgb(0.83, 0.6, 0.13),    // Orange - tool calls
            row_result: Color::srgb(0.25, 0.73, 0.31), // Green - tool results

            // Block text colors - Tokyo Night inspired palette
            block_user: Color::srgb(0.90, 0.90, 0.92),         // Soft white
            block_assistant: Color::srgb(0.58, 0.74, 1.00),    // Light blue (#94bdff)
            block_thinking: Color::srgb(0.45, 0.47, 0.55),     // Dim gray (de-emphasized)
            block_tool_call: Color::srgb(0.89, 0.79, 0.49),    // Amber (ansi.yellow)
            block_tool_result: Color::srgb(0.62, 0.81, 0.42),  // Green (ansi.green)
            block_tool_error: Color::srgb(0.97, 0.38, 0.45),   // Red (ansi.red)
            block_shell_cmd: Color::srgb(0.49, 0.85, 0.82),    // Cyan (ansi.cyan)
            block_shell_output: Color::srgb(0.70, 0.72, 0.78), // Light gray
            block_focus: Color::srgba(0.48, 0.64, 0.97, 0.3),      // Soft blue highlight (#7aa2f7)

            // Semantic
            error: Color::srgb(0.97, 0.38, 0.45),     // Red
            warning: Color::srgb(0.89, 0.79, 0.49),   // Yellow
            success: Color::srgb(0.62, 0.81, 0.42),   // Green

            // Mode colors (vim-style)
            mode_normal: Color::srgb(0.5, 0.5, 0.5),  // Dim gray (matches fg_dim)
            mode_chat: Color::srgb(0.4, 0.8, 0.4),    // Green (chat with LLM)
            mode_shell: Color::srgb(0.3, 0.9, 0.7),   // Terminal green (kaish REPL)
            mode_visual: Color::srgb(0.7, 0.4, 0.9),  // Purple

            // Cursor colors - soft aesthetic terminal style
            cursor_normal: Vec4::new(0.85, 0.92, 1.0, 0.85),  // Soft ice blue
            cursor_insert: Vec4::new(1.0, 0.5, 0.75, 0.95),   // Hot pink
            cursor_visual: Vec4::new(0.95, 0.85, 0.6, 0.9),   // Warm gold

            // ANSI palette
            ansi: AnsiColors::default(),

            // Frame configuration - cyberpunk style defaults
            frame_corner_size: 48.0,
            frame_edge_thickness: 6.0,
            frame_content_padding: 8.0,

            // Frame colors - soft purple base (Tokyo Night aesthetic)
            frame_base: Color::srgb(0.73, 0.60, 0.97),      // #bb9af7 soft purple
            frame_focused: Color::srgb(0.73, 0.60, 0.97),   // Same as base when focused
            frame_insert: Color::srgb(0.62, 0.81, 0.42),    // #9ece6a green - input modes
            frame_visual: Color::srgb(0.48, 0.64, 0.97),    // #7aa2f7 blue - reuse accent
            frame_unfocused: Color::srgba(0.34, 0.37, 0.54, 0.6), // #565f89 dimmed
            frame_edge: Color::srgba(0.73, 0.60, 0.97, 0.5), // Dimmer purple

            // Frame shader params: [glow_radius, intensity, pulse_speed, bracket_length]
            frame_params_base: Vec4::new(0.15, 1.2, 1.5, 0.7),
            frame_params_focused: Vec4::new(0.2, 1.5, 2.0, 0.7),
            frame_params_unfocused: Vec4::new(0.1, 0.6, 0.8, 0.7),

            // Edge dimming: [r_mult, g_mult, b_mult, a_mult]
            frame_edge_dim_unfocused: Vec4::new(0.5, 0.5, 0.5, 0.6),
            frame_edge_dim_focused: Vec4::new(0.7, 0.7, 0.7, 0.8),

            // Shader effect parameters - cyberpunk defaults
            effect_glow_radius: 0.3,
            effect_glow_intensity: 0.5,
            effect_glow_falloff: 2.5,
            effect_sheen_speed: 0.15,
            effect_sheen_sparkle_threshold: 0.92,
            effect_breathe_speed: 1.9,
            effect_breathe_amplitude: 0.1,

            // Chasing border defaults
            effect_chase_speed: 0.25,
            effect_chase_width: 0.10,
            effect_chase_glow_radius: 0.08,
            effect_chase_glow_intensity: 0.6,
            effect_chase_color_cycle: 0.15, // Rainbow cycle speed

            // Input area defaults
            input_minimized_height: 6.0,
            input_docked_height: 80.0,
            input_overlay_width_pct: 0.6,
            input_backdrop_color: Color::srgba(0.0, 0.0, 0.0, 0.4),
        }
    }
}

/// Helper to convert Bevy Color to Vec4 (for shader uniforms).
pub fn color_to_vec4(color: Color) -> Vec4 {
    let srgba = color.to_srgba();
    Vec4::new(srgba.red, srgba.green, srgba.blue, srgba.alpha)
}

/// Helper to convert Bevy Color to linear Vec4 (for GPU storage buffers).
pub fn color_to_linear_vec4(color: Color) -> Vec4 {
    let linear = color.to_linear();
    Vec4::new(linear.red, linear.green, linear.blue, linear.alpha)
}
