//! Theme system for Kaijutsu
//!
//! Provides a scriptable theming system via Rhai. The built-in default
//! is the Kaijutsu theme; users can override any field via `theme.rhai`.

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
    /// Drift push block text color (cyan — conversational)
    pub block_drift_push: Color,
    /// Drift pull/distill block text color (blue — substantive)
    pub block_drift_pull: Color,
    /// Drift merge block text color (purple — structural)
    pub block_drift_merge: Color,
    /// Drift commit block text color (green — like git)
    pub block_drift_commit: Color,
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

    // ═══════════════════════════════════════════════════════════════════════
    // Markdown Rendering Colors
    // ═══════════════════════════════════════════════════════════════════════

    /// Heading text color (bright accent)
    pub md_heading_color: Color,
    /// Inline `code` foreground color
    pub md_code_fg: Color,
    /// Fenced code block foreground color
    pub md_code_block_fg: Color,
    /// Bold/strong emphasis color (None = inherit base block color)
    pub md_strong_color: Option<Color>,

    // ═══════════════════════════════════════════════════════════════════════
    // Font Rendering Quality (MSDF text)
    // ═══════════════════════════════════════════════════════════════════════

    /// Stem darkening strength (0.0-0.5). Thickens thin strokes at small font sizes.
    /// ~0.15 = ClearType-like weight for 12-16px text.
    pub font_stem_darkening: f32,
    /// Hinting strength (0.0-1.0). Sharpens horizontal strokes (stems, crossbars).
    pub font_hint_amount: f32,
    /// Enable temporal anti-aliasing for smoother text edges.
    pub font_taa_enabled: bool,
    /// Number of frames for TAA to converge (4-16). Lower = faster fade-in.
    pub font_taa_convergence_frames: u32,
    /// Initial blend weight (0.3-0.9). Higher = more visible on first frame.
    pub font_taa_initial_weight: f32,
    /// Final blend weight (0.05-0.3). Lower = more temporal smoothing.
    pub font_taa_final_weight: f32,
    /// Horizontal stroke AA scale (1.0-1.3). Wider AA for vertical strokes.
    pub font_horz_scale: f32,
    /// Vertical stroke AA scale (0.5-0.8). Sharper AA for horizontal strokes.
    pub font_vert_scale: f32,
    /// SDF threshold for text rendering (0.45-0.55). Lower = thicker strokes.
    pub font_text_bias: f32,
    /// Gamma correction for alpha (< 1.0 widens AA for light-on-dark, > 1.0 for dark-on-light).
    /// Default 0.85 compensates for perceptual thinning of light text on dark backgrounds.
    pub font_gamma_correction: f32,

    // ═══════════════════════════════════════════════════════════════════════
    // Font Effects (MSDF text)
    // ═══════════════════════════════════════════════════════════════════════

    /// Glow intensity (0.0-1.0). 0 = off.
    pub font_glow_intensity: f32,
    /// Glow spread in pixels (0.5-10.0).
    pub font_glow_spread: f32,
    /// Glow color.
    pub font_glow_color: Color,
    /// Enable rainbow color cycling effect.
    pub font_rainbow: bool,

    // ═══════════════════════════════════════════════════════════════════════
    // Constellation Configuration
    // ═══════════════════════════════════════════════════════════════════════

    /// Base radius for radial tree root ring (pixels)
    pub constellation_base_radius: f32,
    /// Spacing between concentric rings in radial tree (pixels)
    pub constellation_ring_spacing: f32,
    /// Node orb size when idle (pixels)
    pub constellation_node_size: f32,
    /// Node orb size when focused (pixels)
    pub constellation_node_size_focused: f32,
    /// Node glow color for idle state
    pub constellation_node_glow_idle: Color,
    /// Node glow color for active state
    pub constellation_node_glow_active: Color,
    /// Node glow color for streaming state
    pub constellation_node_glow_streaming: Color,
    /// Node glow color for error state
    pub constellation_node_glow_error: Color,
    /// Card width in pixels
    pub constellation_card_width: f32,
    /// Card height in pixels
    pub constellation_card_height: f32,
    /// Card border thickness in pixels
    pub constellation_card_border_thickness: f32,
    /// Card corner radius in pixels
    pub constellation_card_corner_radius: f32,
    /// Card glow radius (0.0-1.0)
    pub constellation_card_glow_radius: f32,
    /// Card glow intensity (0.0-1.0)
    pub constellation_card_glow_intensity: f32,
    /// Connection line glow intensity (0.0-1.0)
    pub constellation_connection_glow: f32,
    /// Connection line color
    pub constellation_connection_color: Color,
    /// Max particles per context for streaming effects
    pub constellation_particle_budget: u32,
    /// Agent color: default (dim cyan) — used when provider is unknown
    pub agent_color_default: Color,
    /// Agent color: human user (electric cyan)
    pub agent_color_human: Color,
    /// Agent color: Anthropic/Claude (hot pink)
    pub agent_color_claude: Color,
    /// Agent color: Google/Gemini (gold)
    pub agent_color_gemini: Color,
    /// Agent color: local models (matrix green)
    pub agent_color_local: Color,
    /// Agent color: DeepSeek (orange)
    pub agent_color_deepseek: Color,

    // ═══════════════════════════════════════════════════════════════════════
    // Block Border Configuration (shader-rendered per-block borders)
    // ═══════════════════════════════════════════════════════════════════════

    /// Tool call border color (amber, default from block_tool_call)
    pub block_border_tool_call: Color,
    /// Tool result border color (green, default from block_tool_result)
    pub block_border_tool_result: Color,
    /// Error border color (red, default from block_tool_error)
    pub block_border_error: Color,
    /// Thinking border color (dim gray, default from block_thinking)
    pub block_border_thinking: Color,
    /// Drift border color (blue, default from block_drift_pull)
    pub block_border_drift: Color,
    /// Border thickness in pixels
    pub block_border_thickness: f32,
    /// Corner radius in pixels
    pub block_border_corner_radius: f32,
    /// Glow spread radius (0.0-1.0)
    pub block_border_glow_radius: f32,
    /// Glow intensity (0.0-1.0)
    pub block_border_glow_intensity: f32,
    /// Base padding inside borders (pixels)
    pub block_border_padding: f32,

    // ═══════════════════════════════════════════════════════════════════════
    // Compose block (input area)
    // ═══════════════════════════════════════════════════════════════════════
    pub compose_border: Color,
    pub compose_bg: Color,

    // ═══════════════════════════════════════════════════════════════════════
    // Modal overlays
    // ═══════════════════════════════════════════════════════════════════════
    pub modal_backdrop: Color,

    // ═══════════════════════════════════════════════════════════════════════
    // User/assistant text block borders (transparent = disabled)
    // ═══════════════════════════════════════════════════════════════════════
    pub block_border_user: Color,
    pub block_border_assistant: Color,
}

