mod common;

use std::{sync::Arc, time::Duration};
use kaijutsu_client::{turn_events_channel, ServerEvent};
use kaijutsu_kernel::llm::{MockClient, Provider, stream::StreamEvent};
use kaijutsu_kernel::mcp::{CallContext, InstanceId, InstancePolicy, KernelCallParams, KernelTool, KernelToolResult, McpResult, McpServerLike, ServerNotification};

struct Desk {
    id: InstanceId,
    notifications: tokio::sync::broadcast::Sender<ServerNotification>,
    calls: Arc<std::sync::atomic::AtomicUsize>,
}

#[async_trait::async_trait]
impl McpServerLike for Desk {
    fn instance_id(&self) -> &InstanceId { &self.id }
    async fn list_tools(&self, _: &CallContext) -> McpResult<Vec<KernelTool>> {
        Ok(["rehearsal_status", "chair_set", "chair_restore", "unrelated"].into_iter().map(|name| KernelTool {
            instance: self.id.clone(), name: name.into(), description: None, input_schema: serde_json::json!({"type": "object"}),
        }).collect())
    }
    async fn call_tool(&self, params: KernelCallParams, _: &CallContext, _: tokio_util::sync::CancellationToken) -> McpResult<KernelToolResult> {
        assert!(["rehearsal_status", "chair_set", "chair_restore"].contains(&params.tool.as_str()));
        self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(KernelToolResult::text("desk ready"))
    }
    fn notifications(&self) -> tokio::sync::broadcast::Receiver<ServerNotification> { self.notifications.subscribe() }
    async fn shutdown(&self) -> McpResult<()> { Ok(()) }
}
use kaijutsu_types::{ContextId, PrincipalId};

async fn tick(server: &kaijutsu_server::SharedKernel, context: ContextId, performer: PrincipalId) {
    let admission = server.kernel.admit_context(context).unwrap();
    let cancel = tokio_util::sync::CancellationToken::new();
    kaijutsu_kernel::rc::run(&server.kj_dispatcher, kaijutsu_kernel::rc::RcInvocation::new("tick", &admission, &cancel), &kaijutsu_kernel::KjCaller {
        principal_id: performer, actor_id: performer, reviewer_id: None, context_id: Some(context),
        session_id: kaijutsu_types::SessionId::new(), confirmed: false, rc_depth: 0, privileged: false, cancel,
    }).await.unwrap();
}

