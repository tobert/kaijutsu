//! The typed ledger over the product SSH/RPC wire: `listAsks`, `getAsk`,
//! `decideAsk`, `escalateAsk`, and the `subscribeLedgerEvents` push of
//! changed asks. None of them takes a context: the ledger is kernel-wide,
//! and an answer is recorded in the ledger, not in a transcript.

mod common;

use std::sync::Arc;
use std::time::Duration;

use common::run_local;
use kaijutsu_client::{KernelHandle, KeySource, LedgerPush, RpcClient, SshClient, SshConfig, ledger_events_channel};
use kaijutsu_kernel::kernel_db::CharacterRow;
use kaijutsu_kernel::mcp::{AskSpec, GlobPattern, HookAction, HookEntry, HookId};
use kaijutsu_server::{AuthDb, SharedKernel, SshServer, SshServerConfig};
use kaijutsu_types::{
    AskAnswerFailureKind, AskFilter, AskOrigin, AskStatus, AskVerdict, AskView, ContextId, PrincipalId,
};
use russh::keys::{Algorithm, PrivateKey};
use tokio::sync::broadcast::Receiver;

async fn connect(addr: std::net::SocketAddr, key: PrivateKey, name: &str) -> RpcClient {
    let mut ssh = SshClient::new(SshConfig {
        host: addr.ip().to_string(), port: addr.port(), username: name.to_string(),
        key_source: KeySource::InMemory(Arc::new(key)), insecure: true,
    });
    let channel = ssh.connect().await.expect("SSH connect");
    let mut client = RpcClient::new(channel.into_stream()).await.expect("RPC client");
    client.retain_ssh_session(ssh);
    client
}

fn add_character(kernel: &SharedKernel, id: PrincipalId, name: &str) {
    kernel.kernel_db.lock().insert_character(&CharacterRow {
        principal_id: id, name: name.to_string(), created_at: 0, retired_at: None, handoff_ctx: None, root_ctx: None, root: false,
    }).expect("insert character");
}

/// Amy (root), a coder who performs, and a lead who can be made reviewer.
struct Rig {
    kernel: SharedKernel,
    db_path: std::path::PathBuf,
    amy: PrincipalId,
    coder: PrincipalId,
    lead: PrincipalId,
    amy_kj: KernelHandle,
    coder_kj: KernelHandle,
    lead_kj: KernelHandle,
    work: ContextId,
    _clients: Vec<RpcClient>,
}

impl Rig {
    async fn new() -> Rig {
        let temp = tempfile::tempdir().unwrap();
        let auth_path = temp.path().join("auth.db");
        std::mem::forget(temp);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let mut config = SshServerConfig::ephemeral_with_root(addr.port(), "amy");
        common::allow_in_shipped_gate(&config, &["kj context create", "kj drift push"]);
        let db_path = config.data_dir.as_ref().unwrap().join("kernel.db");
        let amy = kaijutsu_kernel::KernelDb::open(&db_path)
            .unwrap().get_character_by_name("amy").unwrap().expect("amy is root").principal_id;
        let coder = PrincipalId::new();
        let lead = PrincipalId::new();
        let keys: Vec<PrivateKey> = (0..3)
            .map(|_| PrivateKey::random(&mut rand_v10::rng(), Algorithm::Ed25519).unwrap()).collect();
        let auth = AuthDb::open(&auth_path).unwrap();
        auth.add_key(amy, keys[0].public_key(), Some("amy")).unwrap();
        auth.add_key(coder, keys[1].public_key(), Some("coder")).unwrap();
        auth.add_key(lead, keys[2].public_key(), Some("lead")).unwrap();
        drop(auth);
        config.auth_db_path = Some(auth_path);
        let (tx, rx) = tokio::sync::oneshot::channel();
        tokio::task::spawn_local(async move {
            SshServer::new(config).run_on_listener_with_kernel_sink(listener, tx).await.unwrap();
        });
        let kernel = rx.await.unwrap();
        add_character(&kernel, coder, "coder");
        add_character(&kernel, lead, "lead");
        let amy_client = connect(addr, keys[0].clone(), "amy").await;
        let coder_client = connect(addr, keys[1].clone(), "coder").await;
        let lead_client = connect(addr, keys[2].clone(), "lead").await;
        let (amy_kj, _) = amy_client.bind_kernel().await.unwrap();
        let (coder_kj, _) = coder_client.bind_kernel().await.unwrap();
        let (lead_kj, _) = lead_client.bind_kernel().await.unwrap();
        let parent = kaijutsu_client::choose_parent(None, &amy_kj.list_contexts().await.unwrap()).unwrap();
        let created = amy_kj
            .execute_kj_quiet(parent.context_id, &kaijutsu_client::context_create_argv("ledger-rpc-work", "default", "coder", None))
            .await
            .unwrap();
        let work = kaijutsu_client::context_id_from_create_result(&created).unwrap();
        kernel.kernel.broker().hooks().write().await.pre_call.entries.push(HookEntry {
            id: HookId("ledger-rpc-gate".into()), match_instance: None,
            match_tool: Some(GlobPattern("shell_write".into())), match_context: Some(work),
            match_principal: None, action: HookAction::Ask(AskSpec { description: Some("ledger rpc".into()) }),
            priority: 0, kaish_script_id: None,
        });
        coder_kj.join_context(work, "ledger-rpc-work").await.unwrap();
        Rig {
            kernel, db_path, amy, coder, lead, amy_kj, coder_kj, lead_kj, work,
            _clients: vec![amy_client, coder_client, lead_client],
        }
    }

