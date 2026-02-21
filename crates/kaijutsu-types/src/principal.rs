//! Principal and credential types.
//!
//! A `Principal` is any entity that can act in the system — a human user,
//! an AI model, or the system itself. Principals authenticate via `Credential`s,
//! currently SSH keys only, with mTLS/OAuth planned.

use serde::{Deserialize, Serialize};

use crate::ids::PrincipalId;

/// An entity that can act in the system.
///
/// Replaces the three separate `Identity` structs that existed across
/// ssh, rpc, and client code. One struct, one vocabulary.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Principal {
    /// Globally unique, permanent identifier (UUIDv7).
    pub id: PrincipalId,
    /// Short handle used in RPC and display: "amy", "claude", "system".
    pub username: String,
    /// Full display name: "Amy Tobey", "Claude Opus 4.6".
    pub display_name: String,
}

impl Principal {
    /// Create a new principal with a fresh ID.
    pub fn new(username: impl Into<String>, display_name: impl Into<String>) -> Self {
        Self {
            id: PrincipalId::new(),
            username: username.into(),
            display_name: display_name.into(),
        }
    }

    /// Create the well-known system principal.
    ///
    /// Used for kernel-generated content (shell output, system messages, etc.).
    pub fn system() -> Self {
        Self {
            id: PrincipalId::system(),
            username: "system".into(),
            display_name: "System".into(),
        }
    }
}

impl std::fmt::Display for Principal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} ({})", self.username, self.display_name)
    }
}

/// How a principal authenticates.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CredentialKind {
    /// SSH public key (the only kind today).
    SshKey,
}

/// A credential linking an authentication method to a principal.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Credential {
    /// What kind of credential this is.
    pub kind: CredentialKind,
    /// Unique fingerprint (e.g. SHA256 of SSH public key).
    pub fingerprint: String,
    /// The principal this credential authenticates.
    pub principal_id: PrincipalId,
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_principal_construction() {
        let p = Principal::new("amy", "Amy Tobey");
        assert_eq!(p.username, "amy");
        assert_eq!(p.display_name, "Amy Tobey");
        assert!(!p.id.is_nil());
    }

    #[test]
    fn test_principal_system() {
        let s = Principal::system();
        assert_eq!(s.username, "system");
        assert_eq!(s.display_name, "System");
        assert_eq!(s.id, PrincipalId::system());
    }

    #[test]
    fn test_principal_serde_json_roundtrip() {
        let p = Principal::new("claude", "Claude Opus 4.6");
        let json = serde_json::to_string(&p).unwrap();
        let parsed: Principal = serde_json::from_str(&json).unwrap();
        assert_eq!(p, parsed);
    }

    #[test]
    fn test_principal_postcard_roundtrip() {
        let p = Principal::new("amy", "Amy Tobey");
        let bytes = postcard::to_stdvec(&p).unwrap();
        let parsed: Principal = postcard::from_bytes(&bytes).unwrap();
        assert_eq!(p, parsed);
    }

    #[test]
    fn test_credential_links_to_principal() {
        let p = Principal::new("amy", "Amy Tobey");
        let cred = Credential {
            kind: CredentialKind::SshKey,
            fingerprint: "SHA256:abc123def456".into(),
            principal_id: p.id,
        };
        assert_eq!(cred.principal_id, p.id);
        assert_eq!(cred.kind, CredentialKind::SshKey);
    }

    #[test]
    fn test_credential_serde_roundtrip() {
        let cred = Credential {
            kind: CredentialKind::SshKey,
            fingerprint: "SHA256:abc123def456".into(),
            principal_id: PrincipalId::new(),
        };
        let json = serde_json::to_string(&cred).unwrap();
        let parsed: Credential = serde_json::from_str(&json).unwrap();
        assert_eq!(cred, parsed);
    }

    #[test]
    fn test_system_principal_credential() {
        let s = Principal::system();
        let cred = Credential {
            kind: CredentialKind::SshKey,
            fingerprint: "internal".into(),
            principal_id: s.id,
        };
        assert_eq!(cred.principal_id, PrincipalId::system());
    }

    #[test]
    fn test_principal_display() {
        let p = Principal::new("amy", "Amy Tobey");
        assert_eq!(p.to_string(), "amy (Amy Tobey)");
    }

    #[test]
    fn test_principal_system_display() {
        assert_eq!(Principal::system().to_string(), "system (System)");
    }
}
