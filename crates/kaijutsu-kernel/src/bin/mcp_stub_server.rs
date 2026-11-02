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
//! With `MCP_STUB_IMAGES` set, it advertises tools that return images in each
//! shape an MCP server can send one ([`IMAGE_TOOLS`]), for the image-attach
//! tests in `tests/external_mcp_stub.rs`.
//!
//! Not shipped as a user-facing tool — it exists only as a `cargo test`-time
//! `CARGO_BIN_EXE_mcp_stub_server` fixture, same package as its tests so the
//! env var is reliably set (cross-package `CARGO_BIN_EXE_*` isn't part of
//! Cargo's contract).

use base64::Engine as _;
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, ListToolsResult,
    PaginatedRequestParams, ResourceContents, ServerCapabilities, ServerConfig, Tool,
};
use rmcp::service::{RequestContext, RoleServer};
use rmcp::{ErrorData as McpError, ServerHandler, ServiceExt, transport::stdio};

#[derive(Clone, Default)]
struct StubServer {
    images: bool,
}

/// A 1x1 PNG.
const PNG: &[u8] = &[
    0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44, 0x52,
    0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1F, 0x15, 0xC4,
    0x89, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9C, 0x63, 0xF8, 0xCF, 0xC0, 0xF0,
    0x1F, 0x00, 0x05, 0x00, 0x01, 0xFF, 0x89, 0x99, 0x3D, 0x1D, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45,
    0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82,
];

/// The head of a JPEG: enough for the magic-number check.
const JPEG: &[u8] = &[0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10, b'J', b'F', b'I', b'F', 0x00, 0x01, 0xFF, 0xD9];

/// The tools `MCP_STUB_IMAGES` adds, each returning an image the way one kind
/// of MCP server sends it.
const IMAGE_TOOLS: &[&str] = &[
    "screenshot",
    "two_images",
    "embedded_png",
    "embedded_untyped",
    "mislabeled",
    "not_base64",
    "svg",
];

fn b64(bytes: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

fn image_result(tool: &str) -> Option<Vec<ContentBlock>> {
    Some(match tool {
        // A screenshot tool: a caption, then the typed image.
        "screenshot" => vec![ContentBlock::text("captured the screen"), ContentBlock::image(b64(PNG), "image/png")],
        "two_images" => vec![ContentBlock::image(b64(PNG), "image/png"), ContentBlock::image(b64(JPEG), "image/jpeg")],
        // An embedded resource whose blob is an image.
        "embedded_png" => vec![ContentBlock::resource(
            ResourceContents::blob(b64(PNG), "file:///tmp/screen.png").with_mime_type("image/png"),
        )],
        // The same with no MIME type: only the bytes say it is an image.
        "embedded_untyped" => vec![ContentBlock::resource(ResourceContents::blob(b64(PNG), "file:///tmp/screen"))],
        // Declared PNG, JPEG bytes.
        "mislabeled" => vec![ContentBlock::image(b64(JPEG), "image/png")],
        "not_base64" => vec![ContentBlock::image("%%% not base64 %%%", "image/png")],
        // A vector image: providers take raster images only.
        "svg" => vec![ContentBlock::image(b64(b"<svg xmlns='http://www.w3.org/2000/svg'/>"), "image/svg+xml")],
        _ => return None,
    })
}

impl ServerHandler for StubServer {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        let tools = if self.images {
            let schema = std::sync::Arc::new(serde_json::Map::from_iter([("type".to_string(), "object".into())]));
            IMAGE_TOOLS.iter().map(|name| Tool::new(*name, "returns an image", schema.clone())).collect()
        } else {
            Vec::new()
        };
        Ok(ListToolsResult::with_all_items(tools))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, McpError> {
        match image_result(&request.name).filter(|_| self.images) {
            Some(content) => Ok(CallToolResult::success(content).into()),
            None => Err(McpError::invalid_params(format!("no tool {}", request.name), None)),
        }
    }
}

#[tokio::main]
async fn main() {
    // With `MCP_STUB_PIDFILE` set, record this process's pid there so a test
    // can tell whether the server is still running.
    if let Some(path) = std::env::var_os("MCP_STUB_PIDFILE") {
        std::fs::write(&path, std::process::id().to_string()).expect("write the stub pidfile");
    }
    let service = StubServer { images: std::env::var_os("MCP_STUB_IMAGES").is_some() }
        .serve(stdio())
        .await
        .expect("stub MCP server failed to start");
    let _ = service.waiting().await;
}
