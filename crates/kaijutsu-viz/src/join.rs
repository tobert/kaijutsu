//! Keyed data-join reconciler — the two-cadence primitive for data-driven views.
//!
//! # Concept
//!
//! A [`Join<K, V>`] holds a keyed snapshot of the current dataset. Callers drive
//! it through two distinct operations that correspond to two cost-asymmetric kernel
//! surfaces:
//!
//! - **Layout cadence** — [`Join::reconcile`]: diff a whole new snapshot against
//!   the current state and return a [`JoinDiff`] that may contain `enter`, `update`,
//!   and/or `exit` events. This is the only operation that can change the key set.
//!   [`JoinDiff::needs_relayout`] is `true` iff the key set changed.
//!
//! - **Data cadence** — [`Join::touch`]: update a single *existing* key's value.
//!   Guaranteed not to change the key set; returns [`Touched::Absent`] (without
//!   inserting) if the key is unknown. Used for high-frequency live-status updates
//!   that must never trigger a relayout.
//!
//! # Structural two-cadence distinction
//!
//! The design doc requires the distinction be encoded in the type, not left to
//! caller convention:
//! - `enter` / `exit` → key set changes → `needs_relayout() == true`
//! - `update` / no change → key set unchanged → `needs_relayout() == false`
//! - `touch` cannot enter/exit by construction
//!
//! # Fail-loud stance
//!
//! Duplicate keys in a `reconcile` snapshot panic — a duplicate context id is
//! corruption, and the project stance is "crash over silent data corruption."
//!
//! # Determinism
//!
//! The internal state and all diff vecs are [`BTreeMap`]-ordered by key, so
//! downstream consumers and tests see stable ordering.
//!
//! [`BTreeMap`]: std::collections::BTreeMap

use std::collections::BTreeMap;

// ─── Diff payloads ───────────────────────────────────────────────────────────

/// A key that just entered the dataset (new key in a [`Join::reconcile`] snapshot).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entered<K, V> {
    /// The new key.
    pub key: K,
    /// The initial value for this key.
    pub value: V,
}

/// A key that persisted across a [`Join::reconcile`] but whose value changed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Updated<K, V> {
    /// The key.
    pub key: K,
    /// The value before this reconcile.
    pub old: V,
    /// The value after this reconcile.
    pub new: V,
}

/// A key that was removed by a [`Join::reconcile`].
///
/// The last known value is preserved here so callers (e.g. animated-exit
/// transitions) can inspect what disappeared.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Exited<K, V> {
    /// The removed key.
    pub key: K,
    /// The last value held before removal.
    pub last: V,
}

// ─── JoinDiff ────────────────────────────────────────────────────────────────

/// The diff produced by a single [`Join::reconcile`] call.
///
/// Entries appear in [`BTreeMap`]-key order within each vec for determinism.
///
/// [`BTreeMap`]: std::collections::BTreeMap
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JoinDiff<K, V> {
    /// Keys that did not exist before and now do (with their initial value).
    pub enter: Vec<Entered<K, V>>,
    /// Keys that existed before and still do, but whose value changed
    /// (carries old and new value).
    pub update: Vec<Updated<K, V>>,
    /// Keys that existed before but are absent from the new snapshot
    /// (carries the last value for animated-exit use).
    pub exit: Vec<Exited<K, V>>,
}

impl<K, V> JoinDiff<K, V> {
    /// Returns `true` iff the key set changed (any `enter` or `exit` events).
    ///
    /// This is the structural encoding of the "layout cadence vs. data cadence"
    /// distinction (`docs/timewell.md`, substrate-notes appendix): a status-only update must never
    /// trigger a relayout; only a context being created or archived should.
    pub fn needs_relayout(&self) -> bool {
        !self.enter.is_empty() || !self.exit.is_empty()
    }

    /// Returns `true` iff the diff is entirely empty (no enter, update, or exit).
    pub fn is_empty(&self) -> bool {
        self.enter.is_empty() && self.update.is_empty() && self.exit.is_empty()
    }
}

// ─── Touched ─────────────────────────────────────────────────────────────────

