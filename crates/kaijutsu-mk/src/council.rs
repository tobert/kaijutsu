//! The council v2 calls (gogi's `docs/council-api.md`).
//!
//! The request and response types are `gogi_proto`'s. Each call checks its
//! request with `gogi_proto`'s rules before sending it, so a request the
//! contract refuses is [`MkError::Request`] and never reaches the server.

use std::time::Duration;

use gogi_proto::context::{Context, ContextBuild, ContextBuilt};
use gogi_proto::decision::{DecisionRequest, DecisionResponse};
use gogi_proto::embeddings::{EmbeddingsRequest, EmbeddingsResponse};
use gogi_proto::generation::{GenerationDone, GenerationRequest, StreamEvent, TokenEvent};
use gogi_proto::identity::ServerIdentity;
use gogi_proto::pins::{validate_owner, PinSet, PinsPut, PinsPutResult, SnapshotDetail};
use gogi_proto::reads::{ReadsRequest, ReadsResponse};
use gogi_proto::spec::{HeldSpec, Spec};
use gogi_proto::{SnapshotId, SpecId};
use reqwest::Method;

use crate::client::{excerpt, to_body, Family, MkClient, MkError};
use crate::generate::{EventStream, Frame, StreamItem};

/// The council v2 calls of one [`MkClient`].
#[derive(Clone, Copy, Debug)]
pub struct Council<'a> {
    mk: &'a MkClient,
}

impl MkClient {
    /// This client's council v2 calls.
    pub fn council(&self) -> Council<'_> {
        Council { mk: self }
    }
}

/// One event of a streamed generation. A `ping`, the `output`, `progress`
/// and `preview` events of a reply this client never asks to write objects
/// or preview, and an event the contract does not name, are skipped; an
/// `error` event is [`MkError::CouncilStream`].
#[derive(Clone, Debug, PartialEq)]
pub enum GenerationEvent {
    Token(TokenEvent),
    /// Always the last event.
    Done(Box<GenerationDone>),
}

/// A streamed `POST /council/v2/generations`.
pub type GenerationStream = EventStream<GenerationEvent>;

impl StreamItem for GenerationEvent {
    fn decode(frame: &Frame) -> Result<Option<Self>, MkError> {
        let event = StreamEvent::parse(&frame.event, &frame.data).map_err(|message| MkError::Decode {
            what: "event stream",
            message,
            body: excerpt(frame.data.as_bytes()),
        })?;
        match event {
            Some(StreamEvent::Token(t)) => Ok(Some(GenerationEvent::Token(t))),
            Some(StreamEvent::Done(d)) => Ok(Some(GenerationEvent::Done(d))),
            Some(StreamEvent::Error(b)) => Err(MkError::CouncilStream { error: b.error, request_id: b.request_id }),
            Some(StreamEvent::Ping | StreamEvent::Output(_) | StreamEvent::Progress(_) | StreamEvent::Preview(_))
            | None => Ok(None),
        }
    }

    fn is_last(&self) -> bool {
        matches!(self, GenerationEvent::Done(_))
    }
}

