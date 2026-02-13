//! MCP Server Pool for managing shared MCP connections.
//!
//! This module provides a pool of MCP server connections that can be shared
//! across multiple kernels. MCP servers are spawned as subprocesses and
//! communicate via stdio.
//!
//! # Architecture
//!
//! ```text
//! McpServerPool
//!     │
//!     ├── Server "git" ──────────► uvx mcp-server-git (subprocess)
//!     │       └── tools: [git_status, git_log, git_diff, ...]
//!     │
//!     ├── Server "exa" ──────────► exa-mcp-server (subprocess)
//!     │       └── tools: [web_search, ...]
//!     │
//!     └── Server "custom" ───────► custom-server (subprocess)
//!             └── tools: [custom_tool, ...]
//! ```
//!
//! # Example
//!
//! ```ignore
//! let pool = McpServerPool::new();
//!
//! // Register an MCP server
//! pool.register(McpServerConfig {
//!     name: "git".into(),
//!     command: "uvx".into(),
//!     args: vec!["mcp-server-git".into()],
//!     ..Default::default()
//! }).await?;
//!
//! // Call a tool
//! let result = pool.call_tool("git", "git_status", json!({"repo_path": "."})).await?;
//! ```

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use dashmap::DashMap;
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;
use thiserror::Error;
use tokio::process::Command;
use tokio::sync::Mutex;
use tracing::{debug, info, warn};

use crate::flows::{
    ElicitationAction, ElicitationFlow, ElicitationResponse, LoggingFlow, OpSource, ProgressFlow,
    ResourceFlow, SharedElicitationFlowBus, SharedLoggingFlowBus, SharedProgressFlowBus,
    SharedResourceFlowBus,
};

use rmcp::model::{
    ArgumentInfo, CallToolRequestParams, CallToolResult, ClientCapabilities, ClientInfo,
    CompleteRequestParams, CompleteResult, CreateElicitationRequestParams, CreateElicitationResult,
    ElicitationAction as RmcpElicitationAction, GetPromptRequestParams, GetPromptResult,
    ListRootsResult, LoggingLevel, ProgressNotificationParam, Prompt, PromptArgument,
    Reference, Root, RootsCapabilities, SetLevelRequestParams, Tool as McpTool,
};
use rmcp::service::{RequestContext, RunningService, ServiceError};
use rmcp::transport::{ConfigureCommandExt, StreamableHttpClientTransport, TokioChildProcess};
use rmcp::{ClientHandler, ErrorData, Peer, RoleClient};
use tokio::sync::oneshot;

/// Errors that can occur when working with the MCP pool.
#[derive(Debug, Error)]
pub enum McpPoolError {
    #[error("Server not found: {0}")]
    ServerNotFound(String),

    #[error("Server already registered: {0}")]
    ServerAlreadyExists(String),

    #[error("Tool not found: {server}:{tool}")]
    ToolNotFound { server: String, tool: String },

    #[error("Failed to spawn server: {0}")]
    SpawnError(String),

    #[error("Failed to initialize server: {0}")]
    InitError(String),

    #[error("Service error: {0}")]
    ServiceError(#[from] ServiceError),

    #[error("IO error: {0}")]
    IoError(#[from] std::io::Error),

    #[error("Server disconnected: {0}")]
    Disconnected(String),
}

/// Transport type for MCP server connections.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum McpTransport {
    /// Spawn subprocess, communicate via stdin/stdout.
    #[default]
    Stdio,
    /// Connect to a running server via streamable HTTP.
    StreamableHttp,
}

/// Configuration for an MCP server.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpServerConfig {
    /// Unique name for this server (e.g., "git", "exa").
    pub name: String,
    /// Command to run (stdio transport only).
    pub command: String,
    /// Arguments for the command (stdio transport only).
    #[serde(default)]
    pub args: Vec<String>,
    /// Environment variables to set.
    #[serde(default)]
    pub env: HashMap<String, String>,
    /// Working directory for the server (stdio transport only).
    pub cwd: Option<String>,
    /// Transport type (default: Stdio).
    #[serde(default)]
    pub transport: McpTransport,
    /// Server URL (streamable HTTP transport only).
    pub url: Option<String>,
}

impl Default for McpServerConfig {
    fn default() -> Self {
        Self {
            name: String::new(),
            command: String::new(),
            args: Vec::new(),
            env: HashMap::new(),
            cwd: None,
            transport: McpTransport::Stdio,
            url: None,
        }
    }
}

/// Information about a connected MCP server.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpServerInfo {
    /// Server name.
    pub name: String,
    /// Server's protocol version.
    pub protocol_version: String,
    /// Server's reported name.
    pub server_name: String,
    /// Server's reported version.
    pub server_version: String,
    /// Tools provided by this server.
    pub tools: Vec<McpToolInfo>,
}

/// Information about an MCP tool.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpToolInfo {
    /// Tool name (as reported by the server).
    pub name: String,
    /// Tool description.
    pub description: Option<String>,
    /// Input schema (JSON Schema).
    pub input_schema: JsonValue,
}

impl From<McpTool> for McpToolInfo {
    fn from(tool: McpTool) -> Self {
        Self {
            name: tool.name.to_string(),
            description: tool.description.map(|s| s.to_string()),
            input_schema: JsonValue::Object(tool.input_schema.as_ref().clone()),
        }
    }
}

// =============================================================================
// Prompt Info Types
// =============================================================================

/// Information about an MCP prompt.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpPromptInfo {
    /// Prompt name.
    pub name: String,
    /// Optional title.
    pub title: Option<String>,
    /// Prompt description.
    pub description: Option<String>,
    /// Arguments that can be passed to the prompt.
    pub arguments: Vec<McpPromptArgumentInfo>,
}

/// Information about a prompt argument.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpPromptArgumentInfo {
    /// Argument name.
    pub name: String,
    /// Optional title.
    pub title: Option<String>,
    /// Argument description.
    pub description: Option<String>,
    /// Whether the argument is required.
    pub required: bool,
}

impl From<Prompt> for McpPromptInfo {
    fn from(prompt: Prompt) -> Self {
        Self {
            name: prompt.name,
            title: prompt.title,
            description: prompt.description,
            arguments: prompt
                .arguments
                .unwrap_or_default()
                .into_iter()
                .map(McpPromptArgumentInfo::from)
                .collect(),
        }
    }
}

impl From<PromptArgument> for McpPromptArgumentInfo {
    fn from(arg: PromptArgument) -> Self {
        Self {
            name: arg.name,
            title: arg.title,
            description: arg.description,
            required: arg.required.unwrap_or(false),
        }
    }
}

/// Information about an MCP root (workspace directory).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpRootInfo {
    /// Root URI (typically file:// URI).
    pub uri: String,
    /// Optional name for the root.
    pub name: Option<String>,
}

