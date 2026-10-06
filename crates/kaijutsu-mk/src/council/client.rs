//! The `/council/v1` calls (`docs/council-api.md`).

use std::time::Duration;

use reqwest::Method;

use crate::client::{to_body, Family, MkClient, MkError};
use crate::council::canon;
use crate::council::wire::{
    ContextPut, ContextPutResult, ContextState, DecisionRequest, DecisionResponse, HeldSpec,
    ServerIdentity, SnapshotId, Spec, SpecId,
};

impl MkClient {
    /// `GET /council/v1/identity`.
    pub async fn identity(&self) -> Result<ServerIdentity, MkError> {
        self.json(Family::Council, Method::GET, "/council/v1/identity", None, &[], self.timeout, 200, "identity")
            .await
    }

    /// `PUT /council/v1/contexts/{id}`, conditional on `if_match` when given.
    pub async fn put_context(
        &self,
        id: &str,
        body: &ContextPut,
        if_match: Option<&SnapshotId>,
    ) -> Result<ContextPutResult, MkError> {
        let path = context_path(id)?;
        let bytes = to_body(body)?;
        let mut headers = Vec::new();
        if let Some(head) = if_match {
            headers.push(("if-match", head.as_str().to_string()));
        }
        self.json(Family::Council, Method::PUT, &path, Some(bytes), &headers, self.timeout, 200, "context put result")
            .await
    }

    /// `GET /council/v1/contexts/{id}`.
    pub async fn get_context(&self, id: &str) -> Result<ContextState, MkError> {
        let path = context_path(id)?;
        self.json(Family::Council, Method::GET, &path, None, &[], self.timeout, 200, "context state").await
    }

    /// `DELETE /council/v1/contexts/{id}`.
    pub async fn delete_context(&self, id: &str) -> Result<(), MkError> {
        let path = context_path(id)?;
        self.call(Family::Council, Method::DELETE, &path, None, &[], self.timeout, 204).await.map(|_| ())
    }

    /// `POST /council/v1/specs`. Fails with [`MkError::SpecIdMismatch`]
    /// when the server's id is not the spec's canonical id.
    pub async fn post_spec(&self, spec: &Spec) -> Result<HeldSpec, MkError> {
        let computed = canon::spec_id(spec).map_err(|e| MkError::Request(e.to_string()))?;
        let held: HeldSpec = self
            .json(Family::Council, Method::POST, "/council/v1/specs", Some(to_body(spec)?), &[], self.timeout, 200, "held spec")
            .await?;
        if held.spec_id != computed {
            return Err(MkError::SpecIdMismatch { computed, server: held.spec_id });
        }
        Ok(held)
    }

    /// `GET /council/v1/specs/{spec_id}`.
    pub async fn get_spec(&self, id: &SpecId) -> Result<HeldSpec, MkError> {
        let path = format!("/council/v1/specs/{id}");
        self.json(Family::Council, Method::GET, &path, None, &[], self.timeout, 200, "held spec").await
    }

    /// `POST /council/v1/decisions`. The request is checked against the rules a
    /// client can check ([`DecisionRequest::validate`]) before it is sent;
    /// `timeout` bounds the HTTP call, `request.timeout_ms` the server's work.
    pub async fn decide(
        &self,
        request: &DecisionRequest,
        timeout: Duration,
    ) -> Result<DecisionResponse, MkError> {
        self.decide_traced(request, timeout, None).await
    }

    /// Like [`decide`](Self::decide), sending `traceparent` for this call
    /// instead of the client's own.
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
        self.json(Family::Council, Method::POST, "/council/v1/decisions", Some(to_body(request)?), &headers, timeout, 200, "decision")
            .await
    }
}

