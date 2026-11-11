//! Editor-session values shared by the vi engine, the kernel, the wire, and
//! the renderers. See `docs/vi.md`, "Selection rects".

use std::ops::Range;

use serde::{Deserialize, Serialize};

/// The vim visual-mode kind a selection came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SelectionShape {
    /// `v`: one run of text from the anchor to the cursor, both ends included.
    Charwise,
    /// `V`: whole lines from the anchor's line to the cursor's line.
    Linewise,
    /// `<C-v>`: the same column range on each line from the anchor's line to
    /// the cursor's line.
    Blockwise,
}

/// The active visual-mode selection, as the char ranges a renderer highlights.
///
/// Each span is a half-open range of char offsets into the editor text, in
/// document order. Charwise and linewise selections have one span; a
/// blockwise selection has one span per line that reaches the block's left
/// column. A span that covers a `\n` selects the line break, which a renderer
/// draws as one cell past the line's last char (vim marks a selected empty
/// line this way). Spans never extend past the end of the text.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EditorSelection {
    pub shape: SelectionShape,
    pub spans: Vec<Range<usize>>,
}

impl EditorSelection {
    /// Whether char offset `offset` falls inside any span.
    pub fn contains(&self, offset: usize) -> bool {
        self.spans.iter().any(|span| span.contains(&offset))
    }
}

/// The status-line label for the current file's place in an editor session's
/// argument list: `"(2 of 5)"` for the second of five files, from a 0-based
/// `index`. `None` when the list holds one file: a single file shows no
/// position. See `docs/vi.md`, "The argument list".
pub fn arg_position_label(index: usize, count: usize) -> Option<String> {
    (count > 1).then(|| format!("({} of {count})", index + 1))
}

#[cfg(test)]
#[allow(clippy::single_range_in_vec_init)] // a span list holding one `Range` is the intended value
mod tests {
    use super::*;

    #[test]
    fn contains_is_half_open_per_span() {
        let sel = EditorSelection {
            shape: SelectionShape::Blockwise,
            spans: vec![1..3, 6..8],
        };
        let inside: Vec<usize> = (0..10).filter(|&i| sel.contains(i)).collect();
        assert_eq!(inside, vec![1, 2, 6, 7]);
    }

    #[test]
    fn arg_position_label_counts_from_one_and_hides_a_single_file() {
        assert_eq!(arg_position_label(1, 5).as_deref(), Some("(2 of 5)"));
        assert_eq!(arg_position_label(0, 2).as_deref(), Some("(1 of 2)"));
        assert_eq!(arg_position_label(0, 1), None);
    }

    #[test]
    fn json_shape_is_lowercase_with_start_end_spans() {
        let sel = EditorSelection {
            shape: SelectionShape::Linewise,
            spans: vec![0..4],
        };
        assert_eq!(
            serde_json::to_value(&sel).unwrap(),
            serde_json::json!({"shape": "linewise", "spans": [{"start": 0, "end": 4}]})
        );
    }
}
