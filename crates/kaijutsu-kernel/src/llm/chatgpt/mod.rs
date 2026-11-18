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
//! A request inside a turn goes on its context's Responses WebSocket and
//! continues the last response there when it can
//! (`openai::responses::ws`); `KJ_CHATGPT_WEBSOCKET=0` in the kernel's
//! environment sends every request over HTTP instead.

pub mod credentials;
pub mod oauth;

use std::sync::Arc;

use super::openai;
use crate::llm::stream::BuildOpts;
use crate::llm::{LlmResult, Message};

use self::credentials::Credentials;

/// How a `chatgpt` backend reaches the Responses API.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Transport {
    /// A WebSocket per context, continuing the last response where it can,
    /// with HTTP when the upgrade fails (`openai::responses::ws`).
    WebSocket,
    /// One HTTP POST with an SSE reply per request.
    Http,
}

impl Transport {
    /// The environment variable that selects HTTP when set to `0`.
    pub const ENV: &str = "KJ_CHATGPT_WEBSOCKET";

    /// [`Self::Http`] when [`Self::ENV`] is `0`, else [`Self::WebSocket`].
    pub fn from_env() -> Self {
        match std::env::var(Self::ENV) {
            Ok(value) if value.trim() == "0" => Self::Http,
            _ => Self::WebSocket,
        }
    }
}

/// A `chatgpt` backend's client.
#[derive(Clone, Debug)]
pub struct Client {
    base: openai::Client,
    backend: String,
    credentials: Arc<Credentials>,
    transport: Transport,
    /// Idle WebSockets by context, shared by the client's clones.
    websockets: Arc<openai::responses::ws::Pool>,
}

impl Client {
    /// The client for backend `name`, taking tokens from `credentials`.
    pub fn new(name: impl Into<String>, credentials: Arc<Credentials>) -> Self {
        let backend = name.into();
        Self {
            base: openai::Client::new(backend.clone()).with_base_url(oauth::RESOURCE),
            backend,
            credentials,
            transport: Transport::WebSocket,
            websockets: Arc::default(),
        }
    }

