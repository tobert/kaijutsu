//! Slice 1 of `docs/crdt-position-2026-08.md` — the cold readers (prompts,
//! resources, completions) moved off `RemoteState.synced` (the event-fed
//! mirror that existed at the time) onto authoritative server RPCs. This
//! suite was written to prove it the only way that actually distinguishes
//! "reads the server" from "reads a cache that usually agrees with the
//! server": kill this process's event listener FIRST (so a mirror, if one
//! existed, could never observe what comes next), author a block straight
//! to the server over a *second*, independent connection, then call the
//! real prompt handlers and assert they see it anyway. It was verified to
//! fail against the pre-migration code (which read `with_doc`/
//! `remote.synced` at both these sites) before that migration landed.
//!
//! **The mirror itself is gone now** (slice 4 of the same doc deleted
//! `RemoteState.synced` entirely — there is no client-side replica left in
//! this crate to regress to). So these tests can no longer catch "silently
//! fell back to the mirror" — that failure mode no longer has a mechanism to
//! fail through. What they guard today: the end-to-end read surface
//! (`search_context`, `analyze_document`) against a real server over a real
//! SSH connection, which this crate otherwise has no coverage for, and —
//! because the event listener is killed before the read — that reading still
//! works with the listener dead, so nobody can reintroduce a client-side
//! cache and have these tests stay green while it silently disagrees with
//! the server.
//!
//! Mirrors the ephemeral-SSH-server harness in `tests/hook_remote_e2e.rs`
//! and `tests/e2e_shell.rs` (each file duplicates it rather than sharing —
//! existing convention in this crate, see those files' own harness blocks).

use std::net::SocketAddr;
use std::time::Duration;

use rmcp::handler::server::wrapper::Parameters;
use tokio::net::TcpListener;
use tokio::task::LocalSet;

use kaijutsu_client::{AuthorBlock, KeySource, SshConfig};
use kaijutsu_types::{PrincipalId, Role};
use kaijutsu_mcp::{AnalyzeDocumentArgs, Backend, KaijutsuMcp, SearchContextArgs};
use kaijutsu_server::{SshServer, SshServerConfig};
use kaijutsu_types::BlockQuery;

/// capnp-rpc requires a current-thread runtime with a LocalSet.
fn run_local<F: std::future::Future<Output = ()>>(f: F) {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let local = LocalSet::new();
    rt.block_on(local.run_until(f));
}

/// Start an ephemeral SSH server on a random port; return its address.
async fn start_server() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let config = SshServerConfig::ephemeral(addr.port());

    tokio::task::spawn_local(async move {
        let server = SshServer::new(config);
        if let Err(e) = server.run_on_listener(listener).await {
            log::error!("Server error: {}", e);
        }
    });

    tokio::task::yield_now().await;
    addr
}

/// Connect a `KaijutsuMcp` to the ephemeral server.
async fn connect_mcp(addr: SocketAddr) -> KaijutsuMcp {
    let config = SshConfig {
        host: addr.ip().to_string(),
        port: addr.port(),
        username: "test_user".to_string(),
        key_source: KeySource::ephemeral(),
        insecure: true,
    };
    KaijutsuMcp::connect_with_config(config, "cold-reads-e2e-test", None)
        .await
        .expect("MCP connect failed")
}

/// Auto-register with retry — the freshly-spawned actor needs a moment to
/// finish connecting before RPCs succeed.
async fn auto_register_with_retry(mcp: &KaijutsuMcp, label: &str) -> serde_json::Value {
    let mut last = String::new();
    for _ in 0..100 {
        let raw = mcp
            .register_session_auto(Some(label.to_string()), None)
            .await;
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&raw) {
            return v;
        }
        last = raw;
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    // Carry the last response into the failure: a bare "never became ready"
    // hides whether the actor was slow, refused, or errored — and this test
    // spends five seconds proving nothing if it cannot say which.
    panic!("register_session_auto never became ready; last response: {last}");
}

/// Kill `mcp`'s event listener (the pulse task) and return its joined
/// `context_id`. After this call, nothing authored from now on will ever
/// reach this process through the event feed — exactly reproducing a
/// server-reaped FlowBus subscription (`SubscriberHealth`) without needing
/// to actually starve a live one. There is no mirror left for this to
/// freeze; what it proves is that the cold readers below still see the
/// block, because they read the server directly rather than depending on
/// event delivery at all.
async fn kill_event_listener_and_get_context(mcp: &KaijutsuMcp) -> kaijutsu_types::ContextId {
    let Backend::Remote(remote) = mcp.backend() else {
        panic!("expected Remote backend");
    };
    let guard = remote.joined.read().await;
    let joined = guard.as_ref().expect("register_session must have joined a context");
    joined.debug_kill_event_listener();
    joined.context_id
}

