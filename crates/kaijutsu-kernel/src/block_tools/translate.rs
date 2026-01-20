//! Translation layer between line-based operations and byte offsets.
//!
//! This module bridges the model-friendly line-based interface with
//! the CRDT's character-based operations.

use super::error::{EditError, Result};

/// Convert a 0-indexed line number to a byte offset in the content.
///
/// # Arguments
///
/// * `content` - The text content
/// * `line` - 0-indexed line number
///
/// # Returns
///
/// The byte offset at the start of the specified line.
///
/// # Example
///
/// ```ignore
/// let content = "hello\nworld\n";
/// assert_eq!(line_to_byte_offset(content, 0), Ok(0));
/// assert_eq!(line_to_byte_offset(content, 1), Ok(6));
/// assert_eq!(line_to_byte_offset(content, 2), Ok(12)); // After final newline
/// ```
pub fn line_to_byte_offset(content: &str, line: u32) -> Result<usize> {
    if line == 0 {
        return Ok(0);
    }

    let mut offset = 0;
    let mut current_line = 0;

    for ch in content.chars() {
        if ch == '\n' {
            current_line += 1;
            offset += ch.len_utf8();
            if current_line == line {
                return Ok(offset);
            }
        } else {
            offset += ch.len_utf8();
        }
    }

    // Allow inserting at end (one past the last line)
    let line_count = content.lines().count() as u32;
    let has_trailing_newline = content.ends_with('\n');

    // Calculate the actual number of "line positions" available
    let max_line = if has_trailing_newline || content.is_empty() {
        line_count
    } else {
        line_count // Can insert at the start of a virtual line after content
    };

    if line == max_line {
        Ok(content.len())
    } else if line > max_line {
        Err(EditError::line_out_of_range(line, max_line))
    } else {
        // This shouldn't happen, but handle it gracefully
        Ok(content.len())
    }
}

/// Get the byte offset at the end of a line (start of next line or EOF).
///
/// # Arguments
///
/// * `content` - The text content
/// * `line` - 0-indexed line number
///
/// # Returns
///
/// The byte offset at the end of the specified line (after the newline, or EOF).
pub fn line_end_byte_offset(content: &str, line: u32) -> Result<usize> {
    let lines: Vec<&str> = content.lines().collect();
    let line_count = lines.len() as u32;

    if line >= line_count {
        return Ok(content.len());
    }

    // Get the start of line + 1
    line_to_byte_offset(content, line + 1)
}

/// Get the byte range for a line range (exclusive end).
///
/// # Arguments
///
/// * `content` - The text content
/// * `start_line` - Start line (0-indexed, inclusive)
/// * `end_line` - End line (0-indexed, exclusive)
///
/// # Returns
///
/// A tuple of (start_offset, end_offset) in bytes.
pub fn line_range_to_byte_range(
    content: &str,
    start_line: u32,
    end_line: u32,
) -> Result<(usize, usize)> {
    if end_line < start_line {
        return Err(EditError::InvalidParams(format!(
            "end_line ({}) must be >= start_line ({})",
            end_line, start_line
        )));
    }

    let start = line_to_byte_offset(content, start_line)?;
    let end = line_to_byte_offset(content, end_line)?;

    Ok((start, end))
}

/// Validate that the expected text matches the actual content at the given line range.
///
/// This implements compare-and-set (CAS) semantics for safe editing.
///
/// # Arguments
///
/// * `content` - The full text content
/// * `start_line` - Start line (0-indexed)
/// * `end_line` - End line (exclusive)
/// * `expected` - The expected text in this range
///
/// # Returns
///
/// `Ok(())` if the text matches, `Err(ContentMismatch)` otherwise.
pub fn validate_expected_text(
    content: &str,
    start_line: u32,
    end_line: u32,
    expected: &str,
) -> Result<()> {
    // Get the actual text in the range
    let lines: Vec<&str> = content.lines().collect();
    let line_count = lines.len() as u32;

    // Validate line range
    if start_line > line_count {
        return Err(EditError::line_out_of_range(start_line, line_count));
    }

    // Extract the lines in range
    let actual: String = lines
        .iter()
        .skip(start_line as usize)
        .take((end_line.saturating_sub(start_line)) as usize)
        .cloned()
        .collect::<Vec<_>>()
        .join("\n");

    // Normalize expected text (remove trailing newline for comparison)
    let expected_normalized = expected.trim_end_matches('\n');
    let actual_normalized = actual.trim_end_matches('\n');

    if actual_normalized == expected_normalized {
        Ok(())
    } else {
        Err(EditError::content_mismatch(
            expected_normalized,
            actual_normalized,
            start_line,
            end_line,
        ))
    }
}

/// Count the number of lines in content.
pub fn line_count(content: &str) -> u32 {
    if content.is_empty() {
        0
    } else {
        content.lines().count() as u32
    }
}

