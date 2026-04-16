//! `ContextToolBinding` — per-context tool visibility and sticky naming
//! (§4.2, D-20).
//!
//! A binding selects which instances are visible to a context and preserves
//! the names the LLM has already seen across binding mutations. Qualify mode
//! is `Auto` + sticky: unqualified when unique at first binding; collisions
//! get the `instance.tool` form; names in `name_map` persist even after the
//! backing instance is dropped (those tools report as removed on next call).

use std::collections::HashMap;

use super::types::InstanceId;

/// Resolved tool name → (instance, original tool name).
pub type ResolvedName = (InstanceId, String);

#[derive(Clone, Debug, Default)]
pub struct ContextToolBinding {
    /// Instance visibility; order is a tiebreaker for name resolution (§4.2).
    pub allowed_instances: Vec<InstanceId>,
    /// Sticky resolved names. Once resolved, the binding preserves the name
    /// until an operator explicitly requalifies.
    pub name_map: HashMap<String, ResolvedName>,
}

impl ContextToolBinding {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_instances(instances: Vec<InstanceId>) -> Self {
        Self {
            allowed_instances: instances,
            name_map: HashMap::new(),
        }
    }

    pub fn allow(&mut self, instance: InstanceId) {
        if !self.allowed_instances.contains(&instance) {
            self.allowed_instances.push(instance);
        }
    }

    pub fn revoke(&mut self, instance: &InstanceId) {
        self.allowed_instances.retain(|i| i != instance);
        self.name_map.retain(|_, (inst, _)| inst != instance);
    }

    pub fn is_allowed(&self, instance: &InstanceId) -> bool {
        self.allowed_instances.contains(instance)
    }

    pub fn resolve(&self, visible_name: &str) -> Option<&ResolvedName> {
        self.name_map.get(visible_name)
    }

    /// Merge a freshly-computed `(instance, tool) → visible_name` map into
    /// the sticky `name_map`. Existing entries win (D-20).
    ///
    /// `resolutions` is the set of visible names that would be assigned under
    /// Auto mode given the current `allowed_instances` and the tools each
    /// instance advertises. The broker is responsible for computing it; this
    /// method is the sticky-merge rule.
    pub fn apply_resolutions(&mut self, resolutions: Vec<(ResolvedName, String)>) {
        let mut already_resolved: std::collections::HashSet<ResolvedName> = self
            .name_map
            .values()
            .cloned()
            .collect();

        for ((instance, tool), visible_name) in resolutions {
            let pair = (instance, tool);
            if already_resolved.contains(&pair) {
                continue;
            }
            if self.name_map.contains_key(&visible_name) {
                continue;
            }
            self.name_map.insert(visible_name, pair.clone());
            already_resolved.insert(pair);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inst(s: &str) -> InstanceId {
        InstanceId::new(s)
    }

    #[test]
    fn allow_revoke_is_allowed_roundtrip() {
        let mut b = ContextToolBinding::new();
        let a = inst("a");
        let c = inst("b");

        b.allow(a.clone());
        b.allow(c.clone());
        b.allow(a.clone()); // duplicate — must be deduped
        assert_eq!(b.allowed_instances.len(), 2, "double-allow duplicated");
        assert!(b.is_allowed(&a));
        assert!(b.is_allowed(&c));

        b.revoke(&a);
        assert!(!b.is_allowed(&a));
        assert!(b.is_allowed(&c));
    }

    #[test]
    fn apply_resolutions_is_sticky_across_calls() {
        // D-20: names the LLM has seen must persist across binding mutations.
        let mut b = ContextToolBinding::new();
        let a = inst("a");
        let c = inst("b");

        b.apply_resolutions(vec![((a.clone(), "read".into()), "read".into())]);
        assert_eq!(b.resolve("read"), Some(&(a.clone(), "read".into())));

        // Second call adds a colliding (b,"read") under a qualified name;
        // the first sticky entry must be unchanged.
        b.apply_resolutions(vec![
            ((a.clone(), "read".into()), "read".into()),
            ((c.clone(), "read".into()), "b.read".into()),
        ]);
        assert_eq!(b.name_map.len(), 2);
        assert_eq!(b.resolve("read"), Some(&(a.clone(), "read".into())));
        assert_eq!(b.resolve("b.read"), Some(&(c, "read".into())));

        // Third call tries to rename the sticky — must be ignored.
        b.apply_resolutions(vec![((a.clone(), "read".into()), "renamed".into())]);
        assert_eq!(
            b.resolve("read"),
            Some(&(a, "read".into())),
            "sticky resolution was overwritten (D-20 violation)"
        );
        assert!(b.resolve("renamed").is_none());
    }

    #[test]
    fn resolve_returns_mapped_pair_or_none() {
        let mut b = ContextToolBinding::new();
        b.apply_resolutions(vec![((inst("a"), "read".into()), "read".into())]);

        assert_eq!(b.resolve("read"), Some(&(inst("a"), "read".into())));
        assert!(b.resolve("missing").is_none());
    }

    #[test]
    fn revoke_drops_stale_name_map_entries() {
        // revoke must evict name_map entries pointing at the dropped instance
        // so subsequent calls surface the removed-tool error cleanly.
        let mut b = ContextToolBinding::new();
        let a = inst("a");
        let c = inst("b");

        b.allow(a.clone());
        b.allow(c.clone());
        b.apply_resolutions(vec![
            ((a.clone(), "read".into()), "a.read".into()),
            ((c.clone(), "write".into()), "b.write".into()),
        ]);
        assert_eq!(b.name_map.len(), 2);

        b.revoke(&a);
        assert!(!b.is_allowed(&a));
        assert!(b.name_map.values().all(|(inst, _)| inst != &a));
        assert_eq!(b.resolve("b.write"), Some(&(c, "write".into())));
    }
}