impl Default for Theme {
    fn default() -> Self {
        Self {
            // Base UI
            bg: Color::srgb(0.04, 0.04, 0.06),                  // #0a0a0f near-black
            panel_bg: Color::srgba(0.04, 0.04, 0.06, 0.95),     // Almost opaque
            fg: Color::srgb(0.88, 0.94, 1.00),                  // #e0f0ff cool white
            fg_dim: Color::srgb(0.38, 0.50, 0.63),              // #6080a0 muted cyan-gray
            accent: Color::srgb(0.00, 1.00, 1.00),              // #00ffff electric cyan
            accent2: Color::srgb(1.00, 0.00, 0.50),             // #ff0080 hot pink
            border: Color::srgb(0.19, 0.13, 0.31),              // #302050 deep purple
            selection_bg: Color::srgba(1.00, 0.00, 0.50, 0.25), // #ff0080 pink selection

            // Row type colors
            row_tool: Color::srgb(1.00, 0.67, 0.00),            // #ffaa00 orange
            row_result: Color::srgb(0.00, 1.00, 0.53),          // #00ff88 toxic green

            // Block text colors
            block_user: Color::srgb(0.88, 0.94, 1.00),          // #e0f0ff cool white
            block_assistant: Color::srgb(0.62, 0.81, 0.42),     // #9ece6a terminal green
            block_thinking: Color::srgb(0.38, 0.50, 0.63),      // #6080a0 muted
            block_tool_call: Color::srgb(1.00, 0.67, 0.00),     // #ffaa00 amber
            block_tool_result: Color::srgb(0.00, 1.00, 0.53),   // #00ff88 toxic green
            block_tool_error: Color::srgb(1.00, 0.13, 0.38),    // #ff2060 red
            block_shell_cmd: Color::srgb(0.00, 1.00, 1.00),     // #00ffff cyan
            block_shell_output: Color::srgb(0.88, 0.94, 1.00),  // #e0f0ff cool white
            block_drift_push: Color::srgb(0.00, 1.00, 1.00),    // #00ffff cyan
            block_drift_pull: Color::srgb(0.00, 0.67, 1.00),    // #00aaff electric blue
            block_drift_merge: Color::srgb(1.00, 0.00, 1.00),   // #ff00ff magenta
            block_drift_commit: Color::srgb(0.00, 1.00, 0.53),  // #00ff88 toxic green

            // Semantic
            error: Color::srgb(1.00, 0.13, 0.38),               // #ff2060 red
            warning: Color::srgb(1.00, 0.80, 0.00),             // #ffcc00 electric yellow
            success: Color::srgb(0.00, 1.00, 0.53),             // #00ff88 matrix green

            // Mode colors (vim-style)
            mode_normal: Color::srgb(0.50, 0.56, 0.69),         // #8090b0 cool slate
            mode_chat: Color::srgb(0.00, 1.00, 0.53),           // #00ff88 toxic green
            mode_shell: Color::srgb(1.00, 0.67, 0.00),          // #ffaa00 orange
            mode_visual: Color::srgb(1.00, 0.00, 1.00),         // #ff00ff magenta

            // Cursor colors — maximum glow
            cursor_normal: Vec4::new(0.00, 1.00, 1.00, 0.9),    // #00ffff cyan
            cursor_insert: Vec4::new(1.00, 0.00, 0.50, 1.0),    // #ff0080 hot pink
            cursor_visual: Vec4::new(1.00, 0.00, 1.00, 0.9),    // #ff00ff magenta

            // ANSI palette
            ansi: AnsiColors {
                black: Color::srgb(0.04, 0.04, 0.06),           // #0a0a0f
                red: Color::srgb(1.00, 0.13, 0.38),             // #ff2060
                green: Color::srgb(0.00, 1.00, 0.53),           // #00ff88
                yellow: Color::srgb(1.00, 0.80, 0.00),          // #ffcc00
                blue: Color::srgb(0.00, 0.67, 1.00),            // #00aaff
                magenta: Color::srgb(1.00, 0.00, 1.00),         // #ff00ff
                cyan: Color::srgb(0.00, 1.00, 1.00),            // #00ffff
                white: Color::srgb(0.88, 0.94, 1.00),           // #e0f0ff
                bright_black: Color::srgb(0.25, 0.31, 0.44),    // #405070
                bright_red: Color::srgb(1.00, 0.25, 0.50),      // #ff4080
                bright_green: Color::srgb(0.25, 1.00, 0.67),    // #40ffaa
                bright_yellow: Color::srgb(1.00, 0.93, 0.40),   // #ffee66
                bright_blue: Color::srgb(0.25, 0.80, 1.00),     // #40ccff
                bright_magenta: Color::srgb(1.00, 0.40, 1.00),  // #ff66ff
                bright_cyan: Color::srgb(0.40, 1.00, 1.00),     // #66ffff
                bright_white: Color::srgb(1.00, 1.00, 1.00),    // #ffffff
            },

            // Frame configuration — chunky cyber frames
            frame_corner_size: 56.0,
            frame_edge_thickness: 8.0,
            frame_content_padding: 10.0,

            // Frame colors
            frame_base: Color::srgb(1.00, 0.00, 0.50),          // #ff0080 hot pink
            frame_focused: Color::srgb(0.00, 1.00, 1.00),       // #00ffff cyan
            frame_insert: Color::srgb(0.00, 1.00, 0.53),        // #00ff88 green
            frame_visual: Color::srgb(1.00, 0.00, 1.00),        // #ff00ff magenta
            frame_unfocused: Color::srgba(0.38, 0.25, 0.50, 0.5), // #604080 dim purple
            frame_edge: Color::srgba(1.00, 0.00, 0.50, 0.6),    // #ff0080 pink edges

            // Frame shader params — cranked for maximum glow
            frame_params_base: Vec4::new(0.25, 2.0, 2.5, 0.75),
            frame_params_focused: Vec4::new(0.35, 2.5, 3.0, 0.8),
            frame_params_unfocused: Vec4::new(0.15, 0.8, 1.0, 0.6),

            // Edge dimming — less dim, more glow
            frame_edge_dim_unfocused: Vec4::new(0.6, 0.5, 0.7, 0.7),
            frame_edge_dim_focused: Vec4::new(0.9, 0.8, 1.0, 0.95),

            // Shader effect parameters
            effect_glow_radius: 0.45,
            effect_glow_intensity: 1.2,
            effect_glow_falloff: 1.8,
            effect_sheen_speed: 0.25,
            effect_sheen_sparkle_threshold: 0.88,
            effect_breathe_speed: 2.5,
            effect_breathe_amplitude: 0.18,

            // Chasing border
            effect_chase_speed: 0.20,
            effect_chase_width: 0.10,
            effect_chase_glow_radius: 0.08,
            effect_chase_glow_intensity: 0.6,
            effect_chase_color_cycle: 0.12,

            // Input area defaults
            input_minimized_height: 6.0,
            input_docked_height: 80.0,
            input_overlay_width_pct: 0.6,
            input_backdrop_color: Color::srgba(0.0, 0.0, 0.0, 0.4),

            // Markdown rendering
            md_heading_color: Color::srgb(1.00, 0.00, 1.00),    // #ff00ff magenta
            md_code_fg: Color::srgb(0.00, 1.00, 0.53),          // #00ff88 green
            md_code_block_fg: Color::srgb(0.00, 0.67, 1.00),    // #00aaff blue
            md_strong_color: None,

            // Font rendering quality
            font_stem_darkening: 0.15,
            font_hint_amount: 0.8,
            font_taa_enabled: true,
            font_taa_convergence_frames: 8,
            font_taa_initial_weight: 0.5,
            font_taa_final_weight: 0.1,
            font_horz_scale: 1.1,
            font_vert_scale: 0.6,
            font_text_bias: 0.47,
            font_gamma_correction: 0.78,

            // Font effects — post-process bloom glow
            font_glow_intensity: 0.5,
            font_glow_spread: 4.0,
            font_glow_color: Color::srgba(0.0, 1.0, 1.0, 0.35),
            font_rainbow: false,

            // Constellation
            constellation_base_radius: 120.0,
            constellation_ring_spacing: 160.0,
            constellation_node_size: 160.0,
            constellation_node_size_focused: 200.0,
            constellation_node_glow_idle: Color::srgba(0.3, 0.4, 0.5, 0.3),
            constellation_node_glow_active: Color::srgba(0.00, 1.00, 1.00, 0.7),  // Cyan
            constellation_node_glow_streaming: Color::srgba(0.00, 1.00, 0.53, 0.8), // Green
            constellation_node_glow_error: Color::srgba(1.00, 0.13, 0.38, 0.8),   // Red
            constellation_card_width: 180.0,
            constellation_card_height: 130.0,
            constellation_card_border_thickness: 2.0,
            constellation_card_corner_radius: 6.0,
            constellation_card_glow_radius: 0.4,
            constellation_card_glow_intensity: 0.7,
            constellation_connection_glow: 0.4,
            constellation_connection_color: Color::srgba(0.00, 1.00, 1.00, 0.5),  // Cyan
            constellation_particle_budget: 500,
            agent_color_default: Color::srgba(0.49, 0.85, 0.82, 0.8),   // #7dd9d1 dim cyan
            agent_color_human: Color::srgba(0.49, 0.98, 1.00, 0.9),     // #7df9ff electric cyan
            agent_color_claude: Color::srgba(1.00, 0.43, 0.78, 0.9),    // #ff6ec7 hot pink
            agent_color_gemini: Color::srgba(1.00, 0.84, 0.00, 0.9),    // #ffd700 gold
            agent_color_local: Color::srgba(0.31, 0.98, 0.48, 0.9),     // #50fa7b matrix green
            agent_color_deepseek: Color::srgba(1.00, 0.72, 0.42, 0.9),  // #ffb86c orange

            // Block borders
            block_border_tool_call: Color::srgba(1.00, 0.67, 0.00, 0.6),   // #ffaa00 amber
            block_border_tool_result: Color::srgba(0.00, 1.00, 0.53, 0.4), // #00ff88 green
            block_border_error: Color::srgba(1.00, 0.13, 0.38, 0.8),       // #ff2060 red
            block_border_thinking: Color::srgba(0.38, 0.50, 0.63, 0.3),    // #6080a0 muted
            block_border_drift: Color::srgba(0.00, 0.67, 1.00, 0.5),       // #00aaff blue
            block_border_thickness: 1.5,
            block_border_corner_radius: 4.0,
            block_border_glow_radius: 0.15,
            block_border_glow_intensity: 0.6,
            block_border_padding: 2.0,

            // Compose block — debug defaults (WHITE on BLACK)
            compose_border: Color::WHITE,
            compose_bg: Color::BLACK,

            // Modal backdrop — debug default (WHITE)
            modal_backdrop: Color::WHITE,

            // User/assistant text borders — transparent (opt-in)
            block_border_user: Color::srgba(0.0, 0.0, 0.0, 0.0),
            block_border_assistant: Color::srgba(0.0, 0.0, 0.0, 0.0),
        }
    }
}

