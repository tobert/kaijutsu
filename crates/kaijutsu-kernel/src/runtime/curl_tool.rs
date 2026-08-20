//! The kaijutsu-wide `curl` tool: one configuration, registered once.
//!
//! [`curl_tool`] is called from exactly one site —
//! `kj::context_shell`'s `configure_tools` closure — which every
//! materialized shell shares (rc, hook bodies, the interactive shell, the
//! MCP `shell`/`shell_write` tools, read-only shells). One call site means
//! one allowlist and one ceiling; there is no second place this could drift.

use kaish_tools_curl::{AllowByList, CurlConfig, CurlTool, Limits};

/// The egress host this build's `curl` may reach. One place, so the
/// allowlist and the rule that justifies it stay next to each other —
/// widen it only as a deliberate design conversation, never casually: the
/// allowlist is curl's entire safety story (kaish-tools-curl's
/// `config.rs` module doc), and it is deny-by-empty until a host is named
/// here.
const ALLOWED_HOST: &str = "lfm2d-1.taila4abc.ts.net";

/// Per-request wall-clock ceiling, in seconds. Kept below
/// `TimeoutPolicy::hook_body_timeout` (15s, `kaijutsu-types/src/timeout.rs`)
/// so a slow request fails as a curl timeout (exit 28) inside the hook
/// body's own budget, rather than the hook body's outer timeout firing
/// first and hiding what actually happened.
const MAX_TIME_SECS: f64 = 10.0;

/// Build the `curl` tool every kaijutsu shell registers.
///
/// Deny-by-empty egress is `CurlConfig`'s own default; this narrows it to
/// exactly [`ALLOWED_HOST`] and never widens further. `-k`/`--insecure`
/// stays refused (`CurlConfig::default().insecure_permitted()` is `false`
/// and this function does not turn it on).
pub fn curl_tool() -> CurlTool {
    kaish_tools_curl::tool(
        CurlConfig::default()
            .with_limits(Limits {
                max_time: MAX_TIME_SECS,
                ..Limits::default()
            })
            .with_allow_egress(AllowByList::new().with_allowed_hosts([ALLOWED_HOST])),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Kernel as KaijutsuKernel;
    use crate::block_store::shared_block_store;
    use crate::runtime::context_engine::session_context_map;
    use crate::runtime::embedded_kaish::{EmbeddedKaish, ExternalExec, OutputProfile};
    use kaijutsu_types::{ContextId, PrincipalId, SessionId};
    use kaish_kernel::ExecuteOptions;
    use std::sync::Arc;

    /// Direct `ToolRegistry` lookup — the same registry
    /// `kj::context_shell`'s `configure_tools` closure hands `curl_tool()`
    /// to. Stands in for "a materialized context shell lists a `curl`
    /// tool": if the closure ever stops calling this function, `curl`
    /// disappears from every shell and this is the test that catches it at
    /// the source, without needing a full kernel to materialize one.
    #[test]
    fn curl_tool_registers_under_the_name_curl() {
        let mut registry = kaish_kernel::ToolRegistry::new();
        registry.register(curl_tool());
        assert!(
            registry.contains("curl"),
            "curl_tool() must register under the name \"curl\" — got: {:?}",
            registry.names()
        );
    }

    /// Build a throwaway `EmbeddedKaish` with only `curl_tool()` wired in —
    /// enough to run kaish source against it without pulling in the full
    /// `KjDispatcher` machinery `context_shell.rs` uses in production.
    async fn embedded_with_curl(name: &str) -> EmbeddedKaish {
        let principal = PrincipalId::system();
        let blocks = shared_block_store(principal);
        let kernel = Arc::new(KaijutsuKernel::new_ephemeral(name).await);
        let configure_tools =
            move |_scm, _sid: SessionId, tools: &mut kaish_kernel::ToolRegistry| {
                tools.register(curl_tool());
            };
        EmbeddedKaish::with_identity(
            name,
            blocks,
            kernel,
            None,
            principal,
            ContextId::new(),
            SessionId::new(),
            session_context_map(),
            ExternalExec::Deny,
            OutputProfile::Agent,
            configure_tools,
        )
        .expect("EmbeddedKaish init")
    }

    /// A host outside the allowlist is refused before any connection is
    /// attempted — no network needed for this test. Confirmed by reading
    /// `kaish-tools-curl`'s `backend/ureq.rs`: `config.permit_egress(...)` is
    /// the first thing checked inside the request loop, ahead of DNS
    /// resolution and the ureq call, so a denied host never reaches the
    /// network. The error names the policy that stopped it (exit 7,
    /// `CurlError::CouldNotConnect`, message contains "egress allowlist" —
    /// see kaish-extras' `tests/errors.rs` for the same assertion against
    /// the crate directly).
    #[tokio::test]
    async fn curl_is_refused_for_a_host_outside_the_allowlist() {
        let kaish = embedded_with_curl("test-curl-egress-denied").await;
        let r = kaish
            .execute_with_options("curl https://example.com/", ExecuteOptions::default())
            .await
            .unwrap();
        assert!(!r.ok(), "a non-allowlisted host must be refused: {}", r.err);
        assert_eq!(r.code, 7, "CouldNotConnect's exit code: {}", r.err);
        assert!(
            r.err.contains("egress allowlist"),
            "refusal must name the policy that stopped it: {}",
            r.err
        );
    }

    /// `-k` is a parse-time refusal: `insecure_permitted` is never turned on
    /// in our config, so the flag is rejected before egress is consulted.
    #[tokio::test]
    async fn insecure_flag_is_refused() {
        let kaish = embedded_with_curl("test-curl-insecure-refused").await;
        let r = kaish
            .execute_with_options("curl -k https://example.com/", ExecuteOptions::default())
            .await
            .unwrap();
        assert!(!r.ok(), "-k must be refused: {}", r.err);
        assert!(
            r.err.contains("is not permitted here"),
            "refusal must say the flag is not permitted: {}",
            r.err
        );
    }

    // The `--max-time` clamp (`req.max_time.min(limits.max_time)`) only
    // shortens the ureq timeout; it is observable only by timing a real hang,
    // so it has no unit test here.
}
