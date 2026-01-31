//! Kaish syntax validation for the Kaijutsu app.
//!
//! This module provides in-process parsing of kaish commands for:
//! - Syntax validation (highlight errors as user types)
//! - Token classification (for syntax highlighting)
//! - Completion hints (identify context for suggestions)
//!
//! Uses kaish-kernel's lexer and parser directly without spawning a subprocess.
//!
//! Note: This module is awaiting integration with the shell input mode.

#![allow(dead_code)]

use kaish_kernel::lexer::{Token, TokenCategory};
use kaish_kernel::parser;

/// Result of validating a kaish command.
#[derive(Debug, Clone)]
pub struct ValidationResult {
    /// Whether the input is syntactically valid.
    pub valid: bool,
    /// Error messages if invalid.
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

/// Token with span information for syntax highlighting.
#[derive(Debug, Clone)]
pub struct SpannedToken {
    /// The token kind.
    pub kind: TokenKind,
    /// Byte offset of the start.
    pub start: usize,
    /// Byte offset of the end.
    pub end: usize,
}

/// Simplified token kinds for syntax highlighting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenKind {
    /// Command or identifier.
    Command,
    /// String literal.
    String,
    /// Number literal.
    Number,
    /// Operator (|, &&, ||, etc.).
    Operator,
    /// Variable reference ($foo).
    Variable,
    /// Keyword (if, for, fn, etc.).
    Keyword,
    /// Flag (--flag, -f).
    Flag,
    /// Comment.
    Comment,
    /// Punctuation (, ; { } etc.).
    Punctuation,
    /// Path (/foo/bar).
    Path,
    /// Error token.
    Error,
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

/// Tokenize input for syntax highlighting.
///
/// Returns a list of tokens with their spans and kinds.
pub fn tokenize(input: &str) -> Vec<SpannedToken> {
    use logos::Logos;

    let mut tokens = Vec::new();
    let lexer = Token::lexer(input);

    for (result, span) in lexer.spanned() {
        let kind = match result {
            Ok(token) => classify_token(&token),
            Err(_) => TokenKind::Error,
        };

        tokens.push(SpannedToken {
            kind,
            start: span.start,
            end: span.end,
        });
    }

    tokens
}

/// Classify a token for syntax highlighting.
///
/// Uses kaish's `TokenCategory` for stable classification that doesn't break
/// when new tokens are added to the lexer.
fn classify_token(token: &Token) -> TokenKind {
    match token.category() {
        TokenCategory::Keyword => TokenKind::Keyword,
        TokenCategory::Operator => TokenKind::Operator,
        TokenCategory::String => TokenKind::String,
        TokenCategory::Number => TokenKind::Number,
        TokenCategory::Variable => TokenKind::Variable,
        TokenCategory::Flag => TokenKind::Flag,
        TokenCategory::Punctuation => TokenKind::Punctuation,
        TokenCategory::Comment => TokenKind::Comment,
        TokenCategory::Path => TokenKind::Path,
        TokenCategory::Command => TokenKind::Command,
        TokenCategory::Error => TokenKind::Error,
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

    #[test]
    fn test_tokenize_simple() {
        let tokens = tokenize("echo hello");
        assert!(!tokens.is_empty());
        // First token should be a command
        assert_eq!(tokens[0].kind, TokenKind::Command);
    }

    #[test]
    fn test_tokenize_variable() {
        let tokens = tokenize("echo $HOME");
        let var_tokens: Vec<_> = tokens.iter().filter(|t| t.kind == TokenKind::Variable).collect();
        assert!(!var_tokens.is_empty());
    }

    #[test]
    fn test_tokenize_pipe() {
        let tokens = tokenize("ls | grep foo");
        let pipe_tokens: Vec<_> = tokens.iter().filter(|t| t.kind == TokenKind::Operator).collect();
        assert!(!pipe_tokens.is_empty());
    }

    #[test]
    fn test_tokenize_flags() {
        let tokens = tokenize("ls -la --color");
        let flag_tokens: Vec<_> = tokens.iter().filter(|t| t.kind == TokenKind::Flag).collect();
        assert_eq!(flag_tokens.len(), 2);
    }
}
