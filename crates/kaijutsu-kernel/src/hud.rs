//! Ephemeral per-context HUD telemetry and settled gate-stamp formatting.
//!
//! Holds the live, interaction-rate facts that have no durable table of their
//! own: a gate model call currently in flight, the open `council` stream's
//! token-rate/phase/entropy cells, and the latest operational-weather glyphs
//! and generation timing. Durable HUD facts (`held_refusal`, `control_count`,
//! `--decide`, bump streaks) are joined from `KernelDb` when `ContextHud` is
//! assembled on the wire.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use dashmap::DashMap;
use kaijutsu_types::{ContextHud, ContextId};

use crate::llm::stream::{CouncilUsageExtra, LiveCouncilStats};

/// Ephemeral per-context HUD state owned by [`crate::Kernel`].
#[derive(Debug, Default)]
pub struct KernelHudStore {
    by_context: DashMap<ContextId, ContextLiveHud>,
    next_token: AtomicU64,
}

#[derive(Debug, Default, Clone)]
struct ContextLiveHud {
    judging: Option<JudgingInFlight>,
    live_council: Option<Arc<LiveCouncilStats>>,
    last_vibe: Option<String>,
    last_tps_tenths: Option<u32>,
    last_ttft_ms: Option<u32>,
    last_entropy: Option<String>,
}

#[derive(Debug, Clone)]
struct JudgingInFlight {
    token: u64,
    tool: String,
    started_at_ms: u64,
}

/// Clears its in-flight gate entry on [`KernelHudStore`] when dropped.
pub struct JudgingGuard<'a> {
    store: &'a KernelHudStore,
    context_id: ContextId,
    token: u64,
}

impl Drop for JudgingGuard<'_> {
    fn drop(&mut self) {
        if let Some(mut entry) = self.store.by_context.get_mut(&self.context_id)
            && entry.judging.as_ref().is_some_and(|j| j.token == self.token)
        {
            entry.judging = None;
        }
    }
}

/// Clears its open `council` stream cell on [`KernelHudStore`] when dropped.
pub struct CouncilStreamGuard<'a> {
    store: &'a KernelHudStore,
    context_id: ContextId,
    stats: Arc<LiveCouncilStats>,
}

impl Drop for CouncilStreamGuard<'_> {
    fn drop(&mut self) {
        if let Some(mut entry) = self.store.by_context.get_mut(&self.context_id)
            && entry.live_council.as_ref().is_some_and(|cur| Arc::ptr_eq(cur, &self.stats))
        {
            entry.live_council = None;
        }
    }
}

/// Live fields merged onto [`ContextHud`] during `listContexts`.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct LiveHudSnapshot {
    pub judging_tool: Option<String>,
    pub judging_started_at: Option<u64>,
    pub vibe: Option<String>,
    pub council_phase: Option<String>,
    pub council_tps: Option<f32>,
    pub council_ttft_ms: Option<u32>,
    pub council_entropy: Option<String>,
}

