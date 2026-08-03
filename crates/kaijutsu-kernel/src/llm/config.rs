//! LLM backend configuration — the in-memory shapes the DB rows resolve into.
//!
//! Model configuration is **SQL-native**: `backends` / `backend_models` /
//! `llm_defaults` / `casts` / `cast_slots` / `model_aliases` /
//! `embedding_config` in `kernel_db.rs` are the source of truth. There is no
//! TOML anywhere in this path (`models.toml` was demolished, along with the
//! CRDT doc that carried it). These types are what a row *becomes* once read,
//! and what the registry is built from.
//!
//! Two things the old `ProviderConfig` conflated and this does not:
//!
//! 1. **Name vs kind.** A backend has a free-form unique `name` (the handle
//!    you type) and a closed [`BackendKind`] (which client speaks the wire).
//!    The TOML table name *was* the type, so you could not run two Anthropic
//!    endpoints, and a local server had to be called `ollama`/`lemonade`/
//!    `local` because those strings were hardcoded provider types.
//! 2. **Key material vs key location.** There is no inline `api_key` field
//!    any more — only an env-var NAME or a file PATH. Config never holds a
//!    secret.
//!
//! Tool filtering was retired in Phase 5 D-54; per-context tool visibility is
//! expressed via `ContextToolBinding` + `McpHookPhase::ListTools`.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// BackendKind — the closed set
// ---------------------------------------------------------------------------

/// Which wire dialect a backend speaks. A **closed enum owned by code**, and
/// the one thing about a backend an operator may not invent: each variant maps
/// onto a `Provider` variant with a real client behind it.
///
/// `mock` exists only under `cfg(test)` / `feature = "test-mock"`. The SQL
/// CHECK admits the string so the test harness can seed a row; [`Self::parse`]
/// refuses it in a production build, which keeps "kind is closed" true where
/// it matters (the write path) without a second schema for tests.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BackendKind {
    /// Anthropic's native Messages API (`llm/claude/`).
    Anthropic,
    /// DeepSeek — an OpenAI-compatible preset with required auth and the V4
    /// reasoning-echo quirk (`llm/deepseek/`).
    DeepSeek,
    /// Any OpenAI-compatible `/chat/completions` server: OpenAI itself,
    /// Ollama, lemonade, llama.cpp, a proxy (`llm/openai/`). `base_url` is
    /// what distinguishes them, which is why it is required for this kind.
    OpenAi,
    /// Canned-response client for tests.
    #[cfg(any(test, feature = "test-mock"))]
    Mock,
}

/// Every kind an operator may write, in the order they're listed in errors.
/// `mock` is deliberately absent even when the feature is on: it is not a
/// thing to configure, only a thing a test harness seeds.
pub const SUPPORTED_BACKEND_KINDS: &[&str] = &["anthropic", "deepseek", "openai"];

impl BackendKind {
    /// The canonical lowercase token — what lands in the `kind` column.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Anthropic => "anthropic",
            Self::DeepSeek => "deepseek",
            Self::OpenAi => "openai",
            #[cfg(any(test, feature = "test-mock"))]
            Self::Mock => "mock",
        }
    }

    /// Parse a `kind` token. Unknown tokens fail loud, naming the closed set —
    /// never a silent fallback to some default kind, which would quietly point
    /// a typo'd backend at the wrong API.
    pub fn parse(s: &str) -> Result<Self, String> {
        match s {
            "anthropic" => Ok(Self::Anthropic),
            "deepseek" => Ok(Self::DeepSeek),
            "openai" => Ok(Self::OpenAi),
            #[cfg(any(test, feature = "test-mock"))]
            "mock" => Ok(Self::Mock),
            other => Err(unknown_backend_kind_message(other)),
        }
    }

    /// The environment variable this kind reads when a backend names neither
    /// an `api_key_env` nor an `api_key_file`.
    pub fn standard_env_var(&self) -> Option<&'static str> {
        match self {
            Self::Anthropic => Some("ANTHROPIC_API_KEY"),
            Self::DeepSeek => Some("DEEPSEEK_API_KEY"),
            Self::OpenAi => Some("OPENAI_API_KEY"),
            #[cfg(any(test, feature = "test-mock"))]
            Self::Mock => None,
        }
    }

    /// Does this kind require a `base_url`?
    ///
    /// Only `openai` does: the kind is "some OpenAI-compatible server" and
    /// which server is exactly the information `base_url` carries. Anthropic
    /// accepts one as a gateway override; DeepSeek's endpoint is fixed, so a
    /// `base_url` there is accepted-and-ignored rather than silently changing
    /// where a key goes.
    pub fn requires_base_url(&self) -> bool {
        matches!(self, Self::OpenAi)
    }
}

