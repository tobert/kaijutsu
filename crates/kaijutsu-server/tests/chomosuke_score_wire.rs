mod common;

use std::{sync::Arc, time::Duration};
use kaijutsu_client::{turn_events_channel, ServerEvent};
use kaijutsu_kernel::{ContentStore, llm::{MockClient, Provider}};
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
    kaijutsu_kernel::spawn_kaish_thread("chomosuke-score-test", || common::run_local(async {
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
            let old = server.kernel.blocks().reserve_block_id(score, player_id).unwrap();
            let earlier = BlockSnapshotBuilder::new(old, BlockKind::Text).role(Role::Model).content_type(ContentType::Abc)
                .content("X:1\nT:Earlier phrase\nK:C\nGGGG|").track(track.clone()).tick(Tick(-64)).build();
            server.kernel.blocks().insert_from_snapshot_as(score, earlier, None, Some(player_id)).unwrap();
            let id = server.kernel.blocks().reserve_block_id(score, player_id).unwrap();
            let snapshot = BlockSnapshotBuilder::new(id, BlockKind::Text).role(Role::Model).content_type(ContentType::Abc)
                .content(format!("X:1\nT:{name}\nK:C\nCDEF|\n")).track(track).tick(Tick(0)).build();
            server.kernel.blocks().insert_from_snapshot_as(score, snapshot, None, Some(player_id)).unwrap();
            scores.push(id);
        }
        let desk = "@track/vibe-one/sound";
        let created = kj.execute_kj_quiet(root, &["board".into(), "post".into(), "@track/vibe-one".into(), "--title".into(), "sound".into(), "Rehearsal desk".into()]).await.unwrap();
        assert_eq!(created.exit_code, 0, "{}", created.stderr);
        server.kernel_db.lock().set_context_env(observer, "KJ_ENGINEER_TOPIC", desk).unwrap();
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
        let desk_posts = kj.execute_kj_quiet(root, &["board".into(), "notes".into(), "--topic".into(), desk.into()]).await.unwrap();
        assert_eq!(desk_posts.data.unwrap()["posts"][0]["body"], "Engineer request: Observation from Chomosuke\nThe two lines leave room for each other.");
        let vibe = kj.execute_kj_quiet(root, &["board".into(), "notes".into(), "--topic".into(), topic.clone()]).await.unwrap();
        assert!(!vibe.data.unwrap()["posts"][0]["body"].as_str().unwrap().contains("Engineer request:"));
        let stance = server.documents.block_snapshots(observer).unwrap().into_iter()
            .find(|b| b.role == Role::System && b.content.starts_with("You are Chomosuke")).unwrap().content;
        assert!(stance.contains("same track and tick"));
        assert!(!stance.contains("thread of the conversation"));
        let first = prompts();
        assert_eq!(first.len(), 1);
        assert!(!first[0].contains("Earlier phrase"));
        assert!(first[0].contains("One observation in plain text"));
        assert!(!first[0].contains("JSON object"));
        for score in &scores { assert!(first[0].contains(&score.to_key())); }
        assert!(first[0].contains("vibe-one") && first[0].contains("vibe-two"));
        tick(&server, observer, principal).await;
        assert_eq!(prompts().len(), 1, "unchanged music must not drive again");
        let post = kj.execute_kj_quiet(root, &["board".into(), "post".into(), "@track/vibe-one".into(),
            "--title".into(), "handoff".into(), format!("Leave space for the bass pickup. {}", "a".repeat(400))]).await.unwrap();
        assert_eq!(post.exit_code, 0, "{}", post.stderr);
        let hash = server.kernel.cas().store(b"RIFFold", "audio/wav").unwrap();
        let old_id = server.kernel.blocks().reserve_block_id(observer, principal).unwrap();
        let old = BlockSnapshotBuilder::new(old_id, BlockKind::Text).role(Role::Asset)
            .content_type(ContentType::from_mime("audio/wav")).content(hash.to_string()).build();
        server.kernel.blocks().insert_from_snapshot_as(observer, old, None, Some(principal)).unwrap();
        server.kernel_db.lock().set_context_env(observer, "KJ_CHOMOSUKE_LISTEN_BLOCK", &old_id.to_key()).unwrap();
        let manual_id = server.kernel.blocks().reserve_block_id(observer, principal).unwrap();
        let manual = BlockSnapshotBuilder::new(manual_id, BlockKind::Text).role(Role::Asset)
            .content_type(ContentType::from_mime("audio/wav")).content(hash.to_string()).build();
        server.kernel.blocks().insert_from_snapshot_as(observer, manual, None, Some(principal)).unwrap();
        server.kernel_db.lock().set_context_env(observer, "KJ_LISTEN", "ghost").unwrap();
        tick(&server, observer, principal).await;
        assert!(server.documents.block_snapshots(observer).unwrap().iter().any(|b| b.kind == BlockKind::Error && b.content.contains("ghost")), "render failures must be visible");
        assert!(server.documents.block_snapshots(observer).unwrap().iter().find(|b| b.id == old_id).unwrap().excluded,
            "a failed render must not reuse old audio");
        assert!(!server.documents.block_snapshots(observer).unwrap().iter().find(|b| b.id == manual_id).unwrap().excluded,
            "manual audio must stay included");
        server.kernel_db.lock().set_context_env(observer, "KJ_LISTEN", "").unwrap();
        tokio::time::timeout(Duration::from_secs(15), async {
            loop {
                if matches!(events.recv().await.unwrap(), ServerEvent::TurnCompleted { context_id, .. } if context_id == observer) { break; }
            }
            while server.kernel.turn_in_flight(observer) { tokio::time::sleep(Duration::from_millis(10)).await; }
        }).await.expect("new handoff drives another turn");
        assert!(prompts()[1].contains("Leave space for the bass pickup."));
        assert!(prompts()[1].contains("New handoff"), "{}", prompts()[1]);
        tick(&server, observer, principal).await;
        assert_eq!(prompts().len(), 2, "unchanged handoff must not drive again");
        let stopped = kj.execute_kj_quiet(root, &["transport".into(), "stop".into(), "--track".into(), "vibe-two".into()]).await.unwrap();
        assert_eq!(stopped.exit_code, 0, "{}", stopped.stderr);
        tick(&server, observer, principal).await;
        assert_eq!(prompts().len(), 2, "stopping another track must not change the handoff basis");
        let playing = kj.execute_kj_quiet(root, &["transport".into(), "play".into(), "--track".into(), "vibe-two".into()]).await.unwrap();
        assert_eq!(playing.exit_code, 0, "{}", playing.stderr);
        let track = TrackId::new("vibe-two").unwrap();
        let score = server.kernel_db.lock().get_track("vibe-two").unwrap().unwrap().score_context_id.unwrap();
        let id = server.kernel.blocks().reserve_block_id(score, principal).unwrap();
        let snapshot = BlockSnapshotBuilder::new(id, BlockKind::Text).role(Role::Model).content_type(ContentType::Abc)
            .content("X:1\nK:C\nGABc|\n").track(track).tick(Tick(0)).build();
        server.kernel.blocks().insert_from_snapshot_as(score, snapshot, None, Some(principal)).unwrap();
        server.kernel.llm().write().await.register("mock", Arc::new(Provider::Mock(MockClient::new(
            "The score ends with four rising notes."))));
        let correction = kj.execute_kj_quiet(root, &["board".into(), "reply".into(), desk.into(),
            "Correction to the desk reading: pan is a controller value.".into()]).await.unwrap();
        assert_eq!(correction.exit_code, 0, "{}", correction.stderr);
        let disabled_id = server.kernel.blocks().reserve_block_id(observer, principal).unwrap();
        let disabled = BlockSnapshotBuilder::new(disabled_id, BlockKind::Text).role(Role::Asset)
            .content_type(ContentType::from_mime("audio/wav")).content(hash.to_string()).build();
        server.kernel.blocks().insert_from_snapshot_as(observer, disabled, None, Some(principal)).unwrap();
        server.kernel_db.lock().set_context_env(observer, "KJ_CHOMOSUKE_LISTEN_BLOCK", &disabled_id.to_key()).unwrap();
        tick(&server, observer, principal).await;
        tokio::time::timeout(Duration::from_secs(15), async {
            loop {
                if matches!(events.recv().await.unwrap(), ServerEvent::TurnCompleted { context_id, .. } if context_id == observer) { break; }
            }
        }).await.expect("new score drives another turn");
        let snapshots = server.documents.block_snapshots(observer).unwrap();
        assert!(snapshots.iter().find(|b| b.id == disabled_id).unwrap().excluded, "disabled listening removes its clip");
        assert!(!snapshots.iter().find(|b| b.id == manual_id).unwrap().excluded, "disabled listening preserves manual audio");
        let updated = prompts();
        assert_eq!(updated.len(), 3);
        assert!(!updated[2].contains("Observation from Chomosuke"));
        assert!(!updated[2].contains("pan is a controller value"));
        let posts = tokio::time::timeout(Duration::from_secs(15), async {
            loop {
                let read = kj.execute_kj_quiet(root, &["board".into(), "notes".into(), "--topic".into(), desk.into()]).await.unwrap();
                let posts = read.data.unwrap();
                if posts["posts"][0]["body"] == "Engineer request: Observation from Chomosuke\nThe score ends with four rising notes." { return posts; }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        }).await.expect("the done hook forwards the observation");
        assert_eq!(posts["posts"].as_array().unwrap().len(), 4, "opening post, two distinct listening notes and one desk correction");
        assert!(!updated[2].contains("Leave space for the bass pickup."), "a standing handoff must leave room for the new observation");
        assert!(updated[2].contains(&id.to_key()));
        server.kernel_db.lock().set_context_env(observer, "KJ_POST_NOTES", "vibe-two").unwrap();
        let address = kj.execute_kj_quiet(root, &["board".into(), "post".into(), "@track/vibe-two".into(),
            "--title".into(), "handoff".into(), "Listen to the opening for @Toolie.".into()]).await.unwrap();
        assert_eq!(address.exit_code, 0, "{}", address.stderr);
        let id = server.kernel.blocks().reserve_block_id(score, principal).unwrap();
        let snapshot = BlockSnapshotBuilder::new(id, BlockKind::Text).role(Role::Model).content_type(ContentType::Abc)
            .content("X:1\nM:4/4\nK:C\nCDEF | GABc | fedc | BAGF |]").track(TrackId::new("vibe-two").unwrap()).tick(Tick(0)).build();
        server.kernel.blocks().insert_from_snapshot_as(score, snapshot, None, Some(principal)).unwrap();
        tick(&server, observer, principal).await;
        let posting = prompts().pop().unwrap();
        assert!(posting.contains("add_note") && posting.contains("twelve words"), "{posting}");
        assert!(posting.contains("CDEF | GABc") && !posting.contains("fedc"), "{posting}");
        assert!(!posting.contains("Track vibe-one"), "{posting}");
        assert!(posting.contains("Start body with @Toolie"), "{posting}");
        server.kernel.shutdown_runtime_worker().await.unwrap();
    })).unwrap().join().unwrap();
}

