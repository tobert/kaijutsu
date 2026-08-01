//! Diffstat — the number that goes in the collapsed preview header.

/// Files changed, lines added, lines removed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DiffStat {
    /// Number of file sections.
    pub files_changed: usize,
    /// Total `+` lines.
    pub insertions: usize,
    /// Total `-` lines.
    pub deletions: usize,
}

impl DiffStat {
    /// True when nothing changed at all.
    pub fn is_empty(&self) -> bool {
        self.insertions == 0 && self.deletions == 0
    }
}

impl std::fmt::Display for DiffStat {
    /// The preview header form: `3 files, +42 −17`.
    ///
    /// The minus is U+2212 MINUS SIGN, not a hyphen — it reads as an operator
    /// beside the `+` instead of as a stray dash, and it will never be confused
    /// for a diff line prefix.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let noun = if self.files_changed == 1 {
            "file"
        } else {
            "files"
        };
        write!(
            f,
            "{} {}, +{} \u{2212}{}",
            self.files_changed, noun, self.insertions, self.deletions
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_matches_the_preview_header_contract() {
        let stat = DiffStat {
            files_changed: 3,
            insertions: 42,
            deletions: 17,
        };
        assert_eq!(stat.to_string(), "3 files, +42 −17");
    }

    #[test]
    fn one_file_is_singular() {
        let stat = DiffStat {
            files_changed: 1,
            insertions: 1,
            deletions: 0,
        };
        assert_eq!(stat.to_string(), "1 file, +1 −0");
    }
}
