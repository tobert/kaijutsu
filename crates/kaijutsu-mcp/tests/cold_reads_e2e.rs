//! Slice 1 of `docs/crdt-position-2026-08.md` — the cold readers (prompts,
//! resources, completions) move off `RemoteState.synced` (the event-fed
//! mirror) onto authoritative server RPCs. This suite proves it the only
//! way that actually distinguishes "reads the server" from "reads a cache
//! that usually agrees with the server": kill this process's event bridge
//! FIRST (so the mirror provably can never observe what comes next), author
//! a block straight to the server over a *second*, independent connection,
//! then call the real prompt handlers and assert they see it anyway.
//!
//! If a cold reader silently fell back to (or was still reading)
//! `remote.synced`, these tests would see the block go missing — the mirror
//! never received it and never will, because its event bridge is dead. That
//! is the failure mode this file is built to catch; it was verified to
//! fail by running it against the pre-migration code (which read
//! `with_doc`/`remote.synced` at both these sites) before landing the
//! migration.
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
use kaijutsu_crdt::{PrincipalId, Role};
use kaijutsu_mcp::{AnalyzeDocumentArgs, Backend, KaijutsuMcp, SearchContextArgs};
use kaijutsu_server::{SshServer, SshServerConfig};

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
    for _ in 0..100 {
        let raw = mcp
            .register_session_auto(Some(label.to_string()), None)
            .await;
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&raw) {
            return v;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("register_session_auto never became ready");
}

/// Kill `mcp`'s event bridge and return its joined `context_id`. After this
/// call, `remote.synced` (the mirror) is frozen — nothing authored from now
/// on will ever reach it, exactly reproducing a server-reaped FlowBus
/// subscription (`SubscriberHealth`) without needing to actually starve a
/// live one.
async fn kill_mirror_and_get_context(mcp: &KaijutsuMcp) -> kaijutsu_crdt::ContextId {
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

/// Assert the mirror genuinely does not contain `marker` — the load-bearing
/// sanity check that keeps this test from being vacuous. If this assertion
/// ever fails, the test's premise (mirror provably stale) is broken and the
/// rest of the test proves nothing.
fn assert_mirror_is_stale(mcp: &KaijutsuMcp, marker: &str) {
    let Backend::Remote(remote) = mcp.backend() else {
        panic!("expected Remote backend");
    };
    let mirror_has_it = remote
        .synced
        .lock()
        .as_ref()
        .map(|d| d.blocks().iter().any(|b| b.content.contains(marker)))
        .unwrap_or(false);
    assert!(
        !mirror_has_it,
        "test setup broken: the mirror should never have observed a block authored \
         after its event bridge was killed — if it did, this test proves nothing"
    );
}

/// `search_context` (site 4 of the migration — `context_blocks`, backed by
/// `get_all_blocks`) must find a block that only ever existed on the
/// server, never in this process's mirror.
#[test]
fn search_context_reads_the_server_not_the_stale_mirror() {
    run_local(async {
        let addr = start_server().await;
        let mcp = connect_mcp(addr).await;
        let reg = auto_register_with_retry(&mcp, "cold-read-search-e2e").await;
        assert!(
            reg.get("success").and_then(|v| v.as_bool()).unwrap_or(false),
            "register_session_auto failed: {reg}"
        );

        let context_id = kill_mirror_and_get_context(&mcp).await;

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

        assert_mirror_is_stale(&mcp, marker);

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
            "search_context must find the server-authored block even though the \
             mirror never observed it (event bridge was dead before authoring); \
             got:\n{text}"
        );
    });
}

/// `analyze_document` (sites 3/7 of the migration —
/// `context_blocks_and_version`, backed by `get_context_sync`) must report
/// both the block and the version of a mutation the mirror never observed.
#[test]
fn analyze_document_reads_the_server_not_the_stale_mirror() {
    run_local(async {
        let addr = start_server().await;
        let mcp = connect_mcp(addr).await;
        let reg = auto_register_with_retry(&mcp, "cold-read-analyze-e2e").await;
        assert!(
            reg.get("success").and_then(|v| v.as_bool()).unwrap_or(false),
            "register_session_auto failed: {reg}"
        );

        let context_id = kill_mirror_and_get_context(&mcp).await;

        let Backend::Remote(remote) = mcp.backend().clone() else {
            panic!("expected Remote backend");
        };

        // Baseline version, straight from the server, before the mutation
        // the mirror will never see.
        let before_version = remote
            .actor
            .get_context_sync(context_id)
            .await
            .expect("get_context_sync (before)")
            .version;

        let principal = PrincipalId::for_agent_session("sess-cold-read-analyze");
        let marker = "COLD_READ_PROOF_ANALYZE_5e21db";
        remote
            .actor
            .author_block(AuthorBlock::text(context_id, principal, Role::User, marker))
            .await
            .expect("author_block");

        assert_mirror_is_stale(&mcp, marker);

        let result = mcp
            .analyze_document(Parameters(AnalyzeDocumentArgs {
                document_id: context_id.to_hex(),
                focus: Some("content".to_string()),
            }))
            .await
            .expect("analyze_document must succeed");
        let text = prompt_text(&result);
        assert!(
            text.contains(marker),
            "analyze_document must find the server-authored block even though the \
             mirror never observed it; got:\n{text}"
        );

        // The reported version must reflect the post-mutation server state,
        // not a frozen pre-kill snapshot — a stale mirror would report
        // `before_version` (or panic looking the context up at all, since
        // this process's mirror was never populated past registration).
        let after_version = remote
            .actor
            .get_context_sync(context_id)
            .await
            .expect("get_context_sync (after)")
            .version;
        assert!(
            after_version > before_version,
            "sanity: authoring a block must bump the server's version"
        );
        assert!(
            text.contains(&format!("**Version:** {}", after_version)),
            "analyze_document must report the current server version ({after_version}), \
             not a stale one; got:\n{text}"
        );
    });
}