/// Result of a [`Join::touch`] (data-cadence) operation.
///
/// `touch` is guaranteed not to change the key set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Touched<V> {
    /// The key existed and its value changed.  Carries the **previous** value.
    Changed { old: V },
    /// The key existed and the new value was identical to the old one; no change.
    Unchanged,
    /// The key was not present.  Nothing was inserted.
    Absent,
}

// ─── Join ────────────────────────────────────────────────────────────────────

/// A keyed data-join reconciler.
///
/// See the [module documentation](self) for the full design and usage notes.
#[derive(Debug, Clone)]
pub struct Join<K, V> {
    state: BTreeMap<K, V>,
}

impl<K: Ord + Clone, V: Clone + PartialEq> Join<K, V> {
    /// Create an empty `Join`.
    pub fn new() -> Self {
        Self { state: BTreeMap::new() }
    }

    /// **Layout-cadence operation.** Diff `snapshot` against the current state
    /// and return a [`JoinDiff`].
    ///
    /// After this call `self` reflects the snapshot exactly.
    ///
    /// # Duplicate keys
    ///
    /// Panics if `snapshot` contains duplicate keys — duplicate context ids are
    /// data corruption. The project stance is "crash over silent data corruption."
    ///
    /// # Complexity
    ///
    /// O(S log S + N log N) where S = snapshot size and N = current state size.
    pub fn reconcile(&mut self, snapshot: impl IntoIterator<Item = (K, V)>) -> JoinDiff<K, V>
    where
        K: std::fmt::Debug,
    {
        // Collect snapshot into a BTreeMap, failing loudly on duplicates.
        let mut snap: BTreeMap<K, V> = BTreeMap::new();
        for (k, v) in snapshot {
            if snap.insert(k.clone(), v).is_some() {
                panic!("Join::reconcile: duplicate key {k:?} in snapshot — data corruption");
            }
        }

        let mut enter = Vec::new();
        let mut update = Vec::new();
        let mut exit = Vec::new();

        // --- exits: keys in current state but absent from snapshot ---
        for (k, v) in &self.state {
            if !snap.contains_key(k) {
                exit.push(Exited { key: k.clone(), last: v.clone() });
            }
        }

        // --- enters and updates: keys in snapshot ---
        for (k, v) in &snap {
            match self.state.get(k) {
                None => {
                    enter.push(Entered { key: k.clone(), value: v.clone() });
                }
                Some(old) if old != v => {
                    update.push(Updated { key: k.clone(), old: old.clone(), new: v.clone() });
                }
                Some(_) => {
                    // value unchanged — neither enter nor update
                }
            }
        }

        // Apply the snapshot as the new state.
        self.state = snap;

        JoinDiff { enter, update, exit }
    }

    /// **Data-cadence operation.** Update the value of an *existing* key.
    ///
    /// - Returns [`Touched::Changed`] (with the old value) if the key existed and
    ///   the value changed.
    /// - Returns [`Touched::Unchanged`] if the key existed but the new value was
    ///   identical.
    /// - Returns [`Touched::Absent`] if the key is not present.  Nothing is
    ///   inserted; the key set is never changed.
    pub fn touch(&mut self, key: &K, value: V) -> Touched<V> {
        match self.state.get_mut(key) {
            None => Touched::Absent,
            Some(slot) => {
                if *slot == value {
                    Touched::Unchanged
                } else {
                    let old = std::mem::replace(slot, value);
                    Touched::Changed { old }
                }
            }
        }
    }

    /// Look up the current value for a key.
    pub fn get(&self, key: &K) -> Option<&V> {
        self.state.get(key)
    }

    /// Iterate over all current keys in [`BTreeMap`]-order.
    ///
    /// [`BTreeMap`]: std::collections::BTreeMap
    pub fn keys(&self) -> impl Iterator<Item = &K> {
        self.state.keys()
    }

    /// The number of keys currently in the join.
    pub fn len(&self) -> usize {
        self.state.len()
    }

    /// Returns `true` iff no keys are currently held.
    pub fn is_empty(&self) -> bool {
        self.state.is_empty()
    }

    /// Returns `true` iff the given key is currently held.
    pub fn contains(&self, key: &K) -> bool {
        self.state.contains_key(key)
    }
}

impl<K: Ord + Clone, V: Clone + PartialEq> Default for Join<K, V> {
    fn default() -> Self {
        Self::new()
    }
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── Helpers ────────────────────────────────────────────────────────────

