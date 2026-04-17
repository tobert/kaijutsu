//! TOML-driven LLM configuration.
//!
//! Defines the canonical config types (`LlmConfig`, `ModelsConfig`, etc.)
//! and parses `models.toml` into them.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use super::config::ProviderConfig;
use super::{LlmError, LlmRegistry, LlmResult, RigProvider};

// ---------------------------------------------------------------------------
// Canonical config types
// ---------------------------------------------------------------------------

/// Structured LLM configuration.
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

/// Full models configuration (LLM + embedding).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelsConfig {
    /// LLM provider settings.
    pub llm: LlmConfig,
    /// Embedding model settings (for semantic indexing).
    pub embedding: Option<EmbeddingModelConfig>,
}

/// Configuration for a local ONNX embedding model.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EmbeddingModelConfig {
    /// Whether embedding is enabled.
    pub enabled: bool,
    /// Directory containing model.onnx + tokenizer.json.
    pub model_dir: PathBuf,
    /// Output embedding dimensions (e.g. 384 for bge-small).
    pub dimensions: usize,
    /// Maximum input tokens (truncated beyond this).
    pub max_tokens: usize,
}

/// Build an `LlmRegistry` from a parsed `LlmConfig`.
///
/// Returns an error if `config.default_provider` does not name a provider
/// that was successfully registered (either unknown name, or registration
/// failed, e.g. missing API key). Silent fallback to some other provider
/// would hide the misconfiguration.
pub fn initialize_llm_registry(config: &LlmConfig) -> LlmResult<LlmRegistry> {
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

    if !registry.set_default(&config.default_provider) {
        let available: Vec<String> = registry.list().iter().map(|s| s.to_string()).collect();
        return Err(LlmError::InvalidRequest(format!(
            "default_provider '{}' is not registered; available providers: {:?}",
            config.default_provider, available
        )));
    }

    if let Some(pc) = config
        .providers
        .iter()
        .find(|p| p.provider_type == config.default_provider)
        && let Some(ref model) = pc.default_model
    {
        registry.set_default_model(model);
    }
    tracing::info!(provider = %config.default_provider, "set default LLM provider");

    registry.set_model_aliases(config.model_aliases.clone());
    registry.set_provider_configs(config.providers.clone());

    Ok(registry)
}

// ---------------------------------------------------------------------------
// Intermediate serde structs (TOML shape → internal types)
// ---------------------------------------------------------------------------

/// Top-level TOML shape for models.toml.
#[derive(Deserialize)]
struct ModelsToml {
    #[serde(default = "default_provider")]
    default_provider: String,

    #[serde(default)]
    providers: HashMap<String, ProviderToml>,

    #[serde(default)]
    model_aliases: HashMap<String, ModelAlias>,

    #[serde(default)]
    streaming: Option<StreamingToml>,

    #[serde(default)]
    rate_limits: Option<RateLimitsToml>,

    #[serde(default)]
    embedding: Option<EmbeddingToml>,
}

fn default_provider() -> String {
    "anthropic".into()
}

/// Per-provider TOML section. The provider name comes from the table key.
#[derive(Deserialize)]
struct ProviderToml {
    #[serde(default = "default_true")]
    enabled: bool,

    #[serde(default)]
    api_key_env: Option<String>,

    #[serde(default)]
    base_url: Option<String>,

    #[serde(default)]
    default_model: Option<String>,

    #[serde(default)]
    max_output_tokens: Option<u64>,

    /// Phase 5 D-54: retired. Deserialized and ignored for backwards
    /// compatibility with existing models.toml files that still carry a
    /// `[providers.X.default_tools]` block. New configs should omit it.
    #[serde(default)]
    default_tools: Option<toml::Value>,
}

fn default_true() -> bool {
    true
}

/// Streaming config (preserved for future use, not currently mapped).
#[derive(Deserialize)]
#[allow(dead_code)]
struct StreamingToml {
    enabled: Option<bool>,
    buffer_size: Option<u64>,
    timeout_ms: Option<u64>,
}

/// Rate limits (preserved for future use, not currently mapped).
#[derive(Deserialize)]
#[allow(dead_code)]
struct RateLimitsToml {
    requests_per_minute: Option<u64>,
    tokens_per_minute: Option<u64>,
    min_request_interval_ms: Option<u64>,
}

/// Embedding section in TOML.
#[derive(Deserialize)]
struct EmbeddingToml {
    #[serde(default)]
    enabled: bool,

    #[serde(default)]
    model_dir: Option<String>,

    #[serde(default = "default_dimensions")]
    dimensions: usize,

    #[serde(default = "default_max_tokens")]
    max_tokens: usize,
}

fn default_dimensions() -> usize {
    384
}

