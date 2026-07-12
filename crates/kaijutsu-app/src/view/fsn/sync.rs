//! FSN-world data: the `vfs_snapshot` poll → [`FsnState`] drain, mirroring
//! `time_well::rays`' poll/apply shape (`poll_tracks`/`apply_tracks`) — one
//! in-flight request at a time, no per-frame RPC. This is the "enumeration
//! on demand is the LOD scheduler" plumbing (`docs/scenes/vfs.md` claim 3):
//! [`FsnState::request`] queues a path (initial entry, or an approach/select
//! on a cell whose directory isn't known yet or was truncated);
//! [`poll_fsn_snapshot`] drains the queue one request at a time;
//! [`apply_fsn_snapshot`] unpacks the reply into [`FsnState::listings`].
//!
//! # Why a flat `path -> DirListing` map, not a recursive tree
//!
//! A `vfs_snapshot(path, depth=2, ...)` reply already gives one directory's
//! own listing AND (unless truncated) each of its subdirectories' listings
//! in the same response — [`ingest_snapshot`] unpacks that whole response
//! into the flat map at once, so a single "generous depth" fetch front-loads
//! two levels of LOD for free, exactly the vfs.md claim 3 payoff. A deeper
//! re-query later just overwrites/adds entries at their own paths; nothing
//! needs to walk back up a tree to merge it in.

use std::collections::{HashMap, VecDeque};

use bevy::prelude::*;
use kaijutsu_client::{SnapshotNode, VfsFileType};

use crate::connection::{RpcActor, RpcResultChannel, RpcResultMessage};

use super::layout::join_path;

/// `vfs_snapshot`'s `depth` argument — 2 levels front-loads a directory's
/// children AND grandchildren in one request (see the module doc).
/// **Amy-tunable.**
const FETCH_DEPTH: u32 = 2;

/// `vfs_snapshot`'s `max_entries` cap — generous (slice 0 has no pagination
/// UI yet); the kernel clamps regardless (`KernelHandle::vfs_snapshot`'s own
/// doc). **Amy-tunable.**
const FETCH_MAX_ENTRIES: u32 = 4000;

/// One child of a known directory — the flattened, renderer-facing mirror of
/// [`kaijutsu_client::SnapshotNode`]'s own fields (everything
/// [`super::layout::child_spec`] and the selection/approach logic need).
#[derive(Debug, Clone, PartialEq)]
pub struct ChildMeta {
    pub name: String,
    pub kind: VfsFileType,
    pub size: u64,
    pub child_count: u32,
    /// Display metadata only — **never** filters a child out (thin-client
    /// rule, this module's own contract: `docs/scenes/vfs.md`'s "Gitignored
    /// wastes get weather," not silence). [`super::scene`] renders an
    /// ignored cell dimmer; it never skips spawning one.
    pub ignored: bool,
}

/// One directory's own listing — everything needed to lay out its field.
#[derive(Debug, Clone, PartialEq)]
pub struct DirListing {
    pub generation: u64,
    /// This directory's own listing hit the kernel's cap — re-approach
    /// should re-fetch it (deeper doesn't help; `max_entries` is the limit
    /// here, not depth, but the request/retry path is identical either way).
    pub truncated_here: bool,
    pub children: Vec<ChildMeta>,
}

/// The FSN world's fetched-so-far VFS state: a cache of directory listings
/// keyed by absolute path, plus the debounced fetch queue. Survives a
/// Screen::Fsn exit/re-entry (kept as a cache — re-diving doesn't re-fetch
/// the root from scratch); nothing currently invalidates a stale entry by
/// generation (`docs/scenes/vfs.md`'s stage-2 fsnotify work is out of scope
/// for slice 0 — noted in `docs/issues.md`).
#[derive(Resource, Default)]
pub struct FsnState {
    pub listings: HashMap<String, DirListing>,
    pending: VecDeque<String>,
    in_flight: Option<String>,
}

