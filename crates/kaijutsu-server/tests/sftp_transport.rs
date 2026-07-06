//! End-to-end SFTP *transport* over the real SSH stack: the client
//! (`kaijutsu_client::SftpClient`) requests the `sftp` subsystem on a second
//! session channel and reads a VFS path back over russh.
//!
//! The SFTP *adapter* (the `VfsOps` bridge) is covered by
//! `tests/sftp_adapter.rs` over an in-memory pipe; this pins the client half +
//! the SSH subsystem dispatch that adapter test can't reach.
//!
//! The production kernel mounts host `/tmp` writable on the SFTP-served VFS
//! (`create_shared_kernel`), so a host file under `/tmp` is readable at the same
//! path over SFTP — the transport is provable today, before `/v/cas` exists.

mod common;

use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use common::{run_local, start_server, start_server_with_state_dir};
use kaijutsu_cas::{ContentStore, FileStore};
use kaijutsu_client::{CasResolver, KeySource, SftpClient, SftpError, SshConfig};

fn ephemeral_config(addr: std::net::SocketAddr) -> SshConfig {
    SshConfig {
        host: addr.ip().to_string(),
        port: addr.port(),
        username: "test_user".to_string(),
        key_source: KeySource::ephemeral(),
        insecure: true,
    }
}

/// A unique host `/tmp` filename for one test run (the server mounts host
/// `/tmp`, so writing here makes the bytes readable over SFTP at `/tmp/<name>`).
fn unique_tmp_name(tag: &str) -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!("kaijutsu-sftp-{tag}-{}-{nanos}.bin", std::process::id())
}

#[test]
fn sftp_reads_a_seeded_file_over_real_ssh() {
    run_local(async {
        let addr = start_server().await;

        let name = unique_tmp_name("read");
        let host_path = Path::new("/tmp").join(&name);
        let body: &[u8] = b"sftp transport round-trip\n";
        std::fs::write(&host_path, body).expect("seed /tmp file");

        let sftp = SftpClient::connect(ephemeral_config(addr))
            .await
            .expect("sftp subsystem connect");
        let got = sftp.read(&format!("/tmp/{name}")).await;

        // Clean up the host file regardless of the assertion outcome.
        let _ = std::fs::remove_file(&host_path);

        assert_eq!(got.expect("sftp read"), body);
    });
}

#[test]
fn sftp_read_of_a_missing_path_fails_loud() {
    run_local(async {
        let addr = start_server().await;

        let sftp = SftpClient::connect(ephemeral_config(addr))
            .await
            .expect("sftp subsystem connect");

        // A missing object must surface as a typed NotFound (a normal "404" the
        // resolver can distinguish from a transport failure), never empty bytes.
        let missing = format!("/tmp/{}", unique_tmp_name("missing"));
        let err = sftp.read(&missing).await.expect_err("missing path must error");
        assert!(matches!(err, SftpError::NotFound(_)), "got {err:?}");
    });
}

/// The whole track-B round trip over real SSH: an object seeded into the server's
/// CAS is fetched by a `CasResolver` through the `/v/cas` mount, verified,
/// and cached. This is the first e2e exercising `CasFs` + the client resolver
/// together (the resolver unit tests use a stub; this pins the live mount).
#[test]
fn object_resolves_over_v_cas_and_verifies() {
    run_local(async {
        // Seed an object into the server's CAS *before* it starts — CasFs reads the
        // pool live, and the resolver's sharded path maps straight onto it.
        let state = tempfile::tempdir().expect("state dir");
        let server_cas = FileStore::at_path(state.path().join("cas"));
        let body: &[u8] = b"a clip's worth of bytes, resolved over /v/cas\n";
        let hash = server_cas.store(body, "audio/wav").expect("seed server CAS");

        let addr = start_server_with_state_dir(state.path().to_path_buf()).await;

        // A fresh client cache: the first resolve must cross the wire.
        let cache_dir = tempfile::tempdir().expect("cache dir");
        let sftp = SftpClient::connect(ephemeral_config(addr))
            .await
            .expect("sftp subsystem connect");
        let resolver = CasResolver::new(FileStore::at_path(cache_dir.path()), sftp);

        let got = resolver.resolve(&hash).await.expect("resolve over /v/cas");
        assert_eq!(got, body, "resolved bytes match the seeded object");

        // The verified object landed in the client cache (content-addressed), so a
        // fresh FileStore over the same dir sees it.
        let cache = FileStore::at_path(cache_dir.path());
        assert_eq!(
            cache.retrieve(&hash).expect("cache read").as_deref(),
            Some(body),
            "the fetched object is cached under its hash"
        );
    });
}

/// An object whose on-disk server bytes have been corrupted must fail the
/// resolver's re-hash verification (crash over corruption) and leave the client
/// cache clean — the CAS is self-verifying end to end, past SSH's transport
/// integrity.
#[test]
fn a_corrupted_server_object_fails_verification_and_caches_nothing() {
    run_local(async {
        let state = tempfile::tempdir().expect("state dir");
        let server_cas = FileStore::at_path(state.path().join("cas"));
        let body: &[u8] = b"the real clip bytes, long enough to matter\n";
        let hash = server_cas.store(body, "audio/wav").expect("seed server CAS");

        // Corrupt the object on disk (flip a byte). CasFs serves the file by its
        // hash-named path regardless of content, so the wire now carries a lie.
        let obj = server_cas.path(&hash).expect("object path on disk");
        let mut corrupt = std::fs::read(&obj).expect("read object");
        corrupt[0] ^= 0xff;
        std::fs::write(&obj, &corrupt).expect("corrupt object");

        let addr = start_server_with_state_dir(state.path().to_path_buf()).await;

        let cache_dir = tempfile::tempdir().expect("cache dir");
        let sftp = SftpClient::connect(ephemeral_config(addr))
            .await
            .expect("sftp subsystem connect");
        let resolver = CasResolver::new(FileStore::at_path(cache_dir.path()), sftp);

        let err = resolver
            .resolve(&hash)
            .await
            .expect_err("corrupted object must fail verification");
        assert!(matches!(err, SftpError::HashMismatch { .. }), "got {err:?}");

        // Nothing corrupt entered the cache.
        let cache = FileStore::at_path(cache_dir.path());
        assert!(
            cache.retrieve(&hash).expect("cache read").is_none(),
            "corrupt bytes must never be cached"
        );
    });
}
