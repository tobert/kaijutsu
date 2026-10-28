//! `kj models` — model discovery.
//!
//! A read-only verb, no capability gate (discovery is not escalation). It
//! enumerates the configured providers, each provider's known models, and the
//! friendly aliases — the specs you can hand to `kj cast slot set --model`.
//! The structured `.data` is an array of those specs (alias names +
//! fully-qualified `provider/model`) so `for m in $(kj models)` iterates usable
//! handles, per the kj list-data convention. A context's model is not here:
//! `kj context info` shows the model its slot gives it.
//!
//! The dispatch reads the LLM registry behind its async `RwLock`, so it is
//! async.

use clap::Parser;
use kaijutsu_types::ContentType;

use super::effect::{Classify, Effect};
use super::{clap_help_for, KjDispatcher, KjResult};

/// `kj models` is pure discovery — no positionals, no value flags. The empty
/// struct exists so help routes through the shared `clap_help_for` path for
/// consistency with the other clap-migrated subcommands. Note: bare
/// `kj models` (no argv) deliberately *lists* rather than showing help, so the
/// dispatch path handles the empty-argv case before clap ever sees it.
#[derive(Parser, Debug)]
#[command(
    name = "models",
    about = "List configured LLM providers, their models, and aliases",
    disable_help_subcommand = true,
    no_binary_name = true
)]
pub(crate) struct ModelsArgs {}

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

        // Providers in stable, sorted order. Each provider reports the models
        // its client knows about, enriched with the config-declared default.
        let mut provider_names: Vec<String> = registry.list().iter().map(|s| s.to_string()).collect();
        provider_names.sort();

        // `specs` accumulates every string a caller could name a model by:
        // each alias name, plus the fully-qualified `provider/model` for each
        // known model. This is the iteration-friendly `.data` payload.
        let mut specs: Vec<String> = Vec::new();

        let mut lines: Vec<String> = vec!["## Models".to_string(), String::new()];

        if provider_names.is_empty() {
            lines.push("_No LLM providers configured._".to_string());
        }

        for name in &provider_names {
            // Everything the DB knows this backend can serve (its
            // `backend_models` rows plus every model an alias points at),
            // unioned with whatever the client advertises.
            let mut models: Vec<String> = registry
                .get(name)
                .map(|p| {
                    p.available_models()
                        .iter()
                        .map(|m| m.to_string())
                        .collect::<Vec<String>>()
                })
                .unwrap_or_default();
            models.extend(registry.models_for_provider(name));
            models.sort();
            models.dedup();

            let kind = registry
                .backend_config(name)
                .map(|b| b.kind.as_str())
                .unwrap_or("?");
            lines.push(format!("### {name} _({kind})_"));
            if models.is_empty() {
                lines.push("- _(no models advertised)_".to_string());
            }
            for m in &models {
                let mut marks: Vec<String> = Vec::new();
                // Context window, when configured in the `backend_models`
                // table. Unconfigured models get no annotation (silence is
                // the "unknown" signal in this listing).
                if let Some(w) = registry.backend_config(name).and_then(|c| c.context_window(m)) {
                    marks.push(format!("{w} ctx"));
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

        // Aliases: the friendly model names, grouped after providers.
        let aliases = registry.model_aliases();
        if !aliases.is_empty() {
            let mut alias_names: Vec<&String> = aliases.keys().collect();
            alias_names.sort();
            lines.push("### aliases".to_string());
            for alias in alias_names {
                let a = &aliases[alias];
                lines.push(format!("- `{alias}` → `{}/{}`", a.backend, a.model));
                specs.push(alias.clone());
            }
            lines.push(String::new());
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
}

// Verb class: kj/effect.rs
impl Classify for ModelsArgs {
    fn effect(&self) -> Effect {
        // Pure LLM-registry discovery, no capability gate — see the module doc.
        Effect::Read
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Arc;

    use crate::kj::test_helpers::*;
    use crate::llm::ModelAlias;
    use crate::llm::{BackendConfig, BackendKind, Provider, claude, deepseek};

    fn s(v: &str) -> String {
        v.to_string()
    }

    /// Seed the test kernel's registry with two providers and an alias so
    /// the discovery verb has something to report.
    async fn seed_registry(d: &crate::kj::KjDispatcher) {
        let mut reg = d.kernel().llm().write().await;
        reg.register("anthropic", Arc::new(Provider::Claude(claude::Client::new("fake"))));
        reg.register("deepseek", Arc::new(Provider::DeepSeek(deepseek::Client::new("fake"))));
        reg.set_backends(vec![{
            let mut c = BackendConfig::new("anthropic", BackendKind::Anthropic);
            // Only "claude-opus-4-8" carries a configured window — the
            // "fast" alias below points at "claude-haiku-4-5-20251001",
            // which is deliberately left unconfigured so tests can
            // exercise the "no fabricated default" path through alias
            // resolution too.
            c.models.insert(
                "claude-opus-4-8".to_string(),
                crate::llm::ModelInfo {
                    context_window: Some(1_000_000),
                    extra: None,
                    tunables: Default::default(),
                },
            );
            c
        }]);
        let mut aliases = HashMap::new();
        aliases.insert(
            "fast".to_string(),
            ModelAlias {
                backend: "anthropic".to_string(),
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
        assert!(msg.contains("anthropic"), "lists the anthropic backend: {msg}");
        assert!(msg.contains("deepseek"), "lists the deepseek backend: {msg}");
        assert!(msg.contains("fast"), "lists the alias: {msg}");
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
        // The alias name is a usable model spec...
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
    async fn models_annotates_a_model_with_its_configured_context_window() {
        let d = test_dispatcher().await;
        seed_registry(&d).await;
        let c = test_caller();

        let result = d.dispatch(&[s("models")], &c).await;
        assert!(result.is_ok(), "models failed: {}", result.message());
        let msg = result.message();
        assert!(
            msg.contains("claude-opus-4-8") && msg.contains("1000000"),
            "listing annotates the configured model with its window: {msg}"
        );
    }
}
