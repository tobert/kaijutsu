//! ConfigCrdtBackend: CRDT-backed configuration files.
//!
//! This backend manages config files (theme.rhai, layouts/*.ron, seats/*.rhai)
//! as CRDT documents for collaborative editing. The CRDT is the source of truth;
//! disk files exist for external editors and backup.
//!
//! # Architecture
//!
//! ```text
//! ┌─────────────────────────────────────────────────────────────┐
//! │                    CRDT (source of truth)                   │
//! │        agents, Claude, kaish, rhai all edit here            │
//! └───────────────────────┬─────────────────────────────────────┘
//!                         │
//!           ┌─────────────┴─────────────┐
//!           ▼                           ▼
//!    debounced flush              notify watch
//!           │                           │
//!           ▼                           ▼
//! ┌─────────────────────────────────────────────────────────────┐
//! │                 Disk files (for external editing)           │
//! │              ~/.config/kaijutsu/theme.rhai etc.             │
//! └─────────────────────────────────────────────────────────────┘
//! ```
//!
//! # Document Naming
//!
//! Config documents use the prefix `config:`:
//! - `config:theme.rhai` — Base theme
//! - `config:seats/amy-desktop.rhai` — Seat-specific overrides
//!
//! # Multi-Seat Architecture
//!
//! When multiple computers connect to the same kernel, each needs its own
//! UI config (font size, DPI, layout proportions) while sharing semantic
//! config (colors, styles).
//!
//! Base config is in `theme.rhai`, seat overrides in `seats/{seat_id}.rhai`.
//! The merge happens at apply time: seat values override base values.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use dashmap::DashMap;
use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use parking_lot::RwLock;
use tokio::sync::mpsc;

use kaijutsu_crdt::{BlockId, BlockKind, Role};

use crate::block_store::SharedBlockStore;
use crate::db::DocumentKind;
use crate::flows::{ConfigFlow, ConfigSource, OpSource, SharedConfigFlowBus};

/// Embedded default theme content.
pub const DEFAULT_THEME: &str = include_str!("../../../assets/defaults/theme.rhai");

/// Embedded default LLM configuration.
pub const DEFAULT_LLM_CONFIG: &str = include_str!("../../../assets/defaults/llm.rhai");

/// Embedded example seat config.
pub const EXAMPLE_SEAT: &str = include_str!("../../../assets/defaults/seats/example.rhai");

/// Embedded default MCP server configuration.
pub const DEFAULT_MCP_CONFIG: &str = include_str!("../../../assets/defaults/mcp.rhai");

/// Tracks dirty config files that need flushing to disk.
struct DirtyTracker {
    /// Files marked dirty, with timestamp of last modification.
    files: DashMap<String, Instant>, // config path -> last_modified
    /// Debounce duration.
    debounce: Duration,
}

impl DirtyTracker {
    fn new(debounce: Duration) -> Self {
        Self {
            files: DashMap::new(),
            debounce,
        }
    }

    fn mark_dirty(&self, path: &str) {
        self.files.insert(path.to_string(), Instant::now());
    }

    fn get_flushable(&self) -> Vec<String> {
        let now = Instant::now();
        self.files
            .iter()
            .filter(|entry| now.duration_since(*entry.value()) >= self.debounce)
            .map(|entry| entry.key().clone())
            .collect()
    }

    fn mark_flushed(&self, path: &str) {
        self.files.remove(path);
    }
}

/// File change event from the watcher.
#[derive(Debug, Clone)]
pub struct ConfigFileChange {
    /// Relative path within config directory.
    pub path: String,
    /// Kind of change.
    pub kind: ConfigChangeKind,
}

/// Kind of config file change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigChangeKind {
    /// File was created.
    Created,
    /// File was modified.
    Modified,
    /// File was deleted.
    Deleted,
}

/// Handle to a running file watcher.
pub struct ConfigWatcherHandle {
    /// The watcher itself (keep alive to continue watching).
    _watcher: RecommendedWatcher,
    /// Sender to signal shutdown.
    shutdown_tx: tokio::sync::oneshot::Sender<()>,
}

