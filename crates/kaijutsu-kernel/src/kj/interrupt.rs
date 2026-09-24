//! Interrupt subcommand: stop an admitted turn on a context.
//!
//! Soft (default) sets the turn's `stop_after_turn` flag: the agentic loop
//! checks it before its next call to the model and breaks cleanly, so a
//! tool call already running keeps going until it finishes. `--immediate`
//! cancels the turn's `CancellationToken` instead, which aborts the current
//! model stream right away and, because tool-call execution is threaded off
//! the same token, its in-flight tool calls with it
//! (`runtime/interrupt.rs`).

use clap::Parser;
use kaijutsu_types::ContentType;

use super::effect::{Classify, Effect};
use super::refs;
use super::{KjCaller, KjDispatcher, KjResult};

#[derive(Parser, Debug)]
#[command(
    name = "interrupt",
    about = "Stop an admitted turn on a context.",
    disable_help_subcommand = true,
    no_binary_name = true
)]
pub(crate) struct InterruptArgs {
    /// Context to interrupt (label or id). Required: there is no default to
    /// the caller's own current context, so a bare `kj interrupt` cannot
    /// stop the caller's own turn by accident.
    target: String,
    /// Cancel the current model stream and its in-flight tool calls right
    /// away. Without this flag, the turn stops before its next call to the
    /// model instead, and a tool call already running keeps going until it
    /// finishes.
    #[arg(long)]
    immediate: bool,
}

impl KjDispatcher {
    pub(crate) async fn dispatch_interrupt(&self, argv: &[String], caller: &KjCaller) -> KjResult {
        let parsed = match InterruptArgs::try_parse_from(argv) {
            Ok(p) => p,
            Err(e) => {
                if matches!(
                    e.kind(),
                    clap::error::ErrorKind::DisplayHelp
                        | clap::error::ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand
                ) {
                    return KjResult::ok_ephemeral(e.to_string(), ContentType::Plain);
                }
                return KjResult::Err(format!("kj interrupt: {e}"));
            }
        };

        // Whoever can drive a context can stop it too — same gate as `kj drive`.
        if let Err(denied) = self.require_cap(caller, crate::mcp::Capability::Drive, "interrupt") {
            return denied;
        }

        let target = {
            let db = self.kernel_db().lock();
            match refs::resolve_context_arg(Some(&parsed.target), caller, &db) {
                Ok(id) => id,
                Err(e) => return KjResult::Err(format!("kj interrupt: {e}")),
            }
        };

        let stopped = self.kernel().turns().interrupt(target, parsed.immediate);
        if !stopped {
            return KjResult::Err(format!(
                "kj interrupt: context '{}' has no accepted turn to interrupt",
                target.short()
            ));
        }

        let mode = if parsed.immediate { "immediate" } else { "soft" };
        let display = target.short();
        KjResult::Ok {
            message: format!("interrupted turn in '{display}' ({mode})"),
            content_type: ContentType::Plain,
            ephemeral: false,
            data: Some(serde_json::json!({
                "context_id": target.to_hex(),
                "immediate": parsed.immediate,
            })),
        }
    }
}

// Verb class: kj/effect.rs
impl Classify for InterruptArgs {
    fn effect(&self) -> Effect {
        // No durable write, but it changes what an admitted turn does next.
        Effect::Write
    }
}

#[cfg(test)]
mod tests {
    use crate::kj::test_helpers::*;
    use crate::kj::KjCaller;
    use kaijutsu_types::PrincipalId;

    fn s(v: &str) -> String {
        v.to_string()
    }

    #[tokio::test]
    async fn interrupt_soft_sets_stop_after_turn_not_cancel() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let here = register_context(&d, Some("here"), None, principal);
        let target = register_context(&d, Some("target"), None, principal);
        let lease = d.kernel().turns().begin(target);

        let c = caller_with_context(here);
        let result = d.dispatch(&[s("interrupt"), s("target")], &c).await;
        assert!(result.is_ok(), "interrupt failed: {}", result.message());
        assert!(result.message().contains("soft"), "{}", result.message());

