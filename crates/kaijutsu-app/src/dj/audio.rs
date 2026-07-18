//! The cue-dispatch decision layer — ported from `audio.rs::AudioOutPlugin`'s
//! `play_render_cues`/`drain_prefetch_results` (deleted whole in this task,
//! `docs/midi.md` "The DJ thread": "RenderCue parse + deadline math +
//! dispatch to the rodio thread ... CAS prefetch dispatch"). Playback itself
//! never happens here: every `audio/*`, [`CLIP_MIME`], and [`PREPARE_MIME`]
//! cue is translated into a [`crate::audio_sched::SchedulerCmd`] and handed
//! to the rodio scheduler thread (`audio_sched.rs`) via
//! [`crate::audio_sched::AudioSchedulerHandle`]; a `Cas`-backed cue resolves
//! through [`super::prefetch::CasPrefetch`] first. This module's job is
//! purely *deciding*: parse the cue, resolve its bytes (inline or CAS), work
//! out the backdated deadline, and dispatch.
//!
//! **Design decision (Task #3): direct sink dispatch, not returned actions.**
//! [`dispatch_render_cue`]/[`handle_prefetch_outcome`] take the sinks
//! (`Option<&AudioSchedulerHandle>`, `&CasPrefetch`) as parameters and call
//! straight into them, mirroring `dj::thread::handle_due_clicks`'s own
//! "dispatch to the sink if present" idiom rather than `DjCore`'s
//! pure-report-only stance — a scheduled sound is already an external effect
//! (a channel send) the moment it's decided, so there is no benefit to
//! collecting it into an intermediate `Vec<Action>` only to immediately
//! replay it at the one call site. Every `warn!`/`kaijutsu_telemetry` call
//! stays inline too — same posture, and it keeps this port byte-for-byte
//! comparable against the deleted `audio.rs`. What stays genuinely *pure*
//! (and unit-tested as such) is the field-mapping half: [`clip_media_fetch`]
//! takes no sink at all.
//!
//! **CAS resolution is unchanged** (`docs/slash-v.md` track B): a
//! `CuePayload::Cas(hash)` cue resolves through [`super::prefetch::CasPrefetch`]
//! off the DJ thread's `select!` via its own tokio runtime, landing back here
//! as a [`super::prefetch::PrefetchOutcome`] on the DJ thread's prefetch
//! `select!` arm (`dj::thread::run_loop`). This is still fetch-on-cue (the
//! prepare-horizon prefetch is R4, not yet built) — the difference from the
//! pre-Task-#3 sink is only WHICH thread decides: the DJ's own `select!`
//! loop, not a Bevy `Update` system, closing the frame-jitter bug
//! `docs/midi.md` "The DJ thread" names.
//!
//! **Backdating applies to every audio/clip cue**, not just the zero-lead
//! case: [`crate::audio_sched::effective_deadline`] mirrors
//! `midi.rs::backdate_events`'s epoch-backdating discipline (`docs/midi.md`
//! "The one timebase"). The deadline is snapshotted at CUE RECEIPT — before
//! any CAS fetch is even dispatched — so fetch latency never folds into
//! audio jitter (`docs/pcm.md` Decision 4): a CAS-resolved cue plays against
//! its *original* deadline once bytes land, even if that's now in the past
//! (the scheduler fires an overdue `PlayAt` immediately rather than
//! re-deriving a fresh one).
//!
//! **The clip renderer (R1)**: a `CLIP_MIME` cue is parsed structurally
//! (`Clip::parse` — the kernel already ran `parse_validated` at commit;
//! `media`-presence failures here are still loud, never a panic), its
//! `media` hash resolved through the SAME [`super::prefetch::CasPrefetch`]
//! path as a raw audio cue, and scheduled with
//! `src_offset_ms`/`src_len_ms`/`gain_db` applied. Both `Inline` (the JSON
//! record itself, small, the usual case) and `Cas` (a CAS-stored record)
//! clip payloads are supported — the latter is a two-stage resolve: fetch
//! the record, parse it, then fetch its `media`.
//!
//! Non-audio, non-clip mimes (MIDI `text/vnd.abc`) are still owned by
//! `midi.rs` off the Bevy-side `ServerEventMessage` stream (untouched this
//! task — `docs/midi.md`'s staging note: ABC + clicks + `BeatSync` sinks stay
//! on the old path until Task #4); `RENDER_FLUSH_MIME` is consumed by BOTH
//! (the DJ's scheduler flush here, `midi.rs`'s own cursor there)
//! independently, exactly as the deleted `audio.rs` already documented.

use std::time::Instant;

use kaijutsu_audio::{ABC_MIME, CLIP_MIME, Clip, ClipError, CuePayload, PREPARE_MIME, REF_STALE_MAX, RENDER_FLUSH_MIME, RenderCue};
use kaijutsu_client::SshConfig;
use tracing::warn;

use crate::audio_sched::{AudioSchedulerHandle, DeadlineDecision, GRACE, decide_deadline, effective_deadline};