    fn snap(pairs: &[(&str, u32)]) -> Vec<(String, u32)> {
        pairs.iter().map(|(k, v)| (k.to_string(), *v)).collect()
    }

    fn keys_sorted(join: &Join<String, u32>) -> Vec<String> {
        join.keys().cloned().collect()
    }

    // ── enter from empty ───────────────────────────────────────────────────

    #[test]
    fn enter_from_empty() {
        let mut j: Join<String, u32> = Join::new();
        let diff = j.reconcile(snap(&[("a", 1), ("b", 2)]));
        assert_eq!(diff.enter.len(), 2, "should have 2 enters");
        assert!(diff.update.is_empty());
        assert!(diff.exit.is_empty());
        assert!(diff.needs_relayout(), "enter must trigger relayout");
        // payloads carry the right values
        assert_eq!(diff.enter[0], Entered { key: "a".to_string(), value: 1 });
        assert_eq!(diff.enter[1], Entered { key: "b".to_string(), value: 2 });
        assert_eq!(j.len(), 2);
    }

    // ── exit to empty ──────────────────────────────────────────────────────

    #[test]
    fn exit_to_empty() {
        let mut j: Join<String, u32> = Join::new();
        j.reconcile(snap(&[("x", 10)]));
        let diff = j.reconcile(snap(&[]));
        assert!(diff.enter.is_empty());
        assert!(diff.update.is_empty());
        assert_eq!(diff.exit.len(), 1, "should have 1 exit");
        assert!(diff.needs_relayout(), "exit must trigger relayout");
        assert_eq!(diff.exit[0], Exited { key: "x".to_string(), last: 10 });
        assert!(j.is_empty());
    }

    // ── update on changed value ────────────────────────────────────────────

    #[test]
    fn update_on_changed_value() {
        let mut j: Join<String, u32> = Join::new();
        j.reconcile(snap(&[("a", 1)]));
        let diff = j.reconcile(snap(&[("a", 2)]));
        assert!(diff.enter.is_empty());
        assert!(diff.exit.is_empty());
        assert_eq!(diff.update.len(), 1);
        assert!(!diff.needs_relayout(), "update-only diff must NOT trigger relayout");
        assert_eq!(diff.update[0], Updated { key: "a".to_string(), old: 1, new: 2 });
    }

    // ── no update on unchanged re-poll (idempotent) ────────────────────────

    #[test]
    fn no_update_on_identical_repoll() {
        let mut j: Join<String, u32> = Join::new();
        j.reconcile(snap(&[("a", 1), ("b", 2)]));
        let diff = j.reconcile(snap(&[("a", 1), ("b", 2)]));
        assert!(diff.is_empty(), "identical re-poll must yield empty diff");
        assert!(!diff.needs_relayout());
    }

    // ── mixed: enter + update + exit + unchanged simultaneously ───────────

    #[test]
    fn mixed_reconcile() {
        let mut j: Join<String, u32> = Join::new();
        // Initial state: a=1, b=2, c=3
        j.reconcile(snap(&[("a", 1), ("b", 2), ("c", 3)]));
        // New snapshot: a exits, b updated, c unchanged, d enters
        let diff = j.reconcile(snap(&[("b", 99), ("c", 3), ("d", 4)]));

        // enter: d
        assert_eq!(diff.enter.len(), 1);
        assert_eq!(diff.enter[0].key, "d");
        assert_eq!(diff.enter[0].value, 4);

        // update: b (was 2, now 99)
        assert_eq!(diff.update.len(), 1);
        assert_eq!(diff.update[0].key, "b");
        assert_eq!(diff.update[0].old, 2);
        assert_eq!(diff.update[0].new, 99);

        // exit: a
        assert_eq!(diff.exit.len(), 1);
        assert_eq!(diff.exit[0].key, "a");
        assert_eq!(diff.exit[0].last, 1);

        // c unchanged — NOT in update
        assert!(!diff.update.iter().any(|u| u.key == "c"));

        // needs_relayout because there are enters and exits
        assert!(diff.needs_relayout());

        // post-reconcile state
        assert_eq!(j.len(), 3);
        assert!(j.contains(&"b".to_string()));
        assert!(j.contains(&"c".to_string()));
        assert!(j.contains(&"d".to_string()));
        assert!(!j.contains(&"a".to_string()));
    }