impl ConfigWatcherHandle {
    /// Stop the watcher.
    pub fn stop(self) {
        let _ = self.shutdown_tx.send(());
    }
}

/// Configuration validation result.
#[derive(Debug, Clone)]
pub struct ValidationResult {
    /// Whether validation passed.
    pub valid: bool,
    /// Error message if validation failed.
    pub error: Option<String>,
    /// Warnings (validation passed but has issues).
    pub warnings: Vec<String>,
}

impl ValidationResult {
    fn ok() -> Self {
        Self {
            valid: true,
            error: None,
            warnings: vec![],
        }
    }

    fn error(msg: impl Into<String>) -> Self {
        Self {
            valid: false,
            error: Some(msg.into()),
            warnings: vec![],
        }
    }
}

/// Error type for config operations.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Config not found: {0}")]
    NotFound(String),

    #[error("Validation failed: {0}")]
    ValidationFailed(String),

    #[error("CRDT error: {0}")]
    Crdt(String),

    #[error("Consent required for config changes in collaborative mode")]
    ConsentRequired,
}

/// CRDT-backed configuration backend.
///
/// Manages config files as CRDT documents with file system synchronization.
pub struct ConfigCrdtBackend {
    /// CRDT document/block storage.
    blocks: SharedBlockStore,
    /// Root config directory (~/.config/kaijutsu/).
    config_root: PathBuf,
    /// Dirty file tracker for debounced flushing.
    dirty: DirtyTracker,
    /// Channel for file change events from the watcher.
    watcher_event_tx: mpsc::Sender<ConfigFileChange>,
    /// Receiver for file change events (moved to watcher task).
    watcher_event_rx: RwLock<Option<mpsc::Receiver<ConfigFileChange>>>,
    /// FlowBus for config events.
    config_flows: Option<SharedConfigFlowBus>,
    /// In-progress flush paths (to prevent echo loops).
    flushing: DashMap<String, ()>,
}

impl ConfigCrdtBackend {
    /// Create a new config backend.
    pub fn new(blocks: SharedBlockStore, config_root: PathBuf) -> Self {
        let (tx, rx) = mpsc::channel(256);

        Self {
            blocks,
            config_root,
            dirty: DirtyTracker::new(Duration::from_millis(500)),
            watcher_event_tx: tx,
            watcher_event_rx: RwLock::new(Some(rx)),
            config_flows: None,
            flushing: DashMap::new(),
        }
    }

    /// Create with FlowBus for config events.
    pub fn with_flows(
        blocks: SharedBlockStore,
        config_root: PathBuf,
        config_flows: SharedConfigFlowBus,
    ) -> Self {
        let (tx, rx) = mpsc::channel(256);

        Self {
            blocks,
            config_root,
            dirty: DirtyTracker::new(Duration::from_millis(500)),
            watcher_event_tx: tx,
            watcher_event_rx: RwLock::new(Some(rx)),
            config_flows: Some(config_flows),
            flushing: DashMap::new(),
        }
    }

    /// Get the config root directory.
    pub fn config_root(&self) -> &Path {
        &self.config_root
    }

    /// Document ID for a config path.
    fn doc_id(&self, path: &str) -> String {
        format!("config:{}", path)
    }

    /// Block ID for a config file (one block per config file).
    fn block_id(&self, doc_id: &str) -> BlockId {
        // Use a stable identifier - config files have a single block
        BlockId::new(doc_id, "config", 0)
    }

    /// Emit a ConfigFlow event.
    fn emit(&self, flow: ConfigFlow) {
        if let Some(bus) = &self.config_flows {
            bus.publish(flow);
        }
    }

