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

use crate::flows::{OpSource, ResourceFlow, SharedResourceFlowBus};

use rmcp::model::{CallToolRequestParams, CallToolResult, ClientInfo, Tool as McpTool};
use rmcp::service::{RunningService, ServiceError};
use rmcp::transport::{ConfigureCommandExt, TokioChildProcess};
use rmcp::{ClientHandler, Peer, RoleClient};

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

/// Configuration for an MCP server.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpServerConfig {
    /// Unique name for this server (e.g., "git", "exa").
    pub name: String,
    /// Command to run (e.g., "uvx", "npx", "/path/to/server").
    pub command: String,
    /// Arguments for the command.
    #[serde(default)]
    pub args: Vec<String>,
    /// Environment variables to set.
    #[serde(default)]
    pub env: HashMap<String, String>,
    /// Working directory for the server.
    pub cwd: Option<String>,
}

impl Default for McpServerConfig {
    fn default() -> Self {
        Self {
            name: String::new(),
            command: String::new(),
            args: Vec::new(),
            env: HashMap::new(),
            cwd: None,
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

/// Client handler for kaijutsu's MCP connections.
///
/// Each handler is associated with a specific MCP server and has access to
/// the shared resource cache and flow bus for push-based resource updates.
#[derive(Debug, Clone)]
pub struct KaijutsuClientHandler {
    /// Client info to report to servers.
    client_info: ClientInfo,
    /// The MCP server name this handler is for.
    server_name: String,
    /// Shared resource cache for invalidation on notifications.
    cache: Arc<ResourceCache>,
    /// Flow bus for publishing resource events.
    resource_flows: SharedResourceFlowBus,
}

impl KaijutsuClientHandler {
    /// Create a new handler for a specific server.
    pub fn new(
        server_name: impl Into<String>,
        cache: Arc<ResourceCache>,
        resource_flows: SharedResourceFlowBus,
    ) -> Self {
        let mut info = ClientInfo::default();
        info.client_info.name = "kaijutsu".into();
        info.client_info.version = env!("CARGO_PKG_VERSION").into();
        Self {
            client_info: info,
            server_name: server_name.into(),
            cache,
            resource_flows,
        }
    }

    /// Create a default handler without cache/flow support (for testing).
    pub fn default_handler() -> Self {
        let mut info = ClientInfo::default();
        info.client_info.name = "kaijutsu".into();
        info.client_info.version = env!("CARGO_PKG_VERSION").into();
        Self {
            client_info: info,
            server_name: String::new(),
            cache: Arc::new(ResourceCache::new()),
            resource_flows: crate::flows::shared_resource_flow_bus(64),
        }
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
        async move {
            debug!(
                level = ?params.level,
                logger = ?params.logger,
                "MCP log: {:?}",
                params.data
            );
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
}

impl Default for McpServerPool {
    fn default() -> Self {
        Self::new()
    }
}

impl McpServerPool {
    /// Create a new empty pool.
    pub fn new() -> Self {
        Self {
            servers: RwLock::new(HashMap::new()),
            cache: Arc::new(ResourceCache::new()),
            resource_flows: crate::flows::shared_resource_flow_bus(256),
        }
    }

    /// Create a new pool with a custom resource flow bus.
    pub fn with_resource_flows(resource_flows: SharedResourceFlowBus) -> Self {
        Self {
            servers: RwLock::new(HashMap::new()),
            cache: Arc::new(ResourceCache::new()),
            resource_flows,
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

        info!(name = %name, command = %config.command, "Registering MCP server");

        // Build the command
        let mut cmd = Command::new(&config.command);
        cmd.args(&config.args);

        // Set environment variables
        for (key, value) in &config.env {
            cmd.env(key, value);
        }

        // Set working directory if specified
        if let Some(cwd) = &config.cwd {
            cmd.current_dir(cwd);
        }

        // Spawn the process with stdio transport
        let transport = TokioChildProcess::new(cmd.configure(|_| {}))
            .map_err(|e| McpPoolError::SpawnError(e.to_string()))?;

        // Create a handler for this specific server with cache and flow access
        let handler = KaijutsuClientHandler::new(
            name.clone(),
            self.cache.clone(),
            self.resource_flows.clone(),
        );

        // Connect and perform handshake
        let service = rmcp::serve_client(handler, transport)
            .await
            .map_err(|e| McpPoolError::InitError(e.to_string()))?;

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
    /// Returns tools with fully-qualified names like "git:status".
    pub async fn list_all_tools(&self) -> Vec<(String, McpToolInfo)> {
        let mut all_tools = Vec::new();

        let server_names: Vec<String> = self.servers.read().keys().cloned().collect();

        for name in server_names {
            if let Some(server_arc) = self.servers.read().get(&name).cloned() {
                let server = server_arc.lock().await;
                for tool in &server.tools {
                    let qualified_name = format!("{}:{}", name, tool.name);
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

    /// Call a tool using a fully-qualified name like "git:status".
    pub async fn call_tool_qualified(
        &self,
        qualified_name: &str,
        arguments: JsonValue,
    ) -> Result<CallToolResult, McpPoolError> {
        let (server_name, tool_name) = qualified_name.split_once(':').ok_or_else(|| {
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
/// Each instance represents a single MCP tool (e.g., "git:status").
/// When `execute()` is called, it parses the input as JSON parameters
/// and forwards the call to the appropriate MCP server.
pub struct McpToolEngine {
    /// Reference to the MCP server pool.
    pool: Arc<McpServerPool>,
    /// Server name (e.g., "git").
    server_name: String,
    /// Tool name on the server (e.g., "status").
    tool_name: String,
    /// Fully qualified name for display (e.g., "git:status").
    qualified_name: String,
    /// Tool description.
    description: String,
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
    pub fn new(
        pool: Arc<McpServerPool>,
        server_name: impl Into<String>,
        tool_name: impl Into<String>,
        description: impl Into<String>,
    ) -> Self {
        let server_name = server_name.into();
        let tool_name = tool_name.into();
        let qualified_name = format!("{}:{}", server_name, tool_name);
        Self {
            pool,
            server_name,
            tool_name,
            qualified_name,
            description: description.into(),
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
                let qualified_name = format!("{}:{}", server_name, tool.name);
                let engine = Arc::new(Self::new(
                    pool.clone(),
                    server_name,
                    &tool.name,
                    tool.description.clone().unwrap_or_default(),
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
        let result = pool.call_tool_qualified("git:status", serde_json::json!({})).await;
        assert!(matches!(result, Err(McpPoolError::ServerNotFound(_))));

        // Invalid format (no colon)
        let result = pool.call_tool_qualified("invalid_name", serde_json::json!({})).await;
        assert!(matches!(result, Err(McpPoolError::ToolNotFound { .. })));
    }
}