    /// The coder runs `command` in the work context and the gate asks.
    async fn raise(&self, command: &str) -> String {
        let submission = self.coder_kj.shell_submit(command, self.work, true).await.unwrap();
        submission.refusal.expect("the gate asks").ask.expect("the refusal names its ask").request_id
    }

    fn blocks_mentioning(&self, needle: &str) -> usize {
        self.kernel.kernel.blocks().block_snapshots(self.work).unwrap()
            .iter().filter(|block| block.content.contains(needle)).count()
    }
}

/// Collect pushes until one carries `request_id` in `status`.
async fn await_push(rx: &mut Receiver<LedgerPush>, request_id: &str, status: AskStatus) -> LedgerPush {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let push = match tokio::time::timeout_at(deadline, rx.recv()).await {
            Ok(Ok(push)) => push,
            Ok(Err(e)) => panic!("ledger push channel error: {e}"),
            Err(_) => panic!("no push carried {request_id} as {status}"),
        };
        if push.asks.iter().any(|ask| ask.request_id == request_id && ask.status == status) {
            return push;
        }
    }
}

#[test]
fn a_reviewer_reads_and_answers_the_ledger_without_a_context() {
    run_local(async {
        let rig = Rig::new().await;
        let request_id = rig.raise("echo typed-ledger").await;

        let listing = rig.amy_kj.list_asks(&AskFilter::default()).await.unwrap();
        assert_eq!(listing.asks.len(), 1, "{listing:?}");
        let card = &listing.asks[0];
        assert_eq!(card.request_id, request_id);
        assert_eq!(card.status, AskStatus::Pending);
        assert_eq!(card.origin, AskOrigin::Hook);
        assert_eq!(card.context_id, Some(rig.work));
        assert_eq!(card.statements.len(), 1, "{:?}", card.statements);
        assert!(card.statements[0].contains("echo typed-ledger"), "{:?}", card.statements);
        assert_eq!(card.performer.as_ref().map(|p| (p.id, p.name.as_str())), Some((rig.coder, "coder")));
        assert_eq!(card.reviewer.as_ref().map(|p| p.id), Some(rig.amy));
        assert!(card.answerable_by(rig.amy) && !card.answerable_by(rig.coder));
        assert_eq!(card.lineage_root.as_ref().map(|p| (p.id, p.name.as_str())), Some((rig.amy, "amy")));
        assert_eq!(card.tool_call_block_id, None, "a hook ask raised outside a model tool call names none");
        assert!(card.has_exec_source, "the rig's hook ask carries the program the shell tool submitted");

        let detail = rig.amy_kj.get_ask(&request_id).await.unwrap().expect("the ask exists");
        assert_eq!(&detail.summary, card);
        assert_eq!(detail.exec_source.is_some(), card.has_exec_source, "the card and the record agree");
        assert_eq!(detail.tool.as_deref(), Some("shell_write"));
        assert!(detail.decision.is_none());
        assert!(rig.amy_kj.get_ask("no-such-ask").await.unwrap().is_none());

        // A model's tool call that raised the ask rides on its card, in a
        // listing and in a get. The gate records it with the ask; here the
        // row is written directly, since the wire cannot raise a model call.
        let call = kaijutsu_types::BlockId::new(rig.work, rig.coder, 5);
        rusqlite::Connection::open(&rig.db_path).unwrap().execute(
            "INSERT INTO approval_tool_calls(request_id, block_id) VALUES (?1, ?2)",
            rusqlite::params![request_id, call.to_key()],
        ).unwrap();
        let listed = rig.amy_kj.list_asks(&AskFilter::default()).await.unwrap();
        assert_eq!(listed.asks[0].tool_call_block_id, Some(call));
        assert_eq!(rig.amy_kj.get_ask(&request_id).await.unwrap().unwrap().summary.tool_call_block_id, Some(call));

        let (callback, mut pushes) = ledger_events_channel(16);
        rig.amy_kj.subscribe_ledger_events(callback, listing.generation).await.unwrap();

        let refused = rig.coder_kj.decide_ask(&request_id, AskVerdict::Allow, None).await.unwrap()
            .expect_err("the performer cannot answer its own ask");
        assert_eq!(refused.kind, AskAnswerFailureKind::NotReviewer, "{refused:?}");

        let answered = rig.amy_kj.decide_ask(&request_id, AskVerdict::Allow, None).await.unwrap()
            .expect("the assigned reviewer answers");
        assert_eq!(answered.summary.status, AskStatus::Allowed);
        assert!(answered.remembered.is_none());

        let push = await_push(&mut pushes, &request_id, AskStatus::Allowed).await;
        assert!(push.generation > listing.generation);

        let late = rig.amy_kj.decide_ask(&request_id, AskVerdict::Deny, None).await.unwrap()
            .expect_err("an answered ask takes no second answer");
        assert_eq!(late.kind, AskAnswerFailureKind::AlreadyAnswered, "{late:?}");

        let decided = rig.amy_kj.get_ask(&request_id).await.unwrap().unwrap();
        let decision = decided.decision.expect("decided");
        assert_eq!(decision.decided_by.map(|p| p.id), Some(rig.amy));
        assert_eq!(decision.option.as_deref(), Some("allow_once"));
        assert_eq!(rig.blocks_mentioning("ledger allow"), 0, "the answer is in the ledger, not a transcript");

        let history = rig.amy_kj.list_asks(&AskFilter { view: AskView::History, ..AskFilter::default() }).await.unwrap();
        assert_eq!(history.asks.iter().map(|a| a.request_id.as_str()).collect::<Vec<_>>(), vec![request_id.as_str()]);
        assert!(rig.amy_kj.list_asks(&AskFilter::default()).await.unwrap().asks.is_empty());
    });
}