/// Helper to convert Bevy Color to Vec4 (for shader uniforms).
pub fn color_to_vec4(color: Color) -> Vec4 {
    let srgba = color.to_srgba();
    Vec4::new(srgba.red, srgba.green, srgba.blue, srgba.alpha)
}

/// Map a provider string to an agent color from the theme.
///
/// Uses substring matching: "anthropic" or "claude" → claude color, etc.
/// Returns `agent_color_default` for unknown providers.
pub fn agent_color_for_provider(theme: &Theme, provider: Option<&str>) -> Color {
    let Some(p) = provider else {
        return theme.agent_color_default;
    };
    let p_lower = p.to_ascii_lowercase();
    if p_lower.contains("anthropic") || p_lower.contains("claude") {
        theme.agent_color_claude
    } else if p_lower.contains("google") || p_lower.contains("gemini") {
        theme.agent_color_gemini
    } else if p_lower.contains("deepseek") {
        theme.agent_color_deepseek
    } else if p_lower.contains("ollama") || p_lower.contains("local") || p_lower.contains("llama") {
        theme.agent_color_local
    } else {
        theme.agent_color_default
    }
}

/// Helper to convert Bevy Color to linear Vec4 (for GPU storage buffers).
pub fn color_to_linear_vec4(color: Color) -> Vec4 {
    let linear = color.to_linear();
    Vec4::new(linear.red, linear.green, linear.blue, linear.alpha)
}

