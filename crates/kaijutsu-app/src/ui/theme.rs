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

/// Syntax highlighting colors for kaish shell input.
///
/// Defaults are derived from the ANSI palette (Tokyo Night).
#[allow(dead_code)] // Phase 4: syntax highlighting via Parley spans
#[derive(Clone, Debug)]
pub struct SyntaxColors {
    pub keyword: Color,     // if, for, fn, while
    pub string: Color,      // "hello", 'world'
    pub number: Color,      // 42, 3.14
    pub operator: Color,    // |, &&, ||, ;
    pub variable: Color,    // $HOME, $foo
    pub flag: Color,        // --verbose, -la
    pub comment: Color,     // # comment
    pub command: Color,     // echo, ls, git (the verb)
    pub path: Color,        // /foo/bar
    pub punctuation: Color, // { } ( )
    pub error: Color,       // unrecognized tokens
    pub prefix: Color,      // the : or ` shell prefix char
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
    // ANSI palette (for terminal/syntax use)
    // ═══════════════════════════════════════════════════════════════════════
    pub ansi: AnsiColors,

    // ═══════════════════════════════════════════════════════════════════════
    // Syntax highlighting (kaish shell input) — Phase 4: per-span styling via Parley
    // ═══════════════════════════════════════════════════════════════════════
    #[allow(dead_code)]
    pub syntax: SyntaxColors,

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
    // Markdown Rendering Colors — Phase 4: per-span styling via Parley
    // ═══════════════════════════════════════════════════════════════════════

    #[allow(dead_code)]
    pub md_heading_color: Color,
    #[allow(dead_code)]
    pub md_code_fg: Color,
    #[allow(dead_code)]
    pub md_code_block_fg: Color,
    #[allow(dead_code)]
    pub md_strong_color: Option<Color>,

    // ═══════════════════════════════════════════════════════════════════════
    // Sparkline Rendering
    // ═══════════════════════════════════════════════════════════════════════

    /// Height of sparkline mini-charts in pixels.
    pub sparkline_height: f32,
    /// Sparkline line color.
    pub sparkline_line_color: Color,
    /// Sparkline fill color (area under the curve). None = no fill.
    pub sparkline_fill_color: Option<Color>,

    // ═══════════════════════════════════════════════════════════════════════
    // Font Effects
    // ═══════════════════════════════════════════════════════════════════════

    /// Enable rainbow color cycling effect on user text.
    pub font_rainbow: bool,

    // ═══════════════════════════════════════════════════════════════════════
    // Constellation Configuration
    // ═══════════════════════════════════════════════════════════════════════

    /// Base radius for radial tree root ring (pixels, 2D fallback layout)
    pub constellation_base_radius: f32,
    /// Spacing between concentric rings in radial tree (pixels, 2D fallback layout)
    pub constellation_ring_spacing: f32,
    /// Card width in constellation view (pixels)
    pub constellation_card_width: f32,
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
    // Constellation 3D Configuration (hyperbolic layout)
    // ═══════════════════════════════════════════════════════════════════════

    /// Base hemisphere radius for leaf nodes in the H3 layout
    pub constellation_base_leaf_radius: f64,
    /// Packing factor for hemisphere area sums (gap compensation)
    pub constellation_packing_factor: f64,

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
    /// Command palette border color (accent blue)
    pub compose_palette_border: Color,
    /// Command palette glow spread radius
    pub compose_palette_glow_radius: f32,
    /// Command palette glow alpha multiplier
    pub compose_palette_glow_intensity: f32,

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
            // Base UI — Tokyo Night palette
            bg: Color::srgb(0.102, 0.106, 0.149),               // #1a1b26
            panel_bg: Color::srgba(0.102, 0.106, 0.149, 0.95),  // #1a1b26 semi-transparent
            fg: Color::srgb(0.753, 0.792, 0.961),               // #c0caf5
            fg_dim: Color::srgb(0.337, 0.373, 0.537),           // #565f89
            accent: Color::srgb(0.478, 0.635, 0.969),           // #7aa2f7
            accent2: Color::srgb(0.620, 0.808, 0.416),          // #9ece6a
            border: Color::srgb(0.231, 0.259, 0.380),           // #3b4261
            selection_bg: Color::srgba(0.478, 0.635, 0.969, 0.30), // #7aa2f7 selection

            // Row type colors
            row_tool: Color::srgb(0.733, 0.604, 0.969),         // #bb9af7 purple
            row_result: Color::srgb(0.878, 0.686, 0.404),       // #e0af68 amber

