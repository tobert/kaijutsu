use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Configuration for Content Addressable Storage.
///
/// The caller (kernel) provides the base path — no env vars or file-based config.
/// Objects stored in `{base_path}/objects/`, metadata in `{base_path}/metadata/`,
/// staging in `{base_path}/staging/`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CasConfig {
    pub base_path: PathBuf,

    /// Whether to write metadata JSON alongside objects.
    #[serde(default = "default_true")]
    pub store_metadata: bool,

    /// Read-only mode — prevents any writes.
    #[serde(default)]
    pub read_only: bool,
}

fn default_true() -> bool {
    true
}

impl CasConfig {
    pub fn with_base_path(path: impl Into<PathBuf>) -> Self {
        Self {
            base_path: path.into(),
            store_metadata: true,
            read_only: false,
        }
    }

    pub fn read_only(path: impl Into<PathBuf>) -> Self {
        Self {
            base_path: path.into(),
            store_metadata: false,
            read_only: true,
        }
    }

    pub fn objects_dir(&self) -> PathBuf {
        self.base_path.join("objects")
    }

    pub fn metadata_dir(&self) -> PathBuf {
        self.base_path.join("metadata")
    }

    pub fn staging_dir(&self) -> PathBuf {
        self.base_path.join("staging")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_with_base_path() {
        let config = CasConfig::with_base_path("/custom/path");
        assert_eq!(config.base_path, PathBuf::from("/custom/path"));
        assert!(config.store_metadata);
        assert!(!config.read_only);
    }

    #[test]
    fn test_read_only_config() {
        let config = CasConfig::read_only("/tank/cas");
        assert_eq!(config.base_path, PathBuf::from("/tank/cas"));
        assert!(!config.store_metadata);
        assert!(config.read_only);
    }

    #[test]
    fn test_directory_paths() {
        let config = CasConfig::with_base_path("/test/cas");
        assert_eq!(config.objects_dir(), PathBuf::from("/test/cas/objects"));
        assert_eq!(config.metadata_dir(), PathBuf::from("/test/cas/metadata"));
        assert_eq!(config.staging_dir(), PathBuf::from("/test/cas/staging"));
    }
}
