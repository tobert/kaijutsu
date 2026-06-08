//! `kj binding` — read and write a context's tool-capability allow-set.
//!
//! This is the rc-native setter the capability-policy work hangs off: rc
//! `.kai` create scripts call it to give a `context_type` its loadout, and
//! external agents can call it through `context_shell`. It delegates to the
//! broker's binding API (`binding`/`set_binding`/`clear_binding`), which both
//! persists (`KernelDb`) and fires the per-tool add/remove notifications.
//!
//! ```text
//! kj binding show   [<ctx>]
//! kj binding allow  <cap> [<ctx>]
//! kj binding revoke <cap> [<ctx>]
//! kj binding reset  [<ctx>]          # clear → deny-all (deny-by-default)
//! ```
//!
//! A `<cap>` is one of:
//!   • `builtin.file`              — instance-wide grant (every tool on it)
//!   • `builtin.file:read`         — a single tool on an instance
//!   • `facade:shell`              — a facade surface (shell / edit_input / submit_input)
//!   • `*`                         — every instance (explicit permissive)
//!   • `facade:*`                  — every facade surface
//!   • `admin`                     — binding-admin (write any context's loadout)
//!   • `drive`/`fork`/`drift`/`transport`/`operator` — kj verb authorities
//!     (escalation-relevant kj subcommands; explicit, NOT implied by `*`)
//!
//! Semantics: **deny-by-default** — a context with no binding grants nothing.
//! The rc `create`/`fork` lifecycle assigns the initial loadout (broad roles
//! grant `*` + `facade:*`; read-only roles enumerate). `revoke` removes a
//! grant; it does not add a deny (denying one tool of an otherwise-allowed
//! instance is the dynamic hook layer's job, not the static allow-set).
//!
//! Write policy ([`KjDispatcher::authorize_binding_write`]): the rc lifecycle
//! (privileged kaish) or a `binding_admin` context may widen / target any
//! context; an ordinary context may only narrow (`revoke`/`reset`) **its own**
//! loadout — it cannot self-escalate even though `kj` bypasses `call_tool`.

use clap::{Parser, Subcommand};
use kaijutsu_types::{ContentType, ContextId};

use crate::mcp::{Capability, InstanceId};

use super::refs::resolve_context_arg;
use super::{KjCaller, KjDispatcher, KjResult};

#[derive(Parser, Debug)]
#[command(
    name = "binding",
    about = "Read and write a context's tool-capability allow-set",
    disable_help_subcommand = true,
    no_binary_name = true
)]
pub(crate) struct BindingArgs {
    #[command(subcommand)]
    command: BindingCommand,
}

#[derive(Subcommand, Debug)]
enum BindingCommand {
    /// Show a context's binding (deny-by-default if unbound).
    #[command(alias = "ls", alias = "list")]
    Show {
        /// Target context: . (default) | .parent | <label> | <hex prefix>
        ctx: Option<String>,
    },
    /// Grant a capability (widen the loadout). Privileged/admin only.
    #[command(alias = "grant")]
    Allow {
        /// Capability: <instance> | <instance>:<tool> | facade:<name> | * |
        /// facade:* | admin | rc-write | drive | fork | drift | transport | operator
        cap: String,
        /// Target context: . (default) | .parent | <label> | <hex prefix>
        ctx: Option<String>,
    },
    /// Revoke a capability (narrow the loadout).
    #[command(alias = "deny")]
    Revoke {
        /// Capability to revoke (same forms as `allow`)
        cap: String,
        /// Target context: . (default) | .parent | <label> | <hex prefix>
        ctx: Option<String>,
    },
    /// Clear the binding → deny-all (deny-by-default).
    #[command(alias = "clear")]
    Reset {
        /// Target context: . (default) | .parent | <label> | <hex prefix>
        ctx: Option<String>,
    },
}

