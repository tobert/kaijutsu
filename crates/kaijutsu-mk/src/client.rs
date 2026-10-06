//! The HTTP transport every megakernel call shares.
//!
//! Every failure is a typed [`MkError`]. The client never retries and never
//! turns a failure into a default answer; a caller that wants to retry reads
//! `retry_after` and decides. Each route family adds its calls in its own
//! `impl MkClient` block.

use std::time::Duration;

use reqwest::header::{HeaderName, HeaderValue, CONTENT_TYPE, RETRY_AFTER};
use reqwest::{Method, StatusCode};
use serde::de::DeserializeOwned;

use crate::council::wire::{ErrorBody, ErrorDetail, InvalidRequest, SnapshotId, SpecId};
use crate::json::Json;

/// Longest slice of a response body kept in an error.
const BODY_EXCERPT: usize = 512;

/// Why a call failed.
#[derive(Debug, thiserror::Error)]
pub enum MkError {
    /// A `/council/v1` route answered with an error status. `error` is the
    /// decoded body when it matched the council contract (it carries `head`
    /// for 409 and 412); `body` is the start of the raw text either way.
    #[error("megakernel answered {status}{}", .error.as_ref().map(|e| format!(": {:?}: {}", e.r#type, e.message)).unwrap_or_default())]
    Status {
        status: u16,
        error: Option<ErrorDetail>,
        /// The `Retry-After` header, when it held whole seconds.
        retry_after: Option<Duration>,
        body: String,
    },
    /// A `/mk/v1` route answered with an error status. `error` is the decoded
    /// body when it matched the service's error schema; `body` is the start of
    /// the raw text either way.
    #[error("megakernel answered {status}{}", .error.as_ref().map(|e| format!(": {}: {}", e.r#type, e.message)).unwrap_or_default())]
    Service {
        status: u16,
        error: Option<ServiceError>,
        /// The `Retry-After` header, when it held whole seconds.
        retry_after: Option<Duration>,
        body: String,
    },
    /// The server failed after a stream began and said so in an `error` event.
    /// A 503 of type `pass_timeout_error` dropped only this request.
    #[error("megakernel failed mid-stream ({}): {}: {}", .0.code, .0.r#type, .0.message)]
    Stream(ServiceError),
    /// The stream ended before its `done` event. What arrived is not a reply.
    #[error("megakernel stream ended before its done event: {0}")]
    Truncated(String),
    /// The call took longer than the client's timeout.
    #[error("megakernel call timed out")]
    Timeout,
    /// The request did not reach the server or the reply did not arrive whole.
    #[error("megakernel transport failed: {0}")]
    Transport(String),
    /// The server answered success with a body outside the schema, or with a
    /// success status the contract does not give for the call.
    #[error("megakernel reply outside the schema ({what}): {message}")]
    Decode {
        what: &'static str,
        message: String,
        body: String,
    },
    /// The client refused to send a request the contract forbids.
    #[error("{0}")]
    Request(String),
    /// The server held a spec under a different id than the canonical one.
    #[error("spec id disagrees: computed {computed}, server says {server}")]
    SpecIdMismatch { computed: SpecId, server: SpecId },
}

impl MkError {
    /// For a 409 or 412, the context's current head.
    pub fn head(&self) -> Option<&SnapshotId> {
        match self {
            MkError::Status { error: Some(e), .. } => e.head.as_ref(),
            _ => None,
        }
    }

    /// The HTTP status, when the server answered with one.
    pub fn status(&self) -> Option<u16> {
        match self {
            MkError::Status { status, .. } | MkError::Service { status, .. } => Some(*status),
            MkError::Stream(e) => Some(e.code),
            _ => None,
        }
    }
}

impl From<InvalidRequest> for MkError {
    fn from(e: InvalidRequest) -> Self {
        MkError::Request(e.to_string())
    }
}

/// The inside of a `/mk/v1` error body, `{"error": {...}}`. `type` is the
/// service's own name for the failure, such as `pass_timeout_error`.
#[derive(Clone, Debug, PartialEq, Eq, serde::Deserialize)]
pub struct ServiceError {
    pub code: u16,
    pub message: String,
    pub r#type: String,
}

#[derive(serde::Deserialize)]
pub(crate) struct ServiceErrorBody {
    pub(crate) error: ServiceError,
}

/// Which error schema a route's failures follow.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Family {
    Council,
    Mk,
}

pub(crate) fn excerpt(body: &[u8]) -> String {
    let text = String::from_utf8_lossy(body);
    text.chars().take(BODY_EXCERPT).collect()
}

/// A client for one megakernel service.
#[derive(Clone, Debug)]
pub struct MkClient {
    http: reqwest::Client,
    base: String,
    pub(crate) timeout: Duration,
    traceparent: Option<String>,
}

impl MkClient {
    /// A client for the service at `base_url` (such as `http://localhost:8090`).
    /// `timeout` bounds each call that does not take its own. The client
    /// ignores proxy environment variables: the service is addressed directly.
    pub fn new(base_url: impl Into<String>, timeout: Duration) -> Result<Self, MkError> {
        let http = reqwest::Client::builder()
            .no_proxy()
            .build()
            .map_err(|e| MkError::Transport(e.to_string()))?;
        Ok(MkClient {
            http,
            base: base_url.into().trim_end_matches('/').to_string(),
            timeout,
            traceparent: None,
        })
    }