    /// Reach the API over `transport`.
    pub fn with_transport(mut self, transport: Transport) -> Self {
        self.transport = transport;
        self
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
        let websockets = (self.transport == Transport::WebSocket).then_some(&self.websockets);
        match self.base.stream_responses(opts.clone(), messages.clone(), &token, websockets).await {
            Ok(stream) => Ok(stream),
            Err(openai::responses::OpenError::Unauthorized(_)) => {
                let token = self.credentials.bearer(&self.backend, Some(&token)).await?;
                self.base.stream_responses(opts, messages, &token, websockets).await.map_err(|e| e.into_llm())
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
        assert_eq!(body["store"], serde_json::json!(false));
        assert_eq!(body["input"][0]["role"], "developer");
        assert_eq!(body["input"][0]["content"][0]["text"], "s");
    }

    /// A turn's request carries the context id as both the `session-id`
    /// header and `prompt_cache_key`: the plan route takes cache affinity
    /// from that header, and without it requests scatter across machines
    /// and rarely hit the cache.
    ///
    /// Falsified by a request with no `session-id`, or one whose key differs.
    #[tokio::test]
    async fn a_request_names_its_context_for_cache_affinity() {
        let tmp = tempfile::tempdir().unwrap();
        let (base, seen) = serve_with(|_| sse(COMPLETED)).await;
        let creds = signed_in(tmp.path(), format!("{base}/token"), "tok");
        let client = Client::new("gpt", creds).with_base_url(format!("{base}/v1")).with_transport(Transport::Http);
        let context = kaijutsu_types::ContextId::new();
        let trace = crate::llm::request_dump::RequestTrace::new(context, kaijutsu_types::TurnId::new(), 1, None);
        let mut stream = client.stream(BuildOpts::new("m", 1).with_trace(trace), vec![Message::user("hi")]).await.unwrap();
        while stream.next_event().await.is_some() {}
        let seen = seen.lock().unwrap();
        let body: serde_json::Value = serde_json::from_str(&seen[0].body).unwrap();
        assert_eq!(seen[0].header("session-id"), Some(context.to_string().as_str()));
        assert_eq!(body["prompt_cache_key"], context.to_string());
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

    mod websocket {
        use super::*;
        use crate::llm::openai::responses::build::REASONING_SIGNATURE_PREFIX;
        use crate::llm::openai::responses::ws::test_server::{Answer, Ask, serve};
        use crate::llm::request_dump::RequestTrace;
        use crate::llm::{ContentBlock, MessageContent, Role};
        use kaijutsu_types::{ContextId, TurnId};
        use serde_json::{Value, json};

        /// The events of a response `id` that sends `items`.
        fn response(id: &str, items: Vec<Value>) -> Vec<Value> {
            let mut events = vec![json!({ "type": "response.created", "response": { "id": id } })];
            for item in items {
                events.push(json!({ "type": "response.output_item.added", "item": { "type": item["type"] } }));
                if item["type"] == "message" {
                    events.push(json!({ "type": "response.output_text.delta", "delta": item["content"][0]["text"] }));
                }
                events.push(json!({ "type": "response.output_item.done", "item": item }));
            }
            events.push(json!({ "type": "response.completed", "response": { "id": id, "usage": { "input_tokens": 9, "output_tokens": 1 } } }));
            events
        }

        fn call(id: &str) -> Value {
            json!({ "type": "function_call", "id": "fc", "status": "completed", "call_id": id, "name": "ls", "arguments": "{}" })
        }

        fn reasoning(enc: &str) -> Value {
            json!({ "type": "reasoning", "id": "rs", "summary": [], "encrypted_content": enc })
        }

        fn text(text: &str) -> Value {
            json!({ "type": "message", "id": "msg", "status": "completed", "role": "assistant", "content": [{ "type": "output_text", "text": text }] })
        }

        /// The history after a response that reasoned and called `ls`.
        fn after_call() -> Vec<Message> {
            vec![
                Message::user("go"),
                Message {
                    role: Role::Assistant,
                    content: MessageContent::Blocks(vec![
                        ContentBlock::Reasoning { text: String::new(), signature: Some(format!("{REASONING_SIGNATURE_PREFIX}R1")) },
                        ContentBlock::ToolUse { id: "call_1".into(), name: "ls".into(), input: json!({}), signature: None },
                    ]),
                    turn_scoped: false,
                },
                Message::tool_results(vec![ContentBlock::ToolResult { tool_use_id: "call_1".into(), content: "ok".into(), is_error: false }]),
            ]
        }

        fn opts(context: ContextId) -> BuildOpts {
            BuildOpts::new("m", 1).with_system("s").with_trace(RequestTrace::new(context, TurnId::new(), 1, None))
        }

        async fn drain(client: &Client, opts: BuildOpts, history: Vec<Message>) -> Vec<StreamEvent> {
            let mut stream = client.stream(opts, history).await.unwrap();
            let mut events = Vec::new();
            while let Some(ev) = stream.next_event().await {
                events.push(ev);
            }
            events
        }

        fn ws_client(base: &str, creds: Arc<Credentials>) -> Client {
            Client::new("gpt", creds).with_base_url(format!("{base}/v1"))
        }

        /// A tool loop runs on one connection: the first request sends the
        /// whole input, the next names the first response and sends only
        /// the tool output. The upgrade carries the bearer, the context as
        /// `session-id`, kaijutsu's user agent, and the beta header, and no
        /// Codex headers. The endpoint slot is free at `Done`.
        ///
        /// Falsified by a second connection, a resent history, a frame with
        /// `stream`, or a missing or extra header.
        #[tokio::test]
        async fn a_tool_loop_continues_on_one_connection() {
            let tmp = tempfile::tempdir().unwrap();
            let (base, seen) = serve(|_, ask| match ask {
                Ask::Upgrade(_) => Answer::Accept,
                Ask::Frame(0) => Answer::Events(response("resp_1", vec![reasoning("R1"), call("call_1")])),
                Ask::Frame(_) => Answer::Events(response("resp_2", vec![text("done")])),
            })
            .await;
            let endpoint = crate::llm::endpoint::Endpoints::default().for_url(&base).unwrap();
            let client = ws_client(&base, signed_in(tmp.path(), format!("{base}/token"), "tok")).with_endpoint(endpoint.clone());
            let context = ContextId::new();
            let first = drain(&client, opts(context), vec![Message::user("go")]).await;
            assert!(matches!(first.last(), Some(StreamEvent::Done { stop_reason: Some(r), .. }) if r == "tool_calls"), "{first:?}");
            assert_eq!(endpoint.status().in_flight, 0);
            let second = drain(&client, opts(context), after_call()).await;
            assert!(matches!(second.last(), Some(StreamEvent::Done { stop_reason: Some(r), .. }) if r == "completed"), "{second:?}");

            let seen = seen.lock().unwrap();
            assert_eq!(seen.len(), 1, "{seen:?}");
            let conn = &seen[0];
            assert_eq!(conn.path, "/v1/responses");
            assert_eq!(conn.headers.get("authorization").map(String::as_str), Some("Bearer tok"));
            assert_eq!(conn.headers.get("session-id"), Some(&context.to_string()));
            assert_eq!(conn.headers.get("openai-beta").map(String::as_str), Some("responses_websockets=2026-02-06"));
            assert_eq!(conn.headers.get("user-agent").map(String::as_str), Some(crate::llm::http_user_agent()));
            for refused in ["originator", "thread-id", "x-codex-turn-state", "chatgpt-account-id"] {
                assert!(!conn.headers.contains_key(refused), "{refused} sent: {:?}", conn.headers);
            }
            let [full, delta] = conn.frames.as_slice() else { panic!("{:?}", conn.frames) };
            assert_eq!(full["type"], "response.create");
            assert!(full.get("stream").is_none() && full.get("previous_response_id").is_none(), "{full}");
            assert_eq!(full["store"], false);
            assert_eq!(full["prompt_cache_key"], context.to_string());
            assert_eq!(full["input"].as_array().unwrap().len(), 2, "{full}");
            assert_eq!(delta["previous_response_id"], "resp_1");
            assert_eq!(delta["input"], json!([{ "type": "function_call_output", "call_id": "call_1", "output": "ok" }]));
        }

        /// A continuation whose previous response the server lost is sent
        /// again in full on a new connection, with no error in the stream.
        ///
        /// Falsified by an `Error` event, or a second frame that still names
        /// a previous response.
        #[tokio::test]
        async fn a_lost_previous_response_resends_in_full() {
            let tmp = tempfile::tempdir().unwrap();
            let (base, seen) = serve(|conn, ask| match (conn, ask) {
                (_, Ask::Upgrade(_)) => Answer::Accept,
                (0, Ask::Frame(0)) => Answer::Events(response("resp_1", vec![call("call_1")])),
                (0, Ask::Frame(_)) => Answer::Events(vec![json!({
                    "type": "error", "status": 400,
                    "error": { "code": "previous_response_not_found", "message": "Previous response with id 'resp_1' not found." },
                })]),
                (_, Ask::Frame(_)) => Answer::Events(response("resp_2", vec![text("done")])),
            })
            .await;
            let client = ws_client(&base, signed_in(tmp.path(), format!("{base}/token"), "tok"));
            let context = ContextId::new();
            drain(&client, opts(context), vec![Message::user("go")]).await;
            let events = drain(&client, opts(context), after_call()).await;
            assert!(!events.iter().any(|e| matches!(e, StreamEvent::Error(_))), "{events:?}");
            assert!(matches!(events.last(), Some(StreamEvent::Done { .. })), "{events:?}");
            let seen = seen.lock().unwrap();
            assert_eq!(seen.len(), 2, "{seen:?}");
            assert_eq!(seen[0].frames[1]["previous_response_id"], "resp_1");
            let resent = &seen[1].frames[0];
            assert!(resent.get("previous_response_id").is_none(), "{resent}");
            assert_eq!(resent["input"].as_array().unwrap().len(), 5, "{resent}");
        }

        /// A fresh connection that closes after the request went out is not
        /// sent again, over WebSocket or HTTP: the server may already have
        /// run it. The error is a retryable network error.
        ///
        /// Falsified by a second connection or an HTTP request.
        #[tokio::test]
        async fn a_fresh_connection_lost_after_send_is_not_resent() {
            let tmp = tempfile::tempdir().unwrap();
            let (base, seen) = serve(|_, ask| match ask {
                Ask::Upgrade(_) => Answer::Accept,
                Ask::Frame(_) => Answer::Close,
            })
            .await;
            let client = ws_client(&base, signed_in(tmp.path(), format!("{base}/token"), "tok"));
            let err = client.stream(opts(ContextId::new()), vec![Message::user("go")]).await.err().expect("no stream");
            assert!(matches!(err, crate::llm::LlmError::NetworkError(_)), "{err:?}");
            assert_eq!(seen.lock().unwrap().len(), 1, "one connection, no resend");
        }

        /// A continuation the server rejects is sent once more in full on a
        /// new connection, whichever shape the error takes: the code under
        /// `error`, or at the top level.
        ///
        /// Falsified by a failed turn or a second continuation.
        #[tokio::test]
        async fn a_rejected_continuation_resends_in_full() {
            for rejection in [
                json!({ "type": "error", "status": 400, "code": "previous_response_not_found", "message": "not found" }),
                json!({ "type": "error", "status": 400, "error": { "code": "invalid_request_error", "message": "previous_response_id not supported" } }),
            ] {
                let tmp = tempfile::tempdir().unwrap();
                let (base, seen) = serve(move |conn, ask| match (conn, ask) {
                    (_, Ask::Upgrade(_)) => Answer::Accept,
                    (0, Ask::Frame(0)) => Answer::Events(response("resp_1", vec![call("call_1")])),
                    (0, Ask::Frame(_)) => Answer::Events(vec![rejection.clone()]),
                    (_, Ask::Frame(_)) => Answer::Events(response("resp_2", vec![text("done")])),
                })
                .await;
                let client = ws_client(&base, signed_in(tmp.path(), format!("{base}/token"), "tok"));
                let context = ContextId::new();
                drain(&client, opts(context), vec![Message::user("go")]).await;
                let events = drain(&client, opts(context), after_call()).await;
                assert!(matches!(events.last(), Some(StreamEvent::Done { .. })), "{events:?}");
                let seen = seen.lock().unwrap();
                assert_eq!(seen.len(), 2, "{seen:?}");
                assert!(seen[1].frames[0].get("previous_response_id").is_none());
            }
        }

        /// A metadata event before `response.created`, which the plan route
        /// sends first, is not taken for a stray frame: the response streams
        /// on that connection.
        ///
        /// Falsified by a second connection or a failed request.
        #[tokio::test]
        async fn a_metadata_event_before_the_response_is_skipped() {
            let tmp = tempfile::tempdir().unwrap();
            let (base, seen) = serve(|_, ask| match ask {
                Ask::Upgrade(_) => Answer::Accept,
                Ask::Frame(_) => {
                    let mut events = vec![json!({ "type": "codex.response.metadata", "metadata": {} })];
                    events.extend(response("resp_1", vec![text("hi")]));
                    Answer::Events(events)
                }
            })
            .await;
            let client = ws_client(&base, signed_in(tmp.path(), format!("{base}/token"), "tok"));
            let events = drain(&client, opts(ContextId::new()), vec![Message::user("go")]).await;
            assert!(matches!(events.as_slice(), [.., StreamEvent::TextEnd, StreamEvent::Done { .. }]), "{events:?}");
            assert_eq!(seen.lock().unwrap().len(), 1);
        }

        /// A first frame that does not start a response (a stray event left
        /// from the last one) is not read as the new response: the
        /// connection is dropped and the request sent in full on a new one.
        #[tokio::test]
        async fn a_stray_first_frame_is_not_taken_for_the_response() {
            let tmp = tempfile::tempdir().unwrap();
            let (base, seen) = serve(|conn, ask| match (conn, ask) {
                (_, Ask::Upgrade(_)) => Answer::Accept,
                (0, Ask::Frame(0)) => Answer::Events(response("resp_1", vec![call("call_1")])),
                (0, Ask::Frame(_)) => Answer::Events(vec![json!({ "type": "response.completed", "response": { "id": "resp_1" } })]),
                (_, Ask::Frame(_)) => Answer::Events(response("resp_2", vec![text("done")])),
            })
            .await;
            let client = ws_client(&base, signed_in(tmp.path(), format!("{base}/token"), "tok"));
            let context = ContextId::new();
            drain(&client, opts(context), vec![Message::user("go")]).await;
            let events = drain(&client, opts(context), after_call()).await;
            assert!(matches!(events.as_slice(), [.., StreamEvent::TextEnd, StreamEvent::Done { .. }]), "{events:?}");
            assert_eq!(seen.lock().unwrap().len(), 2);
        }

        /// A pooled connection the server closed while idle is replaced and
        /// the request sent in full, with no error in the stream.
        #[tokio::test]
        async fn a_connection_closed_while_idle_is_replaced() {
            let tmp = tempfile::tempdir().unwrap();
            let (base, seen) = serve(|conn, ask| match (conn, ask) {
                (_, Ask::Upgrade(_)) => Answer::Accept,
                (0, Ask::Frame(0)) => Answer::Events(response("resp_1", vec![call("call_1")])),
                (0, Ask::Frame(_)) => Answer::Close,
                (_, Ask::Frame(_)) => Answer::Events(response("resp_2", vec![text("done")])),
            })
            .await;
            let client = ws_client(&base, signed_in(tmp.path(), format!("{base}/token"), "tok"));
            let context = ContextId::new();
            drain(&client, opts(context), vec![Message::user("go")]).await;
            let events = drain(&client, opts(context), after_call()).await;
            assert!(matches!(events.as_slice(), [.., StreamEvent::TextEnd, StreamEvent::Done { .. }]), "{events:?}");
            let seen = seen.lock().unwrap();
            assert_eq!(seen.len(), 2, "{seen:?}");
            assert!(seen[1].frames[0].get("previous_response_id").is_none());
        }

        /// A 401 at the upgrade refreshes the bearer and connects again with
        /// the new one.
        #[tokio::test]
        async fn an_upgrade_401_refreshes_and_reconnects() {
            let tmp = tempfile::tempdir().unwrap();
            let (token_base, _) = kaijutsu_council::test_server::serve_with(|_| {
                reply(200, r#"{"access_token":"fresh","refresh_token":"r1","expires_in":3600}"#)
            })
            .await;
            let (base, seen) = serve(|_, ask| match ask {
                Ask::Upgrade(h) if h.get("authorization").map(String::as_str) == Some("Bearer fresh") => Answer::Accept,
                Ask::Upgrade(_) => Answer::Refuse(401, r#"{"detail":"token expired"}"#),
                Ask::Frame(_) => Answer::Events(response("resp_1", vec![text("hi")])),
            })
            .await;
            let client = ws_client(&base, signed_in(tmp.path(), format!("{token_base}/token"), "stale"));
            let events = drain(&client, opts(ContextId::new()), vec![Message::user("go")]).await;
            assert!(matches!(events.last(), Some(StreamEvent::Done { .. })), "{events:?}");
            let seen = seen.lock().unwrap();
            let auth: Vec<_> = seen.iter().map(|c| (c.headers["authorization"].as_str(), c.refused)).collect();
            assert_eq!(auth, [("Bearer stale", true), ("Bearer fresh", false)]);
        }

        /// A server that does not speak WebSocket gets the request over HTTP.
        ///
        /// Falsified by an error, or by no POST.
        #[tokio::test]
        async fn a_refused_upgrade_falls_back_to_http() {
            let tmp = tempfile::tempdir().unwrap();
            let (base, seen) = serve_with(|req| match req.method.as_str() {
                "GET" => reply(404, "not found"),
                _ => sse(COMPLETED),
            })
            .await;
            let client = ws_client(&base, signed_in(tmp.path(), format!("{base}/token"), "tok"));
            let events = drain(&client, opts(ContextId::new()), vec![Message::user("go")]).await;
            assert!(matches!(events.last(), Some(StreamEvent::Done { .. })), "{events:?}");
            let seen = seen.lock().unwrap();
            let asked: Vec<_> = seen.iter().map(|r| (r.method.as_str(), r.path.as_str())).collect();
            assert_eq!(asked, [("GET", "/v1/responses"), ("POST", "/v1/responses")]);
        }

        /// A spent plan in the first event is the same balance error the
        /// HTTP route gives, with its guidance.
        #[tokio::test]
        async fn a_spent_plan_keeps_its_classification() {
            let tmp = tempfile::tempdir().unwrap();
            let (base, _) = serve(|_, ask| match ask {
                Ask::Upgrade(_) => Answer::Accept,
                Ask::Frame(_) => Answer::Events(vec![json!({
                    "type": "error", "status": 429,
                    "error": { "code": "subscription_sharing_usage_limit_exceeded", "message": "limit" },
                })]),
            })
            .await;
            let client = ws_client(&base, signed_in(tmp.path(), format!("{base}/token"), "tok"));
            let err = client.stream(opts(ContextId::new()), vec![Message::user("go")]).await.err().unwrap();
            assert!(
                matches!(&err, LlmError::InsufficientBalance(m) if m.contains(crate::llm::openai::responses::stream::PLAN_USAGE_URL)),
                "{err:?}"
            );
        }

        /// A cancelled response closes its connection, so the next request
        /// opens another and sends in full.
        ///
        /// Falsified by a second request on the first connection.
        #[tokio::test]
        async fn a_cancelled_response_closes_its_connection() {
            let tmp = tempfile::tempdir().unwrap();
            let (base, seen) = serve(|conn, ask| match (conn, ask) {
                (_, Ask::Upgrade(_)) => Answer::Accept,
                (0, Ask::Frame(_)) => Answer::Events(vec![
                    json!({ "type": "response.created", "response": { "id": "resp_1" } }),
                    json!({ "type": "response.output_item.added", "item": { "type": "message" } }),
                ]),
                (_, Ask::Frame(_)) => Answer::Events(response("resp_2", vec![text("done")])),
            })
            .await;
            let client = ws_client(&base, signed_in(tmp.path(), format!("{base}/token"), "tok"));
            let context = ContextId::new();
            let mut stream = client.stream(opts(context), vec![Message::user("go")]).await.unwrap();
            assert_eq!(stream.next_event().await, Some(StreamEvent::TextStart));
            stream.cancel();
            assert!(matches!(stream.next_event().await, Some(StreamEvent::Done { stop_reason: None, .. })));
            drop(stream);
            let events = drain(&client, opts(context), vec![Message::user("go"), Message::user("again")]).await;
            assert!(matches!(events.last(), Some(StreamEvent::Done { .. })), "{events:?}");
            let seen = seen.lock().unwrap();
            assert_eq!(seen.len(), 2, "{seen:?}");
            assert_eq!(seen[0].frames.len(), 1);
        }

        /// The HTTP transport sends a context's request as a POST.
        #[tokio::test]
        async fn the_http_transport_posts() {
            let tmp = tempfile::tempdir().unwrap();
            let (base, seen) = serve_with(|_| sse(COMPLETED)).await;
            let client = ws_client(&base, signed_in(tmp.path(), format!("{base}/token"), "tok")).with_transport(Transport::Http);
            drain(&client, opts(ContextId::new()), vec![Message::user("go")]).await;
            let seen = seen.lock().unwrap();
            assert_eq!(seen.iter().map(|r| r.method.as_str()).collect::<Vec<_>>(), ["POST"]);
        }
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
