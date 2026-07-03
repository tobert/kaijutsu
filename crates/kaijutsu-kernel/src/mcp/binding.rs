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
/// Collapsed surfaces: `shell` is the single context-bound shell MCP tool (one
/// `shell_execute` RPC); `edit_input` covers both `write_input` and
/// `edit_input` (write is edit-with-full-delete). `read_input`
/// (`get_input_state`) is intentionally **not** gated — reading compose text is
/// benign and gating it traps the `write_input` handler, which reads before it
/// writes.
///
/// The `shell` facade additionally **projects** the in-kernel `builtin.shell`
/// broker tool (see [`FACADE_PROJECTED_INSTANCES`]) so the native LLM agent —
/// whose tool roster is built from broker tools, not facades — gets a shell.
/// The facade bit gates all three reach paths (human box, external MCP,
/// in-kernel tool) so shell policy stays single-axis.
pub const KNOWN_FACADES: &[&str] = &["shell", "shell_readonly", "edit_input", "submit_input"];

/// The `kj` *authority* capabilities — bare-word grants that gate the
/// escalation-relevant `kj` verbs which never reach the broker `call_tool` path
/// (so they have no `Instance`/`Tool` to hang off). Like `admin`/`rc-write`,
/// these are **deliberately not implied by `*`**: a broad loadout (`coder` with
/// "*") must not silently be able to self-drive, fork, merge drift, drive the
/// transport, or perform context/workspace/preset/doc lifecycle. The narrow
/// roles (`toolie`, `musician`) grant exactly the ones they need.
///
/// Stored as a normalized set (`ContextToolBinding::authorities`) persisted in
/// the `context_binding_authorities` table — extensible (a future authority is
/// a new variant + token, no schema migration).
pub const KNOWN_AUTHORITIES: &[&str] =
    &["drive", "fork", "drift", "transport", "operator", "config-write", "exec"];

/// Builtin broker instances that are the in-kernel **projection** of a facade,
/// as `(instance, facade)` pairs.
///
/// A facade like `shell` is reachable two ways that must share ONE capability:
/// the RPC seam (the human shell box + the external MCP `shell`), gated
/// by `Broker::check_facade`; and a builtin broker tool the in-kernel LLM calls
/// (`builtin.shell`), gated by [`ContextToolBinding::allows`]. The agent's tool
/// roster is built from broker tools, which never included facades — so a
/// native agent "had no shell" regardless of `facade:shell`. The projection
/// closes that gap WITHOUT forking the policy: gating the tool as an ordinary
/// `Instance`/`Tool` grant would mean a context with `facade:shell` but no `*`
/// (e.g. `director`, `musician`) silently loses the model's shell. So the
/// binding treats a projected instance as allowed exactly when its backing
/// facade is allowed. One bit — `facade:shell` — governs both surfaces.
/// `shell_readonly` is the read-only twin: it projects `builtin.shell_readonly`
/// (the `read_only_shell` tool) for roles that must not write or shell out (the
/// `toolie`). A read-only role grants `facade:shell_readonly` and never
/// `facade:shell`, so it sees one shell, not both. Broad `facade:*` roles match
/// both projections and see both tools — a harmless strict subset, accepted to
/// keep the gate single-axis.
pub const FACADE_PROJECTED_INSTANCES: &[(&str, &str)] = &[
    ("builtin.shell", "shell"),
    ("builtin.shell_readonly", "shell_readonly"),
];

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
    /// May write rc lifecycle scripts under `/etc/rc` via the `file:write`/
    /// `edit` tools. Deliberately separate from `AllInstances`/`AllFacades`:
    /// a broad role (e.g. `coder` with "*") must NOT be able to clobber a
    /// privileged lifecycle script by accident — that's an ergonomic nudge,
    /// not a hard wall (host `vim` and `kj rc` always work).
    RcWrite,
    /// `kj drive` — clock an autonomous turn. The musician OODA tick runs under
    /// its context's loadout, so this is the cap that gates self-driving.
    Drive,
    /// `kj fork` — snapshot a context into a child.
    Fork,
    /// `kj drift push/pull/merge/flush/cancel` and `kj stage commit` — the
    /// cross-context write surface (merge into a parent, push edits to a peer).
    Drift,
    /// `kj transport play/pause/stop/tempo/ooda` — drive a context's beat.
    Transport,
    /// Context/workspace/preset/doc/cas lifecycle (create/set/archive/remove…)
    /// and `kj attach`. Operator authority over the durable structure, kept
    /// distinct from `Admin` (which is narrowly loadout-write).
    Operator,
    /// `kj config set/reset` — may write the CRDT-owned config files at
    /// `/etc/config` (models.toml, system.md, theme.toml, mcp.toml). The config
    /// analogue of [`RcWrite`]: dedicated so a broad loadout (e.g. `coder` with
    /// "*") can't silently rewrite which model runs or the base system prompt.
    /// `kj config` writes go straight through the VFS (not the gated file tool),
    /// so this is enforced in the `kj config` dispatcher.
    ///
    /// [`RcWrite`]: Capability::RcWrite
    ConfigWrite,
    /// May spawn host subprocesses from the context shell (kaish external
    /// commands). Enforced at kaish materialization, not per-call: a context
    /// without this authority gets a shell with external execution disabled
    /// (`command not found`), builtins and `kj` unaffected. Like every
    /// authority, deliberately not implied by `*` — a subprocess bypasses the
    /// VFS entirely (real syscalls on the real host), so the roles that don't
    /// need it (musician, toolie) never carry the footgun.
    Exec,
}

