//! Vello scene builders for form visual elements.
//!
//! Pure functions that build `vello::Scene` fragments for form chrome:
//! field borders, row selection highlights, and button backgrounds.
//! Analogous to `view/fieldset.rs` for conversation block borders.

use bevy::prelude::*;
use bevy_vello::vello;
use vello::kurbo::{Affine, RoundedRect, Stroke, Cap};
use vello::peniko::Fill;

use crate::text::components::bevy_color_to_brush;

/// Draw a filled rect for a full-viewport overlay background.
pub fn build_overlay_bg(
    scene: &mut vello::Scene,
    width: f64,
    height: f64,
    color: Color,
) {
    let brush = bevy_color_to_brush(color);
    let rect = RoundedRect::new(0.0, 0.0, width, height, 0.0);
    scene.fill(Fill::NonZero, Affine::IDENTITY, &brush, None, &rect);
}

/// Draw a stroked rounded rect for a form field container border.
pub fn build_form_field_border(
    scene: &mut vello::Scene,
    width: f64,
    height: f64,
    color: Color,
    radius: f64,
    thickness: f64,
) {
    let brush = bevy_color_to_brush(color);
    let stroke = Stroke::new(thickness).with_caps(Cap::Butt);
    let half = thickness / 2.0;
    let rect = RoundedRect::new(half, half, width - half, height - half, radius);
    scene.stroke(&stroke, Affine::IDENTITY, &brush, None, &rect);
}

/// Draw a filled rect for row selection highlight.
pub fn build_row_highlight(
    scene: &mut vello::Scene,
    width: f64,
    height: f64,
    color: Color,
) {
    let brush = bevy_color_to_brush(color);
    let rect = RoundedRect::new(0.0, 0.0, width, height, 2.0);
    scene.fill(Fill::NonZero, Affine::IDENTITY, &brush, None, &rect);
}

/// Draw a filled + stroked rounded rect for modal panel background and border.
pub fn build_modal_panel(
    scene: &mut vello::Scene,
    width: f64,
    height: f64,
    bg_color: Color,
    border_color: Color,
    radius: f64,
    thickness: f64,
) {
    let rect = RoundedRect::new(0.0, 0.0, width, height, radius);
    // Fill background
    let bg_brush = bevy_color_to_brush(bg_color);
    scene.fill(Fill::NonZero, Affine::IDENTITY, &bg_brush, None, &rect);
    // Stroke border
    let border_brush = bevy_color_to_brush(border_color);
    let stroke = Stroke::new(thickness).with_caps(Cap::Butt);
    let half = thickness / 2.0;
    let border_rect = RoundedRect::new(half, half, width - half, height - half, radius);
    scene.stroke(&stroke, Affine::IDENTITY, &border_brush, None, &border_rect);
}

/// Draw a filled rounded rect for button backgrounds.
pub fn build_button_bg(
    scene: &mut vello::Scene,
    width: f64,
    height: f64,
    color: Color,
    radius: f64,
) {
    let brush = bevy_color_to_brush(color);
    let rect = RoundedRect::new(0.0, 0.0, width, height, radius);
    scene.fill(Fill::NonZero, Affine::IDENTITY, &brush, None, &rect);
}
