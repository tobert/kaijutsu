//! Shared argument parsing helpers for kj commands.

use std::collections::HashSet;

use kaijutsu_types::ToolFilter;

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

/// Parse a tool filter spec string.
///
/// Formats:
/// - `"all"` → `ToolFilter::All`
/// - `"allow:glob,grep,read"` → `ToolFilter::AllowList(["glob", "grep", "read"])`
/// - `"deny:bash"` → `ToolFilter::DenyList(["bash"])`
pub fn parse_tool_filter_spec(spec: &str) -> Result<ToolFilter, String> {
    let spec = spec.trim();
    if spec.eq_ignore_ascii_case("all") {
        return Ok(ToolFilter::All);
    }
    if let Some(list) = spec.strip_prefix("allow:") {
        let tools: HashSet<String> = list
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        if tools.is_empty() {
            return Err("allow list must contain at least one tool".to_string());
        }
        return Ok(ToolFilter::AllowList(tools));
    }
    if let Some(list) = spec.strip_prefix("deny:") {
        let tools: HashSet<String> = list
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        if tools.is_empty() {
            return Err("deny list must contain at least one tool".to_string());
        }
        return Ok(ToolFilter::DenyList(tools));
    }
    Err(format!(
        "invalid tool filter spec '{}' — use 'all', 'allow:tool1,tool2', or 'deny:tool1,tool2'",
        spec
    ))
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

    #[test]
    fn parse_tool_filter_all() {
        assert!(matches!(parse_tool_filter_spec("all"), Ok(ToolFilter::All)));
        assert!(matches!(parse_tool_filter_spec("ALL"), Ok(ToolFilter::All)));
    }

    #[test]
    fn parse_tool_filter_allow() {
        match parse_tool_filter_spec("allow:glob,grep,read") {
            Ok(ToolFilter::AllowList(tools)) => {
                assert!(tools.contains("glob"));
                assert!(tools.contains("grep"));
                assert!(tools.contains("read"));
                assert_eq!(tools.len(), 3);
            }
            other => panic!("expected AllowList, got {:?}", other),
        }
    }

    #[test]
    fn parse_tool_filter_deny() {
        match parse_tool_filter_spec("deny:bash") {
            Ok(ToolFilter::DenyList(tools)) => {
                assert!(tools.contains("bash"));
                assert_eq!(tools.len(), 1);
            }
            other => panic!("expected DenyList, got {:?}", other),
        }
    }

    #[test]
    fn parse_tool_filter_invalid() {
        assert!(parse_tool_filter_spec("foobar").is_err());
    }
}
