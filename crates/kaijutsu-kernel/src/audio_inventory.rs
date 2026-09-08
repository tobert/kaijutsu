//! Sink-fed audio inventory — the ephemeral projection of what an audio
//! daemon observes (`docs/audio-daemon.md` "One inventory owner").
//!
//! Division of labour, mirroring `midi_presence.rs`: **the daemon observes,
//! the kernel records.** `kaijutsu-audiod` owns the local ALSA graph, builds
//! a full report (every endpoint, every wire, its own plumbing client ids)
//! and sends it over `reportAudioInventory`; this module is the kernel side
//! — an in-memory store plus the read-only `/run/audio` view over it. The
//! kernel never enumerates hardware itself.
//!
//! ## Ephemeral by construction
//!
//! Presence lives in a `RwLock<BTreeMap<…>>` and nowhere else — no document,
//! no host file, no disk. A kernel restart forgets everything, which is
//! correct: a kernel with no daemons connected knows nothing about what
//! hardware exists anywhere.
//!
//! ## Connection-bound, like MIDI presence
//!
//! A daemon that crashes never gets to send a final report, so a node's
//! inventory must not outlive the connection that reported it. Every record
//! carries the connection it arrived on; [`AudioInventoryStore::reap_connection`]
//! marks that connection's nodes `stale` rather than removing them (unlike
//! `MidiPresenceStore`, which removes on reap) — an audio node's last known
//! wiring is still useful to a reader once the daemon disappears, labeled so
//! nobody mistakes it for current.
//!
//! ## Ordering: connection first, then revision
//!
//! Two independent orderings decide whether a report is accepted:
//!
//! - **Across connections**: a report from an older connection than the one
//!   currently holding a node cannot replace or reap the newer one. `SessionId`
//!   is a UUIDv7, so "older" is simply a smaller id — no separate clock needed.
//! - **Within one connection**: `revision` must strictly increase; a report
//!   with a revision no greater than the one on file from the same connection
//!   is dropped as a duplicate or a reorder.
//!
//! A report from a strictly newer connection is always accepted regardless of
//! its revision — a fresh connection starts its own revision sequence, and
//! the old connection's ownership is definitionally stale the moment a newer
//! one reports.
//!
//! ## The kernel stamps receipt and staleness
//!
//! The daemon cannot know its own report's kernel receipt time or whether the
//! kernel will judge it stale later, so it sends `received_epoch_ns: 0` and
//! `stale: false` and the kernel overwrites both fields before serving the
//! projected body. Every other field is the daemon's own, verbatim.

use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};
use std::sync::RwLock;
use std::sync::atomic::{AtomicU64, Ordering};

use async_trait::async_trait;
use kaijutsu_types::SessionId;
use kaijutsu_types::paths::audio_node_dir;

use crate::vfs::{DirEntry, FileAttr, SetAttr, StatFs, VfsError, VfsOps, VfsResult};

/// The leaf filename under each node directory.
const INVENTORY_FILE: &str = "inventory.json";

/// Why an inventory report was refused. Loud by design — a malformed node key
/// or a report body that isn't even JSON would mint a projection nobody can
/// read or trust.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InventoryError {
    /// Empty, `.`/`..`, or slash-bearing node key (after `/` is stripped for
    /// the directory encoding, the raw node key still must not carry `..`).
    InvalidNode(String),
    /// The report body did not parse as a JSON object.
    InvalidReport(String),
}

impl std::fmt::Display for InventoryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            InventoryError::InvalidNode(n) => write!(
                f,
                "invalid node key '{n}': /run/audio holds one directory per node, \
                 encoded from a peer nick"
            ),
            InventoryError::InvalidReport(e) => {
                write!(f, "audio inventory report is not a JSON object: {e}")
            }
        }
    }
}

/// What [`AudioInventoryStore::record`] did with a report.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Recorded {
    /// Stored — this is now the node's latest known inventory.
    Stored,
    /// Dropped: an older connection tried to replace a newer connection's
    /// report, or the same connection sent a non-increasing revision.
    Ignored,
}

