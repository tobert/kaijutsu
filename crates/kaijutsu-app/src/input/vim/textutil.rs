//! Pure text utility functions for vim motion resolution.
//!
//! All functions operate on `&str` with byte offsets. They handle UTF-8
//! correctly and never panic on valid byte offsets within the string.

/// Returns the 0-indexed line number containing the given byte offset.
pub fn line_of(text: &str, byte_offset: usize) -> usize {
    text[..byte_offset.min(text.len())]
        .bytes()
        .filter(|&b| b == b'\n')
        .count()
}

/// Returns the byte offset of the start of the given 0-indexed line.
/// Returns `text.len()` if `line` is past the last line.
pub fn line_start(text: &str, line: usize) -> usize {
    if line == 0 {
        return 0;
    }
    let mut count = 0;
    for (i, b) in text.bytes().enumerate() {
        if b == b'\n' {
            count += 1;
            if count == line {
                return i + 1;
            }
        }
    }
    text.len()
}

/// Returns the byte offset of the end of the given 0-indexed line (before the `\n`).
/// For the last line (no trailing `\n`), returns `text.len()`.
pub fn line_end(text: &str, line: usize) -> usize {
    let start = line_start(text, line);
    match text[start..].find('\n') {
        Some(i) => start + i,
        None => text.len(),
    }
}

/// Number of lines in the text (at least 1 for non-empty text, 0 for empty).
pub fn line_count(text: &str) -> usize {
    if text.is_empty() {
        return 0;
    }
    text.bytes().filter(|&b| b == b'\n').count() + 1
}

/// Byte offset of the first non-whitespace character on the given line.
/// If the line is all whitespace, returns the line end.
pub fn first_non_blank(text: &str, line: usize) -> usize {
    let start = line_start(text, line);
    let end = line_end(text, line);
    for (i, ch) in text[start..end].char_indices() {
        if !ch.is_ascii_whitespace() {
            return start + i;
        }
    }
    end
}

/// Column (0-indexed, in characters) of a byte offset within its line.
pub fn col_of(text: &str, byte_offset: usize) -> usize {
    let line = line_of(text, byte_offset);
    let start = line_start(text, line);
    text[start..byte_offset.min(text.len())].chars().count()
}

/// Byte offset for a given (line, col) pair. Col is in characters.
/// Clamps to the end of the line if col exceeds line length.
pub fn line_col_to_offset(text: &str, line: usize, col: usize) -> usize {
    let start = line_start(text, line);
    let end = line_end(text, line);
    let mut offset = start;
    for (i, (idx, _)) in text[start..end].char_indices().enumerate() {
        if i == col {
            return start + idx;
        }
        offset = start + idx;
    }
    // col exceeds line length — clamp to last char position
    if col > 0 && start < end {
        // Return offset of last char (not past it — vim Normal mode)
        offset
    } else {
        start
    }
}

/// Previous UTF-8 character boundary before `byte_offset`.
/// Returns 0 if already at start.
pub fn prev_char_boundary(text: &str, byte_offset: usize) -> usize {
    if byte_offset == 0 {
        return 0;
    }
    let mut i = byte_offset - 1;
    while i > 0 && !text.is_char_boundary(i) {
        i -= 1;
    }
    i
}

/// Next UTF-8 character boundary after `byte_offset`.
/// Returns `text.len()` if already at end.
pub fn next_char_boundary(text: &str, byte_offset: usize) -> usize {
    if byte_offset >= text.len() {
        return text.len();
    }
    let mut i = byte_offset + 1;
    while i < text.len() && !text.is_char_boundary(i) {
        i += 1;
    }
    i
}

// ---------------------------------------------------------------------------
// Word motion helpers
// ---------------------------------------------------------------------------

/// Word style for motion resolution.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WordStyle {
    /// Vim `w`/`b`/`e` — keyword chars vs non-blank non-keyword.
    Little,
    /// Vim `W`/`B`/`E` — non-blank sequences.
    Big,
}

