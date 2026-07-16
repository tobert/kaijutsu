//! Text rendering plugin for Bevy using Vello + MSDF.
//!
//! Vello handles vector content (SVG, sparkline, ABC, borders).
//! MSDF handles text (plain, markdown, output) for shader-quality rendering.

use std::collections::HashSet;

use bevy::prelude::*;
use bevy::window::PrimaryWindow;
use bevy::winit::{EventLoopProxyWrapper, WinitUserEvent};

use super::msdf::glyph::GlyphKey;
use super::msdf::{FontDataMap, MsdfGenerator, PositionedGlyph};
use super::resources::{ShapingFonts, SvgFontDb, TextMetrics};
use super::shaping::{ShapingPlugin, VelloFont, VelloFontAxes, VelloTextAlign, VelloTextStyle};

/// Plugin that enables Vello + MSDF text rendering.
///
/// Sets up:
/// - MSDF atlas, generator, and font data map
/// - Font loading and DPI-aware text metrics
///
/// Vector rasterization (SVG, borders, ABC, sparklines) is owned by
/// `VelloRasterizerPlugin` + `UiRttPlugin`.
pub struct KjTextPlugin;

impl Plugin for KjTextPlugin {
    fn build(&self, app: &mut App) {
        app.add_plugins(ShapingPlugin)
            .init_resource::<ShapingFonts>()
            .init_resource::<TextMetrics>()
            .init_resource::<SvgFontDb>()
            .insert_resource(MsdfGenerator::new())
            .init_resource::<FontDataMap>()
            .add_systems(Startup, (load_fonts, load_svg_fontdb))
            .add_systems(
                Update,
                (
                    sync_text_metrics_from_window,
                    update_text_metrics_from_font,
                    poll_msdf_generator,
                ),
            );
    }
}

/// Poll the MSDF generator for completed glyphs and sync atlas to GPU.
fn poll_msdf_generator(
    mut generator: ResMut<MsdfGenerator>,
    mut atlas: Option<ResMut<super::msdf::MsdfAtlas>>,
    mut images: ResMut<Assets<Image>>,
    font_data_map: Res<FontDataMap>,
    mut msdf_blocks: Query<&mut super::msdf::MsdfBlockGlyphs>,
    event_loop_proxy: Res<EventLoopProxyWrapper>,
    mut last_growth_epoch: Local<u64>,
) {
    let Some(ref mut atlas) = atlas else {
        return;
    };

    // Queue any pending glyph requests
    if !atlas.pending.is_empty() {
        let pending_count = atlas.pending.len();
        generator.queue_pending(atlas, &font_data_map);
        if pending_count > 0 {
            trace!(
                "MSDF generator: queued {} pending glyphs, {} font(s) registered",
                pending_count,
                font_data_map.len(),
            );
        }
    }

    // Poll completed generation tasks
    let before = atlas.regions.len();
    let inserted = generator.poll_completed(atlas, &mut images);
    let after = atlas.regions.len();

    // The atlas can also change shape without the region *count* changing:
    // a grow() repacks every existing region into a larger texture, moving
    // positions (and therefore UVs) even though no new glyph landed this
    // frame. `growth_epoch` is the signal for that case — watch it
    // alongside the region-count delta so a repack forces the same
    // re-extract/re-render path as a newly-arrived glyph.
    let grew = atlas.growth_epoch != *last_growth_epoch;
    *last_growth_epoch = atlas.growth_epoch;

    if after > before || grew {
        if after > before {
            info!(
                "MSDF atlas: {} new glyphs, {} total, version {}",
                after - before,
                after,
                atlas.version,
            );
        }

        // Bump version only on the blocks that actually reference one of the
        // glyphs that just landed — during atlas warmup, most on-screen text
        // blocks don't touch whatever single glyph just finished generating,
        // and a full re-extract (glyph Vec clone) + vertex rebuild + render
        // for every one of them, every frame, is the redundant work this
        // targets. Growth is the exception: it repacks the WHOLE atlas, so
        // every existing region's UVs moved even though the glyph set
        // didn't — every non-empty block needs the same forced re-render
        // path as a newly-arrived glyph.
        let mut any_bumped = false;
        for mut glyphs in msdf_blocks.iter_mut() {
            if glyphs.glyphs.is_empty() {
                continue;
            }
            if grew || block_touches(&inserted, &glyphs.glyphs) {
                glyphs.version = glyphs.version.wrapping_add(1);
                any_bumped = true;
            }
        }

        // Wake the event loop so the re-render happens immediately (reactive mode).
        if any_bumped {
            let _ = event_loop_proxy.send_event(WinitUserEvent::WakeUp);
        }
    }

    // Sync atlas pixels to GPU texture
    atlas.sync_to_gpu(&mut images);
}

