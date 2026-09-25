//! A per-connection file-descriptor leak in `kaijutsu-server`.
//!
//! Each RPC channel gets its own OS thread with a dedicated single-threaded
//! tokio runtime (`ConnectionHandler::spawn_rpc_thread`, `ssh.rs`). Anything
//! kernel-lifetime that keeps a handle to one of that runtime's tasks keeps
//! its I/O driver open: an `anon_inode:[eventpoll]` + `anon_inode:[eventfd]`
//! pair per connection, while the thread itself exits.
//!
//! These tests connect and disconnect a real client against an ephemeral
//! server N times, let the server-side threads settle, and assert the
//! eventpoll count did not grow by ~N.
#![cfg(target_os = "linux")]

mod common;
use common::*;

use std::collections::HashSet;
use std::time::Duration;

use kaijutsu_client::kaijutsu_capnp::block_events;

/// A callback that answers every `BlockEvents` push it might receive with the
/// capnp-generated defaults (`Promise::err(unimplemented)`), same as
/// `flow_slow_subscriber_wire.rs`'s `WedgedClient` leaves most methods
/// unoverridden. We never expect real traffic in this test's short window —
/// this exists only to satisfy `subscribe_blocks_filtered`'s callback
/// parameter.
struct NoOpBlockEvents;
impl block_events::Server for NoOpBlockEvents {}

/// Every test here counts this whole process's eventpoll fds, so a test
/// running beside another would count that test's servers too. Each test
/// holds this lock for its whole run.
static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn serial() -> std::sync::MutexGuard<'static, ()> {
    SERIAL.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Count this process's open `anon_inode:[eventpoll]` file descriptors by
/// reading `/proc/self/fd` and following each symlink. Linux-only, matching
/// the leak evidence (`anon_inode:[eventpoll]` + `anon_inode:[eventfd]`
/// pairs) gathered from the live kernel.
fn eventpoll_fd_count() -> usize {
    let mut count = 0;
    let entries = std::fs::read_dir("/proc/self/fd").expect("read /proc/self/fd");
    for entry in entries.flatten() {
        if let Ok(target) = std::fs::read_link(entry.path()) {
            if target.to_string_lossy() == "anon_inode:[eventpoll]" {
                count += 1;
            }
        }
    }
    count
}

/// Every distinct `anon_inode:[...]` target currently open, for a richer
/// failure message than a bare count.
fn anon_inode_kinds() -> Vec<(String, usize)> {
    let mut kinds: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    if let Ok(entries) = std::fs::read_dir("/proc/self/fd") {
        for entry in entries.flatten() {
            if let Ok(target) = std::fs::read_link(entry.path()) {
                let s = target.to_string_lossy().to_string();
                if s.starts_with("anon_inode:") {
                    *kinds.entry(s).or_insert(0) += 1;
                }
            }
        }
    }
    let mut v: Vec<_> = kinds.into_iter().collect();
    v.sort();
    v
}

/// How many connect/disconnect cycles to drive. Large enough that a leak of
/// ~1 eventpoll fd per connection is unmistakable against normal jitter.
const CYCLES: usize = 30;

/// Slack allowed above the pre-loop baseline before we call it a leak. Real
/// growth is expected to track `CYCLES` almost 1:1 (per the live-kernel
/// evidence); a healthy server should grow by ~0 once its RPC threads settle.
const ALLOWED_GROWTH: usize = 5;

/// How long to let server-side connection threads finish tearing down after
/// the client side has dropped. Each connection's RPC thread runs on its own
/// OS thread independent of the test's tokio runtime, so this is a real
/// wall-clock wait, not a tokio yield.
const SETTLE_TIME: Duration = Duration::from_millis(1500);

