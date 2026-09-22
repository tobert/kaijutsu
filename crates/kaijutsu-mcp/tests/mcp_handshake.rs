//! The request shapes Claude Code sends when it connects, as raw JSON-RPC
//! lines over the server's stdio transport.
//!
//! Claude Code 2.1.278 writes a 2026-07-28 `server/discover` probe and, without
//! waiting for its answer, a legacy `initialize` on the same connection, then
//! lists tools, prompts, and resources with no `_meta`
//! (`claude_codes_pipelined_discover_then_initialize_lists_everything`,
//! recorded from a live `/mcp`). On rmcp 3.1.2 and
//! 3.4.0 the discover opener locks the session into the inline lifecycle, so
//! every legacy list call fails its `_meta` check and the session has no
//! kaijutsu tools. The typed rmcp client in `e2e_shell.rs` and one flow per
//! connection never showed this.

use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use kaijutsu_mcp::KaijutsuMcp;
use rmcp::ServiceExt;

fn request_meta() -> Value {
    json!({
        "io.modelcontextprotocol/protocolVersion": "2026-07-28",
        "io.modelcontextprotocol/clientCapabilities": {},
        "io.modelcontextprotocol/clientInfo": { "name": "handshake-test", "version": "0" },
    })
}

/// Serve a fresh in-memory `KaijutsuMcp` on one connection, send `lines`,
/// and return the responses keyed by request id, one per id-bearing line.
async fn exchange(lines: &[Value]) -> Vec<Value> {
    let (server_io, client_io) = tokio::io::duplex(1 << 20);
    tokio::spawn(async move {
        if let Ok(service) = KaijutsuMcp::new().serve(server_io).await {
            let _ = service.waiting().await;
        }
    });

    let (read, mut write) = tokio::io::split(client_io);
    for line in lines {
        write.write_all(format!("{line}\n").as_bytes()).await.unwrap();
    }
    write.flush().await.unwrap();

    let expected = lines.iter().filter(|l| l.get("id").is_some()).count();
    let mut reader = BufReader::new(read).lines();
    let mut responses = Vec::new();
    while responses.len() < expected {
        let next = tokio::time::timeout(Duration::from_secs(10), reader.next_line())
            .await
            .unwrap_or_else(|_| panic!("no response after {} of {expected}: {responses:#?}", responses.len()));
        let Some(line) = next.unwrap() else {
            panic!("server closed the connection after {} of {expected} responses: {responses:#?}", responses.len());
        };
        let value: Value = serde_json::from_str(&line).unwrap();
        if value.get("id").is_some() {
            responses.push(value);
        }
    }
    responses.sort_by_key(|r| r["id"].to_string());
    responses
}

fn assert_result(response: &Value, key: &str) {
    assert!(
        response.get("error").is_none() && response["result"].get(key).is_some(),
        "expected result.{key}, got {response:#}"
    );
}

/// Claude Code's connect, byte-for-byte in method, id, and params shape.
///
/// Ignored: rmcp 3.4.0 locks the session to the inline lifecycle on a
/// discover opener, which modelcontextprotocol/rust-sdk#1248 fixes and no
/// release carries yet. Claude Code sends this pipelined sequence only when
/// its discover probe times out, and `tests/mcp_startup.rs` holds startup
/// work behind the handshake so ours does not. Remove the ignore once an rmcp
/// release passes it.
#[tokio::test]
#[ignore = "needs rmcp with rust-sdk#1248 (discover opener stays bootstrap-neutral)"]
async fn claude_codes_pipelined_discover_then_initialize_lists_everything() {
    let client_info = json!({
        "name": "claude-code", "title": "Claude Code", "version": "2.1.278",
        "description": "Anthropic's agentic coding tool", "websiteUrl": "https://claude.com/claude-code",
    });
    let capabilities = json!({ "roots": { "listChanged": true }, "elicitation": {} });
    let responses = exchange(&[
        json!({
            "jsonrpc": "2.0", "id": "server-discover-probe-1", "method": "server/discover",
            "params": { "_meta": {
                "io.modelcontextprotocol/protocolVersion": "2026-07-28",
                "io.modelcontextprotocol/clientInfo": client_info,
                "io.modelcontextprotocol/clientCapabilities": capabilities,
            } },
        }),
        json!({
            "method": "initialize", "jsonrpc": "2.0", "id": 0,
            "params": { "protocolVersion": "2025-11-25", "capabilities": capabilities, "clientInfo": client_info },
        }),
        json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }),
        json!({ "method": "tools/list", "jsonrpc": "2.0", "id": 1 }),
        json!({ "method": "prompts/list", "jsonrpc": "2.0", "id": 2 }),
        json!({ "method": "resources/list", "jsonrpc": "2.0", "id": 3 }),
    ])
    .await;
    let by_id = |id: Value| responses.iter().find(|r| r["id"] == id).unwrap_or_else(|| panic!("no response for {id}"));
    assert_eq!(by_id(json!(0))["result"]["protocolVersion"], "2025-11-25", "{:#}", by_id(json!(0)));
    assert_result(by_id(json!(1)), "tools");
    assert_result(by_id(json!(2)), "prompts");
    assert_result(by_id(json!(3)), "resources");
}

#[tokio::test]
async fn the_discover_probe_is_answered_not_closed() {
    let responses = exchange(&[json!({
        "jsonrpc": "2.0", "id": 1, "method": "server/discover",
        "params": { "_meta": request_meta() },
    })])
    .await;
    assert_result(&responses[0], "supportedVersions");
    let versions = responses[0]["result"]["supportedVersions"].as_array().unwrap();
    assert!(versions.contains(&json!("2026-07-28")), "{versions:?}");
}

#[tokio::test]
async fn inline_list_calls_work_without_initialize() {
    let responses = exchange(&[
        json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list", "params": { "_meta": request_meta() } }),
        json!({ "jsonrpc": "2.0", "id": 2, "method": "resources/list", "params": { "_meta": request_meta() } }),
        json!({ "jsonrpc": "2.0", "id": 3, "method": "prompts/list", "params": { "_meta": request_meta() } }),
    ])
    .await;
    assert_result(&responses[0], "tools");
    assert_result(&responses[1], "resources");
    assert_result(&responses[2], "prompts");
}

#[tokio::test]
async fn legacy_initialize_then_list_calls_without_meta() {
    let responses = exchange(&[
        json!({
            "jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": {
                "protocolVersion": "2025-11-25", "capabilities": {},
                "clientInfo": { "name": "handshake-test", "version": "0" },
            },
        }),
        json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }),
        json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {} }),
        json!({ "jsonrpc": "2.0", "id": 3, "method": "resources/list", "params": {} }),
        json!({ "jsonrpc": "2.0", "id": 4, "method": "prompts/list", "params": {} }),
    ])
    .await;
    assert_eq!(responses[0]["result"]["protocolVersion"], "2025-11-25", "{:#}", responses[0]);
    assert_result(&responses[1], "tools");
    assert_result(&responses[2], "resources");
    assert_result(&responses[3], "prompts");
}