/// Extract the concatenated text of every message in a `GetPromptResult` —
/// prompt content always arrives as `ContentBlock::Text`.
fn prompt_text(result: &rmcp::model::GetPromptResult) -> String {
    result
        .messages
        .iter()
        .map(|m| match &m.content {
            rmcp::model::ContentBlock::Text(t) => t.text.as_str(),
            _ => "",
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// `search_context` (site 4 of the migration — `context_blocks`, backed by
/// `get_all_blocks`) must find a block that only ever existed on the
/// server, authored after this process's event listener was killed.
#[test]
fn search_context_reads_the_server_with_the_event_listener_dead() {
    run_local(async {
        let addr = start_server().await;
        let mcp = connect_mcp(addr).await;
        let reg = auto_register_with_retry(&mcp, "cold-read-search-e2e").await;
        assert!(
            reg.get("success").and_then(|v| v.as_bool()).unwrap_or(false),
            "register_session_auto failed: {reg}"
        );

        let context_id = kill_event_listener_and_get_context(&mcp).await;

        let Backend::Remote(remote) = mcp.backend().clone() else {
            panic!("expected Remote backend");
        };
        let principal = PrincipalId::for_agent_session("sess-cold-read-search");
        let marker = "COLD_READ_PROOF_SEARCH_7f3a9c";
        remote
            .actor
            .author_block(AuthorBlock::text(context_id, principal, Role::User, marker))
            .await
            .expect("author_block");

        let result = mcp
            .search_context(Parameters(SearchContextArgs {
                query: marker.to_string(),
                document_id: None,
            }))
            .await
            .expect("search_context must succeed");
        let text = prompt_text(&result);
        // NOT `text.contains(marker)` — the "# Search Results for: `<query>`"
        // header echoes the query verbatim regardless of whether anything
        // matched, which would make that check pass even on zero results
        // (caught by deliberately breaking `context_blocks` during this
        // test's own development — see this file's module doc). Assert on
        // the rendered match line instead, which only appears for an actual
        // hit, and rule out the "no matches" branch explicitly.
        assert!(
            !text.contains("No matches found"),
            "search_context found no matches at all — got:\n{text}"
        );
        assert!(
            text.contains(&format!(">>> {marker} <<<")),
            "search_context must find the server-authored block even though this \
             process's event listener was dead before authoring; got:\n{text}"
        );
    });
}

/// `analyze_document` (sites 3/7 of the migration —
/// `context_blocks_and_version`, now backed by `context_blocks`
/// (`get_all_blocks`) plus the projected `get_context_version` RPC, with no
/// oplog decode anywhere in the Remote arm) must report both the blocks, in
/// document order, and the version of mutations this process's event
/// listener never observed.
#[test]
fn analyze_document_reads_the_server_with_the_event_listener_dead() {
    run_local(async {
        let addr = start_server().await;
        let mcp = connect_mcp(addr).await;
        let reg = auto_register_with_retry(&mcp, "cold-read-analyze-e2e").await;
        assert!(
            reg.get("success").and_then(|v| v.as_bool()).unwrap_or(false),
            "register_session_auto failed: {reg}"
        );

        let context_id = kill_event_listener_and_get_context(&mcp).await;

        let Backend::Remote(remote) = mcp.backend().clone() else {
            panic!("expected Remote backend");
        };

        // Baseline version, straight from the server, before the mutations
        // this process's dead event listener will never see. Read via both
        // RPCs the projected `get_context_version` is meant to agree with.
        // `get_context_sync` (the storage-engine-oplog RPC this cross-check used
        // to read against) was deleted in the 2026-08-15 wire flag day
        // (docs/change-feed.md); `get_blocks_versioned` reads the same
        // single-guard version, without an oplog decode anywhere.
        let (_, before_sync_version) = remote
            .actor
            .get_blocks_versioned(context_id, BlockQuery::All)
            .await
            .expect("get_blocks_versioned (before)");
        let before_version = remote
            .actor
            .get_context_version(context_id)
            .await
            .expect("get_context_version (before)");
        assert_eq!(
            before_version, before_sync_version,
            "get_context_version must agree with get_blocks_versioned's version"
        );

        let principal = PrincipalId::for_agent_session("sess-cold-read-analyze");
        // Two markers, authored in this order, so the rendered content can
        // be checked for document order — not just presence — the same
        // property `context_blocks` (site 4) already covers for
        // `search_context`; this proves `context_blocks_and_version`'s
        // Remote arm still returns blocks in that order after routing
        // through the projected version RPC instead of a decoded
        // `SyncedDocument`.
        let marker_a = "COLD_READ_PROOF_ANALYZE_A_5e21db";
        let marker_b = "COLD_READ_PROOF_ANALYZE_B_9c40f1";
        remote
            .actor
            .author_block(AuthorBlock::text(context_id, principal, Role::User, marker_a))
            .await
            .expect("author_block (marker_a)");
        remote
            .actor
            .author_block(AuthorBlock::text(context_id, principal, Role::User, marker_b))
            .await
            .expect("author_block (marker_b)");

        let result = mcp
            .analyze_document(Parameters(AnalyzeDocumentArgs {
                document_id: context_id.to_hex(),
                focus: Some("content".to_string()),
            }))
            .await
            .expect("analyze_document must succeed");
        let text = prompt_text(&result);
        assert!(
            text.contains(marker_a) && text.contains(marker_b),
            "analyze_document must find both server-authored blocks even though this \
             process's event listener never observed them; got:\n{text}"
        );
        let pos_a = text.find(marker_a).expect("marker_a present (checked above)");
        let pos_b = text.find(marker_b).expect("marker_b present (checked above)");
        assert!(
            pos_a < pos_b,
            "analyze_document must report blocks in document order (marker_a authored \
             before marker_b); got:\n{text}"
        );

        // The reported version must reflect the post-mutation server state,
        // not a frozen pre-kill snapshot, and both version RPCs must still
        // agree after the mutations.
        let (_, after_sync_version) = remote
            .actor
            .get_blocks_versioned(context_id, BlockQuery::All)
            .await
            .expect("get_blocks_versioned (after)");
        let after_version = remote
            .actor
            .get_context_version(context_id)
            .await
            .expect("get_context_version (after)");
        assert_eq!(
            after_version, after_sync_version,
            "get_context_version must still agree with get_blocks_versioned after mutations"
        );
        assert!(
            after_version > before_version,
            "sanity: authoring blocks must bump the server's version"
        );
        assert!(
            text.contains(&format!("**Version:** {}", after_version)),
            "analyze_document must report the current server version ({after_version}), \
             not a stale one; got:\n{text}"
        );
    });
}