/// Plain connect + `bind_kernel` + disconnect, looped. This is the generic
/// path every connection takes — including the "refused at `bind_kernel`"
/// case from stale/mismatched clients observed on the live kernel — with no
/// peer or MIDI attachment. If this alone leaks, the leak is in
/// `run_rpc`/`spawn_rpc_thread` itself, not anything peer-specific.
#[test]
fn plain_connect_disconnect_does_not_leak_eventpoll_fds() {
    let _serial = serial();
    run_local(async {
        let addr = start_server().await;

        // Warm the pool: the first connection or two may allocate long-lived
        // process-wide resources (lazy statics, a blocking-pool thread) that
        // would otherwise look like "leak" noise in the baseline.
        for _ in 0..2 {
            let client = connect_client(addr).await;
            let (handle, _) = client.bind_kernel().await.expect("bind_kernel");
            drop(handle);
            drop(client);
        }
        tokio::time::sleep(SETTLE_TIME).await;

        let before = eventpoll_fd_count();

        for _ in 0..CYCLES {
            let client = connect_client(addr).await;
            let (handle, _) = client.bind_kernel().await.expect("bind_kernel");
            drop(handle);
            drop(client);
        }

        tokio::time::sleep(SETTLE_TIME).await;
        let after = eventpoll_fd_count();
        eprintln!(
            "[plain] eventpoll before={before} after={after} growth={} anon_inode={:?}",
            after.saturating_sub(before),
            anon_inode_kinds(),
        );

        assert!(
            after <= before + ALLOWED_GROWTH,
            "eventpoll fds grew by {} over {CYCLES} plain connect/disconnect cycles \
             (before={before}, after={after}, allowed slack={ALLOWED_GROWTH}) — \
             a per-connection tokio I/O driver is not being closed.\n\
             anon_inode fds now open: {:?}",
            after.saturating_sub(before),
            anon_inode_kinds(),
        );
    });
}

/// Same as above, but each connection also attaches as a peer before
/// disconnecting — the shape of the `kaijutsu-audiod` reconnect loop from the
/// live-kernel evidence. Kept as a separate variant so a difference in growth
/// between this test and the plain one would point at peer-attach-specific
/// cleanup rather than the generic connection lifecycle.
#[test]
fn peer_attach_connect_disconnect_does_not_leak_eventpoll_fds() {
    let _serial = serial();
    run_local(async {
        let addr = start_server().await;

        for i in 0..2 {
            let client = connect_client(addr).await;
            let (handle, _) = client.bind_kernel().await.expect("bind_kernel");
            let (tx, _rx) = std::sync::mpsc::channel();
            handle
                .attach_peer(
                    &kaijutsu_client::PeerConfig {
                        nick: "fd-leak-test".to_string(),
                        instance: format!("warm-{i}"),
                    },
                    tx,
                )
                .await
                .expect("attach_peer");
            drop(handle);
            drop(client);
        }
        tokio::time::sleep(SETTLE_TIME).await;

        let before = eventpoll_fd_count();
        let mut seen_instances = HashSet::new();

        for i in 0..CYCLES {
            let client = connect_client(addr).await;
            let (handle, _) = client.bind_kernel().await.expect("bind_kernel");
            let (tx, _rx) = std::sync::mpsc::channel();
            let instance = format!("cycle-{i}");
            seen_instances.insert(instance.clone());
            handle
                .attach_peer(
                    &kaijutsu_client::PeerConfig {
                        nick: "fd-leak-test".to_string(),
                        instance,
                    },
                    tx,
                )
                .await
                .expect("attach_peer");
            drop(handle);
            drop(client);
        }

        tokio::time::sleep(SETTLE_TIME).await;
        let after = eventpoll_fd_count();
        eprintln!(
            "[peer] eventpoll before={before} after={after} growth={} anon_inode={:?} instances={}",
            after.saturating_sub(before),
            anon_inode_kinds(),
            seen_instances.len(),
        );

        assert!(
            after <= before + ALLOWED_GROWTH,
            "eventpoll fds grew by {} over {CYCLES} peer-attach connect/disconnect cycles \
             (before={before}, after={after}, allowed slack={ALLOWED_GROWTH}) — \
             a per-connection tokio I/O driver is not being closed.\n\
             anon_inode fds now open: {:?}",
            after.saturating_sub(before),
            anon_inode_kinds(),
        );
    });
}