impl From<Root> for McpRootInfo {
    fn from(root: Root) -> Self {
        Self {
            uri: root.uri,
            name: root.name,
        }
    }
}

impl From<McpRootInfo> for Root {
    fn from(info: McpRootInfo) -> Self {
        Self {
            uri: info.uri,
            name: info.name,
        }
    }
}

// =============================================================================
// Resource Cache
// =============================================================================

/// Information about a cached MCP resource.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpResourceInfo {
    /// Resource URI.
    pub uri: String,
    /// Resource name.
    pub name: Option<String>,
    /// Resource description.
    pub description: Option<String>,
    /// MIME type.
    pub mime_type: Option<String>,
}

/// Cached resource entry with content and metadata.
#[derive(Debug, Clone)]
pub struct CachedResource {
    /// Resource metadata.
    pub info: McpResourceInfo,
    /// Cached content (serialized ResourceContents).
    pub content: Option<Vec<u8>>,
    /// When this entry was last refreshed.
    pub last_refresh: Instant,
    /// Number of active subscribers (if > 0, TTL is disabled).
    pub subscriber_count: u32,
}

impl CachedResource {
    /// Create a new cached resource entry.
    pub fn new(info: McpResourceInfo) -> Self {
        Self {
            info,
            content: None,
            last_refresh: Instant::now(),
            subscriber_count: 0,
        }
    }

    /// Check if this entry is stale based on TTL.
    pub fn is_stale(&self, ttl: Duration) -> bool {
        // If there are active subscribers, the resource stays fresh
        if self.subscriber_count > 0 {
            return false;
        }
        self.last_refresh.elapsed() > ttl
    }

    /// Mark this entry as refreshed.
    pub fn touch(&mut self) {
        self.last_refresh = Instant::now();
    }
}

/// Cache for MCP resources, keyed by "server:uri".
///
/// Thread-safe via DashMap. Supports TTL-based invalidation
/// and subscriber tracking.
#[derive(Debug)]
pub struct ResourceCache {
    /// Cached resources, keyed by "server:uri".
    entries: DashMap<String, CachedResource>,
    /// Resources grouped by server for list operations.
    server_resources: DashMap<String, Vec<String>>,
    /// Default TTL for cache entries (5 minutes).
    default_ttl: Duration,
}

impl Default for ResourceCache {
    fn default() -> Self {
        Self::new()
    }
}

impl ResourceCache {
    /// Default TTL: 5 minutes.
    pub const DEFAULT_TTL: Duration = Duration::from_secs(300);

    /// Create a new resource cache with default TTL.
    pub fn new() -> Self {
        Self {
            entries: DashMap::new(),
            server_resources: DashMap::new(),
            default_ttl: Self::DEFAULT_TTL,
        }
    }

    /// Create a new resource cache with custom TTL.
    pub fn with_ttl(ttl: Duration) -> Self {
        Self {
            entries: DashMap::new(),
            server_resources: DashMap::new(),
            default_ttl: ttl,
        }
    }

    /// Generate a cache key from server and URI.
    pub fn cache_key(server: &str, uri: &str) -> String {
        format!("{}:{}", server, uri)
    }

    /// Get a cached resource if it exists and is not stale.
    pub fn get(&self, server: &str, uri: &str) -> Option<CachedResource> {
        let key = Self::cache_key(server, uri);
        self.entries.get(&key).and_then(|entry| {
            if entry.is_stale(self.default_ttl) {
                None
            } else {
                Some(entry.clone())
            }
        })
    }

    /// Get a cached resource even if stale (for background refresh).
    pub fn get_any(&self, server: &str, uri: &str) -> Option<CachedResource> {
        let key = Self::cache_key(server, uri);
        self.entries.get(&key).map(|entry| entry.clone())
    }

    /// Insert or update a cached resource.
    pub fn insert(&self, server: &str, info: McpResourceInfo, content: Option<Vec<u8>>) {
        let key = Self::cache_key(server, &info.uri);
        let uri = info.uri.clone();

        // Update or insert the entry
        self.entries
            .entry(key)
            .and_modify(|entry| {
                entry.info = info.clone();
                entry.content = content.clone();
                entry.touch();
            })
            .or_insert_with(|| {
                let mut entry = CachedResource::new(info);
                entry.content = content;
                entry
            });

        // Track this resource under its server
        self.server_resources
            .entry(server.to_string())
            .or_default()
            .push(uri);
    }

    /// Update just the content of a cached resource.
    pub fn update_content(&self, server: &str, uri: &str, content: Vec<u8>) {
        let key = Self::cache_key(server, uri);
        if let Some(mut entry) = self.entries.get_mut(&key) {
            entry.content = Some(content);
            entry.touch();
        }
    }

    /// Invalidate a specific resource (marks as stale by setting last_refresh to epoch).
    pub fn invalidate(&self, server: &str, uri: &str) {
        let key = Self::cache_key(server, uri);
        if let Some(mut entry) = self.entries.get_mut(&key) {
            // Set to a time long ago so it's definitely stale
            entry.last_refresh = Instant::now() - (self.default_ttl * 2);
        }
    }

    /// Invalidate all resources for a server.
    pub fn invalidate_server(&self, server: &str) {
        if let Some(uris) = self.server_resources.get(server) {
            for uri in uris.iter() {
                self.invalidate(server, uri);
            }
        }
    }

    /// Remove a specific resource from the cache.
    pub fn remove(&self, server: &str, uri: &str) {
        let key = Self::cache_key(server, uri);
        self.entries.remove(&key);

        // Also remove from server tracking
        if let Some(mut uris) = self.server_resources.get_mut(server) {
            uris.retain(|u| u != uri);
        }
    }

    /// Remove all resources for a server.
    pub fn remove_server(&self, server: &str) {
        if let Some((_, uris)) = self.server_resources.remove(server) {
            for uri in uris {
                let key = Self::cache_key(server, &uri);
                self.entries.remove(&key);
            }
        }
    }

