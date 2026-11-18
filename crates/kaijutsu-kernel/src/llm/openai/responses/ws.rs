//! The Responses WebSocket transport for the ChatGPT plan route.
//!
//! A request is one `response.create` text frame holding the HTTP body's
//! fields without `stream`. The server answers with the same event objects
//! as the SSE stream, one per text frame, and [`super::Stream`] maps them
//! with the same state machine. The upgrade sends `Authorization`,
//! `session-id`, kaijutsu's `User-Agent`, and [`BETA`].
//!
//! Connections: [`Pool`] keeps at most one idle connection per cache
//! session, the context id. A request takes its session's connection out
//! of the pool, and the stream puts it back when the response completes, so
//! one response is in flight per connection. A second request for the same
//! session while one is in flight opens its own connection and sends its
//! full input rather than wait behind the first. A connection is not
//! reused after [`IDLE_LIMIT`] idle or [`AGE_LIMIT`] open, or once the
//! bearer it was opened with is replaced. A response that fails or is
//! cancelled closes its connection, so no late event of it reaches the
//! next request.
//!
//! Continuation: the server remembers the responses of one connection, so
//! a request that extends the last exchange on its connection sends
//! `previous_response_id` and only its new input items ([`continuation`]).
//! The plan route accepts `previous_response_id` only here, never over
//! HTTP.
//!
//! Fallback: a request is sent again only when the server cannot have run
//! it, at most once over WebSocket, before the first event reaches the
//! caller:
//!
//! - A pooled connection that closes before the first event (the server
//!   closed it while idle) resends in full on a new connection. A fresh
//!   connection that closes after the request went out is a network error
//!   with no resend, since the server may be running it.
//! - A rejected continuation (an error event whose refusal does not stand
//!   for any request), `previous_response_not_found`, or
//!   `websocket_connection_limit_reached` resends in full on a new
//!   connection.
//! - A first event that does not start a response drops the connection and
//!   resends in full.
//! - An upgrade that fails or takes longer than [`UPGRADE_LIMIT`], or a send
//!   that fails twice, goes over HTTP.
//!
//! A 401 at the upgrade goes back to the caller to refresh its bearer; an
//! error event with an HTTP status maps as the HTTP route's error status
//! would ([`super::open_error`]).

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use futures::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::{self, Message};
use tokio_tungstenite::{Connector, MaybeTlsStream, WebSocketStream};

use crate::llm::LlmError;
use crate::llm::endpoint::Slot;
use crate::llm::stream::StreamErrorKind;

use super::{Incoming, OpenError, Stream, open_error};

/// The beta header the upgrade carries.
pub const BETA: (&str, &str) = ("OpenAI-Beta", "responses_websockets=2026-02-06");

/// How long an idle connection is kept for its context's next request.
/// Ten minutes spans a person reading and answering a reply; a context
/// quiet for longer reconnects and sends its next request in full.
pub const IDLE_LIMIT: Duration = Duration::from_secs(10 * 60);

/// How long a connection is used at most. The server closes a connection
/// at 60 minutes with `websocket_connection_limit_reached`.
pub const AGE_LIMIT: Duration = Duration::from_secs(55 * 60);

/// Error codes that mean the request may succeed in full on a new
/// connection.
const RESEND_CODES: [&str; 2] = ["previous_response_not_found", "websocket_connection_limit_reached"];

/// The longest a WebSocket upgrade may take; a slower one goes over HTTP.
const UPGRADE_LIMIT: Duration = Duration::from_secs(30);

type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// When and with what bearer a connection was opened, and when it went idle.
#[derive(Debug, Clone)]
struct Lease {
    bearer: String,
    opened: Instant,
    idle_since: Instant,
}

impl Lease {
    fn expired(&self, now: Instant) -> bool {
        now.duration_since(self.idle_since) >= IDLE_LIMIT || now.duration_since(self.opened) >= AGE_LIMIT
    }
}

/// An open connection and what it last answered.
struct Connection {
    socket: Socket,
    lease: Lease,
    last: Option<Exchange>,
}

/// The idle connections of one backend, at most one per cache session.
#[derive(Default)]
pub struct Pool {
    idle: parking_lot::Mutex<HashMap<String, Connection>>,
    /// Set when the server answered an upgrade with 404, 405, or 426; every
    /// later request goes over HTTP.
    refused: AtomicBool,
}

impl std::fmt::Debug for Pool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Pool")
            .field("idle", &self.idle.lock().len())
            .field("refused", &self.refused.load(Ordering::Relaxed))
            .finish()
    }
}

impl Pool {
    /// Whether the server does not speak WebSocket.
    pub fn refused(&self) -> bool {
        self.refused.load(Ordering::Relaxed)
    }

