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

/// How a register's text puts: `p` puts charwise text inside a line,
/// linewise text on its own lines below the cursor, and blockwise text as a
/// column. Vim's register type; see `docs/vi.md`, "Registers".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RegisterShape {
    Charwise,
    Linewise,
    Blockwise,
}

/// The name of the unnamed register, `""`.
pub const UNNAMED_REGISTER: char = '"';

/// Whether `name` is a text register: the unnamed register `"`, `0`, the
/// delete ring `1`–`9`, the small delete `-`, or a named register `a`–`z`.
pub fn is_text_register(name: char) -> bool {
    matches!(name, '"' | '0'..='9' | '-' | 'a'..='z')
}

/// Which register an editor wrote, as the store's put sees it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RegisterWriteKind {
    /// The unnamed register as a yank: it and `"0` take the text.
    Yank,
    /// The unnamed register as a delete: it takes the text, and the delete
    /// ring takes it when it holds a line break, `"-` otherwise.
    Delete,
    /// A named register `a`–`z`: it and the unnamed register take the text.
    Named(char),
}

/// One write an editor made to its text registers. Another store applies it
/// with the same put, so two stores that apply the same writes in the same
/// order hold the same text registers. See `docs/vi.md`, "A kernel-owned
/// register store".
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegisterWrite {
    pub kind: RegisterWriteKind,
    pub text: String,
    pub shape: RegisterShape,
}

impl RegisterWrite {
    /// Refuse a write a store cannot apply: a named write to anything but
    /// `a`–`z`.
    pub fn check(&self) -> Result<(), String> {
        match self.kind {
            RegisterWriteKind::Named(c) if !c.is_ascii_lowercase() => Err(format!(
                "register write names {c:?}; a named register is one of a-z"
            )),
            _ => Ok(()),
        }
    }
}

/// One text register's content: its name (see [`is_text_register`]), text
/// and shape.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegisterText {
    pub name: char,
    pub text: String,
    pub shape: RegisterShape,
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