/// One node's accepted inventory report.
#[derive(Debug, Clone)]
pub struct AudioInventoryRecord {
    /// The peer nick as the daemon named itself, e.g. `"audio/moltar"` — not
    /// the directory encoding (see [`audio_node_dir`]).
    pub node: String,
    /// The reporting daemon's connection. The reaping key: dies with the
    /// connection, never chosen by the daemon.
    pub session_id: SessionId,
    /// Kernel-assigned acceptance sequence, strictly increasing across every
    /// node this store has ever accepted a report for — a coherence stamp
    /// independent of the daemon's own `revision`.
    pub accepted_seq: u64,
    /// Kernel wallclock (ns since UNIX_EPOCH) when this report was accepted.
    pub received_epoch_ns: u64,
    /// The daemon's own report revision — orders reports within one
    /// connection.
    pub revision: u64,
    /// The daemon's wallclock (ns since UNIX_EPOCH) at observation, in the
    /// kernel's clock domain.
    pub observed_epoch_ns: u64,
    /// The daemon's report body, verbatim, as sent — validated as a JSON
    /// object at [`AudioInventoryStore::record`] time but not otherwise
    /// altered until [`AudioInventoryRecord::to_bytes`] re-stamps it.
    pub report: Vec<u8>,
    /// The node's connection dropped and this is its last observation.
    pub stale: bool,
}

impl AudioInventoryRecord {
    /// The projected bytes served at
    /// `/run/audio/<node-dir>/inventory.json`: the daemon's report with
    /// `received_epoch_ns` and `stale` overwritten by the kernel's own
    /// values — the two fields only the kernel can know, so the daemon's own
    /// `0`/`false` for them are never what a reader sees.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut value: serde_json::Value = serde_json::from_slice(&self.report)
            .expect("report body was validated as a JSON object at record() time");
        value["received_epoch_ns"] = serde_json::json!(self.received_epoch_ns);
        value["stale"] = serde_json::Value::Bool(self.stale);
        let mut s = serde_json::to_string_pretty(&value)
            .expect("audio inventory JSON is always serializable");
        s.push('\n');
        s.into_bytes()
    }
}

/// The kernel-global, in-memory audio inventory store. One per kernel; held
/// behind an `Arc` on `Kernel`, mirroring `MidiPresenceStore`'s placement.
///
/// Keyed internally by the node's `/run/audio` directory encoding
/// ([`audio_node_dir`]) rather than the raw node key: the encoding is
/// deterministic and one-directional, and every lookup this store serves
/// (`/run/audio` listings, path resolution) arrives already encoded.
#[derive(Debug, Default)]
pub struct AudioInventoryStore {
    entries: RwLock<BTreeMap<String, AudioInventoryRecord>>,
    /// Monotonic coherence stamp for the `/run/audio` view.
    generation: AtomicU64,
    /// Source of [`AudioInventoryRecord::accepted_seq`].
    next_seq: AtomicU64,
}

