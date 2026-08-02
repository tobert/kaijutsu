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
//! ## Presence is connection-bound
//!
//! A sink that crashes, loses its network, or is `kill -9`'d never gets to
//! send its `present=false` report — so presence may not be trusted to any
//! statement the sink makes about itself. Every record therefore carries the
//! **connection** it arrived on ([`SinkAttribution::connection`], the server's
//! per-connection `SessionId`), and when that connection dies the server calls
//! [`MidiPresenceStore::reap_connection`]: every record attributed to it is
//! *removed*, back to **unknown**. Removal, not `present=false` — claiming an
//! absence nobody observed would be the same class of lie in the other
//! direction. The sink's self-reported `host` is display/provenance data and
//! is never a reaping key.
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
use kaijutsu_types::SessionId;

use crate::vfs::{DirEntry, FileAttr, SetAttr, StatFs, VfsError, VfsOps, VfsResult};

/// Provenance tag for facts a sink reported (`docs/midi-next.md`: the third,
/// `observed`, arrives with the ear mapping).
pub const SOURCE_SINK: &str = "sink";

/// Provenance tag for facts the **device itself answered** — the doc's third
/// provenance, and it starts here with `kj midi identify` (`docs/midi-next.md`
/// "SysEx: the exchange pattern"). A pulled fact is the strongest thing in
/// this store: the sink didn't infer it from a port name, the device said so.
pub const SOURCE_PULLED: &str = "pulled";

/// One MIDI port as the reporting sink saw it. Backend-neutral: `name` is the
/// display name profile match strings match on; `address` is the backend's own
/// handle (`"client:port"` under ALSA, a unique id under CoreMIDI) — opaque to
/// the kernel, carried for logs and disambiguation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MidiPortFact {
    pub name: String,
    pub address: String,
}

/// Who reported a record, and over which wire.
///
/// The two halves are deliberately different in kind:
///
/// - `connection` is **ours** — the kernel-internal, connection-scoped id the
///   server minted for the sink's session. It is the only reaping key, because
///   it is the only identity a crashed sink cannot lie about or take with it.
/// - `host` is **theirs** — whatever the sink calls itself, for display and
///   provenance (`kj midi list` answering *where* a device is live). Never used
///   to key, group, or reap anything: two sinks on one box, a stale hostname,
///   or an outright fib must not be able to erase each other's records.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SinkAttribution {
    /// The reporting sink's connection (the server's per-connection
    /// `SessionId`). Dies with the connection; see
    /// [`MidiPresenceStore::reap_connection`].
    pub connection: SessionId,
    /// The reporting sink's own name for the machine it runs on. Display only;
    /// empty when a sink didn't say (an older peer).
    pub host: String,
}

impl SinkAttribution {
    pub fn new(connection: SessionId, host: impl Into<String>) -> Self {
        Self { connection, host: host.into() }
    }
}

/// A fact the device answered for itself: its Identity Reply, parsed, with
/// the wallclock of the exchange that pulled it.
///
/// Carried on the presence record because that is where a reader already
/// looks for "what is this device, right now" — but tagged
/// [`SOURCE_PULLED`], never `sink`, so nobody mistakes an answer from the
/// gear for the app's name-matching.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MidiIdentityFact {
    pub identity: crate::midi_identity::MidiIdentity,
    /// Kernel wallclock (ns since UNIX_EPOCH) when the exchange answered.
    pub at_ns: u64,
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
    /// Which sink said so, and over which connection. The connection half is
    /// what makes the record perishable; see [`SinkAttribution`].
    pub sink: SinkAttribution,
    /// What the device itself answered, if anyone has asked
    /// (`kj midi identify`). `None` = never pulled, which is the normal state
    /// — plenty of gear (a Subharmonicon) can't answer at all.
    pub identity: Option<MidiIdentityFact>,
}

