//! `kj cache` — manage Claude prompt-cache breakpoints on the active context.
//!
//! Per the `project_cache_breakpoint_policy` memory: cache breakpoints
//! are populated per-context by rc lifecycle scripts (create/fork/drift),
//! not by a global `models.toml` floor. This subcommand is the surface
//! those rc scripts call.
//!
//! ```text
//! kj cache list
//! kj cache add --target tools  --ttl ephemeral
//! kj cache add --target system --ttl ephemeral
//! kj cache add --target message --index 42 --ttl extended
//! kj cache clear
//! ```
//!
//! Storage is liberal — the 4-breakpoint cap, ordering, and dedupe live
//! in the Claude wire layer (`crate::llm::claude::build::plan_cache`).
//! Adding a 5th breakpoint here succeeds; the wire layer drops it with
//! a `tracing::warn!` at stream time.

use clap::{Parser, Subcommand};
use kaijutsu_types::ContentType;

use crate::llm::stream::{CacheTarget, CacheTtl};

use super::{clap_help_for, KjCaller, KjDispatcher, KjResult};

#[derive(Parser, Debug)]
#[command(
    name = "cache",
    about = "Manage Claude prompt-cache breakpoints on the active context",
    disable_help_subcommand = true,
    no_binary_name = true
)]
pub(crate) struct CacheArgs {
    #[command(subcommand)]
    command: CacheCommand,
}

#[derive(Subcommand, Debug)]
enum CacheCommand {
    /// List cache breakpoints on the active context.
    #[command(alias = "ls")]
    List,
    /// Add a cache breakpoint. `--target message` also needs `--index`.
    Add {
        /// Breakpoint target: tools|system|message
        #[arg(long, short = 't')]
        target: String,
        /// TTL: ephemeral|extended (default ephemeral)
        #[arg(long)]
        ttl: Option<String>,
        /// Message index (required when --target=message)
        #[arg(long, short = 'i')]
        index: Option<usize>,
    },
    /// Clear all cache breakpoints on the active context.
    #[command(alias = "rm")]
    Clear,
}

impl KjDispatcher {
    pub(crate) fn dispatch_cache(&self, argv: &[String], caller: &KjCaller) -> KjResult {
        if argv.is_empty() {
            return clap_help_for::<CacheArgs>();
        }
        let parsed = match CacheArgs::try_parse_from(argv) {
            Ok(p) => p,
            Err(e) => {
                if matches!(
                    e.kind(),
                    clap::error::ErrorKind::DisplayHelp
                        | clap::error::ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand
                ) {
                    return KjResult::ok_ephemeral(e.to_string(), ContentType::Plain);
                }
                return KjResult::Err(format!("kj cache: {e}"));
            }
        };
        match parsed.command {
            CacheCommand::List => self.cache_list(caller),
            CacheCommand::Add {
                target,
                ttl,
                index,
            } => self.cache_add(&target, ttl.as_deref(), index, caller),
            CacheCommand::Clear => self.cache_clear(caller),
        }
    }

    fn cache_list(&self, caller: &KjCaller) -> KjResult {
        let context_id = match caller.require_context() {
            Ok(id) => id,
            Err(e) => return e,
        };

        let db = self.kernel_db().lock();
        match db.list_cache_breakpoints(context_id) {
            Ok(bps) => {
                // Iteration handle: the target descriptor (e.g. "tools",
                // "system", "message[42]"). Cache breakpoints have no
                // standalone id — they're keyed by (context, target).
                // The text view already exposes the same descriptor.
                let targets = serde_json::Value::Array(
                    bps.iter()
                        .map(|bp| serde_json::Value::String(format_target(bp)))
                        .collect(),
                );
                if bps.is_empty() {
                    return KjResult::ok_with_data(
                        "(no cache breakpoints)".to_string(),
                        targets,
                    );
                }
                let lines: Vec<String> = bps
                    .iter()
                    .enumerate()
                    .map(|(i, bp)| format!("  {i}: {}", format_target(bp)))
                    .collect();
                KjResult::ok_with_data(lines.join("\n"), targets)
            }
            Err(e) => KjResult::Err(format!("kj cache list: {e}")),
        }
    }

