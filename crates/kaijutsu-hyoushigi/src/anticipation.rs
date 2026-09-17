use std::sync::{Arc, Mutex, mpsc};
use std::task::Poll;
use std::time::Duration;

use crate::*;
use kaijutsu_types::{PrincipalId, TrackId};

struct Controlled {
    receiver: Mutex<Option<mpsc::Receiver<Result<Resolution, ResolveError>>>>,
    dropped: Arc<std::sync::atomic::AtomicBool>,
}

struct Pending {
    receiver: mpsc::Receiver<Result<Resolution, ResolveError>>,
    dropped: Arc<std::sync::atomic::AtomicBool>,
}

impl Drop for Pending {
    fn drop(&mut self) {
        self.dropped.store(true, std::sync::atomic::Ordering::SeqCst);
    }
}

impl std::future::Future for Pending {
    type Output = Result<Resolution, ResolveError>;

    fn poll(self: std::pin::Pin<&mut Self>, _cx: &mut std::task::Context<'_>) -> Poll<Self::Output> {
        match self.receiver.try_recv() {
            Ok(result) => Poll::Ready(result),
            Err(mpsc::TryRecvError::Empty) => Poll::Pending,
            Err(mpsc::TryRecvError::Disconnected) => Poll::Ready(Err(ResolveError::Failed("producer disconnected".into()))),
        }
    }
}

impl Resolver for Controlled {
    fn id(&self) -> ResolverId { ResolverId::new("controlled") }
    fn can_respeculate(&self) -> bool { false }
    fn estimate_cost(&self, _: &serde_json::Value, _: &dyn ResolverCtx) -> Duration { Duration::from_secs(5) }
    fn compute_basis(&self, _: &serde_json::Value, ctx: &dyn ResolverCtx) -> ContextHash {
        ContextHash::of(&ctx.ambient("basis").unwrap_or_default())
    }
    fn resolve(&self, _: &serde_json::Value, _: &dyn ResolverCtx) -> ResolveFuture {
        Box::pin(Pending {
            receiver: self.receiver.lock().unwrap().take().expect("one controlled attempt"),
            dropped: self.dropped.clone(),
        })
    }
}

fn cell(at: i64) -> Cell {
    Cell::deferred_on(Span::instant(Tick::new(at)), Recipe {
        resolver: ResolverId::new("controlled"),
        params: serde_json::Value::Null,
        query: ContextQuery::default(),
        fallback: Fallback::Skip,
    }, TrackId::solo(), PrincipalId::new())
}

fn fixture() -> (Timeline, mpsc::Sender<Result<Resolution, ResolveError>>, Arc<std::sync::atomic::AtomicBool>) {
    let (sender, receiver) = mpsc::channel();
    let dropped = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let mut timeline = Timeline::new(TickClock { ticks_per_sec: 1.0, safety_factor: 1.0, commit_margin: TickDelta::new(2) });
    timeline.register_resolver(Box::new(Controlled { receiver: Mutex::new(Some(receiver)), dropped: dropped.clone() }));
    (timeline, sender, dropped)
}

#[test]
fn dedicated_preparation_starts_at_admission_and_never_replays_its_producer() {
    let (sender, receiver) = mpsc::channel();
    let dropped = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let mut tl = Timeline::new(TickClock { ticks_per_sec: 1.0, safety_factor: 1.0, commit_margin: TickDelta::new(20) });
    let work = tl.schedule_preparing(cell(100), Box::new(Controlled {
        receiver: Mutex::new(Some(receiver)), dropped: dropped.clone(),
    })).unwrap();
    assert_eq!(tl.status(work).unwrap().started_at, Some(Tick::ZERO));
    sender.send(Ok(Resolution::new(b"ready", "text/plain"))).unwrap();
    tl.advance_to(Tick::new(1));
    tl.set_ambient("basis", b"changed".to_vec());
    tl.advance_to(Tick::new(80));
    assert_eq!(tl.status(work).unwrap().attempt, 1);
    assert!(matches!(tl.status(work).unwrap().disposition,
        Some(Disposition::Fallback { reason: FallbackReason::InvalidBasis, .. })));
    assert_eq!(Arc::strong_count(&dropped), 1, "terminal history must not retain the resolver");
}

