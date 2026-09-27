//! Repair blocks stored out of order before 855ace8a.
//!
//! A turn inserts its blocks after its own anchor and before any input that
//! arrived during the turn and holds the log tail. The old `order_midpoint`
//! could return a key below its lower bound, so some of a turn's blocks
//! sorted before their anchor: a tool result before its call, or a chain of
//! calls piled in reverse just ahead of the held input. Hydration then
//! synthesizes an "interrupted" result for the call and drops the real one.
//!
//! The repair works on **runs**: maximal stretches of turn blocks (model and
//! tool blocks, and system error blocks, that carry a tick). Anything else —
//! user input, notifications, drift, a block with no tick — is a fixed
//! separator and never moves, which keeps held input after the turn that ran
//! around it. Only a run holding a tool result sorted before its call is
//! touched. Within it the target order is tick order: the kernel sequences
//! ticks, so they record the order a turn wrote its blocks. Every block of the
//! run gets a fresh key between the run's neighbors, in one accepted,
//! journaled mutation ([`BlockStore::rekey_blocks`]): stored keys can tie, and
//! no key fits between two equal keys, so moving only the displaced blocks
//! is not enough.

use kaijutsu_types::{BlockId, BlockKind, BlockSnapshot, ContextId, Role};

use crate::block_store::BlockStore;

/// New keys for one damaged run: `blocks` in tick order, `keys` strictly
/// increasing and strictly between the run's neighbors.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rekey {
    pub blocks: Vec<BlockId>,
    pub keys: Vec<String>,
}

/// A tool result stored before its call: `(result, call)`.
pub type Misordered = (BlockId, BlockId);

/// Every tool result whose call sorts after it, in document order.
pub fn misordered_results(blocks: &[BlockSnapshot]) -> Vec<Misordered> {
    let position: std::collections::HashMap<BlockId, usize> =
        blocks.iter().enumerate().map(|(i, b)| (b.id, i)).collect();
    blocks.iter().enumerate()
        .filter(|(_, b)| b.kind == BlockKind::ToolResult)
        .filter_map(|(i, b)| {
            let call = b.tool_call_id?;
            (position.get(&call).is_some_and(|&c| c > i)).then_some((b.id, call))
        })
        .collect()
}

fn is_turn_block(block: &BlockSnapshot) -> bool {
    block.tick.is_some()
        && (matches!(block.role, Role::Model | Role::Tool)
            || (block.role == Role::System && block.kind == BlockKind::Error))
}

/// New keys for every damaged run. Every block of such a run is re-keyed,
/// not only the displaced ones, because stored keys can tie and no key fits
/// between two equal keys. Refuses a run whose neighbors leave no room.
pub fn plan(blocks: &[BlockSnapshot]) -> Result<Vec<Rekey>, String> {
    let damaged: std::collections::HashSet<BlockId> =
        misordered_results(blocks).into_iter().map(|(result, _)| result).collect();
    let key = |i: usize| blocks[i].order_key.clone().unwrap_or_default();
    let mut rekeys = Vec::new();
    let mut start = 0;
    while start < blocks.len() {
        if !is_turn_block(&blocks[start]) { start += 1; continue; }
        let mut end = start;
        while end < blocks.len() && is_turn_block(&blocks[end]) { end += 1; }
        let run = &blocks[start..end];
        if run.iter().any(|b| damaged.contains(&b.id)) {
            let lower = start.checked_sub(1).map(key).unwrap_or_default();
            let upper = (end < blocks.len()).then(|| key(end));
            if upper.as_deref().is_some_and(|upper| upper <= lower.as_str()) {
                return Err(format!(
                    "the run starting at block {} has no room between its neighbors' keys ({lower:?}, {upper:?})",
                    run[0].id.to_key()));
            }
            let mut order: Vec<&BlockSnapshot> = run.iter().collect();
            order.sort_by_key(|b| b.tick.expect("a run block has a tick"));
            rekeys.push(Rekey {
                blocks: order.iter().map(|b| b.id).collect(),
                keys: keys_between(&lower, upper.as_deref(), run.len()),
            });
        }
        start = end;
    }
    Ok(rekeys)
}

/// `n` strictly increasing keys strictly between `lower` and `upper` (open
/// when `None`), by bisection so key length grows with log2(n).
fn keys_between(lower: &str, upper: Option<&str>, n: usize) -> Vec<String> {
    let upper = upper.map(str::to_string).unwrap_or_else(|| format!("{lower}z"));
    let mut keys = Vec::with_capacity(n);
    fill(lower, &upper, n, &mut keys);
    keys
}

