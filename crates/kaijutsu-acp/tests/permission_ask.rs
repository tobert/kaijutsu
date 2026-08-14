//! `session/request_permission`, wired live (D-57, docs/acp.md gap #2).
//!
//! Mirrors `dispatch.rs`'s in-memory-pair idiom, but with the roles the
//! permission ask actually uses reversed: here the AGENT side (this crate's
//! shape) is the one making an outbound `send_request`, and a small fake
//! CLIENT answers it. `permission::run_permission_pump_with_timeout` is
//! driven directly as the `connect_with` main function — exactly the role
//! `lib.rs::serve_stdio`'s `.with_spawned` closure plays for the real binary
//! — fed from a plain `mpsc::channel` standing in for
//! `ActorHandle::take_permission_asks()`, so none of this touches a kernel.
//!
//! `kaijutsu-kernel`'s own hook-engine tests already cover allow/deny/
//! no-subscriber/timeout at the kernel end; `permission_ask_wire.rs` covers
//! the kernel↔bridge wire join. What only this file can prove is the OTHER
//! half of the seam: that a live envelope really does turn into an ACP
//! `session/request_permission` call, that the client's answer maps back
//! correctly, and that every failure mode on THIS side (no session, client
//! error, client timeout) denies rather than falling through open.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use agent_client_protocol::schema::v1::{
    PermissionOptionId, RequestPermissionOutcome, RequestPermissionRequest,
    RequestPermissionResponse, SelectedPermissionOutcome,
};
use agent_client_protocol::{Agent, Client, ConnectTo, ConnectionTo, Responder};
use kaijutsu_acp::permission::run_permission_pump_with_timeout;
use kaijutsu_acp::rank::session_id_of;
use kaijutsu_acp::session::{Session, SessionRegistry};
use kaijutsu_acp::update::UpdateMapper;
use kaijutsu_client::{PermissionAskAnswer, PermissionAskEnvelope, PermissionOptionInfo};
use kaijutsu_crdt::ContextId;
use parking_lot::Mutex as PLMutex;
use tokio::sync::{mpsc, oneshot};

/// Short enough that the timeout-denies test doesn't actually wait 30s.
const TEST_TIMEOUT: Duration = Duration::from_millis(200);

fn session_registry_with(context_id: ContextId) -> SessionRegistry {
    let reg = SessionRegistry::default();
    let session_id = session_id_of(context_id);
    let session = Session {
        context_id,
        label: "test".into(),
        mapper: Arc::new(PLMutex::new(UpdateMapper::new(session_id.clone()))),
    };
    assert!(reg.bind(session_id, session).is_some());
    reg
}

fn envelope(
    context_id: ContextId,
    options: Vec<PermissionOptionInfo>,
) -> (PermissionAskEnvelope, oneshot::Receiver<PermissionAskAnswer>) {
    let (answer, rx) = oneshot::channel();
    let envelope = PermissionAskEnvelope {
        request_id: "req-1".into(),
        context_id,
        description: "about to run rm -rf /".into(),
        instance: "builtin.shell".into(),
        tool: "shell".into(),
        hook_id: "hook-1".into(),
        options,
        answer,
    };
    (envelope, rx)
}

/// Count of `RequestPermissionRequest`s the fake client actually received —
/// distinguishes "denied because we never asked" (no-session path) from
/// "denied because the client said no."
type RequestCount = Arc<Mutex<u32>>;

/// A fake ACP client that answers every `session/request_permission` with
/// `outcome`, tracking how many it actually saw.
fn answering_client(
    outcome: RequestPermissionOutcome,
    seen: RequestCount,
) -> impl ConnectTo<Agent> + 'static {
    Client
        .builder()
        .name("fake-client")
        .on_receive_request(
            move |_req: RequestPermissionRequest,
                  responder: Responder<RequestPermissionResponse>,
                  _cx: ConnectionTo<Agent>| {
                *seen.lock().unwrap() += 1;
                let outcome = outcome.clone();
                async move { responder.respond(RequestPermissionResponse::new(outcome)) }
            },
            agent_client_protocol::on_receive_request!(),
        )
}

/// A fake ACP client that never answers — the timeout-denies case.
fn silent_client(seen: RequestCount) -> impl ConnectTo<Agent> + 'static {
    Client.builder().name("silent-client").on_receive_request(
        move |_req: RequestPermissionRequest, _responder: Responder<RequestPermissionResponse>, _cx: ConnectionTo<Agent>| {
            *seen.lock().unwrap() += 1;
            std::future::pending::<Result<(), agent_client_protocol::Error>>()
        },
        agent_client_protocol::on_receive_request!(),
    )
}

/// Drive one envelope through `run_permission_pump_with_timeout` against
/// `client`, returning what the client saw asked and what the envelope's
/// answer channel received.
async fn drive_one(
    sessions: &SessionRegistry,
    client: impl ConnectTo<Agent> + 'static,
    envelope: PermissionAskEnvelope,
    mut answer_rx: oneshot::Receiver<PermissionAskAnswer>,
) -> PermissionAskAnswer {
    let (tx, rx) = mpsc::channel(4);
    tx.send(envelope).await.expect("buffered send");
    drop(tx); // closes the channel once drained — the pump loop then exits

    Agent
        .builder()
        .name("test-agent")
        .connect_with(client, async move |cx: ConnectionTo<Client>| {
            run_permission_pump_with_timeout(rx, sessions, cx, TEST_TIMEOUT).await;
            Ok(())
        })
        .await
        .expect("acp session");

    answer_rx
        .try_recv()
        .expect("the pump must answer before the connection winds down")
}

