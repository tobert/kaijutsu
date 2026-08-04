//! Selection rectangles — the shape every highlighted range is drawn from.
//!
//! One selection is **many** rectangles: a range that crosses a line break (or
//! a wrap point) covers a ragged first row, some whole middle rows, and a
//! ragged last row. Parley hands exactly that back from
//! `Selection::geometry(layout)`, one box per visual line; everything here is
//! what happens between that and a draw call.
//!
//! Two consumers, deliberately sharing the middle:
//!
//! - [`BlockFxMaterial`](super::BlockFxMaterial) composites the rects **over**
//!   the text in the fragment shader ([`pack_selection_rects`] turns them into
//!   the uniform). That is the path the editor panel and the compose overlay
//!   use, and the one `docs/vi.md` names as the prerequisite for editor
//!   selection rects.
//! - The diff viewer draws its selection **behind** the text as
//!   `MsdfBlockGeometry` quads, because a diff selection has to sit on top of
//!   the `+`/`-` bands and under the glyphs. Same rects, different compositor.
//!
//! # Middle rows are drawn full width, on purpose
//!
//! [`coalesce_selection_rects`] replaces the interior rows of a multi-row
//! selection with **one** full-width band. That is what every text editor
//! draws — the band to the right edge is how you see that the newline is part
//! of the selection — and it has a second, load-bearing property: a contiguous
//! selection is then **at most three rects** no matter how many rows it spans,
//! so the uniform's fixed capacity can never truncate one.

use bevy::prelude::*;
use bevy::render::render_resource::ShaderType;

/// How many rects one surface can composite at once.
///
/// A contiguous selection coalesces to at most three
/// ([`coalesce_selection_rects`]), so this is headroom for the shapes that do
/// not coalesce — bidi runs splitting a row, and the multi-cursor/search-match
/// highlighting a later slice may want — not a bound anything normal reaches.
pub const MAX_SELECTION_RECTS: usize = 16;

/// One selection rectangle, in whatever space its producer works in —
/// surface-local **logical** pixels on the way in, UV on the way out of
/// [`pack_selection_rects`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SelectionRect {
    /// Left edge.
    pub x: f32,
    /// Top edge.
    pub y: f32,
    /// Width; a rect with zero (or negative) width draws nothing.
    pub width: f32,
    /// Height; likewise.
    pub height: f32,
}

impl SelectionRect {
    /// A rect from its top-left corner and size.
    pub fn new(x: f32, y: f32, width: f32, height: f32) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }

    /// A rect from its edges, in the order Parley's `BoundingBox` reports.
    pub fn from_edges(x0: f32, y0: f32, x1: f32, y1: f32) -> Self {
        Self::new(x0, y0, x1 - x0, y1 - y0)
    }

    /// Right edge.
    pub fn right(&self) -> f32 {
        self.x + self.width
    }

    /// Bottom edge.
    pub fn bottom(&self) -> f32 {
        self.y + self.height
    }

    /// Does this rect cover any pixels at all?
    pub fn is_visible(&self) -> bool {
        self.width > 0.0 && self.height > 0.0
    }
}

/// The multi-rect selection uniform: a fixed array of `[x, y, w, h]` rects in
/// **UV space**, and how many of them are live.
///
/// Fixed-size rather than a storage buffer on purpose — a uniform array is
/// portable everywhere wgpu runs, and [`MAX_SELECTION_RECTS`] rects is a
/// quarter of a kilobyte. The shader loops to `count`, not to the capacity, so
/// the usual one-or-three-rect case costs three iterations.
#[derive(ShaderType, Debug, Clone)]
pub struct SelectionRects {
    /// `[x, y, width, height]` per rect, UV space, top-left origin.
    pub rects: [Vec4; MAX_SELECTION_RECTS],
    /// How many entries of `rects` the shader should read.
    pub count: u32,
}

impl Default for SelectionRects {
    fn default() -> Self {
        Self {
            rects: [Vec4::ZERO; MAX_SELECTION_RECTS],
            count: 0,
        }
    }
}

impl SelectionRects {
    /// Nothing selected — the shader skips the composite entirely.
    pub fn none() -> Self {
        Self::default()
    }
}

