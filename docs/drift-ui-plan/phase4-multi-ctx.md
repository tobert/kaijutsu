# Phase 4: Multi-Context Experience

**Goal:** Enable working with multiple contexts simultaneously — the constellation as the
primary navigation metaphor, forking as a first-class operation, per-context LLM configuration,
and fluid context switching.

## Key Files

| File | Action |
|------|--------|
| `crates/kaijutsu-app/src/cell/components.rs` | Multi-document state cache |
| `crates/kaijutsu-app/src/cell/systems.rs` | Context switching, multi-doc sync |
| `crates/kaijutsu-app/src/ui/widget/mod.rs` | Context tabs widget (North dock) |
| `crates/kaijutsu-app/src/ui/constellation/mod.rs` | Fork-from-UI, enhanced navigation |
| `crates/kaijutsu-app/src/ui/constellation/create_dialog.rs` | Fork dialog connected to constellation |
| `crates/kaijutsu-app/src/connection/actor_plugin.rs` | Multi-seat management |

## Architecture Decisions

### 1. Multi-Document State

Currently the app tracks a single document:

```rust
// Current: single active conversation
pub struct ViewingConversation {
    pub conversation_id: String,
    pub last_sync_version: u64,
}
```

Target: cache multiple documents so switching is instant:

```rust
/// Cache of document states for contexts we've visited
#[derive(Resource, Default)]
pub struct DocumentCache {
    /// document_id → cached sync state
    documents: HashMap<String, CachedDocument>,
    /// Currently active document
    active: Option<String>,
}

pub struct CachedDocument {
    pub document: BlockDocument,
    pub sync_version: u64,
    pub context_name: String,
    pub last_accessed: Instant,
}
```

**Eviction:** Cache up to N documents (8?), evict LRU. Documents are re-fetched on switch if evicted.

**Sync:** All cached documents receive block events (matched by `document_id` in `ServerEvent`).
The active document's events drive the UI; background documents update silently. Event routing
uses a `HashMap<String, &mut CachedDocument>` lookup by `document_id` — O(1) per event, not O(N*M).

### ⚠️ Staleness and Resync (Generation Counter)

Each `CachedDocument` tracks `synced_at_generation: u64`. The global `SyncGeneration` resource
(Phase 1) is bumped on broadcast lag or reconnect. Documents detect staleness by comparing
their generation against the global:

```rust
pub struct CachedDocument {
    pub document: BlockDocument,
    pub sync_version: u64,
    pub context_name: String,
    pub last_accessed: Instant,
    pub synced_at_generation: u64,  // compared against SyncGeneration
}
```

**Recovery behavior:**
1. **Active document:** Each frame, check `synced_at_generation < generation.0`. If stale,
   spawn `get_document_state()` and replace cached state. Update `synced_at_generation`.
2. **Background documents:** No immediate action. Staleness detected lazily on switch.
3. **On switch to stale doc:** Same as cache miss — re-fetch before displaying.
   The context switch flow already handles this path.

**Why this is better than universal invalidation:** With 5-10 agents active, a single lag
event doesn't trigger 10 simultaneous `get_document_state()` calls. Only the active doc
resyncs immediately; the rest resync lazily when the user switches to them.

### 2. Context Switching Flow

```
User: gt (next context) or click constellation node
  │
  ▼
join_context(new_context, instance) ──► Server
  │
  ▼
SeatTaken { seat_info } ──► Update active context
  │
  ├─ Document in cache? ──► Switch immediately (apply pending events)
  │
  └─ Not cached? ──► get_document_state() ──► Load into cache ──► Switch
  │
  ▼
UI updates: tabs highlight, constellation focus, cell content swaps
```

**Seat management:** The server's seat system already supports being in multiple contexts. The app currently `leave_seat()` before `join_context()`. For multi-context, we stay seated in all joined contexts and just switch which one is *active* in the UI.

### 3. Constellation as Primary Navigation

The constellation is not a secondary visualization — it IS the multi-context navigation
metaphor. Think 4X strategy game: an information-dense spatial map that expert users scan
instantly to understand overall system state, then zoom into a specific context to work.

