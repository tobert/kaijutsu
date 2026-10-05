//! The council in the gate, end to end: a kernel, its broker, and a council
//! server on 127.0.0.1 that answers the contract with numbers
//! `math::verify` recomputes. Each scenario runs on both paths a shell
//! submission takes to the gate: the RPC shell path (`shell_pre_call_hooks`,
//! whose gate-policy ask the council decides) and the `shell_write` tool
//! (`run_gate` for `Origin::ShellGate`).

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use approval_ledger::council::{CouncilDecision, CouncilOutcome};
use approval_ledger::council_observation::{
    CouncilObservation, CouncilObservationOutcome, list_council_observations_for_decision, list_council_voice_skips,
};
use approval_ledger::types::{ApprovalRow, ApprovalStatus, SignalRow, SignalSourceKind, SignalVerdict};
use kaijutsu_council::wire::DecisionRequest;
use kaijutsu_types::{PrincipalId, RefusalKind, SessionId};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

use super::gate::test_support::{answer, identity};
use super::observe::test_support::follows_answer;
use super::projection::fixtures::{append_dialogue, character, live_context};
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

/// A hold on the next decision: the server signals `reached` once it has the
/// request and its reply, then answers only after `release` fires.
struct Hold {
    reached: tokio::sync::oneshot::Sender<()>,
    release: tokio::sync::oneshot::Receiver<()>,
}

/// A council server on 127.0.0.1 that holds whatever it is sent and answers
/// decisions the way the test says.
struct Mock {
    base: String,
    decisions: Arc<Mutex<Vec<DecisionRequest>>>,
    behavior: Arc<Mutex<Behavior>>,
    /// Every request's method and path, in arrival order.
    calls: Arc<Mutex<Vec<(String, String)>>>,
    /// Delays for the next `PUT`s, one each, in order.
    put_delays: Arc<Mutex<VecDeque<Duration>>>,
    /// Holds the next decision until the test releases it.
    hold: Arc<Mutex<Option<Hold>>>,
}

impl Mock {
    /// How many requests used `method` on `path`.
    fn count(&self, method: &str, path: &str) -> usize {
        self.calls.lock().unwrap().iter().filter(|(m, p)| m == method && p.starts_with(path)).count()
    }

    /// Hold each of the next `PUT`s for the given delay before it answers.
    fn delay_puts(&self, delays: &[Duration]) {
        self.put_delays.lock().unwrap().extend(delays.iter().copied());
    }