    /// Get all cached resources for a server.
    pub fn list_for_server(&self, server: &str) -> Vec<McpResourceInfo> {
        self.server_resources
            .get(server)
            .map(|uris| {
                uris.iter()
                    .filter_map(|uri| {
                        let key = Self::cache_key(server, uri);
                        self.entries.get(&key).map(|e| e.info.clone())
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Increment subscriber count for a resource.
    pub fn add_subscriber(&self, server: &str, uri: &str) {
        let key = Self::cache_key(server, uri);
        if let Some(mut entry) = self.entries.get_mut(&key) {
            entry.subscriber_count += 1;
        }
    }

    /// Decrement subscriber count for a resource.
    pub fn remove_subscriber(&self, server: &str, uri: &str) {
        let key = Self::cache_key(server, uri);
        if let Some(mut entry) = self.entries.get_mut(&key) {
            entry.subscriber_count = entry.subscriber_count.saturating_sub(1);
        }
    }

    /// Get the number of cached entries.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Check if the cache is empty.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Clear all cached entries.
    pub fn clear(&self) {
        self.entries.clear();
        self.server_resources.clear();
    }
}

/// A connected MCP server with its running service.
struct ConnectedServer {
    /// The configuration used to start this server (for reconnection).
    #[allow(dead_code)]
    config: McpServerConfig,
    /// The running service (holds the peer for making requests).
    service: RunningService<RoleClient, KaijutsuClientHandler>,
    /// Cached list of tools.
    tools: Vec<McpToolInfo>,
}

impl ConnectedServer {
    /// Get the peer for making requests.
    fn peer(&self) -> &Peer<RoleClient> {
        self.service.peer()
    }
}

/// Pending elicitation request awaiting response.
pub struct PendingElicitation {
    /// Oneshot channel to send the response.
    pub response_tx: oneshot::Sender<ElicitationResponse>,
}

/// Client handler for kaijutsu's MCP connections.
///
/// Each handler is associated with a specific MCP server and has access to
/// the shared caches, flow buses, and elicitation tracking for push-based updates.
#[derive(Clone)]
pub struct KaijutsuClientHandler {
    /// Client info to report to servers.
    client_info: ClientInfo,
    /// The MCP server name this handler is for.
    server_name: String,
    /// Shared resource cache for invalidation on notifications.
    cache: Arc<ResourceCache>,
    /// Flow bus for publishing resource events.
    resource_flows: SharedResourceFlowBus,
    /// Flow bus for publishing progress events.
    progress_flows: SharedProgressFlowBus,
    /// Flow bus for publishing elicitation requests.
    elicitation_flows: SharedElicitationFlowBus,
    /// Flow bus for publishing logging events.
    logging_flows: SharedLoggingFlowBus,
    /// Pending elicitation requests, keyed by request_id.
    pending_elicitations: Arc<DashMap<String, PendingElicitation>>,
    /// Roots advertised to servers.
    roots: Arc<RwLock<Vec<Root>>>,
}

impl std::fmt::Debug for KaijutsuClientHandler {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KaijutsuClientHandler")
            .field("server_name", &self.server_name)
            .finish_non_exhaustive()
    }
}

impl KaijutsuClientHandler {
    /// Create a new handler for a specific server with all flow buses.
    pub fn new(
        server_name: impl Into<String>,
        cache: Arc<ResourceCache>,
        resource_flows: SharedResourceFlowBus,
        progress_flows: SharedProgressFlowBus,
        elicitation_flows: SharedElicitationFlowBus,
        logging_flows: SharedLoggingFlowBus,
        roots: Arc<RwLock<Vec<Root>>>,
    ) -> Self {
        let mut info = ClientInfo::default();
        info.client_info.name = "kaijutsu".into();
        info.client_info.version = env!("CARGO_PKG_VERSION").into();
        // Enable roots capability so servers can request our roots
        info.capabilities = ClientCapabilities {
            roots: Some(RootsCapabilities {
                list_changed: Some(true),
            }),
            ..Default::default()
        };
        Self {
            client_info: info,
            server_name: server_name.into(),
            cache,
            resource_flows,
            progress_flows,
            elicitation_flows,
            logging_flows,
            pending_elicitations: Arc::new(DashMap::new()),
            roots,
        }
    }

    /// Create a default handler without shared state (for testing).
    pub fn default_handler() -> Self {
        use crate::flows::{
            shared_elicitation_flow_bus, shared_logging_flow_bus, shared_progress_flow_bus,
            shared_resource_flow_bus,
        };
        let mut info = ClientInfo::default();
        info.client_info.name = "kaijutsu".into();
        info.client_info.version = env!("CARGO_PKG_VERSION").into();
        info.capabilities = ClientCapabilities {
            roots: Some(RootsCapabilities {
                list_changed: Some(true),
            }),
            ..Default::default()
        };
        Self {
            client_info: info,
            server_name: String::new(),
            cache: Arc::new(ResourceCache::new()),
            resource_flows: shared_resource_flow_bus(64),
            progress_flows: shared_progress_flow_bus(64),
            elicitation_flows: shared_elicitation_flow_bus(16),
            logging_flows: shared_logging_flow_bus(256),
            pending_elicitations: Arc::new(DashMap::new()),
            roots: Arc::new(RwLock::new(Vec::new())),
        }
    }

    /// Register an elicitation response channel.
    pub fn register_elicitation(
        &self,
        request_id: String,
        response_tx: oneshot::Sender<ElicitationResponse>,
    ) {
        self.pending_elicitations.insert(
            request_id,
            PendingElicitation { response_tx },
        );
    }

    /// Get the pending elicitations map for external response handling.
    pub fn pending_elicitations(&self) -> &Arc<DashMap<String, PendingElicitation>> {
        &self.pending_elicitations
    }
}

impl Default for KaijutsuClientHandler {
    fn default() -> Self {
        Self::default_handler()
    }
}

impl ClientHandler for KaijutsuClientHandler {
    fn get_info(&self) -> ClientInfo {
        self.client_info.clone()
    }

    fn on_logging_message(
        &self,
        params: rmcp::model::LoggingMessageNotificationParam,
        _context: rmcp::service::NotificationContext<RoleClient>,
    ) -> impl std::future::Future<Output = ()> + Send + '_ {
        let server_name = self.server_name.clone();
        let logging_flows = self.logging_flows.clone();
        let level = format!("{:?}", params.level).to_lowercase();
        let logger = params.logger.clone();
        let data = params.data.clone();

        async move {
            debug!(
                server = %server_name,
                level = %level,
                logger = ?logger,
                "MCP log: {:?}",
                data
            );

            // Publish to FlowBus for connected clients
            logging_flows.publish(LoggingFlow::Message {
                server: server_name,
                level,
                logger,
                data,
            });
        }
    }

    fn on_progress(
        &self,
        params: ProgressNotificationParam,
        _context: rmcp::service::NotificationContext<RoleClient>,
    ) -> impl std::future::Future<Output = ()> + Send + '_ {
        let server_name = self.server_name.clone();
        let progress_flows = self.progress_flows.clone();
        // Convert ProgressToken inner NumberOrString to string
        let token = params.progress_token.0.to_string();
        let progress = params.progress;
        let total = params.total;
        let message = params.message.clone();

        async move {
            debug!(
                server = %server_name,
                token = %token,
                progress = %progress,
                total = ?total,
                message = ?message,
                "MCP progress notification"
            );

            // Publish to FlowBus for connected clients
            progress_flows.publish(ProgressFlow::Update {
                server: server_name,
                token,
                progress,
                total,
                message,
            });
        }
    }

    fn on_tool_list_changed(
        &self,
        _context: rmcp::service::NotificationContext<RoleClient>,
    ) -> impl std::future::Future<Output = ()> + Send + '_ {
        async move {
            info!("MCP server tool list changed");
            // TODO: refresh tools cache
        }
    }

    fn on_resource_updated(
        &self,
        params: rmcp::model::ResourceUpdatedNotificationParam,
        _context: rmcp::service::NotificationContext<RoleClient>,
    ) -> impl std::future::Future<Output = ()> + Send + '_ {
        let server_name = self.server_name.clone();
        let uri = params.uri.clone();
        let cache = self.cache.clone();
        let resource_flows = self.resource_flows.clone();

        async move {
            info!(
                server = %server_name,
                uri = %uri,
                "MCP resource updated notification"
            );

            // Remove cache entry to force re-fetch on next read.
            // Using remove() instead of invalidate() avoids a race condition where
            // a concurrent read_resource() could overwrite the invalidation with stale data.
            cache.remove(&server_name, &uri);

            // Publish to FlowBus for connected clients
            resource_flows.publish(ResourceFlow::Updated {
                server: server_name,
                uri,
                content: None, // Content needs to be fetched by client
                source: OpSource::Remote,
            });
        }
    }