        assert!(
            lease.interrupt().stop_after_turn.load(std::sync::atomic::Ordering::Relaxed),
            "soft interrupt must set stop_after_turn"
        );
        assert!(
            !lease.interrupt().cancel.is_cancelled(),
            "soft interrupt must not cancel the stream"
        );
    }

    #[tokio::test]
    async fn interrupt_immediate_cancels_the_stream() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let here = register_context(&d, Some("here"), None, principal);
        let target = register_context(&d, Some("target"), None, principal);
        let lease = d.kernel().turns().begin(target);

        let c = caller_with_context(here);
        let result = d
            .dispatch(&[s("interrupt"), s("target"), s("--immediate")], &c)
            .await;
        assert!(result.is_ok(), "interrupt failed: {}", result.message());
        assert!(result.message().contains("immediate"), "{}", result.message());

        assert!(
            lease.interrupt().cancel.is_cancelled(),
            "--immediate must cancel the stream"
        );
        assert!(
            !lease.interrupt().stop_after_turn.load(std::sync::atomic::Ordering::Relaxed),
            "--immediate alone does not also set the soft flag"
        );
    }

    #[tokio::test]
    async fn interrupt_data_carries_full_context_id_and_mode() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let here = register_context(&d, Some("here"), None, principal);
        let target = register_context(&d, Some("target"), None, principal);
        let _lease = d.kernel().turns().begin(target);

        let c = caller_with_context(here);
        let result = d
            .dispatch(&[s("interrupt"), s("target"), s("--immediate")], &c)
            .await;
        assert!(result.is_ok(), "interrupt failed: {}", result.message());
        let data = match result {
            crate::kj::KjResult::Ok { data, .. } => data.expect("interrupt should carry data"),
            other => panic!("expected Ok, got {other:?}"),
        };
        assert_eq!(data["context_id"], serde_json::json!(target.to_hex()));
        assert_eq!(data["immediate"], serde_json::json!(true));
    }

    #[tokio::test]
    async fn interrupt_with_no_turn_in_flight_errors() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let here = register_context(&d, Some("here"), None, principal);
        let _idle = register_context(&d, Some("idle"), None, principal);

        let c = caller_with_context(here);
        let result = d.dispatch(&[s("interrupt"), s("idle")], &c).await;
        assert!(!result.is_ok(), "interrupting an idle context must error");
        assert!(
            result.message().contains("no accepted turn"),
            "error should say why: {}",
            result.message()
        );
    }

    #[tokio::test]
    async fn interrupt_requires_an_explicit_target() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let here = register_context(&d, Some("here"), None, principal);

        let c = caller_with_context(here);
        // No positional target: clap must refuse it, never default to `.`.
        let result = d.dispatch(&[s("interrupt")], &c).await;
        assert!(!result.is_ok(), "a bare `kj interrupt` must not target the caller's own context");
    }

    #[tokio::test]
    async fn interrupt_denied_without_drive_capability() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let ctx = register_context(&d, Some("nodrive"), None, principal);
        let target = register_context(&d, Some("target"), None, principal);
        let lease = d.kernel().turns().begin(target);

        let mut binding = crate::mcp::ContextToolBinding::new();
        binding.grant(crate::mcp::Capability::AllInstances);
        binding.grant(crate::mcp::Capability::AllFacades);
        binding.grant(crate::mcp::Capability::Operator);
        d.kernel_db().lock().upsert_context_binding(ctx, &binding).unwrap();

        let c = caller_with_context(ctx);
        let result = d.dispatch(&[s("interrupt"), s("target")], &c).await;
        assert!(!result.is_ok(), "interrupt without the `drive` cap must be denied");
        assert!(
            result.message().contains("denied") && result.message().contains("drive"),
            "denial should name the missing capability: {}",
            result.message()
        );
        assert!(
            !lease.interrupt().stop_after_turn.load(std::sync::atomic::Ordering::Relaxed)
                && !lease.interrupt().cancel.is_cancelled(),
            "a denied caller must not reach the turn's interrupt state"
        );

        binding.grant(crate::mcp::Capability::Drive);
        d.kernel_db().lock().upsert_context_binding(ctx, &binding).unwrap();
        let result = d.dispatch(&[s("interrupt"), s("target")], &c).await;
        assert!(result.is_ok(), "interrupt with the `drive` cap should pass: {}", result.message());
    }

    #[tokio::test]
    async fn interrupt_privileged_caller_bypasses_gate() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let ctx = register_context(&d, Some("rc"), None, principal);
        let target = register_context(&d, Some("target"), None, principal);
        let _lease = d.kernel().turns().begin(target);
        d.kernel_db()
            .lock()
            .upsert_context_binding(ctx, &crate::mcp::ContextToolBinding::new())
            .unwrap();

        let c = KjCaller {
            privileged: true,
            ..caller_with_context(ctx)
        };
        let result = d.dispatch(&[s("interrupt"), s("target")], &c).await;
        assert!(
            result.is_ok(),
            "privileged caller should bypass the interrupt gate: {}",
            result.message()
        );
    }

    #[tokio::test]
    async fn interrupt_help() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let ctx = register_context(&d, Some("here"), None, principal);
        let c = caller_with_context(ctx);

        let result = d.dispatch(&[s("interrupt"), s("--help")], &c).await;
        assert!(result.is_ok(), "help failed: {}", result.message());
        assert!(
            result.message().contains("Usage") && result.message().contains("--immediate"),
            "help should carry clap usage + the --immediate flag: {}",
            result.message()
        );
    }
}
