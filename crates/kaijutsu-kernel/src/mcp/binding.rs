//! `ContextToolBinding` — per-context tool visibility and sticky naming
//! (§4.2, D-20).
//!
//! A binding selects which instances are visible to a context and preserves
//! the names the LLM has already seen across binding mutations. Qualify mode
//! is `Auto` + sticky: unqualified when unique at first binding; collisions
//! get the `instance.tool` form; names in `name_map` persist even after the
//! backing instance is dropped (those tools report as removed on next call).

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use super::types::InstanceId;

/// Resolved tool name → (instance, original tool name).
pub type ResolvedName = (InstanceId, String);

/// The facade tool surfaces a context can be granted. These are the
/// non-broker-routed tools the external agent reaches over RPC — they don't
/// pass through `broker.call_tool`, so they're enforced at the MCP/RPC agent
/// boundary (`broker.check_facade`) rather than the broker call path.
///
/// `shell`/`context_shell` and `write_input`/`edit_input` are listed
/// separately on purpose: they are distinct *tool surfaces* even where the
/// underlying RPC coincides, so a role can grant one without the other.
/// First-touch seeding grants this whole set so default-permissive covers the
/// facade axis too (otherwise a context that became non-empty by touching one
/// broker tool would start refusing `context_shell`).
pub const KNOWN_FACADES: &[&str] = &[
    "shell",
    "context_shell",
    "read_input",
    "write_input",
    "edit_input",
    "submit_input",
];