/// Whether `glyphs` references any key that just landed in the atlas — the
/// targeted per-block bump predicate for `poll_msdf_generator`. Only a block
/// that actually draws a newly-arrived glyph needs its version bumped
/// (forcing re-extract + re-render); everything else is unaffected by this
/// particular atlas update. Not consulted on atlas growth — growth bumps
/// every non-empty block unconditionally (see the caller).
fn block_touches(inserted: &HashSet<GlyphKey>, glyphs: &[PositionedGlyph]) -> bool {
    glyphs.iter().any(|g| inserted.contains(&g.key))
}

/// Load bundled fonts for the kaijutsu-owned shaping path (`ShapingFonts`).
fn load_fonts(
    asset_server: Res<AssetServer>,
    mut shaping_fonts: ResMut<ShapingFonts>,
) {
    const MONO: &str = "fonts/CascadiaCodeNF.ttf";
    const SERIF: &str = "fonts/NotoSerif-Regular.ttf";
    const CJK: &str = "fonts/NotoSansCJKJP-Light.ttf";

    shaping_fonts.mono = asset_server.load(MONO);
    shaping_fonts.serif = asset_server.load(SERIF);
    shaping_fonts.cjk = asset_server.load(CJK);

    info!("Loaded Vello fonts: CascadiaCodeNF, NotoSerif, NotoSansCJKJP");
}

/// Load bundled fonts into the SVG fontdb for `<text>` element rendering.
///
/// usvg flattens SVG text to outlines during parsing, but only if the
/// referenced fonts are in its database. Without this, `<text>` elements
/// are silently dropped from rendered SVGs.
fn load_svg_fontdb(mut svg_fontdb: ResMut<SvgFontDb>, theme: Res<crate::ui::theme::Theme>) {
    use std::sync::Arc;
    use vello_svg::usvg::fontdb;

    let mut db = fontdb::Database::new();

    // Load our bundled fonts from the assets directory.
    // The working directory is the workspace root when the app runs.
    let font_dir = std::path::Path::new("assets/fonts");
    if font_dir.is_dir() {
        db.load_fonts_dir(font_dir);
        info!(
            "SvgFontDb: loaded {} font faces from {}",
            db.len(),
            font_dir.display()
        );
    } else {
        // Try relative to the crate directory (cargo test, etc.)
        let alt_dir = std::path::Path::new("../../assets/fonts");
        if alt_dir.is_dir() {
            db.load_fonts_dir(alt_dir);
            info!(
                "SvgFontDb: loaded {} font faces from {}",
                db.len(),
                alt_dir.display()
            );
        } else {
            warn!("SvgFontDb: no font directory found — SVG <text> will not render");
        }
    }

    // Map CSS generic families to our bundled fonts (from theme.toml).
    // Default fontdb maps these to system fonts (Times New Roman, Arial, Courier)
    // which we don't bundle.
    db.set_serif_family(&theme.font_serif);
    db.set_sans_serif_family(&theme.font_sans);
    db.set_monospace_family(&theme.font_mono);

    svg_fontdb.fontdb = Arc::new(db);
    svg_fontdb.default_family = theme.font_mono.clone();
}