impl Council<'_> {
    /// `GET /council/v2/identity`.
    pub async fn identity(&self) -> Result<ServerIdentity, MkError> {
        self.mk
            .json(Family::Council, Method::GET, "/council/v2/identity", None, &[], self.mk.timeout, 200, "identity")
            .await
    }

    /// `POST /council/v2/contexts`: builds `context` on the served model
    /// `model` and holds its snapshots, or, with `dry_run`, reports what a
    /// build would reuse without feeding. A context with no room is
    /// [`MkError::ContextFull`].
    pub async fn build_context(&self, model: &str, context: &Context, dry_run: bool) -> Result<ContextBuilt, MkError> {
        context.check()?;
        let body = ContextBuild { model: model.to_string(), context: context.clone(), dry_run: dry_run.then_some(true), class: None, priority: None };
        self.mk
            .json(Family::Council, Method::POST, "/council/v2/contexts", Some(to_body(&body)?), &[], self.mk.timeout, 200, "context build")
            .await
    }

    /// `GET /council/v2/snapshots/{id}`. A snapshot the server does not hold
    /// is a 404.
    pub async fn snapshot(&self, id: &SnapshotId) -> Result<SnapshotDetail, MkError> {
        let path = format!("/council/v2/snapshots/{id}");
        self.mk.json(Family::Council, Method::GET, &path, None, &[], self.mk.timeout, 200, "snapshot").await
    }

    /// `PUT /council/v2/pins/{owner}`: replaces `owner`'s whole set with
    /// `body` and renews it. Past `pin_bytes` it is a 413 that pins nothing.
    pub async fn put_pins(&self, owner: &str, body: &PinsPut) -> Result<PinsPutResult, MkError> {
        let path = pins_path(owner)?;
        body.validate()?;
        self.mk.json(Family::Council, Method::PUT, &path, Some(to_body(body)?), &[], self.mk.timeout, 200, "pins").await
    }

    /// `GET /council/v2/pins/{owner}`. An owner with no pins is a 404.
    pub async fn get_pins(&self, owner: &str) -> Result<PinSet, MkError> {
        let path = pins_path(owner)?;
        self.mk.json(Family::Council, Method::GET, &path, None, &[], self.mk.timeout, 200, "pin set").await
    }

    /// `DELETE /council/v2/pins/{owner}`. An owner with no pins is a 404.
    pub async fn delete_pins(&self, owner: &str) -> Result<(), MkError> {
        let path = pins_path(owner)?;
        self.mk.call(Family::Council, Method::DELETE, &path, None, &[], self.mk.timeout, 204).await.map(|_| ())
    }

    /// `POST /council/v2/specs` with the spec's JSON. Fails with
    /// [`MkError::SpecIdMismatch`] when the server's id is not the spec's
    /// canonical id.
    pub async fn post_spec(&self, spec: &Spec) -> Result<HeldSpec, MkError> {
        let held: HeldSpec = self
            .mk
            .json(Family::Council, Method::POST, "/council/v2/specs", Some(to_body(&spec.value)?), &[], self.mk.timeout, 200, "held spec")
            .await?;
        let computed = spec.id();
        if held.spec_id.as_str() != computed {
            return Err(MkError::SpecIdMismatch { computed, server: held.spec_id.to_string() });
        }
        Ok(held)
    }

    /// `GET /council/v2/specs/{spec_id}`.
    pub async fn get_spec(&self, id: &SpecId) -> Result<HeldSpec, MkError> {
        let path = format!("/council/v2/specs/{id}");
        self.mk.json(Family::Council, Method::GET, &path, None, &[], self.mk.timeout, 200, "held spec").await
    }

    /// `POST /council/v2/decisions`, sending `traceparent` for this call
    /// when given. `timeout` bounds the HTTP call.
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

    /// `POST /council/v2/embeddings`. The server checks the request against
    /// the served model it names (`EmbeddingsRequest::check` holds the same
    /// rules for a caller that has the model's identity); a refusal is
    /// [`MkError::Status`] with the parameter it names. `timeout` bounds the
    /// whole call.
    pub async fn embeddings(&self, request: &EmbeddingsRequest, timeout: Duration) -> Result<EmbeddingsResponse, MkError> {
        self.mk
            .json(Family::Council, Method::POST, "/council/v2/embeddings", Some(to_body(request)?), &[], timeout, 200, "embeddings")
            .await
    }

    /// `POST /council/v2/generations` without streaming; the call sets
    /// `stream`. `timeout` bounds the whole call. A prompt with no room is
    /// [`MkError::ContextFull`].
    pub async fn generate(&self, request: &GenerationRequest, timeout: Duration) -> Result<GenerationDone, MkError> {
        let request = GenerationRequest { stream: Some(false), ..request.clone() };
        request.validate()?;
        self.mk
            .json(Family::Council, Method::POST, "/council/v2/generations", Some(to_body(&request)?), &[], timeout, 200, "done")
            .await
    }

    /// `POST /council/v2/generations` as a stream: `token` events, then
    /// `done`; the call sets `stream`. `idle` bounds the wait for the
    /// response and then each read. Dropping the stream closes the
    /// connection, which cancels the reply. A prompt with no room is
    /// [`MkError::ContextFull`], before any event.
    pub async fn generate_stream(&self, request: &GenerationRequest, idle: Duration) -> Result<GenerationStream, MkError> {
        let request = GenerationRequest { stream: Some(true), ..request.clone() };
        request.validate()?;
        self.mk.open_stream(Family::Council, "/council/v2/generations", to_body(&request)?, idle).await
    }
}