    /// Take `session`'s idle connection if it was opened with `bearer` and
    /// has not expired. Expired connections of every session are closed.
    fn take(&self, session: &str, bearer: &str) -> Option<Connection> {
        let mut idle = self.idle.lock();
        let now = Instant::now();
        idle.retain(|_, c| !c.lease.expired(now));
        idle.remove(session).filter(|c| c.lease.bearer == bearer)
    }

    /// Keep `connection` idle for `session`'s next request, replacing any
    /// connection already kept for it.
    fn put(&self, session: String, mut connection: Connection) {
        let mut idle = self.idle.lock();
        let now = Instant::now();
        connection.lease.idle_since = now;
        idle.retain(|_, c| !c.lease.expired(now));
        idle.insert(session, connection);
    }
}

/// Where and how to connect.
pub struct Target<'a> {
    /// The `ws://` or `wss://` URL of `/responses`.
    pub url: String,
    /// Sent on the upgrade beside auth, `session-id`, user agent, and [`BETA`].
    pub headers: &'a [(&'static str, &'static str)],
}

impl Target<'_> {
    /// The WebSocket URL of `/responses` under an `http(s)://` base URL.
    pub fn url(base_url: &str) -> String {
        let base = base_url.trim_end_matches('/');
        let base = match base.split_once("://") {
            Some(("https", rest)) => format!("wss://{rest}"),
            Some(("http", rest)) => format!("ws://{rest}"),
            _ => base.to_string(),
        };
        format!("{base}/responses")
    }
}

/// Why a connection did not open.
enum ConnectError {
    /// The server answered the upgrade with an HTTP status.
    Refused { status: u16, body: String },
    /// No answer: DNS, TCP, TLS, a timeout, or a malformed handshake.
    Failed(String),
}

/// The rustls config for `wss://`: aws-lc-rs with the platform verifier.
fn tls() -> Result<Arc<rustls::ClientConfig>, String> {
    static CONFIG: std::sync::OnceLock<Result<Arc<rustls::ClientConfig>, String>> = std::sync::OnceLock::new();
    CONFIG
        .get_or_init(|| {
            use rustls_platform_verifier::BuilderVerifierExt;
            let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
            rustls::ClientConfig::builder_with_provider(provider)
                .with_safe_default_protocol_versions()
                .map_err(|e| e.to_string())?
                .with_platform_verifier()
                .map_err(|e| e.to_string())
                .map(|b| Arc::new(b.with_no_client_auth()))
        })
        .clone()
}

async fn connect(target: &Target<'_>, session: &str, bearer: &str, wait: Duration) -> Result<Connection, ConnectError> {
    let failed = |e: &dyn std::fmt::Display| ConnectError::Failed(super::redact(&e.to_string(), bearer));
    let mut request = target.url.as_str().into_client_request().map_err(|e| failed(&e))?;
    let headers = request.headers_mut();
    let value = |v: &str| HeaderValue::from_str(v).map_err(|e| failed(&e));
    headers.insert("authorization", value(&format!("Bearer {bearer}"))?);
    headers.insert("session-id", value(session)?);
    headers.insert("user-agent", value(crate::llm::http_user_agent())?);
    headers.insert(BETA.0, value(BETA.1)?);
    for (name, v) in target.headers {
        headers.insert(*name, value(v)?);
    }
    let connector = Connector::Rustls(tls().map_err(|e| failed(&e))?);
    let opening = tokio_tungstenite::connect_async_tls_with_config(request, None, true, Some(connector));
    match tokio::time::timeout(wait, opening).await {
        Err(_) => Err(ConnectError::Failed(format!("no upgrade within {wait:?}"))),
        Ok(Ok((socket, _))) => {
            let now = Instant::now();
            Ok(Connection { socket, lease: Lease { bearer: bearer.to_string(), opened: now, idle_since: now }, last: None })
        }
        Ok(Err(tungstenite::Error::Http(response))) => Err(ConnectError::Refused {
            status: response.status().as_u16(),
            body: response.body().as_deref().map(|b| String::from_utf8_lossy(b).into_owned()).unwrap_or_default(),
        }),
        Ok(Err(e)) => Err(failed(&e)),
    }
}

/// The first frame of a response.
enum First {
    Text(String),
    /// The connection closed or failed before any event.
    Lost(String),
    TimedOut,
}

