//! `kj council` — the council seats a context's gate decisions read.
//!
//! ```text
//! kj council build
//! ```
//!
//! `build` plans the seats that depend on the active context (its reviewer
//! chain, its house rules, its judge's server) and builds them in the
//! background with the prepare a decision runs, so the first decision finds
//! them held (`docs/council.md`, "Council v2 seats"). The create and fork rc
//! lifecycles of the gated context types call it.

use clap::{Parser, Subcommand};
use kaijutsu_types::ContentType;

use super::effect::{Classify, Effect};
use super::{clap_help_for, KjCaller, KjDispatcher, KjResult};

#[derive(Parser, Debug)]
#[command(
    name = "council",
    about = "Build the council seats this context's gate decisions read",
    disable_help_subcommand = true,
    no_binary_name = true
)]
pub(crate) struct CouncilArgs {
    #[command(subcommand)]
    command: CouncilCommand,
}

#[derive(Subcommand, Debug)]
enum CouncilCommand {
    /// Build this context's council seats in the background: its reviewer
    /// chain and house rules, and its judge's, on a council v2 server.
    /// Prints the plan and returns at once; the kernel log reports each
    /// build. Prints that there is nothing to build when the context's
    /// type has no council.
    Build,
}

impl KjDispatcher {
    pub(crate) async fn dispatch_council(&self, argv: &[String], caller: &KjCaller) -> KjResult {
        if argv.is_empty() {
            return clap_help_for::<CouncilArgs>();
        }
        let parsed = match CouncilArgs::try_parse_from(argv) {
            Ok(p) => p,
            Err(e) => {
                if matches!(
                    e.kind(),
                    clap::error::ErrorKind::DisplayHelp | clap::error::ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand
                ) {
                    return KjResult::ok_ephemeral(e.to_string(), ContentType::Plain);
                }
                return KjResult::Err(format!("kj council: {e}"));
            }
        };
        match parsed.command {
            CouncilCommand::Build => self.council_build(caller).await,
        }
    }

    async fn council_build(&self, caller: &KjCaller) -> KjResult {
        let context = match caller.require_context() {
            Ok(id) => id,
            Err(e) => return e,
        };
        let kernel = self.kernel().clone();
        let (builds, without_chain) = match crate::council::sync::plan_context_seats(&kernel, context).await {
            Ok(plan) => plan,
            Err(e) => return KjResult::Err(format!("kj council build: {e}")),
        };
        if builds.is_empty() {
            return KjResult::ok_with_data(
                "no council reads for this context's type; no seats to build".to_string(),
                serde_json::json!([]),
            );
        }
        let mut lines = vec!["building council seats in the background; the kernel log reports each build:".to_string()];
        let mut data = Vec::new();
        for build in &builds {
            let mut reads = build.labels.clone();
            if build.house_rules() {
                reads.push(crate::council::sync::HOUSE_RULES_LABEL.to_string());
            }
            lines.push(format!("  {} on {}: {}", build.spec, build.server(), reads.join(", ")));
            data.push(serde_json::json!({"spec": build.spec, "server": build.server(), "reads": reads}));
        }
        if let Some(cause) = without_chain {
            lines.push(format!("  without the reviewer chain: {cause}; the first decision builds its seats"));
        }
        tokio::spawn(async move { crate::council::sync::build_context_seats(&kernel, context, builds).await });
        KjResult::ok_with_data(lines.join("\n"), serde_json::Value::Array(data))
    }
}

impl Classify for CouncilArgs {
    fn effect(&self) -> Effect {
        match self.command {
            CouncilCommand::Build => Effect::Write,
        }
    }
}
