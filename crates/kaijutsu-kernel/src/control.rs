//! Control plane: Lease and ConsentMode.
//!
//! The control plane manages who can mutate a kernel and how
//! collaborative the experience is.

use serde::{Deserialize, Serialize};
use std::time::{Duration, Instant};
use uuid::Uuid;

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

/// Who holds the lease.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum LeaseHolder {
    /// No one holds the lease (available).
    None,
    /// A human user holds the lease.
    Human { username: String },
    /// An AI agent holds the lease.
    Agent { agent_id: String },
}

impl Default for LeaseHolder {
    fn default() -> Self {
        Self::None
    }
}

impl LeaseHolder {
    /// Returns true if no one holds the lease.
    pub fn is_none(&self) -> bool {
        matches!(self, LeaseHolder::None)
    }

    /// Returns true if a human holds the lease.
    pub fn is_human(&self) -> bool {
        matches!(self, LeaseHolder::Human { .. })
    }

    /// Returns true if an agent holds the lease.
    pub fn is_agent(&self) -> bool {
        matches!(self, LeaseHolder::Agent { .. })
    }
}

/// A lease on the kernel.
///
/// Only one entity can hold the lease at a time. The lease holder
/// has exclusive write access. Others can read but not mutate.
#[derive(Debug, Clone)]
pub struct Lease {
    /// Unique ID for this lease.
    pub id: Uuid,
    /// Who holds the lease.
    pub holder: LeaseHolder,
    /// When the lease was acquired.
    pub acquired_at: Instant,
    /// Optional timeout after which the lease expires.
    pub timeout: Option<Duration>,
}

impl Lease {
    /// Create a new lease for the given holder.
    pub fn new(holder: LeaseHolder) -> Self {
        Self {
            id: Uuid::new_v4(),
            holder,
            acquired_at: Instant::now(),
            timeout: None,
        }
    }

    /// Create a lease with a timeout.
    pub fn with_timeout(holder: LeaseHolder, timeout: Duration) -> Self {
        Self {
            id: Uuid::new_v4(),
            holder,
            acquired_at: Instant::now(),
            timeout: Some(timeout),
        }
    }

    /// Check if the lease has expired.
    pub fn is_expired(&self) -> bool {
        if let Some(timeout) = self.timeout {
            self.acquired_at.elapsed() > timeout
        } else {
            false
        }
    }

    /// Check if the lease is still valid (not expired and has a holder).
    pub fn is_valid(&self) -> bool {
        !self.holder.is_none() && !self.is_expired()
    }
}

impl Default for Lease {
    fn default() -> Self {
        Self::new(LeaseHolder::None)
    }
}

/// The control plane manages lease and consent mode.
#[derive(Debug)]
pub struct ControlPlane {
    /// Current lease.
    lease: Lease,
    /// Current consent mode.
    consent_mode: ConsentMode,
}

impl Default for ControlPlane {
    fn default() -> Self {
        Self::new()
    }
}

impl ControlPlane {
    /// Create a new control plane.
    pub fn new() -> Self {
        Self {
            lease: Lease::default(),
            consent_mode: ConsentMode::default(),
        }
    }

    /// Get the current lease.
    pub fn lease(&self) -> &Lease {
        &self.lease
    }

    /// Get the current consent mode.
    pub fn consent_mode(&self) -> ConsentMode {
        self.consent_mode
    }

    /// Set the consent mode.
    pub fn set_consent_mode(&mut self, mode: ConsentMode) {
        self.consent_mode = mode;
    }

    /// Try to acquire the lease.
    ///
    /// Returns `Ok(())` if the lease was acquired, `Err` if someone else holds it.
    pub fn acquire_lease(&mut self, holder: LeaseHolder) -> Result<(), LeaseError> {
        // Check if current lease is expired
        if self.lease.is_expired() {
            self.lease = Lease::new(LeaseHolder::None);
        }

        if self.lease.holder.is_none() {
            self.lease = Lease::new(holder);
            Ok(())
        } else {
            Err(LeaseError::AlreadyHeld {
                holder: self.lease.holder.clone(),
            })
        }
    }