async fn first_frame(socket: &mut Socket, wait: Duration) -> First {
    let deadline = tokio::time::Instant::now() + wait;
    loop {
        match tokio::time::timeout_at(deadline, socket.next()).await {
            Err(_) => return First::TimedOut,
            Ok(Some(Ok(Message::Text(text)))) => return First::Text(text.to_string()),
            Ok(Some(Ok(Message::Ping(_) | Message::Pong(_) | Message::Frame(_)))) => {}
            Ok(Some(Ok(Message::Binary(_)))) => return First::Lost("a binary frame".into()),
            Ok(Some(Ok(Message::Close(_))) | None) => return First::Lost("the connection closed".into()),
            Ok(Some(Err(e))) => return First::Lost(e.to_string()),
        }
    }
}

/// How [`open`] ended.
pub(crate) enum Opened {
    Stream(Box<Stream>),
    /// Send the request over HTTP with this slot.
    Http(Option<Slot>),
}

/// A request's cache session and full body, for [`open`].
pub(crate) struct Request<'a> {
    pub session: &'a str,
    pub body: &'a Value,
    pub bearer: &'a str,
    /// How long to wait for the upgrade and for the first event.
    pub wait: Duration,
}

/// Send `request` on its session's connection, or a new one, and wait for
/// the first event. See the module doc for the fallback rules.
pub(crate) async fn open(
    pool: &Arc<Pool>,
    target: &Target<'_>,
    request: Request<'_>,
    slot: Option<Slot>,
) -> Result<Opened, OpenError> {
    let Request { session, body, bearer, wait } = request;
    let mut kept = pool.take(session, bearer);
    for attempt in 0..2 {
        let reused = kept.is_some();
        let mut conn = match kept.take() {
            Some(conn) => conn,
            None => match connect(target, session, bearer, wait.min(UPGRADE_LIMIT)).await {
                Ok(conn) => conn,
                Err(ConnectError::Refused { status, body: text }) => {
                    if let Some(slot) = &slot {
                        slot.answered_with(status, None);
                    }
                    let status = reqwest::StatusCode::from_u16(status).unwrap_or(reqwest::StatusCode::BAD_GATEWAY);
                    let error = open_error(status, &text, bearer);
                    if refusal_stands(&error) {
                        return Err(error);
                    }
                    if matches!(status.as_u16(), 404 | 405 | 426) {
                        pool.refused.store(true, Ordering::Relaxed);
                    }
                    tracing::warn!(session, %status, "the Responses WebSocket upgrade was refused; sending over HTTP");
                    return Ok(Opened::Http(slot));
                }
                Err(ConnectError::Failed(why)) => {
                    tracing::warn!(session, why, "the Responses WebSocket did not open; sending over HTTP");
                    return Ok(Opened::Http(slot));
                }
            },
        };
        let continued = conn.last.as_ref().and_then(|last| continuation(last, body));
        let frame = frame(body, continued.as_ref());
        tracing::info!(
            session,
            attempt,
            reused,
            incremental = continued.is_some(),
            items = frame["input"].as_array().map_or(0, Vec::len),
            previous_response_id = continued.as_ref().map(|c| c.previous_response_id.as_str()),
            "responses websocket request"
        );
        if let Err(e) = conn.socket.send(Message::text(frame.to_string())).await {
            tracing::debug!(session, error = %e, "the Responses WebSocket send failed; sending in full on a new connection");
            continue;
        }
        let text = match first_frame(&mut conn.socket, wait).await {
            First::Text(text) => text,
            // A pooled connection the server closed while idle never read the
            // request. A fresh one may have, and the server may be running it,
            // so it is not sent again.
            First::Lost(why) if reused && attempt == 0 => {
                tracing::debug!(session, why, "the pooled Responses WebSocket closed before the first event; sending in full on a new connection");
                continue;
            }
            First::Lost(why) => {
                return Err(OpenError::Other(LlmError::NetworkError(format!(
                    "the Responses WebSocket closed after the request was sent ({why}); not sending it again"
                ))));
            }
            First::TimedOut => {
                return Err(OpenError::Other(LlmError::NetworkError(format!(
                    "the Responses WebSocket sent no event within {wait:?}"
                ))));
            }
        };
        let event: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
        match event["type"].as_str() {
            Some("error") => {
                let code = error_code(&event);
                let status = event
                    .get("status")
                    .and_then(Value::as_u64)
                    .and_then(|s| u16::try_from(s).ok())
                    .filter(|s| !(200..300).contains(s));
                let error = status.map(|s| {
                    open_error(reqwest::StatusCode::from_u16(s).unwrap_or(reqwest::StatusCode::BAD_GATEWAY), &text, bearer)
                });
                // The server refused the request outright, so it ran nothing:
                // a rejected continuation, or a lost previous response, goes
                // once more in full on a new connection.
                let rejected_continuation = continued.is_some() && error.as_ref().is_some_and(|e| !refusal_stands(e));
                if attempt == 0 && (RESEND_CODES.contains(&code) || rejected_continuation) {
                    tracing::debug!(session, code, "sending in full on a new connection");
                    continue;
                }
                if let (Some(status), Some(error)) = (status, error) {
                    if let Some(slot) = &slot {
                        slot.answered_with(status, None);
                    }
                    return Err(error);
                }
            }
            Some("response.created" | "response.in_progress") => {}
            // Not the start of this response: a frame left from an earlier
            // one. The connection cannot be trusted to pair events with
            // requests, so it is dropped.
            other if attempt == 0 => {
                tracing::warn!(session, first = ?other, "the Responses WebSocket answered with an event that does not start a response; sending in full on a new connection");
                continue;
            }
            other => {
                return Err(OpenError::Other(LlmError::NetworkError(format!(
                    "the Responses WebSocket answered with {other:?}, not the start of a response"
                ))));
            }
        }
        let live = Live {
            socket: conn.socket,
            lease: conn.lease,
            pool: pool.clone(),
            session: session.to_string(),
            body: body.clone(),
            response_id: None,
            output: Vec::new(),
        };
        return Ok(Opened::Stream(Box::new(Stream::from_websocket(live, &text, slot, bearer))));
    }
    tracing::warn!(session, "the Responses WebSocket failed twice; sending over HTTP");
    Ok(Opened::Http(slot))
}