#[test]
fn engineer_claims_and_reports_once_across_rotation() {
    kaijutsu_kernel::spawn_kaish_thread("engineer-wire", || common::run_local(async {
        let (addr, server) = common::start_server_with_mock_llm_kernel_handle().await;
        let client = common::connect_client(addr).await;
        let (kj, _) = client.bind_kernel().await.unwrap();
        let roots = kj.list_contexts().await.unwrap();
        let root = kaijutsu_client::choose_parent(None, &roots).unwrap().context_id;
        let performer = common::rc_performer(&kj, "toolie").await.unwrap();
        let command = |parts: &[&str]| parts.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let model = kj.execute_kj_quiet(root, &command(&["character", "model", "set", &performer, "mock/mock-model", "--max-tokens", "1024"])).await.unwrap();
        assert_eq!(model.exit_code, 0, "{}", model.stderr);
        let track = kj.execute_kj_quiet(root, &command(&["transport", "attach", "--track", "engineer-test"])).await.unwrap();
        assert_eq!(track.exit_code, 0, "{}", track.stderr);
        let topic = "@track/engineer-test/sound";
        let post = kj.execute_kj_quiet(root, &command(&["board", "post", "@track/engineer-test", "--title", "sound", "Desk"])).await.unwrap();
        assert_eq!(post.exit_code, 0, "{}", post.stderr);
        let reply = kj.execute_kj_quiet(root, &command(&["board", "reply", topic, "Engineer request: Inspect the desk"])).await.unwrap();
        assert_eq!(reply.exit_code, 0, "{}", reply.stderr);
        let created = kj.execute_kj_quiet(root, &command(&["context", "create", &performer, "--name", "engineer", "--env", "KJ_ENGINEER_TOPIC=@track/engineer-test/sound"])).await.unwrap();
        let context = kaijutsu_client::context_id_from_create_result(&created).unwrap();
        let principal = server.kernel_db.lock().get_context(context).unwrap().unwrap().performer.unwrap();
        let stance = server.documents.block_snapshots(context).unwrap().into_iter().map(|b| b.content).collect::<Vec<_>>().join("\n");
        assert!(stance.contains("leave the mix alone"), "{stance}");
        assert!(stance.contains("No check needed.") && stance.contains("once"), "{stance}");
        kj.join_context(context, "engineer").await.unwrap();
        let attached = kj.execute_kj_quiet(root, &command(&["transport", "attach", "--context", "engineer", "--track", "engineer-test", "--wakeup", "8", "--rotate", "4"])).await.unwrap();
        assert_eq!(attached.exit_code, 0, "{}", attached.stderr);
        let (callback, mut events) = turn_events_channel(16);
        kj.subscribe_turn_events(callback).await.unwrap();
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let (notifications, _) = tokio::sync::broadcast::channel(1);
        server.kernel.broker().register(Arc::new(Desk { id: InstanceId::new("external.rehearsal_desk"), notifications, calls: calls.clone() }), InstancePolicy::for_kernel(&server.kernel)).await.unwrap();
        let done = || StreamEvent::Done { stop_reason: Some("end_turn".into()), input_tokens: None, output_tokens: None, extra: None };
        let (mock, tools) = MockClient::new("").with_scripted_stream(vec![
            vec![StreamEvent::ToolUse { id: "read-must-not-set".into(), name: "chair_set".into(), input: serde_json::json!({}), signature: None }, done()],
            vec![StreamEvent::ToolUse { id: "desk-status".into(), name: "rehearsal_status".into(), input: serde_json::json!({}), signature: None }, done()],
            vec![StreamEvent::TextStart, StreamEvent::TextDelta("Desk inspected.".into()), StreamEvent::TextEnd, done()],
        ]).recording_tools();
        server.kernel.llm().write().await.register("mock", Arc::new(Provider::Mock(mock)));
        tokio::join!(tick(&server, context, principal), tick(&server, context, principal));
        tokio::time::timeout(Duration::from_secs(15), async {
            loop { if matches!(events.recv().await.unwrap(), ServerEvent::TurnCompleted { context_id, .. } if context_id == context) { break; } }
        }).await.unwrap_or_else(|_| panic!("engineer turn: {:?}", server.documents.block_snapshots(context).unwrap()));
        tokio::time::timeout(Duration::from_secs(15), async {
            loop {
                let notes = kj.execute_kj_quiet(root, &command(&["board", "notes", "--topic", topic, "--after", "0"])).await.unwrap();
                if notes.stdout.contains("Engineer result:") && server.kernel_db.lock().get_context_env(context).unwrap().iter().any(|v| v.key == "KJ_ENGINEER_CURSOR") { break; }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        }).await.expect("engineer result");
        let cursor = server.kernel_db.lock().get_context_env(context).unwrap();
        assert!(cursor.iter().any(|v| v.key == "KJ_ENGINEER_CURSOR" && v.value == "2"), "{cursor:?}");
        let notes = kj.execute_kj_quiet(root, &command(&["board", "notes", "--topic", topic, "--after", "0"])).await.unwrap();
        let claim_count = notes.stdout.matches("Engineer claimed:").count();
        assert!(claim_count >= 1, "{}", notes.stdout);
        assert!(notes.stdout.contains("desk ready") && !notes.stdout.contains("Desk inspected."), "{}", notes.stdout);
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
        let offered = tools.lock();
        let mut roster = offered[0].clone();
        roster.sort();
        assert_eq!(roster, vec!["rehearsal_status"]);
        drop(offered);
        let prompts = server.documents.block_snapshots(context).unwrap().into_iter().filter(|b| b.content.starts_with("Engineer work\n")).count();
        assert_eq!(prompts, 1, "overlapping ticks must drive only once");
        let change = kj.execute_kj_quiet(root, &command(&["board", "reply", topic, "Engineer change: Set and restore the chair"])).await.unwrap();
        assert_eq!(change.exit_code, 0, "{}", change.stderr);
        let change_number = change.data.as_ref().unwrap()["post_number"].as_u64().unwrap().to_string();
        let (mock, change_tools) = MockClient::new("").with_scripted_stream(vec![
            vec![StreamEvent::ToolUse { id: "desk-set".into(), name: "chair_set".into(), input: serde_json::json!({}), signature: None }, done()],
            vec![StreamEvent::ToolUse { id: "desk-restore".into(), name: "chair_restore".into(), input: serde_json::json!({}), signature: None }, done()],
            vec![StreamEvent::TextStart, StreamEvent::TextDelta("Chair restored.".into()), StreamEvent::TextEnd, done()],
        ]).recording_tools();
        server.kernel.llm().write().await.register("mock", Arc::new(Provider::Mock(mock)));
        tick(&server, context, principal).await;
        tokio::time::timeout(Duration::from_secs(15), async {
            loop { if matches!(events.recv().await.unwrap(), ServerEvent::TurnCompleted { context_id, .. } if context_id == context) { break; } }
        }).await.expect("change turn");
        tokio::time::timeout(Duration::from_secs(15), async {
            loop {
                let notes = kj.execute_kj_quiet(root, &command(&["board", "notes", "--topic", topic, "--after", "0"])).await.unwrap();
                if notes.stdout.contains("chair_restore") && server.kernel_db.lock().get_context_env(context).unwrap().iter().any(|v| v.key == "KJ_ENGINEER_CURSOR" && v.value == change_number) { break; }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        }).await.expect("change result");
        let mut change_roster = change_tools.lock()[0].clone();
        change_roster.sort();
        assert_eq!(change_roster, vec!["chair_restore", "chair_set", "rehearsal_status"]);
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 3);
        let no_tools = kj.execute_kj_quiet(root, &command(&["board", "reply", topic, "Engineer request: Observation from Chomosuke\nThe bass and drums sound balanced."])).await.unwrap();
        let no_tools_id = no_tools.data.as_ref().unwrap()["block_id"].as_str().unwrap();
        let no_tools_number = no_tools.data.as_ref().unwrap()["post_number"].as_u64().unwrap().to_string();
        let (observation_mock, observation_tools) = MockClient::new("The delay is 68 ms.").recording_tools();
        server.kernel.llm().write().await.register("mock", Arc::new(Provider::Mock(observation_mock)));
        tick(&server, context, principal).await;
        tokio::time::timeout(Duration::from_secs(15), async {
            loop {
                let notes = kj.execute_kj_quiet(root, &command(&["board", "notes", "--topic", topic, "--after", "0"])).await.unwrap();
                if notes.stdout.contains(&format!("Engineer result: {no_tools_id}")) && server.kernel_db.lock().get_context_env(context).unwrap().iter().any(|v| v.key == "KJ_ENGINEER_CURSOR" && v.value == no_tools_number) {
                    assert!(notes.stdout.contains("No desk reading or change recorded"), "{}", notes.stdout);
                    assert!(!notes.stdout.contains("68 ms"), "{}", notes.stdout);
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        }).await.expect("no-tools result");
        assert_eq!(observation_tools.lock()[0], vec!["rehearsal_status"]);
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 3, "observation left the desk alone");
        assert!(server.documents.block_snapshots(context).unwrap().iter().any(|b| b.content.contains("Observation from Chomosuke\nThe bass and drums sound balanced.")));
        let interrupted = kj.execute_kj_quiet(root, &command(&["board", "reply", topic, "Engineer request: Change the chair"])).await.unwrap();
        assert_eq!(interrupted.exit_code, 0, "{}", interrupted.stderr);
        let interrupted_id = interrupted.data.as_ref().unwrap()["block_id"].as_str().unwrap();
        let interrupted_claim = format!("Engineer claimed: {interrupted_id}\nContext: {}\nA claim may need inspection if no result follows.", context.to_hex());
        let claim = kj.execute_kj_quiet(root, &command(&["board", "reply", topic, &interrupted_claim])).await.unwrap();
        assert_eq!(claim.exit_code, 0, "{}", claim.stderr);
        let cursor = server.kernel_db.lock().get_context_env(context).unwrap().into_iter().find(|v| v.key == "KJ_ENGINEER_CURSOR").unwrap().value;
        let before_rotation = kj.execute_kj_quiet(root, &command(&["board", "notes", "--topic", topic, "--after", "0"])).await.unwrap();
        let claim_count = before_rotation.stdout.matches("Engineer claimed:").count() - 1;
        let rotated = kj.execute_kj_quiet(root, &command(&["context", "rotate", "engineer"])).await.unwrap();
        assert_eq!(rotated.exit_code, 0, "{}", rotated.stderr);
        let contexts = kj.list_contexts().await.unwrap();
        let successor = contexts.iter().find(|c| c.label == "engineer" && c.id != context).unwrap().id;
        let vars = server.kernel_db.lock().get_context_env(successor).unwrap();
        assert!(vars.iter().any(|v| v.key == "KJ_ENGINEER_TOPIC" && v.value == topic));
        assert!(vars.iter().any(|v| v.key == "KJ_ENGINEER_CURSOR" && v.value == cursor));
        let attachments = server.kernel_db.lock().list_attachments_for_context(successor).unwrap();
        assert_eq!(attachments.len(), 1);
        assert_eq!(attachments[0].wakeup_every, 8);
        assert_eq!(attachments[0].rotate_every_phrases, Some(4));
        tick(&server, successor, principal).await;
        let notes = kj.execute_kj_quiet(root, &command(&["board", "notes", "--topic", topic, "--after", "0"])).await.unwrap();
        assert_eq!(notes.stdout.matches("Engineer claimed:").count(), claim_count + 1, "{}", notes.stdout);
        assert!(notes.stdout.contains(&format!("Engineer result: {interrupted_id}")) && notes.stdout.contains("Status: interrupted"), "{}", notes.stdout);
        kj.join_context(successor, "engineer-successor").await.unwrap();
        let (mock, after_rotation_tools) = MockClient::new("").with_scripted_stream(vec![
            vec![StreamEvent::ToolUse { id: "after-status".into(), name: "rehearsal_status".into(), input: serde_json::json!({}), signature: None }, done()],
            vec![StreamEvent::TextStart, StreamEvent::TextDelta("After rotation inspected.".into()), StreamEvent::TextEnd, done()],
            vec![StreamEvent::ToolUse { id: "after-change".into(), name: "chair_set".into(), input: serde_json::json!({}), signature: None }, done()],
            vec![StreamEvent::TextStart, StreamEvent::TextDelta("After rotation changed.".into()), StreamEvent::TextEnd, done()],
        ]).recording_tools();
        server.kernel.llm().write().await.register("mock", Arc::new(Provider::Mock(mock)));
        for (request, result) in [("Engineer request: Observation from Chomosuke\nThe drums seem late.", "rehearsal_status"), ("Engineer change: Set the chair", "chair_set")] {
            let posted = kj.execute_kj_quiet(root, &command(&["board", "reply", topic, request])).await.unwrap();
            assert_eq!(posted.exit_code, 0, "{}", posted.stderr);
            let posted_id = posted.data.as_ref().unwrap()["block_id"].as_str().unwrap();
            tick(&server, successor, principal).await;
            tokio::time::timeout(Duration::from_secs(15), async {
                loop { if matches!(events.recv().await.unwrap(), ServerEvent::TurnCompleted { context_id, .. } if context_id == successor) { break; } }
            }).await.expect("successor turn");
            tokio::time::timeout(Duration::from_secs(15), async {
                loop {
                    let notes = kj.execute_kj_quiet(root, &command(&["board", "notes", "--topic", topic, "--after", "0"])).await.unwrap();
                    if notes.stdout.contains(&format!("Engineer result: {posted_id}")) && notes.stdout.contains(result) { break; }
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            }).await.expect("successor result");
        }
        let recorded = after_rotation_tools.lock();
        assert_eq!(recorded[0], vec!["rehearsal_status"]);
        let mut roster = recorded[2].clone();
        roster.sort();
        assert_eq!(roster, vec!["chair_restore", "chair_set", "rehearsal_status"]);
    })).unwrap().join().unwrap();
}

#[test]
fn engineer_forwards_only_addressed_native_notes_once_across_rotation() {
    kaijutsu_kernel::spawn_kaish_thread("engineer-notes-wire", || common::run_local(async {
        let (addr, server) = common::start_server_with_mock_llm_kernel_handle().await;
        let client = common::connect_client(addr).await;
        let (kj, _) = client.bind_kernel().await.unwrap();
        let roots = kj.list_contexts().await.unwrap();
        let root = kaijutsu_client::choose_parent(None, &roots).unwrap().context_id;
        let command = |parts: &[&str]| parts.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let toolie = common::rc_performer(&kj, "toolie").await.unwrap();
        let chomo = common::rc_performer(&kj, "chomosuke").await.unwrap();
        let model = kj.execute_kj_quiet(root, &command(&["character", "model", "set", &toolie, "mock/mock-model", "--max-tokens", "1024"])).await.unwrap();
        assert_eq!(model.exit_code, 0, "{}", model.stderr);
        let created = kj.execute_kj_quiet(root, &command(&["context", "create", &chomo, "--name", "notes-source"])).await.unwrap();
        let source = kaijutsu_client::context_id_from_create_result(&created).unwrap();
        let attached = kj.execute_kj_quiet(root, &command(&["transport", "attach", "--context", "notes-source", "--track", "notes-test"])).await.unwrap();
        assert_eq!(attached.exit_code, 0, "{}", attached.stderr);
        let playing = kj.execute_kj_quiet(root, &command(&["transport", "play", "--track", "notes-test"])).await.unwrap();
        assert_eq!(playing.exit_code, 0, "{}", playing.stderr);
        let desk = "@track/notes-test/sound";
        let posted = kj.execute_kj_quiet(root, &command(&["board", "post", "@track/notes-test", "--title", "sound", "Desk"])).await.unwrap();
        assert_eq!(posted.exit_code, 0, "{}", posted.stderr);
        let notes_topic = format!("{chomo}/music-notes/notes-test");
        let source_principal = server.kernel_db.lock().get_context(source).unwrap().unwrap().performer.unwrap();
        let source_caller = kaijutsu_kernel::KjCaller {
            principal_id: source_principal, actor_id: source_principal, reviewer_id: None, context_id: Some(source),
            session_id: kaijutsu_types::SessionId::new(), confirmed: false, rc_depth: 0, privileged: false,
            cancel: tokio_util::sync::CancellationToken::new(),
        };
        let ordinary = server.kj_dispatcher.dispatch(&command(&["board", "add-note", "--track", "notes-test", "--at", "1", "@ToolieX is not an address."]), &source_caller).await;
        assert!(ordinary.is_ok(), "{}", ordinary.message());
        let env = format!("KJ_OBSERVER_NOTES={notes_topic}");
        let created = kj.execute_kj_quiet(root, &command(&["context", "create", &toolie, "--name", "notes-engineer", "--env", "KJ_ENGINEER_TOPIC=@track/notes-test/sound", "--env", &env])).await.unwrap();
        let context = kaijutsu_client::context_id_from_create_result(&created).unwrap();
        let principal = server.kernel_db.lock().get_context(context).unwrap().unwrap().performer.unwrap();
        kj.join_context(context, "notes-engineer").await.unwrap();
        let (notifications, _) = tokio::sync::broadcast::channel(1);
        server.kernel.broker().register(Arc::new(Desk { id: InstanceId::new("external.rehearsal_desk"), notifications, calls: Arc::new(std::sync::atomic::AtomicUsize::new(0)) }), InstancePolicy::for_kernel(&server.kernel)).await.unwrap();
        let (mock, roster) = MockClient::new("No desk inspection needed.").recording_tools();
        server.kernel.llm().write().await.register("mock", Arc::new(Provider::Mock(mock)));
        tick(&server, context, principal).await;
        let contents = server.documents.block_snapshots(context).unwrap();
        assert!(!contents.iter().any(|b| b.content.starts_with("Engineer work\n")), "ordinary notes do not wake the model");
        let ping = server.kj_dispatcher.dispatch(&command(&["board", "add-note", "--track", "notes-test", "--at", "2", "@Toolie: The drums sound late."]), &source_caller).await;
        assert!(ping.is_ok(), "{}", ping.message());
        let source_posts = kj.execute_kj_quiet(root, &command(&["board", "notes", "--topic", &notes_topic, "--after", "0"])).await.unwrap();
        let source_id = source_posts.data.unwrap()["posts"][1]["block_id"].as_str().unwrap().to_string();
        tick(&server, context, principal).await;
        tokio::time::timeout(Duration::from_secs(15), async {
            loop {
                let notes = kj.execute_kj_quiet(root, &command(&["board", "notes", "--topic", desk, "--after", "0"])).await.unwrap();
                if notes.stdout.contains("Engineer result:") { break; }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        }).await.unwrap_or_else(|_| panic!("forwarded note is handled: {:?}", server.documents.block_snapshots(context).unwrap()));
        let notes = kj.execute_kj_quiet(root, &command(&["board", "notes", "--topic", desk, "--after", "0"])).await.unwrap();
        assert!(notes.stdout.contains("Engineer request: Observation from Chomosuke\n@Toolie: The drums sound late."), "{}", notes.stdout);
        assert!(notes.stdout.contains(&format!("Source note: {source_id}")), "{}", notes.stdout);
        let offered = roster.lock();
        let mut names = offered[0].clone(); names.sort();
        assert_eq!(names, vec!["notes", "rehearsal_status"]);
        drop(offered);
        assert_eq!(notes.stdout.matches("Engineer request: Observation from Chomosuke").count(), 1);
        let duplicate = format!("Engineer request: Observation from Chomosuke\n@Toolie: The drums sound late.\nSource note: {source_id}");
        let posted_duplicate = kj.execute_kj_quiet(root, &command(&["board", "reply", desk, &duplicate])).await.unwrap();
        assert_eq!(posted_duplicate.exit_code, 0, "{}", posted_duplicate.stderr);
        let rotated = kj.execute_kj_quiet(root, &command(&["context", "rotate", "notes-engineer"])).await.unwrap();
        assert_eq!(rotated.exit_code, 0, "{}", rotated.stderr);
        let successor = kj.list_contexts().await.unwrap().into_iter().find(|c| c.label == "notes-engineer" && c.id != context).unwrap().id;
        server.kernel_db.lock().set_context_env(successor, "KJ_OBSERVER_CURSOR", "0").unwrap();
        tick(&server, successor, principal).await;
        let notes = kj.execute_kj_quiet(root, &command(&["board", "notes", "--topic", desk, "--after", "0"])).await.unwrap();
        assert_eq!(notes.stdout.matches("Engineer request: Observation from Chomosuke").count(), 2, "{}", notes.stdout);
        assert!(!server.documents.block_snapshots(successor).unwrap().iter().any(|b| b.content.starts_with("Engineer work\n")), "a duplicate source must not start another turn");
        let comma = server.kj_dispatcher.dispatch(&command(&["board", "add-note", "--track", "notes-test", "--at", "3", "@Toolie, the bass is quiet."]), &source_caller).await;
        assert!(comma.is_ok(), "{}", comma.message());
        tick(&server, successor, principal).await;
        let notes = kj.execute_kj_quiet(root, &command(&["board", "notes", "--topic", desk, "--after", "0"])).await.unwrap();
        assert!(notes.stdout.contains("Engineer request: Observation from Chomosuke\n@Toolie, the bass is quiet."), "{}", notes.stdout);
    })).unwrap().join().unwrap();
}