    /// Ensure a config file exists, loading from disk or creating from default.
    ///
    /// Returns the source from which the config was loaded.
    pub async fn ensure_config(&self, path: &str) -> Result<ConfigSource, ConfigError> {
        let doc_id = self.doc_id(path);

        // Check if already loaded in CRDT
        if self.blocks.contains(&doc_id) {
            tracing::debug!(path = %path, "config already loaded");
            return Ok(ConfigSource::Crdt);
        }

        // Try to load from disk
        let disk_path = self.config_root.join(path);
        let (content, source) = if disk_path.exists() {
            let content = tokio::fs::read_to_string(&disk_path).await?;
            (content, ConfigSource::Disk)
        } else {
            // Use embedded default
            let default_content = self.get_default_content(path);
            if let Some(content) = default_content {
                // Write default to disk for future editing
                if let Some(parent) = disk_path.parent() {
                    tokio::fs::create_dir_all(parent).await?;
                }
                tokio::fs::write(&disk_path, &content).await?;
                tracing::info!(path = %path, "created config from default");
                (content, ConfigSource::Default)
            } else {
                return Err(ConfigError::NotFound(path.to_string()));
            }
        };

        // Create CRDT document with content
        self.blocks
            .create_document(doc_id.clone(), DocumentKind::Config, None)
            .map_err(|e| ConfigError::Crdt(e))?;

        // Insert content as a single block
        self.blocks
            .insert_block(
                &doc_id,
                None, // no parent
                None, // at end
                Role::System,
                BlockKind::Text,
                &content,
            )
            .map_err(|e| ConfigError::Crdt(e))?;

        // Emit loaded event
        self.emit(ConfigFlow::Loaded {
            path: path.to_string(),
            source,
            content: content.clone(),
        });

        tracing::info!(path = %path, source = %source, "loaded config");
        Ok(source)
    }

    /// Get default content for a config path.
    fn get_default_content(&self, path: &str) -> Option<String> {
        match path {
            "theme.rhai" => Some(DEFAULT_THEME.to_string()),
            "llm.rhai" => Some(DEFAULT_LLM_CONFIG.to_string()),
            "mcp.rhai" => Some(DEFAULT_MCP_CONFIG.to_string()),
            p if p.starts_with("seats/") && p.ends_with(".rhai") => {
                // Generate seat-specific default from template
                Some(EXAMPLE_SEAT.to_string())
            }
            _ => None,
        }
    }

    /// Reload a config file from disk, discarding CRDT changes.
    ///
    /// This is the safety valve for when CRDT gets into a bad state.
    pub async fn reload_from_disk(&self, path: &str) -> Result<(), ConfigError> {
        let doc_id = self.doc_id(path);
        let disk_path = self.config_root.join(path);

        if !disk_path.exists() {
            return Err(ConfigError::NotFound(path.to_string()));
        }

        let content = tokio::fs::read_to_string(&disk_path).await?;

        // Get or create document
        if !self.blocks.contains(&doc_id) {
            self.blocks
                .create_document(doc_id.clone(), DocumentKind::Config, None)
                .map_err(|e| ConfigError::Crdt(e))?;
        }

        let block_id = self.block_id(&doc_id);

        // Check if block exists
        let block_exists = if let Some(entry) = self.blocks.get(&doc_id) {
            entry.doc.blocks_ordered().iter().any(|b| b.id == block_id)
        } else {
            false
        };

        if block_exists {
            // Replace content
            let current_len = {
                let entry = self.blocks.get(&doc_id).unwrap();
                let blocks = entry.doc.blocks_ordered();
                blocks
                    .iter()
                    .find(|b| b.id == block_id)
                    .map(|b| b.content.len())
                    .unwrap_or(0)
            };

            self.blocks
                .edit_text(&doc_id, &block_id, 0, &content, current_len)
                .map_err(|e| ConfigError::Crdt(e))?;
        } else {
            // Create new block
            self.blocks
                .insert_block(&doc_id, None, None, Role::System, BlockKind::Text, &content)
                .map_err(|e| ConfigError::Crdt(e))?;
        }

        // Emit loaded event
        self.emit(ConfigFlow::Loaded {
            path: path.to_string(),
            source: ConfigSource::Disk,
            content,
        });

        tracing::info!(path = %path, "reloaded config from disk");
        Ok(())
    }