/// Collapse a per-visual-row selection into the three-rect shape an editor
/// draws: ragged head, one full-width body, ragged tail.
///
/// `rects` must be in visual order, one per row (a producer that can return
/// several boxes for one row — bidi — should union them first; the interior
/// rows become full width anyway, so only the first and last rows can be
/// affected, and over-covering a discontiguous *bidi* selection is the same
/// thing every terminal does).
///
/// `left`/`right` are the content edges the full-width band spans, in the same
/// space as the rects. The head is extended to `right` and the tail begins at
/// `left`, because both of those rows continue into the next one — a head that
/// stopped at the last glyph would read as if the newline were excluded.
pub fn coalesce_selection_rects(
    rects: &[SelectionRect],
    left: f32,
    right: f32,
) -> Vec<SelectionRect> {
    let visible: Vec<SelectionRect> = rects.iter().copied().filter(|r| r.height > 0.0).collect();
    match visible.len() {
        0 => Vec::new(),
        // A selection inside one row is exactly what Parley measured.
        1 => visible.into_iter().filter(|r| r.is_visible()).collect(),
        n => {
            let mut out = Vec::with_capacity(3);
            let head = visible[0];
            out.push(SelectionRect::from_edges(
                head.x,
                head.y,
                right.max(head.x),
                head.bottom(),
            ));
            if n > 2 {
                let first = visible[1];
                let last = visible[n - 2];
                out.push(SelectionRect::from_edges(
                    left,
                    first.y,
                    right,
                    last.bottom(),
                ));
            }
            let tail = visible[n - 1];
            out.push(SelectionRect::from_edges(
                left.min(tail.right()),
                tail.y,
                tail.right(),
                tail.bottom(),
            ));
            out.retain(|r| r.is_visible());
            out
        }
    }
}

