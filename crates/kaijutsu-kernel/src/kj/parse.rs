//! Shared argument parsing helpers for kj commands.

use crate::LlmRegistry;

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

/// Resolve a `--model` spec to a concrete `(provider, model)` pair against the
/// registry. The single resolver shared by `kj context set` and `kj fork` so
/// the two surfaces can't drift apart again.
///
/// - `provider/model` (slash): explicit provider — it must exist.
/// - bare name: a `models.toml` alias first (e.g. `deepseek-lite`), else a
///   model on the default provider.
/// - `provider:model` (colon): rejected with a slash hint when the prefix names
///   a real provider. The separator is `/`, never `:` — `:` collides with
///   ollama tags like `gemma4:31b`, so the parser can't split on it.
///
/// An empty spec yields `(None, None)`. Every unresolvable or ambiguous case
/// fails loud here rather than silently routing the literal to the default
/// provider, where it would only surface as a turn-time
/// `not_found_error: model: <the whole string>`.
pub fn resolve_model_choice(
    registry: &LlmRegistry,
    spec: &str,
) -> Result<(Option<String>, Option<String>), String> {
    let (mut provider, mut model) = parse_model_spec(spec);
    if let Some(ref p) = provider {
        // Explicit provider — must exist.
        if registry.get(p).is_none() {
            return Err(format!("unknown provider '{p}'"));
        }
    } else if let Some(m) = model.clone() {
        if let Some((alias_provider, alias_model)) = registry.resolve_alias(&m) {
            let alias_provider = alias_provider.to_string();
            if registry.get(&alias_provider).is_none() {
                return Err(format!(
                    "model alias '{m}' points at unknown provider '{alias_provider}'"
                ));
            }
            model = Some(alias_model.to_string());
            provider = Some(alias_provider);
        } else if let Some((maybe_provider, rest)) = m.split_once(':')
            && registry.get(maybe_provider).is_some()
        {
            return Err(format!(
                "'{m}' looks like 'provider:model' — separate with a slash: '{maybe_provider}/{rest}'"
            ));
        } else {
            match registry.default_provider_name() {
                Some(p) => provider = Some(p.to_string()),
                None => {
                    return Err(format!("no provider configured for model '{m}'"));
                }
            }
        }
    }
    Ok((provider, model))
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

    /// A registry with two providers (`anthropic` as the explicit default and
    /// `deepseek`) plus a `deepseek-lite` alias pointing at deepseek. `register`
    /// alone sets no default — `set_default` does — so we set it explicitly.
    fn registry_with_alias() -> LlmRegistry {
        use crate::llm::{MockClient, Provider};
        use std::collections::HashMap;
        use std::sync::Arc;
        let mut reg = LlmRegistry::new();
        reg.register("anthropic", Arc::new(Provider::Mock(MockClient::new("a"))));
        reg.register("deepseek", Arc::new(Provider::Mock(MockClient::new("d"))));
        reg.set_default("anthropic");
        let mut aliases = HashMap::new();
        aliases.insert(
            s("deepseek-lite"),
            crate::llm::ModelAlias {
                provider: s("deepseek"),
                model: s("deepseek-v4-flash"),
            },
        );
        reg.set_model_aliases(aliases);
        reg
    }

    #[test]
    fn resolve_model_choice_slash_explicit_provider() {
        let reg = registry_with_alias();
        let (p, m) = resolve_model_choice(&reg, "deepseek/deepseek-v4-pro").unwrap();
        assert_eq!(p, Some(s("deepseek")));
        assert_eq!(m, Some(s("deepseek-v4-pro")));
    }

    #[test]
    fn resolve_model_choice_bare_alias_resolves_to_its_provider() {
        // The fork regression: a bare alias must route to deepseek, not the
        // default provider with the literal alias string.
        let reg = registry_with_alias();
        let (p, m) = resolve_model_choice(&reg, "deepseek-lite").unwrap();
        assert_eq!(p, Some(s("deepseek")));
        assert_eq!(m, Some(s("deepseek-v4-flash")));
    }

    #[test]
    fn resolve_model_choice_unknown_provider_errors() {
        let reg = registry_with_alias();
        let err = resolve_model_choice(&reg, "nope/foo").unwrap_err();
        assert!(err.contains("unknown provider"), "got: {err}");
    }

    #[test]
    fn resolve_model_choice_colon_footgun_errors_with_slash_hint() {
        let reg = registry_with_alias();
        let err = resolve_model_choice(&reg, "deepseek:deepseek-v4-flash").unwrap_err();
        assert!(
            err.contains("provider:model") && err.contains("deepseek/deepseek-v4-flash"),
            "expected slash hint, got: {err}"
        );
    }

    #[test]
    fn resolve_model_choice_ollama_tag_not_mistaken_for_provider() {
        // `gemma4:31b` — prefix is not a registered provider, so it must fall
        // through to the default provider, never trip the colon hint. This is
        // why the separator is `/`, not `:`.
        let reg = registry_with_alias();
        let (p, m) = resolve_model_choice(&reg, "gemma4:31b").unwrap();
        assert_eq!(p, Some(s("anthropic")), "falls to default provider");
        assert_eq!(m, Some(s("gemma4:31b")), "tag kept verbatim");
    }

    #[test]
    fn resolve_model_choice_empty_is_none_none() {
        let reg = registry_with_alias();
        let (p, m) = resolve_model_choice(&reg, "").unwrap();
        assert_eq!(p, None);
        assert_eq!(m, None);
    }
}
