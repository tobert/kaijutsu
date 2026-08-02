//! Live model-capability lookup — `GET /v1/models/{id}`.
//!
//! Background: `context_window` in `models.toml` was a hand-maintained
//! per-model value, deliberately resolving to `Option<u64>` — never a
//! fabricated default (see `docs/issues.md` "Day-job coding readiness", the
//! token-accounting entry). A hand-kept table goes stale on every model
//! launch. Anthropic's Models API returns the window live as
//! **`max_input_tokens`** — note there is no `context_window` field on the
//! wire; that name is ours. The API also returns `max_tokens` (the output
//! cap — a different number) and a `capabilities` tree we don't need here.
//!
//! [`ModelCapabilitySource`] is the seam: [`HttpModelCapabilitySource`] is
//! the real implementation (one HTTP call), and tests inject a fake
//! (`FakeModelCapabilitySource`, `#[cfg(test)]`-only below) so the
//! config/cache/live-fallback precedence in [`super::Client::context_window`]
//! and `LlmRegistry::context_window_for_live` can be exercised without
//! `cargo test` ever touching the network — the lesson from a prior lane's
//! test-isolation incident (a test that accidentally spawned a real sibling
//! binary and touched its live state DB): a seam, not a flag.

use async_trait::async_trait;
use serde::Deserialize;

use crate::llm::{LlmError, LlmResult};

/// Live-source abstraction over `GET /v1/models/{id}`.
#[async_trait]
pub trait ModelCapabilitySource: Send + Sync + std::fmt::Debug {
    /// Fetch a model's context window (the wire's `max_input_tokens` — NOT
    /// `max_tokens`, which is the output cap, a different number).
    ///
    /// `Ok(None)` means the model is unknown to the API (e.g. a 404) — a
    /// real, cacheable answer, not a failure. `Err` means the lookup itself
    /// failed (network, auth, malformed response) — the caller degrades to
    /// `None` and logs; it never fabricates a window either way.
    async fn max_input_tokens(&self, model: &str) -> LlmResult<Option<u64>>;
}

/// Real implementation — hits Anthropic's Models API using the same
/// `reqwest::Client` (and therefore the same baked-in `x-api-key` /
/// `anthropic-version` headers) as the Messages API calls, so this reuses
/// the kernel's existing credential resolution rather than inventing a
/// second path.
#[derive(Clone, Debug)]
pub struct HttpModelCapabilitySource {
    http: reqwest::Client,
    base_url: String,
}

impl HttpModelCapabilitySource {
    pub fn new(http: reqwest::Client, base_url: impl Into<String>) -> Self {
        Self {
            http,
            base_url: base_url.into(),
        }
    }
}

/// Deserialize shape for `GET /v1/models/{id}`. Deliberately narrow: we only
/// read `max_input_tokens` (the context window). We do NOT define a field
/// for `max_tokens` (the output cap) or `capabilities` — there is nothing
/// to accidentally read from a field we never named. `#[serde(default)]`
/// so a response that omits the field (shouldn't happen for a real model,
/// but the wire is not our contract to enforce) resolves to `None` rather
/// than a deserialize error.
#[derive(Debug, Deserialize)]
struct ModelResponse {
    #[serde(default)]
    max_input_tokens: Option<u64>,
}

/// Pure parse step, pulled out of [`HttpModelCapabilitySource::max_input_tokens`]
/// so a unit test can assert the field selection (`max_input_tokens`, not
/// `max_tokens`) without any HTTP involved — see the wrong-field regression
/// test below.
fn parse_max_input_tokens(body: &str) -> Result<Option<u64>, serde_json::Error> {
    let parsed: ModelResponse = serde_json::from_str(body)?;
    Ok(parsed.max_input_tokens)
}

#[async_trait]
impl ModelCapabilitySource for HttpModelCapabilitySource {
    async fn max_input_tokens(&self, model: &str) -> LlmResult<Option<u64>> {
        let response = self
            .http
            .get(format!("{}/v1/models/{model}", self.base_url))
            .send()
            .await
            .map_err(super::http_error)?;

        // A 404 means "this model id doesn't exist" — a real, cacheable
        // answer (None), not an error to bubble.
        if response.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }

        let response = super::error_for_status(response).await?;
        let body = response
            .text()
            .await
            .map_err(|e| LlmError::ApiError(format!("models API response read: {e}")))?;
        parse_max_input_tokens(&body)
            .map_err(|e| LlmError::ApiError(format!("models API response JSON parse: {e}")))
    }
}

