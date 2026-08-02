//! Sink-fed MIDI presence — the ephemeral half of a device
//! (`docs/midi-next.md` "Presence is sink-fed", slice 1 step 3).
//!
//! Division of labour, decided and not re-litigated here: **the app matches,
//! the kernel records.** A sink (today `kaijutsu-app`, tomorrow a CoreMIDI
//! one) watches platform hotplug, matches port facts against the match
//! strings in `/etc/midi/devices/<name>`, and reports
//! `{device, present, backend, ports, at}` over the wire
//! (`reportMidiPresence`). This module is the kernel side of that report: an
//! in-memory store plus the read-only `/run/midi` view over it. The kernel
//! never shells, never enumerates, never touches ALSA.
//!
//! ## Ephemeral by construction
//!
//! Presence lives in a `RwLock<BTreeMap<…>>` and nowhere else — no CRDT, no
//! host file, no disk. A kernel restart therefore *forgets*, which is the
//! correct answer: a kernel with no sinks connected knows nothing about what
//! is plugged in anywhere. Stale presence that lies is worse than no
//! presence, so the absence of an entry reads as **unknown** — never as
//! "absent" and never as a remembered "live".
//!
//! ## Provenance, not resolution
//!
//! Every fact renders as `{value, source, at}` with `source: "sink"` — the
//! doc's three-provenance scheme (`sent` / `observed` / `pulled` join it as
//! later slices land). Conflict handling is the documented mechanical rule
//! and nothing more: **latest timestamp wins**, an older report for a device
//! we already have a newer one for is dropped (and the caller told so it can
//! log). No resolution engine; the consumer reads the provenance and judges.
//!
//! ## The `/run` decision
//!
//! `docs/midi-next.md` names `/run/midi/<device>`, and that is what
//! [`MidiPresenceFs`] mounts (`kaijutsu_types::paths::MIDI_RUN_ROOT`). It is
//! deliberately NOT under `/r` (that namespace names live *clients*, one
//! `ShareFs` routing to somebody's laptop) and not under `/v` (kaish-shared
//! virtual filesystems, where kaish itself claims names). `/run` is the unix
//! convention for exactly this — state that exists only while the process
//! does — and giving it its own top-level tree keeps the ephemerality legible
//! at the path.

use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};
use std::sync::RwLock;
use std::sync::atomic::{AtomicU64, Ordering};

use async_trait::async_trait;

use crate::vfs::{DirEntry, FileAttr, SetAttr, StatFs, VfsError, VfsOps, VfsResult};

/// Provenance tag for facts a sink reported (`docs/midi-next.md`: the other
/// two, `observed` and `pulled`, arrive with the ear mapping and `kj midi
/// pull`).
pub const SOURCE_SINK: &str = "sink";

/// One MIDI port as the reporting sink saw it. Backend-neutral: `name` is the
/// display name profile match strings match on; `address` is the backend's own
/// handle (`"client:port"` under ALSA, a unique id under CoreMIDI) — opaque to
/// the kernel, carried for logs and disambiguation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MidiPortFact {
    pub name: String,
    pub address: String,
}

/// One device's presence record, exactly as the reporting sink stated it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MidiPresenceRecord {
    /// Profile key — the `<name>` in both `/etc/midi/devices/<name>` and
    /// `/run/midi/<name>`.
    pub device: String,
    /// Live right now (as of `at_ns`) per the reporting sink. `false` is a
    /// first-class *observation* (unplug), not an omission.
    pub present: bool,
    /// The reporting sink's platform backend: `"alsa"`, `"coremidi"`, …
    pub backend: String,
    /// The ports that matched this device. Empty when `present` is false.
    pub ports: Vec<MidiPortFact>,
    /// The sink's wallclock (ns since UNIX_EPOCH) at observation.
    pub at_ns: u64,
    /// Provenance of every fact in this record ([`SOURCE_SINK`] today).
    pub source: String,
}

impl MidiPresenceRecord {
    /// A sink report, stamped with [`SOURCE_SINK`] provenance.
    pub fn from_sink(
        device: impl Into<String>,
        present: bool,
        backend: impl Into<String>,
        ports: Vec<MidiPortFact>,
        at_ns: u64,
    ) -> Self {
        Self {
            device: device.into(),
            present,
            backend: backend.into(),
            ports,
            at_ns,
            source: SOURCE_SINK.to_string(),
        }
    }