/// Convert pixel-space rects into the material's UV-space uniform.
///
/// Rects that cover no pixels are dropped rather than sent as zero-size
/// entries, so `count` is always the number of rects actually drawn. A surface
/// with no size yet (`rtt_w`/`rtt_h` zero, before the first RTT build) packs to
/// nothing — there is no UV space to map into.
///
/// **Overflow is loud, not silent**: more than [`MAX_SELECTION_RECTS`] visible
/// rects is a producer that did not coalesce, and the extras are dropped with a
/// warning rather than quietly changing what the reader sees.
pub fn pack_selection_rects(rects: &[SelectionRect], rtt_w: f32, rtt_h: f32) -> SelectionRects {
    let mut packed = SelectionRects::none();
    if rtt_w <= 0.0 || rtt_h <= 0.0 {
        return packed;
    }
    let mut n = 0usize;
    let mut dropped = 0usize;
    for rect in rects.iter().filter(|r| r.is_visible()) {
        if n >= MAX_SELECTION_RECTS {
            dropped += 1;
            continue;
        }
        packed.rects[n] = Vec4::new(
            rect.x / rtt_w,
            rect.y / rtt_h,
            rect.width / rtt_w,
            rect.height / rtt_h,
        );
        n += 1;
    }
    if dropped > 0 {
        warn!(
            "selection overflowed the material: {dropped} rect(s) not drawn \
             (cap {MAX_SELECTION_RECTS}) — the producer should coalesce"
        );
    }
    packed.count = n as u32;
    packed
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rows(n: usize, width: f32) -> Vec<SelectionRect> {
        (0..n)
            .map(|i| SelectionRect::new(0.0, i as f32 * 10.0, width, 10.0))
            .collect()
    }

    /// A selection inside one row is left exactly as measured — the ragged
    /// end is the whole point of a character-wise selection.
    #[test]
    fn a_single_row_selection_is_untouched() {
        let one = [SelectionRect::new(12.0, 30.0, 44.0, 10.0)];
        assert_eq!(coalesce_selection_rects(&one, 0.0, 900.0), one.to_vec());
    }

    /// **The property the uniform's capacity rests on**: however many rows a
    /// contiguous selection spans, it draws in at most three rects.
    #[test]
    fn any_contiguous_selection_coalesces_to_at_most_three_rects() {
        for n in 1..200 {
            let out = coalesce_selection_rects(&rows(n, 500.0), 0.0, 900.0);
            assert!(
                out.len() <= 3,
                "{n} rows coalesced to {} rects",
                out.len(),
            );
        }
    }

    /// Head and tail keep their ragged ends and reach the edge they continue
    /// over; the body between them is one full-width band.
    #[test]
    fn the_head_reaches_the_right_edge_and_the_tail_starts_at_the_left() {
        let input = vec![
            SelectionRect::new(120.0, 0.0, 80.0, 10.0),
            SelectionRect::new(0.0, 10.0, 300.0, 10.0),
            SelectionRect::new(0.0, 20.0, 400.0, 10.0),
            SelectionRect::new(0.0, 30.0, 55.0, 10.0),
        ];
        let out = coalesce_selection_rects(&input, 0.0, 900.0);
        assert_eq!(out.len(), 3);

        assert_eq!(out[0], SelectionRect::new(120.0, 0.0, 780.0, 10.0));
        assert_eq!(
            out[1],
            SelectionRect::new(0.0, 10.0, 900.0, 20.0),
            "the interior rows are one full-width band",
        );
        assert_eq!(out[2], SelectionRect::new(0.0, 30.0, 55.0, 10.0));
    }

    /// Two rows have no interior: head and tail, nothing between them.
    #[test]
    fn a_two_row_selection_is_head_and_tail_only() {
        let input = vec![
            SelectionRect::new(50.0, 0.0, 20.0, 10.0),
            SelectionRect::new(0.0, 10.0, 30.0, 10.0),
        ];
        let out = coalesce_selection_rects(&input, 0.0, 900.0);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].right(), 900.0, "the head runs to the edge");
        assert_eq!(out[1], SelectionRect::new(0.0, 10.0, 30.0, 10.0));
    }

    /// Zero-height rows (a Parley line with no metrics yet) contribute
    /// nothing rather than a degenerate quad.
    #[test]
    fn empty_rects_are_dropped() {
        let input = vec![
            SelectionRect::new(0.0, 0.0, 100.0, 0.0),
            SelectionRect::new(0.0, 0.0, 0.0, 10.0),
        ];
        assert!(coalesce_selection_rects(&input, 0.0, 900.0).is_empty());
        assert_eq!(pack_selection_rects(&input, 800.0, 600.0).count, 0);
    }

    #[test]
    fn packing_maps_pixels_to_uv() {
        let packed = pack_selection_rects(
            &[SelectionRect::new(100.0, 300.0, 200.0, 30.0)],
            800.0,
            600.0,
        );
        assert_eq!(packed.count, 1);
        assert_eq!(packed.rects[0], Vec4::new(0.125, 0.5, 0.25, 0.05));
        // Untouched slots stay zero: the shader never reads past `count`, but
        // stale rects in the buffer would be a nasty way to find that out.
        assert_eq!(packed.rects[1], Vec4::ZERO);
    }

    /// A surface that has not been built yet has no UV space to map into.
    #[test]
    fn packing_a_sizeless_surface_selects_nothing() {
        let one = [SelectionRect::new(0.0, 0.0, 10.0, 10.0)];
        assert_eq!(pack_selection_rects(&one, 0.0, 600.0).count, 0);
        assert_eq!(pack_selection_rects(&one, 800.0, 0.0).count, 0);
    }

    /// Overflow drops the extras — visibly, in the log — rather than writing
    /// past the array.
    #[test]
    fn packing_clamps_at_the_capacity() {
        let many = rows(MAX_SELECTION_RECTS + 7, 100.0);
        let packed = pack_selection_rects(&many, 800.0, 600.0);
        assert_eq!(packed.count as usize, MAX_SELECTION_RECTS);
    }

    /// The empty selection is the default, and it is what "draw nothing"
    /// means to the shader.
    #[test]
    fn no_selection_packs_to_a_zero_count() {
        assert_eq!(SelectionRects::none().count, 0);
        assert_eq!(pack_selection_rects(&[], 800.0, 600.0).count, 0);
    }
}