// ============================================================================
// Test-only fake — the seam's other side. Used by this module's own tests
// plus `llm/mod.rs`'s `LlmRegistry::context_window_for_live` tests and any
// `kj` dispatch test that needs a `Provider::Claude` registered without an
// unconfigured model triggering a real network call.
// ============================================================================

#[cfg(any(test, feature = "test-mock"))]
pub mod test_support {
    use super::*;
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;

    /// In-process fake — no HTTP, no process spawn, nothing that could
    /// touch a real network or a live sibling's state. Scripts per-model
    /// responses and counts calls so a test can assert a cache hit avoided
    /// a second lookup.
    #[derive(Debug)]
    pub struct FakeModelCapabilitySource {
        responses: Mutex<HashMap<String, LlmResult<Option<u64>>>>,
        default_response: LlmResult<Option<u64>>,
        calls: AtomicUsize,
    }

    impl FakeModelCapabilitySource {
        /// A fake that resolves every model to `Ok(None)` — "the live API
        /// doesn't know this model either" — unless overridden per-model via
        /// [`Self::with_response`]. The safe default for tests that don't
        /// care about the live path but must avoid a real HTTP call because
        /// a `Provider::Claude` happens to be registered.
        pub fn always_none() -> Self {
            Self {
                responses: Mutex::new(HashMap::new()),
                default_response: Ok(None),
                calls: AtomicUsize::new(0),
            }
        }

        /// Script a specific model's response.
        pub fn with_response(self, model: impl Into<String>, response: LlmResult<Option<u64>>) -> Self {
            self.responses.lock().unwrap().insert(model.into(), response);
            self
        }

        /// Number of times `max_input_tokens` has been called — used to
        /// assert a cache hit does NOT reach this fake a second time.
        pub fn call_count(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }
    }