    /// The provenance-tagged JSON rendered at `/run/midi/<device>`: every
    /// fact is `{value, source, at}` so a reader (a device context's model,
    /// a kai section, `jq`) can judge freshness and origin per fact rather
    /// than trusting the record wholesale.
    pub fn to_json(&self) -> serde_json::Value {
        let tag = |value: serde_json::Value| {
            serde_json::json!({ "value": value, "source": self.source, "at": self.at_ns })
        };
        serde_json::json!({
            "v": 1,
            "device": self.device,
            "present": tag(serde_json::Value::Bool(self.present)),
            "backend": tag(serde_json::Value::String(self.backend.clone())),
            "ports": tag(serde_json::Value::Array(
                self.ports
                    .iter()
                    .map(|p| serde_json::json!({ "name": p.name, "address": p.address }))
                    .collect(),
            )),
        })
    }

    /// The bytes `/run/midi/<device>` serves: pretty JSON with a trailing
    /// newline, so `cat` in kaish reads like a file and not like a wire dump.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut s = serde_json::to_string_pretty(&self.to_json())
            .expect("presence record JSON is always serializable");
        s.push('\n');
        s.into_bytes()
    }
}

/// Why a presence report was refused. Loud by design — a malformed device key
/// would mint a `/run/midi` path nobody can address.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PresenceError {
    /// Empty, `.`/`..`, or slash-bearing device key.
    InvalidDevice(String),
}

impl std::fmt::Display for PresenceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PresenceError::InvalidDevice(d) => write!(
                f,
                "invalid device key '{d}': /run/midi is a flat namespace of \
                 profile names (e.g. keystep-pro)"
            ),
        }
    }
}

/// What [`MidiPresenceStore::record`] did with a report.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Recorded {
    /// Stored — this is now the device's latest known presence.
    Stored,
    /// Dropped: we already hold a *newer* report for this device. Latest
    /// timestamp wins mechanically (`docs/midi-next.md`); a late-arriving
    /// older observation must never overwrite a newer one.
    StaleDropped,
}

/// The kernel-global, in-memory presence store. One per kernel; cloned
/// nowhere (held behind an `Arc` on `Kernel`).
#[derive(Debug, Default)]
pub struct MidiPresenceStore {
    entries: RwLock<BTreeMap<String, MidiPresenceRecord>>,
    /// Monotonic coherence stamp for the `/run/midi` view — bumped on every
    /// accepted report so a caching reader sees strictly-advancing
    /// generations even inside one `SystemTime::now()` tick.
    generation: AtomicU64,
}

impl MidiPresenceStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// A device key is a flat, addressable `/run/midi` leaf name.
    fn validate_device(device: &str) -> Result<(), PresenceError> {
        if device.is_empty()
            || device.contains('/')
            || device == "."
            || device == ".."
            || device.contains('\0')
        {
            return Err(PresenceError::InvalidDevice(device.to_string()));
        }
        Ok(())
    }

    /// Record one sink report. Latest-timestamp-wins; an older report for a
    /// device we already hold newer news about is dropped, not merged.
    pub fn record(&self, record: MidiPresenceRecord) -> Result<Recorded, PresenceError> {
        Self::validate_device(&record.device)?;
        let mut entries = self
            .entries
            .write()
            .expect("midi presence lock poisoned (a writer panicked)");
        if let Some(existing) = entries.get(&record.device)
            && existing.at_ns > record.at_ns
        {
            return Ok(Recorded::StaleDropped);
        }
        self.generation.fetch_add(1, Ordering::Relaxed);
        entries.insert(record.device.clone(), record);
        Ok(Recorded::Stored)
    }

    /// One device's latest record, or `None` for **unknown** — no sink has
    /// ever reported it to this kernel (which a fresh kernel means literally).
    pub fn get(&self, device: &str) -> Option<MidiPresenceRecord> {
        self.entries
            .read()
            .expect("midi presence lock poisoned")
            .get(device)
            .cloned()
    }

    /// Every device we hold a record for, ascending by device key.
    pub fn snapshot(&self) -> Vec<MidiPresenceRecord> {
        self.entries
            .read()
            .expect("midi presence lock poisoned")
            .values()
            .cloned()
            .collect()
    }

    /// Device keys only (the `/run/midi` directory listing).
    pub fn devices(&self) -> Vec<String> {
        self.entries
            .read()
            .expect("midi presence lock poisoned")
            .keys()
            .cloned()
            .collect()
    }

    pub fn is_empty(&self) -> bool {
        self.entries
            .read()
            .expect("midi presence lock poisoned")
            .is_empty()
    }

    fn generation(&self) -> u64 {
        self.generation.load(Ordering::Relaxed)
    }
}

/// The read-only `/run/midi` view over a [`MidiPresenceStore`].
///
/// Read-only **by construction**, not by flag: the only writer is a presence
/// report arriving over the wire, so nothing that reaches the VFS (kaish, the
/// file tools, SFTP, a kai script) can invent or edit a presence fact. That is
/// what keeps `/run/midi` honest enough for a device context to reason from.
pub struct MidiPresenceFs {
    store: std::sync::Arc<MidiPresenceStore>,
}

