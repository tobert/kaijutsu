//! Images from an external MCP server reach the model as image blocks, never
//! as base64 text.
//!
//! `mcp_stub_server` with `MCP_STUB_IMAGES` set returns an image in each shape
//! an MCP server can send one: a typed `image` content item, an embedded
//! resource whose blob is an image (with and without a MIME type), and the
//! cases the kernel refuses. An accepted image lands in the CAS and as an
//! Asset image block in the calling context, the way `view_image` attaches
//! one; the tool result keeps a short line. A refused image leaves a short
//! reason. Neither leaves the base64 in the tool result.

use std::str::FromStr;
use std::sync::Arc;

use base64::Engine as _;
use kaijutsu_cas::{ContentHash, ContentStore};
use kaijutsu_kernel::Kernel;
use kaijutsu_kernel::mcp::{
    CallContext, Capability, ContextMcpServerSpec, ContextToolBinding, InstanceId, KernelCallParams,
    KernelToolResult, ToolContent, external_instance_id,
};
use kaijutsu_types::{BlockSnapshot, ContentType, ContextId, DocKind, Role, SessionId};
use tokio_util::sync::CancellationToken;

/// The stub's 1x1 PNG (`src/bin/mcp_stub_server.rs`).
const PNG: &[u8] = &[
    0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44, 0x52,
    0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1F, 0x15, 0xC4,
    0x89, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9C, 0x63, 0xF8, 0xCF, 0xC0, 0xF0,
    0x1F, 0x00, 0x05, 0x00, 0x01, 0xFF, 0x89, 0x99, 0x3D, 0x1D, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45,
    0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82,
];

/// The stub's JPEG head.
const JPEG: &[u8] = &[0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10, b'J', b'F', b'I', b'F', 0x00, 0x01, 0xFF, 0xD9];

