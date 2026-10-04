//! The council in the gate, end to end: a kernel, its broker, and a council
//! server on 127.0.0.1 that answers the contract with numbers
//! `math::verify` recomputes. Each scenario runs on both paths a shell
//! submission takes to the gate: the RPC shell path (`shell_pre_call_hooks`,
//! whose gate-policy ask the council decides) and the `shell_write` tool
//! (`run_gate` for `Origin::ShellGate`).

use std::sync::{Arc, Mutex};
use std::time::Duration;

use approval_ledger::council::{CouncilDecision, CouncilOutcome};
use approval_ledger::types::{ApprovalRow, ApprovalStatus, SignalRow, SignalSourceKind, SignalVerdict};
use kaijutsu_council::wire::DecisionRequest;
use kaijutsu_types::{PrincipalId, RefusalKind, SessionId};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

use super::gate::test_support::{answer, identity};
use super::projection::fixtures::live_context;
use crate::kj::KjDispatcher;
use crate::mcp::binding::{Capability, ContextToolBinding};
use crate::mcp::servers::ShellServer;
use crate::mcp::{
    Broker, CallContext, InstanceId, InstancePolicy, KernelCallParams, McpError, ShellHookVerdict,
};

/// The docs' shell spec without its `text` question, as the shipped default
/// holds it while the server does not declare `describe`.
fn spec_text() -> String {
    let mut spec: kaijutsu_council::wire::Spec =
        serde_json::from_str(include_str!("../../../kaijutsu-council/tests/fixtures/spec.json")).unwrap();
    spec.questions.retain(|q| !matches!(q, kaijutsu_council::wire::SpecQuestion::Text(_)));
    serde_json::to_string(&spec).unwrap()
}

/// Two confident reads of allow.
const ALLOW: [[f64; 3]; 2] = [[-0.001, -8.0, -9.0], [-0.002, -7.5, -9.0]];
/// Two reads that lean ask.
const ASK: [[f64; 3]; 2] = [[-4.0, -0.03, -5.0], [-3.5, -0.04, -5.0]];
/// Two reads that lean report.
const REPORT: [[f64; 3]; 2] = [[-6.0, -3.0, -0.04], [-6.0, -2.5, -0.03]];

#[derive(Clone)]
struct Reply {
    status: u16,
    body: String,
    delay: Duration,
}

impl Reply {
    fn ok(body: serde_json::Value) -> Self {
        Reply { status: 200, body: body.to_string(), delay: Duration::ZERO }
    }
}

type Behavior = Arc<dyn Fn(&DecisionRequest) -> Reply + Send + Sync>;

/// A council server on 127.0.0.1 that holds whatever it is sent and answers
/// decisions the way the test says.
struct Mock {
    base: String,
    decisions: Arc<Mutex<Vec<DecisionRequest>>>,
    behavior: Arc<Mutex<Behavior>>,
}

impl Mock {
    fn set(&self, behavior: impl Fn(&DecisionRequest) -> Reply + Send + Sync + 'static) {
        *self.behavior.lock().unwrap() = Arc::new(behavior);
    }

    fn answers(&self, logprobs: &'static [[f64; 3]]) {
        self.set(move |req| Reply::ok(answer(req, logprobs)));
    }

    fn edited(&self, logprobs: &'static [[f64; 3]], edit: fn(&mut serde_json::Value)) {
        self.set(move |req| {
            let mut body = answer(req, logprobs);
            edit(&mut body);
            Reply::ok(body)
        });
    }

    fn decisions(&self) -> Vec<DecisionRequest> {
        self.decisions.lock().unwrap().clone()
    }
}

fn server_identity() -> String {
    let mut id = identity();
    id["limits"] = serde_json::json!({"context_tokens": 100000, "state_bytes": 100000, "contexts_per_decision": 8,
                                      "choice_options": 8, "default_timeout_ms": 1000});
    id["capabilities"] = serde_json::json!(["leave_one_out"]);
    id.to_string()
}

async fn read_request(sock: &mut tokio::net::TcpStream) -> Option<(String, String, String)> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 8192];
    let (head_end, len) = loop {
        let n = sock.read(&mut chunk).await.ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&chunk[..n]);
        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            let head = String::from_utf8_lossy(&buf[..i]).to_lowercase();
            let len = head
                .lines()
                .find_map(|l| l.strip_prefix("content-length:"))
                .map(|v| v.trim().parse::<usize>().unwrap())
                .unwrap_or(0);
            break (i + 4, len);
        }
    };
    while buf.len() < head_end + len {
        let n = sock.read(&mut chunk).await.ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&chunk[..n]);
    }
    let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
    let mut first = head.lines().next()?.split(' ');
    let method = first.next()?.to_string();
    let path = first.next()?.to_string();
    Some((method, path, String::from_utf8_lossy(&buf[head_end..]).to_string()))
}

