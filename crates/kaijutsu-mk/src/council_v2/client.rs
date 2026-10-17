//! The `/council/v2` calls (`docs/council-api-v2.md`).

use std::time::Duration;

use reqwest::Method;

use crate::client::{decode, excerpt, to_body, Family, MkClient, MkError, ServiceErrorBody};
use crate::council::canon;
use crate::council::wire::{DecisionResponse, HeldSpec, ServerIdentity, SnapshotId, Spec, SpecId};
use crate::council_v2::wire::{
    ContextPut, ContextPutResult, ContextState, DecisionRequest, GenerationDone, GenerationEvent, GenerationRequest,
    ReadsRequest, ReadsResponse,
};
use crate::generate::{EventStream, Frame, StreamItem};

/// The `/council/v2` calls of one [`MkClient`].
#[derive(Clone, Copy, Debug)]
pub struct CouncilV2<'a> {
    mk: &'a MkClient,
}

impl MkClient {
    /// This client's `/council/v2` calls.
    pub fn v2(&self) -> CouncilV2<'_> {
        CouncilV2 { mk: self }
    }
}

/// A streamed `POST /council/v2/generations`.
pub type GenerationStream = EventStream<GenerationEvent>;

impl StreamItem for GenerationEvent {
    fn decode(frame: &Frame) -> Result<Self, MkError> {
        let data = frame.data.as_bytes();
        match frame.event.as_str() {
            "token" => decode(data, "token event").map(GenerationEvent::Token),
            "done" => decode(data, "done event").map(GenerationEvent::Done),
            "error" => Err(MkError::Stream(decode::<ServiceErrorBody>(data, "error event")?.error)),
            other => Err(MkError::Decode {
                what: "event stream",
                message: format!("unknown event {other:?}"),
                body: excerpt(data),
            }),
        }
    }

    fn is_last(&self) -> bool {
        matches!(self, GenerationEvent::Done(_))
    }
}

/// The body the generation calls send: the request plus `stream`.
#[derive(serde::Serialize)]
struct GenerationWire<'a> {
    #[serde(flatten)]
    request: &'a GenerationRequest,
    stream: bool,
}