    // ── needs_relayout: true for enter, true for exit, false for update-only

    #[test]
    fn needs_relayout_true_for_enter() {
        let mut j: Join<String, u32> = Join::new();
        let diff = j.reconcile(snap(&[("a", 1)]));
        assert!(diff.needs_relayout());
    }

    #[test]
    fn needs_relayout_true_for_exit() {
        let mut j: Join<String, u32> = Join::new();
        j.reconcile(snap(&[("a", 1)]));
        let diff = j.reconcile(snap(&[]));
        assert!(diff.needs_relayout());
    }

    #[test]
    fn needs_relayout_false_for_update_only() {
        let mut j: Join<String, u32> = Join::new();
        j.reconcile(snap(&[("a", 1)]));
        let diff = j.reconcile(snap(&[("a", 2)]));
        assert!(!diff.needs_relayout(), "update-only must not relayout");
    }

    #[test]
    fn needs_relayout_false_for_empty_diff() {
        let mut j: Join<String, u32> = Join::new();
        j.reconcile(snap(&[("a", 1)]));
        let diff = j.reconcile(snap(&[("a", 1)]));
        assert!(!diff.needs_relayout());
        assert!(diff.is_empty());
    }

    // ── touch: Changed / Unchanged / Absent ───────────────────────────────

    #[test]
    fn touch_changed_updates_value_and_returns_old() {
        let mut j: Join<String, u32> = Join::new();
        j.reconcile(snap(&[("a", 10)]));
        let result = j.touch(&"a".to_string(), 20);
        assert_eq!(result, Touched::Changed { old: 10 });
        assert_eq!(j.get(&"a".to_string()), Some(&20));
    }

    #[test]
    fn touch_unchanged_returns_unchanged() {
        let mut j: Join<String, u32> = Join::new();
        j.reconcile(snap(&[("a", 10)]));
        let result = j.touch(&"a".to_string(), 10);
        assert_eq!(result, Touched::Unchanged);
        assert_eq!(j.get(&"a".to_string()), Some(&10));
    }

    #[test]
    fn touch_absent_returns_absent_and_does_not_insert() {
        let mut j: Join<String, u32> = Join::new();
        let before_keys: Vec<String> = j.keys().cloned().collect();
        let result = j.touch(&"missing".to_string(), 42);
        assert_eq!(result, Touched::Absent);
        let after_keys: Vec<String> = j.keys().cloned().collect();
        assert_eq!(before_keys, after_keys, "Absent must not insert the key");
        assert!(!j.contains(&"missing".to_string()));
    }

    #[test]
    fn touch_does_not_change_keys() {
        let mut j: Join<String, u32> = Join::new();
        j.reconcile(snap(&[("a", 1), ("b", 2)]));
        let before = keys_sorted(&j);
        // touch existing key (changed)
        j.touch(&"a".to_string(), 99);
        // touch absent key (absent)
        j.touch(&"z".to_string(), 0);
        let after = keys_sorted(&j);
        assert_eq!(before, after, "keys() must be unchanged after any touch");
    }

    // ── exit carries last value ────────────────────────────────────────────

    #[test]
    fn exit_carries_last_value() {
        let mut j: Join<String, u32> = Join::new();
        j.reconcile(snap(&[("a", 100)]));
        // update via touch before exit
        j.touch(&"a".to_string(), 200);
        let diff = j.reconcile(snap(&[]));
        assert_eq!(diff.exit[0].last, 200, "exit should carry the most-recent value (200)");
    }

    // ── enter carries the value ────────────────────────────────────────────

    #[test]
    fn enter_carries_value() {
        let mut j: Join<String, u32> = Join::new();
        let diff = j.reconcile(snap(&[("k", 77)]));
        assert_eq!(diff.enter[0].value, 77);
    }

    // ── update carries old and new ─────────────────────────────────────────

    #[test]
    fn update_carries_old_and_new() {
        let mut j: Join<String, u32> = Join::new();
        j.reconcile(snap(&[("k", 1)]));
        let diff = j.reconcile(snap(&[("k", 2)]));
        assert_eq!(diff.update[0].old, 1);
        assert_eq!(diff.update[0].new, 2);
    }

