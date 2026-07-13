//! e2e: the VFS activity digest **wire surface** — `subscribeVfsActivity`
//! push channel over a real SSH + Cap'n Proto round-trip (Lane K, FSN
//! slice-1, `docs/scenes/vfs.md`).
//!
//! The digest/cursor math is already covered headless
//! (`kaijutsu-kernel::vfs::activity`); this proves the same absolute-total,
//! lossy-safe stream survives the wire: a subscriber sees a directory's
//! activity total climb (never reset to a bare delta) as real VFS mutations
//! land, and the server's global epoch climbs alongside it.

mod common;

use std::time::Duration;

use tokio::sync::broadcast::Receiver;

use common::{connect_client, run_local, start_server};
use kaijutsu_client::{ServerEvent, vfs_activity_events_channel};

/// Drain the activity push channel until a digest carrying an entry for
/// `dir` arrives, returning `(entry_total, digest_global_total)`. Fails
/// loud on timeout — a missing push is exactly the bug this test exists to
/// catch, never a silently-skipped assertion.
async fn recv_digest_containing(rx: &mut Receiver<ServerEvent>, dir: &str) -> (u64, u64) {
    loop {
        match tokio::time::timeout(Duration::from_secs(5), rx.recv()).await {
            Ok(Ok(ServerEvent::VfsActivity { entries, global_total })) => {
                if let Some(e) = entries.iter().find(|e| e.path == dir) {
                    return (e.total, global_total);
                }
                // A digest that doesn't (yet) mention our directory — keep
                // draining; the next tick should carry it.
                continue;
            }
            Ok(Ok(_)) => continue,
            Ok(Err(e)) => panic!("vfs activity push channel error: {e}"),
            Err(_) => panic!("timed out waiting for a VfsActivity digest containing {dir}"),
        }
    }
}

#[test]
fn vfs_activity_digest_streams_over_the_wire_with_absolute_totals() {
    run_local(async {
        let addr = start_server().await;
        let client = connect_client(addr).await;
        let (kernel, _) = client.bind_kernel().await.unwrap();

        // Subscribe BEFORE mutating, so the digest can't be missed. Ask for
        // the server's floor interval (500ms) so the test doesn't idle a
        // full second per tick.
        let (callback, mut rx) = vfs_activity_events_channel(64);
        kernel.subscribe_vfs_activity(callback, 500).await.unwrap();

        // A unique directory under the server's real, RW-mounted /tmp —
        // real host mutations, not a virtual backend, so this exercises the
        // LocalBackend bump sites. MOUNT DEPENDENCY: this relies on the
        // kernel bootstrap's `kernel.mount("/tmp", LocalBackend::new("/tmp"))`
        // in kaijutsu-server/src/rpc.rs, which carries a cross-reference back
        // to this test. If /tmp stops being RW-mounted there, vfs_create
        // below fails with a no-mount-point error.
        let dir = tempfile::tempdir_in("/tmp").expect("tempdir under /tmp");
        let vfs_dir = dir.path().to_string_lossy().into_owned();
        let file_path = format!("{vfs_dir}/a.txt");

        kernel
            .vfs_create(&file_path, 0o644)
            .await
            .expect("vfs_create over the wire — requires the server's RW /tmp mount (rpc.rs bootstrap)");
        let (total1, global1) = recv_digest_containing(&mut rx, &vfs_dir).await;
        assert!(total1 >= 1, "create must register at least one bump");
        assert!(
            global1 >= total1,
            "the global epoch is the sum of every bump, never less than one directory's total"
        );

        kernel
            .vfs_write(&file_path, 0, b"more content")
            .await
            .expect("vfs_write over the wire");
        let (total2, global2) = recv_digest_containing(&mut rx, &vfs_dir).await;
        assert!(
            total2 > total1,
            "the SAME directory's total must have grown (absolute, cumulative) — \
             got total1={total1} total2={total2}"
        );
        assert!(
            global2 > global1,
            "the global epoch must also have advanced — got global1={global1} global2={global2}"
        );
    });
}