/// The `code` of an error event, under `error` or at the top level.
fn error_code(event: &Value) -> &str {
    event
        .pointer("/error/code")
        .or_else(|| event.get("code"))
        .and_then(Value::as_str)
        .unwrap_or("")
}

/// Whether an upgrade refusal is the answer to the request itself, which
/// HTTP would give too: an expired bearer, a spent plan, a refused sign-in,
/// or a rate limit.
fn refusal_stands(error: &OpenError) -> bool {
    matches!(
        error,
        OpenError::Unauthorized(_)
            | OpenError::Other(
                LlmError::InsufficientBalance(_) | LlmError::AuthError(_) | LlmError::RateLimited(_) | LlmError::ContextFull(_)
            )
    )
}

/// The `response.create` frame for `body`, continuing a previous response
/// when `continued` is given.
fn frame(body: &Value, continued: Option<&Continuation>) -> Value {
    let mut frame = body.clone();
    if let Some(map) = frame.as_object_mut() {
        map.remove("stream");
        map.insert("type".into(), Value::String("response.create".into()));
        if let Some(continued) = continued {
            map.insert("previous_response_id".into(), Value::String(continued.previous_response_id.clone()));
            map.insert("input".into(), Value::Array(continued.input.clone()));
        }
    }
    frame
}

/// A connection with a response in flight.
pub(crate) struct Live {
    socket: Socket,
    lease: Lease,
    pool: Arc<Pool>,
    session: String,
    /// The request's full body.
    body: Value,
    response_id: Option<String>,
    output: Vec<Value>,
}

impl Live {
    /// The next frame.
    pub(super) async fn next(&mut self) -> Incoming {
        match self.socket.next().await {
            Some(Ok(Message::Text(text))) => Incoming::Data(text.to_string()),
            Some(Ok(Message::Binary(_))) => {
                Incoming::Failed(StreamErrorKind::Protocol, "the Responses WebSocket sent a binary frame".into())
            }
            Some(Ok(Message::Close(_))) | None => Incoming::Closed,
            Some(Ok(_)) => Incoming::Skip,
            Some(Err(e)) => Incoming::Failed(StreamErrorKind::Transport, format!("WebSocket transport: {e}")),
        }
    }

    /// Note what a continuation needs from `event`: the response id and the
    /// output items.
    pub(super) fn note(&mut self, event: &Value) {
        match event.get("type").and_then(Value::as_str) {
            Some("response.created") => {
                self.response_id = event.pointer("/response/id").and_then(Value::as_str).map(str::to_string);
            }
            Some("response.output_item.done") => {
                if let Some(item) = event.get("item") {
                    self.output.push(item.clone());
                }
            }
            // The completed response lists its whole output; it outranks the
            // items gathered one by one.
            Some("response.completed") => {
                if let Some(output) = event.pointer("/response/output").and_then(Value::as_array).filter(|o| !o.is_empty()) {
                    self.output = output.clone();
                }
            }
            _ => {}
        }
    }

    /// The response completed: keep the connection for the session's next
    /// request.
    pub(super) fn keep(self) {
        let last = self.response_id.map(|id| Exchange::after(&self.body, &self.output, id));
        let conn = Connection { socket: self.socket, lease: self.lease, last };
        self.pool.put(self.session, conn);
    }
}

