//! A minimal MCP stdio server for this crate's tests: newline-delimited
//! JSON-RPC 2.0 on stdin/stdout, one tool, no network.
//!
//! ```text
//! fixture_echo {"text": "hi"}  ->  "fixture echo: hi"
//! ```
//!
//! With `MCP_FIXTURE_PIDFILE` set, the process writes its pid there before
//! it answers anything, so a test can check that the process is gone after
//! the session that declared it ends. It exits when stdin closes.
//!
//! Built only with the `test-mock` feature; a release build does not carry it.

use std::io::{BufRead, Write};

use serde_json::{Value, json};

fn main() {
    if let Some(path) = std::env::var_os("MCP_FIXTURE_PIDFILE") {
        std::fs::write(&path, std::process::id().to_string()).expect("write the MCP fixture pidfile");
    }
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        let Ok(message) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        // A notification carries no id and gets no answer.
        let Some(id) = message.get("id").cloned() else {
            continue;
        };
        let method = message.get("method").and_then(Value::as_str).unwrap_or("");
        let params = message.get("params").cloned().unwrap_or(Value::Null);
        let reply = match answer(method, &params) {
            Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
            Err((code, text)) => {
                json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": text}})
            }
        };
        if writeln!(stdout, "{reply}").and_then(|()| stdout.flush()).is_err() {
            break;
        }
    }
}

fn answer(method: &str, params: &Value) -> Result<Value, (i64, String)> {
    match method {
        "initialize" => {
            let version = params
                .get("protocolVersion")
                .and_then(Value::as_str)
                .unwrap_or("2025-06-18");
            Ok(json!({
                "protocolVersion": version,
                "capabilities": {"tools": {}},
                "serverInfo": {"name": "mcp-fixture", "version": "0.0.0"},
            }))
        }
        "ping" => Ok(json!({})),
        "tools/list" => Ok(json!({
            "tools": [{
                "name": "fixture_echo",
                "description": "Echo the text back, prefixed with 'fixture echo: '.",
                "inputSchema": {
                    "type": "object",
                    "properties": {"text": {"type": "string"}},
                    "required": ["text"],
                },
            }],
        })),
        "tools/call" => {
            let name = params.get("name").and_then(Value::as_str).unwrap_or("");
            if name != "fixture_echo" {
                return Err((-32602, format!("unknown tool: {name}")));
            }
            let text = params
                .pointer("/arguments/text")
                .and_then(Value::as_str)
                .unwrap_or("");
            Ok(json!({
                "content": [{"type": "text", "text": format!("fixture echo: {text}")}],
                "isError": false,
            }))
        }
        other => Err((-32601, format!("method not found: {other}"))),
    }
}
