//! Does the ACP handler chain actually dispatch?
//!
//! `serve_stdio` chains six `on_receive_*` registrations on one builder —
//! five request types plus `session/cancel`. Chaining is how the SDK composes
//! handlers (each wraps the previous; the first whose `matches_method` hits
//! claims the message), and getting it wrong is a runtime `method_not_found`,
//! not a compile error. These tests pair a real ACP `Client` against an agent
//! built with the *same shape* — same types, same registration order, same
//! spawn-then-respond pattern for `session/prompt` — and assert every method
//! lands in its own handler.
//!
//! They deliberately do **not** touch a kernel: `AcpBridge` needs a live
//! `ActorHandle`, and what is at risk here is the protocol wiring, not the
//! kj translation (which is unit-tested in `update.rs` / `rank.rs`). The
//! kernel half is covered by the live smoke test in docs/acp.md.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use agent_client_protocol::schema::v1::{
    AgentCapabilities, CancelNotification, InitializeRequest, InitializeResponse,
    ListSessionsRequest, ListSessionsResponse, LoadSessionRequest, LoadSessionResponse,
    NewSessionRequest, NewSessionResponse, PromptRequest, PromptResponse, SessionId, SessionInfo,
    ResumeSessionRequest, ResumeSessionResponse, StopReason,
};
use agent_client_protocol::schema::ProtocolVersion;
use agent_client_protocol::{Agent, Client, ConnectionTo, Responder};

/// Which handler saw traffic. The point of the test is that each name appears
/// exactly once, in the right place.
type Log = Arc<Mutex<Vec<&'static str>>>;

/// Mirror of `serve_stdio`'s builder: same seven registrations, same order,
/// same `cx.spawn` for prompt.
fn agent_shape(log: Log) -> impl agent_client_protocol::ConnectTo<Client> + 'static {
    let (l1, l2, l3, l4, l5, l6, l7) = (
        log.clone(),
        log.clone(),
        log.clone(),
        log.clone(),
        log.clone(),
        log.clone(),
        log.clone(),
    );
    Agent
        .builder()
        .name("kaijutsu-acp-test")
        .on_receive_request(
            async move |req: InitializeRequest,
                        responder: Responder<InitializeResponse>,
                        _cx: ConnectionTo<Client>| {
                l1.lock().unwrap().push("initialize");
                responder.respond(
                    InitializeResponse::new(kaijutsu_acp::negotiate_version(req.protocol_version))
                        .agent_capabilities(AgentCapabilities::new()),
                )
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |_req: NewSessionRequest,
                        responder: Responder<NewSessionResponse>,
                        _cx: ConnectionTo<Client>| {
                l2.lock().unwrap().push("session/new");
                responder.respond(NewSessionResponse::new(SessionId::new("ctx-hex")))
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |_req: LoadSessionRequest,
                        responder: Responder<LoadSessionResponse>,
                        _cx: ConnectionTo<Client>| {
                l3.lock().unwrap().push("session/load");
                responder.respond(LoadSessionResponse::new())
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |_req: ResumeSessionRequest,
                        responder: Responder<ResumeSessionResponse>,
                        _cx: ConnectionTo<Client>| {
                l4.lock().unwrap().push("session/resume");
                responder.respond(ResumeSessionResponse::new())
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |_req: ListSessionsRequest,
                        responder: Responder<ListSessionsResponse>,
                        _cx: ConnectionTo<Client>| {
                l5.lock().unwrap().push("session/list");
                responder.respond(ListSessionsResponse::new(vec![SessionInfo::new(
                    SessionId::new("seat-0"),
                    "/tmp",
                )
                .title("ring zero")]))
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |_req: PromptRequest,
                        responder: Responder<PromptResponse>,
                        cx: ConnectionTo<Client>| {
                // The real handler spawns so the dispatch loop stays free for
                // session/cancel. Same shape here.
                let l = l6.clone();
                cx.spawn(async move {
                    l.lock().unwrap().push("session/prompt");
                    responder.respond(PromptResponse::new(StopReason::EndTurn))
                })
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_notification(
            async move |_n: CancelNotification, _cx: ConnectionTo<Client>| {
                l7.lock().unwrap().push("session/cancel");
                Ok(())
            },
            agent_client_protocol::on_receive_notification!(),
        )
}

#[tokio::test(flavor = "current_thread")]
async fn every_acp_method_reaches_its_own_handler() {
    let log: Log = Arc::default();
    let agent = agent_shape(log.clone());

    Client
        .builder()
        .name("test-client")
        .connect_with(agent, async move |cx| {
            let init = cx
                .send_request(InitializeRequest::new(ProtocolVersion::V1))
                .block_task()
                .await?;
            assert_eq!(init.protocol_version, ProtocolVersion::V1);

            let new = cx
                .send_request(NewSessionRequest::new("/tmp"))
                .block_task()
                .await?;
            assert_eq!(new.session_id, SessionId::new("ctx-hex"));

            cx.send_request(LoadSessionRequest::new(
                SessionId::new("ctx-hex"),
                "/tmp",
            ))
            .block_task()
            .await?;

            cx.send_request(ResumeSessionRequest::new(
                SessionId::new("ctx-hex"),
                "/tmp/resumed",
            ))
            .block_task()
            .await?;

            let listed = cx
                .send_request(ListSessionsRequest::new())
                .block_task()
                .await?;
            assert_eq!(listed.sessions.len(), 1);
            assert_eq!(listed.sessions[0].title.as_deref(), Some("ring zero"));

            let prompt = cx
                .send_request(PromptRequest::new(SessionId::new("ctx-hex"), vec![]))
                .block_task()
                .await?;
            assert_eq!(prompt.stop_reason, StopReason::EndTurn);

            cx.send_notification(CancelNotification::new(SessionId::new("ctx-hex")))?;
            // Notifications are fire-and-forget; give the loop a tick to run it.
            tokio::time::sleep(Duration::from_millis(50)).await;
            Ok(())
        })
        .await
        .expect("acp session");

    let seen = log.lock().unwrap().clone();
    assert_eq!(
        seen,
        vec![
            "initialize",
            "session/new",
            "session/load",
            "session/resume",
            "session/list",
            "session/prompt",
            "session/cancel",
        ],
        "each chained handler must claim exactly its own method"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn a_method_we_do_not_implement_is_refused_not_mishandled() {
    // Nothing in the chain claims `session/delete`; the SDK must answer
    // method_not_found rather than letting it fall into a neighbouring
    // handler.
    use agent_client_protocol::schema::v1::DeleteSessionRequest;
    let log: Log = Arc::default();
    let agent = agent_shape(log.clone());

    Client
        .builder()
        .connect_with(agent, async move |cx| {
            let err = cx
                .send_request(DeleteSessionRequest::new(SessionId::new("ctx-hex")))
                .block_task()
                .await
                .expect_err("session/delete is not implemented");
            assert_eq!(
                err.code,
                agent_client_protocol::schema::v1::Error::method_not_found().code
            );
            Ok(())
        })
        .await
        .expect("acp session");

    assert!(
        log.lock().unwrap().is_empty(),
        "an unimplemented method must not wake any handler"
    );
}
