//! Rules both clients' `:` lines share.

/// The shell statement a `:vi <args>` line runs: `vi <args>`, trimmed, the
/// same as `:!vi <args>`, so the builtin's own argument rules apply. `body` is
/// the line without its `:` prefix. `None` for any other line, `vim` and `vi!`
/// included.
pub fn vi_statement(body: &str) -> Option<String> {
    if body == "vi" {
        return Some("vi".to_string());
    }
    body.strip_prefix("vi ").map(|args| format!("vi {}", args.trim()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The rest of the line goes to the shell, so quotes and `$vars` mean what
    /// they mean in the shell.
    #[test]
    fn vi_runs_the_builtin_through_the_shell() {
        assert_eq!(vi_statement("vi notes.md").as_deref(), Some("vi notes.md"));
        assert_eq!(vi_statement("vi").as_deref(), Some("vi"));
        assert_eq!(vi_statement("vi   rc/S00.kai  ").as_deref(), Some("vi rc/S00.kai"));
        assert_eq!(vi_statement("vi \"my notes.md\"").as_deref(), Some("vi \"my notes.md\""));
    }

    /// `:vim` and `:vi!` are not `vi` with a path — a prefix match alone
    /// would open the wrong thing on a typo.
    #[test]
    fn vi_needs_a_space_or_nothing_after_it() {
        assert_eq!(vi_statement("vim notes.md"), None);
        assert_eq!(vi_statement("vi!"), None);
        assert_eq!(vi_statement("w"), None);
        assert_eq!(vi_statement(""), None);
    }
}
