//! Observable ownership and disposition of admitted timeline work.

use serde::{Deserialize, Serialize};
use kaijutsu_types::{PrincipalId, Tick, TrackId};

use crate::{ContentRef, ContextHash, Fallback};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct WorkId(pub uuid::Uuid);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Readiness { Queued, Running, Ready, Failed }

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FallbackReason { ResolveFailed, InvalidBasis, DeadlineMissed }

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Disposition {
    Committed { content: ContentRef },
    Fallback { reason: FallbackReason, policy: Fallback, content: Option<ContentRef> },
    Cancelled,
    Superseded { by: WorkId },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkStatus {
    pub id: WorkId,
    pub track: TrackId,
    pub played_by: PrincipalId,
    pub start: Tick,
    pub admitted_at: Tick,
    pub attempt: u32,
    pub started_at: Option<Tick>,
    pub ready_at: Option<Tick>,
    pub readiness: Readiness,
    pub predicted: Option<ContextHash>,
    pub actual: Option<ContextHash>,
    pub valid: Option<bool>,
    pub error: Option<String>,
    pub settled_at: Option<Tick>,
    pub disposition: Option<Disposition>,
}
