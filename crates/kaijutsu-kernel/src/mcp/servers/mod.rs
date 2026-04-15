//! Virtual in-process MCP servers for kernel builtins (§4, D-01).
//!
//! Each server holds kernel state (BlockStore, caches, DriftRouter) as struct
//! fields and exposes tools via `McpServerLike`. Phase 1 M2 builds these as
//! **delegating wrappers** over the existing `block_tools` / `file_tools`
//! engine bodies — schemars-derived schemas on the MCP surface, preserved
//! logic below. M5 will inline the bodies and delete the old engines.
//!
//! Rationale for the delegating-wrapper approach: lifting ~3000 LOC of engine
//! bodies into this module verbatim before the old paths are removed doubles
//! up code in the tree during M2–M4 and multiplies the risk of divergent
//! behavior. Delegation yields identical behavior with a single source of
//! truth until M5 deletes the old engines and inlines the bodies here.

pub mod adapter;
pub mod block;
pub mod file;
pub mod kernel_info;

pub use block::BlockToolsServer;
pub use file::FileToolsServer;
pub use kernel_info::KernelInfoServer;