impl AudioInventoryStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// A node key must be non-empty and must not resolve `..`/`.` segments
    /// once its directory encoding is split on `/` (the encoding itself
    /// replaces every `/` with `-`, so this only catches a key already
    /// invalid before encoding, e.g. embedded NUL).
    fn validate_node(node: &str) -> Result<(), InventoryError> {
        if node.is_empty() || node.contains('\0') {
            return Err(InventoryError::InvalidNode(node.to_string()));
        }
        Ok(())
    }

    /// Record one daemon report. See the module doc for the two orderings
    /// this enforces (connection first, then revision).
    pub fn record(
        &self,
        node: impl Into<String>,
        session_id: SessionId,
        revision: u64,
        observed_epoch_ns: u64,
        report: &[u8],
        received_epoch_ns: u64,
    ) -> Result<Recorded, InventoryError> {
        let node = node.into();
        Self::validate_node(&node)?;
        let parsed: serde_json::Value = serde_json::from_slice(report)
            .map_err(|e| InventoryError::InvalidReport(e.to_string()))?;
        if !parsed.is_object() {
            return Err(InventoryError::InvalidReport(
                "top-level value is not a JSON object".to_string(),
            ));
        }
        let dir = audio_node_dir(&node);

        let mut entries = self
            .entries
            .write()
            .expect("audio inventory lock poisoned (a writer panicked)");
        if let Some(existing) = entries.get(&dir) {
            if session_id < existing.session_id {
                return Ok(Recorded::Ignored);
            }
            if session_id == existing.session_id && revision <= existing.revision {
                return Ok(Recorded::Ignored);
            }
        }
        let accepted_seq = self.next_seq.fetch_add(1, Ordering::Relaxed);
        entries.insert(
            dir,
            AudioInventoryRecord {
                node,
                session_id,
                accepted_seq,
                received_epoch_ns,
                revision,
                observed_epoch_ns,
                report: report.to_vec(),
                stale: false,
            },
        );
        self.generation.fetch_add(1, Ordering::Relaxed);
        Ok(Recorded::Stored)
    }

    /// Mark every node this connection reported as stale, keeping the last
    /// observation rather than removing it (the audio-daemon rule differs
    /// from `MidiPresenceStore::reap_connection` on purpose — a node's last
    /// known wiring is still useful once the daemon disappears). Returns the
    /// affected node keys (ascending, for the caller's log); a connection
    /// that owns nothing, or whose nodes are already stale, changes nothing
    /// and bumps no generation.
    pub fn reap_connection(&self, session_id: SessionId) -> Vec<String> {
        let mut entries = self
            .entries
            .write()
            .expect("audio inventory lock poisoned (a writer panicked)");
        let mut affected = Vec::new();
        for record in entries.values_mut() {
            if record.session_id == session_id && !record.stale {
                record.stale = true;
                affected.push(record.node.clone());
            }
        }
        if !affected.is_empty() {
            affected.sort();
            self.generation.fetch_add(1, Ordering::Relaxed);
        }
        affected
    }

    /// One node's latest record, by its raw node key.
    pub fn get(&self, node: &str) -> Option<AudioInventoryRecord> {
        let dir = audio_node_dir(node);
        self.get_by_dir(&dir)
    }

    /// One node's latest record, by its `/run/audio` directory encoding.
    pub fn get_by_dir(&self, dir: &str) -> Option<AudioInventoryRecord> {
        self.entries
            .read()
            .expect("audio inventory lock poisoned")
            .get(dir)
            .cloned()
    }

    /// Every node we hold a record for, ascending by directory encoding.
    pub fn snapshot(&self) -> Vec<AudioInventoryRecord> {
        self.entries
            .read()
            .expect("audio inventory lock poisoned")
            .values()
            .cloned()
            .collect()
    }

    /// Node directory names only (the `/run/audio` directory listing).
    pub fn node_dirs(&self) -> Vec<String> {
        self.entries
            .read()
            .expect("audio inventory lock poisoned")
            .keys()
            .cloned()
            .collect()
    }

    pub fn is_empty(&self) -> bool {
        self.entries
            .read()
            .expect("audio inventory lock poisoned")
            .is_empty()
    }

    fn generation(&self) -> u64 {
        self.generation.load(Ordering::Relaxed)
    }
}

/// The read-only `/run/audio` view over an [`AudioInventoryStore`]. One
/// directory per node, each holding exactly one `inventory.json` leaf.
///
/// Read-only by construction, like `MidiPresenceFs`: the only writer is an
/// inventory report arriving over the wire.
pub struct AudioInventoryFs {
    store: std::sync::Arc<AudioInventoryStore>,
}

/// What a mount-relative path resolves to.
enum Resolved {
    /// The mount root (`/run/audio`).
    Root,
    /// One node's directory (`/run/audio/<node-dir>`).
    NodeDir(String),
    /// One node's inventory file (`/run/audio/<node-dir>/inventory.json`).
    File(String),
}

impl AudioInventoryFs {
    pub fn new(store: std::sync::Arc<AudioInventoryStore>) -> Self {
        Self { store }
    }

    fn segments(path: &Path) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for c in path.components() {
            match c {
                Component::Normal(s) => out.push(s.to_string_lossy().to_string()),
                Component::ParentDir => {
                    out.pop();
                }
                Component::RootDir | Component::CurDir | Component::Prefix(_) => {}
            }
        }
        out
    }

    fn resolve(&self, path: &Path) -> VfsResult<Resolved> {
        let segs = Self::segments(path);
        match segs.as_slice() {
            [] => Ok(Resolved::Root),
            [dir] => Ok(Resolved::NodeDir(dir.clone())),
            [dir, leaf] if leaf == INVENTORY_FILE => Ok(Resolved::File(dir.clone())),
            _ => Err(VfsError::not_found(segs.join("/"))),
        }
    }

    /// The record's projected bytes for one node directory, or `NotFound` —
    /// a node nobody has reported is genuinely absent from the tree.
    fn body(&self, dir: &str) -> VfsResult<Vec<u8>> {
        self.store
            .get_by_dir(dir)
            .map(|r| r.to_bytes())
            .ok_or_else(|| VfsError::not_found(dir.to_string()))
    }
}