/// What a mount-relative path resolves to.
enum Resolved {
    /// The mount root (`/run/midi`).
    Root,
    /// One device leaf (`/run/midi/<device>`).
    Device(String),
}

impl MidiPresenceFs {
    pub fn new(store: std::sync::Arc<MidiPresenceStore>) -> Self {
        Self { store }
    }

    /// Split a mount-relative path into clean `Normal` segments, resolving
    /// `.`/`..` and never escaping above the mount root.
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
            [device] => Ok(Resolved::Device(device.clone())),
            // Flat namespace: nothing nests under a device record.
            _ => Err(VfsError::not_found(segs.join("/"))),
        }
    }

    /// The record's bytes, or `NotFound` — an unreported device is genuinely
    /// absent from the tree rather than an empty file (unknown ≠ absent).
    fn body(&self, device: &str) -> VfsResult<Vec<u8>> {
        self.store
            .get(device)
            .map(|r| r.to_bytes())
            .ok_or_else(|| VfsError::not_found(device.to_string()))
    }
}

#[async_trait]
impl VfsOps for MidiPresenceFs {
    async fn getattr(&self, path: &Path) -> VfsResult<FileAttr> {
        match self.resolve(path)? {
            Resolved::Root => Ok(FileAttr::directory(0o555)),
            Resolved::Device(device) => {
                let body = self.body(&device)?;
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
                .devices()
                .into_iter()
                .map(DirEntry::file)
                .collect()),
            Resolved::Device(device) => {
                // Confirm it exists before calling it not-a-directory.
                self.body(&device)?;
                Err(VfsError::not_a_directory(device))
            }
        }
    }