fn fill(lower: &str, upper: &str, n: usize, out: &mut Vec<String>) {
    if n == 0 { return; }
    let mid = crate::blocks::content::order_midpoint(lower, upper);
    let left = (n - 1) / 2;
    fill(lower, &mid, left, out);
    out.push(mid.clone());
    fill(&mid, upper, n - 1 - left, out);
}

/// What a repair found and did in one context.
#[derive(Debug, Default)]
pub struct Report {
    pub misordered: Vec<Misordered>,
    pub rekeys: Vec<Rekey>,
    /// Results still before their call after the repair. Only filled by
    /// [`repair`]; never repaired by guessing.
    pub remaining: Vec<Misordered>,
}

impl Report {
    /// Blocks given new keys (or planned to be).
    pub fn rekeyed(&self) -> usize { self.rekeys.iter().map(|r| r.blocks.len()).sum() }
}

/// Plan a context's repair without changing it.
pub fn inspect(store: &BlockStore, context_id: ContextId) -> Result<Report, String> {
    let blocks = ordered(store, context_id)?;
    Ok(Report {
        misordered: misordered_results(&blocks),
        rekeys: plan(&blocks).map_err(|e| format!("{context_id}: {e}"))?,
        remaining: Vec::new(),
    })
}

/// Apply a context's repair, one accepted mutation per run, and check the
/// result. An error names what was left wrong; runs already re-keyed stay so,
/// each journaled.
pub fn repair(store: &BlockStore, context_id: ContextId) -> Result<Report, String> {
    let mut report = inspect(store, context_id)?;
    for rekey in &report.rekeys {
        let keys: Vec<(BlockId, String)> = rekey.blocks.iter().copied().zip(rekey.keys.iter().cloned()).collect();
        store.rekey_blocks(context_id, &keys)
            .map_err(|e| format!("{context_id}: re-keying the run at {} failed: {e}", rekey.blocks[0].to_key()))?;
    }
    let blocks = ordered(store, context_id)?;
    report.remaining = misordered_results(&blocks);
    for rekey in &report.rekeys {
        let at: Vec<usize> = rekey.blocks.iter()
            .map(|id| blocks.iter().position(|b| b.id == *id).expect("a re-keyed block stays live"))
            .collect();
        if !at.windows(2).all(|w| w[1] == w[0] + 1) {
            return Err(format!("{context_id}: the run at {} is not contiguous in tick order after re-keying; stopping",
                rekey.blocks[0].to_key()));
        }
    }
    Ok(report)
}

