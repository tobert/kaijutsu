//! VFS activity digest/cursor — Lane K, FSN slice-1 (`docs/scenes/vfs.md`).
//!
//! [`MountTable`] tracks per-directory activity as ABSOLUTE monotonic totals
//! (see `mount.rs`: `activity` map + `activity_epoch`). This module turns
//! those totals into a lossy-safe push stream: [`MountTable::activity_digest`]
//! computes what a subscriber needs to catch up, and [`ActivityCursor`]
//! remembers what that subscriber has already been sent.
//!
//! **Why absolute totals, not deltas**: a delta stream requires every tick to
//! arrive, in order, exactly once — drop one and the receiver's running total
//! is permanently wrong with no way to notice. An absolute total self-heals:
//! whatever the last successfully-delivered tick said, the next one simply
//! says the truth as of now. A dropped network write, a lagging subscriber, a
//! cap that couldn't fit every hot directory this tick — none of it is a bug
//! to recover from, it's just "the next tick will say more."
//!
//! **Two-phase diff/commit**: `activity_digest` is pure — it computes a
//! digest from `&self` (the table) and `&ActivityCursor` (read-only) without
//! touching either. The caller (the server-side push bridge) only calls
//! [`ActivityCursor::commit`] after the digest has actually been delivered
//! (send succeeded). This makes every failure mode self-healing by
//! construction:
//! - A digest that fails to send is simply recomputed, identically, next
//!   tick (the cursor never advanced, so nothing was "lost").
//! - An entry dropped by the `max_entries` cap was never recorded into the
//!   cursor's `last_sent` map (only what's actually IN the returned digest
//!   gets committed) — so it stays a diff candidate and reappears on a later
//!   tick, largest-delta-first, until it's finally included. Committing a
//!   TRUNCATED digest deliberately does NOT advance `last_global`
//!   (see [`ActivityCursor::commit`]): the epoch short-circuit stays open,
//!   so the stragglers keep draining on subsequent ticks even if the kernel
//!   goes completely quiet after the burst — without this, a burst wider
//!   than the cap followed by silence would strand the overflow heat
//!   forever (the epoch would equal `last_global` and every later tick
//!   would short-circuit to `None`).
//!
//! **Cost model**: `activity_digest` walks every directory that has EVER
//! recorded activity (`MountTable::activity_snapshot`), which is O(number of
//! directories ever touched since boot) — not O(size of the tree). At FSN's
//! current scale this is accepted as free; the `activity_epoch` short-circuit
//! means a quiet kernel's tick costs one relaxed atomic load and nothing else.
//! Likewise `ActivityCursor::last_sent` costs one `HashMap` entry per
//! directory per subscriber — also accepted at this scale, and scoped
//! per-connection (see `kaijutsu-server`), so it dies with the subscriber.

use std::collections::HashMap;
use std::path::PathBuf;

use super::mount::MountTable;

/// One tick's worth of activity, ready to ship over the wire. `entries` are
/// `(directory, absolute total)` pairs for directories whose total has
/// changed since the subscriber's cursor last saw them; `global_total` is the
/// table's epoch AT THE MOMENT this digest was computed (not "now" — the
/// digest and the epoch it reports must be consistent with each other so
/// [`ActivityCursor::commit`] records the right baseline).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActivityDigest {
    pub entries: Vec<(PathBuf, u64)>,
    pub global_total: u64,
    /// `true` when the `max_entries` cap cut candidates from this digest —
    /// there were more changed directories than fit. Drives the commit
    /// policy (a truncated commit must not advance the cursor's
    /// `last_global`; see [`ActivityCursor::commit`]). Not shipped on the
    /// wire: a subscriber doesn't act on it, the server-side cursor does.
    pub truncated: bool,
}

/// Per-subscriber memory of what's already been delivered. Lives on the
/// server-side push bridge (one per connection) — see
/// `kaijutsu-server::rpc::subscribe_vfs_activity`. `Default` starts a fresh
/// subscriber at "has seen nothing," so its very first digest reports every
/// directory that has ever recorded activity since kernel boot (a full
/// resync, same idea as `BASELINE_GENERATION`'s "never observed" state).
#[derive(Debug, Default, Clone)]
pub struct ActivityCursor {
    last_global: u64,
    last_sent: HashMap<PathBuf, u64>,
}

impl ActivityCursor {
    /// Record a digest that was ACTUALLY delivered. Only entries present in
    /// `digest.entries` update `last_sent` — anything the cap left out simply
    /// isn't touched, so it's still a diff candidate (its `last_sent` value,
    /// if any, is still stale) on the next call to `activity_digest`.
    ///
    /// `last_global` advances to `digest.global_total` ONLY for a complete
    /// (non-truncated) digest. The epoch short-circuit in `activity_digest`
    /// means "the cursor is fully caught up," and a truncated digest is not
    /// that: advancing on truncation would close the short-circuit with
    /// stragglers still undelivered, stranding them forever if no new
    /// activity ever reopens it (lead-review catch, 2026-07-13). Holding
    /// `last_global` back instead keeps every subsequent tick non-quiet, so
    /// the stragglers drain largest-delta-first — each truncated commit
    /// still records its delivered entries into `last_sent`, shrinking the
    /// candidate set — until a final non-truncated digest advances
    /// `last_global` and the quiet-tick short-circuit re-engages. The drain
    /// terminates by construction: each round delivers ≥1 previously-stale
    /// entry, and quiet means no new ones are minted.
    pub fn commit(&mut self, digest: &ActivityDigest) {
        if !digest.truncated {
            self.last_global = digest.global_total;
        }
        for (path, total) in &digest.entries {
            self.last_sent.insert(path.clone(), *total);
        }
    }
}