/// The standard "unknown kind" message, shared by every write surface so an
/// operator sees the same wording wherever the typo is caught.
pub fn unknown_backend_kind_message(name: &str) -> String {
    format!(
        "unknown backend kind '{name}' (supported: {})",
        SUPPORTED_BACKEND_KINDS.join(", ")
    )
}

// ---------------------------------------------------------------------------
// ModelInfo
// ---------------------------------------------------------------------------

/// Per-model metadata for one backend, keyed by model id.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct ModelInfo {
    /// Total context window (input + output) in tokens, if known.
    ///
    /// `None` means "not configured" — this is NOT a default. Computing a
    /// percentage against a fabricated denominator is exactly the kind of
    /// silent-wrong-answer this project rejects; callers must render an
    /// explicit "unknown" instead. The write path refuses a non-positive
    /// value outright (`kernel_db::set_backend_model`), so a stored `Some(0)`
    /// cannot happen.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_window: Option<u64>,

    /// Sparse provider-specific fields as JSON (vision pins, effort-ladder
    /// notes). Opaque here — stored and handed on, never interpreted by the
    /// registry.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extra: Option<String>,
}

// ---------------------------------------------------------------------------
// BackendConfig
// ---------------------------------------------------------------------------

/// One configured LLM endpoint, resolved from a `backends` row plus its
/// `backend_models` rows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackendConfig {
    /// Free-form unique handle — the name you type at `--model <name>/<model>`
    /// and the key this backend occupies in the registry.
    pub name: String,

    /// Which client speaks for it.
    pub kind: BackendKind,

    /// Endpoint override. Required for [`BackendKind::OpenAi`], optional for
    /// Anthropic (a gateway), ignored for DeepSeek.
    pub base_url: Option<String>,

    /// Environment variable NAME holding the API key. Never the key itself.
    pub api_key_env: Option<String>,

    /// Path to a file whose trimmed contents are the API key (e.g.
    /// `~/.deepseek-key`). Lets the kernel read credentials without them
    /// living in its process environment. `~` is expanded.
    pub api_key_file: Option<String>,

    /// Register even with no resolvable key, using a placeholder. For gateways
    /// where auth is network identity rather than a bearer token — the header
    /// still gets sent (some servers require it present) but its value is
    /// never checked downstream.
    pub key_optional: bool,

    /// Per-request timeout. Stored and exposed; wiring it into the provider
    /// clients is Track B.
    pub request_timeout_secs: Option<u64>,

    /// Per-model metadata keyed by model id. Absent entries — and entries
    /// present with no `context_window` — both resolve to `None` via
    /// [`Self::context_window`], never a guessed default.
    pub models: HashMap<String, ModelInfo>,
}

impl BackendConfig {
    /// A minimal backend: name + kind, everything else unset.
    pub fn new(name: impl Into<String>, kind: BackendKind) -> Self {
        Self {
            name: name.into(),
            kind,
            base_url: None,
            api_key_env: None,
            api_key_file: None,
            key_optional: false,
            request_timeout_secs: None,
            models: HashMap::new(),
        }
    }

    /// Builder: environment variable name for the API key.
    pub fn with_api_key_env(mut self, env_var: impl Into<String>) -> Self {
        self.api_key_env = Some(env_var.into());
        self
    }

