//! Protocol-aware form elicitation and MRTR (`requestState`) helpers.
//!
//! Clients that negotiated protocol `2026-07-28` or newer receive an
//! `InputRequiredResult` carrying an HMAC-sealed `requestState` token and
//! fulfill the elicitation on retry. Older clients receive an inline
//! `elicitation/create` server-to-client request. Clients without the
//! `elicitation` capability receive `ElicitationOutcome::Unsupported`.

use std::time::Duration;

use rmcp::model::{
    ElicitRequest, ElicitRequestParams, ElicitResult, ElicitationAction, ElicitationSchema,
    InputRequest, InputRequests, InputRequiredResult, InputResponses, ProtocolVersion,
    RequestStateCodec, SealOptions,
};
use rmcp::service::{RequestContext, RoleServer};
use rmcp::ErrorData as McpError;
use serde::{Serialize, de::DeserializeOwned};

/// Default TTL for sealed `requestState` tokens.
pub const DEFAULT_REQUEST_STATE_TTL: Duration = Duration::from_secs(300);

/// Build a per-server `RequestStateCodec` with a random 32-byte signing key.
pub fn random_request_state_codec() -> RequestStateCodec {
    let mut key = [0u8; 32];
    key[..16].copy_from_slice(uuid::Uuid::new_v4().as_bytes());
    key[16..].copy_from_slice(uuid::Uuid::new_v4().as_bytes());
    RequestStateCodec::try_new(key).expect("32-byte key satisfies RequestStateCodec::MIN_KEY_LENGTH")
}

/// Returns `true` when the client declared form elicitation support.
pub fn client_supports_form_elicitation(context: &RequestContext<RoleServer>) -> bool {
    let Some(caps) = context.client_capabilities() else {
        return false;
    };
    let Some(elicit) = caps.elicitation else {
        return false;
    };
    // Match MCP backward-compatibility: an empty elicitation capability object
    // implies form mode; otherwise form must be explicitly present.
    elicit.form.is_some() || elicit.url.is_none()
}

/// Returns `true` when the negotiated protocol version uses MRTR (`2026-07-28`+).
pub fn uses_mrtr(context: &RequestContext<RoleServer>) -> bool {
    context
        .protocol_version()
        .is_some_and(|v| v.as_str() >= ProtocolVersion::V_2026_07_28.as_str())
}

/// Result of attempting a form elicitation against the current request context.
#[derive(Debug)]
pub enum ElicitationOutcome {
    /// The client did not declare form elicitation capability.
    Unsupported,
    /// Protocol `>= 2026-07-28`: return this `InputRequiredResult` to trigger a client retry.
    InputRequired(InputRequiredResult),
    /// Protocol `< 2026-07-28`: inline `elicitation/create` completed immediately.
    Inline(ElicitResult),
}

/// Request form input from the client using the negotiated protocol's transport.
///
/// - Without form elicitation capability: returns [`ElicitationOutcome::Unsupported`].
/// - On `2026-07-28`+: seals `state` with `codec` and returns [`ElicitationOutcome::InputRequired`].
/// - On older versions: invokes `elicitation/create` inline and returns [`ElicitationOutcome::Inline`].
#[allow(clippy::too_many_arguments)]
pub async fn elicit_form<S: Serialize>(
    codec: &RequestStateCodec,
    context: &RequestContext<RoleServer>,
    key: &str,
    message: impl Into<String>,
    requested_schema: ElicitationSchema,
    state: &S,
    associated_data: &[u8],
    ttl: Option<Duration>,
) -> Result<ElicitationOutcome, McpError> {
    if !client_supports_form_elicitation(context) {
        return Ok(ElicitationOutcome::Unsupported);
    }

    let params = ElicitRequestParams::FormElicitationParams {
        meta: None,
        message: message.into(),
        requested_schema,
    };

    if uses_mrtr(context) {
        let mut opts = SealOptions::new().associated_data(associated_data);
        if let Some(ttl) = ttl {
            opts = opts.ttl(ttl);
        }
        let sealed = codec.seal_json_with(state, &opts).map_err(|e| {
            McpError::internal_error(format!("failed to seal requestState: {e}"), None)
        })?;
        let mut requests = InputRequests::new();
        requests.insert(
            key.to_string(),
            InputRequest::Elicitation(ElicitRequest::new(params)),
        );
        return Ok(ElicitationOutcome::InputRequired(InputRequiredResult::new(
            Some(requests),
            Some(sealed),
        )));
    }

    let result = context.peer.create_elicitation(params).await.map_err(|e| {
        McpError::internal_error(format!("inline elicitation failed: {e}"), None)
    })?;
    Ok(ElicitationOutcome::Inline(result))
}

