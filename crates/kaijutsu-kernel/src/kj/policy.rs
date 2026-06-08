//! `kj policy` — inspect and tune a registered instance's per-call QoS policy
//! (call timeout, max result bytes). This is the kj-rich mirror of the
//! `builtin.policy` MCP tool, so rc `.kai` scripts can tune instances without
//! reaching for the MCP surface.
//!
//! ```text
//! kj policy show <instance>
//! kj policy set  <instance> [--timeout-ms N] [--max-result-bytes N]
//! ```
//!
//! Note: capability *allow-sets* are `kj binding`; this command is the
//! orthogonal resource-limit axis (`InstancePolicy`). `max_concurrency` is
//! set at registration time only (resizing a live semaphore races in-flight
//! permits) and is therefore read-only here.

use std::time::Duration;

use clap::{Parser, Subcommand};

use crate::mcp::InstanceId;

use super::{clap_help_for, KjCaller, KjDispatcher, KjResult};

#[derive(Parser, Debug)]
#[command(
    name = "policy",
    about = "Inspect and tune a registered instance's per-call QoS policy",
    disable_help_subcommand = true,
    no_binary_name = true
)]
struct PolicyArgs {
    #[command(subcommand)]
    command: PolicyCommand,
}

#[derive(Subcommand, Debug)]
enum PolicyCommand {
    /// Show an instance's current QoS policy (timeout, max result bytes, concurrency).
    Show {
        /// Instance to inspect
        instance: String,
    },
    /// Update an instance's per-call QoS policy. Pass at least one flag.
    Set {
        /// Instance to update
        instance: String,
        /// Per-call timeout, in milliseconds
        #[arg(long = "timeout-ms")]
        timeout_ms: Option<u64>,
        /// Maximum result payload size, in bytes
        #[arg(long = "max-result-bytes")]
        max_result_bytes: Option<usize>,
    },
}

impl KjDispatcher {
    pub(crate) async fn dispatch_policy(&self, argv: &[String], caller: &KjCaller) -> KjResult {
        if argv.is_empty() {
            return clap_help_for::<PolicyArgs>();
        }
        let parsed = match PolicyArgs::try_parse_from(argv) {
            Ok(p) => p,
            Err(e) => {
                if matches!(
                    e.kind(),
                    clap::error::ErrorKind::DisplayHelp
                        | clap::error::ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand
                ) {
                    return KjResult::ok_ephemeral(e.to_string(), kaijutsu_types::ContentType::Plain);
                }
                return KjResult::Err(format!("kj policy: {e}"));
            }
        };

        // `set` writes an instance's QoS policy — gated on the same
        // `builtin.policy:policy_set` tool cap the MCP surface uses. `show` reads.
        if matches!(parsed.command, PolicyCommand::Set { .. }) {
            let cap = crate::mcp::Capability::Tool {
                instance: InstanceId::new("builtin.policy"),
                tool: "policy_set".to_string(),
            };
            if let Err(denied) = self.require_cap(caller, cap, "policy set") {
                return denied;
            }
        }

        match parsed.command {
            PolicyCommand::Show { instance } => self.policy_show(&instance).await,
            PolicyCommand::Set {
                instance,
                timeout_ms,
                max_result_bytes,
            } => self.policy_set(&instance, timeout_ms, max_result_bytes).await,
        }
    }

    async fn policy_show(&self, instance_str: &str) -> KjResult {
        let instance = InstanceId::new(instance_str);
        match self.kernel().broker().policy_of(&instance).await {
            Some(p) => {
                let data = serde_json::json!({
                    "instance": instance.as_str(),
                    "call_timeout_ms": p.call_timeout.as_millis() as u64,
                    "max_result_bytes": p.max_result_bytes,
                    "max_concurrency": p.max_concurrency,
                });
                KjResult::ok_with_data(
                    format!(
                        "{}: timeout={}ms max_result_bytes={} max_concurrency={}",
                        instance.as_str(),
                        p.call_timeout.as_millis(),
                        p.max_result_bytes,
                        p.max_concurrency,
                    ),
                    data,
                )
            }
            None => KjResult::Err(format!(
                "kj policy show: instance '{}' not registered",
                instance.as_str()
            )),
        }
    }

    async fn policy_set(
        &self,
        instance_str: &str,
        timeout_ms: Option<u64>,
        max_result_bytes: Option<usize>,
    ) -> KjResult {
        let instance = InstanceId::new(instance_str);

        let timeout = timeout_ms.map(Duration::from_millis);
        let max_bytes = max_result_bytes;
        if timeout.is_none() && max_bytes.is_none() {
            return KjResult::Err(
                "kj policy set: nothing to change (pass --timeout-ms and/or --max-result-bytes)"
                    .into(),
            );
        }

        match self
            .kernel()
            .broker()
            .update_policy(&instance, timeout, max_bytes)
            .await
        {
            Ok(()) => KjResult::ok(format!("updated policy for {}", instance.as_str())),
            Err(e) => KjResult::Err(format!("kj policy set: {e}")),
        }
    }
}