#[tokio::test(flavor = "current_thread")]
async fn allow_round_trips_to_the_client_and_back() {
    let ctx = ContextId::new();
    let sessions = session_registry_with(ctx);
    let (envelope, answer_rx) = envelope(ctx, vec![]);
    let seen: RequestCount = Arc::default();

    let outcome = RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new(
        PermissionOptionId::new("allow"),
    ));
    let answer = drive_one(&sessions, answering_client(outcome, seen.clone()), envelope, answer_rx).await;

    assert_eq!(*seen.lock().unwrap(), 1, "the client must have been asked exactly once");
    assert_eq!(
        answer,
        PermissionAskAnswer {
            allow: true,
            selected_option_id: Some("allow".to_string()),
            remember: None,
        }
    );
}

#[tokio::test(flavor = "current_thread")]
async fn deny_round_trips_to_the_client_and_back() {
    let ctx = ContextId::new();
    let sessions = session_registry_with(ctx);
    let (envelope, answer_rx) = envelope(ctx, vec![]);
    let seen: RequestCount = Arc::default();

    let outcome = RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new(
        PermissionOptionId::new("deny"),
    ));
    let answer = drive_one(&sessions, answering_client(outcome, seen.clone()), envelope, answer_rx).await;

    assert_eq!(*seen.lock().unwrap(), 1);
    assert_eq!(
        answer,
        PermissionAskAnswer {
            allow: false,
            selected_option_id: Some("deny".to_string()),
            remember: None,
        },
        "a recognised, explicit reject still credits which option was picked"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn richer_kernel_options_round_trip_their_own_ids_and_kinds() {
    let ctx = ContextId::new();
    let sessions = session_registry_with(ctx);
    let kernel_options = vec![
        PermissionOptionInfo {
            id: "opt-allow-always".into(),
            label: "Always allow".into(),
            kind: "allow_always".into(),
        },
        PermissionOptionInfo {
            id: "opt-reject".into(),
            label: "Reject".into(),
            kind: "reject_once".into(),
        },
    ];
    let (envelope, answer_rx) = envelope(ctx, kernel_options);
    let seen: RequestCount = Arc::default();

    let outcome = RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new(
        PermissionOptionId::new("opt-allow-always"),
    ));
    let answer = drive_one(&sessions, answering_client(outcome, seen.clone()), envelope, answer_rx).await;

    assert_eq!(*seen.lock().unwrap(), 1);
    assert_eq!(
        answer,
        PermissionAskAnswer {
            allow: true,
            selected_option_id: Some("opt-allow-always".to_string()),
            remember: Some("always".to_string()),
        }
    );
}

#[tokio::test(flavor = "current_thread")]
async fn no_live_session_denies_without_ever_asking_the_client() {
    // No session bound for this context — the registry is empty.
    let sessions = SessionRegistry::default();
    let ctx = ContextId::new();
    let (envelope, answer_rx) = envelope(ctx, vec![]);
    let seen: RequestCount = Arc::default();

    // Whatever the client would have said, it must never be asked.
    let outcome = RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new(
        PermissionOptionId::new("allow"),
    ));
    let answer = drive_one(&sessions, answering_client(outcome, seen.clone()), envelope, answer_rx).await;

    assert_eq!(*seen.lock().unwrap(), 0, "no session for this context — the client must never be asked");
    assert_eq!(answer, PermissionAskAnswer { allow: false, selected_option_id: None, remember: None });
}

#[tokio::test(flavor = "current_thread")]
async fn a_client_that_never_answers_denies_on_timeout() {
    let ctx = ContextId::new();
    let sessions = session_registry_with(ctx);
    let (envelope, answer_rx) = envelope(ctx, vec![]);
    let seen: RequestCount = Arc::default();

    let answer = drive_one(&sessions, silent_client(seen.clone()), envelope, answer_rx).await;

    assert_eq!(*seen.lock().unwrap(), 1, "the client was asked — it just never answered");
    assert_eq!(answer, PermissionAskAnswer { allow: false, selected_option_id: None, remember: None });
}

#[tokio::test(flavor = "current_thread")]
async fn a_cancelled_prompt_denies() {
    let ctx = ContextId::new();
    let sessions = session_registry_with(ctx);
    let (envelope, answer_rx) = envelope(ctx, vec![]);
    let seen: RequestCount = Arc::default();

    let answer = drive_one(
        &sessions,
        answering_client(RequestPermissionOutcome::Cancelled, seen.clone()),
        envelope,
        answer_rx,
    )
    .await;

    assert_eq!(*seen.lock().unwrap(), 1);
    assert_eq!(answer, PermissionAskAnswer { allow: false, selected_option_id: None, remember: None });
}
