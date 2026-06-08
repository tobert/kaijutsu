//! `kj model` / `kj models` — model discovery and inspection.
//!
//! Two read-only verbs, no capability gate (discovery is not escalation):
//!
//! - `kj models` enumerates the configured providers, each provider's known
//!   models, and the friendly `--model` aliases — the specs you can hand to
//!   `kj context set --model …`. The structured `.data` is an array of those
//!   specs (alias names + fully-qualified `provider/model`) so
//!   `for m in $(kj models)` iterates usable handles, per the kj list-data
//!   convention.
//! - `kj model` reports the *effective* model for a context — the column on
//!   the context row when set, otherwise the registry default it falls through
//!   to. Defaults to the current context; `--context <ref>` targets another.
//!   The `.data` is an inspect-style object.
//!
//! Both read the LLM registry behind its async `RwLock`, so the dispatch leaves
//! are async.

use clap::Parser;
use kaijutsu_types::ContentType;

use super::refs;
use super::{clap_help_for, KjCaller, KjDispatcher, KjResult};

/// `kj models` is pure discovery — no positionals, no value flags. The empty
/// struct exists so help routes through the shared `clap_help_for` path for
/// consistency with the other clap-migrated subcommands. Note: bare
/// `kj models` (no argv) deliberately *lists* rather than showing help, so the
/// dispatch path handles the empty-argv case before clap ever sees it.
#[derive(Parser, Debug)]
#[command(
    name = "models",
    about = "List configured LLM providers, their models, and --model aliases",
    disable_help_subcommand = true,
    no_binary_name = true
)]
pub(crate) struct ModelsArgs {}

/// `kj model` — report the effective model for a context. The only knob is
/// `--context <ref>`, which targets a context other than the caller's current.
#[derive(Parser, Debug)]
#[command(
    name = "model",
    about = "Report the effective model for a context",
    disable_help_subcommand = true,
    no_binary_name = true
)]
pub(crate) struct ModelArgs {
    /// Target context: . (default) | .parent | <label> | <hex prefix>
    #[arg(long, short = 'c')]
    context: Option<String>,
}

impl KjDispatcher {
    /// `kj models` — list providers, their models, and `--model` aliases.
    pub(crate) async fn dispatch_models(&self, argv: &[String]) -> KjResult {
        // Bare `kj models` (empty argv) LISTS — preserving the historical
        // behavior — so we only intercept explicit help requests here. An
        // empty `ModelsArgs` means there are no other args to parse: the list
        // path below needs no clap pass.
        if matches!(argv.first().map(|s| s.as_str()), Some("help" | "--help" | "-h")) {
            return clap_help_for::<ModelsArgs>();
        }

        let registry = self.kernel().llm().read().await;
        let default_provider = registry.default_provider_name().map(str::to_string);
        let default_model = registry.default_model().map(str::to_string);

        // Providers in stable, sorted order. Each provider reports the models
        // its client knows about, enriched with the config-declared default.
        let mut provider_names: Vec<String> = registry.list().iter().map(|s| s.to_string()).collect();
        provider_names.sort();

        // `specs` accumulates every string a caller could pass to `--model`:
        // each alias name, plus the fully-qualified `provider/model` for each
        // known model. This is the iteration-friendly `.data` payload.
        let mut specs: Vec<String> = Vec::new();

        let mut lines: Vec<String> = vec!["## Models".to_string(), String::new()];

        if provider_names.is_empty() {
            lines.push("_No LLM providers configured._".to_string());
        }

        for name in &provider_names {
            let cfg_default = registry
                .provider_config(name)
                .and_then(|c| c.default_model.clone());
            let mut models: Vec<String> = registry
                .get(name)
                .map(|p| p.available_models().iter().map(|m| m.to_string()).collect())
                .unwrap_or_default();
            if let Some(d) = &cfg_default
                && !models.contains(d)
            {
                models.push(d.clone());
            }
            models.sort();
            models.dedup();

            let is_default_provider = default_provider.as_deref() == Some(name.as_str());
            let header = if is_default_provider {
                format!("### {name} _(default provider)_")
            } else {
                format!("### {name}")
            };
            lines.push(header);
            if models.is_empty() {
                lines.push("- _(no models advertised)_".to_string());
            }
            for m in &models {
                let mut marks = Vec::new();
                if cfg_default.as_deref() == Some(m.as_str()) {
                    marks.push("provider default");
                }
                if is_default_provider && default_model.as_deref() == Some(m.as_str()) {
                    marks.push("registry default");
                }
                let suffix = if marks.is_empty() {
                    String::new()
                } else {
                    format!(" _({})_", marks.join(", "))
                };
                lines.push(format!("- `{m}`{suffix}"));
                specs.push(format!("{name}/{m}"));
            }
            lines.push(String::new());
        }

        // Aliases: the friendly `--model` names, grouped after providers.
        let aliases = registry.model_aliases();
        if !aliases.is_empty() {
            let mut alias_names: Vec<&String> = aliases.keys().collect();
            alias_names.sort();
            lines.push("### aliases".to_string());
            for alias in alias_names {
                let a = &aliases[alias];
                lines.push(format!("- `{alias}` → `{}/{}`", a.provider, a.model));
                specs.push(alias.clone());
            }
            lines.push(String::new());
        }

        if let Some(model) = &default_model {
            let prov = default_provider.as_deref().unwrap_or("?");
            lines.push(format!("_Default: `{prov}/{model}`_"));
        } else if let Some(prov) = &default_provider {
            lines.push(format!("_Default provider: `{prov}` (no default model set)_"));
        }

        specs.sort();
        specs.dedup();

        // Ephemeral: discovery output is for the operator, not the conversation.
        KjResult::ok_ephemeral_with_data(
            lines.join("\n"),
            ContentType::Markdown,
            serde_json::Value::Array(specs.into_iter().map(serde_json::Value::String).collect()),
        )
    }