async fn serve() -> Mock {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let decisions: Arc<Mutex<Vec<DecisionRequest>>> = Arc::default();
    let behavior: Arc<Mutex<Behavior>> = Arc::new(Mutex::new(Arc::new(|req: &DecisionRequest| Reply::ok(answer(req, &ALLOW)))));
    let (log, how) = (decisions.clone(), behavior.clone());
    tokio::spawn(async move {
        let puts = Arc::new(std::sync::atomic::AtomicU64::new(0));
        loop {
            let Ok((mut sock, _)) = listener.accept().await else { return };
            let (log, how, puts) = (log.clone(), how.clone(), puts.clone());
            tokio::spawn(async move {
                let Some((method, path, body)) = read_request(&mut sock).await else { return };
                let reply = match (method.as_str(), path.as_str()) {
                    ("GET", "/council/v1/identity") => Reply { status: 200, body: server_identity(), delay: Duration::ZERO },
                    ("POST", "/council/v1/specs") => {
                        let spec: kaijutsu_council::wire::Spec = serde_json::from_str(&body).unwrap();
                        let id = kaijutsu_council::canon::spec_id(&spec).unwrap();
                        Reply::ok(serde_json::json!({"spec_id": id, "spec": spec, "template": "mk-letters-1:0123456789abcdef"}))
                    }
                    ("PUT", p) if p.starts_with("/council/v1/contexts/") => {
                        let n = puts.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
                        Reply::ok(serde_json::json!({"id": p.rsplit('/').next().unwrap(), "head": format!("snap:{n:064x}"),
                                                     "tokens": 1, "kept": 0, "fed": 1, "dry_run": false, "snapshots": []}))
                    }
                    ("POST", "/council/v1/decisions") => {
                        let request: DecisionRequest = serde_json::from_str(&body).unwrap();
                        log.lock().unwrap().push(request.clone());
                        let behavior = how.lock().unwrap().clone();
                        behavior(&request)
                    }
                    _ => Reply { status: 500, body: "unexpected".into(), delay: Duration::ZERO },
                };
                tokio::time::sleep(reply.delay).await;
                let out = format!(
                    "HTTP/1.1 {} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                    reply.status,
                    reply.body.len(),
                    reply.body
                );
                let _ = sock.write_all(out.as_bytes()).await;
                let _ = sock.shutdown().await;
            });
        }
    });
    Mock { base, decisions, behavior }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Via {
    /// `Broker::shell_pre_call_hooks`, as the RPC shell paths call it.
    Rpc,
    /// The `shell_write` tool through `Broker::call_tool`.
    Tool,
}

const BOTH: [Via; 2] = [Via::Rpc, Via::Tool];

struct Setup {
    enabled: bool,
    deadline_ms: u64,
    /// Replaces the mock's address, for a server that is down.
    server: Option<String>,
    global: &'static str,
}

impl Default for Setup {
    fn default() -> Self {
        Setup { enabled: true, deadline_ms: 700, server: None, global: "" }
    }
}

fn gate_toml(server: &str, setup: &Setup) -> String {
    format!(
        r#"[global]
{global}

[council]
server = "{server}"
contexts = ["voice", "system-rules"]
pool = {{ method = "loglinear", weights = "mass" }}
deadline_ms = {deadline}

[[council.spec]]
name = "shell-gate"
case = "shell"

[[council.threshold]]
spec = "shell-gate"
weight_hash = "w1"
engine = "e1"
tokenizer_hash = "t1"
template = "mk-letters-1:0123456789abcdef"
allow_at = 0.98
mass_floor = -0.05

[context_type.default.council]
enabled = {enabled}
"#,
        global = setup.global,
        deadline = setup.deadline_ms,
        enabled = setup.enabled,
    )
}

struct Rig {
    via: Via,
    d: Arc<KjDispatcher>,
    broker: Arc<Broker>,
    ctx: CallContext,
    mock: Mock,
}

async fn rig(via: Via, setup: Setup) -> Rig {
    use crate::vfs::VfsOps;
    let d = Arc::new(crate::kj::test_helpers::test_dispatcher_persistent().await);
    d.set_self_arc();
    // The kernel's own broker: the approval worker runs approved commands
    // through it.
    let broker = d.kernel().broker().clone();
    broker.set_kj_dispatcher(&d).await;
    broker
        .register(Arc::new(ShellServer::new(Arc::downgrade(&broker))), InstancePolicy::default())
        .await
        .unwrap();
    let mock = serve().await;
    let server = setup.server.clone().unwrap_or_else(|| mock.base.clone());
    let vfs = d.kernel().vfs();
    vfs.write_all(std::path::Path::new("/config/kernel/gate.toml"), gate_toml(&server, &setup).as_bytes())
        .await
        .unwrap();
    let _ = vfs.mkdir(std::path::Path::new("/config/kernel/council"), 0o755).await;
    vfs.write_all(std::path::Path::new("/config/kernel/council/shell-gate.json"), spec_text().as_bytes())
        .await
        .unwrap();
    live_context(d.kernel(), "voice");
    live_context(d.kernel(), "system-rules");

    let actor = PrincipalId::new();
    let reviewer = PrincipalId::new();
    let context_id = crate::kj::test_helpers::register_context(&d, Some("council-seat"), None, actor);
    d.block_store().create_document(context_id, kaijutsu_types::DocKind::Conversation, None).unwrap();
    {
        let db = d.kernel_db();
        let db = db.lock();
        db.insert_character(&crate::kernel_db::CharacterRow {
            principal_id: reviewer, name: "council-seat-reviewer".into(), created_at: 0, retired_at: None,
            handoff_ctx: None, root_ctx: None, root: false,
        })
        .unwrap();
        db.update_context_review(context_id, Some(actor), Some(reviewer)).unwrap();
    }
    let mut binding = ContextToolBinding::new();
    binding.grant(Capability::Facade("shell_write".into()));
    broker.set_binding(context_id, binding).await.unwrap();
    let ctx = CallContext::new(actor, context_id, SessionId::new(), d.kernel_id()).with_actor(actor, Some(reviewer));
    Rig { via, d, broker, ctx, mock }
}

impl Rig {
    /// Submit `command` the way this rig's path does. `Ok` means it ran (the
    /// tool) or may run (the RPC path's `Proceed`).
    async fn submit(&self, command: &str) -> Result<(), McpError> {
        match self.via {
            Via::Rpc => match self.broker.shell_pre_call_hooks(command, &self.ctx, &CancellationToken::new()).await {
                ShellHookVerdict::Proceed => Ok(()),
                ShellHookVerdict::Denied(error) => Err(error),
                other => panic!("unexpected verdict {other:?}"),
            },
            Via::Tool => {
                let params = KernelCallParams {
                    instance: InstanceId::new(ShellServer::INSTANCE_WRITE),
                    tool: ShellServer::TOOL_WRITE.to_string(),
                    arguments: serde_json::json!({ "command": command }),
                };
                let result = self.broker.call_tool(params, &self.ctx, CancellationToken::new()).await?;
                assert!(!result.is_error, "{:?}: {result:?}", self.via);
                Ok(())
            }
        }
    }

    fn context_bytes(&self) -> Vec<u8> {
        self.ctx.context_id.as_bytes().to_vec()
    }

    /// Every ask this seat raised, oldest first.
    fn asks(&self) -> Vec<ApprovalRow> {
        let db = self.d.kernel_db();
        let db = db.lock();
        let conn = db.conn_for_ledger();
        let mut rows = approval_ledger::ask::list_history(conn, 100).unwrap();
        rows.extend(approval_ledger::ask::list_unresolved(conn).unwrap());
        rows.retain(|r| r.context_id == self.context_bytes());
        rows.sort_by_key(|r| r.created_at);
        rows
    }

    fn only_ask(&self) -> ApprovalRow {
        let asks = self.asks();
        assert_eq!(asks.len(), 1, "{:?}: one ask expected, got {asks:#?}", self.via);
        asks.into_iter().next().unwrap()
    }

    fn decisions_for(&self, request_id: &str) -> Vec<CouncilDecision> {
        let db = self.d.kernel_db();
        let db = db.lock();
        approval_ledger::council::list_council_decisions_for_request(db.conn_for_ledger(), request_id, 10).unwrap()
    }

    fn decisions(&self) -> Vec<CouncilDecision> {
        let db = self.d.kernel_db();
        let db = db.lock();
        approval_ledger::council::list_council_decisions_for_context(db.conn_for_ledger(), &self.context_bytes(), 10)
            .unwrap()
    }

    fn signals(&self, request_id: &str) -> Vec<SignalRow> {
        let db = self.d.kernel_db();
        let db = db.lock();
        approval_ledger::ask::list_signals(db.conn_for_ledger(), request_id).unwrap()
    }

    fn answer_pending(&self, allow: bool) {
        let db = self.d.kernel_db();
        let db = db.lock();
        let conn = db.conn_for_ledger();
        let row = approval_ledger::ask::list_pending(conn).unwrap().into_iter().next().expect("a pending ask");
        let reviewer = row.reviewer_id.as_deref().expect("a reviewer");
        approval_ledger::claim::claim(conn, &row.request_id, reviewer).unwrap();
        approval_ledger::decide::decide(
            conn,
            &row.request_id,
            approval_ledger::decide::DecideInput {
                allow,
                decided_by: Some(approval_ledger::decide::Answerer { principal: reviewer, context: Some(b"another-seat") }),
                decided_option: Some(if allow { "allow_once" } else { "deny" }),
                remember_scope: None,
                auto_reason: None,
            },
        )
        .unwrap();
    }

    fn seat_a_root(&self) {
        let db = self.d.kernel_db();
        let db = db.lock();
        db.insert_character(&crate::kernel_db::CharacterRow {
            principal_id: self.ctx.actor_id, name: format!("root-{}", self.ctx.actor_id), created_at: 0,
            retired_at: None, handoff_ctx: None, root_ctx: None, root: true,
        })
        .unwrap();
    }

    async fn finish(self) {
        self.d.kernel().shutdown_runtime_worker().await.unwrap();
    }
}

fn assert_pending(via: Via, result: Result<(), McpError>) {
    match result {
        Err(error) => assert!(error.is_refusal(RefusalKind::Pending), "{via:?}: {error:?}"),
        Ok(()) => panic!("{via:?}: the submission must ask, and it ran"),
    }
}

fn the_decision(rig: &Rig, ask: &ApprovalRow) -> CouncilDecision {
    let decisions = rig.decisions_for(&ask.request_id);
    assert_eq!(decisions.len(), 1, "{:?}: one decision linked to the ask: {decisions:#?}", rig.via);
    decisions.into_iter().next().unwrap()
}

fn council_signal(rig: &Rig, ask: &ApprovalRow) -> SignalRow {
    let signals = rig.signals(&ask.request_id);
    assert_eq!(signals.len(), 1, "{:?}: {signals:#?}", rig.via);
    let signal = signals.into_iter().next().unwrap();
    assert_eq!(signal.source_kind, SignalSourceKind::Council);
    assert_eq!(signal.source_id.as_deref(), Some("shell-gate"));
    signal
}

/// A council allow runs the submission, leaves an auto-decided row whose
/// reason names the council, a signal, and a decision record linked to it.
/// The request pins both contexts to their prepared heads and carries the
/// deadline and the submission.
///
/// Falsified by dropping the council's verdict before `run_gate_recorded`:
/// the submission asks.
#[tokio::test]
async fn a_council_allow_runs_the_submission_with_a_row_naming_the_council() {
    for via in BOTH {
        let rig = rig(via, Setup::default()).await;
        rig.mock.answers(&ALLOW);
        rig.submit("echo council-ran").await.unwrap_or_else(|e| panic!("{via:?}: the council allows: {e:?}"));

        let ask = rig.only_ask();
        assert_eq!(ask.status, ApprovalStatus::Allowed, "{via:?}");
        let reason = ask.auto_reason.clone().expect("an auto-decided row");
        assert!(reason.contains("council (shell-gate) allows") && reason.contains("at p=0.99"), "{via:?}: {reason}");
        assert!(ask.decided_by.is_none(), "{via:?}: nobody answered it");

        let decision = the_decision(&rig, &ask);
        assert_eq!(decision.decision.outcome, CouncilOutcome::Allow);
        assert_eq!(decision.decision.reads.len(), 2);
        assert!(decision.decision.reads.iter().all(|r| r.expected_head.is_some()));
        assert_eq!(decision.decision.threshold.unwrap().allow_at, 0.98);
        assert_eq!(decision.decision.server.weight_hash, "w1");
        assert!(decision.decision.agreement.unwrap().agree);

        let signal = council_signal(&rig, &ask);
        assert_eq!(signal.verdict, SignalVerdict::Allow);
        assert!(signal.score.unwrap() > 0.98, "{signal:?}");

        let sent = rig.mock.decisions();
        assert_eq!(sent.len(), 1, "{via:?}: one decision per submission");
        let contexts = sent[0].contexts.clone().unwrap();
        assert_eq!(contexts.len(), 2);
        assert!(contexts.iter().all(|c| c.at.is_some()), "every context is pinned to its head");
        assert_eq!(sent[0].timeout_ms, Some(700));
        let state = serde_json::to_value(&sent[0].state).unwrap();
        assert_eq!(state["command"], "echo council-ran", "{via:?}: {state}");
        assert_eq!(state["context_type"], "default");
        rig.finish().await;
    }
}

/// A council ask leaves a pending ask carrying the council's signal and a
/// note naming its answer.
#[tokio::test]
async fn a_council_ask_leaves_an_ask_with_a_council_signal() {
    for via in BOTH {
        let rig = rig(via, Setup::default()).await;
        rig.mock.answers(&ASK);
        assert_pending(via, rig.submit("touch notes.txt").await);
        let ask = rig.only_ask();
        assert_eq!(ask.status, ApprovalStatus::Pending);
        assert!(ask.description.contains("council (shell-gate) answered ask"), "{via:?}: {}", ask.description);
        assert_eq!(the_decision(&rig, &ask).decision.outcome, CouncilOutcome::Ask);
        let signal = council_signal(&rig, &ask);
        assert_eq!(signal.verdict, SignalVerdict::Escalate);
        assert_eq!(signal.label.as_deref(), Some("ask"));
        assert!(signal.score.unwrap() < 0.98);
        rig.finish().await;
    }
}

/// What a test subscriber saw: span names with their recorded fields, and
/// event names with theirs.
#[derive(Default)]
struct Seen {
    spans: Mutex<Vec<(String, Vec<(String, String)>)>>,
    events: Mutex<Vec<(String, Vec<(String, String)>)>>,
}

struct Capture(Arc<Seen>);

struct Fields(Vec<(String, String)>);

impl tracing::field::Visit for Fields {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        self.0.push((field.name().to_string(), format!("{value:?}")));
    }
    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        self.0.push((field.name().to_string(), value.to_string()));
    }
}

