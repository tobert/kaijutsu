//! Observable ownership and disposition of admitted timeline work.

use std::time::Duration;

use serde::{Deserialize, Serialize};
use kaijutsu_types::{PrincipalId, Tick, TickDelta, TrackId};

use crate::{ContentRef, ContextHash, Fallback};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct WorkId(pub uuid::Uuid);

/// Wall-clock time a producer measured for one attempt: how long it waited
/// for its own admission, then how long the work took. Only the producer can
/// separate the two; the timeline observes readiness on a pulse.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Timing {
    #[serde(rename = "queued_ms", with = "millis")]
    pub queued: Duration,
    #[serde(rename = "compute_ms", with = "millis")]
    pub compute: Duration,
}

/// One measured attempt in a resolver's cost window.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CostSample {
    /// The tick the timeline observed the attempt finish.
    pub at: Tick,
    pub played_by: PrincipalId,
    /// The resolver's estimate for this attempt.
    #[serde(rename = "estimate_ms", with = "millis")]
    pub estimate: Duration,
    pub timing: Timing,
    pub outcome: SampleOutcome,
}

/// How a sampled attempt ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SampleOutcome {
    /// The producer returned a result, whether or not it was committed.
    Ready,
    /// The producer returned an error.
    Failed,
    /// The attempt was still running at its deadline. The producer reported
    /// nothing, so `compute` is a lower bound: the ticks it ran, at the clock's
    /// rate when it was dropped.
    Missed,
}

/// Durations cross the wire as fractional milliseconds.
pub(crate) mod millis {
    use std::time::Duration;
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(d: &Duration, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_f64(d.as_secs_f64() * 1000.0)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Duration, D::Error> {
        let ms = f64::deserialize(d)?;
        Duration::try_from_secs_f64(ms / 1000.0).map_err(serde::de::Error::custom)
    }
}

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
    /// The resolver's cost estimate, in ticks at admission's tick rate.
    pub estimate: TickDelta,
    /// The same estimate in wall-clock time, to compare with `timing`.
    #[serde(rename = "estimate_ms", with = "millis")]
    pub estimate_wall: Duration,
    /// The tick preparation was planned to begin: the estimate's lead before
    /// `start`, or admission for work that prepares at once.
    pub prepare_at: Tick,
    pub attempt: u32,
    pub started_at: Option<Tick>,
    pub ready_at: Option<Tick>,
    pub readiness: Readiness,
    pub predicted: Option<ContextHash>,
    pub actual: Option<ContextHash>,
    pub valid: Option<bool>,
    pub error: Option<String>,
    /// What the producer measured for the latest attempt, when it reported it.
    pub timing: Option<Timing>,
    pub settled_at: Option<Tick>,
    pub disposition: Option<Disposition>,
}