    // ── duplicate key in snapshot panics ──────────────────────────────────

    #[test]
    #[should_panic(expected = "Join::reconcile: duplicate key")]
    fn duplicate_key_panics() {
        let mut j: Join<String, u32> = Join::new();
        j.reconcile(vec![("a".to_string(), 1), ("a".to_string(), 2)]);
    }

    // ── deterministic diff order (BTreeMap-backed) ─────────────────────────

    #[test]
    fn diff_order_is_deterministic_btree() {
        let mut j: Join<String, u32> = Join::new();
        // Insert in non-sorted order; diff should emerge in BTree order
        let diff = j.reconcile(snap(&[("c", 3), ("a", 1), ("b", 2)]));
        let enter_keys: Vec<&str> = diff.enter.iter().map(|e| e.key.as_str()).collect();
        assert_eq!(enter_keys, vec!["a", "b", "c"], "enter order must be BTree-sorted");
    }

    // ── accessor correctness ───────────────────────────────────────────────

    #[test]
    fn get_returns_current_value() {
        let mut j: Join<String, u32> = Join::new();
        j.reconcile(snap(&[("x", 5)]));
        assert_eq!(j.get(&"x".to_string()), Some(&5));
        assert_eq!(j.get(&"y".to_string()), None);
    }

    #[test]
    fn len_and_is_empty() {
        let mut j: Join<String, u32> = Join::new();
        assert!(j.is_empty());
        assert_eq!(j.len(), 0);
        j.reconcile(snap(&[("a", 1), ("b", 2)]));
        assert!(!j.is_empty());
        assert_eq!(j.len(), 2);
    }

    #[test]
    fn contains_reflects_current_state() {
        let mut j: Join<String, u32> = Join::new();
        j.reconcile(snap(&[("a", 1)]));
        assert!(j.contains(&"a".to_string()));
        assert!(!j.contains(&"b".to_string()));
    }

    // ── correctness at scale ───────────────────────────────────────────────

    /// Reconcile 500 keys, then apply a second snapshot that enters some new keys,
    /// updates some, exits some, and leaves the rest unchanged.  Asserts exact diff
    /// counts and that post-state matches the second snapshot exactly.
    #[test]
    fn reconcile_at_scale_enter_update_exit_unchanged() {
        // First snapshot: keys 0..500, each with value == key as u32.
        let snap1: Vec<(u32, u32)> = (0u32..500).map(|k| (k, k)).collect();
        let mut j: Join<u32, u32> = Join::new();
        let diff1 = j.reconcile(snap1);
        assert_eq!(diff1.enter.len(), 500, "cold-start: 500 enters");
        assert!(diff1.update.is_empty());
        assert!(diff1.exit.is_empty());

        // Second snapshot:
        //   exit keys 0..100   (100 keys removed)
        //   update keys 100..200 with value+1000  (100 keys updated)
        //   unchanged keys 200..400 (same value)
        //   enter keys 500..600 (100 new keys)
        // Keys 400..500 are also unchanged (included in unchanged range above via 200..500 minus 200..400).
        // Re-using cleaner: unchanged = 200..500 (300 keys).
        let mut snap2: Vec<(u32, u32)> = Vec::new();
        // updated
        for k in 100u32..200 {
            snap2.push((k, k + 1000));
        }
        // unchanged (200..500)
        for k in 200u32..500 {
            snap2.push((k, k));
        }
        // entered
        for k in 500u32..600 {
            snap2.push((k, k));
        }

        let diff2 = j.reconcile(snap2.clone());

        assert_eq!(diff2.exit.len(), 100, "100 keys should exit (0..100)");
        assert_eq!(diff2.update.len(), 100, "100 keys should update (100..200)");
        assert_eq!(diff2.enter.len(), 100, "100 keys should enter (500..600)");
        assert!(diff2.needs_relayout(), "enter+exit means relayout");

        // Verify exited keys are 0..100
        for (i, ex) in diff2.exit.iter().enumerate() {
            assert_eq!(ex.key, i as u32, "exit key mismatch at index {i}");
            assert_eq!(ex.last, i as u32, "exit last-value mismatch at index {i}");
        }

        // Verify updated keys carry correct old/new
        for (i, up) in diff2.update.iter().enumerate() {
            let k = 100 + i as u32;
            assert_eq!(up.key, k);
            assert_eq!(up.old, k, "update.old should be the prior value for key {k}");
            assert_eq!(up.new, k + 1000, "update.new should be key+1000 for key {k}");
        }

        // Verify post-state == second snapshot exactly
        assert_eq!(j.len(), snap2.len(), "post-state size must equal second snapshot size");
        let expected: BTreeMap<u32, u32> = snap2.into_iter().collect();
        let actual: BTreeMap<u32, u32> = j.keys().map(|k| (*k, *j.get(k).unwrap())).collect();
        assert_eq!(actual, expected, "post-state must match second snapshot exactly");
    }

