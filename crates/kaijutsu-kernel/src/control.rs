//! Control plane: ConsentMode.
//!
//! The consent mode determines how collaborative vs autonomous the kernel is.

use serde::{Deserialize, Serialize};

/// Consent mode determines how collaborative vs autonomous the kernel is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ConsentMode {
    /// Human approval required for mutations.
    Collaborative,
    /// Agent can act autonomously.
    Autonomous,
}

impl Default for ConsentMode {
    fn default() -> Self {
        Self::Collaborative
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_consent_mode() {
        assert_eq!(ConsentMode::default(), ConsentMode::Collaborative);
    }
}
