//! e2e: **the lag kick** — a client that cannot drain its event queue is told
//! so and disconnected, rather than silently handed a stream with a hole in it.
//!
//! This is the wire half of Amy's 2026-08-05 call: "when a client's queue fills,
//! it is terminated so it doesn't block others or get an inconsistent view…
//! no lossy solutions. I'd rather be disconnected."
//!
//! The victim connection subscribes with a callback that NEVER answers — the
//! honest model of a wedged client, and exactly the condition that used to make
//! the kernel quietly evict the oldest event for everyone. A second, healthy
//! connection drives the burst, so the load is not riding the socket we expect
//! to die.
//!
//! Two client-visible assertions, in order of importance:
//!   1. the connection DIES — which is what makes the actor's existing
//!      reconnect-with-full-resync path engage, for every client version;
//!   2. it is told WHY first, via `onSubscriptionTerminated` with
//!      `slowSubscriber` — so a UI can say "you fell behind" instead of
//!      guessing.
//!
//! It runs in its own test binary because it shrinks the kernel's queue depth
//! process-wide via `KAIJUTSU_FLOW_QUEUE_DEPTH`; filling the real 8192-deep
//! queue would need a burst no unit test should be generating.

mod common;

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

use capnp::capability::Promise;
use common::{connect_client, run_local, start_server};
use kaijutsu_client::kaijutsu_capnp::block_events;

const RC_PATH: &str = "/etc/rc/coder/create/S00-stance.kai";

/// Shrunk so a modest burst can overflow it. The production default is 8192
/// (see `kaijutsu_kernel::flows::DEFAULT_ORDERED_QUEUE_DEPTH`).
const TEST_QUEUE_DEPTH: &str = "24";

/// Comfortably more than `TEST_QUEUE_DEPTH`, so the queue must overflow.
const BURST_KEYS: usize = 120;

#[derive(Default)]
struct Kick {
    reason: Option<String>,
    topic: String,
    delivered: u64,
    capacity: u64,
}

/// A client that accepts the kick and answers nothing else — ever.
struct WedgedClient {
    kick: Rc<RefCell<Kick>>,
}

#[allow(refining_impl_trait)]
impl block_events::Server for WedgedClient {
    /// Never resolves. The server's per-callback timeout eventually gives up on
    /// each one, but events keep piling into our queue in the meantime — which
    /// is the whole point.
    fn on_block_text_ops(
        self: Rc<Self>,
        _params: block_events::OnBlockTextOpsParams,
        _results: block_events::OnBlockTextOpsResults,
    ) -> Promise<(), capnp::Error> {
        Promise::from_future(std::future::pending())
    }

    fn on_block_inserted(
        self: Rc<Self>,
        _params: block_events::OnBlockInsertedParams,
        _results: block_events::OnBlockInsertedResults,
    ) -> Promise<(), capnp::Error> {
        Promise::from_future(std::future::pending())
    }

    fn on_block_status_changed(
        self: Rc<Self>,
        _params: block_events::OnBlockStatusChangedParams,
        _results: block_events::OnBlockStatusChangedResults,
    ) -> Promise<(), capnp::Error> {
        Promise::from_future(std::future::pending())
    }

    /// The one thing we DO answer: being told we were cut off.
    fn on_subscription_terminated(
        self: Rc<Self>,
        params: block_events::OnSubscriptionTerminatedParams,
        _results: block_events::OnSubscriptionTerminatedResults,
    ) -> Promise<(), capnp::Error> {
        let p = match params.get() {
            Ok(p) => p,
            Err(e) => return Promise::err(e),
        };
        let reason = match p.get_reason() {
            Ok(kaijutsu_client::kaijutsu_capnp::SubscriptionEndReason::SlowSubscriber) => "slowSubscriber",
            Ok(kaijutsu_client::kaijutsu_capnp::SubscriptionEndReason::ServerShutdown) => "serverShutdown",
            Ok(kaijutsu_client::kaijutsu_capnp::SubscriptionEndReason::Superseded) => "superseded",
            Err(_) => "unknown",
        };
        let mut k = self.kick.borrow_mut();
        k.reason = Some(reason.to_string());
        k.topic = p
            .get_topic()
            .ok()
            .and_then(|t| t.to_str().ok())
            .unwrap_or("")
            .to_string();
        k.delivered = p.get_delivered();
        k.capacity = p.get_capacity();
        Promise::ok(())
    }
}

