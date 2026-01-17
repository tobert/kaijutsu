//! LLM integration for kaijutsu-app.
//!
//! Handles sending prompts to Claude and streaming responses into cells.
//! Uses a dedicated tokio runtime for API calls since anthropic-api requires tokio.
//!
//! ## Content Block Support
//!
//! Responses from Claude are parsed into structured content blocks:
//! - Thinking blocks (extended thinking/reasoning)
//! - Text blocks (main response)
//! - Tool use blocks (when tools are enabled)
//!
//! These blocks are stored in the CellEditor for structured display.

use bevy::prelude::*;
use kaijutsu_kernel::{
    AnthropicProvider, CompletionRequest, CompletionResponse, LlmMessage, LlmProvider, ResponseBlock,
};
use std::sync::Arc;
use std::thread;
use tokio::sync::mpsc;

use crate::cell::{Cell, CellEditor, CellKind, CellPosition, CellState, ContentBlock, PromptSubmitted};
use crate::text::{GlyphonText, TextAreaConfig};

/// Plugin for LLM integration.
pub struct LlmPlugin;

impl Plugin for LlmPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<LlmState>()
            .add_message::<LlmResponseComplete>()
            .add_systems(Startup, configure_llm_from_env)
            .add_systems(
                Update,
                (
                    handle_prompt_for_llm,
                    poll_llm_results,
                ),
            );
    }
}

/// Startup system to configure LLM provider from environment.
fn configure_llm_from_env(mut llm_state: ResMut<LlmState>) {
    if std::env::var("ANTHROPIC_API_KEY").is_ok() {
        info!("Found ANTHROPIC_API_KEY, initializing Claude provider");
        let provider = Arc::new(AnthropicProvider::from_env());

        // Create channels for communication
        let (request_tx, request_rx) = mpsc::unbounded_channel::<LlmRequest>();
        let (result_tx, result_rx) = mpsc::unbounded_channel::<LlmResult>();

        // Spawn a dedicated thread with tokio runtime for LLM calls
        thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("Failed to create tokio runtime for LLM");

            rt.block_on(llm_worker(request_rx, result_tx));
        });

        llm_state.provider = Some(provider);
        llm_state.request_tx = Some(request_tx);
        llm_state.result_rx = Some(result_rx);
    } else {
        warn!("ANTHROPIC_API_KEY not set - LLM features disabled. Set the environment variable to enable Claude integration.");
    }
}

/// Worker task that processes LLM requests in a tokio context.
async fn llm_worker(
    mut request_rx: mpsc::UnboundedReceiver<LlmRequest>,
    result_tx: mpsc::UnboundedSender<LlmResult>,
) {
    while let Some(req) = request_rx.recv().await {
        let result = req.provider.complete(req.request).await;

        let _ = result_tx.send(LlmResult {
            cell_entity: req.cell_entity,
            result: result.map_err(|e| format!("LLM error: {}", e)),
        });
    }
    info!("LLM worker shutting down");
}

/// Result from an LLM completion.
struct LlmResult {
    cell_entity: Entity,
    result: Result<CompletionResponse, String>,
}

/// Convert a kernel ResponseBlock to an app ContentBlock.
fn convert_response_block(block: &ResponseBlock) -> ContentBlock {
    match block {
        ResponseBlock::Thinking { thinking, .. } => ContentBlock::Thinking {
            text: thinking.clone(),
            collapsed: true, // Auto-collapse thinking when complete
        },
        ResponseBlock::Text { text } => ContentBlock::Text(text.clone()),
        ResponseBlock::ToolUse { id, name, input } => ContentBlock::ToolUse {
            id: id.clone(),
            name: name.clone(),
            input: input.clone(),
        },
        ResponseBlock::ToolResult {
            tool_use_id,
            content,
            is_error,
        } => ContentBlock::ToolResult {
            tool_use_id: tool_use_id.clone(),
            content: content.clone(),
            is_error: *is_error,
        },
    }
}

/// Request to send to the LLM worker.
struct LlmRequest {
    cell_entity: Entity,
    request: CompletionRequest,
    provider: Arc<dyn LlmProvider>,
}

/// State for LLM operations.
#[derive(Resource)]
pub struct LlmState {
    /// The LLM provider (None until initialized).
    provider: Option<Arc<dyn LlmProvider>>,
    /// Channel to receive completed LLM responses.
    result_rx: Option<mpsc::UnboundedReceiver<LlmResult>>,
    /// Channel to send LLM requests (held by tokio runtime).
    request_tx: Option<mpsc::UnboundedSender<LlmRequest>>,
    /// Default model to use.
    pub default_model: String,
    /// System prompt for the assistant.
    pub system_prompt: String,
}

