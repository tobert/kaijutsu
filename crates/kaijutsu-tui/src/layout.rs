//! Width-aware layout for kaish's structured tool output (`OutputData`).
//!
//! `kaish` owns the data; the tui owns the layout (`docs/tui.md`, guidance
//! 7). [`layout_output`] is the formatter that guidance names as a later
//! lane: given the same `OutputData` the kaish REPL lays out in columns at
//! its own terminal width, this module does the same job with `ratatui`
//! rather than by shelling out to a binary crate that reads the terminal
//! itself.
//!
//! Pure: no RPC, no I/O, no clock — the same discipline [`crate::present`]
//! holds. [`layout_output`] returns `None` when `data` has no shape better
//! than its own text; the caller keeps its existing wrap-and-color path for
//! that case (`kaijutsu_present::format::format_output_data`).

use kaijutsu_present::format::BlockTone;
use kaijutsu_present::markdown::SpanTone;
use kaijutsu_types::{OutputData, OutputEntryType, OutputNode};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthChar;

use crate::present::Palette;

/// Space between adjacent columns, in both the table and the `ls -C` grid.
const GUTTER: usize = 2;

/// Lay out `data` at `width` columns: an aligned table, an `ls -C` grid, an
/// indented tree, or pretty-printed JSON — whichever shape it carries.
///
/// `None` when `data.as_text()` is `Some` — plain text has no shape this
/// module improves on, and the caller's own wrap-and-color path already
/// handles it (word wrap, ANSI spans). The returned lines carry no fg color
/// of their own except an [`entry_style`] hint on a name; the caller patches
/// the block's own tone underneath (`present::render_block`), the same way
/// it already layers markdown span tones over a block's base style.
pub fn layout_output(data: &OutputData, width: u16, palette: &Palette) -> Option<Vec<Line<'static>>> {
    if data.as_text().is_some() {
        return None;
    }
    let width = usize::from(width.max(1));

    let lines = if data.headers.is_some() || data.is_tabular() {
        layout_table(data, width, palette)
    } else if !data.is_flat() {
        layout_tree(data, palette)
    } else if !data.root.is_empty() {
        layout_flat_list(data, width, palette)
    } else if let Some(rich) = &data.rich_json {
        layout_rich_json(rich, width)
    } else {
        return None;
    };

    Some(lines.into_iter().map(|l| clamp_line(l, width)).collect())
}

/// The color hint a name cell gets from its [`OutputEntryType`], drawn only
/// from styles the palette already resolves for another tone — never a new
/// color. `Directory` borrows the markdown "strong" weight, `Executable`
/// borrows the resource-block green, `Symlink` borrows the markdown "code"
/// cyan; `File`/`Text` (and any future variant, since `OutputEntryType` is
/// `#[non_exhaustive]`) carry no hint of their own.
fn entry_style(entry_type: OutputEntryType, palette: &Palette) -> Style {
    match entry_type {
        OutputEntryType::Directory => palette.span(SpanTone::Strong),
        OutputEntryType::Executable => palette.block(BlockTone::Resource),
        OutputEntryType::Symlink => palette.span(SpanTone::Code),
        _ => Style::new(),
    }
}

// ── table ────────────────────────────────────────────────────────────────

/// Aligned columns at `width`: header row (if any) in the palette's heading
/// weight, then one row per node — name first, then its `cells`. Column
/// widths come from display width with a two-space gutter; when the natural
/// width exceeds `width`, [`shrink_to_fit`] narrows the widest columns with
/// an ellipsis before the first column ever gives up a character it needs.
fn layout_table(data: &OutputData, width: usize, palette: &Palette) -> Vec<Line<'static>> {
    let headers = data.headers.as_deref();
    let header_cols = headers.map_or(0, <[String]>::len);
    let max_row_cols = data.root.iter().map(|n| n.cells.len() + 1).max().unwrap_or(0);
    let num_cols = header_cols.max(max_row_cols).max(1);

    let mut natural = vec![0usize; num_cols];
    if let Some(h) = headers {
        for (i, s) in h.iter().enumerate().take(num_cols) {
            natural[i] = natural[i].max(display_width(s));
        }
    }
    for node in &data.root {
        natural[0] = natural[0].max(display_width(node.display_name()));
        for (i, c) in node.cells.iter().enumerate() {
            let col = i + 1;
            if col < num_cols {
                natural[col] = natural[col].max(display_width(c));
            }
        }
    }

    let widths = shrink_to_fit(&natural, width);

    let mut lines = Vec::with_capacity(data.root.len() + 1);
    if let Some(h) = headers {
        let cells: Vec<&str> = h.iter().map(String::as_str).collect();
        let style = palette.span(SpanTone::Heading);
        lines.push(table_row(&cells, &widths, |_| style));
    }
    for node in &data.root {
        let mut cells: Vec<&str> = Vec::with_capacity(num_cols);
        cells.push(node.display_name());
        cells.extend(node.cells.iter().map(String::as_str));
        let entry_type = node.entry_type;
        lines.push(table_row(&cells, &widths, |i| {
            if i == 0 {
                entry_style(entry_type, palette)
            } else {
                Style::new()
            }
        }));
    }
    lines
}