    /// Builder: key file path (trimmed contents). `~` is expanded.
    pub fn with_api_key_file(mut self, path: impl Into<String>) -> Self {
        self.api_key_file = Some(path.into());
        self
    }

    /// Builder: endpoint override.
    pub fn with_base_url(mut self, url: impl Into<String>) -> Self {
        self.base_url = Some(url.into());
        self
    }

    /// Builder: allow registration without a resolvable API key.
    pub fn with_key_optional(mut self, key_optional: bool) -> Self {
        self.key_optional = key_optional;
        self
    }

    /// Configured context window for one of this backend's models, or `None`
    /// when not configured.
    ///
    /// Never guesses: a model absent from `models`, and a model present but
    /// with `context_window: None`, both resolve to `None`. Callers (e.g.
    /// `kj model`) must render that as an explicit "unknown" — never
    /// substitute a default, because a fabricated denominator produces a
    /// confidently wrong percentage.
    pub fn context_window(&self, model: &str) -> Option<u64> {
        self.models.get(model)?.context_window
    }

    /// Resolve the API key, trying sources in order (first hit wins):
    ///
    /// 1. `api_key_file` — trimmed file contents (`~` expanded). A configured
    ///    but unreadable file warns and falls through (not a silent vanish).
    /// 2. environment variable — the explicit `api_key_env` if set, otherwise
    ///    the kind's standard var.
    ///
    /// There is deliberately no third source: config holds no key material.
    ///
    /// Returns `None` when no source yields a key; the registry skips such a
    /// backend with a warning (it only hard-fails on the *default* backend).
    pub fn resolve_api_key(&self) -> Option<String> {
        if let Some(path) = &self.api_key_file {
            match read_key_file(path) {
                Ok(key) => return Some(key),
                Err(e) => tracing::warn!(
                    backend = %self.name,
                    path = %path,
                    error = %e,
                    "api_key_file configured but unreadable; falling through to env"
                ),
            }
        }

        // An explicit-but-unset var does not fall back to the standard var —
        // naming a variable is a statement about where the key lives, and
        // quietly reading a different one would hide the mistake.
        let env_var = match &self.api_key_env {
            Some(name) => name.as_str(),
            None => self.kind.standard_env_var()?,
        };
        std::env::var(env_var).ok()
    }

    /// Write-time structural validation, shared by every surface that stores a
    /// backend (`kj backend set`, the factory floor). Loud, never a fixup.
    pub fn validate(&self) -> Result<(), String> {
        if self.name.trim().is_empty() {
            return Err("backend name must not be empty".to_string());
        }
        if self.kind.requires_base_url() && self.base_url.as_deref().unwrap_or("").trim().is_empty()
        {
            return Err(format!(
                "backend '{}' has kind 'openai', which needs --base-url \
                 (e.g. https://api.openai.com/v1 or http://localhost:11434/v1) — \
                 the kind says 'OpenAI-compatible server', the URL says which one",
                self.name
            ));
        }
        if let Some(secs) = self.request_timeout_secs
            && secs == 0
        {
            return Err(format!(
                "backend '{}': --request-timeout must be a positive number of seconds",
                self.name
            ));
        }
        Ok(())
    }
}

/// Read an API key from a file: expand `~`, read, trim surrounding
/// whitespace. An empty file is an error so the caller can warn rather than
/// register a backend with a blank key.
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

// ---------------------------------------------------------------------------
// Aliases, tunables, casts
// ---------------------------------------------------------------------------

/// A model alias maps a short name to a specific backend + model.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ModelAlias {
    /// Backend NAME (not id) — the registry key.
    pub backend: String,
    pub model: String,
}

/// The knobs a cast slot may override and `llm_defaults` supplies a floor for.
///
/// `None` at every level means "provider default", which is a real answer, not
/// a missing one — we do not invent numbers we have not decided on.
///
/// Track A stores and resolves these; **Track B** plumbs them into provider
/// requests (`llm/stream.rs` `BuildOpts`, the per-client `build()`), which is
/// why nothing here is read by the request path yet.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SlotTunables {
    /// Maximum RESPONSE tokens. Not the context window — a different number.
    pub max_tokens: Option<u64>,
    pub temperature: Option<f64>,
    pub top_p: Option<f64>,
    /// Provider effort ladder token (`low`…`max` on the Claude 5 family).
    pub effort: Option<String>,
    pub thinking_budget: Option<u64>,
    pub thinking_style: Option<String>,
}