impl<S> tracing_subscriber::Layer<S> for Capture
where
    S: tracing::Subscriber + for<'a> tracing_subscriber::registry::LookupSpan<'a>,
{
    // Concurrent tests hit the same callsites with no subscriber; asking
    // again on every hit keeps their cached "never" from hiding this test's.
    fn register_callsite(&self, _: &'static tracing::Metadata<'static>) -> tracing::subscriber::Interest {
        tracing::subscriber::Interest::sometimes()
    }
    fn on_new_span(&self, attrs: &tracing::span::Attributes<'_>, id: &tracing::span::Id, ctx: tracing_subscriber::layer::Context<'_, S>) {
        let mut fields = Fields(Vec::new());
        attrs.record(&mut fields);
        let mut spans = self.0.spans.lock().unwrap();
        let index = spans.len();
        spans.push((attrs.metadata().name().to_string(), fields.0));
        if let Some(span) = ctx.span(id) {
            span.extensions_mut().insert(index);
        }
    }
    fn on_record(&self, id: &tracing::span::Id, values: &tracing::span::Record<'_>, ctx: tracing_subscriber::layer::Context<'_, S>) {
        let Some(index) = ctx.span(id).and_then(|s| s.extensions().get::<usize>().copied()) else { return };
        let mut fields = Fields(Vec::new());
        values.record(&mut fields);
        self.0.spans.lock().unwrap()[index].1.extend(fields.0);
    }
    fn on_event(&self, event: &tracing::Event<'_>, _ctx: tracing_subscriber::layer::Context<'_, S>) {
        let mut fields = Fields(Vec::new());
        event.record(&mut fields);
        self.0.events.lock().unwrap().push((event.metadata().name().to_string(), fields.0));
    }
}

