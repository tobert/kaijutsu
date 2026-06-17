//! hashline — short content hashes that anchor lines for stable edits.
//!
//! The `read` tool annotates every line as `N:hash→ content`; the `edit` tool
//! can then address a line (or range) by `N:hash` instead of reproducing its
//! exact bytes. Before writing, edit re-hashes the *current* line and refuses
//! the edit if the hash moved — so an edit aimed at content that has since
//! changed fails loud rather than splicing the wrong place. Background: "The
//! Harness Problem" (blog.can.ac, 2026) and anthropics/claude-code#25775.
//!
//! The line *number* is the primary anchor; the hash is a checksum that detects
//! the line changed since it was shown. Hashing is over the line content as
//! [`str::lines`] yields it (no terminator, no trailing `\r`), so `read` and
//! `edit` agree byte-for-byte on what a "line" is.

/// Number of hex digits in a line hash. 4 hex = 16 bits → a changed line has
/// only a ~1/65536 chance of colliding with its old hash, so a stale edit is
/// overwhelmingly caught (fail-loud) rather than silently applied to wrong
/// text. Cheap in tokens: 2 extra chars per displayed line beyond the `N:`.
pub const HASH_HEX_LEN: usize = 4;

/// FNV-1a over the line's UTF-8 bytes, truncated to [`HASH_HEX_LEN`] hex digits.
/// Deterministic and dependency-free — read and edit recompute it independently
/// and must agree, so it cannot rely on any unstable/std hasher.
pub fn line_hash(line: &str) -> String {
    const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut h = FNV_OFFSET;
    for b in line.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(FNV_PRIME);
    }
    let mask = (1u64 << (HASH_HEX_LEN * 4)) - 1;
    format!("{:0width$x}", h & mask, width = HASH_HEX_LEN)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_is_stable_and_hex() {
        let a = line_hash("hello world");
        assert_eq!(a, line_hash("hello world"), "same input → same hash");
        assert_eq!(a.len(), HASH_HEX_LEN);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn whitespace_changes_the_hash() {
        // The hash anchors exact line content: indentation matters, so a
        // re-indented line is detected as changed (staleness caught).
        assert_ne!(line_hash("    x = 1"), line_hash("\tx = 1"));
        assert_ne!(line_hash("x = 1"), line_hash("x = 1 "));
    }

    #[test]
    fn distinct_lines_usually_differ() {
        assert_ne!(line_hash("foo"), line_hash("bar"));
    }
}
