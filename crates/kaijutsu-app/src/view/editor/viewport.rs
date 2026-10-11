//! The editor's viewport and chrome math: which rows are on screen, where the
//! mode line and the `~` filler go, and what the mode line says.
//!
//! Pure functions over numbers and strings, so the scrolling contract is
//! tested without a GPU. `render` feeds them parley's line metrics and applies
//! the results. The kernel owns the cursor; the viewport follows it. All
//! lengths are logical px in layout space (the top of the text is 0).

use crate::shaders::SelectionRect;

/// A visual line's vertical extent in layout space.
pub type LineSpan = (f32, f32);

/// Translate a char offset into `text` to a `(row, column)` position, both
/// counted in chars. An offset past the end lands at the end: the kernel owns
/// the cursor, so a disagreement about length must not panic the renderer.
pub fn cursor_row_col(text: &str, offset: u64) -> (usize, usize) {
    let mut row = 0usize;
    let mut col = 0usize;
    for (seen, ch) in text.chars().enumerate() {
        if seen as u64 >= offset {
            return (row, col);
        }
        if ch == '\n' {
            row += 1;
            col = 0;
        } else {
            col += 1;
        }
    }
    (row, col)
}

/// The scroll offset that keeps the cursor line inside a window `body_h` tall.
///
/// The window moves only when the cursor leaves it, the way vi does, and always
/// lands on a line top so no row is cut in half. `cursor_y` is any y inside the
/// cursor's line. `lines` are the visual lines top to bottom.
pub fn follow_cursor(scroll: f32, lines: &[LineSpan], cursor_y: f32, body_h: f32) -> f32 {
    let Some(&(top, bottom)) = lines
        .iter()
        .find(|(t, b)| cursor_y >= *t && cursor_y < *b)
        .or(lines.last())
    else {
        return 0.0;
    };
    if top < scroll {
        return top;
    }
    if bottom > scroll + body_h {
        // The first line top that leaves the cursor line's bottom in the window.
        // A line taller than the window keeps its own top in view.
        let want = (bottom - body_h).min(top);
        return lines
            .iter()
            .map(|(t, _)| *t)
            .find(|t| *t >= want)
            .unwrap_or(top);
    }
    scroll
}

/// Is a glyph whose baseline sits at `baseline` (surface space) on a row
/// inside the body band `top..=bottom`? A baseline sits strictly below its
/// row's top and at or above its bottom, so a row above the band has its
/// baseline at or above `top`.
pub fn baseline_in_body(baseline: f32, top: f32, bottom: f32) -> bool {
    baseline > top && baseline <= bottom
}

/// Clip `rects` to the body band `top..bottom`, dropping those outside it.
pub fn clip_rects_to_band(rects: Vec<SelectionRect>, top: f32, bottom: f32) -> Vec<SelectionRect> {
    rects
        .into_iter()
        .filter_map(|r| {
            let y0 = r.y.max(top);
            let y1 = r.bottom().min(bottom);
            (y1 > y0).then(|| SelectionRect::new(r.x, y0, r.width, y1 - y0))
        })
        .collect()
}

/// How many `~` rows fill the body below the text: the rows between the end of
/// the text (surface space) and the bottom of the band.
pub fn filler_rows(text_end_y: f32, body_bottom: f32, line_h: f32) -> usize {
    if line_h <= 0.0 {
        return 0;
    }
    (((body_bottom - text_end_y) / line_h) + 1e-3).floor().max(0.0) as usize
}

