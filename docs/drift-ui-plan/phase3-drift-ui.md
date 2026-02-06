# Phase 3: Drift UI

**Goal:** Make drift a first-class interactive experience — enhanced block rendering, shell commands that work today, context list widget, drift queue indicator, and constellation drift lines.

## Key Files

| File | Action |
|------|--------|
| `crates/kaijutsu-app/src/cell/systems.rs` | Enhanced drift block rendering |
| `crates/kaijutsu-app/src/ui/drift.rs` | **New** — drift-specific UI systems |
| `crates/kaijutsu-app/src/ui/widget/mod.rs` | Context list + drift queue widgets |
| `crates/kaijutsu-app/src/ui/constellation/mod.rs` | Drift connection lines |
| `crates/kaijutsu-app/src/ui/constellation/render.rs` | Drift line rendering |
| `crates/kaijutsu-kernel/src/drift.rs` | Reference only |

## What Works Today

Shell mode already routes commands to `DriftEngine` via EmbeddedKaish. These commands work from the shell prompt *right now* (once connected to a server with multiple contexts):

```
drift ls                              # list contexts
drift push @abc "check this out"      # stage a message for context abc
drift queue                           # see what's staged
drift cancel 1                        # remove staged item
drift flush                           # send all staged items
drift pull @abc                       # pull and distill from context abc
drift merge @abc                      # merge context abc into current
```

The gap is entirely on the UI side — the commands execute and produce drift blocks, but the rendering and discoverability are minimal.

## Architecture Decisions

### 1. Drift Block Rendering — Full Variant Treatment

Drift blocks carry different DriftKinds and content types. Rather than one-size-fits-all
chrome, each variant gets rendering appropriate to its purpose.

Current rendering (dim gray, single-line header):
```
🚂 Drift from abc123 (claude-sonnet-4-5-20250929)
The distilled content here...
```

#### Variant rendering by DriftKind

**Push (→) — outgoing message, compact:**
```
→ @def  Hey, found the auth bug — it's in session.rs:142
```
Short pushes (< 2 lines) render inline with just a direction arrow and target.
No border — they're conversational, like chat messages between contexts.

**Push (←) — incoming message from another context:**
```
← @abc (claude-sonnet-4-5)  Found the auth bug — it's in session.rs:142
```
Same compact treatment but with source context and model attribution.
Accent color distinguishes from local messages.

**Pull/Distill — substantive summary, boxed:**
```
┌─ 🚂 pulled from @abc ─── claude-sonnet-4-5 ─────────────────────┐
│ Context @abc has been investigating the auth flow. Key findings:  │
│ 1. Session tokens expire too aggressively (30s vs 300s)           │
│ 2. The refresh endpoint doesn't propagate to Redis cluster        │
│ Summary: Increase TTL and fix cache invalidation in session.rs    │
└──────────────────────────────────────────────────────────────────┘
```
Distilled content gets the full box treatment — border, badge, model, wrapping.
These are the substantive briefings that justify drift's existence.

**Merge (⇄) — fork reunion, highlighted:**
```
┌─ ⇄ merged from @abc ────────────────────────────────────────────┐
│ Merged 47 blocks from exploration branch.                        │
│ Key changes: auth flow rewritten, 3 new tests added.             │
└──────────────────────────────────────────────────────────────────┘
```
Similar to pull but with merge icon and potentially a diff summary.

**Commit (📝) — generated commit message:**
```
📝 @abc  feat(auth): increase session TTL and fix Redis propagation
```
One-liner, monospace, looks like a git log entry.

**Error — failed drift operation:**
```
⚠ drift pull @xyz failed: unknown context "xyz"
```
Warning color, no border. Error text from DriftError.

#### Common elements across all variants

- **Source context badge:** `@abc` short ID in accent color (not the full UUID)
- **Model attribution:** Shortened model name (strip provider prefix), shown for AI-generated content
- **Direction indicator:** `→` outgoing, `←` incoming, `⇄` merge, `📝` commit, `⚠` error
- **Collapsible:** Drift blocks participate in j/k navigation and can be collapsed like tool calls
- **Content color:** Normal text (not dim — drift content is substantive, not system chrome)

#### Implementation

Modify the `BlockKind::Drift` match arms in `cell/systems.rs` (lines ~422 and ~1768).
The `BlockSnapshot` already carries `source_context`, `source_model`, and `drift_kind` fields.

