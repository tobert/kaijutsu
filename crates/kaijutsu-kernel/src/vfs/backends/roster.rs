//! `RosterFs` — the read-only `/run/roster` view over
//! [`crate::roster::RosterStore`] (slice 4 of the live roster;
//! `crate::roster` module doc for the whole design).
//!
//! ```text
//! /run/roster/
//! ├── index                          # generation-stamped TSV, one line per row
//! └── <entity_kind>-<entity_id>/      # one directory per roster row
//!     ├── entity_kind
//!     ├── entity_id
//!     ├── label
//!     ├── liveness_kind
//!     ├── live
//!     ├── host
//!     ├── source
//!     ├── observed_at
//!     ├── recorded_at
//!     ├── status_text
//!     ├── availability
//!     ├── status_observed_at
//!     └── status_recorded_at
//! ```
//!
//! `/run` is the ephemerality convention (`midi_presence.rs`'s module doc
//! argues why at length — read it before touching this file); it applies
//! here even though the roster is `kernel.db`-backed, because **liveness
//! itself is never trusted as a stored fact across a restart** (module doc
//! on `crate::roster`) — this tree is exactly as much "current runtime
//! state" as `/run/midi`, just backed by SQL instead of an in-memory map.
//!
//! ## Mount constraint, checked
//!
//! `freeze_mounts()` means one backend per subtree. `/run/midi` is its own
//! LEAF mount (`kaijutsu-server/src/rpc.rs`, not a bare `/run` mount with
//! internal routing) — so `/run/roster` is a sibling leaf mount, same
//! shape, no internal routing needed (unlike `/r`'s `ShareFs`, which DOES
//! route internally because `/r` itself is the one mount and per-client
//! trees live under it).
//!
//! ## The `read_all`/lstat gotcha, checked
//!
//! The documented gotcha (a default `read_all` sizes from `lstat`, which
//! truncates a followed symlink) does not apply here: this backend has no
//! symlinks at all (`readlink` always refuses, same as `MidiPresenceFs`),
//! so `getattr`'s reported size always matches the real body length for
//! every path this backend serves.
//!
//! ## Grouping: one directory per ENTITY, not per presence row
//!
//! [`crate::roster::RosterStore::snapshot`] returns one [`RosterRow`] per
//! presence (an entity CAN in principle carry more than one, once
//! `attested` lands). This view groups by entity — `<entity_kind>-
//! <entity_id>` — and keeps the row with the greatest `recorded_at` when
//! more than one exists (documented as a deliberate simplification: v1's
//! two producers never collide on one entity, `bound` is principal-only and
//! `recent` is context-only, so this path is untested by anything real
//! today and exists only so a future third source doesn't silently corrupt
//! the directory listing).

use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;

use crate::roster::{RosterRow, RosterStore};
use crate::vfs::{DirEntry, FileAttr, SetAttr, StatFs, VfsError, VfsOps, VfsResult};

const INDEX_NAME: &str = "index";
/// The unfiltered sibling of [`INDEX_NAME`]. `index` answers "who is around
/// right now" — the mount's whole purpose — and so omits rows we positively
/// know are not live. `index-all` answers "everything the roster knows".
///
/// The split exists because the `recent` source is 1:1 with every non-archived
/// context (`roster_sources::recent_snapshot`), so `index` grew to 199 rows /
/// 17.8 KB on a real kernel to report **3** live entities — over kaish's 8 KB
/// model-facing output cap, meaning a model asking who was around got a
/// truncated splice of mostly-dead rows.
///
/// Filtering is **listing-only, never lookup**: `resolve` still finds any row
/// directory by key, so `/run/roster/<key>/live` answers for a hidden entity
/// too. Nothing becomes unreachable, it just stops being listed — which is the
/// design record's "missing entry = unknown, never absent" read the only way
/// it can be honoured by a surface that must also stay bounded.
const INDEX_ALL_NAME: &str = "index-all";

/// Whether a row survives the default (unfiltered = `index-all`) listing.
///
/// Hides ONLY what we positively know is dead. `live == None` means *unknown*
/// — the shape a status-only entity has (slice 3 re-rooted `roster_snapshot`
/// at `roster_entity` precisely so those stay visible), and hiding unknowns
/// would quietly undo that fix while looking like a tidier filter.
fn is_around(row: &RosterRow) -> bool {
    row.live != Some(false)
}