impl SlotTunables {
    /// Overlay `self` (the slot) onto `floor` (the `llm_defaults` row): any
    /// field the slot left `None` takes the floor's value. A slot that sets a
    /// field wins even when the floor also sets it.
    pub fn over(&self, floor: &SlotTunables) -> SlotTunables {
        SlotTunables {
            max_tokens: self.max_tokens.or(floor.max_tokens),
            temperature: self.temperature.or(floor.temperature),
            top_p: self.top_p.or(floor.top_p),
            effort: self.effort.clone().or_else(|| floor.effort.clone()),
            thinking_budget: self.thinking_budget.or(floor.thinking_budget),
            thinking_style: self
                .thinking_style
                .clone()
                .or_else(|| floor.thinking_style.clone()),
        }
    }
}

/// One cast slot with the defaults cascade already applied — what a consumer
/// asks the registry for and hands to a turn.
#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedSlot {
    /// The `context_type` this seat is for (coder, musician, mcp, …).
    pub role: String,
    /// Backend NAME.
    pub backend: String,
    pub model: String,
    /// Slot tunables with `llm_defaults` filled in underneath.
    pub tunables: SlotTunables,
    /// Future capability/loadout reference. Stored, no consumer yet.
    pub loadout: Option<String>,
    /// Sparse slot-specific JSON.
    pub extra: Option<String>,
}