    async fn read(&self, path: &Path, offset: u64, size: u32) -> VfsResult<Vec<u8>> {
        match self.resolve(path)? {
            Resolved::Root => Err(VfsError::is_a_directory("/".to_string())),
            Resolved::Device(device) => {
                let body = self.body(&device)?;
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

    fn port(name: &str, addr: &str) -> MidiPortFact {
        MidiPortFact { name: name.into(), address: addr.into() }
    }

    fn live(device: &str, at_ns: u64) -> MidiPresenceRecord {
        MidiPresenceRecord::from_sink(
            device,
            true,
            "alsa",
            vec![port("KeyStep Pro MIDI 1", "24:0")],
            at_ns,
        )
    }

    #[test]
    fn a_reported_device_reads_back_with_sink_provenance() {
        let store = MidiPresenceStore::new();
        assert_eq!(store.record(live("keystep-pro", 100)), Ok(Recorded::Stored));

        let got = store.get("keystep-pro").expect("recorded");
        assert!(got.present);
        assert_eq!(got.source, SOURCE_SINK);
        assert_eq!(got.at_ns, 100);
        assert_eq!(got.ports.len(), 1);
    }

    /// Unknown is a distinct state from absent: a kernel that was never told
    /// about a device holds nothing at all, and must not fabricate a record.
    #[test]
    fn an_unreported_device_is_unknown_not_absent() {
        let store = MidiPresenceStore::new();
        assert!(store.get("keystep-pro").is_none());
        assert!(store.is_empty());
    }

    /// Unplug is first class: present=false replaces a live record rather
    /// than leaving the stale "live" behind.
    #[test]
    fn unplug_overwrites_live_with_absent() {
        let store = MidiPresenceStore::new();
        store.record(live("keystep-pro", 100)).unwrap();
        store
            .record(MidiPresenceRecord::from_sink(
                "keystep-pro",
                false,
                "alsa",
                vec![],
                200,
            ))
            .unwrap();

        let got = store.get("keystep-pro").unwrap();
        assert!(!got.present, "the unplug report wins");
        assert_eq!(got.at_ns, 200);
        assert!(got.ports.is_empty());
    }

    /// Latest timestamp wins mechanically — a late-arriving OLDER report
    /// never resurrects a device that has since been unplugged.
    #[test]
    fn an_older_report_is_dropped_not_merged() {
        let store = MidiPresenceStore::new();
        store
            .record(MidiPresenceRecord::from_sink(
                "keystep-pro",
                false,
                "alsa",
                vec![],
                200,
            ))
            .unwrap();
        assert_eq!(
            store.record(live("keystep-pro", 100)),
            Ok(Recorded::StaleDropped)
        );
        assert!(!store.get("keystep-pro").unwrap().present);
    }

    /// A same-timestamp re-report is accepted (idempotent re-report after a
    /// reconnect must land, and equal stamps carry no ordering information).
    #[test]
    fn an_equal_timestamp_report_is_stored() {
        let store = MidiPresenceStore::new();
        store.record(live("keystep-pro", 100)).unwrap();
        assert_eq!(store.record(live("keystep-pro", 100)), Ok(Recorded::Stored));
    }

    #[test]
    fn a_malformed_device_key_is_refused_loudly() {
        let store = MidiPresenceStore::new();
        for bad in ["", "/", "..", ".", "a/b", "etc/passwd"] {
            assert!(
                matches!(
                    store.record(live(bad, 1)),
                    Err(PresenceError::InvalidDevice(_))
                ),
                "device key {bad:?} must be refused"
            );
        }
        assert!(store.is_empty());
    }

    #[test]
    fn json_tags_every_fact_with_source_and_time() {
        let json = live("keystep-pro", 42).to_json();
        assert_eq!(json["v"], 1);
        assert_eq!(json["device"], "keystep-pro");
        assert_eq!(json["present"]["value"], true);
        assert_eq!(json["present"]["source"], "sink");
        assert_eq!(json["present"]["at"], 42);
        assert_eq!(json["backend"]["value"], "alsa");
        assert_eq!(json["ports"]["value"][0]["name"], "KeyStep Pro MIDI 1");
        assert_eq!(json["ports"]["value"][0]["address"], "24:0");
    }

    // ── the /run/midi view ────────────────────────────────────────────────

    fn fs() -> (Arc<MidiPresenceStore>, MidiPresenceFs) {
        let store = Arc::new(MidiPresenceStore::new());
        (store.clone(), MidiPresenceFs::new(store))
    }

    #[tokio::test]
    async fn the_view_renders_one_file_per_reported_device() {
        let (store, fs) = fs();
        store.record(live("keystep-pro", 1)).unwrap();
        store.record(live("minibrute", 2)).unwrap();

        let names: Vec<String> = fs
            .readdir(Path::new(""))
            .await
            .unwrap()
            .into_iter()
            .map(|e| e.name)
            .collect();
        assert_eq!(names, vec!["keystep-pro".to_string(), "minibrute".into()]);

        let body = fs.read_all(Path::new("keystep-pro")).await.unwrap();
        let parsed: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(parsed["present"]["value"], true);
        assert_eq!(parsed["present"]["source"], "sink");
    }

    #[tokio::test]
    async fn a_fresh_view_lists_nothing_and_finds_nothing() {
        let (_store, fs) = fs();
        assert!(fs.readdir(Path::new("")).await.unwrap().is_empty());
        assert!(matches!(
            fs.getattr(Path::new("keystep-pro")).await,
            Err(VfsError::NotFound(_))
        ));
    }

    /// A record's generation strictly advances on every accepted report, so a
    /// caching reader can tell "same picture" from "new picture" without
    /// trusting mtime.
    #[tokio::test]
    async fn generation_advances_on_each_report() {
        let (store, fs) = fs();
        store.record(live("keystep-pro", 1)).unwrap();
        let g1 = fs.getattr(Path::new("keystep-pro")).await.unwrap().generation;
        store.record(live("keystep-pro", 2)).unwrap();
        let g2 = fs.getattr(Path::new("keystep-pro")).await.unwrap().generation;
        assert!(g2 > g1, "{g2} must exceed {g1}");
    }

    #[tokio::test]
    async fn the_view_refuses_every_write() {
        let (store, fs) = fs();
        store.record(live("keystep-pro", 1)).unwrap();
        assert!(matches!(
            fs.write(Path::new("keystep-pro"), 0, b"lies").await,
            Err(VfsError::ReadOnly)
        ));
        assert!(matches!(
            fs.create(Path::new("invented"), 0o644).await,
            Err(VfsError::ReadOnly)
        ));
        assert!(matches!(
            fs.unlink(Path::new("keystep-pro")).await,
            Err(VfsError::ReadOnly)
        ));
        assert!(fs.read_only());
    }

    /// Offset/length reads behave (SFTP and the pump both chunk).
    #[tokio::test]
    async fn ranged_reads_are_bounded_by_the_record() {
        let (store, fs) = fs();
        store.record(live("keystep-pro", 1)).unwrap();
        let whole = fs.read_all(Path::new("keystep-pro")).await.unwrap();
        let head = fs.read(Path::new("keystep-pro"), 0, 8).await.unwrap();
        assert_eq!(head, whole[..8]);
        // Past EOF reads empty (the documented EOF signal), never an error.
        let past = fs
            .read(Path::new("keystep-pro"), whole.len() as u64 + 10, 64)
            .await
            .unwrap();
        assert!(past.is_empty());
    }

    /// The namespace is flat: nothing nests under a device record.
    #[tokio::test]
    async fn nested_paths_are_not_found() {
        let (store, fs) = fs();
        store.record(live("keystep-pro", 1)).unwrap();
        assert!(matches!(
            fs.getattr(Path::new("keystep-pro/ports")).await,
            Err(VfsError::NotFound(_))
        ));
    }
}