use super::prefetch::{CasPrefetch, PrefetchKind, PrefetchOutcome};

/// `clip.src_offset_ms == 0` maps to `None` rather than `Some(Duration::ZERO)`
/// — functionally identical (`skip_duration(ZERO)` is a no-op) but takes
/// `build_source`'s no-trim fast path instead of a needless wrapper.
fn clip_source_offset(clip: &Clip) -> Option<std::time::Duration> {
    (clip.src_offset_ms != 0).then(|| std::time::Duration::from_millis(clip.src_offset_ms))
}

/// Pure: parse a clip record and map its fields into what should be
/// dispatched next — the `media` hash/mime to fetch, plus the
/// [`PrefetchKind::ClipMedia`] carrying its baked trim/gain and the
/// (already-backdated) `deadline`. Split out from the dispatch call so
/// parsing + field-mapping is unit-testable with no connection, no CAS, no
/// async machinery at all. `stamped` just rides along into the returned
/// `PrefetchKind` — see `PrefetchKind::ClipMedia`'s doc.
pub(crate) fn clip_media_fetch(
    json: &str,
    deadline: Instant,
    stamped: bool,
) -> Result<(kaijutsu_cas::ContentHash, String, PrefetchKind), ClipError> {
    let clip = Clip::parse(json)?;
    Ok((
        clip.media.clone(),
        clip.mime.clone(),
        PrefetchKind::ClipMedia {
            deadline,
            stamped,
            src_offset: clip_source_offset(&clip),
            src_len: clip.src_len_ms.map(std::time::Duration::from_millis),
            gain_db: clip.gain_db,
        },
    ))
}

/// Parse a clip record JSON and, given a live connection, dispatch the fetch
/// for its `media`. A parse failure is loud (the kernel already ran
/// `parse_validated` at commit — a bad record reaching the sink is still
/// worth a warning, never a silent drop or a panic). No connection is the
/// same "arrived before we could fetch" edge the plain audio CAS path
/// already has.
fn dispatch_clip_media(
    json: &str,
    deadline: Instant,
    stamped: bool,
    prefetch: &CasPrefetch,
    connected: bool,
    ssh_config: Option<&SshConfig>,
) {
    let (media, mime, kind) = match clip_media_fetch(json, deadline, stamped) {
        Ok(v) => v,
        Err(e) => {
            warn!("clip record failed to parse; skipping (loud, not silent): {e}");
            return;
        }
    };
    if connected
        && let Some(config) = ssh_config
    {
        prefetch.dispatch(media, mime, config.clone(), kind);
    } else {
        warn!("clip record parsed but no live connection to fetch its media (media={media:?})");
    }
}

/// A clip cue's payload is either the record JSON inline (the usual case —
/// the record itself is small) or a CAS hash of the record (rarer, but
/// supported): resolve accordingly.
fn handle_clip_cue(
    payload: &CuePayload,
    deadline: Instant,
    stamped: bool,
    prefetch: &CasPrefetch,
    connected: bool,
    ssh_config: Option<&SshConfig>,
) {
    match payload {
        CuePayload::Inline(bytes) => match std::str::from_utf8(bytes) {
            Ok(json) => dispatch_clip_media(json, deadline, stamped, prefetch, connected, ssh_config),
            Err(_) => warn!("clip cue payload was not UTF-8; skipping"),
        },
        CuePayload::Cas(hash) => {
            if connected
                && let Some(config) = ssh_config
            {
                prefetch.dispatch(
                    hash.clone(),
                    CLIP_MIME.to_string(),
                    config.clone(),
                    PrefetchKind::ClipRecord { deadline, stamped },
                );
            } else {
                warn!(
                    "CAS clip render cue arrived before a live connection — cannot fetch the record \
                     (hash={hash:?})"
                );
            }
        }
    }
}