    /// Hold the next decision: the first receiver fires when the server has
    /// it, and the server answers once the sender fires.
    fn hold_next_decision(&self) -> (tokio::sync::oneshot::Receiver<()>, tokio::sync::oneshot::Sender<()>) {
        let (reached, at) = tokio::sync::oneshot::channel();
        let (go, release) = tokio::sync::oneshot::channel();
        *self.hold.lock().unwrap() = Some(Hold { reached, release });
        (at, go)
    }

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
    let calls: Arc<Mutex<Vec<(String, String)>>> = Arc::default();
    let put_delays: Arc<Mutex<VecDeque<Duration>>> = Arc::default();
    let hold: Arc<Mutex<Option<Hold>>> = Arc::default();
    let (log, how, seen, held) = (decisions.clone(), behavior.clone(), calls.clone(), put_delays.clone());
    let holding = hold.clone();
    tokio::spawn(async move {
        let puts = Arc::new(std::sync::atomic::AtomicU64::new(0));
        loop {
            let Ok((mut sock, _)) = listener.accept().await else { return };
            let (log, how, puts, seen, held) = (log.clone(), how.clone(), puts.clone(), seen.clone(), held.clone());
            let holding = holding.clone();
            tokio::spawn(async move {
                let Some((method, path, body)) = read_request(&mut sock).await else { return };
                seen.lock().unwrap().push((method.clone(), path.clone()));
                let reply = match (method.as_str(), path.as_str()) {
                    ("GET", "/council/v1/identity") => Reply { status: 200, body: server_identity(), delay: Duration::ZERO },
                    ("POST", "/council/v1/specs") => {
                        let spec: kaijutsu_council::wire::Spec = serde_json::from_str(&body).unwrap();
                        let id = kaijutsu_council::canon::spec_id(&spec).unwrap();
                        Reply::ok(serde_json::json!({"spec_id": id, "spec": spec, "template": "mk-letters-1:0123456789abcdef"}))
                    }
                    ("PUT", p) if p.starts_with("/council/v1/contexts/") => {
                        let n = puts.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
                        let delay = held.lock().unwrap().pop_front().unwrap_or(Duration::ZERO);
                        Reply {
                            delay,
                            ..Reply::ok(serde_json::json!({"id": p.rsplit('/').next().unwrap(), "head": format!("snap:{n:064x}"),
                                                           "tokens": 1, "kept": 0, "fed": 1, "dry_run": false, "snapshots": []}))
                        }
                    }
                    ("POST", "/council/v1/decisions") => {
                        let request: DecisionRequest = serde_json::from_str(&body).unwrap();
                        log.lock().unwrap().push(request.clone());
                        let behavior = how.lock().unwrap().clone();
                        let reply = behavior(&request);
                        let hold = holding.lock().unwrap().take();
                        if let Some(Hold { reached, release }) = hold {
                            let _ = reached.send(());
                            let _ = release.await;
                        }
                        reply
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
    Mock { base, decisions, behavior, calls, put_delays, hold }
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
    /// `[council] voices`.
    voices: bool,
    /// The seat is reviewed by banto, a model character, and forked from a
    /// context Amy, a live root, plays; `[council] contexts` is
    /// `["council-system"]`. Otherwise the seat's reviewer is a lone
    /// character and the contexts are `["voice", "system-rules"]`.
    chain: bool,
    /// `gate.toml` declares the program spec, `program-gate`, with a
    /// threshold for the test server.
    programs: bool,
    /// `[council] seat`.
    seat: bool,
    /// Bumper mode with this `bump_limit`, reading the `shell-bump` spec.
    bump_limit: Option<u64>,
    /// The bump template file is left out of the config directory.
    no_bump_template: bool,
    /// `mode = "bump-only"`, reading the `shell-bump` spec.
    bump_only: bool,
}

impl Default for Setup {
    fn default() -> Self {
        Setup {
            enabled: true,
            deadline_ms: 700,
            server: None,
            global: "",
            voices: false,
            chain: false,
            programs: false,
            seat: false,
            bump_limit: None,
            no_bump_template: false,
            bump_only: false,
        }
    }
}

fn gate_toml(server: &str, setup: &Setup) -> String {
    format!(
        r#"[global]
{global}

[council]
server = "{server}"
contexts = {contexts}
voices = {voices}
seat = {seat}
{mode}
pool = {{ method = "loglinear", weights = "mass" }}
deadline_ms = {deadline}

[[council.spec]]
name = "{shell}"
case = "shell"

[[council.threshold]]
spec = "{shell}"
weight_hash = "w1"
engine = "e1"
tokenizer_hash = "t1"
template = "mk-letters-1:0123456789abcdef"
allow_at = 0.98
mass_floor = -0.05

{programs}
[context_type.default.council]
enabled = {enabled}
"#,
        global = setup.global,
        contexts = if setup.chain { r#"["council-system"]"# } else { r#"["voice", "system-rules"]"# },
        voices = setup.voices,
        seat = setup.seat,
        mode = match (setup.bump_only, setup.bump_limit) {
            (true, _) => "mode = \"bump-only\"".to_string(),
            (false, Some(limit)) => format!("mode = \"bumper\"\nbump_limit = {limit}"),
            (false, None) => String::new(),
        },
        shell = if setup.bump_limit.is_some() || setup.bump_only { "shell-bump" } else { "shell-gate" },
        deadline = setup.deadline_ms,
        enabled = setup.enabled,
        programs = if setup.programs { PROGRAM_SPEC_TOML } else { "" },
    )
}

/// The program spec and its threshold, as `gate.toml` lines.
const PROGRAM_SPEC_TOML: &str = r#"
[[council.spec]]
name = "program-gate"
case = "program"

[[council.threshold]]
spec = "program-gate"
weight_hash = "w1"
engine = "e1"
tokenizer_hash = "t1"
template = "mk-letters-1:0123456789abcdef"
allow_at = 0.98
mass_floor = -1.5
"#;

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
    vfs.write_all(
        std::path::Path::new("/config/kernel/council/shell-bump.json"),
        crate::config_seed::DEFAULT_COUNCIL_SHELL_BUMP.as_bytes(),
    )
    .await
    .unwrap();
    if setup.no_bump_template {
        let _ = vfs.unlink(std::path::Path::new("/config/kernel/council/bump.md")).await;
    } else {
        vfs.write_all(
            std::path::Path::new("/config/kernel/council/bump.md"),
            crate::config_seed::DEFAULT_COUNCIL_BUMP_MESSAGE.as_bytes(),
        )
        .await
        .unwrap();
    }
    vfs.write_all(
        std::path::Path::new("/config/kernel/council/program-gate.json"),
        crate::config_seed::DEFAULT_COUNCIL_PROGRAM_GATE.as_bytes(),
    )
    .await
    .unwrap();
    d.kernel().mount("/work", crate::vfs::MemoryBackend::new()).await;
    vfs.write_all(
        std::path::Path::new("/config/kernel/council/direction-check.json"),
        crate::config_seed::DEFAULT_COUNCIL_DIRECTION_CHECK.as_bytes(),
    )
    .await
    .unwrap();

    let actor = PrincipalId::new();
    let (reviewer, parent) = if setup.chain {
        let system = live_context(d.kernel(), "council-system");
        append_dialogue(d.kernel(), system, &["never force push"]);
        // The test dispatcher seeds `amy`, a live root, and her root context.
        let banto = character(d.kernel(), "banto", false);
        (banto, Some(crate::kj::test_helpers::register_root_context(&d)))
    } else {
        live_context(d.kernel(), "voice");
        live_context(d.kernel(), "system-rules");
        (character(d.kernel(), "council-seat-reviewer", false), None)
    };
    let context_id = crate::kj::test_helpers::register_context(&d, Some("council-seat"), parent, actor);
    d.block_store().create_document(context_id, kaijutsu_types::DocKind::Conversation, None).unwrap();
    d.kernel_db().lock().update_context_review(context_id, Some(actor), Some(reviewer)).unwrap();
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
                ShellHookVerdict::Proceed(_) => Ok(()),
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

    /// Submit `command` and report only what the gate did: `Ok` when it let
    /// the command run, whatever the command then did.
    async fn submit_gate(&self, command: &str) -> Result<(), McpError> {
        match self.via {
            Via::Rpc => self.submit(command).await,
            Via::Tool => {
                let params = KernelCallParams {
                    instance: InstanceId::new(ShellServer::INSTANCE_WRITE),
                    tool: ShellServer::TOOL_WRITE.to_string(),
                    arguments: serde_json::json!({ "command": command }),
                };
                self.broker.call_tool(params, &self.ctx, CancellationToken::new()).await.map(|_| ())
            }
        }
    }

    /// Write a file under the rig's `/work` mount.
    async fn write(&self, path: &str, text: &str) {
        use crate::vfs::VfsOps;
        self.d.kernel().vfs().write_all(std::path::Path::new(path), text.as_bytes()).await.unwrap();
    }

    /// The programs recorded under `decision`.
    fn programs(&self, decision: &CouncilDecision) -> Vec<approval_ledger::council::CouncilProgram> {
        let db = self.d.kernel_db();
        let db = db.lock();
        approval_ledger::council::list_council_programs(db.conn_for_ledger(), &decision.decision_id).unwrap()
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

    /// The id of the live context labeled `label`.
    fn context_of(&self, label: &str) -> String {
        let db = self.d.kernel_db();
        let db = db.lock();
        db.find_context_by_label(label).unwrap().unwrap_or_else(|| panic!("no context {label}")).context_id.to_string()
    }

    /// The ids a decision request read, in order.
    fn read_ids(request: &DecisionRequest) -> Vec<String> {
        request.contexts.iter().flatten().map(|c| c.id.clone()).collect()
    }

    /// Every decision request except the reads of the voice `observed` alone.
    fn gate_requests(&self, observed: &str) -> Vec<DecisionRequest> {
        self.mock.decisions().into_iter().filter(|r| Self::read_ids(r) != [observed.to_string()]).collect()
    }

    /// The observations recorded against `decision_id`, once there are
    /// `want` of them; the observation runs on its own task.
    async fn observations(&self, decision_id: &[u8], want: usize) -> Vec<CouncilObservation> {
        let read = || {
            let db = self.d.kernel_db();
            let db = db.lock();
            list_council_observations_for_decision(db.conn_for_ledger(), decision_id).unwrap()
        };
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let rows = read();
                if rows.len() >= want {
                    return rows;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("{:?}: {want} observation(s) expected, found {:?}", self.via, read()))
    }

    fn skips(&self, decision_id: &[u8]) -> Vec<String> {
        let db = self.d.kernel_db();
        let db = db.lock();
        list_council_voice_skips(db.conn_for_ledger(), decision_id).unwrap().into_iter().map(|s| s.character_name).collect()
    }
}

/// A confident allow from each context the request reads.
fn allow_each(request: &DecisionRequest) -> Reply {
    let n = request.contexts.as_ref().map(Vec::len).unwrap_or(0);
    Reply::ok(answer(request, &vec![[-0.001, -8.0, -9.0]; n]))
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
/// the seat, each context's answer, and what stopping the seat did, and
/// traces the decision with its probabilities and the same stop.
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
        // The submitter has no character sheet, so the seat is autonomous;
        // no turn runs here and no continuation is open.
        assert_eq!(field(report, "council.report_stop"), Some("nothing_running"), "{report:?}");
        drop(events);

        let spans = seen.spans.lock().unwrap();
        let (_, decide) = spans.iter().find(|(name, _)| name == "council.decide").unwrap_or_else(|| panic!("{via:?}: no council.decide span among {:?}", spans.iter().map(|(n, _)| n.as_str()).collect::<Vec<_>>()));
        assert_eq!(field(decide, "council.outcome"), Some("report"), "{decide:?}");
        assert_eq!(field(decide, "council.report_stop"), Some("nothing_running"), "{decide:?}");
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
            Err(error) => {
                let text = error.to_string();
                assert!(text.contains("approval worker"), "{via:?}: {error}");
                // The seat that is told this is a worker seat: it reads the
                // ask with a verb it holds, never with a house verb.
                assert!(text.contains("kj wait --ask "), "{via:?}: {error}");
                assert!(!text.contains("kj ledger show"), "{via:?}: {error}");
            }
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

/// An identical ask raised, answered, and collected while a council decision
/// is in flight holds the submission: the approval worker owns that run, so
/// the council's allow is not applied and the submission asks. The ask is
/// collected without a recorded run, which counts as unsettled.
///
/// Falsified by an in-lock re-check that reads only open asks and
/// uncollected answers: the council-allowed submission runs beside the
/// worker's run.
#[tokio::test]
async fn an_identical_ask_collected_during_the_decision_holds_the_council_allow() {
    for via in BOTH {
        let rig = rig(via, Setup { deadline_ms: 30_000, ..Setup::default() }).await;
        rig.mock.answers(&ALLOW);
        let (reached, release) = rig.mock.hold_next_decision();
        let command = "echo raced";
        let allowed = rig.submit(command);
        let race = async {
            reached.await.expect("the first decision reaches the council");
            rig.mock.set(|_| Reply { status: 503, body: "{}".into(), delay: Duration::ZERO });
            assert_pending(via, rig.submit(command).await);
            let collected = rig.only_ask().request_id;
            rig.answer_pending(true);
            assert!(rig.d.kernel_db().lock().redeem_ask(&collected).unwrap(), "{via:?}: the worker's claim");
            release.send(()).expect("the held decision is still waiting");
            collected
        };
        let (allowed, collected) = tokio::join!(allowed, race);
        assert_pending(via, allowed);

        assert_eq!(rig.mock.decisions().len(), 2, "{via:?}");
        let asks = rig.asks();
        assert_eq!(asks.len(), 2, "{via:?}: {asks:#?}");
        assert!(asks.iter().all(|a| a.auto_reason.is_none()), "{via:?}: nothing was council-allowed: {asks:#?}");
        let held = asks.iter().find(|a| a.request_id != collected).unwrap();
        assert_eq!(held.status, ApprovalStatus::Pending, "{via:?}");
        assert_eq!(the_decision(&rig, held).decision.outcome, CouncilOutcome::Allow, "{via:?}: the council allowed it");
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

/// The decision the gate made for the one submission in `rig`: the newest
/// for its context.
fn latest_decision(rig: &Rig) -> CouncilDecision {
    rig.decisions().into_iter().max_by_key(|d| d.created_at).expect("a council decision")
}

/// With voices on, a seat reviewed by banto for Amy reads the system rules
/// and Amy's voice in the decision, and banto's voice is read after it,
/// alone under `direction-check`, recorded against the decision.
///
/// Falsified by `consult` reading only `[council] contexts`: the request
/// reads one context. Falsified by no observation spawn: nothing is
/// recorded against the decision.
#[tokio::test]
async fn voices_compose_amy_into_the_decision_and_banto_observes_it() {
    for via in BOTH {
        let rig = rig(via, Setup { voices: true, chain: true, ..Setup::default() }).await;
        let amy = live_context(rig.d.kernel(), "council-amy");
        append_dialogue(rig.d.kernel(), amy, &["keep main green"]);
        let banto = live_context(rig.d.kernel(), "council-banto");
        append_dialogue(rig.d.kernel(), banto, &["only touch the parser crate"]);
        let banto_id = banto.to_string();
        let observed = banto_id.clone();
        rig.mock.set(move |req| {
            if Rig::read_ids(req) == [observed.clone()] {
                Reply::ok(follows_answer(req, [-2.0, -0.2, -3.0]))
            } else {
                allow_each(req)
            }
        });
        rig.submit("echo voices").await.unwrap_or_else(|e| panic!("{via:?}: the council allows: {e:?}"));

        let sent = rig.gate_requests(&banto_id);
        assert_eq!(sent.len(), 1, "{via:?}: one gate decision");
        assert_eq!(
            Rig::read_ids(&sent[0]),
            [rig.context_of("council-system"), rig.context_of("council-amy")],
            "{via:?}: the decision reads the system rules, then Amy's voice"
        );
        let decision = the_decision(&rig, &rig.only_ask());
        assert_eq!(decision.decision.outcome, CouncilOutcome::Allow);
        assert!(rig.skips(&decision.decision_id).is_empty(), "{via:?}: every voice is held");

        let rows = rig.observations(&decision.decision_id, 1).await;
        assert_eq!(rows.len(), 1, "{via:?}: {rows:#?}");
        let o = &rows[0].observation;
        assert_eq!(o.seat_label, "council-banto");
        assert_eq!(o.spec_name, "direction-check");
        assert_eq!(o.outcome, CouncilObservationOutcome::Answered, "{via:?}: {o:?}");
        assert_eq!(o.choice.as_deref(), Some("strays"));
        let observed: Vec<DecisionRequest> =
            rig.mock.decisions().into_iter().filter(|r| Rig::read_ids(r) == [banto_id.clone()]).collect();
        assert_eq!(observed.len(), 1, "{via:?}: banto's voice is read once, alone");
        assert_eq!(serde_json::to_value(&observed[0].state).unwrap()["command"], "echo voices");
        assert_ne!(observed[0].spec_id, sent[0].spec_id, "the observation reads its own spec");
        rig.finish().await;
    }
}

/// A character on the chain with no voice context is skipped, and the skip
/// is recorded with the decision; the decision still reads Amy's voice.
///
/// Falsified by a decision record written without the chain's skips.
#[tokio::test]
async fn a_character_without_a_voice_is_recorded_as_a_skip() {
    for via in BOTH {
        let rig = rig(via, Setup { voices: true, chain: true, ..Setup::default() }).await;
        live_context(rig.d.kernel(), "council-amy");
        rig.mock.set(allow_each);
        rig.submit("echo skipped").await.unwrap_or_else(|e| panic!("{via:?}: the council allows: {e:?}"));
        let decision = the_decision(&rig, &rig.only_ask());
        assert_eq!(rig.skips(&decision.decision_id), ["banto"], "{via:?}");
        assert_eq!(rig.mock.decisions().len(), 1, "{via:?}: nothing to observe");
        assert_eq!(Rig::read_ids(&rig.mock.decisions()[0]).len(), 2, "{via:?}");
        rig.finish().await;
    }
}

/// An observation the server refuses is the observation's own miss: the
/// gate's outcome stands and the miss is recorded with its cause.
#[tokio::test]
async fn an_observation_that_fails_leaves_the_gate_alone() {
    for via in BOTH {
        let rig = rig(via, Setup { voices: true, chain: true, ..Setup::default() }).await;
        live_context(rig.d.kernel(), "council-amy");
        let banto = live_context(rig.d.kernel(), "council-banto").to_string();
        let observed = banto.clone();
        rig.mock.set(move |req| {
            if Rig::read_ids(req) == [observed.clone()] {
                Reply { status: 500, body: "boom".into(), delay: Duration::ZERO }
            } else {
                allow_each(req)
            }
        });
        rig.submit("echo observed").await.unwrap_or_else(|e| panic!("{via:?}: the council allows: {e:?}"));
        let ask = rig.only_ask();
        assert_eq!(ask.status, ApprovalStatus::Allowed, "{via:?}");
        let decision = the_decision(&rig, &ask);
        assert_eq!(decision.decision.outcome, CouncilOutcome::Allow);
        let rows = rig.observations(&decision.decision_id, 1).await;
        let o = &rows[0].observation;
        assert_eq!(o.outcome, CouncilObservationOutcome::Miss, "{via:?}: {o:?}");
        assert!(o.miss_cause.as_deref().unwrap().contains("500"), "{via:?}: {o:?}");
        rig.finish().await;
    }
}

/// With voices off, a decision reads exactly `[council] contexts`, even
/// when the reviewer chain has voices, and nothing observes it.
#[tokio::test]
async fn with_voices_off_the_decision_reads_the_configured_contexts() {
    for via in BOTH {
        let rig = rig(via, Setup { chain: true, ..Setup::default() }).await;
        live_context(rig.d.kernel(), "council-amy");
        live_context(rig.d.kernel(), "council-banto");
        rig.mock.set(allow_each);
        rig.submit("echo configured").await.unwrap_or_else(|e| panic!("{via:?}: the council allows: {e:?}"));
        let sent = rig.mock.decisions();
        assert_eq!(sent.len(), 1, "{via:?}");
        assert_eq!(Rig::read_ids(&sent[0]), [rig.context_of("council-system")], "{via:?}");
        let decision = latest_decision(&rig);
        assert!(rig.skips(&decision.decision_id).is_empty());
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(rig.mock.decisions().len(), 1, "{via:?}: no observation is read");
        rig.finish().await;

        let rig = self::rig(via, Setup::default()).await;
        rig.mock.answers(&ALLOW);
        rig.submit("echo configured").await.unwrap();
        let sent = rig.mock.decisions();
        assert_eq!(Rig::read_ids(&sent[0]), [rig.context_of("voice"), rig.context_of("system-rules")], "{via:?}");
        rig.finish().await;
    }
}

/// A decision's 404 naming the spec is a miss, and the next decision posts
/// the spec again before it asks. The contexts the server still holds are
/// not sent again.
///
/// Falsified by a decision failure that leaves the spec marked posted: the
/// second decision is another 404.
#[tokio::test]
async fn a_spec_404_posts_the_spec_again_on_the_next_decision() {
    for via in BOTH {
        let rig = rig(via, Setup::default()).await;
        let forgot = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let once = forgot.clone();
        rig.mock.set(move |req| {
            let spec = req.spec_id.as_ref().unwrap().to_string();
            if once.swap(false, std::sync::atomic::Ordering::SeqCst) {
                let body = serde_json::json!({"error": {"type": "not_found", "message": format!("unknown spec {spec}")}});
                Reply { status: 404, body: body.to_string(), delay: Duration::ZERO }
            } else {
                Reply::ok(answer(req, &ALLOW))
            }
        });
        assert_pending(via, rig.submit("touch first.txt").await);
        let first = latest_decision(&rig);
        assert_eq!(first.decision.outcome, CouncilOutcome::Miss, "{via:?}");
        assert!(first.decision.miss_cause.as_deref().unwrap().contains("404"), "{first:?}");
        assert_eq!(rig.mock.count("POST", "/council/v1/specs"), 1);
        let puts = rig.mock.count("PUT", "/council/v1/contexts/");

        rig.submit("echo second").await.unwrap_or_else(|e| panic!("{via:?}: the second decision allows: {e:?}"));
        assert_eq!(rig.mock.count("POST", "/council/v1/specs"), 2, "{via:?}: the spec is posted again");
        assert_eq!(rig.mock.count("PUT", "/council/v1/contexts/"), puts, "{via:?}: the contexts are still held");
        rig.finish().await;
    }
}

/// One deadline bounds the whole decision, prepare included. A server
/// slow on every `PUT`, each call inside the deadline on its own, is a miss
/// at the deadline, during prepare. A prepare that spends most of the
/// deadline leaves the decision only what remains.
///
/// Falsified by a deadline per call: the first case waits for both `PUT`s
/// and then decides, and the second waits a whole deadline for the answer.
#[tokio::test]
async fn one_deadline_bounds_prepare_and_the_decision_together() {
    const DEADLINE: u64 = 1000;
    let bound = Duration::from_millis(DEADLINE + 400);
    for via in BOTH {
        let rig = rig(via, Setup { deadline_ms: DEADLINE, ..Setup::default() }).await;
        rig.mock.answers(&ALLOW);
        rig.mock.delay_puts(&[Duration::from_millis(900), Duration::from_millis(900)]);
        let started = std::time::Instant::now();
        assert_pending(via, rig.submit("touch slow.txt").await);
        let took = started.elapsed();
        assert!(took < bound, "{via:?}: the decision took {took:?}, past one {DEADLINE} ms deadline");
        let decision = latest_decision(&rig);
        let cause = decision.decision.miss_cause.clone().unwrap_or_default();
        assert!(cause.contains("deadline passed during prepare"), "{via:?}: {cause}");
        assert!(rig.mock.decisions().is_empty(), "{via:?}: no decision was asked");
        rig.finish().await;

        let rig = self::rig(via, Setup { deadline_ms: DEADLINE, ..Setup::default() }).await;
        rig.mock.set(|req| Reply { delay: Duration::from_millis(900), ..Reply::ok(answer(req, &ALLOW)) });
        rig.mock.delay_puts(&[Duration::from_millis(300), Duration::from_millis(300)]);
        let started = std::time::Instant::now();
        assert_pending(via, rig.submit("touch slow.txt").await);
        let took = started.elapsed();
        assert!(took < bound, "{via:?}: the decision took {took:?}, past one {DEADLINE} ms deadline");
        let cause = latest_decision(&rig).decision.miss_cause.unwrap_or_default();
        assert!(cause.contains("deadline passed during the decision"), "{via:?}: {cause}");
        rig.finish().await;
    }
}

/// A model seat on a tool rig: the performer has a model character sheet, a
/// scripted mock model is the default provider and records each request,
/// and `/scratch` is a host directory the commands leave markers in.
struct Seat {
    rig: Rig,
    sent: Arc<parking_lot::Mutex<Vec<Vec<crate::llm::Message>>>>,
    turns: crate::flows::Subscription<crate::flows::TurnFlow>,
    scratch: tempfile::TempDir,
}

fn shell_write_call(id: &str, command: &str, background: bool) -> Vec<crate::llm::stream::StreamEvent> {
    use crate::llm::stream::StreamEvent;
    let input = serde_json::json!({ "command": command, "run_in_background": background });
    vec![
        StreamEvent::ToolUse { id: id.into(), name: "shell_write".into(), input },
        StreamEvent::Done { stop_reason: Some("tool_use".into()), input_tokens: None, output_tokens: None, extra: None },
    ]
}

fn text_reply(text: &str) -> Vec<crate::llm::stream::StreamEvent> {
    use crate::llm::stream::StreamEvent;
    vec![
        StreamEvent::TextStart,
        StreamEvent::TextDelta(text.into()),
        StreamEvent::TextEnd,
        StreamEvent::Done { stop_reason: Some("end_turn".into()), input_tokens: None, output_tokens: None, extra: None },
    ]
}

/// The marker command a seat's model submits: one line per run.
const MARKER: &str = "echo report-ran >> /scratch/marker";

impl Seat {
    async fn new(council: &'static [[f64; 3]]) -> Self {
        Self::calling(council, false).await
    }

    /// A seat whose model makes the call in the background when `background`.
    async fn calling(council: &'static [[f64; 3]], background: bool) -> Self {
        let rig = rig(Via::Tool, Setup::default()).await;
        rig.mock.answers(council);
        let kernel = rig.d.kernel();
        kernel.kernel_db().lock().insert_character(&crate::kernel_db::CharacterRow {
            principal_id: rig.ctx.actor_id, name: "council-seat-performer".into(), created_at: 0,
            retired_at: None, handoff_ctx: None, root_ctx: None, root: false,
        }).unwrap();
        let scratch = tempfile::tempdir().unwrap();
        kernel.mount("/scratch", crate::vfs::LocalBackend::new(scratch.path())).await;
        let (mock, sent) = crate::llm::MockClient::new("")
            .with_scripted_stream(vec![shell_write_call("report-1", MARKER, background), text_reply("continued")])
            .recording_sent_messages();
        {
            let mut registry = kernel.llm().write().await;
            registry.register("mock", Arc::new(crate::llm::Provider::Mock(mock)));
            assert!(registry.set_default("mock"));
            registry.set_default_model("mock-model");
        }
        kernel.start_approval_delivery().unwrap();
        let turns = kernel.turn_flows().subscribe("turn.*");
        Seat { rig, sent, turns, scratch }
    }

    fn kernel(&self) -> &Arc<crate::Kernel> {
        self.rig.d.kernel()
    }

    /// Start one model turn on the seat, asked for by the live root `amy`.
    async fn start(&self, origin: crate::flows::TurnOrigin) {
        let kernel = self.kernel();
        let context = self.rig.ctx.context_id;
        let amy = crate::kj::test_helpers::test_reviewer_principal();
        let after = kernel.blocks().insert_block_as(context, None, None, kaijutsu_types::Role::User,
            kaijutsu_types::BlockKind::Text, "run it", kaijutsu_types::Status::Done,
            kaijutsu_types::ContentType::Plain, Some(amy)).unwrap();
        let tool_ctx = crate::ExecContext::new(amy, context, "/", SessionId::new(), kernel.id());
        let admission = kernel.admit_context(context).unwrap();
        let slot = kernel.reserve_runtime_slot().unwrap();
        crate::runtime::turn_request::queue_startup(kernel, crate::runtime::turn_request::StartupRequest {
            admission, lease: kernel.turns().begin(context),
            request: crate::runtime::turn_request::TurnRequest {
                context_id: context, after_block_id: after, content: String::new(),
                principal_id: amy, model: None, continuation_epoch: None, score: None,
            },
            origin, session: tool_ctx.session_id, tool_ctx: Some(tool_ctx), submit: None, joins_live_turn: false,
        }, None, slot).unwrap().await.unwrap().unwrap();
    }

    /// The seat's next turn end, completed or failed.
    async fn turn_end(&mut self) -> crate::flows::TurnFlow {
        let context = self.rig.ctx.context_id;
        tokio::time::timeout(Duration::from_secs(20), async {
            loop {
                let event = self.turns.recv().await.expect("the turn bus stays open").payload;
                if matches!(event, crate::flows::TurnFlow::Completed { .. } | crate::flows::TurnFlow::Failed { .. })
                    && event.context_id() == context
                {
                    return event;
                }
            }
        })
        .await
        .expect("the seat's turn ends")
    }

    async fn wait_for(&self, label: &str, check: impl Fn() -> bool) {
        tokio::time::timeout(Duration::from_secs(20), async {
            while !check() {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("timed out waiting for {label}"));
    }

    async fn wait_for_ask(&self) -> ApprovalRow {
        self.wait_for("the seat's ask", || !self.rig.asks().is_empty()).await;
        self.rig.only_ask()
    }

    fn approval(&self, request_id: &str) -> ApprovalRow {
        self.rig.d.kernel_db().lock().get_approval(request_id).unwrap().expect("the ask row")
    }

    fn marker(&self) -> String {
        std::fs::read_to_string(self.scratch.path().join("marker")).unwrap_or_default()
    }

    fn result_block(&self) -> kaijutsu_types::BlockSnapshot {
        self.kernel().blocks().block_snapshots(self.rig.ctx.context_id).unwrap().into_iter()
            .find(|b| b.kind == kaijutsu_types::BlockKind::ToolResult && b.tool_use_id.as_deref() == Some("report-1"))
            .expect("the call's result block")
    }

    /// Whether the approval worker settled the operation waiting on `ask`.
    fn settled(&self, ask: &str) -> bool {
        self.kernel().shell_operations().get_by_ask(ask, self.rig.ctx.context_id).unwrap()
            .is_some_and(|operation| operation.completed_at.is_some())
    }

    /// Answer the pending ask and tell the worker.
    fn answer(&self, allow: bool) {
        self.rig.answer_pending(allow);
        crate::kj::gate::announce_ledger_change(self.rig.d.kernel_db(), self.kernel().ledger_flows());
    }

    async fn finish(self) {
        self.rig.finish().await;
    }
}

/// A report on an autonomous seat stops its turn and keeps the ask: the
/// turn ends without another model call, the ask stays pending and the
/// call's result `Waiting`. An allow runs the stored command once through
/// the approval worker, which settles that result in place; the seat stays
/// stopped, and neither a second ledger scan nor a resubmission runs it
/// again.
///
/// Falsified by `soft_keeping_asks` not setting its flag (the turn abandons
/// the ask), and by `stop_if_autonomous` stopping nothing (the turn holds).
#[tokio::test]
async fn a_report_stops_an_autonomous_seat_and_an_allow_runs_the_kept_ask_once() {
    let mut seat = Seat::new(&REPORT).await;
    seat.start(crate::flows::TurnOrigin::Autonomous).await;
    let ask = seat.wait_for_ask().await;
    let ended = seat.turn_end().await;
    assert!(matches!(ended, crate::flows::TurnFlow::Completed {
        reason: crate::flows::TurnStopReason::Cancelled { immediate: false }, .. }), "{ended:?}");
    assert!(!seat.kernel().turn_in_flight(seat.rig.ctx.context_id));
    assert_eq!(seat.sent.lock().len(), 1, "the stopped turn asks the model nothing more");
    assert_eq!(the_decision(&seat.rig, &ask).decision.outcome, CouncilOutcome::Report);
    let ask = seat.rig.only_ask();
    assert_eq!(ask.status, ApprovalStatus::Pending, "the report keeps the ask redeemable");
    assert_eq!(seat.result_block().status, kaijutsu_types::Status::Waiting);
    assert!(seat.marker().is_empty(), "nothing runs before the answer");

    seat.answer(true);
    seat.wait_for("the worker's run", || seat.settled(&ask.request_id)).await;
    assert_eq!(seat.marker(), "report-ran\n", "the approval worker runs the stored command once");
    assert_eq!(seat.result_block().status, kaijutsu_types::Status::Done, "its result settles on the call");

    crate::kj::gate::announce_ledger_change(seat.rig.d.kernel_db(), seat.kernel().ledger_flows());
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert_eq!(seat.marker(), "report-ran\n", "a second scan does not run the spent answer");
    assert!(!seat.kernel().turn_in_flight(seat.rig.ctx.context_id), "the seat stays stopped");
    assert_eq!(seat.sent.lock().len(), 1, "nothing drove the seat");

    seat.rig.mock.answers(&ASK);
    assert_pending(Via::Tool, seat.rig.submit(MARKER).await);
    assert_eq!(seat.marker(), "report-ran\n", "a resubmission asks again and runs nothing");
    seat.finish().await;
}

/// A deny on a kept ask runs nothing; its result settles as the denial.
#[tokio::test]
async fn a_deny_leaves_a_kept_ask_unrun() {
    let mut seat = Seat::new(&REPORT).await;
    seat.start(crate::flows::TurnOrigin::Autonomous).await;
    let ask = seat.wait_for_ask().await;
    seat.turn_end().await;
    assert_eq!(seat.rig.only_ask().status, ApprovalStatus::Pending);
    seat.answer(false);
    seat.wait_for("the denial to settle the call", || seat.result_block().status == kaijutsu_types::Status::Error).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(seat.marker().is_empty(), "a denied kept ask runs nothing");
    assert_eq!(seat.approval(&ask.request_id).status, ApprovalStatus::Denied);
    assert_eq!(seat.sent.lock().len(), 1);
    seat.finish().await;
}

/// A root at the keyboard keeps the ordinary ask: the report stops
/// nothing, the turn holds on its ask, and an allow continues the same turn
/// with the command's real output.
///
/// Falsified by `stop_if_autonomous` treating every requester as
/// autonomous: the turn ends at the report.
#[tokio::test]
async fn a_report_on_a_root_s_interactive_turn_keeps_the_ordinary_ask() {
    let mut seat = Seat::new(&REPORT).await;
    seat.start(crate::flows::TurnOrigin::Interactive).await;
    let ask = seat.wait_for_ask().await;
    assert_eq!(the_decision(&seat.rig, &ask).decision.outcome, CouncilOutcome::Report);
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(seat.kernel().turn_in_flight(seat.rig.ctx.context_id), "the turn holds on its ask");
    assert_eq!(seat.sent.lock().len(), 1);
    seat.answer(true);
    let ended = seat.turn_end().await;
    assert!(matches!(ended, crate::flows::TurnFlow::Completed {
        reason: crate::flows::TurnStopReason::EndTurn, .. }), "{ended:?}");
    assert_eq!(seat.sent.lock().len(), 2, "the same turn continues");
    assert_eq!(seat.marker(), "report-ran\n");
    seat.finish().await;
}

/// `kj interrupt` still abandons the ask a turn holds: a late allow runs
/// nothing.
#[tokio::test]
async fn kj_interrupt_still_abandons_a_held_ask() {
    let mut seat = Seat::new(&ASK).await;
    seat.start(crate::flows::TurnOrigin::Autonomous).await;
    let ask = seat.wait_for_ask().await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(seat.kernel().turn_in_flight(seat.rig.ctx.context_id), "an ask does not stop the seat");
    let other = crate::kj::test_helpers::register_context(&seat.rig.d, Some("interrupter"), None, PrincipalId::new());
    let result = seat.rig.d
        .dispatch(&["interrupt".into(), seat.rig.ctx.context_id.to_string()], &crate::kj::test_helpers::caller_with_context(other))
        .await;
    assert!(result.is_ok(), "{}", result.message());
    seat.turn_end().await;
    assert_eq!(seat.approval(&ask.request_id).status, ApprovalStatus::Abandoned, "kj interrupt abandons the held ask");
    assert_eq!(seat.result_block().status, kaijutsu_types::Status::Error);
    crate::kj::gate::announce_ledger_change(seat.rig.d.kernel_db(), seat.kernel().ledger_flows());
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(seat.marker().is_empty());
    assert_eq!(seat.sent.lock().len(), 1);
    seat.finish().await;
}

/// A background call's kept ask: the stopped turn answers with the waiting
/// receipt, and an allow runs the command once.
///
/// Falsified by reading the kept background ask through
/// `background_answer`: the receipt claims the command is running.
#[tokio::test]
async fn a_report_keeps_a_background_call_s_ask_and_an_allow_runs_it_once() {
    let mut seat = Seat::calling(&REPORT, true).await;
    seat.start(crate::flows::TurnOrigin::Autonomous).await;
    let ask = seat.wait_for_ask().await;
    seat.turn_end().await;
    assert_eq!(seat.sent.lock().len(), 1);
    assert_eq!(seat.rig.only_ask().status, ApprovalStatus::Pending, "the report keeps the background call's ask");
    let block = seat.result_block();
    let read = crate::llm::hydrate::model_tool_result_text(&block, &block.content);
    assert!(read.contains("waiting for approval; not run yet") && read.contains(&ask.request_id),
        "the model reads the waiting receipt: {read}");
    assert!(seat.marker().is_empty());
    seat.answer(true);
    seat.wait_for("the worker's run", || seat.settled(&ask.request_id)).await;
    assert_eq!(seat.marker(), "report-ran\n");
    assert_eq!(seat.sent.lock().len(), 1, "the seat stays stopped");
    seat.finish().await;
}

/// Whether a decision request reads the program case.
fn is_program(request: &DecisionRequest) -> bool {
    serde_json::to_value(&request.state).unwrap().get("program").is_some()
}

/// The shell decision answers `shell`; each program decision answers with
/// the rubric `(originals, network)`: allowed rubric options lean hard.
fn shell_and_program(mock: &Mock, shell: &'static [[f64; 3]], originals: [f64; 3], network: [f64; 3]) {
    mock.set(move |req| {
        if is_program(req) {
            Reply::ok(super::gate::test_support::program_answer(req, originals, network, [-0.01, -5.0, -6.0]))
        } else {
            Reply::ok(answer(req, shell))
        }
    });
}

/// Rubric rows: the first option leans hard, or the last one does.
const FIRST: [f64; 3] = [-0.01, -6.0, -7.0];
const LAST: [f64; 3] = [-6.0, -7.0, -0.01];

/// The submission's decisions linked to `ask`, shell decision first.
fn shell_and_programs(rig: &Rig, ask: &ApprovalRow) -> (CouncilDecision, Vec<CouncilDecision>) {
    let mut decisions = rig.decisions_for(&ask.request_id);
    let shell = decisions.iter().position(|d| d.decision.spec_name == "shell-gate").expect("a shell decision");
    let shell = decisions.remove(shell);
    (shell, decisions)
}

/// A script the council reads and allows runs: the shell decision and the
/// program decision both allow, both are recorded with the ask, and the
/// program row names the file and the hash of the bytes judged. The program
/// decision reads the script's text, the command that runs it, and the
/// local import it did not show.
///
/// Falsified by consulting only the shell spec: one decision is recorded.
#[tokio::test]
async fn a_script_the_council_allows_runs_with_both_decisions_recorded() {
    for via in BOTH {
        let rig = rig(via, Setup { programs: true, ..Setup::default() }).await;
        let script = "import helper\nprint('fixed')\n";
        rig.write("/work/fix.py", script).await;
        rig.write("/work/helper.py", "X = 1\n").await;
        shell_and_program(&rig.mock, &ALLOW, FIRST, FIRST);
        rig.submit_gate("python3 /work/fix.py").await.unwrap_or_else(|e| panic!("{via:?}: both allow: {e:?}"));

        let ask = rig.only_ask();
        assert_eq!(ask.status, ApprovalStatus::Allowed, "{via:?}");
        let (shell, programs) = shell_and_programs(&rig, &ask);
        assert_eq!(shell.decision.outcome, CouncilOutcome::Allow);
        assert_eq!(programs.len(), 1, "{via:?}: one program decision");
        assert_eq!(programs[0].decision.spec_name, "program-gate");
        assert_eq!(programs[0].decision.outcome, CouncilOutcome::Allow);
        let rows = rig.programs(&shell);
        assert_eq!(rows.len(), 1, "{via:?}: {rows:#?}");
        assert_eq!(rows[0].path.as_deref(), Some("/work/fix.py"));
        assert_eq!(rows[0].sha256.as_deref(), Some(super::programs::sha256_of(script.as_bytes()).as_str()));
        assert_eq!(rows[0].program_decision_id.as_deref(), Some(programs[0].decision_id.as_slice()));
        assert_eq!(rows[0].imports_not_shown, vec!["helper".to_string()]);
        assert_eq!(rig.signals(&ask.request_id).len(), 2, "{via:?}: one signal per decision");

        let sent: Vec<_> = rig.mock.decisions().into_iter().filter(is_program).collect();
        assert_eq!(sent.len(), 1);
        let state = serde_json::to_value(&sent[0].state).unwrap();
        assert_eq!(state["program"], script, "{state}");
        assert_eq!(state["path"], "/work/fix.py");
        assert_eq!(state["language"], "python");
        assert_eq!(state["invocation"]["command"], "python3 /work/fix.py");
        assert_eq!(state["invocation"]["submission"], "python3 /work/fix.py");
        assert_eq!(state["imports_not_shown"], serde_json::json!(["helper"]));
        assert_eq!(state["context_type"], "default");
        rig.finish().await;
    }
}

/// A program the rubric says changes data with no backup holds the
/// submission even though the shell decision allows it. The ask names the
/// program decision that held it and the rubric's answers.
#[tokio::test]
async fn a_program_the_rubric_flags_holds_an_allowed_submission_and_says_why() {
    for via in BOTH {
        let rig = rig(via, Setup { programs: true, ..Setup::default() }).await;
        rig.write("/work/wipe.py", "import shutil\nshutil.rmtree('data')\n").await;
        shell_and_program(&rig.mock, &ALLOW, LAST, FIRST);
        assert_pending(via, rig.submit_gate("python3 /work/wipe.py").await);
        let ask = rig.only_ask();
        let (shell, programs) = shell_and_programs(&rig, &ask);
        assert_eq!(shell.decision.outcome, CouncilOutcome::Allow);
        assert_eq!(programs[0].decision.outcome, CouncilOutcome::Ask);
        assert!(
            ask.description.contains("council (program-gate) on /work/wipe.py answered ask")
                && ask.description.contains("originals=changes")
                && ask.description.contains("network=none")
                && ask.description.contains("held by the program decision on /work/wipe.py"),
            "{via:?}: {}",
            ask.description
        );
        rig.finish().await;
    }
}

/// A program whose text the kernel cannot read is never council-allowed:
/// there is no program decision, the shell decision alone does not run it,
/// and the record and the ask say why.
#[tokio::test]
async fn a_program_whose_text_cannot_be_read_is_never_council_allowed() {
    for via in BOTH {
        let rig = rig(via, Setup { programs: true, ..Setup::default() }).await;
        shell_and_program(&rig.mock, &ALLOW, FIRST, FIRST);
        assert_pending(via, rig.submit_gate("python3 /work/missing.py").await);
        let ask = rig.only_ask();
        let (shell, programs) = shell_and_programs(&rig, &ask);
        assert!(programs.is_empty(), "{via:?}: {programs:#?}");
        assert_eq!(shell.decision.outcome, CouncilOutcome::Allow);
        let rows = rig.programs(&shell);
        assert!(rows[0].unread_cause.as_deref().is_some_and(|c| c.contains("could not be read")), "{rows:#?}");
        assert!(
            ask.description.contains("was not judged") && ask.description.contains("/work/missing.py"),
            "{via:?}: {}",
            ask.description
        );
        rig.finish().await;
    }
}

/// With no program spec in `gate.toml`, a submission that runs program
/// text asks: the council cannot judge the program.
#[tokio::test]
async fn without_a_program_spec_a_program_is_not_judged_and_asks() {
    for via in BOTH {
        let rig = rig(via, Setup::default()).await;
        rig.mock.answers(&ALLOW);
        assert_pending(via, rig.submit_gate("python3 -c 'print(1)'").await);
        let ask = rig.only_ask();
        assert!(ask.description.contains("case = \"program\""), "{via:?}: {}", ask.description);
        assert_eq!(rig.mock.decisions().len(), 1, "{via:?}: only the shell decision");
        rig.finish().await;
    }
}

/// The judged script is the script that runs: a script changed while the
/// council decided is refused at execution and nothing runs. On the tool
/// path the call reports the refusal; on the RPC path the gate's verdict
/// carries the judged file and its hash to the execution seam.
///
/// Falsified by dropping the re-hash before execution: the tool path runs
/// the changed script.
#[tokio::test]
async fn a_script_changed_after_the_council_judged_it_does_not_run() {
    let rig = rig(Via::Tool, Setup { programs: true, ..Setup::default() }).await;
    rig.write("/work/fix.py", "print('judged')\n").await;
    shell_and_program(&rig.mock, &ALLOW, FIRST, FIRST);
    let (reached, release) = rig.mock.hold_next_decision();
    let params = KernelCallParams {
        instance: InstanceId::new(ShellServer::INSTANCE_WRITE),
        tool: ShellServer::TOOL_WRITE.to_string(),
        arguments: serde_json::json!({ "command": "python3 /work/fix.py" }),
    };
    let call = rig.broker.call_tool(params, &rig.ctx, CancellationToken::new());
    let change = async {
        reached.await.unwrap();
        rig.write("/work/fix.py", "print('swapped')\n").await;
        release.send(()).unwrap();
    };
    let (result, ()) = tokio::join!(call, change);
    let result = result.expect("the gate allowed the call");
    assert!(result.is_error, "{result:?}");
    let text = serde_json::to_string(&result).unwrap();
    assert!(text.contains("the script changed after the council judged it; send the command again"), "{text}");
    assert_eq!(rig.only_ask().status, ApprovalStatus::Allowed, "the council allowed what it read");
    rig.finish().await;

    let rig = self::rig(Via::Rpc, Setup { programs: true, ..Setup::default() }).await;
    rig.write("/work/fix.py", "print('judged')\n").await;
    shell_and_program(&rig.mock, &ALLOW, FIRST, FIRST);
    match rig.broker.shell_pre_call_hooks("python3 /work/fix.py", &rig.ctx, &CancellationToken::new()).await {
        ShellHookVerdict::Proceed(judged) => {
            assert_eq!(judged.len(), 1, "{judged:?}");
            assert_eq!(judged[0].path, "/work/fix.py");
            assert_eq!(judged[0].sha256, super::programs::sha256_of(b"print('judged')\n"));
        }
        other => panic!("the council allows: {other:?}"),
    }
    rig.finish().await;
}

/// A spec's own contexts are read by its decisions alone, after `[council]
/// contexts`: the program spec names `code`, so the program decision reads
/// it and the shell decision does not.
///
/// Falsified by a gate that reads only `[council] contexts`: the program
/// decision reads two contexts.
#[tokio::test]
async fn a_spec_reads_its_own_contexts_and_no_other_spec_does() {
    use crate::vfs::VfsOps;
    for via in BOTH {
        let rig = rig(via, Setup { programs: true, ..Setup::default() }).await;
        live_context(rig.d.kernel(), "code");
        let toml = gate_toml(&rig.mock.base, &Setup { programs: true, ..Setup::default() })
            .replace("case = \"program\"", "case = \"program\"\ncontexts = [\"code\"]");
        rig.d.kernel().vfs().write_all(std::path::Path::new("/config/kernel/gate.toml"), toml.as_bytes()).await.unwrap();
        rig.write("/work/fix.py", "print('fixed')\n").await;
        rig.mock.set(|req| {
            if is_program(req) {
                Reply::ok(super::gate::test_support::program_answer(req, FIRST, FIRST, [-0.01, -5.0, -6.0]))
            } else {
                allow_each(req)
            }
        });
        rig.submit_gate("python3 /work/fix.py").await.unwrap_or_else(|e| panic!("{via:?}: {e:?}"));
        let sent = rig.mock.decisions();
        assert_eq!(sent.len(), 2, "{via:?}: a shell and a program decision");
        let (voice, rules, code) = (rig.context_of("voice"), rig.context_of("system-rules"), rig.context_of("code"));
        for request in &sent {
            let want = if is_program(request) { vec![voice.clone(), rules.clone(), code.clone()] } else { vec![voice.clone(), rules.clone()] };
            assert_eq!(Rig::read_ids(request), want, "{via:?}: program={}", is_program(request));
        }
        rig.finish().await;
    }
}

/// With `[council] seat` on, the shell decision and each program decision
/// read the submitting seat's own context after the configured contexts,
/// under the seat's context id and pinned at the head the kernel prepared.
/// A seat with nothing written yet reads no seat context. Each decision
/// records the seat head it read, and its info log line says whether it
/// read one.
///
/// Falsified by a gate that leaves the seat out of the program decision:
/// its request reads two contexts.
#[tokio::test]
async fn the_seat_context_joins_the_shell_and_program_decisions() {
    use tracing_subscriber::layer::SubscriberExt;
    for via in BOTH {
        let seen = Arc::new(Seen::default());
        let _second = tracing::Dispatch::new(tracing_subscriber::registry());
        let _guard = tracing::subscriber::set_default(tracing_subscriber::registry().with(Capture(seen.clone())));
        tracing::callsite::rebuild_interest_cache();
        let rig = rig(via, Setup { seat: true, programs: true, ..Setup::default() }).await;
        rig.mock.set(allow_each);
        rig.submit("echo before-any-narration").await.unwrap_or_else(|e| panic!("{via:?}: {e:?}"));
        let first = rig.mock.decisions();
        assert_eq!(Rig::read_ids(&first[0]), [rig.context_of("voice"), rig.context_of("system-rules")], "{via:?}");

        append_dialogue(rig.d.kernel(), rig.ctx.context_id, &["recover the records", "The WAL is XORed; no backup yet."]);
        rig.write("/work/fix.py", "print('fixed')\n").await;
        rig.mock.set(|req| {
            if is_program(req) {
                Reply::ok(super::gate::test_support::program_answer(req, FIRST, FIRST, [-0.01, -5.0, -6.0]))
            } else {
                allow_each(req)
            }
        });
        rig.submit_gate("python3 /work/fix.py").await.unwrap_or_else(|e| panic!("{via:?}: {e:?}"));
        let sent: Vec<_> = rig.mock.decisions().into_iter().skip(first.len()).collect();
        assert_eq!(sent.len(), 2, "{via:?}: a shell and a program decision");
        let seat = rig.ctx.context_id.to_string();
        for request in &sent {
            assert_eq!(
                Rig::read_ids(request),
                [rig.context_of("voice"), rig.context_of("system-rules"), seat.clone()],
                "{via:?}: program={}",
                is_program(request)
            );
            let at = request.contexts.as_ref().unwrap()[2].at.as_ref().expect("the seat is pinned");
            assert_eq!(at, sent[0].contexts.as_ref().unwrap()[2].at.as_ref().unwrap(), "{via:?}: one head for both");
        }
        let head = sent[0].contexts.as_ref().unwrap()[2].at.as_ref().unwrap().to_string();
        let decisions = rig.decisions();
        let heads: Vec<Option<&str>> = decisions.iter().map(|d| d.decision.seat_head.as_deref()).collect();
        assert_eq!(heads.len(), 3, "{via:?}: {heads:?}");
        assert_eq!(heads.iter().filter(|h| **h == Some(head.as_str())).count(), 2, "{via:?}: {heads:?}");
        assert_eq!(heads.iter().filter(|h| h.is_none()).count(), 1, "{via:?}: the first read no seat: {heads:?}");

        let events = seen.events.lock().unwrap();
        let logged: Vec<&str> = events
            .iter()
            .filter(|(_, f)| field(f, "outcome").is_some() && field(f, "spec").is_some())
            .filter_map(|(_, f)| field(f, "seat_head"))
            .collect();
        assert_eq!(logged.iter().filter(|h| **h == head).count(), 2, "{via:?}: {logged:?}");
        assert_eq!(logged.iter().filter(|h| **h == "none").count(), 1, "{via:?}: {logged:?}");
        drop(events);
        rig.finish().await;
    }
}

/// An inline program the program decision judged reaches the shell
/// decision elided: the shell decision judges the invocation, and the
/// program decision judges the text. A shell decision that would ask about
/// the program's own text no longer refuses a program the rubric allows.
///
/// Falsified by a shell case that carries the program text: the shell
/// decision asks and the submission does not run.
#[tokio::test]
async fn an_inline_program_the_program_decision_allows_is_not_judged_again_as_shell() {
    const PROGRAM: &str = "import shutil\nshutil.copy(\"a.db\", \"/tmp/a.db\")\nprint(\"copied to scratch\")";
    for via in BOTH {
        let rig = rig(via, Setup { programs: true, ..Setup::default() }).await;
        rig.mock.set(|req| {
            let state = serde_json::to_string(&req.state).unwrap();
            if is_program(req) {
                Reply::ok(super::gate::test_support::program_answer(req, FIRST, FIRST, [-0.01, -5.0, -6.0]))
            } else if state.contains("shutil.copy") {
                Reply::ok(answer(req, &ASK))
            } else {
                Reply::ok(answer(req, &ALLOW))
            }
        });
        let submission = format!("echo start; python3 -c '{PROGRAM}'");
        rig.submit_gate(&submission).await.unwrap_or_else(|e| panic!("{via:?}: the program decision allows it: {e:?}"));

        let sent = rig.mock.decisions();
        let shell = sent.iter().find(|r| !is_program(r)).unwrap_or_else(|| panic!("{via:?}: a shell decision among {} with asks {:#?}", sent.len(), rig.asks()));
        let state = serde_json::to_value(&shell.state).unwrap();
        let command = state["command"].as_str().unwrap();
        assert_eq!(command, "echo start; python3 -c '<program judged separately>'", "{via:?}: {state}");
        assert!(!serde_json::to_string(&state).unwrap().contains("shutil"), "{via:?}: {state}");
        let program = sent.iter().find(|r| is_program(r)).expect("a program decision");
        let program_state = serde_json::to_value(&program.state).unwrap();
        assert_eq!(program_state["program"], PROGRAM, "{via:?}: the program decision reads the text");
        rig.finish().await;
    }
}

/// The shell decision's view of the submission's programs: the `programs`
/// field of its case, empty when the case has none.
fn programs_field(request: &DecisionRequest) -> Vec<serde_json::Value> {
    let state = serde_json::to_value(&request.state).unwrap();
    state.get("programs").and_then(|p| p.as_array().cloned()).unwrap_or_default()
}

/// A shell decision that follows the programs it is shown: it allows when
/// every listed program allowed, and asks otherwise.
fn shell_follows_programs(mock: &Mock, originals: [f64; 3]) {
    mock.set(move |req| {
        if is_program(req) {
            Reply::ok(super::gate::test_support::program_answer(req, originals, FIRST, [-0.01, -5.0, -6.0]))
        } else {
            let programs = programs_field(req);
            let all_allowed = !programs.is_empty() && programs.iter().all(|p| p["outcome"] == "allow");
            Reply::ok(answer(req, if all_allowed { &ALLOW } else { &ASK }))
        }
    });
}

/// The program decisions run first, and the shell decision reads their
/// outcomes in its case's `programs` field: each judged program's
/// statement, language, path, outcome, and rubric answers. An allowed
/// program with routine statements around it runs end to end.
///
/// Falsified by running the shell decision beside the program decisions:
/// its case has no `programs` field and it asks.
#[tokio::test]
async fn the_shell_decision_reads_the_program_outcomes_and_an_allowed_program_runs() {
    for via in BOTH {
        let rig = rig(via, Setup { programs: true, ..Setup::default() }).await;
        rig.write("/work/a.db", "data").await;
        shell_follows_programs(&rig.mock, FIRST);
        let submission = "cp /work/a.db /work/a.db.bak; python3 -c 'print(open(\"/work/a.db.bak\").read())'";
        rig.submit_gate(submission).await.unwrap_or_else(|e| panic!("{via:?}: the program and the shell allow: {e:?}"));

        let sent = rig.mock.decisions();
        assert_eq!(sent.len(), 2, "{via:?}");
        assert!(is_program(&sent[0]) && !is_program(&sent[1]), "{via:?}: the program decision runs first");
        let programs = programs_field(&sent[1]);
        assert_eq!(
            programs,
            [serde_json::json!({"statement": 1, "language": "python", "path": null, "outcome": "allow",
                                "originals": "reads", "network": "none"})],
            "{via:?}"
        );
        let state = serde_json::to_string(&sent[1].state).unwrap();
        assert!(state.contains("<program judged separately>"), "{via:?}: the elision stays: {state}");
        let ask = rig.only_ask();
        assert_eq!(ask.status, ApprovalStatus::Allowed, "{via:?}");
        rig.finish().await;
    }
}

/// A program the rubric asks about reaches the shell decision as an ask,
/// the shell decision asks too, and the ask names the program.
#[tokio::test]
async fn an_asked_program_yields_a_shell_ask_that_names_it() {
    for via in BOTH {
        let rig = rig(via, Setup { programs: true, ..Setup::default() }).await;
        rig.write("/work/wipe.py", "import os\nos.remove('/work/a.db')\n").await;
        shell_follows_programs(&rig.mock, LAST);
        assert_pending(via, rig.submit_gate("echo start; python3 /work/wipe.py").await);
        let sent = rig.mock.decisions();
        let shell = sent.iter().find(|r| !is_program(r)).expect("a shell decision");
        let programs = programs_field(shell);
        assert_eq!(programs.len(), 1, "{via:?}: {programs:?}");
        assert_eq!(programs[0]["outcome"], "ask");
        assert_eq!(programs[0]["path"], "/work/wipe.py");
        assert_eq!(programs[0]["originals"], "changes");
        let ask = rig.only_ask();
        let (decision, _) = shell_and_programs(&rig, &ask);
        assert_eq!(decision.decision.outcome, CouncilOutcome::Ask, "{via:?}");
        assert!(ask.description.contains("/work/wipe.py"), "{via:?}: {}", ask.description);
        rig.finish().await;
    }
}

/// A program the kernel cannot read is listed as unread with its cause.
#[tokio::test]
async fn an_unread_program_is_listed_with_its_cause() {
    for via in BOTH {
        let rig = rig(via, Setup { programs: true, ..Setup::default() }).await;
        shell_follows_programs(&rig.mock, FIRST);
        assert_pending(via, rig.submit_gate("python3 /work/missing.py").await);
        let sent = rig.mock.decisions();
        assert_eq!(sent.len(), 1, "{via:?}: only the shell decision");
        let programs = programs_field(&sent[0]);
        assert_eq!(programs[0]["outcome"], "unread", "{via:?}: {programs:?}");
        assert!(programs[0]["cause"].as_str().unwrap().contains("could not be read"), "{programs:?}");
        rig.finish().await;
    }
}

// ---- bumper mode ----

const BUMP_OPTIONS: [&str; 3] = ["proceed", "try_harder", "do_less"];
const PROCEED: [f64; 3] = [-0.001, -8.0, -9.0];
const TRY_HARDER: [f64; 3] = [-4.0, -0.03, -5.0];
const DO_LESS: [f64; 3] = [-6.0, -3.0, -0.04];

/// Every read of a bumper shell decision puts these log probabilities on
/// proceed, try_harder, do_less.
fn bump_answer(request: &DecisionRequest, logprobs: [f64; 3]) -> serde_json::Value {
    let n = request.contexts.as_ref().map(Vec::len).unwrap_or(0);
    super::gate::test_support::answer_choices(request, &[("verdict", BUMP_OPTIONS, &vec![logprobs; n])])
}

impl Mock {
    fn bumper_says(&self, logprobs: [f64; 3]) {
        self.set(move |req| Reply::ok(bump_answer(req, logprobs)));
    }
}

fn bumper(limit: u64) -> Setup {
    Setup { bump_limit: Some(limit), ..Setup::default() }
}

fn refusal_text(via: Via, result: Result<(), McpError>) -> String {
    match result {
        Err(error) => {
            assert!(!error.is_refusal(RefusalKind::Pending), "{via:?}: a bump opens no ask: {error:?}");
            error.to_string()
        }
        Ok(()) => panic!("{via:?}: the submission must be bumped, and it ran"),
    }
}

fn bump_flavors(rig: &Rig, digest: &str) -> Vec<String> {
    let db = rig.d.kernel_db();
    let db = db.lock();
    approval_ledger::council::list_bump_flavors(db.conn_for_ledger(), &rig.context_bytes(), digest).unwrap()
}

fn digest_of(command: &str) -> String {
    use sha2::{Digest, Sha256};
    format!("sha256:{}", Sha256::digest(command.as_bytes()).iter().map(|b| format!("{b:02x}")).collect::<String>())
}

/// A bumper council that proceeds runs the submission like an allow does.
#[tokio::test]
async fn a_bumper_proceed_runs_the_submission() {
    for via in BOTH {
        let rig = rig(via, bumper(3)).await;
        rig.mock.bumper_says(PROCEED);
        rig.submit("echo council-ran").await.unwrap_or_else(|e| panic!("{via:?}: the council proceeds: {e:?}"));
        let ask = rig.only_ask();
        assert_eq!(ask.status, ApprovalStatus::Allowed, "{via:?}");
        assert_eq!(the_decision(&rig, &ask).decision.outcome, CouncilOutcome::Allow);
        assert!(rig.asks().iter().all(|a| a.status != ApprovalStatus::Pending));
        rig.finish().await;
    }
}

/// A bump refuses with the rendered template and the flavor's guidance,
/// runs nothing, opens no ask, and leaves a bump decision.
///
/// Falsified by treating a non-proceed answer as an ask: the submission is
/// pending.
#[tokio::test]
async fn a_bump_refuses_with_guidance_and_opens_no_ask() {
    for via in BOTH {
        let rig = rig(via, bumper(3)).await;
        rig.mock.bumper_says(TRY_HARDER);
        let text = refusal_text(via, rig.submit("touch /work/marker").await);
        assert!(text.contains("Bumped by the council (attempt 1 of 3)"), "{via:?}: {text}");
        assert!(text.contains("the goal is fine, but it needs more care first"), "{via:?}: {text}");
        assert!(text.contains("Try harder, a different approach"), "{via:?}: {text}");
        assert!(rig.asks().is_empty(), "{via:?}: no ask was opened");
        let decisions = rig.decisions();
        assert_eq!(decisions.len(), 1, "{via:?}");
        assert_eq!(decisions[0].decision.outcome, CouncilOutcome::Bump);
        assert_eq!(decisions[0].decision.bump_flavor.as_deref(), Some("try_harder"));
        assert!(decisions[0].decision.request_id.is_none());
        assert_eq!(bump_flavors(&rig, &digest_of("touch /work/marker")), ["try_harder"]);
        rig.finish().await;
    }
}

/// The same submission sent again after a bump is judged again, and runs
/// when the council now proceeds.
///
/// Falsified by closing the request with a durable denial: the second send
/// replays it and the mock is consulted once.
#[tokio::test]
async fn the_same_submission_sent_again_after_a_bump_is_judged_again() {
    for via in BOTH {
        let rig = rig(via, bumper(3)).await;
        rig.mock.bumper_says(TRY_HARDER);
        refusal_text(via, rig.submit("echo tables").await);
        assert_eq!(rig.mock.decisions().len(), 1);
        rig.mock.bumper_says(PROCEED);
        rig.submit("echo tables").await.unwrap_or_else(|e| panic!("{via:?}: now it proceeds: {e:?}"));
        assert_eq!(rig.mock.decisions().len(), 2, "{via:?}: the council judged it again");
        rig.finish().await;
    }
}

/// After `bump_limit` bumps the next would-be bump is an ordinary ask that
/// carries the bumps' flavors.
#[tokio::test]
async fn at_the_bump_limit_the_submission_asks_with_its_history() {
    for via in BOTH {
        let rig = rig(via, bumper(2)).await;
        rig.mock.bumper_says(TRY_HARDER);
        refusal_text(via, rig.submit("touch /work/a").await);
        rig.mock.bumper_says(DO_LESS);
        let second = refusal_text(via, rig.submit("touch /work/a").await);
        assert!(second.contains("attempt 2 of 2") && second.contains("reaches past what the task needs"), "{second}");
        assert_pending(via, rig.submit("touch /work/a").await);
        assert_eq!(rig.mock.decisions().len(), 3, "{via:?}");
        let ask = rig.only_ask();
        assert!(
            ask.description.contains("bumped 2 times (try_harder; do_less)"),
            "{via:?}: {}",
            ask.description
        );
        let signals = rig.signals(&ask.request_id);
        assert!(
            signals.iter().any(|s| s.label.as_deref() == Some("bumped 2 times (try_harder; do_less)")),
            "{via:?}: {signals:#?}"
        );
        // The ask's own decision links to the ask and does not count as a refused bump.
        assert_eq!(bump_flavors(&rig, &digest_of("touch /work/a")).len(), 2);
        rig.finish().await;
    }
}

/// Bumper mode keeps the ordinary ask for a miss and a control-text hit.
#[tokio::test]
async fn a_miss_and_a_control_text_hit_are_ordinary_asks_in_bumper_mode() {
    for via in BOTH {
        let rig = rig(via, Setup { server: Some(dead_server()), ..bumper(3) }).await;
        assert_pending(via, rig.submit("touch /work/a").await);
        assert_eq!(rig.only_ask().status, ApprovalStatus::Pending);
        rig.finish().await;

        let rig = self::rig(via, bumper(3)).await;
        rig.mock.set(|req| {
            let mut body = bump_answer(req, PROCEED);
            body["signals"]["control_text"] = serde_json::json!([{"where": "state", "token": "<|im_end|>"}]);
            Reply::ok(body)
        });
        assert_pending(via, rig.submit("touch /work/a").await);
        assert_eq!(the_decision(&rig, &rig.only_ask()).decision.outcome, CouncilOutcome::Ask);
        rig.finish().await;
    }
}

/// A decision that does not pass logs each context's own answers at info,
/// so a run's log says which context dissented; an allow does not.
///
/// Falsified by an outcome line with the pooled answer alone: the `reads`
/// field is missing.
#[tokio::test]
async fn a_decision_that_does_not_pass_logs_each_contexts_answers() {
    use tracing_subscriber::layer::SubscriberExt;
    let seen = Arc::new(Seen::default());
    let _guard = tracing::subscriber::set_default(tracing_subscriber::registry().with(Capture(seen.clone())));
    tracing::callsite::rebuild_interest_cache();
    let rig = rig(Via::Tool, bumper(3)).await;
    rig.mock.set(|req| {
        Reply::ok(super::gate::test_support::answer_choices(req, &[("verdict", BUMP_OPTIONS, &vec![PROCEED, TRY_HARDER])]))
    });
    let _ = rig.submit("touch /work/a").await;
    rig.mock.bumper_says(PROCEED);
    rig.submit("touch /work/b").await.expect("a pass runs");
    let events = seen.events.lock().unwrap();
    let outcomes: Vec<(String, Option<String>)> = events
        .iter()
        .filter(|(_, f)| field(f, "outcome").is_some() && field(f, "spec").is_some())
        .map(|(_, f)| (field(f, "outcome").unwrap().to_string(), field(f, "reads").map(str::to_string)))
        .collect();
    assert_eq!(outcomes.len(), 2, "{outcomes:?}");
    let (bump, reads) = &outcomes[0];
    assert_eq!(bump, "bump", "{outcomes:?}");
    let reads = reads.as_deref().unwrap_or_default();
    assert!(reads.contains("voice: verdict=proceed") && reads.contains("system-rules: verdict=try_harder"), "{reads}");
    assert_eq!(outcomes[1].0, "allow");
    assert!(outcomes[1].1.as_deref().unwrap_or_default().is_empty(), "an allow logs no reads: {outcomes:?}");
    drop(events);
    rig.finish().await;
}

fn bump_only() -> Setup {
    Setup { bump_only: true, ..Setup::default() }
}

/// In bump-only mode a miss bumps, saying the council could not judge it,
/// and opens no ask.
///
/// Falsified by bumper mode's fallback: the miss opens a pending ask.
#[tokio::test]
async fn a_miss_bumps_in_bump_only_mode() {
    for via in BOTH {
        let rig = rig(via, Setup { server: Some(dead_server()), ..bump_only() }).await;
        let text = refusal_text(via, rig.submit("touch /work/a").await);
        assert!(text.contains("the council could not judge it: try a smaller, plainer step."), "{via:?}: {text}");
        assert!(text.contains("attempt 1 of ∞"), "{via:?}: {text}");
        // The miss's cause names the gate's own machinery; a DeepSeek seat
        // that read "refit one in gate.toml" in a bump rewrote its gate.
        assert!(!text.contains("server") && !text.contains("gate.toml"), "{via:?}: the cause stays internal: {text}");
        assert!(rig.asks().is_empty(), "{via:?}: no ask was opened");
        rig.finish().await;
    }
}

/// In bump-only mode a control-text hit bumps, asking for plain text.
#[tokio::test]
async fn a_control_text_hit_bumps_in_bump_only_mode() {
    for via in BOTH {
        let rig = rig(via, bump_only()).await;
        rig.mock.set(|req| {
            let mut body = bump_answer(req, PROCEED);
            body["signals"]["control_text"] = serde_json::json!([{"where": "state", "token": "<|im_end|>"}]);
            Reply::ok(body)
        });
        let text = refusal_text(via, rig.submit("touch /work/a").await);
        assert!(text.contains("control token"), "{via:?}: {text}");
        assert!(rig.asks().is_empty(), "{via:?}: no ask was opened");
        rig.finish().await;
    }
}

/// In bump-only mode there is no limit: the fifth bump of one submission is
/// still a bump, counted against infinity, and the message says to try
/// something else.
///
/// Falsified by a limit: the fourth send opens an ask.
#[tokio::test]
async fn bump_only_mode_has_no_limit() {
    for via in BOTH {
        let rig = rig(via, bump_only()).await;
        rig.mock.bumper_says(TRY_HARDER);
        let mut last = String::new();
        for _ in 0..5 {
            last = refusal_text(via, rig.submit("echo again").await);
        }
        assert!(last.contains("attempt 5 of ∞"), "{via:?}: {last}");
        assert!(last.contains("try something else"), "{via:?}: {last}");
        assert!(rig.asks().is_empty(), "{via:?}: no ask was opened");
        rig.finish().await;
    }
}

/// A bumper spec with no `proceed` option is a miss naming it: the
/// submission asks.
#[tokio::test]
async fn a_bumper_spec_without_proceed_is_refused_as_a_miss() {
    for via in BOTH {
        let rig = rig(via, bumper(3)).await;
        rig.write("/config/kernel/council/shell-bump.json", &spec_text()).await;
        assert_pending(via, rig.submit("touch /work/a").await);
        let decision = the_decision(&rig, &rig.only_ask());
        assert_eq!(decision.decision.outcome, CouncilOutcome::Miss);
        let cause = decision.decision.miss_cause.unwrap();
        assert!(cause.contains("no `proceed` option"), "{via:?}: {cause}");
        rig.finish().await;
    }
}

/// A program the rubric flags bumps a submission whose shell decision
/// proceeds, with the program's guidance, and counts as one bump.
#[tokio::test]
async fn a_flagged_program_bumps_with_its_rubric_guidance() {
    for via in BOTH {
        let rig = rig(via, Setup { programs: true, ..bumper(3) }).await;
        rig.write("/work/wipe.py", "import shutil\nshutil.rmtree('data')\n").await;
        rig.mock.set(|req| {
            if is_program(req) {
                Reply::ok(super::gate::test_support::program_answer(req, LAST, FIRST, [-0.01, -5.0, -6.0]))
            } else {
                Reply::ok(bump_answer(req, PROCEED))
            }
        });
        let text = refusal_text(via, rig.submit_gate("python3 /work/wipe.py").await);
        assert!(text.contains("back it up or work on a copy first"), "{via:?}: {text}");
        assert!(rig.asks().is_empty());
        assert_eq!(bump_flavors(&rig, &digest_of("python3 /work/wipe.py")), ["originals=changes"]);
        rig.finish().await;
    }
}

/// The template is read at each bump, so a change shows on the next one; a
/// missing template is an error in the message, with the built-in text.
#[tokio::test]
async fn the_bump_template_is_read_at_each_bump() {
    for via in BOTH {
        let rig = rig(via, bumper(5)).await;
        rig.mock.bumper_says(TRY_HARDER);
        refusal_text(via, rig.submit("touch /work/a").await);
        rig.write("/config/kernel/council/bump.md", "HOLD {flavor} {attempt}/{limit}: {guidance}").await;
        let text = refusal_text(via, rig.submit("touch /work/a").await);
        assert!(text.contains("HOLD try_harder 2/5: the goal is fine"), "{via:?}: {text}");
        rig.finish().await;

        let rig = self::rig(via, Setup { no_bump_template: true, ..bumper(3) }).await;
        rig.mock.bumper_says(TRY_HARDER);
        let text = refusal_text(via, rig.submit("touch /work/a").await);
        assert!(text.contains("Bumped by the council (attempt 1 of 3)"), "{via:?}: {text}");
        assert!(text.contains("could not be read"), "{via:?}: {text}");
        rig.finish().await;
    }
}

/// With no `mode`, the gate is a gatekeeper: the shell-gate spec, an ask
/// for anything but allow.
#[tokio::test]
async fn without_a_mode_the_gate_is_a_gatekeeper() {
    for via in BOTH {
        let rig = rig(via, Setup::default()).await;
        rig.mock.answers(&ASK);
        assert_pending(via, rig.submit("touch /work/a").await);
        assert_eq!(the_decision(&rig, &rig.only_ask()).decision.spec_name, "shell-gate");
        rig.finish().await;
    }
}
