//! The streaming pump — drives a source's [`VfsOps::open_read_stream`] into
//! a sink, chunk by chunk, without buffering a whole file in kernel memory.
//!
//! `docs/slash-r.md` slice 0: `kj cp` (a [`VfsSink`] destination) and
//! `kj cas put` (a CAS-hashing sink) both sit on [`pump`]. Later slices
//! (share sync) reuse it unchanged — only the sink changes.
//!
//! **No mid-pump source consistency is promised.** If the source mutates
//! while a pump is in flight, the destination ends up a spliced copy of
//! whatever bytes were live when each chunk was read — documented, not
//! defended, exactly like a local `cp` promises nothing about a
//! concurrently-written source. The CAS sink is the honest path when that
//! matters: its hash covers exactly the bytes streamed, so the *result* is
//! never ambiguous about what it captured, even if the source's on-disk
//! state has since moved on.
//!
//! **Interruption is loud.** A source read error or a sink write/finalize
//! error aborts the pump immediately; [`PumpError::bytes_transferred`]
//! reports exactly how many bytes reached the sink before the failure, so a
//! caller never mistakes a truncated destination for a complete one.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use futures::StreamExt;
use thiserror::Error;

use super::error::VfsError;
use super::ops::VfsOps;

/// Errors a [`PumpSink`] can report. A single enum shared by every sink
/// (`VfsSink`, `CasSink`) rather than a generic boxed error, so [`PumpError`]
/// doesn't need a type parameter — `thiserror` gives each source its own
/// `#[from]` without the `From<T> for T` blanket-impl coherence conflict a
/// fully-generic wrapper would hit.
#[derive(Debug, Error)]
pub enum SinkError {
    #[error("VFS error: {0}")]
    Vfs(#[from] VfsError),
    #[error("CAS store error: {0}")]
    Cas(#[from] kaijutsu_cas::StoreError),
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
}

/// A pump's failure, carrying how far it got. Every variant names which side
/// failed (source read vs. sink write vs. sink finalize) and the exact byte
/// count that reached the sink before the failure — the loud-interruption
/// contract (`docs/slash-r.md` slice 0): no partial file is ever silently
/// blessed as complete.
#[derive(Debug, Error)]
pub enum PumpError {
    #[error("source read failed after {bytes_transferred} bytes: {source}")]
    Source {
        bytes_transferred: u64,
        #[source]
        source: VfsError,
    },
    #[error("sink write failed after {bytes_transferred} bytes: {source}")]
    Sink {
        bytes_transferred: u64,
        #[source]
        source: SinkError,
    },
    #[error("sink finalize failed after {bytes_transferred} bytes: {source}")]
    Finalize {
        bytes_transferred: u64,
        #[source]
        source: SinkError,
    },
}

impl PumpError {
    /// Bytes that reached the sink before this failure — present on every
    /// variant so a caller never has to match to find it.
    pub fn bytes_transferred(&self) -> u64 {
        match self {
            PumpError::Source {
                bytes_transferred, ..
            }
            | PumpError::Sink {
                bytes_transferred, ..
            }
            | PumpError::Finalize {
                bytes_transferred, ..
            } => *bytes_transferred,
        }
    }
}

/// A sink that consumes a pump's byte stream, in order.
///
/// `finalize` is only reached after the source stream ends cleanly (a
/// zero-length read — EOF). On any earlier error the pump calls neither
/// `finalize` nor any rollback method: sinks own their own error-path
/// cleanup. [`CasSink`]'s underlying `StreamingWriter` actively unlinks its
/// staging file on `Drop` if `finalize` never ran; [`VfsSink`] leaves a
/// short, truncated destination file — exactly what a local `cp` leaves
/// behind on a source read error, no worse.
#[allow(async_fn_in_trait)]
pub trait PumpSink: Send {
    /// What a completed sink hands back — `()` for a plain VFS copy, a
    /// [`kaijutsu_cas::SealResult`] for the CAS sink.
    type Finalized;

    /// Consume the next chunk. No offset parameter — chunks arrive in
    /// stream order and the sink tracks its own write cursor.
    async fn write_chunk(&mut self, data: &[u8]) -> Result<(), SinkError>;

    /// Complete the sink now that the source has reached clean EOF.
    async fn finalize(self) -> Result<Self::Finalized, SinkError>;
}

/// Outcome of a successful [`pump`] run.
#[derive(Debug)]
pub struct PumpOutcome<F> {
    pub bytes_transferred: u64,
    pub finalized: F,
}

/// Drive `source.open_read_stream(src_path)` into `sink`, chunk by chunk,
/// with bounded memory (never more than one chunk resident at a time).
///
/// `source` is generally the kernel's whole `MountTable` (itself a
/// `VfsOps`) — `src_path` and whatever path `sink` writes to are resolved
/// independently by the same routing, so a pump between two mounts (two
/// `MemoryBackend`s, or a local mount and a future `ShareFs`) is just two
/// different paths through one `Arc<dyn VfsOps>`.
pub async fn pump<S: PumpSink>(
    source: &Arc<dyn VfsOps>,
    src_path: &Path,
    mut sink: S,
) -> Result<PumpOutcome<S::Finalized>, PumpError> {
    let mut bytes_transferred: u64 = 0;
    {
        let mut stream = source.open_read_stream(src_path);
        while let Some(item) = stream.next().await {
            match item {
                Ok(chunk) => {
                    sink.write_chunk(&chunk).await.map_err(|source| PumpError::Sink {
                        bytes_transferred,
                        source,
                    })?;
                    bytes_transferred += chunk.len() as u64;
                }
                Err(source) => {
                    return Err(PumpError::Source {
                        bytes_transferred,
                        source,
                    });
                }
            }
        }
        // `stream` borrows `source`; drop it before `sink.finalize()` so the
        // scope makes plain that the source is fully released by then (no
        // functional requirement — `source` is a shared `Arc` — but it keeps
        // the "read phase, then finalize phase" structure honest to read).
    }
    let finalized = sink.finalize().await.map_err(|source| PumpError::Finalize {
        bytes_transferred,
        source,
    })?;
    Ok(PumpOutcome {
        bytes_transferred,
        finalized,
    })
}

/// The `cp` sink: writes sequentially at an internally-tracked offset into
/// a VFS destination path, creating it if absent (truncating first if it
/// already exists — cp(1)'s overwrite semantics, not an append).
/// `finalize` is a no-op: a VFS write is durable the moment it lands, unlike
/// the CAS sink's staged rename.
pub struct VfsSink {
    dest: Arc<dyn VfsOps>,
    path: PathBuf,
    offset: u64,
}

impl VfsSink {
    /// Create (or truncate) `path` on `dest` and return a sink ready for
    /// [`PumpSink::write_chunk`].
    pub async fn create(dest: Arc<dyn VfsOps>, path: PathBuf) -> Result<Self, SinkError> {
        if dest.exists(&path).await {
            dest.truncate(&path, 0).await?;
        } else {
            dest.create(&path, 0o644).await?;
        }
        Ok(Self {
            dest,
            path,
            offset: 0,
        })
    }
}

impl PumpSink for VfsSink {
    type Finalized = ();

    async fn write_chunk(&mut self, data: &[u8]) -> Result<(), SinkError> {
        // `VfsOps::write` can itself return short (same contract as `read`) —
        // loop until every byte of this chunk has actually landed rather than
        // assuming a single call drains it.
        let mut written = 0usize;
        while written < data.len() {
            let n = self.dest.write(&self.path, self.offset, &data[written..]).await?;
            if n == 0 {
                return Err(SinkError::Io(std::io::Error::new(
                    std::io::ErrorKind::WriteZero,
                    "destination write returned 0 bytes with more data remaining",
                )));
            }
            self.offset += n as u64;
            written += n as usize;
        }
        Ok(())
    }

    async fn finalize(self) -> Result<Self::Finalized, SinkError> {
        Ok(())
    }
}

/// The `cas put` sink: streams into [`kaijutsu_cas::StreamingWriter`] —
/// incremental hashing, staged file discarded on drop if `finalize` never
/// runs (`docs/slash-r.md` slice 0).
pub struct CasSink {
    writer: kaijutsu_cas::StreamingWriter,
}

impl CasSink {
    pub fn create(
        store: &kaijutsu_cas::FileStore,
        mime_type: impl Into<String>,
    ) -> Result<Self, SinkError> {
        Ok(Self {
            writer: store.create_streaming_writer(mime_type)?,
        })
    }
}

impl PumpSink for CasSink {
    type Finalized = kaijutsu_cas::SealResult;

    async fn write_chunk(&mut self, data: &[u8]) -> Result<(), SinkError> {
        self.writer.write(data).map_err(SinkError::from)
    }

    async fn finalize(self) -> Result<Self::Finalized, SinkError> {
        self.writer.finalize().map_err(SinkError::from)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vfs::{DirEntry, FileAttr, MemoryBackend, MountTable, SetAttr, StatFs, VfsResult};
    use async_trait::async_trait;
    use kaijutsu_cas::ContentStore;
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;

    // ========================================================================
    // Fault-injection wrapper — wraps a MemoryBackend and, on `read`,
    // consumes a queued fault instead of (or in addition to) the real read.
    // Faults are consumed in FIFO order across every `read` call on this
    // backend, regardless of path — tests here only ever touch one path at a
    // time, so this stays simple.
    // ========================================================================

    enum Fault {
        /// Return a real read from `inner`, but capped to at most `n` bytes —
        /// a legal SHORT read (non-empty, less than requested) before EOF.
        ShortenTo(u32),
        /// Return `Ok(vec![])` regardless of what `inner` would have
        /// returned — an early, honest EOF the caller must respect.
        ZeroAt,
        /// Return an error instead of calling `inner` at all.
        ErrorAt,
    }

    struct FaultyBackend {
        inner: Arc<dyn VfsOps>,
        faults: Mutex<VecDeque<Fault>>,
    }

    impl FaultyBackend {
        fn new(inner: Arc<dyn VfsOps>, faults: Vec<Fault>) -> Self {
            Self {
                inner,
                faults: Mutex::new(faults.into()),
            }
        }
    }

    #[async_trait]
    impl VfsOps for FaultyBackend {
        async fn getattr(&self, path: &Path) -> VfsResult<FileAttr> {
            self.inner.getattr(path).await
        }
        async fn readdir(&self, path: &Path) -> VfsResult<Vec<DirEntry>> {
            self.inner.readdir(path).await
        }
        async fn read(&self, path: &Path, offset: u64, size: u32) -> VfsResult<Vec<u8>> {
            let fault = self.faults.lock().unwrap().pop_front();
            match fault {
                Some(Fault::ErrorAt) => Err(VfsError::other("injected fault: read error")),
                Some(Fault::ZeroAt) => Ok(Vec::new()),
                Some(Fault::ShortenTo(n)) => {
                    let full = self.inner.read(path, offset, size).await?;
                    Ok(full.into_iter().take(n as usize).collect())
                }
                None => self.inner.read(path, offset, size).await,
            }
        }
        async fn readlink(&self, path: &Path) -> VfsResult<PathBuf> {
            self.inner.readlink(path).await
        }
        async fn write(&self, path: &Path, offset: u64, data: &[u8]) -> VfsResult<u32> {
            self.inner.write(path, offset, data).await
        }
        async fn create(&self, path: &Path, mode: u32) -> VfsResult<FileAttr> {
            self.inner.create(path, mode).await
        }
        async fn mkdir(&self, path: &Path, mode: u32) -> VfsResult<FileAttr> {
            self.inner.mkdir(path, mode).await
        }
        async fn unlink(&self, path: &Path) -> VfsResult<()> {
            self.inner.unlink(path).await
        }
        async fn rmdir(&self, path: &Path) -> VfsResult<()> {
            self.inner.rmdir(path).await
        }
        async fn rename(&self, from: &Path, to: &Path) -> VfsResult<()> {
            self.inner.rename(from, to).await
        }
        async fn truncate(&self, path: &Path, size: u64) -> VfsResult<()> {
            self.inner.truncate(path, size).await
        }
        async fn setattr(&self, path: &Path, attr: SetAttr) -> VfsResult<FileAttr> {
            self.inner.setattr(path, attr).await
        }
        async fn symlink(&self, path: &Path, target: &Path) -> VfsResult<FileAttr> {
            self.inner.symlink(path, target).await
        }
        async fn link(&self, oldpath: &Path, newpath: &Path) -> VfsResult<FileAttr> {
            self.inner.link(oldpath, newpath).await
        }
        fn read_only(&self) -> bool {
            self.inner.read_only()
        }
        async fn statfs(&self) -> VfsResult<StatFs> {
            self.inner.statfs().await
        }
        async fn real_path(&self, path: &Path) -> VfsResult<Option<PathBuf>> {
            self.inner.real_path(path).await
        }
    }

    // ========================================================================
    // Probe backend — overrides `open_read_stream` distinctively (returns
    // the WHOLE file as a single chunk, and counts invocations) so a test
    // can prove `MountTable::open_read_stream` reached this override rather
    // than falling back to the trait's default loop-`read` — the exact
    // regression the design doc calls out (`docs/slash-r.md` slice 0).
    // ========================================================================

    struct OverrideProbeBackend {
        inner: Arc<MemoryBackend>,
        // Shared with the test via a clone taken before the backend moves
        // into the MountTable, so the invocation count stays observable
        // after `mount_arc` takes ownership of the `Arc<dyn VfsOps>`.
        calls: Arc<AtomicUsize>,
    }

    impl OverrideProbeBackend {
        fn new(inner: MemoryBackend, calls: Arc<AtomicUsize>) -> Self {
            Self {
                inner: Arc::new(inner),
                calls,
            }
        }
    }

    #[async_trait]
    impl VfsOps for OverrideProbeBackend {
        async fn getattr(&self, path: &Path) -> VfsResult<FileAttr> {
            self.inner.getattr(path).await
        }
        async fn readdir(&self, path: &Path) -> VfsResult<Vec<DirEntry>> {
            self.inner.readdir(path).await
        }
        async fn read(&self, path: &Path, offset: u64, size: u32) -> VfsResult<Vec<u8>> {
            self.inner.read(path, offset, size).await
        }
        async fn readlink(&self, path: &Path) -> VfsResult<PathBuf> {
            self.inner.readlink(path).await
        }
        async fn write(&self, path: &Path, offset: u64, data: &[u8]) -> VfsResult<u32> {
            self.inner.write(path, offset, data).await
        }
        async fn create(&self, path: &Path, mode: u32) -> VfsResult<FileAttr> {
            self.inner.create(path, mode).await
        }
        async fn mkdir(&self, path: &Path, mode: u32) -> VfsResult<FileAttr> {
            self.inner.mkdir(path, mode).await
        }
        async fn unlink(&self, path: &Path) -> VfsResult<()> {
            self.inner.unlink(path).await
        }
        async fn rmdir(&self, path: &Path) -> VfsResult<()> {
            self.inner.rmdir(path).await
        }
        async fn rename(&self, from: &Path, to: &Path) -> VfsResult<()> {
            self.inner.rename(from, to).await
        }
        async fn truncate(&self, path: &Path, size: u64) -> VfsResult<()> {
            self.inner.truncate(path, size).await
        }
        async fn setattr(&self, path: &Path, attr: SetAttr) -> VfsResult<FileAttr> {
            self.inner.setattr(path, attr).await
        }
        async fn symlink(&self, path: &Path, target: &Path) -> VfsResult<FileAttr> {
            self.inner.symlink(path, target).await
        }
        async fn link(&self, oldpath: &Path, newpath: &Path) -> VfsResult<FileAttr> {
            self.inner.link(oldpath, newpath).await
        }
        fn read_only(&self) -> bool {
            self.inner.read_only()
        }
        async fn statfs(&self) -> VfsResult<StatFs> {
            self.inner.statfs().await
        }
        async fn real_path(&self, path: &Path) -> VfsResult<Option<PathBuf>> {
            self.inner.real_path(path).await
        }

        fn open_read_stream<'a>(
            &'a self,
            path: &'a Path,
        ) -> futures::stream::BoxStream<'a, VfsResult<bytes::Bytes>> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let inner = self.inner.clone();
            let path = path.to_path_buf();
            Box::pin(futures::stream::once(async move {
                inner.read_all(&path).await.map(bytes::Bytes::from)
            }))
        }
    }

    fn vfs_result_bytes(v: Vec<u8>) -> Vec<u8> {
        v
    }

    // ------------------------------------------------------------------
    // 1. EOF contract: default loop stops (successfully) at a zero-length
    //    read, without requesting past it.
    // ------------------------------------------------------------------
    #[tokio::test]
    async fn default_stream_stops_cleanly_at_eof() {
        let backend: Arc<dyn VfsOps> = Arc::new(MemoryBackend::new());
        backend.create(Path::new("/f"), 0o644).await.unwrap();
        backend.write(Path::new("/f"), 0, b"hello").await.unwrap();

        let mut stream = backend.open_read_stream(Path::new("/f"));
        let mut collected = Vec::new();
        while let Some(item) = stream.next().await {
            collected.extend_from_slice(&item.expect("no fault injected"));
        }
        assert_eq!(collected, b"hello");
    }

    // ------------------------------------------------------------------
    // 2. Short-read contract: a short (non-empty, non-EOF) read advances
    //    the offset by the ACTUAL bytes returned, and the stream keeps
    //    pulling until true EOF rather than stopping early or re-requesting
    //    the same bytes.
    // ------------------------------------------------------------------
    #[tokio::test]
    async fn short_read_advances_by_actual_length_and_continues() {
        let mem: Arc<dyn VfsOps> = Arc::new(MemoryBackend::new());
        mem.create(Path::new("/f"), 0o644).await.unwrap();
        mem.write(Path::new("/f"), 0, b"0123456789").await.unwrap();

        // First read call: force a short read of 3 bytes (of the up-to-256KiB
        // requested). Second call onward: real reads to true EOF.
        let faulty: Arc<dyn VfsOps> =
            Arc::new(FaultyBackend::new(mem, vec![Fault::ShortenTo(3)]));

        let mut stream = faulty.open_read_stream(Path::new("/f"));
        let mut collected = Vec::new();
        let mut chunk_count = 0;
        while let Some(item) = stream.next().await {
            let bytes = item.expect("no error injected");
            chunk_count += 1;
            collected.extend_from_slice(&bytes);
        }
        assert_eq!(vfs_result_bytes(collected), b"0123456789");
        // Short first chunk (3 bytes) + the rest in a second chunk + the
        // final zero-length EOF poll (which yields no item) — proves the
        // pump kept pulling past the short read instead of treating it as
        // the end.
        assert!(chunk_count >= 2, "expected the short read to be followed by more chunks, got {chunk_count}");
    }

    // ------------------------------------------------------------------
    // 3. An early, honest zero-length read mid-file is EOF — the stream
    //    stops there even though more data technically exists past it.
    // ------------------------------------------------------------------
    #[tokio::test]
    async fn zero_length_read_is_always_eof_even_mid_file() {
        let mem: Arc<dyn VfsOps> = Arc::new(MemoryBackend::new());
        mem.create(Path::new("/f"), 0o644).await.unwrap();
        mem.write(Path::new("/f"), 0, b"abcdef").await.unwrap();

        let faulty: Arc<dyn VfsOps> = Arc::new(FaultyBackend::new(mem, vec![Fault::ZeroAt]));

        let mut stream = faulty.open_read_stream(Path::new("/f"));
        let first = stream.next().await;
        assert!(first.is_none(), "a zero-length first read must be treated as immediate EOF");
    }

    // ------------------------------------------------------------------
    // 4. A mid-stream source error surfaces as PumpError::Source and
    //    reports exactly how many bytes reached the sink first.
    // ------------------------------------------------------------------
    #[tokio::test]
    async fn pump_error_carries_bytes_written_on_source_failure() {
        let mem: Arc<dyn VfsOps> = Arc::new(MemoryBackend::new());
        mem.create(Path::new("/src"), 0o644).await.unwrap();
        let payload = vec![0xABu8; 10];
        mem.write(Path::new("/src"), 0, &payload).await.unwrap();

        // Chunk 1: a short (partial) real read. Chunk 2: error.
        let faulty: Arc<dyn VfsOps> = Arc::new(FaultyBackend::new(
            mem.clone(),
            vec![Fault::ShortenTo(4), Fault::ErrorAt],
        ));

        mem.create(Path::new("/dst"), 0o644).await.unwrap();
        let sink = VfsSink::create(mem.clone(), PathBuf::from("/dst")).await.unwrap();

        let err = pump(&faulty, Path::new("/src"), sink)
            .await
            .expect_err("injected source error must abort the pump");
        assert_eq!(err.bytes_transferred(), 4, "must report exactly the 4 bytes written before the fault");
        assert!(matches!(err, PumpError::Source { .. }));
    }

    // ------------------------------------------------------------------
    // 5. CAS sink: a mid-stream source error means the staging file is
    //    discarded (never renamed into objects/, and no residue left in
    //    staging/) — the CasSink's underlying StreamingWriter is simply
    //    dropped without `finalize`.
    // ------------------------------------------------------------------
    #[tokio::test]
    async fn cas_sink_discards_staging_on_mid_stream_error() {
        let temp = tempfile::tempdir().unwrap();
        let store = kaijutsu_cas::FileStore::at_path(temp.path());

        let mem: Arc<dyn VfsOps> = Arc::new(MemoryBackend::new());
        mem.create(Path::new("/src"), 0o644).await.unwrap();
        mem.write(Path::new("/src"), 0, b"partial content before boom")
            .await
            .unwrap();

        let faulty: Arc<dyn VfsOps> =
            Arc::new(FaultyBackend::new(mem, vec![Fault::ShortenTo(5), Fault::ErrorAt]));

        let sink = CasSink::create(&store, "application/octet-stream").unwrap();
        let err = pump(&faulty, Path::new("/src"), sink)
            .await
            .expect_err("injected error must abort the pump");
        assert_eq!(err.bytes_transferred(), 5);

        let staging_dir = store.config().staging_dir();
        let residue = walk_files(&staging_dir);
        assert!(residue.is_empty(), "staging must be clean after an aborted CAS pump, found: {residue:?}");
    }

    fn walk_files(dir: &Path) -> Vec<PathBuf> {
        let mut out = Vec::new();
        let mut stack = match std::fs::read_dir(dir) {
            Ok(rd) => rd.flatten().collect::<Vec<_>>(),
            Err(_) => return out,
        };
        while let Some(entry) = stack.pop() {
            let path = entry.path();
            if path.is_dir() {
                stack.extend(std::fs::read_dir(&path).unwrap().flatten());
            } else {
                out.push(path);
            }
        }
        out
    }

    // ------------------------------------------------------------------
    // 6. CAS Drop unlinks staging directly: a StreamingWriter dropped
    //    without ever calling finalize leaves nothing behind.
    // ------------------------------------------------------------------
    #[tokio::test]
    async fn cas_streaming_writer_drop_unlinks_unfinalized_staging() {
        let temp = tempfile::tempdir().unwrap();
        let store = kaijutsu_cas::FileStore::at_path(temp.path());

        {
            let mut writer = store.create_streaming_writer("text/plain").unwrap();
            writer.write(b"never finalized").unwrap();
            // dropped here, no finalize() call
        }

        let staging_dir = store.config().staging_dir();
        let residue = walk_files(&staging_dir);
        assert!(residue.is_empty(), "Drop must unlink the partial staging file, found: {residue:?}");
    }

    // ------------------------------------------------------------------
    // 7. Successful end-to-end pump across two MemoryBackends mounted
    //    under one MountTable — proves MountTable's `read` delegation AND
    //    (via the source path routing through the SAME Arc<dyn VfsOps>)
    //    that a pump between two mounts is just two paths on one table.
    // ------------------------------------------------------------------
    #[tokio::test]
    async fn pump_end_to_end_across_two_mounts() {
        let table = MountTable::new();
        table.mount("/a", MemoryBackend::new()).await;
        table.mount("/b", MemoryBackend::new()).await;
        let table: Arc<dyn VfsOps> = Arc::new(table);

        table.create(Path::new("/a/file.bin"), 0o644).await.unwrap();
        let payload: Vec<u8> = (0..2000u32).map(|i| (i % 251) as u8).collect();
        table.write(Path::new("/a/file.bin"), 0, &payload).await.unwrap();

        let sink = VfsSink::create(table.clone(), PathBuf::from("/b/file.bin"))
            .await
            .unwrap();
        let outcome = pump(&table, Path::new("/a/file.bin"), sink)
            .await
            .expect("cross-mount pump must succeed");
        assert_eq!(outcome.bytes_transferred, payload.len() as u64);

        let roundtrip = table.read_all(Path::new("/b/file.bin")).await.unwrap();
        assert_eq!(roundtrip, payload);
    }

    // ------------------------------------------------------------------
    // 8. MountTable::open_read_stream reaches the OWNING BACKEND'S OWN
    //    override rather than silently falling back to the trait default —
    //    the exact regression the design doc calls out.
    // ------------------------------------------------------------------
    #[tokio::test]
    async fn mount_table_delegates_to_backend_stream_override() {
        // Grab a handle to the counter before the probe moves into the table.
        let calls = Arc::new(AtomicUsize::new(0));
        let probe = OverrideProbeBackend::new(MemoryBackend::new(), calls.clone());
        let table = MountTable::new();
        table.mount_arc("/probe", Arc::new(probe)).await;

        table
            .create(Path::new("/probe/f"), 0o644)
            .await
            .unwrap();
        let payload = b"delegated stream override payload".to_vec();
        table.write(Path::new("/probe/f"), 0, &payload).await.unwrap();

        assert_eq!(calls.load(Ordering::SeqCst), 0, "no stream opened yet");

        let mut stream = table.open_read_stream(Path::new("/probe/f"));
        let mut collected = Vec::new();
        while let Some(item) = stream.next().await {
            collected.extend_from_slice(&item.unwrap());
        }
        assert_eq!(collected, payload);
        // Exactly one call: the probe's override returns the WHOLE file as a
        // single chunk and is invoked once per `open_read_stream` — if
        // `MountTable` had instead fallen back to the trait default (looping
        // the backend's `read`), this override (and its counter) would never
        // be touched at all, and `collected` would still happen to match by
        // coincidence — the counter is what actually proves delegation.
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "MountTable::open_read_stream must call the owning backend's own override exactly once"
        );
    }