/// Drives the raw `bindKernel` capnp method with a stale client wire version,
/// bypassing `RpcClient::bind_kernel` (which always sends the crate's own
/// `WIRE_VERSION`, so it can never exercise a mismatch). Mirrors
/// `rpc_integration.rs`'s `bind_kernel_raw_expect_refused`. This is the
/// exact shape of the live kernel's "stale remote client" reconnect loop:
/// connect, open a channel, get refused at `bind_kernel`, disconnect.
async fn bind_kernel_raw_expect_refused(addr: std::net::SocketAddr) {
    let config = kaijutsu_client::SshConfig {
        host: addr.ip().to_string(),
        port: addr.port(),
        username: "test_user".to_string(),
        key_source: root_key_source(addr),
        insecure: true,
    };
    let mut ssh = kaijutsu_client::SshClient::new(config);
    let channel = ssh.connect().await.expect("SSH connect failed");

    let compat_stream = tokio_util::compat::TokioAsyncReadCompatExt::compat(channel.into_stream());
    let (reader, writer) = futures::AsyncReadExt::split(compat_stream);
    let rpc_network = Box::new(capnp_rpc::twoparty::VatNetwork::new(
        futures::io::BufReader::new(reader),
        futures::io::BufWriter::new(writer),
        capnp_rpc::rpc_twoparty_capnp::Side::Client,
        Default::default(),
    ));
    let mut rpc_system = capnp_rpc::RpcSystem::new(rpc_network, None);
    let world: kaijutsu_client::kaijutsu_capnp::world::Client =
        rpc_system.bootstrap(capnp_rpc::rpc_twoparty_capnp::Side::Server);
    // `RpcClient::from_stream` (kaijutsu-client/src/rpc.rs) keeps this task's
    // `AbortHandle` behind an `RpcSystemGuard` specifically so dropping the
    // client aborts it — without that, the spawned task (and the stream/SSH
    // channel it owns) outlives this function, the connection never closes,
    // and the *server* thread never sees EOF either. Do the same here by
    // hand, since this raw helper bypasses `RpcClient` on purpose.
    let task = tokio::task::spawn_local(rpc_system);

    let mut request = world.bind_kernel_request();
    // 0 is the reserved "predates this field" sentinel — always a mismatch.
    request.get().set_wire_version(0);
    match request.send().promise.await {
        Ok(_) => panic!("mismatched wire version must be refused, not tolerated"),
        Err(_) => {}
    }
    task.abort();
    let _ = task.await;
    // `world`, `ssh`, and the underlying SSH channel drop here — mirroring
    // the stale client's own disconnect right after the refusal.
}

/// A stale client refused at `bind_kernel`, looped. This is the *other* half
/// of the live-kernel evidence: a version-mismatched reconnect loop that does
/// very little (no kernel capability is ever handed out) before the
/// connection closes. If this alone leaks at the same rate as the plain
/// variant, the leak is in the generic `run_rpc`/`spawn_rpc_thread` setup
/// that happens before any RPC method runs, not in `bindKernel` or anything
/// downstream of it.
#[test]
fn bind_kernel_refusal_does_not_leak_eventpoll_fds() {
    let _serial = serial();
    run_local(async {
        let addr = start_server().await;

        for _ in 0..2 {
            bind_kernel_raw_expect_refused(addr).await;
        }
        tokio::time::sleep(SETTLE_TIME).await;

        let before = eventpoll_fd_count();

        for _ in 0..CYCLES {
            bind_kernel_raw_expect_refused(addr).await;
        }

        tokio::time::sleep(SETTLE_TIME).await;
        let after = eventpoll_fd_count();
        eprintln!(
            "[refused] eventpoll before={before} after={after} growth={} anon_inode={:?}",
            after.saturating_sub(before),
            anon_inode_kinds(),
        );

        assert!(
            after <= before + ALLOWED_GROWTH,
            "eventpoll fds grew by {} over {CYCLES} bind_kernel-refusal cycles \
             (before={before}, after={after}, allowed slack={ALLOWED_GROWTH}) — \
             a per-connection tokio I/O driver is not being closed.\n\
             anon_inode fds now open: {:?}",
            after.saturating_sub(before),
            anon_inode_kinds(),
        );
    });
}