/// Character class for word boundary detection.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CharClass {
    Whitespace,
    Keyword,
    Punctuation,
}

fn classify(ch: char, style: WordStyle) -> CharClass {
    if ch.is_ascii_whitespace() {
        CharClass::Whitespace
    } else if style == WordStyle::Big {
        // Big word: everything non-whitespace is one class
        CharClass::Keyword
    } else if ch.is_alphanumeric() || ch == '_' {
        CharClass::Keyword
    } else {
        CharClass::Punctuation
    }
}

/// Move forward to the start of the next word (`w` / `W`).
pub fn word_begin_forward(text: &str, offset: usize, style: WordStyle, count: usize) -> usize {
    let mut pos = offset;
    for _ in 0..count {
        if pos >= text.len() {
            break;
        }
        let start_class = classify(text[pos..].chars().next().unwrap_or(' '), style);

        // Skip current word (same class)
        while pos < text.len() {
            let ch = text[pos..].chars().next().unwrap_or(' ');
            if classify(ch, style) != start_class {
                break;
            }
            pos = next_char_boundary(text, pos);
        }

        // Skip whitespace to reach next word
        while pos < text.len() {
            let ch = text[pos..].chars().next().unwrap_or(' ');
            if !ch.is_ascii_whitespace() {
                break;
            }
            pos = next_char_boundary(text, pos);
        }
    }
    pos.min(text.len())
}

/// Move backward to the start of the previous word (`b` / `B`).
pub fn word_begin_backward(text: &str, offset: usize, style: WordStyle, count: usize) -> usize {
    let mut pos = offset;
    for _ in 0..count {
        if pos == 0 {
            break;
        }

        // Move back one char to get off current position
        pos = prev_char_boundary(text, pos);

        // Skip whitespace backwards
        while pos > 0 {
            let ch = text[pos..].chars().next().unwrap_or(' ');
            if !ch.is_ascii_whitespace() {
                break;
            }
            pos = prev_char_boundary(text, pos);
        }

        // Now on a non-whitespace char — find start of this word
        let word_class = classify(text[pos..].chars().next().unwrap_or(' '), style);
        while pos > 0 {
            let prev = prev_char_boundary(text, pos);
            let ch = text[prev..].chars().next().unwrap_or(' ');
            if classify(ch, style) != word_class {
                break;
            }
            pos = prev;
        }
    }
    pos
}