fn default_max_tokens() -> usize {
    512
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Parse a `models.toml` string into a `ModelsConfig`.
pub fn load_models_config_toml(content: &str) -> LlmResult<ModelsConfig> {
    let raw: ModelsToml = toml::from_str(content)
        .map_err(|e| LlmError::InvalidRequest(format!("models.toml parse error: {e}")))?;

    let llm = convert_llm_config(&raw)?;
    let embedding = convert_embedding(&raw.embedding);

    Ok(ModelsConfig { llm, embedding })
}

/// Parse a `models.toml` string into just the LLM config (no embedding).
pub fn load_llm_config_toml(content: &str) -> LlmResult<LlmConfig> {
    let raw: ModelsToml = toml::from_str(content)
        .map_err(|e| LlmError::InvalidRequest(format!("models.toml parse error: {e}")))?;

    convert_llm_config(&raw)
}

// ---------------------------------------------------------------------------
// Conversion helpers
// ---------------------------------------------------------------------------

fn convert_llm_config(raw: &ModelsToml) -> LlmResult<LlmConfig> {
    let mut providers: Vec<ProviderConfig> = Vec::with_capacity(raw.providers.len());
    for (name, p) in &raw.providers {
        let mut config = ProviderConfig::new(name);
        config.enabled = p.enabled;
        config.api_key_env = p.api_key_env.clone();
        config.base_url = p.base_url.clone();
        config.default_model = p.default_model.clone();
        config.max_output_tokens = p.max_output_tokens;
        // Phase 5 D-54: any `default_tools` block in TOML is silently
        // dropped; tool visibility is now managed by the broker's
        // `ContextToolBinding` + `HookPhase::ListTools`.
        let _ = &p.default_tools;
        providers.push(config);
    }

    Ok(LlmConfig {
        default_provider: raw.default_provider.clone(),
        providers,
        model_aliases: raw.model_aliases.clone(),
    })
}

fn convert_embedding(raw: &Option<EmbeddingToml>) -> Option<EmbeddingModelConfig> {
    let emb = raw.as_ref()?;
    if !emb.enabled {
        return None;
    }

    let model_dir = emb.model_dir.as_ref().map(|s| {
        let expanded = shellexpand::tilde(s);
        PathBuf::from(expanded.as_ref())
    })?;

    Some(EmbeddingModelConfig {
        enabled: true,
        model_dir,
        dimensions: emb.dimensions,
        max_tokens: emb.max_tokens,
    })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    const DEFAULT_TOML: &str = include_str!("../../../../assets/defaults/models.toml");

    #[test]
    fn test_default_models_toml_parses() {
        let config = load_models_config_toml(DEFAULT_TOML).unwrap();
        assert_eq!(config.llm.default_provider, "anthropic");
        assert!(!config.llm.providers.is_empty());
        assert!(!config.llm.model_aliases.is_empty());

        // Embedding config should be present and enabled
        let emb = config
            .embedding
            .expect("embedding section should be present");
        assert!(emb.enabled);
        assert_eq!(emb.dimensions, 384);
        assert_eq!(emb.max_tokens, 512);
        assert!(emb.model_dir.to_str().unwrap().contains("bge-small"));
    }

    #[test]
    fn test_provider_fields() {
        let config = load_models_config_toml(DEFAULT_TOML).unwrap();

        let anthropic = config
            .llm
            .providers
            .iter()
            .find(|p| p.provider_type == "anthropic")
            .unwrap();
        assert!(anthropic.enabled);
        assert_eq!(anthropic.api_key_env.as_deref(), Some("ANTHROPIC_API_KEY"));
        assert_eq!(
            anthropic.default_model.as_deref(),
            Some("claude-haiku-4-5-20251001")
        );
        assert_eq!(anthropic.max_output_tokens, Some(8192));

        let gemini = config
            .llm
            .providers
            .iter()
            .find(|p| p.provider_type == "gemini")
            .unwrap();
        assert!(!gemini.enabled);
    }

    #[test]
    fn test_model_aliases() {
        let config = load_models_config_toml(DEFAULT_TOML).unwrap();
        let fast = &config.llm.model_aliases["fast"];
        assert_eq!(fast.provider, "anthropic");
        assert_eq!(fast.model, "claude-haiku-4-5-20251001");

        let local = &config.llm.model_aliases["local"];
        assert_eq!(local.provider, "ollama");
    }

    #[test]
    fn test_tool_filter_blocks_are_ignored() {
        // Phase 5 D-54: legacy [providers.*.default_tools] blocks are
        // deserialized and silently dropped so existing configs keep
        // parsing.
        let toml = r#"
default_provider = "test"

[providers.test]
enabled = true

[providers.test.default_tools]
type = "deny"
tools = ["shell", "bash"]

[model_aliases]
"#;
        let config = load_llm_config_toml(toml).unwrap();
        let test = config
            .providers
            .iter()
            .find(|p| p.provider_type == "test")
            .unwrap();
        assert_eq!(test.provider_type, "test");
        assert!(test.enabled);
    }

    #[test]
    fn test_empty_toml() {
        let config = load_llm_config_toml("").unwrap();
        assert_eq!(config.default_provider, "anthropic"); // default
        assert!(config.providers.is_empty());
        assert!(config.model_aliases.is_empty());
    }

    #[test]
    fn test_embedding_disabled() {
        let toml = r#"
[embedding]
enabled = false
model_dir = "/tmp/model"
"#;
        let config = load_models_config_toml(toml).unwrap();
        assert!(config.embedding.is_none());
    }

    #[test]
    fn test_embedding_missing() {
        let config = load_models_config_toml("").unwrap();
        assert!(config.embedding.is_none());
    }

    #[test]
    fn test_initialize_registry_skips_disabled() {
        // Only works if the ANTHROPIC_API_KEY env var is present (default
        // provider is "anthropic"). Skip if not set — this is a smoke test
        // for the disabled-provider filtering logic, not for registry
        // initialization error paths.
        if std::env::var("ANTHROPIC_API_KEY").is_err() {
            return;
        }
        let config = load_models_config_toml(DEFAULT_TOML).unwrap();
        let registry = initialize_llm_registry(&config.llm).unwrap();
        // Disabled providers (gemini, openai) should not be registered
        assert!(registry.get("gemini").is_none());
        assert!(registry.get("openai").is_none());
    }

    #[test]
    fn test_initialize_registry_errors_on_unknown_default_provider() {
        let toml = r#"
default_provider = "nonexistent"

[providers.ollama]
enabled = true
base_url = "http://localhost:11434"
default_model = "llama3"

[providers.ollama.default_tools]
type = "all"

[model_aliases]
"#;
        let config = load_llm_config_toml(toml).unwrap();
        let err = initialize_llm_registry(&config).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("nonexistent"),
            "error should name the missing provider: {msg}"
        );
    }

}
