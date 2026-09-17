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

    fn resolve(&self, params: &Value, _ctx: &dyn ResolverCtx) -> Result<Resolution, ResolveError> {
        match params["abc"].as_str() {
            Some(abc) => Ok(Resolution::new(abc.as_bytes(), kaijutsu_audio::ABC_MIME)),
            None => Err(ResolveError::Failed("controlled producer failed".into())),
        }
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
