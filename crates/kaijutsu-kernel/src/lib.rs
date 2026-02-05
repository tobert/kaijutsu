//! # kaijutsu-kernel
//!
//! Core kernel crate with VFS abstraction for kaijutsu.
//!
//! The kernel is the fundamental primitive. Everything is a kernel.
//! A kernel:
//! - Owns `/` in its VFS (virtual filesystem)
//! - Can mount worktrees, repos, other kernels at paths like `/mnt/project`
//! - Has a lease (who holds "the pen" for mutations)
//! - Has a consent mode (collaborative vs autonomous)
//! - Can checkpoint (distill history into summaries)
//! - Can be forked (heavy copy, isolated) or threaded (light, shared VFS)

pub mod agents;
pub mod block_store;
pub mod block_tools;
pub mod config_backend;
pub mod control;
pub mod conversation;
pub mod conversation_db;
pub mod db;
pub mod flows;
pub mod kernel;
pub mod llm;
pub mod mcp_pool;
pub mod rhai_engine;
pub mod state;
pub mod tools;
pub mod vfs;

pub use agents::{
    AgentActivityEvent, AgentCapability, AgentConfig, AgentError, AgentInfo, AgentRegistry,
    AgentStatus, SharedAgentRegistry, shared_agent_registry,
};
pub use block_store::{BlockEvent, BlockStore, DocumentEntry, DocumentId, SharedBlockStore, shared_block_store, shared_block_store_with_db};
pub use block_tools::{
    BlockAppendEngine, BlockCreateEngine, BlockEditEngine, BlockListEngine, BlockReadEngine,
    BlockSearchEngine, BlockSpliceEngine, BlockStatusEngine, KernelSearchEngine,
    EditError, EditOp,
    // Batching
    AppendBatcher, BatchConfig, BatcherStats,
    // Cursor tracking
    CursorEvent, CursorPosition, CursorTracker,
};
pub use control::ConsentMode;
pub use conversation::{AccessLevel, Conversation, Mount, Participant, ParticipantKind};
pub use conversation_db::ConversationDb;
pub use db::{DocumentDb, DocumentKind, DocumentMeta, OpRecord, Snapshot};
pub use kernel::Kernel;
pub use rhai_engine::RhaiEngine;
pub use state::KernelState;
pub use tools::{ExecResult, ExecutionEngine, ToolInfo, ToolRegistry};
pub use llm::{
    // Core types
    LlmError, LlmRegistry, LlmResult, RigProvider,
    Message as LlmMessage, ResponseBlock, Role as LlmRole, Usage as LlmUsage,
    // Tool definitions
    ToolDefinition as LlmToolDefinition,
    // Streaming
    StreamEvent, StreamRequest, RigStreamAdapter, LlmStream,
    // Configuration
    ProviderConfig, ToolConfig, ToolFilter, ContextSegment,
    LlmConfig, ModelAlias, initialize_llm_registry, load_llm_config,
    // Default model
    DEFAULT_MODEL,
};
pub use vfs::{
    backends::{LocalBackend, MemoryBackend},
    DirEntry, FileAttr, FileType, MountTable, SetAttr, StatFs, VfsError, VfsOps, VfsResult,
};
pub use mcp_pool::{
    McpPoolError, McpServerConfig, McpServerInfo, McpServerPool, McpToolEngine, McpToolInfo,
    // Resource types
    CachedResource, McpResourceInfo, ResourceCache,
};
pub use config_backend::{
    ConfigCrdtBackend, ConfigError, ConfigFileChange, ConfigChangeKind,
    ConfigWatcherHandle, ValidationResult,
    DEFAULT_THEME, DEFAULT_LLM_CONFIG, EXAMPLE_SEAT,
};

// Re-export rmcp types needed for resource handling
pub use rmcp::model::ResourceContents as McpResourceContents;
pub use flows::{
    BlockFlow, FlowBus, FlowMessage, HasSubject, OpSource, SharedBlockFlowBus, Subscription,
    matches_pattern, shared_block_flow_bus,
    // Resource flow types
    ResourceFlow, SharedResourceFlowBus, shared_resource_flow_bus,
    // Progress flow types
    ProgressFlow, SharedProgressFlowBus, shared_progress_flow_bus,
    // Elicitation flow types
    ElicitationFlow, ElicitationAction, ElicitationResponse,
    SharedElicitationFlowBus, shared_elicitation_flow_bus,
    // Logging flow types
    LoggingFlow, SharedLoggingFlowBus, shared_logging_flow_bus,
    // Config flow types
    ConfigFlow, ConfigSource, SharedConfigFlowBus, shared_config_flow_bus,
};