#[async_trait]
impl VfsOps for AudioInventoryFs {
    async fn getattr(&self, path: &Path) -> VfsResult<FileAttr> {
        match self.resolve(path)? {
            Resolved::Root => Ok(FileAttr::directory(0o555)),
            Resolved::NodeDir(dir) => {
                // Confirm it exists before calling it a directory.
                self.body(&dir)?;
                Ok(FileAttr::directory(0o555))
            }
            Resolved::File(dir) => {
                let body = self.body(&dir)?;
                let mut attr = FileAttr::file(body.len() as u64, 0o444);
                attr.generation = self.store.generation();
                Ok(attr)
            }
        }
    }

    async fn readdir(&self, path: &Path) -> VfsResult<Vec<DirEntry>> {
        match self.resolve(path)? {
            Resolved::Root => Ok(self
                .store
                .node_dirs()
                .into_iter()
                .map(DirEntry::directory)
                .collect()),
            Resolved::NodeDir(dir) => {
                self.body(&dir)?;
                Ok(vec![DirEntry::file(INVENTORY_FILE.to_string())])
            }
            Resolved::File(dir) => {
                self.body(&dir)?;
                Err(VfsError::not_a_directory(format!("{dir}/{INVENTORY_FILE}")))
            }
        }
    }

    async fn read(&self, path: &Path, offset: u64, size: u32) -> VfsResult<Vec<u8>> {
        match self.resolve(path)? {
            Resolved::Root => Err(VfsError::is_a_directory("/".to_string())),
            Resolved::NodeDir(dir) => Err(VfsError::is_a_directory(dir)),
            Resolved::File(dir) => {
                let body = self.body(&dir)?;
                let start = (offset as usize).min(body.len());
                let end = start.saturating_add(size as usize).min(body.len());
                Ok(body[start..end].to_vec())
            }
        }
    }

    async fn readlink(&self, path: &Path) -> VfsResult<PathBuf> {
        Err(VfsError::NotASymlink(Self::segments(path).join("/")))
    }

    // ── writes: read-only by construction ──────────────────────────────────

    async fn write(&self, _path: &Path, _offset: u64, _data: &[u8]) -> VfsResult<u32> {
        Err(VfsError::ReadOnly)
    }

    async fn create(&self, _path: &Path, _mode: u32) -> VfsResult<FileAttr> {
        Err(VfsError::ReadOnly)
    }

    async fn mkdir(&self, _path: &Path, _mode: u32) -> VfsResult<FileAttr> {
        Err(VfsError::ReadOnly)
    }

    async fn unlink(&self, _path: &Path) -> VfsResult<()> {
        Err(VfsError::ReadOnly)
    }

    async fn rmdir(&self, _path: &Path) -> VfsResult<()> {
        Err(VfsError::ReadOnly)
    }

    async fn rename(&self, _from: &Path, _to: &Path) -> VfsResult<()> {
        Err(VfsError::ReadOnly)
    }

    async fn truncate(&self, _path: &Path, _size: u64) -> VfsResult<()> {
        Err(VfsError::ReadOnly)
    }

    async fn setattr(&self, _path: &Path, _attr: SetAttr) -> VfsResult<FileAttr> {
        Err(VfsError::ReadOnly)
    }

    async fn symlink(&self, _path: &Path, _target: &Path) -> VfsResult<FileAttr> {
        Err(VfsError::ReadOnly)
    }

    async fn link(&self, _oldpath: &Path, _newpath: &Path) -> VfsResult<FileAttr> {
        Err(VfsError::ReadOnly)
    }

    // ── metadata ───────────────────────────────────────────────────────────

    fn read_only(&self) -> bool {
        true
    }

    async fn statfs(&self) -> VfsResult<StatFs> {
        Ok(StatFs::default())
    }