/// The mode line's two halves: `foo.kai [+]` on the left, `-- INSERT --   12,3`
/// on the right. Position is 1-based.
pub fn mode_line_parts(
    path: &str,
    dirty: bool,
    mode: Option<&str>,
    row: usize,
    col: usize,
) -> (String, String) {
    let mut left = path.to_string();
    if dirty {
        left.push_str(" [+]");
    }
    let position = format!("{},{}", row + 1, col + 1);
    let right = match mode {
        Some(m) if !m.is_empty() => format!("{m}   {position}"),
        _ => position,
    };
    (left, right)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Ten 20px lines: (0,20) (20,40) ...
    fn lines(n: usize) -> Vec<LineSpan> {
        (0..n).map(|i| (i as f32 * 20.0, (i + 1) as f32 * 20.0)).collect()
    }

    #[test]
    fn row_col_counts_chars_not_bytes() {
        assert_eq!(cursor_row_col("café\nxy", 0), (0, 0));
        assert_eq!(cursor_row_col("café\nxy", 4), (0, 4));
        assert_eq!(cursor_row_col("café\nxy", 6), (1, 1));
        assert_eq!(cursor_row_col("ab", 99), (0, 2), "past the end lands at the end");
    }

    #[test]
    fn a_cursor_inside_the_window_does_not_scroll() {
        assert_eq!(follow_cursor(0.0, &lines(50), 90.0, 200.0), 0.0);
        assert_eq!(follow_cursor(100.0, &lines(50), 150.0, 200.0), 100.0);
    }

    #[test]
    fn a_cursor_below_the_window_scrolls_it_just_far_enough() {
        // Window holds rows 0..10 (200px); the cursor is on row 14 (280..300).
        let scroll = follow_cursor(0.0, &lines(50), 285.0, 200.0);
        assert_eq!(scroll, 100.0, "row 14 is the last visible row");
        assert_eq!(scroll % 20.0, 0.0, "the window lands on a line top");
    }

    #[test]
    fn a_cursor_above_the_window_scrolls_it_up_to_the_cursor_line() {
        assert_eq!(follow_cursor(300.0, &lines(50), 105.0, 200.0), 100.0);
    }

    #[test]
    fn the_last_row_is_reachable() {
        let l = lines(100);
        let scroll = follow_cursor(0.0, &l, 1990.0, 200.0);
        assert_eq!(scroll, 1800.0);
    }

    #[test]
    fn a_line_taller_than_the_window_keeps_its_top_in_view() {
        // One wrapped line spans 100..500, the window is 200 tall: show the
        // line's top, not a later line's.
        let mut l = lines(5);
        l.push((100.0, 500.0));
        l.sort_by(|a, b| a.0.total_cmp(&b.0));
        l.retain(|(t, b)| !(*t > 100.0 && *b <= 500.0));
        l.push((500.0, 520.0));
        assert_eq!(follow_cursor(0.0, &l, 300.0, 200.0), 100.0);
    }

    #[test]
    fn no_lines_means_no_scroll() {
        assert_eq!(follow_cursor(40.0, &[], 0.0, 200.0), 0.0);
    }

    #[test]
    fn baselines_outside_the_band_are_culled() {
        // Band 52..252: the row above it has its baseline at or above 52.
        assert!(!baseline_in_body(52.0, 52.0, 252.0));
        assert!(baseline_in_body(66.0, 52.0, 252.0));
        assert!(baseline_in_body(252.0, 52.0, 252.0));
        assert!(!baseline_in_body(266.0, 52.0, 252.0));
    }

    #[test]
    fn rects_are_clipped_to_the_band() {
        let rects = vec![
            SelectionRect::new(0.0, 0.0, 10.0, 30.0),   // straddles the top
            SelectionRect::new(0.0, 100.0, 10.0, 20.0), // inside
            SelectionRect::new(0.0, 400.0, 10.0, 20.0), // below
            SelectionRect::new(0.0, 190.0, 10.0, 40.0), // straddles the bottom
        ];
        let out = clip_rects_to_band(rects, 20.0, 200.0);
        assert_eq!(out.len(), 3);
        assert_eq!((out[0].y, out[0].height), (20.0, 10.0));
        assert_eq!((out[1].y, out[1].height), (100.0, 20.0));
        assert_eq!((out[2].y, out[2].height), (190.0, 10.0));
    }

    #[test]
    fn filler_fills_the_rows_below_the_text() {
        assert_eq!(filler_rows(100.0, 200.0, 20.0), 5);
        assert_eq!(filler_rows(100.0, 210.0, 20.0), 5, "a partial row is not a row");
        assert_eq!(filler_rows(200.0, 200.0, 20.0), 0, "a full body has none");
        assert_eq!(filler_rows(300.0, 200.0, 20.0), 0);
        assert_eq!(filler_rows(0.0, 200.0, 0.0), 0);
    }

    #[test]
    fn mode_line_matches_the_tui() {
        assert_eq!(
            mode_line_parts("foo.kai", true, Some("-- INSERT --"), 11, 2),
            ("foo.kai [+]".to_string(), "-- INSERT --   12,3".to_string())
        );
        assert_eq!(
            mode_line_parts("foo.kai", false, None, 0, 0),
            ("foo.kai".to_string(), "1,1".to_string())
        );
        assert_eq!(mode_line_parts("a", false, Some(""), 0, 0).1, "1,1");
    }
}
