//! ChatGPT plan provider: a preset over the OpenAI client that speaks
//! `/v1/responses` on a signed-in ChatGPT plan.
//!
//! OpenAI's "Sign in with ChatGPT" plan usage lets an open-source app call
//! the Responses API on a Plus or Pro subscriber's plan. A `chatgpt` backend
//! carries no API key: `kj backend login <name>` signs it in
//! ([`credentials`], [`oauth`]), and each request takes a current access
//! token from the kernel's [`credentials::Credentials`]. A token close to
//! expiry is refreshed before the request; a 401 refreshes it and sends the
//! request once more.
//!
//! The wire is `crate::llm::openai::responses`. The base URL is
//! `https://api.openai.com/v1`, the `resource` the tokens are issued for.

pub mod credentials;
pub mod oauth;

use std::sync::Arc;

use super::openai;
use crate::llm::stream::BuildOpts;
use crate::llm::{LlmResult, Message};

use self::credentials::Credentials;

/// A `chatgpt` backend's client.
#[derive(Clone, Debug)]
pub struct Client {
    base: openai::Client,
    backend: String,
    credentials: Arc<Credentials>,
}

impl Client {
    /// The client for backend `name`, taking tokens from `credentials`.
    pub fn new(name: impl Into<String>, credentials: Arc<Credentials>) -> Self {
        let backend = name.into();
        Self {
            base: openai::Client::new(backend.clone()).with_base_url(oauth::RESOURCE),
            backend,
            credentials,
        }
    }

