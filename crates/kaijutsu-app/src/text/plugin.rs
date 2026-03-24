//! Text rendering plugin for Bevy using Vello + MSDF.
//!
//! Vello handles vector content (SVG, sparkline, ABC, borders).
//! MSDF handles text (plain, markdown, output) for shader-quality rendering.

use bevy::prelude::*;
use bevy::window::PrimaryWindow;
use bevy_vello::VelloPlugin;
use bevy_vello::integrations::text::VelloFontAxes;
use bevy_vello::prelude::*;

use super::msdf::{FontDataMap, MsdfGenerator};
use super::resources::{FontHandles, SvgFontDb, TextMetrics};

/// Plugin that enables Vello + MSDF text rendering.
///
/// Sets up:
/// - VelloPlugin (vector renderer for SVG, borders, etc.)
/// - MSDF atlas, generator, and font data map
/// - Font loading and DPI-aware text metrics
pub struct KjTextPlugin;

impl Plugin for KjTextPlugin {
    fn build(&self, app: &mut App) {
        app.add_plugins(VelloPlugin::default())
            .init_resource::<FontHandles>()
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
    generator.poll_completed(atlas);
    let after = atlas.regions.len();
    if after > before {
        info!(
            "MSDF atlas: {} new glyphs, {} total, version {}",
            after - before,
            after,
            atlas.version,
        );

        // Bump version on all MSDF blocks so the render world re-extracts them.
        // This is cheaper than a full rebuild — no Parley re-layout, no texture resize,
        // just a re-render with the now-complete atlas.
        for mut glyphs in msdf_blocks.iter_mut() {
            if !glyphs.glyphs.is_empty() {
                glyphs.version = glyphs.version.wrapping_add(1);
            }
        }
    }

    // Sync atlas pixels to GPU texture
    atlas.sync_to_gpu(&mut images);
}

/// Load bundled fonts into VelloFont asset handles.
fn load_fonts(asset_server: Res<AssetServer>, mut font_handles: ResMut<FontHandles>) {
    font_handles.mono = asset_server.load("fonts/CascadiaCodeNF.ttf");
    font_handles.serif = asset_server.load("fonts/NotoSerif-Regular.ttf");
    font_handles.cjk = asset_server.load("fonts/NotoSansCJKJP-Light.ttf");
    info!("Loaded Vello fonts: CascadiaCodeNF, NotoSerif, NotoSansCJKJP");
}

/// Load bundled fonts into the SVG fontdb for `<text>` element rendering.
///
/// usvg flattens SVG text to outlines during parsing, but only if the
/// referenced fonts are in its database. Without this, `<text>` elements
/// are silently dropped from rendered SVGs.
fn load_svg_fontdb(mut svg_fontdb: ResMut<SvgFontDb>, theme: Res<crate::ui::theme::Theme>) {
    use bevy_vello::integrations::svg::usvg::fontdb;
    use std::sync::Arc;

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

    // Map CSS generic families to our bundled fonts (from Rhai theme).
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
/// what bevy_vello renders — critical for accurate cursor placement.
fn update_text_metrics_from_font(
    font_handles: Res<FontHandles>,
    fonts: Res<Assets<VelloFont>>,
    mut text_metrics: ResMut<TextMetrics>,
) {
    // Early exit if both metrics are already measured
    if text_metrics.cell_line_height_from_font && text_metrics.cell_char_width_from_font {
        return;
    }
    let Some(font) = fonts.get(&font_handles.mono) else {
        return;
    };
    let style = VelloTextStyle {
        font_size: text_metrics.cell_font_size,
        font: font_handles.mono.clone(),
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
