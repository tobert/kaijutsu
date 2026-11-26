mod common;

use std::{sync::Arc, time::Duration};
use kaijutsu_client::{turn_events_channel, ServerEvent};
use kaijutsu_kernel::llm::{MockClient, Provider};
use kaijutsu_types::{BlockKind, BlockSnapshotBuilder, ContentType, ContextId, PrincipalId, Role, Tick, TrackId};

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
fn score_observer_publishes_once_for_new_music_on_two_tracks() {
    common::run_local(async {
        let (addr, server) = common::start_server_with_mock_llm_kernel_handle().await;
        let client = common::connect_client(addr).await;
        let (kj, _) = client.bind_kernel().await.unwrap();
        let character = common::rc_performer(&kj, "chomosuke").await.unwrap();
        let roots = kj.list_contexts().await.unwrap();
        let root = kaijutsu_client::choose_parent(None, &roots).unwrap().context_id;
        let configured = kj.execute_kj_quiet(root, &["character".into(), "model".into(), "set".into(), character.clone(), "mock/mock-model".into(), "--max-tokens".into(), "4096".into()]).await.unwrap();
        assert_eq!(configured.exit_code, 0, "{}", configured.stderr);
        let result = kj.execute_kj_quiet(root, &kaijutsu_client::context_create_argv("observer", &character, None)).await.unwrap();
        let observer = kaijutsu_client::context_id_from_create_result(&result).unwrap();
        let principal = server.kernel_db.lock().get_context(observer).unwrap().unwrap().performer.unwrap();
        kj.join_context(observer, "observer").await.unwrap();
        let mut scores = Vec::new();
        for name in ["vibe-one", "vibe-two"] {
            let player = common::create_context(&kj, name).await.unwrap();
            let player_id = common::seed_turn_identity(&server, player);
            let track = TrackId::new(name).unwrap();
            for command in [vec!["transport", "attach", "--track", name], vec!["transport", "ooda", "off", "--track", name], vec!["transport", "play", "--track", name]] {
                let args = command.into_iter().map(String::from).collect::<Vec<_>>();
                let result = kj.execute_kj_quiet(player, &args).await.unwrap();
                assert_eq!(result.exit_code, 0, "{args:?}: {}", result.stderr);
            }
            let score = server.kernel_db.lock().get_track(name).unwrap().unwrap().score_context_id.unwrap();
            let id = server.kernel.blocks().reserve_block_id(score, player_id).unwrap();
            let snapshot = BlockSnapshotBuilder::new(id, BlockKind::Text).role(Role::Model).content_type(ContentType::Abc)
                .content(format!("X:1\nT:{name}\nK:C\nCDEF|\n")).track(track).tick(Tick(0)).build();
            server.kernel.blocks().insert_from_snapshot_as(score, snapshot, None, Some(player_id)).unwrap();
            scores.push(id);
        }
        let (callback, mut events) = turn_events_channel(16);
        kj.subscribe_turn_events(callback).await.unwrap();
        server.kernel.llm().write().await.register("mock", Arc::new(Provider::Mock(MockClient::new("The two lines leave room for each other."))));
        let listing = kj.execute_kj_quiet(observer, &["transport".into(), "list".into(), "--details".into()]).await.unwrap();
        assert_eq!(listing.exit_code, 0, "track listing: {}", listing.stderr);
        assert_eq!(listing.data.as_ref().unwrap().as_array().unwrap().iter().filter(|row| row["playing"] == true).count(), 2, "track listing: {:?}", listing.data);
        tick(&server, observer, principal).await;
        tokio::time::timeout(Duration::from_secs(15), async {
            loop {
                if matches!(events.recv().await.unwrap(), ServerEvent::TurnCompleted { context_id, .. } if context_id == observer) { break; }
            }
        }).await.unwrap_or_else(|_| panic!("observer did not complete: {:?}", server.documents.block_snapshots(observer).unwrap()));
        let topic = format!("{character}/memory/vibe");
        tokio::time::timeout(Duration::from_secs(15), async {
            loop {
                let read = kj.execute_kj_quiet(root, &["board".into(), "read".into(), topic.clone()]).await.unwrap();
                if read.stdout.contains("The two lines leave room") { break; }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        }).await.unwrap_or_else(|_| panic!("observer did not publish: {:?}", server.documents.block_snapshots(observer).unwrap()));
        tokio::time::timeout(Duration::from_secs(15), async {
            while server.kernel.turn_in_flight(observer) { tokio::time::sleep(Duration::from_millis(10)).await; }
        }).await.expect("observer finishes its done hook");
        let prompts = || server.documents.block_snapshots(observer).unwrap().into_iter()
            .filter(|b| b.content.starts_with("Chomosuke memory\n")).map(|b| b.content).collect::<Vec<_>>();
        let first = prompts();
        assert_eq!(first.len(), 1);
        for score in &scores { assert!(first[0].contains(&score.to_key())); }
        assert!(first[0].contains("vibe-one") && first[0].contains("vibe-two"));
        tick(&server, observer, principal).await;
        assert_eq!(prompts().len(), 1, "unchanged music must not drive again");
        let track = TrackId::new("vibe-two").unwrap();
        let score = server.kernel_db.lock().get_track("vibe-two").unwrap().unwrap().score_context_id.unwrap();
        let id = server.kernel.blocks().reserve_block_id(score, principal).unwrap();
        let snapshot = BlockSnapshotBuilder::new(id, BlockKind::Text).role(Role::Model).content_type(ContentType::Abc)
            .content("X:1\nK:C\nGABc|\n").track(track).tick(Tick(0)).build();
        server.kernel.blocks().insert_from_snapshot_as(score, snapshot, None, Some(principal)).unwrap();
        tick(&server, observer, principal).await;
        tokio::time::timeout(Duration::from_secs(15), async {
            loop {
                if matches!(events.recv().await.unwrap(), ServerEvent::TurnCompleted { context_id, .. } if context_id == observer) { break; }
            }
        }).await.expect("new score drives another turn");
        let updated = prompts();
        assert_eq!(updated.len(), 2);
        assert!(updated[1].contains(&id.to_key()));
        server.kernel.shutdown_runtime_worker().await.unwrap();
    });
}