/// What a connection last answered: enough to tell whether the next
/// request continues it.
#[derive(Debug, Clone)]
pub(crate) struct Exchange {
    /// The request's fields other than `input`.
    props: Value,
    /// The request's full input, then the response's output, as [`units`].
    units: Vec<Value>,
    /// The response's id, which a continuation names.
    response_id: String,
}

impl Exchange {
    /// The exchange of a request with full body `body` answered by response
    /// `response_id` with `output` items.
    fn after(body: &Value, output: &[Value], response_id: String) -> Self {
        let mut units: Vec<Value> = units(input(body)).into_iter().map(|u| u.value).collect();
        units.extend(self::units(output).into_iter().map(|u| u.value));
        Self { props: props(body), units, response_id }
    }
}

/// A request that continues the last exchange on its connection.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Continuation {
    pub previous_response_id: String,
    /// The input items after the last exchange.
    pub input: Vec<Value>,
}

/// One comparable piece of an input item list.
#[derive(Debug, Clone, PartialEq)]
struct Unit {
    value: Value,
    /// The index of the item it came from.
    item: usize,
    /// Whether it is that item's first piece.
    first: bool,
}

fn input(body: &Value) -> &[Value] {
    body.get("input").and_then(Value::as_array).map_or(&[], Vec::as_slice)
}

/// A body's fields other than `input` and `stream`.
fn props(body: &Value) -> Value {
    let mut props = body.clone();
    if let Some(map) = props.as_object_mut() {
        map.remove("input");
        map.remove("stream");
    }
    props
}

/// The comparable pieces of `items`.
///
/// Reasoning items are left out: the request builder drops reasoning from
/// before the current turn, while a continued response keeps it on the
/// server. A message gives one piece per content part, so a message the
/// server sent as two items compares equal to one the builder joined. Ids,
/// statuses, and annotations are left out, and function call arguments
/// compare as JSON.
fn units(items: &[Value]) -> Vec<Unit> {
    let mut out = Vec::new();
    for (i, item) in items.iter().enumerate() {
        let kind = item.get("type").and_then(Value::as_str).unwrap_or("");
        let field = |name: &str| item.get(name).cloned().unwrap_or(Value::Null);
        let pieces: Vec<Value> = match kind {
            "reasoning" => continue,
            "message" => match item.get("content") {
                Some(Value::Array(parts)) => {
                    parts.iter().map(|p| json!({ "message": field("role"), "part": part(p) })).collect()
                }
                other => vec![json!({ "message": field("role"), "part": other.cloned().unwrap_or(Value::Null) })],
            },
            "function_call" => vec![json!({
                "type": kind,
                "call_id": field("call_id"),
                "name": field("name"),
                "arguments": arguments(&field("arguments")),
            })],
            "function_call_output" => {
                vec![json!({ "type": kind, "call_id": field("call_id"), "output": field("output") })]
            }
            _ => {
                let mut other = item.clone();
                if let Some(map) = other.as_object_mut() {
                    map.remove("id");
                    map.remove("status");
                }
                vec![other]
            }
        };
        for (n, value) in pieces.into_iter().enumerate() {
            out.push(Unit { value, item: i, first: n == 0 });
        }
    }
    out
}

/// A message content part with only the fields that carry its content.
fn part(part: &Value) -> Value {
    let mut kept = serde_json::Map::new();
    for name in ["type", "text", "image_url", "refusal"] {
        if let Some(value) = part.get(name) {
            kept.insert(name.to_string(), value.clone());
        }
    }
    Value::Object(kept)
}

/// Function call arguments as JSON; empty arguments are `{}`, as the stream
/// reads them.
fn arguments(arguments: &Value) -> Value {
    match arguments.as_str() {
        Some(text) if text.trim().is_empty() => json!({}),
        Some(text) => serde_json::from_str(text).unwrap_or_else(|_| arguments.clone()),
        None => arguments.clone(),
    }
}

