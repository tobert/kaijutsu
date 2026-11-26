mod common;

use std::{sync::Arc, time::Duration};
use kaijutsu_client::{turn_events_channel, ServerEvent};
use kaijutsu_kernel::llm::{MockClient, Provider, stream::StreamEvent};

#[test]
fn native_note_is_mirrored_to_vibe() {
    kaijutsu_kernel::spawn_kaish_thread("chomosuke-note-test", || common::run_local(async {
        let (addr, server) = common::start_server_with_mock_llm_kernel_handle().await;
        let client = common::connect_client(addr).await;
        let (kj, _) = client.bind_kernel().await.unwrap();
        let character = common::rc_performer(&kj, "chomosuke").await.unwrap();
        let root = kaijutsu_client::choose_parent(None, &kj.list_contexts().await.unwrap()).unwrap().context_id;
        let configured = kj.execute_kj_quiet(root, &["character".into(), "model".into(), "set".into(), character.clone(), "mock/mock-model".into(), "--max-tokens".into(), "4096".into()]).await.unwrap();
        assert_eq!(configured.exit_code, 0, "{}", configured.stderr);
        let created = kj.execute_kj_quiet(root, &kaijutsu_client::context_create_argv("observer", &character, None)).await.unwrap();
        let observer = kaijutsu_client::context_id_from_create_result(&created).unwrap();
        kj.join_context(observer, "observer").await.unwrap();
        let grant = kj.execute_kj_quiet(root, &["binding".into(), "allow".into(), "builtin.notes".into(), observer.to_hex()]).await.unwrap();
        assert_eq!(grant.exit_code, 0, "{}", grant.stderr);
        let set = kj.execute_kj_quiet(root, &["context".into(), "set".into(), observer.to_hex(), "--env".into(), "KJ_POST_NOTES=band".into()]).await.unwrap();
        assert_eq!(set.exit_code, 0, "{}", set.stderr);
        for args in [vec!["transport", "attach", "--track", "band"], vec!["transport", "ooda", "off", "--track", "band"], vec!["transport", "play", "--track", "band"]] {
            let args = args.into_iter().map(String::from).collect::<Vec<_>>();
            let result = kj.execute_kj_quiet(root, &args).await.unwrap();
            assert_eq!(result.exit_code, 0, "{args:?}: {}", result.stderr);
        }
        let topic = format!("{character}/memory/vibe");
        let read = kj.execute_kj_quiet(root, &["board".into(), "read".into(), topic.clone(), "--tail".into(), "1".into()]).await.unwrap();
        assert_eq!(read.exit_code, 0, "{}", read.stderr);
        let board = read.data.unwrap();
        let base = board["posts"][0]["block_id"].as_str().unwrap().to_string();
        let done = || StreamEvent::Done { stop_reason: Some("end_turn".into()), input_tokens: None, output_tokens: None, extra: None };
        let mock = MockClient::new("").with_scripted_stream(vec![
            vec![StreamEvent::ToolUse { id: "note-one".into(), name: "add_note".into(), input: serde_json::json!({"track": "band", "body": "The bass rests on beat two."}), signature: None }, done()],
            vec![StreamEvent::TextStart, StreamEvent::TextDelta("Unrelated final text.".into()), StreamEvent::TextEnd, done()],
        ]);
        server.kernel.llm().write().await.register("mock", Arc::new(Provider::Mock(mock)));
        let (callback, mut events) = turn_events_channel(16);
        kj.subscribe_turn_events(callback).await.unwrap();
        let prompt = format!("Chomosuke memory\n{topic}\nsource:1\n{base}\n\nObserve.");
        let drive = kj.execute_kj_quiet(observer, &["drive".into(), "--prompt".into(), prompt]).await.unwrap();
        assert_eq!(drive.exit_code, 0, "{}", drive.stderr);
        tokio::time::timeout(Duration::from_secs(15), async {
            loop { if matches!(events.recv().await.unwrap(), ServerEvent::TurnCompleted { context_id, .. } if context_id == observer) { break; } }
            while server.kernel.turn_in_flight(observer) { tokio::time::sleep(Duration::from_millis(10)).await; }
        }).await.unwrap();
        let read = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let read = kj.execute_kj_quiet(root, &["board".into(), "read".into(), topic.clone(), "--tail".into(), "1".into()]).await.unwrap();
                if read.stdout.contains("The bass rests on beat two.") { break read; }
                tokio::time::sleep(Duration::from_millis(30)).await;
            }
        }).await.unwrap_or_else(|_| panic!("observer blocks: {:?}", server.documents.block_snapshots(observer).unwrap().iter().filter(|b| b.kind == kaijutsu_types::BlockKind::Error || b.kind == kaijutsu_types::BlockKind::ToolResult).collect::<Vec<_>>()));
        assert!(read.stdout.contains("The bass rests on beat two."), "{}; blocks: {:?}", read.stdout, server.documents.block_snapshots(observer).unwrap());
        assert!(!read.stdout.contains("Unrelated final text."));
        assert!(read.stdout.contains("Sources: source:1"));
        let blocks = server.documents.block_snapshots(observer).unwrap();
        let call = blocks.iter().find(|b| b.tool_use_id.as_deref() == Some("note-one") && b.kind == kaijutsu_types::BlockKind::ToolCall).unwrap();
        assert_eq!(call.tool_kind, Some(kaijutsu_types::ToolKind::Builtin));
        let receipt = blocks.iter().find(|b| b.tool_call_id == Some(call.id) && b.kind == kaijutsu_types::BlockKind::ToolResult).unwrap();
        assert!(!receipt.is_error, "{}", receipt.content);
        let base = read.data.unwrap()["posts"][0]["block_id"].as_str().unwrap().to_string();
        let mock = MockClient::new("").with_scripted_stream(vec![
            vec![StreamEvent::ToolUse { id: "bad-note".into(), name: "add_note".into(), input: serde_json::json!({"track": "missing", "body": "This must not be mirrored."}), signature: None }, done()],
            vec![StreamEvent::TextStart, StreamEvent::TextDelta("Do not mirror this either.".into()), StreamEvent::TextEnd, done()],
        ]);
        server.kernel.llm().write().await.register("mock", Arc::new(Provider::Mock(mock)));
        let prompt = format!("Chomosuke memory\n{topic}\nsource:2\n{base}\n\nObserve.");
        let drive = kj.execute_kj_quiet(observer, &["drive".into(), "--prompt".into(), prompt]).await.unwrap();
        assert_eq!(drive.exit_code, 0, "{}", drive.stderr);
        tokio::time::timeout(Duration::from_secs(15), async {
            loop { if matches!(events.recv().await.unwrap(), ServerEvent::TurnCompleted { context_id, .. } if context_id == observer) { break; } }
            while server.kernel.turn_in_flight(observer) { tokio::time::sleep(Duration::from_millis(10)).await; }
        }).await.unwrap();
        tokio::time::sleep(Duration::from_millis(300)).await;
        let blocks = server.documents.block_snapshots(observer).unwrap();
        let failed = blocks.iter().find(|b| b.tool_use_id.as_deref() == Some("bad-note") && b.kind == kaijutsu_types::BlockKind::ToolResult).unwrap();
        assert!(failed.is_error, "{}", failed.content);
        let after = kj.execute_kj_quiet(root, &["board".into(), "read".into(), topic, "--tail".into(), "1".into()]).await.unwrap();
        assert_eq!(after.data.unwrap()["posts"][0]["block_id"].as_str().unwrap(), base);
        server.kernel.shutdown_runtime_worker().await.unwrap();
    })).unwrap().join().unwrap();
}
