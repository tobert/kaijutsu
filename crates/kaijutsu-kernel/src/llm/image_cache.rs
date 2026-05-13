//! Per-hash cache of CAS image bytes encoded as base64 + their stored MIME.
//!
//! `resolve_image_blocks_from_cas` is called once per turn. Without a cache,
//! every screenshot in a long conversation is read from disk and
//! base64-encoded again on every prompt. Hashes are content-addressed, so a
//! global per-kernel cache is correct (the same hash never maps to different
//! bytes), and bounding the entry count keeps memory predictable even when
//! a long-lived process accumulates novel images.

use dashmap::DashMap;
use kaijutsu_cas::ContentHash;
use parking_lot::Mutex;
use std::collections::VecDeque;
use std::sync::Arc;

/// Resolved image payload — what the resolver needs to fill into a
/// `ContentBlock::Image`.
#[derive(Debug, Clone)]
pub struct ResolvedImage {
    pub mime_type: String,
    pub data_base64: String,
}

/// Bounded cache of resolved images keyed by content hash.
///
/// Eviction is FIFO insertion-order — fine for the workload (a turn either
/// re-encodes every image in the conversation or it doesn't; access
/// patterns don't favour LRU enough to justify the bookkeeping).
pub struct ImageBase64Cache {
    entries: DashMap<ContentHash, ResolvedImage>,
    order: Mutex<VecDeque<ContentHash>>,
    capacity: usize,
}

impl ImageBase64Cache {
    pub fn new(capacity: usize) -> Self {
        Self {
            entries: DashMap::new(),
            order: Mutex::new(VecDeque::with_capacity(capacity)),
            capacity: capacity.max(1),
        }
    }

    /// Read a cached entry. Returns a clone so callers can drop the dashmap
    /// guard before doing async work.
    pub fn get(&self, hash: &ContentHash) -> Option<ResolvedImage> {
        self.entries.get(hash).map(|e| e.clone())
    }

    /// Insert a freshly resolved entry. If the cache is at capacity, the
    /// oldest entry is evicted.
    pub fn insert(&self, hash: ContentHash, image: ResolvedImage) {
        if self.entries.contains_key(&hash) {
            // Don't bump order on overwrite — duplicate inserts of the same
            // hash always carry the same bytes (content-addressed), so the
            // existing slot is already correct.
            return;
        }
        let mut order = self.order.lock();
        while order.len() >= self.capacity {
            if let Some(evict) = order.pop_front() {
                self.entries.remove(&evict);
            } else {
                break;
            }
        }
        order.push_back(hash);
        // Drop order guard before dashmap insert to keep the locks small.
        let pushed = order.back().cloned();
        drop(order);
        if let Some(hash) = pushed {
            self.entries.insert(hash, image);
        }
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// Shared handle, since the cache is owned by server state and threaded
/// into the resolver from per-stream tasks.
pub type SharedImageBase64Cache = Arc<ImageBase64Cache>;

#[cfg(test)]
mod tests {
    use super::*;
    use kaijutsu_cas::{ContentStore, FileStore};

    fn sample_hash(seed: u8) -> ContentHash {
        let tmp = tempfile::tempdir().unwrap();
        let cas = FileStore::at_path(tmp.path());
        cas.store(&[seed; 32], "image/png").unwrap()
    }

    #[test]
    fn insert_then_get_returns_payload() {
        let cache = ImageBase64Cache::new(4);
        let h = sample_hash(1);
        cache.insert(
            h.clone(),
            ResolvedImage {
                mime_type: "image/png".into(),
                data_base64: "AAAA".into(),
            },
        );
        let got = cache.get(&h).expect("cached entry must be visible");
        assert_eq!(got.mime_type, "image/png");
        assert_eq!(got.data_base64, "AAAA");
    }

    #[test]
    fn miss_returns_none() {
        let cache = ImageBase64Cache::new(4);
        assert!(cache.get(&sample_hash(2)).is_none());
    }

    #[test]
    fn fifo_eviction_when_over_capacity() {
        let cache = ImageBase64Cache::new(2);
        let a = sample_hash(1);
        let b = sample_hash(2);
        let c = sample_hash(3);
        let mk = |s: &str| ResolvedImage {
            mime_type: "image/png".into(),
            data_base64: s.into(),
        };
        cache.insert(a.clone(), mk("a"));
        cache.insert(b.clone(), mk("b"));
        cache.insert(c.clone(), mk("c"));
        assert!(
            cache.get(&a).is_none(),
            "oldest entry must be evicted at capacity"
        );
        assert!(cache.get(&b).is_some());
        assert!(cache.get(&c).is_some());
        assert_eq!(cache.len(), 2);
    }

    #[test]
    fn duplicate_insert_is_noop() {
        // Same hash means same bytes by CAS invariant — a second insert
        // must not consume an extra slot or reorder eviction.
        let cache = ImageBase64Cache::new(2);
        let a = sample_hash(1);
        let b = sample_hash(2);
        let mk = |s: &str| ResolvedImage {
            mime_type: "image/png".into(),
            data_base64: s.into(),
        };
        cache.insert(a.clone(), mk("a"));
        cache.insert(b.clone(), mk("b"));
        cache.insert(a.clone(), mk("a-again"));
        // Adding a third *new* hash should evict 'a' (the oldest), not 'b'.
        let c = sample_hash(3);
        cache.insert(c.clone(), mk("c"));
        assert!(
            cache.get(&a).is_none(),
            "duplicate insert must not refresh order"
        );
        assert!(cache.get(&b).is_some());
        assert!(cache.get(&c).is_some());
    }
}
