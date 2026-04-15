//! # kaijutsu-kernel
//!
//! Core kernel crate with VFS abstraction for kaijutsu.
//!
//! The kernel is the fundamental primitive. Everything is a kernel.
//! A kernel:
//! - Owns `/` in its VFS (virtual filesystem)
//! - Can mount worktrees, repos, other kernels at paths like `/mnt/project`
//! - Has a consent mode (collaborative vs autonomous)
//! - Can checkpoint (distill history into summaries)
//! - Can be forked (heavy copy, isolated) or threaded (light, shared VFS)
//! - Has a DriftRouter for cross-context communication (shared across fork/thread)

pub mod agents;
pub mod block_store;
pub mod block_tools;
pub mod image;
pub mod config_backend;
pub mod control;
pub mod drift;
pub mod file_tools;
pub mod flows;
pub mod input_doc;
pub mod kernel;
pub mod kernel_db;
pub mod kj;
pub mod llm;
pub mod mcp;
pub mod mcp_config;
pub mod mcp_pool;
pub mod state;
pub mod tools;
pub mod vfs;

pub use agents::{
    AgentActivityEvent, AgentCapability, AgentConfig, AgentError, AgentInfo, AgentRegistry,
    AgentStatus, InvokeRequest, InvokeResponse, SharedAgentRegistry, shared_agent_registry,
};
pub use block_store::DocumentKind;
pub use block_store::{
    BlockStore, BlockStoreError, BlockStoreResult, DbHandle, SharedBlockStore, shared_block_store,
};
pub use block_tools::{
    AbcBlockEngine, BlockAppendEngine, BlockCreateEngine, BlockEditEngine, BlockListEngine,
    BlockReadEngine, BlockSearchEngine, BlockSpliceEngine, BlockStatusEngine, EditOp,
    ImgBlockEngine, ImgBlockFromPathEngine, KernelSearchEngine, SvgBlockEngine,
};
pub use config_backend::{
    ConfigCrdtBackend, ConfigWatcherHandle, DEFAULT_SYSTEM_PROMPT, ValidationResult,
};
pub use control::ConsentMode;
pub use kaijutsu_types::DocKind;
pub use kernel::Kernel;
pub use llm::{
    // Default model
    DEFAULT_MODEL,
    EmbeddingModelConfig,
    LlmConfig,
    // Core types
    LlmError,
    LlmRegistry,
    LlmResult,
    LlmStream,
    Message as LlmMessage,
    ModelAlias,
    ModelsConfig,
    // Configuration
    ProviderConfig,
    ResponseBlock,
    RigProvider,
    RigStreamAdapter,
    Role as LlmRole,
    // Streaming
    StreamEvent,
    StreamRequest,
    ToolConfig,
    // Tool definitions
    ToolDefinition as LlmToolDefinition,
    ToolFilter,
    Usage as LlmUsage,
    // Hydration
    hydrate_from_blocks,
    initialize_llm_registry,
    load_llm_config_toml,
    load_models_config_toml,
};
pub use mcp_config::{McpConfig, load_mcp_config_toml};
pub use mcp_pool::{
    // Resource types
    CachedResource,
    McpForkMode,
    McpPoolError,
    McpRegistration,
    McpResourceInfo,
    McpServerConfig,
    McpServerInfo,
    McpServerPool,
    McpToolEngine,
    McpToolInfo,
    McpTransport,
    ResourceCache,
    extract_tool_result_text,
    register_mcp_prompt_engines, register_mcp_resource_engines, serialize_prompt_messages,
};
pub use state::KernelState;
pub use tools::{EngineArgs, ExecResult, ExecutionEngine, ToolContext, ToolInfo, ToolRegistry};
pub use vfs::{
    DirEntry, FileAttr, FileType, MountTable, SetAttr, StatFs, VfsError, VfsOps, VfsResult,
    backends::{LocalBackend, MemoryBackend},
};

pub use drift::{
    ContextHandle,
    // Distillation helpers
    DISTILLATION_SYSTEM_PROMPT,
    DriftError,
    DriftRouter,
    SharedDriftRouter,
    StagedDrift,
    build_distillation_prompt,
    shared_drift_router,
};
pub use file_tools::{
    EditEngine, FileDocumentCache, GlobEngine, GrepEngine, ReadEngine, WhoamiEngine, WriteEngine,
};

// Re-export rmcp types needed for resource handling
pub use flows::{
    BlockFlow,
    // Config flow types
    ConfigFlow,
    ConfigSource,
    ElicitationAction,
    // Elicitation flow types
    ElicitationFlow,
    ElicitationResponse,
    FlowBus,
    FlowMessage,
    HasSubject,
    // Input doc flow types
    InputDocFlow,
    // Logging flow types
    LoggingFlow,
    OpSource,
    // Progress flow types
    ProgressFlow,
    // Resource flow types
    ResourceFlow,
    SharedBlockFlowBus,
    SharedConfigFlowBus,
    SharedElicitationFlowBus,
    SharedInputDocFlowBus,
    SharedLoggingFlowBus,
    SharedProgressFlowBus,
    SharedResourceFlowBus,
    Subscription,
    shared_block_flow_bus,
    shared_config_flow_bus,
    shared_elicitation_flow_bus,
    shared_input_doc_flow_bus,
    shared_logging_flow_bus,
    shared_progress_flow_bus,
    shared_resource_flow_bus,
};
pub use input_doc::InputDocEntry;
pub use kernel_db::{
    ContextEdgeRow, ContextEnvRow, ContextRow, ContextShellRow, DocSnapshotRow, DocumentRow,
    InputDocSnapshotRow, KernelDb, KernelDbError, KernelDbResult, PresetRow, WorkspacePathRow,
    WorkspaceRow,
};
pub use kj::{KjCaller, KjDispatcher, KjResult};
pub use rmcp::model::ResourceContents as McpResourceContents;
