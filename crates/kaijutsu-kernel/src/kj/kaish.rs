//! `kj kaish` — kaish guidance, composed from `kaish-help` rather than
//! hand-maintained here.
//!
//! `kaish-help` exists so a kaish release updates every embedder's
//! agent-facing prose at once instead of each frontend (kaijutsu, kaibo)
//! drifting its own copy (`docs/composable-help.md` step 4, kaish-help's own
//! crate docs). `kj kaish primer` is the per-context onboarding surface: the
//! `lib/create/S05-kaish.kai` rc script pipes its output into a system block
//! at every context creation, so the primer is recomposed — and so re-synced
//! to whatever kaish version is linked — on every `create`, never stored
//! durably as a copy that can go stale.

use clap::{Parser, Subcommand};
use kaijutsu_types::ContentType;

use super::{KjDispatcher, KjResult, clap_help_for};

#[derive(Parser, Debug)]
#[command(
    name = "kaish",
    about = "kaish guidance composed from kaish-help",
    disable_help_subcommand = true,
    no_binary_name = true
)]
pub(crate) struct KaishArgs {
    #[command(subcommand)]
    command: KaishCommand,
}

#[derive(Subcommand, Debug)]
enum KaishCommand {
    /// Print the composed agent-onboarding primer (model + operating
    /// contract + builtins) — what `S05-kaish.kai` turns into a system block.
    Primer,
}

/// [`kaish_help::GeneratedContent`] with no live builtin index.
///
/// `Recipe::agent_onboarding()`'s `Builtins` concept wants `(name,
/// description)` pairs pulled from a live kaish tool registry, which only
/// exists inside a materialized per-context shell
/// (`KjDispatcher::materialize_context_kaish`) — spinning one up just to list
/// builtin names for a text primer would drag in a context id, principal,
/// session, and block source this context-agnostic verb has no business
/// needing. An empty index makes `compose()` skip the `Builtins` section
/// entirely (see its `index.is_empty()` guard in `kaish_help::compose`)
/// rather than render something stale or fabricated.
struct NoBuiltins;

impl kaish_help::GeneratedContent for NoBuiltins {
    fn builtin_index(&self) -> Vec<(String, String)> {
        Vec::new()
    }

    fn tool_help(&self, _name: &str) -> Option<String> {
        // Never called: `compose()` only reaches `tool_help` through the
        // `help <topic>` compatibility surface (`kaish_help::topic`), not
        // through `Recipe::agent_onboarding()`'s path — which only calls
        // `builtin_index()` for its `Builtins` section.
        None
    }
}

impl KjDispatcher {
    /// `kj kaish <primer>` — no active context required; the primer is
    /// audience-general content, not per-context state.
    pub(crate) fn dispatch_kaish(&self, argv: &[String]) -> KjResult {
        if argv.is_empty() {
            return clap_help_for::<KaishArgs>();
        }
        let parsed = match KaishArgs::try_parse_from(argv) {
            Ok(p) => p,
            Err(e) => {
                if matches!(
                    e.kind(),
                    clap::error::ErrorKind::DisplayHelp
                        | clap::error::ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand
                ) {
                    return KjResult::ok_ephemeral(e.to_string(), ContentType::Plain);
                }
                return KjResult::Err(format!("kj kaish: {e}"));
            }
        };

        match parsed.command {
            KaishCommand::Primer => self.kaish_primer(),
        }
    }

