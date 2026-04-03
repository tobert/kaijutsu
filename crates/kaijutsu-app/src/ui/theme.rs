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
            black: Color::srgb(0.10, 0.11, 0.15),   // #1a1b26
            red: Color::srgb(0.97, 0.38, 0.45),     // #f7616a
            green: Color::srgb(0.62, 0.81, 0.42),   // #9ece6a
            yellow: Color::srgb(0.89, 0.79, 0.49),  // #e0c97d
            blue: Color::srgb(0.48, 0.64, 0.97),    // #7aa2f7
            magenta: Color::srgb(0.73, 0.47, 0.91), // #bb79e8
            cyan: Color::srgb(0.49, 0.85, 0.82),    // #7dd9d1
            white: Color::srgb(0.78, 0.80, 0.85),   // #c8ccd9
            // Bright variants
            bright_black: Color::srgb(0.27, 0.29, 0.35), // #444b59
            bright_red: Color::srgb(1.00, 0.53, 0.58),   // #ff8894
            bright_green: Color::srgb(0.72, 0.91, 0.52), // #b8e885
            bright_yellow: Color::srgb(1.00, 0.89, 0.59), // #ffe397
            bright_blue: Color::srgb(0.58, 0.74, 1.00),  // #94bdff
            bright_magenta: Color::srgb(0.83, 0.57, 1.00), // #d491ff
            bright_cyan: Color::srgb(0.59, 0.95, 0.92),  // #96f2eb
            bright_white: Color::srgb(0.90, 0.90, 0.90), // #e5e5e5
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
#[allow(dead_code)]
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
    pub mode_insert: Color,
    pub mode_chat: Color,
    pub mode_shell: Color,
    pub mode_visual: Color,

    // ═══════════════════════════════════════════════════════════════════════
    // Mode labels (dock HUD text, Rhai-scriptable)
    // ═══════════════════════════════════════════════════════════════════════
    pub mode_label_normal: String,
    pub mode_label_insert: String,
    pub mode_label_visual: String,
    pub mode_label_shell: String,
    pub mode_label_constellation: String,
    pub mode_label_stack: String,
    pub mode_label_input: String,

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
    // OutputData Rendering Colors (structured tool results)
    // ═══════════════════════════════════════════════════════════════════════
    /// Directory entries in structured output (soft blue)
    pub output_directory: Color,
    /// Executable entries in structured output (soft green)
    pub output_executable: Color,
    /// Symlink entries in structured output (cyan)
    pub output_symlink: Color,
    /// Column headers in structured output (dim text)
    pub output_header: Color,

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
    /// Monospace font family name (for SVG generic family fallback + usvg default).
    pub font_mono: String,
    /// Serif font family name (for SVG generic family fallback).
    pub font_serif: String,
    /// Sans-serif font family name (for SVG generic family fallback).
    pub font_sans: String,

    // ═══════════════════════════════════════════════════════════════════════
    // MSDF Text Rendering Quality
    // ═══════════════════════════════════════════════════════════════════════
    /// Hinting strength (0.0 = off, 1.0 = full).
    pub msdf_hint_amount: f32,
    /// Stem darkening (0.0 = off, ~0.15 = ClearType-like).
    pub msdf_stem_darkening: f32,
    /// Horizontal stroke AA scale (1.0-1.3).
    pub msdf_horz_scale: f32,
    /// Vertical stroke AA scale (0.5-0.8).
    pub msdf_vert_scale: f32,
    /// SDF threshold (0.45-0.55).
    pub msdf_text_bias: f32,
    /// Alpha gamma correction.
    pub msdf_gamma_correction: f32,

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
    /// Border glow spread radius (pixels of exponential falloff)
    pub block_border_glow_radius: f32,
    /// Border glow peak brightness multiplier
    pub block_border_glow_intensity: f32,
    /// Text glow halo radius in pixels (0 = disabled)
    pub text_glow_radius: f32,
    /// Text glow halo color (independent of border color)
    pub text_glow_color: Color,
    /// Multiplier against cell_font_size for border inner padding
    pub block_border_padding: f32,
    /// Vertical spacing between blocks (pixels)
    pub block_spacing: f32,

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

    // ═══════════════════════════════════════════════════════════════════════
    // Layout spacing constants
    // ═══════════════════════════════════════════════════════════════════════
    /// Horizontal indentation per nesting level (pixels)
    pub indent_width: f32,
    /// Height reserved for role group divider lines (pixels)
    pub role_header_height: f32,
    /// Spacing below role group divider (pixels)
    pub role_header_spacing: f32,
    /// Font size for border labels (tool name, status text)
    pub label_font_size: f32,
    /// Horizontal inset from block edge where labels start (pixels)
    pub label_inset: f32,
    /// Horizontal padding around label text (pixels)
    pub label_pad: f32,
}

