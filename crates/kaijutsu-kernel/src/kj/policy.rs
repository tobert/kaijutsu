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

use kaijutsu_types::ContentType;

use crate::mcp::InstanceId;

use super::parse::extract_named_arg;
use super::{KjCaller, KjDispatcher, KjResult};

impl KjDispatcher {
    pub(crate) async fn dispatch_policy(&self, argv: &[String], _caller: &KjCaller) -> KjResult {
        match argv.first().map(|s| s.as_str()) {
            Some("show") => self.policy_show(&argv[1..]).await,
            Some("set") => self.policy_set(&argv[1..]).await,
            Some("help") | Some("--help") | Some("-h") | None => {
                KjResult::ok_ephemeral(Self::policy_help(), ContentType::Markdown)
            }
            Some(other) => KjResult::Err(format!(
                "kj policy: unknown subcommand '{other}'\n\n{}",
                Self::policy_help()
            )),
        }
    }

    fn policy_help() -> String {
        concat!(
            "kj policy — inspect/tune an instance's per-call QoS policy\n\n",
            "  kj policy show <instance>\n",
            "  kj policy set  <instance> [--timeout-ms N] [--max-result-bytes N]\n"
        )
        .to_string()
    }

    async fn policy_show(&self, argv: &[String]) -> KjResult {
        let instance = match argv.first() {
            Some(s) if !s.is_empty() => InstanceId::new(s.as_str()),
            _ => return KjResult::Err("kj policy show: <instance> required".into()),
        };
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

    async fn policy_set(&self, argv: &[String]) -> KjResult {
        let instance = match argv.first() {
            Some(s) if !s.is_empty() && !s.starts_with('-') => InstanceId::new(s.as_str()),
            _ => return KjResult::Err("kj policy set: <instance> required".into()),
        };
        let rest = &argv[1..];

        let timeout = match extract_named_arg(rest, &["--timeout-ms"]) {
            Some(s) => match s.parse::<u64>() {
                Ok(n) => Some(Duration::from_millis(n)),
                Err(_) => return KjResult::Err(format!("kj policy set: bad --timeout-ms '{s}'")),
            },
            None => None,
        };
        let max_bytes = match extract_named_arg(rest, &["--max-result-bytes"]) {
            Some(s) => match s.parse::<usize>() {
                Ok(n) => Some(n),
                Err(_) => {
                    return KjResult::Err(format!("kj policy set: bad --max-result-bytes '{s}'"));
                }
            },
            None => None,
        };
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