/// Configuration for a local ONNX embedding model (the former `[embedding]`
/// section of models.toml, now the `embedding_config` singleton row).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct EmbeddingModelConfig {
    /// Whether embedding is enabled.
    pub enabled: bool,
    /// Directory containing model.onnx + tokenizer.json.
    pub model_dir: std::path::PathBuf,
    /// Output embedding dimensions (e.g. 384 for bge-small).
    pub dimensions: usize,
    /// Maximum input tokens (truncated beyond this).
    pub max_tokens: usize,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn context_window_resolves_configured_value() {
        let mut config = BackendConfig::new("anthropic", BackendKind::Anthropic);
        config.models.insert(
            "claude-opus-4-8".to_string(),
            ModelInfo {
                context_window: Some(1_000_000),
                extra: None,
            },
        );
        assert_eq!(config.context_window("claude-opus-4-8"), Some(1_000_000));
    }

    #[test]
    fn context_window_is_none_for_an_unlisted_model() {
        // A model absent from `models` entirely is unknown, not zero and
        // not some backend-wide default.
        let config = BackendConfig::new("anthropic", BackendKind::Anthropic);
        assert_eq!(config.context_window("claude-opus-4-8"), None);
    }

    #[test]
    fn context_window_is_none_when_entry_present_but_unset() {
        // A model that IS listed (e.g. for future metadata) but never had
        // context_window configured must still resolve to None, not 0 or a
        // guessed number — the entry existing is not itself a signal.
        let mut config = BackendConfig::new("anthropic", BackendKind::Anthropic);
        config
            .models
            .insert("claude-sonnet-4-20250514".to_string(), ModelInfo::default());
        assert_eq!(config.context_window("claude-sonnet-4-20250514"), None);
    }

    #[test]
    fn kind_parse_round_trips_and_rejects_unknown() {
        for k in SUPPORTED_BACKEND_KINDS {
            let parsed = BackendKind::parse(k).expect("supported kind parses");
            assert_eq!(parsed.as_str(), *k);
        }
        let err = BackendKind::parse("gemini").unwrap_err();
        assert!(err.contains("unknown backend kind 'gemini'"), "{err}");
        assert!(err.contains("anthropic"), "error names the closed set: {err}");
    }

    #[test]
    fn openai_kind_requires_a_base_url() {
        // "openai" means "some OpenAI-compatible server" — without a URL the
        // row does not say which one, so the write must fail loud.
        let bare = BackendConfig::new("gpt", BackendKind::OpenAi);
        let err = bare.validate().unwrap_err();
        assert!(err.contains("--base-url"), "{err}");

        let with_url = bare.clone().with_base_url("https://api.openai.com/v1");
        assert!(with_url.validate().is_ok());

        // The other kinds have real endpoints of their own.
        assert!(BackendConfig::new("anthropic", BackendKind::Anthropic).validate().is_ok());
        assert!(BackendConfig::new("deepseek", BackendKind::DeepSeek).validate().is_ok());
    }

    #[test]
    fn env_var_resolution_uses_explicit_name() {
        // SAFETY: single-threaded test; unique var name avoids cross-test races.
        unsafe {
            std::env::set_var("KAIJUTSU_TEST_EXPLICIT_ENV", "test-key-from-env");
        }
        let config = BackendConfig::new("t", BackendKind::Anthropic)
            .with_api_key_env("KAIJUTSU_TEST_EXPLICIT_ENV");
        assert_eq!(config.resolve_api_key(), Some("test-key-from-env".into()));
        // SAFETY: single-threaded test cleanup.
        unsafe {
            std::env::remove_var("KAIJUTSU_TEST_EXPLICIT_ENV");
        }
    }

    #[test]
    fn kind_standard_env_var_is_the_fallback() {
        // SAFETY: single-threaded test.
        unsafe {
            std::env::set_var("DEEPSEEK_API_KEY", "sk-standard");
        }
        // No api_key_env, no api_key_file — the kind's own var answers.
        let config = BackendConfig::new("deepseek", BackendKind::DeepSeek);
        assert_eq!(config.resolve_api_key().as_deref(), Some("sk-standard"));
        // SAFETY: single-threaded test cleanup.
        unsafe {
            std::env::remove_var("DEEPSEEK_API_KEY");
        }
    }

    #[test]
    fn key_file_is_read_and_trimmed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("deepseek-key");
        // Trailing newline + surrounding whitespace must be stripped.
        std::fs::write(&path, "  sk-from-file\n").unwrap();

        let config = BackendConfig::new("deepseek", BackendKind::DeepSeek)
            .with_api_key_file(path.to_str().unwrap());
        assert_eq!(config.resolve_api_key().as_deref(), Some("sk-from-file"));
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

        let config = BackendConfig::new("deepseek", BackendKind::DeepSeek)
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
        let config = BackendConfig::new("deepseek", BackendKind::DeepSeek)
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
        let config = BackendConfig::new("deepseek", BackendKind::DeepSeek)
            .with_api_key_file(path.to_str().unwrap())
            .with_api_key_env("DEFINITELY_UNSET_KEY_VAR_XYZ");
        assert_eq!(config.resolve_api_key(), None);
    }

    #[test]
    fn tunables_cascade_slot_over_defaults() {
        let floor = SlotTunables {
            max_tokens: Some(16384),
            temperature: Some(0.5),
            top_p: None,
            effort: Some("low".into()),
            thinking_budget: None,
            thinking_style: None,
        };
        let slot = SlotTunables {
            max_tokens: None,
            temperature: Some(1.0),
            top_p: Some(0.9),
            effort: None,
            thinking_budget: Some(4096),
            thinking_style: None,
        };
        let resolved = slot.over(&floor);
        assert_eq!(resolved.max_tokens, Some(16384), "unset slot knob takes the floor");
        assert_eq!(resolved.temperature, Some(1.0), "set slot knob wins");
        assert_eq!(resolved.top_p, Some(0.9), "slot-only knob survives");
        assert_eq!(resolved.effort.as_deref(), Some("low"), "floor-only knob survives");
        assert_eq!(resolved.thinking_budget, Some(4096));
        assert_eq!(
            resolved.thinking_style, None,
            "unset on both sides stays None — 'provider default' is a real answer"
        );
    }
}
