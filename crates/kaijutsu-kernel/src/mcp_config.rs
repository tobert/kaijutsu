//! Rhai-driven MCP server configuration.
//!
//! Evaluates `mcp.rhai` scripts to extract server definitions.
//! Converts the Rhai scope into `Vec<McpServerConfig>` for pool registration.

use crate::mcp_pool::{McpServerConfig, McpTransport};
use std::collections::HashMap;

/// Parsed MCP configuration containing server definitions.
#[derive(Debug, Clone)]
pub struct McpConfig {
    pub servers: Vec<McpServerConfig>,
}

/// Parse an `mcp.rhai` script into an `McpConfig`.
///
/// The script should define a `servers` map where each key is the server name
/// and the value is a map of configuration fields:
///
/// ```rhai
/// let servers = #{
///     my_server: #{
///         command: "/path/to/server",
///         args: ["--stdio"],
///         env: #{ "KEY": "value" },
///         enabled: true,           // default: true
///         transport: "stdio",      // "stdio" (default) or "streamable_http"
///         url: "http://...",       // required for streamable_http
///         cwd: "/work/dir",        // optional
///     },
/// };
/// ```
pub fn load_mcp_config(script: &str) -> Result<McpConfig, String> {
    let engine = rhai::Engine::new();
    let ast = engine
        .compile(script)
        .map_err(|e| format!("mcp.rhai parse error: {e}"))?;
    let mut scope = rhai::Scope::new();
    engine
        .run_ast_with_scope(&mut scope, &ast)
        .map_err(|e| format!("mcp.rhai eval error: {e}"))?;
    let servers = extract_servers(&scope);
    Ok(McpConfig { servers })
}

fn extract_servers(scope: &rhai::Scope) -> Vec<McpServerConfig> {
    let servers_map = match scope.get_value::<rhai::Map>("servers") {
        Some(map) => map,
        None => return Vec::new(),
    };

    let mut configs = Vec::new();
    for (name, value) in &servers_map {
        let name = name.to_string();
        let Some(map) = value.clone().try_cast::<rhai::Map>() else {
            continue;
        };

        let enabled = map
            .get("enabled")
            .and_then(|v| v.as_bool().ok())
            .unwrap_or(true);
        if !enabled {
            continue;
        }

        let transport_str = map
            .get("transport")
            .and_then(|v| v.clone().into_string().ok())
            .map(|s| s.to_string())
            .unwrap_or_else(|| "stdio".to_string());

        let transport = match transport_str.as_str() {
            "streamable_http" => McpTransport::StreamableHttp,
            _ => McpTransport::Stdio,
        };

        let command = map
            .get("command")
            .and_then(|v| v.clone().into_string().ok())
            .map(|s| s.to_string())
            .unwrap_or_default();

        let url = map
            .get("url")
            .and_then(|v| v.clone().into_string().ok())
            .map(|s| s.to_string());

        let cwd = map
            .get("cwd")
            .and_then(|v| v.clone().into_string().ok())
            .map(|s| s.to_string());

        let args = map
            .get("args")
            .and_then(|v| v.clone().try_cast::<rhai::Array>())
            .map(|arr| {
                arr.into_iter()
                    .filter_map(|v| v.into_string().ok().map(|s| s.to_string()))
                    .collect()
            })
            .unwrap_or_default();

        let env: HashMap<String, String> = map
            .get("env")
            .and_then(|v| v.clone().try_cast::<rhai::Map>())
            .map(|m| {
                m.into_iter()
                    .filter_map(|(k, v)| v.into_string().ok().map(|s| (k.to_string(), s.to_string())))
                    .collect()
            })
            .unwrap_or_default();

        configs.push(McpServerConfig {
            name,
            command,
            args,
            env,
            cwd,
            transport,
            url,
        });
    }
    configs
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_stdio_server() {
        let script = r#"
            let servers = #{
                kaish: #{
                    command: "/usr/bin/kaish-mcp",
                    args: ["--stdio"],
                },
            };
        "#;
        let config = load_mcp_config(script).unwrap();
        assert_eq!(config.servers.len(), 1);
        let s = &config.servers[0];
        assert_eq!(s.name, "kaish");
        assert_eq!(s.command, "/usr/bin/kaish-mcp");
        assert_eq!(s.args, vec!["--stdio"]);
        assert_eq!(s.transport, McpTransport::Stdio);
        assert!(s.url.is_none());
    }

    #[test]
    fn test_parse_http_server() {
        let script = r#"
            let servers = #{
                holler: #{
                    transport: "streamable_http",
                    url: "http://localhost:8080",
                },
            };
        "#;
        let config = load_mcp_config(script).unwrap();
        assert_eq!(config.servers.len(), 1);
        let s = &config.servers[0];
        assert_eq!(s.name, "holler");
        assert_eq!(s.transport, McpTransport::StreamableHttp);
        assert_eq!(s.url.as_deref(), Some("http://localhost:8080"));
        assert!(s.command.is_empty());
    }

    #[test]
    fn test_disabled_server_excluded() {
        let script = r#"
            let servers = #{
                active: #{ command: "/bin/active" },
                disabled: #{ command: "/bin/disabled", enabled: false },
            };
        "#;
        let config = load_mcp_config(script).unwrap();
        assert_eq!(config.servers.len(), 1);
        assert_eq!(config.servers[0].name, "active");
    }

    #[test]
    fn test_env_and_cwd() {
        let script = r#"
            let servers = #{
                test: #{
                    command: "/bin/test",
                    env: #{ "API_KEY": "secret", "DEBUG": "1" },
                    cwd: "/work/dir",
                },
            };
        "#;
        let config = load_mcp_config(script).unwrap();
        let s = &config.servers[0];
        assert_eq!(s.env.get("API_KEY").unwrap(), "secret");
        assert_eq!(s.env.get("DEBUG").unwrap(), "1");
        assert_eq!(s.cwd.as_deref(), Some("/work/dir"));
    }

    #[test]
    fn test_empty_servers() {
        let script = "let servers = #{};";
        let config = load_mcp_config(script).unwrap();
        assert!(config.servers.is_empty());
    }

    #[test]
    fn test_no_servers_var() {
        let script = "let x = 42;";
        let config = load_mcp_config(script).unwrap();
        assert!(config.servers.is_empty());
    }

    #[test]
    fn test_parse_error() {
        let result = load_mcp_config("let x = ");
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("parse error"));
    }

    #[test]
    fn test_multiple_servers_mixed() {
        let script = r#"
            let servers = #{
                kaish: #{ command: "/bin/kaish-mcp" },
                gpal: #{ command: "/bin/gpal" },
                holler: #{
                    transport: "streamable_http",
                    url: "http://localhost:8080",
                },
                disabled: #{ command: "/bin/nope", enabled: false },
            };
        "#;
        let config = load_mcp_config(script).unwrap();
        assert_eq!(config.servers.len(), 3);
        let names: Vec<&str> = config.servers.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"kaish"));
        assert!(names.contains(&"gpal"));
        assert!(names.contains(&"holler"));
        assert!(!names.contains(&"disabled"));
    }
}