#[test]
#[ignore = "requires a General MIDI SoundFont in the model directory at soundfont/default.sf2"]
fn score_listener_renders_and_replaces_audio_in_model_turns() {
    kaijutsu_kernel::spawn_kaish_thread("chomosuke-listen-test", || common::run_local(async {
        use kaijutsu_kernel::llm::{ContentBlock, MessageContent};
        let (addr, server) = common::start_server_with_mock_llm_kernel_handle().await;
        let client = common::connect_client(addr).await;
        let (kj, _) = client.bind_kernel().await.unwrap();
        let character = common::rc_performer(&kj, "chomosuke").await.unwrap();
        let roots = kj.list_contexts().await.unwrap();
        let root = kaijutsu_client::choose_parent(None, &roots).unwrap().context_id;
        let configured = kj.execute_kj_quiet(root, &["character".into(), "model".into(), "set".into(), character.clone(), "mock/mock-model".into(), "--max-tokens".into(), "4096".into()]).await.unwrap();
        assert_eq!(configured.exit_code, 0, "{}", configured.stderr);
        let audio = kj.execute_kj_quiet(root, &["character".into(), "slot".into(), "set".into(), character.clone(), "audio".into(), "mock/mock-model".into(), "--max-tokens".into(), "4096".into()]).await.unwrap();
        assert_eq!(audio.exit_code, 0, "{}", audio.stderr);
        let result = kj.execute_kj_quiet(root, &kaijutsu_client::context_create_argv("listening-observer", &character, None)).await.unwrap();
        let observer = kaijutsu_client::context_id_from_create_result(&result).unwrap();
        let principal = server.kernel_db.lock().get_context(observer).unwrap().unwrap().performer.unwrap();
        server.kernel_db.lock().set_context_env(observer, "KJ_LISTEN", "listened-score").unwrap();
        kj.join_context(observer, "listening-observer").await.unwrap();
        let player = common::create_context(&kj, "listened-player").await.unwrap();
        let player_id = common::seed_turn_identity(&server, player);
        for command in [vec!["transport", "attach", "--track", "listened-score"], vec!["transport", "ooda", "off", "--track", "listened-score"], vec!["transport", "play", "--track", "listened-score"]] {
            let args = command.into_iter().map(String::from).collect::<Vec<_>>();
            let result = kj.execute_kj_quiet(player, &args).await.unwrap();
            assert_eq!(result.exit_code, 0, "{args:?}: {}", result.stderr);
        }
        let score = server.kernel_db.lock().get_track("listened-score").unwrap().unwrap().score_context_id.unwrap();
        let id = server.kernel.blocks().reserve_block_id(score, player_id).unwrap();
        let snapshot = BlockSnapshotBuilder::new(id, BlockKind::Text).role(Role::Model).content_type(ContentType::Abc)
            .content("X:1\nQ:1/4=120\nK:C\nCDEF|\n").track(TrackId::new("listened-score").unwrap()).tick(Tick(-2)).build();
        server.kernel.blocks().insert_from_snapshot_as(score, snapshot, None, Some(player_id)).unwrap();
        let (callback, mut events) = turn_events_channel(16);
        kj.subscribe_turn_events(callback).await.unwrap();
        let (mock, sent) = MockClient::new("The score opens with four rising notes.").recording_sent_messages();
        server.kernel.llm().write().await.register("mock", Arc::new(Provider::Mock(mock)));
        let topic = format!("{character}/memory/vibe");
        let mut previous = None;
        for turn in 0..2 {
            if turn == 1 {
                let post = kj.execute_kj_quiet(root, &["board".into(), "post".into(), "@track/listened-score".into(), "--title".into(), "handoff".into(), "Play softly.".into()]).await.unwrap();
                assert_eq!(post.exit_code, 0, "{}", post.stderr);
            }
            tick(&server, observer, principal).await;
            tokio::time::timeout(Duration::from_secs(20), async {
                loop {
                    if matches!(events.recv().await.unwrap(), ServerEvent::TurnCompleted { context_id, .. } if context_id == observer) { break; }
                }
                while server.kernel.turn_in_flight(observer) { tokio::time::sleep(Duration::from_millis(10)).await; }
            }).await.unwrap_or_else(|_| panic!("turn {turn} did not complete: {:?}", server.documents.block_snapshots(observer).unwrap()));
            let snapshots = server.documents.block_snapshots(observer).unwrap();
            let included = snapshots.iter().filter(|b| b.role == Role::Asset && !b.excluded).collect::<Vec<_>>();
            assert_eq!(included.len(), 1, "traces: {:?}", snapshots.iter().map(|b| (&b.role, &b.kind, &b.content_type, b.excluded, &b.content)).collect::<Vec<_>>());
            let clip = included[0];
            let hash: kaijutsu_cas::ContentHash = clip.content.parse().unwrap();
            let wav = server.kernel.cas().retrieve(&hash).unwrap().unwrap();
            assert_eq!(&wav[..4], b"RIFF");
            assert_eq!(&wav[8..12], b"WAVE");
            let samples = u32::from_le_bytes(wav[40..44].try_into().unwrap()) as usize / 2;
            assert!(samples > 0 && samples <= 8 * 16000, "{samples} samples");
            assert_eq!(server.kernel.cas().inspect(&hash).unwrap().unwrap().mime_type, "audio/wav");
            if let Some(old) = previous {
                assert_ne!(clip.id, old);
                assert!(snapshots.iter().find(|b| b.id == old).unwrap().excluded);
            }
            previous = Some(clip.id);
            let sent = sent.lock();
            let messages = &sent[turn];
            let prompts = messages.iter().filter(|message| match &message.content {
                MessageContent::Text(text) => text.contains("Chomosuke memory"),
                MessageContent::Blocks(blocks) => blocks.iter().any(|block| matches!(block, ContentBlock::Text { text, .. } if text.contains("Chomosuke memory"))),
            }).count();
            assert_eq!(prompts, 1, "the clip and its current score form one observation on turn {turn}");
            assert!(messages.iter().any(|message| match &message.content {
                MessageContent::Blocks(blocks) => blocks.iter().any(|block| matches!(block,
                    ContentBlock::Media { hash, media_type, data_base64: Some(data) } if hash == &clip.content && media_type == "audio/wav" && !data.is_empty())),
                _ => false,
            }), "turn {turn} did not receive clip {}: {messages:?}", clip.content);
            drop(sent);
            tokio::time::timeout(Duration::from_secs(15), async {
                loop {
                    let read = kj.execute_kj_quiet(root, &["board".into(), "read".into(), topic.clone()]).await.unwrap();
                    assert_eq!(read.exit_code, 0, "{}", read.stderr);
                    if read.stdout.matches("The score opens with four rising notes.").count() > turn { break; }
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            }).await.expect("the turn publishes its observation");
        }
        server.kernel.shutdown_runtime_worker().await.unwrap();
    })).unwrap().join().unwrap();
}