/// Consume one `RenderCue`: flush the scheduler on a transport stop/pause,
/// dispatch `audio/*` and `CLIP_MIME` cues (inline or CAS) to it with their
/// backdated deadline, dispatch `PREPARE_MIME` cache-warms, and leave
/// everything else (ABC) to `midi.rs`'s own cursor on the same message
/// stream. The DJ thread's events arm (`dj::thread::run_loop`) calls this
/// once per `ServerEvent::RenderCue`, alongside (not instead of)
/// `handle_server_event`'s clock-mode reaction to the same cue — mirrors how
/// the deleted `audio.rs` and `midi.rs` already independently read
/// `RENDER_FLUSH_MIME` off one shared stream.
///
/// `now`/`now_epoch_ns` are supplied by the caller rather than read here
/// (`docs/midi.md`'s whole "everything takes an explicit `now`" discipline,
/// already `DjCore`'s rule) — mirrors `midi.rs::play_midi_cues`'s discipline
/// of reading the clock ONCE per receipt so several cues buffered into one
/// wakeup age against the SAME instant rather than one drifting per cue.
pub(crate) fn dispatch_render_cue(
    cue: &RenderCue,
    now: Instant,
    now_epoch_ns: u64,
    connected: bool,
    ssh_config: Option<&SshConfig>,
    scheduler: Option<&AudioSchedulerHandle>,
    prefetch: &CasPrefetch,
) {
    if cue.mime == RENDER_FLUSH_MIME {
        if let Some(scheduler) = scheduler {
            scheduler.flush();
        }
        return;
    }

    if cue.mime == PREPARE_MIME {
        // Not gated by staleness — `PREPARE_MIME`'s own contract
        // (`kaijutsu-audio::lib.rs`) says a sink never rejects a prepare
        // cue as stale; the cache is worth warming even if the cue is
        // "old" by the time it's processed.
        match &cue.payload {
            CuePayload::Cas(hash) => {
                if connected
                    && let Some(config) = ssh_config
                {
                    prefetch.dispatch(
                        hash.clone(),
                        cue.mime.clone(),
                        config.clone(),
                        PrefetchKind::Warm { started: now },
                    );
                } else {
                    warn!(
                        "prepare cue arrived before a live connection — cannot warm the cache \
                         (hash={hash:?})"
                    );
                }
            }
            CuePayload::Inline(_) => {
                // A prepare cue's whole point is naming a CAS object to
                // warm ahead of time — an inline payload has nothing to
                // warm and is a producer bug, not a sink concern to hide.
                warn!(
                    "prepare cue carried an Inline payload — protocol misuse ({PREPARE_MIME} \
                     must be CuePayload::Cas); skipping"
                );
            }
        }
        return;
    }

    if cue.mime == CLIP_MIME {
        let stamped = cue.epoch_ns != 0;
        match effective_deadline(now, cue.lead, cue.epoch_ns, now_epoch_ns) {
            Some(deadline) => handle_clip_cue(&cue.payload, deadline, stamped, prefetch, connected, ssh_config),
            None => {
                kaijutsu_telemetry::record_stale_cue_dropped();
                warn!(
                    "clip render cue rejected — stale beyond {REF_STALE_MAX:?}; dropping \
                     rather than fire arbitrarily late"
                );
            }
        }
        return;
    }

    if !cue.mime.starts_with("audio/") {
        // ABC is midi.rs's own mime, read off its own cursor on the same
        // stream. Anything else Cas-backed and genuinely unrecognized by
        // any known sink stays loud (the bytes would otherwise vanish
        // with no trace) — an unrecognized Inline mime is assumed to be
        // a foreign/future symbolic payload no sink here needs to touch.
        if cue.mime != ABC_MIME
            && let CuePayload::Cas(hash) = &cue.payload
        {
            warn!(
                "CAS render cue with unrecognized mime not handled by any known sink \
                 (hash={hash:?}, mime={})",
                cue.mime
            );
        }
        return;
    }

    match &cue.payload {
        CuePayload::Inline(bytes) => {
            if cue.lead.is_zero() && cue.epoch_ns == 0 {
                // True play-now parity: skip the deadline math entirely.
                if let Some(scheduler) = scheduler {
                    scheduler.play_now(bytes.clone());
                }
                return;
            }
            match effective_deadline(now, cue.lead, cue.epoch_ns, now_epoch_ns) {
                Some(deadline) => {
                    if let Some(scheduler) = scheduler {
                        scheduler.play_at(bytes.clone(), deadline, None, None, 0.0);
                    }
                }
                None => {
                    kaijutsu_telemetry::record_stale_cue_dropped();
                    warn!(
                        "audio render cue rejected — stale beyond {REF_STALE_MAX:?}; dropping \
                         rather than play arbitrarily late (mime={})",
                        cue.mime
                    );
                }
            }
        }
        CuePayload::Cas(hash) => match effective_deadline(now, cue.lead, cue.epoch_ns, now_epoch_ns) {
            Some(deadline) => {
                if connected
                    && let Some(config) = ssh_config
                {
                    prefetch.dispatch(
                        hash.clone(),
                        cue.mime.clone(),
                        config.clone(),
                        PrefetchKind::Audio {
                            deadline,
                            stamped: cue.epoch_ns != 0,
                        },
                    );
                } else {
                    warn!(
                        "CAS render cue arrived before a live connection — cannot prefetch \
                         (hash={hash:?}, mime={})",
                        cue.mime
                    );
                }
            }
            None => {
                kaijutsu_telemetry::record_stale_cue_dropped();
                warn!(
                    "CAS audio render cue rejected — stale beyond {REF_STALE_MAX:?}; not even \
                     dispatching the fetch (hash={hash:?}, mime={})",
                    cue.mime
                );
            }
        },
    }
}