    fn on_resource_list_changed(
        &self,
        _context: rmcp::service::NotificationContext<RoleClient>,
    ) -> impl std::future::Future<Output = ()> + Send + '_ {
        let server_name = self.server_name.clone();
        let cache = self.cache.clone();
        let resource_flows = self.resource_flows.clone();

        async move {
            info!(
                server = %server_name,
                "MCP resource list changed notification"
            );

            // Remove all cached resources for this server to force re-fetch.
            // Using remove_server() instead of invalidate_server() avoids race conditions.
            cache.remove_server(&server_name);

            // Publish to FlowBus for connected clients
            resource_flows.publish(ResourceFlow::ListChanged {
                server: server_name,
                resources: None, // List needs to be re-fetched by client
                source: OpSource::Remote,
            });
        }
    }

    fn list_roots(
        &self,
        _ctx: RequestContext<RoleClient>,
    ) -> impl std::future::Future<Output = Result<ListRootsResult, ErrorData>> + Send + '_ {
        let roots = self.roots.clone();
        async move {
            let roots_list = roots.read().clone();
            Ok(ListRootsResult { roots: roots_list })
        }
    }

    fn create_elicitation(
        &self,
        request: CreateElicitationRequestParams,
        _ctx: RequestContext<RoleClient>,
    ) -> impl std::future::Future<Output = Result<CreateElicitationResult, ErrorData>> + Send + '_ {
        let server_name = self.server_name.clone();
        let elicitation_flows = self.elicitation_flows.clone();
        let pending_elicitations = self.pending_elicitations.clone();

        async move {
            // Generate a unique request ID
            let request_id = uuid::Uuid::new_v4().to_string();

            info!(
                server = %server_name,
                request_id = %request_id,
                message = %request.message,
                "MCP elicitation request received"
            );

            // Create a oneshot channel for the response
            let (response_tx, response_rx) = oneshot::channel();

            // Store the pending elicitation
            pending_elicitations.insert(
                request_id.clone(),
                PendingElicitation { response_tx },
            );

            // Publish to FlowBus for UI handling
            elicitation_flows.publish(ElicitationFlow::Request {
                request_id: request_id.clone(),
                server: server_name.clone(),
                message: request.message,
                schema: serde_json::to_value(&request.requested_schema).ok(),
            });

            // Wait for response with timeout (5 minutes)
            let timeout_duration = std::time::Duration::from_secs(300);
            match tokio::time::timeout(timeout_duration, response_rx).await {
                Ok(Ok(response)) => {
                    // Remove from pending
                    pending_elicitations.remove(&request_id);

                    // Convert our ElicitationAction to rmcp's
                    let action = match response.action {
                        ElicitationAction::Accept => RmcpElicitationAction::Accept,
                        ElicitationAction::Decline => RmcpElicitationAction::Decline,
                        ElicitationAction::Cancel => RmcpElicitationAction::Cancel,
                    };

                    Ok(CreateElicitationResult {
                        action,
                        content: response.content,
                    })
                }
                Ok(Err(_)) => {
                    // Channel closed, decline
                    pending_elicitations.remove(&request_id);
                    warn!(
                        server = %server_name,
                        request_id = %request_id,
                        "Elicitation response channel closed"
                    );
                    Ok(CreateElicitationResult {
                        action: RmcpElicitationAction::Decline,
                        content: None,
                    })
                }
                Err(_) => {
                    // Timeout, decline
                    pending_elicitations.remove(&request_id);
                    warn!(
                        server = %server_name,
                        request_id = %request_id,
                        "Elicitation timed out after 5 minutes"
                    );
                    Ok(CreateElicitationResult {
                        action: RmcpElicitationAction::Decline,
                        content: None,
                    })
                }
            }
        }
    }
}

/// Pool of MCP server connections.
///
/// Thread-safe and can be shared across multiple kernels via `Arc`.
pub struct McpServerPool {
    /// Connected servers, keyed by name.
    servers: RwLock<HashMap<String, Arc<Mutex<ConnectedServer>>>>,
    /// Shared resource cache for all MCP servers.
    cache: Arc<ResourceCache>,
    /// Flow bus for resource events.
    resource_flows: SharedResourceFlowBus,
    /// Flow bus for progress events.
    progress_flows: SharedProgressFlowBus,
    /// Flow bus for elicitation requests.
    elicitation_flows: SharedElicitationFlowBus,
    /// Flow bus for logging events.
    logging_flows: SharedLoggingFlowBus,
    /// Roots advertised to all connected servers.
    roots: Arc<RwLock<Vec<Root>>>,
    /// Prompt cache, keyed by "server:prompt_name".
    prompt_cache: DashMap<String, McpPromptInfo>,
}

impl Default for McpServerPool {
    fn default() -> Self {
        Self::new()
    }
}

impl McpServerPool {
    /// Create a new empty pool.
    pub fn new() -> Self {
        use crate::flows::{
            shared_elicitation_flow_bus, shared_logging_flow_bus, shared_progress_flow_bus,
            shared_resource_flow_bus,
        };
        Self {
            servers: RwLock::new(HashMap::new()),
            cache: Arc::new(ResourceCache::new()),
            resource_flows: shared_resource_flow_bus(256),
            progress_flows: shared_progress_flow_bus(256),
            elicitation_flows: shared_elicitation_flow_bus(16),
            logging_flows: shared_logging_flow_bus(1024),
            roots: Arc::new(RwLock::new(Vec::new())),
            prompt_cache: DashMap::new(),
        }
    }

