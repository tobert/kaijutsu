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

use kaijutsu_types::ContentType;

use crate::llm::stream::{CacheTarget, CacheTtl};

use super::parse::extract_named_arg;
use super::{KjCaller, KjDispatcher, KjResult};

impl KjDispatcher {
    pub(crate) fn dispatch_cache(&self, argv: &[String], caller: &KjCaller) -> KjResult {
        if argv.is_empty() {
            return KjResult::Err(self.cache_help());
        }
        match argv[0].as_str() {
            "list" | "ls" => self.cache_list(caller),
            "add" => self.cache_add(&argv[1..], caller),
            "clear" | "rm" => self.cache_clear(caller),
            "help" | "--help" | "-h" => {
                KjResult::ok_ephemeral(self.cache_help(), ContentType::Markdown)
            }
            other => KjResult::Err(format!(
                "kj cache: unknown subcommand '{}'\n\n{}",
                other,
                self.cache_help()
            )),
        }
    }

    fn cache_help(&self) -> String {
        include_str!("../../docs/help/kj-cache.md").to_string()
    }

    fn cache_list(&self, caller: &KjCaller) -> KjResult {
        let context_id = match caller.require_context() {
            Ok(id) => id,
            Err(e) => return e,
        };

        let db = self.kernel_db().lock();
        match db.list_cache_breakpoints(context_id) {
            Ok(bps) if bps.is_empty() => KjResult::ok("(no cache breakpoints)".to_string()),
            Ok(bps) => {
                let lines: Vec<String> = bps
                    .iter()
                    .enumerate()
                    .map(|(i, bp)| format!("  {i}: {}", format_target(bp)))
                    .collect();
                KjResult::ok(lines.join("\n"))
            }
            Err(e) => KjResult::Err(format!("kj cache list: {e}")),
        }
    }

    fn cache_add(&self, argv: &[String], caller: &KjCaller) -> KjResult {
        let context_id = match caller.require_context() {
            Ok(id) => id,
            Err(e) => return e,
        };

        let target_str = match extract_named_arg(argv, &["--target", "-t"]) {
            Some(t) => t,
            None => {
                return KjResult::Err(
                    "kj cache add: --target required (tools|system|message)".to_string(),
                );
            }
        };

        let ttl = match extract_named_arg(argv, &["--ttl"])
            .as_deref()
            .unwrap_or("ephemeral")
        {
            "ephemeral" | "eph" | "5m" => CacheTtl::Ephemeral,
            "extended" | "ext" | "1h" => CacheTtl::Extended,
            other => {
                return KjResult::Err(format!(
                    "kj cache add: unknown --ttl '{other}' (expected ephemeral|extended)"
                ));
            }
        };

        let cache_target = match target_str.as_str() {
            "tools" => CacheTarget::Tools(ttl),
            "system" => CacheTarget::System(ttl),
            "message" | "msg" => {
                let idx_str = match extract_named_arg(argv, &["--index", "-i"]) {
                    Some(s) => s,
                    None => {
                        return KjResult::Err(
                            "kj cache add: --target=message requires --index <N>".to_string(),
                        );
                    }
                };
                let idx: usize = match idx_str.parse() {
                    Ok(n) => n,
                    Err(e) => {
                        return KjResult::Err(format!(
                            "kj cache add: invalid --index '{idx_str}': {e}"
                        ));
                    }
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