    /// Reset a config file to embedded default.
    pub async fn reset_to_default(&self, path: &str) -> Result<(), ConfigError> {
        let default_content = self
            .get_default_content(path)
            .ok_or_else(|| ConfigError::NotFound(format!("no default for {}", path)))?;

        let doc_id = self.doc_id(path);
        let block_id = self.block_id(&doc_id);

        // Get or create document
        if !self.blocks.contains(&doc_id) {
            self.blocks
                .create_document(doc_id.clone(), DocumentKind::Config, None)
                .map_err(|e| ConfigError::Crdt(e))?;
        }

        // Check if block exists
        let block_exists = if let Some(entry) = self.blocks.get(&doc_id) {
            entry.doc.blocks_ordered().iter().any(|b| b.id == block_id)
        } else {
            false
        };

        if block_exists {
            // Replace content
            let current_len = {
                let entry = self.blocks.get(&doc_id).unwrap();
                let blocks = entry.doc.blocks_ordered();
                blocks
                    .iter()
                    .find(|b| b.id == block_id)
                    .map(|b| b.content.len())
                    .unwrap_or(0)
            };

            self.blocks
                .edit_text(&doc_id, &block_id, 0, &default_content, current_len)
                .map_err(|e| ConfigError::Crdt(e))?;
        } else {
            // Create new block
            self.blocks
                .insert_block(
                    &doc_id,
                    None,
                    None,
                    Role::System,
                    BlockKind::Text,
                    &default_content,
                )
                .map_err(|e| ConfigError::Crdt(e))?;
        }

        // Write to disk
        let disk_path = self.config_root.join(path);
        if let Some(parent) = disk_path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        tokio::fs::write(&disk_path, &default_content).await?;

        // Emit reset event
        self.emit(ConfigFlow::Reset {
            path: path.to_string(),
        });

        tracing::info!(path = %path, "reset config to default");
        Ok(())
    }

    /// Get the current content of a config file.
    ///
    /// Config documents have a single block containing the entire file content.
    pub fn get_content(&self, path: &str) -> Result<String, ConfigError> {
        let doc_id = self.doc_id(path);

        let entry = self
            .blocks
            .get(&doc_id)
            .ok_or_else(|| ConfigError::NotFound(path.to_string()))?;

        // Config documents have a single block - get the first one
        let blocks = entry.doc.blocks_ordered();
        let block = blocks
            .first()
            .ok_or_else(|| ConfigError::NotFound(path.to_string()))?;

        Ok(block.content.clone())
    }

    /// Validate config content (Rhai syntax check).
    pub fn validate(&self, path: &str, content: &str) -> ValidationResult {
        if path.ends_with(".rhai") {
            // Parse Rhai to check syntax
            let engine = rhai::Engine::new();
            match engine.compile(content) {
                Ok(_) => ValidationResult::ok(),
                Err(e) => ValidationResult::error(format!("Rhai syntax error: {}", e)),
            }
        } else if path.ends_with(".ron") {
            // Parse RON to check syntax
            match ron::from_str::<ron::Value>(content) {
                Ok(_) => ValidationResult::ok(),
                Err(e) => ValidationResult::error(format!("RON syntax error: {}", e)),
            }
        } else {
            // Unknown format, accept anything
            ValidationResult::ok()
        }
    }