/// The continuation of `last` that `body` is, if any.
///
/// `body` continues `last` when its fields other than `input` are the same,
/// and its input's [`units`] start with the last request's input followed by
/// the last response's output, and go on with at least one more. The
/// continuation sends the input items from the first of those new units on.
/// Anything else, such as an edited or excluded message, a new system
/// prompt, or a changed model, effort, or tool set, is sent in full.
pub(crate) fn continuation(last: &Exchange, body: &Value) -> Option<Continuation> {
    if props(body) != last.props {
        return None;
    }
    let items = input(body);
    let units = units(items);
    let known = last.units.len();
    let next = units.get(known)?;
    if !next.first || units[..known].iter().map(|u| &u.value).ne(last.units.iter()) {
        return None;
    }
    Some(Continuation { previous_response_id: last.response_id.clone(), input: items[next.item..].to_vec() })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::openai::responses::build::{REASONING_SIGNATURE_PREFIX, build_request};
    use crate::llm::stream::BuildOpts;
    use crate::llm::{ContentBlock, Message, MessageContent, Role, ToolDefinition};

    fn opts() -> BuildOpts {
        BuildOpts::new("gpt-6.1-sol", 1).with_system("s").with_effort("high").with_tools(vec![tool("ls")])
    }

    fn tool(name: &str) -> ToolDefinition {
        ToolDefinition { name: name.into(), description: "d".into(), input_schema: json!({ "type": "object" }) }
    }

    fn body(opts: &BuildOpts, history: &[Message]) -> Value {
        build_request(opts, history, "test")
    }

    /// The assistant message kaijutsu stores for a response that reasoned
    /// (`enc`), then said `text` and called `call`.
    fn assistant(enc: &str, text: Option<&str>, call: Option<&str>) -> Message {
        let mut blocks = vec![ContentBlock::Reasoning {
            text: "thought".into(),
            signature: Some(format!("{REASONING_SIGNATURE_PREFIX}{enc}")),
        }];
        if let Some(text) = text {
            blocks.push(ContentBlock::Text { text: text.into() });
        }
        if let Some(call) = call {
            blocks.push(ContentBlock::ToolUse { id: call.into(), name: "ls".into(), input: json!({ "path": "/" }), signature: None });
        }
        Message { role: Role::Assistant, content: MessageContent::Blocks(blocks), turn_scoped: false }
    }

    /// The output items the server sends for the same response, with the
    /// fields the builder never writes.
    fn output(enc: &str, text: Option<&str>, call: Option<&str>) -> Vec<Value> {
        let mut items = vec![json!({
            "type": "reasoning", "id": "rs_1", "encrypted_content": enc,
            "summary": [{ "type": "summary_text", "text": "thought" }],
        })];
        if let Some(text) = text {
            items.push(json!({
                "type": "message", "id": "msg_1", "status": "completed", "role": "assistant",
                "content": [{ "type": "output_text", "text": text, "annotations": [], "logprobs": [] }],
            }));
        }
        if let Some(call) = call {
            items.push(json!({
                "type": "function_call", "id": "fc_1", "status": "completed", "call_id": call,
                "name": "ls", "arguments": "{ \"path\": \"/\" }",
            }));
        }
        items
    }

    fn result(call: &str) -> Message {
        Message::tool_results(vec![ContentBlock::ToolResult { tool_use_id: call.into(), content: "ok".into(), is_error: false }])
    }

    /// A tool loop within a turn continues: the next request sends only the
    /// tool output.
    ///
    /// Falsified by a full resend, or a delta that repeats the call.
    #[test]
    fn a_tool_loop_continues_with_only_the_tool_output() {
        let opts = opts();
        let first = [Message::user("go")];
        let last = Exchange::after(&body(&opts, &first), &output("R1", None, Some("call_1")), "resp_1".into());
        let next = [Message::user("go"), assistant("R1", None, Some("call_1")), result("call_1")];
        let cont = continuation(&last, &body(&opts, &next)).expect("a tool loop continues");
        assert_eq!(cont.previous_response_id, "resp_1");
        assert_eq!(cont.input, [json!({ "type": "function_call_output", "call_id": "call_1", "output": "ok" })]);
    }

    /// A new user turn continues although the builder dropped the last
    /// turn's reasoning from the input.
    ///
    /// Falsified by a full resend because the reasoning item is missing.
    #[test]
    fn a_new_turn_continues_without_the_dropped_reasoning() {
        let opts = opts();
        let first = [Message::user("one")];
        let full = body(&opts, &first);
        let last = Exchange::after(&full, &output("R1", Some("hello"), None), "resp_1".into());
        let next = [Message::user("one"), assistant("R1", Some("hello"), None), Message::user("two")];
        let next_body = body(&opts, &next);
        assert!(input(&next_body).iter().all(|i| i["type"] != "reasoning"), "the builder keeps old reasoning: {next_body}");
        let cont = continuation(&last, &next_body).expect("a new turn continues");
        assert_eq!(cont.input, [json!({ "type": "message", "role": "user", "content": [{ "type": "input_text", "text": "two" }] })]);
    }

    /// The second step of a turn continues from the first step's request,
    /// whose input held reasoning.
    #[test]
    fn a_second_tool_round_continues() {
        let opts = opts();
        let step = [Message::user("go"), assistant("R1", None, Some("call_1")), result("call_1")];
        let last = Exchange::after(&body(&opts, &step), &output("R2", None, Some("call_2")), "resp_2".into());
        let next = [
            Message::user("go"), assistant("R1", None, Some("call_1")), result("call_1"),
            assistant("R2", None, Some("call_2")), result("call_2"),
        ];
        let cont = continuation(&last, &body(&opts, &next)).expect("the loop continues");
        assert_eq!(cont.previous_response_id, "resp_2");
        assert_eq!(cont.input, [json!({ "type": "function_call_output", "call_id": "call_2", "output": "ok" })]);
    }

    /// A changed model, effort, or tool set resends in full.
    #[test]
    fn changed_request_fields_resend_in_full() {
        let first = [Message::user("go")];
        let next = [Message::user("go"), assistant("R1", None, Some("call_1")), result("call_1")];
        let last = Exchange::after(&body(&opts(), &first), &output("R1", None, Some("call_1")), "resp_1".into());
        let changed = [
            ("model", BuildOpts::new("gpt-6.2-sol", 1).with_system("s").with_effort("high").with_tools(vec![tool("ls")])),
            ("effort", opts().with_effort("low")),
            ("tools", opts().with_tools(vec![tool("ls"), tool("cat")])),
        ];
        for (what, opts) in changed {
            assert_eq!(continuation(&last, &body(&opts, &next)), None, "a changed {what} must resend in full");
        }
    }

    /// An edited or excluded earlier message resends in full, and so does a
    /// changed system prompt.
    #[test]
    fn an_edited_history_resends_in_full() {
        let opts = opts();
        let first = [Message::user("one"), Message::assistant("hi"), Message::user("two")];
        let last = Exchange::after(&body(&opts, &first), &output("R1", Some("ok"), None), "resp_1".into());
        let edited = [Message::user("ONE"), Message::assistant("hi"), Message::user("two"), Message::assistant("ok"), Message::user("three")];
        assert_eq!(continuation(&last, &body(&opts, &edited)), None, "an edit");
        let excluded = [Message::user("two"), Message::assistant("ok"), Message::user("three")];
        assert_eq!(continuation(&last, &body(&opts, &excluded)), None, "an exclusion");
        let same = [Message::user("one"), Message::assistant("hi"), Message::user("two"), Message::assistant("ok"), Message::user("three")];
        assert!(continuation(&last, &body(&opts, &same)).is_some(), "the unedited history continues");
        let system = opts.clone().with_system("t");
        assert_eq!(continuation(&last, &body(&system, &same)), None, "a new system prompt");
    }

    /// A request with nothing after the last exchange resends in full rather
    /// than send an empty input.
    #[test]
    fn a_repeated_request_resends_in_full() {
        let opts = opts();
        let first = [Message::user("go")];
        let last = Exchange::after(&body(&opts, &first), &output("R1", Some("hi"), None), "resp_1".into());
        let again = [Message::user("go"), assistant("R1", Some("hi"), None)];
        assert_eq!(continuation(&last, &body(&opts, &again)), None);
    }

    /// History that differs from what the server answered, such as a text
    /// the runtime stored differently, resends in full.
    #[test]
    fn a_response_stored_differently_resends_in_full() {
        let opts = opts();
        let last = Exchange::after(&body(&opts, &[Message::user("go")]), &output("R1", Some("hi"), None), "resp_1".into());
        let next = [Message::user("go"), assistant("R1", Some("hi there"), None), Message::user("more")];
        assert_eq!(continuation(&last, &body(&opts, &next)), None);
    }

    /// A connection expires after the idle limit or the age limit.
    #[test]
    fn a_lease_expires_when_idle_or_old() {
        let now = Instant::now();
        let lease = |opened: Duration, idle: Duration| Lease { bearer: "b".into(), opened: now - opened, idle_since: now - idle };
        let minute = Duration::from_secs(60);
        assert!(!lease(minute, minute).expired(now));
        assert!(lease(IDLE_LIMIT, IDLE_LIMIT).expired(now), "idle");
        assert!(lease(AGE_LIMIT, minute).expired(now), "old");
    }

    /// The WebSocket URL follows the base URL's scheme.
    #[test]
    fn the_url_follows_the_scheme() {
        assert_eq!(Target::url("https://api.openai.com/v1"), "wss://api.openai.com/v1/responses");
        assert_eq!(Target::url("http://127.0.0.1:9/v1/"), "ws://127.0.0.1:9/v1/responses");
    }

    /// A frame is the body as `response.create` without `stream`; a
    /// continuation adds the previous response and replaces the input.
    #[test]
    fn a_frame_is_the_body_as_response_create() {
        let full = body(&opts(), &[Message::user("go")]);
        let sent = frame(&full, None);
        assert_eq!(sent["type"], "response.create");
        assert!(sent.get("stream").is_none() && sent.get("previous_response_id").is_none(), "{sent}");
        assert_eq!(sent["input"], full["input"]);
        let delta = vec![json!({ "type": "function_call_output", "call_id": "c", "output": "ok" })];
        let continued = Continuation { previous_response_id: "resp_1".into(), input: delta.clone() };
        let sent = frame(&full, Some(&continued));
        assert_eq!(sent["previous_response_id"], "resp_1");
        assert_eq!(sent["input"], Value::Array(delta));
        assert_eq!(sent["model"], full["model"]);
    }
}