    /// Create a new pool with custom flow buses.
    pub fn with_flows(
        resource_flows: SharedResourceFlowBus,
        progress_flows: SharedProgressFlowBus,
        elicitation_flows: SharedElicitationFlowBus,
        logging_flows: SharedLoggingFlowBus,
    ) -> Self {
        Self {
            servers: RwLock::new(HashMap::new()),
            cache: Arc::new(ResourceCache::new()),
            resource_flows,
            progress_flows,
            elicitation_flows,
            logging_flows,
            roots: Arc::new(RwLock::new(Vec::new())),
            prompt_cache: DashMap::new(),
        }
    }

    /// Get the shared resource cache.
    pub fn cache(&self) -> &Arc<ResourceCache> {
        &self.cache
    }

    /// Get the resource flow bus for subscribing to resource events.
    pub fn resource_flows(&self) -> &SharedResourceFlowBus {
        &self.resource_flows
    }

    /// Get the progress flow bus for subscribing to progress events.
    pub fn progress_flows(&self) -> &SharedProgressFlowBus {
        &self.progress_flows
    }

    /// Get the elicitation flow bus for subscribing to elicitation requests.
    pub fn elicitation_flows(&self) -> &SharedElicitationFlowBus {
        &self.elicitation_flows
    }

    /// Get the logging flow bus for subscribing to logging events.
    pub fn logging_flows(&self) -> &SharedLoggingFlowBus {
        &self.logging_flows
    }

    /// Get the roots advertised to servers.
    pub fn roots(&self) -> Vec<McpRootInfo> {
        self.roots.read().iter().map(|r| McpRootInfo::from(r.clone())).collect()
    }

    /// Set the roots advertised to servers.
    ///
    /// This will notify all connected servers of the change.
    pub async fn set_roots(&self, roots: Vec<McpRootInfo>) {
        let new_roots: Vec<Root> = roots.into_iter().map(Root::from).collect();
        *self.roots.write() = new_roots;
        self.notify_roots_changed().await;
    }

    /// Add a root to the list advertised to servers.
    pub async fn add_root(&self, uri: impl Into<String>, name: Option<String>) {
        let root = Root {
            uri: uri.into(),
            name,
        };
        self.roots.write().push(root);
        self.notify_roots_changed().await;
    }

    /// Remove a root by URI.
    ///
    /// Returns true if the root was found and removed.
    pub async fn remove_root(&self, uri: &str) -> bool {
        let mut roots = self.roots.write();
        let len_before = roots.len();
        roots.retain(|r| r.uri != uri);
        let removed = roots.len() < len_before;
        drop(roots);
        if removed {
            self.notify_roots_changed().await;
        }
        removed
    }

    /// Notify all connected servers that the roots list has changed.
    async fn notify_roots_changed(&self) {
        let server_names: Vec<String> = self.servers.read().keys().cloned().collect();
        for name in server_names {
            if let Some(server_arc) = self.servers.read().get(&name).cloned() {
                let server = server_arc.lock().await;
                if let Err(e) = server.peer().notify_roots_list_changed().await {
                    warn!(server = %name, error = %e, "Failed to notify server of roots change");
                }
            }
        }
    }

    /// Register and connect to an MCP server.
    ///
    /// This spawns the server process, performs the MCP handshake,
    /// and discovers available tools.
    pub async fn register(&self, config: McpServerConfig) -> Result<McpServerInfo, McpPoolError> {
        let name = config.name.clone();

        // Check if already registered
        if self.servers.read().contains_key(&name) {
            return Err(McpPoolError::ServerAlreadyExists(name));
        }

        info!(name = %name, transport = ?config.transport, "Registering MCP server");

        // Create a handler for this specific server with cache and flow access
        let handler = KaijutsuClientHandler::new(
            name.clone(),
            self.cache.clone(),
            self.resource_flows.clone(),
            self.progress_flows.clone(),
            self.elicitation_flows.clone(),
            self.logging_flows.clone(),
            self.roots.clone(),
        );

        // Connect based on transport type
        let service = match config.transport {
            McpTransport::Stdio => {
                let mut cmd = Command::new(&config.command);
                cmd.args(&config.args);
                for (key, value) in &config.env {
                    cmd.env(key, value);
                }
                if let Some(cwd) = &config.cwd {
                    cmd.current_dir(cwd);
                }
                let transport = TokioChildProcess::new(cmd.configure(|_| {}))
                    .map_err(|e| McpPoolError::SpawnError(e.to_string()))?;
                rmcp::serve_client(handler, transport).await
                    .map_err(|e| McpPoolError::InitError(e.to_string()))?
            }
            McpTransport::StreamableHttp => {
                let url = config.url.as_deref()
                    .ok_or_else(|| McpPoolError::InitError(
                        "StreamableHttp transport requires url".into(),
                    ))?;
                let transport = StreamableHttpClientTransport::from_uri(url);
                rmcp::serve_client(handler, transport).await
                    .map_err(|e| McpPoolError::InitError(e.to_string()))?
            }
        };

        // Get server info
        let peer_info = service.peer().peer_info();
        let server_name = peer_info
            .as_ref()
            .map(|i| i.server_info.name.clone())
            .unwrap_or_else(|| "unknown".to_string());
        let server_version = peer_info
            .as_ref()
            .map(|i| i.server_info.version.clone())
            .unwrap_or_else(|| "unknown".to_string());
        let protocol_version = peer_info
            .as_ref()
            .map(|i| i.protocol_version.to_string())
            .unwrap_or_else(|| "unknown".to_string());

        // Discover tools
        let tools_result = service.peer().list_all_tools().await?;
        let tools: Vec<McpToolInfo> = tools_result.into_iter().map(McpToolInfo::from).collect();

        info!(
            name = %name,
            server = %server_name,
            version = %server_version,
            tool_count = tools.len(),
            "MCP server connected"
        );

        let info = McpServerInfo {
            name: name.clone(),
            protocol_version,
            server_name,
            server_version,
            tools: tools.clone(),
        };

        // Store the connected server
        let connected = ConnectedServer {
            config,
            service,
            tools,
        };

        self.servers
            .write()
            .insert(name, Arc::new(Mutex::new(connected)));

        Ok(info)
    }

    /// Unregister and disconnect from an MCP server.
    ///
    /// The service will be cancelled when the `ConnectedServer` is dropped.
    pub async fn unregister(&self, name: &str) -> Result<(), McpPoolError> {
        let _server = self
            .servers
            .write()
            .remove(name)
            .ok_or_else(|| McpPoolError::ServerNotFound(name.to_string()))?;

        info!(name = %name, "Unregistering MCP server");

        // The service will be cancelled when dropped
        Ok(())
    }

    /// List all registered servers.
    pub fn list_servers(&self) -> Vec<String> {
        self.servers.read().keys().cloned().collect()
    }

