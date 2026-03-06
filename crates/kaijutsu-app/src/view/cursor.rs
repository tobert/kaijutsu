//! Cursor utilities.
//!
//! The overlay cursor is drawn directly in the overlay's Vello scene.
//! This module provides shared helper functions for cursor positioning.

/// Calculate row and column from text and cursor offset.
///
/// O(N) string scan but only runs when cursor position changes.
#[inline]
pub fn cursor_row_col(text: &str, offset: usize) -> (usize, usize) {
    let offset = offset.min(text.len());
    let before = &text[..offset];
    let row = before.matches('\n').count();
    let col = match before.rfind('\n') {
        Some(pos) => before[pos + 1..].chars().count(),
        None => before.chars().count(),
    };
    (row, col)
}