impl MidiPresenceRecord {
    /// A sink report, stamped with [`SOURCE_SINK`] provenance and the
    /// attribution that makes it reapable when the reporter goes away.
    pub fn from_sink(
        device: impl Into<String>,
        present: bool,
        backend: impl Into<String>,
        ports: Vec<MidiPortFact>,
        at_ns: u64,
        sink: SinkAttribution,
    ) -> Self {
        Self {
            device: device.into(),
            present,
            backend: backend.into(),
            ports,
            at_ns,
            source: SOURCE_SINK.to_string(),
            sink,
            identity: None,
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
        let mut json = serde_json::json!({
            "v": 1,
            "device": self.device,
            "present": tag(serde_json::Value::Bool(self.present)),
            "backend": tag(serde_json::Value::String(self.backend.clone())),
            // WHERE the device is live. Provenance data like every other fact
            // here — the sink's own claim about its machine, tagged so a
            // reader judges it rather than trusting it. The connection id
            // behind the record stays kernel-internal: it is a reaping key,
            // not something a reader can act on.
            "host": tag(serde_json::Value::String(self.sink.host.clone())),
            "ports": tag(serde_json::Value::Array(
                self.ports
                    .iter()
                    .map(|p| serde_json::json!({ "name": p.name, "address": p.address }))
                    .collect(),
            )),
        });
        // The pulled fact carries its OWN source and time — it came from a
        // different producer at a different instant than everything above,
        // and flattening it into the record's stamp would be the exact lie
        // the per-fact provenance scheme exists to prevent. Absent entirely
        // when nobody has asked: an empty identity would read as "we asked
        // and got nothing".
        if let Some(fact) = &self.identity {
            json["identity"] = serde_json::json!({
                "value": fact.identity.to_json(),
                "source": SOURCE_PULLED,
                "at": fact.at_ns,
            });
        }
        json
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
    /// A pulled fact arrived for a device this store holds no record of —
    /// the device went away (or was reaped) between the exchange and its
    /// answer. See [`MidiPresenceStore::record_identity`].
    NoRecord(String),
    /// A pulled fact arrived for a device that is on file as ABSENT: a sink
    /// watched it leave while the exchange was in flight. Distinct from
    /// [`PresenceError::NoRecord`] on purpose — "we were told it left" and
    /// "we hold nothing at all" send a player to different places, and this
    /// store never collapses those two.
    NotPresent(String),
}

impl std::fmt::Display for PresenceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PresenceError::InvalidDevice(d) => write!(
                f,
                "invalid device key '{d}': /run/midi is a flat namespace of \
                 profile names (e.g. keystep-pro)"
            ),
            PresenceError::NoRecord(d) => write!(
                f,
                "no presence record for '{d}' — nothing to attach a pulled \
                 fact to (the device went away mid-exchange)"
            ),
            PresenceError::NotPresent(d) => write!(
                f,
                "'{d}' was reported ABSENT while the exchange was in flight — \
                 refusing to file a pulled fact against a device that has left"
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
    pub fn record(&self, mut record: MidiPresenceRecord) -> Result<Recorded, PresenceError> {
        Self::validate_device(&record.device)?;
        let mut entries = self
            .entries
            .write()
            .expect("midi presence lock poisoned (a writer panicked)");
        if let Some(existing) = entries.get(&record.device) {
            if existing.at_ns > record.at_ns {
                return Ok(Recorded::StaleDropped);
            }
            // Carry a pulled identity across reports that keep the device
            // present — the sink re-states presence on every topology change,
            // and dropping the device's own answer each time would make a
            // fingerprint useless the moment anything else is plugged in.
            //
            // Any report that says ABSENT drops it, and it never comes back
            // on the next `present=true`: what returns to that port may be a
            // different unit, and a fingerprint that outlives the device it
            // describes is precisely the "stale presence that lies" this
            // store refuses. Re-plug ⇒ re-`identify`.
            if existing.present && record.present && record.identity.is_none() {
                record.identity = existing.identity.clone();
            }
        }
        self.generation.fetch_add(1, Ordering::Relaxed);
        entries.insert(record.device.clone(), record);
        Ok(Recorded::Stored)
    }

    /// Attach what a device answered for itself to its presence record —
    /// `kj midi identify`'s write, and the first **pulled** fact in the store
    /// (`docs/midi-next.md`).
    ///
    /// Refuses when there is no record to attach to. That is not a
    /// convenience check: an exchange only happens against a device some sink
    /// reported *live*, so a missing record here means the device went away
    /// mid-exchange (or was reaped with its connection), and minting a record
    /// out of an identity reply would resurrect presence nobody is standing
    /// behind.
    pub fn record_identity(
        &self,
        device: &str,
        identity: crate::midi_identity::MidiIdentity,
        at_ns: u64,
    ) -> Result<(), PresenceError> {
        Self::validate_device(device)?;
        let mut entries = self
            .entries
            .write()
            .expect("midi presence lock poisoned (a writer panicked)");
        let Some(record) = entries.get_mut(device) else {
            return Err(PresenceError::NoRecord(device.to_string()));
        };
        // A record that says ABSENT is a sink's observation that the device
        // left, and it may well have left *during* this exchange. Filing the
        // answer against it would render `/run/midi` as "gone, and here is
        // what it is" — a shape no reader should have to reconcile.
        if !record.present {
            return Err(PresenceError::NotPresent(device.to_string()));
        }
        record.identity = Some(MidiIdentityFact { identity, at_ns });
        self.generation.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    /// Forget everything a now-dead connection told us, returning the device
    /// keys that were dropped (ascending, for the caller's log).
    ///
    /// This is the honesty valve for the case a sink cannot cover itself: a
    /// crash, a yanked network, a `kill -9`. No unplug report is coming, so a
    /// `present=true` record attributed to that connection would be a lie the
    /// kernel keeps telling forever. We **remove** rather than flip to
    /// `present=false`: nobody observed an absence, and `/run/midi` treats a
    /// missing entry as *unknown*, which is exactly the truth here — a kernel
    /// with no sinks connected knows nothing.
    ///
    /// Accepted trade-off: removal also drops the latest-timestamp guard for
    /// that device. A stale report still in flight from a reconnecting sink
    /// can therefore land on the empty slot and be believed for a moment.
    /// That is a *brief* wrong answer that the sink's next fresh report
    /// corrects (the app re-states its whole picture on every reconnect),
    /// where keeping the record would be a permanent one. We take the
    /// self-healing error over the sticky one.
    pub fn reap_connection(&self, connection: SessionId) -> Vec<String> {
        let mut entries = self
            .entries
            .write()
            .expect("midi presence lock poisoned (a writer panicked)");
        let doomed: Vec<String> = entries
            .iter()
            .filter(|(_, r)| r.sink.connection == connection)
            .map(|(device, _)| device.clone())
            .collect();
        for device in &doomed {
            entries.remove(device);
        }
        if !doomed.is_empty() {
            self.generation.fetch_add(1, Ordering::Relaxed);
        }
        doomed
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

    /// A stand-in for one sink's wire connection.
    fn sink(host: &str) -> SinkAttribution {
        SinkAttribution::new(SessionId::new(), host)
    }

    fn live(device: &str, at_ns: u64) -> MidiPresenceRecord {
        live_from(device, at_ns, sink("moltar"))
    }

    fn live_from(device: &str, at_ns: u64, sink: SinkAttribution) -> MidiPresenceRecord {
        MidiPresenceRecord::from_sink(
            device,
            true,
            "alsa",
            vec![port("KeyStep Pro MIDI 1", "24:0")],
            at_ns,
            sink,
        )
    }

    fn absent(device: &str, at_ns: u64) -> MidiPresenceRecord {
        MidiPresenceRecord::from_sink(device, false, "alsa", vec![], at_ns, sink("moltar"))
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
        store.record(absent("keystep-pro", 200)).unwrap();

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
        store.record(absent("keystep-pro", 200)).unwrap();
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

    // ── connection-bound presence (the crashed-sink hole) ─────────────────

    /// The headline rule: when a sink's connection dies, ONLY that sink's
    /// records go. A second sink on the same rig keeps reporting for its own
    /// gear and must not lose a thing.
    #[test]
    fn reaping_a_connection_drops_only_its_own_records() {
        let store = MidiPresenceStore::new();
        let moltar = sink("moltar");
        let laptop = sink("laptop");
        store.record(live_from("keystep-pro", 100, moltar.clone())).unwrap();
        store.record(live_from("minibrute", 100, moltar.clone())).unwrap();
        store.record(live_from("keylab", 100, laptop.clone())).unwrap();

        let reaped = store.reap_connection(moltar.connection);
        assert_eq!(reaped, vec!["keystep-pro".to_string(), "minibrute".into()]);
        assert!(store.get("keystep-pro").is_none(), "reaped → unknown");
        assert!(store.get("minibrute").is_none(), "reaped → unknown");
        assert!(
            store.get("keylab").is_some(),
            "the other sink's connection is still up; its record stands"
        );
    }

    /// Reaping means **unknown**, never a fabricated absence: `present=false`
    /// is an observation, and a dead connection observed nothing.
    #[test]
    fn a_reaped_device_is_unknown_not_absent() {
        let store = MidiPresenceStore::new();
        let s = sink("moltar");
        store.record(live_from("keystep-pro", 100, s.clone())).unwrap();
        store.reap_connection(s.connection);
        assert_eq!(
            store.get("keystep-pro"),
            None,
            "a reaped record must vanish, not linger as present=false"
        );
        assert!(store.is_empty());
    }

    /// Two sinks can hold the same device (a shared USB hub, a re-plug across
    /// machines). Latest-timestamp still decides who is on file, and reaping
    /// the *loser's* connection leaves the winner's record alone.
    #[test]
    fn reaping_a_connection_that_lost_the_timestamp_race_changes_nothing() {
        let store = MidiPresenceStore::new();
        let old = sink("laptop");
        let new = sink("moltar");
        store.record(live_from("keystep-pro", 100, old.clone())).unwrap();
        store.record(live_from("keystep-pro", 200, new.clone())).unwrap();
        assert!(store.reap_connection(old.connection).is_empty());
        assert_eq!(
            store.get("keystep-pro").unwrap().sink.connection,
            new.connection,
            "the winning sink's record survives its rival's disconnect"
        );
    }

    /// The documented trade-off, pinned as behavior so nobody "fixes" it by
    /// accident: removal drops the latest-timestamp guard, so a stale report
    /// queued by a reconnecting sink CAN land on the emptied slot. It is a
    /// self-healing wrong answer (the next fresh report corrects it), chosen
    /// over the permanent lie of a record no living connection stands behind.
    #[test]
    fn a_stale_report_can_land_after_a_reap_and_is_corrected_by_the_next_one() {
        let store = MidiPresenceStore::new();
        let old = sink("moltar");
        store.record(live_from("keystep-pro", 500, old.clone())).unwrap();
        store.reap_connection(old.connection);

        // A report older than the reaped one, arriving late on a new
        // connection: accepted, because there is nothing left to compare to.
        let reconnected = sink("moltar");
        assert_eq!(
            store.record(live_from("keystep-pro", 100, reconnected.clone())),
            Ok(Recorded::Stored)
        );
        // …and the sink's fresh re-statement wins immediately after.
        store.record(live_from("keystep-pro", 900, reconnected.clone())).unwrap();
        assert_eq!(store.get("keystep-pro").unwrap().at_ns, 900);
    }

    /// Equal timestamps carry no ordering, so an idempotent re-report from a
    /// *different* connection still lands — and re-attributes the record, so
    /// the new owner's disconnect is what reaps it.
    #[test]
    fn an_equal_timestamp_report_from_a_new_connection_takes_ownership() {
        let store = MidiPresenceStore::new();
        let first = sink("moltar");
        let second = sink("moltar");
        store.record(live_from("keystep-pro", 100, first.clone())).unwrap();
        assert_eq!(
            store.record(live_from("keystep-pro", 100, second.clone())),
            Ok(Recorded::Stored)
        );
        assert!(
            store.reap_connection(first.connection).is_empty(),
            "the stale connection no longer owns the record"
        );
        assert!(store.get("keystep-pro").is_some());
        assert_eq!(store.reap_connection(second.connection).len(), 1);
    }

    /// Reaping nothing is not a change — a connection that never reported
    /// presence must not churn the view's generation.
    #[test]
    fn reaping_a_connection_that_reported_nothing_is_a_no_op() {
        let store = MidiPresenceStore::new();
        store.record(live("keystep-pro", 100)).unwrap();
        let g = store.generation();
        assert!(store.reap_connection(SessionId::new()).is_empty());
        assert_eq!(store.generation(), g, "a no-op reap must not bump generation");
    }

    /// A reap IS a change to the picture: the /run/midi view's generation must
    /// advance so a caching reader re-reads.
    #[test]
    fn a_reap_advances_the_view_generation() {
        let store = MidiPresenceStore::new();
        let s = sink("moltar");
        store.record(live_from("keystep-pro", 1, s.clone())).unwrap();
        let g = store.generation();
        store.reap_connection(s.connection);
        assert!(store.generation() > g);
    }

    #[test]
    fn json_carries_the_sinks_host_as_a_tagged_fact() {
        let json = live_from("keystep-pro", 42, sink("moltar")).to_json();
        assert_eq!(json["host"]["value"], "moltar");
        assert_eq!(json["host"]["source"], "sink");
        assert_eq!(json["host"]["at"], 42);
        assert!(
            json.get("connection").is_none(),
            "the reaping key is kernel-internal and must not leak into the view"
        );
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

    // ── pulled identity (docs/midi-next.md slice 1 step 5) ────────────────

    fn identity() -> crate::midi_identity::MidiIdentity {
        crate::midi_identity::parse_identity_reply(&[
            0xF0, 0x7E, 0x00, 0x06, 0x02, 0x00, 0x20, 0x6B, 0x01, 0x00, 0x02, 0x00, 0x01, 0x00,
            0x03, 0x04, 0xF7,
        ])
        .expect("fixture is a valid Identity Reply")
    }

    #[test]
    fn an_identity_lands_on_the_record_as_a_pulled_fact() {
        let store = MidiPresenceStore::new();
        store.record(live("keystep-pro", 100)).unwrap();
        store.record_identity("keystep-pro", identity(), 500).unwrap();

        let json = store.get("keystep-pro").unwrap().to_json();
        assert_eq!(json["identity"]["source"], SOURCE_PULLED, "NOT 'sink'");
        assert_eq!(json["identity"]["at"], 500, "its own stamp, not the report's");
        assert_eq!(json["identity"]["value"]["manufacturerName"], "Arturia");
        // The sink-reported facts keep their own provenance untouched.
        assert_eq!(json["present"]["source"], SOURCE_SINK);
        assert_eq!(json["present"]["at"], 100);
    }

    /// A device nobody has asked has no `identity` key at all — an empty one
    /// would read as "we asked and got nothing".
    #[test]
    fn an_unasked_device_renders_no_identity_key() {
        let store = MidiPresenceStore::new();
        store.record(live("keystep-pro", 100)).unwrap();
        assert!(store.get("keystep-pro").unwrap().to_json().get("identity").is_none());
    }

    /// The sink re-states presence on every topology change; a fingerprint
    /// that evaporated each time would be useless.
    #[test]
    fn a_pulled_identity_survives_a_re_report_that_keeps_the_device_live() {
        let store = MidiPresenceStore::new();
        store.record(live("keystep-pro", 100)).unwrap();
        store.record_identity("keystep-pro", identity(), 500).unwrap();
        store.record(live("keystep-pro", 900)).unwrap();
        let got = store.get("keystep-pro").unwrap();
        assert_eq!(got.at_ns, 900, "the fresh presence report won");
        assert_eq!(
            got.identity.as_ref().map(|f| f.at_ns),
            Some(500),
            "the pulled fact rode along, still stamped with when IT was pulled"
        );
    }

    /// Unplug drops the fingerprint, and a re-plug does NOT bring it back:
    /// what returned to that port may be a different unit. Re-plug ⇒
    /// re-identify.
    #[test]
    fn an_unplug_drops_the_identity_and_a_replug_does_not_restore_it() {
        let store = MidiPresenceStore::new();
        store.record(live("keystep-pro", 100)).unwrap();
        store.record_identity("keystep-pro", identity(), 500).unwrap();

        store.record(absent("keystep-pro", 600)).unwrap();
        assert!(store.get("keystep-pro").unwrap().identity.is_none());

        store.record(live("keystep-pro", 700)).unwrap();
        assert!(
            store.get("keystep-pro").unwrap().identity.is_none(),
            "a replug must not inherit the previous unit's fingerprint"
        );
    }

    /// A reap takes the identity with the record — a fingerprint for a device
    /// no live connection stands behind is the same lie as a stale presence.
    #[test]
    fn reaping_takes_the_identity_with_the_record() {
        let store = MidiPresenceStore::new();
        let s = sink("moltar");
        store.record(live_from("keystep-pro", 100, s.clone())).unwrap();
        store.record_identity("keystep-pro", identity(), 500).unwrap();
        store.reap_connection(s.connection);
        assert!(store.get("keystep-pro").is_none());
    }

    /// A device that left while the exchange was in flight refuses the file,
    /// and says so differently from "we hold nothing" — the same
    /// unknown-vs-absent distinction the rest of this store lives by.
    #[test]
    fn an_identity_for_a_departed_device_is_refused_by_name() {
        let store = MidiPresenceStore::new();
        store.record(absent("keystep-pro", 100)).unwrap();
        assert_eq!(
            store.record_identity("keystep-pro", identity(), 500),
            Err(PresenceError::NotPresent("keystep-pro".into()))
        );
        assert!(store.get("keystep-pro").unwrap().identity.is_none());
    }

    /// An identity for a device we hold no record of is refused loudly rather
    /// than minting presence nobody reported.
    #[test]
    fn an_identity_for_an_unknown_device_is_refused() {
        let store = MidiPresenceStore::new();
        assert_eq!(
            store.record_identity("keystep-pro", identity(), 500),
            Err(PresenceError::NoRecord("keystep-pro".into()))
        );
        assert!(store.is_empty(), "and nothing was invented");
    }

    #[test]
    fn a_pulled_identity_advances_the_view_generation() {
        let store = MidiPresenceStore::new();
        store.record(live("keystep-pro", 1)).unwrap();
        let g = store.generation();
        store.record_identity("keystep-pro", identity(), 2).unwrap();
        assert!(store.generation() > g, "a caching reader must re-read");
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
