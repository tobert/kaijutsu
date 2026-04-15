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
