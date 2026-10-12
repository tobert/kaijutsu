//! The `:` line's own dialect — parsed by the tui itself, never by
//! `kaijutsu-editor`'s `:w`/`:q` ex-command dialect (`docs/tui.md`, "The `:`
//! line"). That core dialect answers the alternate-screen editor's own `:`
//! bar; this one answers compose's.
//!
//! Pure: [`parse`] takes the submitted line, prefix included (`":kj fork"`),
//! and returns the verb. Nothing here reaches the kernel — `run.rs` is what
//! calls `execute_kj`/`shell_execute` on the result.

/// What a submitted `:` line asked for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ColonVerb {
    /// `:kj <argv>` — run `argv` through `execute_kj`. Whitespace-split, no
    /// shell-style quoting (MVP; `docs/tui.md` names the limitation).
    Kj(Vec<String>),
    /// `:!<statement>` — one kaish statement through `shell_execute`, the
    /// gated human path a key press already takes.
    Shell(String),
    /// `:q` (unforced) / `:q!` (forced).
    Quit { force: bool },
    /// Anything else — the caller posts it verbatim as a status-line notice.
    Unknown,
}

/// Parse a submitted `:` line, `:` prefix included. A line with no `:`
/// prefix at all (should never reach here — the bar only submits ex-command
/// lines) also parses as [`ColonVerb::Unknown`] rather than panicking.
pub fn parse(line: &str) -> ColonVerb {
    let Some(body) = line.strip_prefix(':') else {
        return ColonVerb::Unknown;
    };
    if body == "q" {
        return ColonVerb::Quit { force: false };
    }
    if body == "q!" {
        return ColonVerb::Quit { force: true };
    }
    if let Some(statement) = body.strip_prefix('!') {
        return ColonVerb::Shell(statement.to_string());
    }
    // `:vi <args>` is `:!vi <args>`: the same shell statement, so the
    // builtin's own argument rules apply.
    if body == "vi" {
        return ColonVerb::Shell("vi".to_string());
    }
    if let Some(rest) = body.strip_prefix("vi ") {
        return ColonVerb::Shell(format!("vi {}", rest.trim()));
    }
    if let Some(rest) = body.strip_prefix("kj") {
        // A space must separate the verb from its argv — `:kjunk` is not
        // `:kj unk`, and `:kj` alone is a valid empty-argv call.
        let argv_text = match rest.strip_prefix(' ') {
            Some(r) => r,
            None if rest.is_empty() => "",
            None => return ColonVerb::Unknown,
        };
        return ColonVerb::Kj(argv_text.split_whitespace().map(str::to_string).collect());
    }
    ColonVerb::Unknown
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kj_splits_its_argv_on_whitespace() {
        assert_eq!(
            parse(":kj context list"),
            ColonVerb::Kj(vec!["context".to_string(), "list".to_string()])
        );
    }

    #[test]
    fn bare_kj_is_an_empty_argv() {
        assert_eq!(parse(":kj"), ColonVerb::Kj(vec![]));
    }

    #[test]
    fn kjunk_is_not_kj_with_argv_unk() {
        assert_eq!(parse(":kjunk"), ColonVerb::Unknown);
    }

    #[test]
    fn bang_is_one_kaish_statement() {
        assert_eq!(
            parse(":!echo hi"),
            ColonVerb::Shell("echo hi".to_string())
        );
    }

    #[test]
    fn bare_bang_is_an_empty_statement() {
        assert_eq!(parse(":!"), ColonVerb::Shell(String::new()));
    }

    /// `:vi` is `:!vi`: the rest of the line goes to the shell, so quotes
    /// and `$vars` mean what they mean in the shell.
    #[test]
    fn vi_runs_the_builtin_through_the_shell() {
        assert_eq!(parse(":vi notes.md"), ColonVerb::Shell("vi notes.md".to_string()));
        assert_eq!(parse(":vi"), ColonVerb::Shell("vi".to_string()));
        assert_eq!(parse(":vi   rc/S00.kai  "), ColonVerb::Shell("vi rc/S00.kai".to_string()));
        assert_eq!(parse(":vi \"my notes.md\""), ColonVerb::Shell("vi \"my notes.md\"".to_string()));
        assert_eq!(parse(":vi"), parse(":!vi"));
    }

    /// `:vim` and `:vi!` are not `vi` with a path — a prefix match alone
    /// would open the wrong thing on a typo.
    #[test]
    fn vi_needs_a_space_or_nothing_after_it() {
        assert_eq!(parse(":vim notes.md"), ColonVerb::Unknown);
        assert_eq!(parse(":vi!"), ColonVerb::Unknown);
    }

    #[test]
    fn q_quits_unforced() {
        assert_eq!(parse(":q"), ColonVerb::Quit { force: false });
    }

    #[test]
    fn q_bang_quits_forced() {
        assert_eq!(parse(":q!"), ColonVerb::Quit { force: true });
    }

    /// The mutation this guards against: treating `:q!` as `:q` (dropping
    /// the `force` distinction) would let a running turn silently survive a
    /// quit the player asked to force.
    #[test]
    fn q_and_q_bang_are_never_confused() {
        assert_ne!(parse(":q"), parse(":q!"));
    }

    #[test]
    fn an_unknown_verb_is_unknown() {
        assert_eq!(parse(":wat"), ColonVerb::Unknown);
    }

    #[test]
    fn a_bare_colon_is_unknown_not_a_command() {
        assert_eq!(parse(":"), ColonVerb::Unknown);
    }

    #[test]
    fn a_line_with_no_colon_prefix_is_unknown() {
        assert_eq!(parse("kj fork"), ColonVerb::Unknown);
    }
}
