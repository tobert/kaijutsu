//! `ContextToolBinding` — per-context capability allow-set and sticky naming
//! (§4.2, D-20).
//!
//! A binding is the **positive** allow-set a context may use. It is
//! **deny-by-default**: an empty binding grants nothing. Permissiveness is
//! expressed explicitly — `all_instances` ("*"), `all_facades` ("facade:*") —
//! so there is no "empty means allow everything" sentinel to forget a guard
//! around. The binding also preserves the tool names the LLM has already seen
//! across mutations (sticky `Auto` resolution, D-20).
//!
//! One predicate, [`ContextToolBinding::allows`], answers every enforcement
//! question (broker `call_tool`, `list_visible_tools`, and the facade gate at
//! the shared RPC layer). Facades are consulted through the *same* predicate —
//! there is no separate `allows_facade`.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use super::types::InstanceId;

/// Resolved tool name → (instance, original tool name).
pub type ResolvedName = (InstanceId, String);

/// The facade tool surfaces a context can be granted. Facades are the
/// non-broker-routed tools an agent reaches over RPC — they don't pass through
/// `broker.call_tool`, so they're enforced at the shared kernel RPC layer
/// (`shell_execute`/`edit_input`/`submit_input` handlers), which both the human
/// app and external agents cross. The same [`ContextToolBinding::allows`]
/// predicate is consulted there.
///
/// Collapsed surfaces: `shell` covers both the `shell` and `context_shell` MCP
/// tools (one `shell_execute` RPC); `edit_input` covers both `write_input` and
/// `edit_input` (write is edit-with-full-delete). `read_input`
/// (`get_input_state`) is intentionally **not** gated — reading compose text is
/// benign and gating it traps the `write_input` handler, which reads before it
/// writes.
pub const KNOWN_FACADES: &[&str] = &["shell", "edit_input", "submit_input"];