fn b64(bytes: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

fn stub_spec() -> ContextMcpServerSpec {
    ContextMcpServerSpec {
        name: "shots".to_string(),
        command: env!("CARGO_BIN_EXE_mcp_stub_server").to_string(),
        args: Vec::new(),
        env: vec![("MCP_STUB_IMAGES".to_string(), "1".to_string())],
    }
}

/// A kernel and a context with a document, its image server declared.
async fn context_with_stub() -> (Arc<Kernel>, ContextId, InstanceId) {
    let kernel = Arc::new(Kernel::new_ephemeral("test").await);
    let context = ContextId::new();
    kernel.blocks().create_document(context, DocKind::Conversation, None).expect("create the document");
    let ids = kernel
        .context_mcp()
        .declare(&kernel, context, SessionId::new(), vec![stub_spec()])
        .await
        .expect("declare the stub");
    (kernel, context, ids[0].clone())
}

async fn call(kernel: &Kernel, context: ContextId, instance: &InstanceId, tool: &str) -> KernelToolResult {
    let mut ctx = CallContext::test();
    ctx.context_id = context;
    let params = KernelCallParams {
        instance: instance.clone(),
        tool: tool.to_string(),
        arguments: serde_json::json!({}),
    };
    kernel.broker().call_tool(params, &ctx, CancellationToken::new()).await.expect("the call returns")
}

fn result_text(result: &KernelToolResult) -> String {
    result
        .content
        .iter()
        .map(|c| match c {
            ToolContent::Text(s) => s.clone(),
            ToolContent::Json(v) => v.to_string(),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// The Asset image blocks in `context`, in log order.
fn image_blocks(kernel: &Kernel, context: ContextId) -> Vec<BlockSnapshot> {
    kernel
        .blocks()
        .block_snapshots(context)
        .expect("read the blocks")
        .into_iter()
        .filter(|b| b.role == Role::Asset && b.content_type == ContentType::Image)
        .collect()
}

/// The stored bytes and MIME type of an image block.
fn stored(kernel: &Kernel, block: &BlockSnapshot) -> (Vec<u8>, String) {
    let hash = ContentHash::from_str(&block.content).expect("an image block holds a CAS hash");
    let bytes = kernel.cas().retrieve(&hash).unwrap().expect("the bytes are in the CAS");
    let mime = kernel.cas().inspect(&hash).unwrap().expect("the CAS records the type").mime_type;
    (bytes, mime)
}

/// No image's base64 survives in the result, and the result stays short.
fn assert_no_base64(result: &KernelToolResult) {
    let text = result_text(result);
    for payload in [b64(PNG), b64(JPEG)] {
        assert!(!text.contains(&payload), "the base64 reached the tool result: {text}");
    }
    assert!(text.len() < 600, "the tool result should be a few short lines, got {} bytes: {text}", text.len());
}

#[tokio::test]
async fn a_typed_image_becomes_an_image_block_and_a_short_line() {
    let (kernel, context, instance) = context_with_stub().await;
    let result = call(&kernel, context, &instance, "screenshot").await;

    assert!(!result.is_error, "{}", result_text(&result));
    assert!(result_text(&result).contains("captured the screen"), "the caption stays: {}", result_text(&result));
    assert_no_base64(&result);
    let images = image_blocks(&kernel, context);
    assert_eq!(images.len(), 1, "one image block");
    assert_eq!(stored(&kernel, &images[0]), (PNG.to_vec(), "image/png".to_string()));
    assert!(result_text(&result).contains(&images[0].content), "the line names the image: {}", result_text(&result));

    // The model's conversation carries it as an image, not text.
    let blocks = kernel.blocks().block_snapshots(context).unwrap();
    let hydrated = serde_json::to_string(&kaijutsu_kernel::llm::hydrate_from_blocks(&blocks)).unwrap();
    assert!(
        hydrated.contains(&images[0].content) && hydrated.contains("\"Image\""),
        "the conversation carries an image content block: {hydrated}"
    );
}

#[tokio::test]
async fn two_images_become_two_blocks_in_order() {
    let (kernel, context, instance) = context_with_stub().await;
    let result = call(&kernel, context, &instance, "two_images").await;

    assert_no_base64(&result);
    let images = image_blocks(&kernel, context);
    let stored: Vec<_> = images.iter().map(|b| stored(&kernel, b)).collect();
    assert_eq!(
        stored,
        vec![(PNG.to_vec(), "image/png".to_string()), (JPEG.to_vec(), "image/jpeg".to_string())]
    );
}

#[tokio::test]
async fn an_embedded_image_resource_becomes_an_image_block() {
    let (kernel, context, instance) = context_with_stub().await;
    let result = call(&kernel, context, &instance, "embedded_png").await;

    assert_no_base64(&result);
    let images = image_blocks(&kernel, context);
    assert_eq!(images.len(), 1, "one image block: {}", result_text(&result));
    assert_eq!(stored(&kernel, &images[0]), (PNG.to_vec(), "image/png".to_string()));
}

/// An embedded blob with no MIME type is an image when its bytes are one.
#[tokio::test]
async fn an_untyped_embedded_blob_with_image_bytes_becomes_an_image_block() {
    let (kernel, context, instance) = context_with_stub().await;
    let result = call(&kernel, context, &instance, "embedded_untyped").await;

    assert_no_base64(&result);
    let images = image_blocks(&kernel, context);
    assert_eq!(images.len(), 1, "one image block: {}", result_text(&result));
    assert_eq!(stored(&kernel, &images[0]), (PNG.to_vec(), "image/png".to_string()));
}

/// Each refusal leaves no block, no base64, and a reason the model can read.
#[tokio::test]
async fn a_refused_image_leaves_a_reason_and_no_block() {
    let (kernel, context, instance) = context_with_stub().await;
    for (tool, reason) in [("mislabeled", "image/jpeg"), ("not_base64", "base64"), ("svg", "image/svg+xml")] {
        let result = call(&kernel, context, &instance, tool).await;
        assert_no_base64(&result);
        let text = result_text(&result);
        assert!(text.contains("not attached") && text.contains(reason), "{tool}: {text}");
    }
    assert!(image_blocks(&kernel, context).is_empty(), "no refused image lands");
}

/// A server from `mcp.toml` attaches images too.
#[tokio::test]
async fn a_server_from_the_config_attaches_images() {
    let kernel = Arc::new(Kernel::new_ephemeral("test").await);
    let stub = env!("CARGO_BIN_EXE_mcp_stub_server");
    let toml = format!(
        r#"
[servers.shots]
command = "{stub}"
env = {{ MCP_STUB_IMAGES = "1" }}
"#
    );
    let report = kaijutsu_kernel::mcp::external_registry::reconcile_with_toml(&kernel, &toml).await;
    assert_eq!(report.added, vec!["shots".to_string()], "failed: {:?}", report.failed);

    let context = ContextId::new();
    kernel.blocks().create_document(context, DocKind::Conversation, None).expect("create the document");
    let mut all = ContextToolBinding::new();
    all.grant(Capability::AllInstances);
    kernel.broker().set_binding(context, all).await.unwrap();

    let result = call(&kernel, context, &external_instance_id("shots"), "screenshot").await;
    assert_no_base64(&result);
    let images = image_blocks(&kernel, context);
    assert_eq!(images.len(), 1, "one image block: {}", result_text(&result));
    assert_eq!(stored(&kernel, &images[0]), (PNG.to_vec(), "image/png".to_string()));
}