fn context_path(id: &str) -> Result<String, MkError> {
    if id.is_empty() || !id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-') {
        return Err(MkError::Request(format!("context id {id:?} is not a UUID")));
    }
    Ok(format!("/council/v1/contexts/{id}"))
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use tokio::net::TcpListener;

    use super::*;
    use crate::council::wire::{ContextRef, ErrorType, Role, Turn};
    use crate::json::Json;
    use crate::test_server::{reply, serve};

    const SPEC: &str = include_str!("../../tests/fixtures/spec.json");
    const REQUEST: &str = include_str!("../../tests/fixtures/decision_request.json");
    const RESPONSE: &str = include_str!("../../tests/fixtures/decision_response.json");
    const DOC_SPEC_ID: &str = "sha256:5114ff063b887c710333afd4fe35238c55946ac2a63fb76d6c9004b56a1503ea";
    const CTX: &str = "0199b3c4-6c1e-7a2b-9f00-3e5d1c2a7b11";

    fn snap(c: char) -> String {
        format!("snap:{}", c.to_string().repeat(64))
    }

    fn client(base: &str) -> MkClient {
        MkClient::new(base, Duration::from_secs(5)).unwrap()
    }

    fn error_body(kind: &str, extra: serde_json::Value) -> String {
        let mut e = json!({"type": kind, "message": format!("{kind} happened")});
        for (k, v) in extra.as_object().unwrap() {
            e[k] = v.clone();
        }
        json!({"error": e}).to_string()
    }

    fn put_body() -> ContextPut {
        ContextPut {
            system: "rules".into(),
            turns: vec![Turn { role: Role::User, content: "hi".into(), snap: true, reasoning: None }],
            pin: None,
            warm: None,
            dry_run: None,
            persist: None,
        }
    }

    fn put_result() -> String {
        json!({"id": CTX, "head": snap('a'), "tokens": 10, "kept": 0, "fed": 10, "dry_run": false,
               "snapshots": [{"id": snap('a'), "tokens": 10, "layer": "turn", "turn": 0}]})
        .to_string()
    }

    fn decision_request() -> DecisionRequest {
        serde_json::from_str(REQUEST).unwrap()
    }

    #[tokio::test]
    async fn identity_decodes() {
        let body = json!({"model": "m", "weight_hash": "w", "tokenizer_hash": "t", "template": "x", "engine": "e",
                          "limits": {"context_tokens": 4096, "state_bytes": 1000, "contexts_per_decision": 8,
                                     "choice_options": 26, "default_timeout_ms": 2000},
                          "capabilities": ["warm"]});
        let (base, seen) = serve(vec![reply(200, body.to_string())]).await;
        let id = client(&base).identity().await.unwrap();
        assert_eq!(id.model, "m");
        let seen = seen.lock().unwrap();
        assert_eq!((seen[0].method.as_str(), seen[0].path.as_str()), ("GET", "/council/v1/identity"));
        assert!(seen[0].header("traceparent").is_none());
    }

    #[tokio::test]
    async fn put_context_sends_the_body_and_if_match() {
        let (base, seen) = serve(vec![reply(200, put_result())]).await;
        let head = SnapshotId::parse(snap('f')).unwrap();
        let r = client(&base).put_context(CTX, &put_body(), Some(&head)).await.unwrap();
        assert_eq!((r.fed, r.dry_run), (10, false));
        let seen = seen.lock().unwrap();
        assert_eq!(seen[0].method, "PUT");
        assert_eq!(seen[0].path, format!("/council/v1/contexts/{CTX}"));
        assert_eq!(seen[0].header("if-match"), Some(snap('f').as_str()));
        assert_eq!(seen[0].header("content-type"), Some("application/json"));
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&seen[0].body).unwrap(),
            json!({"system": "rules", "turns": [{"role": "user", "content": "hi", "snap": true}]})
        );
    }

    #[tokio::test]
    async fn put_context_without_if_match_sends_none() {
        let (base, seen) = serve(vec![reply(200, put_result())]).await;
        client(&base).put_context(CTX, &put_body(), None).await.unwrap();
        assert!(seen.lock().unwrap()[0].header("if-match").is_none());
    }

    #[tokio::test]
    async fn a_412_keeps_the_current_head() {
        let body = error_body("head_mismatch", json!({"head": snap('b')}));
        let (base, _) = serve(vec![reply(412, body)]).await;
        let err = client(&base).put_context(CTX, &put_body(), None).await.unwrap_err();
        assert_eq!(err.status(), Some(412));
        assert_eq!(err.head().unwrap().as_str(), snap('b'));
        let MkError::Status { error: Some(e), .. } = &err else { panic!("{err:?}") };
        assert_eq!(e.r#type, ErrorType::HeadMismatch);
    }

    #[tokio::test]
    async fn every_contract_status_maps_to_a_status_error() {
        for (status, kind) in [
            (400, "invalid_request"),
            (404, "not_found"),
            (409, "snapshot_gone"),
            (412, "head_mismatch"),
            (413, "too_large"),
            (429, "busy"),
            (503, "unavailable"),
            (504, "timeout"),
            (507, "pin_budget"),
        ] {
            let body = error_body(kind, json!({"param": "state_bytes", "head": snap('c')}));
            let (base, _) = serve(vec![reply(status, body)]).await;
            let err = client(&base).decide(&decision_request(), Duration::from_secs(5)).await.unwrap_err();
            assert_eq!(err.status(), Some(status), "{err:?}");
            let MkError::Status { error: Some(e), .. } = &err else { panic!("{err:?}") };
            assert_eq!(e.param.as_deref(), Some("state_bytes"));
            assert_eq!(err.head().unwrap().as_str(), snap('c'));
        }
    }

    #[tokio::test]
    async fn retry_after_is_kept_on_busy_and_unavailable() {
        for status in [429, 503] {
            let mut r = reply(status, error_body("busy", json!({})));
            r.headers.push(("Retry-After", "7".into()));
            let (base, _) = serve(vec![r]).await;
            let err = client(&base).get_context(CTX).await.unwrap_err();
            let MkError::Status { retry_after, .. } = err else { panic!() };
            assert_eq!(retry_after, Some(Duration::from_secs(7)));
        }
        let (base, _) = serve(vec![reply(503, error_body("unavailable", json!({})))]).await;
        let MkError::Status { retry_after, .. } = client(&base).get_context(CTX).await.unwrap_err() else { panic!() };
        assert_eq!(retry_after, None);
    }

    #[tokio::test]
    async fn an_error_status_with_a_foreign_body_keeps_the_text() {
        let (base, _) = serve(vec![reply(502, "<html>bad gateway</html>")]).await;
        let err = client(&base).get_context(CTX).await.unwrap_err();
        let MkError::Status { status, error, body, .. } = err else { panic!() };
        assert_eq!(status, 502);
        assert!(error.is_none());
        assert!(body.contains("bad gateway"));
    }

    #[tokio::test]
    async fn get_context_decodes_and_a_malformed_body_is_a_decode_error() {
        let state = json!({"id": CTX, "head": snap('a'), "tokens": 10, "snapshots": []});
        let (base, seen) = serve(vec![reply(200, state.to_string()), reply(200, json!({"id": CTX}).to_string())]).await;
        let c = client(&base);
        assert_eq!(c.get_context(CTX).await.unwrap().tokens, 10);
        assert_eq!(seen.lock().unwrap()[0].method, "GET");
        let err = c.get_context(CTX).await.unwrap_err();
        assert!(matches!(err, MkError::Decode { what: "context state", .. }), "{err:?}");
    }

    #[tokio::test]
    async fn delete_context_expects_204() {
        let (base, seen) = serve(vec![reply(204, ""), reply(404, error_body("not_found", json!({}))), reply(200, "{}")]).await;
        let c = client(&base);
        c.delete_context(CTX).await.unwrap();
        assert_eq!(seen.lock().unwrap()[0].method, "DELETE");
        assert_eq!(c.delete_context(CTX).await.unwrap_err().status(), Some(404));
        assert!(matches!(c.delete_context(CTX).await.unwrap_err(), MkError::Decode { what: "status", .. }));
    }

    #[tokio::test]
    async fn post_spec_checks_the_id_the_server_returns() {
        let spec: Spec = serde_json::from_str(SPEC).unwrap();
        let ok = json!({"spec_id": DOC_SPEC_ID, "spec": serde_json::from_str::<serde_json::Value>(SPEC).unwrap(), "template": "mk-letters-1:0123456789abcdef"});
        let wrong = json!({"spec_id": format!("sha256:{}", "0".repeat(64)), "spec": serde_json::from_str::<serde_json::Value>(SPEC).unwrap(), "template": "t"});
        let (base, seen) = serve(vec![reply(200, ok.to_string()), reply(200, wrong.to_string())]).await;
        let c = client(&base);
        let held = c.post_spec(&spec).await.unwrap();
        assert_eq!(held.spec_id.as_str(), DOC_SPEC_ID);
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&seen.lock().unwrap()[0].body).unwrap(),
            serde_json::from_str::<serde_json::Value>(SPEC).unwrap()
        );
        let err = c.post_spec(&spec).await.unwrap_err();
        assert!(matches!(err, MkError::SpecIdMismatch { .. }), "{err:?}");
    }

    #[tokio::test]
    async fn a_spec_with_a_float_is_refused_before_it_is_sent() {
        let mut spec: Spec = serde_json::from_str(SPEC).unwrap();
        spec.questions[1].set_instructions(serde_json::from_str(r#"{"w":0.5}"#).unwrap());
        let (base, seen) = serve(vec![]).await;
        let err = client(&base).post_spec(&spec).await.unwrap_err();
        assert!(matches!(err, MkError::Request(_)), "{err:?}");
        assert!(seen.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn get_spec_uses_the_spec_path() {
        let body = json!({"spec_id": DOC_SPEC_ID, "spec": serde_json::from_str::<serde_json::Value>(SPEC).unwrap(), "template": "t"});
        let (base, seen) = serve(vec![reply(200, body.to_string())]).await;
        client(&base).get_spec(&SpecId::parse(DOC_SPEC_ID).unwrap()).await.unwrap();
        assert_eq!(seen.lock().unwrap()[0].path, format!("/council/v1/specs/{DOC_SPEC_ID}"));
    }

    #[tokio::test]
    async fn decide_sends_the_request_and_decodes_the_answer() {
        let (base, seen) = serve(vec![reply(200, RESPONSE)]).await;
        let req = decision_request();
        let resp = client(&base).decide(&req, Duration::from_secs(5)).await.unwrap();
        assert_eq!(resp.model, "qwen3.8-flash-next");
        crate::council::math::verify(&resp, &req).unwrap();
        let seen = seen.lock().unwrap();
        assert_eq!((seen[0].method.as_str(), seen[0].path.as_str()), ("POST", "/council/v1/decisions"));
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&seen[0].body).unwrap(),
            serde_json::from_str::<serde_json::Value>(REQUEST).unwrap()
        );
        assert!(seen[0].header("traceparent").is_none());
    }

    #[tokio::test]
    async fn traceparent_is_sent_when_given() {
        let tp = "00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01";
        let (base, seen) = serve(vec![reply(200, RESPONSE), reply(200, RESPONSE)]).await;
        let c = client(&base);
        c.decide_traced(&decision_request(), Duration::from_secs(5), Some(tp)).await.unwrap();
        c.clone().with_traceparent("00-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-bbbbbbbbbbbbbbbb-00")
            .decide(&decision_request(), Duration::from_secs(5)).await.unwrap();
        let seen = seen.lock().unwrap();
        assert_eq!(seen[0].header("traceparent"), Some(tp));
        assert_eq!(seen[1].header("traceparent"), Some("00-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-bbbbbbbbbbbbbbbb-00"));
    }

    #[tokio::test]
    async fn an_invalid_request_is_refused_before_it_is_sent() {
        let (base, seen) = serve(vec![]).await;
        let mut req = decision_request();
        req.contexts = Some(vec![ContextRef { id: "a".into(), at: None }, ContextRef { id: "a".into(), at: None }]);
        req.pool = None;
        let err = client(&base).decide(&req, Duration::from_secs(5)).await.unwrap_err();
        assert!(matches!(err, MkError::Request(_)), "{err:?}");
        let mut neither = decision_request();
        neither.spec_id = None;
        neither.state = Json::from("s");
        assert!(matches!(client(&base).decide(&neither, Duration::from_secs(5)).await, Err(MkError::Request(_))));
        assert!(seen.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_response_outside_the_schema_is_a_decode_error_not_an_answer() {
        let mut v: serde_json::Value = serde_json::from_str(RESPONSE).unwrap();
        v["reads"][0].as_object_mut().unwrap().remove("rendered_sha256");
        let (base, _) = serve(vec![reply(200, v.to_string()), reply(200, "not json")]).await;
        let c = client(&base);
        for _ in 0..2 {
            let err = c.decide(&decision_request(), Duration::from_secs(5)).await.unwrap_err();
            assert!(matches!(err, MkError::Decode { what: "decision", .. }), "{err:?}");
        }
    }

    #[tokio::test]
    async fn a_slow_server_is_a_timeout() {
        let mut slow = reply(200, RESPONSE);
        slow.delay = Duration::from_millis(800);
        let (base, _) = serve(vec![slow]).await;
        let err = client(&base).decide(&decision_request(), Duration::from_millis(100)).await.unwrap_err();
        assert!(matches!(err, MkError::Timeout), "{err:?}");
    }

    #[tokio::test]
    async fn a_closed_port_is_a_transport_error() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        drop(listener);
        let err = client(&base).identity().await.unwrap_err();
        assert!(matches!(err, MkError::Transport(_)), "{err:?}");
    }

    #[tokio::test]
    async fn a_context_id_that_is_not_a_uuid_is_refused() {
        let (base, seen) = serve(vec![]).await;
        for id in ["", "../x", "a/b", "a b", "a?b"] {
            let err = client(&base).get_context(id).await.unwrap_err();
            assert!(matches!(err, MkError::Request(_)), "{id:?}");
        }
        assert!(seen.lock().unwrap().is_empty());
    }
}