/// The full `kaijutsu-audiod` shape: attach as a peer, report MIDI presence,
/// report audio inventory, then disconnect. `attach_peer` alone (the variant
/// above) only exercises `ConnectionState::midi_exchange`/`peer_attachments`;
/// this also exercises `midi_presence` and `audio_inventory`, the other two
/// `Drop`-reaped fields on `ConnectionState` (`rpc.rs` "midi_presence"/
/// "audio_inventory" doc comments).
#[test]
fn full_audiod_shape_connect_disconnect_does_not_leak_eventpoll_fds() {
    let _serial = serial();
    run_local(async {
        let addr = start_server().await;

        async fn one_cycle(addr: std::net::SocketAddr, i: usize) {
            let client = connect_client(addr).await;
            let (handle, _) = client.bind_kernel().await.expect("bind_kernel");
            let (tx, _rx) = std::sync::mpsc::channel();
            handle
                .attach_peer(
                    &kaijutsu_client::PeerConfig {
                        nick: "fd-leak-audiod".to_string(),
                        instance: format!("cycle-{i}"),
                    },
                    tx,
                )
                .await
                .expect("attach_peer");
            let _ = handle
                .report_midi_presence("fd-leak-test-device", true, "alsa", &[], 1, "test-host")
                .await;
            let _ = handle
                .report_audio_inventory("fd-leak-test-node", 1, 1, b"{}")
                .await;
            drop(handle);
            drop(client);
        }

        for i in 0..2 {
            one_cycle(addr, i).await;
        }
        tokio::time::sleep(SETTLE_TIME).await;

        let before = eventpoll_fd_count();

        for i in 0..CYCLES {
            one_cycle(addr, i).await;
        }

        tokio::time::sleep(SETTLE_TIME).await;
        let after = eventpoll_fd_count();
        eprintln!(
            "[audiod-shape] eventpoll before={before} after={after} growth={} anon_inode={:?}",
            after.saturating_sub(before),
            anon_inode_kinds(),
        );

        assert!(
            after <= before + ALLOWED_GROWTH,
            "eventpoll fds grew by {} over {CYCLES} full-audiod-shape connect/disconnect cycles \
             (before={before}, after={after}, allowed slack={ALLOWED_GROWTH}) — \
             a per-connection tokio I/O driver is not being closed.\n\
             anon_inode fds now open: {:?}",
            after.saturating_sub(before),
            anon_inode_kinds(),
        );
    });
}

/// Two reconnect loops running concurrently and overlapping in time — the
/// literal shape of the live-kernel evidence (`kaijutsu-audiod` attaching as
/// a peer roughly every 6s while a stale remote client gets refused at
/// `bind_kernel` roughly every 5s, at the same time). Every prior variant in
/// this file drove one connection at a time; this one checks whether
/// concurrent, interleaved connect/disconnect is what a purely sequential
/// test can't see (a race in shared registries, the active-connections
/// counter, or capnp-rpc's per-connection setup).
#[test]
fn concurrent_reconnect_loops_do_not_leak_eventpoll_fds() {
    let _serial = serial();
    run_local(async {
        let addr = start_server().await;

        // Warm up both shapes once, sequentially, before racing them.
        {
            let client = connect_client(addr).await;
            let (handle, _) = client.bind_kernel().await.expect("bind_kernel");
            let (tx, _rx) = std::sync::mpsc::channel();
            handle
                .attach_peer(
                    &kaijutsu_client::PeerConfig {
                        nick: "fd-leak-concurrent".to_string(),
                        instance: "warm".to_string(),
                    },
                    tx,
                )
                .await
                .expect("attach_peer");
            drop(handle);
            drop(client);
        }
        bind_kernel_raw_expect_refused(addr).await;
        tokio::time::sleep(SETTLE_TIME).await;

        let before = eventpoll_fd_count();

        let peer_loop = async {
            for i in 0..CYCLES {
                let client = connect_client(addr).await;
                let (handle, _) = client.bind_kernel().await.expect("bind_kernel");
                let (tx, _rx) = std::sync::mpsc::channel();
                handle
                    .attach_peer(
                        &kaijutsu_client::PeerConfig {
                            nick: "fd-leak-concurrent".to_string(),
                            instance: format!("race-{i}"),
                        },
                        tx,
                    )
                    .await
                    .expect("attach_peer");
                drop(handle);
                drop(client);
                tokio::time::sleep(Duration::from_millis(15)).await;
            }
        };
        let refusal_loop = async {
            for _ in 0..CYCLES {
                bind_kernel_raw_expect_refused(addr).await;
                tokio::time::sleep(Duration::from_millis(12)).await;
            }
        };
        tokio::join!(peer_loop, refusal_loop);

        tokio::time::sleep(SETTLE_TIME).await;
        let after = eventpoll_fd_count();
        eprintln!(
            "[concurrent] eventpoll before={before} after={after} growth={} anon_inode={:?}",
            after.saturating_sub(before),
            anon_inode_kinds(),
        );

        assert!(
            after <= before + ALLOWED_GROWTH,
            "eventpoll fds grew by {} over {CYCLES} concurrent, interleaved connect/disconnect \
             cycles (before={before}, after={after}, allowed slack={ALLOWED_GROWTH}) — \
             a per-connection tokio I/O driver is not being closed.\n\
             anon_inode fds now open: {:?}",
            after.saturating_sub(before),
            anon_inode_kinds(),
        );
    });
}

