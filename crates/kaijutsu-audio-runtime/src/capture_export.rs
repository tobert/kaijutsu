//! Bounded capture staging over SFTP. Completion means the remote file closed,
//! not that the kernel accepted it into CAS. See `docs/audio-daemon.md`.

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use kaijutsu_cas::ContentHash;
use kaijutsu_client::ssh::{SshClient, SshConfig};
use kaijutsu_types::SSH_SFTP_SUBSYSTEM;
use russh_sftp::{client::SftpSession, protocol::{FileAttributes, OpenFlags}};
use tokio::io::{AsyncWrite, AsyncWriteExt};
use tokio::sync::watch;

pub const MAX_ARTIFACT_BYTES: usize = 16 * 1024 * 1024;
const CHUNK_BYTES: usize = 64 * 1024;
const OPERATION_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug)]
pub struct UploadedCapture {
    pub hash: ContentHash,
    pub bytes: u64,
}

/// Upload an owned artifact on an ordinary async worker. The caller retains its
/// protected take until the kernel confirms CAS acceptance or cancellation.
/// Existing staging files are never overwritten; retry needs a new staging path.
pub async fn upload(
    config: SshConfig,
    staging_path: String,
    bytes: Arc<[u8]>,
    mut cancel: watch::Receiver<bool>,
) -> Result<UploadedCapture, String> {
    validate_path(&staging_path)?;
    validate_size(bytes.len())?;
    let task = tokio::task::spawn_blocking(move || (ContentHash::from_data(&bytes), bytes));
    let (hash, bytes) = finish_hash(task, &mut cancel).await?;
    let mut ssh = SshClient::new(config);
    let channel = guarded(&mut cancel, "connect capture SSH", async {
        ssh.connect_subsystem(SSH_SFTP_SUBSYSTEM).await.map_err(|error| error.to_string())
    }).await?;
    let session = guarded(&mut cancel, "connect capture SFTP", async {
        SftpSession::new(channel.into_stream()).await.map_err(|error| error.to_string())
    }).await?;
    let mut file = guarded(&mut cancel, "create capture staging file", async {
        let mut attributes = FileAttributes::empty();
        attributes.permissions = Some(0o600);
        session.open_with_flags_and_attributes(
            staging_path,
            OpenFlags::WRITE | OpenFlags::CREATE | OpenFlags::EXCLUDE,
            attributes,
        )
            .await.map_err(|error| error.to_string())
    }).await?;
    write_artifact(&mut file, &bytes, &mut cancel).await?;
    // Keep the SSH owner alive until the SFTP file has acknowledged CLOSE.
    drop(file);
    drop(session);
    drop(ssh);
    Ok(UploadedCapture { hash, bytes: bytes.len() as u64 })
}

async fn finish_hash<T>(mut task: tokio::task::JoinHandle<T>, cancel: &mut watch::Receiver<bool>) -> Result<T, String> {
    let result = guarded(cancel, "hash capture", async { Ok((&mut task).await) }).await;
    match result {
        Ok(result) => result.map_err(|error| format!("hash capture worker failed: {error}")),
        Err(error) => {
            // Abort removes queued work. Running blocking work cannot be aborted;
            // join it before releasing the take's charge and its retained bytes.
            task.abort();
            let _ = task.await;
            Err(error)
        }
    }
}

fn validate_size(bytes: usize) -> Result<(), String> {
    if bytes == 0 || bytes > MAX_ARTIFACT_BYTES {
        return Err(format!("capture export requires 1..={MAX_ARTIFACT_BYTES} bytes; received {bytes}"));
    }
    Ok(())
}

fn validate_path(path: &str) -> Result<(), String> {
    let valid = path.strip_prefix("/tmp/kaijutsu-audio-")
        .and_then(|tail| tail.strip_suffix("/capture.json"))
        .is_some_and(|id| uuid::Uuid::parse_str(id)
            .is_ok_and(|parsed| parsed.hyphenated().to_string() == id));
    if !valid {
        return Err("capture staging path must be /tmp/kaijutsu-audio-<UUID>/capture.json".into());
    }
    Ok(())
}

async fn guarded<T>(
    cancel: &mut watch::Receiver<bool>,
    operation: &str,
    future: impl Future<Output = Result<T, String>>,
) -> Result<T, String> {
    if *cancel.borrow() {
        return Err("capture export cancelled".into());
    }
    tokio::select! {
        biased;
        _ = async {
            loop {
                if cancel.changed().await.is_err() || *cancel.borrow() {
                    break;
                }
            }
        } => Err("capture export cancelled".into()),
        result = tokio::time::timeout(OPERATION_TIMEOUT, future) => {
            result.map_err(|_| format!("{operation} timed out after 30 seconds"))?
                .map_err(|error| format!("{operation} failed: {error}"))
        }
    }
}

