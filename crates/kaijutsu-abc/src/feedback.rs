//! Parser feedback (warnings, errors, suggestions).
//!
//! The generous parser philosophy means we try to continue parsing
//! even when encountering issues, collecting feedback along the way.

use serde::{Deserialize, Serialize};

/// Feedback from parsing - warnings, errors, and suggestions
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Feedback {
    pub level: FeedbackLevel,
    pub message: String,
    pub line: usize,
    pub column: usize,
    pub span: Option<(usize, usize)>, // (start, end) byte offsets
    pub suggestion: Option<String>,
}

impl Feedback {
    pub fn error(message: impl Into<String>, line: usize, column: usize) -> Self {
        Feedback {
            level: FeedbackLevel::Error,
            message: message.into(),
            line,
            column,
            span: None,
            suggestion: None,
        }
    }

    pub fn warning(message: impl Into<String>, line: usize, column: usize) -> Self {
        Feedback {
            level: FeedbackLevel::Warning,
            message: message.into(),
            line,
            column,
            span: None,
            suggestion: None,
        }
    }

    pub fn info(message: impl Into<String>, line: usize, column: usize) -> Self {
        Feedback {
            level: FeedbackLevel::Info,
            message: message.into(),
            line,
            column,
            span: None,
            suggestion: None,
        }
    }

    pub fn with_suggestion(mut self, suggestion: impl Into<String>) -> Self {
        self.suggestion = Some(suggestion.into());
        self
    }

    pub fn with_span(mut self, start: usize, end: usize) -> Self {
        self.span = Some((start, end));
        self
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum FeedbackLevel {
    /// Fatal error - can't continue parsing this section
    Error,
    /// Warning - parsed with assumptions, may not be what user intended
    Warning,
    /// Info - style suggestion or minor issue
    Info,
}

/// Collector for feedback during parsing
#[derive(Debug, Default)]
pub struct FeedbackCollector {
    feedback: Vec<Feedback>,
    current_line: usize,
    current_column: usize,
}

impl FeedbackCollector {
    pub fn new() -> Self {
        FeedbackCollector {
            feedback: Vec::new(),
            current_line: 1,
            current_column: 1,
        }
    }

    /// Update position tracking (call when advancing through input)
    pub fn set_position(&mut self, line: usize, column: usize) {
        self.current_line = line;
        self.current_column = column;
    }

    /// Add an error at current position
    pub fn error(&mut self, message: impl Into<String>) {
        self.feedback.push(Feedback::error(
            message,
            self.current_line,
            self.current_column,
        ));
    }

    /// Add a warning at current position
    pub fn warning(&mut self, message: impl Into<String>) {
        self.feedback.push(Feedback::warning(
            message,
            self.current_line,
            self.current_column,
        ));
    }

    /// Add a warning with suggestion at current position
    pub fn warning_with_suggestion(
        &mut self,
        message: impl Into<String>,
        suggestion: impl Into<String>,
    ) {
        self.feedback.push(
            Feedback::warning(message, self.current_line, self.current_column)
                .with_suggestion(suggestion),
        );
    }

    /// Add info at current position
    pub fn info(&mut self, message: impl Into<String>) {
        self.feedback.push(Feedback::info(
            message,
            self.current_line,
            self.current_column,
        ));
    }

    /// Check if any errors were recorded
    pub fn has_errors(&self) -> bool {
        self.feedback
            .iter()
            .any(|f| f.level == FeedbackLevel::Error)
    }

    /// Get all feedback
    pub fn into_feedback(self) -> Vec<Feedback> {
        self.feedback
    }

    /// Get feedback by reference
    pub fn feedback(&self) -> &[Feedback] {
        &self.feedback
    }
}

/// Result of parsing with feedback
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ParseResult<T> {
    pub value: T,
    pub feedback: Vec<Feedback>,
}

impl<T> ParseResult<T> {
    pub fn new(value: T, feedback: Vec<Feedback>) -> Self {
        ParseResult { value, feedback }
    }

    pub fn ok(value: T) -> Self {
        ParseResult {
            value,
            feedback: Vec::new(),
        }
    }

    pub fn has_errors(&self) -> bool {
        self.feedback
            .iter()
            .any(|f| f.level == FeedbackLevel::Error)
    }

    pub fn warnings(&self) -> impl Iterator<Item = &Feedback> {
        self.feedback
            .iter()
            .filter(|f| f.level == FeedbackLevel::Warning)
    }

    pub fn errors(&self) -> impl Iterator<Item = &Feedback> {
        self.feedback
            .iter()
            .filter(|f| f.level == FeedbackLevel::Error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_feedback_builder() {
        let fb = Feedback::warning("Missing M: field", 1, 1)
            .with_suggestion("Add M:4/4 after the title");

        assert_eq!(fb.level, FeedbackLevel::Warning);
        assert_eq!(fb.message, "Missing M: field");
        assert_eq!(fb.suggestion, Some("Add M:4/4 after the title".to_string()));
    }

    #[test]
    fn test_feedback_collector() {
        let mut collector = FeedbackCollector::new();

        collector.warning("Missing M: field");
        collector.set_position(5, 10);
        collector.error("Invalid key signature");

        let feedback = collector.into_feedback();
        assert_eq!(feedback.len(), 2);
        assert_eq!(feedback[0].line, 1);
        assert_eq!(feedback[1].line, 5);
        assert_eq!(feedback[1].column, 10);
    }

    #[test]
    fn test_parse_result() {
        let result: ParseResult<i32> = ParseResult::new(
            42,
            vec![
                Feedback::warning("test warning", 1, 1),
                Feedback::error("test error", 2, 1),
            ],
        );

        assert!(result.has_errors());
        assert_eq!(result.warnings().count(), 1);
        assert_eq!(result.errors().count(), 1);
    }
}
