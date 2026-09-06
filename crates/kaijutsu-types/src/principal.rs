//! Credential types.
//!
//! An entity that can act in the system — a human, an AI model, or the
//! kernel itself — is a bare [`PrincipalId`]. It carries no name: the given
//! name a player reads is `characters.name`, the kernel-owned sheet
//! (`docs/character.md`, "`auth.db` is a keyring"). A principal
//! authenticates via a [`Credential`], currently an SSH key only, with
//! mTLS/OAuth planned.

use serde::{Deserialize, Serialize};

use crate::ids::PrincipalId;

/// How a principal authenticates.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
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
    fn test_credential_links_to_principal() {
        let id = PrincipalId::new();
        let cred = Credential {
            kind: CredentialKind::SshKey,
            fingerprint: "SHA256:abc123def456".into(),
            principal_id: id,
        };
        assert_eq!(cred.principal_id, id);
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
        let cred = Credential {
            kind: CredentialKind::SshKey,
            fingerprint: "internal".into(),
            principal_id: PrincipalId::system(),
        };
        assert_eq!(cred.principal_id, PrincipalId::system());
    }
}