    /// Flush a config file from CRDT to disk.
    ///
    /// Validates before writing. Returns validation result.
    pub async fn flush_to_disk(&self, path: &str) -> Result<ValidationResult, ConfigError> {
        let content = self.get_content(path)?;

        // Validate before writing
        let validation = self.validate(path, &content);
        if !validation.valid {
            // Emit validation failed event
            self.emit(ConfigFlow::ValidationFailed {
                path: path.to_string(),
                error: validation.error.clone().unwrap_or_default(),
                content: content.clone(),
            });
            return Ok(validation);
        }

        // Mark as flushing to prevent echo from watcher
        self.flushing.insert(path.to_string(), ());

        // Write to disk
        let disk_path = self.config_root.join(path);
        if let Some(parent) = disk_path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        tokio::fs::write(&disk_path, &content).await?;

        // Clear flushing flag after a delay (to handle watcher delay)
        let path_clone = path.to_string();
        let flushing = self.flushing.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(200)).await;
            flushing.remove(&path_clone);
        });

        self.dirty.mark_flushed(path);
        tracing::debug!(path = %path, "flushed config to disk");

        Ok(validation)
    }

    /// Flush all dirty config files to disk.
    pub async fn flush_all(&self) -> Result<(), ConfigError> {
        let flushable = self.dirty.get_flushable();

        for path in flushable {
            if let Err(e) = self.flush_to_disk(&path).await {
                tracing::warn!(path = %path, error = %e, "failed to flush config");
            }
        }

        Ok(())
    }

    /// Mark a config path as dirty (needs flushing).
    pub fn mark_dirty(&self, path: &str) {
        self.dirty.mark_dirty(path);
    }

    /// List all loaded config documents.
    pub fn list_configs(&self) -> Vec<String> {
        self.blocks
            .list_ids()
            .into_iter()
            .filter_map(|id| {
                if id.starts_with("config:") {
                    Some(id.strip_prefix("config:").unwrap().to_string())
                } else {
                    None
                }
            })
            .collect()
    }

    /// Start the file watcher for the config directory.
    ///
    /// Watches for external changes (from editors) and syncs them to CRDT.
    pub fn start_watcher(
        self: &std::sync::Arc<Self>,
    ) -> Result<ConfigWatcherHandle, ConfigError> {
        let backend = std::sync::Arc::clone(self);
        let tx = self.watcher_event_tx.clone();
        let config_root = self.config_root.clone();

        // Create the file watcher
        let mut watcher = RecommendedWatcher::new(
            move |result: Result<Event, notify::Error>| {
                if let Ok(event) = result {
                    let kind = match event.kind {
                        EventKind::Create(_) => Some(ConfigChangeKind::Created),
                        EventKind::Modify(_) => Some(ConfigChangeKind::Modified),
                        EventKind::Remove(_) => Some(ConfigChangeKind::Deleted),
                        _ => None,
                    };

                    if let Some(kind) = kind {
                        for path in event.paths {
                            if let Ok(rel_path) = path.strip_prefix(&config_root) {
                                let path_str = rel_path.to_string_lossy().to_string();

                                // Only watch .rhai and .ron files
                                if path_str.ends_with(".rhai") || path_str.ends_with(".ron") {
                                    let _ = tx.try_send(ConfigFileChange {
                                        path: path_str,
                                        kind,
                                    });
                                }
                            }
                        }
                    }
                }
            },
            notify::Config::default().with_poll_interval(Duration::from_millis(500)),
        )
        .map_err(|e| ConfigError::Io(std::io::Error::new(std::io::ErrorKind::Other, e)))?;

        // Create config directory if it doesn't exist
        std::fs::create_dir_all(&self.config_root)?;

        // Watch the config directory
        watcher
            .watch(&self.config_root, RecursiveMode::Recursive)
            .map_err(|e| ConfigError::Io(std::io::Error::new(std::io::ErrorKind::Other, e)))?;

        // Create shutdown channel
        let (shutdown_tx, mut shutdown_rx) = tokio::sync::oneshot::channel();

        // Take the event receiver
        let rx = self
            .watcher_event_rx
            .write()
            .take()
            .ok_or_else(|| ConfigError::Io(std::io::Error::new(
                std::io::ErrorKind::Other,
                "watcher already started",
            )))?;

        // Spawn the event processor task
        tokio::spawn(async move {
            let mut rx = rx;
            let mut debounce_map: std::collections::HashMap<String, Instant> =
                std::collections::HashMap::new();
            let debounce_duration = Duration::from_millis(100);

            loop {
                tokio::select! {
                    _ = &mut shutdown_rx => {
                        tracing::info!("config watcher shutting down");
                        break;
                    }
                    Some(event) = rx.recv() => {
                        // Skip if we're currently flushing this file (echo prevention)
                        if backend.flushing.contains_key(&event.path) {
                            continue;
                        }

                        // Debounce: skip if we saw this file very recently
                        let now = Instant::now();
                        if let Some(last) = debounce_map.get(&event.path) {
                            if now.duration_since(*last) < debounce_duration {
                                continue;
                            }
                        }
                        debounce_map.insert(event.path.clone(), now);

                        // Process the event
                        if let Err(e) = backend.sync_external_change(&event).await {
                            tracing::warn!(
                                path = %event.path,
                                error = %e,
                                "failed to sync external config change"
                            );
                        } else {
                            tracing::debug!(
                                path = %event.path,
                                kind = ?event.kind,
                                "synced external config change to CRDT"
                            );
                        }
                    }
                }
            }
        });

        tracing::info!(path = %self.config_root.display(), "config watcher started");

        Ok(ConfigWatcherHandle {
            _watcher: watcher,
            shutdown_tx,
        })
    }

    /// Sync an external config file change to CRDT.
    async fn sync_external_change(&self, event: &ConfigFileChange) -> Result<(), ConfigError> {
        let doc_id = self.doc_id(&event.path);
        let block_id = self.block_id(&doc_id);

        match event.kind {
            ConfigChangeKind::Created | ConfigChangeKind::Modified => {
                // Read file from disk
                let disk_path = self.config_root.join(&event.path);
                let content = match tokio::fs::read_to_string(&disk_path).await {
                    Ok(c) => c,
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
                    Err(e) => return Err(ConfigError::Io(e)),
                };

                // Get or create document
                if !self.blocks.contains(&doc_id) {
                    self.blocks
                        .create_document(doc_id.clone(), DocumentKind::Config, None)
                        .map_err(|e| ConfigError::Crdt(e))?;
                }

                // Check if block exists
                let block_exists = if let Some(entry) = self.blocks.get(&doc_id) {
                    entry.doc.blocks_ordered().iter().any(|b| b.id == block_id)
                } else {
                    false
                };

                if block_exists {
                    // Replace content
                    let current_len = {
                        let entry = self.blocks.get(&doc_id).unwrap();
                        let blocks = entry.doc.blocks_ordered();
                        blocks
                            .iter()
                            .find(|b| b.id == block_id)
                            .map(|b| b.content.len())
                            .unwrap_or(0)
                    };

                    self.blocks
                        .edit_text(&doc_id, &block_id, 0, &content, current_len)
                        .map_err(|e| ConfigError::Crdt(e))?;
                } else {
                    // Create new block
                    self.blocks
                        .insert_block(&doc_id, None, None, Role::System, BlockKind::Text, &content)
                        .map_err(|e| ConfigError::Crdt(e))?;
                }

                // Emit changed event (source is Remote since it came from external edit)
                self.emit(ConfigFlow::Changed {
                    path: event.path.clone(),
                    ops: vec![], // TODO: Could include CRDT ops here
                    source: OpSource::Remote,
                });
            }
            ConfigChangeKind::Deleted => {
                // For config files, we don't delete the CRDT document
                // Just mark it as needing reload from default
                tracing::warn!(path = %event.path, "config file deleted externally");
            }
        }

        Ok(())
    }

    /// Ensure seat config exists for a seat ID.
    ///
    /// Creates from template if it doesn't exist.
    pub async fn ensure_seat_config(&self, seat_id: &str) -> Result<ConfigSource, ConfigError> {
        let path = format!("seats/{}.rhai", seat_id);
        self.ensure_config(&path).await
    }

    /// Get merged config content (base + seat overrides).
    ///
    /// Returns the base content if seat config doesn't exist.
    pub fn get_merged_content(&self, seat_id: Option<&str>) -> Result<String, ConfigError> {
        let base = self.get_content("theme.rhai")?;

        if let Some(seat_id) = seat_id {
            let seat_path = format!("seats/{}.rhai", seat_id);
            if let Ok(seat_content) = self.get_content(&seat_path) {
                // Return concatenated content - Rhai will handle variable shadowing
                // Later values override earlier ones
                return Ok(format!("// Base theme\n{}\n\n// Seat overrides: {}\n{}", base, seat_id, seat_content));
            }
        }

        Ok(base)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::block_store::shared_block_store;
    use tempfile::TempDir;

    #[tokio::test]
    async fn test_ensure_config_creates_default() {
        let blocks = shared_block_store("test");
        let temp_dir = TempDir::new().unwrap();
        let backend = ConfigCrdtBackend::new(blocks.clone(), temp_dir.path().to_path_buf());

        // theme.rhai should be created from default
        let result = backend.ensure_config("theme.rhai").await;
        println!("ensure_config result: {:?}", result);
        let source = result.unwrap();
        assert_eq!(source, ConfigSource::Default);

        // Check if document was created
        let doc_id = backend.doc_id("theme.rhai");
        println!("doc_id: {}", doc_id);
        println!("documents exist: {}", blocks.contains(&doc_id));

        // Should now be in CRDT
        let content_result = backend.get_content("theme.rhai");
        println!("get_content result: {:?}", content_result);
        let content = content_result.unwrap();
        assert!(!content.is_empty());

        // File should exist on disk
        assert!(temp_dir.path().join("theme.rhai").exists());
    }

    #[tokio::test]
    async fn test_reload_from_disk() {
        let blocks = shared_block_store("test");
        let temp_dir = TempDir::new().unwrap();
        let backend = ConfigCrdtBackend::new(blocks, temp_dir.path().to_path_buf());

        // Create initial config
        backend.ensure_config("theme.rhai").await.unwrap();

        // Modify the disk file directly
        let disk_path = temp_dir.path().join("theme.rhai");
        std::fs::write(&disk_path, "let custom_value = 42;").unwrap();

        // Reload should pick up the change
        backend.reload_from_disk("theme.rhai").await.unwrap();

        let content = backend.get_content("theme.rhai").unwrap();
        assert!(content.contains("custom_value"));
    }

    #[tokio::test]
    async fn test_reset_to_default() {
        let blocks = shared_block_store("test");
        let temp_dir = TempDir::new().unwrap();
        let backend = ConfigCrdtBackend::new(blocks, temp_dir.path().to_path_buf());

        // Create config and modify it
        backend.ensure_config("theme.rhai").await.unwrap();

        // Get original default content
        let original = DEFAULT_THEME.to_string();

        // Reset
        backend.reset_to_default("theme.rhai").await.unwrap();

        let content = backend.get_content("theme.rhai").unwrap();
        assert_eq!(content, original);
    }

    #[test]
    fn test_validation_rhai() {
        let blocks = shared_block_store("test");
        let temp_dir = tempfile::TempDir::new().unwrap();
        let backend = ConfigCrdtBackend::new(blocks, temp_dir.path().to_path_buf());

        // Valid Rhai
        let result = backend.validate("theme.rhai", "let x = 42;");
        assert!(result.valid);

        // Invalid Rhai
        let result = backend.validate("theme.rhai", "let x = ");
        assert!(!result.valid);
        assert!(result.error.is_some());
    }

    #[test]
    fn test_doc_id() {
        let blocks = shared_block_store("test");
        let temp_dir = tempfile::TempDir::new().unwrap();
        let backend = ConfigCrdtBackend::new(blocks, temp_dir.path().to_path_buf());

        assert_eq!(backend.doc_id("theme.rhai"), "config:theme.rhai");
        assert_eq!(
            backend.doc_id("seats/amy-desktop.rhai"),
            "config:seats/amy-desktop.rhai"
        );
    }

    #[test]
    fn test_default_theme_loaded() {
        // Verify the include_str! loaded actual content
        assert!(!DEFAULT_THEME.is_empty(), "DEFAULT_THEME should not be empty");
        assert!(DEFAULT_THEME.contains("let"), "DEFAULT_THEME should contain Rhai code");
        println!("DEFAULT_THEME length: {}", DEFAULT_THEME.len());
    }
}