impl CouncilV2<'_> {
    /// `GET /council/v2/identity`. A server without v2 answers 404
    /// ([`MkError::status`]).
    pub async fn identity(&self) -> Result<ServerIdentity, MkError> {
        self.mk
            .json(Family::Council, Method::GET, "/council/v2/identity", None, &[], self.mk.timeout, 200, "identity")
            .await
    }

    /// `PUT /council/v2/contexts/{id}`, conditional on `if_match` when given.
    /// The body is checked ([`ContextPut::validate`]) before it is sent.
    pub async fn put_context(
        &self,
        id: &str,
        body: &ContextPut,
        if_match: Option<&SnapshotId>,
    ) -> Result<ContextPutResult, MkError> {
        let path = context_path(id)?;
        body.validate()?;
        let mut headers = Vec::new();
        if let Some(head) = if_match {
            headers.push(("if-match", head.as_str().to_string()));
        }
        self.mk
            .json(Family::Council, Method::PUT, &path, Some(to_body(body)?), &headers, self.mk.timeout, 200, "context put result")
            .await
    }

    /// `GET /council/v2/contexts/{id}`.
    pub async fn get_context(&self, id: &str) -> Result<ContextState, MkError> {
        let path = context_path(id)?;
        self.mk.json(Family::Council, Method::GET, &path, None, &[], self.mk.timeout, 200, "context state").await
    }

    /// `DELETE /council/v2/contexts/{id}`.
    pub async fn delete_context(&self, id: &str) -> Result<(), MkError> {
        let path = context_path(id)?;
        self.mk.call(Family::Council, Method::DELETE, &path, None, &[], self.mk.timeout, 204).await.map(|_| ())
    }

    /// `POST /council/v2/specs`. Fails with [`MkError::SpecIdMismatch`] when
    /// the server's id is not the spec's canonical id.
    pub async fn post_spec(&self, spec: &Spec) -> Result<HeldSpec, MkError> {
        let computed = canon::spec_id(spec).map_err(|e| MkError::Request(e.to_string()))?;
        let held: HeldSpec = self
            .mk
            .json(Family::Council, Method::POST, "/council/v2/specs", Some(to_body(spec)?), &[], self.mk.timeout, 200, "held spec")
            .await?;
        if held.spec_id != computed {
            return Err(MkError::SpecIdMismatch { computed, server: held.spec_id });
        }
        Ok(held)
    }

    /// `GET /council/v2/specs/{spec_id}`.
    pub async fn get_spec(&self, id: &SpecId) -> Result<HeldSpec, MkError> {
        let path = format!("/council/v2/specs/{id}");
        self.mk.json(Family::Council, Method::GET, &path, None, &[], self.mk.timeout, 200, "held spec").await
    }

    /// `POST /council/v2/decisions`, sending `traceparent` for this call
    /// when given. The request is checked first; `timeout` bounds the HTTP
    /// call.
    pub async fn decide_traced(
        &self,
        request: &DecisionRequest,
        timeout: Duration,
        traceparent: Option<&str>,
    ) -> Result<DecisionResponse, MkError> {
        request.validate()?;
        let mut headers = Vec::new();
        if let Some(tp) = traceparent {
            headers.push(("traceparent", tp.to_string()));
        }
        self.mk
            .json(Family::Council, Method::POST, "/council/v2/decisions", Some(to_body(request)?), &headers, timeout, 200, "decision")
            .await
    }

    /// `POST /council/v2/reads`.
    pub async fn reads(&self, request: &ReadsRequest, timeout: Duration) -> Result<ReadsResponse, MkError> {
        request.validate()?;
        self.mk
            .json(Family::Council, Method::POST, "/council/v2/reads", Some(to_body(request)?), &[], timeout, 200, "reads")
            .await
    }

    /// `POST /council/v2/generations` without streaming. `timeout` bounds the
    /// whole call. A prompt with no room is [`MkError::ContextFull`].
    pub async fn generate(&self, request: &GenerationRequest, timeout: Duration) -> Result<GenerationDone, MkError> {
        request.validate()?;
        let body = to_body(&GenerationWire { request, stream: false })?;
        self.mk.json(Family::Council, Method::POST, "/council/v2/generations", Some(body), &[], timeout, 200, "done").await
    }

    /// `POST /council/v2/generations` as a stream: `token` events, then
    /// `done`. `idle` bounds the wait for the response and then each read.
    /// Dropping the stream closes the connection, which cancels the reply.
    /// A prompt with no room is [`MkError::ContextFull`], before any event.
    pub async fn generate_stream(&self, request: &GenerationRequest, idle: Duration) -> Result<GenerationStream, MkError> {
        request.validate()?;
        let body = to_body(&GenerationWire { request, stream: true })?;
        self.mk.open_stream(Family::Council, "/council/v2/generations", body, idle).await
    }
}