impl MountTable {
    /// Compute the digest a subscriber holding `cursor` needs to catch up,
    /// or `None` on a quiet tick (the global epoch hasn't moved since the
    /// cursor's last commit — nothing anywhere has changed, so there is
    /// nothing to say). PURE: does not mutate `cursor` or any table state;
    /// the caller commits separately, only after a successful send (see the
    /// module doc's two-phase diff/commit reasoning).
    ///
    /// Non-quiet path: every directory with a recorded total different from
    /// what `cursor` last saw for it (0 if never seen) is a candidate.
    /// Candidates are capped at `max_entries`, keeping the LARGEST deltas
    /// first — the busiest directories are the most useful signal, and
    /// whatever's cut simply reappears next tick (uncommitted candidates
    /// aren't lost, see [`ActivityCursor::commit`]). A capped digest reports
    /// `truncated: true`, which tells `commit` to hold `last_global` back so
    /// the drain continues even on a subsequently quiet kernel.
    pub fn activity_digest(&self, cursor: &ActivityCursor, max_entries: usize) -> Option<ActivityDigest> {
        let epoch = self.global_activity();
        if epoch == cursor.last_global {
            return None;
        }

        let mut candidates: Vec<(PathBuf, u64, u64)> = self
            .activity_snapshot(None)
            .into_iter()
            .filter_map(|(path, total)| {
                let last = cursor.last_sent.get(&path).copied().unwrap_or(0);
                if last == total {
                    None
                } else {
                    let delta = total.saturating_sub(last);
                    Some((path, total, delta))
                }
            })
            .collect();

        // Largest delta first; break ties on path for determinism.
        candidates.sort_by(|a, b| b.2.cmp(&a.2).then_with(|| a.0.cmp(&b.0)));
        let truncated = candidates.len() > max_entries;
        candidates.truncate(max_entries);

        let entries = candidates.into_iter().map(|(path, total, _)| (path, total)).collect();
        Some(ActivityDigest { entries, global_total: epoch, truncated })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vfs::backends::MemoryBackend;
    use crate::vfs::ops::VfsOps;
    use std::path::Path;

    #[tokio::test]
    async fn quiet_tick_returns_none() {
        let table = MountTable::new();
        table.mount("/scratch", MemoryBackend::new()).await;
        let cursor = ActivityCursor::default();

        assert!(
            table.activity_digest(&cursor, 256).is_none(),
            "no activity has ever happened, so a fresh cursor sees a quiet tick"
        );
    }

    #[tokio::test]
    async fn digest_reports_changed_entries_with_absolute_totals() {
        let table = MountTable::new();
        table.mount("/scratch", MemoryBackend::new()).await;
        table.create(Path::new("/scratch/a.txt"), 0o644).await.unwrap();
        table.write(Path::new("/scratch/a.txt"), 0, b"hi").await.unwrap();

        let mut cursor = ActivityCursor::default();
        let digest = table.activity_digest(&cursor, 256).expect("activity happened");
        assert_eq!(digest.global_total, table.global_activity());
        let (_, total) = digest
            .entries
            .iter()
            .find(|(p, _)| p == Path::new("/scratch"))
            .expect("/scratch is a candidate");
        assert_eq!(*total, 2, "create + write = 2 absolute total, no prior baseline to diff against");

        cursor.commit(&digest);
        table.write(Path::new("/scratch/a.txt"), 0, b"more").await.unwrap();
        let digest2 = table.activity_digest(&cursor, 256).expect("more activity happened");
        let (_, total2) = digest2
            .entries
            .iter()
            .find(|(p, _)| p == Path::new("/scratch"))
            .expect("/scratch is a candidate again");
        assert_eq!(*total2, 3, "absolute cumulative total after commit, never a bare delta of 1");
    }

    #[tokio::test]
    async fn cap_keeps_largest_deltas_and_dropped_entries_return_next_tick() {
        let table = MountTable::new();
        table.mount("/a", MemoryBackend::new()).await;
        table.mount("/b", MemoryBackend::new()).await;
        table.mount("/c", MemoryBackend::new()).await;

        // /a: 1 bump. /b: 3 bumps. /c: 2 bumps.
        table.create(Path::new("/a/x"), 0o644).await.unwrap();
        table.create(Path::new("/b/x"), 0o644).await.unwrap();
        table.write(Path::new("/b/x"), 0, b"1").await.unwrap();
        table.write(Path::new("/b/x"), 0, b"22").await.unwrap();
        table.create(Path::new("/c/x"), 0o644).await.unwrap();
        table.write(Path::new("/c/x"), 0, b"1").await.unwrap();

        let mut cursor = ActivityCursor::default();
        let digest = table.activity_digest(&cursor, 2).expect("activity happened");
        assert_eq!(digest.entries.len(), 2, "cap holds only 2 of the 3 changed directories");
        let names: Vec<String> =
            digest.entries.iter().map(|(p, _)| p.to_string_lossy().into_owned()).collect();
        assert!(names.contains(&"/b".to_string()), "largest delta (3) must survive the cap");
        assert!(names.contains(&"/c".to_string()), "second largest delta (2) must survive the cap");
        assert!(!names.contains(&"/a".to_string()), "smallest delta (1) is the one dropped");

        cursor.commit(&digest);

        // New activity anywhere lifts the global epoch so the next tick
        // isn't quiet — this is the moment the dropped /a candidate returns.
        table.write(Path::new("/c/x"), 0, b"333").await.unwrap();

        let digest2 = table.activity_digest(&cursor, 256).expect("epoch moved");
        let a_entry = digest2.entries.iter().find(|(p, _)| p == Path::new("/a"));
        assert_eq!(
            a_entry.map(|(_, t)| *t),
            Some(1),
            "the cap-dropped entry must reappear once ANY new tick fires, absolute total intact"
        );
        let b_entry = digest2.entries.iter().find(|(p, _)| p == Path::new("/b"));
        assert!(b_entry.is_none(), "/b was already committed and hasn't changed since — not a candidate");
        let c_entry = digest2.entries.iter().find(|(p, _)| p == Path::new("/c"));
        assert_eq!(c_entry.map(|(_, t)| *t), Some(3), "/c's new total reflects the latest write");
    }

    #[tokio::test]
    async fn cap_dropped_entries_drain_on_a_quiet_kernel_and_terminate() {
        // The stranding bug (lead review, 2026-07-13): a burst touches more
        // directories than the cap, the capped digest is committed, and then
        // the kernel goes COMPLETELY quiet. If commit advanced last_global to
        // the epoch, the next tick's short-circuit would return None forever
        // and the overflow heat would never arrive — contradicting the module
        // doc's "reappears on a later tick until it's finally included."
        // A truncated commit must therefore leave the epoch check open until
        // a non-truncated digest finally delivers everything.
        let table = MountTable::new();
        table.mount("/a", MemoryBackend::new()).await;
        table.mount("/b", MemoryBackend::new()).await;
        table.mount("/c", MemoryBackend::new()).await;

        // Burst: /a 1 bump, /b 3 bumps, /c 2 bumps. Then total quiet.
        table.create(Path::new("/a/x"), 0o644).await.unwrap();
        table.create(Path::new("/b/x"), 0o644).await.unwrap();
        table.write(Path::new("/b/x"), 0, b"1").await.unwrap();
        table.write(Path::new("/b/x"), 0, b"22").await.unwrap();
        table.create(Path::new("/c/x"), 0o644).await.unwrap();
        table.write(Path::new("/c/x"), 0, b"1").await.unwrap();

        let mut cursor = ActivityCursor::default();

        // Tick 1 (cap=1): only the largest delta (/b) fits. Commit it.
        let d1 = table.activity_digest(&cursor, 1).expect("burst happened");
        assert_eq!(d1.entries.len(), 1);
        assert_eq!(d1.entries[0].0, Path::new("/b"), "largest delta first");
        cursor.commit(&d1);

        // Tick 2, NO new activity anywhere: the dropped entries must still
        // drain. Next-largest straggler is /c.
        let d2 = table
            .activity_digest(&cursor, 1)
            .expect("cap-dropped entries must drain even on a quiet kernel");
        assert_eq!(d2.entries.len(), 1);
        assert_eq!(d2.entries[0], (PathBuf::from("/c"), 2), "next-largest straggler, absolute total");
        cursor.commit(&d2);

        // Tick 3, still quiet: the last straggler /a.
        let d3 = table
            .activity_digest(&cursor, 1)
            .expect("the final straggler must still drain");
        assert_eq!(d3.entries.len(), 1);
        assert_eq!(d3.entries[0], (PathBuf::from("/a"), 1));
        cursor.commit(&d3);

        // Tick 4: everything delivered — the drain terminates. This guards
        // against the opposite bug (never advancing last_global ⇒ a spinning
        // non-quiet tick with an empty diff forever).
        assert!(
            table.activity_digest(&cursor, 1).is_none(),
            "once every straggler is delivered, the quiet-tick short-circuit must re-engage"
        );
    }

    #[tokio::test]
    async fn uncommitted_digest_is_reissued_identically() {
        let table = MountTable::new();
        table.mount("/scratch", MemoryBackend::new()).await;
        table.create(Path::new("/scratch/a.txt"), 0o644).await.unwrap();

        let cursor = ActivityCursor::default();
        let digest1 = table.activity_digest(&cursor, 256).expect("activity happened");
        let digest2 = table.activity_digest(&cursor, 256).expect("still there — nothing committed");

        assert_eq!(digest1, digest2, "a failed/uncommitted send must be safely retryable, byte for byte");
    }
}