    /// Try to acquire the lease with a timeout.
    pub fn acquire_lease_with_timeout(
        &mut self,
        holder: LeaseHolder,
        timeout: Duration,
    ) -> Result<(), LeaseError> {
        if self.lease.is_expired() {
            self.lease = Lease::new(LeaseHolder::None);
        }

        if self.lease.holder.is_none() {
            self.lease = Lease::with_timeout(holder, timeout);
            Ok(())
        } else {
            Err(LeaseError::AlreadyHeld {
                holder: self.lease.holder.clone(),
            })
        }
    }

    /// Release the lease.
    ///
    /// Returns `Ok(())` if released, `Err` if the caller doesn't hold it.
    pub fn release_lease(&mut self, holder: &LeaseHolder) -> Result<(), LeaseError> {
        if &self.lease.holder == holder {
            self.lease = Lease::new(LeaseHolder::None);
            Ok(())
        } else {
            Err(LeaseError::NotHolder {
                expected: self.lease.holder.clone(),
                actual: holder.clone(),
            })
        }
    }

    /// Force release the lease (admin operation).
    pub fn force_release_lease(&mut self) {
        self.lease = Lease::new(LeaseHolder::None);
    }

    /// Check if the given holder has the lease.
    pub fn has_lease(&self, holder: &LeaseHolder) -> bool {
        &self.lease.holder == holder && !self.lease.is_expired()
    }

    /// Check if the lease is available.
    pub fn lease_available(&self) -> bool {
        self.lease.holder.is_none() || self.lease.is_expired()
    }
}

/// Errors related to lease operations.
#[derive(Debug, Clone, thiserror::Error)]
pub enum LeaseError {
    #[error("lease already held by {holder:?}")]
    AlreadyHeld { holder: LeaseHolder },

    #[error("not the lease holder: expected {expected:?}, got {actual:?}")]
    NotHolder {
        expected: LeaseHolder,
        actual: LeaseHolder,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_acquire_and_release() {
        let mut control = ControlPlane::new();

        let holder = LeaseHolder::Human {
            username: "amy".into(),
        };

        assert!(control.lease_available());
        control.acquire_lease(holder.clone()).unwrap();
        assert!(!control.lease_available());
        assert!(control.has_lease(&holder));

        control.release_lease(&holder).unwrap();
        assert!(control.lease_available());
    }

    #[test]
    fn test_lease_conflict() {
        let mut control = ControlPlane::new();

        let amy = LeaseHolder::Human {
            username: "amy".into(),
        };
        let agent = LeaseHolder::Agent {
            agent_id: "claude".into(),
        };

        control.acquire_lease(amy.clone()).unwrap();

        let result = control.acquire_lease(agent);
        assert!(result.is_err());
    }

    #[test]
    fn test_lease_timeout() {
        let mut control = ControlPlane::new();

        let holder = LeaseHolder::Human {
            username: "amy".into(),
        };

        control
            .acquire_lease_with_timeout(holder, Duration::from_millis(1))
            .unwrap();

        std::thread::sleep(Duration::from_millis(10));

        // Lease should be expired, so available
        assert!(control.lease().is_expired());

        // Should be able to acquire now
        let new_holder = LeaseHolder::Agent {
            agent_id: "claude".into(),
        };
        control.acquire_lease(new_holder).unwrap();
    }

    #[test]
    fn test_consent_mode() {
        let mut control = ControlPlane::new();

        assert_eq!(control.consent_mode(), ConsentMode::Collaborative);

        control.set_consent_mode(ConsentMode::Autonomous);
        assert_eq!(control.consent_mode(), ConsentMode::Autonomous);
    }

    #[test]
    fn test_force_release() {
        let mut control = ControlPlane::new();

        let holder = LeaseHolder::Human {
            username: "amy".into(),
        };
        control.acquire_lease(holder).unwrap();

        control.force_release_lease();
        assert!(control.lease_available());
    }
}