fn pins_path(owner: &str) -> Result<String, MkError> {
    validate_owner(owner)?;
    let mut path = String::from("/council/v2/pins/");
    for b in owner.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => path.push(b as char),
            _ => path.push_str(&format!("%{b:02X}")),
        }
    }
    Ok(path)
}

#[cfg(test)]
mod tests {
    use gogi_fake::{Config, Server};
    use gogi_proto::generation::{ReplyToolCall, Sample};
    use gogi_proto::message::Content;
    use gogi_proto::spec::{parse_spec, SpecRules};
    use gogi_proto::{ErrorType, Message, Tier};
    use serde_json::json;

    use super::*;
    use crate::test_server::{reply, serve, sse};

    fn client(base: &str) -> MkClient {
        MkClient::new(base, Duration::from_secs(5)).unwrap()
    }

    fn fake() -> (Server, MkClient) {
        let server = Server::start(Config::default()).unwrap();
        let c = client(&server.url());
        (server, c)
    }

    fn chat() -> Context {
        Context { messages: vec![Message::system("s"), Message::user("how did it go?").snapped()], ..Context::default() }
    }

    /// The fake's one served model.
    const TOY: &str = "gogi-toy-1";

    fn greedy(context: Context) -> GenerationRequest {
        GenerationRequest {
            model: TOY.to_string(),
            context,
            max_tokens: 24,
            sample: Some(Sample { temperature: Some(0.0), ..Sample::default() }),
            stream: None,
            decisions: Vec::new(),
            class: None,
            priority: None,
            output: None,
            previews: None,
        }
    }

    fn gate_spec() -> Spec {
        let v = json!({"name": "gate", "instructions": "Judge the command.", "input_label": "Command",
            "questions": [{"id": "verdict", "type": "choice", "instructions": "What happens with it.",
                           "criteria": [{"option": "allow", "means": "routine"}, {"option": "ask", "means": "not clearly fine"}]}]});
        parse_spec(&v, &SpecRules::default()).unwrap()
    }

    #[tokio::test]
    async fn the_fake_reports_its_identity() {
        let (_server, c) = fake();
        let id = c.council().identity().await.unwrap();
        assert!(id.has(gogi_proto::identity::Capability::Pins));
        let toy = id.route("toy").unwrap();
        assert_eq!(toy.model, TOY);
        assert!(toy.has(gogi_proto::identity::Capability::Generate) && toy.limits.context_tokens > 0);
    }

    /// The second build of the same context reuses the first one's
    /// snapshot, and a dry run reports without holding anything new.
    #[tokio::test]
    async fn a_second_build_feeds_nothing() {
        let (_server, c) = fake();
        let first = c.council().build_context(TOY, &chat(), false).await.unwrap();
        assert!(first.usage.fed > 0 && first.from.is_none() && !first.dry_run);
        first.usage.check().unwrap();
        let again = c.council().build_context(TOY, &chat(), false).await.unwrap();
        assert_eq!((again.usage.fed, &again.head), (0, &first.head));
        let mut longer = chat();
        longer.messages.push(Message::assistant("fine").snapped());
        let dry = c.council().build_context(TOY, &longer, true).await.unwrap();
        assert!(dry.dry_run && dry.usage.kept == first.usage.prompt && dry.usage.fed > 0, "{dry:?}");
    }