impl Capability {
    /// The canonical bare-word token for an *authority* capability (the grants
    /// kept in [`ContextToolBinding::authorities`]), or `None` for the
    /// structural `Instance`/`Tool`/`Facade`/broad-flag variants. Centralizes
    /// the variant⟷token mapping so `allows`/`grant`/`revoke_cap` and the
    /// persistence layer never drift.
    pub fn authority_name(&self) -> Option<&'static str> {
        Some(match self {
            Capability::Drive => "drive",
            Capability::Fork => "fork",
            Capability::Drift => "drift",
            Capability::Transport => "transport",
            Capability::Operator => "operator",
            Capability::ConfigWrite => "config-write",
            Capability::Exec => "exec",
            _ => return None,
        })
    }

    /// Parse a bare-word authority token into its capability, or `None` if the
    /// token is not a known authority. Inverse of [`Self::authority_name`].
    pub fn from_authority_name(token: &str) -> Option<Capability> {
        Some(match token {
            "drive" => Capability::Drive,
            "fork" => Capability::Fork,
            "drift" => Capability::Drift,
            "transport" => Capability::Transport,
            "operator" => Capability::Operator,
            "config-write" => Capability::ConfigWrite,
            "exec" => Capability::Exec,
            _ => return None,
        })
    }
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
    /// rc-write grant ("rc-write"): may write `/etc/rc` lifecycle scripts via
    /// the file tools. Not implied by `all_instances`/`all_facades`.
    #[serde(default)]
    pub binding_rc_write: bool,
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
    /// Authority grants — the bare-word `kj` verb caps (`drive`, `fork`,
    /// `drift`, `transport`, `operator`). A normalized set; **not** implied by
    /// `all_instances`. See [`KNOWN_AUTHORITIES`].
    #[serde(default)]
    pub authorities: std::collections::BTreeSet<String>,
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
            && !self.binding_rc_write
            && self.allowed_instances.is_empty()
            && self.allowed_tools.is_empty()
            && self.allowed_facades.is_empty()
            && self.authorities.is_empty()
    }

    /// True if this context holds the named `kj` authority (`drive`, `fork`,
    /// `drift`, `transport`, `operator`). Sugar over [`Self::allows`].
    pub fn has_authority(&self, name: &str) -> bool {
        self.authorities.contains(name)
    }

    /// True if this context may administer bindings (its own and others').
    pub fn is_admin(&self) -> bool {
        self.binding_admin
    }

    /// True if this context may write rc lifecycle scripts under `/etc/rc`
    /// via the file tools.
    pub fn is_rc_write(&self) -> bool {
        self.binding_rc_write
    }

    /// True if this binding grants `facade` — directly or via `facade:*`.
    fn facade_granted(&self, facade: &str) -> bool {
        self.all_facades || self.allowed_facades.iter().any(|f| f == facade)
    }

    /// True if `instance` is a facade-projected builtin (see
    /// [`FACADE_PROJECTED_INSTANCES`]) whose backing facade this binding grants.
    /// This is what keeps a facade-backed broker tool (`builtin.shell`) and the
    /// RPC facade gate single-axis: `facade:shell` lights up both.
    fn facade_projection_allows(&self, instance: &InstanceId) -> bool {
        FACADE_PROJECTED_INSTANCES
            .iter()
            .any(|(inst, facade)| *inst == instance.as_str() && self.facade_granted(facade))
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
                    || self.facade_projection_allows(instance)
                    || self
                        .allowed_tools
                        .iter()
                        .any(|(i, t)| i == instance && t == tool)
            }
            Capability::Facade(name) => self.facade_granted(name),
            Capability::AllInstances => self.all_instances,
            Capability::AllFacades => self.all_facades,
            Capability::Admin => self.binding_admin,
            Capability::RcWrite => self.binding_rc_write,
            // Authority caps are explicit and **not** implied by `*`: a broad
            // loadout never silently self-drives/forks/merges. Checked against
            // the normalized set, never a flag.
            Capability::Drive
            | Capability::Fork
            | Capability::Drift
            | Capability::Transport
            | Capability::Operator
            | Capability::ConfigWrite
            | Capability::Exec => {
                cap.authority_name().is_some_and(|n| self.authorities.contains(n))
            }
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
            Capability::RcWrite => self.binding_rc_write = true,
            c @ (Capability::Drive
            | Capability::Fork
            | Capability::Drift
            | Capability::Transport
            | Capability::Operator
            | Capability::ConfigWrite
            | Capability::Exec) => {
                if let Some(n) = c.authority_name() {
                    self.authorities.insert(n.to_string());
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
            Capability::AllInstances => self.all_instances = false,
            Capability::AllFacades => self.all_facades = false,
            Capability::Admin => self.binding_admin = false,
            Capability::RcWrite => self.binding_rc_write = false,
            Capability::Drive
            | Capability::Fork
            | Capability::Drift
            | Capability::Transport
            | Capability::Operator
            | Capability::ConfigWrite
            | Capability::Exec => {
                if let Some(n) = cap.authority_name() {
                    self.authorities.remove(n);
                }
            }
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
        // Facade-projected builtins are candidates when their backing facade is
        // granted, so `list_visible_tools` selects the server for facade-only
        // loadouts (director/musician hold `facade:shell` but no instance grant
        // and no `*`). Without this they'd never reach the per-tool filter.
        for (inst, facade) in FACADE_PROJECTED_INSTANCES {
            let id = InstanceId::new(*inst);
            if self.facade_granted(facade) && !out.contains(&id) {
                out.push(id);
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
        // Nor rc-write: a broad loadout (coder) must not be able to clobber
        // a privileged /etc/rc script by accident.
        assert!(
            !b.allows(&Capability::RcWrite),
            "all_instances must NOT imply rc-write"
        );
    }

    #[test]
    fn facade_shell_projects_the_builtin_shell_tool() {
        // The whole point: granting `facade:shell` (and nothing else — no `*`,
        // no instance grant) must light up the `builtin.shell` broker tool, so
        // facade-only loadouts (director/musician) get the model's shell. This
        // is what keeps the RPC seam and the model tool single-axis.
        let mut b = ContextToolBinding::new();
        b.grant(Capability::Facade("shell".into()));
        assert!(
            b.allows_tool(&inst("builtin.shell"), "shell"),
            "facade:shell must project the builtin.shell tool"
        );
        assert!(
            b.candidate_instances().contains(&inst("builtin.shell")),
            "facade:shell must make builtin.shell a list_visible_tools candidate"
        );
    }

    #[test]
    fn facade_wildcard_projects_the_builtin_shell_tool() {
        // default/coder/mcp grant `facade:*` — that must cover the projection too.
        let mut b = ContextToolBinding::new();
        b.grant(Capability::AllFacades);
        assert!(b.allows_tool(&inst("builtin.shell"), "shell"));
        assert!(b.candidate_instances().contains(&inst("builtin.shell")));
    }

    #[test]
    fn no_facade_denies_the_builtin_shell_tool() {
        // toolie: read-only, no facade. The shell tool must stay hidden and
        // uncallable — the projection is the ONLY path to it for a non-`*` role.
        let mut b = ContextToolBinding::new();
        b.grant(Capability::Tool {
            instance: inst("builtin.file"),
            tool: "read".into(),
        });
        assert!(
            !b.allows_tool(&inst("builtin.shell"), "shell"),
            "without facade:shell, the builtin.shell tool must be denied"
        );
        assert!(!b.candidate_instances().contains(&inst("builtin.shell")));
    }

    #[test]
    fn unrelated_facade_does_not_project_the_shell_tool() {
        // facade:edit_input is a different surface; it must NOT grant shell.
        let mut b = ContextToolBinding::new();
        b.grant(Capability::Facade("edit_input".into()));
        assert!(!b.allows_tool(&inst("builtin.shell"), "shell"));
        assert!(!b.candidate_instances().contains(&inst("builtin.shell")));
    }

    #[test]
    fn rc_write_is_a_dedicated_grant() {
        // Even a maximally-broad loadout doesn't imply rc-write...
        let mut b = ContextToolBinding::new();
        b.grant(Capability::AllInstances);
        b.grant(Capability::AllFacades);
        b.grant(Capability::Admin);
        assert!(!b.allows(&Capability::RcWrite), "broad loadout ≠ rc-write");
        assert!(!b.is_rc_write());
        // ...but an explicit grant does, and revoke is surgical.
        b.grant(Capability::RcWrite);
        assert!(b.allows(&Capability::RcWrite));
        assert!(b.is_rc_write());
        b.revoke_cap(&Capability::RcWrite);
        assert!(!b.is_rc_write());
        // Revoking rc-write left the other broad grants intact.
        assert!(b.allows(&Capability::AllInstances) && b.is_admin());
    }

    #[test]
    fn authority_caps_are_explicit_and_not_implied_by_star() {
        // The whole point: a broad loadout ("*" + "facade:*" + admin + rc-write)
        // must NOT silently grant any kj authority verb.
        let mut b = ContextToolBinding::new();
        b.grant(Capability::AllInstances);
        b.grant(Capability::AllFacades);
        b.grant(Capability::Admin);
        b.grant(Capability::RcWrite);
        for cap in [
            Capability::Drive,
            Capability::Fork,
            Capability::Drift,
            Capability::Transport,
            Capability::Operator,
            Capability::ConfigWrite,
            Capability::Exec,
        ] {
            assert!(
                !b.allows(&cap),
                "broad loadout must NOT imply authority {:?}",
                cap.authority_name()
            );
        }
    }

    #[test]
    fn authority_grant_revoke_round_trips_each_variant() {
        for cap in [
            Capability::Drive,
            Capability::Fork,
            Capability::Drift,
            Capability::Transport,
            Capability::Operator,
            Capability::ConfigWrite,
            Capability::Exec,
        ] {
            let mut b = ContextToolBinding::new();
            assert!(!b.allows(&cap), "{cap:?} granted on a fresh binding");
            b.grant(cap.clone());
            assert!(b.allows(&cap), "{cap:?} not allowed after grant");
            let name = cap.authority_name().expect("authority has a token");
            assert!(b.has_authority(name), "{name} missing from set after grant");
            // Granting one authority must not leak to the siblings.
            for other in KNOWN_AUTHORITIES {
                if *other != name {
                    assert!(!b.has_authority(other), "{name} grant leaked to {other}");
                }
            }
            b.revoke_cap(&cap);
            assert!(!b.allows(&cap), "{cap:?} survived revoke");
            assert!(b.is_empty(), "binding not empty after revoking sole authority");
        }
    }

    #[test]
    fn authority_name_round_trips_known_tokens() {
        for token in KNOWN_AUTHORITIES {
            let cap = Capability::from_authority_name(token)
                .unwrap_or_else(|| panic!("{token} did not parse as an authority"));
            assert_eq!(cap.authority_name(), Some(*token));
        }
        assert!(Capability::from_authority_name("nope").is_none());
        assert!(Capability::AllInstances.authority_name().is_none());
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

        // `builtin.shell` joins the candidates because `facade:shell` was
        // granted above — the facade projects the in-kernel shell tool so the
        // server is selected by list_visible_tools (FACADE_PROJECTED_INSTANCES).
        let mut cands = b.candidate_instances();
        cands.sort();
        assert_eq!(cands, vec![inst("block"), inst("builtin.shell"), inst("file")]);
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