    /// Compose the primer: what kaish *is*, plus the builtin index.
    ///
    /// Deliberately NOT `Recipe::agent_onboarding()`, which also carries
    /// `Concept::Foundations` — the operating rules (no word splitting, quote
    /// to join, strict globs, pre-validation). Those already ship in the shell
    /// tool's description, which composes the same fragments: measured against
    /// this rev, 2706 of that description's 2732 bytes were byte-identical
    /// text also present in the onboarding recipe. Selecting both slots from
    /// overlapping recipes paid for the same rules twice on every turn of
    /// every context holding a shell.
    ///
    /// The split follows where each half belongs: rules about *using the
    /// shell* ride the shell tool (and cost nothing in a context without
    /// one); orientation — what this shell is, what builtins exist — rides
    /// the system prompt, where there is no tool to hang it on.
    ///
    /// Overlay guidance is excluded either way: kaijutsu materializes a fresh
    /// context kaish per call and never enables `--overlay`, so that paragraph
    /// would be an active mixed signal, telling the model to `kaish-vfs
    /// commit` for a mode nothing here turns on.
    fn kaish_primer(&self) -> KjResult {
        let selector = kaish_help::Selector {
            concepts: vec![kaish_help::Concept::Model, kaish_help::Concept::Builtins],
            variants: Vec::new(),
            audience: kaish_help::Audience::Agent,
            depth: kaish_help::Depth::Summary,
            locale: kaish_help::DEFAULT_LOCALE.to_string(),
            headers: true,
        };
        let text = kaish_help::compose(&selector, &NoBuiltins);
        KjResult::ok_typed(text, ContentType::Markdown)
    }
}

#[cfg(test)]
mod tests {
    use crate::kj::test_helpers::*;

    #[tokio::test]
    async fn primer_renders_non_empty_and_orients_the_reader() {
        let d = test_dispatcher().await;
        let c = test_caller();

        let result = d
            .dispatch(&[String::from("kaish"), String::from("primer")], &c)
            .await;
        assert!(result.is_ok(), "kaish primer failed: {}", result.message());
        let msg = result.message();
        assert!(!msg.is_empty(), "primer must not be empty");
        assert!(
            msg.contains("kaish"),
            "primer should say what shell this is: {msg}"
        );
    }

    /// The primer and the shell tool description must not both carry the
    /// operating rules. They compose from the same fragment registry, and the
    /// stock `agent_onboarding` / `tool_description` recipes overlap almost
    /// entirely — 2706 of 2732 bytes when this split was written. Paying for
    /// the same paragraphs twice on every turn is invisible until someone
    /// measures it, so measure it here.
    #[tokio::test]
    async fn primer_does_not_repeat_the_shell_tool_description() {
        let d = test_dispatcher().await;
        let c = test_caller();

        let result = d
            .dispatch(&[String::from("kaish"), String::from("primer")], &c)
            .await;
        assert!(result.is_ok(), "kaish primer failed: {}", result.message());
        let primer = result.message();

        let tool_desc = crate::mcp::servers::shell::composed_tool_description();
        let repeated: Vec<&str> = tool_desc
            .lines()
            .filter(|l| l.trim().len() > 40 && primer.contains(l.trim()))
            .collect();

        assert!(
            repeated.is_empty(),
            "these lines ship in BOTH the primer and the shell tool description, \
             so every turn pays for them twice:\n{}",
            repeated.join("\n")
        );
    }

    #[tokio::test]
    async fn primer_excludes_overlay_guidance() {
        let d = test_dispatcher().await;
        let c = test_caller();

        let result = d
            .dispatch(&[String::from("kaish"), String::from("primer")], &c)
            .await;
        assert!(result.is_ok(), "kaish primer failed: {}", result.message());
        let msg = result.message();
        assert!(
            !msg.contains("Overlay mode") && !msg.contains("kaish-vfs commit"),
            "primer must not carry overlay guidance kaijutsu never enables: {msg}"
        );
    }

    #[tokio::test]
    async fn primer_requires_no_active_context() {
        let d = test_dispatcher().await;
        let mut c = test_caller();
        c.context_id = None;

        let result = d
            .dispatch(&[String::from("kaish"), String::from("primer")], &c)
            .await;
        assert!(
            result.is_ok(),
            "kaish primer is context-agnostic content and must work with no active context: {}",
            result.message()
        );
    }

    #[tokio::test]
    async fn bare_kaish_shows_help() {
        let d = test_dispatcher().await;
        let c = test_caller();

        let result = d.dispatch(&[String::from("kaish")], &c).await;
        assert!(result.is_ok(), "bare kaish should show help: {}", result.message());
        assert!(
            result.message().contains("primer"),
            "help should mention the primer subcommand: {}",
            result.message()
        );
    }
}
