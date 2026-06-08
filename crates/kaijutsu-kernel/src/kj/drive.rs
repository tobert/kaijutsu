//! Drive subcommand: clock one autonomous turn on a context.
//!
//! POSIX mental model: `fork` is a snapshot, `drive` execs the child — it
//! clocks a single turn. `kj drive` is the manual-repair handle for acting on
//! a context that isn't currently driving itself: after pushing drift into it,
//! after committing a staged child, or any time a human wants to advance a turn
//! by hand.
//!
//! The kernel can't call the server's turn driver directly. It clocks a turn by
//! publishing `TurnFlow::Requested` on the FlowBus; the server's turn driver
//! subscribes to "turn.requested" and runs the LLM turn. Unlike `kj fork
//! --prompt` (fire-and-forget — it writes an Error block into an inert child if
//! nobody is listening), `kj drive` is an explicit user command, so when no
//! turn driver is subscribed it reports the failure to the user directly rather
//! than burying an Error block in the context.

use clap::Parser;
use kaijutsu_types::ContentType;

use super::refs;
use super::{KjCaller, KjDispatcher, KjResult};

#[derive(Parser, Debug)]
#[command(
    name = "drive",
    about = "Clock one autonomous turn on a context.",
    disable_help_subcommand = true,
    no_binary_name = true
)]
pub(crate) struct DriveArgs {
    /// Seed the turn with this text; when omitted the turn runs against
    /// whatever is already in the context's block log.
    #[arg(long)]
    prompt: Option<String>,
    /// Target context to drive (label or id); defaults to the current context.
    target: Option<String>,
}

impl KjDispatcher {
    pub(crate) async fn dispatch_drive(&self, argv: &[String], caller: &KjCaller) -> KjResult {
        // NOTE: bare `kj drive` (empty sub-args) is a VALID operation — it
        // drives the current context. Both DriveArgs fields are optional, so
        // `try_parse_from(&[])` yields the all-default form; we must NOT treat
        // empty argv as a help request the way subcommand-required tools (cas)
        // do. Help comes only via `--help`/`-h` (clap's DisplayHelp).
        let parsed = match DriveArgs::try_parse_from(argv) {
            Ok(p) => p,
            Err(e) => {
                if matches!(
                    e.kind(),
                    clap::error::ErrorKind::DisplayHelp
                        | clap::error::ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand
                ) {
                    return KjResult::ok_ephemeral(e.to_string(), ContentType::Plain);
                }
                return KjResult::Err(format!("kj drive: {e}"));
            }
        };

        // Self-driving is gated: the caller's loadout must hold `drive`. This is
        // what makes narrowing a composer's binding actually stop its OODA tick.
        if let Err(denied) = self.require_cap(caller, crate::mcp::Capability::Drive, "drive") {
            return denied;
        }

        // --prompt is the optional seed; when omitted the turn runs against
        // whatever is already in the context's block log (the drift-then-drive
        // path). The seed lives in TurnFlow::Requested.content, which is only a
        // hydration-failure fallback, so an empty string is correct when no
        // prompt is given.
        let seed: String = parsed.prompt.unwrap_or_default();

        // The positional target context; default to the caller's current
        // context (".") when omitted. `kj drive` drives here; `kj drive
        // <label-or-id>` drives another context.
        let target_ref = parsed.target.as_deref();

        let target = {
            let db = self.kernel_db().lock();
            match refs::resolve_context_arg(target_ref, caller, &db) {
                Ok(id) => id,
                Err(e) => return KjResult::Err(format!("kj drive: {e}")),
            }
        };

        // A context with no blocks has nothing to anchor a turn after — there's
        // no document/history to act on. Crash loudly rather than publish a
        // turn request with no valid anchor.
        let Some(after) = self.block_store().last_block_id(target) else {
            return KjResult::Err(format!(
                "kj drive: context '{}' has no blocks to anchor a turn after; \
                 there is nothing to drive",
                target.to_hex()
            ));
        };

        let delivered =
            self.publish_turn_request(target, after, seed.as_str(), caller.principal_id);

        // Zero subscribers means no turn driver is listening. Because `kj drive`
        // is an explicit command with the user right here, surface the failure
        // directly — don't silently no-op, and don't write an Error block into
        // the context (the user gets the error in their hand instead).
        if delivered == 0 {
            tracing::warn!(
                context_id = %target,
                "kj drive: no turn driver subscribed; turn was not started"
            );
            return KjResult::Err(
                "kj drive: no turn driver is active; the turn was not started".to_string(),
            );
        }

        // Identify the driven context by a compact handle in the message.
        let display = short_hex(target);
        KjResult::Ok {
            message: format!("driving turn in '{display}'"),
            content_type: ContentType::Plain,
            ephemeral: false,
            data: Some(serde_json::json!({
                "context_id": target.to_hex(),
                "delivered": delivered,
            })),
        }
    }
}

