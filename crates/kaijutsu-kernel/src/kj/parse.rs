//! Shared argument parsing helpers for kj commands.

/// Extract a named argument value from argv (e.g., `--name foo`).
///
/// Checks multiple flag variants and returns the value immediately following.
pub fn extract_named_arg(argv: &[String], names: &[&str]) -> Option<String> {
    for (i, arg) in argv.iter().enumerate() {
        if names.contains(&arg.as_str()) {
            return argv.get(i + 1).cloned();
        }
    }
    None
}

/// Remove a named argument and its value from argv in-place.
pub fn strip_named_arg(argv: &mut Vec<String>, names: &[&str]) {
    let mut i = 0;
    while i < argv.len() {
        if names.contains(&argv[i].as_str()) {
            argv.remove(i);
            if i < argv.len() {
                argv.remove(i);
            }
        } else {
            i += 1;
        }
    }
}

/// Check if a boolean flag is present in argv.
pub fn has_flag(argv: &[String], names: &[&str]) -> bool {
    argv.iter().any(|a| names.contains(&a.as_str()))
}

/// Extract all instances of a repeatable named argument.
///
/// e.g., `--path /a --path /b` → `vec!["/a", "/b"]`
pub fn extract_all_named_args(argv: &[String], names: &[&str]) -> Vec<String> {
    let mut values = Vec::new();
    for (i, arg) in argv.iter().enumerate() {
        if names.contains(&arg.as_str())
            && let Some(val) = argv.get(i + 1)
        {
            values.push(val.clone());
        }
    }
    values
}

/// Rewrite a trailing bare `help` token to `--help` so clap renders the leaf
/// subcommand's help instead of binding `help` as a positional value.
///
/// Guards the footgun where `kj context create help` silently mints a context
/// labelled "help" (the verb's positional `label` swallows the bare token,
/// since the clap structs `disable_help_subcommand`). We only touch a `help`
/// that is **last** and **not the value of a preceding flag** — so
/// `kj context create --name help` (a deliberate "help" label via `--name`)
/// and `kj context retag help target` (help as a non-trailing positional) are
/// left untouched. Returns the argv unchanged when no rewrite applies.
pub fn normalize_trailing_help(argv: &[String]) -> Vec<String> {
    let mut out = argv.to_vec();
    if let Some(last) = out.last()
        && last == "help"
    {
        // A `help` riding behind a flag (`--name help`, `-m help`) is that
        // flag's value, not a help request — leave it alone.
        let after_flag = out
            .len()
            .checked_sub(2)
            .and_then(|i| out.get(i))
            .is_some_and(|prev| prev.starts_with('-'));
        if !after_flag {
            let idx = out.len() - 1;
            out[idx] = "--help".to_string();
        }
    }
    out
}

/// Parse a model spec like "anthropic/claude-opus-4-6" into (provider, model).
///
/// Returns (None, None) for empty strings, (None, Some(model)) for bare model names.
pub fn parse_model_spec(spec: &str) -> (Option<String>, Option<String>) {
    if spec.is_empty() {
        return (None, None);
    }
    match spec.split_once('/') {
        Some((provider, model)) => {
            let p = if provider.is_empty() {
                None
            } else {
                Some(provider.to_string())
            };
            let m = if model.is_empty() {
                None
            } else {
                Some(model.to_string())
            };
            (p, m)
        }
        None => (None, Some(spec.to_string())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &str) -> String {
        v.to_string()
    }

    #[test]
    fn extract_named_arg_found() {
        let argv = vec![s("--name"), s("foo"), s("--other"), s("bar")];
        assert_eq!(extract_named_arg(&argv, &["--name", "-n"]), Some(s("foo")));
    }

    #[test]
    fn extract_named_arg_not_found() {
        let argv = vec![s("--other"), s("bar")];
        assert_eq!(extract_named_arg(&argv, &["--name"]), None);
    }

    #[test]
    fn strip_named_arg_removes() {
        let mut argv = vec![s("fork"), s("--name"), s("foo"), s("--prompt"), s("hi")];
        strip_named_arg(&mut argv, &["--name"]);
        assert_eq!(argv, vec![s("fork"), s("--prompt"), s("hi")]);
    }

    #[test]
    fn has_flag_works() {
        let argv = vec![s("--tree"), s("--verbose")];
        assert!(has_flag(&argv, &["--tree", "-t"]));
        assert!(!has_flag(&argv, &["--quiet"]));
    }

    #[test]
    fn extract_all_named_args_repeatable() {
        let argv = vec![s("--path"), s("/a"), s("--path"), s("/b"), s("other")];
        let vals = extract_all_named_args(&argv, &["--path"]);
        assert_eq!(vals, vec![s("/a"), s("/b")]);
    }

    #[test]
    fn normalize_trailing_help_rewrites_bare_positional() {
        // The footgun: `kj context create help` — without rewrite, `help` is the
        // positional label and a context named "help" is born.
        let argv = vec![s("context"), s("create"), s("help")];
        assert_eq!(
            normalize_trailing_help(&argv),
            vec![s("context"), s("create"), s("--help")]
        );
    }

    #[test]
    fn normalize_trailing_help_rewrites_bare_subcommand_help() {
        let argv = vec![s("context"), s("help")];
        assert_eq!(
            normalize_trailing_help(&argv),
            vec![s("context"), s("--help")]
        );
    }

    #[test]
    fn normalize_trailing_help_leaves_flag_value_alone() {
        // `--name help` deliberately labels a context "help" — not a help request.
        let argv = vec![s("context"), s("create"), s("--name"), s("help")];
        assert_eq!(normalize_trailing_help(&argv), argv);
        // Short flag form too.
        let argv2 = vec![s("context"), s("create"), s("-n"), s("help")];
        assert_eq!(normalize_trailing_help(&argv2), argv2);
    }

    #[test]
    fn normalize_trailing_help_leaves_nontrailing_positional_alone() {
        // `retag help target` — `help` is the source label, not trailing.
        let argv = vec![s("context"), s("retag"), s("help"), s("target")];
        assert_eq!(normalize_trailing_help(&argv), argv);
    }

    #[test]
    fn normalize_trailing_help_passthrough_when_absent() {
        let argv = vec![s("context"), s("list"), s("--tree")];
        assert_eq!(normalize_trailing_help(&argv), argv);
    }

    #[test]
    fn parse_model_spec_full() {
        let (p, m) = parse_model_spec("anthropic/claude-opus-4-6");
        assert_eq!(p, Some(s("anthropic")));
        assert_eq!(m, Some(s("claude-opus-4-6")));
    }

    #[test]
    fn parse_model_spec_bare() {
        let (p, m) = parse_model_spec("claude-opus-4-6");
        assert_eq!(p, None);
        assert_eq!(m, Some(s("claude-opus-4-6")));
    }

    #[test]
    fn parse_model_spec_empty() {
        let (p, m) = parse_model_spec("");
        assert_eq!(p, None);
        assert_eq!(m, None);
    }

}