/// The actual `kaijutsu-audiod` leak: `subscribeBlocksFiltered` with a FRESH
/// `instance` id every cycle — exactly what happens when the whole daemon
/// PROCESS restarts (not just reconnects), since `instance` is "set once at
/// actor construction" (`kaijutsu-client/src/actor.rs:57`) and a new process
/// means a new construction. Every prior variant in this file reused one
/// `SshClient`-level actor per cycle without ever calling
/// `subscribe_blocks_filtered`, which is exactly the RPC this test adds.
///
/// `SharedKernelState::subscription_registry` (`rpc.rs`) dedupes live
/// subscriptions by `(principal, instance)`, but a distinct `instance` per
/// cycle never collides with a prior entry — so nothing ever replaces (and
/// aborts) the old one, and the registry comment's "we do NOT remove our own
/// entry on natural exit... a tiny bounded leak" turns out not to be tiny:
/// the leaked `AbortHandle` keeps the finished task's cell alive, which keeps
/// a clone of the connection's `tokio::runtime::Handle` alive, which owns the
/// io driver's registry-duplicated epoll fd and waker eventfd — the exact
/// pair from the live-kernel evidence.
#[test]
fn subscribe_blocks_filtered_fresh_instance_leaks_eventpoll_fds() {
    let _serial = serial();
    run_local(async {
        let addr = start_server().await;

        async fn one_cycle(addr: std::net::SocketAddr, i: usize) {
            let client = connect_client(addr).await;
            let (handle, _) = client.bind_kernel().await.expect("bind_kernel");
            let callback: block_events::Client = capnp_rpc::new_client(NoOpBlockEvents);
            handle
                .subscribe_blocks_filtered(
                    callback,
                    &kaijutsu_types::BlockEventFilter::default(),
                    // A fresh instance every cycle — the audiod-process-
                    // restart shape, not the reconnect-same-actor shape the
                    // other tests in this file exercise.
                    &format!("kaijutsu-audiod-{i}-{}", uuid::Uuid::new_v4()),
                )
                .await
                .expect("subscribe_blocks_filtered");
            drop(handle);
            drop(client);
        }

        for i in 0..2 {
            one_cycle(addr, i).await;
        }
        tokio::time::sleep(SETTLE_TIME).await;

        let before = eventpoll_fd_count();

        for i in 0..CYCLES {
            one_cycle(addr, i).await;
        }

        tokio::time::sleep(SETTLE_TIME).await;
        let after = eventpoll_fd_count();
        eprintln!(
            "[fresh-instance-subscribe] eventpoll before={before} after={after} growth={} \
             anon_inode={:?}",
            after.saturating_sub(before),
            anon_inode_kinds(),
        );

        assert!(
            after <= before + ALLOWED_GROWTH,
            "eventpoll fds grew by {} over {CYCLES} fresh-instance subscribe_blocks_filtered \
             connect/disconnect cycles (before={before}, after={after}, allowed \
             slack={ALLOWED_GROWTH}) — a per-connection tokio I/O driver is not being closed.\n\
             anon_inode fds now open: {:?}",
            after.saturating_sub(before),
            anon_inode_kinds(),
        );
    });
}