/// A scripted Responses WebSocket server for tests.
#[cfg(test)]
#[allow(clippy::result_large_err)]
pub(crate) mod test_server {
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    use futures::{SinkExt, StreamExt};
    use serde_json::Value;
    use tokio::net::TcpListener;
    use tokio_tungstenite::tungstenite::Message;
    use tokio_tungstenite::tungstenite::handshake::server::{ErrorResponse, Request, Response};

    /// What the server does at an upgrade or a request frame.
    pub enum Answer {
        /// Accept the upgrade.
        Accept,
        /// Refuse the upgrade with this status and body.
        Refuse(u16, &'static str),
        /// Send these events, one text frame each, and keep the connection.
        Events(Vec<Value>),
        /// Close the connection without answering.
        Close,
    }

    /// What the server is answering.
    pub enum Ask<'a> {
        /// An upgrade, with its lowercase headers.
        Upgrade(&'a HashMap<String, String>),
        /// The `n`th request frame on the connection.
        Frame(usize),
    }

    /// One connection the server saw.
    #[derive(Debug, Clone, Default)]
    pub struct Seen {
        pub path: String,
        pub headers: HashMap<String, String>,
        pub refused: bool,
        pub frames: Vec<Value>,
    }

    type Answerer = Box<dyn FnMut(usize, Ask<'_>) -> Answer + Send>;

    /// Serve on a local port; `answer` gets each connection's index and what
    /// it asks. Returns the `http://` base and every connection seen.
    pub async fn serve(
        answer: impl FnMut(usize, Ask<'_>) -> Answer + Send + 'static,
    ) -> (String, Arc<Mutex<Vec<Seen>>>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let seen: Arc<Mutex<Vec<Seen>>> = Arc::default();
        let answer: Arc<Mutex<Answerer>> = Arc::new(Mutex::new(Box::new(answer)));
        let log = seen.clone();
        tokio::spawn(async move {
            loop {
                let (socket, _) = listener.accept().await.unwrap();
                let index = {
                    let mut log = log.lock().unwrap();
                    log.push(Seen::default());
                    log.len() - 1
                };
                let (log, answer) = (log.clone(), answer.clone());
                tokio::spawn(async move {
                    let upgrade = |request: &Request, response: Response| -> Result<Response, ErrorResponse> {
                        let headers: HashMap<String, String> = request
                            .headers()
                            .iter()
                            .map(|(n, v)| (n.as_str().to_lowercase(), v.to_str().unwrap_or("").to_string()))
                            .collect();
                        let verdict = (answer.lock().unwrap())(index, Ask::Upgrade(&headers));
                        let mut log = log.lock().unwrap();
                        log[index].path = request.uri().path().to_string();
                        log[index].headers = headers;
                        match verdict {
                            Answer::Refuse(status, body) => {
                                log[index].refused = true;
                                let mut refusal = ErrorResponse::new(Some(body.to_string()));
                                *refusal.status_mut() = status.try_into().unwrap();
                                Err(refusal)
                            }
                            _ => Ok(response),
                        }
                    };
                    let Ok(mut ws) = tokio_tungstenite::accept_hdr_async(socket, upgrade).await else { return };
                    let mut n = 0;
                    while let Some(Ok(message)) = ws.next().await {
                        let Message::Text(text) = message else { continue };
                        let frame: Value = serde_json::from_str(&text).unwrap();
                        log.lock().unwrap()[index].frames.push(frame.clone());
                        let verdict = (answer.lock().unwrap())(index, Ask::Frame(n));
                        n += 1;
                        match verdict {
                            Answer::Events(events) => {
                                for event in events {
                                    if ws.send(Message::text(event.to_string())).await.is_err() {
                                        return;
                                    }
                                }
                            }
                            Answer::Close => {
                                let _ = ws.close(None).await;
                                return;
                            }
                            Answer::Accept | Answer::Refuse(..) => {}
                        }
                    }
                });
            }
        });
        (base, seen)
    }
}
