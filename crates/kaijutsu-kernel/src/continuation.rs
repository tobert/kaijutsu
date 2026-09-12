//! Continuation-window policy for automatic model resumption.

use std::path::Path;
use std::time::Duration;

use serde::Deserialize;

use crate::kernel::Kernel;
use crate::vfs::{VfsError, VfsOps};

const CONTINUATION_CONFIG_FILE: &str = "continuation.toml";

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ContinuationConfig {
    gate_resume: GateResumeConfig,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct GateResumeConfig {
    window_secs: u64,
}

impl ContinuationConfig {
    fn parse(text: &str) -> Result<Self, String> {
        let config: Self = toml::from_str(text)
            .map_err(|error| format!("continuation.toml parse error: {error}"))?;
        if config.gate_resume.window_secs == 0 {
            return Err("continuation.toml gate_resume.window_secs must be positive".into());
        }
        Ok(config)
    }
}

impl Kernel {
    /// Return the time after the last inference request during which a settled
    /// ask may automatically resume that same yielded turn.
    pub async fn gate_resume_window(&self) -> Result<Duration, String> {
        let path = kaijutsu_types::paths::config_path(CONTINUATION_CONFIG_FILE);
        let text = match self.vfs().read_all(Path::new(&path)).await {
            Ok(bytes) => String::from_utf8(bytes)
                .map_err(|error| format!("continuation.toml is not valid UTF-8: {error}"))?,
            Err(VfsError::NotFound(_)) | Err(VfsError::NoMountPoint(_)) => {
                crate::config_seed::DEFAULT_CONTINUATION_CONFIG.to_string()
            }
            Err(error) => return Err(format!("could not read {path}: {error}")),
        };
        let seconds = ContinuationConfig::parse(&text)?.gate_resume.window_secs;
        Ok(Duration::from_secs(seconds))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn continuation_window_rejects_zero_seconds() {
        let error = ContinuationConfig::parse("[gate_resume]\nwindow_secs = 0\n")
            .expect_err("zero would make automatic behavior ambiguous");
        assert!(error.contains("must be positive"));
    }

    #[test]
    fn shipped_continuation_window_parses() {
        let config = ContinuationConfig::parse(crate::config_seed::DEFAULT_CONTINUATION_CONFIG)
            .expect("shipped continuation policy is valid");
        assert_eq!(config.gate_resume.window_secs, 1800);
    }
}