    // ── property tests ─────────────────────────────────────────────────────

    #[cfg(test)]
    mod props {
        use super::*;
        use proptest::prelude::*;

        // Generate a small keyed snapshot as Vec<(u8, u32)> with bounded key space.
        fn arb_snapshot() -> impl Strategy<Value = Vec<(u8, u32)>> {
            prop::collection::vec((0u8..20u8, 0u32..100u32), 0..10)
        }

        /// Deduplicate a snapshot by last-wins so we can compare sets.
        fn dedup_last_wins(pairs: &[(u8, u32)]) -> Vec<(u8, u32)> {
            let mut m: BTreeMap<u8, u32> = BTreeMap::new();
            for (k, v) in pairs {
                m.insert(*k, *v);
            }
            m.into_iter().collect()
        }

        /// Check whether `pairs` has duplicate keys.
        fn has_duplicates(pairs: &[(u8, u32)]) -> bool {
            let unique: std::collections::HashSet<u8> = pairs.iter().map(|(k, _)| *k).collect();
            unique.len() != pairs.len()
        }

        // (a) After reconcile(snap), keys() equals the key set in snap.
        proptest! {
            #[test]
            fn prop_keys_after_reconcile_match_snapshot(
                raw in arb_snapshot(),
            ) {
                // Use only duplicate-free snapshots for this property.
                prop_assume!(!has_duplicates(&raw));

                let mut j: Join<u8, u32> = Join::new();
                j.reconcile(raw.clone());
                let expected: Vec<u8> = {
                    let mut keys: Vec<u8> = raw.iter().map(|(k, _)| *k).collect();
                    keys.sort();
                    keys.dedup();
                    keys
                };
                let actual: Vec<u8> = j.keys().cloned().collect();
                prop_assert_eq!(actual, expected, "keys after reconcile must match snapshot");
            }
        }

        // (b) Reconcile is idempotent: a second identical reconcile yields empty diff.
        proptest! {
            #[test]
            fn prop_reconcile_is_idempotent(
                raw in arb_snapshot(),
            ) {
                prop_assume!(!has_duplicates(&raw));

                let mut j: Join<u8, u32> = Join::new();
                j.reconcile(raw.clone());
                let diff2 = j.reconcile(raw.clone());
                prop_assert!(diff2.is_empty(), "second identical reconcile must be empty");
            }
        }

        // (c) Applying a JoinDiff to the pre-reconcile state reproduces the post-reconcile state.
        //
        // We simulate "apply diff" manually: start with the old state, apply enter/update/exit,
        // and compare to what the join holds after reconcile.
        proptest! {
            #[test]
            fn prop_diff_applied_to_old_state_yields_new_state(
                raw_before in arb_snapshot(),
                raw_after in arb_snapshot(),
            ) {
                prop_assume!(!has_duplicates(&raw_before));
                prop_assume!(!has_duplicates(&raw_after));

                let mut j: Join<u8, u32> = Join::new();
                j.reconcile(raw_before.clone());

                // Capture the join's actual state before the second reconcile.
                let before_state = j.state.clone();

                let diff = j.reconcile(raw_after.clone());

                // Apply the diff to `before_state` to derive the expected `after_state`.
                let mut derived = before_state;
                for e in &diff.enter {
                    derived.insert(e.key, e.value);
                }
                for u in &diff.update {
                    derived.insert(u.key, u.new);
                }
                for x in &diff.exit {
                    derived.remove(&x.key);
                }

                // Compare derived state to what the join actually holds.
                let actual: BTreeMap<u8, u32> =
                    j.keys().map(|k| (*k, *j.get(k).unwrap())).collect();

                prop_assert_eq!(actual, derived,
                    "diff applied to old state must reproduce the new state");
            }
        }

