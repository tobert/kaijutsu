mod common;

use std::{sync::Arc, time::Duration};
use kaijutsu_client::{turn_events_channel, AuthorBlock, ServerEvent};
use kaijutsu_kernel::llm::{MockClient, Provider};
use kaijutsu_types::{ContextId, PrincipalId, Role};

async fn tick(server: &kaijutsu_server::SharedKernel, context: ContextId, performer: PrincipalId) {
    let admission = server.kernel.admit_context(context).unwrap();
    let cancel = tokio_util::sync::CancellationToken::new();
    kaijutsu_kernel::rc::run(&server.kj_dispatcher, kaijutsu_kernel::rc::RcInvocation {
        ..kaijutsu_kernel::rc::RcInvocation::new("tick", &admission, &cancel)
    }, &kaijutsu_kernel::KjCaller {
        principal_id: performer, actor_id: performer, reviewer_id: None, context_id: Some(context),
        session_id: kaijutsu_types::SessionId::new(), confirmed: false, rc_depth: 0, privileged: false, cancel,
    }).await.unwrap();
}

#[test]
fn conversation_observer_publishes_and_reads_previous_summary_once_per_change() {
    common::run_local(async {
        let (addr, server) = common::start_server_with_mock_llm_kernel_handle().await;
        let client = common::connect_client(addr).await;
        let (kj, _) = client.bind_kernel().await.unwrap();
        let character = common::rc_performer(&kj, "chomosuke").await.unwrap();
        let roots = kj.list_contexts().await.unwrap();
        let root = kaijutsu_client::choose_parent(None, &roots).unwrap().context_id;
        let configured = kj.execute_kj_quiet(root, &["character".into(), "model".into(), "set".into(), character.clone(), "mock/mock-model".into(), "--max-tokens".into(), "4096".into()]).await.unwrap();
        assert_eq!(configured.exit_code, 0, "model setup: {}", configured.stderr);
        let source = common::create_context(&kj, "observed-conversation").await.unwrap();
        let author = common::seed_turn_identity(&server, source);
        let first_id = kj.author_block(&AuthorBlock::text(source, author, Role::User, "We chose the quiet route.")).await.unwrap();
        let mut args = kaijutsu_client::context_create_argv("conversation-observer", &character, None);
        args.extend(["--env".into(), format!("KJ_SUMMARY_CONTEXT={}", source.to_hex())]);
        let result = kj.execute_kj_quiet(root, &args).await.unwrap();
        let observer = kaijutsu_client::context_id_from_create_result(&result)
            .unwrap_or_else(|e| panic!("create observer: {e}; stderr: {}", result.stderr));
        let stance = server.documents.block_snapshots(observer).unwrap().into_iter()
            .find(|b| b.role == Role::System && b.content.starts_with("You are Chomosuke")).unwrap().content;
        assert!(stance.contains("thread of the conversation") && !stance.contains("sound desk"));
        let performer = server.kernel_db.lock().get_context(observer).unwrap().unwrap().performer.unwrap();
        kj.join_context(observer, "conversation-observer").await.unwrap();
        let (callback, mut events) = turn_events_channel(16);
        kj.subscribe_turn_events(callback).await.unwrap();
        server.kernel.llm().write().await.register("mock", Arc::new(Provider::Mock(MockClient::new("The group chose the quiet route."))));
        let topic = format!("{character}/memory/context-{}", source.to_hex());
        let prompts = || server.documents.block_snapshots(observer).unwrap().into_iter()
            .filter(|b| b.content.starts_with("Chomosuke memory\n")).map(|b| b.content).collect::<Vec<_>>();

        tick(&server, observer, performer).await;
        tokio::time::timeout(Duration::from_secs(15), async {
            loop {
                if matches!(events.recv().await.unwrap(), ServerEvent::TurnCompleted { context_id, .. } if context_id == observer) { break; }
            }
        }).await.unwrap_or_else(|_| panic!("observer did not complete: {:?}", server.documents.block_snapshots(observer).unwrap()));
        tokio::time::timeout(Duration::from_secs(15), async {
            loop {
                let read = kj.execute_kj_quiet(root, &["board".into(), "read".into(), topic.clone()]).await.unwrap();
                if read.stdout.contains("The group chose the quiet route.") { break; }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        }).await.unwrap_or_else(|_| panic!("observer did not publish: {:?}", server.documents.block_snapshots(observer).unwrap()));
        tokio::time::timeout(Duration::from_secs(15), async {
            while server.kernel.turn_in_flight(observer) { tokio::time::sleep(Duration::from_millis(10)).await; }
        }).await.expect("observer finishes its done hook");
        let page = kj.execute_kj_quiet(root, &[
            "board".into(), "notes".into(), "--topic".into(), topic.clone(), "--limit".into(), "1".into(),
        ]).await.unwrap();
        let page = page.data.unwrap();
        let summary = page["posts"][0]["body"].as_str().unwrap();
        assert!(summary.starts_with("The group chose the quiet route."),
            "readers with small windows should see Chomosuke's summary before the source IDs: {summary}");
        let initial = prompts();
        assert_eq!(initial.len(), 1);
        assert!(initial[0].contains(&first_id.to_key()), "{}", initial[0]);
        assert!(initial[0].contains("We chose the quiet route."), "{}", initial[0]);
        tick(&server, observer, performer).await;
        assert_eq!(prompts().len(), 1, "unchanged conversation must not drive again");

        let second_id = kj.author_block(&AuthorBlock::text(source, author, Role::User, "We will meet at dawn.")).await.unwrap();
        tick(&server, observer, performer).await;
        tokio::time::timeout(Duration::from_secs(15), async {
            loop {
                if matches!(events.recv().await.unwrap(), ServerEvent::TurnCompleted { context_id, .. } if context_id == observer) { break; }
            }
        }).await.unwrap_or_else(|_| panic!("new conversation did not drive another turn: {:?}", server.documents.block_snapshots(observer).unwrap()));
        let updated = prompts();
        assert_eq!(updated.len(), 2);
        assert!(updated[1].contains(&second_id.to_key()), "{}", updated[1]);
        assert!(updated[1].contains("We will meet at dawn."), "{}", updated[1]);
        assert!(updated[1].contains("Previous summary:\nThe group chose the quiet route."), "{}", updated[1]);
        tokio::time::timeout(Duration::from_secs(15), async {
            while server.kernel.turn_in_flight(observer) { tokio::time::sleep(Duration::from_millis(10)).await; }
        }).await.expect("second done hook finishes");
        tick(&server, observer, performer).await;
        assert_eq!(prompts().len(), 2, "unchanged conversation must not drive a third turn");
        server.kernel.shutdown_runtime_worker().await.unwrap();
    });
}
