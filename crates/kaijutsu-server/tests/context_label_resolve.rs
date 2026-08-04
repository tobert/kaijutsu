//! `resolveContextLabel` (DB-driven label lookup) and the `joinContext`
//! registry-heal it exists to support — server-side halves of the
//! `register_session` upsert/attach fix (docs/issues.md, "register_session
//! hard-fails on label conflict").
//!
//! `join_context_heals_registry_for_an_archived_context_after_restart` below
//! also narrows a claim in docs/issues.md ("MCP-created context invisible to
//! `kj context list` after kernel restart"): boot-time recovery
//! (`create_shared_kernel`'s "Recover contexts" step, `rpc.rs`) already
//! re-registers every NON-ARCHIVED context into the DriftRouter on every
//! boot via `KernelDb::list_active_contexts` (`WHERE archived_at IS NULL`) —
//! confirmed by `list_contexts_recovers_live_context_after_restart` here. So
//! a live or concluded context surviving a restart is NOT actually
//! registry-invisible today; that part of the filed issue does not
//! reproduce against current code and looks stale (see the docs update in
//! this same change for the correction). The one context state boot recovery
//! genuinely skips is `archived_at IS NOT NULL` — an archived context's
//! DriftRouter entry really does not survive a restart, and that's the real,
//! narrow gap `joinContext`'s heal (added alongside this fix) closes.

mod common;
use common::*;

/// `resolveContextLabel` must find a context by label straight from
/// `KernelDb`, independent of whatever `listContexts`' registry currently
/// holds — this is the lookup `register_session`'s upsert decision is built
/// on.
#[test]
fn resolve_context_label_is_db_driven_not_registry_driven() {
    run_local(async {
        let addr = start_server().await;
        let client = connect_client(addr).await;
        let (kernel, _kernel_id) = client.bind_kernel().await.unwrap();

        // No context holds this label yet.
        let miss = kernel.resolve_context_label("resolve-label-miss").await.unwrap();
        assert!(miss.is_none(), "an unused label must resolve to None");

        let context_id = kernel.create_context("resolve-label-hit").await.unwrap();
        let hit = kernel
            .resolve_context_label("resolve-label-hit")
            .await
            .unwrap()
            .expect("just-created label must resolve");
        assert_eq!(hit.id, context_id);
        assert_eq!(hit.label, "resolve-label-hit");
        assert!(hit.concluded_at.is_none(), "fresh context is not concluded");
        assert!(!hit.archived, "fresh context is not archived");
    });
}

/// The concluded/archived half of the upsert decision: `resolveContextLabel`
/// must report `concluded_at`/`archived` honestly so the MCP layer knows not
/// to resurrect the context and must mint a suffixed label instead.
#[test]
fn resolve_context_label_reports_concluded_state() {
    run_local(async {
        let addr = start_server().await;
        let client = connect_client(addr).await;
        let (kernel, _kernel_id) = client.bind_kernel().await.unwrap();

        let context_id = kernel.create_context("resolve-label-concluded").await.unwrap();
        kernel.conclude(context_id).await.unwrap();

        let row = kernel
            .resolve_context_label("resolve-label-concluded")
            .await
            .unwrap()
            .expect("concluded context's label row must still resolve — it is not resurrected \
                     silently, but the caller needs to SEE it to decide not to reuse it");
        assert_eq!(row.id, context_id);
        assert!(
            row.concluded_at.is_some(),
            "resolve_context_label must surface concluded_at so register_session \
             knows not to attach"
        );
    });
}

/// Start a second `SshServer` on a fresh ephemeral port, pointed at the same
/// on-disk `state_dir` as an existing one. The prior server is left running
/// but is never contacted again — functionally equivalent to a kernel
/// restart from the new server's perspective, since `create_shared_kernel`
/// builds every in-memory structure (the DriftRouter chief among them) from
/// scratch on every call, while KernelDb/BlockStore reopen the same durable
/// files.
async fn simulate_kernel_restart(state_dir: std::path::PathBuf) -> std::net::SocketAddr {
    start_server_with_state_dir(state_dir).await
}

