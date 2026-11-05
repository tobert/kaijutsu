//! `/config/rc/lib/recipes/pair.kai` composes a senpai and a kohai
//! (`docs/senpai-kohai.md`, "Composing senpai and kohai"): run from a
//! person's root console, it creates a context of the first character
//! answering to the caller, then a context of the second answering to the
//! first.

mod common;

use common::*;
use kaijutsu_server::{SshServer, SshServerConfig};
use kaijutsu_types::Status;

#[test]
fn the_pair_recipe_makes_a_senpai_and_a_kohai_that_answers_to_it() {
    run_local(async {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let config = SshServerConfig::ephemeral(addr.port());
        allow_in_shipped_gate(&config, &["pair", "kj context create", "kj character create"]);
        register_root_key(addr, config.root_key());
        let (tx, rx) = tokio::sync::oneshot::channel();
        tokio::task::spawn_local(async move {
            SshServer::new(config).run_on_listener_with_kernel_sink(listener, tx).await.unwrap();
        });
        let server = rx.await.unwrap();
        let client = connect_client(addr).await;
        let (kernel, _) = client.bind_kernel().await.unwrap();
        let contexts = kernel.list_contexts().await.unwrap();
        let root = kaijutsu_client::choose_parent(None, &contexts).unwrap().context_id;
        kernel.join_context(root, "pair-test").await.unwrap();
        for (name, dir) in [("lead", "banto"), ("hand", "coder")] {
            let (_, out, status) =
                shell_exec_wait_timeout(&kernel, &format!("kj character create {name} --rc {dir}"), root, 30_000).await;
            assert_eq!(status, Status::Done, "kj character create {name}: {out}");
        }

        let (_, out, status) =
            shell_exec_wait_timeout(&kernel, "PATH=\"/config/rc/lib/recipes:$PATH\" pair lead hand", root, 60_000).await;
        assert_eq!(status, Status::Done, "pair lead hand: {out}");

        let db = server.kernel_db.lock();
        let performed_by = |name: &str| {
            let character = db.get_character_by_name(name).unwrap().unwrap().principal_id;
            let live: Vec<_> = db.list_active_contexts().unwrap().into_iter()
                .filter(|row| row.performer == Some(character) && !row.is_archived()).collect();
            assert_eq!(live.len(), 1, "{name} performs one context: {live:?}");
            live.into_iter().next().unwrap()
        };
        let lead = performed_by("lead");
        let hand = performed_by("hand");
        assert_eq!(lead.senpai_ctx, Some(root), "the senpai answers to the caller");
        assert_eq!(hand.senpai_ctx, Some(lead.context_id), "the kohai answers to the senpai");
        assert!(out.contains(&lead.context_id.to_hex()) && out.contains(&hand.context_id.to_hex()), "both are printed: {out}");
    });
}
