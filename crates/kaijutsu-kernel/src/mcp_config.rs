//! TOML-driven MCP server configuration.
//!
//! Parses `mcp.toml` to extract server definitions for pool registration.

use crate::mcp_pool::{McpForkMode, McpServerConfig, McpTransport};
use std::collections::HashMap;

/// Parsed MCP configuration containing server definitions.
#[derive(Debug, Clone)]
pub struct McpConfig {
    pub servers: Vec<McpServerConfig>,
}

// ---------------------------------------------------------------------------
// TOML parser
// ---------------------------------------------------------------------------

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

        #[serde(default = "default_transport")]
        pub transport: String,

        #[serde(default)]
        pub url: Option<String>,

        #[serde(default = "default_fork")]
        pub fork: String,
    }

    fn default_transport() -> String {
        "stdio".into()
    }

    fn default_fork() -> String {
        "share".into()
    }
}

fn default_true() -> bool {
    true
}

/// Parse an `mcp.toml` string into an `McpConfig`.
pub fn load_mcp_config_toml(content: &str) -> Result<McpConfig, String> {
    let raw: toml_types::McpToml =
        toml::from_str(content).map_err(|e| format!("mcp.toml parse error: {e}"))?;

    let mut servers = Vec::new();
    for (name, srv) in &raw.servers {
        if !srv.enabled {
            continue;
        }

        let transport = match srv.transport.as_str() {
            "streamable_http" => McpTransport::StreamableHttp,
            _ => McpTransport::Stdio,
        };

        let fork_mode = match srv.fork.as_str() {
            "instance" => McpForkMode::Instance,
            "exclude" => McpForkMode::Exclude,
            _ => McpForkMode::Share,
        };

        servers.push(McpServerConfig {
            name: name.clone(),
            command: srv.command.clone().unwrap_or_default(),
            args: srv.args.clone(),
            env: srv.env.clone(),
            cwd: srv.cwd.clone(),
            transport,
            url: srv.url.clone(),
            fork_mode,
        });
    }

    Ok(McpConfig { servers })
}

#[cfg(test)]
mod tests {
    use super::*;

    const DEFAULT_MCP_TOML: &str = include_str!("../../../assets/defaults/mcp.toml");

    #[test]
    fn test_default_mcp_toml_parses() {
        let config = load_mcp_config_toml(DEFAULT_MCP_TOML).unwrap();
        assert_eq!(config.servers.len(), 1);
        assert_eq!(config.servers[0].name, "bevy_brp");
        assert_eq!(config.servers[0].command, "bevy_brp_mcp");
    }

    #[test]
    fn test_stdio_server() {
        let toml = r##"
[servers.kaish]
command = "/usr/bin/kaish-mcp"
args = ["--stdio"]
"##;
        let config = load_mcp_config_toml(toml).unwrap();
        assert_eq!(config.servers.len(), 1);
        let s = &config.servers[0];
        assert_eq!(s.name, "kaish");
        assert_eq!(s.command, "/usr/bin/kaish-mcp");
        assert_eq!(s.args, vec!["--stdio"]);
        assert_eq!(s.transport, McpTransport::Stdio);
    }

    #[test]
    fn test_http_server() {
        let toml = r##"
[servers.holler]
transport = "streamable_http"
url = "http://localhost:8080"
"##;
        let config = load_mcp_config_toml(toml).unwrap();
        assert_eq!(config.servers.len(), 1);
        let s = &config.servers[0];
        assert_eq!(s.name, "holler");
        assert_eq!(s.transport, McpTransport::StreamableHttp);
        assert_eq!(s.url.as_deref(), Some("http://localhost:8080"));
    }

    #[test]
    fn test_disabled_server() {
        let toml = r##"
[servers.active]
command = "/bin/active"

[servers.disabled]
command = "/bin/disabled"
enabled = false
"##;
        let config = load_mcp_config_toml(toml).unwrap();
        assert_eq!(config.servers.len(), 1);
        assert_eq!(config.servers[0].name, "active");
    }

    #[test]
    fn test_env_and_cwd() {
        let toml = r##"
[servers.test]
command = "/bin/test"
cwd = "/work/dir"

[servers.test.env]
API_KEY = "secret"
DEBUG = "1"
"##;
        let config = load_mcp_config_toml(toml).unwrap();
        let s = &config.servers[0];
        assert_eq!(s.env.get("API_KEY").unwrap(), "secret");
        assert_eq!(s.env.get("DEBUG").unwrap(), "1");
        assert_eq!(s.cwd.as_deref(), Some("/work/dir"));
    }

    #[test]
    fn test_fork_modes() {
        let toml = r##"
[servers.brp]
command = "/bin/brp"
fork = "share"

[servers.kaish]
command = "/bin/kaish"
fork = "instance"

[servers.debugger]
command = "/bin/debugger"
fork = "exclude"

[servers.fallback]
command = "/bin/fallback"
"##;
        let config = load_mcp_config_toml(toml).unwrap();
        let find = |name: &str| config.servers.iter().find(|s| s.name == name).unwrap();

        assert_eq!(find("brp").fork_mode, McpForkMode::Share);
        assert_eq!(find("kaish").fork_mode, McpForkMode::Instance);
        assert_eq!(find("debugger").fork_mode, McpForkMode::Exclude);
        assert_eq!(find("fallback").fork_mode, McpForkMode::Share);
    }

    #[test]
    fn test_empty() {
        let config = load_mcp_config_toml("").unwrap();
        assert!(config.servers.is_empty());
    }

    #[test]
    fn test_parse_error() {
        let result = load_mcp_config_toml("[invalid");
        assert!(result.is_err());
    }
}