fn field<'a>(fields: &'a [(String, String)], name: &str) -> Option<&'a str> {
    fields.iter().rev().find(|(n, _)| n == name).map(|(_, v)| v.as_str())
}

/// A report asks, emits a `council.report` event naming the submission,
/// the seat, and each context's answer, and traces the decision with its
/// probabilities.
#[tokio::test]
async fn a_council_report_asks_and_emits_the_report_event() {
    use tracing_subscriber::layer::SubscriberExt;
    for via in BOTH {
        let seen = Arc::new(Seen::default());
        // With one scoped dispatcher registered, tracing-core decides a
        // callsite's interest from the registering thread's default, so a
        // span another test hits first is cached as never. A second live
        // dispatcher makes every registration consult both.
        let _second = tracing::Dispatch::new(tracing_subscriber::registry());
        let subscriber = tracing_subscriber::registry().with(Capture(seen.clone()));
        let _guard = tracing::subscriber::set_default(subscriber);
        tracing::callsite::rebuild_interest_cache();
        let rig = rig(via, Setup::default()).await;
        rig.mock.answers(&REPORT);
        assert_pending(via, rig.submit("rm -r build").await);
        let ask = rig.only_ask();
        assert_eq!(the_decision(&rig, &ask).decision.outcome, CouncilOutcome::Report);
        assert_eq!(council_signal(&rig, &ask).label.as_deref(), Some("report"));

        let events = seen.events.lock().unwrap();
        let (_, report) = events.iter().find(|(name, _)| name == "council.report").unwrap_or_else(|| panic!("{via:?}: no report event"));
        assert_eq!(field(report, "submission"), Some("rm -r build"), "{report:?}");
        assert_eq!(field(report, "context_id"), Some(rig.ctx.context_id.to_string().as_str()));
        assert_eq!(field(report, "actor_id"), Some(rig.ctx.actor_id.to_string().as_str()));
        let answers = field(report, "answers").unwrap();
        assert!(answers.contains("voice") && answers.contains("system-rules") && answers.contains("report"), "{answers}");
        drop(events);

        let spans = seen.spans.lock().unwrap();
        let (_, decide) = spans.iter().find(|(name, _)| name == "council.decide").unwrap_or_else(|| panic!("{via:?}: no council.decide span among {:?}", spans.iter().map(|(n, _)| n.as_str()).collect::<Vec<_>>()));
        assert_eq!(field(decide, "council.outcome"), Some("report"), "{decide:?}");
        for name in ["council.p_allow", "council.p_ask", "council.p_report", "council.agree", "council.spread",
                     "council.weight_hash", "council.engine", "council.ms", "council.prepare_ms"] {
            assert!(field(decide, name).is_some(), "{via:?}: {name} missing from {decide:?}");
        }
        drop(spans);
        rig.finish().await;
    }
}