/// Extract lines with 1-indexed line numbers for display.
///
/// Returns content formatted like:
/// ```text
///      1→ first line
///      2→ second line
/// ```
pub fn content_with_line_numbers(content: &str) -> String {
    let lines: Vec<&str> = content.lines().collect();
    let width = lines.len().to_string().len().max(4);

    lines
        .iter()
        .enumerate()
        .map(|(i, line)| format!("{:>width$}→ {}", i + 1, line, width = width))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Extract a range of lines with 1-indexed line numbers.
///
/// # Arguments
///
/// * `content` - The text content
/// * `start` - Start line (0-indexed)
/// * `end` - End line (0-indexed, exclusive)
pub fn extract_lines_with_numbers(content: &str, start: u32, end: u32) -> String {
    let lines: Vec<&str> = content.lines().collect();
    let width = lines.len().to_string().len().max(4);

    lines
        .iter()
        .enumerate()
        .skip(start as usize)
        .take((end.saturating_sub(start)) as usize)
        .map(|(i, line)| format!("{:>width$}→ {}", i + 1, line, width = width))
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_line_to_byte_offset_empty() {
        assert_eq!(line_to_byte_offset("", 0), Ok(0));
    }

    #[test]
    fn test_line_to_byte_offset_single_line() {
        let content = "hello";
        assert_eq!(line_to_byte_offset(content, 0), Ok(0));
        // Line 1 would be at the end
        assert_eq!(line_to_byte_offset(content, 1), Ok(5));
    }

    #[test]
    fn test_line_to_byte_offset_multiple_lines() {
        let content = "hello\nworld\n";
        assert_eq!(line_to_byte_offset(content, 0), Ok(0));  // Start of "hello"
        assert_eq!(line_to_byte_offset(content, 1), Ok(6));  // Start of "world"
        assert_eq!(line_to_byte_offset(content, 2), Ok(12)); // After final newline
    }

    #[test]
    fn test_line_to_byte_offset_utf8() {
        let content = "héllo\nwörld\n";
        assert_eq!(line_to_byte_offset(content, 0), Ok(0));
        // "héllo\n" = h(1) + é(2) + l(1) + l(1) + o(1) + \n(1) = 7 bytes
        assert_eq!(line_to_byte_offset(content, 1), Ok(7));
    }

    #[test]
    fn test_line_to_byte_offset_out_of_range() {
        let content = "one\ntwo\n";
        let result = line_to_byte_offset(content, 10);
        assert!(result.is_err());
    }

    #[test]
    fn test_line_range_to_byte_range() {
        let content = "line1\nline2\nline3\n";

        // Range for line 1 only
        let (start, end) = line_range_to_byte_range(content, 1, 2).unwrap();
        assert_eq!(start, 6);  // Start of line2
        assert_eq!(end, 12);   // Start of line3
        assert_eq!(&content[start..end], "line2\n");
    }

    #[test]
    fn test_validate_expected_text_match() {
        let content = "line1\nline2\nline3\n";

        // Check line 1 (0-indexed)
        let result = validate_expected_text(content, 1, 2, "line2");
        assert!(result.is_ok());
    }

    #[test]
    fn test_validate_expected_text_mismatch() {
        let content = "line1\nline2\nline3\n";

        let result = validate_expected_text(content, 1, 2, "wrong");
        assert!(result.is_err());

        if let Err(EditError::ContentMismatch { expected, actual, .. }) = result {
            assert_eq!(expected, "wrong");
            assert_eq!(actual, "line2");
        } else {
            panic!("Expected ContentMismatch error");
        }
    }

    #[test]
    fn test_validate_expected_text_multiline() {
        let content = "one\ntwo\nthree\nfour\n";

        // Check lines 1-3 (0-indexed)
        let result = validate_expected_text(content, 1, 3, "two\nthree");
        assert!(result.is_ok());
    }

    #[test]
    fn test_line_count() {
        assert_eq!(line_count(""), 0);
        assert_eq!(line_count("hello"), 1);
        assert_eq!(line_count("hello\n"), 1);
        assert_eq!(line_count("hello\nworld"), 2);
        assert_eq!(line_count("hello\nworld\n"), 2);
    }

    #[test]
    fn test_content_with_line_numbers() {
        let content = "fn main() {\n    println!(\"Hi\");\n}";
        let numbered = content_with_line_numbers(content);
        assert!(numbered.contains("   1→ fn main() {"));
        assert!(numbered.contains("   2→     println!(\"Hi\");"));
        assert!(numbered.contains("   3→ }"));
    }

    #[test]
    fn test_extract_lines_with_numbers() {
        let content = "a\nb\nc\nd\ne\n";
        let extracted = extract_lines_with_numbers(content, 1, 3);
        assert!(extracted.contains("2→ b"));
        assert!(extracted.contains("3→ c"));
        assert!(!extracted.contains("1→"));
        assert!(!extracted.contains("4→"));
    }
}