impl FsnState {
    /// Queue `path` for a (re)fetch, unless it's already known-and-complete,
    /// already queued, or already in flight — the debounce this module's
    /// whole design rests on.
    ///
    /// **Call sites must be edge-triggered, never per-frame.** A truncated
    /// listing (and a path whose last fetch *failed* — it has no listing at
    /// all) stays re-queueable BY DESIGN, so this is deliberately not
    /// idempotent for those cases: a per-frame caller would refetch a
    /// genuinely-truncated directory (more entries than the kernel cap, e.g.
    /// a huge `target/`) forever, one RPC after another. Fire it on events —
    /// world entry, a selection *flip* — and re-selecting later is the retry
    /// policy for both truncation and failure.
    pub fn request(&mut self, path: String) {
        if let Some(listing) = self.listings.get(&path)
            && !listing.truncated_here
        {
            return;
        }
        if self.in_flight.as_deref() == Some(path.as_str()) {
            return;
        }
        if self.pending.contains(&path) {
            return;
        }
        self.pending.push_back(path);
    }

    /// Pop the next pending path and mark it in flight — the claim step of
    /// [`poll_fsn_snapshot`], extracted so tests can drive the queue's
    /// wedge/unwedge behavior without a live RPC actor. `None` while a
    /// request is already in flight (the single-flight debounce) or the
    /// queue is empty.
    fn take_next_request(&mut self) -> Option<String> {
        if self.in_flight.is_some() {
            return None;
        }
        let path = self.pending.pop_front()?;
        self.in_flight = Some(path.clone());
        Some(path)
    }

    /// Clear the in-flight debounce slot if it matches `path` — called on
    /// BOTH reply arms ([`apply_fsn_snapshot`]: received *and* failed), so
    /// the queue can never wedge on a failed fetch. Failure deliberately
    /// does NOT re-queue the path: a permanently-failing path would hot-loop
    /// the RPC; a later `request` (a fresh selection flip) is the retry.
    fn settle_in_flight(&mut self, path: &str) {
        if self.in_flight.as_deref() == Some(path) {
            self.in_flight = None;
        }
    }

    /// Whether `path`'s own listing is known (regardless of truncation) —
    /// the enumeration-state input [`super::layout::lod_tier`] wants.
    pub fn is_enumerated(&self, path: &str) -> bool {
        self.listings.contains_key(path)
    }
}

/// Recursively unpack a `vfs_snapshot` response into `state.listings`:
/// insert `base_path`'s own listing from `node.children`, then recurse into
/// every child that is ITSELF a directory the response already expanded
/// (non-empty `children`, per the depth-2 front-load — see the module doc).
/// A child directory with `child_count > 0` but an empty `children` vec was
/// truncated by depth, not walked; it stays unenumerated until its own
/// `request` fires.
fn ingest_snapshot(state: &mut FsnState, base_path: &str, node: &SnapshotNode) {
    let children: Vec<ChildMeta> = node
        .children
        .iter()
        .map(|c| ChildMeta {
            name: c.name.clone(),
            kind: c.kind,
            size: c.size,
            child_count: c.child_count,
            ignored: c.ignored,
        })
        .collect();
    state.listings.insert(
        base_path.to_string(),
        DirListing { generation: node.generation, truncated_here: node.truncated_here, children },
    );
    for child in &node.children {
        if child.kind == VfsFileType::Directory && !child.children.is_empty() {
            let child_path = join_path(base_path, &child.name);
            ingest_snapshot(state, &child_path, child);
        }
    }
}

/// Drain [`FsnState`]'s pending queue one request at a time — the debounce:
/// nothing fires while a request is already in flight. Runs every frame
/// while `Screen::Fsn` is active (ambient within the screen; there's no
/// "elsewhere" for this data to stay warm across, unlike the well's
/// always-open polls — the world itself only exists while dived).
pub fn poll_fsn_snapshot(
    actor: Option<Res<RpcActor>>,
    mut state: ResMut<FsnState>,
    result_channel: Res<RpcResultChannel>,
) {
    let Some(actor) = actor else { return };
    let Some(path) = state.take_next_request() else { return };

    let handle = actor.handle.clone();
    let tx = result_channel.sender();
    bevy::tasks::IoTaskPool::get()
        .spawn(async move {
            match handle.vfs_snapshot(&path, FETCH_DEPTH, FETCH_MAX_ENTRIES).await {
                Ok(result) => {
                    let _ = tx.send(RpcResultMessage::VfsSnapshotReceived { path, result });
                }
                Err(e) => {
                    // The failure MUST land as a reply too — apply_fsn_snapshot
                    // clears the in-flight debounce on both arms; swallowing
                    // the error here would wedge the queue forever.
                    log::warn!("fsn: vfs_snapshot({path}) failed: {e}");
                    let _ = tx.send(RpcResultMessage::VfsSnapshotFailed { path });
                }
            }
        })
        .detach();
}