#[test]
fn a_client_that_cannot_keep_up_is_told_and_disconnected() {
    // Read once, at kernel construction. Set before any server starts.
    // SAFETY: single-threaded test binary, set before any thread reads it.
    unsafe {
        std::env::set_var("KAIJUTSU_FLOW_QUEUE_DEPTH", TEST_QUEUE_DEPTH);
    }

    run_local(async {
        let addr = start_server().await;

        // The victim: subscribes, then never answers a push.
        let victim = connect_client(addr).await;
        let (victim_kernel, _) = victim.bind_kernel().await.unwrap();
        victim_kernel
            .declare_event_capabilities(false, true)
            .await
            .expect("declare — we want the kick, not the batching");

        let kick = Rc::new(RefCell::new(Kick::default()));
        let callback: block_events::Client = capnp_rpc::new_client(WedgedClient { kick: kick.clone() });
        victim_kernel
            .subscribe_blocks_filtered(
                callback,
                &kaijutsu_types::BlockEventFilter::default(),
                "wedged",
            )
            .await
            .expect("subscribe");

        // A healthy connection drives the load, so the burst is not riding the
        // socket we expect to die.
        let driver = connect_client(addr).await;
        let (driver_kernel, _) = driver.bind_kernel().await.unwrap();

        let opened = driver_kernel.editor_open(RC_PATH).await.expect("editor_open");
        let session = opened.session;
        driver_kernel
            .editor_keys(session, "i")
            .await
            .expect("insert mode");
        let chars: Vec<String> = (0..BURST_KEYS)
            .map(|i| char::from(b'a' + (i % 26) as u8).to_string())
            .collect();
        let calls: Vec<_> = chars
            .iter()
            .map(|ch| driver_kernel.editor_keys(session, ch))
            .collect();
        for r in futures::future::join_all(calls).await {
            r.expect("editor_keys");
        }

        // The bridge is parked on a callback that never resolves; it discovers
        // the termination after that callback's own timeout expires. Poll
        // rather than sleep a fixed budget, and fail loud if it never comes.
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        loop {
            if victim_kernel.list_contexts().await.is_err() {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the wedged client was never disconnected — a subscriber that \
                 cannot keep up must be cut off, not served a partial view"
            );
            tokio::time::sleep(Duration::from_millis(200)).await;
        }

        // …and it was told why, before the socket went away.
        //
        // Scoped so the `Ref` borrow drops before the `.await`s below — the
        // connection is already dead at this point so `WedgedClient` won't
        // fire another `borrow_mut()`, but there's no reason to hold a
        // `Ref` live across an await when it's done being read.
        {
            let k = kick.borrow();
            assert_eq!(
                k.reason.as_deref(),
                Some("slowSubscriber"),
                "the kick must name the reason, not just vanish"
            );
            assert_eq!(
                k.topic, "block.text_ops",
                "the kick names the topic whose event could not be enqueued"
            );
            assert_eq!(
                k.capacity,
                TEST_QUEUE_DEPTH.parse::<u64>().unwrap(),
                "the kick reports the depth that was exceeded"
            );
            assert!(
                k.delivered <= k.capacity,
                "delivered ({}) cannot exceed the queue it overflowed ({})",
                k.delivered,
                k.capacity
            );
        }

        // The healthy connection is untouched: one slow subscriber must never
        // take down the kernel or its neighbours.
        driver_kernel
            .list_contexts()
            .await
            .expect("the healthy connection keeps working");
        driver_kernel.editor_quit(session).await.expect("cleanup");
    });
}