/// First 8 hex chars of a context id, for a compact human-facing handle.
fn short_hex(id: kaijutsu_types::ContextId) -> String {
    let hex = id.to_hex();
    hex.chars().take(8).collect()
}

#[cfg(test)]
mod tests {
    use crate::kj::test_helpers::*;
    use crate::kj::KjCaller;
    use kaijutsu_types::PrincipalId;

    fn s(v: &str) -> String {
        v.to_string()
    }

    /// Seed a context with a document and one block so `last_block_id` resolves
    /// — a turn needs an anchor.
    fn seed_with_block(
        d: &super::super::KjDispatcher,
        ctx: kaijutsu_types::ContextId,
        principal: PrincipalId,
    ) {
        d.block_store()
            .create_document(ctx, crate::DocumentKind::Conversation, None)
            .unwrap();
        d.block_store()
            .insert_block_as(
                ctx,
                None,
                None,
                kaijutsu_crdt::Role::User,
                kaijutsu_crdt::BlockKind::Text,
                "seed".to_string(),
                kaijutsu_crdt::Status::Done,
                kaijutsu_crdt::ContentType::Plain,
                Some(principal),
            )
            .unwrap();
    }

    #[tokio::test]
    async fn drive_current_context_requests_turn() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let ctx = register_context(&d, Some("here"), None, principal);
        seed_with_block(&d, ctx, principal);
        let c = caller_with_context(ctx);
        let mut sub = d.kernel().turn_flows().subscribe("turn.requested");

        let result = d.dispatch(&[s("drive")], &c).await;
        assert!(result.is_ok(), "drive failed: {}", result.message());