/// Drain `vfs_snapshot` replies: a success ingests into
/// [`FsnState::listings`]; BOTH success and failure clear the in-flight
/// debounce ([`FsnState::settle_in_flight`]).
///
/// **UNGATED** — registered outside the `Screen::Fsn` tuple (see
/// `FsnPlugin`): a reply can land after the player Esc'd out of the world,
/// and Bevy messages expire after two frames, so a screen-gated reader would
/// drop that reply and leave the in-flight slot wedged for the next dive.
/// Ingesting on another screen is free (this only writes the cache).
///
/// BOTH arms settle so the next queued path can
/// fire — [`poll_fsn_snapshot`]'s `Err` arm ships `VfsSnapshotFailed` for
/// exactly this, otherwise the first failed fetch would wedge the queue
/// forever. A failure is NOT auto-requeued (a permanently-failing path
/// would hot-loop); a later selection flip re-`request`ing the path is the
/// retry policy.
pub fn apply_fsn_snapshot(mut state: ResMut<FsnState>, mut events: MessageReader<RpcResultMessage>) {
    for ev in events.read() {
        match ev {
            RpcResultMessage::VfsSnapshotReceived { path, result } => {
                state.settle_in_flight(path);
                ingest_snapshot(&mut state, path, &result.root);
            }
            RpcResultMessage::VfsSnapshotFailed { path } => {
                state.settle_in_flight(path);
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(name: &str, kind: VfsFileType, children: Vec<SnapshotNode>) -> SnapshotNode {
        SnapshotNode {
            name: name.into(),
            kind,
            size: 128,
            mtime_secs: 0,
            child_count: children.len() as u32,
            ignored: false,
            generation: 1,
            truncated_here: false,
            children,
        }
    }

    #[test]
    fn request_queues_an_unknown_path() {
        let mut state = FsnState::default();
        state.request("/".into());
        assert_eq!(state.pending.len(), 1);
    }

    #[test]
    fn request_skips_a_path_already_known_and_complete() {
        let mut state = FsnState::default();
        state.listings.insert(
            "/".into(),
            DirListing { generation: 1, truncated_here: false, children: vec![] },
        );
        state.request("/".into());
        assert!(state.pending.is_empty(), "already-complete listing shouldn't re-queue");
    }

    #[test]
    fn request_still_queues_a_truncated_known_path() {
        let mut state = FsnState::default();
        state.listings.insert(
            "/big".into(),
            DirListing { generation: 1, truncated_here: true, children: vec![] },
        );
        state.request("/big".into());
        assert_eq!(state.pending.len(), 1, "a truncated listing must be re-queueable");
    }

    #[test]
    fn request_dedupes_against_pending_and_in_flight() {
        let mut state = FsnState::default();
        state.request("/a".into());
        state.request("/a".into());
        assert_eq!(state.pending.len(), 1, "duplicate pending request must not re-queue");

        state.in_flight = Some("/b".into());
        state.request("/b".into());
        assert_eq!(state.pending.len(), 1, "an in-flight path must not also queue");
    }

    // ── the wedge guard: take_next_request / settle_in_flight ──

    #[test]
    fn take_next_request_claims_one_and_holds_the_single_flight_debounce() {
        let mut state = FsnState::default();
        state.request("/a".into());
        state.request("/b".into());

        assert_eq!(state.take_next_request().as_deref(), Some("/a"));
        assert_eq!(state.in_flight.as_deref(), Some("/a"));
        assert_eq!(state.take_next_request(), None, "second claim must wait for the first to settle");

        state.settle_in_flight("/a");
        assert_eq!(state.take_next_request().as_deref(), Some("/b"), "settling releases the queue");
    }

    /// The wedge regression: a FAILED fetch must clear the in-flight slot
    /// (via `apply_fsn_snapshot`'s `VfsSnapshotFailed` arm →
    /// `settle_in_flight`) so the queue keeps draining, must NOT auto-requeue
    /// the failed path (hot-loop guard), and the path must stay re-requestable
    /// by a later selection flip.
    #[test]
    fn a_failed_fetch_unwedges_the_queue_without_auto_requeueing() {
        let mut state = FsnState::default();
        state.request("/flaky".into());
        let path = state.take_next_request().expect("one pending request to claim");
        assert_eq!(path, "/flaky");

        // While in flight, a re-request is a no-op (the dedup).
        state.request("/flaky".into());
        assert!(state.pending.is_empty());

        // The failure reply lands.
        state.settle_in_flight(&path);
        assert!(state.in_flight.is_none(), "the queue must not wedge on a failed fetch");
        assert!(state.pending.is_empty(), "failure must NOT auto-requeue (hot-loop guard)");

        // A later selection flip retries: no listing exists, so it queues.
        state.request("/flaky".into());
        assert_eq!(state.pending.len(), 1, "a failed path stays re-requestable");
        assert_eq!(state.take_next_request().as_deref(), Some("/flaky"));
    }

    #[test]
    fn settle_in_flight_ignores_a_mismatched_path() {
        let mut state = FsnState::default();
        state.in_flight = Some("/current".into());
        state.settle_in_flight("/other");
        assert_eq!(
            state.in_flight.as_deref(),
            Some("/current"),
            "only the in-flight path's own reply settles it"
        );
    }

    #[test]
    fn is_enumerated_reflects_listings_regardless_of_truncation() {
        let mut state = FsnState::default();
        assert!(!state.is_enumerated("/x"));
        state.listings.insert(
            "/x".into(),
            DirListing { generation: 1, truncated_here: true, children: vec![] },
        );
        assert!(state.is_enumerated("/x"), "truncated is still enumerated (partially known)");
    }

    // ── ingest_snapshot ──

    #[test]
    fn ingest_snapshot_stores_the_root_listing() {
        let mut state = FsnState::default();
        let root = node(
            "/",
            VfsFileType::Directory,
            vec![node("foo.txt", VfsFileType::File, vec![])],
        );
        ingest_snapshot(&mut state, "/", &root);
        let listing = state.listings.get("/").unwrap();
        assert_eq!(listing.children.len(), 1);
        assert_eq!(listing.children[0].name, "foo.txt");
    }

    #[test]
    fn ingest_snapshot_recurses_into_expanded_subdirectories() {
        let mut state = FsnState::default();
        let grandchild = node("baz.rs", VfsFileType::File, vec![]);
        let subdir = node("src", VfsFileType::Directory, vec![grandchild]);
        let root = node("/", VfsFileType::Directory, vec![subdir]);
        ingest_snapshot(&mut state, "/", &root);

        assert!(state.listings.contains_key("/"), "root listed");
        assert!(state.listings.contains_key("/src"), "expanded subdir listed too — depth-2 front-load");
        let src = state.listings.get("/src").unwrap();
        assert_eq!(src.children[0].name, "baz.rs");
    }

    #[test]
    fn ingest_snapshot_does_not_recurse_into_a_truncated_subdirectory() {
        let mut state = FsnState::default();
        // A directory with child_count > 0 but no expanded children — the
        // depth-clamp case: known to exist, not walked.
        let mut deep = node("deep", VfsFileType::Directory, vec![]);
        deep.child_count = 500;
        let root = node("/", VfsFileType::Directory, vec![deep]);
        ingest_snapshot(&mut state, "/", &root);

        assert!(state.listings.contains_key("/"));
        assert!(
            !state.listings.contains_key("/deep"),
            "an unexpanded subdirectory must stay unenumerated until its own request"
        );
    }

    #[test]
    fn ingest_snapshot_join_path_handles_nested_depth() {
        let mut state = FsnState::default();
        let grandchild = node("c.txt", VfsFileType::File, vec![]);
        let subdir = node("b", VfsFileType::Directory, vec![grandchild]);
        let root_child = node("a", VfsFileType::Directory, vec![subdir]);
        let root = node("/", VfsFileType::Directory, vec![root_child]);
        ingest_snapshot(&mut state, "/", &root);
        assert!(state.listings.contains_key("/a"));
        assert!(state.listings.contains_key("/a/b"), "path joins nest correctly past the root");
    }

    #[test]
    fn ingest_snapshot_overwrites_a_stale_listing_at_the_same_path() {
        let mut state = FsnState::default();
        state.listings.insert(
            "/".into(),
            DirListing { generation: 0, truncated_here: true, children: vec![] },
        );
        let fresh = node("/", VfsFileType::Directory, vec![node("new.txt", VfsFileType::File, vec![])]);
        ingest_snapshot(&mut state, "/", &fresh);
        let listing = state.listings.get("/").unwrap();
        assert!(!listing.truncated_here);
        assert_eq!(listing.children.len(), 1);
    }
}