async fn write_artifact(
    file: &mut (impl AsyncWrite + Unpin),
    bytes: &[u8],
    cancel: &mut watch::Receiver<bool>,
) -> Result<(), String> {
    validate_size(bytes.len())?;
    for chunk in bytes.chunks(CHUNK_BYTES) {
        guarded(cancel, "write capture chunk", async {
            file.write_all(chunk).await.map_err(|error| error.to_string())?;
            // SFTP writes may queue acknowledgements. Drain each chunk so a slow
            // peer cannot turn the whole artifact into an unbounded send queue.
            file.flush().await.map_err(|error| error.to_string())
        }).await?;
    }
    guarded(cancel, "close capture staging file", async {
        file.shutdown().await.map_err(|error| error.to_string())
    }).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io;
    use std::pin::Pin;
    use std::task::{Context, Poll};

    #[derive(Default)]
    struct Sink {
        data: Vec<u8>,
        largest_write: usize,
        flushes: usize,
        closed: bool,
        fail_write: bool,
        fail_close: bool,
        cancel_after_write: Option<watch::Sender<bool>>,
    }

    impl AsyncWrite for Sink {
        fn poll_write(mut self: Pin<&mut Self>, _: &mut Context<'_>, bytes: &[u8]) -> Poll<io::Result<usize>> {
            if self.fail_write {
                return Poll::Ready(Err(io::Error::other("write rejected")));
            }
            self.largest_write = self.largest_write.max(bytes.len());
            self.data.extend_from_slice(bytes);
            if let Some(cancel) = &self.cancel_after_write {
                cancel.send_replace(true);
            }
            Poll::Ready(Ok(bytes.len()))
        }

        fn poll_flush(mut self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
            self.flushes += 1;
            Poll::Ready(Ok(()))
        }

        fn poll_shutdown(mut self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
            if self.fail_close {
                return Poll::Ready(Err(io::Error::other("close rejected")));
            }
            self.closed = true;
            Poll::Ready(Ok(()))
        }
    }

    #[tokio::test]
    async fn chunked_transfer_preserves_bytes_and_closes_before_success() {
        let (_tx, mut cancel) = watch::channel(false);
        let bytes = vec![42; CHUNK_BYTES * 2 + 13];
        let mut sink = Sink::default();
        write_artifact(&mut sink, &bytes, &mut cancel).await.unwrap();
        assert_eq!(sink.data, bytes);
        assert_eq!(ContentHash::from_data(&sink.data), ContentHash::from_data(&bytes));
        assert_eq!(sink.largest_write, CHUNK_BYTES);
        assert_eq!(sink.flushes, 3);
        assert!(sink.closed);
    }

    #[tokio::test]
    async fn write_and_close_failures_never_report_success() {
        let (_tx, mut cancel) = watch::channel(false);
        for fail_write in [true, false] {
            let mut sink = Sink { fail_write, fail_close: !fail_write, ..Default::default() };
            let error = write_artifact(&mut sink, b"artifact", &mut cancel).await.unwrap_err();
            assert!(error.contains(if fail_write { "write rejected" } else { "close rejected" }));
            assert!(!sink.closed);
        }
    }

    #[tokio::test]
    async fn cancellation_stops_before_the_next_chunk() {
        let (tx, mut cancel) = watch::channel(false);
        let mut sink = Sink { cancel_after_write: Some(tx), ..Default::default() };
        let error = write_artifact(&mut sink, &vec![7; CHUNK_BYTES * 2], &mut cancel).await.unwrap_err();
        assert!(error.contains("cancelled"));
        assert_eq!(sink.data.len(), CHUNK_BYTES);
        assert!(!sink.closed);
    }

    #[tokio::test]
    async fn closed_cancellation_owner_stops_transfer() {
        let (tx, mut cancel) = watch::channel(false);
        drop(tx);
        let mut sink = Sink::default();
        assert!(write_artifact(&mut sink, b"artifact", &mut cancel).await.unwrap_err().contains("cancelled"));
        assert!(sink.data.is_empty());
    }

    #[tokio::test]
    async fn cancellation_interrupts_a_blocked_write() {
        use tokio::io::AsyncReadExt;
        let (tx, mut cancel) = watch::channel(false);
        let (mut writer, mut reader) = tokio::io::duplex(1);
        let cancel_task = tokio::spawn(async move {
            let mut first_byte = [0];
            reader.read_exact(&mut first_byte).await.unwrap();
            tx.send_replace(true);
            // Keep the reader alive until the writer sees cancellation.
            reader
        });
        let result = tokio::time::timeout(Duration::from_secs(1),
            write_artifact(&mut writer, &[7; CHUNK_BYTES], &mut cancel)).await.unwrap();
        assert!(result.unwrap_err().contains("cancelled"));
        drop(cancel_task.await.unwrap());
    }

    #[test]
    fn staging_path_is_exact_and_does_not_accept_traversal() {
        let id = uuid::Uuid::new_v4();
        assert!(validate_path(&format!("/tmp/kaijutsu-audio-{id}/capture.json")).is_ok());
        for path in ["/tmp/capture.json", "/tmp/kaijutsu-audio-../capture.json", "/tmp/kaijutsu-audio-a/../capture.json"] {
            assert!(validate_path(path).is_err());
        }
        assert!(validate_path(&format!("/tmp/kaijutsu-audio-{id}/../capture.json")).is_err());
        assert!(validate_path(&format!("/tmp/kaijutsu-audio-{id}/other.json")).is_err());
    }

    #[test]
    fn empty_and_oversized_artifacts_are_rejected() {
        assert!(validate_size(0).is_err());
        assert!(validate_size(MAX_ARTIFACT_BYTES + 1).is_err());
        assert!(validate_size(MAX_ARTIFACT_BYTES).is_ok());
    }

    #[test]
    fn cancelling_a_queued_hash_drops_its_payload_before_returning() {
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().max_blocking_threads(1).build().unwrap();
        runtime.block_on(async {
            let (release, blocked) = std::sync::mpsc::channel();
            let (started_tx, started_rx) = tokio::sync::oneshot::channel();
            let blocker = tokio::task::spawn_blocking(move || {
                started_tx.send(()).unwrap();
                blocked.recv().unwrap();
            });
            started_rx.await.unwrap();
            let payload: Arc<[u8]> = Arc::from(&b"retained"[..]);
            let owned = payload.clone();
            let task = tokio::task::spawn_blocking(move || owned);
            let (_tx, mut cancel) = watch::channel(true);
            let waiter = tokio::spawn(async move { finish_hash(task, &mut cancel).await });
            tokio::task::yield_now().await;
            // An aborted queued task is dropped when the blocking worker next
            // polls its queue, so release the blocker while awaiting cleanup.
            release.send(()).unwrap();
            assert!(waiter.await.unwrap().unwrap_err().contains("cancelled"));
            blocker.await.unwrap();
            assert_eq!(Arc::strong_count(&payload), 1);
        });
    }

    #[tokio::test]
    async fn cancelling_a_running_hash_waits_until_its_payload_is_released() {
        let payload: Arc<[u8]> = Arc::from(&b"retained"[..]);
        let owned = payload.clone();
        let (release, blocked) = std::sync::mpsc::channel();
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let task = tokio::task::spawn_blocking(move || {
            started_tx.send(()).unwrap();
            blocked.recv().unwrap();
            owned
        });
        started_rx.await.unwrap();
        let (_tx, mut cancel) = watch::channel(true);
        let waiter = tokio::spawn(async move { finish_hash(task, &mut cancel).await });
        tokio::task::yield_now().await;
        let still_retained = Arc::strong_count(&payload);
        let returned_early = waiter.is_finished();
        release.send(()).unwrap();
        assert!(waiter.await.unwrap().unwrap_err().contains("cancelled"));
        assert!(!returned_early);
        assert_eq!(still_retained, 2);
        assert_eq!(Arc::strong_count(&payload), 1);
    }

    #[cfg(unix)]
    #[tokio::test]
    #[ignore = "requires explicit localhost SSH smoke-test approval"]
    async fn live_localhost_sftp_upload_closes_and_matches_hash() {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(std::env::var("KAIJUTSU_AUDIO_LIVE_SFTP").as_deref(), Ok("1"),
            "set KAIJUTSU_AUDIO_LIVE_SFTP=1 only for the approved local smoke test");
        let directory = tempfile::Builder::new()
            .prefix(&format!("kaijutsu-audio-{}", uuid::Uuid::new_v4()))
            .permissions(std::fs::Permissions::from_mode(0o700))
            .rand_bytes(0).tempdir_in("/tmp").unwrap();
        let path = directory.path().join("capture.json");
        let config = SshConfig {
            host: "localhost".into(), port: 2222, username: "audio/zorak".into(),
            key_source: kaijutsu_client::KeySource::from_file(
                dirs::home_dir().unwrap().join(".ssh/kaijutsu-audio-zorak")),
            insecure: false,
        };
        let payload: Arc<[u8]> = Arc::from(&b"{\"kind\":\"capture-upload-smoke\"}"[..]);
        let expected = ContentHash::from_data(&payload);
        let (_tx, cancel) = watch::channel(false);
        let uploaded = upload(config, path.to_str().unwrap().into(), payload.clone(), cancel).await.unwrap();
        assert_eq!(uploaded.hash, expected);
        assert_eq!(uploaded.bytes, payload.len() as u64);
        let actual = std::fs::read(path).unwrap();
        assert_eq!(actual.as_slice(), &*payload);
        assert_eq!(ContentHash::from_data(&actual), expected);
        directory.close().unwrap();
    }
}