    async fn real_path(&self, _path: &Path) -> VfsResult<Option<PathBuf>> {
        // Synthesized from memory; there is no host file behind it, ever.
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn report_json(revision: u64) -> Vec<u8> {
        serde_json::json!({
            "node": "audio/moltar",
            "revision": revision,
            "observed_epoch_ns": 1_000_000_000u64,
            "received_epoch_ns": 0,
            "stale": false,
            "backend": "alsa",
            "state": "ready",
            "own_clients": [130, 131],
            "endpoints": [],
            "wires": [],
        })
        .to_string()
        .into_bytes()
    }

    fn record(
        store: &AudioInventoryStore,
        node: &str,
        session: SessionId,
        revision: u64,
    ) -> Result<Recorded, InventoryError> {
        store.record(node, session, revision, 1_000_000_000, &report_json(revision), 5_000)
    }

    #[test]
    fn a_reported_node_reads_back_with_kernel_stamped_fields() {
        let store = AudioInventoryStore::new();
        let session = SessionId::new();
        assert_eq!(record(&store, "audio/moltar", session, 1), Ok(Recorded::Stored));

        let got = store.get("audio/moltar").expect("recorded");
        assert_eq!(got.revision, 1);
        assert!(!got.stale);
        assert_eq!(got.received_epoch_ns, 5_000);

        let bytes = got.to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["received_epoch_ns"], 5_000, "kernel stamp overwrites the daemon's 0");
        assert_eq!(json["stale"], false);
    }

    #[test]
    fn an_older_session_cannot_replace_a_newer_one() {
        let store = AudioInventoryStore::new();
        let old = SessionId::new();
        std::thread::sleep(std::time::Duration::from_millis(2));
        let new = SessionId::new();
        assert!(old < new, "UUIDv7 session ids must sort by creation time");

        record(&store, "audio/moltar", new, 1).unwrap();
        assert_eq!(
            record(&store, "audio/moltar", old, 99),
            Ok(Recorded::Ignored),
            "an older connection's report, even at a higher revision, must not replace the newer one"
        );
        assert_eq!(store.get("audio/moltar").unwrap().session_id, new);
        assert_eq!(store.get("audio/moltar").unwrap().revision, 1);
    }

    #[test]
    fn a_same_session_lower_or_equal_revision_is_ignored() {
        let store = AudioInventoryStore::new();
        let session = SessionId::new();
        record(&store, "audio/moltar", session, 5).unwrap();

        assert_eq!(record(&store, "audio/moltar", session, 5), Ok(Recorded::Ignored));
        assert_eq!(record(&store, "audio/moltar", session, 3), Ok(Recorded::Ignored));
        assert_eq!(store.get("audio/moltar").unwrap().revision, 5);

        assert_eq!(record(&store, "audio/moltar", session, 6), Ok(Recorded::Stored));
        assert_eq!(store.get("audio/moltar").unwrap().revision, 6);
    }

