//! Shared presentation logic for hyoushigi anticipation and commitment
//! (`docs/newui.md`, "What a note shows" and "Slack and lead").
//!
//! Pure functions over `WorkStatus` and playhead/beat positions, shared across
//! clients so every client projects note readiness, fallbacks, slack, and
//! alarm identically.

use std::collections::HashMap;

use kaijutsu_hyoushigi::{Disposition, FallbackReason, Readiness, WorkStatus};
use kaijutsu_types::{Tick, TickDelta};

/// Visual state of a scheduled note derived from `(WorkStatus, playhead)` by
/// checking the 10 rules in `docs/newui.md` in strict order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoteVisualState {
    /// Rule 2: `Cancelled` or `Superseded` — fades out in place before its start.
    CancelledOrSuperseded,
    /// Rule 3: `Fallback` with `FallbackReason::Requested` — solid disc with a
    /// hollow core; pad flashes in the performer's color at its start.
    RequestedRepeat { at_start: bool },
    /// Rule 4: `Fallback` with any other reason (`DeadlineMissed`, `ResolveFailed`,
    /// `InvalidBasis`) — red ring in the lock zone; pad flashes red at start.
    TroubleFallback {
        reason: FallbackReason,
        played_content: bool,
        in_lock_zone: bool,
        at_start: bool,
    },
    /// Rule 5: `Committed` — solid, slightly brighter inside the lock line;
    /// plays at start (white pad flash + expanding ring).
    Committed {
        in_lock_zone: bool,
        at_start: bool,
    },
    /// Rule 6: Open, `Readiness::Failed` — red wireframe waiting for deadline.
    FailedOpen,
    /// Rule 7: Open, `Readiness::Ready` — solid disc; pops when `ready_at` appears.
    ReadyOpen { ready_at: Option<Tick> },
    /// Rule 8: Open, `Readiness::Running` with `attempt > 1` — flickering wireframe,
    /// cage tumbles (speculating again after basis invalidation).
    RunningRetry { attempt: u32 },
    /// Rule 9: Open, `Readiness::Running` (first attempt) — full-brightness wireframe.
    RunningOpen,
    /// Rule 10: Open, `Readiness::Queued` — 80% dim wireframe inside the slab.
    QueuedOpen,
}

/// Map `(WorkStatus, playhead, commit_margin)` to a [`NoteVisualState`].
///
/// Rules are checked in exact order (`docs/newui.md`, "What a note shows"):
/// disposition rules precede readiness rules so a `DeadlineMissed` fallback
/// still shows as trouble even if `readiness` was also set.
pub fn classify_note(
    status: &WorkStatus,
    playhead: Tick,
    commit_margin: TickDelta,
) -> NoteVisualState {
    let at_start = playhead >= status.start;
    let lock_start = status.start - commit_margin;
    let in_lock_zone = playhead >= lock_start;

    if let Some(disposition) = &status.disposition {
        return match disposition {
            Disposition::Cancelled | Disposition::Superseded { .. } => {
                NoteVisualState::CancelledOrSuperseded
            }
            Disposition::Fallback {
                reason: FallbackReason::Requested,
                ..
            } => NoteVisualState::RequestedRepeat { at_start },
            Disposition::Fallback {
                reason, content, ..
            } => NoteVisualState::TroubleFallback {
                reason: *reason,
                played_content: content.is_some(),
                in_lock_zone,
                at_start,
            },
            Disposition::Committed { .. } => NoteVisualState::Committed {
                in_lock_zone,
                at_start,
            },
        };
    }

    match status.readiness {
        Readiness::Failed => NoteVisualState::FailedOpen,
        Readiness::Ready => NoteVisualState::ReadyOpen {
            ready_at: status.ready_at,
        },
        Readiness::Running if status.attempt > 1 => NoteVisualState::RunningRetry {
            attempt: status.attempt,
        },
        Readiness::Running => NoteVisualState::RunningOpen,
        Readiness::Queued => NoteVisualState::QueuedOpen,
    }
}

/// Performer slack state (`docs/newui.md`, "Slack and lead").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlackState {
    /// No open work and no covered phrase ahead — calm, no floor drawn.
    Idle,
    /// Slack >= threshold — floor glows in the performer's color.
    Working,
    /// Slack < threshold — floor turns red and pulses.
    Alarm,
}

/// Computed slack and threshold summary for one performer on a track.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PerformerSlack {
    /// Unbroken beats of covered phrases from `now_beats` forward.
    pub slack_beats: f32,
    /// Threshold used for alarm comparison (`lead_beats` if > 0, else `alarm_beats`).
    pub threshold_beats: f32,
    /// Resulting performer state.
    pub state: SlackState,
}