impl Default for Theme {
    fn default() -> Self {
        Self {
            // Base UI — Tokyo Night palette
            bg: Color::srgb(0.102, 0.106, 0.149), // #1a1b26
            panel_bg: Color::srgba(0.102, 0.106, 0.149, 0.95), // #1a1b26 semi-transparent
            fg: Color::srgb(0.753, 0.792, 0.961), // #c0caf5
            fg_dim: Color::srgb(0.337, 0.373, 0.537), // #565f89
            accent: Color::srgb(0.478, 0.635, 0.969), // #7aa2f7
            accent2: Color::srgb(0.620, 0.808, 0.416), // #9ece6a
            border: Color::srgb(0.231, 0.259, 0.380), // #3b4261
            selection_bg: Color::srgba(0.478, 0.635, 0.969, 0.30), // #7aa2f7 selection

            // Row type colors
            row_tool: Color::srgb(0.733, 0.604, 0.969), // #bb9af7 purple
            row_result: Color::srgb(0.878, 0.686, 0.404), // #e0af68 amber

            // Block text colors
            block_user: Color::srgb(0.753, 0.792, 0.961), // #c0caf5 main fg
            block_assistant: Color::srgb(0.478, 0.635, 0.969), // #7aa2f7 blue
            block_thinking: Color::srgb(0.337, 0.373, 0.537), // #565f89 dim
            block_tool_call: Color::srgb(0.878, 0.686, 0.404), // #e0af68 amber
            block_tool_result: Color::srgb(0.620, 0.808, 0.416), // #9ece6a green
            block_tool_error: Color::srgb(0.969, 0.463, 0.557), // #f7768e red
            block_drift_push: Color::srgb(0.490, 0.812, 1.00), // #7dcfff cyan
            block_drift_pull: Color::srgb(0.478, 0.635, 0.969), // #7aa2f7 blue
            block_drift_merge: Color::srgb(0.733, 0.604, 0.969), // #bb9af7 purple
            block_drift_commit: Color::srgb(0.620, 0.808, 0.416), // #9ece6a green

            // Semantic
            error: Color::srgb(0.969, 0.463, 0.557), // #f7768e
            warning: Color::srgb(0.878, 0.686, 0.404), // #e0af68
            success: Color::srgb(0.620, 0.808, 0.416), // #9ece6a

            // Mode colors (vim-style)
            mode_normal: Color::srgb(0.478, 0.635, 0.969), // #7aa2f7 blue
            mode_insert: Color::srgb(0.620, 0.808, 0.416), // #9ece6a green
            mode_chat: Color::srgb(0.620, 0.808, 0.416),   // #9ece6a green
            mode_shell: Color::srgb(0.878, 0.686, 0.404),  // #e0af68 amber
            mode_visual: Color::srgb(0.733, 0.604, 0.969), // #bb9af7 purple

            // Mode labels (dock HUD, Rhai-scriptable)
            mode_label_normal: "NORMAL".into(),
            mode_label_insert: "INSERT".into(),
            mode_label_visual: "VISUAL".into(),
            mode_label_shell: "SHELL".into(),
            mode_label_constellation: "CONSTELLATION".into(),
            mode_label_stack: "STACK".into(),
            mode_label_input: "INPUT".into(),

            // Cursor colors
            cursor_normal: Vec4::new(0.478, 0.635, 0.969, 0.8), // #7aa2f7 blue
            cursor_insert: Vec4::new(0.620, 0.808, 0.416, 0.9), // #9ece6a green
            cursor_visual: Vec4::new(0.733, 0.604, 0.969, 0.7), // #bb9af7 purple

            // ANSI palette — Tokyo Night
            ansi: AnsiColors {
                black: Color::srgb(0.082, 0.086, 0.118),          // #15161e
                red: Color::srgb(0.969, 0.463, 0.557),            // #f7768e
                green: Color::srgb(0.620, 0.808, 0.416),          // #9ece6a
                yellow: Color::srgb(0.878, 0.686, 0.404),         // #e0af68
                blue: Color::srgb(0.478, 0.635, 0.969),           // #7aa2f7
                magenta: Color::srgb(0.733, 0.604, 0.969),        // #bb9af7
                cyan: Color::srgb(0.490, 0.812, 1.00),            // #7dcfff
                white: Color::srgb(0.663, 0.694, 0.839),          // #a9b1d6
                bright_black: Color::srgb(0.255, 0.282, 0.408),   // #414868
                bright_red: Color::srgb(0.969, 0.463, 0.557),     // #f7768e
                bright_green: Color::srgb(0.620, 0.808, 0.416),   // #9ece6a
                bright_yellow: Color::srgb(0.878, 0.686, 0.404),  // #e0af68
                bright_blue: Color::srgb(0.478, 0.635, 0.969),    // #7aa2f7
                bright_magenta: Color::srgb(0.733, 0.604, 0.969), // #bb9af7
                bright_cyan: Color::srgb(0.490, 0.812, 1.00),     // #7dcfff
                bright_white: Color::srgb(0.753, 0.792, 0.961),   // #c0caf5
            },

            // Syntax highlighting — derived from ANSI palette
            syntax: SyntaxColors {
                keyword: Color::srgb(0.733, 0.604, 0.969),     // magenta
                string: Color::srgb(0.620, 0.808, 0.416),      // green
                number: Color::srgb(0.878, 0.686, 0.404),      // yellow
                operator: Color::srgb(0.490, 0.812, 1.00),     // cyan
                variable: Color::srgb(0.878, 0.686, 0.404),    // bright_yellow
                flag: Color::srgb(0.478, 0.635, 0.969),        // bright_blue
                comment: Color::srgb(0.255, 0.282, 0.408),     // bright_black
                command: Color::srgb(0.478, 0.635, 0.969),     // blue
                path: Color::srgb(0.490, 0.812, 1.00),         // cyan
                punctuation: Color::srgb(0.753, 0.792, 0.961), // fg
                error: Color::srgb(0.969, 0.463, 0.557),       // red
                prefix: Color::srgb(0.255, 0.282, 0.408),      // bright_black
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
            frame_edge: Color::srgba(0.231, 0.259, 0.380, 0.6),  // #3b4261

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

            // OutputData rendering (structured tool results)
            output_directory: Color::srgb(0.478, 0.635, 0.969), // #7aa2f7 soft blue
            output_executable: Color::srgb(0.620, 0.808, 0.416), // #9ece6a soft green
            output_symlink: Color::srgb(0.490, 0.812, 1.00),    // #7dcfff cyan
            output_header: Color::srgb(0.337, 0.373, 0.537),    // #565f89 dim

            // Markdown rendering
            md_heading_color: Color::srgb(0.733, 0.604, 0.969), // #bb9af7 purple
            md_code_fg: Color::srgb(0.620, 0.808, 0.416),       // #9ece6a green
            md_code_block_fg: Color::srgb(0.478, 0.635, 0.969), // #7aa2f7 blue
            md_strong_color: None,

            // Sparkline rendering
            sparkline_height: 48.0,
            sparkline_line_color: Color::srgb(0.490, 0.812, 1.00), // #7dcfff Tokyo Night cyan
            sparkline_fill_color: Some(Color::srgba(0.490, 0.812, 1.00, 0.15)), // cyan 15% alpha

            // Font configuration
            font_rainbow: true,
            font_mono: "Cascadia Code NF".into(),
            // MSDF text rendering quality
            msdf_hint_amount: 0.8,
            msdf_stem_darkening: 0.15,
            msdf_horz_scale: 1.1,
            msdf_vert_scale: 0.6,
            msdf_text_bias: 0.5,
            msdf_gamma_correction: 0.85,
            font_serif: "Noto Serif".into(),
            font_sans: "Noto Sans CJK JP".into(),

            // Constellation radial tree layout
            constellation_base_radius: 500.0,
            constellation_ring_spacing: 550.0,
            constellation_card_width: 180.0,

            agent_color_default: Color::srgba(0.49, 0.85, 0.82, 0.8), // #7dd9d1 dim cyan
            agent_color_human: Color::srgba(0.49, 0.98, 1.00, 0.9),   // #7df9ff electric cyan
            agent_color_claude: Color::srgba(1.00, 0.43, 0.78, 0.9),  // #ff6ec7 hot pink
            agent_color_gemini: Color::srgba(1.00, 0.84, 0.00, 0.9),  // #ffd700 gold
            agent_color_local: Color::srgba(0.31, 0.98, 0.48, 0.9),   // #50fa7b matrix green
            agent_color_deepseek: Color::srgba(1.00, 0.72, 0.42, 0.9), // #ffb86c orange

            // Constellation 2.5D layout
            constellation_base_leaf_radius: 1.2, // Increased for better spread with many nodes
            constellation_packing_factor: 1.8,   // More gap between nodes

            // Block borders
            block_border_tool_call: Color::srgba(1.00, 0.67, 0.00, 0.6), // #ffaa00 amber
            block_border_tool_result: Color::srgba(0.00, 1.00, 0.53, 0.4), // #00ff88 green
            block_border_error: Color::srgba(1.00, 0.13, 0.38, 0.8),     // #ff2060 red
            block_border_thinking: Color::srgba(0.38, 0.50, 0.63, 0.3),  // #6080a0 muted
            block_border_drift: Color::srgba(0.00, 0.67, 1.00, 0.5),     // #00aaff blue
            block_border_thickness: 1.5,
            block_border_corner_radius: 4.0,
            block_border_glow_radius: 10.0,
            block_border_glow_intensity: 0.5,
            text_glow_radius: 2.5,
            text_glow_color: Color::srgba(0.75, 0.82, 0.95, 0.35),
            block_border_padding: 0.6,
            block_spacing: 12.0,

            // Compose block
            compose_border: Color::srgb(0.231, 0.259, 0.380), // #3b4261 matches border
            compose_bg: Color::srgb(0.102, 0.106, 0.149),     // #1a1b26 matches bg
            compose_palette_border: Color::srgb(0.478, 0.635, 0.969), // #7aa2f7 accent blue
            compose_palette_glow_radius: 10.0,
            compose_palette_glow_intensity: 0.6,

            // Modal backdrop — semi-transparent dark overlay
            modal_backdrop: Color::srgba(0.0, 0.0, 0.0, 0.6),

            // User/assistant text borders — transparent (opt-in)
            block_border_user: Color::srgba(0.0, 0.0, 0.0, 0.0),
            block_border_assistant: Color::srgba(0.0, 0.0, 0.0, 0.0),

            // Layout spacing
            indent_width: 24.0,
            role_header_height: 20.0,
            role_header_spacing: 4.0,
            label_font_size: 11.0,
            label_inset: 12.0,
            label_pad: 6.0,
        }
    }
}

// ═══════════════════════════════════════════════════════════════════════════
// ThemeData → Theme conversion
// ═══════════════════════════════════════════════════════════════════════════

/// Parse a hex string into a Bevy `Color`. Returns `Color::BLACK` on failure.
fn hex_to_color(s: &str) -> Color {
    match kaijutsu_rhai::parse_hex(s) {
        Some(c) => {
            if c.alpha >= 1.0 {
                Color::srgb(c.red, c.green, c.blue)
            } else {
                Color::srgba(c.red, c.green, c.blue, c.alpha)
            }
        }
        None => Color::BLACK,
    }
}

impl From<kaijutsu_rhai::theme::ThemeData> for Theme {
    fn from(td: kaijutsu_rhai::theme::ThemeData) -> Self {
        let mut theme = Theme::default();

        // Base UI
        theme.bg = hex_to_color(&td.bg);
        theme.panel_bg = hex_to_color(&td.panel_bg);
        theme.fg = hex_to_color(&td.fg);
        theme.fg_dim = hex_to_color(&td.fg_dim);
        theme.accent = hex_to_color(&td.accent);
        theme.accent2 = hex_to_color(&td.accent2);
        theme.border = hex_to_color(&td.border);
        theme.selection_bg = hex_to_color(&td.selection_bg);

        // Row colors
        theme.row_tool = hex_to_color(&td.row_tool);
        theme.row_result = hex_to_color(&td.row_result);

        // Semantic
        theme.error = hex_to_color(&td.error);
        theme.warning = hex_to_color(&td.warning);
        theme.success = hex_to_color(&td.success);

        // Mode colors
        theme.mode_normal = hex_to_color(&td.mode_normal);
        theme.mode_insert = hex_to_color(&td.mode_insert);
        theme.mode_chat = hex_to_color(&td.mode_chat);
        theme.mode_shell = hex_to_color(&td.mode_shell);
        theme.mode_visual = hex_to_color(&td.mode_visual);

        // Mode labels
        theme.mode_label_normal = td.mode_label_normal;
        theme.mode_label_insert = td.mode_label_insert;
        theme.mode_label_visual = td.mode_label_visual;
        theme.mode_label_shell = td.mode_label_shell;
        theme.mode_label_constellation = td.mode_label_constellation;
        theme.mode_label_stack = td.mode_label_stack;
        theme.mode_label_input = td.mode_label_input;

        // Cursor colors
        theme.cursor_normal = Vec4::from(td.cursor_normal);
        theme.cursor_insert = Vec4::from(td.cursor_insert);
        theme.cursor_visual = Vec4::from(td.cursor_visual);

        // ANSI palette
        theme.ansi.black = hex_to_color(&td.ansi.black);
        theme.ansi.red = hex_to_color(&td.ansi.red);
        theme.ansi.green = hex_to_color(&td.ansi.green);
        theme.ansi.yellow = hex_to_color(&td.ansi.yellow);
        theme.ansi.blue = hex_to_color(&td.ansi.blue);
        theme.ansi.magenta = hex_to_color(&td.ansi.magenta);
        theme.ansi.cyan = hex_to_color(&td.ansi.cyan);
        theme.ansi.white = hex_to_color(&td.ansi.white);
        theme.ansi.bright_black = hex_to_color(&td.ansi.bright_black);
        theme.ansi.bright_red = hex_to_color(&td.ansi.bright_red);
        theme.ansi.bright_green = hex_to_color(&td.ansi.bright_green);
        theme.ansi.bright_yellow = hex_to_color(&td.ansi.bright_yellow);
        theme.ansi.bright_blue = hex_to_color(&td.ansi.bright_blue);
        theme.ansi.bright_magenta = hex_to_color(&td.ansi.bright_magenta);
        theme.ansi.bright_cyan = hex_to_color(&td.ansi.bright_cyan);
        theme.ansi.bright_white = hex_to_color(&td.ansi.bright_white);

        // Frame config
        theme.frame_corner_size = td.frame_corner_size;
        theme.frame_edge_thickness = td.frame_edge_thickness;
        theme.frame_content_padding = td.frame_content_padding;

        // Frame colors
        theme.frame_base = hex_to_color(&td.frame_base);
        theme.frame_focused = hex_to_color(&td.frame_focused);
        theme.frame_insert = hex_to_color(&td.frame_insert);
        theme.frame_visual = hex_to_color(&td.frame_visual);
        theme.frame_unfocused = hex_to_color(&td.frame_unfocused);
        theme.frame_edge = hex_to_color(&td.frame_edge);

        // Frame shader params
        theme.frame_params_base = Vec4::from(td.frame_params_base);
        theme.frame_params_focused = Vec4::from(td.frame_params_focused);
        theme.frame_params_unfocused = Vec4::from(td.frame_params_unfocused);

        // Edge dimming
        theme.frame_edge_dim_unfocused = Vec4::from(td.frame_edge_dim_unfocused);
        theme.frame_edge_dim_focused = Vec4::from(td.frame_edge_dim_focused);

        // Shader effects
        theme.effect_glow_radius = td.effect_glow_radius;
        theme.effect_glow_intensity = td.effect_glow_intensity;
        theme.effect_glow_falloff = td.effect_glow_falloff;
        theme.effect_sheen_speed = td.effect_sheen_speed;
        theme.effect_sheen_sparkle_threshold = td.effect_sheen_sparkle_threshold;
        theme.effect_breathe_speed = td.effect_breathe_speed;
        theme.effect_breathe_amplitude = td.effect_breathe_amplitude;

        // Chasing border
        theme.effect_chase_speed = td.effect_chase_speed;
        theme.effect_chase_width = td.effect_chase_width;
        theme.effect_chase_glow_radius = td.effect_chase_glow_radius;
        theme.effect_chase_glow_intensity = td.effect_chase_glow_intensity;
        theme.effect_chase_color_cycle = td.effect_chase_color_cycle;

        // Input area
        theme.input_minimized_height = td.input_minimized_height;
        theme.input_docked_height = td.input_docked_height;
        theme.input_overlay_width_pct = td.input_overlay_width_pct;
        theme.input_backdrop_color = hex_to_color(&td.input_backdrop_color);

        // Font configuration
        theme.font_rainbow = td.font_rainbow;
        theme.font_mono = td.font_mono;
        theme.font_serif = td.font_serif;
        theme.font_sans = td.font_sans;

        // MSDF text rendering quality
        theme.msdf_hint_amount = td.msdf_hint_amount;
        theme.msdf_stem_darkening = td.msdf_stem_darkening;
        theme.msdf_horz_scale = td.msdf_horz_scale;
        theme.msdf_vert_scale = td.msdf_vert_scale;
        theme.msdf_text_bias = td.msdf_text_bias;
        theme.msdf_gamma_correction = td.msdf_gamma_correction;

        // Constellation
        theme.constellation_base_radius = td.constellation_base_radius;
        theme.constellation_ring_spacing = td.constellation_ring_spacing;

        // Block borders
        theme.block_border_tool_call = hex_to_color(&td.block_border_tool_call);
        theme.block_border_tool_result = hex_to_color(&td.block_border_tool_result);
        theme.block_border_error = hex_to_color(&td.block_border_error);
        theme.block_border_thinking = hex_to_color(&td.block_border_thinking);
        theme.block_border_drift = hex_to_color(&td.block_border_drift);
        theme.block_border_thickness = td.block_border_thickness;
        theme.block_border_corner_radius = td.block_border_corner_radius;
        theme.block_border_glow_radius = td.block_border_glow_radius;
        theme.block_border_glow_intensity = td.block_border_glow_intensity;
        theme.text_glow_radius = td.text_glow_radius;
        theme.text_glow_color = hex_to_color(&td.text_glow_color);
        theme.block_border_padding = td.block_border_padding;
        theme.block_spacing = td.block_spacing;

        // Compose
        theme.compose_border = hex_to_color(&td.compose_border);
        theme.compose_bg = hex_to_color(&td.compose_bg);

        // Modal
        theme.modal_backdrop = hex_to_color(&td.modal_backdrop);

        // User/assistant text borders
        theme.block_border_user = hex_to_color(&td.block_border_user);
        theme.block_border_assistant = hex_to_color(&td.block_border_assistant);

        // Layout spacing
        theme.indent_width = td.indent_width;
        theme.role_header_height = td.role_header_height;
        theme.role_header_spacing = td.role_header_spacing;
        theme.label_font_size = td.label_font_size;
        theme.label_inset = td.label_inset;
        theme.label_pad = td.label_pad;

        theme
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