/// One table row: `cells` padded to `widths`, joined by [`GUTTER`] spaces,
/// the last column unpadded (nothing pastes trailing whitespace).
fn table_row(cells: &[&str], widths: &[usize], style_for: impl Fn(usize) -> Style) -> Line<'static> {
    let n = widths.len();
    let mut spans = Vec::with_capacity(n * 2);
    for (i, &w) in widths.iter().enumerate() {
        if i > 0 {
            spans.push(Span::raw(" ".repeat(GUTTER)));
        }
        let cell = cells.get(i).copied().unwrap_or("");
        let cut = ellipsize(cell, w);
        let pad = w.saturating_sub(display_width(&cut));
        let text = if i + 1 < n {
            format!("{cut}{}", " ".repeat(pad))
        } else {
            cut
        };
        spans.push(Span::styled(text, style_for(i)));
    }
    Line::from(spans)
}

/// Column widths that fit `width`: start from each column's natural
/// (longest-content) width, and while the row is too wide, take one column
/// off the currently-widest column that is still above its floor.
///
/// Every column but the first floors at 1; the first column floors at its
/// own natural width — never shrunk — unless that width alone already
/// exceeds `width`, in which case it floors at 1 like the rest. If every
/// column is already at its floor and the row still does not fit, the caller
/// falls back to [`clamp_line`], which never lets a line exceed `width`.
fn shrink_to_fit(natural: &[usize], width: usize) -> Vec<usize> {
    let num_cols = natural.len();
    if num_cols == 0 {
        return Vec::new();
    }
    let gutters = GUTTER * (num_cols - 1);
    let total = |w: &[usize]| w.iter().sum::<usize>() + gutters;

    let mut widths = natural.to_vec();
    if total(&widths) <= width {
        return widths;
    }

    let mut floor = vec![1usize; num_cols];
    if natural[0] <= width {
        floor[0] = natural[0];
    }

    while total(&widths) > width {
        let candidate = widths
            .iter()
            .enumerate()
            .filter(|(i, w)| **w > floor[*i])
            .max_by_key(|(_, w)| **w)
            .map(|(i, _)| i);
        match candidate {
            Some(i) => widths[i] -= 1,
            None => break,
        }
    }
    widths
}

// ── flat list (`ls -C`) ─────────────────────────────────────────────────

/// `ls -C`'s own fill order: column-major, down then across. Column width is
/// the longest name plus a two-space gutter; the column count is however
/// many of those fit in `width`, at least one.
fn layout_flat_list(data: &OutputData, width: usize, palette: &Palette) -> Vec<Line<'static>> {
    let names: Vec<(&str, OutputEntryType)> = data
        .root
        .iter()
        .map(|n| (n.display_name(), n.entry_type))
        .collect();
    if names.is_empty() {
        return Vec::new();
    }

    let longest = names.iter().map(|(n, _)| display_width(n)).max().unwrap_or(0);
    let col_width = longest + GUTTER;
    let num_cols = (width / col_width).max(1);
    let n = names.len();
    let num_rows = n.div_ceil(num_cols);

    let mut lines = Vec::with_capacity(num_rows);
    for r in 0..num_rows {
        let mut spans = Vec::new();
        for c in 0..num_cols {
            let idx = c * num_rows + r;
            let Some((name, entry_type)) = names.get(idx).copied() else {
                break;
            };
            let last_in_row = idx + num_rows >= n;
            let pad = col_width.saturating_sub(display_width(name));
            let text = if last_in_row {
                name.to_string()
            } else {
                format!("{name}{}", " ".repeat(pad))
            };
            spans.push(Span::styled(text, entry_style(entry_type, palette)));
        }
        lines.push(Line::from(spans));
    }
    lines
}

