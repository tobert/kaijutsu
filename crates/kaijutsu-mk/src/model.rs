//! `GET /mk/v1/model` and `POST /mk/v1/render` (`docs/mk.md`).
//!
//! The client pins nothing about the server. A caller reads [`Model`] and
//! decides which identity it accepts.

use std::time::Duration;

use reqwest::Method;
use serde::{Deserialize, Serialize};

use crate::client::{to_body, Family, MkClient, MkError};
use crate::generate::{Message, ReasoningEffort, Tool};

/// The weights a reply came from.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct Identity {
    pub id: String,
    /// A hash naming the weight file set.
    pub weight_hash: String,
}

/// The model and build the service runs.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct Model {
    pub id: String,
    pub weight_hash: String,
    pub hidden_size: u32,
    pub vocab_size: u32,
    /// The most tokens one context holds, prompt and reply together.
    pub max_context: u32,
    pub build: Build,
    pub device: Device,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct Build {
    pub commit: String,
    #[serde(default)]
    pub dirty: Option<bool>,
    pub engine_sha256: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct Device {
    pub name: String,
    pub arch: String,
}

/// Renders `messages` and `tools` the way `generate` would, without
/// generating.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct RenderRequest {
    pub messages: Vec<Message>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<Tool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thinking: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<ReasoningEffort>,
    /// Absent is true: the text ends where the next assistant turn starts.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub generation_prompt: Option<bool>,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct RenderResponse {
    /// For reading only: sent back as a prompt, special-token text in the
    /// content would parse as control tokens.
    pub text: String,
    pub tokens: Vec<u32>,
    pub n_tokens: u32,
}

impl MkClient {
    /// `GET /mk/v1/model`.
    pub async fn model(&self) -> Result<Model, MkError> {
        self.json(Family::Mk, Method::GET, "/mk/v1/model", None, &[], self.timeout, 200, "model").await
    }

    /// `POST /mk/v1/render`. `timeout` bounds the call; rendering a long
    /// conversation tokenizes all of it.
    pub async fn render(&self, request: &RenderRequest, timeout: Duration) -> Result<RenderResponse, MkError> {
        if request.messages.is_empty() {
            return Err(MkError::Request("render messages is empty".into()));
        }
        crate::generate::validate_tools(&request.tools)?;
        let body = to_body(request)?;
        self.json(Family::Mk, Method::POST, "/mk/v1/render", Some(body), &[], timeout, 200, "render").await
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::test_server::{reply, serve};

    const MODEL: &str = include_str!("../tests/fixtures/mk_model.json");

    #[tokio::test]
    async fn model_decodes_the_recorded_identity() {
        let (base, seen) = serve(vec![reply(200, MODEL)]).await;
        let m = MkClient::new(&base, Duration::from_secs(5)).unwrap().model().await.unwrap();
        assert_eq!(m.max_context, 32768);
        assert_eq!(m.weight_hash.len(), 64);
        let seen = seen.lock().unwrap();
        assert_eq!((seen[0].method.as_str(), seen[0].path.as_str()), ("GET", "/mk/v1/model"));
    }

    #[tokio::test]
    async fn render_sends_messages_and_returns_the_count() {
        let (base, seen) = serve(vec![reply(200, json!({"text": "t", "tokens": [1, 2, 3], "n_tokens": 3}).to_string())]).await;
        let c = MkClient::new(&base, Duration::from_secs(5)).unwrap();
        let req = RenderRequest {
            messages: vec![Message::system("s"), Message::user("u")],
            tools: vec![],
            thinking: Some(false),
            reasoning_effort: None,
            generation_prompt: None,
        };
        assert_eq!(c.render(&req, Duration::from_secs(5)).await.unwrap().n_tokens, 3);
        let body: serde_json::Value = serde_json::from_str(&seen.lock().unwrap()[0].body).unwrap();
        assert_eq!(
            body,
            json!({"messages": [{"role": "system", "content": "s"}, {"role": "user", "content": "u"}], "thinking": false})
        );
        let empty = RenderRequest { messages: vec![], ..req };
        assert!(matches!(c.render(&empty, Duration::from_secs(5)).await, Err(MkError::Request(_))));
    }
}