        let msg = sub
            .try_recv()
            .expect("kj drive should publish a turn request");
        match msg.payload {
            crate::flows::TurnFlow::Requested {
                context_id,
                principal_id,
                ..
            } => {
                assert_eq!(context_id, ctx, "the turn targets the current context");
                assert_eq!(principal_id, c.principal_id);
            }
            other => panic!("expected Requested, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn drive_named_context_requests_turn() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let here = register_context(&d, Some("here"), None, principal);
        let other = register_context(&d, Some("other"), None, principal);
        seed_with_block(&d, here, principal);
        seed_with_block(&d, other, principal);
        let c = caller_with_context(here);
        let mut sub = d.kernel().turn_flows().subscribe("turn.requested");

        let result = d.dispatch(&[s("drive"), s("other")], &c).await;
        assert!(result.is_ok(), "drive failed: {}", result.message());

        let msg = sub
            .try_recv()
            .expect("kj drive <ctx> should publish a turn request");
        match msg.payload {
            crate::flows::TurnFlow::Requested { context_id, .. } => {
                assert_eq!(context_id, other, "the turn targets the named context");
                assert_ne!(context_id, here, "not the caller's context");
            }
            other => panic!("expected Requested, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn drive_with_prompt_sets_content() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let ctx = register_context(&d, Some("here"), None, principal);
        seed_with_block(&d, ctx, principal);
        let c = caller_with_context(ctx);
        let mut sub = d.kernel().turn_flows().subscribe("turn.requested");

        let result = d.dispatch(&[s("drive"), s("--prompt"), s("go")], &c).await;
        assert!(result.is_ok(), "drive failed: {}", result.message());

        let msg = sub.try_recv().expect("drive --prompt should publish");
        match msg.payload {
            crate::flows::TurnFlow::Requested { content, .. } => {
                assert_eq!(content, "go");
            }
            other => panic!("expected Requested, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn drive_without_prompt_empty_content() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let ctx = register_context(&d, Some("here"), None, principal);
        seed_with_block(&d, ctx, principal);
        let c = caller_with_context(ctx);
        let mut sub = d.kernel().turn_flows().subscribe("turn.requested");

        let result = d.dispatch(&[s("drive")], &c).await;
        assert!(result.is_ok(), "drive failed: {}", result.message());

        let msg = sub.try_recv().expect("drive should publish");
        match msg.payload {
            crate::flows::TurnFlow::Requested { content, .. } => {
                assert_eq!(
                    content, "",
                    "no --prompt means empty seed; the turn runs against the log"
                );
            }
            other => panic!("expected Requested, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn drive_no_blocks_errors() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        // Context with no document/blocks at all.
        let ctx = register_context(&d, Some("empty"), None, principal);
        let c = caller_with_context(ctx);
        // A subscriber exists, so failure must come from the no-blocks guard,
        // not from the no-driver path.
        let _sub = d.kernel().turn_flows().subscribe("turn.requested");

        let result = d.dispatch(&[s("drive")], &c).await;
        assert!(
            !result.is_ok(),
            "driving a context with no blocks must error, got: {}",
            result.message()
        );
        assert!(
            result.message().contains("no blocks"),
            "error should explain the missing anchor: {}",
            result.message()
        );
    }

    #[tokio::test]
    async fn drive_no_subscriber_errors() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let ctx = register_context(&d, Some("here"), None, principal);
        seed_with_block(&d, ctx, principal);
        let c = caller_with_context(ctx);
        // Deliberately NO subscriber on "turn.requested".

        let result = d.dispatch(&[s("drive")], &c).await;
        assert!(
            !result.is_ok(),
            "drive with no turn driver must error, got: {}",
            result.message()
        );
        assert!(
            result.message().contains("no turn driver"),
            "error should name the missing turn driver: {}",
            result.message()
        );
    }

    #[tokio::test]
    async fn drive_denied_without_drive_capability() {
        // The gate that makes narrowing a composer's binding actually stop its
        // OODA tick: a non-privileged caller whose loadout lacks `drive` is
        // refused before any turn is requested.
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let ctx = register_context(&d, Some("nodrive"), None, principal);
        seed_with_block(&d, ctx, principal);
        // Replace the broad test loadout with everything EXCEPT drive, to prove
        // the denial is specific to the missing `drive` authority. Written to
        // the DB — the authoritative store require_cap reads (the broker's DB
        // handle is unset in test_dispatcher, so broker.set_binding alone would
        // only touch the cache require_cap doesn't consult).
        let mut binding = crate::mcp::ContextToolBinding::new();
        binding.grant(crate::mcp::Capability::AllInstances);
        binding.grant(crate::mcp::Capability::AllFacades);
        binding.grant(crate::mcp::Capability::Operator);
        d.kernel_db().lock().upsert_context_binding(ctx, &binding).unwrap();

        let c = caller_with_context(ctx);
        // A subscriber exists, so a pass would actually publish — isolate the gate.
        let _sub = d.kernel().turn_flows().subscribe("turn.requested");

        let result = d.dispatch(&[s("drive")], &c).await;
        assert!(!result.is_ok(), "drive without the `drive` cap must be denied");
        assert!(
            result.message().contains("denied") && result.message().contains("drive"),
            "denial should name the missing capability: {}",
            result.message()
        );

        // Granting `drive` lets the same caller through.
        binding.grant(crate::mcp::Capability::Drive);
        d.kernel_db().lock().upsert_context_binding(ctx, &binding).unwrap();
        let result = d.dispatch(&[s("drive")], &c).await;
        assert!(result.is_ok(), "drive with the `drive` cap should pass: {}", result.message());
    }

    #[tokio::test]
    async fn drive_privileged_caller_bypasses_gate() {
        // The rc lifecycle (privileged kaish) drives without holding `drive` —
        // the control plane exercises loadouts it assigns.
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let ctx = register_context(&d, Some("rc"), None, principal);
        seed_with_block(&d, ctx, principal);
        // Deny-all in the DB (the source require_cap reads) to prove the
        // privileged bypass holds even with zero granted capabilities.
        d.kernel_db()
            .lock()
            .upsert_context_binding(ctx, &crate::mcp::ContextToolBinding::new())
            .unwrap();
        let _sub = d.kernel().turn_flows().subscribe("turn.requested");

        let c = KjCaller {
            privileged: true,
            ..caller_with_context(ctx)
        };
        let result = d.dispatch(&[s("drive")], &c).await;
        assert!(
            result.is_ok(),
            "privileged caller should bypass the drive gate: {}",
            result.message()
        );
    }

    #[tokio::test]
    async fn drive_help() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let ctx = register_context(&d, Some("here"), None, principal);
        let c = caller_with_context(ctx);

        // `--help` routes through clap's DisplayHelp (the bare `help` word is no
        // longer special — it would parse as a target context ref).
        let result = d.dispatch(&[s("drive"), s("--help")], &c).await;
        assert!(result.is_ok(), "help failed: {}", result.message());
        assert!(
            result.message().contains("Usage") && result.message().contains("--prompt"),
            "help should carry clap usage + the --prompt flag: {}",
            result.message()
        );
    }
}
