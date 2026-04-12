//! TOML-driven LLM configuration.
//!
//! Parses `models.toml` into the same `ModelsConfig` / `LlmConfig` types
//! used by the Rhai parser, enabling a gradual migration.

use std::collections::HashMap;
use std::path::PathBuf;

use serde::Deserialize;

use super::config::{ProviderConfig, ToolFilter};
use super::{LlmError, LlmResult};
use super::rhai_config::{EmbeddingModelConfig, LlmConfig, ModelAlias, ModelsConfig};

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

    #[serde(default)]
    default_tools: Option<ToolFilterToml>,
}

fn default_true() -> bool {
    true
}

/// Tool filter as it appears in TOML.
#[derive(Deserialize)]
struct ToolFilterToml {
    #[serde(rename = "type")]
    filter_type: String,

    #[serde(default)]
    tools: Vec<String>,
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

    let llm = convert_llm_config(&raw);
    let embedding = convert_embedding(&raw.embedding);

    Ok(ModelsConfig { llm, embedding })
}

/// Parse a `models.toml` string into just the LLM config (no embedding).
pub fn load_llm_config_toml(content: &str) -> LlmResult<LlmConfig> {
    let raw: ModelsToml = toml::from_str(content)
        .map_err(|e| LlmError::InvalidRequest(format!("models.toml parse error: {e}")))?;

    Ok(convert_llm_config(&raw))
}

// ---------------------------------------------------------------------------
// Conversion helpers
// ---------------------------------------------------------------------------

fn convert_llm_config(raw: &ModelsToml) -> LlmConfig {
    let providers: Vec<ProviderConfig> = raw
        .providers
        .iter()
        .map(|(name, p)| {
            let mut config = ProviderConfig::new(name);
            config.enabled = p.enabled;
            config.api_key_env = p.api_key_env.clone();
            config.base_url = p.base_url.clone();
            config.default_model = p.default_model.clone();
            config.max_output_tokens = p.max_output_tokens;
            config.default_tools = p
                .default_tools
                .as_ref()
                .map(convert_tool_filter)
                .unwrap_or(ToolFilter::All);
            config
        })
        .collect();

    LlmConfig {
        default_provider: raw.default_provider.clone(),
        providers,
        model_aliases: raw.model_aliases.clone(),
    }
}

fn convert_tool_filter(tf: &ToolFilterToml) -> ToolFilter {
    match tf.filter_type.as_str() {
        "allow" => ToolFilter::allow(tf.tools.clone()),
        "deny" => ToolFilter::deny(tf.tools.clone()),
        _ => ToolFilter::All,
    }
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
        let emb = config.embedding.expect("embedding section should be present");
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
        assert_eq!(anthropic.default_tools, ToolFilter::All);

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
    fn test_tool_filter_deny() {
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
        assert_eq!(test.default_tools, ToolFilter::deny(["shell", "bash"]));
    }

    #[test]
    fn test_tool_filter_allow() {
        let toml = r#"
default_provider = "test"

[providers.test]
enabled = false

[providers.test.default_tools]
type = "allow"
tools = ["read", "write"]

[model_aliases]
"#;
        let config = load_llm_config_toml(toml).unwrap();
        let test = config
            .providers
            .iter()
            .find(|p| p.provider_type == "test")
            .unwrap();
        assert_eq!(test.default_tools, ToolFilter::allow(["read", "write"]));
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
    fn test_parity_with_rhai_defaults() {
        // Load the same config from both formats and verify structural equivalence
        let rhai_script = include_str!("../../../../assets/defaults/models.rhai");
        let rhai_config = super::super::rhai_config::load_models_config(rhai_script).unwrap();
        let toml_config = load_models_config_toml(DEFAULT_TOML).unwrap();

        // Same default provider
        assert_eq!(
            rhai_config.llm.default_provider,
            toml_config.llm.default_provider
        );

        // Same number of providers
        assert_eq!(
            rhai_config.llm.providers.len(),
            toml_config.llm.providers.len()
        );

        // Same model aliases (by key)
        assert_eq!(
            rhai_config.llm.model_aliases.len(),
            toml_config.llm.model_aliases.len()
        );
        for (key, rhai_alias) in &rhai_config.llm.model_aliases {
            let toml_alias = toml_config
                .llm
                .model_aliases
                .get(key)
                .unwrap_or_else(|| panic!("missing alias: {key}"));
            assert_eq!(rhai_alias.provider, toml_alias.provider, "alias {key}");
            assert_eq!(rhai_alias.model, toml_alias.model, "alias {key}");
        }

        // Both have embedding
        assert!(rhai_config.embedding.is_some());
        assert!(toml_config.embedding.is_some());
        let rhai_emb = rhai_config.embedding.unwrap();
        let toml_emb = toml_config.embedding.unwrap();
        assert_eq!(rhai_emb.dimensions, toml_emb.dimensions);
        assert_eq!(rhai_emb.max_tokens, toml_emb.max_tokens);
    }
}