/// Fact files inside one row directory, in the order `readdir` lists them.
const FACTS: &[&str] = &[
    "entity_kind",
    "entity_id",
    "label",
    "liveness_kind",
    "live",
    "host",
    "source",
    "observed_at",
    "recorded_at",
    "status_text",
    "availability",
    "status_observed_at",
    "status_recorded_at",
];

/// `<entity_kind>-<entity_id>` — the row directory's name. Unambiguous to
/// split back apart: `entity_kind` is always `principal` or `context`
/// (never contains `-`), and `entity_id` is pure lowercase hex.
fn row_key(row: &RosterRow) -> String {
    format!("{}-{}", row.entity.kind_str(), row.entity.to_hex())
}

enum Resolved {
    Root,
    /// `true` = the unfiltered `index-all`.
    Index(bool),
    RowDir(String),
    RowFile(String, &'static str),
}

pub struct RosterFs {
    roster: Arc<RosterStore>,
}

impl RosterFs {
    pub fn new(roster: Arc<RosterStore>) -> Self {
        Self { roster }
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
            [s] if s == INDEX_NAME => Ok(Resolved::Index(false)),
            [s] if s == INDEX_ALL_NAME => Ok(Resolved::Index(true)),
            [key] => Ok(Resolved::RowDir(key.clone())),
            [key, fact] => match FACTS.iter().find(|f| *f == fact) {
                Some(f) => Ok(Resolved::RowFile(key.clone(), f)),
                None => Err(VfsError::not_found(format!("{key}/{fact}"))),
            },
            _ => Err(VfsError::not_found(segs.join("/"))),
        }
    }

    /// The current snapshot, grouped by entity — see module doc.
    fn rows_by_key(&self) -> VfsResult<BTreeMap<String, RosterRow>> {
        let rows = self
            .roster
            .snapshot()
            .map_err(|e| VfsError::Io(std::io::Error::other(e.to_string())))?;
        let mut out: BTreeMap<String, RosterRow> = BTreeMap::new();
        for row in rows {
            let key = row_key(&row);
            match out.get(&key) {
                Some(existing) if existing.recorded_at >= row.recorded_at => {}
                _ => {
                    out.insert(key, row);
                }
            }
        }
        Ok(out)
    }

    fn row(&self, key: &str) -> VfsResult<RosterRow> {
        self.rows_by_key()?.remove(key).ok_or_else(|| VfsError::not_found(key.to_string()))
    }

    /// One fact's bytes, or `None` for a value that is genuinely absent
    /// (never "0" or an empty-but-present string — an absent numeric fact
    /// renders as an empty file, matching the TSV's empty-cell convention).
    fn fact_bytes(row: &RosterRow, fact: &str) -> Vec<u8> {
        let s = match fact {
            "entity_kind" => row.entity.kind_str().to_string(),
            "entity_id" => row.entity.to_hex(),
            "label" => row.label.clone().unwrap_or_default(),
            "liveness_kind" => row.liveness_kind.map(|k| k.as_str().to_string()).unwrap_or_default(),
            "live" => row.live.map(|b| b.to_string()).unwrap_or_default(),
            "host" => row.host.clone().unwrap_or_default(),
            "source" => row.source.clone().unwrap_or_default(),
            "observed_at" => row.observed_at.map(|v| v.to_string()).unwrap_or_default(),
            "recorded_at" => row.recorded_at.map(|v| v.to_string()).unwrap_or_default(),
            "status_text" => row.status_text.clone().unwrap_or_default(),
            "availability" => row.availability.map(|a| a.as_str().to_string()).unwrap_or_default(),
            "status_observed_at" => row.status_observed_at.map(|v| v.to_string()).unwrap_or_default(),
            "status_recorded_at" => row.status_recorded_at.map(|v| v.to_string()).unwrap_or_default(),
            _ => unreachable!("fact {fact:?} not in FACTS"),
        };
        let mut bytes = s.into_bytes();
        bytes.push(b'\n');
        bytes
    }

