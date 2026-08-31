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
//!
//! An `env` value that names a source it cannot resolve is the second tier:
//! that one server is dropped with a reason, so a missing key file never
//! launches a server with a blank credential and never takes its neighbors
//! down. Sources resolve here, at load — a running server keeps the
//! environment it was spawned with, exactly as it does for a literal.

use std::collections::HashMap;

use super::servers::external::{McpServerConfig, McpTransport};
use crate::secret_source::{read_secret_env, read_secret_file};

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

        /// Raw, unresolved: a value is either a literal string or a
        /// one-key source table. `resolve_env_value` interprets it.
        #[serde(default)]
        pub env: HashMap<String, toml::Value>,

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

        let env = match resolve_env(&srv.env) {
            Ok(env) => env,
            Err(reason) => {
                warnings.push(format!("server '{name}': {reason} — skipped"));
                invalid.push(InvalidServer { name: name.clone(), reason });
                continue;
            }
        };

        servers.push(McpServerConfig {
            name: name.clone(),
            command: srv.command.clone().unwrap_or_default(),
            args: srv.args.clone(),
            env,
            cwd: srv.cwd.clone(),
            transport,
            url: srv.url.clone(),
            call_timeout: srv.call_timeout_ms.map(std::time::Duration::from_millis),
        });
    }

    Ok(McpConfigLoad { servers, warnings, invalid })
}

/// Resolve every `env` value for one server, or name the first failure.
///
/// Deterministic: variables are visited in sorted order, so the same
/// malformed file always reports the same variable.
fn resolve_env(raw: &HashMap<String, toml::Value>) -> Result<HashMap<String, String>, String> {
    let mut names: Vec<&String> = raw.keys().collect();
    names.sort();

    let mut resolved = HashMap::with_capacity(names.len());
    for var in names {
        resolved.insert(var.clone(), resolve_env_value(var, &raw[var])?);
    }
    Ok(resolved)
}

