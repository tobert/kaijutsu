//! e2e: the audio inventory **wire surface** — `reportAudioInventory`
//! travelling client → kernel → the `/run/audio/<node-dir>/inventory.json`
//! projection, over a real SSH + Cap'n Proto round-trip
//! (`docs/audio-daemon.md` "One inventory owner").
//!
//! The store's connection/revision ordering and stale-marking are already
//! covered headless (`kaijutsu-kernel::audio_inventory`). What only the wire
//! can prove is the join: that a report really lands at the projected path
//! with the kernel's own `received_epoch_ns`/`stale` stamped in, that a
//! second daemon's node is a separate directory, and that a dropped
//! connection's node goes stale — never disappears — within the reap window.

mod common;
use common::*;

use std::time::Duration;

/// How long the kernel may take to notice a dropped connection and mark its
/// inventory stale. Well above LocalSet teardown, well below a keepalive
/// window.
const REAP_WINDOW: Duration = Duration::from_secs(3);

fn fixture_report(node: &str, revision: u64) -> Vec<u8> {
    serde_json::json!({
        "node": node,
        "revision": revision,
        "observed_epoch_ns": 1_788_869_439_000_000_000u64,
        "received_epoch_ns": 0,
        "stale": false,
        "backend": "alsa",
        "state": "ready",
        "own_clients": [130, 131],
        "endpoints": [
            {"client_id": 24, "port_id": 0, "client_name": "JD-Xi", "port_name": "JD-Xi MIDI 1",
             "address": "24:0", "is_source": true, "is_sink": true, "listening": true, "events": 1234,
             "usb_id": "0582:0158"},
        ],
        "wires": [],
    })
    .to_string()
    .into_bytes()
}

async fn read_inventory(kernel: &kaijutsu_client::KernelHandle, path: &str) -> serde_json::Value {
    let body = kernel.vfs_read_all(path).await.expect("vfs_read_all");
    serde_json::from_slice(&body).expect("projected inventory is valid JSON")
}

#[test]
fn a_report_lands_at_the_projected_path_with_kernel_stamped_fields() {
    run_local(async {
        let addr = start_server().await;
        let client = connect_client(addr).await;
        let (kernel, _) = client.bind_kernel().await.unwrap();

        kernel
            .report_audio_inventory("audio/test", 1, 1_788_869_439_000_000_000, &fixture_report("audio/test", 1))
            .await
            .expect("reportAudioInventory");

        let listing = kernel.vfs_snapshot("/run/audio", 1, 100).await.unwrap();
        let names: Vec<String> = listing.root.children.iter().map(|c| c.name.clone()).collect();
        assert_eq!(names, vec!["audio-test".to_string()], "one directory per reported node");

        let json = read_inventory(&kernel, "/run/audio/audio-test/inventory.json").await;
        assert_eq!(json["node"], "audio/test");
        assert_eq!(json["stale"], false);
        assert!(
            json["received_epoch_ns"].as_u64().unwrap() > 0,
            "the kernel must stamp its own receipt time, not the daemon's 0: {json}"
        );
        assert_eq!(json["endpoints"][0]["client_name"], "JD-Xi", "the daemon's own fields ride through verbatim");
    });
}

#[test]
fn a_dropped_connections_node_goes_stale_but_is_not_removed() {
    run_local(async {
        let addr = start_server().await;

        let reporter = connect_client(addr).await;
        let (reporter_kernel, _) = reporter.bind_kernel().await.unwrap();
        reporter_kernel
            .report_audio_inventory("audio/test", 1, 1, &fixture_report("audio/test", 1))
            .await
            .expect("reportAudioInventory");

        // A second, independent connection does the reading — the one that
        // matters is whether OTHER players still see the projection once the
        // reporting daemon is gone, not whether the reporter's own handle
        // survives its drop.
        let reader = connect_client(addr).await;
        let (reader_kernel, _) = reader.bind_kernel().await.unwrap();
        let before = read_inventory(&reader_kernel, "/run/audio/audio-test/inventory.json").await;
        assert_eq!(before["stale"], false);

        // The daemon exits: its RPC system aborts and the stream closes.
        drop(reporter_kernel);
        drop(reporter);

        let deadline = tokio::time::Instant::now() + REAP_WINDOW;
        let after = loop {
            let json = read_inventory(&reader_kernel, "/run/audio/audio-test/inventory.json").await;
            if json["stale"] == true || tokio::time::Instant::now() >= deadline {
                break json;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        };
        assert_eq!(after["stale"], true, "a dropped connection's node must go stale within the reap window");
        assert_eq!(after["node"], "audio/test", "the last observation is kept, not erased");
    });
}

#[test]
fn two_nodes_from_different_reports_are_separate_directories() {
    run_local(async {
        let addr = start_server().await;
        let client = connect_client(addr).await;
        let (kernel, _) = client.bind_kernel().await.unwrap();

        kernel
            .report_audio_inventory("audio/moltar", 1, 1, &fixture_report("audio/moltar", 1))
            .await
            .unwrap();
        kernel
            .report_audio_inventory("audio/zorak", 1, 1, &fixture_report("audio/zorak", 1))
            .await
            .unwrap();

        let listing = kernel.vfs_snapshot("/run/audio", 1, 100).await.unwrap();
        let mut names: Vec<String> = listing.root.children.iter().map(|c| c.name.clone()).collect();
        names.sort();
        assert_eq!(names, vec!["audio-moltar".to_string(), "audio-zorak".to_string()]);
    });
}
