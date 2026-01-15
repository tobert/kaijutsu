//! Cap'n Proto RPC client for sshwarma
//!
//! Provides typed interface to the World, Room, and KaishKernel capabilities.

// Re-export the generated Cap'n Proto code from crate root
pub use crate::kaijutsu_capnp;

/// RPC client wrapper
///
/// TODO: Implement actual RPC logic
/// - Bootstrap World capability from SSH channel
/// - Provide typed methods for all RPC calls
/// - Handle capability lifecycle
pub struct RpcClient {
    // world: Option<kaijutsu_capnp::world::Client>,
}

impl RpcClient {
    pub fn new() -> Self {
        Self {
            // world: None,
        }
    }

    /// Initialize RPC over an SSH channel
    ///
    /// Bootstraps the World capability from the server
    pub async fn init(&mut self /* , channel: SshChannel */) -> Result<(), RpcError> {
        // TODO: Implement
        // 1. Create VatNetwork over SSH channel
        // 2. Bootstrap World capability
        // 3. Store for later use
        Err(RpcError::NotImplemented)
    }

    /// Get current identity
    pub async fn whoami(&self) -> Result<Identity, RpcError> {
        // TODO: Implement
        Err(RpcError::NotImplemented)
    }

    /// List available rooms
    pub async fn list_rooms(&self) -> Result<Vec<RoomInfo>, RpcError> {
        // TODO: Implement
        Err(RpcError::NotImplemented)
    }

    /// Join a room by name
    pub async fn join_room(&self, name: &str) -> Result<RoomHandle, RpcError> {
        // TODO: Implement
        Err(RpcError::NotImplemented)
    }

    /// Create a new room
    pub async fn create_room(&self, config: RoomConfig) -> Result<RoomHandle, RpcError> {
        // TODO: Implement
        Err(RpcError::NotImplemented)
    }
}

impl Default for RpcClient {
    fn default() -> Self {
        Self::new()
    }
}

// ============================================================================
// Local types (mirror schema types for Rust ergonomics)
// ============================================================================

#[derive(Debug, Clone)]
pub struct Identity {
    pub username: String,
    pub display_name: String,
}

#[derive(Debug, Clone)]
pub struct RoomInfo {
    pub id: u64,
    pub name: String,
    pub branch: String,
    pub user_count: u32,
    pub agent_count: u32,
}

#[derive(Debug, Clone)]
pub struct RoomConfig {
    pub name: String,
    pub branch: Option<String>,
    pub repos: Vec<RepoMount>,
}

#[derive(Debug, Clone)]
pub struct RepoMount {
    pub name: String,
    pub url: String,
    pub writable: bool,
}

/// Handle to a joined room
///
/// TODO: Implement room operations
pub struct RoomHandle {
    // room: kaijutsu_capnp::room::Client,
}

impl RoomHandle {
    /// Send a message to the room
    pub async fn send(&self, content: &str) -> Result<Row, RpcError> {
        Err(RpcError::NotImplemented)
    }

    /// Mention an agent
    pub async fn mention(&self, agent: &str, content: &str) -> Result<Row, RpcError> {
        Err(RpcError::NotImplemented)
    }

    /// Get room history
    pub async fn get_history(&self, limit: u32, before_id: u64) -> Result<Vec<Row>, RpcError> {
        Err(RpcError::NotImplemented)
    }

    /// Get the kaish kernel
    pub async fn get_kernel(&self) -> Result<KernelHandle, RpcError> {
        Err(RpcError::NotImplemented)
    }

    /// Leave the room
    pub async fn leave(self) -> Result<(), RpcError> {
        Err(RpcError::NotImplemented)
    }
}

/// Handle to a kaish kernel
pub struct KernelHandle {
    // kernel: kaijutsu_capnp::kaish_kernel::Client,
}

impl KernelHandle {
    /// Execute code in the kernel
    pub async fn execute(&self, code: &str) -> Result<u64, RpcError> {
        Err(RpcError::NotImplemented)
    }

    /// Interrupt an execution
    pub async fn interrupt(&self, exec_id: u64) -> Result<(), RpcError> {
        Err(RpcError::NotImplemented)
    }

    /// Get completions
    pub async fn complete(&self, partial: &str, cursor: u32) -> Result<Vec<Completion>, RpcError> {
        Err(RpcError::NotImplemented)
    }
}

#[derive(Debug, Clone)]
pub struct Row {
    pub id: u64,
    pub parent_id: u64,
    pub row_type: RowType,
    pub sender: String,
    pub content: String,
    pub timestamp: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowType {
    Chat,
    AgentResponse,
    ToolCall,
    ToolResult,
    SystemMessage,
}

#[derive(Debug, Clone)]
pub struct Completion {
    pub text: String,
    pub display_text: String,
    pub kind: CompletionKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompletionKind {
    Command,
    Path,
    Variable,
    Keyword,
}

// ============================================================================
// Errors
// ============================================================================

#[derive(Debug, Clone)]
pub enum RpcError {
    NotImplemented,
    NotConnected,
    CapabilityLost,
    ServerError(String),
}

impl std::fmt::Display for RpcError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RpcError::NotImplemented => write!(f, "RPC not yet implemented"),
            RpcError::NotConnected => write!(f, "Not connected to server"),
            RpcError::CapabilityLost => write!(f, "Capability no longer valid"),
            RpcError::ServerError(s) => write!(f, "Server error: {}", s),
        }
    }
}

impl std::error::Error for RpcError {}
