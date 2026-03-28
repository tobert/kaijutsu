//! Resolve modalkit motion types to byte offsets in overlay text.
//!
//! Takes a modalkit `MoveType` + `Count` and the current cursor/text state,
//! returns the new cursor byte offset.

use editor_types::prelude::{Count, MoveDir1D, MovePosition, MoveType, WordStyle as MkWordStyle};
use modalkit::editing::context::EditContext;
use editor_types::context::Resolve;

use super::textutil::{self, WordStyle};

/// Context for motion resolution — carries state like desired column for j/k.
pub struct MotionContext {
    /// The column the cursor wants to be at (for j/k vertical movement).
    /// Set on horizontal movement, preserved on vertical movement.
    pub desired_col: Option<usize>,
}

/// Result of resolving a motion.
pub struct MotionResult {
    /// New cursor byte offset.
    pub cursor: usize,
    /// Updated desired_col (Some for vertical moves, None to reset).
    pub desired_col: Option<usize>,
}

/// Resolve a MoveType to a new cursor position.
pub fn resolve_motion(
    text: &str,
    cursor: usize,
    move_type: &MoveType,
    count: &Count,
    edit_ctx: &EditContext,
    motion_ctx: &MotionContext,
) -> MotionResult {
    let n: usize = edit_ctx.resolve(count);
    let n = n.max(1); // vim motions always move at least 1

    match move_type {
        // h — move left
        MoveType::Column(MoveDir1D::Previous, _wrap) => {
            let mut pos = cursor;
            for _ in 0..n {
                if pos == 0 {
                    break;
                }
                pos = textutil::prev_char_boundary(text, pos);
                // Don't cross line boundary in Normal mode (unless wrap=true)
                let line_start = textutil::line_start(text, textutil::line_of(text, pos));
                if pos < line_start {
                    pos = line_start;
                    break;
                }
            }
            MotionResult { cursor: pos, desired_col: None }
        }

        // l — move right
        MoveType::Column(MoveDir1D::Next, _wrap) => {
            let mut pos = cursor;
            for _ in 0..n {
                let line_end = textutil::line_end(text, textutil::line_of(text, pos));
                let next = textutil::next_char_boundary(text, pos);
                // In Normal mode, cursor can't go past last char on line
                if next >= line_end {
                    pos = if line_end > 0 {
                        textutil::prev_char_boundary(text, line_end).max(textutil::line_start(text, textutil::line_of(text, pos)))
                    } else {
                        0
                    };
                    break;
                }
                pos = next;
            }
            MotionResult { cursor: pos, desired_col: None }
        }

        // j — move down
        MoveType::Line(MoveDir1D::Next) => {
            let cur_line = textutil::line_of(text, cursor);
            let cur_col = motion_ctx
                .desired_col
                .unwrap_or_else(|| textutil::col_of(text, cursor));
            let total_lines = textutil::line_count(text);
            let target_line = (cur_line + n).min(total_lines.saturating_sub(1));
            let new_cursor = textutil::line_col_to_offset(text, target_line, cur_col);
            MotionResult {
                cursor: new_cursor,
                desired_col: Some(cur_col),
            }
        }

        // k — move up
        MoveType::Line(MoveDir1D::Previous) => {
            let cur_line = textutil::line_of(text, cursor);
            let cur_col = motion_ctx
                .desired_col
                .unwrap_or_else(|| textutil::col_of(text, cursor));
            let target_line = cur_line.saturating_sub(n);
            let new_cursor = textutil::line_col_to_offset(text, target_line, cur_col);
            MotionResult {
                cursor: new_cursor,
                desired_col: Some(cur_col),
            }
        }

        // 0 — beginning of line
        MoveType::LinePos(MovePosition::Beginning) => {
            let line = textutil::line_of(text, cursor);
            MotionResult {
                cursor: textutil::line_start(text, line),
                desired_col: None,
            }
        }

        // $ — end of line
        MoveType::LinePos(MovePosition::End) => {
            let line = textutil::line_of(text, cursor);
            let end = textutil::line_end(text, line);
            // In Normal mode, cursor is ON the last char, not past it
            let pos = if end > textutil::line_start(text, line) {
                textutil::prev_char_boundary(text, end)
            } else {
                textutil::line_start(text, line)
            };
            MotionResult { cursor: pos, desired_col: None }
        }

        // ^ — first non-blank on line
        MoveType::FirstWord(MoveDir1D::Next) | MoveType::FirstWord(MoveDir1D::Previous) => {
            let line = textutil::line_of(text, cursor);
            MotionResult {
                cursor: textutil::first_non_blank(text, line),
                desired_col: None,
            }
        }

        // w / W — word begin forward
        MoveType::WordBegin(style, MoveDir1D::Next) => {
            let ws = convert_word_style(style);
            MotionResult {
                cursor: textutil::word_begin_forward(text, cursor, ws, n),
                desired_col: None,
            }
        }

        // b / B — word begin backward
        MoveType::WordBegin(style, MoveDir1D::Previous) => {
            let ws = convert_word_style(style);
            MotionResult {
                cursor: textutil::word_begin_backward(text, cursor, ws, n),
                desired_col: None,
            }
        }

        // e / E — word end forward
        MoveType::WordEnd(style, MoveDir1D::Next) => {
            let ws = convert_word_style(style);
            MotionResult {
                cursor: textutil::word_end_forward(text, cursor, ws, n),
                desired_col: None,
            }
        }

        // gg — go to first line (or Nth line with count)
        MoveType::BufferPos(MovePosition::Beginning) => {
            let target_line = if edit_ctx.get_count().is_some() {
                // With explicit count: go to line N (1-indexed)
                n.saturating_sub(1)
            } else {
                0
            };
            MotionResult {
                cursor: textutil::first_non_blank(text, target_line),
                desired_col: None,
            }
        }

        // G — go to last line (or Nth line with count)
        MoveType::BufferPos(MovePosition::End) => {
            let total = textutil::line_count(text);
            let target_line = if edit_ctx.get_count().is_some() {
                // With explicit count: go to line N (1-indexed)
                n.saturating_sub(1).min(total.saturating_sub(1))
            } else {
                total.saturating_sub(1)
            };
            MotionResult {
                cursor: textutil::first_non_blank(text, target_line),
                desired_col: None,
            }
        }

        // BufferLineOffset — go to line N (used by gg with count)
        MoveType::BufferLineOffset => {
            let target_line = n.saturating_sub(1); // 1-indexed to 0-indexed
            MotionResult {
                cursor: textutil::first_non_blank(text, target_line),
                desired_col: None,
            }
        }

        // FinalNonBlank — g_ (end of line minus trailing whitespace)
        MoveType::FinalNonBlank(dir) => {
            let cur_line = textutil::line_of(text, cursor);
            let target_line = match dir {
                MoveDir1D::Next => (cur_line + n.saturating_sub(1)).min(textutil::line_count(text).saturating_sub(1)),
                MoveDir1D::Previous => cur_line.saturating_sub(n.saturating_sub(1)),
            };
            let start = textutil::line_start(text, target_line);
            let end = textutil::line_end(text, target_line);
            // Find last non-blank
            let mut pos = end;
            while pos > start {
                let prev = textutil::prev_char_boundary(text, pos);
                let ch = text[prev..].chars().next().unwrap_or(' ');
                if !ch.is_ascii_whitespace() {
                    pos = prev;
                    break;
                }
                pos = prev;
            }
            MotionResult { cursor: pos, desired_col: None }
        }

        // Anything else: no-op (log in dispatch)
        _ => {
            log::trace!("motion: unhandled MoveType: {:?}", move_type);
            MotionResult {
                cursor,
                desired_col: motion_ctx.desired_col,
            }
        }
    }
}

fn convert_word_style(style: &MkWordStyle) -> WordStyle {
    match style {
        MkWordStyle::Big => WordStyle::Big,
        // Little, AlphaNum, FileName, FilePath, etc. all use keyword-style boundaries
        _ => WordStyle::Little,
    }
}