    /// Get information about a specific server.
    pub async fn get_server_info(&self, name: &str) -> Result<McpServerInfo, McpPoolError> {
        let server_arc = self
            .servers
            .read()
            .get(name)
            .cloned()
            .ok_or_else(|| McpPoolError::ServerNotFound(name.to_string()))?;

        let server = server_arc.lock().await;
        let peer_info = server.service.peer().peer_info();

        Ok(McpServerInfo {
            name: name.to_string(),
            protocol_version: peer_info
                .as_ref()
                .map(|i| i.protocol_version.to_string())
                .unwrap_or_else(|| String::new()),
            server_name: peer_info
                .as_ref()
                .map(|i| i.server_info.name.clone())
                .unwrap_or_default(),
            server_version: peer_info
                .as_ref()
                .map(|i| i.server_info.version.clone())
                .unwrap_or_default(),
            tools: server.tools.clone(),
        })
    }

    /// List all tools from all connected servers.
    ///
    /// Returns tools with fully-qualified names like "git__status".
    pub async fn list_all_tools(&self) -> Vec<(String, McpToolInfo)> {
        let mut all_tools = Vec::new();

        let server_names: Vec<String> = self.servers.read().keys().cloned().collect();

        for name in server_names {
            if let Some(server_arc) = self.servers.read().get(&name).cloned() {
                let server = server_arc.lock().await;
                for tool in &server.tools {
                    let qualified_name = format!("{}__{}", name, tool.name);
                    all_tools.push((qualified_name, tool.clone()));
                }
            }
        }

        all_tools
    }

    /// Call a tool on a specific server.
    ///
    /// # Arguments
    ///
    /// * `server_name` - Name of the MCP server
    /// * `tool_name` - Name of the tool to call
    /// * `arguments` - Tool arguments as a JSON object
    #[tracing::instrument(skip(self, arguments), fields(mcp.server = %server_name, mcp.tool = %tool_name))]
    pub async fn call_tool(
        &self,
        server_name: &str,
        tool_name: &str,
        arguments: JsonValue,
    ) -> Result<CallToolResult, McpPoolError> {
        let server_arc = self
            .servers
            .read()
            .get(server_name)
            .cloned()
            .ok_or_else(|| McpPoolError::ServerNotFound(server_name.to_string()))?;

        let server = server_arc.lock().await;

        // Verify tool exists
        if !server.tools.iter().any(|t| t.name == tool_name) {
            return Err(McpPoolError::ToolNotFound {
                server: server_name.to_string(),
                tool: tool_name.to_string(),
            });
        }

        debug!(
            server = %server_name,
            tool = %tool_name,
            "Calling MCP tool"
        );

        let params = CallToolRequestParams {
            meta: None,
            name: tool_name.to_string().into(),
            arguments: arguments.as_object().cloned(),
            task: None,
        };

        let result = server.peer().call_tool(params).await?;

        Ok(result)
    }

    /// Call a tool using a fully-qualified name like "git__status".
    pub async fn call_tool_qualified(
        &self,
        qualified_name: &str,
        arguments: JsonValue,
    ) -> Result<CallToolResult, McpPoolError> {
        let (server_name, tool_name) = qualified_name.split_once("__").ok_or_else(|| {
            McpPoolError::ToolNotFound {
                server: "".to_string(),
                tool: qualified_name.to_string(),
            }
        })?;

        self.call_tool(server_name, tool_name, arguments).await
    }

    /// Refresh the tool list for a server.
    pub async fn refresh_tools(&self, server_name: &str) -> Result<Vec<McpToolInfo>, McpPoolError> {
        let server_arc = self
            .servers
            .read()
            .get(server_name)
            .cloned()
            .ok_or_else(|| McpPoolError::ServerNotFound(server_name.to_string()))?;

        let mut server = server_arc.lock().await;

        let tools_result = server.peer().list_all_tools().await?;
        let tools: Vec<McpToolInfo> = tools_result.into_iter().map(McpToolInfo::from).collect();

        server.tools = tools.clone();

        Ok(tools)
    }

    // =========================================================================
    // Resource Operations
    // =========================================================================

    /// List all resources from an MCP server.
    ///
    /// Uses cache if available and not stale, otherwise fetches from server.
    /// Results are cached for future calls.
    pub async fn list_resources(&self, server_name: &str) -> Result<Vec<McpResourceInfo>, McpPoolError> {
        // Check cache first
        let cached = self.cache.list_for_server(server_name);
        if !cached.is_empty() {
            debug!(server = %server_name, count = cached.len(), "Returning cached resources");
            return Ok(cached);
        }

        let server_arc = self
            .servers
            .read()
            .get(server_name)
            .cloned()
            .ok_or_else(|| McpPoolError::ServerNotFound(server_name.to_string()))?;

        let server = server_arc.lock().await;

        debug!(server = %server_name, "Fetching resources from MCP server");
        let result = server.peer().list_all_resources().await?;

        // Convert and cache the resources
        let resources: Vec<McpResourceInfo> = result
            .into_iter()
            .map(|r| {
                let info = McpResourceInfo {
                    uri: r.uri.clone(),
                    name: Some(r.name.clone()),
                    description: r.description.clone(),
                    mime_type: r.mime_type.clone(),
                };
                // Cache each resource
                self.cache.insert(server_name, info.clone(), None);
                info
            })
            .collect();

        debug!(
            server = %server_name,
            count = resources.len(),
            "Cached resources from MCP server"
        );

        Ok(resources)
    }

    /// Read a resource from an MCP server.
    ///
    /// Uses cache if available and not stale, otherwise fetches from server.
    /// Results are cached for future calls.
    pub async fn read_resource(
        &self,
        server_name: &str,
        uri: &str,
    ) -> Result<rmcp::model::ResourceContents, McpPoolError> {
        // Check cache for content
        if let Some(cached) = self.cache.get(server_name, uri) {
            if let Some(content_bytes) = &cached.content {
                if let Ok(contents) = serde_json::from_slice::<rmcp::model::ResourceContents>(content_bytes) {
                    debug!(server = %server_name, uri = %uri, "Returning cached resource content");
                    return Ok(contents);
                }
            }
        }

        let server_arc = self
            .servers
            .read()
            .get(server_name)
            .cloned()
            .ok_or_else(|| McpPoolError::ServerNotFound(server_name.to_string()))?;

        let server = server_arc.lock().await;

        debug!(server = %server_name, uri = %uri, "Reading resource from MCP server");
        let result = server
            .peer()
            .read_resource(rmcp::model::ReadResourceRequestParams {
                uri: uri.to_string(),
                meta: None,
            })
            .await?;

        // Get the first content (typical case)
        let contents = result
            .contents
            .into_iter()
            .next()
            .ok_or_else(|| McpPoolError::ToolNotFound {
                server: server_name.to_string(),
                tool: format!("resource:{}", uri),
            })?;

        // Cache the content
        if let Ok(content_bytes) = serde_json::to_vec(&contents) {
            self.cache.update_content(server_name, uri, content_bytes);
        }

        Ok(contents)
    }