/// Measure actual line height and character width from the loaded font.
///
/// Fires once after the mono font asset loads, replacing the defaults with
/// real Parley-measured metrics. This ensures cursor positioning matches
/// what the renderer draws — critical for accurate cursor placement.
fn update_text_metrics_from_font(
    shaping_fonts: Res<ShapingFonts>,
    fonts: Res<Assets<VelloFont>>,
    mut text_metrics: ResMut<TextMetrics>,
) {
    // Early exit if both metrics are already measured
    if text_metrics.cell_line_height_from_font && text_metrics.cell_char_width_from_font {
        return;
    }
    let Some(font) = fonts.get(&shaping_fonts.mono) else {
        return;
    };
    let style = VelloTextStyle {
        font_size: text_metrics.cell_font_size,
        font_axes: VelloFontAxes {
            weight: Some(200.0),
            ..default()
        },
        ..default()
    };

    // Measure line height from "X"
    if !text_metrics.cell_line_height_from_font {
        let layout = font.layout("X", &style, VelloTextAlign::Left, None);
        if let Some(line) = layout.lines().next() {
            let measured = line.metrics().line_height;
            if measured > 0.0 {
                info!(
                    "TextMetrics: cell_line_height updated from font: {:.1} → {:.1}",
                    text_metrics.cell_line_height, measured
                );
                text_metrics.cell_line_height = measured;
                text_metrics.cell_line_height_from_font = true;
            }
        }
    }

    // Measure character width from "M" (standard em-width reference)
    if !text_metrics.cell_char_width_from_font {
        let layout = font.layout("M", &style, VelloTextAlign::Left, None);
        if let Some(line) = layout.lines().next()
            && let Some(run) = line.runs().next()
        {
            let advance = run.advance();
            if advance > 0.0 {
                info!(
                    "TextMetrics: cell_char_width updated from font: {:.1} → {:.1}",
                    text_metrics.cell_char_width, advance
                );
                text_metrics.cell_char_width = advance;
                text_metrics.cell_char_width_from_font = true;
            }
        }
    }
}

/// Sync DPI scale factor from the primary window.
fn sync_text_metrics_from_window(
    windows: Query<&Window, With<PrimaryWindow>>,
    mut text_metrics: ResMut<TextMetrics>,
) {
    let Ok(window) = windows.single() else {
        return;
    };

    let scale = window.scale_factor();
    if (text_metrics.scale_factor - scale).abs() > 0.01 {
        text_metrics.scale_factor = scale;
        info!("TextMetrics scale_factor updated: {:.2}", scale);
    }
}

// Rainbow text animation moved to block_render::build_block_scenes.

#[cfg(test)]
mod tests {
    use super::super::msdf::glyph::FontId;
    use super::*;

    fn glyph(key: GlyphKey) -> PositionedGlyph {
        PositionedGlyph {
            key,
            x: 0.0,
            y: 0.0,
            font_size: 16.0,
            color: [255, 255, 255, 255],
            importance: 0.5,
        }
    }

    #[test]
    fn block_touches_true_when_one_of_its_glyphs_just_landed() {
        let key_a = GlyphKey::new(FontId::for_test(1), 0);
        let key_b = GlyphKey::new(FontId::for_test(1), 1);
        let mut inserted = HashSet::new();
        inserted.insert(key_b);

        let glyphs = vec![glyph(key_a), glyph(key_b)];
        assert!(
            block_touches(&inserted, &glyphs),
            "a block referencing a newly-landed glyph must be touched"
        );
    }

    #[test]
    fn block_touches_false_when_none_of_its_glyphs_landed() {
        let key_a = GlyphKey::new(FontId::for_test(1), 0);
        let key_c = GlyphKey::new(FontId::for_test(2), 0);
        let mut inserted = HashSet::new();
        inserted.insert(key_c);

        let glyphs = vec![glyph(key_a)];
        assert!(
            !block_touches(&inserted, &glyphs),
            "a block whose glyphs are all unrelated to this landing must not be touched — \
             this is the whole point of the targeted bump"
        );
    }

    #[test]
    fn block_touches_false_for_empty_glyphs_or_empty_landing() {
        let inserted: HashSet<GlyphKey> = HashSet::new();
        assert!(!block_touches(&inserted, &[]));

        let key_a = GlyphKey::new(FontId::for_test(1), 0);
        let mut inserted = HashSet::new();
        inserted.insert(key_a);
        assert!(!block_touches(&inserted, &[]));
    }
}
