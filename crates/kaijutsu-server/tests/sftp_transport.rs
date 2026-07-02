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
//! path over SFTP — the transport is provable today, before `/v/blobs` exists.

mod common;

use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use common::{run_local, start_server};
use kaijutsu_client::{KeySource, SftpClient, SftpError, SshConfig};

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

        // A missing object must surface as an error, never empty bytes.
        let missing = format!("/tmp/{}", unique_tmp_name("missing"));
        let err = sftp.read(&missing).await.expect_err("missing path must error");
        assert!(matches!(err, SftpError::Protocol(_)), "got {err:?}");
    });
}