fn dead_server() -> String {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    drop(listener);
    format!("http://{address}")
}

/// Every way the council fails to answer asks, and records the miss with
/// its cause: the server down, the deadline passed, numbers that do not
/// recompute, and an identity with no threshold.
#[tokio::test]
async fn a_council_miss_asks_and_records_its_cause() {
    let cases: [(&str, Setup, Option<fn(&Mock)>, &str); 4] = [
        ("down", Setup { server: Some(dead_server()), ..Setup::default() }, None, "identity failed"),
        (
            "slow",
            Setup { deadline_ms: 300, ..Setup::default() },
            Some(|m: &Mock| m.set(|req| Reply { delay: Duration::from_secs(3), ..Reply::ok(answer(req, &ALLOW)) })),
            "no answer within the 300 ms deadline",
        ),
        (
            "mismatch",
            Setup::default(),
            Some(|m: &Mock| m.edited(&ALLOW, |v| v["answers"]["verdict"]["probabilities"]["allow"] = serde_json::json!(0.9999))),
            "do not recompute",
        ),
        (
            "identity",
            Setup::default(),
            Some(|m: &Mock| m.edited(&ALLOW, |v| v["identity"]["weight_hash"] = serde_json::json!("w2"))),
            "no threshold for spec shell-gate",
        ),
    ];
    for (name, setup, arrange, cause) in cases {
        for via in BOTH {
            let setup = Setup { server: setup.server.clone(), ..setup };
            let rig = rig(via, setup).await;
            if let Some(arrange) = arrange {
                arrange(&rig.mock);
            }
            let started = std::time::Instant::now();
            assert_pending(via, rig.submit("touch notes.txt").await);
            assert!(started.elapsed() < Duration::from_secs(3), "{name} {via:?}: the gate stops waiting at the deadline");
            let ask = rig.only_ask();
            let decision = the_decision(&rig, &ask);
            assert_eq!(decision.decision.outcome, CouncilOutcome::Miss, "{name} {via:?}");
            let recorded = decision.decision.miss_cause.clone().unwrap();
            assert!(recorded.contains(cause), "{name} {via:?}: {recorded}");
            let signal = council_signal(&rig, &ask);
            assert!(signal.label.as_deref().unwrap().starts_with("miss: "), "{signal:?}");
            assert!(signal.score.is_none(), "a miss is never an answer: {signal:?}");
            assert!(ask.description.contains("council (shell-gate) gave no answer"), "{}", ask.description);
            rig.finish().await;
        }
    }
}