    #[test]
    fn reaping_marks_stale_and_keeps_the_body() {
        let store = AudioInventoryStore::new();
        let session = SessionId::new();
        record(&store, "audio/moltar", session, 1).unwrap();

        let reaped = store.reap_connection(session);
        assert_eq!(reaped, vec!["audio/moltar".to_string()]);

        let got = store.get("audio/moltar").expect("reap keeps the last observation");
        assert!(got.stale);
        assert_eq!(got.revision, 1, "the body is unchanged, only staleness flips");

        let bytes = got.to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["stale"], true);
    }

    #[test]
    fn reaping_a_connection_that_owns_nothing_is_a_no_op() {
        let store = AudioInventoryStore::new();
        let session = SessionId::new();
        record(&store, "audio/moltar", session, 1).unwrap();
        let g = store.generation();

        assert!(store.reap_connection(SessionId::new()).is_empty());
        assert_eq!(store.generation(), g, "a no-op reap must not bump generation");
    }

    #[test]
    fn a_new_session_after_a_stale_one_takes_over() {
        let store = AudioInventoryStore::new();
        let old = SessionId::new();
        record(&store, "audio/moltar", old, 1).unwrap();
        store.reap_connection(old);
        assert!(store.get("audio/moltar").unwrap().stale);

        std::thread::sleep(std::time::Duration::from_millis(2));
        let new = SessionId::new();
        assert_eq!(
            record(&store, "audio/moltar", new, 1),
            Ok(Recorded::Stored),
            "a newer connection takes over even at revision 1 again"
        );
        let got = store.get("audio/moltar").unwrap();
        assert!(!got.stale, "the fresh report clears staleness");
        assert_eq!(got.session_id, new);
    }

    #[test]
    fn reaping_only_affects_its_own_connections_nodes() {
        let store = AudioInventoryStore::new();
        let moltar = SessionId::new();
        let zorak = SessionId::new();
        record(&store, "audio/moltar", moltar, 1).unwrap();
        record(&store, "audio/zorak", zorak, 1).unwrap();

        let reaped = store.reap_connection(moltar);
        assert_eq!(reaped, vec!["audio/moltar".to_string()]);
        assert!(store.get("audio/moltar").unwrap().stale);
        assert!(!store.get("audio/zorak").unwrap().stale, "the other node's connection is unaffected");
    }

    #[test]
    fn a_malformed_node_key_is_refused_loudly() {
        let store = AudioInventoryStore::new();
        assert_eq!(
            record(&store, "", SessionId::new(), 1),
            Err(InventoryError::InvalidNode(String::new()))
        );
    }

    #[test]
    fn a_non_json_report_is_refused_loudly() {
        let store = AudioInventoryStore::new();
        let err = store.record("audio/moltar", SessionId::new(), 1, 0, b"not json", 0);
        assert!(matches!(err, Err(InventoryError::InvalidReport(_))));
        assert!(store.is_empty());
    }

    // ── the /run/audio view ────────────────────────────────────────────────

    fn fs() -> (Arc<AudioInventoryStore>, AudioInventoryFs) {
        let store = Arc::new(AudioInventoryStore::new());
        (store.clone(), AudioInventoryFs::new(store))
    }

    #[tokio::test]
    async fn the_view_renders_one_directory_per_reported_node() {
        let (store, fs) = fs();
        record(&store, "audio/moltar", SessionId::new(), 1).unwrap();
        record(&store, "audio/zorak", SessionId::new(), 1).unwrap();

        let names: Vec<String> = fs
            .readdir(Path::new(""))
            .await
            .unwrap()
            .into_iter()
            .map(|e| e.name)
            .collect();
        assert_eq!(names, vec!["audio-moltar".to_string(), "audio-zorak".into()]);

        let leaf: Vec<String> = fs
            .readdir(Path::new("audio-moltar"))
            .await
            .unwrap()
            .into_iter()
            .map(|e| e.name)
            .collect();
        assert_eq!(leaf, vec!["inventory.json".to_string()]);

        let body = fs.read_all(Path::new("audio-moltar/inventory.json")).await.unwrap();
        let parsed: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(parsed["node"], "audio/moltar");
        assert_eq!(parsed["stale"], false);
    }

    #[tokio::test]
    async fn a_fresh_view_lists_nothing_and_finds_nothing() {
        let (_store, fs) = fs();
        assert!(fs.readdir(Path::new("")).await.unwrap().is_empty());
        assert!(matches!(
            fs.getattr(Path::new("audio-moltar")).await,
            Err(VfsError::NotFound(_))
        ));
    }

    #[tokio::test]
    async fn the_view_refuses_every_write() {
        let (store, fs) = fs();
        record(&store, "audio/moltar", SessionId::new(), 1).unwrap();
        assert!(matches!(
            fs.write(Path::new("audio-moltar/inventory.json"), 0, b"lies").await,
            Err(VfsError::ReadOnly)
        ));
        assert!(matches!(
            fs.unlink(Path::new("audio-moltar/inventory.json")).await,
            Err(VfsError::ReadOnly)
        ));
        assert!(fs.read_only());
    }

    #[tokio::test]
    async fn generation_advances_on_each_report_and_on_reap() {
        let (store, fs) = fs();
        record(&store, "audio/moltar", SessionId::new(), 1).unwrap();
        let g1 = fs
            .getattr(Path::new("audio-moltar/inventory.json"))
            .await
            .unwrap()
            .generation;

        let session = SessionId::new();
        record(&store, "audio/moltar", session, 1).unwrap();
        // Different connection at the same revision still replaces (a fresh
        // connection always wins), so this must advance the view.
        let g2 = fs
            .getattr(Path::new("audio-moltar/inventory.json"))
            .await
            .unwrap()
            .generation;
        assert!(g2 > g1, "{g2} must exceed {g1}");

        store.reap_connection(session);
        let g3 = fs
            .getattr(Path::new("audio-moltar/inventory.json"))
            .await
            .unwrap()
            .generation;
        assert!(g3 > g2, "a reap must also advance the view's generation");
    }
}