    // ------------------------------------------------------------------
    // 9. CAS ingest happy path: pump a VFS file straight into CasSink and
    //    confirm the resulting hash matches storing the same bytes in one
    //    shot (proves the incremental hash is equivalent, not a distinct
    //    algorithm).
    // ------------------------------------------------------------------
    #[tokio::test]
    async fn pump_into_cas_sink_matches_oneshot_hash() {
        let temp = tempfile::tempdir().unwrap();
        let store = kaijutsu_cas::FileStore::at_path(temp.path());

        let mem: Arc<dyn VfsOps> = Arc::new(MemoryBackend::new());
        mem.create(Path::new("/src"), 0o644).await.unwrap();
        let payload: Vec<u8> = (0..5000u32).map(|i| (i % 200) as u8).collect();
        mem.write(Path::new("/src"), 0, &payload).await.unwrap();

        let sink = CasSink::create(&store, "application/octet-stream").unwrap();
        let outcome = pump(&mem, Path::new("/src"), sink).await.unwrap();

        let expected_hash = kaijutsu_cas::ContentHash::from_data(&payload);
        assert_eq!(outcome.finalized.content_hash, expected_hash);
        assert_eq!(outcome.finalized.size_bytes, payload.len() as u64);
        assert_eq!(outcome.bytes_transferred, payload.len() as u64);

        let retrieved = store.retrieve(&expected_hash).unwrap().unwrap();
        assert_eq!(retrieved, payload);
    }
}