    /// Subscribe to resource updates from an MCP server.
    ///
    /// Sends a subscription request to the MCP server and increments the local
    /// subscriber count, which disables TTL-based cache expiration.
    pub async fn subscribe_resource(
        &self,
        server_name: &str,
        uri: &str,
        subscriber_id: &str,
    ) -> Result<(), McpPoolError> {
        let server_arc = self
            .servers
            .read()
            .get(server_name)
            .cloned()
            .ok_or_else(|| McpPoolError::ServerNotFound(server_name.to_string()))?;

        let server = server_arc.lock().await;

        // Send subscription request to the MCP server
        debug!(server = %server_name, uri = %uri, "Sending MCP resource subscription");
        server
            .peer()
            .subscribe(rmcp::model::SubscribeRequestParams {
                uri: uri.to_string(),
                meta: None,
            })
            .await?;

        // Update local subscriber count
        self.cache.add_subscriber(server_name, uri);

        // Publish subscription event
        self.resource_flows.publish(ResourceFlow::Subscribed {
            server: server_name.to_string(),
            uri: uri.to_string(),
            subscriber_id: subscriber_id.to_string(),
        });

        info!(
            server = %server_name,
            uri = %uri,
            subscriber = %subscriber_id,
            "Resource subscription added"
        );

        Ok(())
    }

    /// Unsubscribe from resource updates.
    ///
    /// Sends an unsubscribe request to the MCP server and decrements the local
    /// subscriber count. When count reaches zero, TTL-based cache expiration is re-enabled.
    pub async fn unsubscribe_resource(
        &self,
        server_name: &str,
        uri: &str,
        subscriber_id: &str,
    ) -> Result<(), McpPoolError> {
        let server_arc = self
            .servers
            .read()
            .get(server_name)
            .cloned()
            .ok_or_else(|| McpPoolError::ServerNotFound(server_name.to_string()))?;

        let server = server_arc.lock().await;

        // Send unsubscribe request to the MCP server
        debug!(server = %server_name, uri = %uri, "Sending MCP resource unsubscription");
        server
            .peer()
            .unsubscribe(rmcp::model::UnsubscribeRequestParams {
                uri: uri.to_string(),
                meta: None,
            })
            .await?;

        // Update local subscriber count
        self.cache.remove_subscriber(server_name, uri);

        // Publish unsubscription event
        self.resource_flows.publish(ResourceFlow::Unsubscribed {
            server: server_name.to_string(),
            uri: uri.to_string(),
            subscriber_id: subscriber_id.to_string(),
        });

        info!(
            server = %server_name,
            uri = %uri,
            subscriber = %subscriber_id,
            "Resource subscription removed"
        );

        Ok(())
    }

    // =========================================================================
    // Prompt Operations
    // =========================================================================

    /// List all prompts from an MCP server.
    ///
    /// Results are cached with TTL-based invalidation.
    pub async fn list_prompts(&self, server_name: &str) -> Result<Vec<McpPromptInfo>, McpPoolError> {
        let server_arc = self
            .servers
            .read()
            .get(server_name)
            .cloned()
            .ok_or_else(|| McpPoolError::ServerNotFound(server_name.to_string()))?;

        let server = server_arc.lock().await;

        debug!(server = %server_name, "Fetching prompts from MCP server");
        let result = server.peer().list_all_prompts().await?;

        // Convert and cache the prompts
        let prompts: Vec<McpPromptInfo> = result
            .into_iter()
            .map(|p| {
                let info = McpPromptInfo::from(p);
                // Cache each prompt
                let cache_key = format!("{}:{}", server_name, info.name);
                self.prompt_cache.insert(cache_key, info.clone());
                info
            })
            .collect();

        debug!(
            server = %server_name,
            count = prompts.len(),
            "Cached prompts from MCP server"
        );

        Ok(prompts)
    }

    /// Get a specific prompt from an MCP server with arguments substituted.
    pub async fn get_prompt(
        &self,
        server_name: &str,
        name: &str,
        arguments: Option<std::collections::HashMap<String, String>>,
    ) -> Result<GetPromptResult, McpPoolError> {
        let server_arc = self
            .servers
            .read()
            .get(server_name)
            .cloned()
            .ok_or_else(|| McpPoolError::ServerNotFound(server_name.to_string()))?;

        let server = server_arc.lock().await;

        debug!(server = %server_name, prompt = %name, "Getting prompt from MCP server");

        // Convert HashMap<String, String> to JsonObject
        let args = arguments.map(|map| {
            map.into_iter()
                .map(|(k, v)| (k, serde_json::Value::String(v)))
                .collect::<serde_json::Map<String, serde_json::Value>>()
        });

        let params = GetPromptRequestParams {
            meta: None,
            name: name.to_string(),
            arguments: args,
        };

        let result = server.peer().get_prompt(params).await?;

        Ok(result)
    }

    /// Invalidate the prompt cache for a server.
    ///
    /// Called when receiving `prompts/list_changed` notification.
    pub fn invalidate_prompt_cache(&self, server_name: &str) {
        let prefix = format!("{}:", server_name);
        self.prompt_cache.retain(|k, _| !k.starts_with(&prefix));
        debug!(server = %server_name, "Invalidated prompt cache");
    }

    // =========================================================================
    // Completion Operations
    // =========================================================================

    /// Get completions for a prompt argument or resource URI.
    pub async fn complete(
        &self,
        server_name: &str,
        reference: Reference,
        argument_name: &str,
        argument_value: &str,
    ) -> Result<CompleteResult, McpPoolError> {
        let server_arc = self
            .servers
            .read()
            .get(server_name)
            .cloned()
            .ok_or_else(|| McpPoolError::ServerNotFound(server_name.to_string()))?;

        let server = server_arc.lock().await;

        debug!(
            server = %server_name,
            ref_type = ?reference.reference_type(),
            argument = %argument_name,
            "Getting completions from MCP server"
        );

        let params = CompleteRequestParams {
            meta: None,
            r#ref: reference,
            argument: ArgumentInfo {
                name: argument_name.to_string(),
                value: argument_value.to_string(),
            },
            context: None,
        };

        let result = server.peer().complete(params).await?;

        Ok(result)
    }

    // =========================================================================
    // Logging Operations
    // =========================================================================

    /// Set the logging level for an MCP server.
    pub async fn set_log_level(
        &self,
        server_name: &str,
        level: LoggingLevel,
    ) -> Result<(), McpPoolError> {
        let server_arc = self
            .servers
            .read()
            .get(server_name)
            .cloned()
            .ok_or_else(|| McpPoolError::ServerNotFound(server_name.to_string()))?;

        let server = server_arc.lock().await;

        debug!(server = %server_name, level = ?level, "Setting log level on MCP server");

        let params = SetLevelRequestParams { meta: None, level };
        server.peer().set_level(params).await?;

        Ok(())
    }