            // Block text colors
            block_user: Color::srgb(0.753, 0.792, 0.961),       // #c0caf5 main fg
            block_assistant: Color::srgb(0.478, 0.635, 0.969),  // #7aa2f7 blue
            block_thinking: Color::srgb(0.337, 0.373, 0.537),   // #565f89 dim
            block_tool_call: Color::srgb(0.878, 0.686, 0.404),  // #e0af68 amber
            block_tool_result: Color::srgb(0.620, 0.808, 0.416), // #9ece6a green
            block_tool_error: Color::srgb(0.969, 0.463, 0.557), // #f7768e red
            block_drift_push: Color::srgb(0.490, 0.812, 1.00),  // #7dcfff cyan
            block_drift_pull: Color::srgb(0.478, 0.635, 0.969), // #7aa2f7 blue
            block_drift_merge: Color::srgb(0.733, 0.604, 0.969), // #bb9af7 purple
            block_drift_commit: Color::srgb(0.620, 0.808, 0.416), // #9ece6a green

            // Semantic
            error: Color::srgb(0.969, 0.463, 0.557),            // #f7768e
            warning: Color::srgb(0.878, 0.686, 0.404),          // #e0af68
            success: Color::srgb(0.620, 0.808, 0.416),          // #9ece6a

            // Mode colors (vim-style)
            mode_normal: Color::srgb(0.478, 0.635, 0.969),      // #7aa2f7 blue
            mode_chat: Color::srgb(0.620, 0.808, 0.416),        // #9ece6a green
            mode_shell: Color::srgb(0.878, 0.686, 0.404),       // #e0af68 amber
            mode_visual: Color::srgb(0.733, 0.604, 0.969),      // #bb9af7 purple

            // Cursor colors
            cursor_normal: Vec4::new(0.478, 0.635, 0.969, 0.8), // #7aa2f7 blue
            cursor_insert: Vec4::new(0.620, 0.808, 0.416, 0.9), // #9ece6a green
            cursor_visual: Vec4::new(0.733, 0.604, 0.969, 0.7), // #bb9af7 purple

            // ANSI palette — Tokyo Night
            ansi: AnsiColors {
                black: Color::srgb(0.082, 0.086, 0.118),        // #15161e
                red: Color::srgb(0.969, 0.463, 0.557),          // #f7768e
                green: Color::srgb(0.620, 0.808, 0.416),        // #9ece6a
                yellow: Color::srgb(0.878, 0.686, 0.404),       // #e0af68
                blue: Color::srgb(0.478, 0.635, 0.969),         // #7aa2f7
                magenta: Color::srgb(0.733, 0.604, 0.969),      // #bb9af7
                cyan: Color::srgb(0.490, 0.812, 1.00),          // #7dcfff
                white: Color::srgb(0.663, 0.694, 0.839),        // #a9b1d6
                bright_black: Color::srgb(0.255, 0.282, 0.408), // #414868
                bright_red: Color::srgb(0.969, 0.463, 0.557),   // #f7768e
                bright_green: Color::srgb(0.620, 0.808, 0.416), // #9ece6a
                bright_yellow: Color::srgb(0.878, 0.686, 0.404), // #e0af68
                bright_blue: Color::srgb(0.478, 0.635, 0.969),  // #7aa2f7
                bright_magenta: Color::srgb(0.733, 0.604, 0.969), // #bb9af7
                bright_cyan: Color::srgb(0.490, 0.812, 1.00),   // #7dcfff
                bright_white: Color::srgb(0.753, 0.792, 0.961), // #c0caf5
            },

            // Syntax highlighting — derived from ANSI palette
            syntax: SyntaxColors {
                keyword: Color::srgb(0.733, 0.604, 0.969),      // magenta
                string: Color::srgb(0.620, 0.808, 0.416),       // green
                number: Color::srgb(0.878, 0.686, 0.404),       // yellow
                operator: Color::srgb(0.490, 0.812, 1.00),      // cyan
                variable: Color::srgb(0.878, 0.686, 0.404),     // bright_yellow
                flag: Color::srgb(0.478, 0.635, 0.969),         // bright_blue
                comment: Color::srgb(0.255, 0.282, 0.408),      // bright_black
                command: Color::srgb(0.478, 0.635, 0.969),       // blue
                path: Color::srgb(0.490, 0.812, 1.00),          // cyan
                punctuation: Color::srgb(0.753, 0.792, 0.961),  // fg
                error: Color::srgb(0.969, 0.463, 0.557),        // red
                prefix: Color::srgb(0.255, 0.282, 0.408),       // bright_black
            },

            // Frame configuration
            frame_corner_size: 16.0,
            frame_edge_thickness: 2.0,
            frame_content_padding: 12.0,

