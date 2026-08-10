//! `setContextOriginHost` — the advisory "which machine did this context's
//! registering client run on" wire round trip (docs/issues.md "cc-* hook
//! re-registration mints a new context per MCP relaunch", last paragraph:
//! "Host field on registration ... would make the fleet board
//! self-evident").
//!
//! `register_session` (kaijutsu-mcp) calls this once, right after
//! `createContext`, with its own `gethostname()`. These tests exercise the
//! RPC directly (no MCP process involved) to prove the wire → KernelDb →
//! `resolveContextLabel`/`listContexts` path round-trips independent of the
//! kaijutsu-mcp client that will normally drive it.

mod common;
use common::*;

/// The base case: an unset context reports `origin_host: None` (empty on
/// the wire), and a `setContextOriginHost` call is visible on the very next
/// `resolveContextLabel` read — the same DB-driven, registry-independent
/// lookup `register_session`'s upsert decision is built on
/// (`context_label_resolve.rs`).
#[test]
fn set_context_origin_host_round_trips_through_resolve_context_label() {
    run_local(async {
        let addr = start_server().await;
        let client = connect_client(addr).await;
        let (kernel, _kernel_id) = client.bind_kernel().await.unwrap();

        let context_id = kernel.create_context("origin-host-wire-test").await.unwrap();

        // Old-client / not-yet-set behavior: honest absence, not a
        // fabricated empty string vs. real unknown ambiguity.
        let before = kernel
            .resolve_context_label("origin-host-wire-test")
            .await
            .unwrap()
            .expect("just-created label must resolve");
        assert_eq!(
            before.origin_host, None,
            "a context nobody has stamped a host on yet must resolve with origin_host = None"
        );

        kernel.set_context_origin_host(context_id, "zorak").await.unwrap();

        let after = kernel
            .resolve_context_label("origin-host-wire-test")
            .await
            .unwrap()
            .expect("label still resolves after the host is set");
        assert_eq!(
            after.origin_host,
            Some("zorak".to_string()),
            "setContextOriginHost must be visible on the very next DB-driven read"
        );
    });
}

/// `listContexts` (the registry-driven surface `kj context list`/the app's
/// fleet board read) must carry the same fact as `resolveContextLabel` — the
/// two surfaces are fed from the same `ContextRow.origin_host` and must
/// never disagree.
#[test]
fn set_context_origin_host_is_visible_in_list_contexts() {
    run_local(async {
        let addr = start_server().await;
        let client = connect_client(addr).await;
        let (kernel, _kernel_id) = client.bind_kernel().await.unwrap();

        let context_id = kernel.create_context("origin-host-list-test").await.unwrap();
        kernel.set_context_origin_host(context_id, "moltar").await.unwrap();

        let contexts = kernel.list_contexts().await.unwrap();
        let found = contexts
            .iter()
            .find(|c| c.id == context_id)
            .expect("just-created, just-stamped context must appear in listContexts");
        assert_eq!(found.origin_host, Some("moltar".to_string()));
    });
}

/// Clearing: `setContextOriginHost` with an empty string clears a
/// previously-set value back to `None` (the server treats empty text as
/// "unknown", same convention as `label`/`context_type` elsewhere on this
/// interface) — never "" masquerading as a real, empty-string hostname.
#[test]
fn set_context_origin_host_with_empty_string_clears_it() {
    run_local(async {
        let addr = start_server().await;
        let client = connect_client(addr).await;
        let (kernel, _kernel_id) = client.bind_kernel().await.unwrap();

        let context_id = kernel.create_context("origin-host-clear-test").await.unwrap();
        kernel.set_context_origin_host(context_id, "zorak").await.unwrap();
        kernel.set_context_origin_host(context_id, "").await.unwrap();

        let resolved = kernel
            .resolve_context_label("origin-host-clear-test")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(resolved.origin_host, None);
    });
}

/// `setContextOriginHost` against an id that names no context must fail
/// loudly over the wire (`success: false` + a message), not silently
/// succeed — the same contract `setContextPaused`/`archiveContext` already
/// give a caller for an unknown id.
#[test]
fn set_context_origin_host_on_unknown_context_fails() {
    run_local(async {
        let addr = start_server().await;
        let client = connect_client(addr).await;
        let (kernel, _kernel_id) = client.bind_kernel().await.unwrap();

        let bogus = kaijutsu_types::ContextId::new();
        let err = kernel
            .set_context_origin_host(bogus, "zorak")
            .await
            .expect_err("an unknown context id must not silently succeed");
        assert!(
            matches!(err, kaijutsu_client::RpcError::ServerError(_)),
            "got: {err:?}"
        );
    });
}

/// Durability: `origin_host` survives a simulated kernel restart just like
/// every other `ContextRow` column — it's a plain persisted SQLite column,
/// but this pins that fact against a regression the same way
/// `context_label_resolve.rs`'s restart tests pin the registry-heal
/// behavior.
#[test]
fn origin_host_survives_a_kernel_restart() {
    run_local(async {
        let tmp = tempfile::tempdir().unwrap();
        let state_dir = tmp.path().to_path_buf();

        let context_id = {
            let addr = start_server_with_state_dir(state_dir.clone()).await;
            let client = connect_client(addr).await;
            let (kernel, _kernel_id) = client.bind_kernel().await.unwrap();
            let context_id = kernel.create_context("origin-host-restart-test").await.unwrap();
            kernel.set_context_origin_host(context_id, "zorak").await.unwrap();
            context_id
        };

        let addr2 = start_server_with_state_dir(state_dir).await;
        let client2 = connect_client(addr2).await;
        let (kernel2, _kernel_id2) = client2.bind_kernel().await.unwrap();

        let resolved = kernel2
            .resolve_context_label("origin-host-restart-test")
            .await
            .unwrap()
            .expect("row survives the restart — it's durable KernelDb, not registry state");
        assert_eq!(resolved.id, context_id);
        assert_eq!(resolved.origin_host, Some("zorak".to_string()));
    });
}
