//! e2e: the approval-ledger change-notification **wire surface** —
//! `LedgerEvents::onChanged` and the `subscribeLedgerEvents` push channel
//! (`kaijutsu-server`'s `rpc.rs::subscribe_ledger_events`).
//!
//! Unlike `turn_events_wire.rs` / `permission_ask_wire.rs`, this does NOT
//! drive a genuine ledger change through the approval-ledger's rule /
//! escalation machinery — that lives in `kaijutsu-kernel`'s `kj::gate`
//! module, out of this crate's territory, and reaching it deterministically
//! from a test would mean wiring up rules and a gated call just to prove a
//! bridge task forwards bus events onto a capnp callback. Instead this
//! publishes `LedgerFlow::Changed` directly onto the live kernel's own
//! `ledger_flows()` bus (reached via `common::start_server_with_kernel_handle`,
//! which hands back the exact `SharedKernel` instance the connected client is
//! talking to) — sanctioned by the task brief as an acceptable way to test
//! the bridge itself, which IS this crate's territory. The kernel-local
//! publish-side wiring (`announce_ledger_change`, `kj::gate::run_gate`) is
//! someone else's tests to write.
//!
//! Two properties matter here:
//!
//!   * a subscriber receives a notification after the bus is published to;
//!   * a burst of several changes coalesces into fewer notifications than
//!     changes, and the one notification that does arrive carries the
//!     HIGHEST generation of the burst — not the first, not a count.

mod common;

use std::time::Duration;

use common::{connect_client, run_local, start_server_with_kernel_handle};
use kaijutsu_client::ledger_events_channel;
use tokio::sync::broadcast::Receiver;

/// Drain the ledger push channel for one notification, with a hard timeout —
/// a missing push is exactly the bug this test exists to catch.
async fn recv_generation(rx: &mut Receiver<i64>) -> i64 {
    match tokio::time::timeout(Duration::from_secs(10), rx.recv()).await {
        Ok(Ok(generation)) => generation,
        Ok(Err(e)) => panic!("ledger push channel error: {e}"),
        Err(_) => panic!("timed out waiting for a ledger-changed push"),
    }
}

/// The headline: publish `LedgerFlow::Changed` on the kernel's bus, and a
/// subscribed client receives `onChanged` naming that generation.
#[test]
fn subscriber_receives_notification_after_ledger_changes() {
    run_local(async {
        let (addr, kernel) = start_server_with_kernel_handle().await;
        let client = connect_client(addr).await;
        let (kernel_handle, _kernel_id) = client.bind_kernel().await.unwrap();

        let (callback, mut rx) = ledger_events_channel(16);
        kernel_handle
            .subscribe_ledger_events(callback)
            .await
            .expect("subscribeLedgerEvents");

        kernel.kernel.ledger_flows().publish(
            kaijutsu_kernel::flows::LedgerFlow::Changed { generation: 42 },
        );

        assert_eq!(
            recv_generation(&mut rx).await,
            42,
            "the pushed generation must match what was published, not a stale \
             or fabricated value"
        );
    });
}

/// A burst of several changes, published back-to-back (well inside the
/// server's coalescing window), collapses into FEWER notifications than
/// changes — and the notification that does arrive carries the HIGHEST
/// generation of the burst, never the first and never a count.
///
/// This is the property most likely to silently regress: a bridge that
/// forgot to coalesce would still "work" (every generation eventually
/// arrives, just as N separate pushes), so a bare `rx.recv()` loop wouldn't
/// catch it. Pinning `notifications.len() < 5` AND the exact max value is
/// what would actually fail if coalescing broke.
#[test]
fn a_burst_of_changes_coalesces_to_the_highest_generation() {
    run_local(async {
        let (addr, kernel) = start_server_with_kernel_handle().await;
        let client = connect_client(addr).await;
        let (kernel_handle, _kernel_id) = client.bind_kernel().await.unwrap();

        let (callback, mut rx) = ledger_events_channel(16);
        kernel_handle
            .subscribe_ledger_events(callback)
            .await
            .expect("subscribeLedgerEvents");

        // Publish 5 changes with no await between them, so all 5 land well
        // inside the server's coalescing window (the change feed's
        // FEED_BATCH_WINDOW, 4ms) before the bridge task gets a chance to
        // drain and send.
        for generation in 1..=5i64 {
            kernel.kernel.ledger_flows().publish(
                kaijutsu_kernel::flows::LedgerFlow::Changed { generation },
            );
        }

        // Collect whatever arrives within a window generous enough that the
        // coalescing window has long closed and any (wrongly) un-coalesced
        // sends have all landed.
        let mut notifications = Vec::new();
        let collect_until = tokio::time::Instant::now() + Duration::from_millis(500);
        loop {
            let remaining = collect_until.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                break;
            }
            match tokio::time::timeout(remaining, rx.recv()).await {
                Ok(Ok(generation)) => notifications.push(generation),
                Ok(Err(e)) => panic!("ledger push channel error: {e}"),
                Err(_) => break,
            }
        }

        assert!(
            !notifications.is_empty(),
            "the burst produced zero notifications — the bridge dropped every one"
        );
        assert!(
            notifications.len() < 5,
            "5 changes published back-to-back must coalesce into fewer than 5 \
             notifications; got {notifications:?}"
        );
        assert_eq!(
            *notifications.iter().max().unwrap(),
            5,
            "coalescing is lossless for this payload — the highest generation \
             in the burst (5) must be the one that reaches the wire, regardless \
             of how many notifications it took; got {notifications:?}"
        );
    });
}