    #[async_trait]
    impl ModelCapabilitySource for FakeModelCapabilitySource {
        async fn max_input_tokens(&self, model: &str) -> LlmResult<Option<u64>> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let responses = self.responses.lock().unwrap();
            match responses.get(model) {
                Some(Ok(w)) => Ok(*w),
                Some(Err(e)) => Err(clone_err(e)),
                None => match &self.default_response {
                    Ok(w) => Ok(*w),
                    Err(e) => Err(clone_err(e)),
                },
            }
        }
    }

    /// `LlmError` isn't `Clone` (it wraps formatted strings via
    /// `thiserror`), so scripting a repeatable `Err` response needs a
    /// manual clone of the one variant these tests use.
    fn clone_err(e: &LlmError) -> LlmError {
        match e {
            LlmError::NetworkError(s) => LlmError::NetworkError(s.clone()),
            LlmError::AuthError(s) => LlmError::AuthError(s.clone()),
            LlmError::ApiError(s) => LlmError::ApiError(s.clone()),
            LlmError::RateLimited(s) => LlmError::RateLimited(s.clone()),
            LlmError::InvalidRequest(s) => LlmError::InvalidRequest(s.clone()),
            LlmError::Unavailable(s) => LlmError::Unavailable(s.clone()),
            LlmError::CompletionError(s) => LlmError::CompletionError(s.clone()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Regression test for the exact mistake the brief calls out: grabbing
    /// `max_tokens` (the output cap) instead of `max_input_tokens` (the
    /// context window). Both fields carry deliberately distinguishable
    /// values so a field-name swap fails loudly.
    #[test]
    fn parses_max_input_tokens_not_max_tokens() {
        let body = r#"{
            "id": "claude-opus-4-8",
            "display_name": "Claude Opus 4.8",
            "max_input_tokens": 1000000,
            "max_tokens": 128000,
            "capabilities": {"image_input": {"supported": true}}
        }"#;
        let window = parse_max_input_tokens(body).expect("valid JSON");
        assert_eq!(
            window,
            Some(1_000_000),
            "must read max_input_tokens (the context window), not max_tokens (the output cap)"
        );
    }

    #[test]
    fn missing_max_input_tokens_field_is_none_not_an_error() {
        let body = r#"{"id": "some-model", "max_tokens": 8192}"#;
        let window = parse_max_input_tokens(body).expect("valid JSON, just missing the field");
        assert_eq!(window, None);
    }

    #[test]
    fn malformed_json_is_a_parse_error() {
        assert!(parse_max_input_tokens("not json").is_err());
    }

    /// Live verification against the real API. Gated behind
    /// `ANTHROPIC_API_KEY`, same convention as
    /// `claude::tests::claude_live_smoke_streams_real_response`.
    ///
    /// ```sh
    /// ANTHROPIC_API_KEY=$(< ~/.anthropic-key.txt) \
    ///   cargo test -p kaijutsu-kernel --lib models_api::tests::claude_live_lookup_matches_reality \
    ///   -- --ignored --nocapture
    /// ```
    ///
    /// **Finding from running this against the real API (2026-08-02):**
    /// `claude-sonnet-4-20250514` — the model models.toml deliberately
    /// leaves unset, and that backs the shipped `balanced`/`default` model
    /// aliases — now 404s on `GET /v1/models/{id}`. Per
    /// `shared/model-migration.md`'s deprecation table this model's
    /// retirement date was June 15, 2026; today is past that, so it has
    /// been fully retired, not merely deprecated. The live lookup can only
    /// close a gap for a model the API still knows about — it correctly,
    /// honestly resolves a *retired* model to `None` too (same as
    /// "unknown"), so this specific brief claim ("the live lookup should
    /// close it") does not hold anymore; the mechanism is doing exactly
    /// the right thing by returning `None` for a model that's gone. See
    /// `docs/issues.md` for the follow-up this surfaces (the `balanced`/
    /// `default` aliases point at a retired model).
    ///
    /// `claude-sonnet-5` is used instead to verify the mechanism itself: a
    /// model genuinely absent from models.toml (it postdates that file)
    /// but very much alive on the API, proving the live path does close
    /// real gaps.
    #[tokio::test]
    #[ignore = "requires ANTHROPIC_API_KEY; run with `cargo test --ignored claude_live`"]
    async fn claude_live_lookup_matches_reality() {
        let api_key = match std::env::var("ANTHROPIC_API_KEY") {
            Ok(k) if !k.is_empty() => k,
            _ => return,
        };
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert("x-api-key", reqwest::header::HeaderValue::from_str(&api_key).unwrap());
        headers.insert(
            "anthropic-version",
            reqwest::header::HeaderValue::from_static("2023-06-01"),
        );
        let http = reqwest::Client::builder().default_headers(headers).build().unwrap();
        let source = HttpModelCapabilitySource::new(http, "https://api.anthropic.com");

        // A model genuinely missing from models.toml (postdates it) and
        // still live — the mechanism must close this gap for real.
        let window = source
            .max_input_tokens("claude-sonnet-5")
            .await
            .expect("live models API call must succeed with a valid key");
        println!("claude-sonnet-5 live max_input_tokens: {window:?}");
        assert!(
            window.is_some(),
            "a currently-live model absent from models.toml must resolve via the live path"
        );

        // The honest-gap model named in the brief — now retired (see the
        // doc comment above). Confirms retirement resolves as a clean
        // Ok(None) (404), not an error and not a stale fabricated window.
        let sonnet_4 = source
            .max_input_tokens("claude-sonnet-4-20250514")
            .await
            .expect("a 404 must resolve as Ok(None), not an Err");
        println!("claude-sonnet-4-20250514 live max_input_tokens: {sonnet_4:?}");
        assert_eq!(
            sonnet_4, None,
            "claude-sonnet-4-20250514 has been retired — the live API 404s it, so it \
             correctly resolves to None (same honest 'unknown' models.toml already gave it)"
        );

        // Sanity check against a model NOT known to have ever existed —
        // must also be a clean Ok(None) (404), not an error.
        let unknown = source
            .max_input_tokens("definitely-not-a-real-claude-model-xyz")
            .await
            .expect("a 404 must resolve as Ok(None), not an Err");
        assert_eq!(unknown, None, "an unknown model id must resolve to None via a live 404");
    }

    #[tokio::test]
    async fn fake_source_call_count_increments() {
        use test_support::FakeModelCapabilitySource;
        let fake = FakeModelCapabilitySource::always_none()
            .with_response("claude-opus-4-8", Ok(Some(1_000_000)));
        assert_eq!(fake.call_count(), 0);
        assert_eq!(
            fake.max_input_tokens("claude-opus-4-8").await.unwrap(),
            Some(1_000_000)
        );
        assert_eq!(fake.call_count(), 1);
        assert_eq!(fake.max_input_tokens("unknown-model").await.unwrap(), None);
        assert_eq!(fake.call_count(), 2);
    }
}
