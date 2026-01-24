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

pub mod block_store;
pub mod block_tools;
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
    AnthropicProvider, CompletionRequest, CompletionResponse, LlmError, LlmProvider, LlmRegistry,
    LlmResult, Message as LlmMessage, ResponseBlock, Role as LlmRole, Usage as LlmUsage,
};
pub use vfs::{
    backends::{LocalBackend, MemoryBackend},
    DirEntry, FileAttr, FileType, MountTable, SetAttr, StatFs, VfsError, VfsOps, VfsResult,
};
pub use mcp_pool::{McpPoolError, McpServerConfig, McpServerInfo, McpServerPool, McpToolEngine, McpToolInfo};
pub use flows::{
    BlockFlow, FlowBus, FlowMessage, HasSubject, SharedBlockFlowBus, Subscription,
    matches_pattern, shared_block_flow_bus,
};
