//! LLM provider configuration.
//!
//! Tool filtering was retired in Phase 5 D-54; per-context tool visibility
//! is now expressed via `ContextToolBinding` + `HookPhase::ListTools`. The
//! per-provider `default_tools` field and the `ToolConfig` type that fed
//! into it are gone.

use serde::{Deserialize, Serialize};

/// Configuration for an LLM provider.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderConfig {
    /// Provider type identifier (e.g., "anthropic", "gemini", "ollama").
    pub provider_type: String,

    /// Whether this provider is enabled.
    #[serde(default = "default_true")]
    pub enabled: bool,

    /// API key (for cloud providers).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,

    /// Environment variable name for API key (alternative to inline key).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key_env: Option<String>,

    /// Base URL override (for custom endpoints or local providers).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,

    /// Default model for this provider.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_model: Option<String>,

    /// Maximum output tokens for this provider.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<u64>,
}

fn default_true() -> bool {
    true
}

impl ProviderConfig {
    /// Create a new provider config.
    pub fn new(provider_type: impl Into<String>) -> Self {
        Self {
            provider_type: provider_type.into(),
            enabled: true,
            api_key: None,
            api_key_env: None,
            base_url: None,
            default_model: None,
            max_output_tokens: None,
        }
    }

    /// Set API key directly.
    pub fn with_api_key(mut self, key: impl Into<String>) -> Self {
        self.api_key = Some(key.into());
        self
    }

    /// Set API key from environment variable name.
    pub fn with_api_key_env(mut self, env_var: impl Into<String>) -> Self {
        self.api_key_env = Some(env_var.into());
        self
    }

    /// Set base URL.
    pub fn with_base_url(mut self, url: impl Into<String>) -> Self {
        self.base_url = Some(url.into());
        self
    }

    /// Set default model.
    pub fn with_default_model(mut self, model: impl Into<String>) -> Self {
        self.default_model = Some(model.into());
        self
    }

    /// Resolve API key from config or environment.
    pub fn resolve_api_key(&self) -> Option<String> {
        // Direct key takes precedence
        if let Some(key) = &self.api_key {
            return Some(key.clone());
        }

        // Try environment variable
        if let Some(env_var) = &self.api_key_env {
            return std::env::var(env_var).ok();
        }

        // Try standard env var for provider type
        let standard_env = match self.provider_type.as_str() {
            "anthropic" => "ANTHROPIC_API_KEY",
            "gemini" => "GEMINI_API_KEY",
            "openai" => "OPENAI_API_KEY",
            _ => return None,
        };
        std::env::var(standard_env).ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_provider_config_resolve_key() {
        // Set up test env
        // SAFETY: Single-threaded test, no other code is reading this env var concurrently
        unsafe {
            std::env::set_var("TEST_API_KEY", "test-key-from-env");
        }

        let config = ProviderConfig::new("test").with_api_key_env("TEST_API_KEY");
        assert_eq!(config.resolve_api_key(), Some("test-key-from-env".into()));

        // Direct key takes precedence
        let config = config.with_api_key("direct-key");
        assert_eq!(config.resolve_api_key(), Some("direct-key".into()));

        // SAFETY: Single-threaded test cleanup
        unsafe {
            std::env::remove_var("TEST_API_KEY");
        }
    }
}