    /// Sends this W3C `traceparent` header on every call.
    pub fn with_traceparent(mut self, traceparent: impl Into<String>) -> Self {
        self.traceparent = Some(traceparent.into());
        self
    }

    /// A call whose success is one JSON body.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn json<T: DeserializeOwned>(
        &self,
        family: Family,
        method: Method,
        path: &str,
        body: Option<Vec<u8>>,
        headers: &[(&'static str, String)],
        timeout: Duration,
        expect: u16,
        what: &'static str,
    ) -> Result<T, MkError> {
        let bytes = self.call(family, method, path, body, headers, timeout, expect).await?;
        decode(&bytes, what)
    }

    /// A call whose whole reply is read within `timeout`.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn call(
        &self,
        family: Family,
        method: Method,
        path: &str,
        body: Option<Vec<u8>>,
        headers: &[(&'static str, String)],
        timeout: Duration,
        expect: u16,
    ) -> Result<Vec<u8>, MkError> {
        let resp = self.send(family, method, path, body, headers, Some(timeout)).await?;
        let status = resp.status();
        let bytes = resp.bytes().await.map_err(map_reqwest)?.to_vec();
        if status.as_u16() == expect {
            return Ok(bytes);
        }
        Err(MkError::Decode {
            what: "status",
            message: format!("expected {expect}, got {status}"),
            body: excerpt(&bytes),
        })
    }

    /// Sends a request and returns the response once its status is a
    /// success. An error status reads the body and becomes the family's error.
    /// With `timeout` of `None`, nothing bounds the call: a stream bounds each
    /// read itself.
    pub(crate) async fn send(
        &self,
        family: Family,
        method: Method,
        path: &str,
        body: Option<Vec<u8>>,
        headers: &[(&'static str, String)],
        timeout: Option<Duration>,
    ) -> Result<reqwest::Response, MkError> {
        let mut req = self.http.request(method, format!("{}{}", self.base, path));
        if let Some(timeout) = timeout {
            req = req.timeout(timeout);
        }
        for (name, value) in headers {
            req = req.header(HeaderName::from_static(name), header_value(name, value)?);
        }
        if let Some(tp) = &self.traceparent
            && !headers.iter().any(|(n, _)| *n == "traceparent")
        {
            req = req.header(HeaderName::from_static("traceparent"), header_value("traceparent", tp)?);
        }
        if let Some(body) = body {
            req = req.header(CONTENT_TYPE, "application/json").body(body);
        }
        let resp = req.send().await.map_err(map_reqwest)?;
        let status = resp.status();
        if status.is_success() {
            return Ok(resp);
        }
        let retry_after = resp
            .headers()
            .get(RETRY_AFTER)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.trim().parse::<u64>().ok())
            .map(Duration::from_secs);
        let bytes = resp.bytes().await.map_err(map_reqwest)?.to_vec();
        Err(status_error(family, status, retry_after, &bytes))
    }
}

/// Decodes a success body. Unknown fields are ignored; a missing field or a
/// wrong type is a decode error.
pub(crate) fn decode<T: DeserializeOwned>(bytes: &[u8], what: &'static str) -> Result<T, MkError> {
    serde_json::from_slice(bytes).map_err(|e| MkError::Decode {
        what,
        message: e.to_string(),
        body: excerpt(bytes),
    })
}

/// Fails unless `value` is a JSON object, as tool-call arguments must be.
pub(crate) fn require_object(value: &Json, what: &'static str) -> Result<(), MkError> {
    match value {
        Json::Object(_) => Ok(()),
        _ => Err(MkError::Decode { what, message: "arguments are not an object".into(), body: String::new() }),
    }
}

fn status_error(family: Family, status: StatusCode, retry_after: Option<Duration>, bytes: &[u8]) -> MkError {
    match family {
        Family::Council => MkError::Status {
            status: status.as_u16(),
            error: serde_json::from_slice::<ErrorBody>(bytes).ok().map(|b| b.error),
            retry_after,
            body: excerpt(bytes),
        },
        Family::Mk => MkError::Service {
            status: status.as_u16(),
            error: serde_json::from_slice::<ServiceErrorBody>(bytes).ok().map(|b| b.error),
            retry_after,
            body: excerpt(bytes),
        },
    }
}

pub(crate) fn map_reqwest(e: reqwest::Error) -> MkError {
    if e.is_timeout() {
        MkError::Timeout
    } else {
        MkError::Transport(e.to_string())
    }
}

fn header_value(name: &str, value: &str) -> Result<HeaderValue, MkError> {
    HeaderValue::from_str(value).map_err(|_| MkError::Request(format!("{name} header value is not valid")))
}

pub(crate) fn to_body<T: serde::Serialize>(value: &T) -> Result<Vec<u8>, MkError> {
    serde_json::to_vec(value).map_err(|e| MkError::Request(format!("request does not serialize: {e}")))
}
