//! The kaijutsu-wide `curl` tool: one configuration per materialized shell.
//!
//! `runtime::context_shell` registers this configuration for contextual
//! shells, built from the calling context's `context_egress` rows
//! (`docs/egress.md`). Read-only construction replaces the tool with a
//! refusal; network calls use the writable shell.

use kaish_tools_curl::{AllowAll, AllowByList, CurlConfig, CurlTool, Limits};

/// The classifier host every context reaches regardless of its own egress
/// list — `docs/egress.md`, "The classifier host". A context with an empty
/// list must still be able to call the lfm2d pre-call hook.
const ALLOWED_HOST: &str = "lfm2d-1.taila4abc.ts.net";

/// Per-request wall-clock ceiling, in seconds. Kept below
/// `TimeoutPolicy::hook_body_timeout` (15s, `kaijutsu-types/src/timeout.rs`)
/// so a slow request fails as a curl timeout (exit 28) inside the hook
/// body's own budget, rather than the hook body's outer timeout firing
/// first and hiding what actually happened.
const MAX_TIME_SECS: f64 = 10.0;

/// Build the `curl` tool a context's shell registers, gated by `hosts` — the
/// context's `context_egress` rows (`docs/egress.md`, "The rule").
///
/// `*` present grants every host, loopback included. Otherwise the
/// allowlist is [`ALLOWED_HOST`] plus `hosts`, and loopback addresses open
/// only when `hosts` names a loopback literal (`localhost`, `127.0.0.1`,
/// `::1`) — a DNS name that happens to resolve to loopback stays refused,
/// matching `docs/egress.md`'s "A DNS name that resolves to a loopback ...
/// address is refused unless the list also holds a loopback literal"
/// paragraph. `-k`/`--insecure` stays refused
/// (`CurlConfig::default().insecure_permitted()` is `false` and this
/// function does not turn it on).
pub fn curl_tool(hosts: &[String]) -> CurlTool {
    let limits = Limits {
        max_time: MAX_TIME_SECS,
        ..Limits::default()
    };
    if hosts.iter().any(|h| h == "*") {
        return kaish_tools_curl::tool(
            CurlConfig::default()
                .with_limits(limits)
                .with_allow_egress(AllowAll),
        );
    }
    let allow_loopback = hosts.iter().any(|h| is_loopback_literal(h));
    let mut allowed = vec![ALLOWED_HOST.to_string()];
    allowed.extend(hosts.iter().cloned());
    kaish_tools_curl::tool(
        CurlConfig::default()
            .with_limits(limits)
            .with_allow_egress(
                AllowByList::new()
                    .with_allowed_hosts(allowed)
                    .with_allow_loopback(allow_loopback),
            ),
    )
}

/// Whether `host` is `localhost` or a loopback address, the same set
/// `kernel_db::validate_egress_host` accepts as a loopback literal.
fn is_loopback_literal(host: &str) -> bool {
    host == "localhost" || host.parse::<std::net::IpAddr>().is_ok_and(|ip| ip.is_loopback())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every loopback form the validator stores opens loopback, not only the
    /// three common spellings.
    #[test]
    fn every_loopback_address_counts_as_a_loopback_literal() {
        for host in ["localhost", "127.0.0.1", "127.0.0.2", "::1"] {
            assert!(is_loopback_literal(host), "{host}");
        }
        for host in ["crates.io", "*", "10.0.0.1"] {
            assert!(!is_loopback_literal(host), "{host}");
        }
    }
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
        registry.register(curl_tool(&[]));
        assert!(
            registry.contains("curl"),
            "curl_tool() must register under the name \"curl\" — got: {:?}",
            registry.names()
        );
    }

    /// Build a throwaway `EmbeddedKaish` with only `curl_tool()` wired in —
    /// enough to run kaish source against it without pulling in the full
    /// `KjDispatcher` machinery `context_shell.rs` uses in production.
    /// `hosts` stands in for the calling context's `context_egress` rows.
    async fn embedded_with_curl(name: &str, hosts: &[&str]) -> EmbeddedKaish {
        let principal = PrincipalId::system();
        let blocks = shared_block_store(principal);
        let kernel = Arc::new(KaijutsuKernel::new_ephemeral(name).await);
        let hosts: Vec<String> = hosts.iter().map(|h| h.to_string()).collect();
        let configure_tools =
            move |_scm, _sid: SessionId, tools: &mut kaish_kernel::ToolRegistry| {
                tools.register(curl_tool(&hosts));
            };
        EmbeddedKaish::with_identity(
            name,
            blocks,
            kernel,
            None,
            crate::runtime::context_shell::ShellIdentity { requester: principal, performer: principal, reviewer: None, context: ContextId::new(), session: SessionId::new() },
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
        let kaish = embedded_with_curl("test-curl-egress-denied", &[]).await;
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
    ///
    /// **The host here is deliberate.** `example.com` is NOT
    /// [`ALLOWED_HOST`] — it is the same non-allowlisted host
    /// `curl_is_refused_for_a_host_outside_the_allowlist` uses to assert the
    /// egress refusal. Same URL, two different refusal reasons, so this test
    /// passes only while flag parsing runs BEFORE the allowlist: if that
    /// order ever flipped, this would see "egress allowlist" instead. The
    /// ordering coverage was accidental when written (the host was picked
    /// only for obviously not being allowlisted); it is load-bearing now, so
    /// do not "simplify" this to an allowlisted host.
    #[tokio::test]
    async fn insecure_flag_is_refused() {
        let kaish = embedded_with_curl("test-curl-insecure-refused", &[]).await;
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
