//! LLM provider configuration and tool filtering.
//!
//! This module provides configuration types for multi-provider LLM support
//! and per-context tool filtering.

use serde::{Deserialize, Serialize};
use std::collections::HashSet;

/// Configuration for an LLM provider.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderConfig {
    /// Provider type identifier (e.g., "anthropic", "gemini", "ollama").
    pub provider_type: String,

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

    /// Default tool filter for this provider.
    #[serde(default)]
    pub default_tools: ToolFilter,
}

impl ProviderConfig {
    /// Create a new provider config.
    pub fn new(provider_type: impl Into<String>) -> Self {
        Self {
            provider_type: provider_type.into(),
            api_key: None,
            api_key_env: None,
            base_url: None,
            default_model: None,
            default_tools: ToolFilter::All,
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

    /// Set default tool filter.
    pub fn with_tool_filter(mut self, filter: ToolFilter) -> Self {
        self.default_tools = filter;
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

/// Filter for which tools are available in a context.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", content = "tools")]
pub enum ToolFilter {
    /// All registered tools are available.
    #[default]
    All,

    /// Only these specific tools are available.
    AllowList(HashSet<String>),

    /// All tools except these are available.
    DenyList(HashSet<String>),
}

impl ToolFilter {
    /// Create an allow list filter.
    pub fn allow<I, S>(tools: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self::AllowList(tools.into_iter().map(Into::into).collect())
    }

    /// Create a deny list filter.
    pub fn deny<I, S>(tools: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self::DenyList(tools.into_iter().map(Into::into).collect())
    }

    /// Check if a tool is allowed by this filter.
    pub fn allows(&self, tool_name: &str) -> bool {
        match self {
            Self::All => true,
            Self::AllowList(allowed) => allowed.contains(tool_name),
            Self::DenyList(denied) => !denied.contains(tool_name),
        }
    }

    /// Merge with another filter (intersection of allowed tools).
    pub fn merge(&self, other: &Self) -> Self {
        match (self, other) {
            // All + anything = the other filter
            (Self::All, other) => other.clone(),
            (this, Self::All) => this.clone(),

            // AllowList + AllowList = intersection
            (Self::AllowList(a), Self::AllowList(b)) => {
                Self::AllowList(a.intersection(b).cloned().collect())
            }

            // DenyList + DenyList = union of denied
            (Self::DenyList(a), Self::DenyList(b)) => {
                Self::DenyList(a.union(b).cloned().collect())
            }

            // AllowList + DenyList = allow list minus denied
            (Self::AllowList(allowed), Self::DenyList(denied)) => {
                Self::AllowList(allowed.difference(denied).cloned().collect())
            }
            (Self::DenyList(denied), Self::AllowList(allowed)) => {
                Self::AllowList(allowed.difference(denied).cloned().collect())
            }
        }
    }
}

/// Tool configuration for a kernel/context.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ToolConfig {
    /// Filter determining which tools are available.
    pub filter: ToolFilter,
}

impl ToolConfig {
    /// Create a new tool config with the given filter.
    pub fn new(filter: ToolFilter) -> Self {
        Self { filter }
    }

    /// Create a config allowing all tools.
    pub fn all() -> Self {
        Self {
            filter: ToolFilter::All,
        }
    }

    /// Check if a tool is allowed.
    pub fn allows(&self, tool_name: &str) -> bool {
        self.filter.allows(tool_name)
    }
}

/// Tracking for context model transitions.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextSegment {
    /// Document ID for this segment.
    pub doc_id: String,

    /// Provider used for this segment.
    pub provider: String,

    /// Model used for this segment.
    pub model: String,

    /// Parent document ID (if forked from another context).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_doc_id: Option<String>,
}

impl ContextSegment {
    /// Create a new context segment.
    pub fn new(
        doc_id: impl Into<String>,
        provider: impl Into<String>,
        model: impl Into<String>,
    ) -> Self {
        Self {
            doc_id: doc_id.into(),
            provider: provider.into(),
            model: model.into(),
            parent_doc_id: None,
        }
    }

    /// Create a forked context segment.
    pub fn forked(
        doc_id: impl Into<String>,
        provider: impl Into<String>,
        model: impl Into<String>,
        parent_doc_id: impl Into<String>,
    ) -> Self {
        Self {
            doc_id: doc_id.into(),
            provider: provider.into(),
            model: model.into(),
            parent_doc_id: Some(parent_doc_id.into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_tool_filter_all() {
        let filter = ToolFilter::All;
        assert!(filter.allows("anything"));
        assert!(filter.allows("bash"));
        assert!(filter.allows("read"));
    }

    #[test]
    fn test_tool_filter_allow_list() {
        let filter = ToolFilter::allow(["bash", "read", "write"]);
        assert!(filter.allows("bash"));
        assert!(filter.allows("read"));
        assert!(!filter.allows("edit"));
        assert!(!filter.allows("unknown"));
    }

    #[test]
    fn test_tool_filter_deny_list() {
        let filter = ToolFilter::deny(["bash", "dangerous_tool"]);
        assert!(!filter.allows("bash"));
        assert!(!filter.allows("dangerous_tool"));
        assert!(filter.allows("read"));
        assert!(filter.allows("write"));
    }

    #[test]
    fn test_tool_filter_merge() {
        // All + AllowList = AllowList
        let all = ToolFilter::All;
        let allow = ToolFilter::allow(["bash", "read"]);
        assert_eq!(all.merge(&allow), allow);

        // AllowList + AllowList = intersection
        let allow1 = ToolFilter::allow(["bash", "read", "write"]);
        let allow2 = ToolFilter::allow(["read", "write", "edit"]);
        let merged = allow1.merge(&allow2);
        match merged {
            ToolFilter::AllowList(set) => {
                assert!(set.contains("read"));
                assert!(set.contains("write"));
                assert!(!set.contains("bash"));
                assert!(!set.contains("edit"));
            }
            _ => panic!("Expected AllowList"),
        }

        // AllowList + DenyList = allow list minus denied
        let allow = ToolFilter::allow(["bash", "read", "write"]);
        let deny = ToolFilter::deny(["bash"]);
        let merged = allow.merge(&deny);
        match merged {
            ToolFilter::AllowList(set) => {
                assert!(!set.contains("bash"));
                assert!(set.contains("read"));
                assert!(set.contains("write"));
            }
            _ => panic!("Expected AllowList"),
        }
    }

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
    fn test_context_segment() {
        let segment = ContextSegment::new("doc-1", "anthropic", "claude-sonnet-4");
        assert_eq!(segment.doc_id, "doc-1");
        assert!(segment.parent_doc_id.is_none());

        let forked = ContextSegment::forked("doc-2", "gemini", "gemini-2.0-pro", "doc-1");
        assert_eq!(forked.parent_doc_id, Some("doc-1".into()));
    }
}