    fn cache_add(
        &self,
        target_str: &str,
        ttl_str: Option<&str>,
        index: Option<usize>,
        caller: &KjCaller,
    ) -> KjResult {
        let context_id = match caller.require_context() {
            Ok(id) => id,
            Err(e) => return e,
        };

        let ttl = match ttl_str.unwrap_or("ephemeral") {
            "ephemeral" | "eph" | "5m" => CacheTtl::Ephemeral,
            "extended" | "ext" | "1h" => CacheTtl::Extended,
            other => {
                return KjResult::Err(format!(
                    "kj cache add: unknown --ttl '{other}' (expected ephemeral|extended)"
                ));
            }
        };

        let cache_target = match target_str {
            "tools" => CacheTarget::Tools(ttl),
            "system" => CacheTarget::System(ttl),
            "message" | "msg" => {
                // `--index` is parsed as a typed `usize` by clap, so an invalid
                // value already failed at parse time; here we only enforce that
                // it's present for the message target.
                let Some(idx) = index else {
                    return KjResult::Err(
                        "kj cache add: --target=message requires --index <N>".to_string(),
                    );
                };
                CacheTarget::MessageIndex(idx, ttl)
            }
            other => {
                return KjResult::Err(format!(
                    "kj cache add: unknown --target '{other}' (expected tools|system|message)"
                ));
            }
        };

        let db = self.kernel_db().lock();
        match db.add_cache_breakpoint(context_id, &cache_target) {
            Ok(seq) => KjResult::ok(format!(
                "added cache breakpoint #{seq}: {}",
                format_target(&cache_target)
            )),
            Err(e) => KjResult::Err(format!("kj cache add: {e}")),
        }
    }

    fn cache_clear(&self, caller: &KjCaller) -> KjResult {
        let context_id = match caller.require_context() {
            Ok(id) => id,
            Err(e) => return e,
        };

        let db = self.kernel_db().lock();
        match db.clear_cache_breakpoints(context_id) {
            Ok(count) => KjResult::ok(format!("cleared {count} cache breakpoint(s)")),
            Err(e) => KjResult::Err(format!("kj cache clear: {e}")),
        }
    }
}

fn format_target(target: &CacheTarget) -> String {
    let ttl = match target.ttl() {
        CacheTtl::Ephemeral => "ephemeral",
        CacheTtl::Extended => "extended",
    };
    match target {
        CacheTarget::Tools(_) => format!("tools (ttl={ttl})"),
        CacheTarget::System(_) => format!("system (ttl={ttl})"),
        CacheTarget::MessageIndex(i, _) => format!("message[{i}] (ttl={ttl})"),
    }
}

#[cfg(test)]
mod tests {
    use super::format_target;
    use crate::kj::test_helpers::*;
    use crate::llm::stream::{CacheTarget, CacheTtl};
    use kaijutsu_types::PrincipalId;

    fn s(v: &str) -> String {
        v.to_string()
    }

    #[test]
    fn format_target_tools_ephemeral() {
        assert_eq!(
            format_target(&CacheTarget::Tools(CacheTtl::Ephemeral)),
            "tools (ttl=ephemeral)"
        );
    }

    #[test]
    fn format_target_system_extended() {
        assert_eq!(
            format_target(&CacheTarget::System(CacheTtl::Extended)),
            "system (ttl=extended)"
        );
    }

    #[test]
    fn format_target_message_index() {
        assert_eq!(
            format_target(&CacheTarget::MessageIndex(42, CacheTtl::Extended)),
            "message[42] (ttl=extended)"
        );
    }