/// Establishes the baseline this whole file leans on: an ordinary (never
/// archived) context survives a simulated restart WITHOUT needing
/// `joinContext`'s heal at all, because boot recovery already re-registered
/// it. Written first so a future change to the recovery step that breaks
/// this gets caught right here, not misattributed to the heal logic below.
#[test]
fn list_contexts_recovers_live_context_after_restart() {
    run_local(async {
        let tmp = tempfile::tempdir().unwrap();
        let state_dir = tmp.path().to_path_buf();

        let (context_id, label) = {
            let addr = start_server_with_state_dir(state_dir.clone()).await;
            let client = connect_client(addr).await;
            let (kernel, _kernel_id) = client.bind_kernel().await.unwrap();
            let label = "restart-recovery-test".to_string();
            let context_id = kernel.create_context(&label).await.unwrap();
            (context_id, label)
        };

        let addr2 = simulate_kernel_restart(state_dir).await;
        let client2 = connect_client(addr2).await;
        let (kernel2, _kernel_id2) = client2.bind_kernel().await.unwrap();

        let after_restart = kernel2.list_contexts().await.unwrap();
        let found = after_restart.iter().find(|c| c.id == context_id).expect(
            "a non-archived context must already be visible in listContexts right after a \
             restart, via create_shared_kernel's boot-time KernelDb recovery — no joinContext \
             call needed",
        );
        assert_eq!(found.label, label);
    });
}

/// The real registry gap: `KernelDb::list_active_contexts` (the query boot
/// recovery walks) filters `WHERE archived_at IS NULL`, so an archived
/// context's DriftRouter entry does NOT survive a restart — unlike the live/
/// concluded case above. `joinContext` must heal it from the durable
/// KernelDb row instead of hard-failing with "use createContext first", and
/// the heal must make it visible to `listContexts` again too (the `kj
/// context list` data source).
#[test]
fn join_context_heals_registry_for_an_archived_context_after_restart() {
    run_local(async {
        let tmp = tempfile::tempdir().unwrap();
        let state_dir = tmp.path().to_path_buf();

        let (context_id, label) = {
            let addr = start_server_with_state_dir(state_dir.clone()).await;
            let client = connect_client(addr).await;
            let (kernel, _kernel_id) = client.bind_kernel().await.unwrap();
            let label = "restart-heal-archived-test".to_string();
            let context_id = kernel.create_context(&label).await.unwrap();
            kernel
                .join_context(context_id, "pre-restart-instance")
                .await
                .unwrap();
            kernel.archive_context(context_id).await.unwrap();
            (context_id, label)
        };

        let addr2 = simulate_kernel_restart(state_dir).await;
        let client2 = connect_client(addr2).await;
        let (kernel2, _kernel_id2) = client2.bind_kernel().await.unwrap();

        // The row is durable — DB-driven resolution finds it straight away,
        // archived state and all.
        let resolved = kernel2
            .resolve_context_label(&label)
            .await
            .unwrap()
            .expect("an archived context's row must still resolve by label via KernelDb");
        assert_eq!(resolved.id, context_id);
        assert!(resolved.archived, "resolve_context_label must report archived honestly");

        // Unlike the non-archived case, the fresh DriftRouter genuinely has
        // never heard of this one: boot recovery's `list_active_contexts`
        // excludes archived rows.
        let before_join = kernel2.list_contexts().await.unwrap();
        assert!(
            !before_join.iter().any(|c| c.id == context_id),
            "an archived context must be missing from the registry-driven listContexts \
             right after a restart — if this fails, boot recovery started covering \
             archived contexts too and this test (and joinContext's heal) can be dropped"
        );

        // Attach: joinContext must heal the registry rather than failing with
        // "use createContext first".
        let joined = kernel2
            .join_context(context_id, "post-restart-instance")
            .await
            .unwrap_or_else(|e| {
                panic!(
                    "join_context must heal the registry from the durable KernelDb row \
                     instead of hard-failing on an archived context after a restart, got: {e}"
                )
            });
        assert_eq!(joined, context_id);

        // And now `kj context list`'s own data source shows it again.
        let after_join = kernel2.list_contexts().await.unwrap();
        let found = after_join
            .iter()
            .find(|c| c.id == context_id)
            .expect(
                "join_context's registry heal must make the context visible to \
                 listContexts again",
            );
        assert_eq!(found.label, label);
    });
}
