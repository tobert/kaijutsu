//! Controlled producers and manual beats, observed through the real SSH client.

mod common;

use std::time::Duration;
use tokio::time::Instant;

use kaijutsu_hyoushigi::{
    Cell, ContextHash, ContextQuery, Fallback, Recipe, Resolution, ResolveError,
    Resolver, ResolverCtx, ResolverId, Span, TickClock, TickDelta,
};
use kaijutsu_kernel::hyoushigi::{Attachment, BeatPolicy};
use kaijutsu_server::beat::BeatScheduler;
use kaijutsu_types::{BlockKind, BlockQuery, PrincipalId, TrackId};
use serde_json::{Value, json};

type Delivery = tokio::sync::oneshot::Receiver<Result<Resolution, ResolveError>>;

struct AsyncProducer(std::sync::Mutex<std::collections::HashMap<String, Delivery>>);

impl Resolver for AsyncProducer {
    fn id(&self) -> ResolverId { ResolverId::new("async_producer") }
    fn estimate_cost(&self, params: &Value, _: &dyn ResolverCtx) -> Duration {
        Duration::from_secs(params["cost"].as_u64().unwrap())
    }
    fn compute_basis(&self, params: &Value, ctx: &dyn ResolverCtx) -> ContextHash {
        ContextHash::of(&ctx.ambient(params["name"].as_str().unwrap()).unwrap_or_default())
    }
    fn resolve(&self, params: &Value, _: &dyn ResolverCtx) -> kaijutsu_hyoushigi::ResolveFuture {
        let receiver = self.0.lock().unwrap().remove(params["name"].as_str().unwrap()).unwrap();
        Box::pin(async move {
            receiver.await.map_err(|_| ResolveError::Failed("controlled producer disconnected".into()))?
        })
    }
}

async fn work_status(kj: &kaijutsu_client::KernelHandle, context: kaijutsu_types::ContextId, track: &TrackId) -> Vec<kaijutsu_hyoushigi::WorkStatus> {
    let result = kj.execute_kj_quiet(context, &[
        "transport".into(), "work".into(), "--track".into(), track.as_str().into(),
    ]).await.unwrap();
    assert_eq!(result.exit_code, 0, "{}", result.stderr);
    serde_json::from_value(result.data.expect("structured work status")).unwrap()
}