fn is_covered(status: &WorkStatus) -> bool {
    match &status.disposition {
        Some(Disposition::Committed { .. }) => true,
        Some(Disposition::Fallback { content, .. }) => content.is_some(),
        Some(Disposition::Cancelled | Disposition::Superseded { .. }) => false,
        None => status.readiness == Readiness::Ready,
    }
}

/// Compute a performer's slack in beats from its `WorkStatus` records.
///
/// Walks phrases forward from the current phrase (`floor(now_beats / P) * P`)
/// in steps of `beats_per_phrase`. A phrase is covered when an active record
/// at that phrase start is `Committed`, `Fallback` with content, or open
/// `Ready`. The unbroken run stops at the first missing or uncovered phrase.
pub fn compute_performer_slack(
    records: &[WorkStatus],
    now_beats: f64,
    beats_per_phrase: u64,
    lead_beats: f32,
    alarm_beats: f32,
) -> PerformerSlack {
    let phrase_len = beats_per_phrase.max(1) as i64;
    let now_tick = now_beats.floor() as i64;
    let current_phrase = now_tick.div_euclid(phrase_len) * phrase_len;

    // Index non-cancelled/non-superseded records by start tick, keeping the
    // highest-attempt / latest record if multiple exist for the same tick.
    let mut by_start: HashMap<i64, &WorkStatus> = HashMap::new();
    let mut has_open_future_work = false;
    for rec in records {
        let start = rec.start.0;
        if rec.disposition.is_none() && start >= current_phrase {
            has_open_future_work = true;
        }
        if matches!(
            rec.disposition,
            Some(Disposition::Cancelled | Disposition::Superseded { .. })
        ) {
            continue;
        }
        by_start
            .entry(start)
            .and_modify(|existing| {
                if rec.attempt >= existing.attempt {
                    *existing = rec;
                }
            })
            .or_insert(rec);
    }

    let mut last_covered_end: Option<i64> = None;
    let mut probe = current_phrase;
    // Bounded walk across any plausible future horizon.
    for _ in 0..256 {
        let Some(rec) = by_start.get(&probe) else {
            break;
        };
        if !is_covered(rec) {
            break;
        }
        last_covered_end = Some(probe + phrase_len);
        probe += phrase_len;
    }

    let slack_beats = match last_covered_end {
        Some(end_tick) => ((end_tick as f64) - now_beats).max(0.0) as f32,
        None => 0.0,
    };

    let threshold_beats = if lead_beats > 0.0 {
        lead_beats
    } else {
        alarm_beats
    };

    let state = if !has_open_future_work && last_covered_end.is_none() {
        SlackState::Idle
    } else if slack_beats < threshold_beats {
        SlackState::Alarm
    } else {
        SlackState::Working
    };

    PerformerSlack {
        slack_beats,
        threshold_beats,
        state,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use kaijutsu_hyoushigi::{ContentRef, Fallback, WorkId};
    use kaijutsu_types::{PrincipalId, TrackId};

    fn nil_work_id() -> WorkId {
        serde_json::from_str("\"00000000-0000-0000-0000-000000000000\"").unwrap()
    }

    fn make_status(start: i64) -> WorkStatus {
        WorkStatus {
            id: nil_work_id(),
            track: TrackId::new("bass").unwrap(),
            played_by: PrincipalId::nil(),
            start: Tick::new(start),
            admitted_at: Tick::new(start - 8),
            estimate: TickDelta::new(2),
            estimate_wall: Duration::from_secs(1),
            prepare_at: Tick::new(start - 4),
            attempt: 1,
            started_at: None,
            ready_at: None,
            readiness: Readiness::Queued,
            predicted: None,
            actual: None,
            valid: None,
            error: None,
            timing: None,
            settled_at: None,
            disposition: None,
        }
    }

    #[test]
    fn note_rules_cover_all_ten_cases_and_precedence_overlaps() {
        let margin = TickDelta::new(1);

        // Rule 2: Cancelled and Superseded override everything before start.
        let mut s = make_status(16);
        s.readiness = Readiness::Ready;
        s.disposition = Some(Disposition::Cancelled);
        assert_eq!(
            classify_note(&s, Tick::new(10), margin),
            NoteVisualState::CancelledOrSuperseded
        );
        s.disposition = Some(Disposition::Superseded {
            by: nil_work_id(),
        });
        assert_eq!(
            classify_note(&s, Tick::new(10), margin),
            NoteVisualState::CancelledOrSuperseded
        );

        // Rule 3: Requested fallback.
        let mut s = make_status(16);
        s.disposition = Some(Disposition::Fallback {
            reason: FallbackReason::Requested,
            policy: Fallback::UseLastGood,
            content: Some(ContentRef::of(b"x", "text/plain")),
        });
        assert_eq!(
            classify_note(&s, Tick::new(15), margin),
            NoteVisualState::RequestedRepeat { at_start: false }
        );
        assert_eq!(
            classify_note(&s, Tick::new(16), margin),
            NoteVisualState::RequestedRepeat { at_start: true }
        );

        // Rule 4: Trouble fallback (beats readiness even if Ready).
        let mut s = make_status(16);
        s.readiness = Readiness::Ready;
        s.disposition = Some(Disposition::Fallback {
            reason: FallbackReason::DeadlineMissed,
            policy: Fallback::Skip,
            content: None,
        });
        assert_eq!(
            classify_note(&s, Tick::new(15), margin),
            NoteVisualState::TroubleFallback {
                reason: FallbackReason::DeadlineMissed,
                played_content: false,
                in_lock_zone: true,
                at_start: false,
            }
        );

        // Rule 5: Committed.
        let mut s = make_status(16);
        s.disposition = Some(Disposition::Committed {
            content: ContentRef::of(b"ok", "text/plain"),
        });
        assert_eq!(
            classify_note(&s, Tick::new(14), margin),
            NoteVisualState::Committed {
                in_lock_zone: false,
                at_start: false,
            }
        );
        assert_eq!(
            classify_note(&s, Tick::new(16), margin),
            NoteVisualState::Committed {
                in_lock_zone: true,
                at_start: true,
            }
        );

        // Rule 6: Open Failed.
        let mut s = make_status(16);
        s.readiness = Readiness::Failed;
        assert_eq!(
            classify_note(&s, Tick::new(10), margin),
            NoteVisualState::FailedOpen
        );

        // Rule 7: Open Ready.
        let mut s = make_status(16);
        s.readiness = Readiness::Ready;
        s.ready_at = Some(Tick::new(12));
        assert_eq!(
            classify_note(&s, Tick::new(10), margin),
            NoteVisualState::ReadyOpen {
                ready_at: Some(Tick::new(12))
            }
        );

        // Rule 8: Open Running attempt > 1.
        let mut s = make_status(16);
        s.readiness = Readiness::Running;
        s.attempt = 2;
        assert_eq!(
            classify_note(&s, Tick::new(10), margin),
            NoteVisualState::RunningRetry { attempt: 2 }
        );

        // Rule 9: Open Running attempt 1.
        let mut s = make_status(16);
        s.readiness = Readiness::Running;
        assert_eq!(
            classify_note(&s, Tick::new(10), margin),
            NoteVisualState::RunningOpen
        );

        // Rule 10: Open Queued.
        let s = make_status(16);
        assert_eq!(
            classify_note(&s, Tick::new(10), margin),
            NoteVisualState::QueuedOpen
        );
    }

    #[test]
    fn slack_breaks_at_missing_or_uncovered_phrase_and_handles_idle_vs_alarm() {
        // Empty records => Idle, never Alarm.
        let idle = compute_performer_slack(&[], 2.0, 8, 0.0, 8.0);
        assert_eq!(idle.state, SlackState::Idle);
        assert_eq!(idle.slack_beats, 0.0);

        // Phrase 0..8 committed, 8..16 ready, 16..24 missing, 24..32 ready (ignored past gap).
        let mut p0 = make_status(0);
        p0.disposition = Some(Disposition::Committed {
            content: ContentRef::of(b"a", "text/plain"),
        });
        let mut p1 = make_status(8);
        p1.readiness = Readiness::Ready;
        let mut p3 = make_status(24);
        p3.readiness = Readiness::Ready;

        let working = compute_performer_slack(&[p0.clone(), p1.clone(), p3], 2.0, 8, 4.0, 8.0);
        assert_eq!(working.slack_beats, 14.0);
        assert_eq!(working.threshold_beats, 4.0);
        assert_eq!(working.state, SlackState::Working);

        // Uncovered (Running) phrase 8 breaks the run at tick 8.
        let mut p1_running = make_status(8);
        p1_running.readiness = Readiness::Running;
        let alarm = compute_performer_slack(&[p0, p1_running], 6.0, 8, 0.0, 8.0);
        assert_eq!(alarm.slack_beats, 2.0);
        assert_eq!(alarm.threshold_beats, 8.0, "uses alarm_beats when lead is 0");
        assert_eq!(alarm.state, SlackState::Alarm);

        // Open running work with no covered phrases => Alarm (not Idle).
        let mut open_cur = make_status(0);
        open_cur.readiness = Readiness::Running;
        let dry = compute_performer_slack(&[open_cur], 1.0, 8, 3.0, 8.0);
        assert_eq!(dry.state, SlackState::Alarm);
        assert_eq!(dry.slack_beats, 0.0);
    }
}