    // =========================================================================
    // Elicitation Response
    // =========================================================================

    /// Send a response to a pending elicitation request.
    ///
    /// Iterates through all connected servers to find the one that owns
    /// the request_id, then sends the response to complete the elicitation.
    ///
    /// Returns true if the response was sent successfully, false if the
    /// request was not found (may have timed out or server disconnected).
    pub fn respond_to_elicitation(&self, request_id: &str, response: ElicitationResponse) -> bool {
        let servers = self.servers.read();

        for (_name, server_arc) in servers.iter() {
            // Try to acquire the lock without blocking
            if let Ok(server) = server_arc.try_lock() {
                let handler = server.service.service();
                // Check if this handler owns the request
                if let Some((_, pending)) = handler.pending_elicitations().remove(request_id) {
                    // Found it! Send the response
                    let sent = pending.response_tx.send(response).is_ok();
                    if sent {
                        info!(request_id = %request_id, "Elicitation response sent successfully");
                    } else {
                        warn!(request_id = %request_id, "Elicitation response channel closed");
                    }
                    return sent;
                }
            }
        }

        warn!(request_id = %request_id, "Elicitation request not found in any server handler");
        false
    }

    /// Shutdown all connected servers.
    pub async fn shutdown_all(&self) {
        let names: Vec<String> = self.servers.read().keys().cloned().collect();

        for name in names {
            if let Err(e) = self.unregister(&name).await {
                warn!(name = %name, error = %e, "Failed to unregister server during shutdown");
            }
        }
    }
}

// =============================================================================
// McpToolEngine - ExecutionEngine implementation for MCP tools
// =============================================================================

use crate::tools::{ExecResult, ExecutionEngine};

/// An execution engine that forwards tool calls to an MCP server.
///
/// Each instance represents a single MCP tool (e.g., "git__status").
/// When `execute()` is called, it parses the input as JSON parameters
/// and forwards the call to the appropriate MCP server.
pub struct McpToolEngine {
    /// Reference to the MCP server pool.
    pool: Arc<McpServerPool>,
    /// Server name (e.g., "git").
    server_name: String,
    /// Tool name on the server (e.g., "status").
    tool_name: String,
    /// Fully qualified name for display (e.g., "git__status").
    qualified_name: String,
    /// Tool description.
    description: String,
    /// JSON Schema for tool input parameters.
    input_schema: JsonValue,
}

impl McpToolEngine {
    /// Create a new MCP tool engine.
    ///
    /// # Arguments
    ///
    /// * `pool` - Shared MCP server pool
    /// * `server_name` - Name of the MCP server
    /// * `tool_name` - Name of the tool on that server
    /// * `description` - Tool description for help text
    /// * `input_schema` - JSON Schema for tool input parameters
    pub fn new(
        pool: Arc<McpServerPool>,
        server_name: impl Into<String>,
        tool_name: impl Into<String>,
        description: impl Into<String>,
        input_schema: JsonValue,
    ) -> Self {
        let server_name = server_name.into();
        let tool_name = tool_name.into();
        let qualified_name = format!("{}__{}", server_name, tool_name);
        Self {
            pool,
            server_name,
            tool_name,
            qualified_name,
            description: description.into(),
            input_schema,
        }
    }

    /// Create engines for all tools from a server.
    ///
    /// Returns a vector of (qualified_name, engine) pairs.
    pub fn from_server_tools(
        pool: Arc<McpServerPool>,
        server_name: &str,
        tools: &[McpToolInfo],
    ) -> Vec<(String, Arc<dyn ExecutionEngine>)> {
        tools
            .iter()
            .map(|tool| {
                let qualified_name = format!("{}__{}", server_name, tool.name);
                let engine = Arc::new(Self::new(
                    pool.clone(),
                    server_name,
                    &tool.name,
                    tool.description.clone().unwrap_or_default(),
                    tool.input_schema.clone(),
                )) as Arc<dyn ExecutionEngine>;
                (qualified_name, engine)
            })
            .collect()
    }
}

impl std::fmt::Debug for McpToolEngine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpToolEngine")
            .field("qualified_name", &self.qualified_name)
            .finish()
    }
}

#[async_trait]
impl ExecutionEngine for McpToolEngine {
    fn name(&self) -> &str {
        &self.qualified_name
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn schema(&self) -> Option<serde_json::Value> {
        Some(self.input_schema.clone())
    }

    async fn execute(&self, code: &str) -> anyhow::Result<ExecResult> {
        // Parse the input as JSON
        let arguments: JsonValue = if code.trim().is_empty() {
            JsonValue::Object(serde_json::Map::new())
        } else {
            serde_json::from_str(code).map_err(|e| {
                anyhow::anyhow!("Failed to parse tool arguments as JSON: {}", e)
            })?
        };

        // Call the MCP tool
        match self
            .pool
            .call_tool(&self.server_name, &self.tool_name, arguments)
            .await
        {
            Ok(result) => {
                // Convert MCP result to ExecResult
                let output = result
                    .content
                    .iter()
                    .filter_map(|c| c.as_text().map(|t| t.text.clone()))
                    .collect::<Vec<_>>()
                    .join("\n");

                if result.is_error.unwrap_or(false) {
                    Ok(ExecResult::failure(1, output))
                } else {
                    Ok(ExecResult::success(output))
                }
            }
            Err(e) => Ok(ExecResult::failure(1, e.to_string())),
        }
    }

    async fn is_available(&self) -> bool {
        // Check if the server is still registered
        self.pool.list_servers().contains(&self.server_name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_mcp_server_config_default() {
        let config = McpServerConfig::default();
        assert!(config.name.is_empty());
        assert!(config.args.is_empty());
        assert_eq!(config.transport, McpTransport::Stdio);
        assert!(config.url.is_none());
    }

    #[test]
    fn test_mcp_pool_creation() {
        let pool = McpServerPool::new();
        assert!(pool.list_servers().is_empty());
    }

    #[tokio::test]
    async fn test_server_not_found() {
        let pool = McpServerPool::new();
        let result = pool.get_server_info("nonexistent").await;
        assert!(matches!(result, Err(McpPoolError::ServerNotFound(_))));
    }

    #[tokio::test]
    async fn test_tool_qualified_name_parsing() {
        let pool = McpServerPool::new();

        // Should fail because server doesn't exist, but parsing should work
        let result = pool.call_tool_qualified("git__status", serde_json::json!({})).await;
        assert!(matches!(result, Err(McpPoolError::ServerNotFound(_))));

        // Invalid format (no double-underscore separator)
        let result = pool.call_tool_qualified("invalid_name", serde_json::json!({})).await;
        assert!(matches!(result, Err(McpPoolError::ToolNotFound { .. })));
    }
}