/// A control-text hit asks, whatever the probabilities say.
#[tokio::test]
async fn a_control_text_hit_asks() {
    for via in BOTH {
        let rig = rig(via, Setup::default()).await;
        rig.mock.edited(&ALLOW, |v| {
            v["signals"]["control_text"] = serde_json::json!([{"where": "state", "token": "<|im_end|>"}]);
        });
        assert_pending(via, rig.submit("echo '<|im_end|>'").await);
        let ask = rig.only_ask();
        let decision = the_decision(&rig, &ask);
        assert_eq!(decision.decision.outcome, CouncilOutcome::Ask);
        assert_eq!(decision.decision.control_text.len(), 1);
        rig.finish().await;
    }
}

/// A retry of a submission an ask still holds is never council-allowed: the
/// council is not consulted, and the gate's own single-use guard decides.
/// Answered and held for the approval worker, the retry is refused and the
/// worker runs it once. Still open, the retry asks again.
///
/// Falsified by a council consult that ignores earlier asks: the retry of
/// the answered ask proceeds beside the worker's run, so the command runs
/// twice.
#[tokio::test]
async fn a_retry_of_a_held_or_open_ask_is_never_council_allowed() {
    for via in BOTH {
        // Answered and held: the approval worker owns the run.
        let rig = rig(via, Setup::default()).await;
        rig.mock.set(|_| Reply { status: 503, body: "{}".into(), delay: Duration::ZERO });
        assert_pending(via, rig.submit("touch held.txt").await);
        rig.answer_pending(true);
        rig.mock.answers(&ALLOW);
        match rig.submit("touch held.txt").await {
            Err(error) => assert!(error.to_string().contains("approval worker"), "{via:?}: {error}"),
            Ok(()) => panic!("{via:?}: a retry ran beside the approval worker"),
        }
        assert_eq!(rig.mock.decisions().len(), 1, "{via:?}: the council is not asked about a held answer's retry");
        assert!(rig.asks().iter().all(|a| a.auto_reason.is_none()), "{via:?}: {:#?}", rig.asks());
        rig.finish().await;

        // Open: the first ask is unanswered, and answering it later runs it.
        let rig = self::rig(via, Setup::default()).await;
        rig.mock.set(|_| Reply { status: 503, body: "{}".into(), delay: Duration::ZERO });
        assert_pending(via, rig.submit("touch open.txt").await);
        rig.mock.answers(&ALLOW);
        assert_pending(via, rig.submit("touch open.txt").await);
        assert_eq!(rig.mock.decisions().len(), 1, "{via:?}: the council is not asked about an open ask's retry");
        assert!(rig.asks().iter().all(|a| a.auto_reason.is_none()), "{via:?}: {:#?}", rig.asks());
        rig.finish().await;
    }
}

