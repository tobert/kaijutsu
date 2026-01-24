//! Embedded kaish executor using KaijutsuBackend.
//!
//! Instead of spawning kaish as a subprocess, this module embeds the kaish
//! interpreter directly, using `KaijutsuBackend` for file operations and
//! tool dispatch.
//!
//! # Architecture
//!
//! ```text
//! kaijutsu-server
//!     │
//!     └── EmbeddedKaish
//!             │
//!             ├── kaish::Kernel (in-process)
//!             │       │
//!             │       └── ExecContext.backend = KaijutsuBackend
//!             │               │
//!             │               ├── File ops → BlockStore (CRDT)
//!             │               └── Tool calls → KaijutsuKernel
//!             │
//!             └── Shared state with kaijutsu kernel
//! ```
//!
//! This enables kaish scripts to read/write blocks as files and call
//! kaijutsu tools directly, without IPC overhead.

use std::sync::Arc;

use anyhow::Result;

use kaish_kernel::interpreter::ExecResult;
use kaish_kernel::{Kernel as KaishKernel, KernelConfig as KaishConfig};

use kaijutsu_kernel::block_store::SharedBlockStore;
use kaijutsu_kernel::Kernel as KaijutsuKernel;

// TODO: Re-enable once path routing is fixed
// use crate::kaish_backend::KaijutsuBackend;

/// Embedded kaish executor backed by CRDT blocks.
///
/// Unlike `KaishProcess` which spawns a subprocess, this embeds the kaish
/// interpreter directly and routes all I/O through `KaijutsuBackend`.
pub struct EmbeddedKaish {
    /// The embedded kaish kernel.
    kernel: KaishKernel,
    /// Kernel name/id.
    name: String,
}

impl EmbeddedKaish {
    /// Create a new embedded kaish executor.
    ///
    /// # Arguments
    ///
    /// * `name` - Name for this kaish kernel (for state persistence)
    /// * `blocks` - Shared block store for CRDT operations
    /// * `kernel` - Kaijutsu kernel for tool dispatch
    ///
    /// # Example
    ///
    /// ```ignore
    /// let blocks = shared_block_store("agent-1");
    /// let kernel = Arc::new(KaijutsuKernel::new("agent-1").await);
    /// let kaish = EmbeddedKaish::new("my-kernel", blocks, kernel)?;
    /// let result = kaish.execute("echo hello").await?;
    /// ```
    pub fn new(
        name: &str,
        _blocks: SharedBlockStore,
        _kernel: Arc<KaijutsuKernel>,
    ) -> Result<Self> {
        // TODO: Re-enable KaijutsuBackend for CRDT block integration once path routing is fixed
        // For now, use a standard kaish kernel with local filesystem access
        // let backend: Arc<dyn KernelBackend> = Arc::new(KaijutsuBackend::new(blocks, kernel));

        // Configure kaish kernel
        // TODO: local_root should come from kernel config / mounts, not hardcoded
        // Mounts: /mnt/local -> $HOME/src, /tmp -> MemoryFs (built into kaish)
        let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
        let src_dir = std::path::PathBuf::from(&home).join("src");
        let config = KaishConfig {
            name: name.to_string(),
            persist: false, // kaijutsu handles persistence
            mount_local: true,
            local_root: Some(src_dir),
            cwd: std::path::PathBuf::from("/mnt/local"),
        };

        // Create standard kaish kernel (mounts local fs at /mnt/local)
        let kernel = KaishKernel::new(config)?;

        Ok(Self {
            kernel,
            name: name.to_string(),
        })
    }

    /// Execute kaish code and return the result.
    pub async fn execute(&self, code: &str) -> Result<ExecResult> {
        self.kernel.execute(code).await
    }

    /// Get a variable value.
    pub async fn get_var(&self, name: &str) -> Option<kaish_kernel::ast::Value> {
        self.kernel.get_var(name).await
    }

    /// Set a variable value.
    pub async fn set_var(&self, name: &str, value: kaish_kernel::ast::Value) {
        self.kernel.set_var(name, value).await
    }

    /// List all variable names.
    pub async fn list_vars(&self) -> Vec<String> {
        self.kernel.list_vars().await.into_iter().map(|(name, _)| name).collect()
    }

    /// Get the kernel name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Ping the kernel (health check) - always succeeds for embedded.
    pub async fn ping(&self) -> Result<String> {
        Ok("pong".to_string())
    }

    /// Shutdown the embedded kernel (no-op for embedded, just drop).
    pub async fn shutdown(self) -> Result<()> {
        // Nothing to do - kernel will be dropped
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kaijutsu_kernel::block_store::shared_block_store;

    #[tokio::test]
    async fn test_embedded_kaish_creation() {
        let blocks = shared_block_store("test-agent");
        let kernel = Arc::new(KaijutsuKernel::new("test-agent").await);

        let kaish = EmbeddedKaish::new("test-kernel", blocks, kernel);
        assert!(kaish.is_ok());

        let kaish = kaish.unwrap();
        assert_eq!(kaish.name(), "test-kernel");
        assert_eq!(kaish.ping().await.unwrap(), "pong");
    }

    #[tokio::test]
    async fn test_embedded_kaish_variables() {
        let blocks = shared_block_store("test-vars");
        let kernel = Arc::new(KaijutsuKernel::new("test-vars").await);
        let kaish = EmbeddedKaish::new("test-vars", blocks, kernel).unwrap();

        // Set and get a variable
        kaish.set_var("X", kaish_kernel::ast::Value::String("hello".into())).await;
        let val = kaish.get_var("X").await;
        assert!(val.is_some());

        match val.unwrap() {
            kaish_kernel::ast::Value::String(s) => assert_eq!(s, "hello"),
            _ => panic!("Expected String value"),
        }
    }
}