/// A single capability grant or query. The allow-set is the positive surface a
/// context may use. `Instance`/`Tool`/`Facade` are the granular grants;
/// `AllInstances`/`AllFacades`/`Admin` are the explicit broad grants that set
/// the binding's flags (so default-permissive is opt-in, never implicit).
///
/// As a *query* to [`ContextToolBinding::allows`], only `Instance`/`Tool`/
/// `Facade` are meaningful at the enforcement points; the broad variants answer
/// against their backing flag for totality.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub enum Capability {
    /// Every tool on an instance.
    Instance(InstanceId),
    /// A single tool on an instance.
    Tool { instance: InstanceId, tool: String },
    /// A facade tool not routed through the broker.
    Facade(String),
    /// Every broker instance ("*"). Does **not** imply `Admin`.
    AllInstances,
    /// Every facade surface ("facade:*").
    AllFacades,
    /// May write *any* context's loadout (the director/operator capability).
    /// Deliberately separate from `AllInstances`: a broad role must not become
    /// an admin just by holding "*".
    Admin,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct ContextToolBinding {
    /// Explicit "every instance" grant ("*"). Future-proof: covers instances
    /// registered after the binding was set.
    #[serde(default)]
    pub all_instances: bool,
    /// Explicit "every facade" grant ("facade:*").
    #[serde(default)]
    pub all_facades: bool,
    /// Binding-admin grant ("admin"): may write any context's loadout. Not
    /// implied by `all_instances`.
    #[serde(default)]
    pub binding_admin: bool,
    /// Instance-wide grants; order is a tiebreaker for name resolution (§4.2).
    pub allowed_instances: Vec<InstanceId>,
    /// Tool-granular grants — `(instance, tool)` pairs allowed even when the
    /// whole instance is not. Lets a role allow `builtin.file:read` without
    /// `builtin.file:write` (read/write are mixed inside an instance).
    #[serde(default)]
    pub allowed_tools: Vec<ResolvedName>,
    /// Facade grants (`shell`, `edit_input`, `submit_input`).
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
            ..Self::default()
        }
    }

    /// True when the binding carries no grants of any kind — the deny-all
    /// state (this is also a fresh binding). No longer a "permit everything"
    /// sentinel: an empty binding now denies every tool and facade.
    pub fn is_empty(&self) -> bool {
        !self.all_instances
            && !self.all_facades
            && !self.binding_admin
            && self.allowed_instances.is_empty()
            && self.allowed_tools.is_empty()
            && self.allowed_facades.is_empty()
    }

    /// True if this context may administer bindings (its own and others').
    pub fn is_admin(&self) -> bool {
        self.binding_admin
    }

    /// The single capability predicate every enforcement point consults:
    /// broker `call_tool` (refuse), `list_visible_tools` (hide), and the
    /// facade gate at the RPC layer. Deny-by-default: anything not positively
    /// granted is denied.
    pub fn allows(&self, cap: &Capability) -> bool {
        match cap {
            Capability::Instance(instance) => {
                self.all_instances || self.allowed_instances.contains(instance)
            }
            Capability::Tool { instance, tool } => {
                self.all_instances
                    || self.allowed_instances.contains(instance)
                    || self
                        .allowed_tools
                        .iter()
                        .any(|(i, t)| i == instance && t == tool)
            }
            Capability::Facade(name) => {
                self.all_facades || self.allowed_facades.iter().any(|f| f == name)
            }
            Capability::AllInstances => self.all_instances,
            Capability::AllFacades => self.all_facades,
            Capability::Admin => self.binding_admin,
        }
    }

    /// Ergonomic wrapper for the `(instance, tool)` query — sugar over
    /// [`Self::allows`] with a `Capability::Tool`. The predicate is `allows`;
    /// this just spares hot call sites the struct literal.
    pub fn allows_tool(&self, instance: &InstanceId, tool: &str) -> bool {
        self.allows(&Capability::Tool {
            instance: instance.clone(),
            tool: tool.to_string(),
        })
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
            Capability::AllInstances => self.all_instances = true,
            Capability::AllFacades => self.all_facades = true,
            Capability::Admin => self.binding_admin = true,
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
            Capability::AllInstances => self.all_instances = false,
            Capability::AllFacades => self.all_facades = false,
            Capability::Admin => self.binding_admin = false,
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
        self.allows(&Capability::Instance(instance.clone()))
    }

    /// Every instance referenced by any grant (instance-wide or tool-granular)
    /// — the set of servers `list_visible_tools` must query. When
    /// `all_instances` is set, the caller must query every registered instance
    /// (not derivable from here); this returns the explicitly-named ones.
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
    pub fn apply_resolutions(&mut self, resolutions: Vec<(ResolvedName, String)>) {
        let mut already_resolved: std::collections::HashSet<ResolvedName> =
            self.name_map.values().cloned().collect();

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
    fn empty_binding_denies_everything() {
        // Deny-by-default: a fresh/empty binding grants nothing.
        let b = ContextToolBinding::new();
        assert!(b.is_empty());
        assert!(!b.allows(&Capability::Tool {
            instance: inst("builtin.file"),
            tool: "read".into()
        }));
        assert!(!b.allows(&Capability::Instance(inst("builtin.file"))));
        assert!(!b.allows(&Capability::Facade("shell".into())));
        assert!(!b.is_admin());
    }

    #[test]
    fn all_instances_grants_every_tool_but_not_admin_or_facades() {
        let mut b = ContextToolBinding::new();
        b.grant(Capability::AllInstances);
        assert!(!b.is_empty());
        assert!(b.allows_tool(&inst("anything"), "any_tool"));
        assert!(b.allows(&Capability::Instance(inst("late.registered"))));
        // The whole point of a separate admin axis: "*" is not admin.
        assert!(!b.is_admin(), "all_instances must NOT imply admin");
        // And "*" does not grant facades.
        assert!(!b.allows(&Capability::Facade("shell".into())));
    }

    #[test]
    fn all_facades_grants_every_facade() {
        let mut b = ContextToolBinding::new();
        b.grant(Capability::AllFacades);
        for f in KNOWN_FACADES {
            assert!(b.allows(&Capability::Facade((*f).into())));
        }
        assert!(!b.allows_tool(&inst("builtin.file"), "read"));
    }

    #[test]
    fn allows_matrix_instance_and_tool_grants() {
        let mut b = ContextToolBinding::new();

        b.grant(Capability::Instance(inst("file")));
        assert!(b.allows_tool(&inst("file"), "read"));
        assert!(b.allows_tool(&inst("file"), "write"));
        assert!(!b.allows_tool(&inst("block"), "read"), "other instance not granted");

        b.grant(Capability::Tool {
            instance: inst("block"),
            tool: "block_read".into(),
        });
        assert!(b.allows_tool(&inst("block"), "block_read"));
        assert!(
            !b.allows_tool(&inst("block"), "block_edit"),
            "tool grant must not leak to sibling tools"
        );

        // Facade goes through the SAME predicate, not a separate one.
        b.grant(Capability::Facade("shell".into()));
        assert!(b.allows(&Capability::Facade("shell".into())));
        assert!(!b.allows(&Capability::Facade("edit_input".into())));

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
        b.grant(tool.clone());
        assert_eq!(b.allowed_tools.len(), 1, "double-grant duplicated");

        b.grant(Capability::Tool {
            instance: inst("file"),
            tool: "write".into(),
        });
        b.revoke_cap(&tool);
        assert!(!b.allows_tool(&inst("file"), "read"));
        assert!(b.allows_tool(&inst("file"), "write"), "sibling tool grant survives");

        b.revoke(&inst("file"));
        assert!(b.is_empty(), "revoking instance cleared its tool grants");
    }

    #[test]
    fn grant_revoke_flags_roundtrip() {
        let mut b = ContextToolBinding::new();
        b.grant(Capability::Admin);
        assert!(b.is_admin());
        b.revoke_cap(&Capability::Admin);
        assert!(!b.is_admin());

        b.grant(Capability::AllInstances);
        b.grant(Capability::AllFacades);
        assert!(b.allows(&Capability::AllInstances));
        assert!(b.allows(&Capability::AllFacades));
        b.revoke_cap(&Capability::AllInstances);
        assert!(!b.allows_tool(&inst("x"), "y"));
    }

    #[test]
    fn apply_resolutions_is_sticky_across_calls() {
        let mut b = ContextToolBinding::new();
        let a = inst("a");
        let c = inst("b");

        b.apply_resolutions(vec![((a.clone(), "read".into()), "read".into())]);
        assert_eq!(b.resolve("read"), Some(&(a.clone(), "read".into())));

        b.apply_resolutions(vec![
            ((a.clone(), "read".into()), "read".into()),
            ((c.clone(), "read".into()), "b.read".into()),
        ]);
        assert_eq!(b.name_map.len(), 2);
        assert_eq!(b.resolve("read"), Some(&(a.clone(), "read".into())));
        assert_eq!(b.resolve("b.read"), Some(&(c, "read".into())));

        b.apply_resolutions(vec![((a.clone(), "read".into()), "renamed".into())]);
        assert_eq!(
            b.resolve("read"),
            Some(&(a, "read".into())),
            "sticky resolution was overwritten (D-20 violation)"
        );
        assert!(b.resolve("renamed").is_none());
    }

    #[test]
    fn revoke_drops_stale_name_map_entries() {
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