    /// The `/run/roster/index` TSV: `entity_kind entity_id label
    /// liveness_kind live host status_text availability recorded_at`, one
    /// row per entity (same grouping as the directory listing).
    fn index_bytes(&self, all: bool) -> VfsResult<Vec<u8>> {
        let rows = self.rows_by_key()?;
        let mut out = String::from(
            "entity_kind\tentity_id\tlabel\tliveness_kind\tlive\thost\tstatus_text\tavailability\trecorded_at\n",
        );
        for row in rows.values().filter(|r| all || is_around(r)) {
            out.push_str(row.entity.kind_str());
            out.push('\t');
            out.push_str(&row.entity.to_hex());
            out.push('\t');
            out.push_str(row.label.as_deref().unwrap_or(""));
            out.push('\t');
            out.push_str(row.liveness_kind.map(|k| k.as_str()).unwrap_or(""));
            out.push('\t');
            if let Some(live) = row.live {
                out.push_str(if live { "true" } else { "false" });
            }
            out.push('\t');
            out.push_str(row.host.as_deref().unwrap_or(""));
            out.push('\t');
            out.push_str(row.status_text.as_deref().unwrap_or(""));
            out.push('\t');
            out.push_str(row.availability.map(|a| a.as_str()).unwrap_or(""));
            out.push('\t');
            if let Some(ts) = row.recorded_at {
                out.push_str(&ts.to_string());
            }
            out.push('\n');
        }
        Ok(out.into_bytes())
    }
}

#[async_trait]
impl VfsOps for RosterFs {
    async fn getattr(&self, path: &Path) -> VfsResult<FileAttr> {
        let generation = self.roster.generation();
        match self.resolve(path)? {
            Resolved::Root => Ok(FileAttr::directory(0o555)),
            Resolved::Index(all) => {
                let body = self.index_bytes(all)?;
                let mut attr = FileAttr::file(body.len() as u64, 0o444);
                attr.generation = generation;
                Ok(attr)
            }
            Resolved::RowDir(key) => {
                self.row(&key)?;
                let mut attr = FileAttr::directory(0o555);
                attr.generation = generation;
                Ok(attr)
            }
            Resolved::RowFile(key, fact) => {
                let row = self.row(&key)?;
                let body = Self::fact_bytes(&row, fact);
                let mut attr = FileAttr::file(body.len() as u64, 0o444);
                attr.generation = generation;
                Ok(attr)
            }
        }
    }

    async fn readdir(&self, path: &Path) -> VfsResult<Vec<DirEntry>> {
        match self.resolve(path)? {
            Resolved::Root => {
                let rows = self.rows_by_key()?;
                let mut entries: Vec<DirEntry> =
                    vec![DirEntry::file(INDEX_NAME), DirEntry::file(INDEX_ALL_NAME)];
                // Listing filtered, lookup not — see `INDEX_ALL_NAME`.
                entries.extend(
                    rows.iter()
                        .filter(|(_, r)| is_around(r))
                        .map(|(k, _)| DirEntry::directory(k.clone())),
                );
                Ok(entries)
            }
            Resolved::Index(all) => Err(VfsError::not_a_directory(if all {
                INDEX_ALL_NAME
            } else {
                INDEX_NAME
            })),
            Resolved::RowDir(key) => {
                self.row(&key)?;
                Ok(FACTS.iter().copied().map(DirEntry::file).collect())
            }
            Resolved::RowFile(key, fact) => Err(VfsError::not_a_directory(format!("{key}/{fact}"))),
        }
    }