/// Act on one prefetched CAS object as it resolves — the async tail of the
/// CAS branches in [`dispatch_render_cue`]. Runs on the DJ thread's own
/// `select!` (the prefetch-outcome arm, `dj::thread::run_loop`), so the
/// off-thread resolve never touches scheduler dispatch directly.
///
/// R4's skip-loud gate (`audio_sched::decide_deadline`) is applied HERE,
/// right before the two places that would otherwise call
/// `scheduler.play_at` — a `DropLate` decision means nothing reaches the
/// scheduler at all, never a late `PlayAt`.
pub(crate) fn handle_prefetch_outcome(
    outcome: PrefetchOutcome,
    now: Instant,
    connected: bool,
    ssh_config: Option<&SshConfig>,
    scheduler: Option<&AudioSchedulerHandle>,
    prefetch: &CasPrefetch,
) {
    match outcome.kind {
        PrefetchKind::Audio { deadline, stamped } => match outcome.result {
            Ok(bytes) => match decide_deadline(deadline, stamped, now) {
                DeadlineDecision::Fire => {
                    if let Some(scheduler) = scheduler {
                        scheduler.play_at(bytes, deadline, None, None, 0.0);
                    }
                }
                DeadlineDecision::DropLate { late_by } => {
                    kaijutsu_telemetry::record_stale_cue_dropped();
                    warn!(
                        "audio media landed too late — dropping rather than firing stale \
                         (hash={:?}, {} bytes, {}ms past deadline, grace is {:?})",
                        outcome.hash,
                        bytes.len(),
                        late_by.as_millis(),
                        GRACE
                    );
                }
            },
            Err(e) => warn!("CAS prefetch failed (mime={}): {e}", outcome.mime),
        },
        PrefetchKind::ClipRecord { deadline, stamped } => match outcome.result {
            Ok(bytes) => match std::str::from_utf8(&bytes) {
                Ok(json) => dispatch_clip_media(json, deadline, stamped, prefetch, connected, ssh_config),
                Err(_) => warn!("CAS clip record was not UTF-8; skipping"),
            },
            Err(e) => warn!("CAS clip record prefetch failed: {e}"),
        },
        PrefetchKind::ClipMedia {
            deadline,
            stamped,
            src_offset,
            src_len,
            gain_db,
        } => match outcome.result {
            Ok(bytes) => match decide_deadline(deadline, stamped, now) {
                DeadlineDecision::Fire => {
                    if let Some(scheduler) = scheduler {
                        scheduler.play_at(bytes, deadline, src_offset, src_len, gain_db);
                    }
                }
                DeadlineDecision::DropLate { late_by } => {
                    kaijutsu_telemetry::record_stale_cue_dropped();
                    warn!(
                        "clip media landed too late — dropping rather than firing stale \
                         (hash={:?}, {} bytes, {}ms past deadline, grace is {:?})",
                        outcome.hash,
                        bytes.len(),
                        late_by.as_millis(),
                        GRACE
                    );
                }
            },
            Err(e) => warn!(
                "clip media not resolved from CAS; skipping (mime={}): {e}",
                outcome.mime
            ),
        },
        PrefetchKind::Warm { started } => match outcome.result {
            // The XDG cache write already happened INSIDE the resolve
            // (`CasResolver::resolve`/`fetch_verify_store` stores every
            // verified fetch) — there is nothing left to do with `bytes`
            // here but report that the warm landed.
            Ok(bytes) => tracing::info!(
                "media warmed: {} bytes in {}ms",
                bytes.len(),
                started.elapsed().as_millis()
            ),
            Err(e) => warn!("media warm failed (mime={}): {e}", outcome.mime),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio_sched::{self, SchedulerCmd};
    use kaijutsu_audio::RenderCue;
    use kaijutsu_cas::ContentHash;
    use std::str::FromStr;
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    fn now_epoch_ns() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos() as u64
    }

    const TEST_WAV: &str = "/home/atobey/src/pawlsa-mcp/pawlsa-test.wav";

    fn test_wav_bytes() -> Option<Vec<u8>> {
        std::fs::read(TEST_WAV).ok()
    }

    // ── Real WAV bytes, play-now / scheduled dispatch ─────────────────────

    /// A zero-lead, unstamped inline `audio/wav` cue is the play-now fast
    /// path — parity with the pre-rodio `AudioPlayer` spawn: exactly one
    /// `PlayNow` with the untouched bytes.
    #[test]
    fn inline_audio_cue_zero_lead_sends_one_play_now() {
        let Some(bytes) = test_wav_bytes() else {
            eprintln!("skipping: {TEST_WAV} not present on this machine");
            return;
        };
        let (scheduler, rx) = audio_sched::test_handle();
        let (prefetch, _prefetch_rx) = CasPrefetch::new();
        let cue = RenderCue::now_inline("audio/wav", bytes.clone());
        dispatch_render_cue(&cue, Instant::now(), 0, false, None, Some(&scheduler), &prefetch);

        match rx.try_recv().expect("one command sent") {
            SchedulerCmd::PlayNow { bytes: sent } => assert_eq!(sent, bytes),
            other => panic!("expected PlayNow, got {other:?}"),
        }
        assert!(rx.try_recv().is_err(), "exactly one command");
    }

    /// R5: a non-zero lead is now honored for ALL audio cues (not just
    /// zero-lead) — an unstamped cue with `lead=500ms` schedules a `PlayAt`
    /// roughly 500ms out, not a `PlayNow`.
    #[test]
    fn inline_audio_cue_with_lead_sends_a_scheduled_play_at() {
        let Some(bytes) = test_wav_bytes() else {
            eprintln!("skipping: {TEST_WAV} not present on this machine");
            return;
        };
        let (scheduler, rx) = audio_sched::test_handle();
        let (prefetch, _prefetch_rx) = CasPrefetch::new();
        let now = Instant::now();
        let cue = RenderCue {
            mime: "audio/wav".into(),
            payload: CuePayload::Inline(bytes.clone()),
            lead: Duration::from_millis(500),
            epoch_ns: 0, // unstamped: lead honored at face value
        };
        dispatch_render_cue(&cue, now, 0, false, None, Some(&scheduler), &prefetch);

        match rx.try_recv().expect("one command sent") {
            SchedulerCmd::PlayAt {
                bytes: sent,
                deadline,
                src_offset,
                src_len,
                gain_db,
            } => {
                assert_eq!(sent, bytes);
                assert!(src_offset.is_none() && src_len.is_none() && gain_db == 0.0);
                // `now` is now an explicit input rather than read internally,
                // so (unlike the pre-Task-#3 Bevy-system version, which could
                // only bracket it between two `Instant::now()` reads) the
                // deadline is exactly derivable.
                assert_eq!(deadline, now + Duration::from_millis(500));
            }
            other => panic!("expected PlayAt, got {other:?}"),
        }
    }

    /// A cue stamped stale beyond `REF_STALE_MAX` is rejected outright —
    /// never fired late, never even reaching the scheduler.
    #[test]
    fn a_stale_audio_cue_is_rejected_outright() {
        let Some(bytes) = test_wav_bytes() else {
            eprintln!("skipping: {TEST_WAV} not present on this machine");
            return;
        };
        let (scheduler, rx) = audio_sched::test_handle();
        let (prefetch, _prefetch_rx) = CasPrefetch::new();
        let epoch = now_epoch_ns();
        let stale_epoch_ns = epoch.saturating_sub((REF_STALE_MAX + Duration::from_secs(2)).as_nanos() as u64);
        let cue = RenderCue {
            mime: "audio/wav".into(),
            payload: CuePayload::Inline(bytes),
            lead: Duration::from_millis(50),
            epoch_ns: stale_epoch_ns,
        };
        dispatch_render_cue(&cue, Instant::now(), epoch, false, None, Some(&scheduler), &prefetch);
        assert!(rx.try_recv().is_err(), "a too-stale cue must not reach the scheduler");
    }

    /// A CAS audio cue with no connection can't prefetch — it warns and sends
    /// nothing to the scheduler this frame (the resolve is off-thread and
    /// needs an SSH config).
    #[test]
    fn cas_audio_cue_without_connection_produces_nothing() {
        let (scheduler, rx) = audio_sched::test_handle();
        let (prefetch, _prefetch_rx) = CasPrefetch::new();
        let cue = RenderCue {
            mime: "audio/wav".into(),
            payload: CuePayload::Cas(ContentHash::from_str("00000000000000000000000000000000").unwrap()),
            lead: Duration::ZERO,
            epoch_ns: 0,
        };
        dispatch_render_cue(&cue, Instant::now(), 0, false, None, Some(&scheduler), &prefetch);
        assert!(rx.try_recv().is_err());
    }

    /// The async tail: a resolved `audio/*` object delivered on the channel
    /// schedules exactly one `PlayAt` at the deadline snapshotted at receipt.
    #[test]
    fn a_resolved_audio_object_is_scheduled() {
        let (scheduler, rx) = audio_sched::test_handle();
        let (prefetch, _prefetch_rx) = CasPrefetch::new();
        let deadline = Instant::now();
        let outcome = PrefetchOutcome {
            mime: "audio/wav".into(),
            hash: ContentHash::from_data(b"test-object"),
            kind: PrefetchKind::Audio { deadline, stamped: true },
            result: Ok(vec![1, 2, 3, 4]),
        };
        handle_prefetch_outcome(outcome, Instant::now(), false, None, Some(&scheduler), &prefetch);

        match rx.try_recv().expect("one command sent") {
            SchedulerCmd::PlayAt {
                bytes,
                deadline: d,
                src_offset,
                src_len,
                gain_db,
            } => {
                assert_eq!(bytes, vec![1, 2, 3, 4]);
                assert_eq!(d, deadline);
                assert!(src_offset.is_none() && src_len.is_none() && gain_db == 0.0);
            }
            other => panic!("expected PlayAt, got {other:?}"),
        }
    }

    /// A failed prefetch is a loud no-op — nothing sent to the scheduler.
    #[test]
    fn a_failed_audio_prefetch_schedules_nothing() {
        let (scheduler, rx) = audio_sched::test_handle();
        let (prefetch, _prefetch_rx) = CasPrefetch::new();
        let outcome = PrefetchOutcome {
            mime: "audio/wav".into(),
            hash: ContentHash::from_data(b"test-object"),
            kind: PrefetchKind::Audio { deadline: Instant::now(), stamped: true },
            result: Err(format!("no such path: {}/…", kaijutsu_types::paths::CAS_ROOT)),
        };
        handle_prefetch_outcome(outcome, Instant::now(), false, None, Some(&scheduler), &prefetch);
        assert!(rx.try_recv().is_err());
    }

    // ── R4: skip-loud on a stale CAS fetch ──────────────────────────────

    /// A stamped (musically-placed) audio cue whose media lands more than
    /// `GRACE` past its deadline is dropped — NOTHING reaches the scheduler,
    /// not even a late `PlayAt` (closes R5's carried interim behavior,
    /// docs/pcm.md R4).
    #[test]
    fn a_stamped_audio_object_landing_past_grace_is_dropped_not_scheduled() {
        let (scheduler, rx) = audio_sched::test_handle();
        let (prefetch, _prefetch_rx) = CasPrefetch::new();
        let deadline = Instant::now() - (GRACE + Duration::from_millis(50));
        let outcome = PrefetchOutcome {
            mime: "audio/wav".into(),
            hash: ContentHash::from_data(b"test-object"),
            kind: PrefetchKind::Audio { deadline, stamped: true },
            result: Ok(vec![1, 2, 3, 4]),
        };
        handle_prefetch_outcome(outcome, Instant::now(), false, None, Some(&scheduler), &prefetch);
        assert!(
            rx.try_recv().is_err(),
            "a stamped cue landing past GRACE must never reach the scheduler"
        );
    }

    /// An UNSTAMPED audio cue (asap semantics, e.g. `kj play --cas`) still
    /// fires even when its media lands wildly late — there is no musical
    /// placement to violate, so R4's skip-loud gate must never touch it.
    #[test]
    fn an_unstamped_audio_object_landing_late_still_schedules() {
        let (scheduler, rx) = audio_sched::test_handle();
        let (prefetch, _prefetch_rx) = CasPrefetch::new();
        let deadline = Instant::now() - Duration::from_secs(5);
        let outcome = PrefetchOutcome {
            mime: "audio/wav".into(),
            hash: ContentHash::from_data(b"test-object"),
            kind: PrefetchKind::Audio { deadline, stamped: false },
            result: Ok(vec![1, 2, 3, 4]),
        };
        handle_prefetch_outcome(outcome, Instant::now(), false, None, Some(&scheduler), &prefetch);
        match rx.try_recv().expect("unstamped cues still fire however late") {
            SchedulerCmd::PlayAt { bytes, .. } => assert_eq!(bytes, vec![1, 2, 3, 4]),
            other => panic!("expected PlayAt, got {other:?}"),
        }
    }

    /// The same skip-loud gate applies to a clip's resolved media, not just
    /// plain audio — a stamped clip cue landing past grace is dropped, never
    /// scheduled with its trim/gain applied.
    #[test]
    fn a_stamped_clip_media_object_landing_past_grace_is_dropped_not_scheduled() {
        let (scheduler, rx) = audio_sched::test_handle();
        let (prefetch, _prefetch_rx) = CasPrefetch::new();
        let deadline = Instant::now() - (GRACE + Duration::from_millis(50));
        let outcome = PrefetchOutcome {
            mime: "audio/wav".into(),
            hash: ContentHash::from_data(b"test-object"),
            kind: PrefetchKind::ClipMedia {
                deadline,
                stamped: true,
                src_offset: None,
                src_len: None,
                gain_db: 0.0,
            },
            result: Ok(vec![9, 9, 9]),
        };
        handle_prefetch_outcome(outcome, Instant::now(), false, None, Some(&scheduler), &prefetch);
        assert!(
            rx.try_recv().is_err(),
            "a stamped clip media landing past GRACE must never reach the scheduler"
        );
    }

    /// A transport flush cue reaches the scheduler as `Flush` — wired to the
    /// same `RENDER_FLUSH_MIME` midi.rs consumes off its own cursor.
    #[test]
    fn a_flush_cue_sends_flush_to_the_scheduler() {
        let (scheduler, rx) = audio_sched::test_handle();
        let (prefetch, _prefetch_rx) = CasPrefetch::new();
        let cue = RenderCue::now_inline(RENDER_FLUSH_MIME, Vec::new());
        dispatch_render_cue(&cue, Instant::now(), 0, false, None, Some(&scheduler), &prefetch);
        assert!(matches!(rx.try_recv(), Ok(SchedulerCmd::Flush)));
    }

    // ── R4: the prepare-cue cache warm (PREPARE_MIME) ──────────────────────

    /// A `PREPARE_MIME` cue with no live connection can't dispatch a warm —
    /// it warns and touches neither the scheduler nor the prefetch outcome
    /// channel (mirrors the plain CAS-audio "no connection" edge).
    #[test]
    fn prepare_cue_with_no_connection_warns_and_dispatches_nothing() {
        let (scheduler, rx) = audio_sched::test_handle();
        let (prefetch, mut prefetch_rx) = CasPrefetch::new();
        let cue = RenderCue {
            mime: PREPARE_MIME.into(),
            payload: CuePayload::Cas(ContentHash::from_data(b"warm-me")),
            lead: Duration::ZERO,
            epoch_ns: 0,
        };
        dispatch_render_cue(&cue, Instant::now(), 0, false, None, Some(&scheduler), &prefetch);
        assert!(rx.try_recv().is_err(), "nothing ever reaches the scheduler for a prepare cue");
        assert!(
            prefetch_rx.try_recv().is_err(),
            "no connection means no dispatch was ever made"
        );
    }

    /// An `Inline` payload under `PREPARE_MIME` is a protocol misuse (the
    /// whole point of a prepare cue is naming a CAS object to warm) — warn
    /// and skip, never dispatch, never panic.
    #[test]
    fn inline_prepare_cue_is_a_protocol_misuse_and_dispatches_nothing() {
        let (scheduler, rx) = audio_sched::test_handle();
        let (prefetch, mut prefetch_rx) = CasPrefetch::new();
        let cue = RenderCue::now_inline(PREPARE_MIME, vec![1, 2, 3]);
        dispatch_render_cue(&cue, Instant::now(), 0, false, None, Some(&scheduler), &prefetch);
        assert!(rx.try_recv().is_err());
        assert!(
            prefetch_rx.try_recv().is_err(),
            "an Inline prepare payload must never dispatch a fetch"
        );
    }

    /// The async tail of a successful warm: nothing is ever scheduled — the
    /// resolve's side effect (writing the XDG cache) already happened inside
    /// `CasResolver::resolve`, so `handle_prefetch_outcome` has nothing left
    /// to do but report it landed.
    #[test]
    fn a_resolved_warm_prefetch_schedules_nothing() {
        let (scheduler, rx) = audio_sched::test_handle();
        let (prefetch, _prefetch_rx) = CasPrefetch::new();
        let outcome = PrefetchOutcome {
            mime: PREPARE_MIME.into(),
            hash: ContentHash::from_data(b"warm-me"),
            kind: PrefetchKind::Warm { started: Instant::now() },
            result: Ok(vec![1, 2, 3, 4]),
        };
        handle_prefetch_outcome(outcome, Instant::now(), false, None, Some(&scheduler), &prefetch);
        assert!(rx.try_recv().is_err(), "a warm never plays or schedules anything");
    }

    /// A failed warm is a loud no-op, same as a failed audio/clip prefetch —
    /// nothing sent to the scheduler.
    #[test]
    fn a_failed_warm_prefetch_schedules_nothing() {
        let (scheduler, rx) = audio_sched::test_handle();
        let (prefetch, _prefetch_rx) = CasPrefetch::new();
        let outcome = PrefetchOutcome {
            mime: PREPARE_MIME.into(),
            hash: ContentHash::from_data(b"warm-me"),
            kind: PrefetchKind::Warm { started: Instant::now() },
            result: Err("transport died mid-warm".to_string()),
        };
        handle_prefetch_outcome(outcome, Instant::now(), false, None, Some(&scheduler), &prefetch);
        assert!(rx.try_recv().is_err());
    }

    // ── The clip renderer (R1) ──────────────────────────────────────────

    fn clip_json(media: ContentHash) -> String {
        format!(r#"{{"v":1,"media":"{media}","mime":"audio/wav","label":"rimshot"}}"#)
    }

    /// An inline clip cue with no live connection can't fetch its media — it
    /// warns and sends nothing, mirroring the plain CAS-audio "no
    /// connection" edge.
    #[test]
    fn inline_clip_cue_without_connection_warns_and_schedules_nothing() {
        let (scheduler, rx) = audio_sched::test_handle();
        let (prefetch, _prefetch_rx) = CasPrefetch::new();
        let json = clip_json(ContentHash::from_data(b"rimshot"));
        let cue = RenderCue::now_inline(CLIP_MIME, json.into_bytes());
        dispatch_render_cue(&cue, Instant::now(), 0, false, None, Some(&scheduler), &prefetch);
        assert!(rx.try_recv().is_err(), "no connection means no fetch means nothing scheduled");
    }

    /// A structurally invalid clip record (the old test's bare `{}`) is
    /// rejected loud at parse — before any connection check, before any
    /// fetch — never a panic, never a silent drop.
    #[test]
    fn invalid_inline_clip_record_is_rejected_loud() {
        let (scheduler, rx) = audio_sched::test_handle();
        let (prefetch, _prefetch_rx) = CasPrefetch::new();
        let cue = RenderCue::now_inline(CLIP_MIME, b"{}".to_vec());
        dispatch_render_cue(&cue, Instant::now(), 0, false, None, Some(&scheduler), &prefetch);
        assert!(rx.try_recv().is_err());
    }

    /// R1's concrete proof: once a clip's media resolves, it schedules a
    /// `PlayAt` with the record's source-range trim and gain applied.
    #[test]
    fn a_resolved_clip_media_object_is_scheduled_with_trim_and_gain() {
        let (scheduler, rx) = audio_sched::test_handle();
        let (prefetch, _prefetch_rx) = CasPrefetch::new();
        let deadline = Instant::now();
        let outcome = PrefetchOutcome {
            mime: "audio/wav".into(),
            hash: ContentHash::from_data(b"test-object"),
            kind: PrefetchKind::ClipMedia {
                deadline,
                stamped: true,
                src_offset: Some(Duration::from_millis(100)),
                src_len: Some(Duration::from_millis(500)),
                gain_db: -6.0,
            },
            result: Ok(vec![9, 9, 9]),
        };
        handle_prefetch_outcome(outcome, Instant::now(), false, None, Some(&scheduler), &prefetch);

        match rx.try_recv().expect("one command sent") {
            SchedulerCmd::PlayAt {
                bytes,
                deadline: d,
                src_offset,
                src_len,
                gain_db,
            } => {
                assert_eq!(bytes, vec![9, 9, 9]);
                assert_eq!(d, deadline);
                assert_eq!(src_offset, Some(Duration::from_millis(100)));
                assert_eq!(src_len, Some(Duration::from_millis(500)));
                assert_eq!(gain_db, -6.0);
            }
            other => panic!("expected PlayAt, got {other:?}"),
        }
    }

    /// A stale clip cue is rejected outright, same as a stale audio cue —
    /// never even attempting to parse/fetch.
    #[test]
    fn a_stale_clip_cue_is_rejected_outright() {
        let (scheduler, rx) = audio_sched::test_handle();
        let (prefetch, _prefetch_rx) = CasPrefetch::new();
        let epoch = now_epoch_ns();
        let stale_epoch_ns = epoch.saturating_sub((REF_STALE_MAX + Duration::from_secs(2)).as_nanos() as u64);
        let json = clip_json(ContentHash::from_data(b"snare"));
        let cue = RenderCue {
            mime: CLIP_MIME.into(),
            payload: CuePayload::Inline(json.into_bytes()),
            lead: Duration::from_millis(50),
            epoch_ns: stale_epoch_ns,
        };
        dispatch_render_cue(&cue, Instant::now(), epoch, false, None, Some(&scheduler), &prefetch);
        assert!(rx.try_recv().is_err());
    }

    // ── clip_media_fetch: pure parsing/field-mapping, no connection at all ─

    #[test]
    fn clip_media_fetch_maps_the_record_fields_into_a_clip_media_prefetch() {
        let media = ContentHash::from_data(b"rimshot");
        let json = format!(
            r#"{{"v":1,"media":"{media}","mime":"audio/wav","label":"rimshot","src_offset_ms":100,"src_len_ms":500,"gain_db":-6.0}}"#
        );
        let deadline = Instant::now() + Duration::from_millis(50);
        let (got_media, got_mime, kind) = clip_media_fetch(&json, deadline, true).expect("valid record");
        assert_eq!(got_media, media);
        assert_eq!(got_mime, "audio/wav");
        match kind {
            PrefetchKind::ClipMedia {
                deadline: d,
                stamped,
                src_offset,
                src_len,
                gain_db,
            } => {
                assert_eq!(d, deadline);
                assert!(stamped, "stamped rides straight through from the caller");
                assert_eq!(src_offset, Some(Duration::from_millis(100)));
                assert_eq!(src_len, Some(Duration::from_millis(500)));
                assert_eq!(gain_db, -6.0);
            }
            _ => panic!("expected ClipMedia"),
        }
    }

    #[test]
    fn clip_media_fetch_defaults_zero_offset_to_none() {
        let media = ContentHash::from_data(b"kick");
        let json = format!(r#"{{"v":1,"media":"{media}","mime":"audio/wav","label":"kick"}}"#);
        let (_, _, kind) = clip_media_fetch(&json, Instant::now(), false).expect("valid minimal record");
        match kind {
            PrefetchKind::ClipMedia {
                stamped,
                src_offset,
                src_len,
                gain_db,
                ..
            } => {
                assert!(!stamped, "stamped rides straight through from the caller");
                assert_eq!(src_offset, None, "zero offset maps to None, build_source's fast path");
                assert_eq!(src_len, None);
                assert_eq!(gain_db, 0.0);
            }
            _ => panic!("expected ClipMedia"),
        }
    }

    #[test]
    fn clip_media_fetch_rejects_an_invalid_record() {
        assert!(clip_media_fetch("{}", Instant::now(), true).is_err());
    }
}