// ═══════════════════════════════════════════════════════════════════════════
// Config Status (Phase 2: Config as CRDT)
// ═══════════════════════════════════════════════════════════════════════════

/// Source of a loaded config file.
#[allow(dead_code)] // Scaffolding for Phase 3 live-reload
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ConfigLoadSource {
    /// Loaded from disk file (~/.config/kaijutsu/).
    #[default]
    Disk,
    /// Loaded from CRDT document (synced from server).
    Crdt,
    /// Using embedded default (fallback).
    Default,
}

impl std::fmt::Display for ConfigLoadSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Disk => write!(f, "disk"),
            Self::Crdt => write!(f, "crdt"),
            Self::Default => write!(f, "default"),
        }
    }
}

/// Status of a single config file.
#[allow(dead_code)] // Scaffolding for Phase 3 live-reload
#[derive(Debug, Clone, Default)]
pub struct ConfigFileStatus {
    /// Where the config was loaded from.
    pub source: ConfigLoadSource,
    /// Error message if there was a problem loading/parsing.
    pub error: Option<String>,
    /// Version counter (increments on changes).
    pub version: u64,
    /// Whether the config has pending CRDT changes not yet applied.
    pub pending_changes: bool,
}

#[allow(dead_code)] // Scaffolding for Phase 3 live-reload
impl ConfigFileStatus {
    /// Create a successful status.
    pub fn success(source: ConfigLoadSource, version: u64) -> Self {
        Self {
            source,
            error: None,
            version,
            pending_changes: false,
        }
    }

    /// Create an error status.
    pub fn with_error(source: ConfigLoadSource, error: impl Into<String>) -> Self {
        Self {
            source,
            error: Some(error.into()),
            version: 0,
            pending_changes: false,
        }
    }
}

/// Resource tracking the status of all config files.
///
/// Used for:
/// - Showing config status in UI (loaded, errors, pending changes)
/// - Triggering theme reloads when config changes
/// - Debugging config issues
#[allow(dead_code)] // Scaffolding for Phase 3 live-reload
#[derive(Resource, Default)]
pub struct ConfigStatus {
    /// Status of the base theme (theme.rhai).
    pub theme: ConfigFileStatus,
    /// Whether live reload is enabled (Phase 3+).
    pub live_reload_enabled: bool,
}