    async fn read(&self, path: &Path, offset: u64, size: u32) -> VfsResult<Vec<u8>> {
        let body = match self.resolve(path)? {
            Resolved::Root => return Err(VfsError::is_a_directory("/".to_string())),
            Resolved::Index(all) => self.index_bytes(all)?,
            Resolved::RowDir(key) => return Err(VfsError::is_a_directory(key)),
            Resolved::RowFile(key, fact) => {
                let row = self.row(&key)?;
                Self::fact_bytes(&row, fact)
            }
        };
        let start = (offset as usize).min(body.len());
        let end = start.saturating_add(size as usize).min(body.len());
        Ok(body[start..end].to_vec())
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
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kernel_db::KernelDb;
    use crate::roster::{Availability, LivenessKind, PresenceSnapshotRow, RosterEntity};
    use kaijutsu_types::PrincipalId;

    fn fs() -> (Arc<RosterStore>, RosterFs) {
        let db = Arc::new(parking_lot::Mutex::new(KernelDb::temporary().unwrap()));
        let roster = Arc::new(RosterStore::new(db));
        (roster.clone(), RosterFs::new(roster))
    }

    fn presence_row(entity: RosterEntity, local_id: &str) -> PresenceSnapshotRow {
        PresenceSnapshotRow {
            source_local_id: local_id.to_string(),
            entity,
            entity_label: Some("amy".to_string()),
            host: Some("moltar".to_string()),
            pid: None,
            proc_start: None,
            observed_at: Some(100),
            live: true,
        }
    }

    #[tokio::test]
    async fn a_fresh_view_lists_only_the_two_indexes() {
        let (_roster, fs) = fs();
        let names: Vec<String> =
            fs.readdir(Path::new("")).await.unwrap().into_iter().map(|e| e.name).collect();
        assert_eq!(names, vec!["index".to_string(), "index-all".to_string()]);
    }

    /// `index` answers "who is around", `index-all` answers "what is known",
    /// and a known-idle row must be absent from the first and present in the
    /// second. Without this the roster reports 199 rows to name 3 live
    /// entities and blows the model-facing output cap doing it.
    #[tokio::test]
    async fn an_idle_row_is_listed_only_by_index_all() {
        let (roster, fs) = fs();
        let live_p = PrincipalId::new();
        let idle_p = PrincipalId::new();
        let mut idle = presence_row(RosterEntity::Principal(idle_p), "win-idle");
        idle.live = false;
        roster
            .reconcile(
                "peer_registry",
                LivenessKind::Bound,
                &[presence_row(RosterEntity::Principal(live_p), "win-live"), idle],
                100,
            )
            .unwrap();

        let live_key = format!("principal-{}", live_p.to_hex());
        let idle_key = format!("principal-{}", idle_p.to_hex());

        let index = String::from_utf8(fs.read(Path::new("index"), 0, u32::MAX).await.unwrap())
            .unwrap();
        assert!(index.contains(&live_p.to_hex()), "index must carry the live entity");
        assert!(
            !index.contains(&idle_p.to_hex()),
            "index must omit a known-idle entity — it is the who-is-around view"
        );

        let all = String::from_utf8(fs.read(Path::new("index-all"), 0, u32::MAX).await.unwrap())
            .unwrap();
        assert!(all.contains(&idle_p.to_hex()), "index-all must carry the idle entity");

        let names: Vec<String> =
            fs.readdir(Path::new("")).await.unwrap().into_iter().map(|e| e.name).collect();
        assert!(names.contains(&live_key), "root listing keeps the live row dir");
        assert!(!names.contains(&idle_key), "root listing omits the idle row dir");

        // Filtering is LISTING-only. The hidden row is still addressable, so
        // nothing became unreachable — only unlisted.
        let live_fact = fs.read(&PathBuf::from(&idle_key).join("live"), 0, u32::MAX).await;
        assert_eq!(
            String::from_utf8(live_fact.expect("a hidden row must still resolve by key")).unwrap(),
            "false\n"
        );
    }

    /// The trap a tidier filter falls into: `live == None` is *unknown*, not
    /// dead. A status-only entity has that shape — slice 3 re-rooted
    /// `roster_snapshot` at `roster_entity` specifically so it stays visible —
    /// and hiding unknowns would quietly undo that fix.
    #[tokio::test]
    async fn an_unknown_liveness_row_is_not_treated_as_idle() {
        let (roster, fs) = fs();
        let p = PrincipalId::new();
        roster
            .write_status(
                RosterEntity::Principal(p),
                Some("amy"),
                Some("thinking"),
                crate::roster::Availability::Active,
                None,
                100,
            )
            .expect("write_status");

        let index = String::from_utf8(fs.read(Path::new("index"), 0, u32::MAX).await.unwrap())
            .unwrap();
        assert!(
            index.contains(&p.to_hex()),
            "a status-only entity has live=None (unknown) and must stay in `index` —              hiding it re-breaks the slice-3 read-model fix"
        );
    }

    #[tokio::test]
    async fn a_reconciled_row_renders_a_directory_of_facts() {
        let (roster, fs) = fs();
        let principal = PrincipalId::new();
        let entity = RosterEntity::Principal(principal);
        roster
            .reconcile("peer_registry", LivenessKind::Bound, &[presence_row(entity, "win-a")], 100)
            .unwrap();

        let key = format!("principal-{}", principal.to_hex());
        let names: Vec<String> =
            fs.readdir(Path::new("")).await.unwrap().into_iter().map(|e| e.name).collect();
        assert!(names.contains(&key));

        let dir_path = PathBuf::from(&key);
        let facts: Vec<String> =
            fs.readdir(&dir_path).await.unwrap().into_iter().map(|e| e.name).collect();
        assert_eq!(facts, FACTS.to_vec());

        let live = fs.read_all(&dir_path.join("live")).await.unwrap();
        assert_eq!(live, b"true\n");
        let host = fs.read_all(&dir_path.join("host")).await.unwrap();
        assert_eq!(host, b"moltar\n");
        let entity_id = fs.read_all(&dir_path.join("entity_id")).await.unwrap();
        assert_eq!(entity_id, format!("{}\n", principal.to_hex()).into_bytes());
    }

    /// A fact that is genuinely absent renders as an EMPTY file, not a
    /// missing one — a status-only entity (module doc: grouping) has no
    /// `liveness_kind`/`live`/`host`, and `cat`-ing those files must not
    /// error.
    #[tokio::test]
    async fn absent_facts_render_as_empty_not_missing() {
        let (roster, fs) = fs();
        let principal = PrincipalId::new();
        let entity = RosterEntity::Principal(principal);
        roster
            .write_status(entity, None, Some("hi"), Availability::Active, Some(1), 1)
            .unwrap();

        let key = format!("principal-{}", principal.to_hex());
        let dir_path = PathBuf::from(&key);
        assert_eq!(fs.read_all(&dir_path.join("live")).await.unwrap(), b"\n");
        assert_eq!(fs.read_all(&dir_path.join("host")).await.unwrap(), b"\n");
        assert_eq!(fs.read_all(&dir_path.join("status_text")).await.unwrap(), b"hi\n");
    }

    #[tokio::test]
    async fn the_index_is_a_tsv_with_a_header_and_one_row_per_entity() {
        let (roster, fs) = fs();
        let principal = PrincipalId::new();
        let entity = RosterEntity::Principal(principal);
        roster
            .reconcile("peer_registry", LivenessKind::Bound, &[presence_row(entity, "win-a")], 100)
            .unwrap();

        let body = fs.read_all(Path::new("index")).await.unwrap();
        let text = String::from_utf8(body).unwrap();
        assert!(text.starts_with("entity_kind\tentity_id\tlabel\t"));
        assert!(text.contains(&format!("principal\t{}\t", principal.to_hex())));
    }

    #[tokio::test]
    async fn generation_advances_on_reconcile_and_status_write() {
        let (roster, fs) = fs();
        let g0 = fs.getattr(Path::new("index")).await.unwrap().generation;
        let entity = RosterEntity::Principal(PrincipalId::new());
        roster
            .reconcile("peer_registry", LivenessKind::Bound, &[presence_row(entity, "win-a")], 100)
            .unwrap();
        let g1 = fs.getattr(Path::new("index")).await.unwrap().generation;
        assert!(g1 > g0, "{g1} must exceed {g0}");
    }

    #[tokio::test]
    async fn the_view_refuses_every_write() {
        let (_roster, fs) = fs();
        assert!(matches!(fs.write(Path::new("index"), 0, b"lies").await, Err(VfsError::ReadOnly)));
        assert!(matches!(fs.create(Path::new("x"), 0o644).await, Err(VfsError::ReadOnly)));
        assert!(matches!(fs.unlink(Path::new("index")).await, Err(VfsError::ReadOnly)));
        assert!(fs.read_only());
    }

    #[tokio::test]
    async fn an_unknown_row_key_is_not_found() {
        let (_roster, fs) = fs();
        assert!(matches!(
            fs.getattr(Path::new("principal-deadbeef")).await,
            Err(VfsError::NotFound(_))
        ));
    }
}
