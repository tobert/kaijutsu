//! Rhai-driven LLM configuration.
//!
//! Evaluates `llm.rhai` scripts to extract provider configurations,
//! model aliases, and default settings. Converts the Rhai scope into
//! typed config used to populate an `LlmRegistry`.

use std::collections::HashMap;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use super::config::ProviderConfig;
use super::{LlmError, LlmRegistry, LlmResult, RigProvider};

/// Structured LLM configuration extracted from a Rhai script.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmConfig {
    /// Name of the default provider (must be present in `providers`).
    pub default_provider: String,
    /// Provider configurations keyed by name.
    pub providers: Vec<ProviderConfig>,
    /// Short names that resolve to a specific provider + model.
    pub model_aliases: HashMap<String, ModelAlias>,
}

/// A model alias maps a short name to a specific provider and model.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelAlias {
    pub provider: String,
    pub model: String,
}

/// Parse an `llm.rhai` script and extract configuration.
///
/// The script is expected to define:
/// - `default_provider` (String)
/// - `providers` (Map of provider configs)
/// - `model_aliases` (Map of alias -> {provider, model})
pub fn load_llm_config(script: &str) -> LlmResult<LlmConfig> {
    let engine = rhai::Engine::new();
    let ast = engine.compile(script).map_err(|e| {
        LlmError::InvalidRequest(format!("llm.rhai parse error: {}", e))
    })?;

    let mut scope = rhai::Scope::new();
    engine.run_ast_with_scope(&mut scope, &ast).map_err(|e| {
        LlmError::InvalidRequest(format!("llm.rhai evaluation error: {}", e))
    })?;

    // Extract default_provider
    let default_provider = scope
        .get_value::<rhai::ImmutableString>("default_provider")
        .map(|s| s.to_string())
        .unwrap_or_else(|| "anthropic".to_string());

    // Extract providers map
    let providers = extract_providers(&scope);

    // Extract model aliases
    let model_aliases = extract_aliases(&scope);

    Ok(LlmConfig {
        default_provider,
        providers,
        model_aliases,
    })
}

/// Extract provider configurations from the Rhai scope.
fn extract_providers(scope: &rhai::Scope) -> Vec<ProviderConfig> {
    let providers_map = match scope.get_value::<rhai::Map>("providers") {
        Some(map) => map,
        None => return Vec::new(),
    };

    let mut configs = Vec::new();

    for (name, value) in &providers_map {
        let name = name.to_string();
        if let Some(map) = value.clone().try_cast::<rhai::Map>() {
            let enabled = map
                .get("enabled")
                .and_then(|v| v.as_bool().ok())
                .unwrap_or(true);

            let api_key_env = map
                .get("api_key_env")
                .and_then(|v| v.clone().into_string().ok())
                .map(|s| s.to_string());

            let base_url = map
                .get("base_url")
                .and_then(|v| v.clone().into_string().ok())
                .map(|s| s.to_string());

            let default_model = map
                .get("default_model")
                .and_then(|v| v.clone().into_string().ok())
                .map(|s| s.to_string());

            let mut config = ProviderConfig::new(&name);
            config.enabled = enabled;
            config.api_key_env = api_key_env;
            config.base_url = base_url;
            config.default_model = default_model;

            configs.push(config);
        }
    }

    configs
}

/// Extract model aliases from the Rhai scope.
fn extract_aliases(scope: &rhai::Scope) -> HashMap<String, ModelAlias> {
    let aliases_map = match scope.get_value::<rhai::Map>("model_aliases") {
        Some(map) => map,
        None => return HashMap::new(),
    };

    let mut aliases = HashMap::new();

    for (name, value) in &aliases_map {
        let name = name.to_string();
        if let Some(map) = value.clone().try_cast::<rhai::Map>() {
            let provider = map
                .get("provider")
                .and_then(|v| v.clone().into_string().ok())
                .map(|s| s.to_string())
                .unwrap_or_default();

            let model = map
                .get("model")
                .and_then(|v| v.clone().into_string().ok())
                .map(|s| s.to_string())
                .unwrap_or_default();

            if !provider.is_empty() && !model.is_empty() {
                aliases.insert(name, ModelAlias { provider, model });
            }
        }
    }

    aliases
}

