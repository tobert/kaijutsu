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
//! kj binding reset  [<ctx>]          # back to default-permissive (clear)
//! ```
//!
//! A `<cap>` is one of:
//!   • `builtin.file`              — instance-wide grant (every tool on it)
//!   • `builtin.file:read`         — a single tool on an instance
//!   • `facade:context_shell`      — a facade surface (broker-call enforcement
//!                                   is a follow-up; the grant is recorded)
//!
//! Semantics: a context with **no** binding is default-permissive (first
//! touch seeds every instance). The first `allow` narrows it to exactly what
//! is granted — that is how read-only roles are built. `revoke` removes a
//! grant; it does not add a deny (denying one tool of an otherwise-allowed
//! instance is the dynamic hook layer's job, not the static allow-set).

use kaijutsu_types::ContentType;

use crate::mcp::{Capability, InstanceId};

use super::refs::resolve_context_arg;
use super::{KjCaller, KjDispatcher, KjResult};

/// Parse a capability token. Order matters: the `facade:` prefix is checked
/// before the generic `instance:tool` split so a facade name containing no
/// colon still routes correctly.
fn parse_capability(s: &str) -> Result<Capability, String> {
    if let Some(rest) = s.strip_prefix("facade:") {
        if rest.is_empty() {
            return Err("kj binding: `facade:` needs a name (e.g. facade:context_shell)".into());
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
        let sub = argv.first().map(|s| s.as_str()).unwrap_or("show");
        match sub {
            "show" | "ls" | "list" => self.binding_show(&argv[1..], caller).await,
            "allow" | "grant" => self.binding_mutate(&argv[1..], caller, true).await,
            "revoke" | "deny" => self.binding_mutate(&argv[1..], caller, false).await,
            "reset" | "clear" => self.binding_reset(&argv[1..], caller).await,
            "help" | "--help" | "-h" => {
                KjResult::ok_ephemeral(Self::binding_help(), ContentType::Markdown)
            }
            other => KjResult::Err(format!(
                "kj binding: unknown subcommand '{other}'\n\n{}",
                Self::binding_help()
            )),
        }
    }

    fn binding_help() -> String {
        concat!(
            "kj binding — manage a context's tool-capability allow-set\n\n",
            "  kj binding show   [<ctx>]\n",
            "  kj binding allow  <cap> [<ctx>]\n",
            "  kj binding revoke <cap> [<ctx>]\n",
            "  kj binding reset  [<ctx>]\n\n",
            "  <cap>: <instance> | <instance>:<tool> | facade:<name>\n",
            "  <ctx>: . (default) | .parent | <label> | <hex prefix>\n"
        )
        .to_string()
    }

    async fn binding_show(&self, argv: &[String], caller: &KjCaller) -> KjResult {
        let ctx_id = {
            let db = self.kernel_db().lock();
            match resolve_context_arg(argv.first().map(|s| s.as_str()), caller, &db) {
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
                "instances": b.allowed_instances.iter().map(|i| i.as_str()).collect::<Vec<_>>(),
                "tools": b.allowed_tools.iter()
                    .map(|(i, t)| format!("{}:{}", i.as_str(), t))
                    .collect::<Vec<_>>(),
                "facades": b.allowed_facades.clone(),
            }),
        };

        let text = match &binding {
            None => format!(
                "context {}: no binding (default-permissive — all instances)",
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
                format!(
                    "context {}:\n  instances: {instances}\n  tools: {tools}\n  facades: {facades}",
                    ctx_id.short()
                )
            }
        };
        KjResult::ok_with_data(text, data)
    }

    async fn binding_mutate(&self, argv: &[String], caller: &KjCaller, allow: bool) -> KjResult {
        let cap = match argv.first() {
            Some(s) => match parse_capability(s) {
                Ok(c) => c,
                Err(e) => return KjResult::Err(e),
            },
            None => {
                return KjResult::Err(format!(
                    "kj binding {}: a capability is required\n\n{}",
                    if allow { "allow" } else { "revoke" },
                    Self::binding_help()
                ));
            }
        };
        let ctx_id = {
            let db = self.kernel_db().lock();
            match resolve_context_arg(argv.get(1).map(|s| s.as_str()), caller, &db) {
                Ok(id) => id,
                Err(e) => return KjResult::Err(e),
            }
        };

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

    async fn binding_reset(&self, argv: &[String], caller: &KjCaller) -> KjResult {
        let ctx_id = {
            let db = self.kernel_db().lock();
            match resolve_context_arg(argv.first().map(|s| s.as_str()), caller, &db) {
                Ok(id) => id,
                Err(e) => return KjResult::Err(e),
            }
        };
        self.kernel().broker().clear_binding(&ctx_id).await;
        KjResult::ok(format!(
            "reset context {} to default-permissive",
            ctx_id.short()
        ))
    }
}

fn cap_label(cap: &Capability) -> String {
    match cap {
        Capability::Instance(i) => i.as_str().to_string(),
        Capability::Tool { instance, tool } => format!("{}:{}", instance.as_str(), tool),
        Capability::Facade(name) => format!("facade:{name}"),
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
        let caller = caller_with_context(ctx);
        let file = InstanceId::new("builtin.file");

        // Granting one tool narrows an otherwise-permissive context.
        let r = d
            .dispatch(&argv(&["binding", "allow", "builtin.file:read"]), &caller)
            .await;
        assert!(!matches!(r, KjResult::Err(_)), "allow failed: {r:?}");
        let b = d.kernel().broker().binding(&ctx).await.expect("bound");
        assert!(b.allows(&file, "read"), "granted tool not allowed");
        assert!(!b.allows(&file, "write"), "ungranted sibling leaked");

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
        assert!(!b.allows(&file, "read"), "revoke did not remove grant");

        // reset clears the binding back to default-permissive (no row).
        let r = d.dispatch(&argv(&["binding", "reset"]), &caller).await;
        assert!(!matches!(r, KjResult::Err(_)), "reset failed: {r:?}");
        assert!(
            d.kernel().broker().binding(&ctx).await.is_none(),
            "reset should clear the binding"
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
        assert!(parse_capability("").is_err());
        assert!(parse_capability("builtin.file:").is_err());
        assert!(parse_capability("facade:").is_err());
    }
}
