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
            // Compute line start once — h never crosses line boundaries.
            let boundary = textutil::line_start(text, textutil::line_of(text, cursor));
            for _ in 0..n {
                if pos <= boundary {
                    break;
                }
                let prev = textutil::prev_char_boundary(text, pos);
                if prev < boundary {
                    pos = boundary;
                    break;
                }
                pos = prev;
            }
            MotionResult { cursor: pos, desired_col: None }
        }

        // l — move right (stops at line boundary)
        //
        // Returns line_end when at the last char — the caller's
        // clamp_normal_cursor will pull it back in Normal mode, but
        // Insert-mode transitions (e.g. 'a') need it at line_end.
        MoveType::Column(MoveDir1D::Next, _wrap) => {
            let mut pos = cursor;
            for _ in 0..n {
                let line_end = textutil::line_end(text, textutil::line_of(text, pos));
                let next = textutil::next_char_boundary(text, pos);
                if next > line_end {
                    // Already at or past line boundary — don't cross
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

#[cfg(test)]
mod tests {
    use super::*;
    use editor_types::context::EditContextBuilder;
    use editor_types::prelude::{Count, MoveDir1D, MovePosition, MoveType};

    fn ctx() -> EditContext {
        EditContextBuilder::default().build()
    }

    fn ctx_with_count(n: usize) -> EditContext {
        EditContextBuilder::default().count(Some(n)).build()
    }

    fn motion_ctx() -> MotionContext {
        MotionContext { desired_col: None }
    }

    fn motion_ctx_col(col: usize) -> MotionContext {
        MotionContext { desired_col: Some(col) }
    }

    fn resolve(text: &str, cursor: usize, mt: &MoveType, c: &Count, ec: &EditContext, mc: &MotionContext) -> MotionResult {
        resolve_motion(text, cursor, mt, c, ec, mc)
    }

    // ── h (Column Previous) ──

    #[test]
    fn h_mid_line() {
        let text = "hello";
        let mt = MoveType::Column(MoveDir1D::Previous, false);
        let r = resolve(text, 3, &mt, &Count::Contextual, &ctx(), &motion_ctx());
        assert_eq!(r.cursor, 2);
        assert!(r.desired_col.is_none());
    }

    #[test]
    fn h_at_start() {
        let mt = MoveType::Column(MoveDir1D::Previous, false);
        let r = resolve("hello", 0, &mt, &Count::Contextual, &ctx(), &motion_ctx());
        assert_eq!(r.cursor, 0);
    }

    #[test]
    fn h_with_count() {
        let mt = MoveType::Column(MoveDir1D::Previous, false);
        let r = resolve("hello", 4, &mt, &Count::Exact(3), &ctx(), &motion_ctx());
        assert_eq!(r.cursor, 1);
    }

    #[test]
    fn h_clamps_to_line_start() {
        let text = "ab\ncd";
        let mt = MoveType::Column(MoveDir1D::Previous, false);
        // cursor at 'c' (byte 3), h should stay at line start (byte 3)
        let r = resolve(text, 3, &mt, &Count::Contextual, &ctx(), &motion_ctx());
        assert_eq!(r.cursor, 3);
    }

    #[test]
    fn h_utf8() {
        let text = "café"; // é is 2 bytes
        let mt = MoveType::Column(MoveDir1D::Previous, false);
        // cursor at é (byte 3), h should go to 'f' (byte 2)
        // Wait — "café": c=0, a=1, f=2, é=3..4
        let r = resolve(text, 3, &mt, &Count::Contextual, &ctx(), &motion_ctx());
        assert_eq!(r.cursor, 2);
    }

    // ── l (Column Next) ──

    #[test]
    fn l_mid_line() {
        let mt = MoveType::Column(MoveDir1D::Next, false);
        let r = resolve("hello", 1, &mt, &Count::Contextual, &ctx(), &motion_ctx());
        assert_eq!(r.cursor, 2);
    }

    #[test]
    fn l_at_last_char() {
        let mt = MoveType::Column(MoveDir1D::Next, false);
        // cursor on 'o' (byte 4), l returns line_end (5) — the dispatch
        // layer's clamp_normal_cursor pulls it back in Normal mode,
        // but 'a' (append) needs it at line_end for Insert positioning.
        let r = resolve("hello", 4, &mt, &Count::Contextual, &ctx(), &motion_ctx());
        assert_eq!(r.cursor, 5); // line_end
    }

    #[test]
    fn l_stops_at_line_end() {
        let text = "ab\ncd";
        let mt = MoveType::Column(MoveDir1D::Next, false);
        // cursor on 'b' (byte 1), l moves to line_end (byte 2, the \n)
        // clamp_normal_cursor handles pulling back to 'b' in Normal mode
        let r = resolve(text, 1, &mt, &Count::Contextual, &ctx(), &motion_ctx());
        assert_eq!(r.cursor, 2); // line_end (the \n position)
    }

    #[test]
    fn l_does_not_cross_newline() {
        let text = "ab\ncd";
        let mt = MoveType::Column(MoveDir1D::Next, false);
        // cursor at '\n' (byte 2), l should not cross into next line
        let r = resolve(text, 2, &mt, &Count::Contextual, &ctx(), &motion_ctx());
        assert_eq!(r.cursor, 2); // stays at line_end
    }

    // ── j (Line Next) ──

    #[test]
    fn j_basic() {
        let text = "hello\nworld";
        let mt = MoveType::Line(MoveDir1D::Next);
        let r = resolve(text, 2, &mt, &Count::Contextual, &ctx(), &motion_ctx());
        // Should be on line 1, col 2 → byte 8 ('r')
        assert_eq!(r.cursor, 8);
        assert_eq!(r.desired_col, Some(2));
    }

    #[test]
    fn j_short_line_clamps() {
        let text = "hello\nab";
        let mt = MoveType::Line(MoveDir1D::Next);
        // cursor at col 4 on "hello", j to "ab" (2 chars) should clamp
        let r = resolve(text, 4, &mt, &Count::Contextual, &ctx(), &motion_ctx());
        // col 4 doesn't exist on "ab" (cols 0,1), should land at end
        assert!(r.cursor >= 6 && r.cursor <= 7); // byte 6='a', 7='b'
        assert_eq!(r.desired_col, Some(4)); // preserves desired col
    }

    #[test]
    fn j_single_line() {
        let text = "hello";
        let mt = MoveType::Line(MoveDir1D::Next);
        let r = resolve(text, 2, &mt, &Count::Contextual, &ctx(), &motion_ctx());
        // No line below — should stay on same line
        assert!(r.cursor <= text.len());
    }

    #[test]
    fn j_preserves_desired_col() {
        let text = "hello\nab\nworld";
        let mt = MoveType::Line(MoveDir1D::Next);
        // First j from col 4 on "hello" → "ab" (clamps to col 1)
        let r1 = resolve(text, 4, &mt, &Count::Contextual, &ctx(), &motion_ctx());
        assert_eq!(r1.desired_col, Some(4));
        // Second j from "ab" with desired_col=4 → "world" (restores col 4)
        let mc = motion_ctx_col(4);
        let r2 = resolve(text, r1.cursor, &mt, &Count::Contextual, &ctx(), &mc);
        assert_eq!(r2.cursor, 13); // byte 9='w', 13='d' (col 4)
        assert_eq!(r2.desired_col, Some(4));
    }

    // ── k (Line Previous) ──

    #[test]
    fn k_basic() {
        let text = "hello\nworld";
        let mt = MoveType::Line(MoveDir1D::Previous);
        // cursor on 'r' (byte 8, line 1 col 2), k should go to line 0 col 2
        let r = resolve(text, 8, &mt, &Count::Contextual, &ctx(), &motion_ctx());
        assert_eq!(r.cursor, 2);
    }

    #[test]
    fn k_at_first_line() {
        let mt = MoveType::Line(MoveDir1D::Previous);
        let r = resolve("hello", 2, &mt, &Count::Contextual, &ctx(), &motion_ctx());
        assert_eq!(r.cursor, 2); // stays on same position
    }

    // ── 0 (Beginning of line) ──

    #[test]
    fn zero_mid_line() {
        let text = "  hello";
        let mt = MoveType::LinePos(MovePosition::Beginning);
        let r = resolve(text, 4, &mt, &Count::Contextual, &ctx(), &motion_ctx());
        assert_eq!(r.cursor, 0);
    }

    #[test]
    fn zero_second_line() {
        let text = "hello\n  world";
        let mt = MoveType::LinePos(MovePosition::Beginning);
        let r = resolve(text, 10, &mt, &Count::Contextual, &ctx(), &motion_ctx());
        assert_eq!(r.cursor, 6); // start of "  world" line
    }

    // ── $ (End of line) ──

    #[test]
    fn dollar_mid_line() {
        let text = "hello\nworld";
        let mt = MoveType::LinePos(MovePosition::End);
        let r = resolve(text, 1, &mt, &Count::Contextual, &ctx(), &motion_ctx());
        // End of "hello" — ON last char 'o' (byte 4)
        assert_eq!(r.cursor, 4);
    }

    #[test]
    fn dollar_empty_line() {
        let text = "hello\n\nworld";
        let mt = MoveType::LinePos(MovePosition::End);
        // cursor on empty line (byte 6, the second \n)
        let r = resolve(text, 6, &mt, &Count::Contextual, &ctx(), &motion_ctx());
        assert_eq!(r.cursor, 6); // empty line start = line end
    }

    // ── ^ (First non-blank) ──

    #[test]
    fn caret_skips_whitespace() {
        let text = "   hello";
        let mt = MoveType::FirstWord(MoveDir1D::Next);
        let r = resolve(text, 0, &mt, &Count::Contextual, &ctx(), &motion_ctx());
        assert_eq!(r.cursor, 3); // 'h'
    }

    #[test]
    fn caret_no_whitespace() {
        let text = "hello";
        let mt = MoveType::FirstWord(MoveDir1D::Next);
        let r = resolve(text, 3, &mt, &Count::Contextual, &ctx(), &motion_ctx());
        assert_eq!(r.cursor, 0);
    }

    // ── w (Word begin forward) ──

    #[test]
    fn w_basic() {
        let text = "hello world";
        let mt = MoveType::WordBegin(MkWordStyle::Little, MoveDir1D::Next);
        let r = resolve(text, 0, &mt, &Count::Contextual, &ctx(), &motion_ctx());
        assert_eq!(r.cursor, 6); // 'w'
    }

    #[test]
    fn w_big() {
        let text = "foo.bar baz";
        let mt = MoveType::WordBegin(MkWordStyle::Big, MoveDir1D::Next);
        let r = resolve(text, 0, &mt, &Count::Contextual, &ctx(), &motion_ctx());
        assert_eq!(r.cursor, 8); // 'b' in "baz"
    }

    // ── b (Word begin backward) ──

    #[test]
    fn b_basic() {
        let text = "hello world";
        let mt = MoveType::WordBegin(MkWordStyle::Little, MoveDir1D::Previous);
        let r = resolve(text, 8, &mt, &Count::Contextual, &ctx(), &motion_ctx());
        assert_eq!(r.cursor, 6); // 'w'
    }

    // ── e (Word end forward) ──

    #[test]
    fn e_basic() {
        let text = "hello world";
        let mt = MoveType::WordEnd(MkWordStyle::Little, MoveDir1D::Next);
        let r = resolve(text, 0, &mt, &Count::Contextual, &ctx(), &motion_ctx());
        assert_eq!(r.cursor, 4); // 'o' in "hello"
    }

    // ── gg (Buffer beginning) ──

    #[test]
    fn gg_no_count() {
        let text = "hello\nworld\nfoo";
        let mt = MoveType::BufferPos(MovePosition::Beginning);
        let r = resolve(text, 8, &mt, &Count::Contextual, &ctx(), &motion_ctx());
        assert_eq!(r.cursor, 0); // first non-blank on line 0
    }

    #[test]
    fn gg_with_count() {
        let text = "hello\nworld\nfoo";
        let mt = MoveType::BufferPos(MovePosition::Beginning);
        let ec = ctx_with_count(2);
        let r = resolve(text, 0, &mt, &Count::Contextual, &ec, &motion_ctx());
        // Line 2 (1-indexed) = line 1 (0-indexed) = "world", first non-blank = byte 6
        assert_eq!(r.cursor, 6);
    }

    // ── G (Buffer end) ──

    #[test]
    fn g_big_no_count() {
        let text = "hello\nworld\nfoo";
        let mt = MoveType::BufferPos(MovePosition::End);
        let r = resolve(text, 0, &mt, &Count::Contextual, &ctx(), &motion_ctx());
        // last line "foo", first non-blank = byte 12
        assert_eq!(r.cursor, 12);
    }

    #[test]
    fn g_big_with_count() {
        let text = "hello\nworld\nfoo";
        let mt = MoveType::BufferPos(MovePosition::End);
        let ec = ctx_with_count(1);
        let r = resolve(text, 8, &mt, &Count::Contextual, &ec, &motion_ctx());
        // 1G = go to line 1 (first line), first non-blank
        assert_eq!(r.cursor, 0);
    }

    // ── g_ (FinalNonBlank) ──

    #[test]
    fn g_underscore_trailing_whitespace() {
        let text = "hello   ";
        let mt = MoveType::FinalNonBlank(MoveDir1D::Next);
        let r = resolve(text, 0, &mt, &Count::Contextual, &ctx(), &motion_ctx());
        assert_eq!(r.cursor, 4); // 'o'
    }

    #[test]
    fn g_underscore_no_trailing() {
        let text = "hello";
        let mt = MoveType::FinalNonBlank(MoveDir1D::Next);
        let r = resolve(text, 0, &mt, &Count::Contextual, &ctx(), &motion_ctx());
        assert_eq!(r.cursor, 4); // 'o'
    }

    // ── Empty text ──

    #[test]
    fn empty_text_h() {
        let mt = MoveType::Column(MoveDir1D::Previous, false);
        let r = resolve("", 0, &mt, &Count::Contextual, &ctx(), &motion_ctx());
        assert_eq!(r.cursor, 0);
    }

    #[test]
    fn empty_text_l() {
        let mt = MoveType::Column(MoveDir1D::Next, false);
        let r = resolve("", 0, &mt, &Count::Contextual, &ctx(), &motion_ctx());
        assert_eq!(r.cursor, 0);
    }

    #[test]
    fn empty_text_j() {
        let mt = MoveType::Line(MoveDir1D::Next);
        let r = resolve("", 0, &mt, &Count::Contextual, &ctx(), &motion_ctx());
        assert_eq!(r.cursor, 0);
    }

    #[test]
    fn empty_text_w() {
        let mt = MoveType::WordBegin(MkWordStyle::Little, MoveDir1D::Next);
        let r = resolve("", 0, &mt, &Count::Contextual, &ctx(), &motion_ctx());
        assert_eq!(r.cursor, 0);
    }

    #[test]
    fn empty_text_gg() {
        let mt = MoveType::BufferPos(MovePosition::Beginning);
        let r = resolve("", 0, &mt, &Count::Contextual, &ctx(), &motion_ctx());
        assert_eq!(r.cursor, 0);
    }

    #[test]
    fn empty_text_dollar() {
        let mt = MoveType::LinePos(MovePosition::End);
        let r = resolve("", 0, &mt, &Count::Contextual, &ctx(), &motion_ctx());
        assert_eq!(r.cursor, 0);
    }

    // ── Unhandled motion ──

    #[test]
    fn unhandled_returns_cursor() {
        let mt = MoveType::WordEnd(MkWordStyle::Little, MoveDir1D::Previous);
        let r = resolve("hello", 2, &mt, &Count::Contextual, &ctx(), &motion_ctx());
        assert_eq!(r.cursor, 2); // unchanged
    }
}
