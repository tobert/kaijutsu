//! e2e: the permission-ask **wire surface** — `HookAction::Ask` +
//! `PermissionEvents::onAsk` (D-57, docs/acp.md gap #2).
//!
//! Drives a real SSH + Cap'n Proto round-trip end to end, no GUI. The
//! kernel-local hook-engine semantics (allow, deny, no-subscriber,
//! timeout) are already covered headless in `kaijutsu-kernel`'s
//! `mcp::broker::tests::permission_ask_*` — what only the wire can prove is
//! the join: that a `subscribePermissionEvents` callback on one connection
//! really does get called across the `!Send` capnp boundary
//! (`PermissionAskBridge` -> per-connection `spawn_local` bridge task ->
//! `onAsk`) when `HookAction::Ask` fires from a `callMcpTool` on that same
//! connection, and that the answer makes it all the way back to gate the
//! call.
//!
//! Uses `whoami` (`KernelInfoServer`) as the hooked tool — no LLM
//! configured, no external state, deterministic shape, and already the
//! `call_mcp_tool` e2e target in `rpc_integration.rs`.

mod common;

use common::{connect_client, run_local, start_server};
use kaijutsu_client::{KernelHandle, PermissionAskAnswer, permission_events_channel};
use kaijutsu_types::ContextId;

/// Join a fresh context and install a `pre_call` `Ask` hook on `whoami`.
/// Returns the context id (not otherwise needed by these tests, but handy
/// for future context-scoped assertions).
async fn setup_context_with_ask_hook(kernel: &KernelHandle, hook_id: &str) -> ContextId {
    let ctx_id = kernel.create_context("permission-ask-test").await.unwrap();
    kernel.join_context(ctx_id, "test").await.unwrap();

    let add = kernel
        .call_mcp_tool(
            "hook_add",
            &serde_json::json!({
                "phase": "pre_call",
                "match_tool": "whoami",
                "hook_id": hook_id,
                "action": {
                    "type": "ask",
                    "description": "about to reveal who is calling",
                },
            }),
        )
        .await
        .expect("hook_add over the wire");
    assert!(!add.is_error, "hook_add should not itself be an error: {add:?}");
    ctx_id
}

/// Subscribe to permission asks, then spawn a task that answers the FIRST
/// ask it sees with `answer` and stops (good enough for these single-ask
/// tests — a test that needs to answer more than once should drain the
/// receiver itself instead of reusing this helper).
async fn subscribe_and_answer_once(kernel: &KernelHandle, answer: PermissionAskAnswer) {
    let (callback, mut rx) = permission_events_channel(16);
    kernel
        .subscribe_permission_events(callback)
        .await
        .expect("subscribePermissionEvents");
    tokio::task::spawn_local(async move {
        if let Some(envelope) = rx.recv().await {
            let _ = envelope.answer.send(answer);
        }
    });
}

/// A subscriber that answers "allow" lets the hooked call through — the
/// headline proof that the whole cross-thread bridge (kernel-wide registry
/// -> per-connection `spawn_local` task -> capnp `onAsk` -> back) actually
/// wires up and doesn't deadlock.
#[test]
fn allow_lets_the_call_through() {
    run_local(async {
        let addr = start_server().await;
        let client = connect_client(addr).await;
        let (kernel, _kernel_id) = client.bind_kernel().await.unwrap();

        subscribe_and_answer_once(
            &kernel,
            PermissionAskAnswer {
                allow: true,
                selected_option_id: None,
                remember: None,
            },
        )
        .await;
        setup_context_with_ask_hook(&kernel, "allow-hook").await;

        let result = kernel
            .call_mcp_tool("whoami", &serde_json::json!({}))
            .await
            .expect("whoami should succeed once the ask is answered 'allow'");
        assert!(!result.is_error, "whoami should not be an error: {result:?}");
    });
}

/// A subscriber that answers "deny" blocks the call as `McpError::Denied`
/// — same wire-visible failure shape as `HookAction::Deny`.
#[test]
fn deny_blocks_the_call() {
    run_local(async {
        let addr = start_server().await;
        let client = connect_client(addr).await;
        let (kernel, _kernel_id) = client.bind_kernel().await.unwrap();

        subscribe_and_answer_once(
            &kernel,
            PermissionAskAnswer {
                allow: false,
                selected_option_id: None,
                remember: Some("session".into()),
            },
        )
        .await;
        setup_context_with_ask_hook(&kernel, "deny-hook").await;

        let err = kernel
            .call_mcp_tool("whoami", &serde_json::json!({}))
            .await
            .expect_err("whoami must fail once the ask is answered 'deny'");
        assert!(
            err.to_string().to_lowercase().contains("den"),
            "expected a Denied-shaped error, got: {err}"
        );
    });
}

/// D-57's fail-closed default: no `PermissionEvents` subscriber attached
/// at all denies the call rather than silently letting it through — same
/// assertion `kaijutsu-kernel`'s `permission_ask_no_subscriber_denies`
/// makes headless, reproduced here over the real wire so a regression that
/// only shows up in the server-side bridge (e.g. the registry never gets
/// wired onto the broker at bootstrap) cannot hide behind the headless
/// test's fake `PermissionAsker`.
#[test]
fn no_subscriber_denies_over_the_wire() {
    run_local(async {
        let addr = start_server().await;
        let client = connect_client(addr).await;
        let (kernel, _kernel_id) = client.bind_kernel().await.unwrap();

        // Deliberately no subscribe_permission_events call.
        setup_context_with_ask_hook(&kernel, "no-sub-hook").await;

        let err = kernel
            .call_mcp_tool("whoami", &serde_json::json!({}))
            .await
            .expect_err("whoami must fail closed with nobody to ask");
        assert!(
            err.to_string().to_lowercase().contains("den"),
            "expected a Denied-shaped error, got: {err}"
        );
    });
}
