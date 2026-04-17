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
pub mod execution;
pub mod file_tools;
pub mod flows;
pub mod input_doc;
pub mod kernel;
pub mod kernel_db;
pub mod kj;
pub mod llm;
pub mod mcp;
pub mod state;
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
    // Tool definitions
    ToolDefinition as LlmToolDefinition,
    Usage as LlmUsage,
    // Hydration
    hydrate_from_blocks,
    initialize_llm_registry,
    load_llm_config_toml,
    load_models_config_toml,
};
pub use execution::{ExecContext, ExecResult};
pub use state::KernelState;
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

// Non-MCP flow buses (block / config / input-doc). The MCP-specific buses
// (Resource / Progress / Logging / Elicitation) were removed in Phase 1 M5
// per D-32; external MCP notifications now ride the ServerNotification
// broadcast on each ExternalMcpServer.
pub use flows::{
    BlockFlow,
    ConfigFlow,
    ConfigSource,
    FlowBus,
    FlowMessage,
    HasSubject,
    InputDocFlow,
    OpSource,
    SharedBlockFlowBus,
    SharedConfigFlowBus,
    SharedInputDocFlowBus,
    Subscription,
    shared_block_flow_bus,
    shared_config_flow_bus,
    shared_input_doc_flow_bus,
};
pub use input_doc::InputDocEntry;
pub use kernel_db::{
    ContextEdgeRow, ContextEnvRow, ContextRow, ContextShellRow, DocSnapshotRow, DocumentRow,
    InputDocSnapshotRow, KernelDb, KernelDbError, KernelDbResult, PresetRow, WorkspacePathRow,
    WorkspaceRow,
};
pub use kj::{KjCaller, KjDispatcher, KjResult};