fn ordered(store: &BlockStore, context_id: ContextId) -> Result<Vec<BlockSnapshot>, String> {
    store.get(context_id)
        .map(|entry| entry.doc.blocks_ordered())
        .ok_or_else(|| format!("{context_id}: not loaded"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::block_store::{shared_block_store, DocumentKind};
    use kaijutsu_types::{ContentType, PrincipalId, Status};

    fn ids(blocks: &[BlockSnapshot]) -> Vec<BlockId> { blocks.iter().map(|b| b.id).collect() }

    /// The shape found on moltar: a turn's calls piled in reverse ahead of
    /// the user input that held the tail, their results left before them.
    #[test]
    fn a_turn_scrambled_ahead_of_held_input_returns_to_tick_order() {
        let store = shared_block_store(PrincipalId::system());
        let ctx = ContextId::new();
        store.create_document(ctx, DocumentKind::Conversation, None).unwrap();
        let text = |after: Option<&BlockId>, role, body: &str| store.insert_block_as(
            ctx, None, after, role, BlockKind::Text, body, Status::Done, ContentType::Plain, None).unwrap();
        let u1 = text(None, Role::User, "question");
        let m1 = text(Some(&u1), Role::Model, "looking");
        let held = text(Some(&m1), Role::User, "arrived during the turn");
        let c1 = store.insert_tool_call(ctx, None, Some(&m1), "shell", serde_json::json!({"command": "a"}), None).unwrap();
        let r1 = store.insert_tool_result(ctx, &c1, Some(&c1), "a out", false, Some(0), None).unwrap();
        let c2 = store.insert_tool_call(ctx, None, Some(&r1), "shell", serde_json::json!({"command": "b"}), None).unwrap();
        let r2 = store.insert_tool_result(ctx, &c2, Some(&c2), "b out", false, Some(0), None).unwrap();
        let intended = vec![u1, m1, c1, r1, c2, r2, held];
        assert_eq!(ids(&store.get(ctx).unwrap().doc.blocks_ordered()), intended);

        // Scramble the way the bad keys did: calls reversed, just ahead of `held`.
        store.move_block(ctx, &c1, Some(&r2)).unwrap();
        store.move_block(ctx, &c2, Some(&r2)).unwrap();
        let scrambled = store.get(ctx).unwrap().doc.blocks_ordered();
        assert_eq!(ids(&scrambled), vec![u1, m1, r1, r2, c2, c1, held]);
        assert_eq!(misordered_results(&scrambled), vec![(r1, c1), (r2, c2)]);

        let report = repair(&store, ctx).unwrap();
        assert!(report.remaining.is_empty(), "{:?}", report.remaining);
        assert_eq!(ids(&store.get(ctx).unwrap().doc.blocks_ordered()), intended);
        assert!(inspect(&store, ctx).unwrap().rekeys.is_empty(), "a second run plans nothing");
    }

    /// A run with no result before its call is left alone, even when its
    /// ticks are out of order: that order may be deliberate.
    #[test]
    fn an_undamaged_run_is_not_touched() {
        let store = shared_block_store(PrincipalId::system());
        let ctx = ContextId::new();
        store.create_document(ctx, DocumentKind::Conversation, None).unwrap();
        let a = store.insert_block_as(ctx, None, None, Role::Model, BlockKind::Text, "a",
            Status::Done, ContentType::Plain, None).unwrap();
        let b = store.insert_block_as(ctx, None, Some(&a), Role::Model, BlockKind::Text, "b",
            Status::Done, ContentType::Plain, None).unwrap();
        store.move_block(ctx, &b, None).unwrap();
        assert!(inspect(&store, ctx).unwrap().rekeys.is_empty());
    }

    /// Stored keys can tie; a run holding tied keys still returns to tick
    /// order, since every block of it gets a fresh key.
    #[test]
    fn a_run_with_tied_keys_is_repaired() {
        let store = shared_block_store(PrincipalId::system());
        let ctx = ContextId::new();
        store.create_document(ctx, DocumentKind::Conversation, None).unwrap();
        let u1 = store.insert_block_as(ctx, None, None, Role::User, BlockKind::Text, "q",
            Status::Done, ContentType::Plain, None).unwrap();
        let c1 = store.insert_tool_call(ctx, None, Some(&u1), "shell", serde_json::json!({"command": "a"}), None).unwrap();
        let r1 = store.insert_tool_result(ctx, &c1, Some(&c1), "a out", false, Some(0), None).unwrap();
        let c2 = store.insert_tool_call(ctx, None, Some(&r1), "shell", serde_json::json!({"command": "b"}), None).unwrap();
        let r2 = store.insert_tool_result(ctx, &c2, Some(&c2), "b out", false, Some(0), None).unwrap();
        let held = store.insert_block_as(ctx, None, Some(&r2), Role::User, BlockKind::Text, "held",
            Status::Done, ContentType::Plain, None).unwrap();
        // c2 and r2 share one key, and c1 sorts after its result r1.
        let key = |id: &BlockId| store.get(ctx).unwrap().doc.get_block_snapshot(id).unwrap().order_key.unwrap();
        let mid = crate::blocks::content::order_midpoint;
        let k1 = mid(&key(&u1), &key(&held));
        let k2 = mid(&k1, &key(&held));
        let k3 = mid(&k2, &key(&held));
        store.rekey_blocks(ctx, &[(r1, k1), (c2, k2.clone()), (r2, k2), (c1, k3)]).unwrap();
        let blocks = store.get(ctx).unwrap().doc.blocks_ordered();
        assert!(blocks.windows(2).any(|w| w[0].order_key == w[1].order_key), "the fixture must hold a tie");
        assert!(!misordered_results(&store.get(ctx).unwrap().doc.blocks_ordered()).is_empty(),
            "the fixture must start damaged");

        let report = repair(&store, ctx).unwrap();
        assert!(report.remaining.is_empty(), "{:?}", report.remaining);
        assert_eq!(ids(&store.get(ctx).unwrap().doc.blocks_ordered()), vec![u1, c1, r1, c2, r2, held]);
    }

    #[test]
    fn keys_between_are_strictly_increasing_inside_their_bounds() {
        for (lower, upper) in [("V1", Some("V2")), ("", None), ("Vzzz", None), ("V0000000002ezqzr3sA", Some("V0000000002ezr3sA"))] {
            let keys = keys_between(lower, upper, 40);
            assert_eq!(keys.len(), 40);
            assert!(keys.first().unwrap().as_str() > lower, "{lower:?} {keys:?}");
            assert!(keys.windows(2).all(|w| w[0] < w[1]), "{keys:?}");
            if let Some(upper) = upper { assert!(keys.last().unwrap().as_str() < upper, "{upper:?} {keys:?}"); }
            assert!(keys.iter().all(|k| k.len() <= lower.len().max(upper.map_or(0, str::len)) + 8), "{keys:?}");
        }
    }
}
