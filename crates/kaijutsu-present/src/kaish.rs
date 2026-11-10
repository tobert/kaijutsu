//! Kaish syntax validation, in process.
//!
//! Parses kaish commands with kaish-kernel's own lexer and parser — no
//! subprocess, the same grammar the kernel executes — so a client can
//! validate on each keystroke and tint invalid input.

use kaish_kernel::parser;

/// Result of validating a kaish command.
#[derive(Debug, Clone)]
pub struct ValidationResult {
    /// Whether the input is syntactically valid.
    pub valid: bool,
    /// Error messages if invalid (used for future error position highlighting).
    pub errors: Vec<SyntaxError>,
    /// Whether input is incomplete (could be valid with more input).
    pub incomplete: bool,
}

/// A syntax error with location information.
#[derive(Debug, Clone)]
pub struct SyntaxError {
    /// Byte offset of the start of the error.
    pub start: usize,
    /// Byte offset of the end of the error.
    pub end: usize,
    /// Human-readable error message.
    pub message: String,
}

/// Validate a kaish command string.
///
/// Returns a result indicating whether the input is valid, any errors,
/// and whether the input is incomplete (waiting for more input).
pub fn validate(input: &str) -> ValidationResult {
    if input.trim().is_empty() {
        return ValidationResult {
            valid: true,
            errors: Vec::new(),
            incomplete: false,
        };
    }

    match parser::parse(input) {
        Ok(_ast) => ValidationResult {
            valid: true,
            errors: Vec::new(),
            incomplete: false,
        },
        Err(errs) => {
            let mut errors = Vec::new();
            let mut incomplete = false;

            for err in errs {
                // Check if this is an "unexpected end of input" error
                let msg = err.message.clone();
                if msg.contains("end of input") || msg.contains("unexpected end") {
                    incomplete = true;
                }

                errors.push(SyntaxError {
                    start: err.span.start,
                    end: err.span.end,
                    message: msg,
                });
            }

            ValidationResult {
                valid: false,
                errors,
                incomplete,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_validate_simple_command() {
        let result = validate("echo hello");
        assert!(result.valid);
        assert!(result.errors.is_empty());
    }

    #[test]
    fn test_validate_pipe() {
        let result = validate("ls | grep foo");
        assert!(result.valid);
    }

    #[test]
    fn test_validate_empty() {
        let result = validate("");
        assert!(result.valid);
        assert!(result.errors.is_empty());
    }
}