    #[tokio::test]
    async fn cache_list_empty() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let ctx = register_context(&d, Some("c"), None, principal);
        let c = caller_with_context(ctx);
        let result = d.dispatch(&[s("cache"), s("list")], &c).await;
        assert!(result.is_ok());
        assert_eq!(result.message(), "(no cache breakpoints)");
    }

    #[tokio::test]
    async fn cache_add_tools_then_list_round_trip() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let ctx = register_context(&d, Some("c"), None, principal);
        let c = caller_with_context(ctx);

        let result = d
            .dispatch(
                &[
                    s("cache"),
                    s("add"),
                    s("--target"),
                    s("tools"),
                    s("--ttl"),
                    s("extended"),
                ],
                &c,
            )
            .await;
        assert!(result.is_ok(), "add failed: {}", result.message());
        assert!(
            result.message().contains("tools"),
            "msg: {}",
            result.message()
        );

        let list = d.dispatch(&[s("cache"), s("list")], &c).await;
        assert!(list.is_ok());
        assert!(
            list.message().contains("tools (ttl=extended)"),
            "msg: {}",
            list.message()
        );
    }

    #[tokio::test]
    async fn cache_add_message_index_requires_index() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let ctx = register_context(&d, Some("c"), None, principal);
        let c = caller_with_context(ctx);

        let result = d
            .dispatch(
                &[s("cache"), s("add"), s("--target"), s("message")],
                &c,
            )
            .await;
        assert!(!result.is_ok(), "must require --index");
        assert!(
            result.message().contains("--index"),
            "msg: {}",
            result.message()
        );
    }

    #[tokio::test]
    async fn cache_add_message_index_persists() {
        // The fork-time case: drop MessageIndex(fork_at - 1) so the
        // shared parent prefix gets cached.
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let ctx = register_context(&d, Some("c"), None, principal);
        let c = caller_with_context(ctx);

        let result = d
            .dispatch(
                &[
                    s("cache"),
                    s("add"),
                    s("--target"),
                    s("message"),
                    s("--index"),
                    s("12"),
                    s("--ttl"),
                    s("extended"),
                ],
                &c,
            )
            .await;
        assert!(result.is_ok(), "add failed: {}", result.message());

        let list = d.dispatch(&[s("cache"), s("list")], &c).await;
        assert!(
            list.message().contains("message[12] (ttl=extended)"),
            "msg: {}",
            list.message()
        );
    }

    #[tokio::test]
    async fn cache_clear_removes_all() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let ctx = register_context(&d, Some("c"), None, principal);
        let c = caller_with_context(ctx);

        d.dispatch(
            &[s("cache"), s("add"), s("--target"), s("tools")],
            &c,
        )
        .await;
        d.dispatch(
            &[s("cache"), s("add"), s("--target"), s("system")],
            &c,
        )
        .await;

        let result = d.dispatch(&[s("cache"), s("clear")], &c).await;
        assert!(result.is_ok());
        assert!(
            result.message().contains("cleared 2"),
            "msg: {}",
            result.message()
        );

        let list = d.dispatch(&[s("cache"), s("list")], &c).await;
        assert_eq!(list.message(), "(no cache breakpoints)");
    }

    #[tokio::test]
    async fn cache_add_unknown_target_errors() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let ctx = register_context(&d, Some("c"), None, principal);
        let c = caller_with_context(ctx);

        let result = d
            .dispatch(
                &[s("cache"), s("add"), s("--target"), s("nonsense")],
                &c,
            )
            .await;
        assert!(!result.is_ok());
        assert!(result.message().contains("nonsense"));
    }

    #[tokio::test]
    async fn cache_add_unknown_ttl_errors() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let ctx = register_context(&d, Some("c"), None, principal);
        let c = caller_with_context(ctx);

        let result = d
            .dispatch(
                &[
                    s("cache"),
                    s("add"),
                    s("--target"),
                    s("tools"),
                    s("--ttl"),
                    s("forever"),
                ],
                &c,
            )
            .await;
        assert!(!result.is_ok());
        assert!(result.message().contains("forever"));
    }

    /// `kj cache list` emits per-breakpoint target descriptors so a kaish
    /// loop can iterate them. Cache breakpoints have no standalone id —
    /// target is the canonical handle.
    #[tokio::test]
    async fn cache_list_emits_target_array() {
        use crate::kj::KjResult;
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let ctx = register_context(&d, Some("c"), None, principal);
        let c = caller_with_context(ctx);

        d.dispatch(&[s("cache"), s("add"), s("--target"), s("tools")], &c).await;
        d.dispatch(
            &[
                s("cache"),
                s("add"),
                s("--target"),
                s("message"),
                s("--index"),
                s("7"),
            ],
            &c,
        )
        .await;

        let result = d.dispatch(&[s("cache"), s("list")], &c).await;
        match result {
            KjResult::Ok { data: Some(v), .. } => {
                let targets: Vec<&str> = v
                    .as_array()
                    .expect("array")
                    .iter()
                    .filter_map(|x| x.as_str())
                    .collect();
                assert!(
                    targets.iter().any(|t| t.starts_with("tools")),
                    "missing tools entry: {targets:?}"
                );
                assert!(
                    targets.iter().any(|t| t.starts_with("message[7]")),
                    "missing message[7] entry: {targets:?}"
                );
            }
            other => panic!("expected Ok with data, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn cache_default_ttl_is_ephemeral() {
        // Omitting --ttl picks ephemeral, matching the 5-minute default
        // that the wire layer also produces when CacheControl::ephemeral()
        // is constructed without the 1h hint.
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let ctx = register_context(&d, Some("c"), None, principal);
        let c = caller_with_context(ctx);

        d.dispatch(
            &[s("cache"), s("add"), s("--target"), s("tools")],
            &c,
        )
        .await;

        let list = d.dispatch(&[s("cache"), s("list")], &c).await;
        assert!(
            list.message().contains("ttl=ephemeral"),
            "default must be ephemeral, got: {}",
            list.message()
        );
    }
}
