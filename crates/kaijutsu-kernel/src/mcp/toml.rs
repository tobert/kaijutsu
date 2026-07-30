//! `mcp.toml` → `Vec<McpServerConfig>` loader.
//!
//! Rebuilt from the deleted `mcp_config.rs` (see `git show
//! cc5cb05a^:crates/kaijutsu-kernel/src/mcp_config.rs` — the commit that ripped
//! out the old pool wholesale) and adapted to the Phase 2 `McpServerConfig`
//! (`mcp/servers/external.rs`):
//!
//! - Dropped the `fork` field. mcp.toml's header comment documented a
//!   "share"/"instance"/"exclude" fork behavior that was never wired to
//!   anything — `McpServerConfig` never carried a `fork_mode`, and nothing
//!   read `McpForkMode` outside the deleted pool. Rather than resurrect
//!   vestigial config, both the doc comment (`assets/defaults/mcp.toml`) and
//!   this loader drop it. If per-context fork behavior for external servers
//!   is wanted later, it's a fresh design against the current
//!   `ContextToolBinding` model, not a resurrection of the old enum.
//! - Added `call_timeout_ms`, the per-server `InstancePolicy::call_timeout`
//!   override (component 3 of `docs/external-mcp.md` — a kaibo consultation
//!   commonly runs 5-15 minutes, well past the kernel-wide 120s default).
//!
//! ## Two failure tiers
//!
//! Matches this project's "loud, not silent" doctrine (CLAUDE.md):
//!
//! - **Whole-file** parse failure (bad TOML syntax, or a field with a shape
//!   `serde`/`toml` can't coerce at all) is the caller's problem —
//!   `load_mcp_config_toml` returns `Err` and callers fall back to the
//!   embedded default, loudly logged (mirrors
//!   `kaijutsu-server::rpc::initialize_kernel_models`'s handling of
//!   `models.toml`).
//! - **Per-entry** semantic failure (stdio transport with no `command`,
//!   streamable_http with no `url`, an unrecognized `transport` string) does
//!   NOT fail the whole file — the entry is dropped from `servers` and a
//!   human-readable reason lands in `warnings`. One malformed
//!   `[servers.X]` table must not take every other configured server down
//!   with it, but it must never vanish silently either — callers are
//!   expected to log every warning at error level.

use super::servers::external::{McpServerConfig, McpTransport};

/// Outcome of parsing `mcp.toml`: the servers that parsed clean, plus a
/// warning per entry that was dropped for being individually malformed.
/// `McpServerConfig` doesn't derive `PartialEq` (nothing upstream of this
/// module compared configs before now), so this type doesn't either — tests
/// compare individual fields (`name`, `command`, `transport`, …) instead of
/// whole-struct/whole-`Vec` equality.
#[derive(Debug, Clone, Default)]
pub struct McpConfigLoad {
    pub servers: Vec<ParsedServer>,
    pub warnings: Vec<String>,
    /// Structured counterpart to `warnings`, one entry per dropped
    /// `[servers.X]` table. `warnings` is free text for logging;
    /// `invalid` exists so a caller that needs to know *which* server
    /// failed and *why* — `kj mcp list`, `Broker::external_mcp_failures`
    /// via `reconcile_with_toml` — doesn't have to string-match log lines.
    /// Before this field existed, a malformed entry was dropped here and
    /// never reached either of those, so it simply vanished instead of
    /// showing up as a failed server (CLAUDE.md: silent fallbacks are a
    /// mistake).
    pub invalid: Vec<InvalidServer>,
}

/// One entry from `mcp.toml` that failed to parse semantically (missing
/// `command`, unrecognized `transport`, …) — the structured (name, reason)
/// pair callers surface as an explicitly FAILED server rather than letting
/// it silently disappear.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvalidServer {
    pub name: String,
    pub reason: String,
}

/// A cleanly-parsed `[servers.X]` entry. Alias kept distinct from
/// `McpServerConfig` at call sites for readability — it's the same type.
pub type ParsedServer = McpServerConfig;

mod toml_types {
    use serde::Deserialize;
    use std::collections::HashMap;