// ── tree ────────────────────────────────────────────────────────────────

/// An indented tree: two spaces per level, no box-drawing glyphs — nothing
/// pasteable sits inside a box (`docs/tui.md`, "Surfaces").
fn layout_tree(data: &OutputData, palette: &Palette) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    for node in &data.root {
        tree_node(node, 0, palette, &mut lines);
    }
    lines
}

fn tree_node(node: &OutputNode, depth: usize, palette: &Palette, lines: &mut Vec<Line<'static>>) {
    let mut spans = vec![Span::raw("  ".repeat(depth))];
    spans.push(Span::styled(
        node.display_name().to_string(),
        entry_style(node.entry_type, palette),
    ));
    if !node.cells.is_empty() {
        spans.push(Span::raw(format!("  {}", node.cells.join("  "))));
    }
    lines.push(Line::from(spans));
    for child in &node.children {
        tree_node(child, depth + 1, palette, lines);
    }
}

// ── rich JSON ───────────────────────────────────────────────────────────

/// Pretty-printed JSON, one output line per source line, each cut to `width`
/// rather than wrapped — a long value reads better cut than folded mid-key.
fn layout_rich_json(rich: &serde_json::Value, width: usize) -> Vec<Line<'static>> {
    let pretty = serde_json::to_string_pretty(rich).unwrap_or_else(|_| rich.to_string());
    pretty
        .lines()
        .map(|l| Line::from(Span::raw(ellipsize(l, width))))
        .collect()
}

// ── shared width helpers ───────────────────────────────────────────────

fn display_width(s: &str) -> usize {
    s.chars().map(|c| c.width().unwrap_or(0)).sum()
}

/// Cut to `width` columns, marking the cut with `…` when anything was lost.
fn ellipsize(s: &str, width: usize) -> String {
    if display_width(s) <= width {
        return s.to_string();
    }
    if width == 0 {
        return String::new();
    }
    let mut out = String::new();
    let mut w = 0usize;
    for ch in s.chars() {
        let cw = ch.width().unwrap_or(0);
        if w + cw > width.saturating_sub(1) {
            break;
        }
        out.push(ch);
        w += cw;
    }
    out.push('…');
    out
}