#[test]
fn dedicated_preparation_releases_its_owner_on_cancel_and_refusal() {
    for at in [0, 100] {
        let (sender, receiver) = mpsc::channel();
        let dropped = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let mut tl = Timeline::new(TickClock::default());
        let result = tl.schedule_preparing(cell(at), Box::new(Controlled {
            receiver: Mutex::new(Some(receiver)), dropped: dropped.clone(),
        }));
        if at == 0 { assert!(matches!(result, Err(ScheduleError::InThePast { .. }))); }
        else { assert!(tl.cancel(result.unwrap())); }
        assert_eq!(Arc::strong_count(&dropped), 1);
        assert!(sender.send(Ok(Resolution::new(b"too late", "text/plain"))).is_err());
    }
}

#[test]
fn admitted_work_keeps_its_resolver_when_registration_changes() {
    let (mut tl, sender, _) = fixture();
    let work = tl.schedule(cell(10)).unwrap();
    let (_replacement, receiver) = mpsc::channel();
    tl.register_resolver(Box::new(Controlled {
        receiver: Mutex::new(Some(receiver)),
        dropped: Arc::new(std::sync::atomic::AtomicBool::new(false)),
    }));
    sender.send(Ok(Resolution::new(b"original", "text/plain")))
        .expect("admission must retain its original preparation owner");
    tl.advance_to(Tick::new(5));
    tl.advance_to(Tick::new(8));
    assert!(matches!(tl.status(work).unwrap().disposition, Some(Disposition::Committed { .. })));
}

#[test]
fn pulse_advances_while_pending_then_commits_once_when_ready() {
    let (mut tl, sender, _) = fixture();
    let work = tl.schedule(cell(10)).unwrap();
    tl.advance_to(Tick::new(5));
    tl.advance_to(Tick::new(8));
    assert_eq!(tl.playhead(), Tick::new(8));
    assert_eq!(tl.status(work).unwrap().readiness, Readiness::Running);
    assert!(tl.committed().is_empty());
    sender.send(Ok(Resolution::new(b"on time", "text/plain"))).unwrap();
    tl.advance_to(Tick::new(9));
    let status = tl.status(work).unwrap();
    assert_eq!(status.ready_at, Some(Tick::new(9)));
    assert_eq!(status.valid, Some(true));
    assert!(matches!(status.disposition, Some(Disposition::Committed { .. })));
    assert_eq!(tl.committed().len(), 1);
    tl.advance_to(Tick::new(20));
    assert_eq!(tl.committed().len(), 1);
}

#[test]
fn late_ready_result_is_not_backdated_when_clock_jumps() {
    let (mut tl, sender, dropped) = fixture();
    let work = tl.schedule(cell(10)).unwrap();
    tl.advance_to(Tick::new(5));
    sender.send(Ok(Resolution::new(b"late", "text/plain"))).unwrap();
    tl.advance_to(Tick::new(11));
    assert!(tl.committed().is_empty());
    assert!(dropped.load(std::sync::atomic::Ordering::SeqCst));
    assert!(matches!(tl.status(work).unwrap().disposition, Some(Disposition::Fallback { reason: FallbackReason::DeadlineMissed, .. })));
}

#[test]
fn cancellation_drops_owned_work_and_rejects_late_delivery() {
    let (mut tl, sender, dropped) = fixture();
    let work = tl.schedule(cell(10)).unwrap();
    tl.advance_to(Tick::new(5));
    assert!(tl.cancel(work));
    assert!(!tl.cancel(work));
    assert!(dropped.load(std::sync::atomic::Ordering::SeqCst));
    assert!(sender.send(Ok(Resolution::new(b"obsolete", "text/plain"))).is_err());
    tl.advance_to(Tick::new(10));
    assert!(tl.committed().is_empty());
    assert_eq!(tl.status(work).unwrap().disposition, Some(Disposition::Cancelled));
}

