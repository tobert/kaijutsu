//! Hand-rolled Google Gemini provider.
//!
//! Phase 1: skeleton only. `Client::stream()` and `Client::prompt()` return
//! [`LlmError::Unavailable`] with a "not yet implemented (unrig phase 3)"
//! detail. Phase 3 lands `generateContent` streaming, tool use, and the
//! Gemini-specific built-ins (`googleSearch`, `codeExecution`,
//! `urlContext`) exposed as virtual MCP tools per `docs/unrig.md`.

use crate::llm::stream::{BuildOpts, StreamEvent};
use crate::llm::{LlmError, LlmResult, Message};

/// Google Gemini client.
///
/// Phase 1 holds only the construction inputs. Phase 3 will grow a
/// `reqwest::Client` field and `GenerateContentRequest` builder methods
/// (`with_google_search`, `with_code_execution`, `with_cached_content`).
#[derive(Clone, Debug)]
#[allow(dead_code)] // Phase 3 uses these
pub struct Client {
    /// Google AI Studio / Vertex API key.
    api_key: String,
    /// Override for the Gemini API base URL. `None` uses the default
    /// (`https://generativelanguage.googleapis.com`).
    base_url: Option<String>,
}

impl Client {
    /// Construct a client from an API key.
    pub fn new(api_key: impl Into<String>) -> Self {
        Self {
            api_key: api_key.into(),
            base_url: None,
        }
    }

    /// Override the API base URL.
    pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = Some(base_url.into());
        self
    }

    /// API key (kept private; exposed via getter for the wire layer in Phase 3).
    #[allow(dead_code)]
    pub(crate) fn api_key(&self) -> &str {
        &self.api_key
    }

    /// Effective base URL.
    #[allow(dead_code)]
    pub(crate) fn base_url(&self) -> &str {
        self.base_url
            .as_deref()
            .unwrap_or("https://generativelanguage.googleapis.com")
    }

    /// Available Gemini model IDs surfaced by this provider.
    pub fn available_models(&self) -> Vec<&'static str> {
        vec!["gemini-2.5-pro", "gemini-2.5-flash", "gemini-2.0-flash"]
    }

    /// One-shot prompt with optional system preamble.
    ///
    /// Phase 1: not implemented.
    pub async fn prompt(
        &self,
        model: &str,
        system: Option<&str>,
        prompt: &str,
    ) -> LlmResult<String> {
        let _ = (model, system, prompt);
        Err(LlmError::Unavailable(
            "gemini provider not yet implemented (unrig phase 3)".into(),
        ))
    }

    /// Start a streaming completion.
    ///
    /// Phase 1: returns the loud "not yet implemented" error before
    /// constructing a [`Stream`]. Phase 3 wires the streaming pipeline.
    pub async fn stream(
        &self,
        opts: BuildOpts,
        messages: Vec<Message>,
    ) -> LlmResult<Stream> {
        let _ = (opts, messages);
        Err(LlmError::Unavailable(
            "gemini streaming not yet implemented (unrig phase 3)".into(),
        ))
    }
}

/// Streaming response from Gemini.
///
/// Phase 1: never constructed. Phase 3 will hold the JSON-streaming
/// parser state and abort handle.
#[allow(dead_code)]
pub struct Stream {
    _phantom: std::marker::PhantomData<()>,
}

impl Stream {
    /// Poll for the next stream event. Returns `None` when the stream is
    /// exhausted (after `Done` or `Error`).
    pub async fn next_event(&mut self) -> Option<StreamEvent> {
        None
    }

    /// Abort the underlying HTTP stream.
    pub fn cancel(&self) {}
}