/// Verify and decode an echoed MRTR `requestState` token.
///
/// Maps every verification or format failure to a single `INVALID_PARAMS` error
/// so internal failure details are not exposed on the wire.
pub fn open_request_state<T: DeserializeOwned>(
    codec: &RequestStateCodec,
    request_state: &str,
    associated_data: &[u8],
) -> Result<T, McpError> {
    codec
        .open_json_with(request_state, associated_data)
        .map_err(|err| {
            tracing::warn!(%err, "MRTR requestState verification failed");
            McpError::invalid_params("invalid or expired requestState", None)
        })
}

/// Extract and parse one form `ElicitResult` from `inputResponses`.
pub fn extract_form_response(
    input_responses: Option<&InputResponses>,
    key: &str,
) -> Result<Option<ElicitResult>, McpError> {
    let Some(value) = input_responses.and_then(|map| map.get(key)) else {
        return Ok(None);
    };
    let parsed: ElicitResult = serde_json::from_value(value.clone()).map_err(|e| {
        McpError::invalid_params(format!("invalid elicitation response for '{key}': {e}"), None)
    })?;
    Ok(Some(parsed))
}

/// Extract a non-empty trimmed string field from an accepted `ElicitResult`.
pub fn accepted_string_field(result: &ElicitResult, field: &str) -> Option<String> {
    if result.action != ElicitationAction::Accept {
        return None;
    }
    result
        .content
        .as_ref()
        .and_then(|v| v.get(field))
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rmcp::model::ErrorCode;
    use serde::Deserialize;

    #[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
    struct TestState {
        step: u32,
    }

    #[test]
    fn request_state_round_trip_and_tamper_rejection() {
        let codec = random_request_state_codec();
        let ad = b"tools/call:register_session";
        let sealed = codec
            .seal_json_with(
                &TestState { step: 1 },
                &SealOptions::new()
                    .associated_data(ad)
                    .ttl(DEFAULT_REQUEST_STATE_TTL),
            )
            .unwrap();

        let opened: TestState = open_request_state(&codec, &sealed, ad).unwrap();
        assert_eq!(opened, TestState { step: 1 });

        // Wrong associated data is rejected with INVALID_PARAMS.
        let wrong_ad = open_request_state::<TestState>(&codec, &sealed, b"other").unwrap_err();
        assert_eq!(wrong_ad.code, ErrorCode::INVALID_PARAMS);

        // Tampered token is rejected with INVALID_PARAMS.
        let mut tampered = sealed.clone();
        tampered.push('x');
        let err = open_request_state::<TestState>(&codec, &tampered, ad).unwrap_err();
        assert_eq!(err.code, ErrorCode::INVALID_PARAMS);
    }

    #[test]
    fn extract_form_response_parses_accept_and_ignores_missing() {
        let mut responses = InputResponses::new();
        responses.insert(
            "session_label".to_string(),
            serde_json::json!({
                "action": "accept",
                "content": { "label": "  my-session  " }
            }),
        );

        let parsed = extract_form_response(Some(&responses), "session_label")
            .unwrap()
            .expect("present");
        assert_eq!(
            accepted_string_field(&parsed, "label").as_deref(),
            Some("my-session")
        );
        assert!(extract_form_response(Some(&responses), "other").unwrap().is_none());
    }
}