/// Parse a capability token. Order matters: the wildcards and the `facade:`
/// prefix are checked before the generic `instance:tool` split so a facade name
/// containing no colon still routes correctly.
///
/// Wildcards make default-permissive explicit (deny-by-default everywhere else):
///   `*`        → every broker instance (`AllInstances`)
///   `facade:*` → every facade surface (`AllFacades`)
///   `admin`    → binding-admin (may write any context's loadout)
///   `rc-write` → may write /etc/rc lifecycle scripts via the file tools
///                (dedicated; NOT implied by `*`/`facade:*`)
fn parse_capability(s: &str) -> Result<Capability, String> {
    match s {
        "*" => return Ok(Capability::AllInstances),
        "facade:*" => return Ok(Capability::AllFacades),
        "admin" => return Ok(Capability::Admin),
        "rc-write" => return Ok(Capability::RcWrite),
        _ => {}
    }
    // Authority caps: bare-word kj verb grants (drive/fork/drift/transport/
    // operator), explicit and not implied by `*`. Checked before the generic
    // `instance:tool` split since they contain no colon.
    if let Some(cap) = Capability::from_authority_name(s) {
        return Ok(cap);
    }
    if let Some(rest) = s.strip_prefix("facade:") {
        if rest.is_empty() {
            return Err("kj binding: `facade:` needs a name (e.g. facade:shell)".into());
        }
        return Ok(Capability::Facade(rest.to_string()));
    }
    if let Some((inst, tool)) = s.split_once(':') {
        if inst.is_empty() || tool.is_empty() {
            return Err(format!(
                "kj binding: invalid capability '{s}' — expected instance:tool"
            ));
        }
        return Ok(Capability::Tool {
            instance: InstanceId::new(inst),
            tool: tool.to_string(),
        });
    }
    if s.is_empty() {
        return Err("kj binding: a capability is required".into());
    }
    Ok(Capability::Instance(InstanceId::new(s)))
}

impl KjDispatcher {
    pub(crate) async fn dispatch_binding(&self, argv: &[String], caller: &KjCaller) -> KjResult {
        // Empty argv defaults to `show` (the original `unwrap_or("show")`), so
        // synthesize that subcommand rather than rendering help.
        let parsed = if argv.is_empty() {
            BindingArgs {
                command: BindingCommand::Show { ctx: None },
            }
        } else {
            match BindingArgs::try_parse_from(argv) {
                Ok(p) => p,
                Err(e) => {
                    if matches!(
                        e.kind(),
                        clap::error::ErrorKind::DisplayHelp
                            | clap::error::ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand
                    ) {
                        return KjResult::ok_ephemeral(e.to_string(), ContentType::Plain);
                    }
                    return KjResult::Err(format!("kj binding: {e}"));
                }
            }
        };

        match parsed.command {
            BindingCommand::Show { ctx } => self.binding_show(ctx.as_deref(), caller).await,
            BindingCommand::Allow { cap, ctx } => {
                self.binding_mutate(&cap, ctx.as_deref(), caller, true).await
            }
            BindingCommand::Revoke { cap, ctx } => {
                self.binding_mutate(&cap, ctx.as_deref(), caller, false).await
            }
            BindingCommand::Reset { ctx } => self.binding_reset(ctx.as_deref(), caller).await,
        }
    }