#[test]
fn different_producer_paces_share_one_pulse_and_reject_obsolete_output() {
    common::run_local(async {
        use kaijutsu_hyoushigi::{Disposition, FallbackReason, Readiness};

        let (addr, shared) = common::start_server_with_kernel_handle().await;
        let client = common::connect_client(addr).await;
        let (kj, _) = client.bind_kernel().await.unwrap();
        let context = common::create_context(&kj, "performance-observer").await.unwrap();
        let help = kj.execute_kj_quiet(context, &["transport".into(), "work".into(), "--help".into()]).await.unwrap();
        assert_eq!(help.exit_code, 0);
        assert!(help.stdout.contains("--track"));
        assert!(help.stdout.contains("256"));
        eprintln!("{}", help.stdout);
        let track = TrackId::new("several-paces").unwrap();
        let mut scheduler = BeatScheduler::new(shared.kernel.clone(), shared.documents.clone());
        let mut attachment = Attachment::musician_default();
        attachment.ooda_armed = false;
        scheduler.attach(track.clone(), context, attachment, BeatPolicy {
            period: Duration::from_secs(1), beats_per_phrase: 32,
        }).unwrap();
        let timeline = shared.kernel.track_timeline(&track).unwrap();
        let origin = timeline.lock().playhead();
        let score = shared.kernel_db.lock().get_track(track.as_str()).unwrap().unwrap().score_context_id.unwrap();
        let mut senders = std::collections::HashMap::new();
        let mut receivers = std::collections::HashMap::new();
        for name in ["fast", "delayed", "failed", "old", "replacement", "missed", "stale"] {
            let (sender, receiver) = tokio::sync::oneshot::channel();
            senders.insert(name, sender);
            receivers.insert(name.to_string(), receiver);
        }
        let player = PrincipalId::new();
        let make_cell = |name: &str, offset: i64, fallback: Fallback| Cell::deferred_on(
            Span::instant(origin + TickDelta::new(offset)), Recipe {
                resolver: ResolverId::new("async_producer"),
                params: json!({"name": name, "cost": offset - 3}),
                query: ContextQuery::default(), fallback,
            }, track.clone(), player,
        );
        let mut ids = std::collections::HashMap::new();
        {
            let mut tl = timeline.lock();
            tl.set_clock(TickClock { ticks_per_sec: 1.0, safety_factor: 1.0, commit_margin: TickDelta::new(2) });
            tl.register_resolver(Box::new(AsyncProducer(std::sync::Mutex::new(receivers))));
            for (name, at) in [("fast", 8), ("delayed", 10), ("failed", 12), ("old", 14), ("missed", 16), ("stale", 18)] {
                let fallback = if name == "failed" { Fallback::UseLastGood } else { Fallback::Skip };
                ids.insert(name, tl.schedule(make_cell(name, at, fallback)).unwrap());
            }
        }
        let fast = "X:1\nK:C\nCDEF|\n";
        let delayed = "X:2\nK:C\nGABc|\n";
        let replacement = "X:3\nK:C\ncBAG|\n";
        senders.remove("fast").unwrap().send(Ok(Resolution::new(fast.as_bytes(), kaijutsu_audio::ABC_MIME))).unwrap();
        senders.remove("failed").unwrap().send(Err(ResolveError::Failed("controlled failure".into()))).unwrap();
        let base = Instant::now();
        scheduler.play(&track, base);
        for beat in 1..=4 {
            scheduler.fire_due(base + Duration::from_secs(beat));
        }
        let replacement_id = timeline.lock().supersede(ids["old"], make_cell("replacement", 14, Fallback::Skip)).unwrap();
        assert!(senders.remove("old").unwrap().send(Ok(Resolution::new(b"obsolete old result", "text/plain"))).is_err());
        senders.remove("replacement").unwrap().send(Ok(Resolution::new(replacement.as_bytes(), kaijutsu_audio::ABC_MIME))).unwrap();
        timeline.lock().set_ambient("stale", b"new basis".to_vec());
        scheduler.fire_due(base + Duration::from_secs(5));
        let pending = work_status(&kj, context, &track).await;
        assert_eq!(timeline.lock().playhead(), origin + TickDelta::new(5));
        assert_eq!(pending.iter().find(|s| s.id == ids["delayed"]).unwrap().readiness, Readiness::Running);
        assert_eq!(pending.iter().find(|s| s.id == ids["fast"]).unwrap().readiness, Readiness::Ready);
        assert_eq!(pending.iter().find(|s| s.id == ids["old"]).unwrap().disposition, Some(Disposition::Superseded { by: replacement_id }));
        for beat in 6..=8 {
            scheduler.fire_due(base + Duration::from_secs(beat));
        }
        senders.remove("delayed").unwrap().send(Ok(Resolution::new(delayed.as_bytes(), kaijutsu_audio::ABC_MIME))).unwrap();
        scheduler.fire_due(base + Duration::from_secs(9));
        senders.remove("stale").unwrap().send(Ok(Resolution::new(b"wrong basis", "text/plain"))).unwrap();
        for beat in 10..=18 {
            scheduler.fire_due(base + Duration::from_secs(beat));
        }
        assert!(senders.remove("missed").unwrap().send(Ok(Resolution::new(b"too late", "text/plain"))).is_err());
        let final_status = work_status(&kj, context, &track).await;
        let status = |name: &str| final_status.iter().find(|s| s.id == ids[name]).unwrap();
        assert_eq!(status("delayed").ready_at, Some(origin + TickDelta::new(9)));
        assert_eq!(status("delayed").settled_at, Some(origin + TickDelta::new(9)));
        assert_eq!(status("delayed").valid, Some(true));
        assert!(matches!(status("failed").disposition, Some(Disposition::Fallback { reason: FallbackReason::ResolveFailed, .. })));
        assert!(matches!(status("missed").disposition, Some(Disposition::Fallback { reason: FallbackReason::DeadlineMissed, .. })));
        assert_eq!(status("stale").valid, Some(false));
        assert!(matches!(status("stale").disposition, Some(Disposition::Fallback { reason: FallbackReason::InvalidBasis, .. })));
        let blocks = kj.get_blocks(score, &BlockQuery::All).await.unwrap();
        for (text, at, author) in [(fast, 8, player), (delayed, 10, player), (delayed, 12, PrincipalId::beat()), (replacement, 14, player)] {
            let matching: Vec<_> = blocks.iter().filter(|b| b.content == text && b.tick == Some(origin + TickDelta::new(at))).collect();
            assert_eq!(matching.len(), 1, "one accepted score cell at offset {at}");
            assert_eq!(matching[0].id.principal_id, author);
        }
        assert!(!blocks.iter().any(|b| ["obsolete old result", "wrong basis", "too late"].contains(&b.content.as_str())));
        for beat in 19..=22 {
            scheduler.fire_due(base + Duration::from_secs(beat));
        }
        assert_eq!(kj.get_blocks(score, &BlockQuery::All).await.unwrap().len(), blocks.len());
        assert_eq!(timeline.lock().future_len(), 0);
    });
}

struct ControlledProducer;

impl Resolver for ControlledProducer {
    fn id(&self) -> ResolverId {
        ResolverId::new("controlled")
    }