/// Amy takes over an ask assigned to another reviewer by reassigning it to
/// herself, then answers it. The ledger records both reviewers and who
/// moved it.
#[test]
fn a_takeover_is_a_recorded_reassignment_then_an_answer() {
    run_local(async {
        let rig = Rig::new().await;
        rig.kernel.kernel_db.lock().assign_test_review_recasting(rig.work, Some(rig.coder), Some(rig.lead)).unwrap();
        let request_id = rig.raise("echo take-over").await;
        let card = rig.amy_kj.get_ask(&request_id).await.unwrap().unwrap().summary;
        assert_eq!(card.reviewer.as_ref().map(|p| p.id), Some(rig.lead));
        assert!(card.can_take_over(rig.amy), "the lineage root takes it from lead");
        assert!(!card.can_take_over(rig.lead) && !card.can_take_over(rig.coder));
        assert!(card.can_escalate(rig.lead) && !card.can_escalate(rig.amy));

        let refused = rig.amy_kj.decide_ask(&request_id, AskVerdict::Allow, None).await.unwrap()
            .expect_err("strict: only the assigned reviewer answers");
        assert_eq!(refused.kind, AskAnswerFailureKind::NotReviewer);

        let moved = rig.amy_kj.escalate_ask(&request_id, Some(rig.amy)).await.unwrap()
            .expect("the lineage root reclaims");
        assert_eq!(moved.summary.reviewer.as_ref().map(|p| p.id), Some(rig.amy));
        assert!(!moved.summary.can_take_over(rig.amy), "the root already holds it");
        let refused = rig.lead_kj.decide_ask(&request_id, AskVerdict::Allow, None).await.unwrap()
            .expect_err("the former reviewer no longer answers");
        assert_eq!(refused.kind, AskAnswerFailureKind::NotReviewer);

        rig.amy_kj.decide_ask(&request_id, AskVerdict::Deny, None).await.unwrap().expect("Amy answers");
        let detail = rig.amy_kj.get_ask(&request_id).await.unwrap().unwrap();
        let moves: Vec<_> = detail.reassignments.iter()
            .map(|m| (m.from.as_ref().map(|p| p.id), m.to.id, m.by.id)).collect();
        assert_eq!(moves, vec![(Some(rig.lead), rig.amy, rig.amy)], "a takeover: Amy moved it from lead to herself");
        assert_eq!(detail.decision.and_then(|d| d.decided_by).map(|p| p.id), Some(rig.amy));
    });
}