impl KernelHudStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Mark a gate model call (`council`, `guardian`, or `decisions`) in flight
    /// on `context_id`. Cleared automatically when the returned guard drops.
    pub fn begin_judging(&self, context_id: ContextId, tool: impl Into<String>) -> JudgingGuard<'_> {
        let token = self.next_token.fetch_add(1, Ordering::Relaxed) + 1;
        let started_at_ms = kaijutsu_types::now_millis();
        let mut entry = self.by_context.entry(context_id).or_default();
        entry.judging = Some(JudgingInFlight {
            token,
            tool: tool.into(),
            started_at_ms,
        });
        drop(entry);
        JudgingGuard { store: self, context_id, token }
    }

    /// Attach `stats` as the active streaming `council` telemetry for `context_id`.
    pub fn begin_council_stream<'a>(
        &'a self,
        context_id: ContextId,
        stats: Arc<LiveCouncilStats>,
    ) -> CouncilStreamGuard<'a> {
        let mut entry = self.by_context.entry(context_id).or_default();
        entry.live_council = Some(stats.clone());
        drop(entry);
        CouncilStreamGuard { store: self, context_id, stats }
    }

    /// Record completed `council` generation telemetry for `context_id`.
    pub fn record_council_done(&self, context_id: ContextId, extra: &CouncilUsageExtra) {
        let mut entry = self.by_context.entry(context_id).or_default();
        if let Some(vibe) = extra.vibe.as_deref().filter(|s| !s.is_empty()) {
            entry.last_vibe = Some(vibe.to_string());
        }
        if let Some(tenths) = extra.tps_tenths.filter(|&t| t > 0) {
            entry.last_tps_tenths = Some(tenths);
        }
        if let Ok(ms) = u32::try_from(extra.first_token_ms)
            && ms > 0
        {
            entry.last_ttft_ms = Some(ms);
        }
        if let Some(spark) = extra.entropy_spark.as_deref().filter(|s| !s.is_empty()) {
            entry.last_entropy = Some(spark.to_string());
        } else if let Some(milli) = extra.mean_entropy_milli {
            entry.last_entropy = Some(format!("{:.2}", milli as f32 / 1000.0));
        }
    }

    /// Record operational weather glyphs from a gate decision (`decisions` judge).
    pub fn record_vibe(&self, context_id: ContextId, vibe: Option<&str>) {
        let Some(vibe) = vibe.filter(|s| !s.is_empty()) else {
            return;
        };
        self.by_context.entry(context_id).or_default().last_vibe = Some(vibe.to_string());
    }

    /// Read the current ephemeral telemetry for `context_id`.
    pub fn snapshot(&self, context_id: ContextId) -> LiveHudSnapshot {
        let Some(entry) = self.by_context.get(&context_id) else {
            return LiveHudSnapshot::default();
        };
        let (judging_tool, judging_started_at) = match &entry.judging {
            Some(j) => (Some(j.tool.clone()), Some(j.started_at_ms)),
            None => (None, None),
        };
        let mut council_phase = None;
        let mut live_tps_tenths = None;
        let mut live_ttft_ms = None;
        let mut live_entropy = None;
        if let Some(live) = &entry.live_council {
            council_phase = live.phase.lock().map(str::to_string);
            let tps = live.tps_tenths.load(Ordering::Relaxed);
            if tps > 0 {
                live_tps_tenths = Some(tps);
            }
            let ttft = live.ttft_ms.load(Ordering::Relaxed);
            if ttft > 0 {
                live_ttft_ms = Some(ttft);
            }
            live_entropy = live.entropy_spark.lock().clone();
        }
        let tps_tenths = live_tps_tenths.or(entry.last_tps_tenths);
        LiveHudSnapshot {
            judging_tool,
            judging_started_at,
            vibe: entry.last_vibe.clone(),
            council_phase,
            council_tps: tps_tenths.map(|t| t as f32 / 10.0),
            council_ttft_ms: live_ttft_ms.or(entry.last_ttft_ms),
            council_entropy: live_entropy.or_else(|| entry.last_entropy.clone()),
        }
    }

    /// Apply ephemeral fields from `snapshot` onto `hud`.
    pub fn fill_hud(&self, context_id: ContextId, hud: &mut ContextHud) {
        let snap = self.snapshot(context_id);
        hud.judging_tool = snap.judging_tool;
        hud.judging_started_at = snap.judging_started_at;
        hud.vibe = snap.vibe;
        hud.council_phase = snap.council_phase;
        hud.council_tps = snap.council_tps;
        hud.council_ttft_ms = snap.council_ttft_ms;
        hud.council_entropy = snap.council_entropy;
    }
}

/// Short model or spec name for HUD pills and `ToolCall` divider stamps:
/// strips `<backend>/` or `<org>/` prefixes and trailing `-YYYYMMDD` dates.
pub fn short_judge_label(raw: &str) -> String {
    let leaf = raw.rsplit('/').next().unwrap_or(raw);
    if leaf.len() > 9 {
        let (stem, tail) = leaf.split_at(leaf.len() - 9);
        if tail.starts_with('-') && tail[1..].bytes().all(|b| b.is_ascii_digit()) && !stem.is_empty() {
            return stem.to_string();
        }
    }
    leaf.to_string()
}

/// Format a probability in `[0.0, 1.0]` for a compact gate stamp (`".97"`, `"1.00"`).
pub fn short_prob(p: f64) -> String {
    let clamped = if p.is_finite() { p.clamp(0.0, 1.0) } else { 0.0 };
    if clamped >= 0.995 {
        "1.00".to_string()
    } else {
        format!(".{:02}", (clamped * 100.0).round() as u32)
    }
}

/// Leaf file name of a `--decide` VFS path (e.g. `"/config/rc/vibe.json"` -> `"vibe"`).
pub fn short_decide_program(path: &str) -> Option<String> {
    let file = path.rsplit('/').next().unwrap_or(path).trim();
    if file.is_empty() {
        return None;
    }
    Some(file.strip_suffix(".json").unwrap_or(file).to_string())
}

/// Compact post-generation telemetry stamp for a Council `Model` block divider
/// (`"🌊 · 31.4t/s · H ▂▃▅"`, `"31.4t/s · H ▂▃▅"`).
pub fn council_divider_stamp(extra: &CouncilUsageExtra) -> Option<String> {
    let mut parts = Vec::new();
    if let Some(vibe) = extra.vibe.as_deref().filter(|s| !s.is_empty()) {
        parts.push(vibe.to_string());
    }
    if let Some(tenths) = extra.tps_tenths.filter(|&t| t > 0) {
        parts.push(format!("{:.1}t/s", tenths as f32 / 10.0));
    }
    if let Some(spark) = extra.entropy_spark.as_deref().filter(|s| !s.is_empty()) {
        parts.push(format!("H {spark}"));
    } else if let Some(milli) = extra.mean_entropy_milli {
        parts.push(format!("H {:.2}", milli as f32 / 1000.0));
    }
    (!parts.is_empty()).then(|| parts.join(" · "))
}