#[test]
fn supersession_cancels_old_work_and_only_replacement_commits() {
    let (mut tl, old_sender, dropped) = fixture();
    let old = tl.schedule(cell(10)).unwrap();
    tl.advance_to(Tick::new(5));
    let (sender, receiver) = mpsc::channel();
    tl.register_resolver(Box::new(Controlled {
        receiver: Mutex::new(Some(receiver)),
        dropped: Arc::new(std::sync::atomic::AtomicBool::new(false)),
    }));
    let replacement = tl.supersede(old, cell(10)).unwrap();
    assert_ne!(old, replacement);
    assert_eq!(tl.status(old).unwrap().disposition, Some(Disposition::Superseded { by: replacement }));
    assert!(dropped.load(std::sync::atomic::Ordering::SeqCst));
    assert!(old_sender.send(Ok(Resolution::new(b"old", "text/plain"))).is_err());
    sender.send(Ok(Resolution::new(b"new", "text/plain"))).unwrap();
    tl.advance_to(Tick::new(8));
    assert_eq!(tl.committed().len(), 1);
    assert_eq!(tl.committed()[0].body, Body::Concrete(ContentRef::of(b"new", "text/plain")));
    assert!(matches!(tl.supersede(old, cell(11)), Err(ScheduleError::NotPending)));
}

#[test]
fn changed_basis_cannot_commit_a_ready_result() {
    let (mut tl, sender, _) = fixture();
    let work = tl.schedule(cell(10)).unwrap();
    tl.set_ambient("basis", b"original".to_vec());
    tl.advance_to(Tick::new(5));
    tl.set_ambient("basis", b"changed".to_vec());
    sender.send(Ok(Resolution::new(b"obsolete", "text/plain"))).unwrap();
    tl.advance_to(Tick::new(9));
    assert!(tl.committed().is_empty());
    let status = tl.status(work).unwrap();
    assert_eq!(status.ready_at, Some(Tick::new(9)));
    assert_eq!(status.valid, Some(false));
    assert!(matches!(status.disposition, Some(Disposition::Fallback { reason: FallbackReason::InvalidBasis, .. })));
}

#[test]
fn deadline_and_timeline_drop_cancel_pending_producers() {
    let (mut tl, sender, dropped) = fixture();
    let work = tl.schedule(cell(10)).unwrap();
    tl.advance_to(Tick::new(5));
    tl.advance_to(Tick::new(10));
    assert!(dropped.load(std::sync::atomic::Ordering::SeqCst));
    assert!(sender.send(Ok(Resolution::new(b"late", "text/plain"))).is_err());
    assert!(matches!(tl.status(work).unwrap().disposition, Some(Disposition::Fallback { reason: FallbackReason::DeadlineMissed, .. })));
    let (mut tl, sender, dropped) = fixture();
    tl.schedule(cell(10)).unwrap();
    tl.advance_to(Tick::new(5));
    drop(tl);
    assert!(dropped.load(std::sync::atomic::Ordering::SeqCst));
    assert!(sender.send(Ok(Resolution::new(b"orphan", "text/plain"))).is_err());
}