/// A drift into the ROOT console reaches the human as an ask on the wire,
/// with origin `Drift` and the drift's text, and lands held in the
/// console. A standing rule is refused, and the human's `decideAsk` allow
/// releases the drift: the kernel's approval worker makes the block `Done`
/// and included.
#[test]
fn a_drift_into_root_waits_on_the_wire_for_the_humans_allow() {
    run_local(async {
        let rig = Rig::new().await;
        let rootbot = PrincipalId::new();
        add_character(&rig.kernel, rootbot, "rootbot");
        let console = common::create_context_typed(&rig.amy_kj, "ROOT", "console").await.unwrap();
        rig.kernel.kernel_db.lock().assign_test_review_recasting(console, Some(rootbot), None).unwrap();
        let source = common::create_context_typed(&rig.amy_kj, "drift-source", "default").await.unwrap();
        rig.kernel.kernel_db.lock().assign_test_review_recasting(source, Some(rig.coder), None).unwrap();

        let argv: Vec<String> = ["drift", "push", "ROOT", "please read this first"].map(String::from).to_vec();
        let pushed = rig.coder_kj.execute_kj_quiet(source, &argv).await.unwrap();
        assert_eq!(pushed.exit_code, 0, "{pushed:?}");
        assert!(pushed.stdout.contains("held"), "{pushed:?}");
        let drift = |kernel: &SharedKernel| kernel.kernel.blocks().block_snapshots(console).unwrap().into_iter()
            .find(|b| b.kind == kaijutsu_types::BlockKind::Drift).expect("the drift landed in ROOT");
        let block = drift(&rig.kernel);
        assert!(block.is_held_drift() && block.excluded, "{block:?}");

        let listing = rig.amy_kj.list_asks(&AskFilter::default()).await.unwrap();
        let card = listing.asks.iter().find(|a| a.origin == AskOrigin::Drift).expect("a drift ask").clone();
        assert_eq!(card.context_id, Some(console));
        assert!(card.description.contains("please read this first"), "{}", card.description);
        assert!(card.answerable_by(rig.amy) && !card.can_remember(rig.amy), "{card:?}");

        let always = kaijutsu_types::Remember { scope: kaijutsu_types::RememberScope::Always, family: false };
        let rule = rig.amy_kj.decide_ask(&card.request_id, AskVerdict::Allow, Some(always)).await.unwrap()
            .expect_err("a drift ask learns no rule");
        assert_eq!(rule.kind, AskAnswerFailureKind::Refused, "{rule:?}");

        rig.amy_kj.decide_ask(&card.request_id, AskVerdict::Allow, None).await.unwrap().expect("amy releases it");
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            let block = drift(&rig.kernel);
            if !block.is_held_drift() {
                assert!(!block.excluded && block.status == kaijutsu_types::Status::Done, "{block:?}");
                break;
            }
            assert!(tokio::time::Instant::now() < deadline, "the allow never released the drift");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    });
}