    fn estimate_cost(&self, params: &Value, _ctx: &dyn ResolverCtx) -> Duration {
        Duration::from_secs(params["cost"].as_u64().unwrap())
    }

    fn compute_basis(&self, params: &Value, _ctx: &dyn ResolverCtx) -> ContextHash {
        ContextHash::of(params.to_string().as_bytes())
    }

    fn resolve(&self, params: &Value, _ctx: &dyn ResolverCtx) -> kaijutsu_hyoushigi::ResolveFuture {
        Box::pin(std::future::ready((|| {
            match params["abc"].as_str() {
                Some(abc) => Ok(Resolution::new(abc.as_bytes(), kaijutsu_audio::ABC_MIME)),
                None => Err(ResolveError::Failed("controlled producer failed".into())),
            }
        })()))
    }
}

#[test]
fn failed_producer_reports_error_and_plays_declared_fallback_once() {
    common::run_local(async {
        let (addr, shared) = common::start_server_with_kernel_handle().await;
        let client = common::connect_client(addr).await;
        let (kj, _) = client.bind_kernel().await.unwrap();
        let context = common::create_context(&kj, "timeline-listener").await.unwrap();
        let track = TrackId::new("controlled-performance").unwrap();
        // This scheduler owns the controlled clock. OODA stays disarmed, so no
        // tick can launch a model turn or an rc script.
        let mut scheduler = BeatScheduler::new(shared.kernel.clone(), shared.documents.clone());
        let mut attachment = Attachment::musician_default();
        attachment.ooda_armed = false;
        scheduler.attach(track.clone(), context, attachment, BeatPolicy {
            period: Duration::from_secs(1),
            beats_per_phrase: 32,
        }).unwrap();
        let score = shared.kernel_db.lock().get_track(track.as_str()).unwrap().unwrap().score_context_id.unwrap();
        let timeline = shared.kernel.track_timeline(&track).unwrap();
        let origin = timeline.lock().playhead();
        let player = PrincipalId::new();
        let first = "X:1\nK:C\nCDEF|\n";
        let next = "X:2\nK:C\nGABc|\n";
        {
            let mut tl = timeline.lock();
            tl.set_clock(TickClock {
                ticks_per_sec: 1.0,
                safety_factor: 1.0,
                commit_margin: TickDelta::new(0),
            });
            tl.register_resolver(Box::new(ControlledProducer));
            for (at, cost, abc, fallback) in [
                (1, 0, Some(first), Fallback::Skip),
                (4, 0, Some(next), Fallback::Skip),
                (6, 3, None, Fallback::UseLastGood),
            ] {
                tl.schedule(Cell::deferred_on(
                    Span::instant(origin + TickDelta::new(at)),
                    Recipe {
                        resolver: ResolverId::new("controlled"),
                        params: json!({"cost": cost, "abc": abc}),
                        query: ContextQuery::default(),
                        fallback,
                    },
                    track.clone(),
                    player,
                )).unwrap();
            }
        }
        let base = Instant::now();
        scheduler.play(&track, base);

        for beat in 1..=3 {
            scheduler.fire_due(base + Duration::from_secs(beat));
        }
        let feedback = kj.get_blocks(context, &BlockQuery::All).await.unwrap();
        assert_eq!(feedback.iter().filter(|b| b.kind == BlockKind::Error).count(), 1);
        assert!(feedback.iter().any(|b| b.content.contains("controlled producer failed")));
        let blocks = kj.get_blocks(score, &BlockQuery::All).await.unwrap();
        assert!(!blocks.iter().any(|b| b.tick == Some(origin + TickDelta::new(6))), "no fallback before its deadline");

        for beat in 4..=6 {
            scheduler.fire_due(base + Duration::from_secs(beat));
        }
        let blocks = kj.get_blocks(score, &BlockQuery::All).await.unwrap();
        let fallback: Vec<_> = blocks.iter().filter(|b| b.content == next && b.tick == Some(origin + TickDelta::new(6))).collect();
        assert_eq!(fallback.len(), 1, "the latest accepted phrase must play at the missed commitment");
        assert_eq!(fallback[0].id.principal_id, PrincipalId::beat());
        assert_eq!(fallback[0].track.as_ref(), Some(&track));
        assert_eq!(timeline.lock().playhead(), origin + TickDelta::new(6));

        for beat in 7..=9 {
            scheduler.fire_due(base + Duration::from_secs(beat));
        }
        let later = kj.get_blocks(score, &BlockQuery::All).await.unwrap();
        assert_eq!(later.len(), blocks.len(), "neither failure nor fallback repeats");
        let later_feedback = kj.get_blocks(context, &BlockQuery::All).await.unwrap();
        assert_eq!(later_feedback.iter().filter(|b| b.kind == BlockKind::Error).count(), 1);
        assert_eq!(timeline.lock().future_len(), 0);
    });
}
