//! Error types for block tools.
//!
//! These errors provide rich feedback to models, helping them understand
//! what went wrong and how to fix it.

use thiserror::Error;

/// Errors that can occur during block editing operations.
#[derive(Debug, Error, PartialEq)]
pub enum EditError {
    /// The requested line number is out of range.
    #[error("line {requested} is out of range (file has {max} lines)")]
    LineOutOfRange {
        /// The line number that was requested.
        requested: u32,
        /// The maximum valid line number (0-indexed count).
        max: u32,
    },

    /// Content mismatch during compare-and-set validation.
    #[error("content mismatch at lines {start_line}..{end_line}\nexpected:\n{expected}\nactual:\n{actual}")]
    ContentMismatch {
        /// The expected content.
        expected: String,
        /// The actual content found.
        actual: String,
        /// Start line of the range.
        start_line: u32,
        /// End line of the range (exclusive).
        end_line: u32,
    },

    /// The specified block was not found.
    #[error("block not found: {0}")]
    BlockNotFound(String),

    /// An operation within an atomic batch failed.
    #[error("atomic batch failed at operation {op_index}: {error}")]
    AtomicBatchFailed {
        /// Index of the failed operation.
        op_index: usize,
        /// The underlying error.
        error: Box<EditError>,
    },

    /// Invalid operation parameters.
    #[error("invalid parameters: {0}")]
    InvalidParams(String),

    /// The block store returned an error.
    #[error("block store error: {0}")]
    StoreError(String),

    /// Invalid regular expression pattern.
    #[error("invalid regex pattern: {0}")]
    InvalidRegex(String),

    /// Search returned no matches.
    #[error("no matches found for pattern")]
    NoMatches,
}

impl EditError {
    /// Create a ContentMismatch error with context.
    pub fn content_mismatch(
        expected: impl Into<String>,
        actual: impl Into<String>,
        start_line: u32,
        end_line: u32,
    ) -> Self {
        Self::ContentMismatch {
            expected: expected.into(),
            actual: actual.into(),
            start_line,
            end_line,
        }
    }

    /// Create a LineOutOfRange error.
    pub fn line_out_of_range(requested: u32, max: u32) -> Self {
        Self::LineOutOfRange { requested, max }
    }

    /// Wrap an error as a batch failure.
    pub fn in_batch(self, op_index: usize) -> Self {
        Self::AtomicBatchFailed {
            op_index,
            error: Box::new(self),
        }
    }
}

/// Result type for block operations.
pub type Result<T> = std::result::Result<T, EditError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_error_messages() {
        let err = EditError::line_out_of_range(10, 5);
        assert!(err.to_string().contains("10"));
        assert!(err.to_string().contains("5"));

        let err = EditError::content_mismatch("expected", "actual", 0, 1);
        let msg = err.to_string();
        assert!(msg.contains("expected"));
        assert!(msg.contains("actual"));

        let err = EditError::BlockNotFound("test-id".into());
        assert!(err.to_string().contains("test-id"));
    }

    #[test]
    fn test_batch_error_wrapping() {
        let inner = EditError::line_out_of_range(10, 5);
        let wrapped = inner.in_batch(2);
        let msg = wrapped.to_string();
        assert!(msg.contains("operation 2"));
        assert!(msg.contains("10"));
    }
}