/// The width guarantee every shape above shares: whatever the column math
/// above computed, a line never leaves this module wider than `width`. A
/// pathological case (more columns than a narrow width can floor at 1
/// character each) falls back to one unstyled, ellipsized run rather than
/// let a line overflow.
fn clamp_line(line: Line<'static>, width: usize) -> Line<'static> {
    let total: usize = line.spans.iter().map(|s| display_width(&s.content)).sum();
    if total <= width {
        return line;
    }
    let plain: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
    Line::from(Span::raw(ellipsize(&plain, width)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use unicode_width::UnicodeWidthStr;

    fn plain(lines: &[Line<'static>]) -> Vec<String> {
        lines
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect()
    }

    #[test]
    fn text_data_returns_none() {
        let data = OutputData::text("hello world");
        assert!(layout_output(&data, 80, &Palette::builtin()).is_none());
    }

    #[test]
    fn an_ls_shaped_list_fills_column_major_at_width_80() {
        let names: Vec<OutputNode> = (0..30).map(|i| OutputNode::new(format!("filename{i:02}"))).collect();
        let data = OutputData::nodes(names);
        let lines = layout_output(&data, 80, &Palette::builtin()).expect("a flat list has a shape");
        let text = plain(&lines);
        // "filename00".."filename29" are 10 columns wide + a 2-space gutter
        // = 12; 80 / 12 = 6 columns, so 30 names need 5 rows.
        assert_eq!(text.len(), 5, "got {text:?}");
        // Column-major fill: the first column is the first `num_rows` names,
        // top to bottom.
        assert!(text[0].starts_with("filename00"), "got {:?}", text[0]);
        assert!(text[1].starts_with("filename01"), "got {:?}", text[1]);
        assert!(text[2].starts_with("filename02"), "got {:?}", text[2]);
        assert!(text[3].starts_with("filename03"), "got {:?}", text[3]);
        assert!(text[4].starts_with("filename04"), "got {:?}", text[4]);
    }

    #[test]
    fn the_same_list_at_width_20_gives_more_rows() {
        let names: Vec<OutputNode> = (0..30).map(|i| OutputNode::new(format!("filename{i:02}"))).collect();
        let data = OutputData::nodes(names);
        let wide = layout_output(&data, 80, &Palette::builtin()).unwrap();
        let narrow = layout_output(&data, 20, &Palette::builtin()).unwrap();
        assert!(
            narrow.len() > wide.len(),
            "narrow ({}) should need more rows than wide ({})",
            narrow.len(),
            wide.len()
        );
    }

    #[test]
    fn a_table_aligns_a_two_row_body() {
        let data = OutputData::table(
            vec!["NAME".into(), "SIZE".into()],
            vec![
                OutputNode::new("a.txt").with_cells(vec!["10".into()]),
                OutputNode::new("bb.txt").with_cells(vec!["200".into()]),
            ],
        );
        let lines = layout_output(&data, 80, &Palette::builtin()).expect("a table has a shape");
        let text = plain(&lines);
        assert_eq!(text.len(), 3, "header + two rows, got {text:?}");
        // The widest NAME cell is "bb.txt" (6 wide); column 1 starts at
        // offset 6 + the 2-space gutter = 8 on every row, header included.
        assert!(text[0][8..].starts_with("SIZE"), "got {:?}", text[0]);
        assert!(text[1][8..].starts_with("10"), "got {:?}", text[1]);
        assert!(text[2][8..].starts_with("200"), "got {:?}", text[2]);
    }

    #[test]
    fn an_overlong_table_at_a_narrow_width_never_exceeds_it() {
        let data = OutputData::table(
            vec!["NAME".into(), "DESCRIPTION".into(), "OWNER".into()],
            vec![
                OutputNode::new("a-very-long-file-name.rs")
                    .with_cells(vec!["a fairly long description of the row".into(), "amy".into()]),
                OutputNode::new("b.rs").with_cells(vec!["short".into(), "claude".into()]),
            ],
        );
        for width in [10u16, 15, 20, 30] {
            let lines = layout_output(&data, width, &Palette::builtin()).unwrap();
            for line in &lines {
                let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
                assert!(
                    UnicodeWidthStr::width(text.as_str()) <= usize::from(width),
                    "width {width}: line {text:?} is wider than {width}"
                );
            }
        }
    }

    #[test]
    fn a_tree_indents_children() {
        let data = OutputData::nodes(vec![
            OutputNode::new("src").with_children(vec![
                OutputNode::new("main.rs"),
                OutputNode::new("lib").with_children(vec![OutputNode::new("util.rs")]),
            ]),
        ]);
        let lines = layout_output(&data, 80, &Palette::builtin()).expect("a tree has a shape");
        let text = plain(&lines);
        assert_eq!(text, vec!["src", "  main.rs", "  lib", "    util.rs"], "got {text:?}");
    }

    #[test]
    fn a_cjk_name_lays_out_by_display_width_not_bytes() {
        // "文件" is 2 chars / 6 bytes but 4 display columns; "a" is 1/1/1.
        let data = OutputData::nodes(vec![OutputNode::new("文件"), OutputNode::new("a")]);
        let lines = layout_output(&data, 80, &Palette::builtin()).expect("a flat list has a shape");
        assert_eq!(lines.len(), 1, "both names fit on one row at width 80");
        let text: String = lines[0].spans.iter().map(|s| s.content.as_ref()).collect();
        // Column width is display_width("文件") + 2 = 6, so "a" starts at
        // display column 6, not byte offset 6 (which would land mid-glyph).
        let a_display_col = UnicodeWidthStr::width(&text[..text.find('a').unwrap()]);
        assert_eq!(a_display_col, 6, "got {text:?}");
    }

    #[test]
    fn rich_json_only_pretty_prints() {
        let data = OutputData::new().with_rich_json(serde_json::json!({"a": 1}));
        let lines = layout_output(&data, 80, &Palette::builtin()).expect("rich_json has a shape");
        let text = plain(&lines).join("\n");
        assert!(text.contains("\"a\""), "got {text:?}");
        assert!(text.contains('1'), "got {text:?}");
    }
}
