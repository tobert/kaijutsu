//! Git-style C-quoting for paths in diff headers.
//!
//! # Why we quote more than git does
//!
//! Git does not quote a path merely because it contains a space, which makes
//! its own `diff --git a/x y b/x y` line formally ambiguous — you cannot tell
//! where the pre-image path ends. Git gets away with it because `git apply`
//! reads the `---`/`+++` headers instead. We take the same escape hatch (see
//! [`crate::parse`]: the `diff --git` paths are *advisory*, the `---`/`+++`
//! headers are authoritative) **and** additionally quote on space, so our own
//! canonical output has no ambiguous line in it at all.
//!
//! We do *not* quote non-ASCII. Git's `core.quotePath` default escapes UTF-8
//! into octal, which is unreadable and unnecessary here: kaijutsu paths are
//! UTF-8 `String`s end to end, and 日本語 filenames should look like 日本語.

use crate::error::DiffError;

/// True when `path` must be double-quoted to appear unambiguously in a header.
pub fn needs_quoting(path: &str) -> bool {
    path.chars()
        .any(|c| c == '"' || c == '\\' || c == ' ' || c.is_control())
}

/// Render `path` for a diff header, quoting and escaping only if required.
pub fn quote(path: &str) -> String {
    if !needs_quoting(path) {
        return path.to_string();
    }
    let mut out = String::with_capacity(path.len() + 2);
    out.push('"');
    for c in path.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '\r' => out.push_str("\\r"),
            c if c.is_control() => {
                // Control characters below 0x80 only; `is_control` also covers
                // U+0080..U+009F, which encode as two bytes — octal-escape each.
                let mut buf = [0u8; 4];
                for byte in c.encode_utf8(&mut buf).as_bytes() {
                    out.push_str(&format!("\\{byte:03o}"));
                }
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Decode a header path token.
///
/// Unquoted tokens pass through. Quoted tokens are C-unescaped; the closing
/// quote must be the last character, because a diff header holds exactly one
/// path per line.
pub fn unquote(token: &str, line: usize) -> Result<String, DiffError> {
    let Some(body) = token.strip_prefix('"') else {
        return Ok(token.to_string());
    };
    let malformed = || DiffError::MalformedPath {
        line,
        found: token.to_string(),
    };
    let body = body.strip_suffix('"').ok_or_else(malformed)?;

    let mut out = String::with_capacity(body.len());
    let mut bytes = Vec::new();
    let mut chars = body.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            flush_octal(&mut bytes, &mut out, line, token)?;
            out.push(c);
            continue;
        }
        match chars.next().ok_or_else(malformed)? {
            '"' => {
                flush_octal(&mut bytes, &mut out, line, token)?;
                out.push('"');
            }
            '\\' => {
                flush_octal(&mut bytes, &mut out, line, token)?;
                out.push('\\');
            }
            'n' => {
                flush_octal(&mut bytes, &mut out, line, token)?;
                out.push('\n');
            }
            't' => {
                flush_octal(&mut bytes, &mut out, line, token)?;
                out.push('\t');
            }
            'r' => {
                flush_octal(&mut bytes, &mut out, line, token)?;
                out.push('\r');
            }
            d @ '0'..='7' => {
                // Octal escapes are byte-wise and may spell a multi-byte
                // character, so they buffer until a non-escape interrupts them.
                let mut value = d.to_digit(8).unwrap();
                for _ in 0..2 {
                    let next = chars.clone().next().ok_or_else(malformed)?;
                    let digit = next.to_digit(8).ok_or_else(malformed)?;
                    value = value * 8 + digit;
                    chars.next();
                }
                bytes.push(u8::try_from(value).map_err(|_| malformed())?);
            }
            _ => return Err(malformed()),
        }
    }
    flush_octal(&mut bytes, &mut out, line, token)?;
    Ok(out)
}

fn flush_octal(
    bytes: &mut Vec<u8>,
    out: &mut String,
    line: usize,
    token: &str,
) -> Result<(), DiffError> {
    if bytes.is_empty() {
        return Ok(());
    }
    let decoded =
        String::from_utf8(std::mem::take(bytes)).map_err(|_| DiffError::MalformedPath {
            line,
            found: token.to_string(),
        })?;
    out.push_str(&decoded);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_paths_pass_through() {
        assert_eq!(quote("src/main.rs"), "src/main.rs");
        assert_eq!(unquote("src/main.rs", 1).unwrap(), "src/main.rs");
    }

    #[test]
    fn unicode_paths_are_not_escaped() {
        assert_eq!(quote("docs/設計.md"), "docs/設計.md");
    }

    #[test]
    fn spaces_and_quotes_round_trip() {
        for path in [
            "a/two words.txt",
            "a/say \"hi\".txt",
            "a/back\\slash",
            "a/tab\there",
        ] {
            let quoted = quote(path);
            assert!(quoted.starts_with('"'), "{path:?} should have been quoted");
            assert_eq!(unquote(&quoted, 1).unwrap(), path);
        }
    }

    #[test]
    fn git_octal_escapes_decode_to_utf8() {
        // git's `core.quotePath` rendering of "é"
        assert_eq!(unquote("\"a/\\303\\251.txt\"", 1).unwrap(), "a/é.txt");
    }

    #[test]
    fn unterminated_quote_is_an_error() {
        assert!(matches!(
            unquote("\"a/oops", 7),
            Err(DiffError::MalformedPath { line: 7, .. })
        ));
    }

    #[test]
    fn unknown_escape_is_an_error() {
        assert!(unquote("\"a/\\q\"", 1).is_err());
    }
}