/// The approval worker collects an answer before it runs the command, so
/// from its claim until the run settles no unanswered or uncollected ask
/// holds the submission. A retry in that window is never council-allowed;
/// once the worker's run settles, the council may allow the same command
/// again.
///
/// Falsified by a guard that reads only open asks and uncollected answers:
/// the retry runs beside the worker's run.
#[tokio::test]
async fn a_retry_while_the_approval_worker_runs_the_command_is_never_council_allowed() {
    for via in BOTH {
        let rig = rig(via, Setup::default()).await;
        rig.d.kernel().start_approval_delivery().unwrap();
        rig.mock.set(|_| Reply { status: 503, body: "{}".into(), delay: Duration::ZERO });
        let command = "sleep 2";
        assert_pending(via, rig.submit(command).await);
        let request = rig.only_ask().request_id;
        rig.answer_pending(true);
        crate::kj::gate::announce_ledger_change(rig.d.kernel_db(), rig.d.kernel().ledger_flows());
        let redeemed = || {
            let db = rig.d.kernel_db();
            let db = db.lock();
            approval_ledger::ask::redeemed_at(db.conn_for_ledger(), &request).unwrap().is_some()
        };
        tokio::time::timeout(Duration::from_secs(5), async {
            while !redeemed() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("the approval worker claims the answer");

        rig.mock.answers(&ALLOW);
        match rig.submit(command).await {
            Err(_) => {}
            Ok(()) => panic!("{via:?}: a retry ran beside the approval worker's run"),
        }
        assert_eq!(rig.mock.decisions().len(), 1, "{via:?}: the council is not asked while the worker runs it: {:#?} {:#?}",
            rig.decisions().iter().map(|d| (d.decision.outcome, d.decision.miss_cause.clone())).collect::<Vec<_>>(), rig.asks());

        let settled = || {
            rig.d.kernel().shell_operations().get_by_ask(&request, rig.ctx.context_id).unwrap()
                .is_some_and(|state| state.completed_at.is_some())
        };
        tokio::time::timeout(Duration::from_secs(10), async {
            while !settled() {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("{via:?}: the worker's run settles: {:?} {:#?}",
            rig.d.kernel().shell_operations().get_by_ask(&request, rig.ctx.context_id),
            rig.d.kernel().shell_operations().list_for_context(rig.ctx.context_id)));
        // The retry made during the run left its own ask, which holds the
        // submission until someone settles it.
        {
            let db = rig.d.kernel_db();
            let db = db.lock();
            for ask in approval_ledger::ask::list_pending(db.conn_for_ledger()).unwrap() {
                approval_ledger::decide::abandon(db.conn_for_ledger(), &ask.request_id, Some("test")).unwrap();
            }
        }
        rig.submit(command).await.unwrap_or_else(|e| panic!("{via:?}: once the run settled the council may allow it: {e:?}"));
        assert_eq!(rig.mock.decisions().len(), 2, "{via:?}");
        rig.finish().await;
    }
}

/// Another hook's ask stands: the council never reads it.
#[tokio::test]
async fn another_hook_s_ask_stands() {
    use crate::mcp::error::HookId;
    use crate::mcp::hook_table::{GlobPattern, HookAction, HookEntry};
    use crate::mcp::AskSpec;
    for via in BOTH {
        let rig = rig(via, Setup::default()).await;
        rig.broker.hooks().write().await.pre_call.entries.push(HookEntry {
            id: HookId("scorer".into()),
            match_instance: None,
            match_tool: Some(GlobPattern("shell_write".into())),
            match_context: None,
            match_principal: None,
            action: HookAction::Ask(AskSpec { description: Some("the scorer is unsure".into()) }),
            priority: 0,
            kaish_script_id: None,
        });
        assert_pending(via, rig.submit("touch notes.txt").await);
        let ask = rig.only_ask();
        assert_eq!(ask.hook_id.as_deref(), Some("scorer"));
        assert!(rig.mock.decisions().is_empty(), "{via:?}: the council read a hook's ask");
        assert!(rig.decisions().is_empty());
        rig.finish().await;
    }
}

/// Dry runs and live root characters on the RPC paths never reach the
/// council; the dry run says it would have. On the tool path a root asks
/// today, and the council reads for it like any seat.
#[tokio::test]
async fn dry_runs_and_rpc_roots_never_reach_the_council() {
    let rig = rig(Via::Rpc, Setup { global: "ask = [\"git push\"]", ..Setup::default() }).await;
    let report = rig
        .broker
        .shell_pre_call_hooks_dry_run("touch notes.txt", &rig.ctx, &CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(report.outcome, crate::mcp::DryRunOutcome::WouldAsk, "{report:?}");
    assert!(report.reason.as_deref().unwrap().contains("The council would read this submission"), "{report:?}");
    assert!(rig.mock.decisions().is_empty(), "a dry run consults nobody");

    rig.seat_a_root();
    rig.submit("touch notes.txt").await.expect("a root's uncovered statement runs");
    assert_pending(Via::Rpc, rig.submit("git push origin main").await);
    assert!(rig.mock.decisions().is_empty(), "a root's asks never reach the council on the RPC path");
    assert!(rig.decisions().is_empty());
    rig.finish().await;

    let tool = rig_root_tool().await;
    tool.submit("echo root-ran").await.expect("the council allows the root's submission");
    assert_eq!(tool.mock.decisions().len(), 1, "on the tool path the council reads for a root");
    tool.finish().await;
}

async fn rig_root_tool() -> Rig {
    let rig = rig(Via::Tool, Setup::default()).await;
    rig.seat_a_root();
    rig.mock.answers(&ALLOW);
    rig
}

/// A static ask stays firm: the council reads the submission for the
/// record, and the gate still asks.
#[tokio::test]
async fn a_static_ask_still_asks_after_a_council_allow() {
    for via in BOTH {
        let rig = rig(via, Setup { global: "ask = [\"git push\"]", ..Setup::default() }).await;
        rig.mock.answers(&ALLOW);
        assert_pending(via, rig.submit("touch notes.txt; git push origin main").await);
        let ask = rig.only_ask();
        assert_eq!(ask.status, ApprovalStatus::Pending);
        assert_eq!(the_decision(&rig, &ask).decision.outcome, CouncilOutcome::Allow, "recorded for the record");
        rig.finish().await;
    }
}

/// With the council off for the context type, nothing changes: the same
/// ask with the same description, no signal, no record, no request.
#[tokio::test]
async fn with_the_council_disabled_nothing_changes() {
    for via in BOTH {
        let rig = rig(via, Setup { enabled: false, ..Setup::default() }).await;
        assert_pending(via, rig.submit("touch notes.txt").await);
        let ask = rig.only_ask();
        let expected = match via {
            Via::Rpc => "gate policy: no layer covers statement #0 (`touch notes.txt`) and the actor is not a root character",
            Via::Tool => "shell_write: 1 statement(s) — touch notes.txt",
        };
        assert_eq!(ask.description, expected, "{via:?}");
        assert!(rig.signals(&ask.request_id).is_empty());
        assert!(rig.decisions().is_empty());
        assert!(rig.mock.decisions().is_empty());
        rig.finish().await;
    }
}

/// A statically allowed submission never reaches the council.
#[tokio::test]
async fn a_static_allow_never_reaches_the_council() {
    for via in BOTH {
        let rig = rig(via, Setup { global: "allow = [\"echo\"]", ..Setup::default() }).await;
        rig.submit("echo static").await.unwrap();
        assert!(rig.mock.decisions().is_empty(), "{via:?}");
        rig.finish().await;
    }
}

/// A decision that cannot be recorded refuses the submission: the ask and
/// its decision roll back together, and nothing runs.
#[tokio::test]
async fn a_decision_that_cannot_be_recorded_refuses_the_submission() {
    for via in BOTH {
        let rig = rig(via, Setup::default()).await;
        rig.mock.answers(&ALLOW);
        rig.d.kernel_db().lock().conn_for_ledger().execute_batch("DROP TABLE council_control_text; DROP TABLE council_pooled;").unwrap();
        match rig.submit("echo never").await {
            Err(error) => assert!(error.is_refusal(RefusalKind::GateUnavailable), "{via:?}: {error:?}"),
            Ok(()) => panic!("{via:?}: an unrecorded decision allowed a submission"),
        }
        assert!(rig.asks().is_empty(), "{via:?}: the ask rolled back with its decision: {:#?}", rig.asks());
        rig.finish().await;
    }
}