    /// A served model that is not ready answers 503 `model_unavailable`
    /// naming itself and its status, and the error keeps both.
    #[tokio::test]
    async fn a_model_that_is_not_ready_is_a_503_naming_it() {
        let (server, c) = fake();
        server.set_status(TOY, gogi_fake::ModelStatus::Loading).unwrap();
        let err = c.council().build_context(TOY, &chat(), false).await.unwrap_err();
        let MkError::Status { status: 503, error: Some(e), .. } = &err else { panic!("{err:?}") };
        assert_eq!((e.r#type, e.model.as_deref(), e.status), (ErrorType::ModelUnavailable, Some(TOY), Some(gogi_fake::ModelStatus::Loading)));
        server.set_status(TOY, gogi_fake::ModelStatus::Ready).unwrap();
        c.council().build_context(TOY, &chat(), false).await.unwrap();
    }

    #[tokio::test]
    async fn a_spec_posts_under_its_canonical_id_and_reads_back() {
        let (_server, c) = fake();
        let spec = gate_spec();
        let held = c.council().post_spec(&spec).await.unwrap();
        assert_eq!(held.spec_id.as_str(), spec.id());
        let again = c.council().get_spec(&held.spec_id).await.unwrap();
        assert_eq!(again.spec, spec.value);
    }

    /// A decision that sends its contexts whole comes back with reads named
    /// by index and numbers that recompute under gogi's math.
    #[tokio::test]
    async fn a_decision_reads_each_context_and_its_numbers_verify() {
        let (_server, c) = fake();
        let held = c.council().post_spec(&gate_spec()).await.unwrap();
        let mut other = chat();
        other.messages.push(Message::assistant("never push to main").snapped());
        let request = DecisionRequest {
            model: TOY.to_string(),
            case: json!("git push origin main"),
            spec_id: held.spec_id.to_string(),
            contexts: vec![chat(), other],
            ..DecisionRequest::default()
        };
        let response = c.council().decide_traced(&request, Duration::from_secs(5), Some("00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01")).await.unwrap();
        gogi_proto::math::verify(&response, &request.expect()).unwrap();
        assert_eq!(response.reads.iter().map(|r| r.context.clone()).collect::<Vec<_>>(), [Some(0), Some(1)]);
        assert_eq!(response.identity.spec_id.as_ref(), Some(&held.spec_id));
    }

    /// Pins replace the owner's set, read back as ids, show on the
    /// snapshot, and go on delete. An owner with a space is escaped.
    #[tokio::test]
    async fn pins_hold_list_and_drop() {
        let (_server, c) = fake();
        let head = c.council().build_context(TOY, &chat(), false).await.unwrap().head;
        let put = c.council().put_pins("kernel 1", &PinsPut { snapshots: vec![head.to_string()], ttl_s: 60 }).await.unwrap();
        assert!(put.snapshots[0].held && put.expires_in_s.is_some());
        assert_eq!(c.council().get_pins("kernel 1").await.unwrap().snapshots, [head.to_string()]);
        let detail = c.council().snapshot(&head).await.unwrap();
        assert_eq!(detail.pins.iter().map(|p| p.owner.as_str()).collect::<Vec<_>>(), ["kernel 1"]);
        assert_ne!(detail.tier, Tier::Unknown);
        c.council().delete_pins("kernel 1").await.unwrap();
        assert_eq!(c.council().get_pins("kernel 1").await.unwrap_err().status(), Some(404));
    }

    #[tokio::test]
    async fn reads_return_one_read_per_context_and_suffix() {
        let (_server, c) = fake();
        let request = ReadsRequest {
            model: TOY.to_string(),
            contexts: vec![chat()],
            suffixes: vec!["a ".into(), "b ".into()],
            letters: vec!["A".into(), "B".into()],
            top: Some(3),
            class: None,
            priority: None,
        };
        let r = c.council().reads(&request, Duration::from_secs(5)).await.unwrap();
        assert_eq!(r.reads.iter().map(|r| (r.context, r.suffix)).collect::<Vec<_>>(), [(0, 0), (0, 1)]);
        assert_eq!(r.reads[0].logprobs.keys().collect::<Vec<_>>(), ["A", "B"]);
    }

    /// The fake's served model that only embeds.
    const EMBED: &str = "embed-toy-1";

    fn fake_embedder(context_tokens: Option<usize>) -> (Server, MkClient) {
        let mut config = Config::default();
        config.models.push(gogi_fake::FakeModel::embedding(EMBED, 7));
        if let Some(tokens) = context_tokens {
            config.context_tokens = tokens;
        }
        let server = Server::start(config).unwrap();
        let c = client(&server.url());
        (server, c)
    }

    fn embed_request(task: &str, inputs: &[&str]) -> EmbeddingsRequest {
        EmbeddingsRequest {
            model: EMBED.to_string(),
            task: task.to_string(),
            inputs: inputs.iter().map(|text| Content::Text((*text).to_string())).collect(),
            dimensions: None,
            dry_run: false,
            class: None,
            priority: None,
        }
    }

    /// One unit vector comes back for each input, in input order, at the
    /// largest dimensions the model offers, with the identity its vectors
    /// depend on.
    #[tokio::test]
    async fn embeddings_return_one_unit_vector_per_input_with_the_identity() {
        let (_server, c) = fake_embedder(None);
        let identity = c.council().identity().await.unwrap();
        let served = identity.route(EMBED).unwrap();
        let offered = served.embeddings.as_ref().expect("an embedding model lists what it embeds");
        let task = offered.tasks[0].clone();
        let r = c.council().embeddings(&embed_request(&task, &["alpha", "beta beta"]), Duration::from_secs(5)).await.unwrap();
        assert_eq!((r.model.as_str(), r.task.as_str()), (EMBED, task.as_str()));
        assert_eq!(r.dimensions, *offered.dimensions.iter().max().unwrap());
        assert_eq!(r.embeddings.iter().map(|e| e.index).collect::<Vec<_>>(), [0, 1]);
        for e in &r.embeddings {
            let vector = e.vector.as_ref().expect("a real request returns vectors");
            assert_eq!(vector.len(), r.dimensions as usize);
            assert!((gogi_proto::embeddings::norm(vector) - 1.0).abs() < 1e-5);
        }
        assert_eq!(r.identity.model, EMBED);
    }

    /// A dry run counts tokens and returns no vectors.
    #[tokio::test]
    async fn an_embeddings_dry_run_counts_tokens_and_returns_no_vectors() {
        let (_server, c) = fake_embedder(None);
        let identity = c.council().identity().await.unwrap();
        let task = identity.route(EMBED).unwrap().embeddings.as_ref().unwrap().tasks[0].clone();
        let request = EmbeddingsRequest { dry_run: true, ..embed_request(&task, &["alpha"]) };
        let r = c.council().embeddings(&request, Duration::from_secs(5)).await.unwrap();
        assert!(r.embeddings[0].vector.is_none() && r.embeddings[0].tokens > 0, "{r:?}");
    }

    /// The server's refusals reach the caller with their status and the
    /// parameter they name: a task it does not have is a 400 naming `task`,
    /// and an input past the model's limit is a 413 that embeds nothing.
    #[tokio::test]
    async fn the_servers_refusals_name_their_parameter() {
        let (_server, c) = fake_embedder(Some(64));
        let err = c.council().embeddings(&embed_request("no-such-task", &["alpha"]), Duration::from_secs(5)).await.unwrap_err();
        let MkError::Status { status: 400, error: Some(e), .. } = &err else { panic!("{err:?}") };
        assert_eq!(e.param.as_deref(), Some("task"));

        let identity = c.council().identity().await.unwrap();
        let task = identity.route(EMBED).unwrap().embeddings.as_ref().unwrap().tasks[0].clone();
        let long = "x".repeat(10_000);
        let err = c.council().embeddings(&embed_request(&task, &[&long]), Duration::from_secs(5)).await.unwrap_err();
        let MkError::Status { status: 413, error: Some(e), .. } = &err else { panic!("{err:?}") };
        assert_eq!((e.r#type, e.param.as_deref()), (ErrorType::TooLarge, Some("context_tokens")));
    }

    /// A greedy reply streams the same text a call without streaming
    /// returns, and the next request built on the reply keeps the reply's
    /// prefix. Thinking is off: with it on, a reply without calls loses its
    /// reasoning once a user message follows, so its end is no boundary.
    #[tokio::test]
    async fn a_streamed_reply_matches_the_plain_one_and_extends_the_context() {
        let (_server, c) = fake();
        let chat = || Context { thinking: Some(false), ..chat() };
        let plain = c.council().generate(&greedy(chat()), Duration::from_secs(5)).await.unwrap();
        let mut s = c.council().generate_stream(&greedy(chat()), Duration::from_secs(5)).await.unwrap();
        let mut tokens = 0;
        let done = loop {
            match s.next().await.unwrap().unwrap() {
                GenerationEvent::Token(_) => tokens += 1,
                GenerationEvent::Done(d) => break d,
            }
        };
        assert!(s.next().await.is_none());
        assert_eq!((done.content.as_str(), done.reasoning.as_str()), (plain.content.as_str(), plain.reasoning.as_str()));
        assert_eq!(tokens as u64, done.usage.completion);
        let mut next = chat();
        next.messages.push(done.as_message().expect("the reply made no unparsed call"));
        next.messages.push(Message::user("and then?"));
        let built = c.council().build_context(TOY, &next, false).await.unwrap();
        assert!(built.usage.kept > done.usage.prompt, "{built:?} after {:?}", done.usage);
    }

    /// The fake calls the first tool when the last message says `CALL`; the
    /// reply's call carries the id a tool message names.
    #[tokio::test]
    async fn a_reply_call_carries_its_id_into_the_next_context() {
        let (_server, c) = fake();
        let tool = gogi_proto::Tool::new("ls", json!({"type": "object", "properties": {}}));
        let context = Context { tools: vec![tool], messages: vec![Message::user("CALL ls")], ..Context::default() };
        let done = c.council().generate(&greedy(context.clone()), Duration::from_secs(5)).await.unwrap();
        let [ReplyToolCall::Parsed { id, name, .. }] = done.tool_calls.as_slice() else { panic!("{:?}", done.tool_calls) };
        assert_eq!(name, "ls");
        let mut next = context;
        next.messages.push(done.as_message().unwrap());
        next.messages.push(Message::tool(id.clone(), "a b c"));
        c.council().build_context(TOY, &next, false).await.unwrap();
    }

    #[tokio::test]
    async fn an_invalid_request_is_refused_before_it_is_sent() {
        let (base, seen) = serve(vec![]).await;
        let c = client(&base);
        let user_reasoning = Context { messages: vec![Message { reasoning: Some("r".into()), ..Message::user("u") }], ..Context::default() };
        assert!(matches!(c.council().build_context(TOY, &user_reasoning, false).await, Err(MkError::Request(_))));
        assert!(matches!(c.council().build_context(TOY, &Context::default(), false).await, Err(MkError::Request(_))));
        assert!(matches!(c.council().get_pins("a/b").await, Err(MkError::Request(_))));
        let forever = PinsPut { snapshots: vec![], ttl_s: 86_401 };
        assert!(matches!(c.council().put_pins("k", &forever).await, Err(MkError::Request(_))));
        let empty = GenerationRequest { max_tokens: 0, ..greedy(chat()) };
        assert!(matches!(c.council().generate(&empty, Duration::from_secs(1)).await, Err(MkError::Request(_))));
        let number = DecisionRequest { model: TOY.into(), case: json!(3), spec_id: gate_spec().id(), ..DecisionRequest::default() };
        assert!(matches!(c.council().decide_traced(&number, Duration::from_secs(1), None).await, Err(MkError::Request(_))));
        assert!(seen.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_spec_held_under_another_id_is_an_error() {
        let spec = gate_spec();
        let held = json!({"spec_id": format!("blake3:{}", "0".repeat(64)), "spec": spec.value});
        let (base, _) = serve(vec![reply(200, held.to_string())]).await;
        let err = client(&base).council().post_spec(&spec).await.unwrap_err();
        assert!(matches!(&err, MkError::SpecIdMismatch { computed, .. } if *computed == spec.id()), "{err:?}");
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
        let too_large = json!({"error": {"type": "too_large", "message": "case", "param": "case_bytes"}}).to_string();
        let (base, _) = serve(vec![reply(413, full.clone()), reply(413, full), reply(413, too_large)]).await;
        let c = client(&base);
        let err = c.council().generate_stream(&greedy(chat()), Duration::from_secs(5)).await.err().unwrap();
        let MkError::ContextFull { message, body, .. } = &err else { panic!("{err:?}") };
        assert!(message.contains("32760") && body.contains("context_tokens"), "{message} {body}");
        assert_eq!(err.status(), Some(413));
        let err = c.council().generate(&greedy(chat()), Duration::from_secs(5)).await.unwrap_err();
        assert!(matches!(err, MkError::ContextFull { .. }), "{err:?}");
        let err = c.council().generate(&greedy(chat()), Duration::from_secs(5)).await.unwrap_err();
        assert!(matches!(err, MkError::Status { status: 413, .. }), "another 413 stays a status: {err:?}");
    }

    /// The fake's context limit is reached by a long enough context, and
    /// the client reads its answer as [`MkError::ContextFull`].
    #[tokio::test]
    async fn the_fakes_context_limit_is_context_full() {
        let server = Server::start(Config { context_tokens: 256, ..Config::default() }).unwrap();
        let c = client(&server.url());
        let long = Context { messages: vec![Message::user("x".repeat(400))], ..Context::default() };
        let err = c.council().generate(&greedy(long), Duration::from_secs(5)).await.unwrap_err();
        assert!(matches!(err, MkError::ContextFull { .. }), "{err:?}");
    }

    /// A stream cut before `done` is truncated. An `error` event ends it as
    /// [`MkError::CouncilStream`] with the request id, and a `ping` or an
    /// event the contract does not name is skipped.
    ///
    /// Falsified by a ping or an unknown event that ends the stream, or an
    /// error event decoded in `/mk/v1`'s shape.
    #[tokio::test]
    async fn a_generation_cut_before_done_is_truncated_and_an_error_event_ends_it() {
        let token = json!({"id": 1, "piece": "Fi", "phase": "content", "p": 0.9, "h": 0.1, "n": 1, "tps": 10.0});
        let token = format!("event: ping\ndata: {{}}\n\nevent: token\ndata: {token}\n\nevent: rumor\ndata: {{}}\n\n");
        let error = r#"{"error":{"type":"model_unavailable","message":"the model unloaded","model":"m","status":"unloaded"},"request_id":"req_7"}"#;
        let (base, _) = serve(vec![sse(token.clone()), sse(format!("{token}event: error\ndata: {error}\n\n"))]).await;
        let c = client(&base);
        for want_stream_error in [false, true] {
            let mut s = c.council().generate_stream(&greedy(chat()), Duration::from_secs(5)).await.unwrap();
            let Some(Ok(GenerationEvent::Token(t))) = s.next().await else { panic!("a token first") };
            assert_eq!(t.phase, gogi_proto::generation::Phase::Content);
            let last = s.next().await.unwrap();
            match want_stream_error {
                false => assert!(matches!(last, Err(MkError::Truncated(_))), "{last:?}"),
                true => {
                    let Err(err) = last else { panic!("{last:?}") };
                    assert!(matches!(&err, MkError::CouncilStream { request_id: Some(id), .. } if id == "req_7"), "{err:?}");
                    assert_eq!(err.status(), Some(503));
                    assert!(err.to_string().contains("(request req_7)"), "{err}");
                }
            }
            assert!(s.next().await.is_none());
        }
    }

    /// An error status names its request by the `request-id` header, else
    /// by the body's `request_id`.
    #[tokio::test]
    async fn an_error_status_names_its_request() {
        let busy = json!({"error": {"type": "busy", "message": "full"}, "request_id": "req_body"}).to_string();
        let with_header = crate::test_server::Reply { headers: vec![("request-id", "req_head".into())], ..reply(429, busy.clone()) };
        let (base, _) = serve(vec![with_header, reply(429, busy)]).await;
        let c = client(&base);
        for want in ["req_head", "req_body"] {
            let err = c.council().identity().await.unwrap_err();
            assert!(matches!(&err, MkError::Status { status: 429, request_id: Some(id), .. } if id == want), "{err:?}");
            assert!(err.to_string().ends_with(&format!("(request {want})")), "{err}");
        }
    }
}