/// Move forward to the end of the current/next word (`e` / `E`).
pub fn word_end_forward(text: &str, offset: usize, style: WordStyle, count: usize) -> usize {
    let mut pos = offset;
    for _ in 0..count {
        if pos >= text.len() {
            break;
        }

        // Move forward one char to get off current position
        pos = next_char_boundary(text, pos);

        // Skip whitespace
        while pos < text.len() {
            let ch = text[pos..].chars().next().unwrap_or(' ');
            if !ch.is_ascii_whitespace() {
                break;
            }
            pos = next_char_boundary(text, pos);
        }

        if pos >= text.len() {
            break;
        }

        // Now on a word char — move to end of this word
        let word_class = classify(text[pos..].chars().next().unwrap_or(' '), style);
        while pos < text.len() {
            let next = next_char_boundary(text, pos);
            if next >= text.len() {
                break;
            }
            let ch = text[next..].chars().next().unwrap_or(' ');
            if classify(ch, style) != word_class {
                break;
            }
            pos = next;
        }
    }
    pos.min(if text.is_empty() { 0 } else { prev_char_boundary(text, text.len()).max(offset) })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_line_of() {
        let text = "hello\nworld\nfoo";
        assert_eq!(line_of(text, 0), 0); // 'h'
        assert_eq!(line_of(text, 5), 0); // '\n'
        assert_eq!(line_of(text, 6), 1); // 'w'
        assert_eq!(line_of(text, 12), 2); // 'f'
    }

    #[test]
    fn test_line_start_end() {
        let text = "hello\nworld\nfoo";
        assert_eq!(line_start(text, 0), 0);
        assert_eq!(line_start(text, 1), 6);
        assert_eq!(line_start(text, 2), 12);
        assert_eq!(line_end(text, 0), 5);
        assert_eq!(line_end(text, 1), 11);
        assert_eq!(line_end(text, 2), 15); // no trailing \n
    }

    #[test]
    fn test_line_count() {
        assert_eq!(line_count(""), 0);
        assert_eq!(line_count("hello"), 1);
        assert_eq!(line_count("hello\nworld"), 2);
        assert_eq!(line_count("a\nb\nc\n"), 4); // trailing newline = extra line
    }

    #[test]
    fn test_first_non_blank() {
        let text = "  hello\n\tworld";
        assert_eq!(first_non_blank(text, 0), 2); // "  hello" → 'h' at 2
        assert_eq!(first_non_blank(text, 1), 9); // "\tworld" → 'w' at 9
    }

    #[test]
    fn test_col_of() {
        let text = "hello\nworld";
        assert_eq!(col_of(text, 0), 0); // 'h'
        assert_eq!(col_of(text, 3), 3); // 'l'
        assert_eq!(col_of(text, 6), 0); // 'w' (line 1, col 0)
        assert_eq!(col_of(text, 8), 2); // 'r' (line 1, col 2)
    }

    #[test]
    fn test_line_col_to_offset() {
        let text = "hello\nworld";
        assert_eq!(line_col_to_offset(text, 0, 0), 0);
        assert_eq!(line_col_to_offset(text, 0, 3), 3);
        assert_eq!(line_col_to_offset(text, 1, 0), 6);
        assert_eq!(line_col_to_offset(text, 1, 2), 8);
    }

    #[test]
    fn test_word_begin_forward() {
        let text = "hello world  foo";
        assert_eq!(word_begin_forward(text, 0, WordStyle::Little, 1), 6); // → 'w'
        assert_eq!(word_begin_forward(text, 6, WordStyle::Little, 1), 13); // → 'f'
        assert_eq!(word_begin_forward(text, 0, WordStyle::Little, 2), 13); // → 'f'
    }

    #[test]
    fn test_word_begin_forward_punctuation() {
        let text = "foo.bar baz";
        // Little: foo → . → bar → baz
        assert_eq!(word_begin_forward(text, 0, WordStyle::Little, 1), 3); // → '.'
        assert_eq!(word_begin_forward(text, 3, WordStyle::Little, 1), 4); // → 'b'
        // Big: foo.bar → baz
        assert_eq!(word_begin_forward(text, 0, WordStyle::Big, 1), 8); // → 'b'
    }

    #[test]
    fn test_word_begin_backward() {
        let text = "hello world foo";
        assert_eq!(word_begin_backward(text, 15, WordStyle::Little, 1), 12); // → 'f'
        assert_eq!(word_begin_backward(text, 12, WordStyle::Little, 1), 6); // → 'w'
        assert_eq!(word_begin_backward(text, 6, WordStyle::Little, 1), 0); // → 'h'
    }

    #[test]
    fn test_word_end_forward() {
        let text = "hello world foo";
        assert_eq!(word_end_forward(text, 0, WordStyle::Little, 1), 4); // → 'o' in hello
        assert_eq!(word_end_forward(text, 4, WordStyle::Little, 1), 10); // → 'd' in world
    }

    #[test]
    fn test_utf8_chars() {
        let text = "café";
        assert_eq!(next_char_boundary(text, 0), 1); // c → a
        assert_eq!(next_char_boundary(text, 3), 5); // é is 2 bytes
        assert_eq!(prev_char_boundary(text, 5), 3); // back over é
    }

    #[test]
    fn test_empty_text() {
        assert_eq!(line_of("", 0), 0);
        assert_eq!(line_start("", 0), 0);
        assert_eq!(line_end("", 0), 0);
        assert_eq!(first_non_blank("", 0), 0);
        assert_eq!(word_begin_forward("", 0, WordStyle::Little, 1), 0);
        assert_eq!(word_begin_backward("", 0, WordStyle::Little, 1), 0);
    }
}
