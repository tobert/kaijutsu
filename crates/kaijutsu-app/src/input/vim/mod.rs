//! Vi surfaces' shared pieces: key conversion and the cursor shape a mode
//! banner draws.
//!
//! Compose runs `kaijutsu_editor::EditorCore` (`input/compose/`), the kernel
//! editor runs it kernel-side, and the diff viewer runs its own `DiffCore`.
//! Each names its mode with a modalkit banner, and [`mode_kind`] turns that
//! banner into a cursor shape for the renderer.

pub mod keyconv;

/// Vim cursor shape — drives the renderer's cursor width and color.
///
/// Stored as a small enum (Copy) so we can stash it on `OverlayCursorGeometry`
/// without coupling the renderer to vim mode strings.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Default)]
pub enum CursorKind {
    /// Insert mode (or no vim) — thin beam cursor.
    #[default]
    Beam,
    /// Normal mode — block cursor that overlays the glyph at cursor.
    Block,
    /// Replace mode (`R`) — thin underline at glyph baseline.
    Underline,
    /// Visual mode — cursor hidden; the selection rect renders instead.
    Hidden,
}

/// Classify a `VimMachine::show_mode()` string into a renderer-facing
/// cursor kind.
///
/// `None` means modalkit is in Normal mode (no banner). The non-None
/// strings come from modalkit and look like `-- INSERT --`, `-- VISUAL --`,
/// `-- VISUAL LINE --`, `-- REPLACE --`, etc. We classify on `starts_with`
/// so future suffixes (block selection, line mode markers) don't break us.
pub fn mode_kind(mode_str: Option<&str>) -> CursorKind {
    match mode_str {
        // Normal mode: no banner.
        None => CursorKind::Block,
        Some(s) => {
            let trimmed = s.trim().trim_start_matches("--").trim();
            if trimmed.starts_with("REPLACE") {
                CursorKind::Underline
            } else if trimmed.starts_with("INSERT") {
                CursorKind::Beam
            } else if trimmed.starts_with("VISUAL") || trimmed.starts_with("SELECT") {
                CursorKind::Hidden
            } else {
                // Unknown mode — default to block (matches Normal). Better
                // than a phantom beam if modalkit adds modes we don't know.
                CursorKind::Block
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mode_kind_normal_is_block() {
        assert_eq!(mode_kind(None), CursorKind::Block);
    }

    #[test]
    fn mode_kind_insert_is_beam() {
        assert_eq!(mode_kind(Some("-- INSERT --")), CursorKind::Beam);
    }

    #[test]
    fn mode_kind_visual_is_hidden() {
        assert_eq!(mode_kind(Some("-- VISUAL --")), CursorKind::Hidden);
        assert_eq!(mode_kind(Some("-- VISUAL LINE --")), CursorKind::Hidden);
        assert_eq!(mode_kind(Some("-- VISUAL BLOCK --")), CursorKind::Hidden);
    }

    #[test]
    fn mode_kind_replace_is_underline() {
        assert_eq!(mode_kind(Some("-- REPLACE --")), CursorKind::Underline);
    }

    #[test]
    fn mode_kind_unknown_defaults_to_block() {
        assert_eq!(mode_kind(Some("-- WEIRDMODE --")), CursorKind::Block);
    }
}
