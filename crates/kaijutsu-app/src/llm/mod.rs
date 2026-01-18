//! LLM integration for kaijutsu-app.
//!
//! Handles sending prompts to Claude and streaming responses into conversations.
//! Uses a dedicated tokio runtime for API calls since anthropic-api requires tokio.
//!
//! ## Conversation Integration
//!
//! When a user submits a prompt:
//! 1. The user message is added to the current conversation (with author attribution)
//! 2. The conversation history is sent to the LLM
//! 3. The response blocks are added to the conversation (with model attribution)
//! 4. The conversation is auto-saved to SQLite

use bevy::prelude::*;
use kaijutsu_kernel::{
    AnthropicProvider, CompletionRequest, CompletionResponse, LlmMessage, LlmProvider,
    Participant, ResponseBlock,
};
use std::sync::Arc;
use std::thread;
use tokio::sync::mpsc;

use crate::cell::PromptSubmitted;
use crate::conversation::{ConversationRegistry, ConversationStore, CurrentConversation};

/// Plugin for LLM integration.
pub struct LlmPlugin;

impl Plugin for LlmPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<LlmState>()
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
            conversation_id: req.conversation_id,
            result: result.map_err(|e| format!("LLM error: {}", e)),
        });
    }
    info!("LLM worker shutting down");
}

/// Result from an LLM completion.
struct LlmResult {
    conversation_id: String,
    result: Result<CompletionResponse, String>,
}

/// Request to send to the LLM worker.
struct LlmRequest {
    conversation_id: String,
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
    /// Model participant ID (for author attribution).
    pub model_participant_id: String,
}

impl Default for LlmState {
    fn default() -> Self {
        Self {
            provider: None,
            result_rx: None,
            request_tx: None,
            default_model: "claude-haiku-4-5-20251001".to_string(),
            system_prompt: "You are a helpful AI assistant in a collaborative coding environment called Kaijutsu. Be concise and helpful.".to_string(),
            model_participant_id: "model:claude".to_string(),
        }
    }
}

/// System to handle prompts and send them to the LLM.
fn handle_prompt_for_llm(
    mut submit_events: MessageReader<PromptSubmitted>,
    llm_state: Res<LlmState>,
    current: Res<CurrentConversation>,
    mut registry: ResMut<ConversationRegistry>,
    store: Option<Res<ConversationStore>>,
) {
    let (Some(provider), Some(request_tx)) = (llm_state.provider.clone(), llm_state.request_tx.as_ref()) else {
        return;
    };

    let Some(conv_id) = current.id() else {
        return;
    };

    for event in submit_events.read() {
        let prompt_text = event.text.clone();

        if prompt_text.trim().is_empty() {
            continue;
        }

        // Get the conversation
        let Some(conv) = registry.get_mut(conv_id) else {
            error!("Current conversation not found: {}", conv_id);
            continue;
        };

        // Add model participant if not present
        if !conv.has_participant(&llm_state.model_participant_id) {
            conv.add_participant(Participant::model(
                &llm_state.model_participant_id,
                "Claude",
                "anthropic",
                &llm_state.default_model,
            ));
        }

        // Get user participant ID
        let user_id = format!("user:{}", whoami::username());

        // Add user message to conversation
        conv.add_text_message(&user_id, &prompt_text);

        info!(
            "Added user message to conversation {}: {}...",
            conv_id,
            &prompt_text.chars().take(50).collect::<String>()
        );

        // Build messages from conversation history
        // Only include Text blocks (skip Thinking, ToolUse, ToolResult for now)
        let messages: Vec<LlmMessage> = conv
            .messages()
            .iter()
            .filter(|block| block.content.block_type() == kaijutsu_crdt::BlockType::Text)
            .map(|block| {
                let text = block.content.text();
                if block.author.starts_with("user:") {
                    LlmMessage::user(text)
                } else {
                    LlmMessage::assistant(text)
                }
            })
            .collect();

        // Build the request with conversation history
        let request = CompletionRequest::new(&llm_state.default_model, messages)
            .with_system(&llm_state.system_prompt)
            .with_max_tokens(4096);

        // Save conversation after adding user message
        if let Some(ref store) = store {
            store.save(conv);
        }

        // Send to worker thread
        if let Err(e) = request_tx.send(LlmRequest {
            conversation_id: conv_id.to_string(),
            request,
            provider: provider.clone(),
        }) {
            error!("Failed to send LLM request: {}", e);
        }
    }
}

/// System to poll for completed LLM results and update conversations.
fn poll_llm_results(
    mut llm_state: ResMut<LlmState>,
    mut registry: ResMut<ConversationRegistry>,
    store: Option<Res<ConversationStore>>,
) {
    // Extract what we need before mutably borrowing result_rx
    let model_id = llm_state.model_participant_id.clone();

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
                    "LLM response received ({} chars, {} blocks, thinking: {}) for conversation {}",
                    response.content.len(),
                    block_count,
                    has_thinking,
                    llm_result.conversation_id
                );

                // Get the conversation
                let Some(conv) = registry.get_mut(&llm_result.conversation_id) else {
                    error!("Conversation not found: {}", llm_result.conversation_id);
                    continue;
                };

                // Add response blocks to conversation
                if response.blocks.is_empty() {
                    // Fallback: add raw text as single message
                    conv.add_text_message(&model_id, &response.content);
                } else {
                    // Add structured blocks
                    for block in &response.blocks {
                        match block {
                            ResponseBlock::Thinking { thinking, .. } => {
                                conv.add_thinking_message(&model_id, thinking);
                            }
                            ResponseBlock::Text { text } => {
                                conv.add_text_message(&model_id, text);
                            }
                            ResponseBlock::ToolUse { id, name, input } => {
                                conv.add_tool_use(&model_id, id, name, input.clone());
                            }
                            ResponseBlock::ToolResult {
                                tool_use_id,
                                content,
                                is_error,
                            } => {
                                conv.add_tool_result(&model_id, tool_use_id, content, *is_error);
                            }
                        }
                    }
                }

                info!(
                    "Conversation {} updated: {} total messages",
                    llm_result.conversation_id,
                    conv.message_count()
                );

                // Auto-save conversation
                if let Some(ref store) = store {
                    store.save(conv);
                }
            }
            Err(e) => {
                error!("LLM error for conversation {}: {}", llm_result.conversation_id, e);

                // Add error message to conversation
                if let Some(conv) = registry.get_mut(&llm_result.conversation_id) {
                    conv.add_text_message(&model_id, &format!("❌ Error: {}", e));

                    if let Some(ref store) = store {
                        store.save(conv);
                    }
                }
            }
        }
    }
}