    #[derive(Deserialize)]
    pub struct McpToml {
        #[serde(default)]
        pub servers: HashMap<String, ServerToml>,
    }

    #[derive(Deserialize)]
    pub struct ServerToml {
        #[serde(default = "super::default_true")]
        pub enabled: bool,

        #[serde(default)]
        pub command: Option<String>,

        #[serde(default)]
        pub args: Vec<String>,

        #[serde(default)]
        pub env: HashMap<String, String>,

        #[serde(default)]
        pub cwd: Option<String>,

        #[serde(default = "super::default_transport")]
        pub transport: String,

        #[serde(default)]
        pub url: Option<String>,

        #[serde(default)]
        pub call_timeout_ms: Option<u64>,
    }
}

fn default_true() -> bool {
    true
}

fn default_transport() -> String {
    "stdio".into()
}

/// Parse an `mcp.toml` string into servers + per-entry warnings.
///
/// `Err` only for whole-file failures (bad TOML syntax or a field whose shape
/// `serde` can't coerce — e.g. `args` given as a string instead of an array).
pub fn load_mcp_config_toml(content: &str) -> Result<McpConfigLoad, String> {
    let raw: toml_types::McpToml =
        toml::from_str(content).map_err(|e| format!("mcp.toml parse error: {e}"))?;

    let mut names: Vec<&String> = raw.servers.keys().collect();
    names.sort(); // deterministic order — HashMap iteration isn't, and both
    // `kj mcp list` and tests depend on stable ordering.

    let mut servers = Vec::new();
    let mut warnings = Vec::new();
    let mut invalid = Vec::new();

    for name in names {
        let srv = &raw.servers[name];
        if !srv.enabled {
            continue;
        }

        let transport = match srv.transport.as_str() {
            "stdio" => McpTransport::Stdio,
            "streamable_http" => McpTransport::StreamableHttp,
            other => {
                let reason = format!(
                    "unrecognized transport '{other}' (expected 'stdio' or 'streamable_http')"
                );
                warnings.push(format!("server '{name}': {reason} — skipped"));
                invalid.push(InvalidServer { name: name.clone(), reason });
                continue;
            }
        };

        match transport {
            McpTransport::Stdio => {
                let command = srv.command.as_deref().unwrap_or("").trim();
                if command.is_empty() {
                    let reason = "stdio transport requires a non-empty 'command'".to_string();
                    warnings.push(format!("server '{name}': {reason} — skipped"));
                    invalid.push(InvalidServer { name: name.clone(), reason });
                    continue;
                }
            }
            McpTransport::StreamableHttp => {
                let url = srv.url.as_deref().unwrap_or("").trim();
                if url.is_empty() {
                    let reason =
                        "streamable_http transport requires a non-empty 'url'".to_string();
                    warnings.push(format!("server '{name}': {reason} — skipped"));
                    invalid.push(InvalidServer { name: name.clone(), reason });
                    continue;
                }
            }
        }

        servers.push(McpServerConfig {
            name: name.clone(),
            command: srv.command.clone().unwrap_or_default(),
            args: srv.args.clone(),
            env: srv.env.clone(),
            cwd: srv.cwd.clone(),
            transport,
            url: srv.url.clone(),
            call_timeout: srv.call_timeout_ms.map(std::time::Duration::from_millis),
        });
    }

    Ok(McpConfigLoad { servers, warnings, invalid })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    const DEFAULT_MCP_TOML: &str = include_str!("../../../../assets/defaults/mcp.toml");

    #[test]
    fn default_mcp_toml_parses() {
        let load = load_mcp_config_toml(DEFAULT_MCP_TOML).unwrap();
        assert!(load.warnings.is_empty(), "warnings: {:?}", load.warnings);
        assert_eq!(load.servers.len(), 2);
        assert_eq!(load.servers[0].name, "bevy_brp");
        assert_eq!(load.servers[0].command, "bevy_brp_mcp");
        assert_eq!(load.servers[0].transport, McpTransport::Stdio);
        assert_eq!(load.servers[0].call_timeout, None);
        assert_eq!(load.servers[1].name, "kaibo");
        assert_eq!(load.servers[1].command, "/home/atobey/src/kaibo/target/debug/kaibo");
        assert_eq!(load.servers[1].args, vec!["--root", "/home/atobey/src/kaijutsu"]);
        assert_eq!(load.servers[1].transport, McpTransport::Stdio);
        assert_eq!(
            load.servers[1].call_timeout,
            Some(std::time::Duration::from_millis(900_000))
        );
    }

    #[test]
    fn stdio_server_parses() {
        let toml = r##"
[servers.kaibo]
command = "/home/atobey/src/kaibo/target/debug/kaibo"
args = ["--root", "/home/atobey/src/kaijutsu"]
"##;
        let load = load_mcp_config_toml(toml).unwrap();
        assert_eq!(load.servers.len(), 1);
        let s = &load.servers[0];
        assert_eq!(s.name, "kaibo");
        assert_eq!(s.command, "/home/atobey/src/kaibo/target/debug/kaibo");
        assert_eq!(s.args, vec!["--root", "/home/atobey/src/kaijutsu"]);
        assert_eq!(s.transport, McpTransport::Stdio);
    }

    #[test]
    fn http_server_parses() {
        let toml = r##"
[servers.holler]
transport = "streamable_http"
url = "http://localhost:8080"
"##;
        let load = load_mcp_config_toml(toml).unwrap();
        assert_eq!(load.servers.len(), 1);
        let s = &load.servers[0];
        assert_eq!(s.name, "holler");
        assert_eq!(s.transport, McpTransport::StreamableHttp);
        assert_eq!(s.url.as_deref(), Some("http://localhost:8080"));
    }

    #[test]
    fn disabled_server_is_excluded_without_a_warning() {
        let toml = r##"
[servers.active]
command = "/bin/active"

[servers.disabled]
command = "/bin/disabled"
enabled = false
"##;
        let load = load_mcp_config_toml(toml).unwrap();
        assert_eq!(load.servers.len(), 1);
        assert_eq!(load.servers[0].name, "active");
        assert!(
            load.warnings.is_empty(),
            "disabling a server is not a malformed entry"
        );
    }

    #[test]
    fn env_and_cwd_parse() {
        let toml = r##"
[servers.test]
command = "/bin/test"
cwd = "/work/dir"

[servers.test.env]
API_KEY = "secret"
DEBUG = "1"
"##;
        let load = load_mcp_config_toml(toml).unwrap();
        let s = &load.servers[0];
        assert_eq!(s.env.get("API_KEY").unwrap(), "secret");
        assert_eq!(s.env.get("DEBUG").unwrap(), "1");
        assert_eq!(s.cwd.as_deref(), Some("/work/dir"));
    }

    /// Component 3: the per-server QoS override that reaches `InstancePolicy`
    /// at registration.
    #[test]
    fn call_timeout_ms_parses_into_a_duration() {
        let toml = r##"
[servers.kaibo]
command = "/home/atobey/src/kaibo/target/debug/kaibo"
call_timeout_ms = 900000
"##;
        let load = load_mcp_config_toml(toml).unwrap();
        assert_eq!(load.servers[0].call_timeout, Some(Duration::from_millis(900_000)));
    }

    #[test]
    fn call_timeout_ms_absent_is_none() {
        let toml = r##"
[servers.plain]
command = "/bin/plain"
"##;
        let load = load_mcp_config_toml(toml).unwrap();
        assert_eq!(load.servers[0].call_timeout, None);
    }

    #[test]
    fn empty_file_parses_to_no_servers() {
        let load = load_mcp_config_toml("").unwrap();
        assert!(load.servers.is_empty());
        assert!(load.warnings.is_empty());
    }

    #[test]
    fn whole_file_syntax_error_is_a_hard_err() {
        let result = load_mcp_config_toml("[invalid");
        assert!(result.is_err());
    }

    // ---- Per-entry malformed handling: loud, not fatal ----------------

    #[test]
    fn stdio_with_empty_command_is_a_warning_not_a_hard_error() {
        let toml = r##"
[servers.broken]
command = ""

[servers.fine]
command = "/bin/fine"
"##;
        let load = load_mcp_config_toml(toml).unwrap();
        assert_eq!(load.servers.len(), 1, "the malformed entry is dropped");
        assert_eq!(load.servers[0].name, "fine", "the other entry still loads");
        assert_eq!(load.warnings.len(), 1);
        assert!(load.warnings[0].contains("broken"));
        assert!(load.warnings[0].contains("command"));
        assert_eq!(load.invalid.len(), 1);
        assert_eq!(load.invalid[0].name, "broken");
        assert!(load.invalid[0].reason.contains("command"));
    }

    #[test]
    fn stdio_with_no_command_at_all_is_a_warning() {
        let toml = r##"
[servers.broken]
"##;
        let load = load_mcp_config_toml(toml).unwrap();
        assert!(load.servers.is_empty());
        assert_eq!(load.warnings.len(), 1);
        assert_eq!(load.invalid.len(), 1);
        assert_eq!(load.invalid[0].name, "broken");
    }

    #[test]
    fn streamable_http_with_no_url_is_a_warning() {
        let toml = r##"
[servers.broken]
transport = "streamable_http"

[servers.fine]
command = "/bin/fine"
"##;
        let load = load_mcp_config_toml(toml).unwrap();
        assert_eq!(load.servers.len(), 1);
        assert_eq!(load.servers[0].name, "fine");
        assert_eq!(load.warnings.len(), 1);
        assert!(load.warnings[0].contains("broken"));
        assert!(load.warnings[0].contains("url"));
        assert_eq!(load.invalid.len(), 1);
        assert_eq!(load.invalid[0].name, "broken");
        assert!(load.invalid[0].reason.contains("url"));
    }

    #[test]
    fn unrecognized_transport_is_a_warning_not_a_silent_stdio_fallback() {
        // The deleted mcp_config.rs silently defaulted an unrecognized
        // `transport` string to Stdio (`_ => McpTransport::Stdio`). Silent
        // fallbacks are a mistake (CLAUDE.md) — an operator who typos
        // "streemable_http" should see it, not get a stdio spawn of
        // whatever `command` happens to be set (or isn't).
        let toml = r##"
[servers.broken]
transport = "carrier_pigeon"
command = "/bin/whatever"
"##;
        let load = load_mcp_config_toml(toml).unwrap();
        assert!(load.servers.is_empty());
        assert_eq!(load.warnings.len(), 1);
        assert!(load.warnings[0].contains("carrier_pigeon"));

        // Defect: dropping the entry at parse time with only a free-text
        // warning meant it never reached `reconcile_external_mcp_servers`
        // and so was never recorded in `Broker::external_mcp_failures` — to
        // `kj mcp list`/`status` it simply didn't exist. `invalid` is the
        // structured (name, reason) counterpart callers use to surface a
        // malformed entry as an explicitly FAILED server instead.
        assert_eq!(load.invalid.len(), 1);
        assert_eq!(load.invalid[0].name, "broken");
        assert!(load.invalid[0].reason.contains("carrier_pigeon"));
    }

    #[test]
    fn one_malformed_entry_does_not_take_down_the_rest() {
        let toml = r##"
[servers.a]
command = "/bin/a"

[servers.broken]
transport = "nonsense"

[servers.b]
command = "/bin/b"
"##;
        let load = load_mcp_config_toml(toml).unwrap();
        let names: Vec<&str> = load.servers.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, vec!["a", "b"]);
        assert_eq!(load.warnings.len(), 1);
        assert_eq!(load.invalid.len(), 1);
        assert_eq!(load.invalid[0].name, "broken");
    }

    #[test]
    fn server_order_is_deterministic() {
        let toml = r##"
[servers.zebra]
command = "/bin/z"
[servers.alpha]
command = "/bin/a"
[servers.mango]
command = "/bin/m"
"##;
        let load = load_mcp_config_toml(toml).unwrap();
        let names: Vec<&str> = load.servers.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, vec!["alpha", "mango", "zebra"]);
    }
}