fn context_path(id: &str) -> Result<String, MkError> {
    if id.is_empty() || !id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-') {
        return Err(MkError::Request(format!("context id {id:?} is not a UUID")));
    }
    Ok(format!("/council/v2/contexts/{id}"))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::council::wire::{Capability, ContextRef, DecisionRequest as V1Decision};
    use crate::council_v2::wire::{Message, ReplyToolCall, Think};
    use crate::test_server::{reply, serve, sse};

    const CTX: &str = "0199b3c4-6c1e-7a2b-9f00-3e5d1c2a7b12";
    const RESPONSE: &str = include_str!("../../tests/fixtures/decision_response.json");
    const REQUEST: &str = include_str!("../../tests/fixtures/decision_request.json");
    const SPEC: &str = include_str!("../../tests/fixtures/spec.json");
    const DOC_SPEC_ID: &str = "sha256:5114ff063b887c710333afd4fe35238c55946ac2a63fb76d6c9004b56a1503ea";

    fn snap(c: char) -> String {
        format!("snap:{}", c.to_string().repeat(64))
    }

    fn client(base: &str) -> MkClient {
        MkClient::new(base, Duration::from_secs(5)).unwrap()
    }

    fn put_result() -> String {
        json!({"id": CTX, "head": snap('a'), "tokens": 10, "kept": 0, "fed": 10, "from": null, "dry_run": false,
               "snapshots": [{"id": snap('a'), "message": 1, "tokens": 10, "tier": "device"}]})
        .to_string()
    }

    fn generation() -> GenerationRequest {
        GenerationRequest { context: CTX.into(), messages: vec![Message::user("how did it go?").snapped()], max_tokens: Some(64), sample: None }
    }

    fn done_json() -> serde_json::Value {
        json!({"content": "Fine.", "reasoning": "", "tool_calls": [], "finish": "stop",
               "usage": {"kept": 10, "fed": 6, "completion": 2},
               "head": snap('b'), "snapshots": [{"id": snap('a'), "message": 1, "tokens": 10}]})
    }

    fn token(n: u32, piece: &str) -> String {
        let t = json!({"id": n, "piece": piece, "phase": "answer", "p": 0.9, "h": 0.1, "n": n, "tps": 10.0});
        format!("event: token\ndata: {t}\n\n")
    }

    #[tokio::test]
    async fn identity_reads_the_v2_path_and_its_capabilities() {
        let body = json!({"model": "m", "weight_hash": "w", "tokenizer_hash": "t", "template": "x", "engine": "e",
                          "limits": {"context_tokens": 32768, "state_bytes": 1000, "contexts_per_decision": 8,
                                     "choice_options": 26, "default_timeout_ms": 2000, "snapshot_bytes": 4096},
                          "capabilities": ["generate", "reads", "think", "fork", "tiers", "warm"]});
        let (base, seen) = serve(vec![reply(200, body.to_string()), reply(404, "not found")]).await;
        let c = client(&base);
        let id = c.v2().identity().await.unwrap();
        assert_eq!(
            id.capabilities,
            [Capability::Generate, Capability::Reads, Capability::Think, Capability::Fork, Capability::Tiers, Capability::Warm]
        );
        assert_eq!(seen.lock().unwrap()[0].path, "/council/v2/identity");
        assert_eq!(c.v2().identity().await.unwrap_err().status(), Some(404), "a server without v2");
    }

    #[tokio::test]
    async fn put_context_sends_messages_fork_and_if_match_to_the_v2_path() {
        let (base, seen) = serve(vec![reply(200, put_result())]).await;
        let body = ContextPut {
            messages: vec![Message::system("s"), Message::user("u").snapped()],
            fork: Some(crate::council_v2::wire::Fork { context: "parent".into(), at: SnapshotId::parse(snap('9')).unwrap() }),
            ..ContextPut::default()
        };
        let head = SnapshotId::parse(snap('f')).unwrap();
        let r = client(&base).v2().put_context(CTX, &body, Some(&head)).await.unwrap();
        assert_eq!((r.fed, r.from.is_none()), (10, true));
        let seen = seen.lock().unwrap();
        assert_eq!((seen[0].method.as_str(), seen[0].path.as_str()), ("PUT", &*format!("/council/v2/contexts/{CTX}")));
        assert_eq!(seen[0].header("if-match"), Some(snap('f').as_str()));
        let sent: serde_json::Value = serde_json::from_str(&seen[0].body).unwrap();
        assert_eq!(sent["fork"], json!({"context": "parent", "at": snap('9')}));
        assert_eq!(sent["messages"][1], json!({"role": "user", "content": "u", "snap": true}));
    }

    #[tokio::test]
    async fn an_invalid_body_or_id_is_refused_before_it_is_sent() {
        let (base, seen) = serve(vec![]).await;
        let user_reasoning = Message { reasoning: Some("r".into()), ..Message::user("u") };
        let bad = ContextPut { messages: vec![user_reasoning], ..ContextPut::default() };
        assert!(matches!(client(&base).v2().put_context(CTX, &bad, None).await, Err(MkError::Request(_))));
        assert!(matches!(client(&base).v2().get_context("../x").await, Err(MkError::Request(_))));
        let empty = GenerationRequest { max_tokens: Some(0), ..generation() };
        assert!(matches!(client(&base).v2().generate(&empty, Duration::from_secs(1)).await, Err(MkError::Request(_))));
        assert!(seen.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn specs_and_contexts_use_the_v2_paths() {
        let spec: Spec = serde_json::from_str(SPEC).unwrap();
        let held = json!({"spec_id": DOC_SPEC_ID, "spec": serde_json::from_str::<serde_json::Value>(SPEC).unwrap(), "template": "t"});
        let state = json!({"id": CTX, "head": snap('a'), "tokens": 10, "snapshots": [{"id": snap('a'), "message": 1, "tokens": 10, "tier": "host"}]});
        let (base, seen) = serve(vec![
            reply(200, held.to_string()),
            reply(200, held.to_string()),
            reply(200, state.to_string()),
            reply(204, ""),
        ])
        .await;
        let c = client(&base);
        c.v2().post_spec(&spec).await.unwrap();
        c.v2().get_spec(&SpecId::parse(DOC_SPEC_ID).unwrap()).await.unwrap();
        assert_eq!(c.v2().get_context(CTX).await.unwrap().snapshots[0].tier, Some(crate::council_v2::wire::Tier::Host));
        c.v2().delete_context(CTX).await.unwrap();
        let paths: Vec<(String, String)> = seen.lock().unwrap().iter().map(|c| (c.method.clone(), c.path.clone())).collect();
        assert_eq!(
            paths,
            [
                ("POST".to_string(), "/council/v2/specs".to_string()),
                ("GET".into(), format!("/council/v2/specs/{DOC_SPEC_ID}")),
                ("GET".into(), format!("/council/v2/contexts/{CTX}")),
                ("DELETE".into(), format!("/council/v2/contexts/{CTX}")),
            ]
        );
    }

    #[tokio::test]
    async fn decide_sends_v1_plus_think_and_reads_the_thought() {
        let mut answer: serde_json::Value = serde_json::from_str(RESPONSE).unwrap();
        answer["reads"][0]["thought"] = json!("It publishes main.");
        let (base, seen) = serve(vec![reply(200, answer.to_string())]).await;
        let req = DecisionRequest { decision: serde_json::from_str::<V1Decision>(REQUEST).unwrap(), think: Some(Think { tokens: 32 }) };
        let resp = client(&base).v2().decide_traced(&req, Duration::from_secs(5), None).await.unwrap();
        assert_eq!(resp.reads[0].thought.as_deref(), Some("It publishes main."));
        crate::council::math::verify(&resp, &req.decision).unwrap();
        let seen = seen.lock().unwrap();
        assert_eq!(seen[0].path, "/council/v2/decisions");
        let mut want: serde_json::Value = serde_json::from_str(REQUEST).unwrap();
        want["think"] = json!({"tokens": 32});
        assert_eq!(serde_json::from_str::<serde_json::Value>(&seen[0].body).unwrap(), want);
    }

    #[tokio::test]
    async fn reads_posts_the_suffixes() {
        let body = json!({"reads": [{"context": CTX, "logprobs": {"A": -0.1, "B": -2.4}, "mass": -0.01, "top": []}]});
        let (base, seen) = serve(vec![reply(200, body.to_string())]).await;
        let req = ReadsRequest {
            contexts: vec![ContextRef { id: CTX.into(), at: None }],
            suffixes: vec!["verdict: ".into()],
            letters: vec!["A".into(), "B".into()],
            top: Some(5),
        };
        let r = client(&base).v2().reads(&req, Duration::from_secs(5)).await.unwrap();
        assert_eq!(r.reads[0].mass, -0.01);
        assert_eq!(seen.lock().unwrap()[0].path, "/council/v2/reads");
    }

    #[tokio::test]
    async fn a_generation_streams_tokens_then_done() {
        let stream = format!("{}{}event: done\ndata: {}\n\n", token(1, "Fine"), token(2, "."), done_json());
        let (base, seen) = serve(vec![sse(stream)]).await;
        let mut s = client(&base).v2().generate_stream(&generation(), Duration::from_secs(5)).await.unwrap();
        let mut pieces = String::new();
        let done = loop {
            match s.next().await.unwrap().unwrap() {
                GenerationEvent::Token(t) => pieces.push_str(&t.piece),
                GenerationEvent::Done(d) => break d,
            }
        };
        assert!(s.next().await.is_none());
        assert_eq!((pieces.as_str(), done.content.as_str(), done.head.as_str()), ("Fine.", "Fine.", snap('b').as_str()));
        let seen = seen.lock().unwrap();
        assert_eq!((seen[0].method.as_str(), seen[0].path.as_str()), ("POST", "/council/v2/generations"));
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&seen[0].body).unwrap(),
            json!({"context": CTX, "messages": [{"role": "user", "content": "how did it go?", "snap": true}],
                   "max_tokens": 64, "stream": true})
        );
    }

    #[tokio::test]
    async fn a_generation_without_streaming_returns_done_with_call_ids() {
        let mut d = done_json();
        d["tool_calls"] = json!([{"id": "c7", "name": "ls", "arguments": {"path": "/"}}]);
        let (base, seen) = serve(vec![reply(200, d.to_string())]).await;
        let done = client(&base).v2().generate(&generation(), Duration::from_secs(5)).await.unwrap();
        assert_eq!(done.tool_calls[0].id(), "c7");
        assert!(matches!(&done.tool_calls[0], ReplyToolCall::Parsed { name, .. } if name == "ls"));
        let body: serde_json::Value = serde_json::from_str(&seen.lock().unwrap()[0].body).unwrap();
        assert_eq!(body["stream"], json!(false));
    }

    /// A prompt that leaves no room is a 413 of type `context_full`, before
    /// anything runs, and the client names it apart from every other
    /// status, on either call.
    ///
    /// Falsified by a 413 that decodes as an ordinary status error.
    #[tokio::test]
    async fn a_413_context_full_is_its_own_error() {
        let full = json!({"error": {"type": "context_full", "message": "the prompt takes 32760 of 32768 tokens",
                                    "prompt_tokens": 32760, "context_tokens": 32768}})
        .to_string();
        let too_large = json!({"error": {"type": "too_large", "message": "state", "param": "state_bytes"}}).to_string();
        let (base, _) = serve(vec![reply(413, full.clone()), reply(413, full), reply(413, too_large)]).await;
        let c = client(&base);
        let err = c.v2().generate_stream(&generation(), Duration::from_secs(5)).await.err().unwrap();
        let MkError::ContextFull { message, body } = &err else { panic!("{err:?}") };
        assert!(message.contains("32760") && body.contains("context_tokens"), "{message} {body}");
        assert_eq!(err.status(), Some(413));
        let err = c.v2().generate(&generation(), Duration::from_secs(5)).await.unwrap_err();
        assert!(matches!(err, MkError::ContextFull { .. }), "{err:?}");
        let err = c.v2().generate(&generation(), Duration::from_secs(5)).await.unwrap_err();
        assert!(matches!(err, MkError::Status { status: 413, .. }), "another 413 stays a status: {err:?}");
    }

    #[tokio::test]
    async fn a_generation_cut_before_done_is_truncated_and_an_error_event_ends_it() {
        let error = r#"{"error":{"code":503,"message":"spin","type":"pass_timeout_error"}}"#;
        let (base, _) = serve(vec![sse(token(1, "Fi")), sse(format!("{}event: error\ndata: {error}\n\n", token(1, "Fi")))]).await;
        let c = client(&base);
        for want_stream_error in [false, true] {
            let mut s = c.v2().generate_stream(&generation(), Duration::from_secs(5)).await.unwrap();
            assert!(matches!(s.next().await, Some(Ok(GenerationEvent::Token(_)))));
            let last = s.next().await.unwrap();
            match want_stream_error {
                false => assert!(matches!(last, Err(MkError::Truncated(_))), "{last:?}"),
                true => assert!(matches!(last, Err(MkError::Stream(_))), "{last:?}"),
            }
            assert!(s.next().await.is_none());
        }
    }
}