#[test]
fn bounded_admission_and_failed_replacement_preserve_current_work() {
    let mut tl = Timeline::with_capacity(TickClock::default(), std::num::NonZeroUsize::new(1).unwrap());
    let (_, receiver) = mpsc::channel();
    tl.register_resolver(Box::new(Controlled {
        receiver: Mutex::new(Some(receiver)),
        dropped: Arc::new(std::sync::atomic::AtomicBool::new(false)),
    }));
    let old = tl.schedule(cell(10)).unwrap();
    assert!(matches!(tl.schedule(cell(11)), Err(ScheduleError::AtCapacity(1))));
    assert!(matches!(tl.supersede(old, cell(0)), Err(ScheduleError::InThePast { .. })));
    assert!(tl.status(old).unwrap().disposition.is_none());
    let replacement = tl.supersede(old, cell(11)).unwrap();
    assert_eq!(tl.future_len(), 1, "a replacement reuses admission capacity");
    assert!(tl.cancel(replacement));
    assert!(tl.schedule(cell(12)).is_ok());
}

#[test]
fn clock_rejects_invalid_numeric_inputs_before_admission() {
    for rate in [0.0, -1.0, f64::NAN, f64::INFINITY] {
        assert!(std::panic::catch_unwind(|| Timeline::new(TickClock { ticks_per_sec: rate, ..TickClock::default() })).is_err());
    }
    for safety in [-1.0, f64::NAN, f64::INFINITY] {
        assert!(std::panic::catch_unwind(|| Timeline::new(TickClock { safety_factor: safety, ..TickClock::default() })).is_err());
    }
    assert!(std::panic::catch_unwind(|| Timeline::new(TickClock { commit_margin: TickDelta::new(-1), ..TickClock::default() })).is_err());
    assert!(std::panic::catch_unwind(|| {
        let (mut tl, _, _) = fixture();
        tl.set_clock(TickClock { ticks_per_sec: 1e300, ..TickClock::default() });
        let _ = tl.schedule(cell(10));
    }).is_err(), "lead time overflow must be explicit");
}

#[test]
fn an_emission_refused_at_capacity_is_observable() {
    struct Emits;
    let make = |at| Cell::deferred_on(Span::instant(Tick::new(at)), Recipe {
        resolver: ResolverId::new("emits"), params: serde_json::Value::Null,
        query: ContextQuery::default(), fallback: Fallback::Skip,
    }, TrackId::solo(), PrincipalId::beat());
    impl Resolver for Emits {
        fn id(&self) -> ResolverId { ResolverId::new("emits") }
        fn estimate_cost(&self, _: &serde_json::Value, _: &dyn ResolverCtx) -> Duration { Duration::ZERO }
        fn compute_basis(&self, _: &serde_json::Value, _: &dyn ResolverCtx) -> ContextHash { ContextHash::of(b"stable") }
        fn resolve(&self, _: &serde_json::Value, _: &dyn ResolverCtx) -> ResolveFuture {
            let children = [20, 21].into_iter().map(|at| Cell::deferred_on(Span::instant(Tick::new(at)), Recipe {
                resolver: ResolverId::new("emits"), params: serde_json::Value::Null,
                query: ContextQuery::default(), fallback: Fallback::Skip,
            }, TrackId::solo(), PrincipalId::beat())).collect();
            Box::pin(std::future::ready(Ok(Resolution::new(b"parent", "text/plain").with_emitted(children))))
        }
    }
    let mut tl = Timeline::with_capacity(TickClock::default(), std::num::NonZeroUsize::new(1).unwrap());
    tl.register_resolver(Box::new(Emits));
    tl.schedule(make(10)).unwrap();
    tl.advance_to(Tick::new(10));
    assert_eq!(tl.future_len(), 1);
    assert_eq!(tl.failures().len(), 1);
    assert!(tl.failures()[0].error.contains("capacity"));
    assert_eq!(tl.failures()[0].start, Tick::new(21));
    assert_eq!(tl.failures()[0].work_id, None);
}

#[test]
fn cancelled_admission_does_not_make_a_timeline_virgin_again() {
    let (mut tl, _, _) = fixture();
    let work = tl.schedule(cell(10)).unwrap();
    tl.cancel(work);
    assert!(matches!(tl.seed_playhead(Tick::new(100)), Err(SeedError::NotVirgin { .. })));
}