/// Interpret one `env` value: a literal string, or a table naming exactly one
/// source.
///
/// ```toml
/// env.RUST_LOG      = "debug"                      # literal
/// env.KAIBO_API_KEY = { file = "~/.kaibo-key" }    # trimmed file contents
/// env.GITHUB_TOKEN  = { env = "GITHUB_TOKEN" }     # the kernel's own environment
/// ```
///
/// Errors name the variable and the source, never the resolved value, so they
/// are safe to log and to show a player.
fn resolve_env_value(var: &str, value: &toml::Value) -> Result<String, String> {
    let table = match value {
        toml::Value::String(literal) => return Ok(literal.clone()),
        toml::Value::Table(table) => table,
        _ => {
            return Err(format!(
                "env.{var} must be a string, or a table naming one source: \
                 {{ file = \"…\" }} or {{ env = \"…\" }}"
            ));
        }
    };

    if table.is_empty() {
        return Err(format!(
            "env.{var} is an empty table — name one source: \
             {{ file = \"…\" }} or {{ env = \"…\" }}"
        ));
    }
    if table.len() > 1 {
        let mut named: Vec<&str> = table.keys().map(String::as_str).collect();
        named.sort();
        return Err(format!(
            "env.{var} names {} sources ({}) — a source table takes exactly one",
            table.len(),
            named.join(", ")
        ));
    }

    let (source, argument) = table.iter().next().expect("table holds exactly one key");
    let argument = argument
        .as_str()
        .ok_or_else(|| format!("env.{var}: '{source}' must be a string"))?;

    match source.as_str() {
        "file" => read_secret_file(argument).map_err(|e| format!("env.{var}: {e}")),
        "env" => read_secret_env(argument).map_err(|e| format!("env.{var}: {e}")),
        "command" => Err(format!(
            "env.{var}: a command source is not implemented — use \
             {{ file = \"…\" }} or {{ env = \"…\" }}"
        )),
        other => Err(format!(
            "env.{var}: unknown source '{other}' — use file or env"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    const DEFAULT_MCP_TOML: &str = include_str!("../../../../assets/defaults/mcp.toml");

    /// The shipped default must parse clean **and** launch nothing.
    ///
    /// A seeded server entry spawns a real, unsandboxed subprocess in every
    /// kernel booted from this default — including every kernel a test boots,
    /// where it opened the developer's live `~/.local/state` (2026-08-15). The
    /// emptiness is the point, so it is what this asserts.
    #[test]
    fn default_mcp_toml_parses_and_launches_nothing() {
        let load = load_mcp_config_toml(DEFAULT_MCP_TOML).unwrap();
        assert!(load.warnings.is_empty(), "warnings: {:?}", load.warnings);
        assert!(
            load.servers.is_empty(),
            "the shipped mcp.toml must configure no servers — a seeded entry \
             spawns a host process in every fresh kernel, tests included; got {:?}",
            load.servers.iter().map(|s| &s.name).collect::<Vec<_>>()
        );
    }

    // ---- env value resolution -------------------------------------------

    #[test]
    fn a_literal_env_value_still_parses() {
        let load = load_mcp_config_toml(
            r#"
            [servers.a]
            command = "x"
            env = { RUST_LOG = "debug" }
            "#,
        )
        .unwrap();
        assert_eq!(load.servers[0].env["RUST_LOG"], "debug");
        assert!(load.invalid.is_empty());
    }

    #[test]
    fn a_file_source_is_read_and_trimmed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("kaibo-key");
        std::fs::write(&path, "  sk-from-file\n").unwrap();

        let toml = format!(
            r#"
            [servers.kaibo]
            command = "kaibo-mcp"
            env.KAIBO_API_KEY = {{ file = "{}" }}
            "#,
            path.to_str().unwrap()
        );
        let load = load_mcp_config_toml(&toml).unwrap();
        assert!(load.invalid.is_empty(), "{:?}", load.invalid);
        assert_eq!(load.servers[0].env["KAIBO_API_KEY"], "sk-from-file");
    }

    #[test]
    fn an_env_source_reads_the_kernels_own_environment() {
        // SAFETY: single-threaded test; unique var name avoids cross-test races.
        unsafe {
            std::env::set_var("KAIJUTSU_MCP_TOML_TEST_TOKEN", "sk-from-env");
        }
        let load = load_mcp_config_toml(
            r#"
            [servers.a]
            command = "x"
            env.TOKEN = { env = "KAIJUTSU_MCP_TOML_TEST_TOKEN" }
            "#,
        )
        .unwrap();
        assert!(load.invalid.is_empty(), "{:?}", load.invalid);
        assert_eq!(load.servers[0].env["TOKEN"], "sk-from-env");
        // SAFETY: single-threaded test cleanup.
        unsafe {
            std::env::remove_var("KAIJUTSU_MCP_TOML_TEST_TOKEN");
        }
    }

    /// The whole reason resolution happens here: an unresolvable secret is a
    /// per-entry failure, so the server is FAILED and visible rather than
    /// launched with a blank credential — and its neighbors still start.
    #[test]
    fn an_unresolvable_secret_fails_its_entry_not_the_file() {
        let load = load_mcp_config_toml(
            r#"
            [servers.broken]
            command = "x"
            env.TOKEN = { file = "/nonexistent/path/to/token" }

            [servers.healthy]
            command = "y"
            "#,
        )
        .unwrap();

        assert_eq!(load.servers.len(), 1, "the healthy neighbor must still launch");
        assert_eq!(load.servers[0].name, "healthy");
        assert_eq!(load.invalid.len(), 1);
        assert_eq!(load.invalid[0].name, "broken");
        assert!(load.invalid[0].reason.contains("env.TOKEN"), "{}", load.invalid[0].reason);
        assert!(
            load.invalid[0].reason.contains("/nonexistent/path/to/token"),
            "the reason must name the source: {}",
            load.invalid[0].reason
        );
        assert_eq!(load.warnings.len(), 1);
    }

    #[test]
    fn an_empty_secret_file_is_not_an_empty_env_var() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("blank");
        std::fs::write(&path, "   \n").unwrap();

        let toml = format!(
            r#"
            [servers.a]
            command = "x"
            env.TOKEN = {{ file = "{}" }}
            "#,
            path.to_str().unwrap()
        );
        let load = load_mcp_config_toml(&toml).unwrap();
        assert!(load.servers.is_empty(), "a blank credential must not launch a server");
        assert!(load.invalid[0].reason.contains("empty after trimming"));
    }

    #[test]
    fn an_unset_env_source_fails_the_entry() {
        let load = load_mcp_config_toml(
            r#"
            [servers.a]
            command = "x"
            env.TOKEN = { env = "KAIJUTSU_MCP_TOML_TEST_DEFINITELY_UNSET" }
            "#,
        )
        .unwrap();
        assert!(load.servers.is_empty());
        assert!(load.invalid[0].reason.contains("is not set"), "{}", load.invalid[0].reason);
    }

    /// `command` is deferred, not unknown — host process execution has one
    /// owner, so the message says what to use instead of falling into the
    /// generic unknown-source path.
    #[test]
    fn a_command_source_is_deferred_with_an_instruction() {
        let load = load_mcp_config_toml(
            r#"
            [servers.a]
            command = "x"
            env.TOKEN = { command = "pass show gh/token" }
            "#,
        )
        .unwrap();
        let reason = &load.invalid[0].reason;
        assert!(reason.contains("not implemented"), "{reason}");
        assert!(reason.contains("file"), "the message must name the alternatives: {reason}");
    }

    #[test]
    fn an_unknown_source_is_loud_not_ignored() {
        let load = load_mcp_config_toml(
            r#"
            [servers.a]
            command = "x"
            env.TOKEN = { fil = "~/.token" }
            "#,
        )
        .unwrap();
        assert!(load.servers.is_empty(), "a typo'd source must not silently drop the value");
        assert!(load.invalid[0].reason.contains("unknown source 'fil'"), "{}", load.invalid[0].reason);
    }

    #[test]
    fn two_sources_in_one_value_fail_the_entry() {
        let load = load_mcp_config_toml(
            r#"
            [servers.a]
            command = "x"
            env.TOKEN = { file = "~/.token", env = "TOKEN" }
            "#,
        )
        .unwrap();
        assert!(load.servers.is_empty());
        let reason = &load.invalid[0].reason;
        assert!(reason.contains("exactly one"), "{reason}");
        assert!(reason.contains("env, file"), "the reason must name both: {reason}");
    }

    #[test]
    fn a_non_string_env_value_fails_the_entry() {
        let load = load_mcp_config_toml(
            r#"
            [servers.a]
            command = "x"
            env.PORT = 8080
            "#,
        )
        .unwrap();
        assert!(load.servers.is_empty());
        assert!(load.invalid[0].reason.contains("env.PORT"), "{}", load.invalid[0].reason);
    }

    /// A failure message is logged and shown to players, so it must never
    /// quote the value it just read.
    #[test]
    fn a_failure_reason_never_carries_the_secret() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("token");
        std::fs::write(&path, "sk-super-secret-value").unwrap();

        // Resolvable file, but paired with a second source — the failure
        // happens after the value is reachable.
        let toml = format!(
            r#"
            [servers.a]
            command = "x"
            env.TOKEN = {{ file = "{}", env = "TOKEN" }}
            "#,
            path.to_str().unwrap()
        );
        let load = load_mcp_config_toml(&toml).unwrap();
        assert!(
            !load.invalid[0].reason.contains("sk-super-secret-value"),
            "the reason leaked the secret: {}",
            load.invalid[0].reason
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