**Design principles:**
- Constellation shows *all* known contexts (not just ones you're seated in)
- Each node conveys: activity state (idle/streaming/error), agent presence, drift connections
- "Zooming in" to a node means switching your focal view to that context's document
- Multiple contexts can be pulled up in split view or across kaijutsu-app instances
- MRU (most recently used) drives which nodes are prominent vs faded

**Interaction model:**
- `gt`/`gT` cycles through MRU contexts (not left-to-right like browser tabs)
- `Ctrl-^` toggles between two most recent (vim alternate buffer)
- Click a constellation node to zoom into it
- `f` on a focused node to fork from it
- Constellation mode (Tab key) zooms out to full map for spatial overview

**North dock context strip (secondary):**
```
[kaijutsu]  [@abc main] [@def exploration] [@ghi review]  [connected ●]
```
This is a *shortcut into the constellation*, not the primary metaphor. Shows MRU joined
contexts as compact badges. Clicking a badge switches context. But the constellation is
where you go to understand the full picture.

Widget type: `WidgetContent::ContextStrip` — driven by `DocumentCache` MRU order.

### 4. Per-Context LLM Configuration

Different models in different contexts is central to drift's value — Claude distills for Claude,
Gemini for Gemini, and drift carries insights across the boundary. This is not a stretch goal;
it's what makes multi-context worth having.

Each kernel can have its own LLM config. The Tier 2 ActorHandle methods make this accessible:

```
get_llm_config() → { provider, model, available_providers, available_models }
set_default_provider(provider) → bool
set_default_model(provider, model) → bool
```

**UI integration:**
- Constellation node shows current model as a subtle badge (e.g., `sonnet`, `gemini`)
- `m` on a focused constellation node opens model picker
- Shell command `llm config` / `llm set-model provider/model` for keyboard-driven config
- Fork dialog offers model selection (fork with a different model to explore alternatives)

### 5. Fork-from-UI

Forking is a fundamental operation in the drift model — it's how you create parallel
explorations with isolated state. It belongs in the constellation, not buried in a timeline menu.

**Flow:**
1. User is in constellation, focused on a context node
2. Press `f` to fork
3. Dialog appears: name the new context, select version (latest or specific), choose model
4. `fork_from_version(document_id, version, context_name)` → creates new context
5. New node appears in constellation with ancestry line, user is auto-joined
6. Optionally set a different LLM model on the fork (explore with Gemini while parent uses Claude)

**Dialog reuse:** `create_dialog.rs` already has a dialog for joining/creating contexts. Extend it with a "Fork from" option that pre-fills the source context and offers model selection.

### 6. Constellation Visual Enhancements

With constellation as the primary navigation tool, it needs to be information-dense:

- **Active context highlight:** Glow the active node brighter
- **Activity streaming:** Nodes pulse when their context has active LLM streaming
- **Model badge:** Small text showing current model per node
- **Fork lines:** Draw parent→child lines for forked contexts
- **Drift lines:** (from Phase 3) Show pending/recent drift connections
- **Agent presence:** Show how many agents are active in each context
- **MRU ordering:** Most recently used contexts are visually prominent, old ones fade
- **Graph trimming:** For large constellations, show MRU subset + ancestors + drift-connected

## Implementation Steps

### Step 1: DocumentCache resource
- Define `DocumentCache` and `CachedDocument` (with `synced_at_generation: u64`) in `cell/components.rs`
- Modify block event handling to route by `document_id` via HashMap lookup (O(1) per event)
- Active document drives UI rendering, others update silently
- Check `SyncGeneration` each frame: if active doc is behind, trigger resync

### Step 2: Per-context LLM config
- Add `get_llm_config()` and `set_default_model()` to Tier 2 ActorHandle methods
- Show current model as badge on constellation nodes
- Shell commands `llm config` / `llm set-model` for keyboard-driven config
- This enables the core value prop: different models in different contexts

### Step 3: Context switching + constellation navigation
- Implement `switch_context()` system: join new context, update active document
- Handle cache hit (instant) vs cache miss / stale generation (fetch + load)
- Constellation click → switch context ("zoom in")
- `gt`/`gT` cycles MRU order, `Ctrl-^` toggles alternate
- Keep seats in all joined contexts (no leave on switch)

### Step 4: Fork-from-UI
- `f` on focused constellation node opens fork dialog
- Pre-populate source context, offer version selector and model picker
- Execute `fork_from_version()` and auto-join result
- New node appears in constellation with ancestry line

### Step 5: Context strip widget (North dock)
- Add `WidgetContent::ContextStrip` — compact MRU badges as shortcut into constellation
- Active context highlighted, click to switch
- Close button → leave seat + remove from cache
- Secondary to constellation, not the primary navigation

### Step 6: Constellation visual enhancements
- Model badges on nodes
- Agent presence indicators
- MRU prominence (recent nodes bright, old nodes faded)
- Graph trimming for large constellations (MRU + ancestors + drift-connected)

## Verification

- [ ] Can join multiple contexts and switch between them with `gt`/`gT` (MRU order)
- [ ] Constellation click switches active context ("zoom in")
- [ ] Switching contexts is instant when document is cached
- [ ] Switching to an uncached or stale-generation context fetches before display
- [ ] Block events for background contexts are received and cached
- [ ] Stale generation detected and active doc resyncs within one frame
- [ ] Per-context LLM model shown on constellation nodes
- [ ] `m` on focused node opens model picker
- [ ] Fork from constellation (`f`) creates new context with model selection
- [ ] Ancestry lines visible in constellation between parent and child
- [ ] `Ctrl-^` toggles between two most recent contexts
- [ ] Context strip in North dock shows MRU badges
- [ ] Closing a context (leave seat) removes from cache and constellation
- [ ] Large constellation (10+ contexts) trims to MRU + ancestors + drift-connected

## Dependencies

- **Phase 2** must be complete (ActorPlugin)
- **Phase 3** partially (drift rendering for drift connection lines in constellation)
- Constellation already has node rendering, zoom, and mode switching

## Status Log

| Date | Status | Notes |
|------|--------|-------|
| | | |
