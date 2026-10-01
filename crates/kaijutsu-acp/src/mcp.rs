//! ACP `mcpServers` → the kernel's context MCP declaration.
//!
//! An ACP client declares MCP servers on `session/new`, `session/load`, and
//! `session/resume`. The bridge hands them to the kernel
//! (`declareContextMcpServers`), which starts them on the kernel host and
//! grants them to the session's context alone. Stdio is the only transport:
//! an `http` or `sse` server is refused by name rather than dropped. See
//! docs/acp.md, "Client-declared MCP servers".

use agent_client_protocol::schema::v1::McpServer;
use kaijutsu_client::ContextMcpServerDecl;

/// Translate a declaration, refusing a transport this agent does not
/// support. The error names the server.
pub fn declarations(servers: &[McpServer]) -> Result<Vec<ContextMcpServerDecl>, String> {
    servers
        .iter()
        .map(|server| match server {
            McpServer::Stdio(stdio) => {
                let command = stdio.command.to_str().ok_or_else(|| {
                    format!("MCP server '{}': command is not valid UTF-8", stdio.name)
                })?;
                Ok(ContextMcpServerDecl {
                    name: stdio.name.clone(),
                    command: command.to_string(),
                    args: stdio.args.clone(),
                    env: stdio.env.iter().map(|v| (v.name.clone(), v.value.clone())).collect(),
                })
            }
            McpServer::Http(http) => Err(unsupported(&http.name, "http")),
            McpServer::Sse(sse) => Err(unsupported(&sse.name, "sse")),
            _ => Err("an MCP server uses a transport this agent does not support; declare it as stdio"
                .to_string()),
        })
        .collect()
}

fn unsupported(name: &str, transport: &str) -> String {
    format!(
        "MCP server '{name}' uses the {transport} transport, which this agent does not \
         support; declare it as a stdio server"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_client_protocol::schema::v1::{EnvVariable, McpServerHttp, McpServerStdio};

    #[test]
    fn stdio_servers_carry_command_args_and_env() {
        let server = McpServer::Stdio(
            McpServerStdio::new("fixture", "/usr/bin/fixture")
                .args(vec!["--flag".into()])
                .env(vec![EnvVariable::new("KEY", "value")]),
        );
        assert_eq!(
            declarations(&[server]).unwrap(),
            vec![ContextMcpServerDecl {
                name: "fixture".into(),
                command: "/usr/bin/fixture".into(),
                args: vec!["--flag".into()],
                env: vec![("KEY".into(), "value".into())],
            }]
        );
    }

    #[test]
    fn an_http_server_is_refused_by_name() {
        let server = McpServer::Http(McpServerHttp::new("remote", "http://127.0.0.1:9/mcp"));
        let err = declarations(&[server]).unwrap_err();
        assert!(err.contains("'remote'") && err.contains("http"), "{err}");
    }
}