/// Assemble [`ContextHud`] snapshots for `listed` contexts in one batched pass.
pub async fn build_context_huds(
    kernel: &crate::Kernel,
    listed: &[ContextId],
    bound_by_ctx: &std::collections::HashMap<ContextId, crate::ContextModelLookup>,
) -> std::collections::HashMap<ContextId, ContextHud> {
    use std::collections::{HashMap, HashSet};

    let council_backends: HashSet<String> = kernel
        .llm()
        .read()
        .await
        .backend_configs()
        .iter()
        .filter(|b| b.kind == crate::llm::BackendKind::Council)
        .map(|b| b.name.clone())
        .collect();
    let gate_config = crate::kj::gate_policy::load_config(kernel.vfs(), kernel.kernel_db()).await.ok();

    let mut out = HashMap::with_capacity(listed.len());
    let now_ms = kaijutsu_types::now_millis() as i64;
    {
        let db = kernel.kernel_db().lock();
        let control_counts: HashMap<ContextId, u32> = db
            .conn_for_ledger()
            .prepare("SELECT context_id, COUNT(*) FROM context_controls GROUP BY context_id")
            .and_then(|mut stmt| {
                let rows = stmt.query_map([], |row| {
                    let bytes: Vec<u8> = row.get(0)?;
                    let count: u32 = row.get(1)?;
                    Ok((bytes, count))
                })?;
                let mut map = HashMap::new();
                for r in rows.flatten() {
                    if let Some(id) = ContextId::try_from_slice(&r.0) {
                        map.insert(id, r.1);
                    }
                }
                Ok(map)
            })
            .unwrap_or_default();

        for &id in listed {
            let mut hud = ContextHud::default();
            if let Some(crate::ContextModelLookup::Model(m)) = bound_by_ctx.get(&id) {
                hud.is_council_backend = council_backends.contains(&m.backend);
                hud.decide_program = m.tunables.decide.as_deref().and_then(short_decide_program);
            }
            hud.held_refusal = db.last_turn_was_refused(id).unwrap_or(false);
            hud.control_count = control_counts.get(&id).copied().unwrap_or(0);

            if let Some(cfg) = &gate_config
                && let Ok(performer) = crate::kj::gate_policy::performer_of(&db, Some(id))
                && let Ok(Some((label, bump_limit, escalate))) =
                    crate::council::gate::hud_gate_info(&db, cfg, id, performer.as_ref())
            {
                hud.gate_label = Some(label);
                let filter = approval_ledger::council::SubmissionFilter {
                    context_id: Some(id.as_bytes().to_vec()),
                    since_ms: escalate.map(|e| now_ms - e.minutes as i64 * 60_000),
                    limit: 16,
                };
                if let Ok(rows) = approval_ledger::council::list_council_submissions(db.conn_for_ledger(), &filter)
                    && let Some(streak) = approval_ledger::council::bump_streak(&rows)
                    && streak.count > 0
                {
                    hud.bump_streak = streak.count as u32;
                    hud.bump_limit = escalate.map(|e| e.bumps).or(bump_limit).unwrap_or(0) as u32;
                    hud.last_bump_flavor = streak.flavors.last().cloned();
                }
            }
            out.insert(id, hud);
        }
    }
    for (&id, hud) in &mut out {
        kernel.hud().fill_hud(id, hud);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn judging_guard_clears_only_its_own_in_flight_entry() {
        let store = KernelHudStore::new();
        let ctx = ContextId::new();
        let g1 = store.begin_judging(ctx, "shell_write");
        assert_eq!(store.snapshot(ctx).judging_tool.as_deref(), Some("shell_write"));
        let g2 = store.begin_judging(ctx, "shell");
        drop(g1);
        assert_eq!(store.snapshot(ctx).judging_tool.as_deref(), Some("shell"));
        drop(g2);
        assert_eq!(store.snapshot(ctx).judging_tool, None);
    }

    #[test]
    fn short_helpers_format_judge_labels_probabilities_and_decide_names() {
        assert_eq!(short_judge_label("openrouter/typesafe/jev-1.13-20260917"), "jev-1.13");
        assert_eq!(short_judge_label("openai/gpt-6-luna"), "gpt-6-luna");
        assert_eq!(short_judge_label("shell-gate"), "shell-gate");
        assert_eq!(short_prob(0.972), ".97");
        assert_eq!(short_prob(0.999), "1.00");
        assert_eq!(short_prob(0.04), ".04");
        assert_eq!(short_decide_program("/config/kernel/council/haiku.json"), Some("haiku".into()));
    }

    #[test]
    fn council_divider_stamp_formats_vibe_tps_and_entropy() {
        let mut extra = CouncilUsageExtra::default();
        assert_eq!(council_divider_stamp(&extra), None);

        extra.tps_tenths = Some(314);
        extra.entropy_spark = Some("▂▃▅".to_string());
        assert_eq!(
            council_divider_stamp(&extra).as_deref(),
            Some("31.4t/s · H ▂▃▅")
        );

        extra.vibe = Some("🌊".to_string());
        assert_eq!(
            council_divider_stamp(&extra).as_deref(),
            Some("🌊 · 31.4t/s · H ▂▃▅")
        );
    }
}