/// Build an `LlmRegistry` from a parsed `LlmConfig`.
///
/// Iterates providers, skips disabled ones, creates `RigProvider` instances,
/// logs warnings for missing API keys, and sets the default provider/model.
pub fn initialize_llm_registry(config: &LlmConfig) -> LlmRegistry {
    let mut registry = LlmRegistry::new();

    for provider_config in &config.providers {
        if !provider_config.enabled {
            tracing::debug!(
                provider = %provider_config.provider_type,
                "skipping disabled provider"
            );
            continue;
        }

        match RigProvider::from_config(provider_config) {
            Ok(provider) => {
                let name = provider_config.provider_type.clone();
                tracing::info!(provider = %name, "registered LLM provider");
                registry.register(&name, Arc::new(provider));
            }
            Err(e) => {
                tracing::warn!(
                    provider = %provider_config.provider_type,
                    error = %e,
                    "failed to initialize provider (missing API key?)"
                );
            }
        }
    }

    // Set default provider
    if registry.set_default(&config.default_provider) {
        // Set default model from the provider's config
        if let Some(pc) = config.providers.iter().find(|p| p.provider_type == config.default_provider) {
            if let Some(ref model) = pc.default_model {
                registry.set_default_model(model);
            }
        }
        tracing::info!(provider = %config.default_provider, "set default LLM provider");
    } else {
        // Fallback: use the first available provider
        let available: Vec<_> = registry.list().iter().map(|s| s.to_string()).collect();
        if let Some(first) = available.first() {
            registry.set_default(first);
            tracing::warn!(
                requested = %config.default_provider,
                fallback = %first,
                "default provider unavailable, using fallback"
            );
        } else {
            tracing::warn!("no LLM providers available");
        }
    }

    // Set model aliases
    registry.set_model_aliases(config.model_aliases.clone());

    registry
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEST_SCRIPT: &str = r#"
let default_provider = "anthropic";

let providers = #{
    anthropic: #{
        enabled: true,
        api_key_env: "ANTHROPIC_API_KEY",
        default_model: "claude-haiku-4-5-20251001",
    },
    gemini: #{
        enabled: false,
        api_key_env: "GEMINI_API_KEY",
        default_model: "gemini-2.0-flash",
    },
    ollama: #{
        enabled: false,
        base_url: "http://localhost:11434",
        default_model: "qwen2.5-coder:7b",
    },
};

let model_aliases = #{
    "fast": #{ provider: "anthropic", model: "claude-haiku-4-5-20251001" },
    "smart": #{ provider: "anthropic", model: "claude-opus-4-5-20251101" },
    "local": #{ provider: "ollama", model: "qwen2.5-coder:7b" },
};
"#;

    #[test]
    fn test_load_llm_config() {
        let config = load_llm_config(TEST_SCRIPT).unwrap();
        assert_eq!(config.default_provider, "anthropic");
        assert_eq!(config.providers.len(), 3);

        let anthropic = config.providers.iter().find(|p| p.provider_type == "anthropic").unwrap();
        assert!(anthropic.enabled);
        assert_eq!(anthropic.api_key_env.as_deref(), Some("ANTHROPIC_API_KEY"));
        assert_eq!(anthropic.default_model.as_deref(), Some("claude-haiku-4-5-20251001"));

        let gemini = config.providers.iter().find(|p| p.provider_type == "gemini").unwrap();
        assert!(!gemini.enabled);

        assert_eq!(config.model_aliases.len(), 3);
        let fast = &config.model_aliases["fast"];
        assert_eq!(fast.provider, "anthropic");
        assert_eq!(fast.model, "claude-haiku-4-5-20251001");
    }

    #[test]
    fn test_load_empty_script() {
        let config = load_llm_config("let x = 1;").unwrap();
        assert_eq!(config.default_provider, "anthropic"); // fallback default
        assert!(config.providers.is_empty());
        assert!(config.model_aliases.is_empty());
    }

    #[test]
    fn test_load_invalid_script() {
        let result = load_llm_config("this is not valid rhai {{{{");
        assert!(result.is_err());
    }

    #[test]
    fn test_initialize_registry_skips_disabled() {
        let config = load_llm_config(TEST_SCRIPT).unwrap();
        // Without real API keys, anthropic will fail to init too,
        // but gemini/ollama should be skipped before even trying
        let registry = initialize_llm_registry(&config);

        // The registry may or may not have anthropic depending on env
        // but it should NOT have gemini (disabled)
        assert!(registry.get("gemini").is_none());
    }

    #[test]
    fn test_default_llm_rhai_parses() {
        // Verify the actual default llm.rhai asset parses correctly
        let default_script = include_str!("../../../../assets/defaults/llm.rhai");
        let config = load_llm_config(default_script).unwrap();
        assert_eq!(config.default_provider, "anthropic");
        assert!(!config.providers.is_empty());
        assert!(!config.model_aliases.is_empty());
    }
}