    /// `kj model` — report the effective model for a context.
    pub(crate) async fn dispatch_model(&self, argv: &[String], caller: &KjCaller) -> KjResult {
        // Bare `kj model` (no argv) reports the current context's model, so
        // empty argv parses cleanly to `ModelArgs { context: None }` — no
        // early help return needed here (unlike subcommand-bearing modules).
        let parsed = match ModelArgs::try_parse_from(argv) {
            Ok(p) => p,
            Err(e) => {
                // `--help` / `-h` come through as DisplayHelp; route them to
                // ok-ephemeral so kaish prints the help and exits 0.
                if matches!(
                    e.kind(),
                    clap::error::ErrorKind::DisplayHelp
                        | clap::error::ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand
                ) {
                    return KjResult::ok_ephemeral(e.to_string(), ContentType::Plain);
                }
                return KjResult::Err(format!("kj model: {e}"));
            }
        };
        let ctx_ref = parsed.context;

        // Read the context row (its explicit provider/model columns, if any).
        let (ctx_id, row_provider, row_model) = {
            let db = self.kernel_db().lock();
            let ctx_id = match refs::resolve_context_arg(ctx_ref.as_deref(), caller, &db) {
                Ok(id) => id,
                Err(e) => return KjResult::Err(format!("kj model: {e}")),
            };
            match db.get_context(ctx_id) {
                Ok(Some(row)) => (ctx_id, row.provider, row.model),
                Ok(None) => {
                    return KjResult::Err(format!(
                        "kj model: context {} not found",
                        ctx_id.short()
                    ));
                }
                Err(e) => return KjResult::Err(format!("kj model: {e}")),
            }
        };

        // Effective model: the row's column when set, else the registry default
        // the context falls through to at turn time.
        let (provider, model, source) = match row_model {
            Some(m) => (row_provider, Some(m), "context"),
            None => {
                let registry = self.kernel().llm().read().await;
                (
                    registry.default_provider_name().map(str::to_string),
                    registry.default_model().map(str::to_string),
                    "default",
                )
            }
        };

        let display = match (&provider, &model) {
            (Some(p), Some(m)) => format!("{p}/{m}"),
            (None, Some(m)) => m.clone(),
            _ => "(none configured)".to_string(),
        };

        let message = if source == "context" {
            format!("{}: {display}", ctx_id.short())
        } else {
            format!("{}: {display} (registry default)", ctx_id.short())
        };

        KjResult::ok_ephemeral_with_data(
            message,
            ContentType::Plain,
            serde_json::json!({
                "context_id": ctx_id.to_hex(),
                "provider": provider,
                "model": model,
                "source": source,
            }),
        )
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Arc;

    use crate::kj::test_helpers::*;
    use crate::llm::toml_config::ModelAlias;
    use crate::llm::{Provider, ProviderConfig, claude, gemini};
    use kaijutsu_types::PrincipalId;

    fn s(v: &str) -> String {
        v.to_string()
    }

    /// Seed the test kernel's registry with two providers, a config default,
    /// and an alias so the discovery verbs have something to report.
    async fn seed_registry(d: &crate::kj::KjDispatcher) {
        let mut reg = d.kernel().llm().write().await;
        reg.register("anthropic", Arc::new(Provider::Claude(claude::Client::new("fake"))));
        reg.register("gemini", Arc::new(Provider::Gemini(gemini::Client::new("fake"))));
        reg.set_default("anthropic");
        reg.set_default_model("claude-opus-4-8");
        reg.set_provider_configs(vec![
            {
                let mut c = ProviderConfig::new("anthropic");
                c.default_model = Some("claude-opus-4-8".to_string());
                c
            },
        ]);
        let mut aliases = HashMap::new();
        aliases.insert(
            "fast".to_string(),
            ModelAlias {
                provider: "anthropic".to_string(),
                model: "claude-haiku-4-5-20251001".to_string(),
            },
        );
        reg.set_model_aliases(aliases);
    }

    #[tokio::test]
    async fn models_lists_providers_and_aliases() {
        let d = test_dispatcher().await;
        seed_registry(&d).await;
        let c = test_caller();

        let result = d.dispatch(&[s("models")], &c).await;
        assert!(result.is_ok(), "models failed: {}", result.message());
        let msg = result.message();
        assert!(msg.contains("anthropic"), "lists anthropic provider: {msg}");
        assert!(msg.contains("gemini"), "lists gemini provider: {msg}");
        assert!(msg.contains("fast"), "lists the alias: {msg}");
        assert!(
            msg.contains("default provider"),
            "marks the default provider: {msg}"
        );
    }

    #[tokio::test]
    async fn models_data_is_iterable_spec_array() {
        let d = test_dispatcher().await;
        seed_registry(&d).await;
        let c = test_caller();

        let result = d.dispatch(&[s("models")], &c).await;
        let data = match result {
            crate::kj::KjResult::Ok { data: Some(d), .. } => d,
            other => panic!("expected data payload, got {other:?}"),
        };
        let arr = data.as_array().expect("data is an array");
        let specs: Vec<&str> = arr.iter().filter_map(|v| v.as_str()).collect();
        // The alias name is a usable `--model` spec...
        assert!(specs.contains(&"fast"), "alias spec present: {specs:?}");
        // ...as is a fully-qualified provider/model.
        assert!(
            specs.iter().any(|s| s.starts_with("anthropic/")),
            "qualified spec present: {specs:?}"
        );
        // Iteration-friendly: sorted and deduped.
        let mut sorted = specs.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(specs, sorted, "specs are sorted and deduped");
    }

    #[tokio::test]
    async fn models_without_providers_reports_empty() {
        let d = test_dispatcher().await;
        let c = test_caller();
        // No registry seeding — empty providers.

        let result = d.dispatch(&[s("models")], &c).await;
        assert!(result.is_ok(), "empty models still succeeds: {}", result.message());
        assert!(
            result.message().contains("No LLM providers configured"),
            "names the empty state: {}",
            result.message()
        );
    }

    #[tokio::test]
    async fn model_reports_context_column_when_set() {
        let d = test_dispatcher().await;
        seed_registry(&d).await;
        let principal = PrincipalId::new();
        let ctx = register_context(&d, Some("c"), None, principal);
        // Give the context its own model column.
        {
            let db = d.kernel_db().lock();
            db.update_model(ctx, Some("deepseek"), Some("deepseek-r1")).unwrap();
        }
        let c = caller_with_context(ctx);

        let result = d.dispatch(&[s("model")], &c).await;
        assert!(result.is_ok(), "model failed: {}", result.message());
        let data = match result {
            crate::kj::KjResult::Ok { data: Some(d), .. } => d,
            other => panic!("expected data, got {other:?}"),
        };
        assert_eq!(data["model"], "deepseek-r1");
        assert_eq!(data["provider"], "deepseek");
        assert_eq!(data["source"], "context", "column-set → source=context");
    }

    #[tokio::test]
    async fn model_falls_back_to_registry_default() {
        let d = test_dispatcher().await;
        seed_registry(&d).await;
        let principal = PrincipalId::new();
        // Fresh context with no model column.
        let ctx = register_context(&d, Some("c"), None, principal);
        let c = caller_with_context(ctx);

        let result = d.dispatch(&[s("model")], &c).await;
        assert!(result.is_ok(), "model failed: {}", result.message());
        assert!(
            result.message().contains("registry default"),
            "message flags the fallback"
        );
        let data = match result {
            crate::kj::KjResult::Ok { data: Some(d), .. } => d,
            other => panic!("expected data, got {other:?}"),
        };
        assert_eq!(data["model"], "claude-opus-4-8", "uses registry default model");
        assert_eq!(data["provider"], "anthropic");
        assert_eq!(data["source"], "default", "no column → source=default");
    }

    #[tokio::test]
    async fn model_without_active_context_errors() {
        let d = test_dispatcher().await;
        seed_registry(&d).await;
        let c = KjCaller_no_context();

        let result = d.dispatch(&[s("model")], &c).await;
        assert!(!result.is_ok(), "no context → error");
    }

    // A caller with no active context, for the no-context error path.
    #[allow(non_snake_case)]
    fn KjCaller_no_context() -> crate::kj::KjCaller {
        let mut c = test_caller();
        c.context_id = None;
        c
    }
}
