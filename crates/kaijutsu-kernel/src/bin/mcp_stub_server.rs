//! Minimal MCP stdio server — a real-handshake fixture for `kaijutsu-kernel`'s
//! own test suite (`mcp::external_registry`'s reconcile tests, and
//! `kaijutsu-server`'s boot-integration tests), standing in for a real
//! external server (kaibo, bevy_brp) so `ExternalMcpServer::connect()` gets
//! exercised end to end — spawn, rmcp handshake, `tools/list` — without
//! depending on either being installed, and without touching the shared
//! systemd `kaijutsu-server` other agents may be using.
//!
//! `StubServer` implements no methods of its own: every `ServerHandler`
//! method is the crate's own default (`ServerInfo::default()`, an empty tool
//! list, `method_not_found` for anything else), which is exactly what
//! `connect()` needs (spawn → handshake → `list_all_tools` → empty `Vec`).
//!
//! Not shipped as a user-facing tool — it exists only as a `cargo test`-time
//! `CARGO_BIN_EXE_mcp_stub_server` fixture, same package as its tests so the
//! env var is reliably set (cross-package `CARGO_BIN_EXE_*` isn't part of
//! Cargo's contract).

use rmcp::{ServerHandler, ServiceExt, transport::stdio};

#[derive(Clone, Default)]
struct StubServer;

impl ServerHandler for StubServer {}

#[tokio::main]
async fn main() {
    let service = StubServer
        .serve(stdio())
        .await
        .expect("stub MCP server failed to start");
    let _ = service.waiting().await;
}