impl Default for LlmState {
    fn default() -> Self {
        Self {
            provider: None,
            result_rx: None,
            request_tx: None,
            default_model: "claude-haiku-4-5-20251001".to_string(),
            system_prompt: "You are a helpful AI assistant in a collaborative coding environment called Kaijutsu. Be concise and helpful.".to_string(),
        }
    }
}

/// Message fired when an LLM response is complete.
#[derive(Message)]
pub struct LlmResponseComplete {
    pub cell_entity: Entity,
    pub success: bool,
    pub error: Option<String>,
}

/// System to handle prompts and send them to the LLM.
fn handle_prompt_for_llm(
    mut commands: Commands,
    mut submit_events: MessageReader<PromptSubmitted>,
    llm_state: Res<LlmState>,
    cells: Query<&CellPosition, With<Cell>>,
) {
    let (Some(provider), Some(request_tx)) = (llm_state.provider.clone(), llm_state.request_tx.as_ref()) else {
        // No provider or channel configured, skip
        return;
    };

    for event in submit_events.read() {
        let prompt_text = event.text.clone();

        if prompt_text.trim().is_empty() {
            continue;
        }

        info!("Sending prompt to LLM: {}...", &prompt_text.chars().take(50).collect::<String>());

        // Find the next row for the agent response
        let next_row = cells
            .iter()
            .filter(|p| p.col == 0 && p.row != u32::MAX)
            .map(|p| p.row)
            .max()
            .map(|max| max.saturating_add(1))
            .unwrap_or(0);

        // Create agent message cell with thinking indicator
        let cell = Cell::new(CellKind::AgentMessage);
        let cell_entity = commands
            .spawn((
                cell,
                CellEditor::default().with_text("⏳ Thinking..."),
                CellState::new(),
                CellPosition::new(0, next_row),
                GlyphonText,
                TextAreaConfig::default(),
            ))
            .id();

        // Build the request
        let request = CompletionRequest::new(
            &llm_state.default_model,
            vec![LlmMessage::user(&prompt_text)],
        )
        .with_system(&llm_state.system_prompt)
        .with_max_tokens(4096);

        // Send to worker thread
        if let Err(e) = request_tx.send(LlmRequest {
            cell_entity,
            request,
            provider: provider.clone(),
        }) {
            error!("Failed to send LLM request: {}", e);
        }
    }
}

/// System to poll for completed LLM results and update cells.
fn poll_llm_results(
    mut llm_state: ResMut<LlmState>,
    mut complete_events: MessageWriter<LlmResponseComplete>,
    mut editors: Query<&mut CellEditor>,
) {
    let Some(result_rx) = llm_state.result_rx.as_mut() else {
        return;
    };

    // Drain all available results (non-blocking)
    while let Ok(llm_result) = result_rx.try_recv() {
        match llm_result.result {
            Ok(response) => {
                let block_count = response.blocks.len();
                let has_thinking = response.has_thinking();
                info!(
                    "LLM response received ({} chars, {} blocks, thinking: {}) for entity {:?}",
                    response.content.len(),
                    block_count,
                    has_thinking,
                    llm_result.cell_entity
                );

                match editors.get_mut(llm_result.cell_entity) {
                    Ok(mut editor) => {
                        // Convert response blocks to content blocks
                        let content_blocks: Vec<ContentBlock> = response
                            .blocks
                            .iter()
                            .map(convert_response_block)
                            .collect();

                        if content_blocks.is_empty() {
                            // Fallback: use raw text as single block
                            editor.set_text(response.content);
                        } else {
                            // Use structured blocks
                            editor.replace_blocks(content_blocks);
                        }

                        info!(
                            "Cell editor updated: {} blocks, text len: {}",
                            editor.blocks.len(),
                            editor.text().len()
                        );
                    }
                    Err(e) => {
                        error!(
                            "Failed to get editor for entity {:?}: {:?}",
                            llm_result.cell_entity, e
                        );
                    }
                }
                complete_events.write(LlmResponseComplete {
                    cell_entity: llm_result.cell_entity,
                    success: true,
                    error: None,
                });
            }
            Err(e) => {
                error!("LLM error: {}", e);
                if let Ok(mut editor) = editors.get_mut(llm_result.cell_entity) {
                    editor.set_text(format!("❌ {}", e));
                }
                complete_events.write(LlmResponseComplete {
                    cell_entity: llm_result.cell_entity,
                    success: false,
                    error: Some(e),
                });
            }
        }
    }
}
