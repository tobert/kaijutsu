//! LLM provider configuration.
//!
//! Tool filtering was retired in Phase 5 D-54; per-context tool visibility
//! is now expressed via `ContextToolBinding` + `McpHookPhase::ListTools`. The
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

    /// Path to a file whose trimmed contents are the API key (e.g.
    /// `~/.deepseek-key`). Lets the kernel read credentials without the key
    /// living in its process environment. `~` is expanded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key_file: Option<String>,

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
            api_key_file: None,
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

    /// Set API key from a file path (trimmed contents). `~` is expanded.
    pub fn with_api_key_file(mut self, path: impl Into<String>) -> Self {
        self.api_key_file = Some(path.into());
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

    /// Resolve the API key, trying sources in order (first hit wins):
    ///
    /// 1. inline `api_key` — most explicit
    /// 2. `api_key_file` — trimmed file contents (`~` expanded). A configured
    ///    but unreadable file warns and falls through (not a silent vanish).
    /// 3. environment variable — the explicit `api_key_env` if set, otherwise
    ///    the provider type's standard var.
    ///
    /// Returns `None` when no source yields a key; the registry skips such a
    /// provider with a warning (it only hard-fails on the *default* provider).
    pub fn resolve_api_key(&self) -> Option<String> {
        // 1. Inline key.
        if let Some(key) = &self.api_key {
            return Some(key.clone());
        }

        // 2. Key file.
        if let Some(path) = &self.api_key_file {
            match read_key_file(path) {
                Ok(key) => return Some(key),
                Err(e) => tracing::warn!(
                    provider = %self.provider_type,
                    path = %path,
                    error = %e,
                    "api_key_file configured but unreadable; falling through to env"
                ),
            }
        }

        // 3. Environment variable: explicit name if given, else the standard
        //    var for this provider type. An explicit-but-unset var does not
        //    fall back to the standard var (preserves prior behavior).
        let env_var = match &self.api_key_env {
            Some(name) => name.as_str(),
            None => match self.provider_type.as_str() {
                "anthropic" => "ANTHROPIC_API_KEY",
                "gemini" => "GEMINI_API_KEY",
                "deepseek" => "DEEPSEEK_API_KEY",
                "openai" => "OPENAI_API_KEY",
                _ => return None,
            },
        };
        std::env::var(env_var).ok()
    }
}

/// Read an API key from a file: expand `~`, read, trim surrounding
/// whitespace. An empty file is an error so the caller can warn rather than
/// register a provider with a blank key.
fn read_key_file(path: &str) -> std::io::Result<String> {
    let expanded = shellexpand::tilde(path);
    let key = std::fs::read_to_string(expanded.as_ref())?.trim().to_string();
    if key.is_empty() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "key file is empty after trimming",
        ));
    }
    Ok(key)
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

    #[test]
    fn key_file_is_read_and_trimmed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("deepseek-key");
        // Trailing newline + surrounding whitespace must be stripped.
        std::fs::write(&path, "  sk-from-file\n").unwrap();

        let config =
            ProviderConfig::new("deepseek").with_api_key_file(path.to_str().unwrap());
        assert_eq!(config.resolve_api_key().as_deref(), Some("sk-from-file"));
    }

    #[test]
    fn inline_key_beats_key_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("key");
        std::fs::write(&path, "sk-from-file").unwrap();

        let config = ProviderConfig::new("deepseek")
            .with_api_key_file(path.to_str().unwrap())
            .with_api_key("sk-inline");
        assert_eq!(config.resolve_api_key().as_deref(), Some("sk-inline"));
    }

    #[test]
    fn key_file_beats_env() {
        // SAFETY: single-threaded test; unique var name avoids cross-test races.
        unsafe {
            std::env::set_var("KEYFILE_PRECEDENCE_ENV", "sk-from-env");
        }
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("key");
        std::fs::write(&path, "sk-from-file").unwrap();

        let config = ProviderConfig::new("deepseek")
            .with_api_key_env("KEYFILE_PRECEDENCE_ENV")
            .with_api_key_file(path.to_str().unwrap());
        assert_eq!(config.resolve_api_key().as_deref(), Some("sk-from-file"));

        // SAFETY: single-threaded test cleanup.
        unsafe {
            std::env::remove_var("KEYFILE_PRECEDENCE_ENV");
        }
    }

    #[test]
    fn unreadable_key_file_falls_through_to_env() {
        // SAFETY: single-threaded test; unique var name.
        unsafe {
            std::env::set_var("MISSING_FILE_FALLBACK_ENV", "sk-from-env");
        }
        let config = ProviderConfig::new("deepseek")
            .with_api_key_env("MISSING_FILE_FALLBACK_ENV")
            .with_api_key_file("/nonexistent/path/to/key");
        // A configured-but-missing file must not silently yield "no key";
        // it warns (see resolve_api_key) and falls through to the env var.
        assert_eq!(config.resolve_api_key().as_deref(), Some("sk-from-env"));

        // SAFETY: single-threaded test cleanup.
        unsafe {
            std::env::remove_var("MISSING_FILE_FALLBACK_ENV");
        }
    }

    #[test]
    fn empty_key_file_is_not_a_key() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("blank-key");
        std::fs::write(&path, "   \n\n").unwrap();

        // The helper rejects a whitespace-only file outright...
        assert!(read_key_file(path.to_str().unwrap()).is_err());

        // ...and resolve_api_key falls through to an (unset) explicit env var,
        // yielding None rather than a blank key.
        let config = ProviderConfig::new("deepseek")
            .with_api_key_file(path.to_str().unwrap())
            .with_api_key_env("DEFINITELY_UNSET_KEY_VAR_XYZ");
        assert_eq!(config.resolve_api_key(), None);
    }
}