            // Frame colors
            frame_base: Color::srgba(0.102, 0.106, 0.149, 0.95), // #1a1b26
            frame_focused: Color::srgba(0.478, 0.635, 0.969, 0.15), // #7aa2f7
            frame_insert: Color::srgba(0.620, 0.808, 0.416, 0.12), // #9ece6a
            frame_visual: Color::srgba(0.733, 0.604, 0.969, 0.12), // #bb9af7
            frame_unfocused: Color::srgba(0.102, 0.106, 0.149, 0.80), // #1a1b26
            frame_edge: Color::srgba(0.231, 0.259, 0.380, 0.6), // #3b4261

            // Frame shader params
            frame_params_base: Vec4::new(0.0, 0.0, 0.0, 0.0),
            frame_params_focused: Vec4::new(0.3, 0.0, 0.0, 0.0),
            frame_params_unfocused: Vec4::new(0.0, 0.0, 0.0, 0.0),

            // Edge dimming
            frame_edge_dim_unfocused: Vec4::new(0.5, 0.5, 0.5, 0.6),
            frame_edge_dim_focused: Vec4::new(1.0, 1.0, 1.0, 1.0),

            // Shader effect parameters
            effect_glow_radius: 4.0,
            effect_glow_intensity: 0.3,
            effect_glow_falloff: 2.0,
            effect_sheen_speed: 0.5,
            effect_sheen_sparkle_threshold: 0.95,
            effect_breathe_speed: 1.0,
            effect_breathe_amplitude: 0.05,

            // Chasing border
            effect_chase_speed: 2.0,
            effect_chase_width: 0.15,
            effect_chase_glow_radius: 8.0,
            effect_chase_glow_intensity: 0.5,
            effect_chase_color_cycle: 0.0,

            // Input area defaults
            input_minimized_height: 48.0,
            input_docked_height: 200.0,
            input_overlay_width_pct: 0.6,
            input_backdrop_color: Color::srgba(0.102, 0.106, 0.149, 0.85),

            // Markdown rendering
            md_heading_color: Color::srgb(0.733, 0.604, 0.969), // #bb9af7 purple
            md_code_fg: Color::srgb(0.620, 0.808, 0.416),       // #9ece6a green
            md_code_block_fg: Color::srgb(0.478, 0.635, 0.969), // #7aa2f7 blue
            md_strong_color: None,

            // Sparkline rendering
            sparkline_height: 48.0,
            sparkline_line_color: Color::srgb(0.490, 0.812, 1.00), // #7dcfff Tokyo Night cyan
            sparkline_fill_color: Some(Color::srgba(0.490, 0.812, 1.00, 0.15)), // cyan 15% alpha

            // Font effects
            font_rainbow: true,

            // Constellation radial tree layout
            constellation_base_radius: 500.0,
            constellation_ring_spacing: 550.0,
            constellation_card_width: 180.0,

            agent_color_default: Color::srgba(0.49, 0.85, 0.82, 0.8),   // #7dd9d1 dim cyan
            agent_color_human: Color::srgba(0.49, 0.98, 1.00, 0.9),     // #7df9ff electric cyan
            agent_color_claude: Color::srgba(1.00, 0.43, 0.78, 0.9),    // #ff6ec7 hot pink
            agent_color_gemini: Color::srgba(1.00, 0.84, 0.00, 0.9),    // #ffd700 gold
            agent_color_local: Color::srgba(0.31, 0.98, 0.48, 0.9),     // #50fa7b matrix green
            agent_color_deepseek: Color::srgba(1.00, 0.72, 0.42, 0.9),  // #ffb86c orange

            // Constellation 2.5D layout
            constellation_base_leaf_radius: 1.2,  // Increased for better spread with many nodes
            constellation_packing_factor: 1.8,    // More gap between nodes

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

            // Compose block
            compose_border: Color::srgb(0.231, 0.259, 0.380), // #3b4261 matches border
            compose_bg: Color::srgb(0.102, 0.106, 0.149),     // #1a1b26 matches bg
            compose_palette_border: Color::srgb(0.478, 0.635, 0.969), // #7aa2f7 accent blue
            compose_palette_glow_radius: 6.0,
            compose_palette_glow_intensity: 0.25,

            // Modal backdrop — semi-transparent dark overlay
            modal_backdrop: Color::srgba(0.0, 0.0, 0.0, 0.6),

            // User/assistant text borders — transparent (opt-in)
            block_border_user: Color::srgba(0.0, 0.0, 0.0, 0.0),
            block_border_assistant: Color::srgba(0.0, 0.0, 0.0, 0.0),
        }
    }
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