    async fn binding_show(&self, ctx: Option<&str>, caller: &KjCaller) -> KjResult {
        let ctx_id = {
            let db = self.kernel_db().lock();
            match resolve_context_arg(ctx, caller, &db) {
                Ok(id) => id,
                Err(e) => return KjResult::Err(e),
            }
        };

        let binding = self.kernel().broker().binding(&ctx_id).await;
        let data = match &binding {
            None => serde_json::json!({
                "context_id": ctx_id.to_hex(),
                "bound": false,
                "instances": [],
                "tools": [],
                "facades": [],
            }),
            Some(b) => serde_json::json!({
                "context_id": ctx_id.to_hex(),
                "bound": true,
                "all_instances": b.all_instances,
                "all_facades": b.all_facades,
                "admin": b.binding_admin,
                "rc_write": b.binding_rc_write,
                "instances": b.allowed_instances.iter().map(|i| i.as_str()).collect::<Vec<_>>(),
                "tools": b.allowed_tools.iter()
                    .map(|(i, t)| format!("{}:{}", i.as_str(), t))
                    .collect::<Vec<_>>(),
                "facades": b.allowed_facades.clone(),
                "authorities": b.authorities.iter().cloned().collect::<Vec<_>>(),
            }),
        };

        let text = match &binding {
            None => format!(
                "context {}: no binding — denies all (deny-by-default; grant with `kj binding allow`)",
                ctx_id.short()
            ),
            Some(b) => {
                let instances = if b.allowed_instances.is_empty() {
                    "(none)".to_string()
                } else {
                    b.allowed_instances
                        .iter()
                        .map(|i| i.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                };
                let tools = if b.allowed_tools.is_empty() {
                    "(none)".to_string()
                } else {
                    b.allowed_tools
                        .iter()
                        .map(|(i, t)| format!("{}:{}", i.as_str(), t))
                        .collect::<Vec<_>>()
                        .join(", ")
                };
                let facades = if b.allowed_facades.is_empty() {
                    "(none)".to_string()
                } else {
                    b.allowed_facades.join(", ")
                };
                let authorities = if b.authorities.is_empty() {
                    "(none)".to_string()
                } else {
                    b.authorities.iter().cloned().collect::<Vec<_>>().join(", ")
                };
                // Flags first — they dominate the granular lists (`*` allows every
                // instance regardless of the explicit list, etc.). Without this
                // line a broad binding misleadingly reads as all "(none)".
                let mut flags = Vec::new();
                if b.all_instances {
                    flags.push("* (all instances)");
                }
                if b.all_facades {
                    flags.push("facade:* (all facades)");
                }
                if b.binding_admin {
                    flags.push("admin");
                }
                if b.binding_rc_write {
                    flags.push("rc-write");
                }
                let flags = if flags.is_empty() {
                    "(none)".to_string()
                } else {
                    flags.join(", ")
                };
                format!(
                    "context {}:\n  grants: {flags}\n  instances: {instances}\n  tools: {tools}\n  facades: {facades}\n  authorities: {authorities}",
                    ctx_id.short()
                )
            }
        };
        KjResult::ok_with_data(text, data)
    }

    async fn binding_mutate(
        &self,
        cap: &str,
        ctx: Option<&str>,
        caller: &KjCaller,
        allow: bool,
    ) -> KjResult {
        let cap = match parse_capability(cap) {
            Ok(c) => c,
            Err(e) => return KjResult::Err(e),
        };
        let ctx_id = {
            let db = self.kernel_db().lock();
            match resolve_context_arg(ctx, caller, &db) {
                Ok(id) => id,
                Err(e) => return KjResult::Err(e),
            }
        };

        // `allow` widens the loadout; `revoke` narrows it. Only the rc
        // lifecycle or a binding-admin context may widen / touch another
        // context — everyone else may only attenuate their own.
        if let Err(e) = self.authorize_binding_write(caller, ctx_id, allow).await {
            return KjResult::Err(e);
        }

        let broker = self.kernel().broker();
        let mut binding = broker.binding(&ctx_id).await.unwrap_or_default();
        if allow {
            binding.grant(cap.clone());
        } else {
            binding.revoke_cap(&cap);
        }
        broker.set_binding(ctx_id, binding).await;

        let verb = if allow { "allowed" } else { "revoked" };
        KjResult::ok(format!("{verb} {} on context {}", cap_label(&cap), ctx_id.short()))
    }

    async fn binding_reset(&self, ctx: Option<&str>, caller: &KjCaller) -> KjResult {
        let ctx_id = {
            let db = self.kernel_db().lock();
            match resolve_context_arg(ctx, caller, &db) {
                Ok(id) => id,
                Err(e) => return KjResult::Err(e),
            }
        };
        // reset clears the binding → deny-all. Pure attenuation, so an ordinary
        // context may reset itself; targeting another context still needs admin.
        if let Err(e) = self.authorize_binding_write(caller, ctx_id, false).await {
            return KjResult::Err(e);
        }
        self.kernel().broker().clear_binding(&ctx_id).await;
        KjResult::ok(format!(
            "reset context {} — now denies all (deny-by-default; grant with `kj binding allow`)",
            ctx_id.short()
        ))
    }

    /// Authorize a binding *write* on `target` from `caller`. Three tiers:
    /// rc-privileged (the lifecycle assigning loadouts) → anything; a
    /// binding-admin context → anything, any target; otherwise → only narrowing
    /// (`widening == false`) of the caller's *own* context. Widening is `allow`
    /// (grant); narrowing is `revoke`/`reset`.
    async fn authorize_binding_write(
        &self,
        caller: &KjCaller,
        target: ContextId,
        widening: bool,
    ) -> Result<(), String> {
        if caller.privileged {
            return Ok(());
        }
        let caller_ctx = caller.context_id;
        let is_admin = match caller_ctx {
            Some(c) => self
                .kernel()
                .broker()
                .binding(&c)
                .await
                .map(|b| b.is_admin())
                .unwrap_or(false),
            None => false,
        };
        if is_admin {
            return Ok(());
        }
        if caller_ctx != Some(target) {
            return Err("kj binding: only a binding-admin context (or the rc lifecycle) \
                 may modify another context's loadout"
                .to_string());
        }
        if widening {
            return Err("kj binding: this context may only narrow (revoke) its own loadout; \
                 widening needs a binding-admin context or the rc lifecycle"
                .to_string());
        }
        Ok(())
    }
}

pub(crate) fn cap_label(cap: &Capability) -> String {
    match cap {
        Capability::Instance(i) => i.as_str().to_string(),
        Capability::Tool { instance, tool } => format!("{}:{}", instance.as_str(), tool),
        Capability::Facade(name) => format!("facade:{name}"),
        Capability::AllInstances => "*".to_string(),
        Capability::AllFacades => "facade:*".to_string(),
        Capability::Admin => "admin".to_string(),
        Capability::RcWrite => "rc-write".to_string(),
        Capability::Drive
        | Capability::Fork
        | Capability::Drift
        | Capability::Transport
        | Capability::Operator => cap
            .authority_name()
            .expect("authority variant has a token")
            .to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kj::test_helpers::{caller_with_context, register_context, test_dispatcher};
    use kaijutsu_types::PrincipalId;

    fn argv(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|s| s.to_string()).collect()
    }

    #[tokio::test]
    async fn kj_binding_allow_show_revoke_reset_round_trip() {
        let d = test_dispatcher().await;
        let ctx = register_context(&d, Some("bind-test"), None, PrincipalId::system());
        // register_context grants a broad test loadout; clear it so this test
        // starts from the real deny-by-default state it asserts about.
        d.kernel().broker().clear_binding(&ctx).await;
        // Widening (`allow`) requires a privileged (rc) or admin caller — the
        // rc lifecycle is what assigns loadouts. Simulate the rc path here.
        let caller = KjCaller {
            privileged: true,
            ..caller_with_context(ctx)
        };
        let file = InstanceId::new("builtin.file");

        // Granting one tool builds a loadout from deny-all (deny-by-default).
        let r = d
            .dispatch(&argv(&["binding", "allow", "builtin.file:read"]), &caller)
            .await;
        assert!(!matches!(r, KjResult::Err(_)), "allow failed: {r:?}");
        let b = d.kernel().broker().binding(&ctx).await.expect("bound");
        assert!(b.allows_tool(&file, "read"), "granted tool not allowed");
        assert!(!b.allows_tool(&file, "write"), "ungranted sibling leaked");

        // show returns structured data listing the grant.
        let show = d.dispatch(&argv(&["binding", "show"]), &caller).await;
        let data = match show {
            KjResult::Ok { data: Some(d), .. } => d,
            other => panic!("expected show data, got {other:?}"),
        };
        let tools = data["tools"].as_array().expect("tools array");
        assert_eq!(tools, &vec![serde_json::json!("builtin.file:read")]);

        // revoke removes the grant.
        let r = d
            .dispatch(&argv(&["binding", "revoke", "builtin.file:read"]), &caller)
            .await;
        assert!(!matches!(r, KjResult::Err(_)), "revoke failed: {r:?}");
        let b = d.kernel().broker().binding(&ctx).await.expect("still bound");
        assert!(!b.allows_tool(&file, "read"), "revoke did not remove grant");

        // reset clears the binding → deny-all (no row).
        let r = d.dispatch(&argv(&["binding", "reset"]), &caller).await;
        assert!(!matches!(r, KjResult::Err(_)), "reset failed: {r:?}");
        assert!(
            d.kernel().broker().binding(&ctx).await.is_none(),
            "reset should clear the binding"
        );
    }

    #[tokio::test]
    async fn loadout_write_guard_enforces_self_narrow_only() {
        // Deny-by-default + write policy: an ordinary (non-rc, non-admin)
        // context may narrow its OWN loadout but never widen, and never touch
        // another context.
        let d = test_dispatcher().await;
        let ctx = register_context(&d, Some("ordinary"), None, PrincipalId::system());
        let other = register_context(&d, Some("other"), None, PrincipalId::system());
        // Clear the broad test loadout: this guard is about deny-by-default and
        // self-narrow-only, so both contexts must start from a clean slate.
        d.kernel().broker().clear_binding(&ctx).await;
        d.kernel().broker().clear_binding(&other).await;
        let file = InstanceId::new("builtin.file");

        // Seed a loadout as the rc lifecycle would (privileged).
        let rc = KjCaller {
            privileged: true,
            ..caller_with_context(ctx)
        };
        let r = d.dispatch(&argv(&["binding", "allow", "builtin.file:read"]), &rc).await;
        assert!(!matches!(r, KjResult::Err(_)), "rc allow failed: {r:?}");

        // Ordinary caller in `ctx` (not privileged, not admin).
        let me = caller_with_context(ctx);

        // Widen own loadout → DENIED.
        let r = d.dispatch(&argv(&["binding", "allow", "builtin.file:write"]), &me).await;
        assert!(matches!(r, KjResult::Err(_)), "self-widen must be denied");
        let b = d.kernel().broker().binding(&ctx).await.expect("bound");
        assert!(!b.allows_tool(&file, "write"), "denied widen still mutated");

        // Narrow own loadout → ALLOWED.
        let r = d.dispatch(&argv(&["binding", "revoke", "builtin.file:read"]), &me).await;
        assert!(!matches!(r, KjResult::Err(_)), "self-narrow must be allowed: {r:?}");
        let b = d.kernel().broker().binding(&ctx).await.expect("still bound");
        assert!(!b.allows_tool(&file, "read"), "self-narrow did not take effect");

        // Self-grant of admin → DENIED (no self-escalation).
        let r = d.dispatch(&argv(&["binding", "allow", "admin"]), &me).await;
        assert!(matches!(r, KjResult::Err(_)), "self-grant admin must be denied");

        // Touch ANOTHER context → DENIED for a non-admin.
        let r = d
            .dispatch(&argv(&["binding", "revoke", "builtin.file:read", &other.to_hex()]), &me)
            .await;
        assert!(matches!(r, KjResult::Err(_)), "cross-context write must be denied");
    }

    #[tokio::test]
    async fn admin_context_may_widen_and_target_others() {
        // A binding_admin context (director) may widen its own loadout and
        // write another context's — the "everything + manage others" path.
        let d = test_dispatcher().await;
        let admin_ctx = register_context(&d, Some("director"), None, PrincipalId::system());
        let target = register_context(&d, Some("managed"), None, PrincipalId::system());
        // Clear the broad test loadout so the admin grant + widening below are
        // the only grants in play (otherwise both start fully capable).
        d.kernel().broker().clear_binding(&admin_ctx).await;
        d.kernel().broker().clear_binding(&target).await;

        // Make admin_ctx an admin (privileged rc bootstrap).
        let rc = KjCaller {
            privileged: true,
            ..caller_with_context(admin_ctx)
        };
        let r = d.dispatch(&argv(&["binding", "allow", "admin"]), &rc).await;
        assert!(!matches!(r, KjResult::Err(_)), "rc admin grant failed: {r:?}");

        // Now act as the (non-privileged) admin context.
        let admin = caller_with_context(admin_ctx);

        // Widen self → allowed (admin).
        let r = d.dispatch(&argv(&["binding", "allow", "*"]), &admin).await;
        assert!(!matches!(r, KjResult::Err(_)), "admin self-widen failed: {r:?}");
        assert!(d.kernel().broker().binding(&admin_ctx).await.unwrap().all_instances);

        // Widen ANOTHER context → allowed (admin).
        let r = d
            .dispatch(&argv(&["binding", "allow", "builtin.file:read", &target.to_hex()]), &admin)
            .await;
        assert!(!matches!(r, KjResult::Err(_)), "admin cross-context write failed: {r:?}");
        assert!(
            d.kernel()
                .broker()
                .binding(&target)
                .await
                .unwrap()
                .allows_tool(&InstanceId::new("builtin.file"), "read")
        );
    }

    #[test]
    fn parse_capability_distinguishes_kinds() {
        assert_eq!(
            parse_capability("builtin.file").unwrap(),
            Capability::Instance(InstanceId::new("builtin.file"))
        );
        assert_eq!(
            parse_capability("builtin.file:read").unwrap(),
            Capability::Tool {
                instance: InstanceId::new("builtin.file"),
                tool: "read".into()
            }
        );
        assert_eq!(
            parse_capability("facade:context_shell").unwrap(),
            Capability::Facade("context_shell".into())
        );
        assert_eq!(parse_capability("admin").unwrap(), Capability::Admin);
        assert_eq!(parse_capability("rc-write").unwrap(), Capability::RcWrite);
        assert_eq!(parse_capability("drive").unwrap(), Capability::Drive);
        assert_eq!(parse_capability("fork").unwrap(), Capability::Fork);
        assert_eq!(parse_capability("drift").unwrap(), Capability::Drift);
        assert_eq!(parse_capability("transport").unwrap(), Capability::Transport);
        assert_eq!(parse_capability("operator").unwrap(), Capability::Operator);
        assert!(parse_capability("").is_err());
        assert!(parse_capability("builtin.file:").is_err());
        assert!(parse_capability("facade:").is_err());
    }
}