        // (d-touch) touch return value: Absent / Unchanged / Changed{old} contract.
        proptest! {
            #[test]
            fn prop_touch_return_value(
                raw in arb_snapshot(),
                touch_key in 0u8..20u8,
                touch_val in 0u32..100u32,
            ) {
                prop_assume!(!has_duplicates(&raw));

                let mut j: Join<u8, u32> = Join::new();
                j.reconcile(raw.clone());

                let prior = j.get(&touch_key).cloned();

                match prior {
                    None => {
                        // Key is absent: touch must return Absent and not insert.
                        let result = j.touch(&touch_key, touch_val);
                        prop_assert_eq!(result, Touched::Absent);
                        prop_assert!(j.get(&touch_key).is_none(),
                            "Absent touch must not insert the key");
                    }
                    Some(old_val) if old_val == touch_val => {
                        // Key present and value equal: must return Unchanged.
                        let result = j.touch(&touch_key, touch_val);
                        prop_assert_eq!(result, Touched::Unchanged);
                        prop_assert_eq!(j.get(&touch_key), Some(&touch_val),
                            "Unchanged touch must not change the stored value");
                    }
                    Some(old_val) => {
                        // Key present and value different: must return Changed{old} and store new.
                        let result = j.touch(&touch_key, touch_val);
                        prop_assert_eq!(result, Touched::Changed { old: old_val },
                            "Changed touch must carry the prior value");
                        prop_assert_eq!(j.get(&touch_key), Some(&touch_val),
                            "Changed touch must store the new value");
                    }
                }
            }
        }

        // (d) touch never changes the key set.
        proptest! {
            #[test]
            fn prop_touch_never_changes_key_set(
                raw in arb_snapshot(),
                touch_key in 0u8..20u8,
                touch_val in 0u32..100u32,
            ) {
                prop_assume!(!has_duplicates(&raw));

                let mut j: Join<u8, u32> = Join::new();
                j.reconcile(raw.clone());

                let before: Vec<u8> = j.keys().cloned().collect();
                j.touch(&touch_key, touch_val);
                let after: Vec<u8> = j.keys().cloned().collect();

                prop_assert_eq!(before, after, "touch must not change key set");
            }
        }

        // (e) needs_relayout is false for purely data-tick (update-only) diffs.
        proptest! {
            #[test]
            fn prop_update_only_diff_no_relayout(
                raw in arb_snapshot(),
                delta in prop::collection::vec((0u8..20u8, 0u32..100u32), 0..5),
            ) {
                prop_assume!(!has_duplicates(&raw));
                prop_assume!(!has_duplicates(&delta));

                let mut j: Join<u8, u32> = Join::new();
                j.reconcile(raw.clone());

                // Build a snapshot with same keys but potentially different values.
                let existing_keys: Vec<u8> = j.keys().cloned().collect();
                if existing_keys.is_empty() {
                    return Ok(());
                }

                // Same keys, values from delta (mod range to keep u32)
                let same_keys_new_vals: Vec<(u8, u32)> = existing_keys
                    .iter()
                    .enumerate()
                    .map(|(i, k)| {
                        let v = delta.get(i % delta.len().max(1))
                            .map(|(_, v)| *v)
                            .unwrap_or(0);
                        (*k, v)
                    })
                    .collect();

                // Only proceed if the values are not identical to current (to get an
                // update-only diff; otherwise we'd just confirm empty-diff is fine too).
                let diff = j.reconcile(same_keys_new_vals);
                prop_assert!(!diff.needs_relayout(),
                    "diff over same key set must not require relayout");
            }
        }

        // (f) dedup_last_wins sanity — the helper is used elsewhere.
        proptest! {
            #[test]
            fn prop_dedup_last_wins_no_duplicates(raw in arb_snapshot()) {
                let deduped = dedup_last_wins(&raw);
                let unique: std::collections::HashSet<u8> = deduped.iter().map(|(k, _)| *k).collect();
                prop_assert_eq!(unique.len(), deduped.len(), "dedup_last_wins must produce unique keys");
            }
        }
    }
}