    /// Override the API base URL (a test server).
    pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base = self.base.with_base_url(base_url);
        self
    }

    /// Set the per-request HTTP timeout.
    pub fn with_request_timeout(mut self, timeout: std::time::Duration) -> Self {
        self.base = self.base.with_request_timeout(timeout);
        self
    }

    /// Take a slot from `endpoint` before each request.
    pub fn with_endpoint(mut self, endpoint: Arc<crate::llm::endpoint::Endpoint>) -> Self {
        self.base = self.base.with_endpoint(endpoint);
        self
    }

    pub fn endpoint(&self) -> Option<&Arc<crate::llm::endpoint::Endpoint>> {
        self.base.endpoint()
    }

    /// The backend name, which labels the provider.
    pub fn provider_name(&self) -> &str {
        self.base.provider_name()
    }

    /// Start a streaming response, refreshing the token and sending once
    /// more if the first token is refused.
    pub async fn stream(&self, opts: BuildOpts, messages: Vec<Message>) -> LlmResult<openai::responses::Stream> {
        let token = self.credentials.bearer(&self.backend, None).await?;
        match self.base.stream_responses(opts.clone(), messages.clone(), &token).await {
            Ok(stream) => Ok(stream),
            Err(openai::responses::OpenError::Unauthorized(_)) => {
                let token = self.credentials.bearer(&self.backend, Some(&token)).await?;
                self.base.stream_responses(opts, messages, &token).await.map_err(|e| e.into_llm())
            }
            Err(e) => Err(e.into_llm()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::LlmError;
    use crate::llm::stream::StreamEvent;
    use credentials::Grant;
    use kaijutsu_council::test_server::{reply, serve_with, sse};

    const COMPLETED: &str = "data: {\"type\":\"response.output_item.added\",\"item\":{\"type\":\"message\"}}\n\n\
data: {\"type\":\"response.output_text.delta\",\"delta\":\"ok\"}\n\n\
data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"message\"}}\n\n\
data: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":1,\"output_tokens\":1}}}\n\n";

    fn signed_in(dir: &std::path::Path, token_url: String, access: &str) -> Arc<Credentials> {
        let creds = Credentials::at(dir);
        let grant = Grant {
            client_id: "oaiapp_1".into(),
            token_url,
            access_token: access.into(),
            refresh_token: "r0".into(),
            expires_at: credentials::now() + 3000,
            account: None,
            scope: None,
        };
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(dir.join("gpt.chatgpt.json"), serde_json::to_vec(&grant).unwrap()).unwrap();
        Arc::new(creds)
    }

    /// A refused token is refreshed and the request sent once more with the
    /// new bearer; the body follows the plan contract.
    ///
    /// Falsified by a second request with the old token, or a request that
    /// is not sent to `/responses`.
    #[tokio::test]
    async fn a_401_refreshes_and_resends_once() {
        let tmp = tempfile::tempdir().unwrap();
        let (base, seen) = serve_with(|req| match (req.path.as_str(), req.header("authorization")) {
            ("/token", _) => reply(200, r#"{"access_token":"fresh","refresh_token":"r1","expires_in":3600}"#),
            ("/v1/responses", Some("Bearer fresh")) => sse(COMPLETED),
            _ => reply(401, r#"{"detail":"token expired"}"#),
        })
        .await;
        let creds = signed_in(tmp.path(), format!("{base}/token"), "stale");
        let client = Client::new("gpt", creds).with_base_url(format!("{base}/v1"));
        let mut stream = client.stream(BuildOpts::new("gpt-6.1-sol", 1).with_system("s"), vec![Message::user("hi")]).await.unwrap();
        let mut events = Vec::new();
        while let Some(ev) = stream.next_event().await {
            events.push(ev);
        }
        assert!(matches!(events.last(), Some(StreamEvent::Done { .. })), "{events:?}");
        let seen = seen.lock().unwrap();
        let paths: Vec<_> = seen.iter().map(|r| (r.path.as_str(), r.header("authorization"))).collect();
        assert_eq!(paths, [("/v1/responses", Some("Bearer stale")), ("/token", None), ("/v1/responses", Some("Bearer fresh"))]);
        let body: serde_json::Value = serde_json::from_str(&seen[2].body).unwrap();
        assert_eq!((body["store"].clone(), body["instructions"].clone()), (serde_json::json!(false), serde_json::json!("s")));
    }

    /// The endpoint slot is free once `Done` is read: the runtime stops
    /// reading there and runs tools with the stream still in scope.
    ///
    /// Falsified by a request still in flight after `Done`.
    #[tokio::test]
    async fn the_slot_is_free_at_done() {
        let tmp = tempfile::tempdir().unwrap();
        let (base, _) = serve_with(|_| sse(COMPLETED)).await;
        let creds = signed_in(tmp.path(), format!("{base}/token"), "tok");
        let endpoint = crate::llm::endpoint::Endpoints::default().for_url(&base).unwrap();
        let client = Client::new("gpt", creds).with_base_url(format!("{base}/v1")).with_endpoint(endpoint.clone());
        let mut stream = client.stream(BuildOpts::new("m", 1), vec![Message::user("hi")]).await.unwrap();
        while let Some(ev) = stream.next_event().await {
            if matches!(ev, StreamEvent::Done { .. }) {
                break;
            }
        }
        assert_eq!(endpoint.status().in_flight, 0);
    }

    /// A provider error that echoes the bearer does not carry it on.
    #[tokio::test]
    async fn an_echoed_bearer_is_redacted() {
        let tmp = tempfile::tempdir().unwrap();
        let (base, _) = serve_with(|_| reply(403, r#"{"detail":"refused sk-plan-secret"}"#)).await;
        let creds = signed_in(tmp.path(), format!("{base}/token"), "sk-plan-secret");
        let client = Client::new("gpt", creds).with_base_url(format!("{base}/v1"));
        let err = client.stream(BuildOpts::new("m", 1), vec![Message::user("hi")]).await.err().unwrap();
        assert!(!err.to_string().contains("sk-plan-secret") && err.to_string().contains("[redacted]"), "{err}");
    }

    /// Without a sign-in nothing is sent and the error names the command.
    #[tokio::test]
    async fn no_sign_in_sends_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let (base, seen) = serve_with(|_| reply(200, "{}")).await;
        let client = Client::new("gpt", Arc::new(Credentials::at(tmp.path()))).with_base_url(base);
        let err = client.stream(BuildOpts::new("m", 1), vec![Message::user("hi")]).await.err().unwrap();
        assert!(matches!(&err, LlmError::AuthError(m) if m.contains("kj backend login gpt")), "{err:?}");
        assert!(seen.lock().unwrap().is_empty());
    }
}
