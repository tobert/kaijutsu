//! Registry of available image generation backends.

use std::collections::HashMap;
use std::sync::Arc;

use super::backend::{ImageBackend, ImageError, ImageGenOpts, ImageStream};

/// Registry that maps backend names to implementations.
pub struct ImageBackendRegistry {
    backends: HashMap<String, Arc<dyn ImageBackend>>,
    default: Option<String>,
}

impl ImageBackendRegistry {
    pub fn new() -> Self {
        Self {
            backends: HashMap::new(),
            default: None,
        }
    }

    /// Register a backend. The first registered backend becomes the default.
    pub fn register(&mut self, backend: Arc<dyn ImageBackend>) {
        let name = backend.name().to_string();
        if self.default.is_none() {
            self.default = Some(name.clone());
        }
        self.backends.insert(name, backend);
    }

    /// Get a backend by name, or the default if name is None.
    pub fn get(&self, name: Option<&str>) -> Result<&Arc<dyn ImageBackend>, ImageError> {
        let key = name
            .map(|n| n.to_string())
            .or_else(|| self.default.clone())
            .ok_or_else(|| ImageError::NotAvailable("no image backends registered".into()))?;

        self.backends
            .get(&key)
            .ok_or_else(|| ImageError::NotAvailable(format!("backend '{}' not found", key)))
    }

    /// Generate an image using the specified (or default) backend.
    pub async fn generate(
        &self,
        prompt: &str,
        opts: ImageGenOpts,
    ) -> Result<ImageStream, ImageError> {
        let backend = self.get(opts.backend.as_deref())?;
        backend.generate(prompt, opts).await
    }

    pub fn is_empty(&self) -> bool {
        self.backends.is_empty()
    }

    pub fn backend_names(&self) -> Vec<&str> {
        self.backends.keys().map(|s| s.as_str()).collect()
    }
}

impl Default for ImageBackendRegistry {
    fn default() -> Self {
        Self::new()
    }
}