/// A single capability grant in a context's allow-set. The allow-set is the
/// positive surface a context may use; default-permissive is expressed as
/// instance-wide grants (what first-touch seeding writes), while tool-granular
/// roles (explorer, director) enumerate `Tool`/`Facade` grants.
///
/// `Instance` is the coarse grant and stays fresh as an instance registers new
/// tools; `Tool` pins one tool on one instance; `Facade` names a non-broker
/// tool surface (`context_shell`, `shell`, `*_input`). Facade *enforcement* is
/// a kj/RPC-layer follow-up — the broker call path only routes `Instance`/`Tool`
/// — but the grant is representable here so the setter and persistence are
/// complete.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub enum Capability {
    /// Every tool on an instance.
    Instance(InstanceId),
    /// A single tool on an instance.
    Tool { instance: InstanceId, tool: String },
    /// A facade tool not routed through the broker.
    Facade(String),
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct ContextToolBinding {
    /// Instance-wide grants; order is a tiebreaker for name resolution (§4.2).
    pub allowed_instances: Vec<InstanceId>,
    /// Tool-granular grants — `(instance, tool)` pairs allowed even when the
    /// whole instance is not. Lets a role allow `builtin.file:read` without
    /// `builtin.file:write` (read/write are mixed inside an instance).
    #[serde(default)]
    pub allowed_tools: Vec<ResolvedName>,
    /// Facade grants (`context_shell`, `shell`, `*_input`). Stored and
    /// persisted; broker-call enforcement is a follow-up (see `Capability`).
    #[serde(default)]
    pub allowed_facades: Vec<String>,
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
            allowed_tools: Vec::new(),
            allowed_facades: Vec::new(),
            name_map: HashMap::new(),
        }
    }

    /// The default-permissive binding written by first-touch seeding: all
    /// registered instances plus every known facade, so a never-bound context
    /// can use the full surface on every axis. A role bundle narrows from
    /// empty instead (via `kj binding allow`) and so never lands here.
    pub fn permissive(instances: Vec<InstanceId>) -> Self {
        Self {
            allowed_instances: instances,
            allowed_tools: Vec::new(),
            allowed_facades: KNOWN_FACADES.iter().map(|s| s.to_string()).collect(),
            name_map: HashMap::new(),
        }
    }

    /// True when the binding carries no grants of any kind. This is the
    /// "never bound" sentinel: callers seed the default-permissive "bind all
    /// registered instances" only when this holds, so a tool-only role bundle
    /// (empty `allowed_instances`, populated `allowed_tools`) is *not*
    /// re-seeded into permissiveness.
    pub fn is_empty(&self) -> bool {
        self.allowed_instances.is_empty()
            && self.allowed_tools.is_empty()
            && self.allowed_facades.is_empty()
    }

    /// Add one capability grant (idempotent).
    pub fn grant(&mut self, cap: Capability) {
        match cap {
            Capability::Instance(instance) => self.allow(instance),
            Capability::Tool { instance, tool } => {
                let pair = (instance, tool);
                if !self.allowed_tools.contains(&pair) {
                    self.allowed_tools.push(pair);
                }
            }
            Capability::Facade(name) => {
                if !self.allowed_facades.contains(&name) {
                    self.allowed_facades.push(name);
                }
            }
        }
    }

    /// Remove one capability grant (idempotent if absent). Revoking an
    /// instance also drops tool grants and `name_map` entries for it.
    pub fn revoke_cap(&mut self, cap: &Capability) {
        match cap {
            Capability::Instance(instance) => self.revoke(instance),
            Capability::Tool { instance, tool } => {
                self.allowed_tools
                    .retain(|(i, t)| !(i == instance && t == tool));
                self.name_map
                    .retain(|_, (i, t)| !(i == instance && t == tool));
            }
            Capability::Facade(name) => self.allowed_facades.retain(|f| f != name),
        }
    }

    pub fn allow(&mut self, instance: InstanceId) {
        if !self.allowed_instances.contains(&instance) {
            self.allowed_instances.push(instance);
        }
    }

    pub fn revoke(&mut self, instance: &InstanceId) {
        self.allowed_instances.retain(|i| i != instance);
        self.allowed_tools.retain(|(i, _)| i != instance);
        self.name_map.retain(|_, (inst, _)| inst != instance);
    }

    pub fn is_allowed(&self, instance: &InstanceId) -> bool {
        self.allowed_instances.contains(instance)
    }

    /// The single capability predicate both pinch points consult:
    /// `list_visible_tools` (hide) and `call_tool` (refuse). A `(instance,
    /// tool)` is allowed if the whole instance is granted or the specific
    /// tool is granted.
    pub fn allows(&self, instance: &InstanceId, tool: &str) -> bool {
        self.allowed_instances.contains(instance)
            || self
                .allowed_tools
                .iter()
                .any(|(i, t)| i == instance && t == tool)
    }

    /// True if a facade tool surface is granted (or no facade narrowing is in
    /// effect). Facade enforcement is not yet wired into a call path.
    pub fn allows_facade(&self, name: &str) -> bool {
        self.allowed_facades.iter().any(|f| f == name)
    }

    /// Every instance referenced by any grant (instance-wide or tool-granular)
    /// — the set of servers `list_visible_tools` must query.
    pub fn candidate_instances(&self) -> Vec<InstanceId> {
        let mut out = self.allowed_instances.clone();
        for (inst, _) in &self.allowed_tools {
            if !out.contains(inst) {
                out.push(inst.clone());
            }
        }
        out
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
    fn allows_matrix_instance_and_tool_grants() {
        let mut b = ContextToolBinding::new();
        assert!(b.is_empty(), "fresh binding is the never-bound sentinel");

        // Instance grant allows every tool on that instance.
        b.grant(Capability::Instance(inst("file")));
        assert!(b.allows(&inst("file"), "read"));
        assert!(b.allows(&inst("file"), "write"));
        assert!(!b.allows(&inst("block"), "read"), "other instance not granted");
        assert!(!b.is_empty());

        // Tool grant allows only that one tool on the named instance.
        b.grant(Capability::Tool {
            instance: inst("block"),
            tool: "block_read".into(),
        });
        assert!(b.allows(&inst("block"), "block_read"));
        assert!(
            !b.allows(&inst("block"), "block_edit"),
            "tool grant must not leak to sibling tools"
        );

        // Facade grants are a separate axis; allows() (broker path) ignores them.
        b.grant(Capability::Facade("context_shell".into()));
        assert!(b.allows_facade("context_shell"));
        assert!(!b.allows_facade("shell"));

        // candidate_instances unions instance- and tool-granted instances.
        let mut cands = b.candidate_instances();
        cands.sort();
        assert_eq!(cands, vec![inst("block"), inst("file")]);
    }

    #[test]
    fn grant_is_idempotent_and_revoke_cap_is_surgical() {
        let mut b = ContextToolBinding::new();
        let tool = Capability::Tool {
            instance: inst("file"),
            tool: "read".into(),
        };
        b.grant(tool.clone());
        b.grant(tool.clone()); // duplicate
        assert_eq!(b.allowed_tools.len(), 1, "double-grant duplicated");

        b.grant(Capability::Tool {
            instance: inst("file"),
            tool: "write".into(),
        });
        b.revoke_cap(&tool);
        assert!(!b.allows(&inst("file"), "read"));
        assert!(b.allows(&inst("file"), "write"), "sibling tool grant survives");

        // Revoking the instance drops remaining tool grants for it too.
        b.revoke(&inst("file"));
        assert!(b.is_empty(), "revoking instance cleared its tool grants");
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
