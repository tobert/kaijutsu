//! Context lifecycle RPCs keep the drift router in step with the kernel DB,
//! even while another thread is reading the router.

mod common;
use common::*;

/// Hold a read guard on the drift router from another thread for a short
/// while, returning once the guard is held.
fn hold_router_read(kernel: &kaijutsu_server::SharedKernel) -> std::thread::JoinHandle<()> {
    let drift = kernel.kernel.drift().clone();
    let (held_tx, held_rx) = std::sync::mpsc::channel();
    let holder = std::thread::spawn(move || {
        let _guard = drift.read();
        held_tx.send(()).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(200));
    });
    held_rx.recv().unwrap();
    holder
}

/// A rename while another thread reads the router still updates the
/// router's label, so label resolution through the router agrees with the
/// DB.
#[test]
fn rename_updates_the_router_while_another_thread_reads_it() {
    run_local(async {
        let (addr, server) = start_server_with_kernel_handle().await;
        let client = connect_client(addr).await;
        let (kj, _) = client.bind_kernel().await.unwrap();
        let context = create_context(&kj, "router-sync-before").await.unwrap();

        let holder = hold_router_read(&server);
        kj.rename_context(context, "router-sync-after").await.unwrap();
        holder.join().unwrap();

        let label = server.kernel.drift().read().get(context).and_then(|h| h.label.clone());
        assert_eq!(label.as_deref(), Some("router-sync-after"));
        server.kernel.shutdown_runtime_worker().await.unwrap();
    });
}

/// A conclude while another thread reads the router succeeds and leaves the
/// router showing the context concluded.
#[test]
fn conclude_updates_the_router_while_another_thread_reads_it() {
    run_local(async {
        let (addr, server) = start_server_with_kernel_handle().await;
        let client = connect_client(addr).await;
        let (kj, _) = client.bind_kernel().await.unwrap();
        let context = create_context(&kj, "router-sync-conclude").await.unwrap();

        let holder = hold_router_read(&server);
        context_verb(&kj, "conclude", context, &[]).await;
        holder.join().unwrap();

        let state = server.kernel.drift().read().get(context).map(|h| h.state);
        assert_eq!(state, Some(kaijutsu_types::ContextState::Concluded));
        server.kernel.shutdown_runtime_worker().await.unwrap();
    });
}