Add a `format_drift_block(block: &BlockSnapshot) -> String` function that switches on
`drift_kind` and content length to select the appropriate variant. Keep all rendering
in one function so the variants stay cohesive.

### 2. Context List Widget

A widget showing all registered drift contexts. Candidates for placement:
- **South dock, center** (between mode and hints) — compact, always visible
- **East dock** — more room but takes horizontal space from content

Start with South dock (minimal, shows context count + active short IDs):

```
South dock layout:
[mode]  [@abc @def @ghi ·2 staged]  [hints]
```

Widget type: `WidgetContent::Contexts` — queries drift contexts periodically (every 5s or on drift events).

Data source: `ActorHandle::list_all_contexts()` (already implemented).

### 3. Drift Queue Indicator

When items are staged (between `drift push` and `drift flush`), show a count:

```
·2 staged
```

This integrates into the context list widget or stands alone. Clicking/activating it could show the queue contents.

Data source: `ActorHandle::drift_queue()` (already implemented).

### 4. Constellation Drift Lines

The constellation already renders context nodes with position, activity state, and glow. Add drift connection lines:

- When a drift push is staged from A → B, draw a dashed line A → B
- When a drift is flushed/delivered, briefly animate the line (pulse)
- When contexts share a parent (forked), draw a thin ancestry line

Implementation in `constellation/render.rs`:
- Query `drift_queue()` to get staged connections
- Draw lines between node positions using Bevy's `Gizmos` or mesh-based lines
- Use drift accent color with alpha for undelivered, full opacity pulse for delivery

### 5. Drift Notifications

When a drift block arrives in the current context (from another context pushing to us), show a brief notification:

```
🚂 Drift from @abc: "Summarized findings about..."
```

This could be a transient widget (auto-dismiss after 5s) or a flash in the context list widget.

## Implementation Steps

### Step 1: Full variant drift block rendering
- Add `format_drift_block()` function that switches on `drift_kind` + content length
- Compact inline for short pushes, boxed for pulls/distillations/merges, one-liner for commits
- Error variant with warning styling for failed operations
- Update `block_color()`: drift accent color (not `fg_dim`), direction-aware tinting
- Drift blocks are collapsible and participate in j/k focus navigation
- Truncate model name for display (strip provider prefix)

### Step 2: Create `ui/drift.rs`
- System to periodically poll `list_all_contexts()` and `drift_queue()`
- Store results in a `DriftState` resource
- Provide data for the context list widget

### Step 3: Context list widget
- Add `WidgetContent::Contexts` variant to widget system
- Render context short IDs with activity indicators
- Include staged drift count
- Register in South dock

### Step 4: Drift queue detail view
- When drift queue widget is selected/activated, show expanded queue
- List staged items: `→ @target: "preview of content..." (3m ago)`
- Keyboard shortcut to flush all or cancel individual items

### Step 5: Constellation drift lines
- Add drift line rendering to constellation
- Query staged drifts for pending connections
- Draw ancestry lines for forked contexts
- Animate delivery pulse

### Step 6: Drift arrival notification
- Detect new drift blocks arriving (via `ServerEvent::BlockInserted` where kind is Drift)
- Show transient notification widget or flash
- Optional: sound/visual ping

## Verification

- [ ] Push blocks render compact (inline arrow + badge, no border for short content)
- [ ] Pull/distill blocks render boxed (border, badge, model, wrapping)
- [ ] Merge blocks render with ⇄ icon and merge-specific treatment
- [ ] Commit blocks render as one-liner monospace
- [ ] Error drift operations render with warning styling
- [ ] Drift blocks are collapsible and navigable with j/k
- [ ] Context list widget shows registered contexts in South dock
- [ ] Drift queue count appears when items are staged
- [ ] `drift push/flush/pull/merge` commands produce correctly rendered blocks
- [ ] Constellation shows drift connection lines between contexts
- [ ] Drift arrival from another context shows notification

## Dependencies

- **Phase 2** must be complete (ActorPlugin, so we can call `ActorHandle` methods from Bevy systems)
- Drift server-side is already complete (DriftRouter, DriftEngine, all RPC methods)

## Status Log

| Date | Status | Notes |
|------|--------|-------|
| | | |
